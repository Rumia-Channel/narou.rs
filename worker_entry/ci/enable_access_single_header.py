"""Access アプリケーションを「単一ヘッダー」方式にそろえる（冪等）。

AI 系のクライアント (MCP クライアントや API 連携サービス) は
`Authorization` ヘッダしか送れないことがあるため、Access アプリケーションの
`read_service_tokens_from_header` を `Authorization` にする。設定後は

    Authorization: {"cf-access-client-id": "<ID>", "cf-access-client-secret": "<SECRET>"}

の 1 枚だけで Service Token 認証ができる。

Access アプリケーションの更新は「PUT で全フィールドを返す」仕様なので、先に
GET した内容へ 1 フィールドを足して PUT する（他の設定を失わないため）。

develop / production のどちらのデプロイでも同じ経路を通す
（`deploy_worker.py` から呼ばれる）。権限が無い・対象が無い・失敗は
デプロイを止めない（警告を残して配信を続ける。冪等なので次回も試される）。

必要な環境変数:

- `CLOUDFLARE_ACCOUNT_ID` / `CLOUDFLARE_API_TOKEN` … トークンには
  `Access: Apps and Policies (Write)` が要る
- `NAROU_ACCESS_DOMAIN` … 対象ドメイン (例 `narou.rumia-ch.uk`)
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request
from typing import Any, NoReturn

API = "https://api.cloudflare.com/client/v4"
HEADER_NAME = "Authorization"


def fail(message: str) -> NoReturn:
    raise SystemExit(message)


def required(name: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        fail(f"Missing required value: {name}")
    return value


def request(method: str, path: str, token: str, payload: dict[str, Any] | None = None) -> Any:
    body = json.dumps(payload).encode("utf-8") if payload is not None else None
    http_request = urllib.request.Request(
        API + path,
        data=body,
        method=method,
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(http_request) as response:
            payload = json.load(response)
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", "replace")
        fail(
            f"{method} {path} failed ({error.code}): {detail}\n"
            "ヒント: トークンに Access: Apps and Policies (Write) 権限があるか確認してください。"
        )
    if isinstance(payload, dict) and not payload.get("success", True):
        fail(f"{method} {path} failed: {json.dumps(payload.get('errors'), ensure_ascii=False)}")
    return payload.get("result") if isinstance(payload, dict) else payload


def find_app(account_id: str, token: str, domain: str) -> dict[str, Any] | None:
    apps = request("GET", f"/accounts/{account_id}/access/apps?per_page=100", token)
    for app in apps or []:
        if app.get("domain") == domain:
            return app
    return None


def main() -> None:
    account_id = required("CLOUDFLARE_ACCOUNT_ID")
    token = required("CLOUDFLARE_API_TOKEN")
    domain = required("NAROU_ACCESS_DOMAIN")

    app = find_app(account_id, token, domain)
    if app is None:
        print(f"::notice::Access アプリケーションが見つかりません ({domain})")
        return
    app_id = app.get("id", "")
    current = app.get("read_service_tokens_from_header")
    if current == HEADER_NAME:
        print(f"単一ヘッダー方式は設定済みです ({domain})")
        return

    # PUT は全フィールドの置き換えなので、GET した内容に 1 つ足す。
    updated = dict(app)
    updated["read_service_tokens_from_header"] = HEADER_NAME
    request("PUT", f"/accounts/{account_id}/access/apps/{app_id}", token, updated)
    print(
        f"Access を単一ヘッダー方式に切り替えました ({domain}: "
        f"{current or '未設定'} -> {HEADER_NAME})"
    )


if __name__ == "__main__":
    main()
