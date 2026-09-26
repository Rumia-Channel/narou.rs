"""Worker を Cloudflare へデプロイする（環境ごとに 1 回）。

流れ:

1. D1 と Queue を冪等に用意する（`ci/provision_resources.py`）
2. `wrangler.ci.toml` をレンダリングする（`ci/render_config.py`）
3. リモート D1 に migration を適用する
4. secret を `--secrets-file` で投入してデプロイする
5. デプロイ先の URL に対して契約テストを流す（smoke。`NAROU_SMOKE=0` で省略）

必須の環境変数:

- `NAROU_DEPLOY_TARGET` … `develop` / `production`
- `CLOUDFLARE_ACCOUNT_ID` / `CLOUDFLARE_API_TOKEN`
- `NAROU_S3_ENDPOINT` / `NAROU_S3_REGION` / `NAROU_S3_BUCKET` … 挿絵の保存先
- `SERVICE_DOMAIN` … production のみ（カスタムドメイン）

任意:

- `NAROU_ADMIN_TOKEN` … Worker の API トークン（`--secrets-file` で投入）。
  `NAROU_AUTH_REQUIRED=false`（Zero Trust を境界にする）ときは不要。
- `NAROU_RS_LOGIN_KEY` … 資格情報の at-rest 鍵（base64 32 バイト）。無い場合は
  平文の行だけを読む。
- `NAROU_S3_ACCESS_KEY_ID` / `NAROU_S3_SECRET_ACCESS_KEY` … S3 資格情報。`--secrets-file` で
  Worker secret (`S3_ACCESS_KEY_ID` / `S3_SECRET_ACCESS_KEY`) として投入する。無い場合は
  Secrets Store の `*_SECRET_NAME` を使うか、`wrangler secret put` で別途投入する。
- `NAROU_AUTH_REQUIRED` / `NAROU_WORKERS_DEV` / `DEVELOP_DOMAIN` /
  `NAROU_S3_PREFIX` / `NAROU_D1_BASE_NAME` / `NAROU_JOB_QUEUE_BASE` / `NAROU_SMOKE=0`
- `SERVICE_DOMAIN` / `DEVELOP_DOMAIN` … custom domain。secret を推奨（ログへ出さない）
- `NAROU_DEPLOY_URL` … smoke の宛先を明示する（既定は domain → workers.dev）
- `CF_ACCESS_CLIENT_ID` / `CF_ACCESS_CLIENT_SECRET` … Access の service token。
  未設定で Access に弾かれた場合は smoke を省略する。

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
TARGETS = ("develop", "production")
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


def mask(value: str) -> None:
    """GitHub のログで値を伏せる（`::add-mask::` は以降の出力に効く）。"""
    if value:
        print(f"::add-mask::{value}")


def auth_required() -> bool:
    """Bearer トークン検査が有効か（Zero Trust を境界にする場合は false）。"""
    return os.environ.get("NAROU_AUTH_REQUIRED", "true").strip().lower() != "false"


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

    `NAROU_AUTH_REQUIRED=false`（Cloudflare Access などの Zero Trust が境界）なら
    `NAROU_ADMIN_TOKEN` は要らない。`*_SECRET_NAME` を渡した項目は Secrets Store に
    置くので、値が無くても失敗にしない。
    """
    # Worker 側の名前 -> GitHub 側の環境変数名。`*_SECRET_NAME` を渡した項目は
    # Secrets Store に置くので値が無くてもよい。
    plain = (
        ("NAROU_ADMIN_TOKEN", "NAROU_ADMIN_TOKEN", "NAROU_ADMIN_TOKEN_SECRET_NAME"),
        ("NAROU_RS_LOGIN_KEY", "NAROU_RS_LOGIN_KEY", "NAROU_RS_LOGIN_KEY_SECRET_NAME"),
        ("S3_ACCESS_KEY_ID", "NAROU_S3_ACCESS_KEY_ID", "NAROU_S3_ACCESS_KEY_ID_SECRET_NAME"),
        (
            "S3_SECRET_ACCESS_KEY",
            "NAROU_S3_SECRET_ACCESS_KEY",
            "NAROU_S3_SECRET_ACCESS_KEY_SECRET_NAME",
        ),
    )
    secrets: dict[str, str] = {}
    for name, source, store_var in plain:
        value = os.environ.get(source, "").strip()
        if value:
            secrets[name] = value
        elif os.environ.get(store_var, "").strip():
            continue
        elif name == "NAROU_ADMIN_TOKEN":
            if auth_required():
                fail(
                    "NAROU_ADMIN_TOKEN is required (or set NAROU_ADMIN_TOKEN_SECRET_NAME, "
                    "or NAROU_AUTH_REQUIRED=false when Zero Trust is the boundary)"
                )
        elif name == "NAROU_RS_LOGIN_KEY":
            print(f"::notice::{source} is not set; stored login credentials stay plaintext-only")
        else:
            print(
                f"::notice::{source} is not set; the S3 illustration backend stays fail-closed "
                f"(set {source} or {store_var})"
            )
    if not secrets:
        return None
    path = WORKER_DIR / f".deploy-secrets-{target}.json"
    path.write_text(json.dumps(secrets), encoding="utf-8")
    return path


def deploy(secrets: Path | None) -> str | None:
    """デプロイし、wrangler が報告した workers.dev の URL を返す（無ければ None）。

    custom domain 運用 (`workers_dev = false`) では有人の URL を出さないため、
    smoke の宛先は `smoke_url()` が custom domain から決める。ログ中の任意の
    https URL を拾うと無関係なリンクを掴むので、フォールバックはしない。
    """
    command = ["npx", "--yes", "wrangler@4", "deploy", "-c", "wrangler.ci.toml"]
    if secrets is not None:
        command += ["--secrets-file", str(secrets)]
    log = run(command, capture=True)
    print(log)
    urls = WORKER_URL_PATTERN.findall(log)
    return urls[-1] if urls else None


def hide(url: str) -> bool:
    """workers.dev 以外（= custom domain）なら伏せる。"""
    if url and ".workers.dev" not in url:
        mask(url)
        return True
    return False


def smoke_url(target: str, reported: str | None) -> tuple[str | None, bool]:
    """smoke の宛先 `(url, custom_domain 由来か)`。明示 > custom domain > workers.dev。

    どれも分からない場合は `(None, False)` を返し、呼び出し側は smoke を省略する。
    """
    explicit = os.environ.get("NAROU_DEPLOY_URL", "").strip()
    if explicit:
        return explicit, hide(explicit)
    domain = os.environ.get({"develop": "DEVELOP_DOMAIN", "production": "SERVICE_DOMAIN"}[target], "").strip()
    if domain:
        # 公開ログ・step summary にドメインを残さない（値は GitHub の secret を想定）。
        mask(domain)
        return f"https://{domain}", hide(f"https://{domain}")
    if reported:
        return reported, hide(reported)
    return None, False


def smoke(base_url: str) -> bool | None:
    """デプロイ先に契約テストを流す。失敗してもデプロイは巻き戻さない。

    前段の Cloudflare Access に弾かれた場合（exit 3）と、ドメインがまだ届かない
    場合（exit 4。証明書・DNS の準備待ち）は「省略」として扱う。
    """
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
    if result.returncode == 3:
        print("::notice::Access が前段にあるため smoke を省略しました (CF_ACCESS_CLIENT_ID/SECRET で実行できます)")
        return None
    if result.returncode == 4:
        print(
            "::notice::デプロイ先に到達できないため smoke を省略しました "
            "(新しい custom domain は証明書と DNS の準備に数分かかることがあります)"
        )
        return None
    return result.returncode == 0


def summary(target: str, url: str, smoke_ok: bool | None, *, hidden_url: bool = False) -> None:
    lines = [
        f"### Worker deploy ({target})",
        "",
        f"- URL: {'（custom domain。ログでは伏せています）' if hidden_url else url}",
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
        fail("NAROU_DEPLOY_TARGET must be develop or production")
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

    target_url, hidden_url = smoke_url(target, url)
    smoke_result: bool | None = None
    if os.environ.get("NAROU_SMOKE", "1") == "0":
        pass
    elif target_url is None:
        print("::notice::smoke の宛先が分からないため省略しました (NAROU_DEPLOY_URL で指定できます)")
    else:
        smoke_result = smoke(target_url)
    summary(target, target_url or url or "(unknown)", smoke_result, hidden_url=hidden_url)
    if smoke_result is False:
        fail(f"smoke test failed against {target_url}")


if __name__ == "__main__":
    main()
