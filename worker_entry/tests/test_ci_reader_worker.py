"""Reader Worker の設定とデプロイ経路の契約テスト。資格情報・ネットワークは使わない。

同じ Rust 成果物を 2 つの script として配備する:

- writer (既存): 更新と管理。queue / cron / RATE_LIMITER / 静的アセットを持つ。
- reader (新規): 配信だけ。既存ドメインの配信パスだけを edge で受け取り、
  D1 / S3 / 認証は writer と同一。queue / cron / RATE_LIMITER / 静的アセット /
  ビルドは持たず、writer の script 名と PushHub DO を参照する。

Run: python -m unittest discover -s worker_entry/tests -p 'test_ci_reader_worker.py'
"""

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import tomllib
import unittest
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[1]


def load_module(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / "ci" / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


RENDER = load_module("render_config")
DEPLOY = load_module("deploy_worker")

ENV = {
    "NAROU_DEPLOY_TARGET": "develop",
    "CLOUDFLARE_ACCOUNT_ID": "fake-account",
    "CLOUDFLARE_API_TOKEN": "fake-cloudflare-token",
    # require_probe_auth() は CI 側トークンに 32 文字以上を要求する。
    "NAROU_ADMIN_TOKEN": "fake-admin-token-0123456789abcdef",
    "NAROU_D1_DATABASE_NAME": "narou-rs-develop",
    "NAROU_D1_DATABASE_ID": "12345678-1234-1234-1234-123456789012",
    "NAROU_JOB_QUEUE": "narou-jobs-develop",
    "NAROU_JOB_DLQ": "narou-jobs-develop-dlq",
    "NAROU_CONVERT_QUEUE": "narou-jobs-convert-develop",
    "NAROU_CONVERT_DLQ": "narou-jobs-convert-develop-dlq",
    "NAROU_S3_ENDPOINT": "https://s3.example.invalid",
    "NAROU_S3_REGION": "us-east-1",
    "NAROU_S3_BUCKET": "fake-bucket",
}

TEMPLATES = ("wrangler.develop.toml", "wrangler.production.toml", "wrangler.reader.toml")


class RenderCase(unittest.TestCase):
    """実テンプレートを隔離ディレクトリへ置いて描画する土台。"""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        for name in TEMPLATES:
            self.write_template(name, (ROOT / name).read_text(encoding="utf-8"))

    def write_template(self, name, text):
        (self.directory / name).write_text(text, encoding="utf-8")

    def render(self, role, extra=None, args=None):
        environment = {**ENV, **(extra or {})}
        with patch.dict(os.environ, environment, clear=True), \
                patch.object(RENDER, "WORKER_DIR", self.directory), \
                contextlib.redirect_stdout(io.StringIO()):
            RENDER.main(args if args is not None else ["--role", role])
        name = "wrangler.ci.toml" if role == "writer" else "wrangler.reader.ci.toml"
        return tomllib.loads((self.directory / name).read_text(encoding="utf-8"))


class ReaderConfigTests(RenderCase):
    def test_the_reader_is_the_writer_plus_a_suffix(self):
        writer = self.render("writer")
        reader = self.render("reader")
        self.assertEqual(writer["name"], "narou-rs-develop")
        self.assertEqual(reader["name"], "narou-rs-develop-reader")

    def test_the_reader_declares_the_reader_role(self):
        reader = self.render("reader")
        self.assertEqual(reader["vars"]["NAROU_WORKER_ROLE"], "reader")
        writer = self.render("writer")
        self.assertEqual(writer["vars"]["NAROU_WORKER_ROLE"], "writer")

    def test_the_reader_shares_the_database_and_storage_with_the_writer(self):
        writer = self.render("writer")
        reader = self.render("reader")
        self.assertEqual(writer["d1_databases"], reader["d1_databases"])
        for key in ("S3_ENDPOINT", "S3_REGION", "S3_BUCKET", "S3_PREFIX", "NAROU_AUTH_REQUIRED"):
            self.assertEqual(writer["vars"][key], reader["vars"][key])
        # キュー名は reader には要らない (queue を持たない)。
        self.assertNotIn("CONVERT_QUEUE_NAME", reader["vars"])

    def test_the_reader_has_no_queue_cron_or_rate_limiter(self):
        reader = self.render("reader")
        self.assertNotIn("queues", reader)
        self.assertNotIn("triggers", reader)
        self.assertNotIn("migrations", reader)
        self.assertNotIn("exports", reader)
        bindings = {entry["name"] for entry in reader["durable_objects"]["bindings"]}
        self.assertEqual(bindings, {"PUSH_HUB"})

    def test_the_reader_has_no_static_assets_and_does_not_build(self):
        reader = self.render("reader")
        self.assertNotIn("assets", reader)
        self.assertNotIn("build", reader)
        # 同じ成果物を共有する (main が同じファイルを指す)。
        writer = self.render("writer")
        self.assertEqual(writer["main"], reader["main"])

    def test_the_reader_binds_the_writer_service_and_push_hub(self):
        reader = self.render("reader")
        services = {entry["binding"]: entry["service"] for entry in reader["services"]}
        self.assertEqual(services, {"NAROU_WRITER": "narou-rs-develop"})
        push = reader["durable_objects"]["bindings"][0]
        self.assertEqual(push["name"], "PUSH_HUB")
        self.assertEqual(push["class_name"], "PushHub")
        # writer の DO 名前空間を使う (reader は新しい名前空間を作らない)。
        self.assertEqual(push["script_name"], "narou-rs-develop")

    def test_the_reader_routes_only_the_delivery_paths(self):
        reader = self.render("reader", {"DEVELOP_DOMAIN": "app.example.invalid"})
        patterns = {route["pattern"] for route in reader["routes"]}
        self.assertEqual(
            patterns,
            {
                "app.example.invalid/api/v1*",
                "app.example.invalid/api/read/v1*",
                "app.example.invalid/api/novels*",
                "app.example.invalid/api/virtual_novels*",
                "app.example.invalid/novels*",
            },
        )
        # 既存ドメインの DNS は触らない (custom domain ではなく route)。
        for route in reader["routes"]:
            self.assertFalse(route["custom_domain"])

    def test_the_writer_keeps_its_own_domain_and_the_reader_stays_on_routes(self):
        writer = self.render("writer", {"DEVELOP_DOMAIN": "app.example.invalid"})
        self.assertEqual(
            [route["pattern"] for route in writer["routes"]],
            ["app.example.invalid"],
        )
        self.assertTrue(writer["routes"][0]["custom_domain"])

    def test_without_a_domain_the_reader_has_no_route_and_keeps_workers_dev(self):
        reader = self.render("reader")
        self.assertNotIn("routes", reader)
        self.assertTrue(reader["workers_dev"])

    def test_a_domain_closes_workers_dev_for_both_scripts(self):
        extra = {"DEVELOP_DOMAIN": "app.example.invalid"}
        self.assertFalse(self.render("writer", extra)["workers_dev"])
        self.assertFalse(self.render("reader", extra)["workers_dev"])

    def test_the_reader_secrets_store_excludes_the_login_key(self):
        extra = {
            "NAROU_SECRETS_STORE_ID": "fake-store",
            "NAROU_ADMIN_TOKEN_SECRET_NAME": "admin-token",
            "NAROU_RS_LOGIN_KEY_SECRET_NAME": "login-key",
        }
        reader = self.render("reader", extra)
        bindings = {row["binding"] for row in reader["secrets_store_secrets"]}
        self.assertEqual(bindings, {"NAROU_ADMIN_TOKEN_STORE"})
        writer = self.render("writer", extra)
        bindings = {row["binding"] for row in writer["secrets_store_secrets"]}
        self.assertEqual(
            bindings, {"NAROU_ADMIN_TOKEN_STORE", "NAROU_RS_LOGIN_KEY_STORE"}
        )

    def test_a_reader_with_a_queue_is_refused(self):
        template = (ROOT / "wrangler.reader.toml").read_text(encoding="utf-8")
        self.write_template(
            "wrangler.reader.toml",
            template.replace(
                "[[d1_databases]]",
                '[[queues.producers]]\nbinding = "NAROU_JOBS"\nqueue = "narou-jobs-develop"\n\n[[d1_databases]]',
            ),
        )
        with self.assertRaises(SystemExit):
            self.render("reader")

    def test_a_reader_with_a_cron_trigger_is_refused(self):
        template = (ROOT / "wrangler.reader.toml").read_text(encoding="utf-8")
        self.write_template(
            "wrangler.reader.toml",
            template + '\n[triggers]\ncrons = ["* * * * *"]\n',
        )
        with self.assertRaises(SystemExit):
            self.render("reader")

    def test_a_reader_with_its_own_build_is_refused(self):
        template = (ROOT / "wrangler.reader.toml").read_text(encoding="utf-8")
        self.write_template(
            "wrangler.reader.toml",
            template + '\n[build]\ncommand = "worker-build --release"\n',
        )
        with self.assertRaises(SystemExit):
            self.render("reader")

    def test_a_reader_without_the_role_is_refused(self):
        template = (ROOT / "wrangler.reader.toml").read_text(encoding="utf-8")
        self.write_template(
            "wrangler.reader.toml",
            template.replace('NAROU_WORKER_ROLE = "reader"\n', ""),
        )
        with self.assertRaises(SystemExit):
            self.render("reader")

    def test_a_reader_that_owns_a_durable_object_namespace_is_refused(self):
        template = (ROOT / "wrangler.reader.toml").read_text(encoding="utf-8")
        self.write_template(
            "wrangler.reader.toml",
            template + '\n[[migrations]]\ntag = "v1"\nnew_sqlite_classes = ["PushHub"]\n',
        )
        with self.assertRaises(SystemExit):
            self.render("reader")


class ReaderSecretTests(unittest.TestCase):
    """reader へ渡す secret は最小権限 (writer 独有的ものは渡さない)。"""

    def build(self, role, extra=None):
        with tempfile.TemporaryDirectory() as directory, \
                patch.dict(os.environ, {**ENV, **(extra or {})}, clear=True), \
                patch.object(DEPLOY, "WORKER_DIR", Path(directory)), \
                contextlib.redirect_stdout(io.StringIO()):
            path = DEPLOY.secret_file("develop", role=role)
            if path is None:
                return {}
            return json.loads(path.read_text(encoding="utf-8"))

    def test_the_reader_gets_no_login_key_and_no_relay(self):
        payload = self.build(
            "reader",
            {
                "NAROU_RS_LOGIN_KEY": "bG9naW4ta2V5LWtleS0zMi1ieXRlcy1sb25n",
                "NAROU_RELAY_ENDPOINT": "https://relay.example.invalid",
                "NAROU_RELAY_KEY": "relay-key",
                "NAROU_S3_SECRET_ACCESS_KEY": "fake-s3-secret",
            },
        )
        self.assertNotIn("NAROU_RS_LOGIN_KEY", payload)
        self.assertNotIn("NAROU_RELAY_ENDPOINT", payload)
        self.assertNotIn("NAROU_RELAY_KEY", payload)
        # 認証と S3 の読み書き (EPUB の staging を含む) は reader にも要る。
        self.assertEqual(payload["NAROU_ADMIN_TOKEN"], ENV["NAROU_ADMIN_TOKEN"])
        self.assertEqual(payload["S3_SECRET_ACCESS_KEY"], "fake-s3-secret")

    def test_the_writer_keeps_the_login_key_and_the_relay(self):
        payload = self.build(
            "writer",
            {
                "NAROU_RS_LOGIN_KEY": "bG9naW4ta2V5LWtleS0zMi1ieXRlcy1sb25n",
                "NAROU_RELAY_ENDPOINT": "https://relay.example.invalid",
                "NAROU_RELAY_KEY": "relay-key",
            },
        )
        self.assertIn("NAROU_RS_LOGIN_KEY", payload)
        self.assertEqual(payload["NAROU_RELAY_ENDPOINT"], "https://relay.example.invalid")
        self.assertEqual(payload["NAROU_RELAY_KEY"], "relay-key")


class ReaderDeployTests(unittest.TestCase):
    """デプロイは writer を先に、reader を後に置く (冪等に同じ設定で)。"""

    def setUp(self):
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)
        self.stack.enter_context(patch.dict(os.environ, ENV, clear=True))
        self.stack.enter_context(contextlib.redirect_stdout(io.StringIO()))
        self.calls = Mock()
        values = {
            "provision": {
                "database_name": "narou-rs-develop",
                "database_id": ENV["NAROU_D1_DATABASE_ID"],
                "queue_name": "narou-jobs-develop",
                "dead_letter_queue": "narou-jobs-develop-dlq",
                "convert_queue_name": "narou-jobs-convert-develop",
                "convert_dead_letter_queue": "narou-jobs-convert-develop-dlq",
            },
            "render": "narou-rs-develop",
            "enable_read_replication": None,
            "run": "",
            "secret_file": None,
            "deploy": None,
            "smoke_url": ("https://writer.example.workers.dev", False),
            "reader_smoke_url": ("https://reader.example.workers.dev", False),
            "verify_s3": "ok",
            "verify_writer_ready": "ok",
            "smoke": True,
            "summary": None,
        }
        for name, value in values.items():
            mock = self.stack.enter_context(patch.object(DEPLOY, name, return_value=value))
            self.calls.attach_mock(mock, name)
        # writer と reader は別の script として、writer の Reported URL を返す。
        self.calls.deploy.side_effect = [
            "https://writer.example.workers.dev",
            "https://reader.example.workers.dev",
        ]

    def test_the_writer_readiness_runs_before_the_reader_is_deployed(self):
        # reader は source を変更しないので、配信 path を移す前に writer の
        # 初期化 (属性の backfill) が済んでいる必要がある。
        DEPLOY.main()
        order = [name for name, _, _ in self.calls.mock_calls]
        first = order.index("deploy")
        second = order.index("deploy", first + 1)
        ready = order.index("verify_writer_ready")
        self.assertLess(first, ready)
        self.assertLess(ready, second)

    def test_an_unverified_writer_readiness_stops_the_reader_deployment(self):
        # writer の readiness を確認できないときは reader を配備しない。
        # 既存の配信 Route は変更しない (初回なら writer、更新時なら既存 reader)。
        self.calls.verify_writer_ready.side_effect = SystemExit(
            "the writer's readiness could not be reached"
        )
        with self.assertRaises(SystemExit):
            DEPLOY.main()
        self.assertEqual(self.calls.deploy.call_count, 1)
        self.calls.summary.assert_not_called()

    def test_reader_secret_creation_failure_removes_the_writer_secret_file(self):
        with tempfile.TemporaryDirectory() as directory:
            writer = Path(directory) / "writer-secrets.json"
            writer.write_text('{"synthetic":"secret"}', encoding="utf-8")
            self.calls.secret_file.side_effect = [writer, OSError("reader secret creation failed")]
            with self.assertRaises(OSError):
                DEPLOY.main()
            self.assertFalse(writer.exists())
        self.calls.deploy.assert_not_called()

    def test_a_failing_reader_smoke_fails_the_deployment(self):
        self.calls.smoke.side_effect = [True, False]
        with self.assertRaises(SystemExit):
            DEPLOY.main()


class LocalReaderConfigTests(unittest.TestCase):
    """ローカルの reader 設定も CI と同じ形 (配信だけ / writer を参照する)。"""

    def setUp(self):
        self.reader = tomllib.loads(
            (ROOT / "wrangler.reader.local.toml").read_text(encoding="utf-8")
        )
        self.writer = tomllib.loads((ROOT / "wrangler.toml").read_text(encoding="utf-8"))

    def test_the_local_reader_serves_reads_only(self):
        self.assertEqual(self.reader["vars"]["NAROU_WORKER_ROLE"], "reader")
        self.assertNotIn("queues", self.reader)
        self.assertNotIn("triggers", self.reader)
        self.assertNotIn("assets", self.reader)
        self.assertNotIn("build", self.reader)
        self.assertNotIn("migrations", self.reader)

    def test_the_local_reader_points_at_the_local_writer(self):
        services = {entry["binding"]: entry["service"] for entry in self.reader["services"]}
        self.assertEqual(services, {"NAROU_WRITER": self.writer["name"]})
        push = self.reader["durable_objects"]["bindings"][0]
        self.assertEqual(push["name"], "PUSH_HUB")
        self.assertEqual(push["script_name"], self.writer["name"])
        # 同じ Rust 成果物を共有する (ビルドは 1 回だけ)。
        self.assertEqual(self.reader["main"], self.writer["main"])
        self.assertEqual(self.reader["d1_databases"], self.writer["d1_databases"])


class ReaderUrlTests(unittest.TestCase):
    """reader の smoke 宛先は reader 自身 (writer の URL ではない)。"""

    def setUp(self):
        self.env = patch.dict(os.environ, ENV, clear=True)
        self.env.start()
        self.addCleanup(self.env.stop)

    def test_without_a_domain_the_reader_uses_its_own_workers_dev_url(self):
        url, hidden = DEPLOY.reader_smoke_url("develop", "https://reader.example.workers.dev")
        self.assertEqual(url, "https://reader.example.workers.dev")
        self.assertFalse(hidden)

    def test_a_domain_serves_the_delivery_paths_of_the_reader(self):
        os.environ["DEVELOP_DOMAIN"] = "app.example.invalid"
        url, hidden = DEPLOY.reader_smoke_url("develop", "https://reader.example.workers.dev")
        self.assertEqual(url, "https://app.example.invalid")
        self.assertTrue(hidden)

    def test_an_explicit_reader_url_wins(self):
        os.environ["NAROU_READER_DEPLOY_URL"] = "https://reader.example.invalid"
        url, _ = DEPLOY.reader_smoke_url("develop", "https://reader.example.workers.dev")
        self.assertEqual(url, "https://reader.example.invalid")


if __name__ == "__main__":
    unittest.main()
