//! App state, the view, key routing and background pumping.

use std::path::{Path, PathBuf};
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

use crate::config::{Config, Session, SessionTab, TabHighlight};
use crate::macos::Screen;
use crate::selection::{CellPos, Kind, Selection, line_at, word_at};
use crate::split::{Dir, GAP, Rect, Tree};
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
const TAB_MIN_WIDTH: f32 = 120.0;
const TAB_MIN_FILL_WIDTH: f32 = 44.0;
const TAB_GAP: f32 = 3.0;
const TAB_CONTROLS_WIDTH: f32 = 50.0;

/// The colours a tab can carry; the index is what the session stores.
pub const TAB_COLORS: [Rgb; 8] = [
    Rgb::new(0xe0, 0x6c, 0x75),
    Rgb::new(0xd1, 0x9a, 0x66),
    Rgb::new(0xe5, 0xc0, 0x7b),
    Rgb::new(0x98, 0xc3, 0x79),
    Rgb::new(0x56, 0xb6, 0xc2),
    Rgb::new(0x61, 0xaf, 0xef),
    Rgb::new(0xc6, 0x78, 0xdd),
    Rgb::new(0xe0, 0x6c, 0xc0),
];

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

/// One libghostty terminal and its pty. A tab holds at least one; splits add more.
pub struct Pane {
    pub vt: Vt,
    pub pty: Option<Pty>,
    pub dead: bool,
    pub last_title: String,
    /// Folder to show until the shell supplies its first title.
    pub fallback_title: String,
    pub progress: Option<Progress>,
    /// A visual bell rings until this frame time.
    pub bell_until: f64,
    /// The grid size this pane gets from its share of the panel.
    pub metrics: Metrics,
    /// Its slice of the terminal area, in points, set by `Sash::relayout`.
    pub rect: Rect,
}

impl Pane {
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
        let fallback_title = cwd
            .as_deref()
            .and_then(path_title)
            .unwrap_or_else(|| "Home".into());

        if demo {
            vt.write(DEMO_SCREEN.as_bytes());
            vt.refresh(true);
            return Self {
                vt,
                pty: None,
                dead: true,
                last_title: "sash".into(),
                fallback_title: "sash".into(),
                progress: None,
                bell_until: 0.0,
                metrics,
                rect: Rect::default(),
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

        match Pty::spawn(builder, metrics.cols, metrics.rows, cell_w, cell_h, "pane".into(), waker.clone()) {
            Ok(pty) => Self {
                vt,
                pty: Some(pty),
                dead: false,
                last_title: String::new(),
                fallback_title,
                progress: None,
                bell_until: 0.0,
                metrics,
                rect: Rect::default(),
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
                    fallback_title: "sash".into(),
                    progress: None,
                    bell_until: 0.0,
                    metrics,
                    rect: Rect::default(),
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
            self.fallback_title.clone()
        } else {
            directory_title(t).unwrap_or_else(|| truncate(t, 28))
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

/// A readable label for a directory, with the user's home called Home rather
/// than the anonymous-looking tilde used by many shell OSC titles.
fn path_title(path: &Path) -> Option<String> {
    if std::env::var_os("HOME").is_some_and(|home| Path::new(&home) == path) {
        return Some("Home".into());
    }
    path.file_name()
        .map(|name| truncate(&name.to_string_lossy(), 28))
        .or_else(|| path.has_root().then(|| path.to_string_lossy().into_owned()))
}

/// Directory-only OSC titles name the folder, not a punctuation shortcut or a
/// full path. Other titles belong to programs and pass through unchanged.
fn directory_title(title: &str) -> Option<String> {
    let title = title.trim();
    if matches!(title, "~" | "~/") {
        return Some("Home".into());
    }
    if let Some(rest) = title.strip_prefix("~/") {
        return path_title(Path::new(rest));
    }
    title.starts_with('/').then(|| path_title(Path::new(title))).flatten()
}

fn tab_strip_visible(tab_count: usize, any_pinned: bool) -> bool {
    tab_count > 1 || any_pinned
}

/// Widths shared by tab drawing and pointer hit-testing. Natural tabs have a
/// useful minimum even for one-character titles; fill mode divides whatever
/// remains after the strip's controls equally between all tabs.
fn calculate_tab_widths(
    window_width: f32,
    hotkey_width: f32,
    natural: &[f32],
    fill: bool,
) -> Vec<f32> {
    if !fill {
        return natural.iter().map(|width| width.max(TAB_MIN_WIDTH)).collect();
    }
    if natural.is_empty() {
        return Vec::new();
    }
    // Tabs + plus + flexible spacer + settings + hotkey leave n + 3 gaps.
    let gaps = (natural.len() + 3) as f32 * TAB_GAP;
    let reserved = PAD_X * 2.0 + TAB_CONTROLS_WIDTH + hotkey_width + gaps;
    let width = ((window_width - reserved) / natural.len() as f32).max(TAB_MIN_FILL_WIDTH);
    vec![width; natural.len()]
}

/// A tab is one or more panes plus the split tree that lays them out. Every pane
/// owns its own libghostty terminal; the tree is only geometry and focus.
pub struct Tab {
    pub panes: Vec<Pane>,
    pub tree: Tree,
    /// Index into `panes` that owns the keyboard and the cursor.
    pub focus: usize,
    /// A name the user set from the tab menu. OSC titles keep flowing underneath;
    /// clearing this shows them again.
    pub display_name: Option<String>,
    /// Index into TAB_COLORS, or none for the theme's default.
    pub color: Option<u8>,
    /// A pinned tab refuses to close until it is unpinned.
    pub pinned: bool,
    /// Output arrived while this tab was not the one on screen.
    pub unread: bool,
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
        Self {
            panes: vec![Pane::new(cfg, screen, metrics, demo, waker, cwd)],
            tree: Tree::new(0),
            focus: 0,
            display_name: None,
            color: None,
            pinned: false,
            unread: false,
        }
    }

    pub fn focused(&self) -> &Pane {
        let index = self.focus.min(self.panes.len().saturating_sub(1));
        &self.panes[index]
    }

    pub fn focused_mut(&mut self) -> &mut Pane {
        let index = self.focus.min(self.panes.len().saturating_sub(1));
        &mut self.panes[index]
    }

    pub fn title(&self) -> String {
        if let Some(name) = self.display_name.as_deref() {
            let name = name.trim();
            if !name.is_empty() {
                return truncate(name, 28);
            }
        }
        self.focused().title()
    }

    /// The focused pane's shell directory, used by duplicate and session save.
    pub fn current_dir(&self) -> Option<PathBuf> {
        self.focused().current_dir()
    }

    /// Any pane busy, so closing the tab asks first.
    pub fn busy(&self) -> bool {
        self.panes.iter().any(Pane::busy)
    }

    pub fn focused_busy(&self) -> bool {
        self.focused().busy()
    }

    pub fn progress(&self) -> Option<Progress> {
        self.focused().progress
    }

    /// The longest-running bell among the panes, for the tab chip and frame pump.
    pub fn bell_until(&self) -> f64 {
        self.panes.iter().map(|pane| pane.bell_until).fold(0.0, f64::max)
    }
}

/// Terminal fields on a tab mean its focused pane; split and tab fields stay on
/// `Tab`. Code that must reach every pane loops over `tab.panes` explicitly.
impl std::ops::Deref for Tab {
    type Target = Pane;
    fn deref(&self) -> &Pane {
        self.focused()
    }
}

impl std::ops::DerefMut for Tab {
    fn deref_mut(&mut self) -> &mut Pane {
        self.focused_mut()
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
    /// The window is open at zero height while the display is resolved; the reveal
    /// starts once the window is placed and resized.
    pub pending_reveal: bool,

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
    /// A tab or pane waiting on "a command is still running" confirmation.
    pub pending_close: Option<CloseTarget>,
    /// The quit waiting on the same confirmation.
    pub pending_quit: bool,
    /// The input method's marked text, drawn at the caret until it commits.
    pub preedit: Option<String>,
    /// The panel height when a bottom-edge drag began.
    pub resize_drag: Option<f32>,
    /// The configured height before the panel was maximized, kept for restore.
    pub pre_max: Option<Option<f32>>,
    /// The ⌘P quick open overlay.
    pub palette: Palette,
    /// The tab bar context menu, anchored at the secondary press.
    pub tab_menu: Option<TabMenu>,
    /// The inline tab rename field.
    pub rename: Option<Rename>,
    /// The one-time first-run card, shown on the panel's first reveal.
    pub welcome: bool,
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

/// The settings page groups its controls by what they change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingsTab {
    #[default]
    Panel,
    Terminal,
    Tabs,
    Keys,
}

impl SettingsTab {
    pub const ALL: [Self; 4] = [Self::Panel, Self::Terminal, Self::Tabs, Self::Keys];

    pub fn label(self) -> &'static str {
        match self {
            Self::Panel => "Panel",
            Self::Terminal => "Terminal",
            Self::Tabs => "Tabs",
            Self::Keys => "Shortcuts",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Panel => "panel",
            Self::Terminal => "terminal",
            Self::Tabs => "tabs",
            Self::Keys => "keys",
        }
    }
}

/// State for the in-panel settings page.
#[derive(Default)]
pub struct SettingsPane {
    pub open: bool,
    /// The group on screen.
    pub tab: SettingsTab,
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

/// The ⌘P quick open overlay: an input, a selection, and an action list.
pub struct Palette {
    pub open: bool,
    pub edit: TextEdit,
    pub selected: usize,
}

impl Palette {
    fn new() -> Self {
        Self { open: false, edit: TextEdit::new(String::new()), selected: 0 }
    }
}

/// A right-click menu on one tab, anchored in window points.
#[derive(Clone, Copy, Debug)]
pub struct TabMenu {
    pub tab: usize,
    pub x: f32,
    pub y: f32,
}

/// What a close confirmation will close once the user agrees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseTarget {
    Tab(usize),
    Pane { tab: usize, pane: usize },
}

/// A tab rename waiting for Return or Escape.
pub struct Rename {
    pub tab: usize,
    pub edit: TextEdit,
}

#[derive(Clone, Debug)]
pub enum PaletteAction {
    SelectTab(usize),
    NewTab,
    NewTabAt(PathBuf),
    DuplicateTab,
    RenameTab,
    CloseTab,
    ClosePane,
    SplitRight,
    SplitDown,
    Settings,
    OpenPath { path: PathBuf, line: Option<u32> },
    Maximize,
    FontUp,
    FontDown,
}

pub struct PaletteItem {
    pub label: String,
    pub detail: String,
    /// Extra text the filter can match but the row does not show.
    pub search: String,
    pub action: PaletteAction,
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
        // A Finder launch has no window and no Dock icon, so the first run shows
        // the panel itself instead of looking like nothing happened.
        let welcome = !cfg.welcome_seen && !demo && sash_command().is_none();
        let session = if restoring { Session::load() } else { Session::default() };
        let mut tabs: Vec<Tab> = session
            .tabs
            .iter()
            .take(12)
            .map(|tab| {
                let mut pane = Tab::new(&cfg, screen, metrics, demo, &waker, tab.cwd.clone());
                pane.display_name = tab.name.clone();
                pane.color = tab.color.filter(|color| (*color as usize) < TAB_COLORS.len());
                pane.pinned = tab.pinned;
                pane
            })
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
            want_show: settings_open
                || welcome
                || std::env::var_os("SASH_SHOW").is_some(),
            want_place: false,
            pending_reveal: false,
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
            resize_drag: None,
            pre_max: None,
            palette: Palette::new(),
            tab_menu: None,
            rename: None,
            welcome,
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

    pub fn active_pane(&self) -> Option<&Pane> {
        self.active_tab().map(Tab::focused)
    }

    pub fn active_pane_mut(&mut self) -> Option<&mut Pane> {
        self.active_tab_mut().map(Tab::focused_mut)
    }

    /// Give the keyboard to one pane of the active tab. Selection, find and
    /// preedit belong to the focused pane, so they start clean.
    pub fn focus_pane(&mut self, index: usize) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        if index >= tab.panes.len() || index == tab.focus {
            return;
        }
        self.close_find();
        self.selection = None;
        self.preedit = None;
        self.wheel.reset();
        self.mouse_down = false;
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        tab.focus = index;
        tab.unread = false;
        self.metrics = Some(tab.focused().metrics);
    }

    pub fn focus_next_pane(&mut self, forward: bool) {
        let Some(tab) = self.active_tab() else { return };
        let next = tab.tree.focus_next(tab.focus, forward);
        self.focus_pane(next);
    }

    /// Split the focused pane right or down, spawning a shell in its directory.
    pub fn split_focused(&mut self, dir: Dir) {
        let Some(tab) = self.active_tab() else { return };
        let cwd = tab.current_dir();
        let metrics = tab.focused().metrics;
        self.close_find();
        let pane = Pane::new(&self.cfg, self.screen, metrics, self.demo, &self.waker, cwd);
        let active = self.active;
        let Some(tab) = self.tabs.get_mut(active) else { return };
        let index = tab.panes.len();
        if !tab.tree.split(tab.focus, index, dir) {
            return;
        }
        tab.panes.push(pane);
        tab.focus = index;
        tab.unread = false;
        self.selection = None;
        self.preedit = None;
        self.wheel.reset();
        self.relayout();
    }

    /// Close one pane; the tab goes with its last pane.
    pub fn close_pane(&mut self, tab_index: usize, pane_index: usize) {
        let Some(tab) = self.tabs.get(tab_index) else { return };
        if pane_index >= tab.panes.len() {
            return;
        }
        if tab.panes.len() == 1 {
            self.close_tab(tab_index);
            return;
        }
        if tab_index == self.active && pane_index == tab.focus {
            self.close_find();
        }
        let Some(tab) = self.tabs.get_mut(tab_index) else { return };
        let next = tab.tree.focus_next(pane_index, true);
        let next_after = if next > pane_index { next - 1 } else { next };
        let old_focus = tab.focus;
        if !tab.tree.remove(pane_index) {
            return;
        }
        tab.tree.remap_after_remove(pane_index);
        tab.panes.remove(pane_index);
        tab.focus = if old_focus == pane_index {
            next_after.min(tab.panes.len() - 1)
        } else if old_focus > pane_index {
            old_focus - 1
        } else {
            old_focus
        };
        if tab_index == self.active {
            self.selection = None;
            self.preedit = None;
            self.wheel.reset();
        }
        self.relayout();
        self.save_session();
    }

    pub fn request_close_pane(&mut self, tab_index: usize, pane_index: usize) {
        let Some(tab) = self.tabs.get(tab_index) else { return };
        if tab.panes.len() == 1 {
            self.request_close_tab(tab_index);
            return;
        }
        let busy = tab.panes.get(pane_index).is_some_and(Pane::busy);
        if busy && self.cfg.confirm_close {
            self.pending_close = Some(CloseTarget::Pane { tab: tab_index, pane: pane_index });
            self.pending_quit = false;
            return;
        }
        self.close_pane(tab_index, pane_index);
    }

    pub fn request_close_focused(&mut self) {
        let Some(tab) = self.active_tab() else { return };
        let focus = tab.focus;
        self.request_close_pane(self.active, focus);
    }

    pub fn metrics(&self) -> Metrics {
        self.active_pane()
            .map(|pane| pane.metrics)
            .or(self.metrics)
            .unwrap_or_else(|| provisional_metrics(&self.cfg, self.width, self.height))
    }

    /// The strip collapses for one ordinary tab, but a pin keeps it visible.
    fn tab_bar_visible(&self) -> bool {
        tab_strip_visible(self.tabs.len(), self.tabs.iter().any(|tab| tab.pinned))
    }

    pub fn bar_height(&self) -> f32 {
        if self.tab_bar_visible() { TAB_H } else { 0.0 }
    }

    fn tab_widths(&self) -> Vec<f32> {
        let natural = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let weight = if index == self.active { 500.0 } else { 400.0 };
                let active_dot = index == self.active && self.cfg.tab_highlight == TabHighlight::Dot;
                18.0
                    + if tab.color.is_some() { 14.0 } else { 0.0 }
                    + if tab.unread { 14.0 } else { 0.0 }
                    + if active_dot { 13.0 } else { 0.0 }
                    + rune::text::advance(&tab.title(), self.small_style(weight, 12.0))
                    + 7.0
                    + 15.0
            })
            .collect::<Vec<_>>();
        let hotkey = rune::text::advance(&pretty_hotkey(&self.cfg.hotkey), self.small_style(400.0, 11.0));
        calculate_tab_widths(self.width, hotkey, &natural, self.cfg.tabs_fill_width)
    }

    /// The tab whose chip contains `x`, using exactly the widths the strip draws.
    pub fn tab_at_x(&self, x: f32) -> Option<usize> {
        let mut left = PAD_X;
        for (index, width) in self.tab_widths().into_iter().enumerate() {
            if x >= left && x <= left + width {
                return Some(index);
            }
            left += width + TAB_GAP;
        }
        None
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

    /// Re-measure the monospace cell once the app's fonts are loaded, then give
    /// every pane its share of the panel.
    pub fn init_metrics(&mut self) -> bool {
        let style = self.run_style(400.0);
        let advance = rune::text::advance("MMMMMMMMMM", style);
        if !advance.is_finite() || advance <= 0.0 {
            return false;
        }
        let cw = (advance / 10.0 * 64.0).round() / 64.0;
        let ch = (self.cfg.font_size * 1.32).round().max(10.0);
        self.metrics = Some(Metrics { cw, ch, cols: 0, rows: 0 });
        self.relayout();
        true
    }

    /// The whole area the pane tree divides: below the tab strip, if it is showing.
    pub fn terminal_area(&self) -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            w: self.width.max(1.0),
            h: (self.height - self.bar_height()).max(1.0),
        }
    }

    /// Lay every tab's split tree out, then resize each pane's libghostty grid to
    /// the cells that fit its slice. Terminal state stays libghostty's; only the
    /// geometry is sash's.
    pub fn relayout(&mut self) {
        let Some(cell) = self.metrics else { return };
        let (cw, ch) = (cell.cw, cell.ch);
        if cw <= 0.0 || ch <= 0.0 {
            return;
        }
        let area = self.terminal_area();
        let screen = self.screen;
        for tab in &mut self.tabs {
            for (index, rect) in tab.tree.layout(area, GAP) {
                let Some(pane) = tab.panes.get_mut(index) else { continue };
                pane.rect = rect;
                let cols = (((rect.w - PAD_X * 2.0).max(cw)) / cw).floor() as u16;
                let rows = (((rect.h - PAD_Y * 2.0).max(ch)) / ch).floor() as u16;
                let metrics = Metrics {
                    cw,
                    ch,
                    cols: cols.max(4),
                    rows: rows.max(2),
                };
                pane.resize(metrics, screen);
                pane.metrics = metrics;
            }
        }
        if let Some(metrics) = self.active_tab().map(|tab| tab.focused().metrics) {
            self.metrics = Some(metrics);
        }
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
        self.close_palette();
        self.tab_menu = None;
        self.cancel_rename();
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
        // Do not use Rune's set_always_on_top here: it queues a normal/floating
        // level that would overwrite this synchronous change after the handler.
        // Its runtime setter changes only the level, not the panel's Spaces behavior.
        crate::macos::set_panel_level(on);
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
        let size = ((size * 2.0).round() / 2.0).clamp(6.0, 72.0);
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
            for pane in &mut tab.panes {
                pane.vt.set_scrollback(Some(lines));
            }
        }
        self.cfg.save();
    }

    /// Cycle Follow pointer → Display 1 → Display 2 → … → Follow pointer. The
    /// panel moves immediately; a display that is gone falls back to the main one.
    pub fn cycle_display(&mut self, tx: &mut Tx) {
        let count = crate::macos::displays().len();
        let next = next_display(self.cfg.display, count);
        if self.cfg.display == next {
            return;
        }
        self.cfg.display = next;
        self.cfg.save();
        self.want_place = true;
        tx.wake_after(Duration::ZERO);
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

    /// `fraction` of the usable height (screen top to Dock); 1.0 stores "full".
    pub fn set_height_fraction(&mut self, fraction: f32, tx: &mut Tx) {
        let full = self.screen.usable_height_pt().max(120.0);
        let fraction = fraction.clamp(0.2, 1.0);
        self.cfg.height = if fraction >= 0.995 {
            None
        } else {
            Some((full * fraction).round())
        };
        self.pre_max = None;
        self.apply_height(tx);
        self.cfg.save();
    }

    /// While the bottom edge is dragged: follow the pointer without writing the
    /// config on every frame; the release commits it.
    fn set_height_live(&mut self, height: f32, tx: &mut Tx) {
        let full = self.screen.usable_height_pt().max(120.0);
        let height = height.clamp(120.0, full);
        if (height - self.height).abs() < 0.5 {
            return;
        }
        self.cfg.height = if height >= full - 0.5 {
            None
        } else {
            Some(height.round())
        };
        self.height = height;
        self.init_metrics();
        if self.shown {
            self.reveal = height;
        }
        tx.resize_window_instant((self.width, height));
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

    /// Re-resolve the configured display before the next reveal, and ask Rune to put
    /// the panel at its top edge. By default the panel follows the pointer across
    /// screens; with `display` set it stays on (or moves to) that display.
    pub fn sync_display(&mut self, tx: &mut Tx) -> bool {
        if !self.want_place {
            return false;
        }
        if crate::window().is_none() {
            // Headless runs have no native window to place. A real run waits for
            // the handle: `on_ready` wakes it, and the reveal starts then.
            if rune::is_headless() {
                self.want_place = false;
                if self.pending_reveal {
                    self.pending_reveal = false;
                    self.animate_to(self.height, false);
                }
            }
            return false;
        }
        self.want_place = false;
        let Some(screen) = crate::macos::preferred_screen(self.cfg.display) else {
            return false;
        };
        let changed = screen.width_px != self.screen.width_px
            || screen.usable_height_px != self.screen.usable_height_px
            || screen.top != self.screen.top
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
            if self.shown && self.pending_reveal {
                self.reveal = 0.0;
            } else if self.shown {
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
        if self.pending_reveal {
            // A resize is applied after this turn. Give it one turn before the
            // first visible frame; the panel is still at zero height.
            if changed {
                self.want_place = true;
                tx.wake_after(Duration::from_millis(8));
            } else {
                self.pending_reveal = false;
                self.animate_to(self.height, false);
            }
        }
        changed
    }

    /// Place the window on the pointer's display, flush with its physical top edge.
    /// Rune handles mixed backing scales and display origins.
    fn place(&self) {
        if crate::macos::place_panel(self.screen.id, self.width as f64, self.height as f64) {
            return;
        }
        if let Some(window) = crate::window() {
            window.place(
                DisplayTarget::UnderPointer,
                OverlayPlacement::Edge {
                    edge: ScreenEdge::Top,
                    margin: 0.0,
                    align: Align::Center,
                    inset: 0.0,
                },
            );
        }
    }

    /// The first-run card only ever appears once, so dismissal is saved at once.
    pub fn dismiss_welcome(&mut self) {
        self.welcome = false;
        if !self.cfg.welcome_seen {
            self.cfg.welcome_seen = true;
            self.cfg.save();
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
        // Window levels below 25 let AppKit paint the menu bar over the panel.
        crate::macos::set_panel_level(self.cfg.always_on_top);
        // Asking for keys counts as holding them, even before the platform's focus
        // event lands: otherwise a hide-on-blur would never fire for this reveal.
        self.had_focus = self.cfg.focus_on_open;
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.unread = false;
        }
        // Open at zero height: the display is resolved and the window placed and
        // resized while nothing is visible, then the reveal starts. Showing first
        // and resizing mid-slide is what flashes the old surface on a second display.
        self.reveal = 0.0;
        self.pending_reveal = true;
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
        self.close_palette();
        self.tab_menu = None;
        self.cancel_rename();
        self.cancel_pending();
        self.shown = false;
        self.pending_reveal = false;
        self.want_place = false;
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
        // A newly appearing strip takes a row back from the grid.
        self.init_metrics();
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
                .map(|tab| SessionTab {
                    cwd: tab.current_dir(),
                    name: tab.display_name.clone(),
                    color: tab.color,
                    pinned: tab.pinned,
                })
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
        if self.tabs.get(index).is_some_and(|tab| tab.pinned) {
            return;
        }
        let busy = self.tabs.get(index).is_some_and(Tab::busy);
        if busy && self.cfg.confirm_close {
            self.pending_close = Some(CloseTarget::Tab(index));
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
        match self.pending_close.take() {
            Some(CloseTarget::Tab(index)) => self.close_tab(index),
            Some(CloseTarget::Pane { tab, pane }) => self.close_pane(tab, pane),
            None => {}
        }
    }

    pub fn cancel_pending(&mut self) {
        self.pending_close = None;
        self.pending_quit = false;
    }

    pub fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() || self.tabs[index].pinned {
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
        // The last tab gone, the strip gives its row back to the grid.
        if self.tabs.len() == 1 {
            self.init_metrics();
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

    /// Start renaming a tab, with its current title in the field.
    pub fn begin_rename(&mut self, index: usize) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let current = tab
            .display_name
            .clone()
            .unwrap_or_else(|| tab.title());
        self.tab_menu = None;
        self.close_find();
        self.preedit = None;
        let mut edit = TextEdit::new(current);
        edit.focused = true;
        self.rename = Some(Rename { tab: index, edit });
    }

    pub fn commit_rename(&mut self) {
        let Some(rename) = self.rename.take() else {
            return;
        };
        let name = rename.edit.text.trim().to_string();
        if let Some(tab) = self.tabs.get_mut(rename.tab) {
            tab.display_name = (!name.is_empty()).then_some(name);
        }
        self.save_session();
    }

    pub fn cancel_rename(&mut self) {
        self.rename = None;
    }

    /// Clicking anywhere but the rename field cancels it; the button paths set
    /// the state themselves.
    pub fn sync_rename(&mut self) -> bool {
        if self
            .rename
            .as_ref()
            .is_some_and(|rename| !rename.edit.focused)
        {
            self.rename = None;
            return true;
        }
        false
    }

    /// Forget a manual name; the shell's OSC title shows through again.
    pub fn clear_tab_name(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.display_name = None;
            self.save_session();
        }
    }

    /// Give a tab one of TAB_COLORS, or none for the theme default.
    pub fn set_tab_color(&mut self, index: usize, color: Option<u8>) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.color = color.filter(|color| (*color as usize) < TAB_COLORS.len());
        }
        self.tab_menu = None;
        self.save_session();
    }

    /// Protect a tab from Close Tab and its × until it is unpinned.
    pub fn toggle_tab_pin(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.pinned = !tab.pinned;
        }
        self.tab_menu = None;
        // Pinning the only tab reveals the strip and takes a row from the grid.
        self.relayout();
        self.save_session();
    }

    /// Reorder tabs after a drag or a menu command. The active tab follows its tab.
    pub fn move_tab(&mut self, from: usize, to: usize) {
        if from >= self.tabs.len() || from == to {
            return;
        }
        let to = to.min(self.tabs.len() - 1);
        let tab = self.tabs.remove(from);
        let mut active = self.active;
        if from == active {
            active = to;
        } else {
            if from < active {
                active -= 1;
            }
            if to <= active {
                active += 1;
            }
        }
        self.tabs.insert(to, tab);
        self.active = active.min(self.tabs.len() - 1);
        self.save_session();
    }

    pub fn open_palette(&mut self) {
        if self.palette.open {
            return;
        }
        self.close_find();
        self.preedit = None;
        self.palette.open = true;
        self.palette.edit = TextEdit::new(String::new());
        self.palette.edit.focused = true;
        self.palette.selected = 0;
        if !self.shown {
            self.show();
        }
    }

    pub fn close_palette(&mut self) {
        self.palette.open = false;
        self.palette.edit.focused = false;
        self.palette.selected = 0;
    }

    pub fn toggle_palette(&mut self) {
        if self.palette.open {
            self.close_palette();
        } else {
            self.open_palette();
        }
    }

    /// Everything the palette can reach. Tabs first, then app actions, then the
    /// directories the open tabs are sitting in.
    pub fn palette_items(&self) -> Vec<PaletteItem> {
        let mut items = Vec::new();
        if let Some((path, line)) = palette_target(&self.palette.edit.text) {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            let suffix = line.map(|line| format!(":{line}")).unwrap_or_default();
            items.push(PaletteItem {
                label: format!("Open {name}{suffix}"),
                detail: path.to_string_lossy().into_owned(),
                search: self.palette.edit.text.trim().to_string(),
                action: PaletteAction::OpenPath { path, line },
            });
        }
        for (i, tab) in self.tabs.iter().enumerate() {
            items.push(PaletteItem {
                label: tab.title(),
                detail: if i == self.active {
                    "Current tab".into()
                } else {
                    format!("Tab {}", i + 1)
                },
                search: String::new(),
                action: PaletteAction::SelectTab(i),
            });
        }
        let actions = [
            ("New Tab", "⌘T", PaletteAction::NewTab),
            ("Duplicate Tab", "⌘⇧T", PaletteAction::DuplicateTab),
            ("Rename Tab…", "Name this tab", PaletteAction::RenameTab),
            ("Close Tab", "⌘⇧W", PaletteAction::CloseTab),
            ("Close Pane", "⌘W", PaletteAction::ClosePane),
            ("Split Right", "⌘D", PaletteAction::SplitRight),
            ("Split Down", "⌘⇧D", PaletteAction::SplitDown),
            ("Settings", "⌘,", PaletteAction::Settings),
            ("Maximize Panel", "⌘⇧M", PaletteAction::Maximize),
            ("Larger Font", "⌘+", PaletteAction::FontUp),
            ("Smaller Font", "⌘−", PaletteAction::FontDown),
        ];
        for (label, detail, action) in actions {
            items.push(PaletteItem {
                label: label.into(),
                detail: detail.into(),
                search: String::new(),
                action,
            });
        }
        let mut seen = Vec::new();
        for tab in &self.tabs {
            let Some(dir) = tab.current_dir() else { continue };
            if seen.contains(&dir) {
                continue;
            }
            seen.push(dir.clone());
            let name = dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.to_string_lossy().into_owned());
            items.push(PaletteItem {
                label: format!("New Tab in {name}"),
                detail: dir.to_string_lossy().into_owned(),
                search: String::new(),
                action: PaletteAction::NewTabAt(dir),
            });
        }
        items
    }

    /// Item indices that match every word in the query, in display order.
    pub fn palette_matches(&self) -> Vec<usize> {
        let items = self.palette_items();
        let query = self.palette.edit.text.to_lowercase();
        items
            .iter()
            .enumerate()
            .filter(|(_, item)| palette_matches(item, &query))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn move_palette(&mut self, delta: isize) {
        let count = self.palette_matches().len();
        if count == 0 {
            self.palette.selected = 0;
            return;
        }
        let n = count as isize;
        self.palette.selected = (((self.palette.selected as isize + delta) % n + n) % n) as usize;
    }

    pub fn run_palette(&mut self, tx: &mut Tx) {
        let matches = self.palette_matches();
        let Some(index) = matches.get(self.palette.selected).copied() else {
            return;
        };
        let Some(item) = self.palette_items().into_iter().nth(index) else {
            return;
        };
        self.run_palette_action(item.action, tx);
    }

    pub fn run_palette_action(&mut self, action: PaletteAction, tx: &mut Tx) {
        self.close_palette();
        match action {
            PaletteAction::SelectTab(index) => self.select(index),
            PaletteAction::NewTab => self.new_tab(),
            PaletteAction::NewTabAt(dir) => self.spawn_tab(Some(dir)),
            PaletteAction::DuplicateTab => self.duplicate_tab(),
            PaletteAction::RenameTab => self.begin_rename(self.active),
            PaletteAction::CloseTab => self.request_close_tab(self.active),
            PaletteAction::ClosePane => self.request_close_focused(),
            PaletteAction::SplitRight => self.split_focused(Dir::Right),
            PaletteAction::SplitDown => self.split_focused(Dir::Down),
            PaletteAction::OpenPath { path, line } => open_in_editor(&path, line),
            PaletteAction::Settings => self.open_settings(),
            PaletteAction::Maximize => self.toggle_maximize(tx),
            PaletteAction::FontUp => self.set_font_size(self.cfg.font_size + 1.0),
            PaletteAction::FontDown => self.set_font_size(self.cfg.font_size - 1.0),
        }
    }

    /// ⌘⇧M: fill the space above the Dock, or go back to the configured height.
    pub fn toggle_maximize(&mut self, tx: &mut Tx) {
        if let Some(previous) = self.pre_max.take() {
            self.cfg.height = previous;
        } else if self.cfg.height.is_some() {
            self.pre_max = Some(self.cfg.height);
            self.cfg.height = None;
        } else {
            return;
        }
        self.cfg.save();
        self.apply_height(tx);
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
            || (self.palette.open && self.palette.edit.focused)
            || self.rename.as_ref().is_some_and(|rename| rename.edit.focused)
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

    /// Drain every pane's pty output into its terminal state. Returns true when
    /// anything visible changed.
    pub fn pump(&mut self) -> bool {
        let mut changed = false;
        let (active, visible) = (self.active, self.shown && !self.settings.open);
        let now = self.now;
        let mut announcements = Vec::new();
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            let active_tab = visible && index == active;
            for (pane_index, pane) in tab.panes.iter_mut().enumerate() {
                if let Some(pty) = pane.pty.as_mut() {
                    let bytes = pty.drain();
                    if !bytes.is_empty() {
                        pane.vt.write(&bytes);
                        changed = true;
                        if !active_tab {
                            tab.unread = true;
                        }
                    }
                    let replies = pane.vt.take_replies();
                    if !replies.is_empty() {
                        pty.write(&replies);
                    }
                    if pty.eof() && !pane.dead {
                        pane.dead = true;
                        // A child that exited on its own leaves a zombie until it is
                        // waited on; the pty being dropped is not enough.
                        let _ = pty.try_wait();
                        pane.vt.write(b"\r\n\x1b[90m[sash] process exited. \xe2\x8c\x98T opens a new tab.\x1b[0m\r\n");
                        changed = true;
                    }
                }
                if pane.vt.take_bell() {
                    pane.bell_until = now + 0.45;
                    changed = true;
                    if !active_tab {
                        tab.unread = true;
                    }
                }
                let progress = pane.vt.progress();
                if progress != pane.progress {
                    pane.progress = progress;
                    changed = true;
                }
                let notes = pane.vt.take_notifications();
                if !notes.is_empty() {
                    if !active_tab {
                        tab.unread = true;
                    }
                    announcements.extend(notes);
                    changed = true;
                }
                if let Some(end) = pane.vt.take_command_end() {
                    changed = true;
                    let focused = self.focused
                        && active_tab
                        && pane_index == tab.focus;
                    if !focused {
                        let failed =
                            end.exit.is_some_and(|code| code != 0) || !end.error.is_empty();
                        let title = if failed { "Command failed" } else { "Command finished" };
                        let mut body = pane.title();
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
                if pane.vt.refresh(false) {
                    changed = true;
                }
                if pane.vt.title() != pane.last_title {
                    pane.last_title = pane.vt.title().to_string();
                    changed = true;
                }
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

    /// Apply one menu-bar action from the tray event handler.
    pub fn handle_tray(&mut self, id: &str) {
        match id {
            "toggle" => self.toggle(),
            "new-tab" => self.new_tab(),
            "settings" => self.open_settings(),
            "quit" => self.request_quit(),
            _ => {}
        }
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
        if self.welcome {
            if matches!(ev.key, Key::Escape | Key::Enter) {
                self.dismiss_welcome();
            }
            return;
        }
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
        if self.tab_menu.is_some() {
            if ev.key == Key::Escape {
                self.tab_menu = None;
            }
            return;
        }
        if self.rename.is_some() {
            if ev.key == Key::Escape {
                self.cancel_rename();
            }
            return;
        }
        if self.palette.open {
            if ev.key == Key::Command('p') || ev.key == Key::Escape {
                self.close_palette();
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
                't' => {
                    if ev.mods.shift {
                        self.duplicate_tab();
                    } else {
                        self.new_tab();
                    }
                }
                'd' => self.split_focused(if ev.mods.shift { Dir::Down } else { Dir::Right }),
                'w' => {
                    if ev.mods.shift {
                        self.request_close_tab(self.active);
                    } else {
                        self.request_close_focused();
                    }
                }
                'm' => {
                    if ev.mods.shift {
                        self.toggle_maximize(tx);
                    }
                }
                'c' => self.copy_selection(tx),
                'a' => self.select_all(),
                'v' => self.paste(),
                'h' => self.hide(),
                'q' => self.request_quit(),
                'p' => self.toggle_palette(),
                '+' | '=' => self.set_font_size(self.cfg.font_size + 1.0),
                '-' => self.set_font_size(self.cfg.font_size - 1.0),
                '1'..='9' => self.select(c as usize - '1' as usize),
                '[' => {
                    if ev.mods.shift {
                        self.cycle(-1);
                    } else {
                        self.focus_next_pane(false);
                    }
                }
                ']' => {
                    if ev.mods.shift {
                        self.cycle(1);
                    } else {
                        self.focus_next_pane(true);
                    }
                }
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


/// A file target typed into the palette: `path`, `path:line` or `path:line:col`.
/// Only an existing file becomes an item, so ordinary searches are unaffected.
fn palette_target(query: &str) -> Option<(PathBuf, Option<u32>)> {
    let query = query.trim();
    if query.is_empty() || query.contains('\n') {
        return None;
    }
    let expand = |text: &str| -> PathBuf {
        if let Some(rest) = text.strip_prefix("~/")
            && let Some(home) = std::env::var_os("HOME")
        {
            return PathBuf::from(home).join(rest);
        }
        PathBuf::from(text)
    };
    let direct = expand(query);
    if direct.is_file() {
        return Some((direct, None));
    }
    let (without_col, col) = query
        .rsplit_once(':')
        .filter(|(_, value)| value.parse::<u32>().is_ok())
        .unwrap_or((query, "0"));
    let after_line = expand(without_col);
    if col != "0"
        && after_line.is_file()
        && let Ok(line) = col.parse::<u32>()
    {
        return Some((after_line, Some(line)));
    }
    if let Some((path, line)) = without_col.rsplit_once(':')
        && let Ok(line) = line.parse::<u32>()
    {
        let path = expand(path);
        if path.is_file() {
            return Some((path, Some(line)));
        }
    }
    None
}

/// Open a palette target in the user's editor. VS Code and Cursor understand
/// `file:line` on their CLI; everything else opens through LaunchServices.
fn open_in_editor(path: &Path, line: Option<u32>) {
    const EDITORS: [&str; 2] = [
        "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
        "/Applications/Cursor.app/Contents/Resources/app/bin/cursor",
    ];
    if let Some(line) = line {
        for editor in EDITORS {
            if !Path::new(editor).is_file() {
                continue;
            }
            let target = format!("{}:{line}", path.display());
            if let Ok(mut child) = std::process::Command::new(editor)
                .arg("-g")
                .arg(target)
                .spawn()
            {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return;
            }
        }
    }
    if let Ok(mut child) = std::process::Command::new("/usr/bin/open").arg(path).spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Every whitespace-separated term must appear in the label or the detail.
fn palette_matches(item: &PaletteItem, query: &str) -> bool {
    let label = item.label.to_lowercase();
    let detail = item.detail.to_lowercase();
    let search = item.search.to_lowercase();
    query
        .split_whitespace()
        .all(|term| label.contains(term) || detail.contains(term) || search.contains(term))
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

/// The next display choice in the settings cycle. `count` is how many displays are
/// plugged in; an index that is configured but missing stays in the cycle so the
/// user can move past it when it comes back.
fn next_display(current: Option<u32>, count: usize) -> Option<u32> {
    let last = count.max(current.unwrap_or(0) as usize) as u32;
    match current {
        None if last > 0 => Some(1),
        None => None,
        Some(n) if n < last => Some(n + 1),
        Some(_) => None,
    }
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

/// The end of a long path, kept short enough for a palette row.
fn tail(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let skip = count.saturating_sub(max.saturating_sub(1));
    let mut out = String::from("…");
    out.extend(s.chars().skip(skip));
    out
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
        .border(1.0, border);
    let bar = s.tab_bar_visible();
    if bar && !s.cfg.tabs_at_bottom {
        card = card.child(tab_bar(s));
    }
    card = card.child(if s.settings.open { settings_page(s) } else { terminal(s) });
    if bar && s.cfg.tabs_at_bottom {
        card = card.child(tab_bar(s));
    }
    card = card.child(resize_handle(s));
    if s.palette.open {
        card = card.child(palette_backdrop());
        card = card.child(palette_card(s));
    }
    if s.rename.is_some() {
        card = card.child(rename_overlay(s));
    }
    if let Some(menu) = s.tab_menu {
        card = card.child(tab_menu_backdrop());
        card = card.child(tab_menu_card(s, menu));
    }
    if s.pending_quit || s.pending_close.is_some() {
        card = card.child(confirm_overlay(s));
    }
    if s.welcome {
        card = card.child(welcome_overlay(s));
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


/// A five-point grab strip on the panel's bottom edge, in the padding below the grid.
fn resize_handle(s: &Sash) -> El {
    el()
        .key("resize-handle")
        .absolute(0.0, s.height - 5.0)
        .w_full()
        .h(5.0)
        .fill(Color::WHITE.alpha(0.02))
        .on_drag(|s: &mut Sash, d: &mut Drag, tx: &mut Tx| match d.phase {
            Phase::Down => {
                s.resize_drag = Some(s.height);
                s.pre_max = None;
            }
            Phase::Move => {
                if let Some(start) = s.resize_drag {
                    s.set_height_live(start + d.dy, tx);
                }
            }
            Phase::Up => {
                if let Some(start) = s.resize_drag.take() {
                    if d.cancelled() {
                        s.set_height_live(start, tx);
                    }
                    s.cfg.save();
                }
            }
        })
}

fn tab_bar(s: &Sash) -> El {
    let t = s.theme();
    let accent = srgb(ACCENT, 1.0);
    let widths = s.tab_widths();
    let mut bar = row()
        .key("tabbar")
        .w_full()
        .h(TAB_H)
        .px(PAD_X)
        .gap(TAB_GAP)
        .center_y()
        .fill(Color::WHITE.alpha(0.02));

    for (i, tab) in s.tabs.iter().enumerate() {
        let active = i == s.active;
        let title = tab.title();
        let ringing = tab.bell_until() > s.now;
        let fill = if ringing {
            srgb(ACCENT, 0.26)
        } else if active && s.cfg.tab_highlight == TabHighlight::WholeTab {
            Color::WHITE.alpha(0.085)
        } else {
            Color::TRANSPARENT
        };
        let mut chip = row()
            .key(format!("tab-{i}"))
            .w(widths[i])
            .h(25.0)
            .px(9.0)
            .gap(7.0)
            .radius(7.0)
            .center_y()
            .fill(fill)
            .on_secondary_press(move |s: &mut Sash, p, _tx| {
                s.select(i);
                s.tab_menu = Some(TabMenu { tab: i, x: p.x, y: p.y });
            })
            .on_drag(move |s: &mut Sash, d: &mut Drag, _tx: &mut Tx| {
                if d.phase == Phase::Down {
                    s.select(i);
                    return;
                }
                // Drop on whatever tab is under the pointer.
                if d.phase == Phase::Up && d.clicks == 1 && d.dx.abs() > 6.0 {
                    let point = d.window_point();
                    let bar_top = if s.cfg.tabs_at_bottom { s.height - TAB_H } else { 0.0 };
                    if point[1] >= bar_top
                        && point[1] <= bar_top + TAB_H
                        && let Some(to) = s.tab_at_x(point[0])
                    {
                        s.move_tab(i, to);
                    }
                }
            });
        if active && s.cfg.tab_highlight == TabHighlight::Dot {
            chip = chip.child(
                el()
                    .key(format!("active-dot-{i}"))
                    .size(6.0, 6.0)
                    .radius(3.0)
                    .fill(accent),
            );
        }
        if let Some(color) = tab.color.and_then(tab_color) {
            chip = chip.child(
                el()
                    .key(format!("color-{i}"))
                    .size(7.0, 7.0)
                    .radius(3.5)
                    .fill(color),
            );
        }
        if tab.unread {
            // A hollow ring, so background output never reads as the solid Dot
            // that marks the active tab.
            let error = matches!(tab.progress(), Some(p) if p.state == ProgressState::Error);
            chip = chip.child(
                el()
                    .key(format!("dot-{i}"))
                    .size(7.0, 7.0)
                    .radius(3.5)
                    .border(1.2, if error { srgb(ERROR, 1.0) } else { accent }),
            );
        }
        let title_color = if active && s.cfg.tab_highlight == TabHighlight::Text {
            accent
        } else if active {
            t.text
        } else {
            t.text_secondary
        };
        chip = chip.child(
            text(title)
                .font(s.small_style(if active { 500.0 } else { 400.0 }, 12.0))
                .color(title_color)
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        );
        chip = if tab.pinned {
            chip.child(
                el()
                    .key(format!("pin-{i}"))
                    .size(15.0, 15.0)
                    .radius(4.0)
                    .center()
                    .on_press(move |s: &mut Sash| s.toggle_tab_pin(i))
                    .child(pin_glyph(t.text_secondary.alpha(0.85))),
            )
        } else {
            chip.child(
                el()
                    .key(format!("close-{i}"))
                    .size(15.0, 15.0)
                    .radius(4.0)
                    .center()
                    .on_press(move |s: &mut Sash| s.request_close_tab(i))
                    .child(text("×").font(s.small_style(400.0, 12.0)).color(t.text_secondary)),
            )
        };
        if active && s.cfg.tab_highlight == TabHighlight::Underline {
            chip = chip.child(
                el()
                    .key(format!("active-underline-{i}"))
                    .absolute(9.0, 22.0)
                    .w((widths[i] - 18.0).max(0.0))
                    .h(2.0)
                    .radius(1.0)
                    .fill(accent)
                    .a11y_hidden(),
            );
        }
        bar = bar.child(chip);
    }

    if let Some(progress) = s.tabs.get(s.active).and_then(|tab| tab.progress()) {
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
                .absolute(0.0, if s.cfg.tabs_at_bottom { 0.0 } else { TAB_H - 2.0 })
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

fn tab_color(index: u8) -> Option<Color> {
    TAB_COLORS.get(index as usize).copied().map(|color| srgb(color, 1.0))
}

/// A small pushpin: head, shaft, base.
fn pin_glyph(color: Color) -> El {
    col()
        .key("pin")
        .w(9.0)
        .h(15.0)
        .center()
        .child(el().size(6.0, 6.0).radius(3.0).fill(color))
        .child(el().w(1.5).h(4.5).fill(color))
        .child(el().w(8.0).h(1.5).radius(0.75).fill(color))
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
        .child(setting_row(s, "set-display", "Display", display_control(s)))
        .child(separator("sep-display", hair))
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
        .child(toggle_row(
            s,
            "set-animation",
            "Slide animation",
            s.cfg.animation_ms > 0,
            flip_animation,
        ))
        .child(separator("sep-animation", hair))
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
        .child(setting_row(s, "set-hotkey", "Drop-down shortcut", shortcut_control(s)));

    let terminal_card = col()
        .key("settings-terminal-card")
        .w_full()
        .radius(10.0)
        .fill(Color::WHITE.alpha(0.035))
        .border(1.0, hair)
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
        .child(separator("sep-font-size", hair))
        .child(setting_row(
            s,
            "set-font-family",
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
        ))
        .child(separator("sep-font-family", hair))
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
        .child(separator("sep-shell", hair))
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
        ));

    let tabs_card = col()
        .key("settings-tabs-card")
        .w_full()
        .radius(10.0)
        .fill(Color::WHITE.alpha(0.035))
        .border(1.0, hair)
        .child(toggle_row(
            s,
            "set-tab-position",
            "Tabs at the bottom",
            s.cfg.tabs_at_bottom,
            flip_tabs_at_bottom,
        ))
        .child(separator("sep-tab-position", hair))
        .child(toggle_row(
            s,
            "set-tabs-fill",
            "Fill available width with tabs",
            s.cfg.tabs_fill_width,
            flip_tabs_fill_width,
        ))
        .child(separator("sep-tabs-fill", hair))
        .child(setting_row(
            s,
            "set-tab-highlight",
            "Active tab highlight",
            tab_highlight_control(s),
        ))
        .child(separator("sep-tab-highlight", hair))
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
        .child(
            row()
                .key("tabs-hint")
                .h(52.0)
                .px(14.0)
                .center_y()
                .child(
                    text("Right-click to rename, colour, or pin. A pinned tab keeps the strip visible and ignores Close Tab.")
                        .font(s.small_style(400.0, 11.0))
                        .color(t.text_secondary.alpha(0.65)),
                ),
        );

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

    let mut cards = col().key("settings-cards").w_full().gap(10.0);
    match s.settings.tab {
        SettingsTab::Panel => cards = cards.child(panel_card),
        SettingsTab::Terminal => cards = cards.child(terminal_card),
        SettingsTab::Tabs => cards = cards.child(tabs_card),
        SettingsTab::Keys => cards = cards.child(shortcuts_card(s)),
    }
    cards = cards.child(
        text(note.unwrap_or_default())
            .font(s.small_style(400.0, 11.0))
            .color(note_color),
    );

    let mut tabs = row().key("settings-tabs").w_full().h(26.0).gap(3.0).center_y();
    for tab in SettingsTab::ALL {
        tabs = tabs.child(settings_tab_button(s, tab));
    }

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
                            text(crate::version_label())
                                .font(s.small_style(400.0, 10.0))
                                .color(t.text_secondary.alpha(0.45)),
                        )
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
                .child(tabs)
                .child(
                    col()
                        .key(format!("settings-scroll-{}", s.settings.tab.key()))
                        .w_full()
                        .grow(1.0)
                        .min_h(0.0)
                        .scroll(ScrollSpec { stick_bottom: false })
                        .child(cards),
                ),
        )
}

fn settings_tab_button(s: &Sash, tab: SettingsTab) -> El {
    let t = s.theme();
    let active = s.settings.tab == tab;
    el()
        .key(format!("settings-tab-{}", tab.key()))
        .h(26.0)
        .px(12.0)
        .radius(7.0)
        .center()
        .fill(if active { Color::WHITE.alpha(0.10) } else { Color::TRANSPARENT })
        .on_press(move |s: &mut Sash| s.settings.tab = tab)
        .child(
            text(tab.label())
                .font(s.small_style(if active { 500.0 } else { 400.0 }, 12.0))
                .color(if active { t.text } else { t.text_secondary }),
        )
}


fn shortcuts_card(s: &Sash) -> El {
    let t = s.theme();
    let hair = Color::WHITE.alpha(0.08);
    let left = [
        ("⌘T", "New tab"),
        ("⌘⇧T", "Duplicate tab"),
        ("⌘⇧W", "Close tab"),
        ("⌘W", "Close pane"),
        ("⌘D", "Split right"),
        ("⌘⇧D", "Split down"),
        ("⌘[  ⌘]", "Focus pane"),
        ("⌘⇧[  ⌘⇧]", "Switch tab"),
        ("⌃Tab", "Cycle tabs"),
        ("Drag tab", "Reorder"),
    ];
    let right = [
        ("⌘P", "Quick open"),
        ("⌘F", "Find"),
        ("⌘G", "Next match"),
        ("⌘⇧M", "Maximize panel"),
        ("⌘,", "Settings"),
        ("⌘H", "Hide panel"),
        ("⌘↑  ⌘↓", "Scroll"),
        ("⌘⇧↑  ⌘⇧↓", "Jump prompt"),
        ("Right-click tab", "Rename, colour, pin"),
        ("⌘Q", "Quit"),
    ];
    let column = |rows: &[(&str, &str)]| {
        let mut col = col().grow(1.0).min_w(0.0).gap(2.0);
        for (keys, label) in rows {
            col = col.child(
                row()
                    .w_full()
                    .h(19.0)
                    .gap(8.0)
                    .center_y()
                    .child(
                        el().w(96.0).child(
                            text(*keys)
                                .font(s.small_style(500.0, 10.5))
                                .color(t.text_secondary),
                        ),
                    )
                    .child(
                        text(*label)
                            .font(s.small_style(400.0, 11.0))
                            .color(t.text),
                    ),
            );
        }
        col
    };
    col()
        .key("settings-keys-card")
        .w_full()
        .px(14.0)
        .py(10.0)
        .gap(8.0)
        .radius(10.0)
        .fill(Color::WHITE.alpha(0.035))
        .border(1.0, hair)
        .child(
            text("Keyboard")
                .font(s.small_style(600.0, 12.0))
                .color(t.text),
        )
        .child(
            row()
                .w_full()
                .gap(24.0)
                .child(column(&left))
                .child(column(&right)),
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

fn display_control(s: &Sash) -> El {
    let t = s.theme();
    let displays = crate::macos::displays();
    let label = match s.cfg.display {
        None => "Follow pointer".to_string(),
        Some(n) => displays
            .iter()
            .find(|display| display.index == n)
            .map(|display| format!("{n} · {}", display.name))
            .unwrap_or_else(|| format!("Display {n} (unplugged)")),
    };
    el()
        .key("display-button")
        .role("button")
        .label(format!("Display: {label}"))
        .h(27.0)
        .w(240.0)
        .px(11.0)
        .radius(6.0)
        .center()
        .focusable(true)
        .fill(Color::WHITE.alpha(0.07))
        .border(1.0, Color::WHITE.alpha(0.12))
        .on_press_tx(|s: &mut Sash, tx| s.cycle_display(tx))
        .child(
            text(label)
                .font(s.small_style(500.0, 12.0))
                .color(t.text)
                .w_full()
                .truncate(),
        )
}

fn tab_highlight_control(s: &Sash) -> El {
    let t = s.theme();
    let label = format!("{}  ›", s.cfg.tab_highlight.label());
    el()
        .key("tab-highlight-button")
        .role("button")
        .label(format!("Active tab highlight: {}", s.cfg.tab_highlight.label()))
        .h(27.0)
        .w(160.0)
        .px(11.0)
        .radius(6.0)
        .center()
        .focusable(true)
        .fill(Color::WHITE.alpha(0.07))
        .border(1.0, Color::WHITE.alpha(0.12))
        .on_press(cycle_tab_highlight)
        .child(text(label).font(s.small_style(500.0, 12.0)).color(t.text))
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

fn flip_tabs_at_bottom(s: &mut Sash) {
    s.cfg.tabs_at_bottom = !s.cfg.tabs_at_bottom;
    s.cfg.save();
}

fn flip_tabs_fill_width(s: &mut Sash) {
    s.cfg.tabs_fill_width = !s.cfg.tabs_fill_width;
    s.cfg.save();
}

fn cycle_tab_highlight(s: &mut Sash) {
    s.cfg.tab_highlight = s.cfg.tab_highlight.next();
    s.cfg.save();
}

fn tab_menu_backdrop() -> El {
    el()
        .key("tab-menu-backdrop")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .fill(Color::BLACK.alpha(0.01))
        .on_press(|s: &mut Sash| s.tab_menu = None)
        .on_secondary_press(|s: &mut Sash, _p, _tx| s.tab_menu = None)
}

fn tab_menu_card(s: &Sash, menu: TabMenu) -> El {
    let t = s.theme();
    let width = 210.0;
    let tab = s.tabs.get(menu.tab);
    let named = tab.is_some_and(|tab| tab.display_name.is_some());
    let pinned = tab.is_some_and(|tab| tab.pinned);
    let current = tab.and_then(|tab| tab.color);
    let items = if named { 7 } else { 6 };
    let height = items as f32 * 27.0 + 62.0;
    let x = menu.x.min(s.width - width - 8.0).max(8.0);
    let y = menu.y.min(s.height - height - 8.0).max(8.0);
    let mut card = col()
        .key("tab-menu")
        .absolute(x, y)
        .w(width)
        .py(6.0)
        .px(4.0)
        .gap(1.0)
        .radius(9.0)
        .fill(srgb(Rgb::new(0x16, 0x1b, 0x25), 0.99))
        .border(1.0, Color::WHITE.alpha(0.14));
    let item = |key: &str, label: &str| {
        el()
            .key(key)
            .w_full()
            .h(26.0)
            .px(9.0)
            .center_y()
            .radius(5.0)
            .child(text(label).font(s.small_style(400.0, 12.0)).color(t.text))
    };
    let muted = |key: &str, label: &str| {
        el()
            .key(key)
            .w_full()
            .h(26.0)
            .px(9.0)
            .center_y()
            .radius(5.0)
            .child(
                text(label)
                    .font(s.small_style(400.0, 12.0))
                    .color(t.text_secondary.alpha(0.45)),
            )
    };
    card = card.child(
        item("tab-rename", "Rename Tab…")
            .on_press(move |s: &mut Sash| s.begin_rename(menu.tab)),
    );
    if named {
        card = card.child(
            item("tab-auto-name", "Use Automatic Title").on_press(move |s: &mut Sash| {
                s.tab_menu = None;
                s.clear_tab_name(menu.tab);
            }),
        );
    }
    card = card
        .child(item("tab-duplicate", "Duplicate Tab").on_press(move |s: &mut Sash| {
            s.tab_menu = None;
            s.duplicate_tab();
        }))
        .child(item("tab-move-left", "Move Left").on_press(move |s: &mut Sash| {
            s.tab_menu = None;
            s.move_tab(menu.tab, menu.tab.saturating_sub(1));
        }))
        .child(item("tab-move-right", "Move Right").on_press(move |s: &mut Sash| {
            s.tab_menu = None;
            s.move_tab(menu.tab, menu.tab + 1);
        }))
        .child(
            item(
                "tab-pin",
                if pinned { "Unpin Tab" } else { "Pin Tab" },
            )
            .on_press(move |s: &mut Sash| s.toggle_tab_pin(menu.tab)),
        );
    card = card.child(if pinned {
        muted("tab-close", "Close Tab (pinned)")
    } else {
        item("tab-close", "Close Tab").on_press(move |s: &mut Sash| {
            s.tab_menu = None;
            s.request_close_tab(menu.tab);
        })
    });
    card = card
        .child(separator("tab-color-separator", Color::WHITE.alpha(0.08)))
        .child(
            row()
                .key("tab-color-label")
                .w_full()
                .h(17.0)
                .px(9.0)
                .center_y()
                .child(
                    text("Colour")
                        .font(s.small_style(500.0, 10.5))
                        .color(t.text_secondary),
                ),
        );
    let mut swatches = row().key("tab-colors").w_full().h(22.0).px(9.0).gap(4.0).center_y();
    swatches = swatches.child(
        el()
            .key("tab-color-none")
            .size(16.0, 16.0)
            .radius(8.0)
            .fill(Color::TRANSPARENT)
            .border(
                if current.is_none() { 2.0 } else { 1.0 },
                if current.is_none() { Color::WHITE } else { Color::WHITE.alpha(0.28) },
            )
            .on_press(move |s: &mut Sash| s.set_tab_color(menu.tab, None)),
    );
    for (index, color) in TAB_COLORS.iter().enumerate() {
        let index = index as u8;
        let selected = current == Some(index);
        swatches = swatches.child(
            el()
                .key(format!("tab-color-{index}"))
                .size(16.0, 16.0)
                .radius(8.0)
                .fill(srgb(*color, if selected { 1.0 } else { 0.78 }))
                .border(
                    if selected { 2.0 } else { 1.0 },
                    if selected { Color::WHITE } else { Color::WHITE.alpha(0.20) },
                )
                .on_press(move |s: &mut Sash| s.set_tab_color(menu.tab, Some(index))),
        );
    }
    card.child(swatches)
}

fn rename_overlay(s: &Sash) -> El {
    let t = s.theme();
    let Some(rename) = &s.rename else {
        return el();
    };
    el()
        .key("rename-overlay")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .center()
        .fill(Color::BLACK.alpha(0.28))
        .child(
            col()
                .key("rename-card")
                .w(360.0)
                .px(16.0)
                .py(14.0)
                .gap(12.0)
                .radius(10.0)
                .fill(srgb(DEFAULT_BG, 0.98))
                .border(1.0, Color::WHITE.alpha(0.12))
                .child(
                    text("Rename Tab")
                        .font(s.small_style(500.0, 13.0))
                        .color(t.text),
                )
                .child(
                    text_input(&rename.edit, s.small_style(400.0, 12.0))
                        .key("rename-input")
                        .single_line()
                        .label("Tab name")
                        .w_full()
                        .h(28.0)
                        .px(8.0)
                        .radius(6.0)
                        .fill(Color::WHITE.alpha(0.07))
                        .on_input_key(|s: &mut Sash, ev: &KeyEvent, _tx: &mut Tx| match ev.key {
                            Key::Enter => {
                                s.commit_rename();
                                true
                            }
                            Key::Escape => {
                                s.cancel_rename();
                                true
                            }
                            _ => false,
                        })
                        .on_edit(|s: &mut Sash| &mut s.rename.as_mut().expect("rename open").edit),
                )
                .child(
                    row()
                        .w_full()
                        .gap(8.0)
                        .center_y()
                        .child(spacer())
                        .child(
                            el()
                                .key("rename-cancel")
                                .h(26.0)
                                .px(10.0)
                                .radius(6.0)
                                .center()
                                .fill(Color::WHITE.alpha(0.07))
                                .on_press(|s: &mut Sash| s.cancel_rename())
                                .child(
                                    text("Cancel")
                                        .font(s.small_style(400.0, 12.0))
                                        .color(t.text),
                                ),
                        )
                        .child(
                            el()
                                .key("rename-save")
                                .h(26.0)
                                .px(10.0)
                                .radius(6.0)
                                .center()
                                .fill(srgb(ACCENT, 1.0))
                                .on_press(|s: &mut Sash| s.commit_rename())
                                .child(
                                    text("Rename")
                                        .font(s.small_style(500.0, 12.0))
                                        .color(srgb(DEFAULT_BG, 1.0)),
                                ),
                        ),
                ),
        )
}

fn palette_backdrop() -> El {
    el()
        .key("palette-backdrop")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .fill(Color::BLACK.alpha(0.24))
        .on_press(|s: &mut Sash| s.close_palette())
}

fn palette_card(s: &Sash) -> El {
    let t = s.theme();
    let items = s.palette_items();
    let matches = s.palette_matches();
    let width = (s.width - 80.0).clamp(240.0, 560.0);
    let x = ((s.width - width) / 2.0).max(12.0);
    let y = (s.height * 0.12).max(18.0);
    let rows = matches.len().min(9);
    let mut card = col()
        .key("palette")
        .absolute(x, y)
        .w(width)
        .px(10.0)
        .py(10.0)
        .gap(6.0)
        .radius(12.0)
        .fill(srgb(Rgb::new(0x16, 0x1b, 0x25), 0.99))
        .border(1.0, Color::WHITE.alpha(0.16))
        .child(
            text_input(&s.palette.edit, s.small_style(400.0, 13.0))
                .key("palette-input")
                .single_line()
                .label("Search tabs and commands")
                .w_full()
                .h(32.0)
                .px(9.0)
                .radius(7.0)
                .fill(Color::WHITE.alpha(0.07))
                .on_input_change(|s: &mut Sash, tx: &mut Tx| {
                    s.palette.selected = 0;
                    tx.instant();
                })
                .on_input_key(|s: &mut Sash, ev: &KeyEvent, tx: &mut Tx| match ev.key {
                    Key::Enter => {
                        s.run_palette(tx);
                        true
                    }
                    Key::Escape => {
                        s.close_palette();
                        true
                    }
                    Key::Up => {
                        s.move_palette(-1);
                        tx.instant();
                        true
                    }
                    Key::Down => {
                        s.move_palette(1);
                        tx.instant();
                        true
                    }
                    _ => false,
                })
                .on_edit(|s: &mut Sash| &mut s.palette.edit),
        );
    if matches.is_empty() {
        card = card.child(
            text("No matches")
                .font(s.small_style(400.0, 11.0))
                .color(t.text_secondary)
                .py(8.0),
        );
    }
    let first = s.palette.selected.saturating_sub(rows.saturating_sub(1));
    for (slot, index) in matches.into_iter().skip(first).take(rows).enumerate() {
        let Some(item) = items.get(index) else { continue };
        let selected = first + slot == s.palette.selected;
        let action = item.action.clone();
        card = card.child(
            row()
                .key(format!("palette-row-{slot}"))
                .w_full()
                .h(30.0)
                .px(9.0)
                .gap(8.0)
                .center_y()
                .radius(6.0)
                .fill(if selected { srgb(ACCENT, 0.22) } else { Color::TRANSPARENT })
                .on_press_tx(move |s: &mut Sash, tx: &mut Tx| {
                    s.run_palette_action(action.clone(), tx)
                })
                .child(
                    text(item.label.clone())
                        .font(s.small_style(if selected { 500.0 } else { 400.0 }, 12.0))
                        .color(t.text),
                )
                .child(spacer())
                .child(
                    text(tail(&item.detail, 52))
                        .font(s.small_style(400.0, 10.0))
                        .color(t.text_secondary),
                ),
        );
    }
    card
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
    let closing_pane = matches!(s.pending_close, Some(CloseTarget::Pane { .. }));
    let title = if quitting {
        "Quit sash?"
    } else if closing_pane {
        "Close this pane?"
    } else {
        "Close this tab?"
    };
    let detail = if quitting {
        match busy.len() {
            0 => "A command is still running.".to_string(),
            1 => format!("{} is still running a command.", busy[0]),
            n => format!("{n} tabs are still running a command."),
        }
    } else {
        let name = match s.pending_close {
            Some(CloseTarget::Tab(index)) => s
                .tabs
                .get(index)
                .map(|tab| tab.title())
                .unwrap_or_else(|| "This tab".into()),
            Some(CloseTarget::Pane { tab, pane }) => s
                .tabs
                .get(tab)
                .and_then(|tab| tab.panes.get(pane))
                .map(|pane| pane.title())
                .unwrap_or_else(|| "This pane".into()),
            None => "This tab".into(),
        };
        format!("{name} is still running a command.")
    };
    let confirm = if quitting {
        "Quit"
    } else if closing_pane {
        "Close Pane"
    } else {
        "Close Tab"
    };
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

/// A first-run card: the app opens with no window, so name the shortcut and
/// the menu-bar menu instead of leaving the launch unexplained.
fn welcome_overlay(s: &Sash) -> El {
    let t = s.theme();
    let accent = srgb(ACCENT, 1.0);
    let hotkey = pretty_hotkey(&s.cfg.hotkey);
    let line = |label: String| {
        text(label)
            .font(s.small_style(400.0, 11.0))
            .color(t.text_secondary)
    };
    el()
        .key("welcome-overlay")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .center()
        .fill(Color::BLACK.alpha(0.45))
        .child(
            col()
                .key("welcome-card")
                .w(400.0)
                .px(18.0)
                .py(16.0)
                .gap(9.0)
                .radius(10.0)
                .fill(srgb(DEFAULT_BG, 0.98))
                .border(1.0, Color::WHITE.alpha(0.12))
                .child(
                    text("Welcome to Sash")
                        .font(s.small_style(600.0, 14.0))
                        .color(t.text),
                )
                .child(line("Sash lives in the menu bar; there is no Dock icon.".into()))
                .child(line(format!(
                    "Press {hotkey} to drop it down, and again to hide it."
                )))
                .child(line("⌘, opens Settings, and the ghost menu can quit.".into()))
                .child(line("Right-click a tab to name, colour, or pin it.".into()))
                .child(
                    row()
                        .w_full()
                        .gap(8.0)
                        .center_y()
                        .child(spacer())
                        .child(
                            el()
                                .key("welcome-done")
                                .h(26.0)
                                .px(12.0)
                                .radius(6.0)
                                .center()
                                .fill(accent)
                                .on_press(|s: &mut Sash| s.dismiss_welcome())
                                .child(
                                    text("Got it")
                                        .font(s.small_style(500.0, 12.0))
                                        .color(srgb(DEFAULT_BG, 1.0)),
                                ),
                        ),
                ),
        )
}

/// The find bar floats over the top-right of the grid, where a terminal's
/// scrollback search usually lives.
fn find_bar(s: &Sash, m: &Metrics, cols: u16, ox: f32, oy: f32) -> El {
    let t = s.theme();
    let width = (cols as f32 * m.cw - 16.0).clamp(180.0, 380.0).min((cols as f32 * m.cw).max(120.0));
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
        .absolute(ox + x, oy + 4.0)
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

/// A single character that the cell grid, not the font, must draw.
fn special_cell(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let ch = chars.next()?;
    (chars.next().is_none() && crate::cell_glyphs::is_cell_glyph(ch)).then_some(ch)
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
    let mut root = el().key("term").w_full().grow(1.0);
    let Some(tab) = s.active_tab() else {
        return root.child(text("no tab").font(s.run_style(400.0)).color(t.text_secondary));
    };
    let split = tab.panes.len() > 1;
    for (index, pane) in tab.panes.iter().enumerate() {
        root = root.child(pane_terminal(s, pane, index, index == tab.focus, split));
    }
    if s.pending_paste.is_some() {
        root = root.child(paste_overlay(s));
    }
    root
}

/// One pane's grid, overlays and pointer handling, placed at its slice of the
/// terminal area. Everything inside draws in pane-local cells; libghostty owns
/// the grid itself.
fn pane_terminal(s: &Sash, pane: &Pane, index: usize, focused: bool, split: bool) -> El {
    let rect = pane.rect;
    let m = pane.metrics;
    // Rune anchors absolute children to the nearest absolute ancestor, not to a
    // padded wrapper, so every cell position carries the pane's padding itself.
    let ox = PAD_X;
    let oy = PAD_Y;
    let border = if focused && split {
        srgb(ACCENT, 0.34)
    } else if split {
        Color::WHITE.alpha(0.08)
    } else {
        Color::TRANSPARENT
    };
    let outer = el()
        .key(format!("term-pane-{index}"))
        .absolute(rect.x, rect.y)
        .w(rect.w)
        .h(rect.h)
        .border(1.0, border)
        .on_wheel(move |s: &mut Sash, ev, _tx| {
            if s.pending_paste.is_some() {
                return false;
            }
            let rows = s.wheel.rows(ev.dy, m.ch);
            if rows == 0 {
                return true;
            }
            if s.active_tab().is_some_and(|tab| tab.focus != index) {
                s.focus_pane(index);
            }
            let tracked = !s.mods.shift
                && s.active_pane().is_some_and(|pane| pane.vt.mouse_tracking());
            if tracked {
                s.wheel_mouse(ev.x, ev.y, rows > 0, rows.abs());
            } else {
                s.scroll_by(rows);
            }
            // Claim even sub-row travel, so the accumulator keeps it.
            true
        })
        .pointer_buttons([PointerButton::Middle, PointerButton::Back, PointerButton::Forward])
        .on_pointer_move_local(move |s: &mut Sash, p, _tx| {
            if s.mouse_down || s.pending_paste.is_some() || s.mods.shift {
                return;
            }
            let Some(tab) = s.active_tab() else { return };
            if tab.focus != index {
                return;
            }
            if tab.panes.get(index).is_some_and(|pane| pane.vt.mouse_any_event()) {
                s.send_mouse(ffi::GHOSTTY_MOUSE_ACTION_MOTION, None, p.x, p.y);
            }
        })
        .on_secondary_press_local(move |s: &mut Sash, p, _tx| {
            if s.pending_paste.is_some() || s.mods.shift {
                return;
            }
            if s.active_tab().is_some_and(|tab| tab.focus != index) {
                s.focus_pane(index);
            }
            if s.active_pane().is_some_and(|pane| pane.vt.mouse_tracking()) {
                s.send_mouse(ffi::GHOSTTY_MOUSE_ACTION_PRESS, Some(ffi::GHOSTTY_MOUSE_BUTTON_RIGHT), p.x, p.y);
                s.send_mouse(ffi::GHOSTTY_MOUSE_ACTION_RELEASE, Some(ffi::GHOSTTY_MOUSE_BUTTON_RIGHT), p.x, p.y);
            }
        })
        .on_drag(move |s: &mut Sash, d: &mut Drag, tx| {
            if s.pending_paste.is_some() {
                return;
            }
            if s.active_tab().is_some_and(|tab| tab.focus != index) {
                s.focus_pane(index);
            }
            let Some(at) = s.cell_at(d.x, d.y) else {
                return;
            };
            let tracking = !s.mods.shift
                && s.active_pane().is_some_and(|pane| pane.vt.mouse_tracking());
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
                        2 => s.active_pane().map(|pane| word_at(pane.vt.grid(), at)),
                        _ => s.active_pane().map(|pane| line_at(pane.vt.grid(), at)),
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
    let mut area = el()
        .key(format!("grid-{index}"))
        .absolute(0.0, 0.0)
        .w(rect.w)
        .h(rect.h);

    let grid = pane.vt.grid();

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
                        .key(format!("bg-{index}-{y}-{start}"))
                        .transition(instant)
                        .absolute(ox + start as f32 * m.cw, oy + y as f32 * m.ch)
                        .size((x - start) as f32 * m.cw, m.ch)
                        .fill(srgb(bg, 1.0)),
                );
            }
        }
    }

    // Selection highlight, one quad per selected run, under the cursor and glyphs.
    // The range is resolved once per frame: a word selection must not re-scan its
    // word bounds for every cell of the grid.
    if focused
        && let Some(selection) = s.selection
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
                        .key(format!("sel-{index}-{y}-{start}"))
                        .transition(instant)
                        .absolute(ox + start as f32 * m.cw, oy + y as f32 * m.ch)
                        .size((x - start) as f32 * m.cw, m.ch)
                        .fill(srgb(ACCENT, 0.28)),
                );
            }
        }
    }

    // The selected find match, over the selection tint but under the text.
    for (y, x0, x1) in pane.vt.search_highlight() {
        area = area.child(
            el()
                .key(format!("find-{index}-{y}-{x0}"))
                .transition(instant)
                .absolute(ox + x0 as f32 * m.cw, oy + y as f32 * m.ch)
                .size((x1 - x0 + 1) as f32 * m.cw, m.ch)
                .fill(srgb(Rgb::new(0xf9, 0xd8, 0x8a), 0.38)),
        );
    }

    // Cursor under the text so glyphs stay readable. Only the focused pane shows
    // one, which is how a split says where the keyboard goes.
    if focused && grid.cursor_visible {
        if let Some((cx, cy)) = grid.cursor {
            let (w, h, x, y) = match grid.cursor_style.unwrap_or(CursorStyle::Block) {
                CursorStyle::Bar => (2.0, m.ch, ox + cx as f32 * m.cw, oy + cy as f32 * m.ch),
                CursorStyle::Underline => (m.cw, 2.0, ox + cx as f32 * m.cw, oy + cy as f32 * m.ch + m.ch - 2.0),
                CursorStyle::Block | CursorStyle::Hollow => (m.cw, m.ch, ox + cx as f32 * m.cw, oy + cy as f32 * m.ch),
            };
            area = area.child(
                el()
                    .key(format!("cursor-{index}"))
                    .transition(instant)
                    .absolute(x, y)
                    .size(w, h)
                    .fill(srgb(ACCENT, 0.55)),
            );
            // Tell the input method where the caret is, so candidates and the
            // emoji picker open beside the text they will replace.
            area = area.child(
                el()
                    .key(format!("ime-caret-{index}"))
                    .absolute(x, y)
                    .size(1.0, m.ch)
                    .ime_cursor(),
            );
        }
    }

    // Text runs: consecutive cells sharing fg + attributes, plus adjacent blank cells
    // skipped so a run never starts mid-gap. Block and box characters break a
    // run: the font's em box is shorter than the cell, so drawing them as text
    // leaves seams through block art and between the rows of a border.
    for y in 0..grid.rows {
        let mut x = 0u16;
        while x < grid.cols {
            let Some(cell) = grid.cell(x, y) else { break };
            if cell.text.is_empty() {
                x += 1;
                continue;
            }
            if let Some(ch) = special_cell(&cell.text) {
                let alpha = if cell.flags & FLAG_FAINT != 0 { 0.6 } else { 1.0 };
                if let Some(rects) = crate::cell_glyphs::rects(ch, m.cw, m.ch) {
                    for (i, rect) in rects.iter().enumerate() {
                        area = area.child(
                            el()
                                .key(format!("glyph-{index}-{y}-{x}-{i}"))
                                .transition(instant)
                                .absolute(
                                    ox + x as f32 * m.cw + rect.x,
                                    oy + y as f32 * m.ch + rect.y,
                                )
                                .size(rect.w, rect.h)
                                .fill(srgb(cell.fg, alpha * rect.alpha)),
                        );
                    }
                }
                x += 1;
                continue;
            }
            let fg = cell.fg;
            let attr = cell.flags & (FLAG_BOLD | FLAG_FAINT);
            let start = x;
            let mut run = String::new();
            while x < grid.cols {
                let Some(c) = grid.cell(x, y) else { break };
                if c.text.is_empty()
                    || c.fg != fg
                    || (c.flags & (FLAG_BOLD | FLAG_FAINT)) != attr
                    || special_cell(&c.text).is_some()
                {
                    break;
                }
                run.push_str(&c.text);
                x += 1;
            }
            if run.is_empty() {
                continue;
            }
            let weight = if attr & FLAG_BOLD != 0 { 700.0 } else { 400.0 };
            let alpha = if attr & FLAG_FAINT != 0 { 0.6 } else { 1.0 };
            area = area.child(
                text(run)
                    .key(format!("tx-{index}-{y}-{start}"))
                    .transition(instant)
                    .font(s.run_style(weight))
                    .color(srgb(fg, alpha))
                    .absolute(ox + start as f32 * m.cw, oy + y as f32 * m.ch)
                    .h(m.ch),
            );
        }
    }

    // Scrollback position: a slim thumb on the right edge, absent at a live prompt.
    if let Some(bar) = pane.vt.scrollbar()
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
                .key(format!("scrollbar-{index}"))
                .transition(instant)
                .absolute(ox + grid.cols as f32 * m.cw - 4.0, oy + y)
                .size(3.0, thumb)
                .radius(1.5)
                .fill(Color::WHITE.alpha(0.18)),
        );
    }

    if focused
        && let Some(preedit) = &s.preedit
        && let Some((cx, cy)) = grid.cursor
    {
        let x = (cx as f32 + 1.0).min(grid.cols.saturating_sub(1) as f32);
        let width = (preedit.chars().count() as f32 * m.cw).max(m.cw);
        area = area.child(
            el()
                .key(format!("preedit-bg-{index}"))
                .absolute(ox + x * m.cw, oy + cy as f32 * m.ch)
                .size(width, m.ch)
                .radius(2.0)
                .fill(srgb(ACCENT, 0.24)),
        );
        area = area.child(
            text(preedit.clone())
                .key(format!("preedit-{index}"))
                .font(s.run_style(400.0))
                .color(srgb(DEFAULT_FG, 1.0))
                .absolute(ox + x * m.cw, oy + cy as f32 * m.ch)
                .h(m.ch),
        );
    }

    if focused && s.find.open {
        area = area.child(find_bar(s, &m, grid.cols, ox, oy));
    }

    outer.child(area)
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
    fn directory_only_titles_become_folder_names() {
        assert_eq!(directory_title("~").as_deref(), Some("Home"));
        assert_eq!(directory_title("~/").as_deref(), Some("Home"));
        assert_eq!(directory_title("~/mine/sash/").as_deref(), Some("sash"));
        assert_eq!(directory_title("/tmp/a-project/").as_deref(), Some("a-project"));
        assert_eq!(directory_title("/").as_deref(), Some("/"));
        assert_eq!(directory_title("opencode"), None);
        assert_eq!(directory_title("sash"), None);
    }

    #[test]
    fn natural_tabs_have_a_minimum_and_fill_tabs_share_the_free_width() {
        assert_eq!(calculate_tab_widths(1_000.0, 80.0, &[60.0, 140.0], false), [120.0, 140.0]);
        let filled = calculate_tab_widths(1_000.0, 80.0, &[60.0, 140.0], true);
        assert_eq!(filled, [417.5, 417.5]);
        let reserved = PAD_X * 2.0 + TAB_CONTROLS_WIDTH + 80.0 + 5.0 * TAB_GAP;
        assert_eq!(filled.iter().sum::<f32>() + reserved, 1_000.0);
        assert_eq!(calculate_tab_widths(100.0, 80.0, &[60.0], true), [TAB_MIN_FILL_WIDTH]);
        assert!(calculate_tab_widths(1_000.0, 80.0, &[], true).is_empty());
    }

    #[test]
    fn a_pinned_single_tab_keeps_the_strip_visible() {
        assert!(!tab_strip_visible(1, false));
        assert!(tab_strip_visible(1, true));
        assert!(tab_strip_visible(2, false));
        assert!(!tab_strip_visible(0, false));
    }

    #[test]
    fn the_display_cycle_wraps_through_follow_pointer() {
        assert_eq!(next_display(None, 2), Some(1));
        assert_eq!(next_display(Some(1), 2), Some(2));
        assert_eq!(next_display(Some(2), 2), None);
        assert_eq!(next_display(None, 0), None);
        assert_eq!(next_display(Some(3), 2), None);
    }

    #[test]
    fn full_height_uses_the_space_above_the_dock() {
        assert_eq!(panel_height(None, 932.0), 932.0);
        assert_eq!(panel_height(Some(420.0), 932.0), 420.0);
        assert_eq!(panel_height(Some(5000.0), 932.0), 932.0);
        assert_eq!(panel_height(Some(10.0), 200.0), 120.0);
    }

    #[test]
    fn palette_targets_existing_files_and_their_lines() {
        let path = std::env::temp_dir().join(format!("sash-palette-{}.txt", std::process::id()));
        std::fs::write(&path, "x").unwrap();
        let text = path.to_string_lossy().into_owned();
        assert_eq!(palette_target(&text), Some((path.clone(), None)));
        assert_eq!(palette_target(&format!("{text}:42")), Some((path.clone(), Some(42))));
        assert_eq!(
            palette_target(&format!("{text}:42:7")),
            Some((path.clone(), Some(42)))
        );
        assert_eq!(palette_target("/definitely/not/a/sash/file:1"), None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn palette_terms_match_labels_details_and_hidden_text() {
        let item = PaletteItem {
            label: "New Tab".into(),
            detail: "⌘T".into(),
            search: "Cargo.toml".into(),
            action: PaletteAction::NewTab,
        };
        assert!(palette_matches(&item, "new tab"));
        assert!(palette_matches(&item, "cargo"));
        assert!(!palette_matches(&item, "settings"));
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
