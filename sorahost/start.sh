#!/usr/bin/env bash
# SORAHOST (PteWorker の node モード) で narou_rs を常駐させる起動スクリプト。
#
# 配置 (プロジェクトルート = コンテナのボリューム):
#   sorahost.json   配備の定義 (PteWorker が読む)
#   start.sh        このスクリプト。`sorahost.json` の start が呼ぶ
#   app/            実行ファイル + webnovel/ + preset/ (配備で入れ替わる)
#   library/        narou のデータ (配備に含めない。作品・設定はここだけ)
#
#   → ライブラリを `include` に入れないので、配備で作品が消えない (はず。
#     初回は配備を 2 回流して `library/` が残ることを確認する)。
#
# 公開は PteWorker が行う。アプリは **PteWorker から渡される PORT** に
# ループバックで束縛する (外部へ直接公開しない)。cloudflared は使わない。
#
# 再起動: `sorahost-cli deploy` のあと PteWorker が起動し直す。手で再起動して
# もよい (`restart` コマンド)。コンソールは PTY なので、対話プロンプトを持つ
# コマンドには `< /dev/null` を付けて stdin を端末でなくす。

set -euo pipefail

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
APP="${NAROU_RS_APP:-$SELF_DIR/app}"
LIB="${NAROU_RS_LIBRARY:-$SELF_DIR/library}"
BIN="$APP/narou_rs"

# 相乗り (NAROU_RELAY=1) のときはプラットフォームの PORT をリレーが使うので、
# narou 側は別ポートにする (sorahost/README.md §10)。
if [ "${NAROU_RELAY:-0}" = "1" ]; then
  NAROU_PORT="${NAROU_RS_PORT:-8080}"
else
  NAROU_PORT="${NAROU_RS_PORT:-${PORT:-8080}}"
fi

[ -x "$BIN" ] || {
  echo "[narou] $BIN がありません (配備が不完全です)" >&2
  exit 1
}

mkdir -p "$LIB"

# --- 初回セットアップ -----------------------------------------------------
cd "$LIB"
if [ ! -d "$LIB/.narou" ]; then
  echo "[narou] ライブラリを初期化します: $LIB"
  # 同梱の webnovel/*.yaml は実行ファイルの隣 ($APP/webnovel) からコピーされる。
  "$BIN" init < /dev/null
  [ -d "$LIB/.narou" ] || {
    echo "[narou] init がライブラリを作れませんでした: $LIB" >&2
    exit 1
  }
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
SETTINGS=(
  "server-bind=127.0.0.1"
  "server-port=$NAROU_PORT"
  "server-reverse-proxy.enable=true"
  "server-basic-auth.enable=true"
  # 容量節約のため、話ごとの変換キャッシュは作らない (再変換が少し遅くなるだけ)。
  # 環境変数 NAROU_RS_SECTION_CACHE=0 で切る指定は従来どおり任意。
  "convert.section-cache=false"
)
if [ -n "${NAROU_WEB_USER:-}" ]; then
  SETTINGS+=("server-basic-auth.user=$NAROU_WEB_USER")
fi
if [ -n "${NAROU_WEB_PASSWORD:-}" ]; then
  SETTINGS+=("server-basic-auth.password=$NAROU_WEB_PASSWORD")
fi
"$BIN" setting "${SETTINGS[@]}" < /dev/null

if [ -z "${NAROU_WEB_PASSWORD:-}" ] \
  && [ -z "$("$BIN" setting server-basic-auth.password < /dev/null)" ]; then
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
    PIDS+=("$!")
    echo "[narou] 取得リレーを起動しました (待受は 127.0.0.1:${PORT:-3000} / narou は ${NAROU_PORT})"
  fi
fi

echo "[narou] 起動します (127.0.0.1:${NAROU_PORT} / ライブラリ ${LIB})"
"$BIN" web --port "$NAROU_PORT" --no-browser </dev/null &
PIDS+=("$!")

# どれかが落ちたら全体を終了し、PteWorker に再起動させる。
wait -n "${PIDS[@]}"
