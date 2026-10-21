#Requires -Version 7.0
<#
.SYNOPSIS
  自宅回線で取得リレー (Rust 版 narou_relay) を起動し、cloudflared のクイックトンネルで HTTPS 公開する。

.DESCRIPTION
  Cloud Run / SORAHOST はデータセンター回線のため、Cloudflare のチャレンジ配下に
  あるサイト (ハーメルン等) を取得できない。自宅回線では取得できることを実測して
  いるため、このスクリプトはその用途向けにリレーとトンネルをまとめて起動する。

  リレーはワークスペース crate `relay/` のバイナリ (`narou_relay`) を実行する。
  バイナリが無ければ (または -Rebuild 指定時) リポジトリルートで
  `cargo build --release -p narou_relay` を実行してから起動する。

  認証キーと許可ホストは `scripts/relay/.env` (このスクリプトと同じ場所) から
  リレーが読み込む (このスクリプトが NAROU_RELAY_ENV_FILE で場所を明示する)。
  `.env.example` をコピーして使うこと。

  表示された公開 URL を GitHub Secrets の `NAROU_RELAY_ENDPOINT` に設定し、
  `NAROU_RELAY_KEY` を `.env` と同じ値にする。クイックトンネルの URL は
  起動のたびに変わるので、常用する場合は docs/home-relay.md の「常設化」を参照。

.PARAMETER Port
  リレーの待受ポート。既定 3000。

.PARAMETER NoTunnel
  トンネルを張らず、ローカル (127.0.0.1) のみで起動する。

.PARAMETER CloudflaredPath
  cloudflared のパス。省略時は PATH と既定のインストール先から探す。

.PARAMETER BinaryPath
  リレーのバイナリのパス。省略時はリポジトリの `target/release/narou_relay(.exe)`。

.PARAMETER Rebuild
  既存のバイナリがあっても `cargo build --release -p narou_relay` を実行してから起動する。
#>
[CmdletBinding()]
param(
    [int]$Port = 3000,
    [switch]$NoTunnel,
    [string]$CloudflaredPath = "",
    [string]$BinaryPath = "",
    [switch]$Rebuild
)

$ErrorActionPreference = "Stop"

$relayDir = $PSScriptRoot
$repoRoot = Split-Path -Parent (Split-Path -Parent $relayDir)
$envFile = Join-Path $relayDir ".env"
$binaryName = if ($IsWindows) { "narou_relay.exe" } else { "narou_relay" }
$defaultBinary = Join-Path $repoRoot "target/release/$binaryName"

function Read-EnvFile {
    param([string]$Path)
    $values = @{}
    if (-not (Test-Path -LiteralPath $Path)) { return $values }
    foreach ($line in Get-Content -LiteralPath $Path) {
        if ($line -match '^\s*#' -or $line -notmatch '=') { continue }
        $index = $line.IndexOf('=')
        $key = $line.Substring(0, $index).Trim()
        if ($key -notmatch '^[A-Za-z_][A-Za-z0-9_]*$') { continue }
        $values[$key] = $line.Substring($index + 1)
    }
    return $values
}

function Resolve-RelayBinary {
    param([string]$ExplicitPath, [switch]$ForceBuild)
    if ($ExplicitPath) {
        if (-not (Test-Path -LiteralPath $ExplicitPath)) {
            throw "リレーのバイナリが見つかりません: $ExplicitPath"
        }
        return (Resolve-Path -LiteralPath $ExplicitPath).Path
    }
    if (-not $ForceBuild -and (Test-Path -LiteralPath $defaultBinary)) {
        return $defaultBinary
    }
    $cargo = Get-Command cargo -ErrorAction SilentlyContinue
    if (-not $cargo) {
        throw "cargo が見つかりません。Rust ツールチェーンをインストールしてください (https://rustup.rs/)"
    }
    Write-Host "リレーをビルドしています (cargo build --release -p narou_relay) ..."
    Push-Location -LiteralPath $repoRoot
    try {
        & $cargo.Source build --release -p narou_relay
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build --release -p narou_relay が失敗しました (終了コード $LASTEXITCODE)"
        }
    } finally {
        Pop-Location
    }
    if (-not (Test-Path -LiteralPath $defaultBinary)) {
        throw "ビルド後もリレーのバイナリが見つかりません: $defaultBinary"
    }
    return $defaultBinary
}

function Wait-Health {
    param([string]$Url, [int]$Attempts = 80)
    for ($i = 0; $i -lt $Attempts; $i++) {
        try {
            $response = Invoke-WebRequest -Uri $Url -TimeoutSec 2 -UseBasicParsing
            if ($response.StatusCode -eq 200) { return $true }
        } catch {
            # 起動待ち。次の試行へ。
        }
        Start-Sleep -Milliseconds 250
    }
    return $false
}

$fileEnv = Read-EnvFile -Path $envFile
if (-not $env:NAROU_RELAY_KEY -and -not $fileEnv.ContainsKey("NAROU_RELAY_KEY")) {
    throw "NAROU_RELAY_KEY が未設定です。$envFile を作成してください（$($envFile).example を参照）"
}
if (-not $env:NAROU_RELAY_ALLOWED_HOSTS -and -not $fileEnv.ContainsKey("NAROU_RELAY_ALLOWED_HOSTS")) {
    Write-Warning "NAROU_RELAY_ALLOWED_HOSTS が未設定です。取得はすべて 400 で拒否されます"
}

$binaryPath = Resolve-RelayBinary -ExplicitPath $BinaryPath -ForceBuild:$Rebuild

$env:NAROU_RELAY_ENV_FILE = $envFile
$env:PORT = "$Port"
if (-not $env:NAROU_RELAY_HOST) { $env:NAROU_RELAY_HOST = "127.0.0.1" }

$relay = $null
$tunnel = $null
$tunnelLog = Join-Path ([System.IO.Path]::GetTempPath()) ("narou-relay-tunnel-{0}.log" -f $PID)
$tunnelOut = Join-Path ([System.IO.Path]::GetTempPath()) ("narou-relay-tunnel-{0}.out" -f $PID)

try {
    Write-Host "リレーを起動しています (127.0.0.1:$Port) ..."
    $relay = Start-Process -FilePath $binaryPath -WorkingDirectory $repoRoot -PassThru -NoNewWindow

    if (-not (Wait-Health -Url "http://127.0.0.1:$Port/health")) {
        if ($relay.HasExited) {
            throw "リレーが起動直後に終了しました (終了コード $($relay.ExitCode))。表示された 'narou relay startup failed: ...' と $envFile の NAROU_RELAY_KEY / NAROU_RELAY_ALLOWED_HOSTS (必要なら NAROU_RELAY_FETCH / NAROU_RELAY_CACERT) を確認してください"
        }
        throw "リレーの /health が応答しません (port $Port)"
    }

    $publicUrl = ""
    if (-not $NoTunnel) {
        if (-not $CloudflaredPath) {
            $command = Get-Command cloudflared -ErrorAction SilentlyContinue
            if ($command) { $CloudflaredPath = $command.Source }
            elseif (Test-Path -LiteralPath "${env:ProgramFiles(x86)}\cloudflared\cloudflared.exe") {
                $CloudflaredPath = "${env:ProgramFiles(x86)}\cloudflared\cloudflared.exe"
            } elseif (Test-Path -LiteralPath "$env:ProgramFiles\cloudflared\cloudflared.exe") {
                $CloudflaredPath = "$env:ProgramFiles\cloudflared\cloudflared.exe"
            }
        }
        if (-not $CloudflaredPath -or -not (Test-Path -LiteralPath $CloudflaredPath)) {
            Write-Warning "cloudflared が見つかりません。トンネルなしで起動します (-NoTunnel と同じ)"
        } else {
            Write-Host "cloudflared のクイックトンネルを開始しています ..."
            $tunnel = Start-Process -FilePath $CloudflaredPath `
                -ArgumentList @("tunnel", "--no-autoupdate", "--url", "http://127.0.0.1:$Port") `
                -PassThru -WindowStyle Hidden -RedirectStandardError $tunnelLog -RedirectStandardOutput $tunnelOut
            for ($i = 0; $i -lt 160; $i++) {
                if ($tunnel.HasExited) { break }
                $text = ""
                if (Test-Path -LiteralPath $tunnelLog) {
                    $text = [string](Get-Content -LiteralPath $tunnelLog -Raw -ErrorAction SilentlyContinue)
                }
                if ($text -match "https://[a-z0-9-]+\.trycloudflare\.com") {
                    $publicUrl = $Matches[0]
                    break
                }
                Start-Sleep -Milliseconds 250
            }
            if (-not $publicUrl) {
                Write-Warning "クイックトンネルの URL を取得できませんでした。ログ: $tunnelLog"
            }
        }
    }

    Write-Host ""
    Write-Host "ローカル確認: http://127.0.0.1:$Port/health"
    if ($publicUrl) {
        Write-Host "公開 URL:     $publicUrl"
        $publicHealthy = Wait-Health -Url "$publicUrl/health" -Attempts 80
        if ($publicHealthy) {
            Write-Host "公開 URL の疎通: OK ($publicUrl/health)"
        } else {
            Write-Warning "公開 URL の /health に到達できません。DNS 反映待ちか、トンネルの制限を確認してください"
        }
        Write-Host ""
        Write-Host "Worker の設定 (GitHub Secrets):"
        Write-Host "  NAROU_RELAY_ENDPOINT = $publicUrl"
        Write-Host "  NAROU_RELAY_KEY      = scripts/relay/.env と同じ値"
        Write-Host "設定後、Worker を再配備してハーメルンの取得を試してください。"
        Write-Host "クイックトンネルの URL は再起動のたびに変わります。常設化は docs/home-relay.md を参照。"
    }
    Write-Host ""
    Write-Host "Ctrl+C で停止します。"

    while (-not $relay.HasExited) { Start-Sleep -Seconds 1 }
} finally {
    if ($tunnel -and -not $tunnel.HasExited) {
        Stop-Process -Id $tunnel.Id -Force -ErrorAction SilentlyContinue
    }
    if ($relay -and -not $relay.HasExited) {
        Stop-Process -Id $relay.Id -Force -ErrorAction SilentlyContinue
    }
    foreach ($path in @($tunnelLog, $tunnelOut)) {
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue }
    }
}
