# Changelog

All notable changes to rscreen are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and rscreen uses
[Semantic Versioning](https://semver.org/).

## [0.1.1] - 2026-11-09

Security, robustness and UI follow-up. **The local protocol changed to a
bounded custom codec; kill and recreate sessions started by 0.1.0 before
upgrading.**

### Security

- Replaced unmaintained `bincode` with a small explicit, length-bounded wire
  codec; malformed and oversized frames are rejected before allocation.
- Added peer-UID verification on Linux, capped attached clients, bounded client
  canvases/output queues and partial-frame lifetimes.
- Hardened the runtime directory (owner/mode checks, fail closed), rejected
  unsafe config paths (non-regular, symlinked, FIFO/device, foreign-owned or
  group/world-writable configs), and removed the world-writable `/tmp` config
  fallback.
- Set close-on-exec on PTY masters/slaves and close other server descriptors in
  children, so crashed servers cannot leave shells holding the PTY master.
- Bounded PTY/output reads per event-loop turn so a flooding process cannot
  starve signals, input or other panes.
- Added configurable application OSC 52 clipboard writes, stripped bracketed
  paste delimiters from pasted data, restricted OSC 8 schemes, and filtered C1
  control characters at every rendering boundary.
- Added a graceful SIGTERM/SIGHUP grace period before forced pane termination.
- Fixed an OSC 7 percent-decoding panic, a config color parsing panic, and
  alt-screen resize state that could panic the server.
- Added bounded scrollback memory, combining-mark and search-match caps.

### Fixed

- Wide (CJK/emoji) glyphs are now composed with their continuation cells in
  live panes and copy mode.
- Copy-mode clicking no longer copies a single cell and exits.
- Modified keys/Alt chords no longer cancel text prompts or replay as copy-mode
  commands.
- Pending modals are cleared on detach; Enter cancels confirmations instead of
  confirming them.
- Wheel events no longer steal keyboard focus; app mouse grabs are routed
  correctly; 1003 any-motion tracking is enabled.
- Prefix + Unicode keys are decoded instead of silently swallowed; digit and
  Escape key-binding edge cases are handled.
- Help scrolling clamps correctly; help lists every default action.
- Long palette/prompt input scrolls horizontally; narrow overlays and status
  bars no longer overwrite borders or each other.
- Fixed alt-screen mode/margin restoration, CUU/CUD outside margins, wide glyph
  edge cases, tab-stop preservation on resize, OSC 7/8 handling, copy search
  case-folding alignment, wrapped-line trailing spaces and offset layouts.
- Final pane output is rendered before session exit, and startup failures clean
  up their socket/child.

[0.1.1]: https://github.com/summertools/rscreen/releases/tag/v0.1.1

## [0.1.0] - 2026-11-09

First public release.

### Added

- Detachable sessions over Unix sockets, with multiple attached clients and
  session listing, attach, detach and kill commands.
- Tabbed windows, nested side-by-side/stacked panes, zoom, focus navigation,
  pane resize, renaming and confirmations.
- A custom VT100/xterm screen model with scrollback, wide and combining
  characters, 256/truecolor, alternate screen, scroll regions, DEC charsets,
  hyperlinks, bracketed paste, focus and mouse reporting.
- A diff-based ANSI renderer using semantic cells and synchronized output.
- Vi-style copy mode with frozen snapshots, selections, search (`/`, `?`,
  `n`, `N`), an internal paste buffer and OSC 52 clipboard synchronization.
- A polished status bar, prefix hint strip, copy-mode strip, command palette,
  help overlay, prompts, confirmations and toast notifications.
- Mouse support: wheel scrolling, drag-to-select, pane/tab click focus and
  translation of SGR/X10 mouse events to applications.
- TOML configuration for the prefix key, scrollback, status layout, mouse
  behavior, bell behavior, key bindings and the complete color theme.
- Terminal restore guard (termios, alternate screen, mouse, bracketed paste,
  title stack) on normal exit, signals and errors.
- Linux and other Unix support, with session logs and stale-socket handling.
- Unit, renderer/protocol, PTY end-to-end and CLI test suites.

### Known limitations

- Scrollback is not reflowed when the terminal width changes.
- No Sixel/kitty graphics, kitty keyboard protocol or full ZWJ grapheme
  clustering.
- Multiple clients share one layout; the most recently active client controls
  pane dimensions.
- Copy mode on the alternate screen has no primary-screen history.
- OSC 52 clipboard support depends on the outer terminal's policy.

[0.1.0]: https://github.com/summertools/rscreen/releases/tag/v0.1.0
