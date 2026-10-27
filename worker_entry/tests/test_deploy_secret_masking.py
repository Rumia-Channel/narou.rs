"""デプロイ用 secret ファイルの取扱いの契約テスト。ネットワーク・資格情報は使わない。

`worker_entry/ci/deploy_worker.py` は `--secrets-file` へ渡す JSON を
一時ファイルに書く。该ファイルには管理トークンや S3 資格情報が含まれるため、

- 値が GitHub のログマスクに登録されている (ログへ出ない)
- ファイルが同ホストの別ユーザーから読めない権限 (Unix では 0600) である
- 終了時に必ず削除される

ことを固定する。

Run: python -m unittest discover -s worker_entry/tests -p 'test_deploy_secret_masking.py'
"""

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import unittest
from unittest.mock import patch

WORKER = Path(__file__).resolve().parents[1]

ADMIN_TOKEN = "fake-admin-token-0123456789abcdef"
S3_SECRET = "fake-s3-secret-access-key"


def load_module(name):
    spec = importlib.util.spec_from_file_location(name, WORKER / "ci" / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


deploy = load_module("deploy_worker")


class DeploySecretMaskingTests(unittest.TestCase):
    def setUp(self):
        self.env = {
            "NAROU_ADMIN_TOKEN": ADMIN_TOKEN,
            "NAROU_S3_SECRET_ACCESS_KEY": S3_SECRET,
            "NAROU_RS_LOGIN_KEY": "",
            "NAROU_S3_ACCESS_KEY_ID": "",
            "RELAY_DOMAIN": "",
            "NAROU_RELAY_ENDPOINT": "",
            "NAROU_RELAY_KEY": "",
        }

    def _secret_file(self):
        buffer = io.StringIO()
        with patch.dict(os.environ, self.env, clear=True), contextlib.redirect_stdout(buffer):
            path = deploy.secret_file("develop")
        return path, buffer.getvalue()

    def test_secret_values_are_registered_for_log_masking(self):
        path, output = self._secret_file()
        try:
            self.assertIsNotNone(path)
            self.assertIn(f"::add-mask::{ADMIN_TOKEN}", output)
            self.assertIn(f"::add-mask::{S3_SECRET}", output)
        finally:
            if path is not None:
                path.unlink(missing_ok=True)

    def test_secret_file_is_not_readable_by_other_users(self):
        if os.name == "nt":
            self.skipTest("Unix の権限モデルではない")
        path, _ = self._secret_file()
        try:
            self.assertIsNotNone(path)
            mode = path.stat().st_mode & 0o777
            self.assertEqual(mode, 0o600)
        finally:
            if path is not None:
                path.unlink(missing_ok=True)

    def test_the_file_contains_the_expected_secret_names(self):
        path, _ = self._secret_file()
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(payload["NAROU_ADMIN_TOKEN"], ADMIN_TOKEN)
            self.assertEqual(payload["S3_SECRET_ACCESS_KEY"], S3_SECRET)
        finally:
            if path is not None:
                path.unlink(missing_ok=True)


if __name__ == "__main__":
    unittest.main()
