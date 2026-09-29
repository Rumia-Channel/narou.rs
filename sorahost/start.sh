#!/bin/sh
# SORAHOST (PteWorker の node モード) で narou_rs を常駐させる起動スクリプト。
#
# POSIX sh で書いてある (コンテナの /bin/sh だけを前提にする。bash が入って
# いない最小イメージでも動かすため)。
#
# 配置 (プロジェクトルート = コンテナのボリューム):
#   sorahost.json   配備の定義 (PteWorker が読む)
#   start.sh        このスクリプト。`sorahost.json` の start が呼ぶ
#   app/            実行ファイル + webnovel/ (配備で入れ替わる)
#   library/        narou のデータ (配備に含めない。作品・設定はここだけ)
#
# 公開は 2 通り:
#   (a) PteWorker の公開 URL をそのまま使う (平文 HTTP。basic 認証だけが頼り)
#   (b) Cloudflare Tunnel + Access を前段に置く (推奨。TLS と本人確認が付く)
#       TUNNEL_TOKEN を置いて NAROU_TUNNEL_HOST を設定すると (b) になり、直の
#       IP:ポート宛は narou の Host 許可リストで弾かれる。
# アプリはどちらでも **PteWorker から渡される PORT** にループバックで束縛する。
#
# コンソールは PTY なので、対話プロンプトを持つコマンドには `< /dev/null` を
# 付けて stdin を端末でなくす (付けないと初回起動が止まる)。

set -eu

SELF_DIR="$(cd "$(dirname "$0")" && pwd)"
APP="${NAROU_RS_APP:-$SELF_DIR/app}"

# ライブラリは配備で消えない場所へ置く。PteWorker は配備ごとに
# `.sorahost/releases/<日時>-<hash>/` を作り直すので、その外 (ボリューム直下) に置く。
case "$SELF_DIR" in
  */.sorahost/releases/*)
    VOLUME_DIR="$(cd "$SELF_DIR/../../.." 2>/dev/null && pwd || echo "$SELF_DIR")"
    ;;
  *)
    VOLUME_DIR="${HOME:-$SELF_DIR}"
    ;;
esac
LIB="${NAROU_RS_LIBRARY:-$VOLUME_DIR/narou-library}"
BIN_DIR="$VOLUME_DIR/bin"
TOKEN_FILE="${TUNNEL_TOKEN_FILE:-$VOLUME_DIR/.cloudflared-token}"
CF_BIN="$BIN_DIR/cloudflared"
CF_TAG="2026.9.3"
CF_SHA256="77e26d8d900e0b8469f416239d14b5f296525fdf79fee6f511ef55609e3fbac2"
CF_URL="https://github.com/cloudflare/cloudflared/releases/download/${CF_TAG}/cloudflared-linux-amd64"
TUNNEL_HOST="${NAROU_TUNNEL_HOST:-}"

BIN="$APP/narou_rs"
PIDS=""

# 相乗り (NAROU_RELAY=1) のときはプラットフォームの PORT をリレーが使うので、
# narou 側は別ポートにする (sorahost/README.md §10)。
if [ "${NAROU_RELAY:-0}" = "1" ]; then
  NAROU_PORT="${NAROU_RS_PORT:-8080}"
else
  NAROU_PORT="${NAROU_RS_PORT:-${PORT:-8080}}"
fi

# 配備の経路 (tar / CLI / プラットフォームの展開) で実行ビットが落ちることが
# あるので、読み込みが済んでいれば自分で付け直す。
if [ -f "$BIN" ] && [ ! -x "$BIN" ]; then
  chmod +x "$BIN" 2>/dev/null || true
fi
if [ -f "$APP/narou_rs_backup" ] && [ ! -x "$APP/narou_rs_backup" ]; then
  chmod +x "$APP/narou_rs_backup" 2>/dev/null || true
fi

if [ ! -x "$BIN" ]; then
  echo "[narou] $BIN が実行できません (配備が不完全か、実行属性がありません)" >&2
  ls -l "$APP" >&2 || true
  exit 1
fi

mkdir -p "$LIB"

# --- 初回セットアップ -----------------------------------------------------
cd "$LIB"
if [ ! -d "$LIB/.narou" ]; then
  echo "[narou] ライブラリを初期化します: $LIB"
  # 同梱の webnovel/*.yaml は実行ファイルの隣 ($APP/webnovel) からコピーされる。
  "$BIN" init < /dev/null
  if [ ! -d "$LIB/.narou" ]; then
    echo "[narou] init がライブラリを作れませんでした: $LIB" >&2
    exit 1
  fi
  # SQLite 管理 (Lite) にする。YAML に戻すときは `narou db export-yaml --in-place`。
  printf 'sqlite\n' > "$LIB/.narou/storage-backend"
  # 容量節約の既定 (初回だけ入れるので、後から変えても上書きされない):
  #   nosave_diff  更新のたびに作られる差分スナップショットを保存しない
  #   nosave_raw   取得した raw HTML を保存しない (挿絵のローカライズは取得時に
  #                メモリ上で行うので、新規話の挿絵は従来どおり保存される)
  #   no-epub      EPUB を保存しない (Web UI の「EPUB をダウンロード」は都度生成)
  #   keep-txt     txt を残さない (変換結果は SQLite にあり EPUB はそこから生成)
  #   mirror-files 小説データ/ へ実ファイルを書かない (DB だけが保存先)
  # concurrency: DL/update と convert/send を別レーンで並行に流す (小説単位の
  # 排他は .narou/lock.yaml が効くので、同じ小説が両方で走ることはない)。
  "$BIN" setting \
    economy=nosave_diff,nosave_raw \
    convert.no-epub=true \
    convert.keep-txt=false \
    sqlite.mirror-files=false \
    concurrency=true < /dev/null
fi

# --- basic 認証 (公開エンドポイントなので必須) -----------------------------
# 資格情報が空だと narou は認証ヘッダを作らず素通しになるため、未設定のまま
# 公開しない。ただし起動自体を止めると PteWorker がデプロイ失敗 (422) と見なし、
# **前のリリースを配り続けてしまう** (実測) ので、その場でランダムなパスワードを
# 設定して「誰も入れない状態」で起動する。値を決めたいときは NAROU_WEB_PASSWORD を
# パネルの .env に置く。前段 (Cloudflare Access 等) で守る構成なら
# NAROU_ALLOW_NO_PASSWORD=1 で認証なしのまま起動する。
STORED_PASSWORD="$("$BIN" setting server-basic-auth.password < /dev/null)"
if [ -z "${NAROU_WEB_PASSWORD:-}" ] && [ -z "$STORED_PASSWORD" ]; then
  if [ "${NAROU_ALLOW_NO_PASSWORD:-0}" = "1" ]; then
    echo "[narou] 警告: basic 認証なしで公開します (NAROU_ALLOW_NO_PASSWORD=1)" >&2
  else
    NAROU_WEB_PASSWORD="$(tr -dc 'A-Za-z0-9' < /dev/urandom | head -c 24)"
    echo "[narou] NAROU_WEB_PASSWORD が未設定のため、一時的なパスワードを設定しました:" >&2
    echo "[narou]   ${NAROU_WEB_USER:-admin} / $NAROU_WEB_PASSWORD" >&2
    echo "[narou]   パネルの .env に NAROU_WEB_PASSWORD を入れて再起動すると置き換わります" >&2
  fi
fi
# narou は user と password の**両方**が埋まっていないと認証ヘッダを作らない
# (片方だけだと素通しになる) ので、パスワードがあるときは user を既定で補う。
if [ -n "${NAROU_WEB_PASSWORD:-}" ] || [ -n "$STORED_PASSWORD" ]; then
  NAROU_WEB_USER="${NAROU_WEB_USER:-admin}"
fi

# --- 毎回そろえる設定 -----------------------------------------------------
# ループバックだけを向き、Host / Origin は前段 (PteWorker) が渡す公開ホスト名と
# 一致させる。
# server-ws-port=0: 併設 WebSocket リスナーを作らない。narou.rb は
# `server-port + 1` も使うが、PteWorker はその番号を自分のルータ (workerd) に
# 使うため衝突する。WebSocket は本体ポートの `/ws` で受けるので機能は落ちない。
if [ -n "$TUNNEL_HOST" ]; then
  # トンネル経由 (Host = 公開ホスト名) だけを受け付ける。直の IP:ポート宛は
  # Host が許可リストに無いので 400 で落ちる。
  set -- \
    "server-bind=127.0.0.1" \
    "server-port=$NAROU_PORT" \
    "server-reverse-proxy.enable=false" \
    "server-add-accepted-hosts=$TUNNEL_HOST" \
    "server-ws-add-accepted-domains=$TUNNEL_HOST" \
    "server-basic-auth.enable=true" \
    "server-ws-port=0" \
    "convert.section-cache=false"
else
  set -- \
    "server-bind=127.0.0.1" \
    "server-port=$NAROU_PORT" \
    "server-reverse-proxy.enable=true" \
    "server-basic-auth.enable=true" \
    "server-ws-port=0" \
    "convert.section-cache=false"
fi
if [ -n "${NAROU_WEB_USER:-}" ]; then
  set -- "$@" "server-basic-auth.user=$NAROU_WEB_USER"
fi
if [ -n "${NAROU_WEB_PASSWORD:-}" ]; then
  set -- "$@" "server-basic-auth.password=$NAROU_WEB_PASSWORD"
fi
"$BIN" setting "$@" < /dev/null

# --- 起動したプロセスをまとめて片付ける -----------------------------------
cleanup() {
  for pid in $PIDS; do
    kill "$pid" 2>/dev/null || true
  done
}
# TERM/INT は明示的に抜ける (ハンドラから戻るとループが再開してしまうため)
trap 'cleanup; exit 0' INT TERM
trap cleanup EXIT

# --- Cloudflare Tunnel (任意) ----------------------------------------------
# TUNNEL_TOKEN (または TUNNEL_TOKEN_FILE) があるときだけ起動する。トークンは
# Cloudflare のダッシュボード (Networks → Tunnels) で発行し、パネルの .env に置く。
NAROU_TUNNEL_TOKEN="${TUNNEL_TOKEN:-}"
if [ -z "$NAROU_TUNNEL_TOKEN" ] && [ -f "$TOKEN_FILE" ]; then
  NAROU_TUNNEL_TOKEN="$(tr -d '\r\n' < "$TOKEN_FILE")"
fi
if [ -n "$NAROU_TUNNEL_TOKEN" ]; then
  mkdir -p "$BIN_DIR"
  if [ ! -x "$CF_BIN" ]; then
    echo "[narou] cloudflared ${CF_TAG} を取得します"
    if ! curl -fsSL -o "$CF_BIN.tmp" "$CF_URL"; then
      echo "[narou] cloudflared を取得できませんでした (手動で $CF_BIN に置けば起動します)" >&2
    fi
  fi
  if [ -f "$CF_BIN.tmp" ]; then
    GOT="$(sha256sum "$CF_BIN.tmp" | cut -d' ' -f1)"
    if [ "$GOT" = "$CF_SHA256" ]; then
      mv "$CF_BIN.tmp" "$CF_BIN"
      chmod +x "$CF_BIN"
      echo "[narou] cloudflared のハッシュを確認しました"
    else
      rm -f "$CF_BIN.tmp"
      echo "[narou] cloudflared のハッシュが一致しません (期待 $CF_SHA256 / 実際 $GOT)" >&2
    fi
  fi
  if [ -x "$CF_BIN" ] && [ -z "$TUNNEL_HOST" ]; then
    echo "[narou] 警告: NAROU_TUNNEL_HOST が未設定です (直の IP:ポート宛も受け付けます)" >&2
  fi
  if [ -x "$CF_BIN" ]; then
    TUNNEL_TOKEN="$NAROU_TUNNEL_TOKEN" "$CF_BIN" tunnel --no-autoupdate --loglevel info run </dev/null &
    PIDS="$PIDS $!"
    echo "[narou] Cloudflare Tunnel を起動しました (向き先 127.0.0.1:${NAROU_PORT})"
  fi
fi

# --- 取得リレー (任意・Worker の踏み台) ------------------------------------
# 通常は別サーバーなので何もしない。同じサーバーで動かすときだけ NAROU_RELAY=1。
RELAY_JS="$SELF_DIR/server.mjs"
if [ "${NAROU_RELAY:-0}" = "1" ]; then
  if [ ! -f "$RELAY_JS" ]; then
    echo "[narou] 警告: NAROU_RELAY=1 ですが $RELAY_JS がありません" >&2
  elif [ -z "${SORAHOST_PROXY_KEY:-}" ]; then
    echo "[narou] 警告: SORAHOST_PROXY_KEY が無いためリレーを起動しません" >&2
    echo "[narou] (Worker 側の SORAHOST_PROXY_KEY と同じ値を .env に置いて下さい)" >&2
  else
    node "$RELAY_JS" </dev/null &
    PIDS="$PIDS $!"
    echo "[narou] 取得リレーを起動しました (待受は 127.0.0.1:${PORT:-3000} / narou は ${NAROU_PORT})"
  fi
fi

echo "[narou] 起動します (127.0.0.1:${NAROU_PORT} / ライブラリ ${LIB})"
"$BIN" web --port "$NAROU_PORT" --no-browser </dev/null &
PIDS="$PIDS $!"

# どれかが落ちたら全体を終了し、PteWorker に再起動させる (wait -n は POSIX に
# 無いので、1 秒間隔で生存を確かめる)。
while :; do
  for pid in $PIDS; do
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "[narou] プロセス $pid が終了しました。全体を止めます" >&2
      exit 0
    fi
  done
  sleep 2
done
