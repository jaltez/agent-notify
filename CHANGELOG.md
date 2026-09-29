# Changelog

## Unreleased

- **Tray blink fix**: an idle fleet no longer blinks green forever.
  Entering the waiting state now blinks for 2 minutes (the moment the
  popup fires), then settles into a steady green; a tray that starts
  with already-idle agents never blinks. Blocked still blinks until
  resolved. Waiting remains a color state — it just stops being a
  permanent alarm.

- **Setup wizard — the binary installs itself**: `agent-notify setup`
  walks a freshly downloaded binary through deploying it: self-copy to
  the canonical location (`~/.local/bin` / `%LOCALAPPDATA%\Programs\agent-notify`,
  renamed downloads canonicalized back to `agent-notify`), PATH, login
  residency, config and a test popup. `--status` prints the deployment
  state, `--yes` takes defaults, `--no-autostart`/`--service`/`--with-wsl`
  preselect answers. `agent-notify uninstall [--purge-config]` reverses
  everything.
- **First-run onboarding**: when a run looks un-deployed (not installed,
  no autostart, no config) the tray gains a **Set up agent-notify…**
  menu item and fires a single one-time toast; the wizard opens in a
  console window on Windows (`agent-notify-console.exe`) or a terminal
  on Linux. Nothing is ever modified without the user clicking through.
- **Residency per OS best practice**: Windows Startup shortcut via the
  wizard itself (no PowerShell-only path anymore); Linux desktops get
  XDG autostart (`~/.config/autostart/agent-notify.desktop`); headless
  Linux/WSL gets the systemd user service with the unit **embedded in
  the binary** (no unit download at install time), optional
  `loginctl enable-linger`, and exact fix instructions when systemd is
  off in a WSL distro.
- **Daemon self-update**: headless `run` (incl. the systemd service and
  the WSL daemon) now checks for releases daily and applies them on its
  own — checksum-verified swap, then a restart through
  `systemctl --user restart agent-notify` when running as a unit
  (`INVOCATION_ID`). The unit now uses `Restart=always` so the clean
  self-update exit brings the daemon back on the new binary.
- **Installers delegate to the wizard**: `install.ps1` and
  `install.sh --service` now download/verify/place the binary and hand
  everything else to `setup --yes`, so script installs and manual
  installs end up identical (same PATH handling, same shortcut, same
  WSL flow).
- **Windows installer**: `irm https://raw.githubusercontent.com/jaltez/agent-notify/main/scripts/install.ps1 | iex`
  — downloads the latest release, verifies the sha256 checksum, installs
  into `%LOCALAPPDATA%\Programs\agent-notify`. A running tray is stopped
  and restarted so upgrades work in place; `-WithWSL` also installs the
  headless daemon inside the WSL distro. The Windows release zip bundles
  `scripts/install.ps1`.

## 0.3.0 — 2026-09-11

- **Self-update**: `agent-notify update` (checksum-verified download via
  GitHub releases, safe running-exe swap) plus tray integration — silent
  check at startup + daily, toast and an "Update & restart" menu item
  when a release lands.
- **Install script**: `curl …/install.sh | sh` for Linux/WSL.
- **goreleaser pipeline**: tag-driven releases with `checksums.txt`;
  per-build version injection (`-X buildinfo.Version`).
- **Config**: `config path | edit | validate` commands; WSL automatically
  falls back to the Windows-side `%APPDATA%` config when no local one
  exists (one file for both sides).

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
