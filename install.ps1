# Lomora installer for Windows x86_64.
#
#   irm https://raw.githubusercontent.com/marcuz-apl/lomora/master/install.ps1 | iex
#
# `irm | iex` cannot pass arguments, so use environment variables first:
#   $env:LOMORA_VERSION = 'v0.1.11'      # pin a release; default: latest
#   $env:LOMORA_INSTALL_DIR = 'C:\Tools' # default: %LOCALAPPDATA%\lomora\bin
#   $env:LOMORA_SKIP_VERIFY = '1'       # skip the SHA-256 check
#   $env:LOMORA_RELEASE_BASE = 'https://mirror/...' # fetch from a mirror
#
# No admin rights are needed: the binary goes into the user profile and the
# directory is added to the *user* PATH.

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo = 'marcuz-apl/lomora'
$InstallDir = if ($env:LOMORA_INSTALL_DIR) {
    $env:LOMORA_INSTALL_DIR
} else {
    Join-Path $env:LOCALAPPDATA 'lomora\bin'
}

if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64' -or $env:PROCESSOR_ARCHITEW6432 -eq 'ARM64' -or
    $env:PROCESSOR_ARCHITECTURE -eq 'x86') {
    throw 'Only Windows x86_64 builds are published. Build from source with: cargo build --release'
}

if ($env:LOMORA_VERSION) {
    $Tag = if ($env:LOMORA_VERSION.StartsWith('v')) { $env:LOMORA_VERSION } else { "v$($env:LOMORA_VERSION)" }
} else {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" `
        -Headers @{ 'User-Agent' = 'lomora-installer' }
    $Tag = $release.tag_name
}
if (-not $Tag) { throw 'Could not determine a release tag; set $env:LOMORA_VERSION and retry.' }

$Asset = 'lomora-windows-x86_64.exe'
$Base = if ($env:LOMORA_RELEASE_BASE) {
    $env:LOMORA_RELEASE_BASE.TrimEnd('/')
} else {
    "https://github.com/$Repo/releases/download/$Tag"
}
$Tmp = Join-Path ([IO.Path]::GetTempPath()) ('lomora-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $Tmp | Out-Null

try {
    $Downloaded = Join-Path $Tmp $Asset
    Write-Host "Downloading $Asset ($Tag)..."
    Invoke-WebRequest -Uri "$Base/$Asset" -OutFile $Downloaded -UseBasicParsing

    # Older releases have no SHA256SUMS; verification then degrades to HTTPS only.
    $Sums = Join-Path $Tmp 'SHA256SUMS'
    try {
        Invoke-WebRequest -Uri "$Base/SHA256SUMS" -OutFile $Sums -UseBasicParsing -ErrorAction Stop
    } catch {
        $Sums = $null
    }

    if ($env:LOMORA_SKIP_VERIFY -eq '1') {
        Write-Host '  checksum verification skipped (LOMORA_SKIP_VERIFY=1)'
    } elseif (-not $Sums) {
        Write-Warning 'No SHA256SUMS in this release; verified by HTTPS only.'
    } else {
        $Line = Get-Content $Sums | Where-Object {
            $_.TrimEnd().EndsWith($Asset) -or $_.TrimEnd().EndsWith("*$Asset")
        } | Select-Object -First 1
        if (-not $Line) { throw "$Asset is not listed in SHA256SUMS" }
        $Expected = ($Line -split '\s+')[0].ToLowerInvariant()
        $Actual = (Get-FileHash -Algorithm SHA256 $Downloaded).Hash.ToLowerInvariant()
        if ($Actual -ne $Expected) {
            throw "Checksum mismatch for $Asset (expected $Expected, got $Actual)"
        }
        Write-Host '  checksum OK'
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $Dest = Join-Path $InstallDir 'lomora.exe'
    Copy-Item -Force $Downloaded $Dest

    $UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $UserPath) { $UserPath = '' }
    if ($UserPath -notlike "*$InstallDir*") {
        [Environment]::SetEnvironmentVariable('Path', ($UserPath.TrimEnd(';') + ';' + $InstallDir), 'User')
        $env:Path = $env:Path + ';' + $InstallDir
        $PathChanged = $true
    } else {
        $PathChanged = $false
    }

    Write-Host ''
    Write-Host "Installed lomora $Tag -> $Dest"
    if ($PathChanged) {
        Write-Host 'Added that folder to your user PATH; open a NEW terminal for it to take effect.'
    }
    Write-Host ''
    Write-Host 'Run it from a folder of its own - the database is created at .\data\lomora.db:'
    Write-Host '    mkdir ~\lomora; cd ~\lomora'
    Write-Host '    lomora             # then open http://localhost:8501'
    Write-Host ''
    Write-Host 'Other options: lomora --help, $env:PORT = 9000; lomora'
    Write-Host 'The binary is unsigned. Terminal installs normally avoid the SmartScreen prompt;'
    Write-Host 'a copy downloaded with a browser will warn once (More info -> Run anyway).'
} finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}
