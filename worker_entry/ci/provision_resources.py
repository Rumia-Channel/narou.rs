"""D1 と Queue を冪等に用意し、導出した名前を GITHUB_OUTPUT へ渡す。

`list → create → 再 list` の順で扱うため、並行ワークフローが同時に走っても
二重作成で失敗しない。queue は本体と `-dlq` の 2 本を作る。
"""

from __future__ import annotations

import json
import os
import re
import subprocess
from pathlib import Path
from typing import Any, NoReturn

TARGETS = ("develop", "staging", "production")
NAME_PATTERN = re.compile(r"^[a-z0-9][a-z0-9-]{0,62}$")
UUID_PATTERN = re.compile(
    r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b"
)
# Cloudflare の D1 / Queue 名は 63 文字まで。
MAX_NAME_LENGTH = 63


def fail(message: str) -> NoReturn:
    raise SystemExit(message)


def wrangler(*args: str) -> subprocess.CompletedProcess[str]:
    """`npx wrangler <args>` を実行して生の結果を返す (成否の判断は呼び手)。"""

    return subprocess.run(
        ["npx", "wrangler", *args],
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )


def run_wrangler(*args: str) -> str:
    result = wrangler(*args)
    if result.returncode != 0:
        details = (result.stderr or result.stdout).strip()
        fail(f"wrangler {' '.join(args)} failed: {details}")
    return result.stdout


def parse_json_output(output: str) -> Any:
    stripped = output.strip()
    if not stripped:
        fail("wrangler returned no output")
    try:
        return json.loads(stripped)
    except json.JSONDecodeError:
        # wrangler は警告行を混ぜることがあるため、最後の JSON ブロックを拾う。
        start = min(
            (index for index in (stripped.find("["), stripped.find("{")) if index >= 0),
            default=-1,
        )
        if start >= 0:
            try:
                return json.loads(stripped[start:])
            except json.JSONDecodeError:
                pass
    fail("wrangler returned invalid JSON")


def database_records() -> list[dict[str, Any]]:
    payload = parse_json_output(run_wrangler("d1", "list", "--json"))
    if not isinstance(payload, list):
        fail("wrangler d1 list returned an unexpected payload")
    return [record for record in payload if isinstance(record, dict)]


def database_id(record: dict[str, Any]) -> str | None:
    for key in ("uuid", "database_id", "id"):
        value = record.get(key)
        if isinstance(value, str) and UUID_PATTERN.fullmatch(value):
            return value
    return None


def ensure_database(name: str) -> str:
    for record in database_records():
        if record.get("name") == name:
            identifier = database_id(record)
            if identifier:
                return identifier
            fail(f"D1 database {name!r} has no UUID in Wrangler output")

    create = wrangler("d1", "create", name)
    if create.returncode == 0:
        match = UUID_PATTERN.search(create.stdout + create.stderr)
        if match:
            return match.group(0)

    # 並行ワークフローが list と create の間に作成した可能性を再確認する。
    for record in database_records():
        if record.get("name") == name:
            identifier = database_id(record)
            if identifier:
                return identifier
    details = (create.stderr or create.stdout).strip()
    fail(f"could not create or resolve D1 database {name!r}: {details}")


def queue_records() -> list[tuple[str, str]]:
    output = run_wrangler("queues", "list")
    records: list[tuple[str, str]] = []
    for line in output.splitlines():
        columns = [column.strip() for column in line.split("│")]
        if len(columns) < 4:
            continue
        identifier, name = columns[1], columns[2]
        if identifier and name and identifier.lower() != "id" and name.lower() != "name":
            records.append((identifier, name))
    return records


def ensure_queue(name: str) -> None:
    if any(queue_name == name for _, queue_name in queue_records()):
        return

    create = wrangler("queues", "create", name)
    if create.returncode == 0:
        return

    if any(queue_name == name for _, queue_name in queue_records()):
        return
    details = (create.stderr or create.stdout).strip()
    fail(f"could not create or resolve Queue {name!r}: {details}")


def write_outputs(values: dict[str, str]) -> None:
    output_path = os.environ.get("GITHUB_OUTPUT")
    if output_path:
        with Path(output_path).open("a", encoding="utf-8") as output:
            for key, value in values.items():
                output.write(f"{key}={value}\n")
    else:
        for key, value in values.items():
            print(f"{key}={value}")


def main() -> None:
    target = os.environ.get("NAROU_DEPLOY_TARGET", "").strip()
    base_database = os.environ.get("NAROU_D1_BASE_NAME", "narou-rs").strip()
    base_queue = os.environ.get("NAROU_JOB_QUEUE_BASE", "narou-jobs").strip()
    if target not in TARGETS:
        fail("NAROU_DEPLOY_TARGET must be develop, staging, or production")
    for label, value in (
        ("NAROU_D1_BASE_NAME", base_database),
        ("NAROU_JOB_QUEUE_BASE", base_queue),
    ):
        if not NAME_PATTERN.fullmatch(value):
            fail(f"{label} must be a lowercase base name without an environment suffix")
        if any(value.endswith(f"-{name}") for name in TARGETS):
            fail(f"{label} must not include a target suffix")

    database_name = f"{base_database}-{target}"
    queue_name = f"{base_queue}-{target}"
    dead_letter_queue = f"{queue_name}-dlq"
    if any(
        len(name) > MAX_NAME_LENGTH
        for name in (database_name, queue_name, dead_letter_queue)
    ):
        fail("derived D1 or Queue name exceeds 63 characters")

    database_uuid = ensure_database(database_name)
    ensure_queue(queue_name)
    ensure_queue(dead_letter_queue)
    write_outputs(
        {
            "database_name": database_name,
            "database_id": database_uuid,
            "queue_name": queue_name,
            "dead_letter_queue": dead_letter_queue,
        }
    )


if __name__ == "__main__":
    main()
