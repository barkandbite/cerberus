//! Windowing adapter (ADR-0004): run a [`FrameApp`] in a real OS window using
//! `winit` (window/fullscreen/input + event loop) and `softbuffer` (CPU
//! framebuffer presentation).
//!
//! This crate is the *only* place that names winit/softbuffer types; it
//! translates window events into `FrameApp` calls and blits the returned
//! `Framebuffer`. The same `FrameApp` runs headlessly in tests and in the
//! headless render mode — no display required there.
//!
//! Keyboard: Enter submits, Backspace deletes, F11 toggles fullscreen, Esc
//! leaves fullscreen; other text goes to the app's URL box.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;

use cerberus_shell::{FrameApp, Waker};
use cerberus_types::Size;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

/// Wraps a winit proxy so a worker thread can wake the event loop.
struct ProxyWaker(EventLoopProxy<()>);

impl Waker for ProxyWaker {
    fn wake(&self) {
        let _ = self.0.send_event(());
    }
}

/// Errors from running the windowed event loop.
#[derive(Debug)]
pub enum WinitError {
    /// The event loop could not be created or run.
    EventLoop(String),
    /// A window or drawing surface could not be created.
    Surface(String),
}

impl std::fmt::Display for WinitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WinitError::EventLoop(m) => write!(f, "event loop error: {m}"),
            WinitError::Surface(m) => write!(f, "surface error: {m}"),
        }
    }
}

impl std::error::Error for WinitError {}

type WinitSurface = softbuffer::Surface<Rc<Window>, Rc<Window>>;

/// Holds the running app plus its window and drawing surface.
struct State<A: FrameApp> {
    app: A,
    window: Option<Rc<Window>>,
    surface: Option<WinitSurface>,
    cursor: (f64, f64),
    fullscreen: bool,
    start_fullscreen: bool,
    error: Option<WinitError>,
}

impl<A: FrameApp> State<A> {
    fn redraw(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let size = window.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));

        // Render before borrowing the surface (disjoint field borrows).
        let frame = self.app.render_frame(Size::new(w, h));

        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let (Some(nw), Some(nh)) = (NonZeroU32::new(w), NonZeroU32::new(h)) else {
            return;
        };
        if surface.resize(nw, nh).is_err() {
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        for (dst, px) in buffer.iter_mut().zip(frame.rgba.chunks_exact(4)) {
            // softbuffer expects 0x00RRGGBB.
            *dst = (px[0] as u32) << 16 | (px[1] as u32) << 8 | px[2] as u32;
        }
        let _ = buffer.present();
    }

    fn set_fullscreen(&mut self, on: bool) {
        self.fullscreen = on;
        if let Some(window) = &self.window {
            let mode = on.then(|| Fullscreen::Borderless(None));
            window.set_fullscreen(mode);
        }
    }

    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn handle_key(&mut self, event: KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }
        let redraw = match &event.logical_key {
            Key::Named(NamedKey::Enter) => self.app.submit(),
            Key::Named(NamedKey::Backspace) => self.app.backspace(),
            Key::Named(NamedKey::F11) => {
                self.set_fullscreen(!self.fullscreen);
                true
            }
            Key::Named(NamedKey::Escape) if self.fullscreen => {
                self.set_fullscreen(false);
                true
            }
            _ => match event.text {
                Some(text) => text
                    .chars()
                    .fold(false, |acc, c| self.app.text_input(c) || acc),
                None => false,
            },
        };
        if redraw {
            self.request_redraw();
        }
    }
}

impl<A: FrameApp> ApplicationHandler for State<A> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let mut attrs = Window::default_attributes().with_title(self.app.title());
        if self.start_fullscreen {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
            self.fullscreen = true;
        }
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                self.error = Some(WinitError::Surface(e.to_string()));
                event_loop.exit();
                return;
            }
        };
        let surface = match softbuffer::Context::new(window.clone())
            .and_then(|ctx| softbuffer::Surface::new(&ctx, window.clone()))
        {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(WinitError::Surface(e.to_string()));
                event_loop.exit();
                return;
            }
        };
        self.surface = Some(surface);
        self.window = Some(window);
        self.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) => self.request_redraw(),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if state == ElementState::Pressed && button == MouseButton::Left {
                    let (x, y) = (self.cursor.0 as i32, self.cursor.1 as i32);
                    if self.app.pointer_down(x, y) {
                        self.request_redraw();
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => self.handle_key(event),
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
        // A worker woke us; let the app drain its results and redraw if needed.
        if self.app.poll() {
            self.request_redraw();
        }
    }
}

/// Run `app` in a window until the user closes it. `fullscreen` starts the
/// window borderless-fullscreen (toggle later with F11). Requires a display
/// server; the headless path is used in CI/tests instead.
pub fn run(app: impl FrameApp + 'static, fullscreen: bool) -> Result<(), WinitError> {
    let event_loop = EventLoop::<()>::with_user_event()
        .build()
        .map_err(|e| WinitError::EventLoop(e.to_string()))?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut state = State {
        app,
        window: None,
        surface: None,
        cursor: (0.0, 0.0),
        fullscreen: false,
        start_fullscreen: fullscreen,
        error: None,
    };
    // Hand the app a waker so its network worker can wake the loop.
    state
        .app
        .set_waker(Arc::new(ProxyWaker(event_loop.create_proxy())));

    event_loop
        .run_app(&mut state)
        .map_err(|e| WinitError::EventLoop(e.to_string()))?;

    match state.error.take() {
        Some(err) => Err(err),
        None => Ok(()),
    }
}
