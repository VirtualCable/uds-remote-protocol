# BSD 3-Clause License
# Copyright (c) 2026, Virtual Cable S.L.
# All rights reserved.
# Authors: Adolfo Gómez, dkmaster at dkmon dot com

# Builds the Windows binaries of the workspace inside the uds-rp-builder
# container. Run from building/windows/:
#
#   .\build.ps1                    # default: the lab client package
#   .\build.ps1 -Package server    # any workspace package
#
# The image mounts the repo at c:\crate and writes build outputs to
# <repo>/target-windows/ (bind-mounted over the image's c:\out), so the host
# Linux target/ is never touched and the .exe files are directly available
# under target-windows/release/.

param(
    [string]$Package = "labclient",
    [string]$Image = "uds-rp-builder"
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

docker run --rm `
    -v "${projectDir}:c:\crate" `
    -v "${projectDir}/target-windows:c:\out" `
    -w /crate $Image `
    cargo build --release -p $Package
exit $LASTEXITCODE
