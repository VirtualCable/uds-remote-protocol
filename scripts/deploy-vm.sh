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
# Keep target-windows (the cargo cache mounted as c:\out): only the sources
# are refreshed, so rebuilds are incremental.
if (Test-Path '$EXPORT_DIR') {
    Get-ChildItem '$EXPORT_DIR' -Exclude 'target-windows' | Remove-Item -Recurse -Force
}
New-Item -ItemType Directory -Force '$EXPORT_DIR' | Out-Null
tar -xf C:\Users\builder\udsrp-master.tar -C '$EXPORT_DIR'
New-Item -ItemType Directory -Force '$EXPORT_DIR\target-windows' | Out-Null
docker run --rm -v ${EXPORT_DIR}:c:\crate -v ${EXPORT_DIR}\target-windows:c:\out -w /crate $IMAGE cargo build --release -p service -p server -p udscredprov -p vcam --features '$FEATURES'
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
Copy-Item C:\Users\dkmaster\udscredprov.dll $bin -Force
Copy-Item C:\Users\dkmaster\vcam.dll $bin -Force
# Register the "UDS Webcam" DirectShow source filter (DllRegisterServer
# writes HKCR\CLSID + the video-input-category instance entry). regsvr32
# leaves $LASTEXITCODE unset under ssh (false failure), so we P/Invoke the
# export directly.
$regSrc = @'
using System;
using System.Runtime.InteropServices;
public static class VcamReg {
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    public delegate int DllRegDelegate();
    [DllImport("kernel32", SetLastError=true, CharSet=CharSet.Unicode)]
    public static extern IntPtr LoadLibrary(string path);
    [DllImport("kernel32", SetLastError=true, CharSet=CharSet.Ansi)]
    public static extern IntPtr GetProcAddress(IntPtr h, string name);
    public static int Call(string path) {
        IntPtr h = LoadLibrary(path);
        if (h == IntPtr.Zero) return -1;
        IntPtr p = GetProcAddress(h, "DllRegisterServer");
        if (p == IntPtr.Zero) return -2;
        return ((DllRegDelegate)Marshal.GetDelegateForFunctionPointer(p, typeof(DllRegDelegate)))();
    }
}
'@
Add-Type -TypeDefinition $regSrc
$hr = [VcamReg]::Call("$bin\vcam.dll")
if ($hr -ne 0) { throw ("vcam DllRegisterServer failed: 0x{0:X8}" -f $hr) }
# The credential provider COM registration must point at the deployed DLL.
# (Lesson of 2026-11-03: the registration was left pointing at the old
# in-tree debug build; when that tree was cleaned, LogonUI silently stopped
# loading the provider and auto-unlock died with no error anywhere.)
$clsid = '{effe8c95-8e24-4d7f-bc4a-2c377479cf32}'
$dllPath = "$bin\udscredprov.dll"
$clsidKey = "HKLM:\SOFTWARE\Classes\CLSID\$clsid"
New-Item -Force $clsidKey | Out-Null
Set-ItemProperty $clsidKey -Name '(default)' -Value 'UDSRemoteProtocolCredentialProvider'
$inproc = "$clsidKey\InprocServer32"
New-Item -Force $inproc | Out-Null
Set-ItemProperty $inproc -Name '(default)' -Value $dllPath
Set-ItemProperty $inproc -Name 'ThreadingModel' -Value 'Apartment'
$cpKey = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Authentication\Credential Providers\$clsid"
New-Item -Force $cpKey | Out-Null
Set-ItemProperty $cpKey -Name '(default)' -Value 'UDSRemoteProtocolCredentialProvider'
$fltKey = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Authentication\Credential Provider Filters\$clsid"
New-Item -Force $fltKey | Out-Null
Set-ItemProperty $fltKey -Name '(default)' -Value 'UDSRemoteProtocolCredentialProvider'
# The agent binds its UDP detection socket on an ephemeral port, so the
# firewall must allow the binary itself, not just port 14433 (lesson of
# 2026-11-03: moving the exe path silently broke UDP until the rule was
# re-pointed).
$fw = Get-NetFirewallRule -DisplayName 'udsrp-server-exe' -ErrorAction SilentlyContinue
if ($fw) {
    Set-NetFirewallRule -DisplayName 'udsrp-server-exe' -Program "$bin\server.exe"
} else {
    New-NetFirewallRule -DisplayName 'udsrp-server-exe' -Direction Inbound -Action Allow -Program "$bin\server.exe" | Out-Null
}
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
scp -q "$BUILDER:C:/Users/builder/udsrp-export/target-windows/release/udscredprov.dll" /tmp/udscredprov.dll
scp -q "$BUILDER:C:/Users/builder/udsrp-export/target-windows/release/vcam.dll" /tmp/vcam.dll

echo "== 3/4 upload to $VM_HOST"
scp -q /tmp/udsrp-service.exe /tmp/uds-rp-server-agent.exe /tmp/udscredprov.dll /tmp/vcam.dll "$VM_HOST:C:/Users/dkmaster/"

echo "== 4/4 swap binaries + restart"
scp -q "$VM_PS1" "$VM_HOST:C:/Users/dkmaster/udsrp-install.ps1"
ssh "$VM_HOST" 'powershell -ExecutionPolicy Bypass -File C:\Users\dkmaster\udsrp-install.ps1'

echo "DEPLOY-OK"
