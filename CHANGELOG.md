# Changelog

All notable changes to Sash are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-11-26

First public test build.

### Added

- Drop-down panel from the physical top edge, 140 ms reveal, global hotkey,
  menu-bar item, display targeting and per-display return.
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
