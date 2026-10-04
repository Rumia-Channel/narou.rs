"""Worker を Cloudflare へデプロイする（環境ごとに 1 回）。

流れ:

1. D1 と Queue を冪等に用意する（`ci/provision_resources.py`）
2. `wrangler.ci.toml` をレンダリングする（`ci/render_config.py`）
3. リモート D1 に schema migration を適用する
4. secret を `--secrets-file` で投入してデプロイする
5. 認証済み診断 API で実際の S3 backend と LIST を検証する（トークンが
   Secrets Store 専用の場合は LIST を notice 付きで省略し、health と
   未認証 401 の fail-closed 確認だけを行う）
6. デプロイ先の URL に対して契約テストを流す（smoke。`NAROU_SMOKE=0` で省略）

CI は NAROU_REQUIRE_S3=true 固定。旧 D1 の挿絵との互換性は維持せず、
runtime が挿絵の保存先を S3 に固定する。既存 D1 のマーカーや挿絵行を検査・
変更・削除しない。メタデータと本文は引き続き D1 に置く。

必須の環境変数:

- `NAROU_DEPLOY_TARGET` … `develop` / `production`
- `CLOUDFLARE_ACCOUNT_ID` / `CLOUDFLARE_API_TOKEN`
- `NAROU_S3_ENDPOINT` / `NAROU_S3_REGION` / `NAROU_S3_BUCKET` … 挿絵の保存先
- `SERVICE_DOMAIN` … production のみ（カスタムドメイン）

任意:

- `NAROU_ADMIN_TOKEN` … Worker の API トークン（`--secrets-file` で投入）。
  Bearer 認証が有効なら、Secrets Store に置いた場合も CI の診断認証に必要
  （ストアの値は読み戻せないため）。ストア専用運用で CI 側に値が無いときは
  認証付きの S3 LIST 検証と smoke を明示 notice 付きで省略し、無認証の
  health 検査と 401 の fail-closed 確認は必ず行う。
  `NAROU_AUTH_REQUIRED=false`（Zero Trust を境界にする）ときは不要。
- `NAROU_RS_LOGIN_KEY` … 資格情報の at-rest 鍵（base64 32 バイト）。無い場合は
  平文の行だけを読む。
- `NAROU_S3_ACCESS_KEY_ID` / `NAROU_S3_SECRET_ACCESS_KEY` … S3 資格情報。`--secrets-file` で
  Worker secret (`S3_ACCESS_KEY_ID` / `S3_SECRET_ACCESS_KEY`) として投入する。無い場合は
  Secrets Store の `*_SECRET_NAME` を使うか、`wrangler secret put` で別途投入する。
- `NAROU_AUTH_REQUIRED` / `NAROU_WORKERS_DEV` / `DEVELOP_DOMAIN` /
  `NAROU_S3_PREFIX` / `NAROU_D1_BASE_NAME` / `NAROU_JOB_QUEUE_BASE` / `NAROU_SMOKE=0`
- `SORAHOST_PROXY_ENDPOINT` / `SORAHOST_PROXY_KEY` … 外部の取得リレー（任意）。接続先（例:
  `http://<IP>:<port>/_sorahost/...`）を `SORAHOST_PROXY_ENDPOINT` に、リレーの合言葉
  （PteWorker の `.env` に置く `SORAHOST_PROXY_KEY` と同じ値）を `SORAHOST_PROXY_KEY` に入れる。
  Worker secret (`SORAHOST_PROXY_ENDPOINT` / `SORAHOST_PROXY_KEY`) として投入する。
- `SERVICE_DOMAIN` / `DEVELOP_DOMAIN` … custom domain。secret を推奨（ログへ出さない）
- `NAROU_DEPLOY_URL` … smoke の宛先を明示する（既定は domain → workers.dev）
- `CF_ACCESS_CLIENT_ID` / `CF_ACCESS_CLIENT_SECRET` … Access の service token。
  Access に遮られた場合、必須 S3 検証は失敗する。DNS/TLS エラーも省略しない。

ローカルからは実行しない（Cloudflare の資格情報が要る）。CI からのみ呼ぶ。
"""

from __future__ import annotations

import http.client
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request
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


def admin_token() -> str:
    """CI 側で使える Bearer トークン。Secrets Store の値は API でも読み戻せない。"""
    return os.environ.get("NAROU_ADMIN_TOKEN", "").strip()


def require_probe_auth() -> None:
    """必須の配備後診断に使う認証を、リソースの変更より前に検証する。

    トークンを Secrets Store だけに置く運用 (値は読み戻せない) では CI 側の
    Bearer を要求しない。代わりに `verify_s3` が無認証の health 検査と
    fail-closed の 401 確認だけを行い、認証付き検査は notice 付きで省略する。
    """
    if (
        auth_required()
        and not admin_token()
        and not os.environ.get("NAROU_ADMIN_TOKEN_SECRET_NAME", "").strip()
    ):
        fail(
            "NAROU_ADMIN_TOKEN is required (or set NAROU_ADMIN_TOKEN_SECRET_NAME to "
            "keep the token in the Secrets Store, or NAROU_AUTH_REQUIRED=false "
            "when Zero Trust is the boundary)"
        )
    access_id = os.environ.get("CF_ACCESS_CLIENT_ID", "").strip()
    access_secret = os.environ.get("CF_ACCESS_CLIENT_SECRET", "").strip()
    if bool(access_id) != bool(access_secret):
        fail("CF_ACCESS_CLIENT_ID and CF_ACCESS_CLIENT_SECRET must be supplied together")
    for name in ("NAROU_ADMIN_TOKEN", "CF_ACCESS_CLIENT_ID", "CF_ACCESS_CLIENT_SECRET"):
        value = os.environ.get(name, "").strip()
        if any(ord(character) < 32 or ord(character) > 126 for character in value):
            fail(f"{name} must be a valid ASCII HTTP header value")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """認証ヘッダを別の宛先や Access のログイン画面へ送らない。"""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def deployment_url(base_url: str) -> None:
    """診断の宛先として安全な HTTPS URL か検証する (不適なら失敗)。"""
    try:
        parsed = urllib.parse.urlsplit(base_url)
        if (parsed.scheme != "https" or not parsed.hostname or parsed.username is not None
                or parsed.password is not None or parsed.query or parsed.fragment
                or any(ord(character) <= 32 for character in base_url)):
            raise ValueError
        parsed.port  # Validate any explicit port before building authenticated headers.
    except ValueError:
        fail("deployment checks require an HTTPS deployment URL without credentials, query or fragment")


def probe_headers(token: bool = True) -> dict[str, str]:
    """診断 GET のヘッダ。Bearer は `token=True` かつ値があるときだけ載せる。"""
    headers = {"Accept": "application/json"}
    value = admin_token()
    if token and value:
        headers["Authorization"] = f"Bearer {value}"
    access_id = os.environ.get("CF_ACCESS_CLIENT_ID", "").strip()
    if access_id:
        headers["CF-Access-Client-Id"] = access_id
        headers["CF-Access-Client-Secret"] = os.environ["CF_ACCESS_CLIENT_SECRET"].strip()
    return headers


def probe_get(base_url: str, path: str, *, token: bool = True) -> tuple[int, bytes]:
    """`(status, body)` を返す GET。HTTP エラーは `HTTPError` として上げる。"""
    request = urllib.request.Request(
        urllib.parse.urljoin(base_url, path),
        headers=probe_headers(token),
        method="GET",
    )
    with urllib.request.build_opener(NoRedirect()).open(request, timeout=30) as response:
        return response.status, response.read(16 * 1024 + 1)


def probe_json(base_url: str, path: str, label: str, *, token: bool = True) -> dict:
    """200 の JSON オブジェクトを返す。それ以外は {label} つきで失敗。"""
    try:
        status, raw = probe_get(base_url, path, token=token)
        if status != 200:
            fail(f"{label} failed (HTTP {status}); deployment is not verified")
    except urllib.error.HTTPError as error:
        error.close()
        fail(f"{label} failed (HTTP {error.code}); check Worker/Access authentication and configuration")
    except (urllib.error.URLError, OSError, ValueError, http.client.HTTPException):
        fail(f"{label} could not reach the deployment; check DNS/TLS/network. Deployment is not verified")
    if len(raw) > 16 * 1024:
        fail(f"{label} returned an oversized response; deployment is not verified")
    try:
        payload = json.loads(raw)
    except (ValueError, UnicodeError):
        fail(f"{label} returned invalid JSON; deployment is not verified")
    if not isinstance(payload, dict):
        fail(f"{label} returned an unexpected payload; deployment is not verified")
    return payload


def verify_health(base_url: str) -> None:
    """無認証で通る配備確認。認証済み検査を省略する代わりの下限 (fail-closed)。

    1. `/health/live` が `status=alive`、`authentication_required=true`
       (Worker 側も auth 要求)、`authentication_configured=true` (ストアの
       トークンが Worker で実際に解決できた証拠)。
    2. `/health/ready` が `status=ready` (D1 / queue / DO の構成が通る)。
    3. 認証付き probe は無認証だと 401 `authentication_required` で閉じる
       (200/500 なら認証の構成が壊れているので失敗)。
    """
    live = probe_json(base_url, "/health/live", "health check")
    if (live.get("status") != "alive" or live.get("authentication_required") is not True
            or live.get("authentication_configured") is not True):
        fail("health check did not confirm a live, authenticated and configured deployment")
    ready = probe_json(base_url, "/health/ready", "readiness check")
    if ready.get("status") != "ready":
        fail("readiness check did not confirm the deployment stack")
    try:
        status, raw = probe_get(base_url, "/api/storage/mode?probe=s3", token=False)
        if status != 401:
            fail(
                f"authenticated probe returned HTTP {status} without a token; "
                "the deployment must fail closed"
            )
    except urllib.error.HTTPError as error:
        with error:
            status, raw = error.code, error.read(16 * 1024 + 1)
    except (urllib.error.URLError, OSError, ValueError, http.client.HTTPException):
        fail("deployment checks could not reach the deployment; check DNS/TLS/network. Deployment is not verified")
    try:
        code = json.loads(raw).get("error", {}).get("code")
    except (ValueError, UnicodeError, AttributeError):
        code = None
    if status != 401 or code != "authentication_required":
        fail("the deployment must answer 401 authentication_required without a token")


def verify_s3(base_url: str) -> bool | None:
    """選択済み S3 handle の実 LIST を確認する。失敗・未実行は成功扱いしない。

    戻り値は `True`=検証済み、`None`=認証付き検査を省略 (トークンが Secrets
    Store 専用で CI 側に値が無い場合。health と 401 の fail-closed は確認済み)。
    """
    require_probe_auth()
    deployment_url(base_url)
    token = admin_token()
    if auth_required() and not token:
        # Secrets Store の値は REST/wrangler とも読み戻せないため、CI 側に
        # トークンが無いとき認証付きの LIST は打てない。空成功にせず、無認証で
        # 確かめられる範囲だけ検査して「省略」として報告する。
        verify_health(base_url)
        print(
            "::notice::NAROU_ADMIN_TOKEN が Secrets Store 専用のため認証付きの "
            "S3 LIST 検証を省略しました (health の検査と未認証 401 の確認は実施済み)。"
            "LIST まで検証するには secrets.NAROU_ADMIN_TOKEN にも同じ値を置いてください"
        )
        return None
    payload = probe_json(base_url, "/api/storage/mode?probe=s3", "S3 verification")
    if (payload.get("success") is not True
            or payload.get("metadata_backend") != "d1"
            or payload.get("illustration_backend") != "s3" or payload.get("s3_required") is not True
            or payload.get("s3_list") != "ok"):
        fail("S3 verification did not confirm D1 metadata, required S3 illustrations and a successful S3 LIST")
    print("S3 deployment check: selected backend and read-only LIST verified")
    return True


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


def enable_read_replication(resources: dict[str, str]) -> None:
    """D1 の read replication を有効にする（冪等・develop / production 共通）。

    Sessions API を使わない限り全クエリは primary に行くため、有効化と
    併せてアプリ側のセッション利用が前提になる（`build_ui` を参照）。

    失敗してもデプロイは止めない: 資格情報の権限不足でリリースが止まるより、
    警告を残して配信を続ける方が安全（有効化は冪等なので次回も試行される）。
    """
    env = os.environ.copy()
    env["NAROU_D1_DATABASE_ID"] = resources["database_id"]
    result = subprocess.run(
        [sys.executable, "ci/enable_read_replication.py"],
        cwd=WORKER_DIR,
        check=False,
        text=True,
        encoding="utf-8",
        env=env,
    )
    if result.returncode != 0:
        print(
            "::warning::read replication を有効化できませんでした"
            " (CLOUDFLARE_API_TOKEN に D1:Edit が必要です)。デプロイは続行します。"
        )


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
    # SORAHOST の取得リレー（任意）。値があるときだけ Worker secret として渡す。
    # 未設定なら Worker 側のリレー段は無効のまま（Cloudflare から取れないサイトが残る）。
    for name in ("SORAHOST_PROXY_ENDPOINT", "SORAHOST_PROXY_KEY"):
        value = os.environ.get(name, "").strip()
        if value:
            secrets[name] = value
        else:
            print(f"::notice::{name} is not set; the relay fallback stays disabled")

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

    前段の Cloudflare Access に弾かれた場合（exit 3）、ドメインがまだ届かない
    場合（exit 4。証明書・DNS の準備待ち）、CI 側にトークンが無く認証済み
    検査を始められない場合（exit 2）は「省略」として扱う。
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
    if result.returncode == 2 and auth_required() and not admin_token():
        # Secrets Store の値は API でも読み戻せないため、CI 側にトークンが無い
        # と認証済みの smoke は実行できない (verify_s3 の時点で health と
        # fail-closed の 401 は検証済み)。空成功にはせず「省略」として報告する。
        print(
            "::notice::NAROU_ADMIN_TOKEN を CI 側で解決できないため認証済みの "
            "smoke を省略しました (health と未認証 401 の検証は実施済み)。"
            "smoke まで実行するには secrets.NAROU_ADMIN_TOKEN にも同じ値を置いてください"
        )
        return None
    return result.returncode == 0


def summary(target: str, url: str, smoke_ok: bool | None, *, hidden_url: bool = False, s3_ok: bool | None = False) -> None:
    lines = [
        f"### Worker deploy ({target})",
        "",
        f"- URL: {'（custom domain。ログでは伏せています）' if hidden_url else url}",
        f"- S3 backend + LIST: {'ok' if s3_ok else 'skipped (admin token store-only)' if s3_ok is None else 'not verified'}",
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
    require_probe_auth()

    resources = provision(target)
    database = render(target, resources)
    enable_read_replication(resources)
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
    if target_url is None:
        fail("Mandatory S3 verification needs a deployment URL; configure NAROU_DEPLOY_URL or a deployment domain")
    s3_result = verify_s3(target_url)
    smoke_result: bool | None = None
    if os.environ.get("NAROU_SMOKE", "1") == "0":
        pass
    else:
        smoke_result = smoke(target_url)
    summary(target, target_url, smoke_result, hidden_url=hidden_url, s3_ok=s3_result)
    if smoke_result is False:
        fail("smoke test failed against the deployment")


if __name__ == "__main__":
    main()
