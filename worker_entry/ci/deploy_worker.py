"""Worker を Cloudflare へデプロイする（環境ごとに 1 回）。

流れ:

1. D1 と Queue を冪等に用意する（`ci/provision_resources.py`）
2. `wrangler.ci.toml` をレンダリングする（`ci/render_config.py`）
3. リモート D1 に migration を適用する
4. secret を `--secrets-file` で投入してデプロイする
5. デプロイ先の URL に対して契約テストを流す（smoke。`NAROU_SMOKE=0` で省略）

必須の環境変数:

- `NAROU_DEPLOY_TARGET` … `develop` / `staging` / `production`
- `CLOUDFLARE_ACCOUNT_ID` / `CLOUDFLARE_API_TOKEN`
- `NAROU_ADMIN_TOKEN` … Worker の API トークン（`--secrets-file` で投入）
- `NAROU_RS_LOGIN_KEY` … 資格情報の at-rest 鍵（base64 32 バイト）
- `NAROU_S3_ENDPOINT` / `NAROU_S3_REGION` / `NAROU_S3_BUCKET` … 挿絵の保存先
- `SERVICE_DOMAIN` … production のみ（カスタムドメイン）

任意: `NAROU_S3_PREFIX` / `NAROU_D1_BASE_NAME` / `NAROU_JOB_QUEUE_BASE` / `NAROU_SMOKE=0`。

ローカルからは実行しない（Cloudflare の資格情報が要る）。CI からのみ呼ぶ。
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path
from typing import NoReturn

WORKER_DIR = Path(__file__).resolve().parent.parent
TARGETS = ("develop", "staging", "production")
URL_PATTERN = re.compile(r"https://[A-Za-z0-9.-]+")
WORKER_URL_PATTERN = re.compile(r"https://[A-Za-z0-9.-]+\.workers\.dev")


def fail(message: str) -> NoReturn:
    raise SystemExit(message)


def run(command: list[str], *, capture: bool = False, env: dict[str, str] | None = None) -> str:
    result = subprocess.run(
        command,
        cwd=WORKER_DIR,
        check=False,
        capture_output=capture,
        text=True,
        encoding="utf-8",
        env=env,
    )
    if result.returncode != 0:
        if capture:
            sys.stderr.write(result.stdout or "")
            sys.stderr.write(result.stderr or "")
        fail(f"{' '.join(command)} failed with {result.returncode}")
    return (result.stdout or "") if capture else ""


def required(name: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        fail(f"{name} is required")
    return value


def provision(target: str) -> dict[str, str]:
    """D1 と Queue を用意し、導出された名前・ID を返す。"""
    output_file = WORKER_DIR / ".provision-output"
    output_file.unlink(missing_ok=True)
    env = dict(os.environ)
    env["NAROU_DEPLOY_TARGET"] = target
    env["GITHUB_OUTPUT"] = str(output_file)
    run([sys.executable, "ci/provision_resources.py"], env=env)
    values: dict[str, str] = {}
    for line in output_file.read_text(encoding="utf-8").splitlines():
        key, _, value = line.partition("=")
        if key:
            values[key.strip()] = value.strip()
    output_file.unlink(missing_ok=True)
    for key in ("database_name", "database_id", "queue_name", "dead_letter_queue"):
        if not values.get(key):
            fail(f"provisioning did not report {key}")
    return values


def render(target: str, resources: dict[str, str]) -> str:
    """`wrangler.ci.toml` を作り、D1 のデータベース名を返す。"""
    env = dict(os.environ)
    env["NAROU_DEPLOY_TARGET"] = target
    env["NAROU_D1_DATABASE_NAME"] = resources["database_name"]
    env["NAROU_D1_DATABASE_ID"] = resources["database_id"]
    env["NAROU_JOB_QUEUE"] = resources["queue_name"]
    env["NAROU_JOB_DLQ"] = resources["dead_letter_queue"]
    run([sys.executable, "ci/render_config.py"], env=env)
    return resources["database_name"]


def secret_file(target: str) -> Path | None:
    """`--secrets-file` に渡す JSON を書く（終了時に必ず消す）。

    Secrets Store (`NAROU_ADMIN_TOKEN_SECRET_NAME` / `NAROU_RS_LOGIN_KEY_SECRET_NAME`) に
    置く場合は値が無くてもよい（その場合はファイルを作らない）。
    """
    stored = {
        "NAROU_ADMIN_TOKEN": os.environ.get("NAROU_ADMIN_TOKEN_SECRET_NAME", "").strip(),
        "NAROU_RS_LOGIN_KEY": os.environ.get("NAROU_RS_LOGIN_KEY_SECRET_NAME", "").strip(),
    }
    secrets: dict[str, str] = {}
    for name, store_name in stored.items():
        value = os.environ.get(name, "").strip()
        if value:
            secrets[name] = value
        elif not store_name:
            fail(f"{name} is required (or set {name}_SECRET_NAME to keep it in the Secrets Store)")
    if not secrets:
        return None
    path = WORKER_DIR / f".deploy-secrets-{target}.json"
    path.write_text(json.dumps(secrets), encoding="utf-8")
    return path


def deploy(secrets: Path | None) -> str:
    """デプロイして、報告された URL を返す。"""
    command = ["npx", "--yes", "wrangler@4", "deploy", "-c", "wrangler.ci.toml"]
    if secrets is not None:
        command += ["--secrets-file", str(secrets)]
    log = run(command, capture=True)
    print(log)
    # custom domain 運用 (workers_dev = false) では wrangler が workers.dev を
    # 出さないので、明示指定 → workers.dev → 任意の https URL の順に採用する。
    explicit = os.environ.get("NAROU_DEPLOY_URL", "").strip()
    if explicit:
        return explicit
    urls = WORKER_URL_PATTERN.findall(log)
    if not urls:
        urls = URL_PATTERN.findall(log)
    if not urls:
        fail("wrangler did not report a URL")
    return urls[-1]


def smoke(base_url: str) -> bool:
    """デプロイ先に契約テストを流す。失敗してもデプロイは巻き戻さない。"""
    env = dict(os.environ)
    env["BASE_URL"] = base_url
    result = subprocess.run(
        ["node", "tests/contract.mjs"],
        cwd=WORKER_DIR,
        check=False,
        text=True,
        encoding="utf-8",
        env=env,
    )
    return result.returncode == 0


def summary(target: str, url: str, smoke_ok: bool | None) -> None:
    lines = [
        f"### Worker deploy ({target})",
        "",
        f"- URL: {url}",
        f"- smoke: {'ok' if smoke_ok else 'skipped' if smoke_ok is None else 'failed'}",
    ]
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    text = "\n".join(lines) + "\n"
    if path:
        with Path(path).open("a", encoding="utf-8") as handle:
            handle.write(text)
    print(text)


def main() -> None:
    target = os.environ.get("NAROU_DEPLOY_TARGET", "").strip()
    if target not in TARGETS:
        fail("NAROU_DEPLOY_TARGET must be develop, staging, or production")
    # 資格情報は早めに検証する（デプロイ途中で落ちないように）。
    required("CLOUDFLARE_ACCOUNT_ID")
    required("CLOUDFLARE_API_TOKEN")

    resources = provision(target)
    database = render(target, resources)
    run(
        [
            "npx",
            "--yes",
            "wrangler@4",
            "d1",
            "migrations",
            "apply",
            database,
            "--remote",
            "-c",
            "wrangler.ci.toml",
        ]
    )

    secrets = secret_file(target)
    try:
        url = deploy(secrets)
    finally:
        if secrets is not None:
            secrets.unlink(missing_ok=True)

    smoke_result: bool | None = None
    if os.environ.get("NAROU_SMOKE", "1") != "0":
        smoke_result = smoke(url)
    summary(target, url, smoke_result)
    if smoke_result is False:
        fail(f"smoke test failed against {url}")


if __name__ == "__main__":
    main()
