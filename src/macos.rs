// SPDX-FileCopyrightText: 2026 Sash contributors
// SPDX-License-Identifier: MIT

//! macOS screen geometry and panel placement. AppKit supplies the menu-bar inset,
//! which is part of the panel's height now that it covers the menu bar.

use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use block2::RcBlock;
use objc2::ClassType;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2::{class, msg_send};
use objc2_app_kit::{NSApplication, NSEvent, NSPanel, NSScreen};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

#[derive(Clone, Copy, Debug)]
pub struct Screen {
    /// `CGDirectDisplayID`, to place the panel on the display this describes.
    pub id: u32,
    /// Left edge in Cocoa's global point space, for ordering displays.
    pub x_pt: f64,
    /// Bottom edge in Cocoa's global point space, for ordering displays.
    pub y_pt: f64,
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
            id: 0,
            x_pt: 0.0,
            y_pt: 0.0,
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
    /// Height in points from the physical top of the display down to the Dock: the
    /// visible frame plus the menu-bar strip that the panel covers.
    pub fn usable_height_pt(&self) -> f32 {
        (self.usable_height_px as f64 / self.scale) as f32 + self.inset_pt()
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
        id: screen_number(screen),
        x_pt: frame.origin.x,
        y_pt: frame.origin.y,
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

/// A display in the order the `display` config numbers them: the menu-bar display
/// first, then the rest left to right and top to bottom.
#[derive(Clone, Debug)]
pub struct Display {
    /// 1-based number stored in the config.
    pub index: u32,
    pub name: String,
    pub screen: Screen,
}

/// Every display, numbered for the settings page and the `display` config.
pub fn displays() -> Vec<Display> {
    let Some(mtm) = MainThreadMarker::new() else {
        return Vec::new();
    };
    let main_id = NSScreen::mainScreen(mtm).map(|screen| screen_number(&screen));
    let screens = NSScreen::screens(mtm)
        .iter()
        .map(|screen| {
            let id = screen_number(&screen);
            (
                screen_from(&screen),
                screen.localizedName().to_string(),
                Some(id) == main_id,
            )
        })
        .collect();
    ordered_displays(screens)
}

fn ordered_displays(mut screens: Vec<(Screen, String, bool)>) -> Vec<Display> {
    screens.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then(a.0.x_pt.total_cmp(&b.0.x_pt))
            .then(b.0.y_pt.total_cmp(&a.0.y_pt))
    });
    screens
        .into_iter()
        .enumerate()
        .map(|(i, (screen, name, _))| Display {
            index: i as u32 + 1,
            name,
            screen,
        })
        .collect()
}

/// The screen the panel should use: the configured 1-based display, or the screen
/// under the pointer when following it. A configured display that is not plugged in
/// falls back to the menu-bar display; the caller re-resolves when displays change.
pub fn preferred_screen(display: Option<u32>) -> Option<Screen> {
    let screens = displays();
    match display.filter(|n| *n > 0) {
        Some(n) => screens
            .iter()
            .find(|display| display.index == n)
            .map(|display| display.screen)
            .or_else(main_screen)
            .or_else(|| screens.first().map(|display| display.screen)),
        None => screen_under_pointer()
            .or_else(main_screen)
            .or_else(|| screens.first().map(|display| display.screen)),
    }
}

static DISPLAYS_CHANGED: AtomicBool = AtomicBool::new(false);
static WATCHING: OnceLock<()> = OnceLock::new();

/// Whether AppKit reported a display being added, removed, or reconfigured since
/// the last poll.
pub fn take_display_change() -> bool {
    DISPLAYS_CHANGED.swap(false, Ordering::AcqRel)
}

/// Wake the app when displays come or go, so a panel pinned to a display can move
/// to it as soon as it is plugged in, or back to the main display when it leaves.
pub fn watch_display_changes(waker: rune::Waker) {
    if MainThreadMarker::new().is_none() || WATCHING.set(()).is_err() {
        return;
    }
    let block = RcBlock::new(move |_note: NonNull<AnyObject>| {
        DISPLAYS_CHANGED.store(true, Ordering::Release);
        waker();
    });
    // SAFETY: registration runs on the main thread. AppKit copies the block and
    // owns the observer token for the life of the process, so both may be leaked.
    unsafe {
        let center: Retained<AnyObject> = msg_send![class!(NSNotificationCenter), defaultCenter];
        let name = NSString::from_str("NSApplicationDidChangeScreenParametersNotification");
        let token: Option<Retained<AnyObject>> = msg_send![
            &*center,
            addObserverForName: &*name,
            object: std::ptr::null::<AnyObject>(),
            queue: std::ptr::null::<AnyObject>(),
            usingBlock: &*block
        ];
        std::mem::forget(token);
    }
}

/// Keep the panel above the menu bar (window level 24). Level 25 is AppKit's
/// status-window level; always-on-top goes one higher to cover status items too.
/// Both remain below transient UI such as menus.
pub fn set_panel_level(always_on_top: bool) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    if let Some(panel) = app
        .windows()
        .iter()
        .find(|window| window.isKindOfClass(NSPanel::class()))
    {
        panel.setLevel(if always_on_top { 26 } else { 25 });
    }
}

/// Put the panel's window on the display `id`, centred across it and flush with
/// that display's physical top edge, in AppKit points. Returns false before the panel's
/// native window exists, so the caller can fall back to Rune's placement.
#[allow(unsafe_code)]
pub fn place_panel(id: u32, width: f64, height: f64) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let screens = NSScreen::screens(mtm);
    let Some(screen) = screens.iter().find(|screen| screen_number(screen) == id) else {
        return false;
    };
    let frame = screen.frame();
    let x = frame.origin.x + (frame.size.width - width) / 2.0;
    let y = frame.origin.y + frame.size.height - height;
    let rect = NSRect::new(NSPoint::new(x, y), NSSize::new(width, height));
    let app = NSApplication::sharedApplication(mtm);
    let Some(panel) = app
        .windows()
        .iter()
        .find(|window| window.isKindOfClass(NSPanel::class()))
    else {
        return false;
    };
    if panel.frame() != rect {
        panel.setFrame_display(rect, true);
        panel.invalidateShadow();
    }
    true
}

#[allow(unsafe_code)]
fn screen_number(screen: &NSScreen) -> u32 {
    // SAFETY: AppKit calls on the main thread; `deviceDescription` and its
    // `NSScreenNumber` value are live for the screen's lifetime.
    unsafe {
        let description: *mut AnyObject = msg_send![screen, deviceDescription];
        let key = NSString::from_str("NSScreenNumber");
        let number: *mut AnyObject = msg_send![description, objectForKey: &*key];
        msg_send![number, unsignedIntValue]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(id: u32, x_pt: f64, y_pt: f64) -> Screen {
        Screen {
            id,
            x_pt,
            y_pt,
            ..Screen::default()
        }
    }

    #[test]
    fn panel_height_starts_above_the_menu_bar_and_ends_at_the_dock() {
        let screen = Screen::default();
        assert_eq!(screen.inset_pt(), 12.0);
        assert_eq!(screen.usable_height_pt(), 820.0);
        assert_eq!(screen.height_pt() - screen.usable_height_pt(), 80.0);
    }

    #[test]
    fn displays_start_with_the_menu_bar_screen_then_read_left_to_right() {
        let list = ordered_displays(vec![
            (at(3, 1800.0, 0.0), "external".into(), false),
            (at(2, -1200.0, 240.0), "left".into(), false),
            (at(1, 0.0, 0.0), "built-in".into(), true),
        ]);
        let names: Vec<&str> = list.iter().map(|display| display.name.as_str()).collect();
        assert_eq!(names, ["built-in", "left", "external"]);
        assert_eq!(
            list.iter().map(|display| display.index).collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }
}
