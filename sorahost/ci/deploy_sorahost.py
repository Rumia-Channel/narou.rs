#!/usr/bin/env python3
"""SORAHOST へ narou.rs を配備し、Cloudflare Tunnel を用意する。

やること:
  1. Cloudflare API で tunnel を用意する (無ければ作成・あれば再利用)
     - 公開ホスト名 → `http://127.0.0.1:<port>` の ingress を設定
     - `<ホスト名>` の CNAME を `<tunnel-id>.cfargotunnel.com` に向ける (proxied)
  2. ビルド済みバンドルを SFTP で SORAHOST へ転送する
     - 実行ファイル (narou_rs / narou_rs_backup / narou_rs_login) + webnovel/ + preset/
     - start.sh と tunnel トークン (トークンは値が変わったときだけ書く)
  3. パネルの API が設定されていればサーバーを再起動する (無ければ手動案内を出す)

設計上の注意:
  - tunnel トークンは **job 出力や artifact で渡さない**。このリポジトリは public で、
    Actions の API (runs/jobs) は未認証でも読めるため、job 出力に載せた秘密は公開される。
    そのため Cloudflare の資格情報も、配備先の値と同じ Environment (SORAHOST) に置く。
  - 何度実行しても同じ結果になる (tunnel 名・ホスト名・DNS は既存を再利用する)。

環境変数 (必須):
  CLOUDFLARE_API_TOKEN       Cloudflare API トークン
                             (Account: Cloudflare Tunnel Edit / Zone: DNS Edit)
  CLOUDFLARE_ACCOUNT_ID      Cloudflare のアカウント ID
  SORAHOST_TUNNEL_HOSTNAME   公開ホスト名 (例: narou.example.com)
  SORAHOST_HOST              SFTP ホスト (例: nagoya.sorahost.net)
  SORAHOST_USER              SFTP ユーザ
  SORAHOST_PASSWORD          SFTP パスワード

環境変数 (任意):
  SORAHOST_TUNNEL_NAME       tunnel 名 (既定 narou-sorahost)
  SORAHOST_SFTP_PORT         SFTP ポート (既定 2022)
  SORAHOST_REMOTE_DIR        SFTP ルートからの相対配置先 (既定 /narou)
  SORAHOST_SERVICE_PORT      コンテナ内の待受ポート (既定 8080)
  SORAHOST_PANEL_URL         パネル URL (再起動用。例 https://panel.example.com)
  SORAHOST_SERVER_ID         サーバー ID (再起動用)
  SORAHOST_CLIENT_API_KEY    パネルのクライアント API キー (再起動用)
  SORAHOST_SSH_KEY           SFTP を鍵認証にする場合の秘密鍵 (パスワードの代わり)

使い方:
  python sorahost/ci/deploy_sorahost.py --bundle narou_rs-0.4.4-linux-x86_64-gpl.tar.gz \
      --start-sh sorahost/start.sh
"""

from __future__ import annotations

import argparse
import io
import json
import os
import stat
import sys
import tarfile
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any, NoReturn

CF_API = "https://api.cloudflare.com/client/v4"
BINARIES = ("narou_rs", "narou_rs_backup", "narou_rs_login")
TOKEN_FILE = ".cloudflared-token"


def fail(message: str) -> NoReturn:
    print(f"error: {message}", file=sys.stderr)
    raise SystemExit(1)


def mask(value: str) -> None:
    """GitHub Actions のログで伏せる (ローカル実行では単なる行として出る)。"""
    if value:
        print(f"::add-mask::{value}")


def required(name: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        fail(f"{name} が設定されていません")
    return value


def optional(name: str, default: str) -> str:
    return os.environ.get(name, "").strip() or default


def cf_request(
    method: str,
    path: str,
    token: str,
    body: dict[str, Any] | None = None,
) -> dict[str, Any]:
    data = json.dumps(body).encode("utf-8") if body is not None else None
    request = urllib.request.Request(
        f"{CF_API}{path}",
        data=data,
        method=method,
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
        },
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
        errors = payload.get("errors") or []
        fail(f"Cloudflare {method} {path} が失敗しました: {errors}")
    return payload


def ensure_tunnel(token: str, account: str, name: str) -> dict[str, Any]:
    query = urllib.parse.urlencode({"name": name, "is_deleted": "false"})
    existing = cf_request("GET", f"/accounts/{account}/cfd_tunnel?{query}", token)["result"]
    for tunnel in existing:
        if tunnel.get("name") == name and not tunnel.get("deleted_at"):
            print(f"tunnel を再利用します: {name} ({tunnel['id']})")
            return tunnel
    created = cf_request(
        "POST",
        f"/accounts/{account}/cfd_tunnel",
        token,
        {"name": name, "config_src": "cloudflare"},
    )["result"]
    print(f"tunnel を作成しました: {name} ({created['id']})")
    return created


def tunnel_token(token: str, account: str, tunnel_id: str) -> str:
    result = cf_request("GET", f"/accounts/{account}/cfd_tunnel/{tunnel_id}/token", token)
    value = result.get("result")
    if not isinstance(value, str) or not value:
        fail("tunnel トークンを取得できませんでした")
    mask(value)
    return value


def configure_ingress(token: str, account: str, tunnel_id: str, hostname: str, port: str) -> None:
    config = {
        "config": {
            "ingress": [
                {"hostname": hostname, "service": f"http://127.0.0.1:{port}"},
                {"service": "http_status:404"},
            ]
        }
    }
    cf_request("PUT", f"/accounts/{account}/cfd_tunnel/{tunnel_id}/configurations", token, config)
    print(f"ingress を設定しました: {hostname} → http://127.0.0.1:{port}")


def zone_for(token: str, hostname: str) -> tuple[str, str]:
    """ホスト名から zone を探す (example.com から順に短くしていく)。"""
    labels = hostname.split(".")
    for index in range(len(labels) - 1):
        candidate = ".".join(labels[index:])
        query = urllib.parse.urlencode({"name": candidate, "status": "active"})
        found = cf_request("GET", f"/zones?{query}", token)["result"]
        for zone in found:
            if zone.get("name") == candidate:
                return zone["id"], candidate
    fail(f"{hostname} の zone が Cloudflare アカウントに見つかりません")


def ensure_dns(token: str, hostname: str, tunnel_id: str) -> None:
    zone_id, zone_name = zone_for(token, hostname)
    target = f"{tunnel_id}.cfargotunnel.com"
    query = urllib.parse.urlencode({"name": hostname, "type": "CNAME"})
    records = cf_request("GET", f"/zones/{zone_id}/dns_records?{query}", token)["result"]
    body = {"type": "CNAME", "name": hostname, "content": target, "proxied": True}
    for record in records:
        if record.get("content") == target and record.get("proxied"):
            print(f"DNS は設定済みです: {hostname} → {target} ({zone_name})")
            return
        cf_request("PUT", f"/zones/{zone_id}/dns_records/{record['id']}", token, body)
        print(f"DNS を更新しました: {hostname} → {target} ({zone_name})")
        return
    cf_request("POST", f"/zones/{zone_id}/dns_records", token, body)
    print(f"DNS を作成しました: {hostname} → {target} ({zone_name})")


def extract_bundle(bundle: Path) -> Path:
    if not bundle.is_file():
        fail(f"バンドルが見つかりません: {bundle}")
    work = Path(tempfile.mkdtemp(prefix="narou-sorahost-"))
    with tarfile.open(bundle, "r:gz") as archive:
        archive.extractall(work, filter="data")
    roots = [entry for entry in work.iterdir() if entry.is_dir()]
    if not roots:
        fail(f"バンドルの中にディレクトリがありません: {bundle}")
    root = roots[0]
    for name in BINARIES[0:1]:
        if not (root / name).is_file():
            fail(f"バンドルに {name} が含まれていません: {root}")
    return root


def connect_sftp(host: str, port: int, user: str, password: str, key: str) -> Any:
    try:
        import paramiko
    except ImportError:
        fail("paramiko が必要です (pip install paramiko)")

    client = paramiko.SSHClient()
    client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    try:
        if key:
            key_file = io.StringIO(key)
            try:
                pkey = paramiko.Ed25519Key.from_private_key(key_file)
            except paramiko.SSHException:
                key_file.seek(0)
                try:
                    pkey = paramiko.RSAKey.from_private_key(key_file)
                except paramiko.SSHException:
                    key_file.seek(0)
                    pkey = paramiko.ECDSAKey.from_private_key(key_file)
            client.connect(host, port=port, username=user, pkey=pkey, timeout=30)
        else:
            client.connect(host, port=port, username=user, password=password, timeout=30)
    except Exception as error:  # noqa: BLE001 - 接続失敗はそのまま伝える
        fail(f"SFTP に接続できません ({host}:{port}): {error}")
    return client


def remote_makedirs(sftp: Any, path: str) -> None:
    parts = [part for part in path.split("/") if part]
    current = "/" if path.startswith("/") else ""
    for part in parts:
        current = f"{current}{part}" if current in ("", "/") else f"{current}/{part}"
        try:
            sftp.stat(current)
        except OSError:
            sftp.mkdir(current)


def read_remote(sftp: Any, path: str) -> str | None:
    try:
        with sftp.open(path, "r") as handle:
            return handle.read().decode("utf-8")
    except OSError:
        return None


def upload(sftp: Any, local: Path, remote: str, *, executable: bool = False) -> None:
    sftp.put(str(local), remote)
    if executable:
        sftp.chmod(remote, stat.S_IRWXU | stat.S_IRGRP | stat.S_IXGRP)
    print(f"  転送: {remote}")


def upload_tree(sftp: Any, root: Path, remote_dir: str, binary: Path) -> None:
    """バンドルの内容を転送する (実行ファイルと webnovel/ だけ)。"""
    upload(sftp, binary, f"{remote_dir}/{binary.name}", executable=True)
    for name in BINARIES[1:]:
        path = root / name
        if path.is_file():
            upload(sftp, path, f"{remote_dir}/{name}", executable=True)
    for sub in ("webnovel", "preset"):
        source = root / sub
        if not source.is_dir():
            continue
        remote_makedirs(sftp, f"{remote_dir}/{sub}")
        for item in sorted(source.iterdir()):
            if item.is_file():
                upload(sftp, item, f"{remote_dir}/{sub}/{item.name}")


def write_tunnel_token(sftp: Any, remote_dir: str, value: str) -> bool:
    path = f"{remote_dir}/{TOKEN_FILE}"
    current = read_remote(sftp, path)
    if current is not None and current.strip() == value:
        print(f"  トークンは変更なし: {path}")
        return False
    with sftp.open(path, "w") as handle:
        handle.write(value)
    sftp.chmod(path, stat.S_IRUSR | stat.S_IWUSR)
    print(f"  トークンを書き込みました: {path}")
    return True


def restart_server(panel: str, server_id: str, key: str) -> bool:
    url = f"{panel.rstrip('/')}/api/client/servers/{server_id}/power"
    request = urllib.request.Request(
        url,
        data=json.dumps({"signal": "restart"}).encode("utf-8"),
        method="POST",
        headers={
            "Authorization": f"Bearer {key}",
            "Content-Type": "application/json",
            "Accept": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            response.read()
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", "replace")[:300]
        print(f"warning: 再起動 API が {error.code} を返しました: {detail}", file=sys.stderr)
        return False
    except urllib.error.URLError as error:
        print(f"warning: 再起動 API に接続できません: {error}", file=sys.stderr)
        return False
    print("サーバーの再起動を要求しました")
    return True


def main() -> int:
    parser = argparse.ArgumentParser(description="Deploy narou.rs to SORAHOST over SFTP")
    parser.add_argument("--bundle", required=True, help="CI が作った tar.gz")
    parser.add_argument("--start-sh", default="sorahost/start.sh", help="起動スクリプト")
    parser.add_argument("--dry-run", action="store_true", help="転送せず内容だけ確認する")
    args = parser.parse_args()

    token = required("CLOUDFLARE_API_TOKEN")
    account = required("CLOUDFLARE_ACCOUNT_ID")
    hostname = required("SORAHOST_TUNNEL_HOSTNAME")
    host = required("SORAHOST_HOST")
    user = required("SORAHOST_USER")
    password = os.environ.get("SORAHOST_PASSWORD", "").strip()
    key = os.environ.get("SORAHOST_SSH_KEY", "").strip()
    if not password and not key:
        fail("SORAHOST_PASSWORD か SORAHOST_SSH_KEY のどちらかが必要です")

    tunnel_name = optional("SORAHOST_TUNNEL_NAME", "narou-sorahost")
    sftp_port = int(optional("SORAHOST_SFTP_PORT", "2022"))
    remote_dir = "/" + optional("SORAHOST_REMOTE_DIR", "/narou").strip("/")
    service_port = optional("SORAHOST_SERVICE_PORT", "8080")

    root = extract_bundle(Path(args.bundle))
    binary = root / BINARIES[0]
    start_sh = Path(args.start_sh)
    if not start_sh.is_file():
        fail(f"起動スクリプトが見つかりません: {start_sh}")

    print("== Cloudflare Tunnel")
    tunnel = ensure_tunnel(token, account, tunnel_name)
    token_value = tunnel_token(token, account, tunnel["id"])
    configure_ingress(token, account, tunnel["id"], hostname, service_port)
    ensure_dns(token, hostname, tunnel["id"])

    print("== SORAHOST への転送")
    print(f"  接続: {host}:{sftp_port} (user={user})")
    if args.dry_run:
        print(f"  (dry-run のため転送しません。転送先 {remote_dir})")
        return 0

    client = connect_sftp(host, sftp_port, user, password, key)
    try:
        sftp = client.open_sftp()
        remote_makedirs(sftp, remote_dir)
        upload_tree(sftp, root, remote_dir, binary)
        upload(sftp, start_sh, f"{remote_dir}/start.sh", executable=True)
        changed = write_tunnel_token(sftp, remote_dir, token_value)
        sftp.close()
    finally:
        client.close()

    print("== 再起動")
    panel = os.environ.get("SORAHOST_PANEL_URL", "").strip()
    server_id = os.environ.get("SORAHOST_SERVER_ID", "").strip()
    client_key = os.environ.get("SORAHOST_CLIENT_API_KEY", "").strip()
    if panel and server_id and client_key:
        restart_server(panel, server_id, client_key)
    else:
        print(
            "  パネルの API が未設定なので再起動は手動で行ってください "
            "(SORAHOST_PANEL_URL / SORAHOST_SERVER_ID / SORAHOST_CLIENT_API_KEY)"
        )

    print()
    print("完了しました。確認:")
    print(f"  https://{hostname}/api/novels/count (basic 認証)")
    if changed:
        print("  ※ tunnel トークンを更新したので、コンテナの再起動が必要です")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
