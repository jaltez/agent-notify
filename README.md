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
  Attention states (blocked, waiting) blink until resolved.
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

**Windows** — download `agent-notify_windows_amd64.zip` from the
[latest release](https://github.com/jaltez/agent-notify/releases), unzip,
run `agent-notify.exe`. (Or
`winget install jaltez.agent-notify` once the winget manifest lands.)

**Linux / WSL** —

```bash
curl -fsSL https://raw.githubusercontent.com/jaltez/agent-notify/main/scripts/install.sh | sh
```

Or `go install github.com/jaltez/agent-notify@latest`.

Then: run it. A tray icon appears; sessions are auto-discovered.
Left-click the icon for the panel. No config required — `agent-notify
probe` shows what it can see, `agent-notify test` fires a test popup.

## Updates

- The tray checks GitHub releases at startup and daily; when one lands you
  get a toast and a **tray menu → Update & restart** item (download is
  checksum-verified; the swap renames the running exe safely on Windows).
- CLI: `agent-notify update` applies immediately, `--check` only looks.
- Package managers update their own way (winget; install.sh re-run).

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

- **Windows** — `Win+R` → `shell:startup` → shortcut to `agent-notify.exe`.
- **Linux/macOS headless** — `agent-notify run` + `contrib/install-systemd.sh`
  (systemd user service; logs via `journalctl --user -u agent-notify`).

## Commands

`tray` (default) · `run` (headless daemon) · `monitor [--json] [--all]` ·
`probe` · `test` · `init [--force]` · `flytest` (UI diagnostics) · `version`

## Troubleshooting

| Symptom | Fix |
| --- | --- |
| No sessions appear | Start the herdr servers; `agent-notify probe` shows per-session errors. |
| No popups on Windows | Focus Assist / Do Not Disturb silently queues toasts — check the notification center. |
| `popup: unavailable` | Neither `notify-send` nor `powershell.exe` in PATH; install one or set `binary` in the sink. |
| WSL sessions missing on Windows | `wsl.exe -e sh -c 'ls ~/.config/herdr'` must list sockets; non-default herdr paths need `herdr.wsl.extra_path`. |
| `no display available` | Headless box — use `run`/`monitor`, not the tray. |
| Second tray icon | Not possible — a second instance refuses to start by design. |

## Contributing

Issues and PRs welcome. `make test vet fmt` before submitting; keep PRs
focused.

## License

[MIT](LICENSE)
