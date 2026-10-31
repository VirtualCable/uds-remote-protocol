//! Build script: provides Cargo with the WDK build/link configuration.

fn main() -> Result<(), wdk_build::ConfigError> {
    wdk_build::configure_wdk_binary_build()
}
