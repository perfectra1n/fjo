<#
.SYNOPSIS
    Install fjo from a published GitHub release.

.DESCRIPTION
    irm https://raw.githubusercontent.com/perfectra1n/fjo/main/install.ps1 | iex

    Knobs (set before piping):
      $env:FJO_VERSION       tag to install (default: the latest release)
      $env:FJO_INSTALL_DIR   where the binary goes
                             (default: %LOCALAPPDATA%\Programs\fjo\bin)

    Unlike install.sh, this script appends to the *user* PATH. That asymmetry is
    deliberate: on Unix there is no single well-defined place to write, so the
    shell script prints a line instead of guessing between .bashrc, .zshrc,
    .profile and friends. Windows has exactly one user PATH value, editing it is
    the platform norm (rustup and scoop both do it), and it is trivially
    reversible -- so declining to do it here would only produce
    "fjo is not recognized" the first time someone opens a new terminal.
#>

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Repo = 'perfectra1n/fjo'
$InstallDir = if ($env:FJO_INSTALL_DIR) { $env:FJO_INSTALL_DIR }
              else { Join-Path $env:LOCALAPPDATA 'Programs\fjo\bin' }

function Say { param($m) Write-Host "fjo: $m" }

# Only one Windows target is published today. Fail clearly on arm64 rather than
# handing someone an x64 binary and letting emulation decide how that goes.
$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
if ($arch -ne 'X64') {
    throw "fjo: no published Windows build for $arch. Build from source: cargo install --git https://github.com/$Repo --locked fjo"
}
$target = 'x86_64-pc-windows-msvc'

$tag = if ($env:FJO_VERSION) { $env:FJO_VERSION } else {
    (Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest").tag_name
}
if (-not $tag) { throw 'fjo: could not resolve the latest release tag' }

$stage = "fjo-$tag-$target"
$url   = "https://github.com/$Repo/releases/download/$tag/$stage.zip"

Say "installing $tag for $target"

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
    $zip = Join-Path $tmp "$stage.zip"
    Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing

    # release.yaml writes the Windows checksum with certutil, which emits the
    # bare hash with no filename -- unlike the shasum format used for tar.gz.
    try {
        $expected = (Invoke-WebRequest -Uri "$url.sha256" -UseBasicParsing).Content.Trim()
    } catch { $expected = $null }

    if ($expected) {
        $actual = (Get-FileHash -Path $zip -Algorithm SHA256).Hash
        if ($actual -ne $expected.Trim()) {
            throw "fjo: checksum mismatch`n  expected $expected`n  actual   $actual"
        }
        Say 'checksum ok'
    } else {
        Say "warning: no published checksum for $stage.zip; skipping verification"
    }

    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    $exe = Join-Path $tmp "$stage\fjo.exe"
    if (-not (Test-Path $exe)) { throw 'fjo: archive did not contain fjo.exe' }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Copy-Item -Path $exe -Destination (Join-Path $InstallDir 'fjo.exe') -Force
    Say "installed $InstallDir\fjo.exe"

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($userPath -notlike "*$InstallDir*") {
        [Environment]::SetEnvironmentVariable('Path', "$userPath;$InstallDir", 'User')
        Say "added $InstallDir to your user PATH (restart your terminal)"
    }

    # PowerShell completions are loaded from the profile, and rewriting someone's
    # profile is a bigger intrusion than appending to PATH -- so this one prints.
    $comp = Join-Path $tmp "$stage\completions\fjo.powershell"
    if (Test-Path $comp) {
        $dest = Join-Path $InstallDir 'fjo.completion.ps1'
        Copy-Item -Path $comp -Destination $dest -Force
        Say "completions written to $dest"
        Say "  to enable, add to `$PROFILE:  . '$dest'"
    }
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

Say "done -- run 'fjo auth login --host git.example.org' to get started"
