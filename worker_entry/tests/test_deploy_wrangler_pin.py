"""wrangler のバージョン固定の契約テスト。ネットワーク・資格情報は使わない。

デプロイ (`wrangler deploy`) と D1 migration (`wrangler d1 migrations apply`) は
`wrangler@4` のような範囲指定ではなく、完全一致のバージョンで実行する。
既定は `WRANGLER_VERSION`、`NAROU_WRANGLER_VERSION` で `x.y.z` 形式に限り
上書きできる。

Run: python -m unittest discover -s worker_entry/tests -p 'test_deploy_wrangler_pin.py'
"""

import contextlib
import importlib.util
import io
import os
from pathlib import Path
import re
import subprocess
import unittest
from unittest.mock import Mock, patch

WORKER = Path(__file__).resolve().parents[1]

ENV = {
    "NAROU_DEPLOY_TARGET": "develop",
    "CLOUDFLARE_ACCOUNT_ID": "fake-account",
    "CLOUDFLARE_API_TOKEN": "fake-cloudflare-token",
    # require_probe_auth() は CI 側トークンに 32 文字以上を要求する。
    "NAROU_ADMIN_TOKEN": "fake-admin-token-0123456789abcdef",
}


def load_module(name):
    spec = importlib.util.spec_from_file_location(name, WORKER / "ci" / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


deploy = load_module("deploy_worker")
PINNED_REFERENCE = re.compile(r"^wrangler@\d+\.\d+\.\d+$")


def completed(stdout="", stderr=""):
    return subprocess.CompletedProcess([], 0, stdout, stderr)


class WranglerPinTests(unittest.TestCase):
    def test_default_version_is_an_exact_semver(self):
        self.assertRegex(deploy.WRANGLER_VERSION, r"^\d+\.\d+\.\d+$")
        self.assertEqual(deploy.WRANGLER_VERSION, "4.151.3")

    def test_deploy_command_pins_the_wrangler_version(self):
        with patch.object(deploy.subprocess, "run", return_value=completed()) as run:
            code, _ = deploy.run_deploy(None)
        self.assertEqual(code, 0)
        command = run.call_args.args[0]
        self.assertEqual(command[:3], ["npx", "--yes", f"wrangler@{deploy.WRANGLER_VERSION}"])
        self.assertRegex(command[2], PINNED_REFERENCE)
        self.assertEqual(command[3:], ["deploy", "-c", "wrangler.ci.toml"])

    def test_secrets_file_is_still_passed_to_the_pinned_deploy(self):
        with patch.object(deploy.subprocess, "run", return_value=completed()) as run:
            deploy.run_deploy(Path("secrets.json"))
        command = run.call_args.args[0]
        self.assertRegex(command[2], PINNED_REFERENCE)
        self.assertIn("--secrets-file", command)
        self.assertIn("secrets.json", command)

    def test_d1_migrations_use_the_same_pinned_version(self):
        values = {
            "provision": {
                "database_name": "narou-rs-develop",
                "database_id": "12345678-1234-1234-1234-123456789012",
                "queue_name": "narou-jobs-develop",
                "dead_letter_queue": "narou-jobs-develop-dlq",
                "convert_queue_name": "narou-jobs-convert-develop",
                "convert_dead_letter_queue": "narou-jobs-convert-develop-dlq",
            },
            "render": "narou-rs-develop",
            "enable_read_replication": None,
            "secret_file": None,
            "deploy": "https://worker.example.workers.dev",
            "smoke_url": ("https://worker.example.workers.dev", False),
            "verify_s3": "ok",
            "smoke": True,
            "summary": None,
        }
        with patch.dict(os.environ, ENV, clear=True), \
                patch.object(deploy, "run", wraps=deploy.run) as run, \
                contextlib.redirect_stdout(io.StringIO()):
            for name, value in values.items():
                self.enterContext(patch.object(deploy, name, return_value=value))
            self.enterContext(patch.object(deploy.subprocess, "run", return_value=completed()))
            deploy.main()
        command, = run.call_args.args
        self.assertEqual(command[:3], ["npx", "--yes", f"wrangler@{deploy.WRANGLER_VERSION}"])
        self.assertRegex(command[2], PINNED_REFERENCE)
        self.assertEqual(command[3:6], ["d1", "migrations", "apply"])
        self.assertIn("--remote", command)

    def test_environment_override_is_used_by_both_commands(self):
        with patch.dict(os.environ, {"NAROU_WRANGLER_VERSION": "4.200.1"}, clear=False), \
                patch.object(deploy.subprocess, "run", return_value=completed()) as run:
            deploy.run_deploy(None)
        self.assertEqual(run.call_args.args[0][2], "wrangler@4.200.1")

    def test_invalid_override_is_rejected_before_any_subprocess(self):
        for value in ("4", "4.1", "latest", "v4.151.3", "4.151.3-beta", "4.1.2.3", "4.151.3.0"):
            with self.subTest(value=value), \
                    patch.dict(os.environ, {"NAROU_WRANGLER_VERSION": value}, clear=False), \
                    patch.object(deploy.subprocess, "run") as run:
                with self.assertRaises(SystemExit):
                    deploy.run_deploy(None)
                run.assert_not_called()

    def test_wrangler_command_never_uses_a_floating_range(self):
        command = deploy.wrangler_command("deploy")
        self.assertRegex(command[2], PINNED_REFERENCE)
        self.assertNotEqual(command[2], "wrangler@4")

    def test_source_has_no_floating_wrangler_reference(self):
        source = (WORKER / "ci" / "deploy_worker.py").read_text(encoding="utf-8")
        self.assertNotIn('"wrangler@4"', source)
        # 定義 1 か所 + 呼び出し 2 か所 (deploy と D1 migration)。
        self.assertEqual(source.count("wrangler_command("), 3)


class AdminTokenFloorTests(unittest.TestCase):
    """CI 側の管理トークンは 32 文字以上 (総当たりを避ける最小長)。"""

    def test_short_ci_token_is_rejected(self):
        for token in ("short", "fake-admin-token", "x" * 31):
            with self.subTest(token=token), \
                    patch.dict(os.environ, {**ENV, "NAROU_ADMIN_TOKEN": token}, clear=True):
                with self.assertRaises(SystemExit) as failure:
                    deploy.require_probe_auth()
                self.assertIn("at least 32", str(failure.exception))

    def test_rejection_explains_existing_deployments_should_rotate(self):
        with patch.dict(os.environ, {**ENV, "NAROU_ADMIN_TOKEN": "x" * 31}, clear=True):
            with self.assertRaises(SystemExit) as failure:
                deploy.require_probe_auth()
        self.assertIn("rotate", str(failure.exception))

    def test_minimum_length_token_passes(self):
        with patch.dict(os.environ, {**ENV, "NAROU_ADMIN_TOKEN": "x" * 32}, clear=True):
            deploy.require_probe_auth()

    def test_store_only_token_is_not_length_checked(self):
        # Secrets Store 専用運用では CI 側に値が無い (読み戻せない) ので、
        # 長さの検査対象にしない。
        env = {**ENV}
        del env["NAROU_ADMIN_TOKEN"]
        env["NAROU_ADMIN_TOKEN_SECRET_NAME"] = "runtime-token"
        with patch.dict(os.environ, env, clear=True):
            deploy.require_probe_auth()

    def test_short_token_fails_before_provisioning(self):
        with patch.dict(os.environ, {**ENV, "NAROU_ADMIN_TOKEN": "x" * 31}, clear=True), \
                patch.object(deploy, "provision") as provision, \
                contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(SystemExit):
                deploy.main()
        provision.assert_not_called()


if __name__ == "__main__":
    unittest.main()
