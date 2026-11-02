#!/usr/bin/env bash
# BSD 3-Clause License
# Copyright (c) 2026, Virtual Cable S.L.
# All rights reserved.
# Authors: Adolfo Gómez, dkmaster at dkmon dot com

# Compila (y opcionalmente prueba) la parte servidora en la VM Windows.
#
# Uso:
#   scripts/build-windows.sh [--release] [--test]
#
# Flujo: la VM (win22-dev, accesible por SSH con clave pública) tiene el repo
# clonado en %USERPROFILE%\uds-remote-protocol. Este script hace git pull y
# cargo build allí; no hace falta rsync (git ya transfiere los cambios).
#
# Nota: git y cargo escriben su progreso a stderr; el wrapper SSH de
# PowerShell puede propagar eso como "error" aunque todo vaya bien. Por eso
# el exit code real se decide con $LASTEXITCODE dentro de la VM.
set -euo pipefail

VM_HOST="${VM_HOST:-win22-dev}"
PROFILE="debug"
CARGO_ARGS=""
TEST=0

for arg in "$@"; do
    case "$arg" in
        --release) PROFILE="release"; CARGO_ARGS="--release" ;;
        --test) TEST=1 ;;
        *) echo "argumento desconocido: $arg" >&2; exit 2 ;;
    esac
done

ssh -o BatchMode=yes "$VM_HOST" "
\$env:PATH = \"\$env:USERPROFILE\.cargo\bin;\$env:PATH\"
Set-Location \"\$env:USERPROFILE\uds-remote-protocol\"
git pull --ff-only 2>&1 | Out-Host
if (\$LASTEXITCODE -ne 0) { exit \$LASTEXITCODE }
cargo build --workspace $CARGO_ARGS 2>&1 | Out-Host
if (\$LASTEXITCODE -ne 0) { exit \$LASTEXITCODE }
if ($TEST -eq 1) {
    cargo test --workspace 2>&1 | Out-Host
    if (\$LASTEXITCODE -ne 0) { exit \$LASTEXITCODE }
}
"

echo "OK: build $PROFILE en $VM_HOST (binarios en target/$PROFILE de la VM)"
