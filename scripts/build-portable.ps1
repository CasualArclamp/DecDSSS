# made by CasualArclamp
# Builds single-file DecDSSS executables for 64-bit Windows 10/11 that need nothing
# installed: the C runtime is linked statically (no Visual C++ redistributable). The
# results go to exe\ (git-ignored): decdsss-gui.exe (receiver + transmitter) and
# decdsss.exe (command line).
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

Push-Location $Root
try {
    cargo build --release --target $triple -p decdsss-gui -p decdsss-cli
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    New-Item -ItemType Directory -Force $Out | Out-Null
    foreach ($exe in "decdsss-gui.exe", "decdsss.exe") {
        $built = Join-Path $env:CARGO_TARGET_DIR "$triple\release\$exe"
        if (Select-String -Path $built -Pattern "vcruntime140", "api-ms-win-crt" -SimpleMatch -Quiet) {
            throw "$exe imports a C runtime DLL: not a static build"
        }
        Copy-Item $built $Out -Force
        $mb = (Get-Item (Join-Path $Out $exe)).Length / 1MB
        Write-Host ("{0,-18} {1,6:N1} MB" -f $exe, $mb)
    }
    Write-Host "written to $Out"
} finally {
    Pop-Location
}
