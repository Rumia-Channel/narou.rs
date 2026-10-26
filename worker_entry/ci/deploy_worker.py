"""Worker を Cloudflare へデプロイする（環境ごとに 1 回）。

流れ:

1. D1 と Queue を冪等に用意する（`ci/provision_resources.py`）
2. `wrangler.ci.toml` をレンダリングする（`ci/render_config.py`）
3. リモート D1 に schema migration を適用する
4. secret を `--secrets-file` で投入してデプロイする
5. 認証済み診断 API で実際の S3 backend と LIST を検証する（トークンが
   Secrets Store 専用の場合は LIST を notice 付きで省略し、health と
   未認証 401 の fail-closed 確認だけを行う。custom domain が Cloudflare
   Access の内側にあり CI に service token が無い場合は、前段の 403 を
   警告つきのスキップとして報告し、成功扱いにはしない）
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
  CI 側に値を置く場合は 32 文字以上 (ASCII) を要求する。既に配備済みの短い
  トークンはこの検査では失効しないが、次回デプロイ時にローテーションする。
  `NAROU_AUTH_REQUIRED=false`（Zero Trust を境界にする）ときは不要。
- `NAROU_RS_LOGIN_KEY` … 資格情報の at-rest 鍵（base64 32 バイト）。無い場合は
  平文の行だけを読む。
- `NAROU_S3_ACCESS_KEY_ID` / `NAROU_S3_SECRET_ACCESS_KEY` … S3 資格情報。`--secrets-file` で
  Worker secret (`S3_ACCESS_KEY_ID` / `S3_SECRET_ACCESS_KEY`) として投入する。無い場合は
  Secrets Store の `*_SECRET_NAME` を使うか、`wrangler secret put` で別途投入する。
- `NAROU_AUTH_REQUIRED` / `NAROU_WORKERS_DEV` / `DEVELOP_DOMAIN` /
  `NAROU_S3_PREFIX` / `NAROU_D1_BASE_NAME` / `NAROU_JOB_QUEUE_BASE` / `NAROU_SMOKE=0`
- `NAROU_WRANGLER_VERSION` … デプロイと D1 migration に使う wrangler の
  バージョン (`x.y.z` 形式のみ)。既定は [`WRANGLER_VERSION`]。
- `NAROU_RELAY_ENDPOINT` / `NAROU_RELAY_KEY` … 任意の外部取得リレー。
  リレーの公開先と `X-Proxy-Token` の合言葉を同名の Worker secret として投入する。
  リレー自体の配置・更新はこの CI からは行わない。
- `RELAY_DOMAIN` … リレーの公開ドメイン（例 `relay.example.com`）。設定時は
  `NAROU_RELAY_ENDPOINT` ではなく `https://<RELAY_DOMAIN>` を Worker secret に入れる。
  Cloudflare の名前付きトンネル等で URL が固定されている場合に使う。
- `SERVICE_DOMAIN` / `DEVELOP_DOMAIN` … custom domain。secret を推奨（ログへ出さない）
- `NAROU_DEPLOY_URL` … smoke の宛先を明示する（既定は domain → workers.dev）
- `CF_ACCESS_CLIENT_ID` / `CF_ACCESS_CLIENT_SECRET` … Access の service token。
  custom domain が Access の内側にある場合は、無いと必須の S3 検証が Worker に
  到達できない (S3 LIST の失敗は Worker が 503 で返し、この経路で 403 は返さない。
  403 は前段で弾かれた印)。その場合は失敗にせず、警告つきのスキップとして報告する
  (ステップサマリも `skipped`)。DNS/TLS エラーは省略しない。

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

# デプロイと D1 migration に使う wrangler はバージョンを固定する。
# `wrangler@4` のような範囲指定だとデプロイのたびに新しい 4.x が入り、
# wrangler.toml / D1 / secrets の扱いが予告なく変わる (再現性が無く、
# サプライチェーンの影響も受けやすい)。`NAROU_WRANGLER_VERSION` で
# 明示的に上げるときだけ上書きする。
WRANGLER_VERSION = "4.151.3"
WRANGLER_VERSION_PATTERN = re.compile(r"^\d+\.\d+\.\d+$")

# CI が使う管理 API トークンの最小長。短いトークンは総当たりで破れるため
# デプロイ時に拒否する (値は Worker の secret として配備される)。
# Secrets Store 専用で CI 側に値が無い場合は検査できないのでそのまま通す。
ADMIN_TOKEN_MIN_CHARS = 32

# 配備後の診断 GET に載せる User-Agent。アプリ本体の既定値と同じブラウザ風の
# 文字列を使う (`src/downloader/mod.rs` の `DEFAULT_USER_AGENT`)。urllib の既定
# (`Python-urllib/3.x`) は Cloudflare のボット判定に掛かり、Worker の手前で
# 403 になる。
PROBE_USER_AGENT = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:131.0) Gecko/20100101 Firefox/131.0"
)

# custom domain を付けられないときの wrangler のエラー文言 (Cloudflare error
# 100117)。既存のフロントエンド等が同じホスト名の DNS レコードを持っていると出る。
DNS_CONFLICT_MARKER = "externally managed DNS records"

# Cloudflare Access のログインリダイレクト先。ルートで Worker を受けている
# ホスト名では、302 でここへ飛ばされるのも「前段で弾かれた」印になる。
ACCESS_REDIRECT_MARKER = "cloudflareaccess.com"

# 必須の S3 検証が 403 で返ったときの説明。Worker 自身は S3 LIST の失敗を
# 503 で返し、この経路で 403 は返さないため、403 は前段 (Cloudflare Access や
# WAF) で弾かれたことを意味する。custom domain が Access の内側にある構成では
# service token を CI に渡すまでこの検証は通らない。CI 側からは手の打ちようが
# ないので、失敗ではなく警告つきスキップとして報告し、成功とは扱わない。
ACCESS_BLOCKED_NOTICE = (
    "the mandatory S3 check did not run: HTTP 403 was answered before the Worker "
    "(Cloudflare Access or WAF). This route answers 503 for a failing S3 LIST and never 403, "
    "so the deployment was not reached and is NOT verified. Set secrets CF_ACCESS_CLIENT_ID "
    "and CF_ACCESS_CLIENT_SECRET (an Access service token allowed by the application policy) "
    "in the Cloudflare environment, or bypass Access for the probed path, to run the full check"
)


class AccessBlocked(Exception):
    """前段 (Cloudflare Access / WAF) が Worker より先に 403 を返した。

    `detail` は前段が付けた手掛かり (`cf-mitigated` / `content-type`) で、
    原因の切り分け用。秘密値は含めない。
    """

    def __init__(self, detail: str = "") -> None:
        super().__init__(detail)
        self.detail = detail


def blocked_detail(headers) -> str:
    """403 応答から原因の手掛かりだけを取り出す (秘密・本文は含めない)。"""
    parts = []
    for name in ("cf-mitigated", "content-type"):
        value = (headers.get(name) or "").strip() if headers else ""
        if value:
            parts.append(f"{name}={value[:40]}")
    return ", ".join(parts)


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


def wrangler_version() -> str:
    """デプロイに使う wrangler のバージョン。

    既定は [`WRANGLER_VERSION`]。`NAROU_WRANGLER_VERSION` で上書きできるが、
    `x.y.z` の完全一致だけを受け付ける (`4` や `latest` のような範囲指定は
    再現性が無いので拒否する)。
    """
    value = os.environ.get("NAROU_WRANGLER_VERSION", "").strip()
    if not value:
        return WRANGLER_VERSION
    if not WRANGLER_VERSION_PATTERN.fullmatch(value):
        fail(
            "NAROU_WRANGLER_VERSION must be an exact x.y.z version "
            f"(got {value!r}); ranges such as 4 or latest are not pinned"
        )
    return value


def wrangler_command(*args: str) -> list[str]:
    """固定バージョンの wrangler を `npx --yes` で実行するコマンド。"""
    return ["npx", "--yes", f"wrangler@{wrangler_version()}", *args]


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
    # Bearer トークンは総当たりされない長さを要求する (CI 側だけで検査し、
    # Worker 実行時には持ち込まない)。既に配備済みの短いトークンをこの検証で
    # 失効させることはないが、次回デプロイ/ローテーション時に 32 文字以上へ
    # 更新する必要がある。Secrets Store 専用 (CI に値が無い) 場合は検査しない。
    token = admin_token()
    if token and len(token) < ADMIN_TOKEN_MIN_CHARS:
        fail(
            f"NAROU_ADMIN_TOKEN must be at least {ADMIN_TOKEN_MIN_CHARS} characters "
            "(existing deployments should rotate to a "
            f">={ADMIN_TOKEN_MIN_CHARS} character token on the next deploy)"
        )


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
    # urllib 既定の `Python-urllib/3.x` は Cloudflare のボット判定で 403 になる
    # (同じ配備へ Node の fetch で流す contract.mjs は通る)。アプリ本体と同じく
    # ブラウザ風の UA を必ず送る。
    headers = {"Accept": "application/json", "User-Agent": PROBE_USER_AGENT}
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


def probe_json(base_url: str, path: str, label: str, *, token: bool = True,
               blocked_is_access: bool = False) -> dict:
    """200 の JSON オブジェクトを返す。それ以外は {label} つきで失敗。

    `blocked_is_access=True` のときだけ 403 と Access のログインリダイレクト
    (302 → cloudflareaccess.com) を [`AccessBlocked`] として上げる。どちらも
    Worker に到達していない印で、CI からは回避できない。
    """
    try:
        status, raw = probe_get(base_url, path, token=token)
    except urllib.error.HTTPError as error:
        with error:
            status = error.code
            headers = error.headers
            detail = blocked_detail(headers) if status == 403 else ""
            access_redirect = 300 <= status < 400 and ACCESS_REDIRECT_MARKER in (
                (headers.get("Location") or "") if headers else ""
            )
        if blocked_is_access and access_redirect:
            raise AccessBlocked(f"redirect to {ACCESS_REDIRECT_MARKER}") from None
        if status == 403 and blocked_is_access:
            raise AccessBlocked(detail) from None
        fail(probe_failure(label, status))
    except (urllib.error.URLError, OSError, ValueError, http.client.HTTPException):
        fail(f"{label} could not reach the deployment; check DNS/TLS/network. Deployment is not verified")
    if status == 403 and blocked_is_access:
        raise AccessBlocked
    if status != 200:
        fail(probe_failure(label, status))
    if len(raw) > 16 * 1024:
        fail(f"{label} returned an oversized response; deployment is not verified")
    try:
        payload = json.loads(raw)
    except (ValueError, UnicodeError):
        fail(f"{label} returned invalid JSON; deployment is not verified")
    if not isinstance(payload, dict):
        fail(f"{label} returned an unexpected payload; deployment is not verified")
    return payload


def probe_failure(label: str, status: int) -> str:
    """診断失敗の一般文言。403 を [`AccessBlocked`] として扱わない呼び出しで使う。"""
    return f"{label} failed (HTTP {status}); check Worker/Access authentication and configuration"


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


def verify_s3(base_url: str) -> str:
    """選択済み S3 handle の実 LIST を確認する。失敗・未実行は成功扱いしない。

    戻り値はステップサマリに出す状態文字列で、`"ok"` か
    `"skipped: <理由>"`。401 / 503 / 到達不可などの失敗は例外で終わる。

    403 だけは前段 (Cloudflare Access / WAF) が Worker より先に返した印であり、
    CI 側からは回避できないため、警告つきのスキップとして報告する (`summary` も
    `skipped` と表示する)。成功扱いにはしない。
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
        return "skipped: admin token is available only in the Secrets Store"
    try:
        payload = probe_json(
            base_url,
            "/api/storage/mode?probe=s3",
            "S3 verification",
            blocked_is_access=True,
        )
    except AccessBlocked as blocked:
        detail = f" (edge response headers: {blocked.detail})" if blocked.detail else ""
        print(f"::warning::{ACCESS_BLOCKED_NOTICE}{detail}")
        return "skipped: blocked before the Worker (Cloudflare Access or WAF); not verified"
    if (payload.get("success") is not True
            or payload.get("metadata_backend") != "d1"
            or payload.get("illustration_backend") != "s3" or payload.get("s3_required") is not True
            or payload.get("s3_list") != "ok"):
        fail("S3 verification did not confirm D1 metadata, required S3 illustrations and a successful S3 LIST")
    print("S3 deployment check: selected backend and read-only LIST verified")
    return "ok"


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
    for key in (
        "database_name", "database_id", "queue_name", "dead_letter_queue",
        "convert_queue_name", "convert_dead_letter_queue",
    ):
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
    env["NAROU_CONVERT_QUEUE"] = resources["convert_queue_name"]
    env["NAROU_CONVERT_DLQ"] = resources["convert_dead_letter_queue"]
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
    # 外部の取得リレー（任意）。値があるときだけ Worker secret として渡す。
    # 未設定なら Worker 側のリレー段は無効のまま（Cloudflare から取れないサイトが残る）。
    # RELAY_DOMAIN があれば https://<domain> を公開先として優先する（名前付きトンネル用）。
    # 公開先とキーは両方揃ったときだけ入れる（片方だけ残して古い値と組み合わさらないように）。
    relay_domain = os.environ.get("RELAY_DOMAIN", "").strip()
    relay_endpoint = os.environ.get("NAROU_RELAY_ENDPOINT", "").strip()
    if relay_domain:
        relay_endpoint = relay_domain if "://" in relay_domain else f"https://{relay_domain}"
    relay_key = os.environ.get("NAROU_RELAY_KEY", "").strip()
    if relay_endpoint and relay_key:
        secrets["NAROU_RELAY_ENDPOINT"] = relay_endpoint
        secrets["NAROU_RELAY_KEY"] = relay_key
    else:
        missing = []
        if not relay_endpoint:
            missing.append("RELAY_DOMAIN / NAROU_RELAY_ENDPOINT")
        if not relay_key:
            missing.append("NAROU_RELAY_KEY")
        print(f"::notice::{', '.join(missing)} is not set; the relay fallback stays disabled")

    if not secrets:
        return None
    # 値は GitHub のログマスクに登録する。wrangler の出力に値が入っている場合も
    # ログには伏せられる (登録は以降の出力に効く)。
    for value in secrets.values():
        mask(value)
    path = WORKER_DIR / f".deploy-secrets-{target}.json"
    path.write_text(json.dumps(secrets), encoding="utf-8")
    # 同ホストの別ユーザーから読まれないよう、秘密ファイルは 0600 にする。
    if os.name != "nt":
        os.chmod(path, 0o600)
    return path


def run_deploy(secrets: Path | None) -> tuple[int, str]:
    """`wrangler deploy` を実行し `(exit code, log)` を返す (失敗でも落とさない)。

    wrangler は固定バージョン ([`wrangler_command`]) を使う。
    """
    command = wrangler_command("deploy", "-c", "wrangler.ci.toml")
    if secrets is not None:
        command += ["--secrets-file", str(secrets)]
    result = subprocess.run(
        command,
        cwd=WORKER_DIR,
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )
    return result.returncode, (result.stdout or "") + (result.stderr or "")


def deploy(
    secrets: Path | None,
    target: str,
    resources: dict[str, str],
) -> str | None:
    """デプロイし、wrangler が報告した workers.dev の URL を返す（無ければ None）。

    custom domain 運用 (`workers_dev = false`) では有人の URL を出さないため、
    smoke の宛先は `smoke_url()` が custom domain から決める。ログ中の任意の
    https URL を拾うと無関係なリンクを掴むので、フォールバックはしない。

    ホスト名の DNS レコードを既存のフロントエンド等が持っている場合、custom
    domain は付け替えられない (`externally managed DNS records`)。そのときは
    `NAROU_DOMAIN_MODE=route` で組み直し、同じホスト名に Workers の route と
    して載せ直す (DNS は触らない・Access はホスト名単位のまま)。
    """
    code, log = run_deploy(secrets)
    if code != 0 and DNS_CONFLICT_MARKER in log:
        print(
            "::notice::ホスト名の既存 DNS レコードのため custom domain を route に切り替えます "
            "(NAROU_DOMAIN_MODE=route)"
        )
        os.environ["NAROU_DOMAIN_MODE"] = "route"
        render(target, resources)
        code, log = run_deploy(secrets)
    print(log)
    if code != 0:
        fail(
            f"npx --yes wrangler@{wrangler_version()} deploy -c wrangler.ci.toml "
            f"failed with {code}"
        )
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


def summary(target: str, url: str, smoke_ok: bool | None, *, hidden_url: bool = False,
            s3_status: str = "not verified") -> None:
    lines = [
        f"### Worker deploy ({target})",
        "",
        f"- URL: {'（custom domain。ログでは伏せています）' if hidden_url else url}",
        f"- S3 backend + LIST: {s3_status}",
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
    # wrangler はデプロイと同じ固定バージョンで migration を適用する。
    run(
        wrangler_command(
            "d1",
            "migrations",
            "apply",
            database,
            "--remote",
            "-c",
            "wrangler.ci.toml",
        )
    )

    secrets = secret_file(target)
    try:
        url = deploy(secrets, target, resources)
    finally:
        if secrets is not None:
            secrets.unlink(missing_ok=True)

    target_url, hidden_url = smoke_url(target, url)
    if target_url is None:
        fail("Mandatory S3 verification needs a deployment URL; configure NAROU_DEPLOY_URL or a deployment domain")
    s3_status = verify_s3(target_url)
    smoke_result: bool | None = None
    if os.environ.get("NAROU_SMOKE", "1") == "0":
        pass
    else:
        smoke_result = smoke(target_url)
    summary(target, target_url, smoke_result, hidden_url=hidden_url, s3_status=s3_status)
    if smoke_result is False:
        fail("smoke test failed against the deployment")


if __name__ == "__main__":
    main()
