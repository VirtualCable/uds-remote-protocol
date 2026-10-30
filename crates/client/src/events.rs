//! Events delivered from the protocol session to the consumer (§15.3).
//!
//! The consumer injects a [`flume::Sender<ClientEvent>`] when building the
//! [`crate::Session`]. Pixels never travel through the channel: frame
//! contents live in the shared per-display framebuffers
//! ([`crate::Displays`]) and [`ClientEvent::FrameDirty`] only carries the
//! damage rectangles to copy — the same contract as freerdp-integration's
//! `UpdateRects` + shared primary buffer.

use codec::VideoCodecType;

/// Recommended bound for the consumer-provided event channel (§15.3):
/// notifications are dropped (`try_send`), never pixels, so a small queue
/// keeps a slow consumer from piling up stale damage.
pub const EVENT_CHANNEL_CAPACITY: usize = 128;

/// One damage rectangle in display-local pixels (§15.3). Mirrors
/// freerdp-integration's `geom::Rect`: signed origin so future
/// virtual-desktop layouts with negative coordinates fit unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width in pixels.
    pub w: u32,
    /// Height in pixels.
    pub h: u32,
}

/// Protocol → consumer notifications (§15.3).
///
/// Sent with `try_send` on the consumer-provided channel: when the channel
/// is full the notification is dropped (a dropped `FrameDirty` is harmless —
/// the pixels stay in the framebuffer and the next damage rect covers the
/// whole frame today); a disconnected channel ends the session.
#[derive(Clone, PartialEq)]
pub enum ClientEvent {
    /// Handshake done and session up. `udp` is true when the lossy leg was
    /// detected (§4.2), false when everything runs on the reliable stream.
    Connected {
        /// True when the UDP leg is active.
        udp: bool,
    },
    /// A display was announced or re-announced (codec ladder, §8.5). The
    /// shared framebuffer for `display_id` exists from this point on and is
    /// resized to the announced geometry.
    DisplayInfo {
        /// Display identifier (0 = primary).
        display_id: u8,
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
        /// Codec the frames on this display are encoded with.
        codec: VideoCodecType,
    },
    /// New pixels are available in the shared framebuffer of `display_id`;
    /// the consumer copies the listed rects. The wire protocol carries no
    /// dirty-rect metadata yet (§8), so today this is always a single
    /// full-frame rect (a rect degenerating to full screen is the legitimate
    /// limit case, §15.1).
    FrameDirty {
        /// Display identifier.
        display_id: u8,
        /// Damaged rectangles in display-local pixels.
        rects: Vec<Rect>,
    },
    /// Remote cursor became visible or hidden (§10: the cursor is painted
    /// locally, never decoded from the video).
    CursorVisibility {
        /// True when the remote cursor is visible.
        visible: bool,
    },
    /// New remote cursor shape; the consumer paints it locally at the
    /// pointer position it injects (§10).
    CursorShape {
        /// Width in pixels.
        width: u16,
        /// Height in pixels.
        height: u16,
        /// Hotspot X offset inside the image.
        hot_x: u16,
        /// Hotspot Y offset inside the image.
        hot_y: u16,
        /// RGBA pixels, `width * height * 4` bytes.
        rgba: Vec<u8>,
    },
    /// Text pulled from the remote clipboard (M8); the consumer should
    /// apply it to the local clipboard.
    ClipboardData {
        /// Clipboard text (UTF-8).
        text: String,
    },
    /// Non-fatal session condition worth surfacing (e.g. the video stream
    /// broke and a keyframe was requested). Fatal errors end
    /// [`crate::Session::run`] with `Err` instead.
    Error(String),
    /// The session ended (peer close, [`crate::ClientCommand::Close`], or
    /// the command channel shut down). Always the last event.
    Disconnected,
}

// Manual Debug: bulky payloads (cursor pixels) stay out of the logs, as in
// freerdp-integration's RdpMessage.
impl std::fmt::Debug for ClientEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connected { udp } => f.debug_struct("Connected").field("udp", udp).finish(),
            Self::DisplayInfo {
                display_id,
                width,
                height,
                codec,
            } => f
                .debug_struct("DisplayInfo")
                .field("display_id", display_id)
                .field("width", width)
                .field("height", height)
                .field("codec", codec)
                .finish(),
            Self::FrameDirty { display_id, rects } => f
                .debug_struct("FrameDirty")
                .field("display_id", display_id)
                .field("rects", &rects.len())
                .finish(),
            Self::CursorVisibility { visible } => f
                .debug_struct("CursorVisibility")
                .field("visible", visible)
                .finish(),
            Self::CursorShape {
                width,
                height,
                hot_x,
                hot_y,
                ..
            } => f
                .debug_struct("CursorShape")
                .field("width", width)
                .field("height", height)
                .field("hot_x", hot_x)
                .field("hot_y", hot_y)
                .finish(),
            Self::ClipboardData { text } => f
                .debug_struct("ClipboardData")
                .field("len", &text.len())
                .finish(),
            Self::Error(e) => f.debug_tuple("Error").field(e).finish(),
            Self::Disconnected => f.debug_struct("Disconnected").finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_bulkypayloads() {
        let shape = ClientEvent::CursorShape {
            width: 2,
            height: 2,
            hot_x: 0,
            hot_y: 0,
            rgba: vec![255; 16],
        };
        let dbg = format!("{shape:?}");
        assert!(dbg.contains("CursorShape"));
        assert!(!dbg.contains("rgba"));
        let dirty = ClientEvent::FrameDirty {
            display_id: 0,
            rects: vec![Rect {
                x: 0,
                y: 0,
                w: 4,
                h: 4,
            }],
        };
        assert!(format!("{dirty:?}").contains("rects: 1"));
    }
}
