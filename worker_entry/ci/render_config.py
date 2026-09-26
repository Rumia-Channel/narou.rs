"""`wrangler.<target>.toml` をレンダリングして `wrangler.ci.toml` を作る。

account 固有の値 (D1 の database_id、S3 の接続先、公開ドメイン) はリポジトリに
置かず、CI の環境変数から注入する。値の形式は置換前に検証し、未解決の
プレースホルダが残っていれば失敗させる。
"""

from __future__ import annotations

import os
import re
from pathlib import Path
from typing import NoReturn
from uuid import UUID

TARGETS = ("develop", "staging", "production")
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
        fail("NAROU_DEPLOY_TARGET must be develop, staging, or production")

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
    secret_names = {
        "S3_ACCESS_KEY_ID_SECRET_NAME": optional("NAROU_S3_ACCESS_KEY_ID_SECRET_NAME", "").strip(),
        "S3_SECRET_ACCESS_KEY_SECRET_NAME": optional(
            "NAROU_S3_SECRET_ACCESS_KEY_SECRET_NAME", ""
        ).strip(),
        "S3_ENDPOINT_SECRET_NAME": optional("NAROU_S3_ENDPOINT_SECRET_NAME", "").strip(),
        "S3_REGION_SECRET_NAME": optional("NAROU_S3_REGION_SECRET_NAME", "").strip(),
        "S3_BUCKET_SECRET_NAME": optional("NAROU_S3_BUCKET_SECRET_NAME", "").strip(),
    }
    store_mode = bool(secret_store_id) or any(secret_names.values())
    if store_mode:
        if not secret_store_id or not all(secret_names.values()):
            fail(
                "Secrets Store を使う場合は NAROU_SECRETS_STORE_ID と "
                "NAROU_S3_*_SECRET_NAME を 5 つ揃えて渡す"
            )
        if not SECRET_NAME_PATTERN.fullmatch(secret_store_id):
            fail("NAROU_SECRETS_STORE_ID has an invalid value")
        for label, value in secret_names.items():
            if not SECRET_NAME_PATTERN.fullmatch(value):
                fail(f"{label} has an invalid Secrets Store name")
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
    if store_mode:
        blocks = "".join(
            f'\n[[secrets_store_secrets]]\nbinding = "{binding}"\n'
            f'store_id = "{secret_store_id}"\nsecret_name = "{name}"\n'
            for binding, name in (
                ("S3_ACCESS_KEY_ID_STORE", secret_names["S3_ACCESS_KEY_ID_SECRET_NAME"]),
                ("S3_SECRET_ACCESS_KEY_STORE", secret_names["S3_SECRET_ACCESS_KEY_SECRET_NAME"]),
                ("S3_ENDPOINT_STORE", secret_names["S3_ENDPOINT_SECRET_NAME"]),
                ("S3_REGION_STORE", secret_names["S3_REGION_SECRET_NAME"]),
                ("S3_BUCKET_STORE", secret_names["S3_BUCKET_SECRET_NAME"]),
            )
        )
        replacements["__SECRET_STORE_BLOCKS__"] = blocks
    if target == "production":
        service_domain = required("SERVICE_DOMAIN")
        if not HOSTNAME_PATTERN.fullmatch(service_domain) or "." not in service_domain:
            fail("SERVICE_DOMAIN must be a hostname without a scheme or path")
        replacements["SERVICE_DOMAIN"] = service_domain

    template_path = WORKER_DIR / f"wrangler.{target}.toml"
    template = template_path.read_text(encoding="utf-8")
    if "__SECRET_STORE_BLOCKS__" in template:
        template = template.replace("__SECRET_STORE_BLOCKS__", replacements.pop("__SECRET_STORE_BLOCKS__", ""))
    else:
        replacements.pop("__SECRET_STORE_BLOCKS__", None)
    for name, value in replacements.items():
        template = template.replace("__" + name + "__", value)
    remaining = unresolved_placeholders(template)
    if remaining:
        names = ", ".join(sorted(set(remaining)))
        fail(f"{template_path.name} still contains unresolved placeholders: {names}")

    output_path = WORKER_DIR / "wrangler.ci.toml"
    output_path.write_text(template, encoding="utf-8")
    print(output_path)


if __name__ == "__main__":
    main()
