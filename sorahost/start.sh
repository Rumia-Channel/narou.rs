#!/usr/bin/env bash
# SORAHOST (Pterodactyl) のコンテナで narou_rs を常駐させる起動スクリプト。
#
# 設計:
#   - ライブラリ (.narou / 小説データ / webnovel) は再デプロイで消えない
#     ディレクトリに置く ($NAROU_RS_ROOT、既定 /home/container/narou)。
#   - アプリは 127.0.0.1:$NAROU_RS_PORT にだけバインドする。外部への公開は
#     cloudflared のトンネル (外向き接続のみ) が担うので、受信ポートは不要。
#     そのため SORAHOST 側の割当ポート番号には依存しない。
#   - 挿絵を S3 に置く構成は環境変数 (S3_* / NAROU_RS_ASSET_BACKEND) だけで
#     完結する (narou setting でも指定できる)。
#
# Pterodactyl の起動コマンド:
#   bash /home/container/narou/start.sh
#
# 注意: コンソールは PTY なので、対話プロンプトを持つコマンドには
# `< /dev/null` を付けて stdin を端末でなくす (付けないと初回起動が止まる)。

set -euo pipefail

ROOT="${NAROU_RS_ROOT:-/home/container/narou}"
LIB="${NAROU_RS_LIBRARY:-$ROOT/library}"
NAROU_PORT="${NAROU_RS_PORT:-8080}"
BIN="$ROOT/bin"
CF_BIN="$BIN/cloudflared"
CF_TAG="2026.9.3"
CF_SHA256="77e26d8d900e0b8469f416239d14b5f296525fdf79fee6f511ef55609e3fbac2"
CF_URL="https://github.com/cloudflare/cloudflared/releases/download/${CF_TAG}/cloudflared-linux-amd64"
TOKEN_FILE="${TUNNEL_TOKEN_FILE:-$ROOT/.cloudflared-token}"

NAROU="$ROOT/narou_rs"
[ -x "$NAROU" ] || { echo "[narou] $NAROU がありません (ビルド成果物を配置して下さい)" >&2; exit 1; }

mkdir -p "$BIN" "$LIB"

fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1"
  else
    echo "[narou] curl も wget も無いため $1 を取得できません" >&2
    return 1
  fi
}

verify_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    echo "$2  $1" | sha256sum -c -
  elif command -v shasum >/dev/null 2>&1; then
    echo "$2  $1" | shasum -a 256 -c -
  else
    echo "[narou] sha256sum が無いため検証できません" >&2
    return 1
  fi
}

# --- cloudflared (トンネルのコネクタ) --------------------------------------
# 取得できない環境では、手元で落としたバイナリを $CF_BIN に置けばよい。
if [ ! -x "$CF_BIN" ]; then
  echo "[narou] cloudflared ${CF_TAG} を取得します"
  fetch "$CF_URL" "$CF_BIN.tmp"
  verify_sha256 "$CF_BIN.tmp" "$CF_SHA256" || { rm -f "$CF_BIN.tmp"; exit 1; }
  mv "$CF_BIN.tmp" "$CF_BIN"
  chmod +x "$CF_BIN"
fi

# --- 初回セットアップ -----------------------------------------------------
cd "$LIB"
if [ ! -d "$LIB/.narou" ]; then
  echo "[narou] ライブラリを初期化します: $LIB"
  "$NAROU" init < /dev/null
  # SQLite 管理 (Lite) にする。YAML に戻すときは `narou db export-yaml --in-place`。
  printf 'sqlite\n' > "$LIB/.narou/storage-backend"
  # 容量節約の既定 (初回だけ入れるので、後から変えても上書きされない):
  #   nosave_diff  更新のたびに作られる差分スナップショットを保存しない
  #   nosave_raw   取得した raw HTML を保存しない (挿絵のローカライズは取得時に
  #                メモリ上で行うので、新規話の挿絵は従来どおり保存される)
  #   no-epub      EPUB を保存しない (Web UI の「EPUB をダウンロード」は都度生成)
  #   keep-txt     txt を残さない (変換結果は SQLite にあり EPUB はそこから生成)
  #   mirror-files 小説データ/ へ実ファイルを書かない (DB だけが保存先。変換は
  #                必要な間だけ取り出して消す)
  # concurrency: DL/update と convert/send を別レーンで並行に流す (小説単位の
  # 排他は .narou/lock.yaml が効くので、同じ小説が両方で走ることはない)。
  # 実測のピークは 2 ジョブ同時で約 120MB (debug ビルド) + 常駐 31MB。
  "$NAROU" setting \
    economy=nosave_diff,nosave_raw \
    convert.no-epub=true \
    convert.keep-txt=false \
    sqlite.mirror-files=false \
    concurrency=true < /dev/null
fi

# 公開はトンネル経由なので、ループバックだけを向き、Host / Origin は前段が
# 渡す公開ホスト名と一致させる (server-add-accepted-hosts は不要)。
SETTINGS=(
  "server-bind=127.0.0.1"
  "server-port=$NAROU_PORT"
  "server-reverse-proxy.enable=true"
  "server-basic-auth.enable=true"
  # 容量節約のため、話ごとの変換キャッシュは作らない (再変換が少し遅くなるだけ)。
  "convert.section-cache=false"
)
if [ -n "${NAROU_WEB_USER:-}" ]; then
  SETTINGS+=("server-basic-auth.user=$NAROU_WEB_USER")
fi
if [ -n "${NAROU_WEB_PASSWORD:-}" ]; then
  SETTINGS+=("server-basic-auth.password=$NAROU_WEB_PASSWORD")
fi
"$NAROU" setting "${SETTINGS[@]}" < /dev/null

if [ -z "${NAROU_WEB_PASSWORD:-}" ] \
  && [ -z "$("$NAROU" setting server-basic-auth.password < /dev/null)" ]; then
  echo "[narou] 警告: Web UI の basic 認証が未設定です (NAROU_WEB_PASSWORD を設定して下さい)" >&2
fi

# --- 起動したプロセスをまとめて片付ける -----------------------------------
PIDS=()
cleanup() {
  for pid in "${PIDS[@]}"; do
    kill "$pid" 2>/dev/null || true
  done
}
trap cleanup EXIT

# --- cloudflared (トンネル) と本体を同時に動かす ---------------------------
if [ -f "$TOKEN_FILE" ]; then
  "$CF_BIN" tunnel --no-autoupdate --loglevel info \
    run --token-file "$TOKEN_FILE" </dev/null &
  PIDS+=("$!")
else
  echo "[narou] 警告: $TOKEN_FILE が無いため cloudflared を起動しません" >&2
  echo "[narou] (公開するには Cloudflare Tunnel のトークンを置いて下さい)" >&2
fi

# --- 取得リレー (Worker の踏み台) ------------------------------------------
# Worker から取れないサイト用のリレー (scripts/sorahost-proxy/server.mjs) を
# **同じコンテナで** 動かすときだけ NAROU_RELAY=1 にする。リレーは 1 プロジェクトに
# 1 つしか起動コマンドを置けないため、その場合は PteWorker 側の start を
# このスクリプトにして、リレーはここから起動する (sorahost/README.md 参照)。
RELAY_JS="$ROOT/../server.mjs"
if [ "${NAROU_RELAY:-0}" = "1" ]; then
  if [ ! -f "$RELAY_JS" ]; then
    echo "[narou] 警告: NAROU_RELAY=1 ですが $RELAY_JS がありません" >&2
  elif [ -z "${SORAHOST_PROXY_KEY:-}" ]; then
    echo "[narou] 警告: SORAHOST_PROXY_KEY が無いためリレーを起動しません" >&2
    echo "[narou] (Worker 側の SORAHOST_PROXY_KEY と同じ値を .env に置いて下さい)" >&2
  else
    node "$RELAY_JS" </dev/null &
    PIDS+=("$!")
    echo "[narou] 取得リレーを起動しました (待受は 127.0.0.1:${PORT:-3000} / narou は ${NAROU_PORT})"
  fi
fi

"$NAROU" web --port "$NAROU_PORT" --no-browser </dev/null &
PIDS+=("$!")

# どれかが落ちたら全体を終了し、Pterodactyl に再起動させる。
wait -n "${PIDS[@]}"
