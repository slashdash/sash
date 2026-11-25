// SPDX-FileCopyrightText: 2026 Sash contributors
// SPDX-License-Identifier: MIT

//! sash — a Quake-style drop-down terminal built on Rune and libghostty-vt.

pub mod app;
pub mod cell_glyphs;
pub mod config;
pub mod ffi;
pub mod macos;
pub mod selection;
pub mod split;
pub mod term;
pub mod textops;
pub mod tray;

use std::sync::OnceLock;

use rune::{
    App, AppHandle, AppSpec, Cx, El, KeyEvent, MacWindowOptions, Tx, WindowHandle, WindowOptions,
};

use app::Sash;
use config::Config;
use macos::Screen;

/// Version identity, baked at build time for Settings and bug reports.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GIT_SHA: &str = env!("SASH_GIT_SHA");
pub const BUILD_DATE: &str = env!("SASH_BUILD_DATE");

/// One line a tester can quote: `0.1.0 (b0b93e3, 2026-11-25)`.
pub fn version_label() -> String {
    format!("{VERSION} ({GIT_SHA}, {BUILD_DATE})")
}

struct Boot {
    cfg: Config,
    screen: Screen,
    width: f32,
    height: f32,
}

static BOOT: OnceLock<Boot> = OnceLock::new();
static HANDLE: OnceLock<AppHandle<Sash>> = OnceLock::new();
/// The main window's native handle, kept for settings that change it while it runs.
static WINDOW: OnceLock<WindowHandle> = OnceLock::new();

/// The live panel, if its native window has been created.
pub fn window() -> Option<&'static WindowHandle> {
    WINDOW.get()
}

/// The running app, for work that arrives off the UI thread.
pub fn handle() -> Option<&'static AppHandle<Sash>> {
    HANDLE.get()
}

fn init() -> Sash {
    let boot = BOOT.get().expect("boot");
    Sash::new(
        boot.cfg.clone(),
        boot.screen,
        HANDLE.get().expect("handle").clone(),
        boot.width,
        boot.height,
    )
}

fn view(s: &Sash, cx: &mut Cx<Sash>) -> El {
    app::view(s, cx)
}

fn poll(s: &mut Sash, seconds: f64, tx: &mut Tx) -> bool {
    s.now = seconds;
    let mut changed = false;
    if s.want_show {
        s.want_show = false;
        s.show();
        changed = true;
    }
    // The window opens at zero height, so a display change is placed and resized
    // before the reveal starts instead of flashing the old surface mid-slide.
    if macos::take_display_change() {
        // A display came or went: resolve the configured target again, so the panel
        // follows a display back into the arrangement as soon as it is plugged in.
        s.want_place = true;
    }
    changed |= s.sync_display(tx);
    changed |= s.tick_animation();
    // A visual bell is a short colour flash, so keep frames coming until it fades.
    if s.tabs.iter().any(|tab| tab.bell_until() > s.now) {
        tx.wake_after(std::time::Duration::from_millis(100));
    }
    if !s.refined {
        s.refined = true;
        changed |= s.init_metrics();
    }
    changed |= s.ensure_hotkey();
    changed |= s.sync_shell_edit();
    changed |= s.sync_font_edit();
    changed |= s.pump();
    changed |= s.sync_find();
    changed |= s.sync_rename();
    // An incremental scrollback search keeps ticking until it has caught up.
    if s.find.open && !s.find.complete && !s.find.needle.is_empty() {
        tx.wake_after(std::time::Duration::ZERO);
    }
    // A slide stepped from the frame clock has to ask for the next frame itself:
    // otherwise the window goes to rest after the frame that started it and the
    // reveal freezes partway down (and the hide never reaches `handle.hide`).
    if s.anim.is_some() {
        tx.wake_after(std::time::Duration::ZERO);
    }
    changed
}

fn setup(s: &mut Sash, waker: rune::Waker) {
    macos::watch_display_changes(waker);
    if s.tray.is_none() && std::env::var_os("SASH_NO_TRAY").is_none() {
        tray::forward_events();
        s.tray = tray::build();
        if s.tray.is_some() {
            eprintln!("sash: menu-bar item ready (hotkey {})", s.cfg.hotkey);
        }
    }
}

fn on_key(s: &mut Sash, ev: &KeyEvent, tx: &mut Tx) -> bool {
    s.key(ev, tx);
    true
}

fn on_focus(s: &mut Sash, focused: bool, _tx: &mut Tx) {
    s.focus_changed(focused);
}

fn on_modifiers(s: &mut Sash, mods: rune::Mods, _tx: &mut Tx) {
    s.set_modifiers(mods);
}

fn on_ime(s: &mut Sash, ev: &rune::Ime, _tx: &mut Tx) -> bool {
    s.ime(ev)
}

fn on_quit(s: &mut Sash, _tx: &mut Tx) {
    s.save_session();
}

/// A Finder launch has no terminal, so stderr would be lost. Testers can send
/// `~/Library/Logs/Sash/sash.log` with a bug report instead.
fn init_log() {
    use std::io::IsTerminal;
    use std::os::fd::AsRawFd;

    if std::io::stderr().is_terminal() {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let dir = std::path::PathBuf::from(home).join("Library/Logs/Sash");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("sash.log");
    // One session's scrollback is not worth growing without bound; start clean.
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > 1_000_000) {
        let _ = std::fs::remove_file(&path);
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    // SAFETY: dup2 onto this process's own stderr before the app starts threads.
    unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) };
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        eprintln!(
            "backtrace:
{}",
            std::backtrace::Backtrace::force_capture()
        );
    }));
    eprintln!(
        "--- sash {} on macOS {} ---",
        version_label(),
        std::env::consts::ARCH
    );
}

pub fn run() {
    init_log();
    let cfg = Config::load();
    let screen = macos::preferred_screen(cfg.display).unwrap_or_default();

    let width = cfg
        .width
        .unwrap_or_else(|| screen.width_pt())
        .clamp(320.0, screen.width_pt());
    let height = app::panel_height(cfg.height, screen.usable_height_pt());
    let px_w = (width * screen.scale as f32).round() as u32;
    let px_h = (height * screen.scale as f32).round() as u32;
    let x = ((screen.width_px as f32 - px_w as f32) / 2.0).round() as i32;
    let top = 0;

    let _ = BOOT.set(Boot {
        cfg: cfg.clone(),
        screen,
        width,
        height,
    });
    eprintln!(
        "sash: panel {width}x{height} pt at ({x}, {top}) on a {}x{} pt screen, scale {}",
        screen.width_pt(),
        screen.height_pt(),
        screen.scale
    );

    let spec = AppSpec {
        families: ("Inter", "Menlo"),
        poll: Some(poll),
        on_key: Some(on_key),
        setup: Some(setup),
        ..AppSpec::new("sash", (width, height), init, view)
    };

    let native = MacWindowOptions::default()
        .nonactivating_panel(true)
        .shadow(true)
        .corner_radius(12.0)
        .level(if cfg.always_on_top { 26 } else { 25 });
    let options = WindowOptions {
        visible: false,
        borderless: true,
        transparent: true,
        always_on_top: cfg.always_on_top,
        resizable: false,
        position: Some((x, top)),
        physical_size: Some((px_w, px_h)),
        min_size: Some((0.0, 0.0)),
        ..Default::default()
    }
    .macos(native);

    let app = App::new(spec)
        .accessory()
        .keep_running()
        .configured_options(options)
        .on_focus(on_focus)
        .on_modifiers(on_modifiers)
        .on_ime(on_ime)
        .on_quit(on_quit)
        .on_ready(|w| {
            let _ = WINDOW.set(w.clone());
            // Registration needs the running event loop: the first poll after the window
            // exists grabs the configured chord, and rotates it when settings change it.
            if let Some(handle) = HANDLE.get() {
                handle.wake();
            }
        });

    let _ = HANDLE.set(app.handle());
    app.run();
}
