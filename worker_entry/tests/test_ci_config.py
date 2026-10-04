"""Validate rendered deployment contracts without credentials or network."""
import contextlib
import importlib.util
import io
import os
from pathlib import Path
import tempfile
import tomllib
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("render_config", ROOT / "ci/render_config.py")
RENDER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RENDER)


class RenderConfigTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        (self.directory / "wrangler.develop.toml").write_text(
            (ROOT / "wrangler.develop.toml").read_text(encoding="utf-8"), encoding="utf-8")
        self.environment = {
            "NAROU_DEPLOY_TARGET": "develop",
            "NAROU_D1_DATABASE_NAME": "audit-develop",
            "NAROU_D1_DATABASE_ID": "11111111-1111-4111-8111-111111111111",
            "NAROU_JOB_QUEUE": "audit-develop",
            "NAROU_JOB_DLQ": "audit-develop-dlq",
            "NAROU_S3_ENDPOINT": "https://s3.example.com",
            "NAROU_S3_REGION": "us-east-1",
            "NAROU_S3_BUCKET": "audit-bucket",
        }

    def render(self, extra):
        with patch.dict(os.environ, self.environment | extra, clear=True), \
                patch.object(RENDER, "WORKER_DIR", self.directory), \
                contextlib.redirect_stdout(io.StringIO()):
            RENDER.main()
        return tomllib.loads((self.directory / "wrangler.ci.toml").read_text(encoding="utf-8"))

    def test_s3_vars_without_store(self):
        config = self.render({})
        self.assertEqual(config["vars"]["S3_ENDPOINT"], "https://s3.example.com")
        self.assertNotIn("secrets_store_secrets", config)

    def test_token_and_login_key_store_keep_s3_vars(self):
        config = self.render({
            "NAROU_SECRETS_STORE_ID": "audit-store",
            "NAROU_ADMIN_TOKEN_SECRET_NAME": "admin-token",
            "NAROU_RS_LOGIN_KEY_SECRET_NAME": "login-key",
        })
        self.assertEqual(config["vars"]["S3_BUCKET"], "audit-bucket")
        bindings = {row["binding"]: row["secret_name"] for row in config["secrets_store_secrets"]}
        self.assertEqual(bindings, {"NAROU_ADMIN_TOKEN_STORE": "admin-token", "NAROU_RS_LOGIN_KEY_STORE": "login-key"})

    def test_complete_s3_store(self):
        extra = {"NAROU_SECRETS_STORE_ID": "audit-store"}
        names = ("ACCESS_KEY_ID", "SECRET_ACCESS_KEY", "ENDPOINT", "REGION", "BUCKET")
        extra.update({f"NAROU_S3_{name}_SECRET_NAME": name.lower() for name in names})
        config = self.render(extra)
        self.assertEqual(config["vars"]["S3_ENDPOINT"], "")
        self.assertEqual({row["binding"] for row in config["secrets_store_secrets"]},
                         {f"S3_{name}_STORE" for name in names})

    def test_incomplete_s3_store_is_rejected(self):
        with self.assertRaises(SystemExit):
            self.render({"NAROU_SECRETS_STORE_ID": "audit-store", "NAROU_S3_BUCKET_SECRET_NAME": "bucket"})

    def test_token_store_requires_valid_store_id(self):
        for store_id in ("", "invalid store"):
            with self.subTest(store_id=store_id), self.assertRaises(SystemExit):
                self.render({"NAROU_SECRETS_STORE_ID": store_id, "NAROU_ADMIN_TOKEN_SECRET_NAME": "admin-token"})


if __name__ == "__main__":
    unittest.main()
