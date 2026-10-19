# made by CasualArclamp
# Builds single-file DecDSSS executables for 64-bit Windows 10/11 that need nothing
# installed: the C runtime is linked statically (no Visual C++ redistributable). The
# results go to exe\ (git-ignored), with the version from Cargo.toml in the name:
# decdsss-gui-v<version>.exe (receiver + transmitter) and decdsss-v<version>.exe
# (command line). The version is the workspace's, the one the window shows top left.
#
#   powershell -ExecutionPolicy Bypass -File scripts\build-portable.ps1
#
# Builds in target\portable, so the usual target directory and its cache stay as they
# are. -Root builds another checkout (a git worktree of a commit, say) and -Out puts
# the executables somewhere other than <root>\exe.
param(
    [string]$Root = (Split-Path -Parent $PSScriptRoot),
    [string]$Out = ""
)

$ErrorActionPreference = "Stop"
$triple = "x86_64-pc-windows-msvc"
if ($Out -eq "") { $Out = Join-Path $Root "exe" }

# With an explicit --target the flags reach only the executables (and the C code that cc
# builds for them: Codec2), not build scripts and proc macros.
$env:RUSTFLAGS = "-C target-feature=+crt-static"
$env:CARGO_TARGET_DIR = Join-Path $Root "target\portable"

# The workspace version: the first `version = "..."` in the root Cargo.toml.
$versionLine = Select-String -Path (Join-Path $Root "Cargo.toml") -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1
if (-not $versionLine) { throw "no version in Cargo.toml" }
$version = $versionLine.Matches[0].Groups[1].Value

Push-Location $Root
try {
    cargo build --release --target $triple -p decdsss-gui -p decdsss-cli
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    New-Item -ItemType Directory -Force $Out | Out-Null
    foreach ($name in "decdsss-gui", "decdsss") {
        $built = Join-Path $env:CARGO_TARGET_DIR "$triple\release\$name.exe"
        if (Select-String -Path $built -Pattern "vcruntime140", "api-ms-win-crt" -SimpleMatch -Quiet) {
            throw "$name.exe imports a C runtime DLL: not a static build"
        }
        $target = "$name-v$version.exe"
        Copy-Item $built (Join-Path $Out $target) -Force
        # The name used before the version was added would now be a stale copy.
        Remove-Item (Join-Path $Out "$name.exe") -Force -ErrorAction SilentlyContinue
        $mb = (Get-Item (Join-Path $Out $target)).Length / 1MB
        Write-Host ("{0,-28} {1,6:N1} MB" -f $target, $mb)
    }
    Write-Host "written to $Out"
} finally {
    Pop-Location
}
