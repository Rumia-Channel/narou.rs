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
# 公開は PteWorker が行う。アプリは **PteWorker から渡される PORT** に
# ループバックで束縛する (外部へ直接公開しない)。cloudflared は使わない。
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

# --- 毎回そろえる設定 -----------------------------------------------------
# ループバックだけを向き、Host / Origin は前段 (PteWorker) が渡す公開ホスト名と
# 一致させる。basic 認証は公開エンドポイントなので必須。
# server-ws-port=0: 併設 WebSocket リスナーを作らない。narou.rb は
# `server-port + 1` も使うが、PteWorker はその番号を自分のルータ (workerd) に
# 使うため衝突する。WebSocket は本体ポートの `/ws` で受けるので機能は落ちない。
set -- \
  "server-bind=127.0.0.1" \
  "server-port=$NAROU_PORT" \
  "server-reverse-proxy.enable=true" \
  "server-basic-auth.enable=true" \
  "server-ws-port=0" \
  "convert.section-cache=false"
if [ -n "${NAROU_WEB_USER:-}" ]; then
  set -- "$@" "server-basic-auth.user=$NAROU_WEB_USER"
fi
if [ -n "${NAROU_WEB_PASSWORD:-}" ]; then
  set -- "$@" "server-basic-auth.password=$NAROU_WEB_PASSWORD"
fi
"$BIN" setting "$@" < /dev/null

# 公開エンドポイントなので、basic 認証が無いまま公開しない (fail closed)。
# 前段 (Cloudflare Access 等) で守る構成のときだけ NAROU_ALLOW_NO_PASSWORD=1 で通す。
if [ -z "${NAROU_WEB_PASSWORD:-}" ] \
  && [ -z "$("$BIN" setting server-basic-auth.password < /dev/null)" ]; then
  if [ "${NAROU_ALLOW_NO_PASSWORD:-0}" = "1" ]; then
    echo "[narou] 警告: basic 認証なしで公開します (NAROU_ALLOW_NO_PASSWORD=1)" >&2
  else
    echo "[narou] NAROU_WEB_PASSWORD が未設定です。公開エンドポイントを認証なしで" >&2
    echo "[narou] 公開しないため起動しません (意図的なら NAROU_ALLOW_NO_PASSWORD=1)。" >&2
    exit 1
  fi
fi

# --- 起動したプロセスをまとめて片付ける -----------------------------------
cleanup() {
  for pid in $PIDS; do
    kill "$pid" 2>/dev/null || true
  done
}
# TERM/INT は明示的に抜ける (ハンドラから戻るとループが再開してしまうため)
trap 'cleanup; exit 0' INT TERM
trap cleanup EXIT

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
