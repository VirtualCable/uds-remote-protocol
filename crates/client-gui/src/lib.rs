//! # client-gui
//!
//! Reference GUI for the uds-rp client (winit + wgpu), mirroring the
//! architecture of uds-client's launcher GUI: the protocol session runs on
//! a Tokio current-thread runtime in a worker thread and the GUI talks to
//! it exclusively through the `client` crate's `ClientEvent`/`ClientCommand`
//! flume channels. That keeps the front-end swappable (phase 4 embeds the
//! same session in the UDS Launcher; an HTML5 front-end is another
//! possibility) without touching the protocol code.

mod connect;
mod gpu;
mod keymap;
mod session_view;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use client::{ClientConfig, Session};
use client::mic::MicMode;
use tracing::{error, warn};
use wgpu_text::glyph_brush::{OwnedSection, OwnedText};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

pub use connect::ConnectForm;
use connect::ConnectDialog;
use gpu::Gpu;
use session_view::SessionView;

/// Frame pacing for the GUI thread (events arrive through flume; redraws
/// are capped at this interval).
const FRAME_INTERVAL: Duration = Duration::from_millis(8);

/// Optional session features chosen by the caller (CLI flags of the
/// reference client; checkboxes in a future iteration).
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionFlags {
    /// Audio playback (server-side announcement permitting).
    pub audio: bool,
    /// Offer smartcard redirection.
    pub smartcard: bool,
    /// Ask the server to cover the physical console (M7).
    pub privacy_screen: bool,
}

pub use server_config::ticket_bytes;

/// Runs the GUI (blocking, main thread). With `form = Some(..)` the client
/// connects immediately; with `None` the connect dialog shows first.
///
/// # Errors
/// Returns an error if the window or the GPU cannot be initialized.
pub fn run_gui(form: Option<ConnectForm>, flags: SessionFlags) -> Result<()> {
    let event_loop = EventLoop::new().context("creating the event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App {
        window: None,
        gpu: None,
        mode: Mode::Dialog(form.as_ref().map_or_else(ConnectDialog::default, ConnectDialog::prefilled)),
        flags,
        pending: form,
        last_frame: Instant::now().checked_sub(FRAME_INTERVAL).unwrap_or_else(Instant::now),
    };
    event_loop.run_app(&mut app).context("running the event loop")
}

enum Mode {
    Dialog(ConnectDialog),
    Session(Box<SessionView>, flume::Receiver<Result<(), String>>),
    Farewell(String),
}

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    mode: Mode,
    flags: SessionFlags,
    /// A form to auto-connect once the window exists.
    pending: Option<ConnectForm>,
    last_frame: Instant,
}

impl App {
    fn start_session(&mut self, form: &ConnectForm) {
        match spawn_session(form, self.flags) {
            Ok((view, result)) => self.mode = Mode::Session(Box::new(view), result),
            Err(e) => {
                let mut dialog = ConnectDialog::default();
                dialog.set_status(e);
                self.mode = Mode::Dialog(dialog);
            }
        }
    }

    #[expect(clippy::cast_precision_loss, reason = "window sizes fit in f32")]
    fn render(&mut self) {
        let (Some(window), Some(gpu)) = (&self.window, &mut self.gpu) else {
            return;
        };
        let width = gpu.config.width as f32;
        let (texture, clear, sections) = match &mut self.mode {
            Mode::Dialog(dialog) => (None, BG, dialog.sections(width)),
            Mode::Session(view, _) => {
                let mut sections = Vec::new();
                if view.texture_view().is_none() || !view.status().is_empty() && view.is_disconnected()
                {
                    sections.push(
                        OwnedSection::default()
                            .with_screen_position((16.0, 14.0))
                            .add_text(
                                OwnedText::new(view.status().to_string())
                                    .with_scale(16.0)
                                    .with_color([0.7, 0.72, 0.75, 1.0]),
                            ),
                    );
                }
                (view.texture_view(), wgpu::Color::BLACK, sections)
            }
            Mode::Farewell(message) => (
                None,
                BG,
                vec![
                    OwnedSection::default()
                        .with_screen_position((40.0, 40.0))
                        .add_text(
                            OwnedText::new("Session ended")
                                .with_scale(28.0)
                                .with_color([0.35, 0.75, 1.0, 1.0]),
                        ),
                    OwnedSection::default()
                        .with_screen_position((40.0, 90.0))
                        .with_bounds((width - 80.0, 60.0))
                        .add_text(
                            OwnedText::new(message.clone())
                                .with_scale(18.0)
                                .with_color([0.92, 0.93, 0.95, 1.0]),
                        ),
                    OwnedSection::default()
                        .with_screen_position((40.0, 150.0))
                        .add_text(
                            OwnedText::new("Enter: new connection · Esc: quit")
                                .with_scale(16.0)
                                .with_color([0.55, 0.58, 0.62, 1.0]),
                        ),
                ],
            ),
        };
        if let Err(e) = gpu.render(texture, clear, &sections) {
            warn!("render: {e:#}");
        }
        let _ = window;
    }
}

/// Dark background of the dialog/farewell screens.
const BG: wgpu::Color = wgpu::Color {
    r: 0.07,
    g: 0.08,
    b: 0.10,
    a: 1.0,
};

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("UDS Remote Protocol — Client")
            .with_inner_size(winit::dpi::LogicalSize::new(1024.0, 640.0));
        match event_loop.create_window(attrs) {
            Ok(window) => {
                let window = Arc::new(window);
                match Gpu::new(&window) {
                    Ok(gpu) => {
                        self.gpu = Some(gpu);
                        self.window = Some(window);
                        if let Some(form) = self.pending.take() {
                            self.start_session(&form);
                        }
                    }
                    Err(e) => {
                        error!("GPU init failed: {e:#}");
                        event_loop.exit();
                    }
                }
            }
            Err(e) => {
                error!("window creation failed: {e}");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(window) = self.window.clone() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                if let Mode::Session(view, _) = &self.mode {
                    view.close();
                }
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.resize(size.width, size.height);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    if let Mode::Session(view, _) = &self.mode
                        && let Some(code) = session_view::physical_code(&event)
                    {
                        view.handle_key(code, false);
                    }
                    return;
                }
                match &mut self.mode {
                    Mode::Dialog(dialog) => match dialog.handle_key(&event) {
                        connect::DialogAction::Connect(form) => self.start_session(&form),
                        connect::DialogAction::Quit => event_loop.exit(),
                        connect::DialogAction::Nothing => {}
                    },
                    Mode::Session(view, _) => {
                        if let Some(code) = session_view::physical_code(&event) {
                            view.handle_key(code, true);
                        }
                    }
                    Mode::Farewell(_) => match event.logical_key {
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Enter) => {
                            self.mode = Mode::Dialog(ConnectDialog::default());
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Escape) => {
                            event_loop.exit();
                        }
                        _ => {}
                    },
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Mode::Session(view, _) = &mut self.mode {
                    view.handle_pointer_move(position.x, position.y, &window);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Mode::Session(view, _) = &self.mode {
                    view.handle_button(button, state == ElementState::Pressed);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Mode::Session(view, _) = &self.mode {
                    view.handle_wheel(delta);
                }
            }
            WindowEvent::Focused(true) => {
                if let Mode::Session(view, _) = &mut self.mode {
                    view.sync_clipboard();
                }
            }
            WindowEvent::RedrawRequested => self.render(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(window) = self.window.clone() else {
            return;
        };
        if let Mode::Session(view, result) = &mut self.mode
            && !view.poll_events(self.gpu.as_ref().expect("gpu after session"), &window, event_loop)
        {
            let message = result
                .try_recv()
                .ok()
                .and_then(Result::err)
                .unwrap_or_else(|| "the session ended".to_string());
            self.mode = Mode::Farewell(message);
        }
        let now = Instant::now();
        if now.duration_since(self.last_frame) >= FRAME_INTERVAL {
            self.last_frame = now;
            window.request_redraw();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(now + FRAME_INTERVAL));
    }
}

/// Builds the client session and spawns its worker thread.
fn spawn_session(
    form: &ConnectForm,
    flags: SessionFlags,
) -> Result<(SessionView, flume::Receiver<Result<(), String>>), String> {
    let addr: SocketAddr = form
        .addr
        .parse()
        .map_err(|_| format!("bad server address {:?} (expected host:port)", form.addr))?;
    let key: [u8; 32] = protocol::hex::decode(&form.key_hex)
        .map_err(|_| "bad key: expected 64 hex chars (32 bytes)".to_string())?;
    let config = ClientConfig {
        addr,
        ticket: ticket_bytes(&form.ticket),
        key,
        audio: flags.audio,
        audio_dump: None,
        mic: MicMode::Off,
        privacy_screen: flags.privacy_screen,
        smartcard: flags.smartcard,
    };
    let (events_tx, events_rx) = flume::bounded(client::EVENT_CHANNEL_CAPACITY);
    let (session, commands, displays) = Session::new(config, events_tx);
    let (result_tx, result_rx) = flume::bounded(1);
    std::thread::Builder::new()
        .name("uds-rp-session".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            let result = runtime.block_on(session.run());
            let _ = result_tx.send(result.map_err(|e| format!("{e:#}")));
        })
        .map_err(|e| format!("spawning the session thread: {e}"))?;
    Ok((SessionView::new(commands, events_rx, displays), result_rx))
}
