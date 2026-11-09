# Changelog

All notable changes to rscreen are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and rscreen uses
[Semantic Versioning](https://semver.org/).

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
