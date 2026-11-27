# Changelog

All notable changes to Sash are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Split panes are resizable by dragging their divider and zoomable with
  `⌘⇧Return`; split trees, ratios and focus are restored across launches.
- Configurable bell: `off`, `visual`, `audible` or `both` (default `visual`).
- `menu_bar_click` setting: left-click toggles the panel (default) or opens the
  existing menu; right-click always opens the menu.
- Shell-integration installer for zsh, bash and fish, which appends guarded
  OSC 133 hooks for prompt jumps, command-finished notices and cwd tracking.
- Command-line client: `sash open`, `run`, `new-tab`, `toggle` and `settings`
  reach the running instance through a token-guarded distributed notification.

### Changed

- Build libghostty-vt 1.0.2 from the pinned stable source with Zig, replacing
  the nightly prebuilt binary.

## [0.1.0] - 2026-11-26

First public test build.

### Added

- Drop-down panel from the physical top edge, 140 ms reveal, global hotkey,
  menu-bar item, display targeting and per-display return.
- Panel placement setting: cover the menu bar at the physical top edge, or hang
  directly beneath it.
- Tabs: rename, reorder, duplicate, eight colours, pinning, session restore,
  folder-name titles, fill width, 120 pt minimum, four highlight styles,
  unread ring and OSC 9;4 progress.
- Split panes: right/down, focus cycling, exclusive pty and VT per pane.
- Terminal: libghostty-vt core, 256 colours, cursor shapes, Kitty keyboard
  protocol, IME preedit, mouse reporting, bracketed paste, selection, search,
  scrollback, OSC 7/8/9/52/133, desktop notifications.
- Cell-grid rectangles for block and box-drawing characters.
- Settings page (Panel, Terminal, Tabs, Shortcuts) with live config writes.
- Quick open (`⌘P`) over tabs, actions, settings and paths.
- `scripts/bundle.sh` and `scripts/package.sh`: versioned app bundle and
  ad-hoc-signed DMG with a Read Me and checksum.
- Build identity in Settings and the menu-bar menu, stderr logging to
  `~/Library/Logs/Sash/sash.log`, and a one-time first-run card.
- MIT licence, SPDX headers, generated third-party notices.

### Known limitations

- Ad-hoc signed, not notarized.
- Split layout is not restored across launches.
- Shell integration (OSC 133) is not installed automatically.
