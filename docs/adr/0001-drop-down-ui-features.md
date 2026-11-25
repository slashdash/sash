---
status: accepted
date: 2026-11-24
---

# Borrow drop-down UI conventions, not terminal behavior

sash already matches the core drop-down conventions (hide on focus loss, opacity,
full height, tabs, search, notifications, session restore). The remaining ideas
worth borrowing from Guake, Yakuake and iTerm2's hotkey window are chrome and
layout features: splits, tab rename/reorder, quick open, edge resizing, a
collapsing tab bar, and a shortcut reference. We adopt those, and reject any
borrowing that would duplicate or replace libghostty-vt: parsing, grids,
scrollback, reflow, styles, cursor, modes, search, OSC handling, and key/mouse
encoding stay libghostty's.

## Boundary

sash owns:

- panel geometry, animation, opacity and placement;
- tab and pane layout, and focus routing;
- names and accents shown in the tab bar;
- overlays (settings, find bar, paste confirmation, palette) and their input
  precedence;
- clipboard admission policy (opt-in, size cap, control-byte stripping) before
  the OS call;
- selection coordinates and the text handed to the OS, read through libghostty.

libghostty-vt owns:

- byte parsing, terminal state, grids and cells, reflow;
- scrollback storage and trimming;
- styles, colors, cursor and modes;
- search state and match enumeration;
- OSC parsing and dispatch, with sash supplying the callbacks;
- key and mouse encoding, read from the terminal's current modes.

The rule: a feature that needs terminal state calls libghostty. If an API is
missing, extend libghostty, not sash. sash keeps no shadow grid, scrollback,
search index, title parser, key encoder, or terminal palette.

## Adopted

| Feature | Source | sash layer | Constraint |
|---|---|---|---|
| Split panes, focus cycling, close pane | Guake, Yakuake | A pane tree above `Vt`; one `Vt` per pane | Layout is new sash code; every pane's terminal state stays libghostty's. Sequenced last. |
| Tab rename, drag-reorder, tab context menu | Guake, Yakuake | `Tab.display_name` and tab ordering | A name only overrides display; OSC titles keep updating libghostty and return when the override is cleared. |
| Quick Open palette (`⌘P`): tabs, actions, recent directories, Guake-style `file:line` targets | Guake | A list overlay; targets open in an external editor | Path detection reads text already extracted through libghostty; it is not a second output parser or terminal feature. |
| Drag the bottom edge to resize; maximize/restore chord | Yakuake, Guake (`F11`) | Panel geometry only | Cell metrics and grid reflow still come from libghostty's metrics and `resize`. |
| Collapse the tab bar when one tab is open | Guake | Tab-bar chrome only | Settings stays reachable from `⌘,` and the menu bar. |
| Shortcut reference card in Settings | Guake | A static list kept beside the key router | Documentation only; no second key-handling path. |

## Rejected

- **Background images and palette/theme pickers.** Guake and Yakuake offer
  these, but they render terminal appearance. That belongs to Ghostty's theme
  and config; a sash palette would fork rendering from libghostty and replace
  Ghostty behavior. sash keeps window-level opacity only.
- **Per-tab global shortcuts.** Redundant with `⌘1`–`⌘9`, and it would create a
  second routing table outside `Sash::key`.
- **A sash-side fallback for search, scrollback, or quick-open scanning.** Even
  as a temporary measure, a shadow implementation would drift from libghostty
  and make the boundary untestable.

## Consequences

- Splits are the only structural change. `Tab` becomes a pane tree; tab title,
  busy state, progress and notification derive from the focused or last-active
  pane, while an unread dot means any pane produced output. Find, selection,
  IME, mouse routing and scroll become pane-scoped. Closing goes pane-first and
  only closes the tab with its last pane.
- Rename is display-only. OSC titles continue to update; the context menu gets
  "Use Automatic Title" to clear the override. No rename path writes back to
  the terminal.
- The palette joins settings, find and paste confirmation as a modal layer. The
  key router must define their precedence once, not per overlay.
- Split chords follow Ghostty, which uses `⌘D` for split right and `⌘⇧D` for
  split down. `⌘D` can therefore no longer mean "duplicate tab"; duplication
  moves to `⌘⇧T` or the tab context menu when splits land.
- Because none of this adds terminal state, libghostty upgrades remain drop-in.
  A proposed feature that cannot respect this boundary needs an upstream change
  first.

## Considered options

- **Use libghostty for splits.** Not possible: libghostty-vt is terminal state,
  not a window or pane manager. Ghostty's split management is application code,
  so sash must own its own layout.
- **Port Guake or Yakuake behavior as-is.** Their tab bars and overlays assume
  GTK/KDE input and window models. sash borrows the interaction, not the
  implementation.
