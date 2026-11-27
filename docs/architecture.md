# Architecture

Sash is a thin macOS shell around two libraries. [Rune](https://github.com/anomalyco/rune)
owns the window, the GPU UI, the menu-bar item and the global shortcut;
[libghostty-vt](https://lib.ghostty.org/vt/) owns every byte of terminal state —
parsing, grids, styles, cursor, modes, and key and mouse encoding. Sash keeps no
shadow grid, scrollback index, title parser or key encoder.

Decisions with real trade-offs live in [`docs/adr/`](adr/). The rule they record:
if a feature needs terminal state, it calls libghostty; if the API is missing,
libghostty is extended, not duplicated here.

## Modules

| File | Role |
| --- | --- |
| `src/term.rs` | `Vt`: libghostty-vt terminal, render state, dirty-row grid snapshots, colours, cursor, scrollback, search, mouse encoding, effects callbacks (bell, pwd, clipboard, notifications, progress, OSC 133), paste modes |
| `src/term/keys.rs` | Rune `KeyEvent` → `GhosttyKeyEvent` → pty bytes via libghostty's encoder |
| `src/term/pty.rs` | portable-pty session, reader thread → channel, writer, resize, live pid/cwd, foreground job detection |
| `src/term/cwd.rs` | `proc_pidinfo` fallback for a shell's live directory (zsh emits no OSC 7 by default) |
| `src/cell_glyphs.rs` | cell-grid rectangles for block elements and box drawing, so TUI logos, meters and frames tile without seams |
| `src/selection.rs` | drag/word/line selection over the grid, copy text with trailing-space trimming |
| `src/split.rs` | pane split tree: insert, remove, focus order, draggable ratios, serialisable layouts and rectangle layout; no terminal state |
| `src/textops.rs` | link detection, shell quoting, bracketed-paste shaping, wheel-pixel to row accumulation |
| `src/app.rs` | `Sash` state, tabs and panes, selection, find, palette, rename/menu, confirm and paste UI, effects, session save/restore, display placement, slide animation clock, the Rune view (tab bar + pane grids + tabbed settings page) |
| `src/tray.rs` | `NSStatusItem` through `tray-icon`, with a generated ghost template glyph; left-click can toggle the panel and right-click opens the menu |
| `src/integration.rs` | optional OSC 133 shell snippets and the guarded rc-file installer for zsh, bash and fish |
| `src/ipc.rs` | `sash open/run/toggle/settings/new-tab` client and the token-guarded distributed-notification listener |
| `src/macos.rs` | `NSScreen` geometry: menu-bar inset, panel height above the Dock, display size, backing scale, numbered display list, the screen under the pointer, display-change notifications, and physical-top placement and overlay level of the panel's `NSPanel` |
| `src/lib.rs` | AppSpec wiring: panel options, global shortcut, poll loop, key routing |

## Rendering

Rendering is a grid of absolutely positioned runs: one background quad per
same-colour span, one text element per same-style span, cell-grid rectangles for
block and box characters, plus a cursor quad. The view is rebuilt from the VT
snapshot, and libghostty-vt tells us which rows are dirty, so a quiet terminal
costs almost nothing per frame.

Terminal rows are rebuilt from the grid each frame, so runs mount and unmount
constantly while a TUI redraws. Each run is keyed by row and column and uses
`Transition::none()`: Rune's default 0.2 s enter and 0.15 s exit fades would
otherwise smear a full-screen app, with old glyphs lingering where they used to
be. Terminal content is state, not an entrance.

## The poll loop and the drop-down

`poll` drains every pane's pty, writes replies (device attributes, cursor
position reports) back to the pty, refreshes the grid and advances the reveal.
Each pane's pty reader thread calls `AppHandle::wake` as bytes arrive, so shell
output is drained and painted on the next frame instead of waiting for another
keystroke or an idle tick.

Rune places the window on the pointer's display when the panel opens, or on the
configured `display`; an AppKit screen-parameters notification re-resolves that
target when displays come or go, while the panel is hidden or open. The window
opens at zero height while the display is resolved and the window placed and
resized, so moving between displays of different sizes or scales does not flash
the old surface mid-slide. The view clips the panel to the animated height, so
the tab bar appears first and the grid unrolls beneath it. While the reveal is
moving, `poll` asks for the next frame with `Tx::wake_after(Duration::ZERO)`;
without that request the native loop goes to rest after the first few frames and
the curtain freezes part-way down, leaving the hide unable to finish.

## The global shortcut

The initial global shortcut is registered from `poll` after the window host
exists (a chord can't be grabbed before its event loop routing does), then the
settings page unregisters it while recording and registers the new chord. The
old chord is released first so pressing it can be recorded instead of toggling
the panel; cancelling with Escape re-registers the old one.
