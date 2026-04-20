param(
    [Parameter(Mandatory = $true)]
    [string]$BinaryPath,

    [Parameter(Mandatory = $true)]
    [ValidateSet("win", "mac", "linux")]
    [string]$Platform,

    [string]$Arch = "x64",

    [string]$OutputDir = "dist",

    [string]$PackageRoot = "narou",

    [string[]]$ExtraFiles = @("LICENSE"),

    [string[]]$ResourceDirectories = @("webnovel", "preset"),

    [string]$CommitVersion
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if (-not (Test-Path -Path $BinaryPath -PathType Leaf)) {
    throw "Binary not found: $BinaryPath"
}

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null

$resolvedBinary = (Resolve-Path -Path $BinaryPath).Path
$resolvedOutputDir = (Resolve-Path -Path $OutputDir).Path
$archiveName = "narou_rs_{0}_{1}.zip" -f $Platform, $Arch
$archivePath = Join-Path -Path $resolvedOutputDir -ChildPath $archiveName

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
        if (-not (Test-Path -Path $extraFile -PathType Leaf)) {
            continue
        }

        $resolvedExtraFile = (Resolve-Path -Path $extraFile).Path
        Add-FileToArchive `
            -Archive $archive `
            -SourcePath $resolvedExtraFile `
            -EntryPath (Join-Path -Path $PackageRoot -ChildPath ([System.IO.Path]::GetFileName($resolvedExtraFile)))
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
