# agent-notify

**A tray companion for your AI coding agents.** One small Go binary that
watches your [herdr](https://herdr.dev) sessions — local, Windows-side, and
inside WSL — and shows at a glance what every agent is doing, popping up only
when one needs you.

```
herdr sessions (local · windows · wsl)
        │  poll + diff
        ▼
   agent-notify ──▶ tray icon (color = fleet state) ──▶ flyout panel on click
                 └─▶ popups · bell · command · webhook
```

[![ci](https://github.com/jaltez/agent-notify/actions/workflows/ci.yml/badge.svg)](https://github.com/jaltez/agent-notify/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

## Why

Agent runners are quiet: you check on them, or you don't. agent-notify makes
the fleet glanceable and loud exactly when it should be — an agent finished,
went idle, or got blocked — and silent the rest of the time.

- **Tray icon** — color tracks the worst live state:
  🔴 blocked · 🟠 session offline · 🔵 working · 🟢 all stopped, someone waits · ⚪ idle.
  Blocked blinks until resolved; entering 🟢 blinks briefly (2 min) so a
  finished fleet catches the eye, then settles into a steady green — an
  idle fleet is the resting state, not an alarm.
- **Left click → flyout panel** — fleet summary, spaces grouped by
  priority (blocked first), every agent listed — status dot, subtle runner
  name, and what it's doing — refreshed every second. Mouse wheel scrolls
  past 20 rows; auto-close (8 s) only applies while everything fits, and
  outside click / Escape always dismiss.
- **Right click → menu** — same information, native menu (the only UI on
  Linux/macOS).
- **Popups** — Windows toasts (or notify-send) when an agent wants you:
  `claude finished` / *what it was doing* / `project · space — fleet counts`.

## Quickstart

**Windows** —

```powershell
irm https://raw.githubusercontent.com/jaltez/agent-notify/main/scripts/install.ps1 | iex
```

Installs into `%LOCALAPPDATA%\Programs\agent-notify`, adds it to PATH and
creates a Startup shortcut. Options: `-NoAutostart`, `-Version v0.3.0`,
`-WithWSL` (also installs the headless daemon inside the WSL distro as a
systemd user service; `-WslDistro <name>` picks a non-default distro).
Manual route: download `agent-notify_windows_amd64.zip` from the
[latest release](https://github.com/jaltez/agent-notify/releases), unzip,
run `agent-notify.exe` — a one-time toast and the tray menu offer
**Set up agent-notify…**, which installs to the standard location and
registers autostart in a few prompts.

**Linux / WSL** —

```bash
curl -fsSL https://raw.githubusercontent.com/jaltez/agent-notify/main/scripts/install.sh | sh
```

Or `go install github.com/jaltez/agent-notify@latest`. A manually
downloaded binary offers the same `agent-notify setup` wizard.

Then: run it. A tray icon appears; sessions are auto-discovered.
Left-click the icon for the panel. No config required — `agent-notify
probe` shows what it can see, `agent-notify test` fires a test popup.

## Setup & uninstall

`agent-notify setup` is the interactive wizard behind all of the above —
run it from anywhere (a Downloads copy installs itself to the standard
location: `~/.local/bin`, or `%LOCALAPPDATA%\Programs\agent-notify` on
Windows). It offers, one prompt at a time:

- install / self-copy to the canonical location (renamed downloads are
  canonicalized back to `agent-notify`),
- adding that directory to PATH,
- residency — Windows: Startup shortcut; Linux desktop: XDG autostart;
  headless Linux/WSL: systemd user service (+ optional lingering so it
  runs without a login session),
- the WSL daemon (from Windows), an annotated config, and a test popup.

`--yes` takes the defaults (what the install scripts pass through),
`--status` prints the current deployment state, `--no-autostart`,
`--service`, `--with-wsl`/`--wsl-distro` preselect the answers.
`agent-notify uninstall [--purge-config]` reverses all of it.

## Updates

- The tray checks GitHub releases at startup and daily; when one lands you
  get a toast and a **tray menu → Update & restart** item (download is
  checksum-verified; the swap renames the running exe safely on Windows).
- CLI: `agent-notify update` applies immediately, `--check` only looks.
- The headless daemon (`run`, incl. the systemd service and the WSL
  daemon) applies updates on its own — daily check, checksum-verified
  swap, then restart through systemd when running as a unit.
- Package managers update their own way (install.ps1 / install.sh re-run).

## Events

| Kind             | Meaning                             | Default |
| ---------------- | ----------------------------------- | ------- |
| `agent_idle`     | `working → idle` (turn finished)    | on      |
| `agent_done`     | `working → done` (task finished)    | on      |
| `agent_blocked`  | agent became blocked                | on      |
| `agent_working`  | agent started working               | off     |
| `agent_spawned`  | new agent pane appeared             | off     |
| `agent_left`     | agent pane disappeared              | off     |
| `session_down`   | a session stopped responding        | off     |
| `session_up`     | a session responded again           | off     |

`events = ["attention"]` (the default) selects the first three. Use explicit
kinds, `"all"`, or `"none"`.

## Where sessions come from

| Backend   | Discovers                                       | Auto-enabled when                |
| --------- | ----------------------------------------------- | -------------------------------- |
| `local`   | Unix sockets in `~/.config/herdr`               | running on Linux/WSL/macOS       |
| `windows` | `herdr.exe` default + configured sessions       | `herdr.exe` resolves in PATH     |
| `wsl`     | all sessions inside a WSL distro (via `wsl.exe`)| running natively on Windows      |

So one binary covers every placement — on Windows it sees Windows **and**
WSL sessions; in WSL it sees local sockets **and** the Windows side.

## Configuration

Zero config works. To customize:

```bash
agent-notify init     # writes an annotated config, prints its path
```

Path: `--config` → `$AGENT_NOTIFY_CONFIG` →
`~/.config/agent-notify/config.toml` / `%APPDATA%\agent-notify\config.toml`.
On WSL, when no local config exists, agent-notify automatically loads the
**Windows-side** `%APPDATA%` config — one file drives both the Windows
tray and a WSL daemon (`agent-notify config path` shows the resolution).
`agent-notify config edit | validate` manage it.

```toml
events   = ["attention"]
cooldown = "500ms"

[herdr]
exclude = ["scratch*"]      # session-name globs

[[sink]]
type  = "tray"
popup = true                # attention popups from the tray

[[sink]]
type     = "webhook"        # Discord/Slack/ntfy/Home Assistant — any HTTP hook
url      = "https://discord.com/api/webhooks/…"
body_template = '{"content": "{{.Session}}: {{.Agent}} {{.Verb}} — {{.Title}}"}'
```

Notifiers: `tray`, `popup`, `bell`, `command` (templated argv), `webhook`
(JSON or templated body), `log`. Multiple instances allowed.

Templates (popup title/body, command argv, webhook bodies) are Go
`text/template`s over the event: `.Kind .Verb .Time .Source .Host .Session
.Agent .From .To .Title .Project .PaneID .Focused`.

## Build

Requires [Go](https://go.dev) 1.25+ (any OS; macOS tray needs cgo):

```bash
git clone https://github.com/jaltez/agent-notify && cd agent-notify
make build            # bin/agent-notify + bin/agent-notify.exe (+ console debug build)
make test
```

The Windows binary is `windowsgui` — no console window, ever; CLI
subcommands (`probe`, `test`, `monitor`) still print from terminals.

> On Windows a **running tray instance locks its exe** — close the old
> tray (or `taskkill /IM agent-notify.exe /F`) before rebuilding, or the
> build fails with "Access is denied" and the old binary stays in place.

## Run at login

- **Windows** — `agent-notify setup` (or the installer) creates a Startup
  shortcut (`shell:startup`, visible and removable; skip with
  `--no-autostart`).
- **Linux desktop** — the wizard writes an XDG autostart entry
  (`~/.config/autostart/agent-notify.desktop`), the standard mechanism
  for tray apps.
- **Linux/WSL headless** — `agent-notify setup --service` (or
  `install.sh --service`) enables the systemd user service; logs via
  `journalctl --user -u agent-notify`. Optional lingering runs it without
  a login session. The unit is embedded in the binary — no downloads at
  install time.

## Commands

`tray` (default) · `run` (headless daemon) · `monitor [--json] [--all]` ·
`probe` · `test` · `init [--force]` · `setup [--status] [--yes]
[--service] [--no-autostart] [--with-wsl]` · `uninstall [--purge-config]` ·
`config path|edit|validate` · `update [--check]` · `version` · `flytest`
(UI diagnostics)

## Troubleshooting

| Symptom | Fix |
| --- | --- |
| No sessions appear | Start the herdr servers; `agent-notify probe` shows per-session errors. |
| No popups on Windows | Focus Assist / Do Not Disturb silently queues toasts — check the notification center. |
| `popup: unavailable` | Neither `notify-send` nor `powershell.exe` in PATH; install one or set `binary` in the sink. |
| WSL sessions missing on Windows | `wsl.exe -e sh -c 'ls ~/.config/herdr'` must list sockets; non-default herdr paths need `herdr.wsl.extra_path`. |
| WSL daemon service skipped | systemd off in the distro — set `[boot] systemd=true` in `/etc/wsl.conf`, run `wsl --shutdown`, re-run `setup --service`. |
| `setup` can't enable the service on WSL | Same fix; `setup --status` shows whether a systemd user manager is reachable. |
| Setup wizard window closes instantly | It runs in `agent-notify-console.exe`; re-run `agent-notify setup` from any terminal instead. |
| `no display available` | Headless box — use `run`/`monitor`, not the tray. |
| Second tray icon | Not possible — a second instance refuses to start by design. |

## Contributing

Issues and PRs welcome. `make test vet fmt` before submitting; keep PRs
focused.

## License

[MIT](LICENSE)
