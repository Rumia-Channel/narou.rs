#!/usr/bin/env python3
"""外部リレーの認証が Worker 側の設定と一致しているかを手動で確かめる。

Worker は踏み台へ `X-Proxy-Token: <NAROU_RELAY_KEY>` を付けて `POST /proxy` する。
リレー側は同じ `NAROU_RELAY_KEY` と突き合わせるため、
**この 2 つがずれると Worker からは 403 になり、元の fetch 結果へ黙って戻る**
(サイトが取得できない、という形でしか見えない)。ここで配備直後に検出する。

確かめること:
  1. `GET <endpoint>/health` が 200 を返すか (到達性)
  2. `POST <endpoint>/proxy` に**わざと違うトークン**を付けると 403 か
     (403 にならないなら認証が効いていない)
  3. 正しいトークン (`NAROU_RELAY_KEY`) なら 403/500 にならず 200 か
     (403 = リレーと Worker の secret が不一致 / 500 = リレーに鍵が無い)

環境変数:
  NAROU_RELAY_ENDPOINT    リレーの公開先 (Worker の secret と同じ値)
                         スキーム省略時は https:// を使う。平文 HTTP は loopback のみ許可。
  NAROU_RELAY_KEY         リレーの合言葉 (Worker の secret と同じ値。必須)

任意:
  NAROU_RELAY_PROBE_URL   3 番目で取得する URL。未指定なら
                          NAROU_RELAY_ALLOWED_HOSTS の先頭ホストを使う
                          (例: NAROU_RELAY_ALLOWED_HOSTS=syosetu.org → https://syosetu.org/)
  NAROU_RELAY_ALLOWED_HOSTS  許可ホスト (未指定なら https://example.com/)
  NAROU_RELAY_TIMEOUT     1 リクエストの制限秒数 (既定 30)

注意:
  - 3xx は追跡しない。`X-Proxy-Token` を別オリジンへ再送しないため。
  - 到達不能・想定外の応答は fail closed (exit 1) として扱う。
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request

TIMEOUT = float(os.environ.get("NAROU_RELAY_TIMEOUT", "").strip() or 30)
LOOPBACK_HOSTS = {"127.0.0.1", "::1", "localhost"}
FAILURES: list[str] = []


def _default_probe_url() -> str:
    """許可リストの先頭ホストを確認先にする (許可外ホストだと 400 になるため)。"""
    allowed = os.environ.get("NAROU_RELAY_ALLOWED_HOSTS", "").strip()
    for host in allowed.split(","):
        host = host.strip()
        if host:
            return f"https://{host}/"
    return "https://example.com/"


PROBE_URL = os.environ.get("NAROU_RELAY_PROBE_URL", "").strip() or _default_probe_url()


class _RejectRedirects(urllib.request.HTTPRedirectHandler):
    """リダイレクトを追跡しない。

    urllib の既定ハンドラは 301/302/303 で content-length/content-type 以外の
    ヘッダを引き継ぐため、リレーの鍵がリダイレクト先の別オリジンへ送られる。
    検証スクリプトでは追跡せず、3xx を失敗として扱う。
    """

    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: D401, ANN001
        return None


_OPENER = urllib.request.build_opener(_RejectRedirects)


def endpoint_base() -> str:
    raw = os.environ.get("NAROU_RELAY_ENDPOINT", "").strip()
    if not raw:
        print("error: NAROU_RELAY_ENDPOINT が設定されていません", file=sys.stderr)
        raise SystemExit(1)
    if "://" not in raw:
        # スキーム省略時は安全側 (https) に倒す。平文で鍵を送らない。
        raw = f"https://{raw}"
    parts = urllib.parse.urlsplit(raw)
    if parts.scheme not in ("http", "https") or not parts.netloc:
        print("error: NAROU_RELAY_ENDPOINT は http(s) の URL で指定してください", file=sys.stderr)
        raise SystemExit(1)
    host = (parts.hostname or "").lower()
    if parts.scheme == "http" and host not in LOOPBACK_HOSTS:
        print(
            "error: 公開エンドポイントには https:// を指定してください "
            "(平文 HTTP は loopback のローカル確認だけで許可されます)",
            file=sys.stderr,
        )
        raise SystemExit(1)
    # Worker 側 (`platform::relay::RelayConfig`) と同じくオリジンだけを使う。
    # パスは `/proxy` に固定するので、前置きのパスは落とす。
    return f"{parts.scheme}://{parts.netloc}"


def request(method: str, url: str, *, token: str | None, payload: dict | None = None):
    data = json.dumps(payload).encode("utf-8") if payload is not None else None
    headers = {"Content-Type": "application/json"}
    if token is not None:
        headers["x-proxy-token"] = token
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with _OPENER.open(req, timeout=TIMEOUT) as response:
            return response.status, response.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode("utf-8", "replace")
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        return None, str(error)


def main() -> int:
    base = endpoint_base()
    key = os.environ.get("NAROU_RELAY_KEY", "").strip()
    print(f"リレー: {base}")
    print(f"取得確認 URL: {PROBE_URL}")

    status, body = request("GET", f"{base}/health", token=None)
    if status == 200:
        print(f"  health: 200 {body.strip()[:80]}")
    elif status is None:
        FAILURES.append(f"health に到達できません ({body[:120]})")
    else:
        FAILURES.append(
            f"/health が {status} (リレー未起動 / URL 違い / 旧版の可能性): {body.strip()[:120]}"
        )

    status, body = request(
        "POST", f"{base}/proxy", token="wrong-token-for-check", payload={"url": PROBE_URL}
    )
    if status is None:
        FAILURES.append(f"/proxy に到達できません ({body[:120]})")
    elif status == 403:
        print("  不正なトークン: 403 (認証は有効)")
    elif status == 200:
        FAILURES.append("不正なトークンでも 200 が返る (認証が効いていない)")
    else:
        FAILURES.append(f"不正なトークンで {status} (リレーの状態を確認): {body.strip()[:120]}")

    if not key:
        FAILURES.append("NAROU_RELAY_KEY が未設定のため、合言葉の一致を確認できない")
    else:
        status, body = request("POST", f"{base}/proxy", token=key, payload={"url": PROBE_URL})
        if status is None:
            FAILURES.append(f"正しいトークンでの確認に到達できません ({body[:120]})")
        elif status == 200:
            try:
                envelope = json.loads(body)
                upstream = envelope.get("status", "?")
                via = envelope.get("via", "?")
            except json.JSONDecodeError:
                upstream = via = "?"
            print(f"  正しいトークン: 200 (上流 {upstream}, via={via})")
        elif status == 403:
            FAILURES.append(
                "正しいトークンでも 403 (リレーの NAROU_RELAY_KEY と Worker の secret が不一致)"
            )
        elif status == 500:
            FAILURES.append("リレーが 500 を返す (リレーの環境変数 NAROU_RELAY_KEY が未設定)")
        else:
            FAILURES.append(
                f"正しいトークンで {status}: {body.strip()[:120]} "
                "(取得先が NAROU_RELAY_ALLOWED_HOSTS に含まれるか、"
                "NAROU_RELAY_PROBE_URL を設定してください)"
            )

    for message in FAILURES:
        print(f"::error::{message}")
    if FAILURES:
        return 1
    print("認証の一致を確認しました")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
