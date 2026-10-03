"""Validate the actual smoke-check shell without making network requests."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = Path(os.environ.get("SORAHOST_WORKFLOW", ROOT / ".github/workflows/deploy-sorahost.yml"))


def smoke_script():
    source = WORKFLOW.read_text()
    step = source.split("      - name: Smoke check\n", 1)[1]
    lines = step.split("        run: |\n", 1)[1].splitlines()
    script = []
    for line in lines:
        if line.strip() and not line.startswith("          "):
            break
        script.append(line)
    return textwrap.dedent("\n".join(script)) + "\n"


class SmokeCheckTests(unittest.TestCase):
    def test_shell_syntax(self):
        result = subprocess.run(["bash", "-n"], input=smoke_script(), text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)

    def run_smoke(self, status):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "deploy-result.json").write_text(json.dumps({"url": "https://example.invalid/smoke"}))
            curl = root / "curl"
            curl.write_text('#!/bin/sh\nprintf "%s" "$TEST_HTTP_STATUS"\n')
            curl.chmod(0o755)
            sleep = root / "sleep"
            sleep.write_text("#!/bin/sh\nexit 0\n")
            sleep.chmod(0o755)
            env = dict(os.environ, PATH=f"{root}{os.pathsep}{os.environ['PATH']}", TEST_HTTP_STATUS=status,
                       SORAHOST_SMOKE_URL="")
            return subprocess.run(["bash", "-e"], input=smoke_script(), text=True, capture_output=True,
                                  cwd=root, env=env, timeout=10)

    def test_expected_http_statuses_finish_on_first_attempt(self):
        for status in ["200", "204", "400", "401", "403"]:
            with self.subTest(status=status):
                result = self.run_smoke(status)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn(f"attempt 1: HTTP {status}", result.stdout)
                self.assertNotIn("attempt 2:", result.stdout)
                self.assertIn("応答を確認しました", result.stdout)

    def test_unavailable_service_retries_then_warns(self):
        result = self.run_smoke("503")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.count("attempt "), 6)
        self.assertIn("::warning::", result.stdout)
        self.assertNotIn("応答を確認しました", result.stdout)


if __name__ == "__main__":
    unittest.main()
