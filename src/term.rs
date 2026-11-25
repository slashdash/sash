// SPDX-FileCopyrightText: 2026 Sash contributors
// SPDX-License-Identifier: MIT

//! Terminal core: libghostty-vt terminal state, pty plumbing, key encoding.

pub mod cwd;
pub mod keys;
pub mod pty;

use std::ffi::c_void;
use std::mem::size_of;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::ffi;

pub const FLAG_BOLD: u16 = 1 << 0;
pub const FLAG_ITALIC: u16 = 1 << 1;
pub const FLAG_FAINT: u16 = 1 << 2;
pub const FLAG_BLINK: u16 = 1 << 3;
pub const FLAG_INVERSE: u16 = 1 << 4;
pub const FLAG_INVISIBLE: u16 = 1 << 5;
pub const FLAG_STRIKETHROUGH: u16 = 1 << 6;
pub const FLAG_OVERLINE: u16 = 1 << 7;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

impl From<ffi::GhosttyColorRgb> for Rgb {
    fn from(c: ffi::GhosttyColorRgb) -> Self {
        Self {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    }
}

impl From<Rgb> for ffi::GhosttyColorRgb {
    fn from(c: Rgb) -> Self {
        Self {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Cell {
    pub text: String,
    pub fg: Rgb,
    /// `None` means "use the default background", which lets a renderer skip the cell.
    pub bg: Option<Rgb>,
    pub flags: u16,
    pub selected: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorStyle {
    Bar,
    Block,
    Underline,
    Hollow,
}

#[derive(Clone, Debug, Default)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Cell>,
    pub default_fg: Rgb,
    pub default_bg: Rgb,
    pub palette: Vec<Rgb>,
    pub cursor: Option<(u16, u16)>,
    pub cursor_visible: bool,
    pub cursor_style: Option<CursorStyle>,
}

impl Grid {
    pub fn cell(&self, x: u16, y: u16) -> Option<&Cell> {
        if x >= self.cols || y >= self.rows {
            return None;
        }
        self.cells.get(y as usize * self.cols as usize + x as usize)
    }
}

/// What a program says its work is doing (OSC 9;4), for the tab chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub state: ProgressState,
    /// `None` when the program omitted a percentage (indeterminate).
    pub percent: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressState {
    Determinate,
    Error,
    Indeterminate,
    Paused,
}

/// The end of a command reported through OSC 133.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandEnd {
    pub command: String,
    pub error: String,
    pub exit: Option<i32>,
}

/// A notification a program asked the desktop to show (OSC 9 / OSC 777).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub body: String,
}

/// The terminal's scroll viewport over its scrollback, in rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scrollbar {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
}

struct Host {
    /// Bytes libghostty-vt wants written back to the pty (device attribute replies,
    /// cursor-position reports, clipboard replies).
    replies: Vec<u8>,
    title: String,
    bell: bool,
    pwd: Option<PathBuf>,
    notifications: Vec<Notification>,
    progress: Option<Progress>,
    command_line: String,
    command_end: Option<CommandEnd>,
}

unsafe extern "C" fn on_write_pty(
    _terminal: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    if userdata.is_null() || data.is_null() {
        return;
    }
    let host = unsafe { &mut *(userdata as *mut Host) };
    host.replies
        .extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
}

unsafe extern "C" fn on_title_changed(terminal: ffi::GhosttyTerminal, userdata: *mut c_void) {
    if userdata.is_null() {
        return;
    }
    let mut title = ffi::GhosttyString::default();
    let result = unsafe {
        ffi::ghostty_terminal_get(
            terminal,
            ffi::GHOSTTY_TERMINAL_DATA_TITLE,
            &mut title as *mut _ as *mut c_void,
        )
    };
    if result != ffi::GHOSTTY_SUCCESS || title.ptr.is_null() {
        return;
    }
    let bytes = unsafe { std::slice::from_raw_parts(title.ptr, title.len) };
    let host = unsafe { &mut *(userdata as *mut Host) };
    host.title = String::from_utf8_lossy(bytes).into_owned();
}

unsafe extern "C" fn on_bell(_terminal: ffi::GhosttyTerminal, userdata: *mut c_void) {
    if userdata.is_null() {
        return;
    }
    let host = unsafe { &mut *(userdata as *mut Host) };
    host.bell = true;
}

unsafe extern "C" fn on_pwd_changed(terminal: ffi::GhosttyTerminal, userdata: *mut c_void) {
    if userdata.is_null() {
        return;
    }
    let mut raw = ffi::GhosttyString::default();
    let result = unsafe {
        ffi::ghostty_terminal_get(
            terminal,
            ffi::GHOSTTY_TERMINAL_DATA_PWD,
            &mut raw as *mut _ as *mut c_void,
        )
    };
    if result != ffi::GHOSTTY_SUCCESS {
        return;
    }
    let text = unsafe { string_from(raw) };
    let host = unsafe { &mut *(userdata as *mut Host) };
    host.pwd = decode_pwd(&text);
}

unsafe extern "C" fn on_desktop_notification(
    _terminal: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    notification: *const ffi::GhosttyTerminalDesktopNotification,
) {
    if userdata.is_null() || notification.is_null() {
        return;
    }
    let n = unsafe { &*notification };
    let host = unsafe { &mut *(userdata as *mut Host) };
    host.notifications.push(Notification {
        title: unsafe { string_from(n.title) },
        body: unsafe { string_from(n.body) },
    });
}

unsafe extern "C" fn on_progress(
    _terminal: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    report: *const ffi::GhosttyTerminalProgressReport,
) {
    if userdata.is_null() || report.is_null() {
        return;
    }
    let r = unsafe { &*report };
    let host = unsafe { &mut *(userdata as *mut Host) };
    host.progress = match r.state {
        ffi::GHOSTTY_TERMINAL_PROGRESS_STATE_REMOVE => None,
        ffi::GHOSTTY_TERMINAL_PROGRESS_STATE_SET => Some(Progress {
            state: ProgressState::Determinate,
            percent: percent_of(r.progress),
        }),
        ffi::GHOSTTY_TERMINAL_PROGRESS_STATE_ERROR => Some(Progress {
            state: ProgressState::Error,
            percent: percent_of(r.progress),
        }),
        ffi::GHOSTTY_TERMINAL_PROGRESS_STATE_INDETERMINATE => Some(Progress {
            state: ProgressState::Indeterminate,
            percent: None,
        }),
        ffi::GHOSTTY_TERMINAL_PROGRESS_STATE_PAUSE => Some(Progress {
            state: ProgressState::Paused,
            percent: percent_of(r.progress),
        }),
        _ => host.progress,
    };
}

/// Whether a program may replace the clipboard. Off by default: a program that can
/// write the clipboard can also plant a payload for the next paste.
static CLIPBOARD_WRITE: AtomicBool = AtomicBool::new(false);

/// A larger write is refused instead of copied.
const CLIPBOARD_MAX_BYTES: usize = 1024 * 1024;

pub fn set_clipboard_write(allow: bool) {
    CLIPBOARD_WRITE.store(allow, Ordering::Relaxed);
}

/// Whether a payload is small enough to write. Measured on the decoded text, which
/// the caller already holds, so the check never copies the data again.
fn clipboard_fits(text: &str) -> bool {
    text.len() <= CLIPBOARD_MAX_BYTES
}

/// OSC 52 / iTerm2 / Kitty clipboard writes. The reply is mandatory: without it the
/// program is told the write was refused.
unsafe extern "C" fn on_clipboard_write(
    _terminal: ffi::GhosttyTerminal,
    _userdata: *mut c_void,
    write: *const ffi::GhosttyClipboardWrite,
) {
    if write.is_null() {
        return;
    }
    let write = unsafe { &*write };
    let result = if write.location != ffi::GHOSTTY_CLIPBOARD_LOCATION_STANDARD {
        ffi::GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED
    } else if !CLIPBOARD_WRITE.load(Ordering::Relaxed) {
        // sash never sets `remember` on a reply, so `write.granted` can only come
        // from a grant it never stores; this policy stays the only authority.
        ffi::GHOSTTY_CLIPBOARD_WRITE_RESULT_DENIED
    } else {
        match unsafe { clipboard_text(write) } {
            Some(Some(text)) if !clipboard_fits(&text) => {
                ffi::GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA
            }
            Some(Some(text)) => {
                let Ok(mut clipboard) = arboard::Clipboard::new() else {
                    return reply_to(write, ffi::GHOSTTY_CLIPBOARD_WRITE_RESULT_IO_ERROR);
                };
                if clipboard.set_text(sanitize_clipboard(&text)).is_ok() {
                    ffi::GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS
                } else {
                    ffi::GHOSTTY_CLIPBOARD_WRITE_RESULT_IO_ERROR
                }
            }
            // An empty representation list means "clear the clipboard".
            Some(None) => {
                let Ok(mut clipboard) = arboard::Clipboard::new() else {
                    return reply_to(write, ffi::GHOSTTY_CLIPBOARD_WRITE_RESULT_IO_ERROR);
                };
                if clipboard.clear().is_ok() {
                    ffi::GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS
                } else {
                    ffi::GHOSTTY_CLIPBOARD_WRITE_RESULT_IO_ERROR
                }
            }
            None => ffi::GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA,
        }
    };
    reply_to(write, result);
}

fn reply_to(write: &ffi::GhosttyClipboardWrite, result: ffi::GhosttyClipboardWriteResult) {
    if let Some(reply) = write.reply {
        let reply_data = ffi::GhosttyClipboardWriteReply {
            size: std::mem::size_of::<ffi::GhosttyClipboardWriteReply>(),
            result,
            remember: false,
        };
        unsafe { reply(write, &reply_data) };
    }
}

/// `Some(Some(text))` for a text representation, `Some(None)` for an explicit clear,
/// and `None` when the request has no text representation.
unsafe fn clipboard_text(write: &ffi::GhosttyClipboardWrite) -> Option<Option<String>> {
    if write.contents_len == 0 || write.contents.is_null() {
        return Some(None);
    }
    let contents = unsafe { std::slice::from_raw_parts(write.contents, write.contents_len) };
    for content in contents {
        let mime = unsafe { bytes_from(content.mime) };
        if mime == b"text/plain" || mime.starts_with(b"text/plain;") {
            let data = unsafe { bytes_from(content.data) };
            return Some(Some(String::from_utf8_lossy(data).into_owned()));
        }
    }
    None
}

/// Strip C0 controls, keeping tab, LF and CR: a clipboard carrying ESC/CSI is the
/// payload for an injection when a later paste feeds it back to a terminal.
fn sanitize_clipboard(text: &str) -> String {
    text.chars()
        .filter(|&c| !matches!(c, '\u{0}'..='\u{1f}') || matches!(c, '\t' | '\n' | '\r'))
        .collect()
}

unsafe fn bytes_from(value: ffi::GhosttyString) -> &'static [u8] {
    if value.ptr.is_null() || value.len == 0 {
        return &[];
    }
    unsafe { std::slice::from_raw_parts(value.ptr, value.len) }
}

unsafe fn string_from(value: ffi::GhosttyString) -> String {
    String::from_utf8_lossy(unsafe { bytes_from(value) }).into_owned()
}

fn percent_of(percent: i8) -> Option<u8> {
    (0..=100).contains(&percent).then_some(percent as u8)
}

/// Decode the raw bytes of OSC 7 / OSC 9;9 / OSC 1337 into a path.
fn decode_pwd(raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let path = if let Some(rest) = raw.strip_prefix("file://") {
        let slash = rest.find('/')?;
        &rest[slash..]
    } else if raw.starts_with('/') {
        raw
    } else {
        return None;
    };
    Some(PathBuf::from(percent_decode(path)))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(high * 16 + low);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// A live terminal: libghostty-vt state plus the handles needed to snapshot it.
pub struct Vt {
    term: ffi::GhosttyTerminal,
    render: ffi::GhosttyRenderState,
    rows_iter: ffi::GhosttyRenderStateRowIterator,
    cells: ffi::GhosttyRenderStateRowCells,
    key_encoder: ffi::GhosttyKeyEncoder,
    key_event: ffi::GhosttyKeyEvent,
    /// The key event's text. libghostty-vt borrows it instead of copying, so it must
    /// outlive the event and must not move; every encode reuses this allocation.
    key_text: Vec<u8>,
    mouse_encoder: ffi::GhosttyMouseEncoder,
    mouse_event: ffi::GhosttyMouseEvent,
    /// Created on the first find, then reused for every needle.
    search: ffi::GhosttySearch,
    host: Box<Host>,
    grid: Grid,
    need_full: bool,
}

impl Vt {
    pub fn new(cols: u16, rows: u16, cell_w: u32, cell_h: u32) -> Self {
        let mut host = Box::new(Host {
            replies: Vec::new(),
            title: String::new(),
            bell: false,
            pwd: None,
            notifications: Vec::new(),
            progress: None,
            command_line: String::new(),
            command_end: None,
        });
        let userdata = host.as_mut() as *mut Host as *mut c_void;

        unsafe {
            let mut term: ffi::GhosttyTerminal = std::ptr::null_mut();
            check(
                ffi::ghostty_terminal_new(std::ptr::null(), &mut term, cols, rows),
                "terminal_new",
            );

            ffi::ghostty_terminal_set(
                term,
                ffi::GHOSTTY_TERMINAL_OPT_USERDATA,
                userdata as *const c_void,
            );
            ffi::ghostty_terminal_set(
                term,
                ffi::GHOSTTY_TERMINAL_OPT_WRITE_PTY,
                on_write_pty as *const () as *const c_void,
            );
            ffi::ghostty_terminal_set(
                term,
                ffi::GHOSTTY_TERMINAL_OPT_TITLE_CHANGED,
                on_title_changed as *const () as *const c_void,
            );
            ffi::ghostty_terminal_set(
                term,
                ffi::GHOSTTY_TERMINAL_OPT_BELL,
                on_bell as *const () as *const c_void,
            );
            ffi::ghostty_terminal_set(
                term,
                ffi::GHOSTTY_TERMINAL_OPT_PWD_CHANGED,
                on_pwd_changed as *const () as *const c_void,
            );
            ffi::ghostty_terminal_set(
                term,
                ffi::GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION,
                on_desktop_notification as *const () as *const c_void,
            );
            ffi::ghostty_terminal_set(
                term,
                ffi::GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT,
                on_progress as *const () as *const c_void,
            );
            ffi::ghostty_terminal_set(
                term,
                ffi::GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE,
                on_clipboard_write as *const () as *const c_void,
            );
            ffi::ghostty_terminal_set(
                term,
                ffi::GHOSTTY_TERMINAL_OPT_SEMANTIC_PROMPT,
                on_semantic_prompt as *const () as *const c_void,
            );

            let mut render: ffi::GhosttyRenderState = std::ptr::null_mut();
            check(
                ffi::ghostty_render_state_new(std::ptr::null(), &mut render),
                "render_state_new",
            );
            let mut rows_iter: ffi::GhosttyRenderStateRowIterator = std::ptr::null_mut();
            check(
                ffi::ghostty_render_state_row_iterator_new(std::ptr::null(), &mut rows_iter),
                "row_iterator_new",
            );
            let mut cells: ffi::GhosttyRenderStateRowCells = std::ptr::null_mut();
            check(
                ffi::ghostty_render_state_row_cells_new(std::ptr::null(), &mut cells),
                "row_cells_new",
            );
            let mut key_encoder: ffi::GhosttyKeyEncoder = std::ptr::null_mut();
            check(
                ffi::ghostty_key_encoder_new(std::ptr::null(), &mut key_encoder),
                "key_encoder_new",
            );
            let mut key_event: ffi::GhosttyKeyEvent = std::ptr::null_mut();
            check(
                ffi::ghostty_key_event_new(std::ptr::null(), &mut key_event),
                "key_event_new",
            );
            let mut mouse_encoder: ffi::GhosttyMouseEncoder = std::ptr::null_mut();
            check(
                ffi::ghostty_mouse_encoder_new(std::ptr::null(), &mut mouse_encoder),
                "mouse_encoder_new",
            );
            let mut mouse_event: ffi::GhosttyMouseEvent = std::ptr::null_mut();
            check(
                ffi::ghostty_mouse_event_new(std::ptr::null(), &mut mouse_event),
                "mouse_event_new",
            );

            let mut vt = Self {
                term,
                render,
                rows_iter,
                cells,
                key_encoder,
                key_event,
                key_text: Vec::new(),
                mouse_encoder,
                mouse_event,
                search: std::ptr::null_mut(),
                host,
                need_full: true,
                grid: Grid {
                    cols,
                    rows,
                    cells: vec![Cell::default(); cols as usize * rows as usize],
                    ..Default::default()
                },
            };
            vt.set_colors(DEFAULT_FG, DEFAULT_BG, &default_palette());
            vt.resize(cols, rows, cell_w, cell_h);
            // The first `refresh` snapshots the grid: the app resizes every tab after
            // its provisional metrics settle, and a snapshot here is one of the two
            // full snapshots per restored tab that costs startup time.
            vt
        }
    }

    fn set_colors(&mut self, fg: Rgb, bg: Rgb, palette: &[Rgb; 256]) {
        let fg: ffi::GhosttyColorRgb = fg.into();
        let bg: ffi::GhosttyColorRgb = bg.into();
        let palette: [ffi::GhosttyColorRgb; 256] = std::array::from_fn(|i| palette[i].into());
        unsafe {
            ffi::ghostty_terminal_set(
                self.term,
                ffi::GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND,
                &fg as *const _ as *const c_void,
            );
            ffi::ghostty_terminal_set(
                self.term,
                ffi::GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND,
                &bg as *const _ as *const c_void,
            );
            ffi::ghostty_terminal_set(
                self.term,
                ffi::GHOSTTY_TERMINAL_OPT_COLOR_PALETTE,
                palette.as_ptr() as *const c_void,
            );
            let cursor: ffi::GhosttyColorRgb = ACCENT.into();
            ffi::ghostty_terminal_set(
                self.term,
                ffi::GHOSTTY_TERMINAL_OPT_COLOR_CURSOR,
                &cursor as *const _ as *const c_void,
            );
        }
    }

    /// Feed bytes from the pty (or anywhere else) into the terminal.
    pub fn write(&mut self, bytes: &[u8]) {
        unsafe {
            ffi::ghostty_terminal_vt_write(self.term, bytes.as_ptr(), bytes.len());
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell_w: u32, cell_h: u32) {
        unsafe {
            let _ = ffi::ghostty_terminal_resize(self.term, cols, rows, cell_w, cell_h);
            // The mouse encoder turns surface points into cells, so it needs the same
            // geometry the terminal was just given.
            let size = ffi::GhosttyMouseEncoderSize {
                size: size_of::<ffi::GhosttyMouseEncoderSize>(),
                screen_width: cell_w.saturating_mul(cols as u32),
                screen_height: cell_h.saturating_mul(rows as u32),
                cell_width: cell_w.max(1),
                cell_height: cell_h.max(1),
                ..Default::default()
            };
            let _ = ffi::ghostty_mouse_encoder_set(
                self.mouse_encoder,
                ffi::GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                &size as *const _ as *const c_void,
            );
        }
        if self.grid.cols != cols || self.grid.rows != rows {
            self.grid.cols = cols;
            self.grid.rows = rows;
            self.grid.cells = vec![Cell::default(); cols as usize * rows as usize];
        }
        // The render state's dirty rows were consumed by the last refresh, and the
        // grid buffer may have just been reallocated empty, so rebuild all of it.
        self.need_full = true;
    }

    /// Pull pending pty replies (DA1, DSR, ...) out of the terminal.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.host.replies)
    }

    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.host.bell)
    }

    /// The shell's directory as last reported through an escape sequence.
    pub fn pwd(&self) -> Option<&PathBuf> {
        self.host.pwd.as_ref()
    }

    pub fn take_notifications(&mut self) -> Vec<Notification> {
        std::mem::take(&mut self.host.notifications)
    }

    /// A finished command reported by shell integration, if any.
    pub fn take_command_end(&mut self) -> Option<CommandEnd> {
        self.host.command_end.take()
    }

    /// The OSC 8 hyperlink attached to a viewport cell, if the program set one.
    pub fn hyperlink_at(&self, x: u16, y: u16) -> Option<String> {
        unsafe {
            let point = ffi::GhosttyPoint {
                tag: ffi::GHOSTTY_POINT_TAG_VIEWPORT,
                value: ffi::GhosttyPointValue {
                    coordinate: ffi::GhosttyPointCoordinate { x, y: y as u32 },
                },
            };
            let mut reference = ffi::GhosttyGridRef::default();
            if ffi::ghostty_terminal_grid_ref(self.term, point, &mut reference)
                != ffi::GHOSTTY_SUCCESS
            {
                return None;
            }
            let mut len = 0usize;
            if ffi::ghostty_grid_ref_hyperlink_uri(&reference, std::ptr::null_mut(), 0, &mut len)
                != ffi::GHOSTTY_OUT_OF_SPACE
                || len == 0
            {
                return None;
            }
            let mut buf = vec![0u8; len];
            if ffi::ghostty_grid_ref_hyperlink_uri(&reference, buf.as_mut_ptr(), len, &mut len)
                != ffi::GHOSTTY_SUCCESS
            {
                return None;
            }
            buf.truncate(len);
            String::from_utf8(buf).ok()
        }
    }

    /// Jump backward or forward by whole shell prompts. A no-op without OSC 133.
    pub fn scroll_prompts(&mut self, delta: isize) {
        let viewport = ffi::GhosttyTerminalScrollViewport {
            tag: ffi::GHOSTTY_TERMINAL_SCROLL_VIEWPORT_DELTA_PROMPT,
            value: ffi::GhosttyTerminalScrollViewportValue {
                delta_prompt: delta,
            },
        };
        unsafe {
            let _ = ffi::ghostty_terminal_scroll_viewport(self.term, viewport);
        }
        self.refresh(true);
    }

    /// The latest OSC 9;4 progress report; `None` once the program removed it.
    pub fn progress(&self) -> Option<Progress> {
        self.host.progress
    }

    /// Keep `lines` rows of scrollback, or no line limit at all. The byte budget is always
    /// lifted from libghostty-vt's 10 KB default, which is barely a page.
    pub fn set_scrollback(&mut self, lines: Option<u32>) {
        unsafe {
            let bytes = usize::MAX;
            ffi::ghostty_terminal_set(
                self.term,
                ffi::GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
                &bytes as *const _ as *const c_void,
            );
            let lines = lines.map(|n| n as usize).unwrap_or(usize::MAX);
            ffi::ghostty_terminal_set(
                self.term,
                ffi::GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES,
                &lines as *const _ as *const c_void,
            );
        }
    }

    /// Start (or restart) a scrollback search. Matches are ASCII
    /// case-insensitive, as libghostty-vt defines them.
    pub fn search_start(&mut self, needle: &str) -> bool {
        unsafe {
            if self.search.is_null()
                && ffi::ghostty_search_new(std::ptr::null(), &mut self.search, self.term)
                    != ffi::GHOSTTY_SUCCESS
            {
                return false;
            }
            let string = ffi::GhosttyString {
                ptr: needle.as_ptr(),
                len: needle.len(),
            };
            ffi::ghostty_search_set(
                self.search,
                ffi::GHOSTTY_SEARCH_OPT_NEEDLE,
                &string as *const _ as *const c_void,
            ) == ffi::GHOSTTY_SUCCESS
        }
    }

    /// Advance an incremental search by a bounded amount. Returns true once it
    /// has caught up with the scrollback; false while there is more to do.
    pub fn search_poll(&mut self) -> bool {
        if self.search.is_null() {
            return true;
        }
        unsafe {
            for _ in 0..64 {
                let mut status: ffi::GhosttySearchStatus = ffi::GHOSTTY_SEARCH_STATUS_RUNNING;
                if ffi::ghostty_search_tick(self.search, &mut status) != ffi::GHOSTTY_SUCCESS {
                    return true;
                }
                match status {
                    ffi::GHOSTTY_SEARCH_STATUS_COMPLETE => return true,
                    ffi::GHOSTTY_SEARCH_STATUS_FEED_REQUIRED => {
                        if ffi::ghostty_search_feed(self.search) != ffi::GHOSTTY_SUCCESS {
                            return true;
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            false
        }
    }

    /// (total matches, selected index from newest).
    pub fn search_counts(&self) -> (usize, Option<usize>) {
        if self.search.is_null() {
            return (0, None);
        }
        unsafe {
            let mut total = 0usize;
            let mut index = 0usize;
            let total_ok = ffi::ghostty_search_get(
                self.search,
                ffi::GHOSTTY_SEARCH_DATA_TOTAL_MATCHES,
                &mut total as *mut _ as *mut c_void,
            ) == ffi::GHOSTTY_SUCCESS;
            let index_ok = ffi::ghostty_search_get(
                self.search,
                ffi::GHOSTTY_SEARCH_DATA_SELECTED_INDEX,
                &mut index as *mut _ as *mut c_void,
            ) == ffi::GHOSTTY_SUCCESS;
            (if total_ok { total } else { 0 }, index_ok.then_some(index))
        }
    }

    /// Select the next (older) or previous (newer) match and scroll it in.
    pub fn search_select(&mut self, forward: bool) -> bool {
        if self.search.is_null() {
            return false;
        }
        let result = unsafe {
            if forward {
                ffi::ghostty_search_select_next(self.search)
            } else {
                ffi::ghostty_search_select_prev(self.search)
            }
        };
        self.refresh(true);
        result == ffi::GHOSTTY_SUCCESS
    }

    /// Drop results and stop highlighting.
    pub fn search_clear(&mut self) {
        if self.search.is_null() {
            return;
        }
        unsafe {
            let _ = ffi::ghostty_search_set(
                self.search,
                ffi::GHOSTTY_SEARCH_OPT_NEEDLE,
                std::ptr::null(),
            );
        }
        self.refresh(true);
    }

    /// The selected match as row spans clipped to the viewport, for highlights.
    pub fn search_highlight(&self) -> Vec<(u16, u16, u16)> {
        if self.search.is_null() {
            return Vec::new();
        }
        unsafe {
            let mut selection = ffi::GhosttySelection::default();
            if ffi::ghostty_search_get(
                self.search,
                ffi::GHOSTTY_SEARCH_DATA_SELECTED_MATCH,
                &mut selection as *mut _ as *mut c_void,
            ) != ffi::GHOSTTY_SUCCESS
            {
                return Vec::new();
            }
            let mut start = ffi::GhosttyPointCoordinate::default();
            let mut end = ffi::GhosttyPointCoordinate::default();
            if ffi::ghostty_terminal_point_from_grid_ref(
                self.term,
                &selection.start,
                ffi::GHOSTTY_POINT_TAG_VIEWPORT,
                &mut start,
            ) != ffi::GHOSTTY_SUCCESS
                || ffi::ghostty_terminal_point_from_grid_ref(
                    self.term,
                    &selection.end,
                    ffi::GHOSTTY_POINT_TAG_VIEWPORT,
                    &mut end,
                ) != ffi::GHOSTTY_SUCCESS
            {
                return Vec::new();
            }
            let (first, last) = if (start.y, start.x) <= (end.y, end.x) {
                (start, end)
            } else {
                (end, start)
            };
            let cols = self.grid.cols;
            let rows = self.grid.rows;
            let mut spans = Vec::new();
            for y in first.y..=last.y {
                if y >= rows as u32 {
                    break;
                }
                let x0 = if y == first.y { first.x } else { 0 };
                let x1 = if y == last.y {
                    last.x
                } else {
                    cols.saturating_sub(1)
                };
                if x0 < cols && x0 <= x1 {
                    spans.push((y as u16, x0, x1));
                }
            }
            spans
        }
    }

    /// The viewport's position over the scrollback, for drawing a scrollbar.
    pub fn scrollbar(&self) -> Option<Scrollbar> {
        let mut bar = ffi::GhosttyTerminalScrollbar::default();
        let result = unsafe {
            ffi::ghostty_terminal_get(
                self.term,
                ffi::GHOSTTY_TERMINAL_DATA_SCROLLBAR,
                &mut bar as *mut _ as *mut c_void,
            )
        };
        (result == ffi::GHOSTTY_SUCCESS).then_some(Scrollbar {
            total: bar.total,
            offset: bar.offset,
            len: bar.len,
        })
    }

    /// Whether the viewport tracks the live terminal rather than scrolled-back history.
    pub fn viewport_active(&self) -> bool {
        let mut active = false;
        let result = unsafe {
            ffi::ghostty_terminal_get(
                self.term,
                ffi::GHOSTTY_TERMINAL_DATA_VIEWPORT_ACTIVE,
                &mut active as *mut _ as *mut c_void,
            )
        };
        result == ffi::GHOSTTY_SUCCESS && active
    }

    pub fn title(&self) -> &str {
        &self.host.title
    }

    /// Snapshot the grid. Only rows libghostty-vt reports dirty are rebuilt, so calling
    /// this every frame stays cheap.
    pub fn refresh(&mut self, force_full: bool) -> bool {
        unsafe {
            // Per-frame path: a failed update must not take the whole app down.
            if ffi::ghostty_render_state_update(self.render, self.term) != ffi::GHOSTTY_SUCCESS {
                return false;
            }

            let mut dirty: ffi::GhosttyRenderStateDirty = 0;
            let r = ffi::ghostty_render_state_get(
                self.render,
                ffi::GHOSTTY_RENDER_STATE_DATA_DIRTY,
                &mut dirty as *mut _ as *mut c_void,
            );
            if r != ffi::GHOSTTY_SUCCESS {
                return false;
            }
            let full =
                force_full || self.need_full || dirty == ffi::GHOSTTY_RENDER_STATE_DIRTY_FULL;
            if dirty == ffi::GHOSTTY_RENDER_STATE_DIRTY_FALSE && !full {
                return false;
            }
            self.need_full = false;
            let expected = self.grid.cols as usize * self.grid.rows as usize;
            if self.grid.cells.len() != expected {
                self.grid.cells = vec![Cell::default(); expected];
            }
            if full {
                for cell in &mut self.grid.cells {
                    cell.text.clear();
                    cell.bg = None;
                    cell.flags = 0;
                    cell.selected = false;
                }
            }

            let mut colors = ffi::GhosttyRenderStateColors {
                size: std::mem::size_of::<ffi::GhosttyRenderStateColors>(),
                ..Default::default()
            };
            if ffi::ghostty_render_state_get(
                self.render,
                ffi::GHOSTTY_RENDER_STATE_DATA_COLORS,
                &mut colors as *mut _ as *mut c_void,
            ) == ffi::GHOSTTY_SUCCESS
            {
                self.grid.default_fg = colors.foreground.into();
                self.grid.default_bg = colors.background.into();
                self.grid.palette = colors.palette.iter().map(|c| Rgb::from(*c)).collect();
            }
            let default_fg = self.grid.default_fg;
            let default_bg = self.grid.default_bg;

            let r = ffi::ghostty_render_state_get(
                self.render,
                ffi::GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                &mut self.rows_iter as *mut _ as *mut c_void,
            );
            if r != ffi::GHOSTTY_SUCCESS {
                return true;
            }

            let cols = self.grid.cols as usize;
            let rows = self.grid.rows as usize;
            if full {
                // Not every row has to be dirty (a resize with the same grid keeps the
                // screen), so walk all captured rows instead of the dirty ones.
                let mut y = 0u16;
                while ffi::ghostty_render_state_row_iterator_next(self.rows_iter) {
                    if (y as usize) < rows {
                        self.read_row_cells(y, cols, default_fg, default_bg);
                    }
                    y += 1;
                }
            } else {
                let mut y: u16 = 0;
                while ffi::ghostty_render_state_row_iterator_next_dirty(self.rows_iter, &mut y) {
                    if (y as usize) < rows {
                        self.read_row_cells(y, cols, default_fg, default_bg);
                    }
                }
            }

            let mut cursor = ffi::GhosttyRenderStateCursor {
                size: std::mem::size_of::<ffi::GhosttyRenderStateCursor>(),
                ..Default::default()
            };
            if ffi::ghostty_render_state_get(
                self.render,
                ffi::GHOSTTY_RENDER_STATE_DATA_CURSOR,
                &mut cursor as *mut _ as *mut c_void,
            ) == ffi::GHOSTTY_SUCCESS
            {
                self.grid.cursor_visible = cursor.visible && cursor.viewport_has_value;
                self.grid.cursor = if cursor.viewport_has_value {
                    Some((cursor.viewport_x, cursor.viewport_y))
                } else {
                    None
                };
                self.grid.cursor_style = Some(match cursor.visual_style {
                    ffi::GHOSTTY_CURSOR_STYLE_BAR => CursorStyle::Bar,
                    ffi::GHOSTTY_CURSOR_STYLE_BLOCK => CursorStyle::Block,
                    ffi::GHOSTTY_CURSOR_STYLE_UNDERLINE => CursorStyle::Underline,
                    ffi::GHOSTTY_CURSOR_STYLE_BLOCK_HOLLOW => CursorStyle::Hollow,
                    _ => CursorStyle::Block,
                });
            }

            let _ = ffi::ghostty_render_state_clean(self.render);
        }
        true
    }

    /// Whether the running program asked for bracketed paste (mode 2004).
    pub fn bracketed_paste(&self) -> bool {
        self.mode_enabled(2004)
    }

    fn mode_enabled(&self, number: u16) -> bool {
        let mut config = ffi::GhosttyTerminalModeConfig {
            mode: mode(number, false),
            value: false,
        };
        let result = unsafe {
            ffi::ghostty_terminal_get(
                self.term,
                ffi::GHOSTTY_TERMINAL_DATA_MODE,
                &mut config as *mut _ as *mut c_void,
            )
        };
        result == ffi::GHOSTTY_SUCCESS && config.value
    }

    /// Whether the program asked to receive the pointer (X10, button or any-event
    /// tracking). Programs that turn this on get clicks and drags instead of a
    /// selection; Shift still selects locally.
    pub fn mouse_tracking(&self) -> bool {
        self.mode_enabled(1000) || self.mode_enabled(1002) || self.mode_enabled(1003)
    }

    /// Mode 1003: report motion even with no button down.
    pub fn mouse_any_event(&self) -> bool {
        self.mode_enabled(1003)
    }

    /// Encode one pointer event the way the running program asked for it. The
    /// terminal's mouse modes and format are read from libghostty each time a
    /// button changes or the wheel turns, so an app that flips modes mid-session
    /// is followed.
    pub fn encode_mouse(
        &mut self,
        action: ffi::GhosttyMouseAction,
        button: Option<ffi::GhosttyMouseButton>,
        x: f32,
        y: f32,
        mods: ffi::GhosttyMods,
    ) -> Vec<u8> {
        unsafe {
            if action != ffi::GHOSTTY_MOUSE_ACTION_MOTION {
                let _ = ffi::ghostty_mouse_encoder_set_from_terminal(self.mouse_encoder, self.term);
            }
            let _ = ffi::ghostty_mouse_event_set_action(self.mouse_event, action);
            match button {
                Some(button) => {
                    let _ = ffi::ghostty_mouse_event_set_button(self.mouse_event, button);
                }
                None => {
                    let _ = ffi::ghostty_mouse_event_clear_button(self.mouse_event);
                }
            }
            let _ = ffi::ghostty_mouse_event_set_mods(self.mouse_event, mods);
            let _ = ffi::ghostty_mouse_event_set_position(
                self.mouse_event,
                ffi::GhosttyMousePosition { x, y },
            );

            let mut buf = [0u8; 128];
            let mut written = 0usize;
            let mut result = ffi::ghostty_mouse_encoder_encode(
                self.mouse_encoder,
                self.mouse_event,
                buf.as_mut_ptr(),
                buf.len(),
                &mut written,
            );
            if result == ffi::GHOSTTY_OUT_OF_SPACE {
                let mut bigger = vec![0u8; written];
                written = 0;
                result = ffi::ghostty_mouse_encoder_encode(
                    self.mouse_encoder,
                    self.mouse_event,
                    bigger.as_mut_ptr(),
                    bigger.len(),
                    &mut written,
                );
                if result == ffi::GHOSTTY_SUCCESS {
                    bigger.truncate(written);
                    return bigger;
                }
                return Vec::new();
            }
            if result != ffi::GHOSTTY_SUCCESS {
                return Vec::new();
            }
            buf[..written].to_vec()
        }
    }

    unsafe fn read_row_cells(&mut self, y: u16, cols: usize, default_fg: Rgb, default_bg: Rgb) {
        unsafe {
            let r = ffi::ghostty_render_state_row_get(
                self.rows_iter,
                ffi::GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                &mut self.cells as *mut _ as *mut c_void,
            );
            if r != ffi::GHOSTTY_SUCCESS {
                return;
            }
            let mut x = 0usize;
            while ffi::ghostty_render_state_row_cells_next(self.cells) {
                if x >= cols {
                    break;
                }
                let cell = &mut self.grid.cells[y as usize * cols + x];

                let mut has_styling = false;
                ffi::ghostty_render_state_row_cells_get(
                    self.cells,
                    ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_HAS_STYLING,
                    &mut has_styling as *mut _ as *mut c_void,
                );
                let mut flags = 0;
                if has_styling {
                    let mut style = ffi::GhosttyStyle::default();
                    if ffi::ghostty_render_state_row_cells_get(
                        self.cells,
                        ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
                        &mut style as *mut _ as *mut c_void,
                    ) == ffi::GHOSTTY_SUCCESS
                    {
                        flags = style.flags;
                    }
                }

                let mut fg = ffi::GhosttyColorRgb::default();
                let fg_ok = ffi::ghostty_render_state_row_cells_get(
                    self.cells,
                    ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR,
                    &mut fg as *mut _ as *mut c_void,
                ) == ffi::GHOSTTY_SUCCESS;
                let mut bg = ffi::GhosttyColorRgb::default();
                let bg_ok = ffi::ghostty_render_state_row_cells_get(
                    self.cells,
                    ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR,
                    &mut bg as *mut _ as *mut c_void,
                ) == ffi::GHOSTTY_SUCCESS;

                let mut fg = if fg_ok { Rgb::from(fg) } else { default_fg };
                let mut bg = if bg_ok { Some(Rgb::from(bg)) } else { None };
                if flags & FLAG_INVERSE != 0 {
                    let fg2 = bg.unwrap_or(default_bg);
                    let bg2 = Some(fg);
                    fg = fg2;
                    bg = bg2;
                }

                let mut buf = [0u8; 64];
                let mut bytes = ffi::GhosttyBuffer {
                    ptr: buf.as_mut_ptr(),
                    cap: buf.len(),
                    len: 0,
                };
                // Reuse the cell's buffer: this runs for every visible cell on every
                // full snapshot, and a fresh String per cell was pure garbage.
                cell.text.clear();
                let r = ffi::ghostty_render_state_row_cells_get(
                    self.cells,
                    ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_UTF8,
                    &mut bytes as *mut _ as *mut c_void,
                );
                if (r == ffi::GHOSTTY_SUCCESS || r == ffi::GHOSTTY_OUT_OF_SPACE) && bytes.len > 0 {
                    if r == ffi::GHOSTTY_OUT_OF_SPACE || bytes.len > buf.len() {
                        let mut big = vec![0u8; bytes.len];
                        let mut bytes = ffi::GhosttyBuffer {
                            ptr: big.as_mut_ptr(),
                            cap: big.len(),
                            len: 0,
                        };
                        if ffi::ghostty_render_state_row_cells_get(
                            self.cells,
                            ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_UTF8,
                            &mut bytes as *mut _ as *mut c_void,
                        ) == ffi::GHOSTTY_SUCCESS
                        {
                            cell.text
                                .push_str(&String::from_utf8_lossy(&big[..bytes.len]));
                        }
                    } else {
                        cell.text
                            .push_str(&String::from_utf8_lossy(&buf[..bytes.len]));
                    }
                }

                if flags & FLAG_INVISIBLE != 0 {
                    cell.text.clear();
                }
                cell.fg = fg;
                cell.bg = bg;
                cell.flags = flags;
                cell.selected = false;
                x += 1;
            }
        }
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    /// Scroll by `delta` rows: negative moves up into history, positive toward the
    /// live screen (libghostty-vt's sign).
    pub fn scroll(&mut self, delta: i32) {
        let viewport = ffi::GhosttyTerminalScrollViewport {
            tag: ffi::GHOSTTY_TERMINAL_SCROLL_VIEWPORT_DELTA,
            value: ffi::GhosttyTerminalScrollViewportValue {
                delta: delta as isize,
            },
        };
        unsafe {
            let _ = ffi::ghostty_terminal_scroll_viewport(self.term, viewport);
        }
        self.refresh(true);
    }

    pub fn scroll_to_top(&mut self) {
        let viewport = ffi::GhosttyTerminalScrollViewport {
            tag: ffi::GHOSTTY_TERMINAL_SCROLL_VIEWPORT_TOP,
            value: ffi::GhosttyTerminalScrollViewportValue { _padding: [0, 0] },
        };
        unsafe {
            let _ = ffi::ghostty_terminal_scroll_viewport(self.term, viewport);
        }
        self.refresh(true);
    }

    pub fn scroll_to_bottom(&mut self) {
        // Called on every keypress, click and wheel step: when the viewport already
        // tracks the live terminal there is nothing to scroll and nothing to snapshot.
        if self.viewport_active() {
            return;
        }
        let viewport = ffi::GhosttyTerminalScrollViewport {
            tag: ffi::GHOSTTY_TERMINAL_SCROLL_VIEWPORT_BOTTOM,
            value: ffi::GhosttyTerminalScrollViewportValue { _padding: [0, 0] },
        };
        unsafe {
            let _ = ffi::ghostty_terminal_scroll_viewport(self.term, viewport);
        }
        self.refresh(true);
    }

    pub fn encode_key(&mut self, ev: &rune::KeyEvent) -> Vec<u8> {
        unsafe {
            let _ = ffi::ghostty_key_encoder_set_from_terminal(self.key_encoder, self.term);
            let one: u8 = 1;
            let _ = ffi::ghostty_key_encoder_set(
                self.key_encoder,
                ffi::GHOSTTY_KEY_ENCODER_OPT_MACOS_OPTION_AS_ALT,
                &one as *const _ as *const c_void,
            );
        }
        keys::encode(self, ev)
    }

    /// The encoder, its event, and the buffer the event's text points into.
    pub(crate) fn raw(&mut self) -> (ffi::GhosttyKeyEncoder, ffi::GhosttyKeyEvent, &mut Vec<u8>) {
        (self.key_encoder, self.key_event, &mut self.key_text)
    }
}

impl Drop for Vt {
    fn drop(&mut self) {
        unsafe {
            ffi::ghostty_render_state_row_cells_free(self.cells);
            ffi::ghostty_render_state_row_iterator_free(self.rows_iter);
            ffi::ghostty_render_state_free(self.render);
            ffi::ghostty_key_event_free(self.key_event);
            ffi::ghostty_key_encoder_free(self.key_encoder);
            ffi::ghostty_mouse_event_free(self.mouse_event);
            ffi::ghostty_mouse_encoder_free(self.mouse_encoder);
            if !self.search.is_null() {
                ffi::ghostty_search_free(self.search);
            }
            ffi::ghostty_terminal_free(self.term);
        }
    }
}

pub const ACCENT: Rgb = Rgb::new(0x7a, 0xa2, 0xf7);
pub const ERROR: Rgb = Rgb::new(0xf3, 0x8b, 0xa8);
pub const DEFAULT_FG: Rgb = Rgb::new(0xcd, 0xd6, 0xf4);
pub const DEFAULT_BG: Rgb = Rgb::new(0x0b, 0x0e, 0x14);

/// Pack a mode number the way `ghostty_mode_new` does: bit 15 marks ANSI modes.
pub const fn mode(value: u16, ansi: bool) -> ffi::GhosttyMode {
    value | if ansi { 1 << 15 } else { 0 }
}

fn check(result: ffi::GhosttyResult, what: &str) {
    assert_eq!(
        result,
        ffi::GHOSTTY_SUCCESS,
        "libghostty-vt {what} failed: {result}"
    );
}

/// OSC 133 shell integration: remember the command line at OUTPUT_START and
/// report the exit at COMMAND_END, so the host can notify only when a command
/// actually finishes.
unsafe extern "C" fn on_semantic_prompt(
    _terminal: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    event: *const ffi::GhosttyTerminalSemanticPrompt,
) {
    if userdata.is_null() || event.is_null() {
        return;
    }
    unsafe {
        let host = &mut *(userdata as *mut Host);
        let event = &*event;
        let text = |s: ffi::GhosttyString| -> String {
            if s.ptr.is_null() || s.len == 0 {
                String::new()
            } else {
                String::from_utf8_lossy(std::slice::from_raw_parts(s.ptr, s.len)).into_owned()
            }
        };
        match event.kind {
            ffi::GHOSTTY_SEMANTIC_PROMPT_PROMPT_START => host.command_line.clear(),
            ffi::GHOSTTY_SEMANTIC_PROMPT_OUTPUT_START => host.command_line = text(event.command),
            ffi::GHOSTTY_SEMANTIC_PROMPT_COMMAND_END => {
                host.command_end = Some(CommandEnd {
                    command: std::mem::take(&mut host.command_line),
                    error: text(event.error),
                    exit: event.has_exit_code.then_some(event.exit_code),
                });
            }
            _ => {}
        }
    }
}

fn default_palette() -> [Rgb; 256] {
    const ANSI: [(u8, u8, u8); 16] = [
        (0x45, 0x47, 0x5a),
        (0xf7, 0x76, 0x8e),
        (0xa6, 0xe3, 0xa1),
        (0xf9, 0xe2, 0xaf),
        (0x7a, 0xa2, 0xf7),
        (0xbb, 0x9a, 0xf7),
        (0x89, 0xdd, 0xeb),
        (0xac, 0xb0, 0xc0),
        (0x58, 0x5b, 0x70),
        (0xf7, 0x76, 0x8e),
        (0xa6, 0xe3, 0xa1),
        (0xf9, 0xe2, 0xaf),
        (0x7a, 0xa2, 0xf7),
        (0xbb, 0x9a, 0xf7),
        (0x89, 0xdd, 0xeb),
        (0xcd, 0xd6, 0xf4),
    ];
    let mut palette = [Rgb::default(); 256];
    for (i, (r, g, b)) in ANSI.iter().enumerate() {
        palette[i] = Rgb::new(*r, *g, *b);
    }
    let steps = [0u8, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
    for i in 0..216 {
        let r = steps[i / 36];
        let g = steps[(i / 6) % 6];
        let b = steps[i % 6];
        palette[16 + i] = Rgb::new(r, g, b);
    }
    for i in 0..24 {
        let v = 0x08 + (i as u8) * 0x0a;
        palette[232 + i] = Rgb::new(v, v, v);
    }
    palette
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use super::*;

    #[test]
    fn osc7_paths_are_decoded() {
        assert_eq!(
            decode_pwd("file://host/Users/me/My%20Code"),
            Some(PathBuf::from("/Users/me/My Code"))
        );
        assert_eq!(decode_pwd("/tmp/plain"), Some(PathBuf::from("/tmp/plain")));
        assert_eq!(decode_pwd("/tmp/a%2Fb"), Some(PathBuf::from("/tmp/a/b")));
        assert_eq!(decode_pwd("file://host"), None);
        assert_eq!(decode_pwd(""), None);
        assert_eq!(decode_pwd("relative/path"), None);
    }

    #[test]
    fn clipboard_writes_prefer_text_plain() {
        let mime = ffi::GhosttyString {
            ptr: b"text/plain;charset=utf-8".as_ptr(),
            len: 23,
        };
        let data = ffi::GhosttyString {
            ptr: b"hello".as_ptr(),
            len: 5,
        };
        let content = ffi::GhosttyClipboardContent { mime, data };
        let write = ffi::GhosttyClipboardWrite {
            location: ffi::GHOSTTY_CLIPBOARD_LOCATION_STANDARD,
            contents: std::ptr::from_ref(&content),
            contents_len: 1,
            ..Default::default()
        };
        assert_eq!(
            unsafe { clipboard_text(&write) },
            Some(Some("hello".into()))
        );

        let empty = ffi::GhosttyClipboardWrite {
            location: ffi::GHOSTTY_CLIPBOARD_LOCATION_STANDARD,
            contents: std::ptr::null(),
            contents_len: 0,
            ..Default::default()
        };
        assert_eq!(unsafe { clipboard_text(&empty) }, Some(None));

        let other = ffi::GhosttyClipboardContent {
            mime: ffi::GhosttyString {
                ptr: b"image/png".as_ptr(),
                len: 9,
            },
            data,
        };
        let image = ffi::GhosttyClipboardWrite {
            location: ffi::GHOSTTY_CLIPBOARD_LOCATION_STANDARD,
            contents: std::ptr::from_ref(&other),
            contents_len: 1,
            ..Default::default()
        };
        assert_eq!(unsafe { clipboard_text(&image) }, None);
    }

    static REPLY_RESULT: AtomicU32 = AtomicU32::new(u32::MAX);

    unsafe extern "C" fn record_reply(
        _write: *const ffi::GhosttyClipboardWrite,
        reply: *const ffi::GhosttyClipboardWriteReply,
    ) {
        if reply.is_null() {
            return;
        }
        REPLY_RESULT.store(unsafe { (*reply).result }, Ordering::SeqCst);
    }

    /// Drive the callback and return the result it handed back to libghostty. Only
    /// for requests that are refused before the real clipboard is opened.
    fn clipboard_reply(write: ffi::GhosttyClipboardWrite) -> ffi::GhosttyClipboardWriteResult {
        REPLY_RESULT.store(u32::MAX, Ordering::SeqCst);
        unsafe { on_clipboard_write(std::ptr::null_mut(), std::ptr::null_mut(), &write) };
        REPLY_RESULT.load(Ordering::SeqCst)
    }

    #[test]
    fn clipboard_writes_are_denied_until_enabled() {
        assert!(!CLIPBOARD_WRITE.load(Ordering::SeqCst), "writes start off");

        let content = ffi::GhosttyClipboardContent {
            mime: ffi::GhosttyString {
                ptr: b"text/plain".as_ptr(),
                len: 10,
            },
            data: ffi::GhosttyString {
                ptr: b"hello".as_ptr(),
                len: 5,
            },
        };
        let text = ffi::GhosttyClipboardWrite {
            location: ffi::GHOSTTY_CLIPBOARD_LOCATION_STANDARD,
            contents: std::ptr::from_ref(&content),
            contents_len: 1,
            reply: Some(record_reply),
            ..Default::default()
        };
        // No contents means "clear", and it is gated by the same policy.
        let clear = ffi::GhosttyClipboardWrite {
            location: ffi::GHOSTTY_CLIPBOARD_LOCATION_STANDARD,
            reply: Some(record_reply),
            ..Default::default()
        };

        assert_eq!(
            clipboard_reply(text),
            ffi::GHOSTTY_CLIPBOARD_WRITE_RESULT_DENIED
        );
        assert_eq!(
            clipboard_reply(clear),
            ffi::GHOSTTY_CLIPBOARD_WRITE_RESULT_DENIED
        );

        // A non-standard location stays unsupported even once writes are allowed.
        set_clipboard_write(true);
        let selection = ffi::GhosttyClipboardWrite {
            location: ffi::GHOSTTY_CLIPBOARD_LOCATION_SELECTION,
            reply: Some(record_reply),
            ..Default::default()
        };
        assert_eq!(
            clipboard_reply(selection),
            ffi::GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED
        );
        set_clipboard_write(false);
    }

    #[test]
    fn clipboard_text_is_sanitized_and_bounded() {
        assert_eq!(sanitize_clipboard("a\x1b[2Jb\x07c"), "a[2Jbc");
        assert_eq!(sanitize_clipboard("tab\there\r\n"), "tab\there\r\n");
        assert_eq!(sanitize_clipboard("\u{0}\u{1}\u{1f}"), "");

        assert!(clipboard_fits("hello"));
        assert!(clipboard_fits(&"x".repeat(CLIPBOARD_MAX_BYTES)));
        assert!(!clipboard_fits(&"x".repeat(CLIPBOARD_MAX_BYTES + 1)));
    }

    #[test]
    fn mouse_modes_and_encoding_follow_the_terminal() {
        let mut vt = Vt::new(80, 24, 10, 20);
        assert!(!vt.mouse_tracking());
        vt.write(b"\x1b[?1000h\x1b[?1006h");
        assert!(vt.mouse_tracking());
        assert!(!vt.mouse_any_event());
        let press = vt.encode_mouse(
            ffi::GHOSTTY_MOUSE_ACTION_PRESS,
            Some(ffi::GHOSTTY_MOUSE_BUTTON_LEFT),
            15.0,
            25.0,
            0,
        );
        assert_eq!(press, b"\x1b[<0;2;2M");
        let wheel = vt.encode_mouse(
            ffi::GHOSTTY_MOUSE_ACTION_PRESS,
            Some(ffi::GHOSTTY_MOUSE_BUTTON_FOUR),
            15.0,
            25.0,
            0,
        );
        assert_eq!(wheel, b"\x1b[<64;2;2M");
        vt.write(b"\x1b[?1000l");
        assert!(!vt.mouse_tracking());
        vt.write(b"\x1b[?1003h");
        assert!(vt.mouse_any_event());
    }

    #[test]
    fn progress_percentages_are_clamped() {
        assert_eq!(percent_of(-1), None);
        assert_eq!(percent_of(0), Some(0));
        assert_eq!(percent_of(100), Some(100));
        assert_eq!(percent_of(101), None);
    }
}
