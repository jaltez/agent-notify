# Rust port notes (branch `rust-port`)

Status: **WIP, being iterated with the user.** Everything below documents what
exists on the branch today, not what is planned.

- Branch: `rust-port` (base: scaffold commit `ea414ca rust-port: scaffold that
  compiles (rune pinned rev 003a5c7)`). The module ports live in the working
  tree on top of it, not yet committed.
- Toolkit: [`rune`](https://github.com/anomalyco/rune) pinned to rev
  `003a5c70c5f9805eae86ebb35adf965aa262c5aa` (`Cargo.toml:28-30`, crates
  `rune`, `rune-kit`, `rune-notify` all at the same rev). The vendored source
  under `~/.cargo/git/checkouts/rune-c9b805e9ed8a3730/003a5c7` was read to
  verify every rune API the port uses.
- Layout: a binary crate named `agent-notify` at the repo root, coexisting
  with the Go tree (`internal/`, `main.go` are untouched). Edition 2024,
  matching rune's workspace.
- `rune`'s Linux tray backend is StatusNotifierItem/dbusmenu over zbus and is
  enabled by the default `dbus` feature (not disabled here), which is what
  makes WSLg testing possible.

## Gates (re-run for this document, 2026-11-12)

| Check | Command | Result |
| --- | --- | --- |
| cargo check | `CARGO_TARGET_DIR=/tmp/an-notes-check cargo check` | green — `Finished \`dev\` profile [unoptimized + debuginfo] target(s) in 29.25s`, 0 errors |
| cargo test | `cargo test` | green — `95 passed; 0 failed; 1 ignored` in 2.28s (the ignored test is the headless flyout PNG smoke, see §6) |
| windows-gnu probe | `cargo check --target x86_64-pc-windows-gnu` | exit 0 — fresh compile (not cached), `Finished \`dev\` profile ... in 33.76s` |

Round-1 gates recorded during the port were also all green (check, test,
windows-gnu check).

## 1. Module parity table

| Go (internal/) | Rust (src/) | State |
| --- | --- | --- |
| `herdr/` (herdr.go, local.go, windows.go, wsl.go) | `herdr.rs` | Complete. Audited line-by-line against the Go; 5 fidelity fixes over the scaffold; 4/4 Go tests ported and passing. |
| `engine/` (engine.go, tracker.go, view.go) | `engine.rs` | Complete. Exact hysteresis (downAfter=3, sticky unknown, wants latch, `kind\|host\|session\|pane` cooldown), sorted view + severity + summary, per-backend ticker threads on `std::thread` + mpsc (no tokio). All 16 Go tests ported and passing. |
| `render/` | `render.rs` | Complete, with a deliberately minimal template engine (limits in §4). |
| `sink/` | `sink.rs` | Complete: Manager fan-out (std threads + mpsc, per-sink queue of 16, 30s delivery timeout), popup, bell, command, webhook, log, `build_sinks`. |
| `setup/` | `setup/{mod,posix,windows}.rs` + embedded templates `setup/agent-notify.service`, `setup/agent-notify.desktop` | Complete. go:embed templates copied byte-for-byte and `include_str!`-ed. |
| `update/` | `update.rs` | Complete. The go-selfupdate v1.6.0 flow (DetectLatest os/arch suffixes + skip draft/prerelease, checksums.txt sha256, tar.gz/zip/gz extraction, `.new`/`.old` swap with Windows hide) reimplemented over ureq+sha2+hex+flate2/tar/zip. |
| `tray/` (tray.go, flyout_rows.go, flyout_windows.go, flyout_other.go) | `tray.rs` + `flyout.rs` | Complete on rune/rune-kit. Palette byte-exact (verified by pixel-sampling a rendered PNG); 6 flyout_rows tests, 2 tray tests + 4 added. |
| `cli/` (cli.go, setup_cmd.go, console_*, singleinstance_*, setuplaunch_*, tty_*) + `main.go` | `cli.rs` + `main.rs` + `build.rs` | Complete. All 12 subcommands (tray/run/monitor/probe/test/init/setup/uninstall/config/update/version/flytest) wired to the real modules. internal/cli has no Go tests; 18 Rust tests pin the Go literals/trees. |
| `event/`, `config/`, `proc/`, `icon/`, `buildinfo/` | `event.rs`, `config.rs`, `proc.rs`, `icon.rs`, `buildinfo.rs` | Complete (these were the scaffold's real modules; verified then by a scratch crate with 40+ checks, and by the test suite now). |

Dependencies added for the port: `ureq 2` (tls+json), `sha2`, `hex`,
`flate2`, `tar`, `zip 6` (deflate) for `update.rs`; `libc` for
`platformStopInstances`' `syscall.Kill` and the log sink's local timestamps;
`windows 0.62` with 10 feature groups (cfg windows); rune/rune-kit/rune-notify
pinned as above.

## 2. Workarounds and explicit decisions

### WORKAROUND(rune): tray icon is recreated on every change
rune's `Tray` has no icon-update API — the vendored source
(`crates/rune/src/tray.rs`) only offers `new`, `set_menu` and `set_tooltip`;
the icon is fixed in `TraySpec` and "Dropping the Tray removes the icon".
Every icon change (severity switch and the blink ring) therefore destroys and
recreates the `Tray`, re-applying the current menu and tooltip. Cost while
blinking on Linux: ~2 D-Bus re-registrations per second. Recreations are
serialized through a dedicated mutex (`Tray::swap_icon`, `src/tray.rs:426`)
so blink and refresh can never leave two native trays alive. This is marked
`// WORKAROUND(rune): recreate tray until official icon-update API lands` and
should be revisited when rune ships the real API.

### Flat menu without submenus (explicit user decision #2)
Agents sit in root rows prefixed `host/session · ` instead of being nested
under the session header as in Go. Per-item tooltips are lost (`TrayItem`
only carries id + label). Agent items stay disabled, like the Go.
`rebuildMenu`/`updateMenu` collapse into a single `set_menu` per fingerprint
change (rune has no per-item SetTitle; `set_menu` is atomic and safe with the
menu open on both backends — verified in the vendored win32 pick() and SNI
paths). `structural_key` is still computed and tested; it feeds the
fingerprint.

### Flyout enabled on all platforms (explicit user decision #3)
The flyout is a rune `App`, so it exists on Linux too. On Linux the work-area
clamp is best-effort (bounds of the primary display, docks not excluded;
Wayland ignores `set_position`). The Windows `SPI_GETWORKAREA` path is
cfg-gated on `windows 0.62` — its signature was verified in the vendored
source but that code has **not** been compiled on this host.

### flytest simplified (requested in the port ask)
`flytest` opens and closes the panel once via a driver thread + `FlyControl`
and logs `visible=true/false`. The Win32 introspection of Go's `SelfTest`
(hwnd/rect/foreground/paint counters) is not replicated. Unlike Go, where
flytest was a no-op on non-Windows, it works on every platform.

### Other structural notes
- The tray-app chain is `rune::App::new(AppSpec::new(...)).windowless().handle()`
  plus a `Tray::new` on its own thread. The rune `App` is `!Send`, so the
  main-thread app is wrapped in `MainThreadApp(App<FlyState>)` with an
  `unsafe impl Send` and a SAFETY comment: the app is only created and
  consumed on the main thread (the Go InitFlyout-before-Run rule); the loops
  only touch the `Send + Sync` `FlyControl`.
- Scroll uses rune's viewport (`.scroll(ScrollSpec)`, rune draws the thumb),
  replacing Go's hand-rolled wheelStep 40px math; the "auto-close only armed
  if not scrollable" semantics are ported exactly.
- Sessions lay out in a columns × rows grid, porting `flyout.layout`'s
  scoring onto rune: candidate column counts (170..300 pt per column, 24 pt
  gap), and the **tallest count that fits the fixed grid area** wins — Go's
  aspect-ratio scoring kept small fleets in one honest narrow column, the
  thing the redesign left behind. Nothing fitting ⇒ widest grid, the
  viewport scrolls. rune's `.equal_columns(n)` lays the cells (row-major,
  row height = tallest session), so the hand-rolled 2D hit-testing of the
  Go is gone — clicks are per-row `on_press` anywhere in the grid. Each
  section carries `min_w(0)`: a grid column floors at its widest child's
  min-content, and without the floor a long unbreakable title pushed the
  column past the panel edge.
- **Fixed window + tabs (2026-11-12 redesign)**: the panel is a fixed
  window — ~62 % of the work area wide (1080 pt cap) by ~72 % tall
  (780 pt cap), opened **centered** in the work area, no longer hooked to
  the tray-icon corner; the content-fit shrink is gone — its height
  estimates drifted from rune's real layout, which is where the surprise
  scrollbar came from. A kit `SegmentedControl` tab bar
  switches rooms: "All" (the grid) plus one full-width room per session;
  labels repeat the host only when a session name is ambiguous across
  hosts, and the pick resolves through the current model so a stale index
  cannot pin a vanished session. Auto-close always arms now (the hover
  re-arm keeps long reads); session sort gained a host tiebreak so tabs
  order stably across refreshes.
- **A regular window (2026-11-12)**: titled, resizable, min/max/close,
  not topmost, taskbar icon (`rune::set_app_icon` with the working-yellow
  disc; on Windows it sets title bar, taskbar and Alt+Tab icons). Opening
  geometry (metrics size, centered) applies exactly once — afterwards the
  user owns the window. The chrome is **integrated**, not native: the
  window is borderless, the header row carries `window_drag` plus drawn
  minimize (`Tx::minimize`), maximize (`Tx::toggle_maximized`) and close
  (hide; the app keeps running and the tray reopens the window with its
  geometry) buttons — `El::no_window_drag` keeps presses off the drag
  region. The Go flyover dismissal semantics are gone: no auto-close
  timer, no Escape/click-outside dismissal, no focus-loss hide.
- **Bulk row controls + title mode (2026-11-12)**: right of the tab bar,
  `collapse all` / `expand all` set an explicit open override on every
  session and settled group in the fleet (`FlyState::set_all`), and
  `titles: task` / `titles: tab` switches agent labels between herdr's
  task title and the colored tab name "<space> - #<n>" (project muted,
  pane accent — `title_parts` parses the pane id). The mode rides the
  content fingerprint, so a toggle re-renders. Close hides the window;
  agent clicks keep it open (the fleet overview stays up while the client
  jumps to the agent).
- Go's `activateGrace` (600 ms) is gone with the flyover semantics — a
  regular window has no focus-loss dismissal to gate.
- `gdiSafe` is kept byte-for-byte in the model (even though rune renders emoji
  via Parley) for model fidelity and its tests.
- Palette: Go COLORREFs are converted with `srgb_decode` to linear light
  because `rune::Color` works in linear space; the first version skipped the
  decode and rendered ~2x too bright — fixed and pinned by a test against
  `rune::color::srgb_decode`. Washes and all palette colors are byte-exact
  (pixel-sampled: bg (32,32,32), wash (52,36,37) vs Go (51,36,37), ±rounding).
- Native Windows popup (documented improvement): popups on native Windows go
  through rune-notify (register an `Identity` with a fixed icon, then `post`)
  instead of shelling PowerShell, which Go also did on Windows. `register()`
  writes to HKCU — an effect Go never had; on failure it falls back to
  notify-send. The native identity id is `cfg.app_id` when set and
  backslash-free, else `NATIVE_APP_ID = "com.jaltez.agent-notify"` (Go's
  default PowerShell AUMID is not usable as a registry key).

## 3. Deviations and PORT-NOTEs by module

### herdr.rs
- Timeout: Go backends store their timeout in the constructor and wrap ctx on
  every call; the trait contract passes the timeout as a `discover`/`fetch`
  parameter — the engine keeps it in `BackendRun{poll,timeout}` from config.
  Semantics identical: every call bounded by the configured timeout.
- `parse_snapshot` keeps the `"parse snapshot JSON: "` error prefix; the inner
  text is serde_json's, not encoding/json's.
- `Local::discover` returns an empty Vec where Go returned a nil slice (same
  for the caller); Rust's sort is stable and Go's `sort.Slice` is not
  (irrelevant: names are unique per socket).
- `Session.Ref` renamed `ref_` (Rust keyword); `Session.Home` kept.
- Go's `TestDecodeUTF16` lives in herdr_test.go but exercises proc; ported as
  `test_decode_utf16` in the same place (it calls `proc::decode_utf16`).

### engine.rs
- `warnf`/`debugf` are no-ops: the scaffold Engine contract has no logger
  field (Go's `WithLogger`/slog), so discovery-failure warnings, fetch-failure
  and queue-drop debug lines are dropped — same silence as a Go Engine built
  without `WithLogger` (`src/engine.rs:1082-1089`).
- `events()`/`state_change()` return `&Mutex<mpsc::Receiver<..>>`, not the
  scaffold's `&mpsc::Receiver<..>`: std's Receiver is explicitly `!Sync`
  (`std/src/sync/mpsc.rs:191`), so a raw `&Receiver` cannot cross threads the
  way Go's receive-only channel can. No other module consumed these accessors
  (verified by grep); consumers lock + `recv()`/`try_recv()`.
- Poll-loop and `probe_once` Discover are bounded by the backend's configured
  timeout where Go bounds them with ctx only (per the herdr trait contract).
- `run()`'s stop wait polls the AtomicBool every 50ms instead of blocking on
  `ctx.Done()`; shutdown latency is bounded by ~50ms plus one in-flight fetch
  timeout.
- `Engine::new` drops Go's WithBackends-injection short-circuit (unreachable
  without builder Options); `with_backends` covers that path and, unlike Go,
  takes no WithLogger (see the logging deviation).
- `probe_once` keeps Go's double fetch per session (one for the error report,
  one inside poll_session) — faithful, deliberately kept.
- Tests-only: FakeBackend returns a default Snapshot for an unset session
  where Go's fake returns `(nil, nil)` (nil-deref crash in tracker.apply);
  tests always `set()` first.
- Go's `path.Match` is ported byte-for-byte (go1.26 path/match.go:
  scanChunk/matchChunk/getEsc + utf8.DecodeRuneInString semantics) so
  include/exclude globs behave identically, including ErrBadPattern patterns
  never matching.

### sink.rs + render.rs
- render: a minimal engine replicating only what the real templates use
  (example.toml + sinks): literal text, `{{.Field}}` (Go-capital fields of
  event.Event, including the `.Verb` method and `.Kind` as string) and
  `{{if}}{{else if}}{{else}}{{end}}` with Go truth (empty string/false=false,
  struct=true). Pipelines, functions, range/with, `{{- -}}` trimming, nested
  fields and method args are **parse errors** (Go would accept some).
  Unknown field is a runtime error and Render returns the raw spec (Go
  returns `t.Root.String()` — identical for the repo's templates). `.Time`
  prints RFC3339, not Go's `time.String()` layout (no real template prints
  it).
- popup: resolution order kept with the native path first — native
  (rune-notify, Windows only) → notify-send → powershell.exe (WSL interop; on
  native Windows powershell.exe → pwsh.exe like Go). The third line (extra /
  fleet summary) folds into the body with `\n` (as the notify-send path) because
  the rune-notify toast has no third text line; the per-severity logo does not
  exist on that path (the identity carries a fixed agent-tile icon (working-yellow prompt));
  per-severity logos remain on the PowerShell path (script byte-identical to
  Go, 1173 bytes verified). `Mode()` adds "native" to the Go values.
- Manager: slog → `eprintln!` to stderr ("sink queue full; dropping event",
  "sink delivery failed") because the scaffold Manager has no logger; workers
  poll the stop flag every 100ms (Go selects ctx.Done instantly) — only delays
  shutdown, not delivery. Deliver after stop: Go would buffer into the
  never-closed channel; Rust ignores Disconnected. `run()` twice: the second
  finds no receiver and does not duplicate workers (Go would).
- Command: Go's CombinedOutput interleaves stdout+stderr in one pipe; std has
  no shared pipe — captured separately and concatenated (same bytes, different
  interleaving in the error snippet). Exit-status text uses Go formatting
  ("exit status 3"; Rust's Display would say "exit status: 3") via
  `go_exit_status`.
- Log: Go's "15:04:05" is local time; std has no timezone — uses libc
  `localtime_r`/`localtime_s` (UTC fallback). JSON mode serializes with serde
  using the same field names as Go (guaranteed by event.rs).
- Webhook: ureq instead of net/http. Per-request timeout =
  min(manager deadline, configured timeout), equivalent to Go client.Timeout +
  ctx. Headers sent verbatim (Go canonicalizes e.g. x-token→X-Token; the repo
  tests use canonical names). ≥400 errors keep Go's format
  `"webhook {url}: HTTP {code} {status_text}"`.
- `toastImage` writes with `fs::write` (0666&umask, 0644 under a normal
  umask) where Go uses explicit 0o644.
- `build_sinks` (cli.buildSinks, cli.go:218, placed in sink.rs by the
  scaffold): signature extended with `eng: &Arc<Engine>` for fleetExtra as in
  Go; warnings (skipping tray sink / popups unavailable) go to stderr.

### setup/ + update.rs
- `uninstall` returns `(Vec<String>, anyhow::Result<()>)` instead of the
  scaffold's `Result<Vec<String>>`: Go returns `([]string, error)` and the
  caller prints the notes even on error; with a pure Result the notes would
  be lost.
- update: assets download via browser_download_url; GITHUB_TOKEN is only used
  for the release listing. go-selfupdate downloads via the GitHub API
  (asset id + Accept octet-stream) — identical result for public repos, and it
  avoids sending Authorization to the S3 redirect.
- `.xz/.tar.xz/.bz2` assets are rejected with an error (go-selfupdate supports
  them); no xz/bz2 decoder in the dependencies, and `.goreleaser.yml` only
  produces tar.gz/zip.
- semver hand-rolled, compatible with Masterminds v3 (v-prefix, missing parts
  = 0, metadata ignored, prerelease per spec); `Release.greater_than` returns
  false on unparseable input where Go would panic with MustParse; GOARM
  probing not ported (assets are amd64-only).
- Release listing: a single per_page=30 page — exactly what
  githubSource.ListReleases does in v1.6.0 (no pagination in Go).
- nudge-state: timestamp in RFC3339 UTC (`...Z`) vs Go's RFC3339 with local
  offset; the file is only checked for existence (NudgeDue), nothing re-reads
  it.
- CombinedOutput equivalents concatenate stdout+stderr (stdout first) instead
  of interleaving: runPowerShell, the systemctl error in
  platform_install_service, and install_wsl streaming (stderr over an mpsc
  channel between stdout reads).
- Windows registry errors format `WIN32_ERROR` as Debug (e.g.
  `WIN32_ERROR(2)`) instead of Go's FormatMessage text; registry Path entries
  with `%VAR%` are NOT expanded — same quirk as Go, whose os.ExpandEnv does
  not expand `%VAR%` either.
- Go path helpers (Clean/Dir/Base) ported with simplified Windows volume
  handling (drive letter, `\\?\` and `\\?\UNC\share` prefixes; deeper UNCs not
  modeled); `go_quote` passes non-ASCII through instead of escaping
  non-printable runes with `\u`.
- The `--status`/`--yes` flags, the wizard and the final popup test live in
  internal/cli/setup_cmd.go (cli module, ported by the cli portee); this
  module covers the whole internal/setup + internal/update surface the wizard
  consumes.

### tray.rs + flyout.rs
- See §2 for the workaround/decisions specific to these files. Additional
  quirks preserved bug-for-bug: `View.Counts` never increments blocked
  (view.go:48 — severity/summary never show "blocked" at fleet level,
  verified against the Go source; the per-session tally does, via
  RowSeverity); `SetUpdateKnown` stays fingerprint-gated (the item appears on
  the next view change); and the layout height estimate differs from the
  per-rowY height (both formulas ported as-is).
- The panel opens 1 frame at the AppSpec size (420x300) before poll resizes
  it to content (Go sized before ShowWindow).
- The agent-row hover is a 10% overlay in linear light ≈ Go's sRGB lift, not
  byte-exact (everything else in the palette is).
- Summary/menu tooltips of Go ("Live agent status" etc.) are lost — no
  tooltips on `TrayItem`.
- The same 420/14/170/300/24/14… constants are interpreted as logical points
  (rune is DPI-aware) where Go painted physical pixels at scale 1.

### cli.rs + main.rs + build.rs
- **No eng.Events()→mgr.Deliver bridge**, exactly like Go (PORT-NOTE in the
  module docs and cmd_tray/run/monitor): verified across history
  (`git log -S 'eng.Events()'` empty) and empirically — the Go binary
  `monitor --all` prints only the 2 log lines; engine events reach no sink in
  tray/run/monitor, only the synthetic ones (update/setup/test). Ported
  identically; if Go adds the pump, port it too.
- flytest: see §2.
- `attach_parent_console`: Go rebinds os.Stdout/Stderr/Stdin with
  GetStdHandle+os.NewFile; in Rust the std handles resolve lazily on first
  use, so `main()` attaches BEFORE `cli::run` and no rebinding is needed
  (documented in cli.rs and main.rs). Anything depending on the exe console
  twin (setuplaunch_windows.go): if `agent-notify-console.exe` is missing next
  to the binary it falls back to self-attaching the caller console, like Go.
- `cmd_version`: the compiler string comes from a new `build.rs`
  (`cargo:rustc-env=RUSTC_VERSION=` from `rustc --version`, fallback "rustc")
  where Go prints `runtime.Version()` baked in; GOOS/GOARCH mapped to Go
  spelling (darwin/amd64, not macos/x86_64).
- parse_flags errors print only the error line (Go also dumps "Usage of x:"
  with defaults); `-h` is ErrHelp → exit 2, same code.
- `cmd_monitor`: the log sink owns the process stdout (`Box<dyn Write + Send>`
  required by sink::Log::new); Go passes the command's writer — same fd in
  production, documented.
- `cmd_probe`: the 48-byte title truncation cuts at the last valid UTF-8 char
  boundary (Go cuts raw bytes and may split a rune).
- `cmd_update` without a signal ctx: ^C keeps its default (kills the process)
  instead of cancelling the 20s check as in Go.
- `slog_time` on Windows falls back to a `Z` suffix (MSVCRT's tm has no
  gmtoff); on unix it prints the local offset +01:00 like Go.
- `residency_mode` gains a `gui: bool` parameter (Go reads guiAvailable()
  inline) so the decision tree is testable without a display.
- setup plumbing: Go's cmd.Run() discards children's stdout/stderr; the
  systemctl calls in posix.rs and restart_daemon use `.output()` so the
  "enabled" line is not inherited/leaked.
- Signals without a new crate: `libc::sigaction` (SIGINT/SIGTERM, SA_RESTART)
  on unix and SetConsoleCtrlHandler on windows write to a static AtomicBool; a
  relay thread copies it into each command's `Arc<AtomicBool>` stop flag
  (handlers only touch async-signal-safe state).
- Windows-only cli helpers (console_windows, singleinstance, setuplaunch, tty)
  are documented as notes at the foot of cli.rs; no per-platform stubs.

### Scaffold-era notes that still apply
- `src/main.rs:13` carries `#![allow(dead_code)]` to silence the scaffold's
  ~170 dead-code warnings; narrow it as modules get wired (still in place).
- config.rs's unknown-key warnings (Go's `md.Undecoded()`) use a hand-rolled
  schema walker (`CONFIG_SCHEMA`/`SINK_SCHEMA`) that must be kept in sync with
  the structs — documented in the source.
- `parse_duration` parses negatives but saturates to 0 (`std::time::Duration`
  is unsigned); Go accepts them. Does not affect any real config value.
- event.rs keeps `Time{secs,nanos}` with hand-rolled RFC3339 (Hinnant's
  civil_from_days/days_from_civil) instead of chrono/time, per the dependency
  budget; the JSON shape matches Go (RFC3339 with trimmed fraction).
- herdr.rs `Local`: the Unix-socket check is cfg(unix); on other targets it
  returns false (irrelevant: local only applies on Linux/WSL/macOS).

## 4. Parity reviewer findings and status

Statuses from the parity review: **verified** = reproduced in the ported
source; **unconfirmed** = reported, not re-verified. None of the four is
fixed yet.

| # | Where | What | Severity | Status |
| --- | --- | --- | --- | --- |
| 1 | `src/flyout.rs:1339` (focus_agent), `src/tray.rs:243-245`, `src/cli.rs:1145` — nobody calls `FlyControl::set_focus_agent` in all of src/; `src/herdr.rs:513` (wsl_herdr_args) and Engine::session_socket are dead code | In Go, clickRow→focusAgent (flyout_windows.go:396-422) runs `herdr agent focus <pane>` (wsl.exe -e env PATH/HERDR_SOCKET_PATH, or herdr.exe --session, 5s timeout) then raiseHerdrWindow. In Rust the focus callback is never installed, so clicking an agent in the flyout only closes the panel and focuses nothing — a visible behavior loss. | high | **fixed (2026-11-12)** — `wire_focus_agent` is installed on every host (was `#[cfg(windows)]`); the raise is session-aware (see §5 step 7) |
| 2 | `src/engine.rs:1086-1088` (warnf/debugf no-ops) vs engine.go:318-328 with WithLogger wired in internal/cli/cli.go:185 | The Go engine logs "session discovery failed: …", "snapshot X failed: …" and "event queue full; dropping …"; the Rust engine never writes those warnings (logger removed from the API), losing discovery/fetch diagnostics. | low | unconfirmed |
| 3 | `src/proc.rs:174-203` (look_path) vs exec.LookPath/findExecutable (local.go:53, windows.go:35) | Go requires the exec bit when resolving binaries from PATH (Unix); Rust only checks is_file, so a present-but-not-+x binary enables the backend and fails every fetch (PermissionDenied → session drops after 3 polls) where Go would treat it as not-found and disable the backend. | low | unconfirmed |
| 4 | `src/proc.rs:97` vs proc.Run/exec.CommandContext (proc.go:26-38) | On timeout Go returns "signal: killed" (verified empirically with go1.26.3); Rust returns "context deadline exceeded" — different visible error text in `agent-notify probe` and sink-annotated errors. | low | unconfirmed |

## 5. How to test it in WSL2

Build and inspect (no display needed):

```sh
cargo build                     # binary at target/debug/agent-notify
./target/debug/agent-notify version
./target/debug/agent-notify probe          # one pass: sessions, agents, sink availability
./target/debug/agent-notify init           # write the annotated example config
./target/debug/agent-notify config validate
cargo test                                 # 88 passed / 1 ignored
cargo test -- --ignored smoke              # headless flyout render to PNG (needs a wgpu adapter)
```

Run the tray (needs WSLg and a panel that hosts StatusNotifierItem icons;
WSLg's system tray and most Linux panels do):

```sh
./target/debug/agent-notify             # default command = tray icon + attention popups
./target/debug/agent-notify test        # test notification through every configured sink
./target/debug/agent-notify flytest     # open/close the flyout once, logs visible=true/false
./target/debug/agent-notify setup --status
```

Manual checklist:

1. **Tray visible in WSLg** — icon appears; icon color follows severity
    (working yellow / waiting green / blocked red / down orange / idle gray).
2. **Tooltip** — shows the live summary (e.g. "1 working · 1 waiting"; "all
   quiet" when idle).
3. **Menu** — flat layout: summary item on top, session headers, agents as
   root rows prefixed `host/session · `, disabled (per decision #2).
4. **Icon churn** — while the blink is armed, expect the tray to be torn down
   and recreated ~2×/s on Linux (the WORKAROUND above); brief D-Bus flicker is
   normal, not a bug.
5. **Panel** — opens on tray primary click, centered; the title bar has
   min/max/close (close hides, the app keeps running; the tray reopens
   it); the user can resize/maximize freely (opening geometry applies
   once); content taller than the window scrolls (rune viewport thumb);
   collapsible session groups toggle compact rows.
6. **Test notification toast** — `agent-notify test` fires through every
   configured sink (notify-send on Linux/WSLg; the powershell.exe interop path
   if configured; the rune-notify native path is Windows-only).
7. **Click on an agent** — runs `herdr agent focus <pane>` (herdr.exe for
   windows-hosted sessions, wsl.exe + HERDR_SOCKET_PATH otherwise), then
   raises the client window. The raise is **session-aware**: with several
   terminals running herdr clients (a WSL-projects window, a pods
   window…), it prefers a visible window whose title names the clicked
   session (`pods`), falling back to the first "herdr"-titled one — the
   Go always took the first herdr window, which was often the wrong
   terminal. Windows runs the scan natively (EnumWindows); every other
   host (WSLg tray) runs the same scan over PowerShell interop
   (`-EncodedCommand`, C# EnumWindows payload; the script requires CRLF
   line endings — Windows PowerShell 5.1 rejects LF-only here-strings).
   Caveat: a client whose window title carries neither "herdr" nor the
   session name stays unfindable — same best-effort caveat as the Go
   ("a client hidden behind other tabs is not findable here").

## 6. Pending

- **Windows exe build** — never built; only checked. Exact probe result of
  `cargo check --target x86_64-pc-windows-gnu`: exit 0,
  `Finished \`dev\` profile [unoptimized + debuginfo] target(s) in 0.34s`
  (cached today; the full cross-compile ran green on the first attempt during
  the port rounds). No `.exe` was linked, and nothing has run on real Windows:
  the tray, the rune-notify native toast path, `register()`'s HKCU write and
  the `SPI_GETWORKAREA` flyout code are all uncompiled or unexercised.
- **macOS tray** — rune has no Tray on macOS (no tray backend in the vendored
  source; this is a toolkit limitation, not a port decision). The headless
  commands (run/monitor/probe/test) are unaffected; the tray+flyout experience
  cannot ship there until rune supports it.
- **Parity findings open** — #2 (engine logger no-ops), #3 (look_path exec
  bit), #4 (timeout error text). #1 (flyout agent focus never wired, high)
  is fixed: the callback is installed on every host and the raise prefers
  the clicked session's window (§5 step 7).
- **rune icon-update API** — `swap_icon` tray recreation is a workaround;
  revisit when rune lands an official set_icon/update_icon.
- **Render template engine decision** — which engine replaces Go
  text/template long-term is open; the current minimal engine errors on
  pipelines, functions, range/with, `{{- -}}` trimming and nested fields
  (fine for every template in the repo, documented in render.rs).
- **flytest introspection** — the Win32 SelfTest counters are not replicated
  (§2).
- **CLI engine→sink pump** — deliberately absent (faithful to Go); if Go adds
  `eng.Events()`→`mgr.Deliver`, port it (PORT-NOTE in cli.rs).
- **`cmd_update` ^C** — does not cancel the 20s update check (default process
  kill), unlike Go's signal ctx.
- **icon.rs ICO variant** — implemented byte-for-byte from Go's icoFrom but
  never verified against a real Windows LoadImage; the PNG and the RGBA
  rasters for `rune::TrayIcon` are verified.
- **`#![allow(dead_code)]`** in main.rs — still broad; narrow as modules stay
  wired (finding #1's focus path is wired again since 2026-11-12; the
  remaining dead code is shrinking).
- **Config schema walker** — `CONFIG_SCHEMA`/`SINK_SCHEMA` must be kept in
  sync with the structs by hand when the config grows.
