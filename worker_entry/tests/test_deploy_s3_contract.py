"""Offline CI deployment contract tests. No credentials or remote services are used.

Run: python -m unittest discover -s worker_entry/tests -p 'test_deploy_s3_contract.py'
"""

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch
import urllib.error


WORKER = Path(__file__).resolve().parents[1]


def load_module(name):
    spec = importlib.util.spec_from_file_location(name, WORKER / "ci" / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


deploy = load_module("deploy_worker")
render = load_module("render_config")
ENV = {
    "NAROU_DEPLOY_TARGET": "develop",
    "CLOUDFLARE_ACCOUNT_ID": "fake-account",
    "CLOUDFLARE_API_TOKEN": "fake-cloudflare-token",
    "NAROU_ADMIN_TOKEN": "fake-admin-token",
    "NAROU_D1_DATABASE_NAME": "narou-rs-develop",
    "NAROU_D1_DATABASE_ID": "12345678-1234-1234-1234-123456789012",
    "NAROU_JOB_QUEUE": "narou-jobs-develop",
    "NAROU_JOB_DLQ": "narou-jobs-develop-dlq",
    "NAROU_S3_ENDPOINT": "https://s3.example.invalid",
    "NAROU_S3_REGION": "us-east-1",
    "NAROU_S3_BUCKET": "fake-bucket",
}
VERIFIED = {
    "success": True,
    "metadata_backend": "d1",
    "illustration_backend": "s3",
    "s3_required": True,
    "s3_list": "ok",
}


class FakeResponse(io.BytesIO):
    def __init__(self, payload, status=200):
        super().__init__(payload if isinstance(payload, bytes) else json.dumps(payload).encode())
        self.status = status


class DeploymentFlowTests(unittest.TestCase):
    def setUp(self):
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)
        self.stack.enter_context(patch.dict(os.environ, ENV, clear=True))
        self.stack.enter_context(contextlib.redirect_stdout(io.StringIO()))
        self.calls = Mock()
        values = {
            "provision": {"database_name": "narou-rs-develop", "database_id": ENV["NAROU_D1_DATABASE_ID"]},
            "render": "narou-rs-develop",
            "enable_read_replication": None,
            "run": "",
            "secret_file": None,
            "deploy": "https://worker.example.workers.dev",
            "smoke_url": ("https://worker.example.workers.dev", False),
            "smoke": True,
            "summary": None,
        }
        for name, value in values.items():
            mock = self.stack.enter_context(patch.object(deploy, name, return_value=value))
            self.calls.attach_mock(mock, name)
        # All external boundaries are fakes even when testing the old baseline.
        self.d1 = self.stack.enter_context(patch.object(deploy.subprocess, "run"))
        self.http = self.stack.enter_context(patch.object(deploy.urllib.request, "build_opener")) if hasattr(deploy, "urllib") else None

    def d1_rows(self, rows):
        self.d1.return_value = subprocess.CompletedProcess([], 0, json.dumps([{"success": True, "results": rows}]), "")

    def ready(self):
        if self.http:
            self.http.return_value.open.return_value = FakeResponse(VERIFIED)

    def test_old_d1_marker_or_data_does_not_block_deployment(self):
        # If a deployment tries the former D1 query, these are its responses.
        # Successful deployment must not depend on reading or rewriting them.
        for backend, illustrations in ((None, 0), ('"d1"', 1), ('"s3"', 1), ("invalid", 1)):
            with self.subTest(backend=backend, illustrations=illustrations):
                self.calls.reset_mock()
                self.ready()
                self.d1_rows([{"asset_backend": backend, "has_d1_illustrations": illustrations}])
                deploy.main()
                self.d1.assert_not_called()
                self.calls.deploy.assert_called_once()
                # Schema migrations remain; no ad-hoc data query or deletion is introduced.
                command, = self.calls.run.call_args.args
                self.assertEqual(command[3:6], ["d1", "migrations", "apply"])
                self.assertNotIn("--command", command)

    def test_required_probe_cannot_be_skipped_by_general_smoke_switch(self):
        self.ready()
        os.environ["NAROU_SMOKE"] = "0"
        self.calls.smoke_url.return_value = (None, False)
        with self.assertRaises(SystemExit):
            deploy.main()

    def test_missing_bearer_for_store_only_auth_fails_before_provision(self):
        os.environ.pop("NAROU_ADMIN_TOKEN")
        os.environ["NAROU_ADMIN_TOKEN_SECRET_NAME"] = "runtime-token"
        with self.assertRaises(SystemExit):
            deploy.main()
        self.calls.provision.assert_not_called()

    def test_invalid_auth_headers_fail_before_provision_without_leaking(self):
        for name in ("NAROU_ADMIN_TOKEN", "CF_ACCESS_CLIENT_ID", "CF_ACCESS_CLIENT_SECRET"):
            with self.subTest(name=name), patch.dict(os.environ, {"CF_ACCESS_CLIENT_ID": "fake-id", "CF_ACCESS_CLIENT_SECRET": "fake-secret", name: "private\ninvalid"}):
                with self.assertRaises(SystemExit) as failure:
                    deploy.main()
                self.assertNotIn("private", str(failure.exception))
                self.calls.provision.assert_not_called()

    def test_s3_failure_still_fails_when_smoke_is_disabled(self):
        self.ready()
        os.environ["NAROU_SMOKE"] = "0"
        self.http.return_value.open.return_value = FakeResponse({**VERIFIED, "s3_list": "failed"}, 503)
        with self.assertRaises(SystemExit):
            deploy.main()
        self.calls.smoke.assert_not_called()

    def test_success_verifies_s3_even_when_general_smoke_is_disabled(self):
        self.ready()
        os.environ["NAROU_SMOKE"] = "0"
        deploy.main()
        self.http.return_value.open.assert_called_once()
        self.calls.smoke.assert_not_called()
        self.calls.deploy.assert_called_once()


class ProbeTests(unittest.TestCase):
    def setUp(self):
        self.env = patch.dict(os.environ, ENV, clear=True)
        self.env.start()
        self.addCleanup(self.env.stop)

    def probe(self, payload=VERIFIED, status=200):
        opener = Mock()
        opener.open.return_value = FakeResponse(payload, status)
        with patch.object(deploy.urllib.request, "build_opener", return_value=opener):
            deploy.verify_s3("https://worker.example.invalid")
        return opener.open.call_args

    def test_authenticated_get_has_no_write_body(self):
        os.environ["CF_ACCESS_CLIENT_ID"] = "fake-access-id"
        os.environ["CF_ACCESS_CLIENT_SECRET"] = "fake-access-secret"
        call = self.probe()
        request = call.args[0]
        self.assertEqual(request.full_url, "https://worker.example.invalid/api/storage/mode?probe=s3")
        self.assertEqual(request.get_method(), "GET")
        self.assertIsNone(request.data)
        self.assertEqual(request.get_header("Authorization"), "Bearer fake-admin-token")
        self.assertEqual(request.get_header("Cf-access-client-id"), "fake-access-id")
        self.assertEqual(request.get_header("Cf-access-client-secret"), "fake-access-secret")
        self.assertLessEqual(call.kwargs["timeout"], 60)

    def test_wrong_backend_flag_or_list_result_is_not_success(self):
        for field, value in (("success", False), ("success", "true"), ("metadata_backend", "s3"), ("illustration_backend", "d1"), ("s3_required", False), ("s3_required", "true"), ("s3_list", "failed"), ("s3_list", "not_selected")):
            with self.subTest(field=field, value=value), self.assertRaises(SystemExit):
                self.probe({**VERIFIED, field: value})
        for payload in ({}, [], None, b"<html>Login</html>", b"x" * 65537):
            with self.subTest(payload_type=type(payload)), self.assertRaises(SystemExit):
                self.probe(payload)

    def test_redirect_auth_and_server_errors_are_failures(self):
        for status in (301, 302, 307, 401, 403, 404, 503):
            with self.subTest(status=status), self.assertRaises(SystemExit):
                self.probe(VERIFIED, status)

    def test_provider_failures_do_not_leak_credentials_or_urls(self):
        for error in (urllib.error.HTTPError("https://private.invalid", 403, "secret-detail", {}, None), urllib.error.URLError("secret-detail"), TimeoutError("secret-detail"), ValueError("secret-detail"), deploy.http.client.HTTPException("secret-detail")):
            with self.subTest(error=error), patch.object(deploy.urllib.request, "build_opener") as factory:
                factory.return_value.open.side_effect = error
                with self.assertRaises(SystemExit) as failure:
                    deploy.verify_s3("https://private.invalid")
                self.assertNotIn("secret-detail", str(failure.exception))
                self.assertNotIn("private.invalid", str(failure.exception))

    def test_unsafe_url_is_rejected_before_authentication_is_sent(self):
        for url in ("http://worker.example.invalid", "https://user:password@worker.example.invalid", "https://worker.example.invalid?private=value", "https://worker.example.invalid/#private", "https:///missing", "https://worker.example.invalid:invalid", "https://worker.example.invalid/\nprivate"):
            with self.subTest(url=url), patch.object(deploy.urllib.request, "build_opener") as factory:
                with self.assertRaises(SystemExit):
                    deploy.verify_s3(url)
                factory.assert_not_called()

    def test_redirect_handler_never_forwards_auth(self):
        request = deploy.urllib.request.Request("https://worker.example.invalid", headers={"Authorization": "Bearer fake-token"})
        self.assertIsNone(deploy.NoRedirect().redirect_request(request, None, 302, "Found", {}, "https://other.invalid"))


class RenderTests(unittest.TestCase):
    def render_template(self, template, target="develop", extra_env=None):
        env = {**ENV, "NAROU_DEPLOY_TARGET": target, "SERVICE_DOMAIN": "production.example.invalid"}
        for name in ("NAROU_D1_DATABASE_NAME", "NAROU_JOB_QUEUE", "NAROU_JOB_DLQ"):
            env[name] = env[name].replace("develop", target)
        env.update(extra_env or {})
        with tempfile.TemporaryDirectory() as directory, patch.object(render, "WORKER_DIR", Path(directory)), patch.dict(os.environ, env, clear=True), contextlib.redirect_stdout(io.StringIO()):
            (Path(directory) / f"wrangler.{target}.toml").write_text(template, encoding="utf-8")
            render.main()
            return (Path(directory) / "wrangler.ci.toml").read_text(encoding="utf-8")

    def test_ci_template_without_s3_requirement_is_refused(self):
        with self.assertRaises(SystemExit):
            self.render_template('[vars]\nS3_BUCKET = "__S3_BUCKET__"\n')

    def test_false_or_boolean_requirement_is_refused(self):
        for value in ('"false"', 'false', 'true'):
            with self.subTest(value=value), self.assertRaises(SystemExit):
                self.render_template(f"[vars]\nNAROU_REQUIRE_S3 = {value}\n")

    def test_ci_s3_requirement_is_not_configurable_by_environment(self):
        template = '[vars]\nNAROU_REQUIRE_S3 = "true"\n'
        self.assertIn('NAROU_REQUIRE_S3 = "true"', self.render_template(template, extra_env={"NAROU_REQUIRE_S3": "false"}))

    def test_real_deploy_templates_require_s3_and_reject_tampering(self):
        for target in ("develop", "production"):
            template = (WORKER / f"wrangler.{target}.toml").read_text(encoding="utf-8")
            with self.subTest(target=target):
                self.assertIn('NAROU_REQUIRE_S3 = "true"', self.render_template(template, target))
            for replacement in ("", 'NAROU_REQUIRE_S3 = "false"'):
                with self.subTest(target=target, replacement=replacement), self.assertRaises(SystemExit):
                    self.render_template(template.replace('NAROU_REQUIRE_S3 = "true"', replacement), target)

    def test_secrets_store_render_keeps_ci_s3_requirement(self):
        names = {f"NAROU_S3_{name}_SECRET_NAME": f"test-{name.lower()}" for name in ("ACCESS_KEY_ID", "SECRET_ACCESS_KEY", "ENDPOINT", "REGION", "BUCKET")}
        names["NAROU_SECRETS_STORE_ID"] = "fake-store"
        for target in ("develop", "production"):
            with self.subTest(target=target):
                template = (WORKER / f"wrangler.{target}.toml").read_text(encoding="utf-8")
                result = self.render_template(template, target, names)
                self.assertIn('NAROU_REQUIRE_S3 = "true"', result)
                self.assertIn('binding = "S3_ACCESS_KEY_ID_STORE"', result)


if __name__ == "__main__":
    unittest.main()
