//! App state, the view, key routing and background pumping.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use portable_pty::CommandBuilder;
use rune::tok::{self, Theme};
use rune::{
    Align, AppHandle, Color, Cx, DisplayTarget, Drag, El, Family, Key, KeyEvent, Mods,
    OverlayPlacement, Phase, PointerButton, ScreenEdge, ScrollSpec, Shortcut, ShortcutError,
    ShortcutId, TextEdit, TextStyle, Transition, Tx, col, el, row, spacer, text, text_input,
};

use crate::ffi;

use crate::config::{Config, Session, SessionTab};
use crate::macos::Screen;
use crate::selection::{CellPos, Kind, Selection, line_at, word_at};
use crate::term::pty::{Pty, Waker};
use crate::term::{
    ACCENT, CursorStyle, DEFAULT_BG, DEFAULT_FG, ERROR, FLAG_BOLD, FLAG_FAINT, Progress,
    ProgressState, Rgb, Vt,
};
use crate::textops::{
    WheelAccumulator, link_at, needs_paste_confirmation, paste_bytes, shell_quote,
};

pub const TAB_H: f32 = 36.0;
pub const PAD_X: f32 = 10.0;
pub const PAD_Y: f32 = 8.0;

/// How long the find field must be quiet before its text is handed to libghostty,
/// so typing does not rescan the scrollback for every key.
const FIND_QUIET: Duration = Duration::from_millis(80);
/// How long the font field must be quiet before it re-measures and writes config.json.
const FONT_COMMIT_QUIET: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub cw: f32,
    pub ch: f32,
    pub cols: u16,
    pub rows: u16,
}

pub struct Tab {
    pub vt: Vt,
    pub pty: Option<Pty>,
    pub dead: bool,
    pub last_title: String,
    /// Output arrived while this tab was not the one on screen.
    pub unread: bool,
    /// A visual bell rings until this frame time.
    pub bell_until: f64,
    pub progress: Option<Progress>,
}

impl Tab {
    pub fn new(
        cfg: &Config,
        screen: Screen,
        metrics: Metrics,
        demo: bool,
        waker: &Waker,
        cwd: Option<PathBuf>,
    ) -> Self {
        let cell_w = (metrics.cw * screen.scale as f32).round() as u32;
        let cell_h = (metrics.ch * screen.scale as f32).round() as u32;
        let mut vt = Vt::new(metrics.cols, metrics.rows, cell_w, cell_h);
        vt.set_scrollback(cfg.scrollback_lines);

        if demo {
            vt.write(DEMO_SCREEN.as_bytes());
            vt.refresh(true);
            return Self {
                vt,
                pty: None,
                dead: true,
                last_title: "sash".into(),
                unread: false,
                bell_until: 0.0,
                progress: None,
            };
        }

        let mut builder = match sash_command() {
            Some(command) => {
                let mut b = CommandBuilder::new("/bin/sh");
                b.args(["-c", &command]);
                b
            }
            None => {
                let shell = cfg
                    .shell
                    .clone()
                    .or_else(|| std::env::var("SHELL").ok())
                    .unwrap_or_else(|| "/bin/zsh".into());
                let mut b = CommandBuilder::new(&shell);
                b.arg("-l");
                b
            }
        };
        builder.env("SASH", "1");
        if let Some(dir) = cwd.filter(|dir| dir.is_dir()) {
            builder.cwd(dir);
        }

        match Pty::spawn(builder, metrics.cols, metrics.rows, cell_w, cell_h, "tab".into(), waker.clone()) {
            Ok(pty) => Self {
                vt,
                pty: Some(pty),
                dead: false,
                last_title: String::new(),
                unread: false,
                bell_until: 0.0,
                progress: None,
            },
            Err(e) => {
                let message = format!("\r\n\x1b[31msash: could not start a shell: {e}\x1b[0m\r\n");
                vt.write(message.as_bytes());
                vt.refresh(true);
                Self {
                    vt,
                    pty: None,
                    dead: true,
                    last_title: "sash".into(),
                    unread: false,
                    bell_until: 0.0,
                    progress: None,
                }
            }
        }
    }

    /// The shell's live directory: the kernel's answer first, then OSC 7.
    pub fn current_dir(&self) -> Option<PathBuf> {
        self.pty
            .as_ref()
            .and_then(|pty| pty.cwd())
            .or_else(|| self.vt.pwd().cloned())
    }

    /// Something other than the shell owns the pty: a command, an editor, a job.
    pub fn busy(&self) -> bool {
        !self.dead
            && self
                .pty
                .as_ref()
                .is_some_and(|pty| pty.has_running_command())
    }

    pub fn title(&self) -> String {
        let t = self.vt.title().trim();
        let t = if t.is_empty() { self.last_title.as_str() } else { t };
        if t.is_empty() {
            "shell".into()
        } else {
            truncate(t, 28)
        }
    }

    pub fn resize(&mut self, metrics: Metrics, screen: Screen) {
        let cell_w = (metrics.cw * screen.scale as f32).round() as u32;
        let cell_h = (metrics.ch * screen.scale as f32).round() as u32;
        self.vt.resize(metrics.cols, metrics.rows, cell_w, cell_h);
        if let Some(pty) = self.pty.as_ref() {
            pty.resize(metrics.cols, metrics.rows, cell_w, cell_h);
        }
    }
}

pub struct Sash {
    pub cfg: Config,
    pub screen: Screen,
    pub handle: AppHandle<Sash>,
    pub metrics: Option<Metrics>,
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub shown: bool,
    pub tray: Option<tray_icon::TrayIcon>,
    /// How much of the panel the window reveals, from the top edge down. The
    /// window itself stays put; the card is clipped to this height.
    pub reveal: f32,
    pub anim: Option<Anim>,
    pub now: f64,
    pub width: f32,
    pub height: f32,
    pub demo: bool,
    pub refined: bool,
    pub want_show: bool,
    /// The next frame re-resolves the display and places the window on it.
    pub want_place: bool,
    /// Wakes the UI thread when any tab's pty produces output.
    pub waker: Waker,
    /// The settings page replaces the terminal while it is open.
    pub settings: SettingsPane,
    /// The live global-shortcut registration, rotated when the chord changes.
    pub hotkey_id: Option<ShortcutId>,
    /// Set once the configured chord failed, so `poll` stops retrying it.
    pub hotkey_failed: bool,
    pub hotkey_error: Option<String>,
    /// Mouse selection over the active tab's grid.
    pub selection: Option<Selection>,
    /// Pixel wheel travel not yet turned into whole rows.
    pub wheel: WheelAccumulator,
    /// Text waiting for the paste confirmation.
    pub pending_paste: Option<String>,
    /// Modifiers held right now, for ⌘-click links.
    pub mods: Mods,
    /// Whether the panel currently holds the keyboard.
    pub focused: bool,
    /// Whether it held it since it was last shown.
    pub had_focus: bool,
    /// The shell path editor in settings.
    pub shell_edit: TextEdit,
    /// The font family editor in settings.
    pub font_edit: TextEdit,
    /// The configured font family, leaked on each real commit so `Family::Named`
    /// can borrow it for as long as the app lives.
    pub font_name: Option<&'static str>,
    /// The slider being dragged and its live value.
    pub slider: Option<(&'static str, f32)>,
    /// Last desktop notification time, so a chatty program can't flood Notification Center.
    pub last_notification: f64,
    /// Whether a pointer button is down, so plain motion is not reported twice.
    pub mouse_down: bool,
    /// The scrollback find bar.
    pub find: Find,
    /// A tab waiting on "a command is still running" confirmation.
    pub pending_close: Option<usize>,
    /// The quit waiting on the same confirmation.
    pub pending_quit: bool,
    /// The input method's marked text, drawn at the caret until it commits.
    pub preedit: Option<String>,
}

/// The find bar: a text field over the grid, plus the search's progress.
#[derive(Default)]
pub struct Find {
    pub open: bool,
    pub edit: TextEdit,
    /// The needle last handed to libghostty.
    pub needle: String,
    /// Whether the incremental search has caught up with the scrollback.
    pub complete: bool,
    pub total: usize,
    pub index: Option<usize>,
    /// When the field was last edited. A new needle only reaches libghostty once
    /// this is older than `FIND_QUIET`, or at once on Return and ⌘G.
    pub last_edit: Option<Instant>,
}

/// State for the in-panel settings page.
#[derive(Default)]
pub struct SettingsPane {
    pub open: bool,
    /// Waiting for the next key combination to become the global shortcut.
    pub capturing: bool,
    /// Slider position while dragging, as a fraction of the usable screen height.
    pub height_drag: Option<f32>,
    /// A note under the controls: a shortcut conflict or a hint.
    pub message: Option<String>,
    /// When the font field last changed, so the config write and re-measure wait
    /// for a pause in the typing.
    pub font_edit_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug)]
pub struct Anim {
    pub from: f32,
    pub to: f32,
    /// Set on the first frame after the request, so a handler that runs before
    /// the frame's poll can't make the slide start in the past.
    pub start: Option<f64>,
    pub duration: f64,
    pub hide_after: bool,
}

impl Sash {
    pub fn new(cfg: Config, screen: Screen, handle: AppHandle<Sash>, width: f32, height: f32) -> Self {
        let demo = std::env::var_os("SASH_DEMO").is_some();
        let settings_open = std::env::var_os("SASH_SETTINGS").is_some();
        // Dev and demo hook: open the paste confirmation on a given text.
        let pending_paste = std::env::var("SASH_PASTE").ok();
        // Programs may only replace the clipboard when the user allowed it.
        crate::term::set_clipboard_write(cfg.clipboard_write);
        let metrics = provisional_metrics(&cfg, width, height);
        let waker: Waker = {
            let handle = handle.clone();
            Arc::new(move || handle.wake())
        };
        let restoring = cfg.restore_session && !demo && sash_command().is_none();
        let session = if restoring { Session::load() } else { Session::default() };
        let mut tabs: Vec<Tab> = session
            .tabs
            .iter()
            .take(12)
            .map(|tab| Tab::new(&cfg, screen, metrics, demo, &waker, tab.cwd.clone()))
            .collect();
        if tabs.is_empty() {
            tabs.push(Tab::new(&cfg, screen, metrics, demo, &waker, None));
        }
        let active = session.active.min(tabs.len().saturating_sub(1));
        let shell_edit = TextEdit::new(cfg.shell.clone().unwrap_or_default());
        let font_edit = TextEdit::new(cfg.font_family.clone().unwrap_or_default());
        let font_name = cfg
            .font_family
            .as_deref()
            .map(|name| &*Box::leak(name.to_owned().into_boxed_str()));
        Self {
            cfg,
            screen,
            handle,
            metrics: Some(metrics),
            tabs,
            active,
            shown: false,
            tray: None,
            reveal: 0.0,
            anim: None,
            now: 0.0,
            width,
            height,
            demo,
            refined: false,
            want_show: settings_open || std::env::var_os("SASH_SHOW").is_some(),
            want_place: false,
            waker,
            settings: SettingsPane { open: settings_open, ..Default::default() },
            hotkey_id: None,
            hotkey_failed: false,
            hotkey_error: None,
            selection: None,
            wheel: WheelAccumulator::default(),
            pending_paste,
            mods: Mods::default(),
            focused: false,
            had_focus: false,
            shell_edit,
            font_edit,
            font_name,
            slider: None,
            last_notification: f64::NEG_INFINITY,
            mouse_down: false,
            find: Find::default(),
            pending_close: None,
            pending_quit: false,
            preedit: None,
        }
    }

    pub fn theme(&self) -> Theme {
        tok::theme(true)
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    pub fn metrics(&self) -> Metrics {
        self.metrics.unwrap_or_else(|| provisional_metrics(&self.cfg, self.width, self.height))
    }

    /// The grid cell under a point in the terminal element's local space, which
    /// includes its padding.
    pub fn cell_at(&self, x: f32, y: f32) -> Option<CellPos> {
        let m = self.metrics();
        if m.cw <= 0.0 || m.ch <= 0.0 {
            return None;
        }
        let (x, y) = (x - PAD_X, y - PAD_Y);
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let (col, row) = ((x / m.cw) as u32, (y / m.ch) as u32);
        if col >= m.cols as u32 || row >= m.rows as u32 {
            return None;
        }
        Some(CellPos { x: col as u16, y: row as u16 })
    }

    /// Pass one pointer event to the running program, in the terminal's pixel
    /// space (the encoder was sized with physical cells).
    fn send_mouse(
        &mut self,
        action: ffi::GhosttyMouseAction,
        button: Option<ffi::GhosttyMouseButton>,
        x: f32,
        y: f32,
    ) {
        let scale = self.screen.scale as f32;
        let px = ((x - PAD_X).max(0.0) * scale).round();
        let py = ((y - PAD_Y).max(0.0) * scale).round();
        let mods = mouse_mods(&self.mods);
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        let bytes = tab.vt.encode_mouse(action, button, px, py, mods);
        if let Some(pty) = tab.pty.as_mut() {
            pty.write(&bytes);
        }
        tab.vt.scroll_to_bottom();
    }

    fn wheel_mouse(&mut self, x: f32, y: f32, up: bool, count: i32) {
        let button = if up {
            ffi::GHOSTTY_MOUSE_BUTTON_FOUR
        } else {
            ffi::GHOSTTY_MOUSE_BUTTON_FIVE
        };
        for _ in 0..count.clamp(1, 5) {
            self.send_mouse(ffi::GHOSTTY_MOUSE_ACTION_PRESS, Some(button), x, y);
        }
    }

    /// The URL printed in the cell under the pointer, for ⌘-click.
    pub fn link_at(&self, at: CellPos) -> Option<String> {
        let tab = self.active_tab()?;
        if let Some(url) = tab.vt.hyperlink_at(at.x, at.y) {
            return Some(url);
        }
        let grid = tab.vt.grid();
        let mut row = String::with_capacity(grid.cols as usize);
        for x in 0..grid.cols {
            match grid.cell(x, at.y) {
                Some(cell) if !cell.text.is_empty() => row.push_str(&cell.text),
                _ => row.push(' '),
            }
        }
        link_at(&row, at.x as usize)
    }

    /// Re-measure the monospace cell once the app's fonts are loaded.
    pub fn init_metrics(&mut self) -> bool {
        let style = self.run_style(400.0);
        let advance = rune::text::advance("MMMMMMMMMM", style);
        if !advance.is_finite() || advance <= 0.0 {
            return false;
        }
        let cw = (advance / 10.0 * 64.0).round() / 64.0;
        let ch = (self.cfg.font_size * 1.32).round().max(10.0);
        let usable_w = self.width - PAD_X * 2.0;
        let usable_h = self.height - TAB_H - PAD_Y * 2.0;
        let cols = ((usable_w / cw).floor() as u16).max(20);
        let rows = ((usable_h / ch).floor() as u16).max(4);
        let metrics = Metrics { cw, ch, cols, rows };
        self.metrics = Some(metrics);
        for tab in &mut self.tabs {
            tab.resize(metrics, self.screen);
        }
        true
    }

    pub fn term_family(&self) -> Family {
        self.font_name.map(Family::Named).unwrap_or(Family::Mono)
    }

    pub fn run_style(&self, weight: f32) -> TextStyle {
        TextStyle::new(self.cfg.font_size, weight)
            .family(self.term_family())
            .lh(self.metrics.map(|m| m.ch).unwrap_or(17.0))
    }

    pub fn small_style(&self, weight: f32, size: f32) -> TextStyle {
        TextStyle::new(size, weight).family(Family::Mono)
    }

    pub fn toggle(&mut self) {
        if self.shown {
            self.hide();
        } else {
            self.show();
        }
    }

    /// Register the configured chord once the event loop can take it. Called from `poll`
    /// until it succeeds; a chord already in use leaves an error for the settings page.
    pub fn ensure_hotkey(&mut self) -> bool {
        if self.hotkey_id.is_some() || self.hotkey_failed {
            return false;
        }
        match self.cfg.hotkey.parse::<Shortcut>() {
            Ok(shortcut) => match self.handle.register_shortcut(shortcut, hotkey_toggle) {
                Ok(id) => {
                    self.hotkey_id = Some(id);
                    self.hotkey_error = None;
                    eprintln!("sash: hotkey ready ({})", self.cfg.hotkey);
                    true
                }
                // The window host (and its shortcut routing) may not exist yet.
                Err(ShortcutError::NotRunning) => false,
                Err(e) => {
                    eprintln!("sash: could not register shortcut {}: {e}", self.cfg.hotkey);
                    self.hotkey_failed = true;
                    self.hotkey_error = Some(e.to_string());
                    true
                }
            },
            Err(e) => {
                eprintln!("sash: invalid shortcut {}: {e}", self.cfg.hotkey);
                self.hotkey_failed = true;
                self.hotkey_error = Some(e.to_string());
                true
            }
        }
    }

    pub fn open_settings(&mut self) {
        self.close_find();
        self.settings.open = true;
        self.settings.message = None;
        self.pending_paste = None;
        self.preedit = None;
        if !self.shown {
            self.show();
        }
    }

    pub fn close_settings(&mut self) {
        let was_open = self.settings.open;
        if self.settings.capturing {
            self.cancel_capture();
        }
        // Closing blurs the field, so a typed font name must not be lost with it.
        if was_open {
            self.commit_font_edit();
        }
        self.settings.open = false;
        if was_open && let Some(tab) = self.tabs.get_mut(self.active) {
            tab.unread = false;
        }
    }

    pub fn toggle_settings(&mut self) {
        if self.settings.open {
            self.close_settings();
        } else {
            self.open_settings();
        }
    }

    /// Stop the old chord first, so pressing it can be recorded instead of toggling the panel.
    pub fn begin_capture(&mut self) {
        if self.settings.capturing {
            return;
        }
        self.settings.message = None;
        if let Some(id) = self.hotkey_id.take() {
            let _ = self.handle.unregister_shortcut(id);
        }
        self.settings.capturing = true;
    }

    pub fn cancel_capture(&mut self) {
        if !self.settings.capturing {
            return;
        }
        self.settings.capturing = false;
        self.restore_hotkey();
    }

    fn restore_hotkey(&mut self) {
        if self.hotkey_id.is_some() {
            return;
        }
        self.hotkey_failed = false;
        self.ensure_hotkey();
    }

    /// Turn the captured key event into a global shortcut, register it, and save it.
    fn capture_key(&mut self, ev: &KeyEvent, tx: &mut Tx) {
        let Some(code) = tx.physical_key() else {
            self.settings.message = Some("That key can't be used as a shortcut.".into());
            return;
        };
        let built = Shortcut::from_physical_key(code, ev.mods)
            .and_then(|shortcut| shortcut.require_modifier().map(|_| shortcut));
        match built {
            Ok(shortcut) => match self.handle.register_shortcut(shortcut.clone(), hotkey_toggle) {
                Ok(id) => {
                    self.hotkey_id = Some(id);
                    self.cfg.hotkey = shortcut.to_string();
                    self.cfg.save();
                    self.settings.capturing = false;
                    self.settings.message = None;
                    self.hotkey_failed = false;
                    self.hotkey_error = None;
                }
                Err(e) => {
                    let owner = self.handle.shortcut_owner(&shortcut);
                    self.settings.capturing = false;
                    self.restore_hotkey();
                    self.settings.message = Some(match owner {
                        Some(owner) => format!("That shortcut is taken by {owner}."),
                        None => format!("That shortcut is already in use ({e})."),
                    });
                }
            },
            // No modifier (or a key Rune can't register): keep listening so the user can
            // try the combination again, or press Esc.
            Err(e) => self.settings.message = Some(e.to_string()),
        }
    }

    pub fn set_always_on_top(&mut self, on: bool) {
        if self.cfg.always_on_top == on {
            return;
        }
        self.cfg.always_on_top = on;
        self.cfg.save();
        if let Some(window) = crate::window() {
            window.set_always_on_top(on);
        }
    }

    pub fn set_focus_on_open(&mut self, on: bool) {
        if self.cfg.focus_on_open == on {
            return;
        }
        self.cfg.focus_on_open = on;
        self.cfg.save();
    }

    pub fn set_hide_on_blur(&mut self, on: bool) {
        if self.cfg.hide_on_blur != on {
            self.cfg.hide_on_blur = on;
            self.cfg.save();
        }
    }

    pub fn set_hide_on_escape(&mut self, on: bool) {
        if self.cfg.hide_on_escape != on {
            self.cfg.hide_on_escape = on;
            self.cfg.save();
        }
    }

    pub fn set_copy_on_select(&mut self, on: bool) {
        if self.cfg.copy_on_select != on {
            self.cfg.copy_on_select = on;
            self.cfg.save();
        }
    }

    /// OSC 52 and the Kitty/iTerm2 clipboard sequences write the system clipboard
    /// only while this is on.
    pub fn set_clipboard_write(&mut self, on: bool) {
        if self.cfg.clipboard_write != on {
            self.cfg.clipboard_write = on;
            self.cfg.save();
        }
        crate::term::set_clipboard_write(on);
    }

    pub fn set_confirm_paste(&mut self, on: bool) {
        if self.cfg.confirm_multiline_paste != on {
            self.cfg.confirm_multiline_paste = on;
            self.cfg.save();
        }
    }

    pub fn set_desktop_notifications(&mut self, on: bool) {
        if self.cfg.desktop_notifications != on {
            self.cfg.desktop_notifications = on;
            self.cfg.save();
        }
    }

    pub fn set_animation(&mut self, on: bool) {
        let ms = if on { 140 } else { 0 };
        if self.cfg.animation_ms != ms {
            self.cfg.animation_ms = ms;
            self.cfg.save();
        }
    }

    pub fn set_font_size(&mut self, size: f32) {
        let size = (size * 2.0).round() / 2.0;
        if (self.cfg.font_size - size).abs() < f32::EPSILON {
            return;
        }
        self.cfg.font_size = size;
        self.init_metrics();
        self.cfg.save();
    }

    pub fn set_opacity(&mut self, opacity: f32) {
        let opacity = opacity.clamp(0.55, 1.0);
        if (self.cfg.opacity - opacity).abs() < f32::EPSILON {
            return;
        }
        self.cfg.opacity = opacity;
        self.cfg.save();
    }

    pub fn set_scrollback_lines(&mut self, lines: u32) {
        self.cfg.scrollback_lines = Some(lines);
        for tab in &mut self.tabs {
            tab.vt.set_scrollback(Some(lines));
        }
        self.cfg.save();
    }

    /// `fraction` of the display width; 1.0 stores "full width".
    pub fn set_width_fraction(&mut self, fraction: f32, tx: &mut Tx) {
        let full = self.screen.width_pt().max(320.0);
        let fraction = fraction.clamp(0.35, 1.0);
        self.cfg.width = if fraction >= 0.995 {
            None
        } else {
            Some((full * fraction).round())
        };
        self.apply_width(tx);
        self.cfg.save();
    }

    fn apply_width(&mut self, tx: &mut Tx) {
        self.width = self
            .cfg
            .width
            .unwrap_or_else(|| self.screen.width_pt())
            .clamp(320.0, self.screen.width_pt());
        self.init_metrics();
        tx.resize_window_instant((self.width, self.height));
        self.place();
    }

    /// The shell field writes through to the config as it is typed, so new tabs
    /// use it without a Save button.
    pub fn sync_shell_edit(&mut self) -> bool {
        if !self.settings.open {
            return false;
        }
        let text = self.shell_edit.text.trim().to_string();
        let stored = self.cfg.shell.clone().unwrap_or_default();
        if text == stored {
            return false;
        }
        self.cfg.shell = (!text.is_empty()).then_some(text);
        self.cfg.save();
        true
    }

    /// ⌘F: show the find bar over the active tab and select its text.
    pub fn open_find(&mut self) {
        if self.pending_paste.is_some() {
            return;
        }
        self.close_settings();
        self.find.open = true;
        self.find.edit.focused = true;
        // Start from the selection when it is one line, as other terminals do.
        if let Some(text) = self.selection_text()
            && text.lines().count() == 1
            && !text.trim().is_empty()
        {
            self.find.edit = TextEdit::new(text.trim().to_string());
            self.find.edit.focused = true;
        }
        self.find.needle.clear();
        self.find.complete = false;
        self.find.last_edit = None;
    }

    pub fn close_find(&mut self) {
        if !self.find.open {
            return;
        }
        self.find.open = false;
        self.find.edit.focused = false;
        self.find.needle.clear();
        self.find.complete = false;
        self.find.total = 0;
        self.find.index = None;
        self.find.last_edit = None;
        if let Some(tab) = self.active_tab_mut() {
            tab.vt.search_clear();
        }
        self.selection = None;
    }

    /// Hand the field's text to libghostty, starting a fresh scan (or clearing the
    /// results of an empty needle). `find_next` also calls it, so Return and ⌘G
    /// never wait for the quiet period.
    fn start_find(&mut self) {
        self.find.last_edit = None;
        if self.find.edit.text == self.find.needle {
            return;
        }
        self.find.needle = self.find.edit.text.clone();
        self.find.complete = false;
        self.find.total = 0;
        self.find.index = None;
        let needle = self.find.needle.clone();
        if let Some(tab) = self.active_tab_mut() {
            if needle.is_empty() {
                tab.vt.search_clear();
            } else {
                let _ = tab.vt.search_start(&needle);
            }
        }
    }

    /// Move to the next (older) or previous (newer) match.
    pub fn find_next(&mut self, forward: bool) {
        if !self.find.open {
            return;
        }
        // Return and ⌘G jump at once, with whatever the field holds now.
        self.start_find();
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        if tab.vt.search_select(forward) {
            let (total, index) = tab.vt.search_counts();
            self.find.total = total;
            self.find.index = index;
            self.find.complete = true;
        }
        self.selection = None;
    }

    /// Follow the field, advance an unfinished search, and refresh its counts.
    /// Returns true only when something visible changed.
    pub fn sync_find(&mut self) -> bool {
        if !self.find.open {
            return false;
        }
        // Escape and clicking elsewhere both blur the field; either way the find
        // bar has done its job and should get out of the way.
        if !self.find.edit.focused {
            self.close_find();
            return true;
        }
        if self.find.edit.text != self.find.needle {
            // Wait for the typing to pause; the field's change handler stamped the
            // last edit and asked for a wake once the quiet period is over.
            if let Some(at) = self.find.last_edit
                && at.elapsed() < FIND_QUIET
            {
                return false;
            }
            self.start_find();
            return true;
        }
        self.find.last_edit = None;
        if self.find.complete || self.find.needle.is_empty() {
            return false;
        }
        let mut complete = false;
        let mut counts = (0usize, None);
        if let Some(tab) = self.active_tab_mut() {
            complete = tab.vt.search_poll();
            counts = tab.vt.search_counts();
            if complete && counts.0 > 0 && counts.1.is_none() {
                let _ = tab.vt.search_select(true);
                counts = tab.vt.search_counts();
            }
        }
        let changed = complete != self.find.complete
            || counts.0 != self.find.total
            || counts.1 != self.find.index;
        self.find.complete = complete;
        self.find.total = counts.0;
        self.find.index = counts.1;
        changed
    }

    /// The font field commits after a quiet pause, on Return, or when it loses
    /// focus, so typing does not re-measure every tab and write config.json per key.
    pub fn sync_font_edit(&mut self) -> bool {
        if !self.settings.open {
            return false;
        }
        if self.font_edit.text.trim() == self.font_name.unwrap_or_default() {
            // The field already names the current family: an undo back to it needs
            // no write and no leak.
            self.settings.font_edit_at = None;
            return false;
        }
        let quiet = self
            .settings
            .font_edit_at
            .is_some_and(|at| at.elapsed() >= FONT_COMMIT_QUIET);
        if self.font_edit.focused && !quiet {
            return false;
        }
        self.commit_font_edit()
    }

    /// Apply the font field now. Return, blur, the quiet timer and closing the
    /// settings page all land here. Only a real change leaks the new name.
    fn commit_font_edit(&mut self) -> bool {
        self.settings.font_edit_at = None;
        let text = self.font_edit.text.trim();
        if text == self.font_name.unwrap_or_default() {
            return false;
        }
        let name = (!text.is_empty()).then(|| text.to_owned());
        self.font_name = name
            .as_deref()
            .map(|name| &*Box::leak(name.to_owned().into_boxed_str()));
        self.cfg.font_family = name;
        self.cfg.save();
        self.init_metrics();
        true
    }

    /// `fraction` of the usable height (menu bar to Dock); 1.0 stores "full".
    pub fn set_height_fraction(&mut self, fraction: f32, tx: &mut Tx) {
        let full = self.screen.usable_height_pt().max(120.0);
        let fraction = fraction.clamp(0.2, 1.0);
        self.cfg.height = if fraction >= 0.995 {
            None
        } else {
            Some((full * fraction).round())
        };
        self.apply_height(tx);
        self.cfg.save();
    }

    /// Resize the panel and every tab's grid to the configured height.
    fn apply_height(&mut self, tx: &mut Tx) {
        let height = panel_height(self.cfg.height, self.screen.usable_height_pt());
        self.height = height;
        self.init_metrics();
        if self.shown {
            if self.anim.is_none() {
                self.reveal = height;
            } else if let Some(anim) = self.anim.as_mut()
                && anim.to > 0.0
            {
                anim.to = height;
            }
            tx.resize_window_instant((self.width, height));
        }
    }

    /// Re-resolve the display under the pointer before the next reveal, and ask Rune
    /// to put the panel at its top edge. The panel follows the pointer across screens.
    pub fn sync_display(&mut self, tx: &mut Tx) -> bool {
        if !self.want_place || crate::window().is_none() {
            return false;
        }
        self.want_place = false;
        let Some(screen) = crate::macos::screen_under_pointer()
            .or_else(crate::macos::main_screen)
        else {
            return false;
        };
        let changed = screen.width_px != self.screen.width_px
            || screen.usable_height_px != self.screen.usable_height_px
            || screen.scale != self.screen.scale;
        self.screen = screen;
        if changed {
            self.width = self
                .cfg
                .width
                .unwrap_or_else(|| screen.width_pt())
                .clamp(320.0, screen.width_pt());
            self.height = panel_height(self.cfg.height, screen.usable_height_pt());
            self.init_metrics();
            tx.resize_window_instant((self.width, self.height));
            if self.shown {
                if let Some(anim) = self.anim.as_mut()
                    && anim.to > 0.0
                {
                    anim.to = self.height;
                } else {
                    self.reveal = self.height;
                }
            }
        }
        self.place();
        changed
    }

    /// Place the window on the pointer's display, just below its menu bar. Rune handles
    /// mixed backing scales and display origins; `margin` is that screen's inset.
    fn place(&self) {
        if let Some(window) = crate::window() {
            window.place(
                DisplayTarget::UnderPointer,
                OverlayPlacement::Edge {
                    edge: ScreenEdge::Top,
                    margin: self.screen.inset_pt(),
                    align: Align::Center,
                    inset: 0.0,
                },
            );
        }
    }

    pub fn show(&mut self) {
        if self.shown {
            return;
        }
        if self.tabs.is_empty() {
            let metrics = self.metrics();
            self.tabs.push(Tab::new(&self.cfg, self.screen, metrics, self.demo, &self.waker, None));
            self.active = 0;
        }
        self.want_place = true;
        self.shown = true;
        // Asking for keys counts as holding them, even before the platform's focus
        // event lands: otherwise a hide-on-blur would never fire for this reveal.
        self.had_focus = self.cfg.focus_on_open;
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.unread = false;
        }
        // A show that reverses a hide continues from where the panel is; one that
        // follows a finished hide starts from the top edge again.
        if !self.handle.is_visible() {
            self.reveal = 0.0;
        }
        // `open` asks for keys (without activating an accessory app); `show` leaves the
        // focused app alone, which is what "focus on open" off means.
        if self.cfg.focus_on_open {
            self.handle.open();
        } else {
            self.handle.show();
        }
        // Only the tab on screen needs the live viewport; the others keep their place.
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.vt.scroll_to_bottom();
        }
        self.animate_to(self.height, false);
    }

    pub fn hide(&mut self) {
        // `shown` can already be false while a hide is still sliding; let the
        // visible window finish going away instead of leaving it on screen.
        if !self.shown && !self.handle.is_visible() {
            return;
        }
        if self.settings.open {
            self.close_settings();
        }
        self.close_find();
        self.cancel_pending();
        self.shown = false;
        self.had_focus = false;
        self.selection = None;
        self.pending_paste = None;
        self.preedit = None;
        self.animate_to(0.0, true);
    }

    fn animate_to(&mut self, to: f32, hide_after: bool) {
        // A hand-edited config can ask for minutes; cap the slide at two seconds.
        let duration = (self.cfg.animation_ms as f64 / 1000.0).min(2.0);
        if duration <= 0.001 {
            self.reveal = to;
            self.anim = None;
            if hide_after && !self.shown {
                self.handle.hide();
            }
            return;
        }
        self.anim = Some(Anim { from: self.reveal, to, start: None, duration, hide_after });
    }

    /// Advance the slide; returns true while the panel is moving.
    pub fn tick_animation(&mut self) -> bool {
        let Some(mut anim) = self.anim else { return false };
        let Some(start) = anim.start else {
            anim.start = Some(self.now);
            self.anim = Some(anim);
            return true;
        };
        let t = ((self.now - start) / anim.duration).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - t as f32).powi(3);
        self.reveal = anim.from + (anim.to - anim.from) * eased;
        if t >= 1.0 {
            self.reveal = anim.to;
            self.anim = None;
            // A show() in the same frame may have taken over; only finish a hide
            // that is still wanted.
            if anim.hide_after && !self.shown {
                self.handle.hide();
            }
        }
        true
    }

    pub fn new_tab(&mut self) {
        self.spawn_tab(None);
    }

    /// ⌘D: a tab in the active shell's live directory.
    pub fn duplicate_tab(&mut self) {
        let cwd = self.active_tab().and_then(|tab| tab.current_dir());
        self.spawn_tab(cwd);
    }

    fn spawn_tab(&mut self, cwd: Option<PathBuf>) {
        self.close_settings();
        let metrics = self.metrics();
        let tab = Tab::new(&self.cfg, self.screen, metrics, self.demo, &self.waker, cwd);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.selection = None;
        self.wheel.reset();
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.unread = false;
        }
        if !self.shown {
            self.show();
        }
    }

    /// Persist the tab list for the next launch.
    pub fn save_session(&self) {
        if self.demo || sash_command().is_some() {
            return;
        }
        let session = Session {
            tabs: self
                .tabs
                .iter()
                .map(|tab| SessionTab { cwd: tab.current_dir() })
                .collect(),
            active: self.active,
        };
        // An empty tab list would restore nothing, and the next launch makes a
        // fresh tab anyway.
        if session.tabs.is_empty() {
            Session::clear();
        } else {
            session.save();
        }
    }

    /// ⌘W and the tab's × go through here: a busy tab asks first.
    pub fn request_close_tab(&mut self, index: usize) {
        let busy = self.tabs.get(index).is_some_and(Tab::busy);
        if busy && self.cfg.confirm_close {
            self.pending_close = Some(index);
            self.pending_quit = false;
            return;
        }
        self.close_tab(index);
    }

    pub fn request_quit(&mut self) {
        let busy = self.tabs.iter().any(Tab::busy);
        if busy && self.cfg.confirm_close {
            self.pending_quit = true;
            self.pending_close = None;
            if !self.shown {
                self.show();
            }
            return;
        }
        self.save_session();
        self.handle.quit();
    }

    pub fn confirm_pending(&mut self) {
        if self.pending_quit {
            self.pending_quit = false;
            self.save_session();
            self.handle.quit();
            return;
        }
        if let Some(index) = self.pending_close.take() {
            self.close_tab(index);
        }
    }

    pub fn cancel_pending(&mut self) {
        self.pending_close = None;
        self.pending_quit = false;
    }

    pub fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.tabs.remove(index);
        self.selection = None;
        self.pending_paste = None;
        self.wheel.reset();
        if self.find.open {
            self.find.needle.clear();
            self.find.complete = false;
            self.find.last_edit = None;
        }
        if self.tabs.is_empty() {
            self.save_session();
            self.hide();
            return;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if index < self.active {
            self.active -= 1;
        }
        self.save_session();
    }

    pub fn select(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.close_settings();
            self.active = index;
            self.selection = None;
            self.wheel.reset();
            if self.find.open {
                self.find.needle.clear();
                self.find.complete = false;
                self.find.last_edit = None;
            }
            if let Some(tab) = self.tabs.get_mut(index) {
                tab.unread = false;
            }
        }
    }

    pub fn cycle(&mut self, delta: isize) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as isize;
        self.active = (((self.active as isize + delta) % n + n) % n) as usize;
        self.selection = None;
        self.wheel.reset();
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.unread = false;
        }
    }

    /// The panel took or lost the keyboard. Losing it while shown is the Quake hide,
    /// unless the user turned that off or the panel never had focus to lose.
    pub fn focus_changed(&mut self, focused: bool) {
        self.focused = focused;
        if focused {
            self.had_focus = true;
        } else {
            self.preedit = None;
            if self.shown && self.cfg.hide_on_blur && self.had_focus {
                self.hide();
            }
        }
    }

    pub fn set_modifiers(&mut self, mods: Mods) {
        self.mods = mods;
    }

    /// Input-method events for the terminal. A focused text field keeps its own
    /// composition; everything the input method commits goes to the pty as text.
    pub fn ime(&mut self, ev: &rune::Ime) -> bool {
        use rune::Ime;
        if (self.settings.open && (self.shell_edit.focused || self.font_edit.focused))
            || (self.find.open && self.find.edit.focused)
        {
            return false;
        }
        match ev {
            Ime::Preedit(text, ..) => {
                self.preedit = (!text.is_empty()).then(|| text.clone());
                true
            }
            Ime::Commit(text) => {
                self.preedit = None;
                let Some(tab) = self.active_tab_mut() else {
                    return true;
                };
                if let Some(pty) = tab.pty.as_mut() {
                    pty.write(text.as_bytes());
                }
                tab.vt.scroll_to_bottom();
                self.selection = None;
                true
            }
            // A composition the input method cancels leaves no ghost text behind;
            // claim the event so the cleared caret area is drawn.
            Ime::Disabled | Ime::Select(_) => {
                self.preedit = None;
                true
            }
            Ime::Enabled => false,
        }
    }

    /// Drain pty output into the terminal state. Returns true when anything visible changed.
    pub fn pump(&mut self) -> bool {
        let mut changed = false;
        let (active, visible) = (self.active, self.shown && !self.settings.open);
        let now = self.now;
        let mut announcements = Vec::new();
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            if let Some(pty) = tab.pty.as_mut() {
                let bytes = pty.drain();
                if !bytes.is_empty() {
                    tab.vt.write(&bytes);
                    changed = true;
                    if !(visible && index == active) {
                        tab.unread = true;
                    }
                }
                let replies = tab.vt.take_replies();
                if !replies.is_empty() {
                    pty.write(&replies);
                }
                if pty.eof() && !tab.dead {
                    tab.dead = true;
                    // A child that exited on its own leaves a zombie until it is
                    // waited on; the pty being dropped is not enough.
                    let _ = pty.try_wait();
                    tab.vt.write(b"\r\n\x1b[90m[sash] process exited. \xe2\x8c\x98T opens a new tab.\x1b[0m\r\n");
                    changed = true;
                }
            }
            if tab.vt.take_bell() {
                tab.bell_until = now + 0.45;
                changed = true;
                if !(visible && index == active) {
                    tab.unread = true;
                }
            }
            let progress = tab.vt.progress();
            if progress != tab.progress {
                tab.progress = progress;
                changed = true;
            }
            let notes = tab.vt.take_notifications();
            if !notes.is_empty() {
                if !(visible && index == active) {
                    tab.unread = true;
                }
                announcements.extend(notes);
                changed = true;
            }
            if let Some(end) = tab.vt.take_command_end() {
                changed = true;
                let focused = self.focused && index == active;
                if !focused {
                    let failed = end.exit.is_some_and(|code| code != 0) || !end.error.is_empty();
                    let title = if failed { "Command failed" } else { "Command finished" };
                    let mut body = tab.title();
                    if !end.command.trim().is_empty() {
                        body = format!("{} · {}", body, end.command.trim());
                    }
                    if let Some(code) = end.exit
                        && code != 0
                    {
                        body = format!("{body} (exit {code})");
                    }
                    announcements.push(crate::term::Notification {
                        title: title.into(),
                        body,
                    });
                }
            }
            if tab.vt.refresh(false) {
                changed = true;
            }
            if tab.vt.title() != tab.last_title {
                tab.last_title = tab.vt.title().to_string();
                changed = true;
            }
        }
        self.announce(announcements);
        changed
    }

    /// Desktop notifications only when the panel is not already in front of the user,
    /// at most one per this many seconds, so a chatty program cannot flood the screen.
    fn announce(&mut self, notes: Vec<crate::term::Notification>) {
        if notes.is_empty() || !self.cfg.desktop_notifications {
            return;
        }
        if self.shown && self.focused {
            return;
        }
        const QUIET: f64 = 1.5;
        for note in notes {
            if self.now - self.last_notification < QUIET {
                break;
            }
            self.last_notification = self.now;
            notify(&note.title, &note.body);
        }
    }

    pub fn poll_tray(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
            match event.id().0.as_str() {
                "toggle" => {
                    self.toggle();
                    changed = true;
                }
                "new-tab" => {
                    self.new_tab();
                    changed = true;
                }
                "settings" => {
                    self.open_settings();
                    changed = true;
                }
                "quit" => self.request_quit(),
                _ => {}
            }
        }
        changed
    }

    /// The selected cell range's text, trimmed the way a terminal copies it.
    pub fn selection_text(&self) -> Option<String> {
        let selection = self.selection?;
        let grid = self.active_tab()?.vt.grid();
        let text = selection.text(grid);
        (!text.trim().is_empty()).then_some(text)
    }

    pub fn copy_selection(&mut self, tx: &mut Tx) {
        if let Some(text) = self.selection_text() {
            tx.copy(text);
        }
    }

    pub fn select_all(&mut self) {
        let Some(grid) = self.active_tab().map(|tab| tab.vt.grid()) else {
            return;
        };
        if grid.cols == 0 || grid.rows == 0 {
            return;
        }
        let mut selection = Selection::new(CellPos { x: 0, y: 0 }, Kind::Cells);
        selection.extend(CellPos { x: grid.cols - 1, y: grid.rows - 1 });
        self.selection = Some(selection);
    }

    /// ⌘V: read the clipboard, and ask first when the text spans lines.
    pub fn paste(&mut self) {
        if self.pending_paste.is_some() {
            return;
        }
        let Ok(mut clipboard) = arboard::Clipboard::new() else {
            return;
        };
        // A file copied in Finder arrives as a file list; terminals paste its path.
        if let Ok(files) = clipboard.get().file_list()
            && !files.is_empty()
        {
            let text = files
                .iter()
                .map(|path| shell_quote(&path.to_string_lossy()))
                .collect::<Vec<_>>()
                .join(" ");
            self.deliver_paste(text);
            return;
        }
        let Ok(text) = clipboard.get_text() else {
            return;
        };
        if self.cfg.confirm_multiline_paste && needs_paste_confirmation(&text) {
            self.pending_paste = Some(text);
            return;
        }
        self.deliver_paste(text);
    }

    pub fn confirm_paste(&mut self) {
        if let Some(text) = self.pending_paste.take() {
            self.deliver_paste(text);
        }
    }

    pub fn cancel_paste(&mut self) {
        self.pending_paste = None;
    }

    fn deliver_paste(&mut self, text: String) {
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        let bytes = paste_bytes(&text, tab.vt.bracketed_paste());
        if let Some(pty) = tab.pty.as_mut() {
            pty.write(&bytes);
        }
        tab.vt.scroll_to_bottom();
        self.selection = None;
    }

    /// Scroll by `rows`, positive toward older output.
    fn scroll_by(&mut self, rows: i32) {
        if rows == 0 {
            return;
        }
        if let Some(tab) = self.active_tab_mut() {
            // libghostty-vt's delta is negative for up.
            tab.vt.scroll(-rows);
        }
        self.selection = None;
    }

    /// Route one key: settings capture, app chords, then the terminal's encoder.
    pub fn key(&mut self, ev: &KeyEvent, tx: &mut Tx) {
        if self.settings.capturing {
            match ev.key {
                Key::Escape if !ev.mods.cmd && !ev.mods.ctrl && !ev.mods.alt => {
                    self.cancel_capture()
                }
                Key::Modifier { .. } => {}
                _ => self.capture_key(ev, tx),
            }
            return;
        }
        // The paste confirmation is modal for text keys, but app chords still work.
        if self.pending_paste.is_some() {
            match ev.key {
                Key::Enter => {
                    self.confirm_paste();
                    return;
                }
                Key::Escape => {
                    self.cancel_paste();
                    return;
                }
                Key::Command(_) => {}
                _ => return,
            }
        }
        if self.pending_close.is_some() || self.pending_quit {
            match ev.key {
                Key::Enter => self.confirm_pending(),
                Key::Escape => self.cancel_pending(),
                _ => {}
            }
            return;
        }
        // A focused settings field owns editing keys; a paste here must not fall
        // through to the terminal behind the settings page.
        if self.settings.open && (self.shell_edit.focused || self.font_edit.focused) {
            if ev.key == Key::Escape {
                self.close_settings();
            }
            return;
        }
        if self.find.open {
            if ev.key == Key::Command('f') {
                self.close_find();
                return;
            }
            if self.find.edit.focused {
                match ev.key {
                    Key::Command('g') => self.find_next(!ev.mods.shift),
                    Key::Command(',') => {
                        self.close_find();
                        self.toggle_settings();
                    }
                    // The focused field handles Escape (blur), Return and typing
                    // itself in `find_bar`; only app chords reach this far.
                    _ => {}
                }
                return;
            }
        }
        if let Key::Command(c) = ev.key {
            match c {
                ',' => self.toggle_settings(),
                'f' => self.open_find(),
                't' => self.new_tab(),
                'd' => self.duplicate_tab(),
                'w' => self.request_close_tab(self.active),
                'c' => self.copy_selection(tx),
                'a' => self.select_all(),
                'v' => self.paste(),
                'h' => self.hide(),
                'q' => self.request_quit(),
                '1'..='9' => self.select(c as usize - '1' as usize),
                '[' => self.cycle(-1),
                ']' => self.cycle(1),
                _ => {}
            }
            return;
        }
        if ev.mods.cmd && ev.mods.shift && !ev.mods.ctrl && !ev.mods.alt {
            match ev.key {
                Key::Up => {
                    if let Some(tab) = self.active_tab_mut() {
                        tab.vt.scroll_prompts(-1);
                    }
                    self.selection = None;
                    return;
                }
                Key::Down => {
                    if let Some(tab) = self.active_tab_mut() {
                        tab.vt.scroll_prompts(1);
                    }
                    self.selection = None;
                    return;
                }
                _ => {}
            }
        }
        if ev.mods.cmd && !ev.mods.ctrl && !ev.mods.alt {
            let page = (self.metrics().rows as i32 - 1).max(1);
            match ev.key {
                Key::Up | Key::PageUp => {
                    self.scroll_by(page);
                    return;
                }
                Key::Down | Key::PageDown => {
                    self.scroll_by(-page);
                    return;
                }
                Key::Home => {
                    if let Some(tab) = self.active_tab_mut() {
                        tab.vt.scroll_to_top();
                    }
                    self.selection = None;
                    return;
                }
                Key::End => {
                    if let Some(tab) = self.active_tab_mut() {
                        tab.vt.scroll_to_bottom();
                    }
                    return;
                }
                _ => {}
            }
        }
        if ev.mods.ctrl && ev.key == Key::Tab {
            self.cycle(if ev.mods.shift { -1 } else { 1 });
            return;
        }
        if self.settings.open {
            if ev.key == Key::Escape && !ev.mods.ctrl && !ev.mods.alt {
                self.close_settings();
            }
            return;
        }
        if self.cfg.hide_on_escape && ev.key == Key::Escape && !ev.mods.ctrl && !ev.mods.alt {
            self.hide();
            return;
        }
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        let bytes = tab.vt.encode_key(ev);
        if !bytes.is_empty() {
            if let Some(pty) = tab.pty.as_mut() {
                pty.write(&bytes);
            }
            tab.vt.scroll_to_bottom();
            self.selection = None;
        }
    }
}

/// A macOS notification without pulling in a notification framework: `osascript`
/// passes the strings as argv, so titles and bodies never become script text.
fn notify(title: &str, body: &str) {
    let clean = |s: &str, max: usize| -> String {
        s.lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(max)
            .collect::<String>()
            .trim()
            .to_string()
    };
    let title = clean(title, 80);
    let body = clean(body, 240);
    if title.is_empty() && body.is_empty() {
        return;
    }
    let title = if title.is_empty() { "sash".to_string() } else { title };
    let spawned = std::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title (item 1 of argv)",
            "-e",
            "end run",
            "--",
            &title,
            &body,
        ])
        .spawn();
    // osascript takes a moment to exit; wait off the UI thread so the child is
    // reaped instead of staying a zombie until sash quits.
    if let Ok(mut child) = spawned {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

fn hotkey_toggle(s: &mut Sash, _tx: &mut Tx) {
    s.toggle();
}

/// The panel's height in points: the configured one, or the space above the Dock.
pub fn panel_height(configured: Option<f32>, usable: f32) -> f32 {
    let usable = usable.max(120.0);
    configured.map(|h| h.clamp(120.0, usable)).unwrap_or(usable)
}

/// The headless command hook. Debug builds only, so a release binary always runs
/// the configured login shell no matter what its environment holds.
fn sash_command() -> Option<String> {
    if cfg!(debug_assertions) {
        std::env::var("SASH_COMMAND").ok()
    } else {
        None
    }
}

fn provisional_metrics(cfg: &Config, width: f32, height: f32) -> Metrics {
    // A hand-edited zero font size must not divide by zero and saturate the
    // column count at u16::MAX, which would build a grid 65535 columns wide.
    let cw = ((cfg.font_size * 0.6018 * 64.0).round() / 64.0).max(1.0);
    let ch = (cfg.font_size * 1.32).round().max(10.0);
    let cols = (((width - PAD_X * 2.0) / cw).floor() as u16).max(20);
    let rows = (((height - TAB_H - PAD_Y * 2.0) / ch).floor() as u16).max(4);
    Metrics { cw, ch, cols, rows }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub fn srgb(c: Rgb, a: f32) -> Color {
    let lin = |v: u8| {
        let s = v as f32 / 255.0;
        if s <= 0.04045 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    };
    Color { r: lin(c.r), g: lin(c.g), b: lin(c.b), a }
}

pub fn pretty_hotkey(chord: &str) -> String {
    let mut out = String::new();
    for part in chord.split('+') {
        let name = part.trim().to_ascii_lowercase();
        let label = match name.as_str() {
            "cmd" | "command" | "super" | "meta" => "⌘".to_string(),
            "shift" => "⇧".to_string(),
            "ctrl" | "control" => "⌃".to_string(),
            "alt" | "option" | "opt" => "⌥".to_string(),
            "space" | "spacebar" => "Space".to_string(),
            "backquote" | "`" => "`".to_string(),
            "tab" => "⇥".to_string(),
            "enter" | "return" => "↩".to_string(),
            "escape" | "esc" => "Esc".to_string(),
            "backspace" => "⌫".to_string(),
            "delete" | "del" => "⌦".to_string(),
            "left" => "←".to_string(),
            "right" => "→".to_string(),
            "up" => "↑".to_string(),
            "down" => "↓".to_string(),
            "slash" => "/".to_string(),
            "backslash" => "\\".to_string(),
            "comma" => ",".to_string(),
            "period" => ".".to_string(),
            "semicolon" => ";".to_string(),
            "quote" => "'".to_string(),
            "minus" => "-".to_string(),
            "equal" => "=".to_string(),
            "bracketleft" => "[".to_string(),
            "bracketright" => "]".to_string(),
            "pageup" => "Page Up".to_string(),
            "pagedown" => "Page Down".to_string(),
            other => match other.strip_prefix('f') {
                Some(n) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => {
                    format!("F{n}")
                }
                _ => other.to_string(),
            },
        };
        out.push_str(&label);
    }
    out
}

// ---------------------------------------------------------------- view

pub fn view(s: &Sash, _cx: &mut Cx<Sash>) -> El {
    let bg = srgb(DEFAULT_BG, s.cfg.opacity);
    let border = Color::WHITE.alpha(0.08);
    let mut card = col()
        .key("root")
        .w_full()
        .h(s.height)
        .absolute(0.0, 0.0)
        .clip_rounded(12.0)
        .fill(bg)
        .border(1.0, border)
        .child(tab_bar(s))
        .child(if s.settings.open { settings_page(s) } else { terminal(s) });
    if s.pending_quit || s.pending_close.is_some() {
        card = card.child(confirm_overlay(s));
    }
    let clip = col()
        .key("reveal")
        .w_full()
        .h(s.reveal)
        .min_h(0.0)
        .clip_rounded(12.0)
        .child(card);
    el().w_full().h_full().child(clip)
}

fn tab_bar(s: &Sash) -> El {
    let t = s.theme();
    let accent = srgb(ACCENT, 1.0);
    let mut bar = row()
        .key("tabbar")
        .w_full()
        .h(TAB_H)
        .px(PAD_X)
        .gap(3.0)
        .center_y()
        .fill(Color::WHITE.alpha(0.02));

    for (i, tab) in s.tabs.iter().enumerate() {
        let active = i == s.active;
        let title = tab.title();
        let ringing = tab.bell_until > s.now;
        let fill = if ringing {
            srgb(ACCENT, 0.26)
        } else if active {
            Color::WHITE.alpha(0.085)
        } else {
            Color::TRANSPARENT
        };
        let mut chip = row()
            .key(format!("tab-{i}"))
            .h(25.0)
            .px(9.0)
            .gap(7.0)
            .radius(7.0)
            .center_y()
            .fill(fill)
            .on_press(move |s: &mut Sash| s.select(i));
        if tab.unread {
            let error = matches!(tab.progress, Some(p) if p.state == ProgressState::Error);
            chip = chip.child(
                el()
                    .key(format!("dot-{i}"))
                    .size(6.0, 6.0)
                    .radius(3.0)
                    .fill(if error { srgb(ERROR, 1.0) } else { accent }),
            );
        }
        chip = chip
            .child(
                text(title)
                    .font(s.small_style(if active { 500.0 } else { 400.0 }, 12.0))
                    .color(if active { t.text } else { t.text_secondary }),
            )
            .child(
                el()
                    .key(format!("close-{i}"))
                    .size(15.0, 15.0)
                    .radius(4.0)
                    .center()
                    .on_press(move |s: &mut Sash| s.request_close_tab(i))
                    .child(text("×").font(s.small_style(400.0, 12.0)).color(t.text_secondary)),
            );
        bar = bar.child(chip);
    }

    if let Some(progress) = s.tabs.get(s.active).and_then(|tab| tab.progress) {
        let fraction = progress.percent.map(|p| p as f32 / 100.0).unwrap_or(0.35).clamp(0.05, 1.0);
        let color = match progress.state {
            ProgressState::Error => srgb(ERROR, 1.0),
            ProgressState::Paused => accent.alpha(0.55),
            _ => accent,
        };
        let width = ((s.width - PAD_X * 2.0) * fraction).max(6.0);
        bar = bar.child(
            el()
                .key("tab-progress")
                .absolute(0.0, TAB_H - 2.0)
                .w(width)
                .h(2.0)
                .radius(1.0)
                .fill(color)
                .a11y_hidden(),
        );
    }

    let plus = el()
        .key("new-tab")
        .size(25.0, 25.0)
        .radius(7.0)
        .center()
        .on_press(|s: &mut Sash| s.new_tab())
        .child(text("+").font(s.small_style(500.0, 15.0)).color(t.text_secondary));

    let settings = el()
        .key("settings")
        .size(25.0, 25.0)
        .radius(7.0)
        .center()
        .fill(if s.settings.open { Color::WHITE.alpha(0.085) } else { Color::TRANSPARENT })
        .on_press(|s: &mut Sash| s.toggle_settings())
        .child(text("⚙").font(s.small_style(400.0, 13.0)).color(t.text_secondary));

    bar.child(plus)
        .child(spacer())
        .child(settings)
        .child(
            text(pretty_hotkey(&s.cfg.hotkey))
                .font(s.small_style(400.0, 11.0))
                .color(t.text_secondary.alpha(0.55)),
        )
}

fn settings_page(s: &Sash) -> El {
    let t = s.theme();
    let accent = srgb(ACCENT, 1.0);
    let hair = Color::WHITE.alpha(0.08);
    let full = s.screen.usable_height_pt().max(120.0);
    let current = s
        .settings
        .height_drag
        .or_else(|| s.cfg.height.map(|h| (h / full).clamp(0.2, 1.0)))
        .unwrap_or(1.0);

    let width_value = s
        .cfg
        .width
        .map(|w| (w / s.screen.width_pt().max(1.0)).clamp(0.35, 1.0))
        .unwrap_or(1.0);
    let scroll_lines = s.cfg.scrollback_lines.unwrap_or(10_000);
    let scroll_value = (scroll_lines as f32 / 1_000.0).clamp(1.0, 50.0);

    // Panel geometry first, then what the terminal does with input and output.
    let panel_card = col()
        .key("settings-panel-card")
        .w_full()
        .radius(10.0)
        .fill(Color::WHITE.alpha(0.035))
        .border(1.0, hair)
        .child(setting_row(s, "set-height", "Drop-down height", height_control(s, current)))
        .child(separator("sep-height", hair))
        .child(setting_row(
            s,
            "set-width",
            "Drop-down width",
            slider_row(
                s,
                "width",
                "Drop-down width",
                width_value,
                (0.35, 1.0, 0.05),
                format_width,
                |s: &mut Sash, value, tx| s.set_width_fraction(value, tx),
            ),
        ))
        .child(separator("sep-width", hair))
        .child(setting_row(
            s,
            "set-font",
            "Font size",
            slider_row(
                s,
                "font",
                "Font size",
                s.cfg.font_size,
                (10.0, 20.0, 0.5),
                format_points,
                |s: &mut Sash, value, _tx| s.set_font_size(value),
            ),
        ))
        .child(separator("sep-font", hair))
        .child(setting_row(
            s,
            "set-opacity",
            "Background opacity",
            slider_row(
                s,
                "opacity",
                "Background opacity",
                s.cfg.opacity,
                (0.55, 1.0, 0.01),
                format_percent,
                |s: &mut Sash, value, _tx| s.set_opacity(value),
            ),
        ))
        .child(separator("sep-opacity", hair))
        .child(setting_row(
            s,
            "set-scrollback",
            "Scrollback",
            slider_row(
                s,
                "scrollback",
                "Scrollback",
                scroll_value,
                (1.0, 50.0, 1.0),
                format_scrollback,
                |s: &mut Sash, value, _tx| s.set_scrollback_lines((value * 1_000.0) as u32),
            ),
        ))
        .child(separator("sep-scrollback", hair))
        .child(toggle_row(
            s,
            "set-animation",
            "Slide animation",
            s.cfg.animation_ms > 0,
            flip_animation,
        ))
        .child(separator("sep-animation", hair))
        .child(setting_row(s, "set-hotkey", "Drop-down shortcut", shortcut_control(s)));

    let behavior_card = col()
        .key("settings-behavior-card")
        .w_full()
        .radius(10.0)
        .fill(Color::WHITE.alpha(0.035))
        .border(1.0, hair)
        .child(toggle_row(
            s,
            "set-top",
            "Always on top",
            s.cfg.always_on_top,
            flip_always_on_top,
        ))
        .child(separator("sep-top", hair))
        .child(toggle_row(
            s,
            "set-focus",
            "Focus window on open",
            s.cfg.focus_on_open,
            flip_focus_on_open,
        ))
        .child(separator("sep-focus", hair))
        .child(toggle_row(
            s,
            "set-blur",
            "Hide when focus is lost",
            s.cfg.hide_on_blur,
            flip_hide_on_blur,
        ))
        .child(separator("sep-blur", hair))
        .child(toggle_row(
            s,
            "set-escape",
            "Hide on Escape",
            s.cfg.hide_on_escape,
            flip_hide_on_escape,
        ))
        .child(separator("sep-escape", hair))
        .child(toggle_row(
            s,
            "set-copy",
            "Copy selection on release",
            s.cfg.copy_on_select,
            flip_copy_on_select,
        ))
        .child(separator("sep-copy", hair))
        .child(toggle_row(
            s,
            "set-paste",
            "Confirm multi-line pastes",
            s.cfg.confirm_multiline_paste,
            flip_confirm_paste,
        ))
        .child(separator("sep-paste", hair))
        .child(toggle_row(
            s,
            "set-clipboard",
            "Allow clipboard writes (OSC 52)",
            s.cfg.clipboard_write,
            flip_clipboard_write,
        ))
        .child(separator("sep-clipboard", hair))
        .child(toggle_row(
            s,
            "set-notify",
            "Desktop notifications",
            s.cfg.desktop_notifications,
            flip_desktop_notifications,
        ))
        .child(separator("sep-notify", hair))
        .child(toggle_row(
            s,
            "set-confirm-close",
            "Confirm before closing a running tab",
            s.cfg.confirm_close,
            flip_confirm_close,
        ))
        .child(separator("sep-confirm-close", hair))
        .child(toggle_row(
            s,
            "set-restore",
            "Restore tabs on launch",
            s.cfg.restore_session,
            flip_restore_session,
        ))
        .child(separator("sep-restore", hair))
        .child(setting_row(
            s,
            "set-shell",
            "Shell",
            text_input(&s.shell_edit, s.small_style(400.0, 12.0))
                .key("shell-input")
                .single_line()
                .label("Shell path")
                .w(250.0)
                .h(28.0)
                .px(8.0)
                .radius(6.0)
                .fill(Color::WHITE.alpha(0.07))
                .border(1.0, Color::WHITE.alpha(0.12))
                .on_edit(|s: &mut Sash| &mut s.shell_edit),
        ))
        .child(separator("sep-font", hair))
        .child(setting_row(
            s,
            "set-font",
            "Font family",
            text_input(&s.font_edit, s.small_style(400.0, 12.0))
                .key("font-input")
                .single_line()
                .label("JetBrains Mono, Menlo")
                .w(250.0)
                .h(28.0)
                .px(8.0)
                .radius(6.0)
                .fill(Color::WHITE.alpha(0.07))
                .border(1.0, Color::WHITE.alpha(0.12))
                .on_input_key(|s: &mut Sash, ev: &KeyEvent, _tx: &mut Tx| {
                    if ev.key == Key::Enter {
                        s.commit_font_edit();
                        true
                    } else {
                        false
                    }
                })
                .on_input_change(|s: &mut Sash, tx: &mut Tx| {
                    // Re-measure and save once the name has stopped changing.
                    s.settings.font_edit_at = Some(Instant::now());
                    tx.wake_after(FONT_COMMIT_QUIET);
                })
                .on_edit(|s: &mut Sash| &mut s.font_edit),
        ));

    let mut note = s.settings.message.clone();
    let note_error = note.is_some() || s.hotkey_error.is_some();
    if note.is_none() && s.settings.capturing {
        note = Some("Press a combination with ⌘, ⌃ or ⌥. Esc cancels.".into());
    }
    if note.is_none() {
        note = s.hotkey_error.as_ref().map(|e| format!("Shortcut unavailable: {e}"));
    }
    if note.is_none() {
        note = Some("Full fills the screen above the Dock · changes save immediately".into());
    }
    let note_color = if note_error {
        srgb(ERROR, 1.0)
    } else if s.settings.capturing {
        accent
    } else {
        t.text_secondary.alpha(0.6)
    };

    col()
        .key("settings")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .items_center()
        .px(16.0)
        .py(10.0)
        .child(
            col()
                .key("settings-inner")
                .w_full()
                .max_w(680.0)
                .grow(1.0)
                .min_h(0.0)
                .gap(10.0)
                .child(
                    row()
                        .key("settings-head")
                        .w_full()
                        .h(26.0)
                        .center_y()
                        .gap(8.0)
                        .child(text("Settings").font(s.small_style(600.0, 15.0)).color(t.text))
                        .child(spacer())
                        .child(
                            el()
                                .key("done")
                                .h(24.0)
                                .px(9.0)
                                .radius(6.0)
                                .center()
                                .fill(Color::WHITE.alpha(0.07))
                                .on_press(|s: &mut Sash| s.close_settings())
                                .child(
                                    text("Done")
                                        .font(s.small_style(500.0, 12.0))
                                        .color(t.text),
                                ),
                        ),
                )
                .child(
                    col()
                        .key("settings-scroll")
                        .w_full()
                        .grow(1.0)
                        .min_h(0.0)
                        .scroll(ScrollSpec { stick_bottom: false })
                        .child(
                            col()
                                .key("settings-cards")
                                .w_full()
                                .gap(10.0)
                                .child(panel_card)
                                .child(behavior_card)
                                .child(
                                    text(note.unwrap_or_default())
                                        .font(s.small_style(400.0, 11.0))
                                        .color(note_color),
                                ),
                        ),
                ),
        )
}

fn setting_row(s: &Sash, key: &str, label: &str, control: El) -> El {
    let t = s.theme();
    row()
        .key(key)
        .h(46.0)
        .px(14.0)
        .center_y()
        .justify_between()
        .child(text(label.to_string()).font(s.small_style(400.0, 13.0)).color(t.text))
        .child(control)
}

fn separator(key: &str, hair: Color) -> El {
    el().key(key).h(1.0).w_full().fill(hair).a11y_hidden()
}

fn toggle_row(s: &Sash, key: &str, label: &str, on: bool, flip: fn(&mut Sash)) -> El {
    setting_row(s, key, label, toggle_switch(on))
        .role("switch")
        .label(label.to_string())
        .checked(on)
        .focusable(true)
        .on_press(flip)
}

fn toggle_switch(on: bool) -> El {
    let (w, h, k) = (38.0, 22.0, 18.0);
    let inset = (h - k) / 2.0;
    el()
        .key("switch")
        .size(w, h)
        .radius(h / 2.0)
        .fill(if on { srgb(ACCENT, 1.0) } else { Color::WHITE.alpha(0.16) })
        .child(
            el()
                .key("knob")
                .size(k, k)
                .absolute(if on { w - k - inset } else { inset }, inset)
                .radius(k / 2.0)
                .fill(Color::WHITE),
        )
}

fn shortcut_control(s: &Sash) -> El {
    let t = s.theme();
    let accent = srgb(ACCENT, 1.0);
    let capturing = s.settings.capturing;
    let label = if capturing {
        "Press keys…".to_string()
    } else {
        pretty_hotkey(&s.cfg.hotkey)
    };
    el()
        .key("hotkey-button")
        .role("button")
        .h(27.0)
        .px(11.0)
        .radius(6.0)
        .center()
        .focusable(true)
        .fill(if capturing { accent.alpha(0.22) } else { Color::WHITE.alpha(0.07) })
        .border(1.0, if capturing { accent } else { Color::WHITE.alpha(0.12) })
        .on_press_tx(|s: &mut Sash, tx| {
            s.begin_capture();
            // The button would otherwise keep focus and swallow Space/Return while
            // the user is trying to record a chord.
            tx.blur();
        })
        .child(
            text(label)
                .font(s.small_style(500.0, 12.0))
                .color(if capturing { accent } else { t.text }),
        )
}

fn height_control(s: &Sash, value: f32) -> El {
    let t = s.theme();
    let accent = srgb(ACCENT, 1.0);
    let (w, h, k, bar) = (170.0, 20.0, 14.0, 4.0);
    let travel = w - k;
    let to_value = move |x: f32| ((x - k / 2.0) / travel).clamp(0.2, 1.0);
    let label = if value >= 0.995 {
        "Full".to_string()
    } else {
        format!("{}%", (value * 100.0).round() as i32)
    };

    row()
        .key("height-control")
        .gap(10.0)
        .center_y()
        .child(
            el()
                .key("height-slider")
                .role("slider")
                .label("Drop-down height")
                .range_value(f64::from(value), 0.2, 1.0)
                .range_step(0.05)
                .size(w, h)
                .on_drag(move |s: &mut Sash, d: &mut Drag, tx| match d.phase {
                    Phase::Down => {
                        d.data = value;
                        let knob = k / 2.0 + value * travel;
                        if (d.x - knob).abs() > k / 2.0 + 2.0 {
                            s.settings.height_drag = Some(to_value(d.x));
                        }
                    }
                    Phase::Move => {
                        let knob = k / 2.0 + d.data * travel;
                        let grab = if (d.start_x - knob).abs() <= k / 2.0 + 2.0 {
                            d.start_x - knob
                        } else {
                            0.0
                        };
                        s.settings.height_drag = Some(to_value(d.x - grab));
                        tx.instant();
                    }
                    Phase::Up => {
                        if d.cancelled() {
                            s.settings.height_drag = None;
                        } else if let Some(fraction) = s.settings.height_drag.take() {
                            s.set_height_fraction(fraction, tx);
                        }
                    }
                })
                .child(
                    el()
                        .key("track")
                        .size(w, bar)
                        .absolute(0.0, (h - bar) / 2.0)
                        .radius(bar / 2.0)
                        .fill(Color::WHITE.alpha(0.14))
                        .a11y_hidden(),
                )
                .child(
                    el()
                        .key("fill")
                        .size(k / 2.0 + value * travel, bar)
                        .absolute(0.0, (h - bar) / 2.0)
                        .radius(bar / 2.0)
                        .fill(accent)
                        .a11y_hidden(),
                )
                .child(
                    el()
                        .key("knob")
                        .size(k, k)
                        .absolute(value * travel, (h - k) / 2.0)
                        .radius(k / 2.0)
                        .fill(Color::WHITE)
                        .a11y_hidden(),
                ),
        )
        .child(
            text(label)
                .key("height-value")
                .w(44.0)
                .font(s.small_style(400.0, 12.0))
                .color(t.text_secondary)
                .align(Align::End),
        )
}

/// One slider row for a numeric setting. The value only commits on release; while
/// dragging, the label and knob read `Sash::slider`, so the control never jumps.
fn slider_row(
    s: &Sash,
    key: &'static str,
    title: &'static str,
    value: f32,
    (min, max, step): (f32, f32, f32),
    format: fn(f32) -> String,
    apply: fn(&mut Sash, f32, &mut Tx),
) -> El {
    let t = s.theme();
    let accent = srgb(ACCENT, 1.0);
    let (w, h, k, bar) = (150.0, 20.0, 14.0, 4.0);
    let travel = w - k;
    let shown = s
        .slider
        .filter(|(dragging, _)| *dragging == key)
        .map(|(_, value)| value)
        .unwrap_or(value)
        .clamp(min, max);
    let to_value = move |x: f32| {
        let raw = min + ((x - k / 2.0) / travel).clamp(0.0, 1.0) * (max - min);
        (min + ((raw - min) / step).round() * step).clamp(min, max)
    };
    let label = format(shown);

    row()
        .key(format!("{key}-control"))
        .gap(10.0)
        .center_y()
        .child(
            el()
                .key(format!("{key}-slider"))
                .role("slider")
                .label(title)
                .range_value(f64::from(shown), f64::from(min), f64::from(max))
                .range_step(f64::from(step))
                .size(w, h)
                .on_drag(move |s: &mut Sash, d: &mut Drag, tx| match d.phase {
                    Phase::Down => {
                        d.data = shown;
                        let knob = k / 2.0 + (shown - min) / (max - min) * travel;
                        if (d.x - knob).abs() > k / 2.0 + 2.0 {
                            s.slider = Some((key, to_value(d.x)));
                        }
                    }
                    Phase::Move => {
                        let knob = k / 2.0 + (shown - min) / (max - min) * travel;
                        let grab = if (d.start_x - knob).abs() <= k / 2.0 + 2.0 {
                            d.start_x - knob
                        } else {
                            0.0
                        };
                        s.slider = Some((key, to_value(d.x - grab)));
                        tx.instant();
                    }
                    Phase::Up => {
                        if d.cancelled() {
                            s.slider = None;
                        } else if let Some((dragging, value)) = s.slider.take()
                            && dragging == key
                        {
                            apply(s, value, tx);
                        }
                    }
                })
                .child(
                    el()
                        .key("track")
                        .size(w, bar)
                        .absolute(0.0, (h - bar) / 2.0)
                        .radius(bar / 2.0)
                        .fill(Color::WHITE.alpha(0.14))
                        .a11y_hidden(),
                )
                .child(
                    el()
                        .key("fill")
                        .size(k / 2.0 + (shown - min) / (max - min) * travel, bar)
                        .absolute(0.0, (h - bar) / 2.0)
                        .radius(bar / 2.0)
                        .fill(accent)
                        .a11y_hidden(),
                )
                .child(
                    el()
                        .key("knob")
                        .size(k, k)
                        .absolute((shown - min) / (max - min) * travel, (h - k) / 2.0)
                        .radius(k / 2.0)
                        .fill(Color::WHITE)
                        .a11y_hidden(),
                ),
        )
        .child(
            text(label)
                .key(format!("{key}-value"))
                .w(72.0)
                .font(s.small_style(400.0, 12.0))
                .color(t.text_secondary)
                .align(Align::End),
        )
}

fn format_percent(value: f32) -> String {
    format!("{}%", (value * 100.0).round() as i32)
}

fn format_width(value: f32) -> String {
    if value >= 0.995 {
        "Full".to_string()
    } else {
        format_percent(value)
    }
}

fn format_points(value: f32) -> String {
    format!("{value:.1} pt")
}

fn format_scrollback(value: f32) -> String {
    format!("{} lines", (value * 1_000.0).round() as u32)
}

fn flip_always_on_top(s: &mut Sash) {
    s.set_always_on_top(!s.cfg.always_on_top);
}

fn flip_focus_on_open(s: &mut Sash) {
    s.set_focus_on_open(!s.cfg.focus_on_open);
}

fn flip_hide_on_blur(s: &mut Sash) {
    s.set_hide_on_blur(!s.cfg.hide_on_blur);
}

fn flip_hide_on_escape(s: &mut Sash) {
    s.set_hide_on_escape(!s.cfg.hide_on_escape);
}

fn flip_copy_on_select(s: &mut Sash) {
    s.set_copy_on_select(!s.cfg.copy_on_select);
}

fn flip_confirm_paste(s: &mut Sash) {
    s.set_confirm_paste(!s.cfg.confirm_multiline_paste);
}

fn flip_clipboard_write(s: &mut Sash) {
    s.set_clipboard_write(!s.cfg.clipboard_write);
}

fn flip_desktop_notifications(s: &mut Sash) {
    s.set_desktop_notifications(!s.cfg.desktop_notifications);
}

fn flip_confirm_close(s: &mut Sash) {
    s.cfg.confirm_close = !s.cfg.confirm_close;
    s.cfg.save();
}

fn flip_restore_session(s: &mut Sash) {
    s.cfg.restore_session = !s.cfg.restore_session;
    s.cfg.save();
    if !s.cfg.restore_session {
        crate::config::Session::clear();
    } else {
        s.save_session();
    }
}

fn flip_animation(s: &mut Sash) {
    s.set_animation(s.cfg.animation_ms == 0);
}

/// A yes/no dialog for closing a tab or quitting while a command runs.
fn confirm_overlay(s: &Sash) -> El {
    let t = s.theme();
    let quitting = s.pending_quit;
    let busy: Vec<String> = s
        .tabs
        .iter()
        .filter(|tab| tab.busy())
        .map(|tab| tab.title())
        .collect();
    let title = if quitting { "Quit sash?" } else { "Close this tab?" };
    let detail = if quitting {
        match busy.len() {
            0 => "A command is still running.".to_string(),
            1 => format!("{} is still running a command.", busy[0]),
            n => format!("{n} tabs are still running a command."),
        }
    } else {
        let name = s
            .pending_close
            .and_then(|i| s.tabs.get(i))
            .map(|tab| tab.title())
            .unwrap_or_else(|| "This tab".into());
        format!("{name} is still running a command.")
    };
    let confirm = if quitting { "Quit" } else { "Close Tab" };
    el()
        .key("confirm-overlay")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .center()
        .fill(Color::BLACK.alpha(0.32))
        .child(
            col()
                .key("confirm-card")
                .w(350.0)
                .px(16.0)
                .py(14.0)
                .gap(12.0)
                .radius(10.0)
                .fill(srgb(DEFAULT_BG, 0.98))
                .border(1.0, Color::WHITE.alpha(0.12))
                .child(text(title).font(s.small_style(500.0, 13.0)).color(t.text))
                .child(
                    text(detail)
                        .font(s.small_style(400.0, 11.0))
                        .color(t.text_secondary),
                )
                .child(
                    row()
                        .w_full()
                        .gap(8.0)
                        .center_y()
                        .child(spacer())
                        .child(
                            el()
                                .key("confirm-cancel")
                                .h(26.0)
                                .px(10.0)
                                .radius(6.0)
                                .center()
                                .fill(Color::WHITE.alpha(0.07))
                                .on_press(|s: &mut Sash| s.cancel_pending())
                                .child(
                                    text("Cancel")
                                        .font(s.small_style(400.0, 12.0))
                                        .color(t.text),
                                ),
                        )
                        .child(
                            el()
                                .key("confirm-go")
                                .h(26.0)
                                .px(10.0)
                                .radius(6.0)
                                .center()
                                .fill(srgb(ERROR, 1.0))
                                .on_press(|s: &mut Sash| s.confirm_pending())
                                .child(
                                    text(confirm)
                                        .font(s.small_style(500.0, 12.0))
                                        .color(srgb(DEFAULT_BG, 1.0)),
                                ),
                        ),
                ),
        )
}

/// The find bar floats over the top-right of the grid, where a terminal's
/// scrollback search usually lives.
fn find_bar(s: &Sash, m: &Metrics, cols: u16) -> El {
    let t = s.theme();
    let width = (cols as f32 * m.cw - 16.0).clamp(250.0, 380.0);
    let x = (cols as f32 * m.cw - width - 2.0).max(0.0);
    let count = if s.find.needle.is_empty() {
        String::new()
    } else if !s.find.complete {
        "…".to_string()
    } else if s.find.total == 0 {
        "no match".to_string()
    } else {
        format!(
            "{} / {}",
            s.find.index.map(|i| i + 1).unwrap_or(0),
            s.find.total
        )
    };
    let label = |size: f32| s.small_style(400.0, size);
    row()
        .key("find-bar")
        .absolute(x, 4.0)
        .w(width)
        .h(32.0)
        .px(10.0)
        .gap(7.0)
        .center_y()
        .radius(8.0)
        .fill(srgb(Rgb::new(0x16, 0x1b, 0x25), 0.97))
        .border(1.0, Color::WHITE.alpha(0.16))
        .child(text("find").font(label(11.0)).color(t.text_secondary))
        .child(
            text_input(&s.find.edit, s.small_style(400.0, 12.0))
                .key("find-input")
                .single_line()
                .label("Search scrollback")
                .grow(1.0)
                .h(24.0)
                .px(6.0)
                .radius(5.0)
                .fill(Color::WHITE.alpha(0.06))
                // A focused field keeps its own keys, so `El::on_key` never runs
                // here; `on_input_key` does, before the editor. Escape stays the
                // field's blur, which `sync_find` turns into a close.
                .on_input_key(|s: &mut Sash, ev: &KeyEvent, _tx: &mut Tx| {
                    if ev.key == Key::Enter {
                        s.find_next(!ev.mods.shift);
                        true
                    } else {
                        false
                    }
                })
                .on_input_change(|s: &mut Sash, tx: &mut Tx| {
                    // Restart the quiet period, and come back when it has passed,
                    // so the scrollback is scanned once per pause, not once per key.
                    s.find.last_edit = Some(Instant::now());
                    tx.wake_after(FIND_QUIET);
                })
                .on_edit(|s: &mut Sash| &mut s.find.edit),
        )
        .child(text(count).font(label(11.0)).color(t.text_secondary))
        .child(
            text("↑")
                .font(label(13.0))
                .color(t.text_secondary)
                .on_press(|s: &mut Sash| s.find_next(true)),
        )
        .child(
            text("↓")
                .font(label(13.0))
                .color(t.text_secondary)
                .on_press(|s: &mut Sash| s.find_next(false)),
        )
        .child(
            text("×")
                .font(label(12.0))
                .color(t.text_secondary)
                .on_press(|s: &mut Sash| s.close_find()),
        )
}

fn mouse_mods(mods: &Mods) -> ffi::GhosttyMods {
    let mut bits: ffi::GhosttyMods = 0;
    if mods.shift {
        bits |= ffi::GHOSTTY_MODS_SHIFT as ffi::GhosttyMods;
    }
    if mods.alt {
        bits |= ffi::GHOSTTY_MODS_ALT as ffi::GhosttyMods;
    }
    if mods.ctrl {
        bits |= ffi::GHOSTTY_MODS_CTRL as ffi::GhosttyMods;
    }
    bits
}

fn mouse_button(button: PointerButton) -> ffi::GhosttyMouseButton {
    match button {
        PointerButton::Middle => ffi::GHOSTTY_MOUSE_BUTTON_MIDDLE,
        PointerButton::Back => ffi::GHOSTTY_MOUSE_BUTTON_EIGHT,
        PointerButton::Forward => ffi::GHOSTTY_MOUSE_BUTTON_NINE,
        _ => ffi::GHOSTTY_MOUSE_BUTTON_LEFT,
    }
}

fn terminal(s: &Sash) -> El {
    let t = s.theme();
    let mut area = col()
        .key("term")
        .w_full()
        .grow(1.0)
        .px(PAD_X)
        .py(PAD_Y)
        .on_wheel(|s: &mut Sash, ev, _tx| {
            if s.pending_paste.is_some() {
                return false;
            }
            let tracked = !s.mods.shift
                && s.active_tab().is_some_and(|tab| tab.vt.mouse_tracking());
            let rows = s.wheel.rows(ev.dy, s.metrics().ch);
            if rows != 0 {
                if tracked {
                    s.wheel_mouse(ev.x, ev.y, rows > 0, rows.abs());
                } else {
                    s.scroll_by(rows);
                }
            }
            // Claim even sub-row travel, so the accumulator keeps it.
            true
        })
        .pointer_buttons([PointerButton::Middle, PointerButton::Back, PointerButton::Forward])
        .on_pointer_move_local(|s: &mut Sash, p, _tx| {
            if s.mouse_down || s.pending_paste.is_some() || s.mods.shift {
                return;
            }
            if s.active_tab().is_some_and(|tab| tab.vt.mouse_any_event()) {
                s.send_mouse(ffi::GHOSTTY_MOUSE_ACTION_MOTION, None, p.x, p.y);
            }
        })
        .on_secondary_press_local(|s: &mut Sash, p, _tx| {
            if s.pending_paste.is_some() || s.mods.shift {
                return;
            }
            if s.active_tab().is_some_and(|tab| tab.vt.mouse_tracking()) {
                s.send_mouse(ffi::GHOSTTY_MOUSE_ACTION_PRESS, Some(ffi::GHOSTTY_MOUSE_BUTTON_RIGHT), p.x, p.y);
                s.send_mouse(ffi::GHOSTTY_MOUSE_ACTION_RELEASE, Some(ffi::GHOSTTY_MOUSE_BUTTON_RIGHT), p.x, p.y);
            }
        })
        .on_drag(|s: &mut Sash, d: &mut Drag, tx| {
            if s.pending_paste.is_some() {
                return;
            }
            let Some(at) = s.cell_at(d.x, d.y) else {
                return;
            };
            let tracking = !s.mods.shift
                && s.active_tab().is_some_and(|tab| tab.vt.mouse_tracking());
            match d.phase {
                Phase::Down => {
                    if s.mods.cmd
                        && d.clicks == 1
                        && let Some(url) = s.link_at(at)
                    {
                        let _ = tx.open_url(&url);
                        s.selection = None;
                        tx.instant();
                        return;
                    }
                    if tracking {
                        s.mouse_down = true;
                        s.selection = None;
                        let button = mouse_button(tx.button());
                        s.send_mouse(ffi::GHOSTTY_MOUSE_ACTION_PRESS, Some(button), d.x, d.y);
                        return;
                    }
                    s.selection = match d.clicks {
                        1 => Some(Selection::new(at, Kind::Cells)),
                        2 => s.active_tab().map(|tab| word_at(tab.vt.grid(), at)),
                        _ => s.active_tab().map(|tab| line_at(tab.vt.grid(), at)),
                    };
                    tx.instant();
                }
                Phase::Move => {
                    // A ⌘-click opened a link; do not turn it into a drag.
                    if s.mods.cmd && d.clicks == 1 {
                        return;
                    }
                    if tracking {
                        s.send_mouse(ffi::GHOSTTY_MOUSE_ACTION_MOTION, Some(mouse_button(tx.button())), d.x, d.y);
                        return;
                    }
                    if let Some(selection) = s.selection.as_mut() {
                        selection.extend(at);
                        tx.instant();
                    }
                }
                Phase::Up => {
                    if tracking || s.mouse_down {
                        s.mouse_down = false;
                        s.send_mouse(ffi::GHOSTTY_MOUSE_ACTION_RELEASE, Some(mouse_button(tx.button())), d.x, d.y);
                        return;
                    }
                    let dragged = d.dx.abs() > 3.0 || d.dy.abs() > 3.0;
                    if d.clicks == 1 && !dragged {
                        s.selection = None;
                    } else if s.cfg.copy_on_select {
                        s.copy_selection(tx);
                    }
                    tx.instant();
                }
            }
        });
    // Every cell run is rebuilt from the grid each frame, so runs come and go
    // constantly while a TUI redraws. Rune mounts new elements with a 0.2 s fade
    // and keeps removed ones around for a 0.15 s fade, which smears a full-screen
    // app across the panel; terminal content is state, not an entrance.
    let instant = Transition::none();

    let Some(tab) = s.active_tab() else {
        return area.child(text("no tab").font(s.run_style(400.0)).color(t.text_secondary));
    };
    let grid = tab.vt.grid();
    let m = s.metrics();

    // Backgrounds first, grouped into horizontal runs.
    for y in 0..grid.rows {
        let mut x = 0u16;
        while x < grid.cols {
            let bg = grid.cell(x, y).and_then(|c| c.bg);
            let start = x;
            while x < grid.cols && grid.cell(x, y).and_then(|c| c.bg) == bg {
                x += 1;
            }
            if let Some(bg) = bg {
                area = area.child(
                    el()
                        .key(format!("bg-{y}-{start}"))
                        .transition(instant)
                        .absolute(start as f32 * m.cw, y as f32 * m.ch)
                        .size((x - start) as f32 * m.cw, m.ch)
                        .fill(srgb(bg, 1.0)),
                );
            }
        }
    }

    // Selection highlight, one quad per selected run, under the cursor and glyphs.
    // The range is resolved once per frame: a word selection must not re-scan its
    // word bounds for every cell of the grid.
    if let Some(selection) = s.selection
        && let Some((lo, hi)) = selection.range(grid)
    {
        let cols = grid.cols as usize;
        for y in 0..grid.rows {
            let mut x = 0u16;
            while x < grid.cols {
                let key = y as usize * cols + x as usize;
                if key < lo || key > hi {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < grid.cols {
                    let key = y as usize * cols + x as usize;
                    if key < lo || key > hi {
                        break;
                    }
                    x += 1;
                }
                area = area.child(
                    el()
                        .key(format!("sel-{y}-{start}"))
                        .transition(instant)
                        .absolute(start as f32 * m.cw, y as f32 * m.ch)
                        .size((x - start) as f32 * m.cw, m.ch)
                        .fill(srgb(ACCENT, 0.28)),
                );
            }
        }
    }

    // The selected find match, over the selection tint but under the text.
    for (y, x0, x1) in tab.vt.search_highlight() {
        area = area.child(
            el()
                .key(format!("find-{y}-{x0}"))
                .transition(instant)
                .absolute(x0 as f32 * m.cw, y as f32 * m.ch)
                .size((x1 - x0 + 1) as f32 * m.cw, m.ch)
                .fill(srgb(Rgb::new(0xf9, 0xd8, 0x8a), 0.38)),
        );
    }

    // Cursor under the text so glyphs stay readable.
    if grid.cursor_visible {
        if let Some((cx, cy)) = grid.cursor {
            let (w, h, x, y) = match grid.cursor_style.unwrap_or(CursorStyle::Block) {
                CursorStyle::Bar => (2.0, m.ch, cx as f32 * m.cw, cy as f32 * m.ch),
                CursorStyle::Underline => (m.cw, 2.0, cx as f32 * m.cw, cy as f32 * m.ch + m.ch - 2.0),
                CursorStyle::Block | CursorStyle::Hollow => (m.cw, m.ch, cx as f32 * m.cw, cy as f32 * m.ch),
            };
            area = area.child(
                el()
                    .key("cursor")
                    .transition(instant)
                    .absolute(x, y)
                    .size(w, h)
                    .radius(2.0)
                    .fill(srgb(ACCENT, 0.55)),
            );
            // Tell the input method where the caret is, so candidates and the
            // emoji picker open beside the text they will replace.
            area = area.child(
                el()
                    .key("ime-caret")
                    .absolute(x, y)
                    .size(1.0, m.ch)
                    .ime_cursor(),
            );
        }
    }

    // Text runs: consecutive cells sharing fg + attributes, plus adjacent blank cells
    // skipped so a run never starts mid-gap.
    for y in 0..grid.rows {
        let mut x = 0u16;
        while x < grid.cols {
            let Some(cell) = grid.cell(x, y) else { break };
            if cell.text.is_empty() {
                x += 1;
                continue;
            }
            let fg = cell.fg;
            let attr = cell.flags & (FLAG_BOLD | FLAG_FAINT);
            let start = x;
            let mut run = String::new();
            while x < grid.cols {
                let Some(c) = grid.cell(x, y) else { break };
                if c.text.is_empty() || c.fg != fg || (c.flags & (FLAG_BOLD | FLAG_FAINT)) != attr {
                    break;
                }
                run.push_str(&c.text);
                x += 1;
            }
            let weight = if attr & FLAG_BOLD != 0 { 700.0 } else { 400.0 };
            let alpha = if attr & FLAG_FAINT != 0 { 0.6 } else { 1.0 };
            area = area.child(
                text(run)
                    .key(format!("tx-{y}-{start}"))
                    .transition(instant)
                    .font(s.run_style(weight))
                    .color(srgb(fg, alpha))
                    .absolute(start as f32 * m.cw, y as f32 * m.ch)
                    .h(m.ch),
            );
        }
    }

    // Scrollback position: a slim thumb on the right edge, absent at a live prompt.
    if let Some(bar) = tab.vt.scrollbar()
        && bar.total > bar.len
        && bar.len > 0
    {
        let track = grid.rows as f32 * m.ch;
        let thumb = (bar.len as f32 / bar.total as f32 * track).clamp(18.0, track);
        let max_offset = (bar.total - bar.len) as f32;
        let y = if max_offset > 0.0 {
            (bar.offset as f32 / max_offset) * (track - thumb)
        } else {
            0.0
        };
        area = area.child(
            el()
                .key("scrollbar")
                .transition(instant)
                .absolute(grid.cols as f32 * m.cw - 4.0, y)
                .size(3.0, thumb)
                .radius(1.5)
                .fill(Color::WHITE.alpha(0.18)),
        );
    }

    if let Some(preedit) = &s.preedit
        && let Some((cx, cy)) = grid.cursor
    {
        let x = (cx as f32 + 1.0).min(grid.cols.saturating_sub(1) as f32);
        let width = (preedit.chars().count() as f32 * m.cw).max(m.cw);
        area = area.child(
            el()
                .key("preedit-bg")
                .absolute(x * m.cw, cy as f32 * m.ch)
                .size(width, m.ch)
                .radius(2.0)
                .fill(srgb(ACCENT, 0.24)),
        );
        area = area.child(
            text(preedit.clone())
                .key("preedit")
                .font(s.run_style(400.0))
                .color(srgb(DEFAULT_FG, 1.0))
                .absolute(x * m.cw, cy as f32 * m.ch)
                .h(m.ch),
        );
    }

    if s.find.open {
        area = area.child(find_bar(s, &m, grid.cols));
    }

    if s.pending_paste.is_some() {
        area = area.child(paste_overlay(s));
    }

    area
}

/// A multiline paste waits here until confirmed, since Return would run it.
fn paste_overlay(s: &Sash) -> El {
    let t = s.theme();
    let Some(pending) = &s.pending_paste else {
        return el();
    };
    let lines = pending.lines().count().max(1);
    el()
        .key("paste-overlay")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .center()
        .fill(Color::BLACK.alpha(0.24))
        .child(
            col()
                .key("paste-card")
                .w(340.0)
                .px(16.0)
                .py(14.0)
                .gap(12.0)
                .radius(10.0)
                .fill(srgb(DEFAULT_BG, 0.98))
                .border(1.0, Color::WHITE.alpha(0.12))
                .child(
                    text(format!("Paste {lines} lines?"))
                        .font(s.small_style(500.0, 13.0))
                        .color(t.text),
                )
                .child(
                    text("The text runs commands when it reaches the shell.")
                        .font(s.small_style(400.0, 11.0))
                        .color(t.text_secondary),
                )
                .child(
                    row()
                        .w_full()
                        .gap(8.0)
                        .center_y()
                        .child(spacer())
                        .child(
                            el()
                                .key("paste-cancel")
                                .h(26.0)
                                .px(10.0)
                                .radius(6.0)
                                .center()
                                .fill(Color::WHITE.alpha(0.07))
                                .on_press(|s: &mut Sash| s.cancel_paste())
                                .child(
                                    text("Cancel")
                                        .font(s.small_style(400.0, 12.0))
                                        .color(t.text),
                                ),
                        )
                        .child(
                            el()
                                .key("paste-go")
                                .h(26.0)
                                .px(10.0)
                                .radius(6.0)
                                .center()
                                .fill(srgb(ACCENT, 1.0))
                                .on_press(|s: &mut Sash| s.confirm_paste())
                                .child(
                                    text("Paste")
                                        .font(s.small_style(500.0, 12.0))
                                        .color(srgb(DEFAULT_BG, 1.0)),
                                ),
                        ),
                ),
        )
}

pub const DEMO_SCREEN: &str = concat!(
    "\x1b[1;38;2;122;162;247msash\x1b[0m \x1b[90m— a Quake-style drop-down terminal\x1b[0m\r\n",
    "\r\n",
    "\x1b[32m❯\x1b[0m git status\r\n",
    "On branch main\r\n",
    "Your branch is up to date with 'origin/main'.\r\n",
    "\r\n",
    "\x1b[32m❯\x1b[0m printf '\\033[1mbold\\033[0m \\033[31mred\\033[0m \\033[44m blue bg \\033[0m\\n'\r\n",
    "\x1b[1mbold\x1b[0m \x1b[31mred\x1b[0m \x1b[44m blue bg \x1b[0m\r\n",
    "\r\n",
    "\x1b[32m❯\x1b[0m \x1b[7m \x1b[0m\r\n",
);

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Mods, PhysicalKeyCode};

    #[test]
    fn full_height_uses_the_space_above_the_dock() {
        assert_eq!(panel_height(None, 932.0), 932.0);
        assert_eq!(panel_height(Some(420.0), 932.0), 420.0);
        assert_eq!(panel_height(Some(5000.0), 932.0), 932.0);
        assert_eq!(panel_height(Some(10.0), 200.0), 120.0);
    }

    #[test]
    fn a_captured_chord_round_trips_through_the_config() {
        let cases = [
            (
                PhysicalKeyCode::Space,
                Mods { cmd: true, shift: true, ..Default::default() },
                "⇧⌘Space",
            ),
            (PhysicalKeyCode::Slash, Mods { cmd: true, ..Default::default() }, "⌘/"),
            (PhysicalKeyCode::F12, Mods { alt: true, ..Default::default() }, "⌥F12"),
        ];
        for (code, mods, pretty) in cases {
            let shortcut = Shortcut::from_physical_key(code, mods).unwrap();
            let stored = shortcut.to_string();
            let parsed: Shortcut = stored.parse().unwrap();
            assert_eq!(parsed, shortcut, "{stored} must parse back at launch");
            assert_eq!(pretty_hotkey(&stored), pretty);
        }
    }
}
