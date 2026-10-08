"""`wrangler.<target>.toml` をレンダリングして `wrangler.ci.toml` を作る。

account 固有の値 (D1 の database_id、S3 の接続先、公開ドメイン) はリポジトリに
置かず、CI の環境変数から注入する。値の形式は置換前に検証し、未解決の
プレースホルダが残っていれば失敗させる。
"""

from __future__ import annotations

import os
import re
import tomllib
from pathlib import Path
from typing import NoReturn
from uuid import UUID

TARGETS = ("develop", "production")
NAME_PATTERN = re.compile(r"^[a-z0-9][a-z0-9-]{0,62}$")
BUCKET_PATTERN = re.compile(r"^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$")
REGION_PATTERN = re.compile(r"^[a-z0-9][a-z0-9-]{1,62}$")
SECRET_NAME_PATTERN = re.compile(r"^[A-Za-z0-9._-]+$")
PREFIX_PATTERN = re.compile(r"^[A-Za-z0-9._/-]+$")
HOSTNAME_PATTERN = re.compile(r"^[A-Za-z0-9.-]+$")

WORKER_DIR = Path(__file__).resolve().parent.parent


def fail(message: str) -> NoReturn:
    raise SystemExit(message)


def required(name: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        fail(f"Missing deployment value: {name}")
    return value


def optional(name: str, default: str) -> str:
    value = os.environ.get(name, "").strip()
    return value or default


def unresolved_placeholders(template: str) -> list[str]:
    """コメント行を除いた本文に残った `__NAME__` を列挙する。"""

    found: list[str] = []
    for line in template.splitlines():
        stripped = line.strip()
        if stripped.startswith("#"):
            continue
        found.extend(re.findall(r"__[A-Z][A-Z0-9_]*__", stripped))
    return found


def main() -> None:
    target = required("NAROU_DEPLOY_TARGET")
    if target not in TARGETS:
        fail("NAROU_DEPLOY_TARGET must be develop or production")

    database_name = required("NAROU_D1_DATABASE_NAME")
    database_id = required("NAROU_D1_DATABASE_ID")
    queue_name = required("NAROU_JOB_QUEUE")
    dead_letter_queue = required("NAROU_JOB_DLQ")

    suffix = f"-{target}"
    for label, value in (
        ("NAROU_D1_DATABASE_NAME", database_name),
        ("NAROU_JOB_QUEUE", queue_name),
    ):
        if not NAME_PATTERN.fullmatch(value):
            fail(f"{label} must be a lowercase name (a-z, 0-9, -)")
        if not value.endswith(suffix):
            fail(f"{label} must end with the target suffix {suffix!r}")
        base = value[: -len(suffix)]
        if not NAME_PATTERN.fullmatch(base):
            fail(f"{label} has an invalid base name")
    # DLQ は queue 名 + "-dlq" で導出される (provision_resources.py と同じ規則)。
    if dead_letter_queue != f"{queue_name}-dlq":
        fail("NAROU_JOB_DLQ must be the job queue name with a -dlq suffix")
    if not NAME_PATTERN.fullmatch(dead_letter_queue) or len(dead_letter_queue) > 63:
        fail("NAROU_JOB_DLQ must be a valid lowercase queue name (max 63 characters)")
    try:
        UUID(database_id)
    except ValueError:
        fail("NAROU_D1_DATABASE_ID must be a UUID")

    # 挿絵の保存先は 2 通り:
    #   (a) 値を vars で渡す (既定) — endpoint/region/bucket をそのまま書く
    #   (b) Cloudflare Secrets Store を使う — `NAROU_SECRETS_STORE_ID` と
    #       5 つの `<変数名>_SECRET_NAME` を渡し、値はストア側に置く。
    #       リポジトリと CI には「名前」しか残らない (Dantalian と同じ方式)。
    secret_store_id = optional("NAROU_SECRETS_STORE_ID", "").strip()
    s3_secret_names = {
        "S3_ACCESS_KEY_ID_SECRET_NAME": optional("NAROU_S3_ACCESS_KEY_ID_SECRET_NAME", "").strip(),
        "S3_SECRET_ACCESS_KEY_SECRET_NAME": optional(
            "NAROU_S3_SECRET_ACCESS_KEY_SECRET_NAME", ""
        ).strip(),
        "S3_ENDPOINT_SECRET_NAME": optional("NAROU_S3_ENDPOINT_SECRET_NAME", "").strip(),
        "S3_REGION_SECRET_NAME": optional("NAROU_S3_REGION_SECRET_NAME", "").strip(),
        "S3_BUCKET_SECRET_NAME": optional("NAROU_S3_BUCKET_SECRET_NAME", "").strip(),
    }
    # S3 の store モードは 5 つ揃ったときだけ成立し、1 つでも欠ければ失敗。
    # トークン系 (下) とは独立に選べる。
    s3_store_mode = any(s3_secret_names.values())
    if s3_store_mode and not all(s3_secret_names.values()):
        fail(
            "Secrets Store に S3 を置く場合は NAROU_S3_*_SECRET_NAME を 5 つ揃えて渡す"
        )
    # アプリのトークンと復号鍵も Secrets Store に置ける（任意）。名前を渡した
    # 項目だけ <NAME>_STORE バインディングを足す。S3 を vars のままにして
    # トークンだけストアへ置く組み合わせも可能。
    token_secrets = {
        "NAROU_ADMIN_TOKEN": optional("NAROU_ADMIN_TOKEN_SECRET_NAME", "").strip(),
        "NAROU_RS_LOGIN_KEY": optional("NAROU_RS_LOGIN_KEY_SECRET_NAME", "").strip(),
    }
    store_consumers = s3_store_mode or any(token_secrets.values())
    if store_consumers and not secret_store_id:
        fail("NAROU_SECRETS_STORE_ID is required for Secrets Store bindings")
    if secret_store_id:
        # ID の形式は利用者の有無に関係なく検証する。
        if not SECRET_NAME_PATTERN.fullmatch(secret_store_id):
            fail("NAROU_SECRETS_STORE_ID has an invalid value")
        if not store_consumers:
            fail(
                "NAROU_SECRETS_STORE_ID is set but no *_SECRET_NAME uses it; "
                "set the secret names or unset the store ID"
            )
    for label, value in s3_secret_names.items():
        if value and not SECRET_NAME_PATTERN.fullmatch(value):
            fail(f"{label} has an invalid Secrets Store name")
    for label, value in token_secrets.items():
        if value and not SECRET_NAME_PATTERN.fullmatch(value):
            fail(f"{label}_SECRET_NAME has an invalid Secrets Store name")
    if s3_store_mode:
        endpoint = region = bucket = ""
    else:
        endpoint = required("NAROU_S3_ENDPOINT")
        if not re.fullmatch(r"https?://[A-Za-z0-9.:/-]+", endpoint):
            fail("NAROU_S3_ENDPOINT must be an http(s) URL without a query or fragment")
        region = required("NAROU_S3_REGION")
        if not REGION_PATTERN.fullmatch(region):
            fail("NAROU_S3_REGION must be a lowercase region name")
        bucket = required("NAROU_S3_BUCKET")
        if not BUCKET_PATTERN.fullmatch(bucket):
            fail("NAROU_S3_BUCKET must be a valid S3 bucket name")
    prefix = optional("NAROU_S3_PREFIX", f"narou/{target}").strip("/")
    if not PREFIX_PATTERN.fullmatch(prefix):
        fail("NAROU_S3_PREFIX must be a key prefix without spaces")

    replacements = {
        "NAROU_D1_DATABASE_NAME": database_name,
        "NAROU_D1_DATABASE_ID": database_id,
        "NAROU_JOB_QUEUE": queue_name,
        "NAROU_JOB_DLQ": dead_letter_queue,
        "S3_ENDPOINT": endpoint,
        "S3_REGION": region,
        "S3_BUCKET": bucket,
        "S3_PREFIX": prefix,
    }
    if secret_store_id:
        # 名前を渡した項目だけ <NAME>_STORE バインディングを足す
        # (S3 の 5 つは全部入るか全部入らないか。トークン系は個別)。
        blocks = "".join(
            f'\n[[secrets_store_secrets]]\nbinding = "{binding}"\n'
            f'store_id = "{secret_store_id}"\nsecret_name = "{name}"\n'
            for binding, name in (
                ("S3_ACCESS_KEY_ID_STORE", s3_secret_names["S3_ACCESS_KEY_ID_SECRET_NAME"]),
                ("S3_SECRET_ACCESS_KEY_STORE", s3_secret_names["S3_SECRET_ACCESS_KEY_SECRET_NAME"]),
                ("S3_ENDPOINT_STORE", s3_secret_names["S3_ENDPOINT_SECRET_NAME"]),
                ("S3_REGION_STORE", s3_secret_names["S3_REGION_SECRET_NAME"]),
                ("S3_BUCKET_STORE", s3_secret_names["S3_BUCKET_SECRET_NAME"]),
                ("NAROU_ADMIN_TOKEN_STORE", token_secrets["NAROU_ADMIN_TOKEN"]),
                ("NAROU_RS_LOGIN_KEY_STORE", token_secrets["NAROU_RS_LOGIN_KEY"]),
            )
            if name
        )
        replacements["__SECRET_STORE_BLOCKS__"] = blocks
    # custom domain の route。production は必須、develop は任意で、未設定なら
    # workers.dev の URL だけで動く。前段に Cloudflare Access (Zero Trust) を
    # 置く前提なので、route を入れた環境では workers_dev を閉じる。
    domain_var = {"production": "SERVICE_DOMAIN", "develop": "DEVELOP_DOMAIN"}[target]
    domain = required(domain_var) if target == "production" else optional(domain_var, "").strip()
    if domain:
        if not HOSTNAME_PATTERN.fullmatch(domain) or "." not in domain:
            fail(f"{domain_var} must be a hostname without a scheme or path")
        # custom domain は Cloudflare に DNS レコードを作らせる (= 既存レコードが
        # あると "externally managed DNS records" で失敗する)。SORAHOST の
        # コネクタのように既存レコードがそのホスト名を握っている場合は、
        # `NAROU_DOMAIN_MODE=route` で Workers の route として載せる — DNS は
        # 触らず、そのホスト名への要求は Workers が前段で受ける (Access も
        # ホスト名単位のまま効く)。
        mode = optional("NAROU_DOMAIN_MODE", "custom_domain").strip().lower()
        if mode not in ("custom_domain", "route"):
            fail("NAROU_DOMAIN_MODE must be custom_domain or route")
        if mode == "route":
            zone = optional("NAROU_ZONE_NAME", "").strip()
            if zone and (not HOSTNAME_PATTERN.fullmatch(zone) or "." not in zone):
                fail("NAROU_ZONE_NAME must be a hostname without a scheme or path")
            zone_line = f'zone_name = "{zone}"\n' if zone else ""
            replacements["__CUSTOM_DOMAIN_BLOCK__"] = (
                f'\n[[routes]]\npattern = "{domain}/*"\n'
                f"{zone_line}custom_domain = false\n"
            )
        else:
            replacements["__CUSTOM_DOMAIN_BLOCK__"] = (
                f'\n[[routes]]\npattern = "{domain}"\ncustom_domain = true\n'
            )
    # workers_dev は domain の有無から決める (NAROU_WORKERS_DEV で明示もできる)。
    workers_dev = optional("NAROU_WORKERS_DEV", "").strip().lower()
    if not workers_dev:
        workers_dev = "false" if domain else "true"
    if workers_dev not in ("true", "false"):
        fail("NAROU_WORKERS_DEV must be true or false")
    replacements["WORKERS_DEV"] = workers_dev
    # Zero Trust が境界なら false。既定は今までどおり true (fail-closed)。
    auth_required = optional("NAROU_AUTH_REQUIRED", "true").strip().lower()
    if auth_required not in ("true", "false"):
        fail("NAROU_AUTH_REQUIRED must be true or false")
    replacements["NAROU_AUTH_REQUIRED"] = auth_required

    template_path = WORKER_DIR / f"wrangler.{target}.toml"
    template = template_path.read_text(encoding="utf-8")
    # ブロック単位のプレースホルダは「値があれば差し込み、無ければ空にする」。
    for block in ("__SECRET_STORE_BLOCKS__", "__CUSTOM_DOMAIN_BLOCK__"):
        template = template.replace(block, replacements.pop(block, ""))
    for name, value in replacements.items():
        template = template.replace("__" + name + "__", value)
    remaining = unresolved_placeholders(template)
    if remaining:
        names = ", ".join(sorted(set(remaining)))
        fail(f"{template_path.name} still contains unresolved placeholders: {names}")

    # CI は必ず挿絵を S3 へ保存する。ローカル wrangler.toml の互換モードと
    # 分け、環境変数やテンプレートの編集でこの契約を無効化させない。
    try:
        config = tomllib.loads(template)
    except tomllib.TOMLDecodeError:
        fail(f"{template_path.name} is not valid TOML")
    if config.get("vars", {}).get("NAROU_REQUIRE_S3") != "true":
        fail(f'{template_path.name} must set vars.NAROU_REQUIRE_S3 = "true" for CI')

    output_path = WORKER_DIR / "wrangler.ci.toml"
    output_path.write_text(template, encoding="utf-8")
    print(output_path)


if __name__ == "__main__":
    main()
