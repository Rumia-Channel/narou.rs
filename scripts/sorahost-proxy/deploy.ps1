# SORAHOST の取得リレーをビルドして PteWorker へデプロイする。
#
# 事前準備:
#   1. Pterodactyl の .env に SORAHOST_PROXY_KEY=<長いランダム文字列> を設定して再起動
#   2. このスクリプトを PowerShell で実行する（初回に接続先とデプロイトークンを聞かれる）
#
# PteWorker のコンテナには curl が入っていないため、ハッシュ検証済みの静的 curl を
# 同梱してデプロイする。リレーは同梱の curl を書き込み可能な場所へコピーして実行する。
#
# 使い方:
#   powershell -ExecutionPolicy Bypass -File scripts\sorahost-proxy\deploy.ps1
param(
    [string]$WorkDir = "$env:TEMP\narou-sorahost-relay",
    [string]$CurlVersion = "v8.21.0"
)

$ErrorActionPreference = "Stop"
$source = Join-Path $PSScriptRoot "server.mjs"
if (-not (Test-Path $source)) { throw "server.mjs が見つかりません: $source" }

New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null
Push-Location $WorkDir
try {
    $base = "https://github.com/moparisthebest/static-curl/releases/download/$CurlVersion"
    Write-Host "静的 curl を取得しています ($CurlVersion)…"
    curl.exe -fsSL -o curl "$base/curl-amd64"
    curl.exe -fsSL -o sha256sum.txt "$base/sha256sum.txt"
    if (-not (Test-Path cacert.pem)) {
        curl.exe -fsSL -o cacert.pem "https://curl.se/ca/cacert.pem"
    }

    $want = ((Select-String -Path sha256sum.txt -Pattern "curl-amd64").Line -split "\s+")[0].ToLower()
    $got = (Get-FileHash curl -Algorithm SHA256).Hash.ToLower()
    if ($want -ne $got) { throw "curl のハッシュが一致しません (期待 $want / 実際 $got)" }
    Write-Host "ハッシュ検証 OK: $got"

    Copy-Item $source . -Force
    Set-Content -Path sorahost.json -Encoding ascii -Value '{"mode":"node","framework":"node","start":"node server.mjs","include":["server.mjs","curl","cacert.pem"]}'

    Write-Host "PteWorker へデプロイします (接続先とデプロイトークンを入力)…"
    npx --yes sorahost-cli deploy
}
finally {
    Pop-Location
}

Write-Host ""
Write-Host "完了後、次を確認してください:"
Write-Host "  curl http://<接続先のホスト>/health   → {'ok':true}"
Write-Host "  不正なトークンで GET /proxy           → 403 (403 なら GET 対応の新版)"
