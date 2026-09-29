#!/usr/bin/env python3
"""SORAHOST の前段に置く Cloudflare のコネクタ (cloudflared) と Access を用意する。

やること (何度実行しても同じ結果になる):
  1. コネクタを名前で探し、無ければ作る (remotely managed)
  2. ingress を `<公開ホスト名> -> http://127.0.0.1:<PORT>` に設定する
  3. `<公開ホスト名>` の CNAME を `<id>.cfargotunnel.com` に向ける (proxied)
  4. `SORAHOST_ACCESS_EMAIL` があれば、そのホストに Access のアプリと
     allow ポリシー (メール一致) を作る

トークンはここでは出力しない (公開リポジトリのログに出さないため)。接続用の
トークンは Cloudflare のダッシュボード (Networks → Tunnels → 該当のコネクタ →
Add a replica) で確認し、PteWorker の .env に NAROU_CONNECTOR_TOKEN として置く。

環境変数:
  CLOUDFLARE_API_TOKEN      Account: 次のいずれか (どれでも可)
                              Cloudflare One Connectors (Write/Edit)
                              Cloudflare One Connector: cloudflared (Write/Edit)
                              Cloudflare Tunnel (Write/Edit)
                            Zone: DNS (Write/Edit) と Zone: Zone (Read)
                            (+ Access を使うなら Account: Access: Apps and Policies (Write/Edit))
                            ※ Argo Tunnel (Legacy) は旧版なので使わない
                            ※ ダッシュボードは Read/Edit、API リファレンスは Read/Write
  CLOUDFLARE_ACCOUNT_ID
  SORAHOST_PUBLIC_HOSTNAME  公開ホスト名 (例 narou.example.com)
  SORAHOST_SERVICE_PORT     コンテナ内の待受ポート (既定 18080 = PteWorker の PORT)

任意:
  SORAHOST_CONNECTOR_NAME   コネクタ名 (既定 narou-sorahost)
  SORAHOST_ACCESS_EMAIL     Access で許可するメール (カンマ区切りで複数可)
  SORAHOST_ACCESS_DOMAIN    Access で許可するメールドメイン (カンマ区切り)
  SORAHOST_ACCESS_SESSION   Access のセッション有効期限 (既定 24h)
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, NoReturn

CF_API = "https://api.cloudflare.com/client/v4"


def fail(message: str) -> NoReturn:
    print(f"error: {message}", file=sys.stderr)
    raise SystemExit(1)


def required(name: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        fail(f"{name} が設定されていません")
    return value


def optional(name: str, default: str) -> str:
    return os.environ.get(name, "").strip() or default


def cf(method: str, path: str, token: str, body: dict[str, Any] | None = None) -> dict[str, Any]:
    """Cloudflare API を 1 回叩く。失敗はそのまま伝える。"""
    request = urllib.request.Request(
        f"{CF_API}{path}",
        data=json.dumps(body).encode("utf-8") if body is not None else None,
        method=method,
        headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            payload = json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", "replace")[:400]
        fail(f"Cloudflare {method} {path} が {error.code} を返しました: {detail}")
    except urllib.error.URLError as error:
        fail(f"Cloudflare {method} {path} に接続できません: {error}")
    if not payload.get("success", False):
        fail(f"Cloudflare {method} {path} が失敗しました: {payload.get('errors')}")
    return payload


def ensure_connector(token: str, account: str, name: str) -> str:
    query = urllib.parse.urlencode({"name": name, "is_deleted": "false"})
    for item in cf("GET", f"/accounts/{account}/cfd_tunnel?{query}", token)["result"]:
        if item.get("name") == name and not item.get("deleted_at"):
            print(f"コネクタ: 再利用 {name} ({item['id']})")
            return item["id"]
    created = cf(
        "POST",
        f"/accounts/{account}/cfd_tunnel",
        token,
        {"name": name, "config_src": "cloudflare"},
    )["result"]
    print(f"コネクタ: 作成 {name} ({created['id']})")
    return created["id"]


def configure_ingress(token: str, account: str, connector_id: str, hostname: str, port: str) -> None:
    cf(
        "PUT",
        f"/accounts/{account}/cfd_tunnel/{connector_id}/configurations",
        token,
        {
            "config": {
                "ingress": [
                    {"hostname": hostname, "service": f"http://127.0.0.1:{port}"},
                    {"service": "http_status:404"},
                ]
            }
        },
    )
    print(f"向き先: {hostname} -> http://127.0.0.1:{port}")


def zone_for(token: str, hostname: str) -> str:
    labels = hostname.split(".")
    for index in range(len(labels) - 1):
        candidate = ".".join(labels[index:])
        query = urllib.parse.urlencode({"name": candidate, "status": "active"})
        for zone in cf("GET", f"/zones?{query}", token)["result"]:
            if zone.get("name") == candidate:
                return zone["id"]
    fail(
        f"{hostname} の zone が見つかりません"
        " (トークンに Zone:Zone:Read と対象ゾーンの Zone:DNS:Write が入っているか確認)"
    )


def ensure_dns(token: str, hostname: str, connector_id: str) -> None:
    zone_id = zone_for(token, hostname)
    target = f"{connector_id}.cfargotunnel.com"
    query = urllib.parse.urlencode({"name": hostname, "type": "CNAME"})
    body = {"type": "CNAME", "name": hostname, "content": target, "proxied": True}
    for record in cf("GET", f"/zones/{zone_id}/dns_records?{query}", token)["result"]:
        if record.get("content") == target and record.get("proxied"):
            print(f"DNS: 設定済み {hostname} -> {target}")
            return
        cf("PUT", f"/zones/{zone_id}/dns_records/{record['id']}", token, body)
        print(f"DNS: 更新 {hostname} -> {target}")
        return
    cf("POST", f"/zones/{zone_id}/dns_records", token, body)
    print(f"DNS: 作成 {hostname} -> {target}")


def ensure_access(
    token: str,
    account: str,
    hostname: str,
    emails: list[str],
    domains: list[str],
    session: str,
) -> None:
    """Access のアプリと allow ポリシーを用意する。

    Zero Trust が未有効のアカウントでは API がエラーを返すので、その場合は
    ダッシュボードで有効化してもらう (ここでは失敗として伝える)。
    """
    query = urllib.parse.urlencode({"domain": hostname})
    apps = cf("GET", f"/accounts/{account}/access/apps?{query}", token)["result"]
    app_id = next((app["id"] for app in apps if app.get("domain") == hostname), None)
    body = {"name": f"narou.rs ({hostname})", "domain": hostname, "type": "self_hosted",
            "session_duration": session}
    if app_id is None:
        app_id = cf("POST", f"/accounts/{account}/access/apps", token, body)["result"]["id"]
        print(f"access: アプリを作成 ({hostname})")
    else:
        cf("PUT", f"/accounts/{account}/access/apps/{app_id}", token, body)
        print(f"access: アプリを更新 ({hostname})")

    include: list[dict[str, Any]] = [{"email": {"email": email}} for email in emails]
    include += [{"email_domain": {"domain": domain}} for domain in domains]
    policies = cf("GET", f"/accounts/{account}/access/apps/{app_id}/policies", token)["result"]
    policy_body = {"name": "allow-configured", "decision": "allow", "include": include,
                   "session_duration": session}
    existing = next((p["id"] for p in policies if p.get("name") == policy_body["name"]), None)
    if existing is None:
        cf("POST", f"/accounts/{account}/access/apps/{app_id}/policies", token, policy_body)
        print(f"access: ポリシーを作成 (allow {len(include)} 件)")
    else:
        cf(
            "PUT",
            f"/accounts/{account}/access/apps/{app_id}/policies/{existing}",
            token,
            policy_body,
        )
        print(f"access: ポリシーを更新 (allow {len(include)} 件)")


def main() -> int:
    token = required("CLOUDFLARE_API_TOKEN")
    account = required("CLOUDFLARE_ACCOUNT_ID")
    hostname = required("SORAHOST_PUBLIC_HOSTNAME")
    port = optional("SORAHOST_SERVICE_PORT", "18080")
    name = optional("SORAHOST_CONNECTOR_NAME", "narou-sorahost")
    emails = [v.strip() for v in optional("SORAHOST_ACCESS_EMAIL", "").split(",") if v.strip()]
    domains = [v.strip() for v in optional("SORAHOST_ACCESS_DOMAIN", "").split(",") if v.strip()]
    session = optional("SORAHOST_ACCESS_SESSION", "24h")

    print(f"公開ホスト: {hostname} (-> 127.0.0.1:{port})")
    connector_id = ensure_connector(token, account, name)
    configure_ingress(token, account, connector_id, hostname, port)
    ensure_dns(token, hostname, connector_id)

    if emails or domains:
        ensure_access(token, account, hostname, emails, domains, session)
    else:
        print("access: 未設定 (SORAHOST_ACCESS_EMAIL / SORAHOST_ACCESS_DOMAIN を置くと作る)")

    print()
    print("次: Cloudflare の Networks → Tunnels → 該当のコネクタ → Add a replica で")
    print("    トークンを確認し、PteWorker の .env に次を置いて再起動する:")
    print("      NAROU_CONNECTOR_TOKEN=<トークン>")
    print(f"      NAROU_PUBLIC_HOST={hostname}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
