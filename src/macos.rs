//! macOS screen geometry. AppKit is the only source for the menu bar inset, which is
//! where a Quake-style panel starts.

use objc2_app_kit::{NSEvent, NSScreen};
use objc2_foundation::MainThreadMarker;

#[derive(Clone, Copy, Debug)]
pub struct Screen {
    /// Physical pixels from the top of the display to the top of the visible area
    /// (the menu bar inset), in the top-left coordinate space winit uses.
    pub top: i32,
    pub width_px: u32,
    pub height_px: u32,
    /// Physical pixels between the menu bar and the Dock (the visible frame height).
    pub usable_height_px: u32,
    pub scale: f64,
}

impl Default for Screen {
    fn default() -> Self {
        Self {
            top: 24,
            width_px: 1440 * 2,
            height_px: 900 * 2,
            usable_height_px: 900 * 2 - 24 - 160,
            scale: 2.0,
        }
    }
}

impl Screen {
    pub fn width_pt(&self) -> f32 {
        (self.width_px as f64 / self.scale) as f32
    }
    pub fn height_pt(&self) -> f32 {
        (self.height_px as f64 / self.scale) as f32
    }
    /// Height in points from the menu bar down to the Dock: the tallest the panel can be
    /// without covering either.
    pub fn usable_height_pt(&self) -> f32 {
        (self.usable_height_px as f64 / self.scale) as f32
    }
    /// The menu bar's height in points, the gap between the screen top and its
    /// visible area.
    pub fn inset_pt(&self) -> f32 {
        (self.top as f64 / self.scale) as f32
    }
}

/// The display the pointer is on, with the same geometry `main_screen` reports.
pub fn screen_under_pointer() -> Option<Screen> {
    let mtm = MainThreadMarker::new()?;
    let pointer = NSEvent::mouseLocation();
    let screens = NSScreen::screens(mtm);
    let screen = screens
        .iter()
        .find(|screen| {
            let frame = screen.frame();
            pointer.x >= frame.origin.x
                && pointer.x < frame.origin.x + frame.size.width
                && pointer.y > frame.origin.y
                && pointer.y <= frame.origin.y + frame.size.height
        })
        .or_else(|| NSScreen::mainScreen(mtm))?;
    Some(screen_from(&screen))
}

fn screen_from(screen: &NSScreen) -> Screen {
    let frame = screen.frame();
    let visible = screen.visibleFrame();
    let scale = screen.backingScaleFactor();
    // Cocoa is bottom-left origin; the menu bar is the gap between the top of the
    // frame and the top of the visible frame.
    let top_pt = (frame.origin.y + frame.size.height) - (visible.origin.y + visible.size.height);
    Screen {
        top: (top_pt * scale).round() as i32,
        width_px: (frame.size.width * scale).round() as u32,
        height_px: (frame.size.height * scale).round() as u32,
        usable_height_px: (visible.size.height * scale).round() as u32,
        scale,
    }
}

pub fn main_screen() -> Option<Screen> {
    let mtm = MainThreadMarker::new()?;
    let screen = NSScreen::mainScreen(mtm)?;
    Some(screen_from(&screen))
}
