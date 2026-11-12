# BSD 3-Clause License
# Copyright (c) 2026, Virtual Cable S.L.
# All rights reserved.
# Authors: Adolfo Gómez, dkmaster at dkmon dot com

# Builds the Windows binaries of the workspace inside the uds-rp-builder
# container. Run from building/windows/:
#
#   .\build.ps1                        # everything: all packages + drivers
#   .\build.ps1 -SkipDrivers           # all packages, no drivers
#   .\build.ps1 -Package server        # a single workspace package
#   .\build.ps1 -Profile dev           # cargo profile (default: release)
#   .\build.ps1 -Jobs 8                # cargo parallelism (default: 2)
#
# "Everything" means the full binary set: server, service, udscredprov, vcam,
# labclient (testclient + uds-rp-client) and labtools, built with the full
# feature set (windows-capture, vp9, vp8, jpeg, ladder, codec-mf, codec-nvenc),
# plus the drivers (udsaudio, udsscard) via build-drivers.ps1. Nothing is
# signed with a real certificate here: executables ship unsigned and drivers
# get cargo-wdk's auto-generated WDRLocalTestCert unless -CertPassword is
# given (pinned lab cert, see build-drivers.ps1). Pick what you need from
# target-windows/ afterwards.
#
# Why -Jobs defaults to 2: on modest build hosts, several parallel rustc
# instances compiling the big crates (windows, image, nokhwa, wgpu) die with
# 0xc0000409 (STATUS_STACK_BUFFER_OVERRUN). -j1 always works but is slow; -j2
# is the safe default. Raise it on bigger machines.
#
# Volumes: c:\out is the NAMED volume udsrp-target (persistent cargo cache —
# do NOT delete it, a cold rebuild takes ~1h). Bind-mounting a host dir as
# the cargo target breaks rustc (it cannot mmap fresh rmeta over Windows bind
# mounts — doc/plan/instalacion-server-windows.md §1.2). After the build, the
# .exe/.dll/.pdb artifacts are robocopied out of the volume into the repo's
# target-windows\release\ (plain file writes over the bind mount are fine).
# The repo is mounted at c:\crate.

param(
    [string]$Package = "",
    [string]$Profile = "release",
    [string]$Image = "uds-rp-builder",
    [string]$CertPassword = "",
    [int]$Jobs = 2,
    [string]$TargetVolume = "udsrp-target",
    [switch]$SkipDrivers
)

$ErrorActionPreference = "Continue"  # cargo/docker write progress to stderr

# Ensure the builder image exists (build it the first time).
docker image inspect $Image | Out-Null
if ($LASTEXITCODE -ne 0) {
    docker build -t $Image .
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

$projectDir = Convert-Path ../..
New-Item -ItemType Directory -Force "$projectDir/target-windows" | Out-Null

$mounts = @(
    "-v", "${projectDir}:c:\crate",
    "-v", "${TargetVolume}:c:\out"
)

if ($Package -ne "") {
    # Single-package mode: plain cargo build of that package.
    docker run --rm -e CARGO_BUILD_JOBS=$Jobs @mounts -w /crate $Image `
        cargo build --profile $Profile -p $Package
    exit $LASTEXITCODE
}

# Full build: every binary package with the full feature set. server needs
# windows-capture (else secure_pipe.rs has no implementation) and the codec
# presets; labtools gets the probe tools.
$features = @(
    "server/windows-capture",
    "server/vp9",
    "server/vp8",
    "server/jpeg",
    "server/ladder",
    "server/codec-mf",
    "server/codec-nvenc",
    "labtools/mf-probe",
    "labtools/nvenc-probe"
) -join ","

docker run --rm -e CARGO_BUILD_JOBS=$Jobs @mounts -w /crate $Image `
    cargo build --profile $Profile `
        -p server -p service -p udscredprov -p vcam -p labclient -p labtools `
        --features $features
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Copy the top-level release artifacts (.exe/.dll/.pdb, no deps/ or build/
# subtrees) out of the cache volume into the repo's target-windows\release\.
# (Explicit exit 0: robocopy's "files copied" code 1 would otherwise leak as
# the container exit code and abort the script before the drivers step.)
docker run --rm @mounts -w /crate $Image `
    powershell -NoProfile -Command `
        "New-Item -ItemType Directory -Force c:\crate\target-windows\release | Out-Null; `
         robocopy c:\out\release c:\crate\target-windows\release *.exe *.dll *.pdb /NFL /NDL /NJH /NJS /NP; `
         if (`$LASTEXITCODE -gt 7) { exit `$LASTEXITCODE }; exit 0"
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

if ($SkipDrivers) { exit 0 }

# Drivers: build-drivers.ps1 runs inside the same image but needs
# container-local scratch (see that script's header); the named volume
# udsrp-drivers keeps driver rebuilds incremental. Staging lands directly in
# the repo mount's target-windows\drivers\. With no -CertPassword it uses
# cargo-wdk's default test signing (WDRLocalTestCert).
$drvArgs = @("-Profile", $Profile)
if ($CertPassword -ne "") { $drvArgs += @("-CertPassword", $CertPassword) }

docker run --rm `
    -v "${projectDir}:c:\crate" `
    -v "udsrp-drivers:c:\drv" `
    -w /crate $Image `
    powershell -NoProfile -ExecutionPolicy Bypass `
        -File c:\crate\building\windows\build-drivers.ps1 @drvArgs
exit $LASTEXITCODE
