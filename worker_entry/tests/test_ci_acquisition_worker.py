"""Offline contracts for the writer + reader + four acquisition scripts."""

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest
import urllib.error
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]


def load_module(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / "ci" / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


RENDER = load_module("render_config")
PROVISION = load_module("provision_resources")
DEPLOY = load_module("deploy_worker")
DATABASE_ID = "12345678-1234-1234-1234-123456789012"


def resources(target):
    values = {
        "database_name": f"narou-rs-{target}",
        "database_id": DATABASE_ID,
        "queue_name": f"narou-jobs-{target}",
        "dead_letter_queue": f"narou-jobs-{target}-dlq",
        "convert_queue_name": f"narou-jobs-convert-{target}",
        "convert_dead_letter_queue": f"narou-jobs-convert-{target}-dlq",
    }
    for number in range(1, 5):
        values[f"acquire_queue_name_{number}"] = f"narou-jobs-acquire-{number}-{target}"
        values[f"acquire_dead_letter_queue_{number}"] = f"narou-jobs-acquire-{number}-{target}-dlq"
    return values


def environment(target):
    values = resources(target)
    result = {
        "NAROU_DEPLOY_TARGET": target,
        "NAROU_D1_DATABASE_NAME": values["database_name"],
        "NAROU_D1_DATABASE_ID": values["database_id"],
        "NAROU_JOB_QUEUE": values["queue_name"],
        "NAROU_JOB_DLQ": values["dead_letter_queue"],
        "NAROU_CONVERT_QUEUE": values["convert_queue_name"],
        "NAROU_CONVERT_DLQ": values["convert_dead_letter_queue"],
        "NAROU_S3_ENDPOINT": "https://s3.example.invalid",
        "NAROU_S3_REGION": "us-east-1",
        "NAROU_S3_BUCKET": "synthetic-bucket",
        "SERVICE_DOMAIN": "app.example.invalid",
    }
    for number in range(1, 5):
        result[f"NAROU_ACQUIRE_QUEUE_{number}"] = values[f"acquire_queue_name_{number}"]
        result[f"NAROU_ACQUIRE_DLQ_{number}"] = values[f"acquire_dead_letter_queue_{number}"]
    return result


class AcquisitionConfigTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        for name in (
            "wrangler.develop.toml", "wrangler.production.toml",
            "wrangler.reader.toml", "wrangler.acquisition.toml",
        ):
            (self.directory / name).write_text((ROOT / name).read_text(encoding="utf-8"), encoding="utf-8")

    def render(self, role, target="develop", lane=None, extra=None):
        args = ["--role", role]
        if lane is not None:
            args += ["--lane", str(lane)]
        with patch.dict(os.environ, environment(target) | (extra or {}), clear=True), \
                patch.object(RENDER, "WORKER_DIR", self.directory), \
                contextlib.redirect_stdout(io.StringIO()):
            RENDER.main(args)
        name = {
            "writer": "wrangler.ci.toml",
            "reader": "wrangler.reader.ci.toml",
            "acquisition": f"wrangler.acquire-{lane + 1}.ci.toml" if lane is not None else "",
        }[role]
        return tomllib.loads((self.directory / name).read_text(encoding="utf-8"))

    def assert_topology(self, writer, reader, lanes, target):
        jobs = f"narou-jobs-{target}"
        convert = f"narou-jobs-convert-{target}"
        queue_names = [f"narou-jobs-acquire-{number}-{target}" for number in range(1, 5)]
        lane_producers = {f"ACQUISITION_QUEUE_{lane}": queue for lane, queue in enumerate(queue_names)}
        self.assertEqual(writer["vars"]["NAROU_WORKER_ROLE"], "writer")
        self.assertEqual(reader["name"], f"{writer['name']}-reader")
        self.assertEqual(len({config["name"] for config in (writer, reader, *lanes)}), 6)
        self.assertEqual(
            {row["binding"]: row["queue"] for row in writer["queues"]["producers"]},
            {"NAROU_JOBS": jobs, "CONVERT_QUEUE": convert, **lane_producers},
        )
        self.assertEqual(len(writer["queues"]["consumers"]), 2)
        self.assertEqual({row["queue"] for row in writer["queues"]["consumers"]}, {jobs, convert})
        for consumer in writer["queues"]["consumers"]:
            self.assertEqual(consumer["max_batch_size"], 1)
            self.assertEqual(consumer["max_concurrency"], 1)
            self.assertEqual(consumer["dead_letter_queue"], f"{consumer['queue']}-dlq")
        self.assertIn("assets", writer)
        self.assertIn("triggers", writer)
        self.assertIn("migrations", writer)
        self.assertNotIn("queues", reader)
        self.assertNotIn("build", reader)
        self.assertEqual(reader["d1_databases"], writer["d1_databases"])
        self.assertEqual(reader["main"], writer["main"])
        for lane, config in enumerate(lanes):
            with self.subTest(target=target, lane=lane):
                self.assertEqual(config["name"], f"{writer['name']}-acquire-{lane + 1}")
                self.assertEqual(config["vars"]["NAROU_WORKER_ROLE"], "acquisition")
                self.assertEqual(config["vars"]["NAROU_ACQUISITION_LANE"], str(lane))
                self.assertEqual(config["main"], writer["main"])
                self.assertEqual(config["d1_databases"], writer["d1_databases"])
                for key in ("S3_ENDPOINT", "S3_REGION", "S3_BUCKET", "S3_PREFIX", "NAROU_AUTH_REQUIRED"):
                    self.assertEqual(config["vars"][key], writer["vars"][key])
                self.assertEqual(config["vars"]["CONVERT_QUEUE_NAME"], convert)
                self.assertFalse(config["workers_dev"])
                for key in ("assets", "routes", "triggers", "migrations", "exports", "build"):
                    self.assertNotIn(key, config)
                self.assertEqual(
                    {row["binding"]: row["queue"] for row in config["queues"]["producers"]},
                    {"CONVERT_QUEUE": convert, **lane_producers},
                )
                self.assertEqual(config["queues"]["consumers"], [{
                    "queue": queue_names[lane], "max_batch_size": 1, "max_retries": 3,
                    "dead_letter_queue": f"{queue_names[lane]}-dlq", "max_concurrency": 1,
                }])
                self.assertEqual(config["services"], [{"binding": "NAROU_WRITER", "service": writer["name"]}])
                self.assertEqual(config["durable_objects"]["bindings"], [
                    {"name": "RATE_LIMITER", "class_name": "SiteRateLimiter", "script_name": writer["name"]},
                    {"name": "PUSH_HUB", "class_name": "PushHub", "script_name": writer["name"]},
                ])

    def test_develop_and_production_render_all_six_scripts(self):
        for target in ("develop", "production"):
            with self.subTest(target=target):
                writer = self.render("writer", target)
                reader = self.render("reader", target)
                lanes = [self.render("acquisition", target, lane) for lane in range(4)]
                self.assert_topology(writer, reader, lanes, target)
                self.assertNotIn("build", writer)
                for config in (writer, reader, *lanes):
                    self.assertEqual(config["vars"]["NAROU_REQUIRE_S3"], "true")

    def test_checked_in_local_configs_have_the_same_topology(self):
        def config(name):
            return tomllib.loads((ROOT / name).read_text(encoding="utf-8"))
        self.assert_topology(
            config("wrangler.toml"), config("wrangler.reader.local.toml"),
            [config(f"wrangler.acquire-{number}.local.toml") for number in range(1, 5)],
            "local",
        )

    def test_acquisition_uses_the_writer_secrets_store_bindings(self):
        extra = {
            "NAROU_SECRETS_STORE_ID": "synthetic-store",
            "NAROU_ADMIN_TOKEN_SECRET_NAME": "admin-token",
            "NAROU_RS_LOGIN_KEY_SECRET_NAME": "login-key",
            "NAROU_CF_ACCESS_CLIENT_ID_SECRET_NAME": "access-id",
            "NAROU_CF_ACCESS_CLIENT_SECRET_SECRET_NAME": "access-secret",
            **{f"NAROU_S3_{name}_SECRET_NAME": name.lower() for name in (
                "ACCESS_KEY_ID", "SECRET_ACCESS_KEY", "ENDPOINT", "REGION", "BUCKET",
            )},
        }
        writer = self.render("writer", extra=extra)
        for lane in range(4):
            acquisition = self.render("acquisition", lane=lane, extra=extra)
            self.assertEqual(acquisition["secrets_store_secrets"], writer["secrets_store_secrets"])
            self.assertEqual(acquisition["vars"]["S3_ENDPOINT"], "")
        reader = self.render("reader", extra=extra)
        self.assertNotIn("NAROU_RS_LOGIN_KEY_STORE", {row["binding"] for row in reader["secrets_store_secrets"]})

    def test_private_lanes_ignore_public_workers_dev_override(self):
        config = self.render("acquisition", lane=0, extra={"NAROU_WORKERS_DEV": "true"})
        self.assertFalse(config["workers_dev"])
        self.assertNotIn("routes", config)

    def test_acquisition_templates_cannot_widen_their_execution_topology(self):
        original = (ROOT / "wrangler.acquisition.toml").read_text(encoding="utf-8")
        variants = {
            "wrong queue": original.replace('queue = "__NAROU_ACQUIRE_QUEUE__"', 'queue = "__NAROU_JOB_QUEUE__"'),
            "wrong lane": original.replace('NAROU_ACQUISITION_LANE = "__ACQUISITION_LANE__"', 'NAROU_ACQUISITION_LANE = "3"'),
            "wrong role": original.replace('NAROU_WORKER_ROLE = "acquisition"', 'NAROU_WORKER_ROLE = "writer"'),
            "concurrent consumer": original.replace("max_concurrency = 1", "max_concurrency = 2"),
            "larger batch": original.replace("max_batch_size = 1", "max_batch_size = 2"),
            "wrong dlq": original.replace('dead_letter_queue = "__NAROU_ACQUIRE_DLQ__"', 'dead_letter_queue = "__NAROU_JOB_DLQ__"'),
            "missing producer": original.replace('binding = "ACQUISITION_QUEUE_3"', 'binding = "ACQUISITION_QUEUE_4"'),
            "own DO": original.replace('script_name = "__WRITER_NAME__"', 'script_name = "__ACQUISITION_NAME__"'),
            "public ingress": original.replace("workers_dev = false", "workers_dev = true"),
            "assets": original + '\n[assets]\ndirectory = "public"\n',
            "build": original + '\n[build]\ncommand = "worker-build --release"\n',
            "cron": original + '\n[triggers]\ncrons = ["* * * * *"]\n',
            "route": original + '\n[[routes]]\npattern = "app.example.invalid/*"\n',
            "DO migration": original + '\n[[migrations]]\ntag = "v3"\nnew_sqlite_classes = ["PushHub"]\n',
        }
        for label, template in variants.items():
            with self.subTest(label=label):
                (self.directory / "wrangler.acquisition.toml").write_text(template, encoding="utf-8")
                with self.assertRaises(SystemExit):
                    self.render("acquisition", lane=0)

    def test_writer_cannot_execute_acquisitions_or_rebuild_the_shared_artifact(self):
        original = (ROOT / "wrangler.develop.toml").read_text(encoding="utf-8")
        for template in (
            original.replace("max_concurrency = 1", "max_concurrency = 4", 1),
            original + '\n[build]\ncommand = "worker-build --release"\n',
            original + '\n[[queues.consumers]]\nqueue = "__NAROU_ACQUIRE_QUEUE_1__"\n',
        ):
            with self.subTest(template=template[-80:]):
                (self.directory / "wrangler.develop.toml").write_text(template, encoding="utf-8")
                with self.assertRaises(SystemExit):
                    self.render("writer")

    def test_missing_duplicate_or_mismatched_lane_queue_values_are_rejected(self):
        for extra in (
            {"NAROU_ACQUIRE_QUEUE_1": ""},
            {"NAROU_ACQUIRE_QUEUE_1": "narou-jobs-develop", "NAROU_ACQUIRE_DLQ_1": "narou-jobs-develop-dlq"},
            {"NAROU_ACQUIRE_QUEUE_1": "narou-jobs-acquire-2-develop", "NAROU_ACQUIRE_DLQ_1": "narou-jobs-acquire-2-develop-dlq"},
            {"NAROU_ACQUIRE_DLQ_1": "unrelated-dlq"},
            {"NAROU_ACQUIRE_QUEUE_1": "narou-jobs-acquire-1-production"},
        ):
            with self.subTest(extra=extra), self.assertRaises(SystemExit):
                self.render("acquisition", lane=0, extra=extra)

    def test_lane_arguments_are_explicit_and_zero_based(self):
        for lane in range(4):
            self.assertEqual(RENDER.parse_options(["--role=acquisition", f"--lane={lane}"]), ("acquisition", lane))
        for args in (
            ["--role", "acquisition"], ["--role", "acquisition", "--lane", "4"],
            ["--role", "acquisition", "--lane", "-1"], ["--role", "acquisition", "--lane", "x"],
            ["--role", "writer", "--lane", "0"], ["--role", "reader", "--lane", "0"],
            ["--role"], ["--lane"], ["--unknown"],
        ):
            with self.subTest(args=args), self.assertRaises(SystemExit):
                RENDER.parse_options(args)

    def test_first_writer_bootstrap_is_private_but_keeps_shared_do_namespaces(self):
        writer = self.render("writer")
        uploaded = []

        def upload(secrets, path):
            uploaded.append((json.loads(Path(path).read_text(encoding="utf-8")), Path(path)))
            return 0, "private owner created"

        with patch.object(DEPLOY, "WORKER_DIR", self.directory), \
                patch.object(DEPLOY, "writer_exists", return_value=False), \
                patch.object(DEPLOY, "run_deploy", side_effect=upload), \
                contextlib.redirect_stdout(io.StringIO()):
            DEPLOY.bootstrap_writer(None)
        bootstrap, path = uploaded[0]
        self.assertEqual(bootstrap["name"], writer["name"])
        self.assertEqual(bootstrap["migrations"], writer["migrations"])
        self.assertEqual(bootstrap["durable_objects"], writer["durable_objects"])
        self.assertEqual(bootstrap["queues"]["producers"], writer["queues"]["producers"])
        self.assertEqual(bootstrap["d1_databases"], writer["d1_databases"])
        self.assertEqual(bootstrap["vars"], writer["vars"])
        self.assertEqual(bootstrap["main"], str((self.directory / writer["main"]).resolve()))
        self.assertFalse(bootstrap["workers_dev"])
        self.assertFalse(bootstrap["preview_urls"])
        self.assertNotIn("consumers", bootstrap["queues"])
        for key in ("assets", "routes", "route", "triggers", "build"):
            self.assertNotIn(key, bootstrap)
        self.assertFalse(path.exists(), "bootstrap configuration must be removed after upload")
        self.assertEqual(tomllib.loads((self.directory / DEPLOY.WRITER_CONFIG).read_text(encoding="utf-8")), writer)

    def test_existing_writer_is_not_replaced_by_private_bootstrap(self):
        writer = self.render("writer")
        with patch.object(DEPLOY, "WORKER_DIR", self.directory), \
                patch.object(DEPLOY, "writer_exists", return_value=True), \
                patch.object(DEPLOY, "run_deploy", side_effect=AssertionError("existing writer must not be bootstrapped")):
            DEPLOY.bootstrap_writer(None)
        self.assertFalse((self.directory / ".wrangler").exists())
        self.assertEqual(tomllib.loads((self.directory / DEPLOY.WRITER_CONFIG).read_text(encoding="utf-8")), writer)

    def test_failed_bootstrap_stops_cutover_and_removes_its_configuration(self):
        self.render("writer")
        with patch.object(DEPLOY, "WORKER_DIR", self.directory), \
                patch.object(DEPLOY, "writer_exists", return_value=False), \
                patch.object(DEPLOY, "run_deploy", return_value=(1, "fixture failure")), \
                contextlib.redirect_stdout(io.StringIO()), self.assertRaises(SystemExit):
            DEPLOY.bootstrap_writer(None)
        self.assertEqual(list((self.directory / ".wrangler").iterdir()), [])


class WriterDiscoveryTests(unittest.TestCase):
    def lookup(self, body=None, error=None):
        with patch.dict(os.environ, {
            "CLOUDFLARE_ACCOUNT_ID": "synthetic-account",
            "CLOUDFLARE_API_TOKEN": "synthetic-token",
        }, clear=True), patch.object(DEPLOY.urllib.request, "build_opener") as factory:
            if error is not None:
                factory.return_value.open.side_effect = error
            else:
                factory.return_value.open.return_value = io.BytesIO(body)
            return DEPLOY.writer_exists("narou-rs-fixture")

    def test_existing_writer_is_recognized_from_successful_metadata(self):
        self.assertTrue(self.lookup(json.dumps({"success": True, "result": {}}).encode()))

    def test_only_explicit_missing_worker_response_permits_bootstrap(self):
        body = json.dumps({"success": False, "errors": [{"code": 10007}]}).encode()
        missing = urllib.error.HTTPError("https://api.cloudflare.com/", 404, "not found", {}, io.BytesIO(body))
        self.assertFalse(self.lookup(error=missing))

    def test_auth_redirect_unknown_404_and_invalid_metadata_fail_closed(self):
        for status, body in ((403, b"forbidden"), (302, b"redirect"), (404, b"unknown route"),
                (404, b'{"success":false,"errors":[{"code":10000}]}')):
            with self.subTest(status=status, body=body), self.assertRaises(SystemExit):
                self.lookup(error=urllib.error.HTTPError("https://api.cloudflare.com/", status, "fixture", {}, io.BytesIO(body)))
        for body in (b"not JSON", b'{"success":false,"result":{}}', b'{"success":true,"result":null}', b"x" * 65537):
            with self.subTest(body=body[:32]), self.assertRaises(SystemExit):
                self.lookup(body)
        with self.assertRaises(SystemExit):
            self.lookup(error=urllib.error.URLError("fixture unreachable"))


class ProvisioningTests(unittest.TestCase):
    def test_provisioning_adds_four_lane_queues_and_dlqs_and_keeps_the_old_queues(self):
        for target in ("develop", "production"):
            with self.subTest(target=target):
                expected = resources(target)
                queues = {
                    expected[key] for key in (
                        "queue_name", "dead_letter_queue", "convert_queue_name", "convert_dead_letter_queue",
                    )
                }
                created = []

                def wrangler(*args):
                    if args == ("d1", "list", "--json"):
                        output = json.dumps([{"name": expected["database_name"], "uuid": DATABASE_ID}])
                    elif args == ("queues", "list"):
                        output = "\n".join(f"│ queue-{index} │ {name} │ ready │" for index, name in enumerate(sorted(queues)))
                    elif args[:2] == ("queues", "create"):
                        created.append(args[2])
                        queues.add(args[2])
                        output = "created"
                    else:
                        raise AssertionError(f"unexpected remote operation: {args}")
                    return subprocess.CompletedProcess([], 0, output, "")

                with patch.dict(os.environ, {"NAROU_DEPLOY_TARGET": target}, clear=True), \
                        patch.object(PROVISION, "wrangler", side_effect=wrangler), \
                        patch.object(PROVISION, "write_outputs") as output:
                    PROVISION.main()
                    PROVISION.main()
                new_names = {
                    expected[f"acquire_{kind}_{number}"]
                    for number in range(1, 5) for kind in ("queue_name", "dead_letter_queue")
                }
                self.assertEqual(set(created), new_names)
                self.assertEqual(len(created), 8, "the second provisioning must not recreate any queue")
                self.assertEqual(output.call_args.args[0], expected)
                self.assertEqual(len(queues), 12)

    def test_lane_dlq_name_length_is_checked_before_remote_operations(self):
        with patch.dict(os.environ, {"NAROU_DEPLOY_TARGET": "production", "NAROU_JOB_QUEUE_BASE": "a" * 40}, clear=True), \
                patch.object(PROVISION, "wrangler") as wrangler, self.assertRaises(SystemExit):
            PROVISION.main()
        wrangler.assert_not_called()




if __name__ == "__main__":
    unittest.main()

