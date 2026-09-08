# Changelog

## Unreleased

- Tray icon blinks (filled ↔ hollow ring) while the fleet is in an
  attention state — blocked, or agents stopped and waiting.
- Flyout: whole agent blocks highlighted with a status-colored wash and
  hover selection; title-first two-line rows; spaces grouped by priority;
  mouse-wheel scrolling past 20 agent blocks.

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
