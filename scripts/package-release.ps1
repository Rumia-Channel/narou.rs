param(
    [Parameter(Mandatory = $true)]
    [string]$BinaryPath,

    [Parameter(Mandatory = $true)]
    [string]$UpdaterBinaryPath,

    [Parameter(Mandatory = $true)]
    [string]$BackupBinaryPath,

    [Parameter(Mandatory = $true)]
    [string]$LoginBinaryPath,
    [Parameter(Mandatory = $true)]
    [ValidateSet("win", "mac", "linux")]
    [string]$Platform,

    [string]$Arch = "x64",

    [string]$OutputDir = "dist",

    [string]$PackageRoot = "narou",

    [string[]]$ExtraFiles = @("LICENSE", "README.md", "Third-Party-License.md"),

    [string[]]$ResourceDirectories = @("webnovel", "preset"),

    [string]$CommitVersion,

    # Optional build variant tag appended to the archive name (e.g. "GPL").
    [string]$Variant = "",

    # 署名検証を省く。署名できないローカル検証用の抜け道で、
    # リリース CI では指定しない (未署名のまま配布する事故を防ぐ)。
    [switch]$SkipSignatureCheck
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if (-not (Test-Path -Path $BinaryPath -PathType Leaf)) {
    throw "Binary not found: $BinaryPath"
}
if (-not (Test-Path -Path $UpdaterBinaryPath -PathType Leaf)) {
    throw "Updater binary not found: $UpdaterBinaryPath"
}
if (-not (Test-Path -Path $BackupBinaryPath -PathType Leaf)) {
    throw "Backup binary not found: $BackupBinaryPath"
}
if (-not (Test-Path -Path $LoginBinaryPath -PathType Leaf)) {
    throw "Login binary not found: $LoginBinaryPath"
}

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null

$resolvedBinary = (Resolve-Path -Path $BinaryPath).Path
$resolvedUpdaterBinary = (Resolve-Path -Path $UpdaterBinaryPath).Path
$resolvedBackupBinary = (Resolve-Path -Path $BackupBinaryPath).Path
$resolvedLoginBinary = (Resolve-Path -Path $LoginBinaryPath).Path
$resolvedOutputDir = (Resolve-Path -Path $OutputDir).Path

# Windows 版は本体とサブ実行ファイルすべてに Authenticode 署名が要る。
# 署名ジョブが成果物を落とした場合に未署名のまま zip へ入るのを防ぐため、
# 梱包前に検証する。
function Assert-WindowsBinarySigned {
    param([Parameter(Mandatory = $true)][string]$Path)

    $signature = Get-AuthenticodeSignature -FilePath $Path
    if ($signature.Status -ne "Valid") {
        throw "Windows binary is not signed: $Path (status: $($signature.Status))"
    }
}

if ($Platform -eq "win" -and -not $SkipSignatureCheck) {
    foreach ($windowsBinary in @(
            $resolvedBinary,
            $resolvedUpdaterBinary,
            $resolvedBackupBinary,
            $resolvedLoginBinary
        )) {
        Assert-WindowsBinarySigned -Path $windowsBinary
    }
}

$variantSuffix = if ([string]::IsNullOrWhiteSpace($Variant)) { "" } else { "-$Variant" }
$archiveName = "narou_rs_{0}_{1}{2}.zip" -f $Platform, $Arch, $variantSuffix
$archivePath = Join-Path -Path $resolvedOutputDir -ChildPath $archiveName

# 同梱するサードパーティライセンス全文。GPL 版は GPL-3.0-only の
# AozoraEpub3_Lite を含むため GPL 記載のあるノーティスを、非 GPL 版は
# copyleft を含まないノーティスを選ぶ。どちらもアーカイブ内では
# Third-Party-License.md という名前で入る。
$LicenseNoticeSource = if ([string]::IsNullOrWhiteSpace($Variant)) {
    "Third-Party-License-non-GPL.md"
}
else {
    "Third-Party-License.md"
}

if (Test-Path -Path $archivePath -PathType Leaf) {
    Remove-Item -Path $archivePath -Force
}

Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

function Add-FileToArchive {
    param(
        [Parameter(Mandatory = $true)]
        [System.IO.Compression.ZipArchive]$Archive,

        [Parameter(Mandatory = $true)]
        [string]$SourcePath,

        [Parameter(Mandatory = $true)]
        [string]$EntryPath
    )

    [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
        $Archive,
        $SourcePath,
        $EntryPath.Replace('\', '/'),
        [System.IO.Compression.CompressionLevel]::Optimal
    ) | Out-Null
}

function Add-DirectoryToArchive {
    param(
        [Parameter(Mandatory = $true)]
        [System.IO.Compression.ZipArchive]$Archive,

        [Parameter(Mandatory = $true)]
        [string]$SourceDir,

        [Parameter(Mandatory = $true)]
        [string]$EntryRoot
    )

    $resolvedDir = (Resolve-Path -Path $SourceDir).Path
    $files = Get-ChildItem -Path $resolvedDir -Recurse -File
    foreach ($file in $files) {
        $relative = [System.IO.Path]::GetRelativePath($resolvedDir, $file.FullName)
        $entryPath = Join-Path -Path $EntryRoot -ChildPath $relative
        Add-FileToArchive -Archive $Archive -SourcePath $file.FullName -EntryPath $entryPath
    }
}

function Resolve-CommitVersion {
    param([string]$ExplicitValue)

    if (-not [string]::IsNullOrWhiteSpace($ExplicitValue)) {
        return $ExplicitValue.Trim()
    }

    if (-not [string]::IsNullOrWhiteSpace($env:GITHUB_REF_NAME)) {
        return $env:GITHUB_REF_NAME.Trim()
    }

    $gitVersion = git describe --always 2>$null
    if ($LASTEXITCODE -eq 0 -and -not [string]::IsNullOrWhiteSpace($gitVersion)) {
        return $gitVersion.Trim()
    }

    $cargoToml = Join-Path -Path $PSScriptRoot -ChildPath "..\Cargo.toml"
    if (Test-Path -Path $cargoToml -PathType Leaf) {
        $cargoContent = Get-Content -Path $cargoToml -Raw
        if ($cargoContent -match '(?m)^version\s*=\s*"([^"]+)"') {
            return $Matches[1]
        }
    }

    return "release"
}

$archive = [System.IO.Compression.ZipFile]::Open(
    $archivePath,
    [System.IO.Compression.ZipArchiveMode]::Create
)

try {
    Add-FileToArchive `
        -Archive $archive `
        -SourcePath $resolvedBinary `
        -EntryPath (Join-Path -Path $PackageRoot -ChildPath ([System.IO.Path]::GetFileName($resolvedBinary)))

    # Updater は本体プロセス起動中に自分自身を上書きできないので、
    # 必ず ".new" 拡張子付きで同梱する。新本体の起動時に
    # コンパイル時埋込みハッシュと比較して通常名へ昇格する。
    $updaterFileName = [System.IO.Path]::GetFileName($resolvedUpdaterBinary)
    $updaterEntryName = "{0}.new" -f $updaterFileName
    Add-FileToArchive `
        -Archive $archive `
        -SourcePath $resolvedUpdaterBinary `
        -EntryPath (Join-Path -Path $PackageRoot -ChildPath $updaterEntryName)

    # バックアップ用サブ実行ファイル。本体と同じフォルダに置く。
    Add-FileToArchive `
        -Archive $archive `
        -SourcePath $resolvedBackupBinary `
        -EntryPath (Join-Path -Path $PackageRoot -ChildPath ([System.IO.Path]::GetFileName($resolvedBackupBinary)))

    # ログイン用サブ実行ファイル。本体と同じフォルダに置く。
    Add-FileToArchive `
        -Archive $archive `
        -SourcePath $resolvedLoginBinary `
        -EntryPath (Join-Path -Path $PackageRoot -ChildPath ([System.IO.Path]::GetFileName($resolvedLoginBinary)))

    foreach ($resourceDir in $ResourceDirectories) {
        if ([string]::IsNullOrWhiteSpace($resourceDir)) {
            continue
        }
        if (-not (Test-Path -Path $resourceDir -PathType Container)) {
            throw "Required resource directory not found: $resourceDir"
        }

        Add-DirectoryToArchive `
            -Archive $archive `
            -SourceDir $resourceDir `
            -EntryRoot (Join-Path -Path $PackageRoot -ChildPath $resourceDir)
    }

    foreach ($extraFile in $ExtraFiles) {
        if ([string]::IsNullOrWhiteSpace($extraFile)) {
            continue
        }

        # 同梱するサードパーティライセンス全文はバリアントで異なる。GPL 版は
        # GPL-3.0-only の AozoraEpub3_Lite を含むため GPL 記載のある
        # ノーティスを、非 GPL 版は copyleft を含まないノーティスを使う。
        # アーカイブ内のファイル名はどちらも Third-Party-License.md にする。
        $entryName = [System.IO.Path]::GetFileName($extraFile)
        $sourceFile = $extraFile
        if ($entryName -eq "Third-Party-License.md") {
            $noticeDir = [System.IO.Path]::GetDirectoryName($extraFile)
            $sourceFile = if ([string]::IsNullOrWhiteSpace($noticeDir)) {
                $LicenseNoticeSource
            }
            else {
                Join-Path -Path $noticeDir -ChildPath $LicenseNoticeSource
            }
        }
        if (-not (Test-Path -Path $sourceFile -PathType Leaf)) {
            continue
        }

        $resolvedExtraFile = (Resolve-Path -Path $sourceFile).Path
        Add-FileToArchive `
            -Archive $archive `
            -SourcePath $resolvedExtraFile `
            -EntryPath (Join-Path -Path $PackageRoot -ChildPath $entryName)
    }

    $commitEntry = $archive.CreateEntry((Join-Path -Path $PackageRoot -ChildPath "commitversion").Replace('\', '/'))
    $writer = New-Object System.IO.StreamWriter($commitEntry.Open())
    try {
        $writer.Write((Resolve-CommitVersion -ExplicitValue $CommitVersion))
    }
    finally {
        $writer.Dispose()
    }
}
finally {
    $archive.Dispose()
}

Write-Output $archivePath
