"""`wrangler.<target>.toml` をレンダリングして `wrangler.ci.toml` を作る。

reader (配信専用) は writer と同じ Rust 成果物を別の script 名で配備するため、
`--role reader` で `wrangler.reader.ci.toml` も作る。
`--role acquisition --lane 0..3` は取得専用の同じ成果物を別 script に配備する
`wrangler.acquire-1.ci.toml` ... `wrangler.acquire-4.ci.toml` を作る。

account 固有の値 (D1 の database_id、S3 の接続先、公開ドメイン) はリポジトリに
置かず、CI の環境変数から注入する。値の形式は置換前に検証し、未解決の
プレースホルダが残っていれば失敗させる。
"""

from __future__ import annotations

import os
import re
import sys
import tomllib
from pathlib import Path
from typing import NoReturn, Sequence
from uuid import UUID

TARGETS = ("develop", "production")
ROLES = ("writer", "reader", "acquisition")
ACQUISITION_LANE_COUNT = 4
NAME_PATTERN = re.compile(r"^[a-z0-9][a-z0-9-]{0,62}$")
BUCKET_PATTERN = re.compile(r"^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$")
REGION_PATTERN = re.compile(r"^[a-z0-9][a-z0-9-]{1,62}$")
SECRET_NAME_PATTERN = re.compile(r"^[A-Za-z0-9._-]+$")
PREFIX_PATTERN = re.compile(r"^[A-Za-z0-9._/-]+$")
HOSTNAME_PATTERN = re.compile(r"^[A-Za-z0-9.-]+$")

# reader の script 名は writer の名前の後ろに付ける (既存 writer は変えない)。
READER_NAME_SUFFIX = "-reader"

# reader が edge で受け取る配信パスの接頭辞。Cloudflare の Route はパス途中の
# ワイルドカードに対応しないため、末尾の `*` で前方一致させる。
# パスの境界は Worker が検証し、既存ドメインの URL と Access を維持する。
READER_ROUTE_PREFIXES = (
    "/api/v1",
    "/api/read/v1",
    "/api/novels",
    "/api/virtual_novels",
    "/novels",
)

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


def parse_options(argv: Sequence[str]) -> tuple[str, int | None]:
    """`--role writer|reader|acquisition` と取得レーンの `--lane 0..3`。"""

    role = "writer"
    lane = None
    index = 0
    while index < len(argv):
        argument = argv[index]
        name, separator, value = argument.partition("=")
        if name not in ("--role", "--lane"):
            fail(f"unknown argument: {argument}")
        if not separator:
            index += 1
            if index >= len(argv):
                fail(f"{name} needs a value")
            value = argv[index]
        if name == "--role":
            role = value
        else:
            if value not in tuple(str(lane) for lane in range(ACQUISITION_LANE_COUNT)):
                fail("--lane must be 0, 1, 2 or 3")
            lane = int(value)
        index += 1
    if role not in ROLES:
        fail("--role must be writer, reader or acquisition")
    if role == "acquisition" and lane is None:
        fail("--role acquisition requires --lane 0..3")
    if role != "acquisition" and lane is not None:
        fail("--lane is only valid for --role acquisition")
    return role, lane


def template_name(path: Path) -> str:
    """テンプレートの `name = "..."` を取り出す (プレースホルダ無しの静的値)。"""

    match = re.search(
        r'^name\s*=\s*"([^"]+)"', path.read_text(encoding="utf-8"), re.MULTILINE
    )
    if not match:
        fail(f"{path.name} has no name")
    return match.group(1)


def reader_routes(domain: str, zone: str) -> str:
    """reader へ向ける path route を組み立てる (ドメインが無ければ空)。"""

    if not domain:
        return ""
    zone_line = f'zone_name = "{zone}"\n' if zone else ""
    return "".join(
        f'\n[[routes]]\npattern = "{domain}{prefix}*"\n{zone_line}custom_domain = false\n'
        for prefix in READER_ROUTE_PREFIXES
    )


def secret_store_blocks(
    secret_store_id: str,
    s3_secret_names: dict[str, str],
    token_secrets: dict[str, str],
    access_secrets: dict[str, str],
    *,
    include_login_key: bool,
) -> str:
    """`[[secrets_store_secrets]]` のブロックを組み立てる。

    名前を渡した項目だけバインディングを足す (S3 の 5 つは全部入るか全部入ら
    ないか。トークン系は個別)。reader にはログイン資格情報の暗号鍵を載せない
    (reader は資格情報を読み書きしない)。
    """

    entries = [
        ("S3_ACCESS_KEY_ID_STORE", s3_secret_names["S3_ACCESS_KEY_ID_SECRET_NAME"]),
        ("S3_SECRET_ACCESS_KEY_STORE", s3_secret_names["S3_SECRET_ACCESS_KEY_SECRET_NAME"]),
        ("S3_ENDPOINT_STORE", s3_secret_names["S3_ENDPOINT_SECRET_NAME"]),
        ("S3_REGION_STORE", s3_secret_names["S3_REGION_SECRET_NAME"]),
        ("S3_BUCKET_STORE", s3_secret_names["S3_BUCKET_SECRET_NAME"]),
        ("NAROU_ADMIN_TOKEN_STORE", token_secrets["NAROU_ADMIN_TOKEN"]),
        (
            "NAROU_CF_ACCESS_CLIENT_ID_STORE",
            access_secrets["NAROU_CF_ACCESS_CLIENT_ID"],
        ),
        (
            "NAROU_CF_ACCESS_CLIENT_SECRET_STORE",
            access_secrets["NAROU_CF_ACCESS_CLIENT_SECRET"],
        ),
    ]
    if include_login_key:
        entries.append(("NAROU_RS_LOGIN_KEY_STORE", token_secrets["NAROU_RS_LOGIN_KEY"]))
    return "".join(
        f'\n[[secrets_store_secrets]]\nbinding = "{binding}"\n'
        f'store_id = "{secret_store_id}"\nsecret_name = "{name}"\n'
        for binding, name in entries
        if name
    )


def validate_reader(config: dict, path: Path, writer_name: str) -> None:
    """reader のトポロジーを固定する (queue / cron / DO の新規作成を持ち込まない)。

    reader は配信だけを担う。書き込み系 (queue / cron) と静的アセット、ビルドを
    持ち込むと、reader が更新経路まで引きを受けて(writer と二重に)動作する。
    """

    if config.get("vars", {}).get("NAROU_WORKER_ROLE") != "reader":
        fail(f'{path.name} must set vars.NAROU_WORKER_ROLE = "reader"')
    if config.get("queues"):
        fail(f"{path.name} must not bind queues; the reader only serves reads")
    if config.get("triggers", {}).get("crons"):
        fail(f"{path.name} must not define cron triggers")
    if "assets" in config:
        fail(f"{path.name} must not serve static assets; the writer owns the Web UI")
    if "build" in config:
        fail(f"{path.name} must not build; the reader reuses the writer's artifact")
    if config.get("migrations") or config.get("exports"):
        fail(f"{path.name} must not create Durable Object namespaces")
    bindings = config.get("durable_objects", {}).get("bindings", [])
    names = {binding.get("name") for binding in bindings}
    if "RATE_LIMITER" in names:
        fail(f"{path.name} must not bind RATE_LIMITER")
    push = [binding for binding in bindings if binding.get("name") == "PUSH_HUB"]
    if len(push) != 1 or push[0].get("script_name") != writer_name:
        fail(f"{path.name} must bind the writer's PUSH_HUB through script_name")
    services = {
        service.get("binding"): service.get("service")
        for service in config.get("services", [])
    }
    if services.get("NAROU_WRITER") != writer_name:
        fail(f'{path.name} must bind the writer as the "NAROU_WRITER" service')
    if not config.get("d1_databases"):
        fail(f"{path.name} must bind the shared D1 database")


def validate_queue_producers(config: dict, path: Path, expected: dict[str, str]) -> None:
    entries = config.get("queues", {}).get("producers", [])
    actual = {entry.get("binding"): entry.get("queue") for entry in entries}
    if len(entries) != len(expected) or actual != expected:
        fail(f"{path.name} must bind the deployment's queue producers: {', '.join(expected)}")


def validate_queue_consumers(config: dict, path: Path, queues: dict[str, str]) -> None:
    entries = config.get("queues", {}).get("consumers", [])
    if len(entries) != len(queues) or {entry.get("queue") for entry in entries} != set(queues):
        fail(f"{path.name} must consume only its assigned queues")
    for entry in entries:
        if (entry.get("max_batch_size") != 1 or entry.get("max_concurrency") != 1
                or entry.get("dead_letter_queue") != queues[entry["queue"]]
                or entry.get("max_retries") != 3):
            fail(f"{path.name} consumers must use batch=1, concurrency=1, retries=3 and their own DLQ")


def validate_acquisition(
    config: dict, path: Path, writer_name: str, lane: int,
    producers: dict[str, str], queue: str, dlq: str,
) -> None:
    """各取得 script は自分のキューだけを消費し、writer の DO を共有する。"""
    if config.get("vars", {}).get("NAROU_WORKER_ROLE") != "acquisition":
        fail(f'{path.name} must set vars.NAROU_WORKER_ROLE = "acquisition"')
    if config.get("vars", {}).get("NAROU_ACQUISITION_LANE") != str(lane):
        fail(f"{path.name} must set its assigned vars.NAROU_ACQUISITION_LANE")
    if config.get("workers_dev") is not False or config.get("routes"):
        fail(f"{path.name} must not expose a public HTTP endpoint")
    if "assets" in config or "build" in config or config.get("triggers", {}).get("crons"):
        fail(f"{path.name} must not define assets, builds or cron triggers")
    if config.get("migrations") or config.get("exports"):
        fail(f"{path.name} must not create Durable Object namespaces")
    if config.get("main") != "build/worker/shim.mjs":
        fail(f"{path.name} must reuse the shared Worker artifact")
    validate_queue_producers(config, path, producers)
    validate_queue_consumers(config, path, {queue: dlq})
    bindings = config.get("durable_objects", {}).get("bindings", [])
    expected = {"RATE_LIMITER": "SiteRateLimiter", "PUSH_HUB": "PushHub"}
    if (len(bindings) != len(expected)
            or {row.get("name"): row.get("class_name") for row in bindings} != expected
            or any(row.get("script_name") != writer_name for row in bindings)):
        fail(f"{path.name} must bind the writer's RATE_LIMITER and PUSH_HUB through script_name")
    services = {row.get("binding"): row.get("service") for row in config.get("services", [])}
    if services.get("NAROU_WRITER") != writer_name:
        fail(f"{path.name} must bind the writer as the NAROU_WRITER service")
    databases = config.get("d1_databases", [])
    if len(databases) != 1 or databases[0].get("binding") != "DB":
        fail(f"{path.name} must bind the shared D1 database as DB")


def main(argv: Sequence[str] | None = None) -> None:
    role, lane = parse_options(argv or ())
    target = required("NAROU_DEPLOY_TARGET")
    if target not in TARGETS:
        fail("NAROU_DEPLOY_TARGET must be develop or production")

    database_name = required("NAROU_D1_DATABASE_NAME")
    database_id = required("NAROU_D1_DATABASE_ID")
    queue_name = required("NAROU_JOB_QUEUE")
    dead_letter_queue = required("NAROU_JOB_DLQ")
    convert_queue_name = required("NAROU_CONVERT_QUEUE")
    convert_dead_letter_queue = required("NAROU_CONVERT_DLQ")
    acquisition_queues = [
        required(f"NAROU_ACQUIRE_QUEUE_{number}")
        for number in range(1, ACQUISITION_LANE_COUNT + 1)
    ]
    acquisition_dlqs = [
        required(f"NAROU_ACQUIRE_DLQ_{number}")
        for number in range(1, ACQUISITION_LANE_COUNT + 1)
    ]

    suffix = f"-{target}"
    for label, value in (
        ("NAROU_D1_DATABASE_NAME", database_name),
        ("NAROU_JOB_QUEUE", queue_name),
        ("NAROU_CONVERT_QUEUE", convert_queue_name),
        *(
            (f"NAROU_ACQUIRE_QUEUE_{number}", name)
            for number, name in enumerate(acquisition_queues, start=1)
        ),
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
    if convert_dead_letter_queue != f"{convert_queue_name}-dlq":
        fail("NAROU_CONVERT_DLQ must be the conversion queue name with a -dlq suffix")
    if not NAME_PATTERN.fullmatch(convert_dead_letter_queue) or len(convert_dead_letter_queue) > 63:
        fail("NAROU_CONVERT_DLQ must be a valid lowercase queue name (max 63 characters)")
    if len({queue_name, convert_queue_name, *acquisition_queues}) != ACQUISITION_LANE_COUNT + 2:
        fail("legacy ingress, conversion and acquisition lanes must use different queues")
    for number, (queue, dlq) in enumerate(zip(acquisition_queues, acquisition_dlqs), start=1):
        if dlq != f"{queue}-dlq" or not NAME_PATTERN.fullmatch(dlq):
            fail(f"NAROU_ACQUIRE_DLQ_{number} must be its acquisition queue name with a valid -dlq suffix")
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
    # Zero Trust (Cloudflare Access) の Service Token も Secrets Store に
    # 置ける (任意)。Worker は `NAROU_CF_ACCESS_CLIENT_ID` を読むので、
    # バインディングは `NAROU_CF_ACCESS_CLIENT_ID_STORE` になる。
    access_secrets = {
        "NAROU_CF_ACCESS_CLIENT_ID": optional(
            "NAROU_CF_ACCESS_CLIENT_ID_SECRET_NAME", ""
        ).strip(),
        "NAROU_CF_ACCESS_CLIENT_SECRET": optional(
            "NAROU_CF_ACCESS_CLIENT_SECRET_SECRET_NAME", ""
        ).strip(),
    }
    store_consumers = (
        s3_store_mode
        or any(token_secrets.values())
        or any(access_secrets.values())
    )
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
    for label, value in access_secrets.items():
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
        "NAROU_CONVERT_QUEUE": convert_queue_name,
        "NAROU_CONVERT_DLQ": convert_dead_letter_queue,
        **{
            f"NAROU_ACQUIRE_QUEUE_{number}": name
            for number, name in enumerate(acquisition_queues, start=1)
        },
        "S3_ENDPOINT": endpoint,
        "S3_REGION": region,
        "S3_BUCKET": bucket,
        "S3_PREFIX": prefix,
    }
    if secret_store_id:
        # 名前を渡した項目だけ <NAME>_STORE バインディングを足す
        # (S3 の 5 つは全部入るか全部入らないか。トークン系は個別)。
        replacements["__SECRET_STORE_BLOCKS__"] = secret_store_blocks(
            secret_store_id,
            s3_secret_names,
            token_secrets,
            access_secrets,
            include_login_key=(role in ("writer", "acquisition")),
        )
    # custom domain の route。production は必須、develop は任意で、未設定なら
    # workers.dev の URL だけで動く。前段に Cloudflare Access (Zero Trust) を
    # 置く前提なので、route を入れた環境では workers_dev を閉じる。
    domain_var = {"production": "SERVICE_DOMAIN", "develop": "DEVELOP_DOMAIN"}[target]
    domain = (
        required(domain_var) if target == "production" and role != "acquisition"
        else optional(domain_var, "").strip()
    )
    zone = ""
    if domain:
        if not HOSTNAME_PATTERN.fullmatch(domain) or "." not in domain:
            fail(f"{domain_var} must be a hostname without a scheme or path")
        # custom domain は Cloudflare に DNS レコードを作らせる (= 既存レコードが
        # あると "externally managed DNS records" で失敗する)。既存の
        # フロントエンド等がそのホスト名を握っている場合は、
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

    # writer の script 名から reader の名前と参照先を決める。既存の writer の
    # 名前と DO 名前空間は変えない (reader は writer 名 + "-reader")。
    writer_template = WORKER_DIR / f"wrangler.{target}.toml"
    writer_name = template_name(writer_template) if role != "writer" else ""
    if role == "reader":
        # writer の既存 script 名をそのまま参照する。
        reader_name = f"{writer_name}{READER_NAME_SUFFIX}"
        if not NAME_PATTERN.fullmatch(reader_name):
            fail("the reader name must be a lowercase name (a-z, 0-9, -)")
        replacements["READER_NAME"] = reader_name
        replacements["WRITER_NAME"] = writer_name
        # reader は既存ドメインの配信パスだけを向き取る (DNS / Access は触らない)。
        replacements["__READER_ROUTES_BLOCK__"] = reader_routes(domain, zone)
    elif role == "acquisition":
        acquisition_name = f"{writer_name}-acquire-{lane + 1}"
        if not NAME_PATTERN.fullmatch(acquisition_name):
            fail("the acquisition script name must be a valid lowercase name (max 63 characters)")
        replacements["ACQUISITION_NAME"] = acquisition_name
        replacements["ACQUISITION_LANE"] = str(lane)
        replacements["WRITER_NAME"] = writer_name
        replacements["NAROU_ACQUIRE_QUEUE"] = acquisition_queues[lane]
        replacements["NAROU_ACQUIRE_DLQ"] = acquisition_dlqs[lane]

    template_path = {
        "writer": writer_template,
        "reader": WORKER_DIR / "wrangler.reader.toml",
        "acquisition": WORKER_DIR / "wrangler.acquisition.toml",
    }[role]
    template = template_path.read_text(encoding="utf-8")
    # ブロック単位のプレースホルダは「値があれば差し込み、無ければ空にする」。
    for block in ("__SECRET_STORE_BLOCKS__", "__CUSTOM_DOMAIN_BLOCK__", "__READER_ROUTES_BLOCK__"):
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
    if role == "reader":
        validate_reader(config, template_path, writer_name)
    elif role == "acquisition":
        validate_acquisition(
            config, template_path, writer_name, lane,
            {"CONVERT_QUEUE": convert_queue_name, **{
                f"ACQUISITION_QUEUE_{index}": name
                for index, name in enumerate(acquisition_queues)
            }},
            acquisition_queues[lane], acquisition_dlqs[lane],
        )
    else:
        if config.get("vars", {}).get("NAROU_WORKER_ROLE", "writer") != "writer":
            fail(f'{template_path.name} must set vars.NAROU_WORKER_ROLE = "writer"')
        if "build" in config:
            fail(f"{template_path.name} must reuse the artifact built once by ci/deploy_worker.py")
        validate_queue_producers(config, template_path, {
            "NAROU_JOBS": queue_name, "CONVERT_QUEUE": convert_queue_name, **{
                f"ACQUISITION_QUEUE_{index}": name
                for index, name in enumerate(acquisition_queues)
            },
        })
        validate_queue_consumers(config, template_path, {
            queue_name: dead_letter_queue, convert_queue_name: convert_dead_letter_queue,
        })

    output_name = {
        "writer": "wrangler.ci.toml",
        "reader": "wrangler.reader.ci.toml",
        "acquisition": f"wrangler.acquire-{lane + 1}.ci.toml" if lane is not None else "",
    }[role]
    output_path = WORKER_DIR / output_name
    output_path.write_text(template, encoding="utf-8")
    print(output_path)


if __name__ == "__main__":
    main(sys.argv[1:])
