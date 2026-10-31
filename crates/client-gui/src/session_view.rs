//! Session view: renders the remote display (display 0 for now; the model
//! is multi-display ready), forwards keyboard/pointer input and paints the
//! cursor locally (§10: the cursor never travels inside the video).

use client::{ClientCommand, ClientEvent, CommandSender, Displays, Rect};
use protocol::InputMessage;
use tracing::{debug, warn};
use winit::event::{MouseButton, MouseScrollDelta};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorIcon, CustomCursor, Window};

use crate::gpu::Gpu;
use crate::keymap;

/// Live session state owned by the GUI thread.
pub struct SessionView {
    commands: CommandSender,
    events: flume::Receiver<ClientEvent>,
    displays: Displays,
    texture: Option<Texture>,
    /// Display geometry (display 0).
    display_size: (u32, u32),
    status: String,
    disconnected: bool,
    clipboard: Option<arboard::Clipboard>,
    pointer: (f64, f64),
    /// One scratch buffer for rect copies (hot path: no per-frame alloc).
    scratch: Vec<u8>,
}

struct Texture {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

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
            status: "connecting…".to_string(),
            disconnected: false,
            clipboard: arboard::Clipboard::new().ok(),
            pointer: (0.0, 0.0),
            scratch: Vec::new(),
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

    /// Drains pending session events; returns false when the session is gone.
    pub fn poll_events(&mut self, gpu: &Gpu, window: &Window, event_loop: &ActiveEventLoop) -> bool {
        while let Ok(event) = self.events.try_recv() {
            match event {
                ClientEvent::Connected { udp } => {
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
                        self.texture = Some(make_texture(gpu, width, height));
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

    /// Forwards a key press/release to the remote.
    pub fn handle_key(&self, code: KeyCode, down: bool) {
        if let Some((scancode, extended)) = keymap::map_key(code) {
            let _ = self.commands.try_send(ClientCommand::Input(InputMessage::Key {
                down,
                scancode,
                extended,
            }));
        }
    }

    /// Forwards pointer motion, mapping window coordinates to display pixels.
    pub fn handle_pointer_move(&mut self, x: f64, y: f64, window: &Window) {
        self.pointer = (x, y);
        let (dw, dh) = self.display_size;
        if dw == 0 || dh == 0 {
            return;
        }
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        #[expect(clippy::cast_possible_truncation)]
        let (dx, dy) = (
            (x * f64::from(dw) / f64::from(size.width)).round() as i32,
            (y * f64::from(dh) / f64::from(size.height)).round() as i32,
        );
        let _ = self
            .commands
            .try_send(ClientCommand::Input(InputMessage::PointerMove {
                display_id: 0,
                x: dx.clamp(0, dw.saturating_sub(1).cast_signed()),
                y: dy.clamp(0, dh.saturating_sub(1).cast_signed()),
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
