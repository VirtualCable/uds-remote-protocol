# uds-remote-protocol Windows builder image.
# Mirrors the model of uds-client/building/windows: Windows Server Core ltsc2022
# with the full toolchain needed to build the workspace (server, client,
# credential provider, UMDF display driver) and the kernel audio driver
# (drivers/ workspace, excluded from the main one).
#
# Build:  docker build -t udsrp-builder .
# Use:    see build.ps1

FROM mcr.microsoft.com/windows/servercore:ltsc2022
SHELL ["powershell","-NoProfile","-Command"]

# Chocolatey for dependency management
RUN Set-ExecutionPolicy Bypass -Scope Process -Force; \
    [System.Net.ServicePointManager]::SecurityProtocol = 'Tls12'; \
    iex ((New-Object System.Net.WebClient).DownloadString('https://chocolatey.org/install.ps1'))

# Base tools (LLVM provides libclang for bindgen/wdk-build)
RUN choco install -y git ; \
    choco install -y cmake --installargs 'ADD_CMAKE_TO_PATH=System' ; \
    choco install -y ninja ; \
    choco install -y llvm

# Visual Studio Build Tools 2022 (MSVC toolchain)
RUN choco install -y visualstudio2022buildtools \
    --package-parameters "'--installPath C:\BuildTools --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended --quiet --wait --norestart'"

# Environment for Rust, vcpkg, LLVM and the CMake 4.x policy floor
# (audiopus_sys' bundled libopus asks for cmake <3.5 compat)
RUN $old=[Environment]::GetEnvironmentVariable('PATH','Machine'); \
    $new=$old+';C:\rust\cargo\bin;C:\vcpkg;C:\Program Files\LLVM\bin'; \
    [Environment]::SetEnvironmentVariable('PATH',$new,'Machine'); \
    [Environment]::SetEnvironmentVariable('CARGO_HOME','C:\rust\cargo','Machine'); \
    [Environment]::SetEnvironmentVariable('RUSTUP_HOME','C:\rust\rustup','Machine'); \
    [Environment]::SetEnvironmentVariable('VCPKG_ROOT','C:\vcpkg','Machine'); \
    [Environment]::SetEnvironmentVariable('LIBCLANG_PATH','C:\Program Files\LLVM\bin','Machine'); \
    [Environment]::SetEnvironmentVariable('CMAKE_POLICY_VERSION_MINIMUM','3.5','Machine');

# Rust (MSVC toolchain)
RUN Invoke-WebRequest -Uri https://win.rustup.rs/x86_64 -OutFile rustup-init.exe; \
    Start-Process -FilePath .\rustup-init.exe -ArgumentList "'-y --default-toolchain stable-x86_64-pc-windows-msvc'" -Wait; \
    Remove-Item rustup-init.exe;

# cargo-wdk (driver build/packaging).
# Pinned to the version validated on the lab VM (win22-dev).
RUN cargo install cargo-wdk --version 0.1.1

# vcpkg + libvpx (VP8/VP9 codecs, static-md triplet as validated on the lab VM)
RUN git clone https://github.com/microsoft/vcpkg $env:VCPKG_ROOT ; \
    & "$env:VCPKG_ROOT\bootstrap-vcpkg.bat" ; \
    C:\vcpkg\vcpkg install libvpx:x64-windows-static-md

# Windows SDK + WDK 10.0.22621 (the kit must carry km/ content matching the
# highest SDK version — installing WDK on top of the same-version SDK is the
# validated combo; see doc/plan/driver-audio-windows.md traps).
# Silent full-feature installs; ~several GB download.
RUN Invoke-WebRequest -Uri https://go.microsoft.com/fwlink/?linkid=2249912 -OutFile winsdksetup.exe; \
    Start-Process -FilePath .\winsdksetup.exe -ArgumentList "'/q /features +'" -Wait; \
    Remove-Item winsdksetup.exe; \
    Invoke-WebRequest -Uri https://go.microsoft.com/fwlink/?linkid=2249371 -OutFile wdksetup.exe; \
    Start-Process -FilePath .\wdksetup.exe -ArgumentList "'/q'" -Wait; \
    Remove-Item wdksetup.exe
