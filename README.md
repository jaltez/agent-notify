# agent-notify

**Tray notifier for AI coding agents.** A single Go binary that watches
[herdr](https://herdr.dev) sessions — locally, on the Windows side, or inside
WSL — and tells you the moment an agent wants you: a live tray icon that
changes color with agent state, click-to-inspect menus, and desktop popups
when an agent finishes, blocks, or goes idle.

```
                    ┌─────────────────────────────────────────────┐
  herdr sessions    │                agent-notify                 │
  local  ──sockets──▶                                             │
  windows ─herdr.exe▶  poll → diff snapshots → events → sinks      │
  wsl    ──wsl.exe──▶                              │              │
                    └─────────────────────────────────────────────┘
                                       ┌────────────┴────────────┐
                                  tray icon + menu        popups · bell ·
                                  (color = worst state)   command · webhook
```

## What you get

- **Tray icon** whose color tracks the fleet state: 🔴 blocked · 🟠 session offline · 🔵 working · 🟢 all agents stopped, someone waits for you · ⚪ all idle.
- **Left click opens a flyout panel** (Windows): the fleet summary, one
  block per herdr space with one colored row per agent, refreshed live.
  It dismisses on click-outside, Escape, or after 8 s.
- **Right click opens a menu** with the same information (and on Linux/macOS
  the menu is the detail surface).
- Toasts carry the space name and fleet summary as context.
- **Attention popups** on `working → idle/done` and `anything → blocked`
  (Windows toast or `notify-send`; on by default, configurable).
- Extra sinks when you want them: terminal bell, arbitrary commands
  (templated argv), and HTTP webhooks (JSON or templated body — Discord,
  Slack, ntfy, Home Assistant all work).
- **Zero config to start**: sessions are auto-discovered; a sensible
  annotated config is one `agent-notify init` away.

## Events

| Kind             | Meaning                              | Default |
| ---------------- | ------------------------------------ | ------- |
| `agent_idle`     | `working → idle` (turn finished)     | ✅ on   |
| `agent_done`     | `working → done` (task finished)     | ✅ on   |
| `agent_blocked`  | agent became blocked                 | ✅ on   |
| `agent_working`  | agent started working                | off     |
| `agent_spawned`  | new agent pane appeared              | off     |
| `agent_left`     | agent pane disappeared               | off     |
| `session_down`   | a session stopped responding         | off     |
| `session_up`     | a session responded again            | off     |

`events = ["attention"]` (the default) selects the three ✅ kinds. Use
explicit kind names, `"all"`, or `"none"`.

## Session backends (auto-detected)

| Backend   | Discovers                                            | Auto-enabled when                        |
| --------- | ---------------------------------------------------- | ---------------------------------------- |
| `local`   | Unix sockets in `~/.config/herdr`                    | running on Linux/WSL/macOS               |
| `windows` | `herdr.exe` default + configured named sessions      | `herdr.exe` resolves in PATH             |
| `wsl`     | all sessions inside a WSL distro (via `wsl.exe`)     | running natively on Windows              |

So the same binary covers every placement:

| Where it runs          | What it sees                                |
| ---------------------- | ------------------------------------------- |
| Windows (tray app)     | Windows herdr sessions **and** WSL sessions |
| WSL / Linux            | local sockets **and** the Windows side      |

## Quickstart

```bash
git clone <this repo> agent-notify && cd agent-notify
make build            # bin/agent-notify (linux) + bin/agent-notify.exe (windows)

# On Windows: copy bin/agent-notify.exe anywhere and double-click it —
# windowsgui subsystem, so no console window ever appears. From WSL:
# ./bin/agent-notify.exe                     # tray icon appears on Windows
# CLI subcommands still print from terminals (console re-attach) and WSL.

./bin/agent-notify probe     # what can it see, right now?
./bin/agent-notify test      # fire a test popup
./bin/agent-notify monitor   # watch the event stream, no notifications
```

Commands: `tray` (default) · `run` (headless) · `monitor [--json] [--all]` ·
`probe` · `test` · `init [--force]` · `version`.

## Configuration

No config is required. To customize:

```bash
agent-notify init            # writes an annotated config (see below for path)
agent-notify init            # (again) print/rewrite the path if lost
```

Path precedence: `--config PATH` → `$AGENT_NOTIFY_CONFIG` →
`~/.config/agent-notify/config.toml` (Linux/WSL) or
`%APPDATA%\agent-notify\config.toml` (Windows).

A small taste (the full annotated reference is the file `init` writes):

```toml
events   = ["attention"]    # or explicit kinds, "all", "none"
cooldown = "500ms"

[herdr]
include = []                 # session-name globs, e.g. ["work*"]
exclude = []

[herdr.windows]
sessions = ["work"]          # extra named herdr.exe sessions to poll

[[sink]]
type  = "tray"
popup = true                 # attention popups from the tray

[[sink]]
type    = "command"
command = ["notify-send", "-u", "critical", "{{.Agent}} {{.Verb}}", "{{.Title}}"]

[[sink]]
type          = "webhook"
url           = "https://discord.com/api/webhooks/…"
body_template = '{"content": "{{.Session}}: {{.Agent}} {{.Verb}} — {{.Title}}"}'
```

### Templates

Popup titles/bodies, command argv and webhook bodies are Go
`text/template`s over the event:

`.Kind` · `.Verb` · `.Time` · `.Source` · `.Host` · `.Session` · `.Agent` ·
`.From` · `.To` · `.Title` (terminal title) · `.Project` (cwd basename) ·
`.PaneID` · `.Focused`

Defaults render like `claude finished` / `Refactor auth module`.

## Running at login

**Windows** — put a shortcut to `agent-notify.exe` in `shell:startup`
(Win+R → `shell:startup`). Build with `make windows-gui` for a binary
without a console window.

**Linux / WSL (headless, no tray)** — systemd user service:

```bash
sudo make install PREFIX=/usr/local   # or cp bin/agent-notify ~/.local/bin
contrib/install-systemd.sh            # installs + enables agent-notify.service
journalctl --user -u agent-notify -f  # follow the log
```

## Popups & toast styling

Toasts carry three lines:

1. **Title** — from the render templates (`claude finished`)
2. **Body** — the agent's terminal title (what it was doing)
3. **Context** — `project · session — fleet summary`
   (`api · local/work — 2 working · 1 blocked · 1 waiting`)

`image = true` (default) adds a severity-colored circular logo to the toast
(native Windows only — toast images need a Windows-visible file path).
notify-send gets the context line appended to its body instead.

Styling: toast chrome (fonts, colors, position, animations) is rendered by
the Windows shell and follows your system theme — an app cannot restyle it.
What an app controls: its identity name/icon (via the AppUserModelID and a
Start Menu shortcut), the logo image, sounds, expiry, and — with extra COM
setup — action buttons. agent-notify uses the content, logo, and templates;
the rest stays native on purpose.

## Troubleshooting

| Symptom | Fix |
| --- | --- |
| Rows never appear | The herdr servers aren't running — start `herdr` in the session. `probe` shows per-session errors. |
| No popups on Windows | Focus Assist / Do Not Disturb queues toasts silently — check the notification center. |
| `popup: unavailable` on WSL | Neither `notify-send` nor `powershell.exe` found in PATH; install one or set `binary` in the sink. |
| WSL sessions missing from the Windows app | Check `wsl.exe -e sh -c 'ls ~/.config/herdr'` runs and lists sockets; custom herdr locations need `herdr.wsl.extra_path`. |
| `no display available` | Headless box — use `agent-notify run` / `monitor` instead of the tray. |
| A console window appears with the exe | You grabbed `agent-notify-console.exe` (the debug build); the normal `agent-notify.exe` never opens one. |
| Tray icon is a gray dot | That's "all quiet": every visible agent is idle and sessions are reachable. |

## Development

```bash
make test      # go test ./...
make fmt vet
make build     # linux + windows binaries into bin/
```

Layout:

```
main.go                 entry point
internal/cli            subcommands, config/sink wiring
internal/config         TOML config, defaults, embedded example
internal/event          normalized event model
internal/herdr          herdr source: local / windows / wsl backends
internal/engine         poll loops, snapshot diffing, filters, view
internal/sink           tray-adjacent sinks: popup, bell, command, webhook, log
internal/tray           tray icon rendering (DIB/PNG), menu, refresh loop
internal/proc           subprocess helpers (interop quirks, UTF-16, WSLENV)
contrib/                systemd service + installer
```

New sources (other agent runners) implement the `herdr.Backend`-style
discover/fetch interface; new notifiers implement `sink.Sink`.

## License

[MIT](LICENSE)
