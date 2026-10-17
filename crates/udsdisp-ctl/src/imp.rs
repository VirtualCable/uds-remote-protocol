//! CLI layer of udsdisp-ctl: argument parsing and printing. All the driver
//! communication lives in `udsdisp_client::client`.

use anyhow::Context as _;
use udsdisp_client::client::{Client, Mode};
use udsdisp::logic;

/// CLI entry.
pub fn run() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map_or("help", String::as_str);
    match cmd {
        "list" => list(),
        "create" => {
            let (connector, mode) = parse_connector_mode(&args[1..])?;
            create(connector, mode)
        }
        "set-mode" => {
            let (connector, mode) = parse_connector_mode(&args[1..])?;
            set_mode(connector, mode)
        }
        "remove" => {
            let connector = parse_connector(&args[1..])?;
            let client = Client::open()?;
            client.remove(connector)?;
            println!("monitor on connector {connector} removed");
            Ok(())
        }
        "cursor" => {
            let connector = parse_connector(&args[1..])?;
            cursor(connector)
        }
        _ => {
            eprintln!(
                "usage: udsdisp-ctl <list | create | set-mode | remove | cursor> [connector] [WxH[@HZ]]"
            );
            std::process::exit(64);
        }
    }
}

/// Parses `<connector> <WxH[@HZ]>`.
fn parse_connector_mode(args: &[String]) -> anyhow::Result<(u32, Mode)> {
    let connector = parse_connector(args)?;
    let mode_text = args.get(1).context("missing mode (WxH[@HZ])")?;
    let mode = logic::parse_mode(mode_text)
        .with_context(|| format!("invalid mode '{mode_text}' (expected WxH[@HZ])"))?;
    anyhow::ensure!(mode.is_supported(), "mode {mode:?} is not in the driver target modes");
    Ok((connector, mode))
}

/// Parses `<connector>`.
fn parse_connector(args: &[String]) -> anyhow::Result<u32> {
    let connector: u32 = args
        .first()
        .context("missing connector index")?
        .parse()
        .context("connector must be a number (0-3)")?;
    anyhow::ensure!(connector < logic::MAX_MONITORS, "connector must be 0-{}", logic::MAX_MONITORS - 1);
    Ok(connector)
}

/// `list`: dump every connector slot.
fn list() -> anyhow::Result<()> {
    let client = Client::open()?;
    for status in &client.list()? {
        if status.connected {
            let m = &status.mode;
            println!(
                "connector {}: CONNECTED {}x{}@{}Hz",
                status.connector_index, m.width, m.height, m.vsync
            );
        } else {
            println!("connector {}: -", status.connector_index);
        }
    }
    Ok(())
}

/// `create`: hot-plug a monitor with the given preferred mode.
fn create(connector: u32, mode: Mode) -> anyhow::Result<()> {
    let client = Client::open()?;
    client.create(connector, mode)?;
    println!(
        "monitor created on connector {connector} ({}x{}@{}Hz preferred)",
        mode.width, mode.height, mode.vsync
    );
    println!(
        "note: if the desktop does not extend onto it automatically, run \
         `displayswitch /extend` from the interactive session"
    );
    Ok(())
}

/// `set-mode`: push the new preferred mode into the driver and then
/// best-effort apply it to the desktop (the latter only succeeds in the
/// interactive console session).
fn set_mode(connector: u32, mode: Mode) -> anyhow::Result<()> {
    let client = Client::open()?;
    client.set_mode(connector, mode)?;
    println!(
        "driver accepted mode {}x{}@{}Hz for connector {connector}",
        mode.width, mode.height, mode.vsync
    );
    match client.apply_desktop_mode(connector, mode) {
        Ok(()) => println!("desktop switched to the new mode"),
        Err(e) => println!("note: desktop switch failed ({e:#}); the new mode is still offered to the OS"),
    }
    Ok(())
}

/// `cursor`: dump the driver-tracked hardware cursor state of a connector.
fn cursor(connector: u32) -> anyhow::Result<()> {
    let client = Client::open()?;
    let status = client.cursor(connector)?;
    if !status.known {
        println!("connector {connector}: no cursor data yet (no commit with an active path?)");
        return Ok(());
    }
    println!(
        "connector {connector}: visible={} pos=({},{}) shape_id={} shape={}x{} pitch={} type={} ({})",
        status.visible,
        status.x,
        status.y,
        status.shape_id,
        status.width,
        status.height,
        status.pitch,
        status.shape_type,
        match status.shape_type {
            1 => "masked color",
            2 => "alpha",
            _ => "unknown",
        },
    );
    Ok(())
}
