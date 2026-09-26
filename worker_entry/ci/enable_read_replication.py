"""D1 の read replication を有効にする（冪等）。

Cloudflare では read replication は **D1 Sessions API と併せて初めて効く**
（未使用なら全クエリが primary で実行され続ける）。有効化は REST API しか
手段が無く、`wrangler d1` に該当コマンドが無い。develop / production の
どちらのデプロイでも同じ経路を通す（`deploy_worker.py` から呼ばれる）。

必要な環境変数:

- `CLOUDFLARE_ACCOUNT_ID` / `CLOUDFLARE_API_TOKEN` … トークンには `D1:Edit` が要る
- `NAROU_D1_DATABASE_ID` … 対象 DB の UUID（`provision_resources.py` が導出する）
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request
from typing import Any, NoReturn

API = "https://api.cloudflare.com/client/v4"


def fail(message: str) -> NoReturn:
    raise SystemExit(message)


def required(name: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        fail(f"Missing required value: {name}")
    return value


def request(method: str, path: str, token: str, payload: dict[str, Any] | None = None) -> Any:
    body = json.dumps(payload).encode("utf-8") if payload is not None else None
    request = urllib.request.Request(
        API + path,
        data=body,
        method=method,
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(request) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", "replace")
        fail(
            f"{method} {path} failed ({error.code}): {detail}\n"
            "ヒント: トークンに D1:Edit 権限があるか確認してください。"
        )


def read_replication_mode(payload: Any) -> str | None:
    result = payload.get("result") if isinstance(payload, dict) else None
    if not isinstance(result, dict):
        return None
    replication = result.get("read_replication")
    if not isinstance(replication, dict):
        return None
    mode = replication.get("mode")
    return mode if isinstance(mode, str) else None


def main() -> None:
    account_id = required("CLOUDFLARE_ACCOUNT_ID")
    token = required("CLOUDFLARE_API_TOKEN")
    database_id = required("NAROU_D1_DATABASE_ID")
    path = f"/accounts/{account_id}/d1/database/{database_id}"

    current = read_replication_mode(request("GET", path, token))
    if current == "auto":
        print(f"read replication は既に有効です ({database_id})")
        return

    request("PUT", path, token, {"read_replication": {"mode": "auto"}})
    print(
        f"read replication を有効化しました ({database_id}: {current or 'disabled'} -> auto)"
    )


if __name__ == "__main__":
    main()
