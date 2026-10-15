# Builds uds-remote-protocol Windows artifacts inside the udsrp-builder image.
# To be executed on building/windows/ directory.
#
#   .\build.ps1                     # server (release, windows-capture+vp9+ladder)
#   $env:UDS_CARGO_ARGS='build --release -p client'; .\build.ps1
#   $env:UDS_BUILD_DRIVERS='1'; .\build.ps1   # also builds drivers/ (WDK)
#
# The repository root is mounted at C:\crate inside the container; build
# outputs land in the usual target/ (and drivers/target/) directories.

# Ensure the builder image is up to date
docker build -t udsrp-builder .

# Full path of the ../.. directory (i.e., the root of the project)
$projectDir = Convert-Path ../..

# What to build: overridable through UDS_CARGO_ARGS
$cargoArgs = @("build", "--release", "-p", "server", "--features", "windows-capture,vp9,ladder")
if ($env:UDS_CARGO_ARGS) { $cargoArgs = $env:UDS_CARGO_ARGS.Split(' ') }

docker run --rm -v ${projectDir}:c:\crate -w /crate udsrp-builder cargo @cargoArgs

if ($env:UDS_BUILD_DRIVERS -eq '1') {
    # drivers/ is a separate workspace (panic=abort, no_std, WDK), excluded
    # from the main one; cargo-wdk drives the WDK build, INF stamping and
    # test-certificate packaging.
    docker run --rm -v ${projectDir}:c:\crate -w /crate\drivers udsrp-builder cargo wdk build
}
