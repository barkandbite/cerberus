//! The platform surface seam.
//!
//! `PlatformSurface` abstracts "somewhere to present a framebuffer". The
//! scaffold ships only `HeadlessSurface` (captures frames in memory), which is
//! all the M0 trivial render and CI need and which also serves the headless
//! rendering mode (M8).
//!
//! A real windowed surface (e.g. via a `winit` + `softbuffer` adapter) is a
//! future adapter behind this same trait. That windowing dependency is **not
//! yet approved** — it needs its own ADR (see PLAN.md "Open decisions"); until
//! then we deliberately do not pull a GUI stack into the tree.

use cerberus_paint::Framebuffer;
use cerberus_types::Size;

/// Errors from a platform surface.
#[derive(Clone, Debug)]
pub enum ShellError {
    /// Presenting the frame failed.
    Present(String),
}

/// Somewhere a rendered frame can be presented (a window, or a headless
/// capture). Callers depend only on this trait, never on a windowing library.
pub trait PlatformSurface {
    /// The surface size in device pixels.
    fn size(&self) -> Size;

    /// Present a frame. The framebuffer's size should match [`size`](Self::size).
    fn present(&mut self, frame: &Framebuffer) -> Result<(), ShellError>;
}

/// A surface that keeps the most recently presented frame in memory. Used for
/// the headless render path, tests, and the memory gate.
#[derive(Debug)]
pub struct HeadlessSurface {
    size: Size,
    last: Option<Framebuffer>,
}

impl HeadlessSurface {
    /// Create a headless surface of the given size.
    pub fn new(size: Size) -> Self {
        Self { size, last: None }
    }

    /// The most recently presented frame, if any.
    pub fn last_frame(&self) -> Option<&Framebuffer> {
        self.last.as_ref()
    }
}

impl PlatformSurface for HeadlessSurface {
    fn size(&self) -> Size {
        self.size
    }

    fn present(&mut self, frame: &Framebuffer) -> Result<(), ShellError> {
        self.last = Some(frame.clone());
        Ok(())
    }
}

/// An interactive application the platform layer drives.
///
/// It renders a frame for a given size and reacts to input. The browser
/// implements this; the windowing adapter (`cerberus-shell-winit`) calls it from
/// the event loop, and tests can drive it headlessly. Each input method returns
/// whether a redraw is needed. No windowing type ever appears here.
pub trait FrameApp {
    /// Window title.
    fn title(&self) -> String;

    /// Render a frame at the given size.
    fn render_frame(&mut self, size: Size) -> Framebuffer;

    /// Pointer press at device coordinates.
    fn pointer_down(&mut self, x: i32, y: i32) -> bool;

    /// A typed character.
    fn text_input(&mut self, c: char) -> bool;

    /// Enter / confirm (e.g. submit the URL box).
    fn submit(&mut self) -> bool;

    /// Backspace.
    fn backspace(&mut self) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;
    use cerberus_types::Color;

    #[test]
    fn headless_surface_captures_frames() {
        let size = Size::new(8, 8);
        let mut surface = HeadlessSurface::new(size);
        assert!(surface.last_frame().is_none());

        let mut fb = Framebuffer::new(size);
        fb.clear(Color::WHITE);
        surface.present(&fb).unwrap();

        assert_eq!(
            surface.last_frame().unwrap().pixel(0, 0),
            Some(Color::WHITE)
        );
    }
}
