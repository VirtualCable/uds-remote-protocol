// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Session view: renders the remote display (display 0 for now; the model
//! is multi-display ready), forwards keyboard/pointer input and paints the
//! cursor locally (§10: the cursor never travels inside the video).

use client::{ClientCommand, ClientEvent, CommandSender, Displays, Rect, StatsSnapshot};
use protocol::InputMessage;
use tracing::{debug, warn};
use winit::event::{MouseButton, MouseScrollDelta};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorIcon, CustomCursor, Window};

use crate::gpu::{Gpu, Panel};
use crate::keymap;

/// Live session state owned by the GUI thread.
pub struct SessionView {
    commands: CommandSender,
    events: flume::Receiver<ClientEvent>,
    displays: Displays,
    texture: Option<Texture>,
    /// Display geometry (display 0).
    display_size: (u32, u32),
    /// Codec of display 0, from its announcement.
    codec: Option<codec::VideoCodecType>,
    /// True while the UDP leg is active (from `ClientEvent::Connected`).
    udp: bool,
    status: String,
    disconnected: bool,
    clipboard: Option<arboard::Clipboard>,
    pointer: (f64, f64),
    /// One scratch buffer for rect copies (hot path: no per-frame alloc).
    scratch: Vec<u8>,
    /// Latest stats snapshot (with arrival time) and the previous one (for
    /// per-second rates).
    stats: Option<(StatsSnapshot, std::time::Instant)>,
    prev_stats: Option<(StatsSnapshot, std::time::Instant)>,
    /// Debug overlay visible (toggled with F3, not forwarded to the remote).
    show_stats: bool,
    /// Keys currently held down, as (scancode, extended), so they can all be
    /// released when the window loses focus (otherwise they stick remotely).
    pressed_keys: std::collections::HashSet<(u16, bool)>,
    /// AltGr↔Ctrl pairing state (Windows' fake LCtrl events are unreliable).
    altgr: crate::altgr::AltGr,
}

struct Texture {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

/// Stats overlay geometry (top-right corner).
const STATS_PANEL_W: f32 = 322.0;
/// Stats overlay height: 5 lines × 16 px + padding.
const STATS_PANEL_H: f32 = 92.0;
/// Stats overlay margin from the window edges.
const STATS_PANEL_MARGIN: f32 = 10.0;

impl SessionView {
    /// Wires the pieces handed by `client::Session::new`.
    #[must_use]
    pub fn new(
        commands: CommandSender,
        events: flume::Receiver<ClientEvent>,
        displays: Displays,
    ) -> Self {
        Self {
            commands,
            events,
            displays,
            texture: None,
            display_size: (0, 0),
            codec: None,
            udp: false,
            status: "connecting…".to_string(),
            disconnected: false,
            clipboard: arboard::Clipboard::new().ok(),
            pointer: (0.0, 0.0),
            scratch: Vec::new(),
            stats: None,
            prev_stats: None,
            show_stats: false,
            pressed_keys: std::collections::HashSet::new(),
            altgr: crate::altgr::AltGr::default(),
        }
    }

    /// True once the session ended (the app shows a farewell screen).
    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        self.disconnected
    }

    /// Current one-line status for the overlay.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Sends a session-ending command.
    pub fn close(&self) {
        let _ = self.commands.try_send(ClientCommand::Close);
    }

    /// Notifies the session of a new preferred resolution (§18.37): the
    /// server re-applies its resolution policy and answers with a fresh
    /// `DisplayInfo` carrying the real geometry.
    pub fn resize(&self, width: u32, height: u32) {
        let _ = self.commands.try_send(ClientCommand::Resize { width, height });
    }

    /// Drains pending session events; returns false when the session is gone.
    pub fn poll_events(&mut self, gpu: &Gpu, window: &Window, event_loop: &ActiveEventLoop) -> bool {
        while let Ok(event) = self.events.try_recv() {
            match event {
                ClientEvent::Connected { udp } => {
                    self.udp = udp;
                    self.status = format!("connected ({})", if udp { "UDP" } else { "TCP" });
                }
                ClientEvent::DisplayInfo {
                    display_id,
                    width,
                    height,
                    codec,
                } => {
                    debug!(display_id, width, height, ?codec, "display announced");
                    if display_id == 0 {
                        self.display_size = (width, height);
                        self.codec = Some(codec);
                        // Recreate the texture only when the geometry changes:
                        // on a codec switch (same size) keeping it retains the
                        // last presented frame instead of flashing black until
                        // the first frame of the new codec arrives.
                        let size_changed = self
                            .texture
                            .as_ref()
                            .is_none_or(|t| t.texture.width() != width || t.texture.height() != height);
                        if size_changed {
                            self.texture = Some(make_texture(gpu, width, height));
                        }
                        let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(
                            width.max(1),
                            height.max(1),
                        ));
                    }
                }
                ClientEvent::FrameDirty { display_id, rects } => {
                    if display_id == 0 {
                        self.upload_rects(gpu, display_id, &rects);
                    }
                }
                ClientEvent::CursorVisibility { visible } => {
                    window.set_cursor_visible(visible);
                }
                ClientEvent::CursorShape {
                    width,
                    height,
                    hot_x,
                    hot_y,
                    rgba,
                } => {
                    Self::apply_cursor(event_loop, window, width, height, hot_x, hot_y, &rgba);
                }
                ClientEvent::ClipboardData { text } => {
                    if let Some(clipboard) = &mut self.clipboard
                        && let Err(e) = clipboard.set_text(text)
                    {
                        warn!("setting the local clipboard: {e}");
                    }
                }
                ClientEvent::Stats(snapshot) => {
                    self.prev_stats = self
                        .stats
                        .replace((snapshot, std::time::Instant::now()));
                }
                ClientEvent::Error(message) => {
                    self.status = message;
                }
                ClientEvent::Disconnected => {
                    self.disconnected = true;
                    self.status = "disconnected".to_string();
                    return false;
                }
                ClientEvent::StorageList { .. }
                | ClientEvent::TransferDone { .. }
                | ClientEvent::TransferError { .. } => {}
            }
        }
        true
    }

    /// Sends one key event and tracks it in the held set.
    fn send_key(&mut self, key: (u16, bool), down: bool) {
        if down {
            self.pressed_keys.insert(key);
        } else {
            self.pressed_keys.remove(&key);
        }
        let (scancode, extended) = key;
        let _ = self.commands.try_send(ClientCommand::Input(InputMessage::Key {
            down,
            scancode,
            extended,
        }));
    }

    /// Forwards a key press/release to the remote and tracks the held set.
    /// AltGr (RAlt) is paired with an owned LCtrl (see `altgr` module).
    pub fn handle_key(&mut self, code: KeyCode, down: bool) {
        const LCTRL: (u16, bool) = (0x1D, false);
        match (code, down) {
            (KeyCode::ControlLeft, true) => self.altgr.ctrl_down(std::time::Instant::now()),
            (KeyCode::ControlLeft, false) => self.altgr.ctrl_up(),
            (KeyCode::AltRight, down) => {
                let now = std::time::Instant::now();
                let action = if down {
                    self.altgr.altgr_down(now, self.pressed_keys.contains(&LCTRL))
                } else {
                    self.altgr.altgr_up()
                };
                match action {
                    crate::altgr::CtrlAction::Press => self.send_key(LCTRL, true),
                    // Release after the RAlt-up below (mirror of the OS order).
                    crate::altgr::CtrlAction::Release | crate::altgr::CtrlAction::Keep => {}
                }
                if let Some(key) = keymap::map_key(code) {
                    self.send_key(key, down);
                }
                if action == crate::altgr::CtrlAction::Release {
                    self.send_key(LCTRL, false);
                }
                return;
            }
            _ => {}
        }
        if let Some(key) = keymap::map_key(code) {
            self.send_key(key, down);
        }
    }

    /// Releases every key currently held (window lost focus, so the matching
    /// key-up events will never reach us and the keys would stick remotely).
    pub fn release_all_keys(&mut self) {
        self.altgr.reset();
        for (scancode, extended) in self.pressed_keys.drain() {
            let _ = self.commands.try_send(ClientCommand::Input(InputMessage::Key {
                down: false,
                scancode,
                extended,
            }));
        }
    }

    /// Forwards pointer motion, normalized to 0..=65535 over the window
    /// (§18.37): the server maps it onto the whole desktop at injection
    /// time, so resizes never race and no side stores the other's geometry.
    pub fn handle_pointer_move(&mut self, x: f64, y: f64, window: &Window) {
        self.pointer = (x, y);
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (nx, ny) = (
            (x / f64::from(size.width) * 65535.0).round().clamp(0.0, 65535.0) as u16,
            (y / f64::from(size.height) * 65535.0).round().clamp(0.0, 65535.0) as u16,
        );
        let _ = self
            .commands
            .try_send(ClientCommand::Input(InputMessage::PointerMove {
                display_id: 0,
                x: nx,
                y: ny,
            }));
    }

    /// Forwards a pointer button.
    pub fn handle_button(&self, button: MouseButton, down: bool) {
        let button = match button {
            MouseButton::Left => protocol::PointerButton::Left,
            MouseButton::Right => protocol::PointerButton::Right,
            MouseButton::Middle => protocol::PointerButton::Middle,
            MouseButton::Back => protocol::PointerButton::X1,
            MouseButton::Forward => protocol::PointerButton::X2,
            MouseButton::Other(_) => return,
        };
        let _ = self
            .commands
            .try_send(ClientCommand::Input(InputMessage::PointerButton { button, down }));
    }

    /// Forwards wheel motion (detents).
    pub fn handle_wheel(&self, delta: MouseScrollDelta) {
        let (v, h) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (y, x),
            MouseScrollDelta::PixelDelta(pos) => {
                #[expect(clippy::cast_possible_truncation)]
                let (x, y) = (pos.x as f32 / 120.0, pos.y as f32 / 120.0);
                (y, x)
            }
        };
        #[expect(clippy::cast_possible_truncation)]
        let (v, h) = ((v * 120.0) as i16, (h * 120.0) as i16);
        if v != 0 || h != 0 {
            let _ = self
                .commands
                .try_send(ClientCommand::Input(InputMessage::PointerWheel {
                    delta: v,
                    hdelta: h,
                }));
        }
    }

    /// Publishes local clipboard text changes to the remote. Call on focus.
    pub fn sync_clipboard(&mut self) {
        if let Some(clipboard) = &mut self.clipboard
            && let Ok(text) = clipboard.get_text()
            && !text.is_empty()
        {
            let _ = self.commands.try_send(ClientCommand::ClipboardChanged(text));
        }
    }

    /// The texture to draw this frame, if any.
    #[must_use]
    pub fn texture_view(&self) -> Option<&wgpu::TextureView> {
        self.texture.as_ref().map(|t| &t.view)
    }

    /// Toggles the debug stats overlay (F3; the key is never forwarded).
    pub fn toggle_stats(&mut self) {
        self.show_stats = !self.show_stats;
    }

    /// Translucent background panel for the stats overlay, top-right.
    #[must_use]
    #[expect(clippy::cast_precision_loss, reason = "window sizes fit in f32")]
    pub fn stats_panel(&self, window_width: u32) -> Option<Panel> {
        if !self.show_stats || self.stats.is_none() {
            return None;
        }
        Some(Panel {
            x: (window_width as f32 - STATS_PANEL_W - STATS_PANEL_MARGIN).max(0.0),
            y: STATS_PANEL_MARGIN,
            w: STATS_PANEL_W,
            h: STATS_PANEL_H,
            color: [0.02, 0.03, 0.05, 0.65],
        })
    }

    /// Text sections of the stats overlay (top-right, small). The loss line
    /// turns red-ish past 5 ‰.
    #[must_use]
    #[expect(clippy::cast_precision_loss, reason = "window sizes fit in f32")]
    pub fn stats_sections(&self, window_width: u32) -> Vec<wgpu_text::glyph_brush::OwnedSection> {
        use wgpu_text::glyph_brush::{OwnedSection, OwnedText};
        let Some(panel) = self.stats_panel(window_width) else {
            return Vec::new();
        };
        let Some((snap, _)) = &self.stats else {
            return Vec::new();
        };
        let dt = self
            .prev_stats
            .as_ref()
            .map_or(1.0, |(_, t)| t.elapsed().as_secs_f32())
            .max(0.001);
        let rate = |cur: u64, prev: u64| cur.saturating_sub(prev) as f32 / dt;
        let (fps, dec_rate, plc_rate) = self.prev_stats.as_ref().map_or((0.0, 0.0, 0.0), |(p, _)| {
            (
                rate(snap.presented, p.presented),
                rate(snap.audio_decoded, p.audio_decoded),
                rate(snap.audio_plc, p.audio_plc),
            )
        });
        let (w, h) = self.display_size;
        let codec = self.codec.map_or_else(|| "?".to_string(), |c| format!("{c:?}").to_lowercase());
        let underrun_s = snap.audio_underrun_samples as f32 / 96_000.0; // 48k stereo
        let lines = [
            format!(
                "{} {w}x{h} {codec}",
                if snap.udp { "UDP" } else { "TCP" }
            ),
            format!(
                "rtt {:.2}ms · jit {:.2}ms · loss {:.1}‰",
                snap.rtt_ms, snap.jitter_ms, snap.loss_permille
            ),
            format!(
                "fps {fps:.0} · hold {} · ovf {} · stale {} · kf {}",
                snap.present_queue_len, snap.dropped_overflow, snap.dropped_stale, snap.keyframe_requests
            ),
            format!(
                "avsync {} · skew {}/{}ms",
                if snap.audio_clock_live { "audio" } else { "no-audio" },
                snap.skew_avg_ms, snap.skew_max_abs_ms
            ),
            format!(
                "audio {dec_rate:.0}/s · plc {plc_rate:.0}/s · rs {} · und {underrun_s:.1}s",
                snap.audio_resyncs
            ),
        ];
        let loss_color = if snap.loss_permille > 5.0 {
            [1.0, 0.45, 0.4, 1.0]
        } else {
            [0.75, 0.9, 0.75, 1.0]
        };
        lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let color = if i == 1 { loss_color } else { [0.85, 0.88, 0.92, 1.0] };
                OwnedSection::default()
                    .with_screen_position((panel.x + 8.0, panel.y + 6.0 + i as f32 * 16.0))
                    .add_text(OwnedText::new(line.clone()).with_scale(13.0).with_color(color))
            })
            .collect()
    }

    fn upload_rects(&mut self, gpu: &Gpu, display_id: u8, rects: &[Rect]) {
        let (Some(display), Some(tex)) = (self.displays.get(display_id), &self.texture) else {
            return;
        };
        let Ok(display) = display.read() else {
            return;
        };
        for rect in rects {
            self.scratch.clear();
            display.copy_rect_to(*rect, &mut self.scratch);
            if self.scratch.is_empty() {
                continue;
            }
            let (x, y) = (rect.x.max(0).cast_unsigned(), rect.y.max(0).cast_unsigned());
            let clipped_w = rect.w.min(display.width().saturating_sub(x));
            let clipped_h = rect.h.min(display.height().saturating_sub(y));
            if clipped_w == 0 || clipped_h == 0 {
                continue;
            }
            gpu.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &self.scratch,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(clipped_w * 4),
                    rows_per_image: Some(clipped_h),
                },
                wgpu::Extent3d {
                    width: clipped_w,
                    height: clipped_h,
                    depth_or_array_layers: 1,
                },
            );
        }
    }

    fn apply_cursor(
        event_loop: &ActiveEventLoop,
        window: &Window,
        width: u16,
        height: u16,
        hot_x: u16,
        hot_y: u16,
        rgba: &[u8],
    ) {
        match CustomCursor::from_rgba(rgba.to_vec(), width, height, hot_x, hot_y) {
            Ok(source) => {
                let cursor = event_loop.create_custom_cursor(source);
                window.set_cursor(cursor);
            }
            Err(e) => {
                debug!("custom cursor rejected ({e}); falling back to the default arrow");
                window.set_cursor(CursorIcon::Default);
            }
        }
    }
}

fn make_texture(gpu: &Gpu, width: u32, height: u32) -> Texture {
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("display"),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        // Decoded frames are tight BGRA (codec crate) — the exact match for
        // this format, so uploads need no conversion. It must be the sRGB
        // variant: the surface is sRGB, and sampling a non-sRGB texture
        // would feed sRGB-encoded values into the surface's linear→sRGB
        // write-back, washing the whole picture out (double gamma).
        format: wgpu::TextureFormat::Bgra8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Texture { texture, view }
}

/// Physical key extraction helper for the app shell.
#[must_use]
pub fn physical_code(event: &winit::event::KeyEvent) -> Option<KeyCode> {
    match event.physical_key {
        PhysicalKey::Code(code) => Some(code),
        PhysicalKey::Unidentified(_) => None,
    }
}
