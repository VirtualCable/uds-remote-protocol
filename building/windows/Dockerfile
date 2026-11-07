# BSD 3-Clause License
# Copyright (c) 2026, Virtual Cable S.L.
# All rights reserved.
# Authors: Adolfo Gómez, dkmaster at dkmon dot com

# UDS Remote Protocol — Windows builder image.
# Windows Server Core with the full native toolchain the workspace needs:
# VS Build Tools (MSVC linker), cmake+ninja (opus/turbojpeg C builds),
# LLVM (libclang for bindgen consumers), NASM (assembly in C deps),
# Rust stable (msvc), and vcpkg with a static libvpx (crates/vpx-sys).
FROM mcr.microsoft.com/windows/servercore:ltsc2022
SHELL ["powershell","-NoProfile","-Command"]

# Chocolatey for dependency management
RUN Set-ExecutionPolicy Bypass -Scope Process -Force; \
    [System.Net.ServicePointManager]::SecurityProtocol = 'Tls12'; \
    iex ((New-Object System.Net.WebClient).DownloadString('https://chocolatey.org/install.ps1'))

# Git, cmake, ninja, LLVM, NASM
RUN choco install -y git ; \
    choco install -y cmake --installargs 'ADD_CMAKE_TO_PATH=System' ; \
    choco install -y ninja ; \
    choco install -y llvm ; \
    choco install -y nasm

# Visual Studio Build Tools 2022 (MSVC toolchain)
RUN choco install -y visualstudio2022buildtools \
    --package-parameters "'--installPath C:\BuildTools --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended --quiet --wait --norestart'"

# Environment: Rust, vcpkg, LLVM; cmake 4 rejects the old
# cmake_minimum_required of some C deps without the policy floor; crt-static
# keeps the produced binaries self-contained (no VC++ redist needed).
RUN $old=[Environment]::GetEnvironmentVariable('PATH','Machine'); \
    $new=$old+';C:\rust\cargo\bin;C:\vcpkg;C:\Program Files\LLVM\bin'; \
    [Environment]::SetEnvironmentVariable('PATH',$new,'Machine'); \
    [Environment]::SetEnvironmentVariable('CARGO_HOME','C:\rust\cargo','Machine'); \
    [Environment]::SetEnvironmentVariable('RUSTUP_HOME','C:\rust\rustup','Machine'); \
    [Environment]::SetEnvironmentVariable('VCPKG_ROOT','C:\vcpkg','Machine'); \
    [Environment]::SetEnvironmentVariable('LIBCLANG_PATH','C:\Program Files\LLVM\bin','Machine'); \
    [Environment]::SetEnvironmentVariable('CMAKE_POLICY_VERSION_MINIMUM','3.5','Machine'); \
    [Environment]::SetEnvironmentVariable('RUSTFLAGS','-C target-feature=+crt-static','Machine');

# Rust (stable, MSVC)
RUN Invoke-WebRequest -Uri https://win.rustup.rs/x86_64 -OutFile rustup-init.exe; \
    Start-Process -FilePath .\rustup-init.exe -ArgumentList "'-y --default-toolchain stable-x86_64-pc-windows-msvc'" -Wait; \
    Remove-Item rustup-init.exe;

# vcpkg + libvpx. The plain `x64-windows-static` triplet (/MT): vcpkg-rs
# picks it automatically because RUSTFLAGS sets crt-static.
RUN git clone https://github.com/microsoft/vcpkg $env:VCPKG_ROOT ; \
    & "$env:VCPKG_ROOT\bootstrap-vcpkg.bat" ; \
    C:\vcpkg\vcpkg install libvpx:x64-windows-static

# WDK 26100 for the drivers/ workspace (udsaudio WDM/PortCls, udsscard UMDF):
# kernel-mode headers+libs (km/, portcls.lib), WDF headers/stubs and the
# driver build tools (stampinf, inf2cat, infverif), none of which ship with
# the VS-bundled SDK. The official wdksetup.exe is a Burn web bootstrapper
# that is unreliable inside containers, so the official NuGet package
# (Microsoft.Windows.WDK.x64, same "Windows Kits\10" layout) is merged over
# the installed SDK tree. The version MUST match the highest SDK present
# (10.0.26100.0): wdk-build only accepts km/ content from the newest kit
# (doc/plan/driver-audio-windows.md, trap M0). robocopy exit codes 0-7 are
# success, so the code is captured before cleanup and only real errors fail.
RUN Invoke-WebRequest -Uri 'https://api.nuget.org/v3-flatcontainer/microsoft.windows.wdk.x64/10.0.26100.6584/microsoft.windows.wdk.x64.10.0.26100.6584.nupkg' -OutFile C:\wdk-nuget.zip ; \
    Expand-Archive C:\wdk-nuget.zip C:\wdk-nuget ; \
    robocopy C:\wdk-nuget\c 'C:\Program Files (x86)\Windows Kits\10' /E /NFL /NDL /NJH /NJS /NP ; \
    $rc=$LASTEXITCODE ; \
    Remove-Item C:\wdk-nuget.zip -Force ; \
    Remove-Item C:\wdk-nuget -Recurse -Force ; \
    if ($rc -gt 7) { exit $rc } ; \
    if (-not (Test-Path 'C:\Program Files (x86)\Windows Kits\10\Include\10.0.26100.0\km\portcls.h')) { exit 1 }

# LLVM 17.0.6 libclang side-by-side (C:\LLVM17) for the driver builds ONLY:
# the WDK bindings (wdk-sys + our bindgen pass over portcls.h) generate wrong
# struct layouts with the LLVM 22 that choco installs (const size asserts
# fail at compile time); LLVM 17 is the validated bindgen toolchain for
# windows-drivers-rs (doc/plan/driver-audio-windows.md). Only libclang.dll is
# needed (bindgen loads it directly). The official NSIS installer hangs in
# containers, so 7-Zip (which reads NSIS) extracts just that DLL. curl.exe
# because Invoke-WebRequest is pathologically slow in container builds.
# The main crate build keeps using the newer LLVM from PATH;
# build-drivers.ps1 points LIBCLANG_PATH here per-process.
RUN choco install -y 7zip ; \
    curl.exe -L -sS -o C:\llvm17.exe 'https://github.com/llvm/llvm-project/releases/download/llvmorg-17.0.6/LLVM-17.0.6-win64.exe' ; \
    & 'C:\Program Files\7-Zip\7z.exe' x C:\llvm17.exe -oC:\llvm17-tmp 'bin\libclang.dll' -y | Out-Null ; \
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE } ; \
    New-Item -ItemType Directory -Force C:\LLVM17\bin | Out-Null ; \
    Copy-Item C:\llvm17-tmp\bin\libclang.dll C:\LLVM17\bin\ ; \
    Remove-Item C:\llvm17.exe -Force ; \
    Remove-Item C:\llvm17-tmp -Recurse -Force

# cargo-wdk: `cargo wdk build` compiles, packages (stampinf/inf2cat/infverif)
# and test-signs drivers in one step.
RUN cargo install cargo-wdk --locked

# Build outputs must not mix with the host's target/ (Linux): a fixed
# in-image dir that build.ps1 bind-mounts back out as target-windows/.
# (Forward slashes: the Dockerfile parser eats the backslash in c:\out.)
ENV CARGO_TARGET_DIR=c:/out
RUN mkdir C:\out
