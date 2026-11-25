// SPDX-FileCopyrightText: 2026 Sash contributors
// SPDX-License-Identifier: MIT

//! User configuration, loaded from `~/.config/sash/config.json`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Panel height the previous release wrote by default. Config files saved before the
/// settings screen have no `always_on_top` key, so this value is migrated to "full height"
/// once (the panel now defaults to filling the space above the Dock).
const LEGACY_DEFAULT_HEIGHT: f32 = 420.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabHighlight {
    Dot,
    Underline,
    #[default]
    WholeTab,
    Text,
}

impl TabHighlight {
    pub fn next(self) -> Self {
        match self {
            Self::Dot => Self::Underline,
            Self::Underline => Self::WholeTab,
            Self::WholeTab => Self::Text,
            Self::Text => Self::Dot,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Dot => "Dot",
            Self::Underline => "Underline",
            Self::WholeTab => "Whole tab",
            Self::Text => "Text",
        }
    }
}

/// Where the panel hangs from the display.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelPlacement {
    /// Flush with the physical top edge, covering the menu bar.
    #[default]
    ScreenTop,
    /// Directly beneath the menu bar, leaving it visible.
    BelowMenuBar,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Global shortcut that drops the panel down. Needs ⌘, ⌃ or ⌥.
    pub hotkey: String,
    /// Panel height in points. `null` fills the screen from its physical top to the Dock.
    pub height: Option<f32>,
    /// Panel width in points. `null` means the full display width.
    pub width: Option<f32>,
    /// Display to open on, counted from 1: the menu-bar display first, then the
    /// rest left to right. `null` follows the pointer. Falls back to the menu-bar
    /// display while the chosen one is unplugged.
    pub display: Option<u32>,
    /// Cover the menu bar at the physical top edge, or hang directly beneath it.
    pub panel_placement: PanelPlacement,
    /// Put the tab strip under the terminal instead of above it.
    pub tabs_at_bottom: bool,
    /// Divide the available strip width equally between tabs instead of sizing
    /// each tab to its title (with a 120 pt minimum).
    pub tabs_fill_width: bool,
    /// How the active tab is marked.
    pub tab_highlight: TabHighlight,
    /// Terminal font family, or a comma-separated fallback list. `null` uses
    /// the system monospaced font.
    pub font_family: Option<String>,
    /// Terminal font size in points.
    pub font_size: f32,
    /// Background opacity, 0.0–1.0.
    pub opacity: f32,
    /// Slide animation length in milliseconds.
    pub animation_ms: u64,
    /// Shell to run. `null` uses `$SHELL`.
    pub shell: Option<String>,
    /// Hide the panel when Escape is pressed (off by default: vim needs Escape).
    pub hide_on_escape: bool,
    /// Keep the panel above other windows.
    pub always_on_top: bool,
    /// Give the panel keyboard focus when it drops down.
    pub focus_on_open: bool,
    /// Hide the panel when it loses keyboard focus (the Quake reflex).
    pub hide_on_blur: bool,
    /// Lines of scrollback kept per tab. `null` means no line limit.
    pub scrollback_lines: Option<u32>,
    /// Copy a fresh mouse selection to the clipboard without pressing ⌘C.
    pub copy_on_select: bool,
    /// Allow programs to write the clipboard (OSC 52, iTerm2 and Kitty sequences).
    pub clipboard_write: bool,
    /// Ask before pasting text that spans more than one line.
    pub confirm_multiline_paste: bool,
    /// Show desktop notifications requested by programs (OSC 9 / OSC 777).
    pub desktop_notifications: bool,
    /// Ask before closing a tab or quitting while a command still runs.
    pub confirm_close: bool,
    /// Reopen the last session's tabs and directories on launch.
    pub restore_session: bool,
    /// The first-run card has been dismissed. Testers' bug reports quote the
    /// build, so the card only ever appears once.
    pub welcome_seen: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: "cmd+shift+space".into(),
            height: None,
            width: None,
            display: None,
            panel_placement: PanelPlacement::ScreenTop,
            tabs_at_bottom: false,
            tabs_fill_width: false,
            tab_highlight: TabHighlight::WholeTab,
            font_family: None,
            font_size: 13.0,
            opacity: 0.96,
            animation_ms: 140,
            shell: None,
            hide_on_escape: false,
            always_on_top: true,
            focus_on_open: true,
            hide_on_blur: true,
            scrollback_lines: Some(10_000),
            copy_on_select: false,
            clipboard_write: false,
            confirm_multiline_paste: true,
            desktop_notifications: true,
            confirm_close: true,
            restore_session: true,
            welcome_seen: false,
        }
    }
}

impl Config {
    pub fn dir() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("sash")
    }

    pub fn path() -> PathBuf {
        Self::dir().join("config.json")
    }

    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match Self::parse(&text) {
                Ok(config) => config,
                Err(e) => {
                    eprintln!("sash: {}: {e}; using defaults", path.display());
                    Self::default()
                }
            },
            Err(_) => {
                let config = Self::default();
                config.save();
                config
            }
        }
    }

    /// Parse a config file, migrating the panel height the previous release wrote by
    /// default. Files without `always_on_top` predate the settings screen. Numbers
    /// outside the ranges the UI can produce are clamped here, in memory only.
    fn parse(text: &str) -> Result<Self, serde_json::Error> {
        let value: serde_json::Value = serde_json::from_str(text)?;
        let legacy = value.get("always_on_top").is_none();
        let mut config: Config = serde_json::from_value(value)?;
        if legacy && config.height == Some(LEGACY_DEFAULT_HEIGHT) {
            config.height = None;
        }
        // Zero divides by zero in the provisional font metrics; huge values make the
        // terminal impossibly wide or the frame loop wake forever.
        config.font_size = config.font_size.clamp(6.0, 72.0);
        config.animation_ms = config.animation_ms.clamp(0, 2_000);
        // Display numbers are 1-based; 0 behaves like "follow the pointer".
        config.display = config.display.filter(|display| *display > 0);
        Ok(config)
    }

    pub fn save(&self) {
        if let Ok(text) = serde_json::to_string_pretty(self) {
            write_atomic(&Self::path(), &(text + "\n"));
        }
    }
}

/// Write `text` to `path` through a uniquely named sibling file: the bytes are
/// fsynced, then renamed over the destination, so a crash never leaves a truncated
/// config behind. The rename replaces a planted symlink instead of following it.
fn write_atomic(path: &Path, text: &str) {
    let Some(parent) = path.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(parent);

    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let temp = parent.join(format!(
            ".{}.{}.{}.tmp",
            path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp);
        if let Ok(mut file) = file {
            let written = file
                .write_all(text.as_bytes())
                .and_then(|()| file.sync_all());
            drop(file);
            if written.is_err() || std::fs::rename(&temp, path).is_err() {
                let _ = std::fs::remove_file(&temp);
            }
        }
    }

    #[cfg(not(unix))]
    let _ = std::fs::write(path, text);
}

/// What survives a quit: one directory per tab, and the tab that was in front.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub tabs: Vec<SessionTab>,
    pub active: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionTab {
    pub cwd: Option<PathBuf>,
    /// A name set from the tab menu; absent keeps the shell's own title.
    pub name: Option<String>,
    /// Index into the tab colour palette; absent uses the theme's default.
    pub color: Option<u8>,
    /// A pinned tab refuses to close until it is unpinned.
    pub pinned: bool,
}

impl Session {
    pub fn path() -> PathBuf {
        Config::dir().join("session.json")
    }

    /// A missing or malformed session is simply an empty one; it must never
    /// stop the app from starting.
    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(text) = serde_json::to_string_pretty(self) {
            write_atomic(&Self::path(), &(text + "\n"));
        }
    }

    pub fn clear() {
        let _ = std::fs::remove_file(Self::path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_height_is_the_default() {
        assert_eq!(Config::default().height, None);
        assert!(Config::default().always_on_top);
        assert!(Config::default().focus_on_open);
        assert!(Config::default().hide_on_blur);
        assert_eq!(Config::default().font_family, None);
        assert!(!Config::default().tabs_at_bottom);
        assert!(!Config::default().tabs_fill_width);
        assert_eq!(Config::default().tab_highlight, TabHighlight::WholeTab);
        assert!(Config::default().confirm_close);
        assert!(Config::default().restore_session);
        assert!(!Config::default().welcome_seen);
        assert_eq!(Config::default().panel_placement, PanelPlacement::ScreenTop);
        assert_eq!(Config::default().scrollback_lines, Some(10_000));
        assert!(!Config::default().copy_on_select);
        assert!(!Config::default().clipboard_write);
        assert!(Config::default().confirm_multiline_paste);
        assert!(Config::default().desktop_notifications);
    }

    #[test]
    fn a_config_without_the_new_keys_gets_their_defaults() {
        let config = Config::parse(
            r#"{"hotkey":"cmd+shift+space","always_on_top":true,"focus_on_open":true}"#,
        )
        .unwrap();
        assert!(config.hide_on_blur);
        assert_eq!(config.scrollback_lines, Some(10_000));
        assert!(!config.copy_on_select);
        assert!(!config.clipboard_write);
        assert!(config.confirm_multiline_paste);
        assert!(config.desktop_notifications);
        assert!(config.confirm_close);
        assert!(config.restore_session);
        assert!(!config.welcome_seen);
        assert_eq!(config.panel_placement, PanelPlacement::ScreenTop);
        assert!(!config.tabs_at_bottom);
        assert!(!config.tabs_fill_width);
        assert_eq!(config.tab_highlight, TabHighlight::WholeTab);
        assert_eq!(config.font_family, None);
    }

    #[test]
    fn a_pinned_display_round_trips_and_zero_means_follow_the_pointer() {
        let config = Config {
            display: Some(2),
            ..Config::default()
        };
        let text = serde_json::to_string(&config).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.display, Some(2));

        let config = Config::parse(r#"{"display":0}"#).unwrap();
        assert_eq!(config.display, None);
        assert_eq!(Config::default().display, None);
    }

    #[test]
    fn panel_placement_round_trips() {
        let config = Config {
            panel_placement: PanelPlacement::BelowMenuBar,
            ..Config::default()
        };
        let text = serde_json::to_string(&config).unwrap();
        assert!(text.contains("\"panel_placement\":\"below_menu_bar\""));
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.panel_placement, PanelPlacement::BelowMenuBar);
    }

    #[test]
    fn a_bottom_tab_strip_round_trips() {
        let config = Config {
            tabs_at_bottom: true,
            ..Config::default()
        };
        let text = serde_json::to_string(&config).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert!(back.tabs_at_bottom);
    }

    #[test]
    fn tab_width_and_highlight_settings_round_trip() {
        let config = Config {
            tabs_fill_width: true,
            tab_highlight: TabHighlight::Underline,
            ..Config::default()
        };
        let text = serde_json::to_string(&config).unwrap();
        assert!(text.contains("\"tab_highlight\":\"underline\""));
        let back: Config = serde_json::from_str(&text).unwrap();
        assert!(back.tabs_fill_width);
        assert_eq!(back.tab_highlight, TabHighlight::Underline);
        assert_eq!(TabHighlight::Dot.next(), TabHighlight::Underline);
        assert_eq!(TabHighlight::Underline.next(), TabHighlight::WholeTab);
        assert_eq!(TabHighlight::WholeTab.next(), TabHighlight::Text);
        assert_eq!(TabHighlight::Text.next(), TabHighlight::Dot);
    }

    #[test]
    fn a_font_family_round_trips() {
        let config = Config {
            font_family: Some("JetBrains Mono, Menlo".into()),
            ..Config::default()
        };
        let text = serde_json::to_string(&config).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.font_family.as_deref(), Some("JetBrains Mono, Menlo"));
    }

    #[test]
    fn the_new_keys_round_trip() {
        let config = Config {
            hide_on_blur: false,
            scrollback_lines: None,
            copy_on_select: true,
            confirm_multiline_paste: false,
            desktop_notifications: false,
            confirm_close: false,
            restore_session: false,
            welcome_seen: true,
            ..Config::default()
        };
        let text = serde_json::to_string(&config).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert!(!back.hide_on_blur);
        assert_eq!(back.scrollback_lines, None);
        assert!(back.copy_on_select);
        assert!(!back.confirm_multiline_paste);
        assert!(!back.desktop_notifications);
        assert!(!back.confirm_close);
        assert!(!back.restore_session);
        assert!(back.welcome_seen);
    }

    #[test]
    fn a_legacy_default_height_migrates_to_full() {
        let config = Config::parse(
            r#"{"hotkey":"cmd+shift+space","height":420.0,"font_size":13.0,"opacity":0.96,"animation_ms":140,"hide_on_escape":false}"#,
        )
        .unwrap();
        assert_eq!(config.height, None);
        assert!(config.always_on_top);
        assert!(config.focus_on_open);
    }

    #[test]
    fn a_height_chosen_in_settings_is_not_migrated_again() {
        let config =
            Config::parse(r#"{"height":420.0,"always_on_top":true,"focus_on_open":true}"#).unwrap();
        assert_eq!(config.height, Some(420.0));
    }

    #[test]
    fn an_explicit_pixel_height_survives_a_round_trip() {
        let config = Config {
            height: Some(512.0),
            ..Config::default()
        };
        let text = serde_json::to_string(&config).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.height, Some(512.0));
        assert!(back.always_on_top);
    }

    #[test]
    fn clipboard_write_is_off_by_default_and_round_trips() {
        assert!(!Config::default().clipboard_write);
        let config = Config {
            clipboard_write: true,
            ..Config::default()
        };
        let text = serde_json::to_string(&config).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert!(back.clipboard_write);
    }

    #[test]
    fn a_zero_font_size_and_a_huge_animation_are_clamped() {
        let config = Config::parse(r#"{"font_size":0.0,"animation_ms":10000000}"#).unwrap();
        assert_eq!(config.font_size, 6.0);
        assert_eq!(config.animation_ms, 2_000);
    }

    #[test]
    fn a_too_large_font_size_is_clamped() {
        let config = Config::parse(r#"{"font_size":500.0}"#).unwrap();
        assert_eq!(config.font_size, 72.0);
    }

    #[test]
    fn numbers_in_range_are_left_alone() {
        let config = Config::parse(r#"{"font_size":13.0,"animation_ms":0}"#).unwrap();
        assert_eq!(config.font_size, 13.0);
        assert_eq!(config.animation_ms, 0);
    }

    #[test]
    fn a_session_round_trips() {
        let session = Session {
            tabs: vec![
                SessionTab {
                    cwd: Some(PathBuf::from("/tmp")),
                    name: Some("build".into()),
                    color: Some(3),
                    pinned: true,
                },
                SessionTab {
                    cwd: None,
                    name: None,
                    color: None,
                    pinned: false,
                },
            ],
            active: 1,
        };
        let text = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&text).unwrap();
        assert_eq!(back.tabs.len(), 2);
        assert_eq!(back.tabs[0].name.as_deref(), Some("build"));
        assert_eq!(back.tabs[0].cwd.as_deref(), Some(Path::new("/tmp")));
        assert_eq!(back.tabs[0].color, Some(3));
        assert!(back.tabs[0].pinned);
        assert_eq!(back.tabs[1].cwd, None);
        assert_eq!(back.active, 1);
    }

    #[test]
    fn a_malformed_session_is_a_serde_error() {
        assert!(serde_json::from_str::<Session>("{\"tabs\": 2").is_err());
    }

    /// A fresh directory under the system temp dir that no other test is using.
    fn temp_dir(tag: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("sash-{tag}-{}-{unique}", std::process::id()))
    }

    #[test]
    fn a_save_is_atomic_and_private() {
        let dir = temp_dir("config");
        let path = dir.join("nested").join("config.json");
        write_atomic(&path, "hello\n");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("config.json")]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_save_replaces_a_planted_symlink() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("symlink");
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, "untouched").unwrap();
        let path = dir.join("config.json");
        symlink(&victim, &path).unwrap();

        write_atomic(&path, "new\n");

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
        assert!(!std::fs::symlink_metadata(&path).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
