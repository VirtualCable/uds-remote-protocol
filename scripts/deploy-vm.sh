#!/usr/bin/env bash
# BSD 3-Clause License
# Copyright (c) 2026, Virtual Cable S.L.
# All rights reserved.
# Authors: Adolfo Gómez, dkmaster at dkmon dot com

# Deploys the Windows service+agent to the lab VM using the Windows builder
# (win22-build) for compilation — the lab VM no longer builds anything.
#
# Usage:
#   scripts/deploy-vm.sh                 # build current master + deploy
#   VM_HOST=other scripts/deploy-vm.sh   # against another VM
#
# Flow: git archive master → builder (docker cargo build) → scp the exes to
# the VM → stop service, drop binaries into the service's configured
# directory, start service. The lab VM keeps only %USERPROFILE%\uds-rp-bin.
# Remote steps run as .ps1 files (inline quoting over ssh is unreliable).
set -euo pipefail

VM_HOST="${VM_HOST:-dkmaster@172.27.1.47}"
BUILDER="${BUILDER:-builder@win22-build}"
EXPORT_DIR='C:\Users\builder\udsrp-export'
IMAGE="${IMAGE:-uds-rp-builder}"
FEATURES='server/windows-capture,server/vp9,server/vp8,server/jpeg,server/ladder'

BUILD_PS1=/tmp/udsrp-build.ps1
VM_PS1=/tmp/udsrp-install.ps1

cat > "$BUILD_PS1" <<EOF
\$ErrorActionPreference = 'Stop'
if (Test-Path '$EXPORT_DIR') { Remove-Item -Recurse -Force '$EXPORT_DIR' }
New-Item -ItemType Directory '$EXPORT_DIR' | Out-Null
tar -xf C:\Users\builder\udsrp-master.tar -C '$EXPORT_DIR'
New-Item -ItemType Directory -Force '$EXPORT_DIR\target-windows' | Out-Null
docker run --rm -v ${EXPORT_DIR}:c:\crate -v ${EXPORT_DIR}\target-windows:c:\out -w /crate $IMAGE cargo build --release -p service -p server --features '$FEATURES'
if (\$LASTEXITCODE -ne 0) { exit \$LASTEXITCODE }
EOF

cat > "$VM_PS1" <<'EOF'
$ErrorActionPreference = 'Stop'
$bin = 'C:\Users\dkmaster\uds-rp-bin'
New-Item -ItemType Directory -Force $bin | Out-Null
$svc = Get-ItemProperty HKLM:\SYSTEM\CurrentControlSet\Services\UdsRpService
if ($svc.ImagePath -ne "$bin\udsrp-service.exe") {
    Set-ItemProperty HKLM:\SYSTEM\CurrentControlSet\Services\UdsRpService -Name ImagePath -Value "$bin\udsrp-service.exe"
}
Stop-Service UdsRpService -Force
Copy-Item C:\Users\dkmaster\udsrp-service.exe $bin -Force
Copy-Item C:\Users\dkmaster\uds-rp-server-agent.exe $bin\server.exe -Force
Start-Service UdsRpService
Start-Sleep 2
(Get-Service UdsRpService).Status
EOF

echo "== 1/4 archive + build on $BUILDER"
git archive master -o /tmp/udsrp-master.tar
scp -q /tmp/udsrp-master.tar "$BUILDER:C:/Users/builder/udsrp-master.tar"
scp -q "$BUILD_PS1" "$BUILDER:C:/Users/builder/udsrp-build.ps1"
ssh "$BUILDER" 'powershell -ExecutionPolicy Bypass -File C:\Users\builder\udsrp-build.ps1'

echo "== 2/4 fetch binaries"
scp -q "$BUILDER:C:/Users/builder/udsrp-export/target-windows/release/udsrp-service.exe" /tmp/udsrp-service.exe
scp -q "$BUILDER:C:/Users/builder/udsrp-export/target-windows/release/server.exe" /tmp/uds-rp-server-agent.exe

echo "== 3/4 upload to $VM_HOST"
scp -q /tmp/udsrp-service.exe /tmp/uds-rp-server-agent.exe "$VM_HOST:C:/Users/dkmaster/"

echo "== 4/4 swap binaries + restart"
scp -q "$VM_PS1" "$VM_HOST:C:/Users/dkmaster/udsrp-install.ps1"
ssh "$VM_HOST" 'powershell -ExecutionPolicy Bypass -File C:\Users\dkmaster\udsrp-install.ps1'

echo "DEPLOY-OK"
