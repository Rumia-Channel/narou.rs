#!/usr/bin/env python3
"""SORAHOST リレーの認証が Worker 側の設定と一致しているかを確かめる。

Worker は踏み台へ `X-Proxy-Token: <SORAHOST_PROXY_KEY>` を付けて `POST /proxy` する。
リレー側はコンテナの `SORAHOST_PROXY_KEY` (パネルの `.env`) と突き合わせるため、
**この 2 つがずれると Worker からは 403 になり、元の fetch 結果へ黙って戻る**
(サイトが取得できない、という形でしか見えない)。ここで配備直後に検出する。

確かめること:
  1. `GET <endpoint>/health` が 200 を返すか (到達性。旧版は 404 がありうる)
  2. `POST <endpoint>/proxy` に**わざと違うトークン**を付けると 403 か
     (403 にならないなら認証が効いていない)
  3. 正しいトークン (`SORAHOST_PROXY_KEY`) なら 403/500 にならないか
     (403 = パネルの .env と secret が不一致 / 500 = リレーの .env に鍵が無い)

環境変数:
  SORAHOST_PROXY_ENDPOINT  リレーの公開先 (Worker の secret と同じ値)
  SORAHOST_PROXY_KEY       リレーの合言葉 (Worker の secret と同じ値)

任意:
  SORAHOST_PROXY_PROBE_URL 3 番目で取得する URL (既定 https://example.com/)
  SORAHOST_PROXY_TIMEOUT   1 リクエストの制限秒数 (既定 30)
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request

TIMEOUT = float(os.environ.get("SORAHOST_PROXY_TIMEOUT", "").strip() or 30)
PROBE_URL = os.environ.get("SORAHOST_PROXY_PROBE_URL", "").strip() or "https://example.com/"
FAILURES: list[str] = []


def endpoint_base() -> str:
    raw = os.environ.get("SORAHOST_PROXY_ENDPOINT", "").strip()
    if not raw:
        print("error: SORAHOST_PROXY_ENDPOINT が設定されていません", file=sys.stderr)
        raise SystemExit(1)
    if "://" not in raw:
        raw = f"http://{raw}"
    parts = urllib.parse.urlsplit(raw)
    # Worker 側 (`platform::relay::RelayConfig`) と同じくオリジンだけを使う。
    # パスは `/proxy` に固定するので、`/_sorahost/...` のような前置きは落とす。
    return f"{parts.scheme}://{parts.netloc}"


def request(method: str, url: str, *, token: str | None, payload: dict | None = None):
    data = json.dumps(payload).encode("utf-8") if payload is not None else None
    headers = {"Content-Type": "application/json"}
    if token is not None:
        headers["x-proxy-token"] = token
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as response:
            return response.status, response.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode("utf-8", "replace")
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        return None, str(error)


def main() -> int:
    base = endpoint_base()
    key = os.environ.get("SORAHOST_PROXY_KEY", "").strip()
    print(f"リレー: {base}")

    status, body = request("GET", f"{base}/health", token=None)
    if status == 200:
        print(f"  health: 200 {body.strip()[:80]}")
    elif status is None:
        print(f"::warning::health に到達できません ({body[:120]})")
    elif status == 404:
        print("::warning::/health が 404 (旧版のリレーか、エンドポイントのパス違い)")
    else:
        print(f"::warning::/health が {status}: {body.strip()[:120]}")

    reachable = status is not None

    status, body = request(
        "POST", f"{base}/proxy", token="wrong-token-for-check",
        payload={"url": PROBE_URL, "redirect": "follow"},
    )
    if status is None:
        print(f"::warning::/proxy に到達できません ({body[:120]})")
    elif status == 403:
        print("  不正なトークン: 403 (認証は有効)")
    elif status == 200:
        FAILURES.append("不正なトークンでも 200 が返る (認証が効いていない)")
    else:
        print(f"::warning::不正なトークンで {status}: {body.strip()[:120]}")

    if not key:
        print("::warning::SORAHOST_PROXY_KEY が未設定のため、合言葉の一致は確認できません")
        return 1 if FAILURES else 0

    status, body = request(
        "POST", f"{base}/proxy", token=key, payload={"url": PROBE_URL, "redirect": "follow"}
    )
    if status is None:
        print(f"::warning::正しいトークンでの確認に到達できません ({body[:120]})")
    elif status == 200:
        try:
            upstream = json.loads(body).get("status")
        except json.JSONDecodeError:
            upstream = "?"
        print(f"  正しいトークン: 200 (上流 {upstream}, via={json.loads(body).get('via', '?')})")
    elif status == 403:
        FAILURES.append(
            "正しいトークンでも 403 (コンテナの SORAHOST_PROXY_KEY と Worker の secret が不一致)"
        )
    elif status == 500:
        FAILURES.append("リレーが 500 を返す (コンテナの環境変数 SORAHOST_PROXY_KEY が未設定)")
    else:
        FAILURES.append(f"正しいトークンで {status}: {body.strip()[:120]}")

    for message in FAILURES:
        print(f"::error::{message}")
    if FAILURES:
        return 1
    if not reachable:
        print("::warning::リレーに到達できないため、認証の一致は確認できませんでした")
        return 0
    print("認証の一致を確認しました")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
