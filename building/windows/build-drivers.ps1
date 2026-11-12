# BSD 3-Clause License
# Copyright (c) 2026, Virtual Cable S.L.
# All rights reserved.
# Authors: Adolfo Gómez, dkmaster at dkmon dot com

# Builds and test-signs the lab drivers INSIDE the uds-rp-builder container
# (the image carries WDK 26100 merged into the SDK tree + cargo-wdk):
#
#   drivers\          (workspace) -> udsaudio  (WDM/PortCls kernel driver)
#   drivers\udsscard  (own workspace, wdk-sys allows a single driver model
#                      per dependency graph)      (UMDF 2.23 smart card reader)
#
# Invoked by the builder host (win22-build) after the main crate build:
#
#   docker run --rm -v C:\Users\builder\udsrp-export:c:\crate `
#       -v udsrp-drivers:c:\drv `
#       -v C:\Users\builder\udsrp-labcert:c:\labcert `
#       -w /crate uds-rp-builder `
#       powershell -NoProfile -ExecutionPolicy Bypass `
#       -File c:\crate\building\windows\build-drivers.ps1 -CertPassword <pwd>
#
# Why the sources are copied to container-local scratch (c:\drv) first:
# 1. wdk-build's find_top_level_cargo_manifest() walks the OUT_DIR ancestors
#    looking for Cargo.lock, so driver builds REQUIRE the default
#    <workspace>\target dir — a redirected CARGO_TARGET_DIR (the image sets
#    c:\out for the main build) breaks the build. CARGO_TARGET_DIR is
#    therefore unset below.
# 2. The bind-mounted repo (c:\crate) cannot BE the target dir either: rustc
#    fails to mmap freshly written rmeta over Windows bind mounts (the E0786
#    trap, doc/plan/instalacion-server-windows.md §1.2). Container-local
#    scratch storage has neither problem.
# udsscard's path dependency (../../crates/udsscard-proto) inherits
# workspace.package from the main workspace, so the whole crates/ tree and
# the root manifests are mirrored alongside, preserving the relative layout.
#
# Signing: pinned LAB-ONLY self-signed cert mounted at c:\labcert
# (udsrp-labtest.pfx + udsrp-labtest.cer). Pinned so the public cert installed
# once in the lab VM keeps matching across rebuilds (cargo-wdk's default
# WDRLocalTestCert would be regenerated in every fresh container). Production
# drivers need attestation/EV signing — see doc/plan/instalacion-server-windows.md.

param(
    [string]$Profile = "release",
    [string]$CertDir = "c:\labcert",
    # Optional: pinned lab cert. When empty, cargo-wdk signs per -SignMode
    # ("test" = auto-generated WDRLocalTestCert; "off" = no signing, e.g. the
    # enterprise builder, which EV-signs afterwards itself).
    [string]$CertPassword = "",
    [string]$SignMode = "test"
)

$ErrorActionPreference = 'Stop'

$pfx = "$CertDir\udsrp-labtest.pfx"
$cer = "$CertDir\udsrp-labtest.cer"
if ($CertPassword -ne "") {
    if (-not (Test-Path $pfx)) { throw "lab signing cert not found: $pfx" }
    if (-not (Test-Path $cer)) { throw "lab signing public cert not found: $cer" }
}

# The cargo profile name maps to the target subdir (dev -> debug).
$profileDir = if ($Profile -eq 'dev') { 'debug' } else { $Profile }

Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
# bindgen over the WDK headers only produces correct layouts with LLVM 17
# (see the Dockerfile note); the image default LLVM 22 generates broken
# bindings for wdk-sys.
$env:LIBCLANG_PATH = 'C:\LLVM17\bin'
if (-not (Test-Path "$env:LIBCLANG_PATH\libclang.dll")) { throw "LLVM 17 not found at $env:LIBCLANG_PATH" }
# Refresh the sources into the (persistent) scratch tree; the target\ dirs
# are kept so driver rebuilds are incremental (robocopy /MIR minus target).
# udsscard's path dependency (../../crates/udsscard-proto) inherits
# workspace.package from the MAIN workspace root, so the whole crates/ tree
# (every listed member must parse) and the root manifests come along.
New-Item -ItemType Directory -Force c:\drv\crates | Out-Null
robocopy c:\crate\drivers c:\drv\drivers /MIR /XD target /NFL /NDL /NJH /NJS /NP | Out-Null
if ($LASTEXITCODE -gt 7) { throw "robocopy drivers failed: $LASTEXITCODE" }
robocopy c:\crate\crates c:\drv\crates /MIR /XD target /NFL /NDL /NJH /NJS /NP | Out-Null
if ($LASTEXITCODE -gt 7) { throw "robocopy crates failed: $LASTEXITCODE" }
Copy-Item c:\crate\Cargo.toml c:\drv\ -Force
Copy-Item c:\crate\Cargo.lock c:\drv\ -Force

foreach ($drv in @('c:\drv\drivers', 'c:\drv\drivers\udsscard')) {
    Push-Location $drv
    try {
        Write-Host "== cargo wdk build ($drv, profile $Profile)"
        if ($CertPassword -ne "") {
            & cargo wdk build --profile $Profile `
                --signtool-args "/fd SHA256 /f $pfx /p $CertPassword"
        } else {
            & cargo wdk build --profile $Profile --sign-mode $SignMode
        }
        if ($LASTEXITCODE -ne 0) { throw "cargo wdk build failed in $drv (exit $LASTEXITCODE)" }
    } finally { Pop-Location }
}

# Stage the packaged drivers next to the other artifacts so the builder host
# can pick them up from target-windows\drivers\<name>\.
foreach ($name in @('udsaudio', 'udsscard')) {
    $ws = if ($name -eq 'udsaudio') { 'c:\drv\drivers' } else { 'c:\drv\drivers\udsscard' }
    $pkg = Get-ChildItem "$ws\target\$profileDir" -Directory -Filter "$name*package*" -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $pkg) { throw "package dir for $name not found under $ws\target\$profileDir" }
    $dst = "c:\crate\target-windows\drivers\$name"
    New-Item -ItemType Directory -Force $dst | Out-Null
    Copy-Item "$($pkg.FullName)\*" $dst -Recurse -Force
    if ($CertPassword -ne "") { Copy-Item $cer $dst -Force }
    Write-Host "== staged $name package -> $dst"
    Get-ChildItem $dst | Select-Object -ExpandProperty Name
}
