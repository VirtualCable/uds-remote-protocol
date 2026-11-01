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

# Build outputs must not mix with the host's target/ (Linux): a fixed
# in-image dir that build.ps1 bind-mounts back out as target-windows/.
# (Forward slashes: the Dockerfile parser eats the backslash in c:\out.)
ENV CARGO_TARGET_DIR=c:/out
RUN mkdir C:\out
