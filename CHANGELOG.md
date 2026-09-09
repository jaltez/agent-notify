# Changelog

## 0.2.0 — 2026-09-07

### Added

- **Flyout panel** (Windows): left-click the tray icon for a live status
  panel — spaces grouped by attention priority, every agent listed
  (title-first two-line rows, dim runner column, whole-block status wash,
  hover selection). Grows to the top of the work area; mouse-wheel
  scrolling when the list outgrows the screen; auto-close (8 s) only while
  everything fits, outside click / Escape always dismiss.
- **Attention blink**: the tray icon alternates filled disc ↔ hollow ring
  while the fleet is blocked or waiting.
- **Richer toasts**: three lines (title, what it was doing,
  `project · space — fleet summary`) with a severity-colored logo.
- Toast logo image on native Windows (`image` sink option, default on).
- `flytest` UI diagnostics command.

### Changed

- Windows binary is now `windowsgui` — no console window when launched
  (CLI subcommands re-attach; debug console build still shipped).
- Tray menu lists all agents (cap removed); flyout sizing grows to the
  top of the work area before scrolling.
- Icon rendering refactored to shared pixel functions (filled disc +
  hollow ring); `CreateFontIndirectW` avoids a `CreateFontW` AV seen on
  some systems; children spawn with `CREATE_NO_WINDOW` (no console
  flashes); single-instance tray lock.

## 0.1.0 — 2026-09-07

Initial release.

- Tray icon + flyout panel (Windows) / tray menu tracking live herdr agent
  state across sessions; attention popups, bell, command, webhook, and log
  sinks. Flyout groups spaces by attention priority, lists every agent
  (stable order, dim runner column). The panel stretches to the top of
  the work area and scrolls only when the list outgrows the screen.
- herdr source with auto-detected backends: native Unix sockets,
  `herdr.exe` (Windows native or WSL→Win interop), `wsl.exe` (Win→WSL).
- Event engine: attention set by default (`agent_idle`, `agent_done`,
  `agent_blocked`), all transitions configurable, per-session filters,
  cooldown.
- Single cross-compiling binary (`windowsgui` on Windows, console
  subcommands intact); TOML config with zero-config defaults; systemd user
  service contrib.
