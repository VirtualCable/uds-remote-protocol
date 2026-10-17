//! Minimal EDID block generation for the virtual monitor.
//!
//! A monitor that reports no EDID shows up in Windows as a "Generic Non-PnP
//! Monitor" with an empty GDI mode list — which both hides our identity
//! (nothing matches "UDS Virtual Display") and makes the OS refuse to attach
//! the display (`DISP_CHANGE_BADMODE` on every `ChangeDisplaySettingsExW`).
//! Reporting a small synthetic EDID fixes both: the monitor gets a proper
//! PnP identity, a name and a mode list.
//!
//! Pure byte math, no Windows types, unit-tested on Linux.

use crate::logic::Mode;

/// Bytes of a base (1.4) EDID block.
pub const EDID_LEN: usize = 128;

/// Monitor name embedded in the EDID name descriptor (max 13 bytes, padded
/// with line feeds). This is the string Windows shows in Display Settings
/// and what `EnumDisplayDevicesW` reports for the monitor.
pub const MONITOR_NAME: &[u8; 13] = b"UDS Virtual\n ";

// Fixed geometry of the synthetic panel (a ~24" 16:9 screen).
const H_SIZE_MM: u16 = 530;
const V_SIZE_MM: u16 = 300;

// CEA-style fixed blanking/sync parameters (as in the 1920x1080@60 timing).
const H_BLANK: u16 = 280;
const V_BLANK: u16 = 45;
const H_SYNC_OFFSET: u16 = 88;
const H_SYNC_WIDTH: u16 = 44;
const V_SYNC_OFFSET: u8 = 4;
const V_SYNC_WIDTH: u8 = 5;

/// Builds a 128-byte EDID 1.4 block advertising `preferred` as the
/// detailed timing (and therefore the OS-preferred mode).
// The truncating casts are intentional: EDID fields are 8-bit with the
// high nibble packed elsewhere; the values (max 4095 px, 300 mm) fit the
// packing by construction.
#[allow(clippy::cast_possible_truncation)]
#[must_use]
pub fn edid_for_mode(preferred: Mode) -> [u8; EDID_LEN] {
    let mut edid = [0u8; EDID_LEN];

    edid[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    // Manufacturer ID "UDS": (21<<10)|(4<<5)|19, big-endian.
    edid[8..10].copy_from_slice(&0x5493u16.to_be_bytes());
    // Product code, serial, manufacture week/year.
    edid[10..12].copy_from_slice(&1u16.to_le_bytes());
    edid[12..16].copy_from_slice(&1u32.to_le_bytes());
    edid[16] = 1;
    edid[17] = 35; // 2025
    edid[18] = 1;
    edid[19] = 4; // EDID 1.4
    edid[20] = 0x80; // digital input
    edid[21] = (H_SIZE_MM / 10) as u8; // screen size in cm
    edid[22] = (V_SIZE_MM / 10) as u8;
    edid[23] = 120; // gamma 2.2
    edid[24] = 0x02; // preferred timing in the first DTD
    // Typical sRGB chromaticity coordinates.
    edid[25..35].copy_from_slice(&[0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54]);
    // Established timings: none. Standard timings: unused slots.
    for slot in edid[38..54].as_chunks_mut::<2>().0 {
        *slot = [0x01, 0x01];
    }

    write_detailed_timing(&mut edid[54..72], preferred);
    write_name_descriptor(&mut edid[72..90]);
    write_range_limits(&mut edid[90..108]);
    // 108..126: unused descriptor (all zeros), 126: no extension blocks.

    let sum: u8 = edid[..127].iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
    edid[127] = 0u8.wrapping_sub(sum);
    edid
}

/// Detailed Timing Descriptor (18 bytes): the preferred mode with fixed
/// CEA-style blanking.
// Same intentional 8-bit packing as `edid_for_mode`.
#[allow(clippy::cast_possible_truncation)]
fn write_detailed_timing(dtd: &mut [u8], mode: Mode) {
    let h_total = mode.width + u32::from(H_BLANK);
    let v_total = mode.height + u32::from(V_BLANK);
    let pixel_clock_hz = u64::from(h_total) * u64::from(v_total) * u64::from(mode.vsync);
    let clock_10khz = u16::try_from(pixel_clock_hz / 10_000).unwrap_or(u16::MAX);

    dtd[0..2].copy_from_slice(&clock_10khz.to_le_bytes());
    dtd[2] = mode.width as u8; // hactive LSB
    dtd[3] = H_BLANK as u8; // hblank LSB
    dtd[4] = (((mode.width >> 8) as u8) << 4) | ((H_BLANK >> 8) as u8);
    dtd[5] = mode.height as u8; // vactive LSB
    dtd[6] = V_BLANK as u8; // vblank LSB
    dtd[7] = (((mode.height >> 8) as u8) << 4) | ((V_BLANK >> 8) as u8);
    dtd[8] = H_SYNC_OFFSET as u8;
    dtd[9] = H_SYNC_WIDTH as u8;
    dtd[10] = V_SYNC_OFFSET | (V_SYNC_WIDTH << 4);
    dtd[11] = 0; // sync offset/width high bits (all fit in the low bytes)
    dtd[12] = H_SIZE_MM as u8;
    dtd[13] = V_SIZE_MM as u8;
    dtd[14] = (((H_SIZE_MM >> 8) as u8) << 4) | ((V_SIZE_MM >> 8) as u8);
    dtd[15] = 0; // horizontal border
    dtd[16] = 0; // vertical border
    dtd[17] = 0x1E; // digital separate sync, +hsync, +vsync, non-interlaced
}

/// Monitor Name descriptor (tag 0xFC).
fn write_name_descriptor(desc: &mut [u8]) {
    desc[3] = 0xFC;
    desc[5..18].copy_from_slice(MONITOR_NAME);
}

/// Monitor Range Limits descriptor (tag 0xFD) with generous fixed bounds.
fn write_range_limits(desc: &mut [u8]) {
    desc[3] = 0xFD;
    desc[5] = 50; // min vertical Hz
    desc[6] = 75; // max vertical Hz
    desc[7] = 30; // min horizontal kHz
    desc[8] = 83; // max horizontal kHz
    desc[9] = 17; // max pixel clock, 170 MHz (units of 10 MHz)
    desc[10] = 0; // no GTF
    desc[11] = 0x0A;
    for b in &mut desc[12..18] {
        *b = 0x20;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_makes_the_block_sum_zero() {
        for mode in [Mode::new(1920, 1080, 60), Mode::new(1280, 720, 60)] {
            let edid = edid_for_mode(mode);
            let sum: u8 = edid.iter().fold(0, |acc, b| acc.wrapping_add(*b));
            assert_eq!(sum, 0, "EDID checksum invalid for {mode:?}");
        }
    }

    #[test]
    fn detailed_timing_matches_cea_1080p60() {
        let edid = edid_for_mode(Mode::new(1920, 1080, 60));
        // 2200x1125@60 = 148.5 MHz -> 14850 units of 10 kHz.
        assert_eq!(u16::from_le_bytes([edid[54], edid[55]]), 14850);
        let dtd = &edid[54..72];
        let hactive = u32::from(dtd[2]) | (u32::from(dtd[4] >> 4) << 8);
        let vactive = u32::from(dtd[5]) | (u32::from(dtd[7] >> 4) << 8);
        assert_eq!((hactive, vactive), (1920, 1080));
    }

    #[test]
    fn name_descriptor_carries_the_monitor_name() {
        let edid = edid_for_mode(Mode::new(1920, 1080, 60));
        assert_eq!(edid[75], 0xFC);
        assert_eq!(&edid[77..90], MONITOR_NAME);
    }
}
