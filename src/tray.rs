//! The tray sink: severity-colored icon, per-session/per-agent menu, the
//! attention blink, and hooks for update/setup actions. Port of
//! `internal/tray/tray.go` (fyne systray → rune's `Tray`).
//!
//! RUNE MAPPING (verified against the vendored rune source at rev 003a5c7):
//! rune's `Tray` runs on its own thread (SNI + dbusmenu over zbus on Linux,
//! `Shell_NotifyIcon` on Windows) and exposes [`rune::tray::Tray::new`],
//! `set_menu` and `set_tooltip`. Primary click → `TrayEvent::Activate`; a
//! menu pick → `TrayEvent::Menu(id)`.
//!
//! WORKAROUND(rune): recreate tray until official icon-update API lands —
//! `TraySpec` carries the icon and the built `Tray` has no `set_icon` (only
//! `set_menu`/`set_tooltip` exist), so every icon change (severity change
//! and blink phase) drops the old `Tray` (which removes the icon) and
//! builds a new one with the current menu and tooltip re-applied.
//!
//! PORT-NOTE (user decision #2): the Go menu nests agent rows under a
//! session header (`hdr.AddSubMenuItem`); rune's tray menu is flat only
//! (`TrayItem::Action` | `Separator`, no submenus — an explicit decision,
//! the flyout replaces them), so agent rows are top-level items prefixed
//! with their session.
//!
//! PORT-NOTE: the Go tray distinguishes a full rebuild (`rebuildMenu`) from
//! in-place title updates (`updateMenu`, `menuTree`) because Windows
//! destroys a popup menu reset while open. rune has no per-item `SetTitle`:
//! `set_menu` replaces the whole flat menu atomically (the Win32 backend
//! clones it per right-click, SNI bumps its revision and emits
//! `LayoutUpdated` — both safe while open), so every fingerprint change
//! goes through one `set_menu` call and `structural_key` only feeds the
//! fingerprint. `set_tooltip` stays an in-place update, as in the Go.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use rune::tray::{Tray as RuneTray, TrayEvent, TrayIcon, TrayItem, TraySpec};
use rune::{AnyWindowSpec, App, Display, RunOptions, WindowSpec};

use crate::engine::{self, Engine, View};
use crate::event::Event;
use crate::flyout::{FlyControl, FlyState, Flyout};
use crate::icon;

const ICON_SIZE: usize = 32;
/// Refresh debounce for bursts of state changes (`debounce`).
const DEBOUNCE: Duration = Duration::from_millis(300);
/// Blink cadence (`blinkEvery`).
const BLINK_EVERY: Duration = Duration::from_millis(500);
/// How long the icon blinks after the fleet enters waiting: long enough to
/// catch the eye from afar, short enough that an idle fleet settles instead
/// of crying wolf (`waitingBlinkWindow`).
const WAITING_BLINK_WINDOW: Duration = Duration::from_secs(2 * 60);
/// How often the wait loops re-check the stop flag (Go's `select` on
/// `ctx.Done()` is instant; this only bounds shutdown latency).
const STOP_POLL: Duration = Duration::from_millis(50);

const TRAY_ID: &str = "agent-notify";
const TRAY_TITLE: &str = "agent-notify";

/// An injectable action (the CLI plugs the real implementations in).
pub type HookFn = Box<dyn Fn() + Send + Sync>;
type Hook = Arc<dyn Fn() + Send + Sync>;

/// The update and setup hooks the Go tray wires through its menu
/// (`tray.go` Tray fields).
struct Hooks {
    on_setup: Option<Hook>,
    on_check_update: Option<Hook>,
    on_update_restart: Option<Hook>,
    update_known: bool,
}

/// State shared with the tray event handler, which must be `'static` (rune
/// owns it inside the native tray thread).
struct Shared {
    hooks: Mutex<Hooks>,
    on_test: Mutex<Option<Hook>>,
    stop: Mutex<Option<Arc<AtomicBool>>>,
    fly: Mutex<Option<FlyControl>>,
}

/// What the tray renders and what the refresh and blink loops share
/// (`tray.go` Tray's touched-from-refresh-goroutine fields, guarded by one
/// mutex: those loops are the only writers).
#[derive(Default)]
struct Inner {
    rune_tray: Option<RuneTray>,
    /// last rendered severity (the blink loop reads it, `tray.curSev`).
    cur_sev: Option<String>,
    /// waiting-blink deadline; None = solid (`tray.waitBlinkTill`).
    wait_blink_till: Option<SystemTime>,
    /// severity currently on the icon (skips no-op recreations).
    rendered_sev: Option<String>,
    /// rendered content fingerprint (`tray.lastFP`).
    last_fp: Option<String>,
    /// last applied menu (`tray.tree`'s payload; re-sent on recreation).
    menu: Vec<TrayItem>,
    /// last applied tooltip (`tray` has no read API to rebuild from).
    tooltip: String,
}

impl Inner {
    /// Times the waiting-blink window. Only a genuine "the fleet just
    /// stopped and wants a glance" transition arms it: working or blocked
    /// collapsing into waiting, the same moment the popup fires. Recovering
    /// from an offline session (down → waiting) or seeing the first idle
    /// agents at startup does not: those are not finish events, and poll
    /// flapping would otherwise re-arm the blink forever. The window is
    /// never cleared mid-flight; it just expires (`tray.armWaitBlink`).
    fn arm_wait_blink(&mut self, sev: &str) {
        if sev == "waiting"
            && let Some(prev) = &self.cur_sev
            && (prev == "working" || prev == "blocked")
        {
            self.wait_blink_till = Some(SystemTime::now() + WAITING_BLINK_WINDOW);
        }
    }
}

/// The tray sink (`tray.Tray`). Also carries the update and setup hooks the
/// Go tray wires through its menu, and the flyout panel it toggles.
pub struct Tray {
    eng: Arc<Engine>,
    pop: Option<Arc<dyn crate::sink::Sink>>,
    shared: Arc<Shared>,
    inner: Mutex<Inner>,
    /// coalesced "re-render the menu" signal (`tray.updateCh`, cap 1).
    update_tx: Mutex<mpsc::SyncSender<()>>,
    update_rx: Mutex<mpsc::Receiver<()>>,
    /// the windowless panel app, handed to the event loop by [`Tray::run`].
    fly_app: Mutex<Option<MainThreadApp>>,
    /// Serializes tray (re)creations: the refresh and blink loops both
    /// swap the icon, and the take → drop → build sequence must not
    /// interleave or two native trays (two icons) end up alive.
    swap: Mutex<()>,
}

/// The windowless panel app, parked between [`Tray::init_flyout`] and
/// [`Tray::run`].
///
/// SAFETY: `rune::App` is `!Send` because its `init` closure is boxed
/// without a `Send` bound, but this app never crosses threads: it is built
/// and consumed on the main thread only (the Go `InitFlyout` "before Run,
/// on the main OS thread" rule), and the loops [`Tray::run`] spawns touch
/// only the `Send + Sync` [`FlyControl`] handle. The wrapper exists so the
/// tray itself can be a `Send + Sync` sink.
struct MainThreadApp(App<FlyState>);
unsafe impl Send for MainThreadApp {}

impl Tray {
    /// Builds the tray sink (`tray.New`, tray.go:71). `popup` is the
    /// attention-popup sink it forwards events to; None disables popups
    /// (silent icon-only mode).
    pub fn new(engine: Arc<Engine>, popup: Option<Arc<dyn crate::sink::Sink>>) -> Tray {
        let (update_tx, update_rx) = mpsc::sync_channel(1);
        Tray {
            eng: engine,
            pop: popup,
            shared: Arc::new(Shared {
                hooks: Mutex::new(Hooks {
                    on_setup: None,
                    on_check_update: None,
                    on_update_restart: None,
                    update_known: false,
                }),
                on_test: Mutex::new(None),
                stop: Mutex::new(None),
                fly: Mutex::new(None),
            }),
            inner: Mutex::new(Inner::default()),
            update_tx: Mutex::new(update_tx),
            update_rx: Mutex::new(update_rx),
            fly_app: Mutex::new(None),
            swap: Mutex::new(()),
        }
    }

    /// Installs the update-menu hooks (`tray.SetUpdateHooks`, tray.go:76).
    /// Both optional: a None hides its menu item.
    pub fn set_update_hooks(
        &self,
        on_check: HookFn,
        on_restart: HookFn,
    ) {
        let mut h = self.shared.hooks.lock().unwrap();
        h.on_check_update = Some(Arc::from(on_check));
        h.on_update_restart = Some(Arc::from(on_restart));
    }

    /// Marks the update availability known/unknown: flips the
    /// "Update && restart" menu item and asks for a re-render
    /// (`tray.SetUpdateKnown`, tray.go:82).
    ///
    /// PORT-NOTE: like the Go, the coalesced signal only reaches
    /// [`Tray::render`], whose fingerprint gate early-returns when the view
    /// has not changed — so the menu item actually appears on the next view
    /// change that comes through, exactly as the Go does.
    pub fn set_update_known(&self, known: bool) {
        self.shared.hooks.lock().unwrap().update_known = known;
        let _ = self.update_tx.lock().unwrap().try_send(());
    }

    /// Installs the first-run setup hook (`tray.SetSetupHook`, tray.go:91).
    /// A None hides the menu item.
    pub fn set_setup_hook(&self, on_setup: HookFn) {
        self.shared.hooks.lock().unwrap().on_setup = Some(Arc::from(on_setup));
    }

    /// Quits the whole app (`tray.QuitApp`, tray.go:96): ends the rune
    /// event loop [`Tray::run`] blocks in.
    pub fn quit_app(&self) {
        if let Some(ctl) = self.shared.fly.lock().unwrap().clone() {
            ctl.quit();
        }
        if let Some(stop) = self.shared.stop.lock().unwrap().clone() {
            stop.store(true, Ordering::Relaxed);
        }
    }

    /// The sink name (`tray.Name`).
    pub fn name(&self) -> &'static str {
        "tray"
    }

    /// Receives events (`tray.Deliver`, tray.go:102): forwards to the
    /// popup sink when one is wired; the tray itself renders from the
    /// engine's view, not from events.
    pub fn deliver(&self, ev: &Event, deadline: Duration) -> anyhow::Result<()> {
        match &self.pop {
            Some(p) => p.deliver(ev, deadline),
            None => Ok(()),
        }
    }

    /// Launches the flyout panel (`tray.InitFlyout`, tray.go:130). Must run
    /// before [`Tray::run`] (the Go rule: window creation before the
    /// message loop); on success, tray left click toggles the panel while
    /// the right-click menu stays. Failure degrades to the plain tray menu.
    pub fn init_flyout(&self) {
        let mut slot = self.fly_app.lock().unwrap();
        if slot.is_none() {
            let f = Flyout::new(Arc::clone(&self.eng));
            *self.shared.fly.lock().unwrap() = Some(f.control());
            *slot = Some(MainThreadApp(f.into_app()));
        }
    }

    /// The panel's control handle, for diagnostics (`cli.cmdFlytest` drives
    /// the show/hide cycle like the Go `NewFlyoutForTest` bridge did).
    /// None until [`Tray::init_flyout`] ran.
    pub fn flyout_control(&self) -> Option<FlyControl> {
        self.shared.fly.lock().unwrap().clone()
    }

    /// Starts the tray UI and blocks until quit (`tray.Run`, tray.go:111):
    /// builds the panel and the native tray icon, renders the initial view,
    /// spawns the refresh and blink loops, then runs rune's event loop on
    /// the calling (main) thread. `stop` is the `ctx.Done()` analog.
    pub fn run(&self, stop: Arc<AtomicBool>, on_test: HookFn) {
        self.init_flyout();
        *self.shared.stop.lock().unwrap() = Some(Arc::clone(&stop));
        *self.shared.on_test.lock().unwrap() = Some(Arc::from(on_test));

        // Go onReady: starting icon + tooltip, then the first render.
        self.inner.lock().unwrap().tooltip = "agent-notify — starting…".to_string();
        self.swap_icon("idle", false);
        self.render();

        std::thread::scope(|scope| {
            scope.spawn(|| self.refresh_loop(&stop));
            scope.spawn(|| self.blink_loop(&stop));
            // The rune event loop owns this thread until quit, like
            // systray.Run owned the Go main goroutine.
            self.run_event_loop(&stop);
        });
        // Leaving the scope joined the loops; take the icon down at once
        // (Go: systray.Quit removes it via the tray loop's exit).
        let _ = self.inner.lock().unwrap().rune_tray.take();
    }

    /// Runs the panel's event loop on the calling thread. Without a panel
    /// app it parks until stop, matching the Go tray loop that outlives a
    /// failed `InitFlyout`.
    fn run_event_loop(&self, stop: &AtomicBool) {
        let app = self.fly_app.lock().unwrap().take().map(|m| m.0);
        let Some(app) = app else {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(STOP_POLL);
            }
            return;
        };
        let windows: Vec<AnyWindowSpec> = vec![
            WindowSpec::from_app("flyout", app)
                .options(crate::flyout::window_options())
                .into(),
        ];
        let ctl = self.shared.fly.lock().unwrap().clone();
        let result = rune::try_run_windows_with_args(RunOptions::default(), [], move |displays: &[Display]| {
            // Detect the work area once, before the first frame: SPI on
            // Windows, display bounds elsewhere (Linux is best-effort).
            let area = crate::flyout::work_area(displays);
            if let Some(ctl) = &ctl {
                ctl.apply_work_area(area);
            }
            windows
        });
        if let Err(err) = result {
            eprintln!("agent-notify: flyout event loop failed: {err}");
        }
    }

    /// Renders the view on state changes, debounced by [`refresh_loop`]
    /// (`tray.render`, tray.go:172).
    fn render(&self) {
        let v = self.eng.view();
        // armWaitBlink reads the previous severity before it is overwritten.
        {
            let mut g = self.inner.lock().unwrap();
            g.arm_wait_blink(v.severity());
            g.cur_sev = Some(v.severity().to_string());
        }
        let fp = fingerprint(&v);
        let mut need_icon = None;
        {
            let mut g = self.inner.lock().unwrap();
            if g.last_fp.as_deref() == Some(fp.as_str()) {
                return; // nothing view-relevant changed; keep the menu untouched
            }
            if g.rendered_sev.as_deref() != Some(v.severity()) {
                g.rendered_sev = Some(v.severity().to_string());
                need_icon = Some(v.severity());
            }
            g.tooltip = format!("agent-notify — {}", v.summary());
            g.menu = self.menu_items(&v);
            g.last_fp = Some(fp);
        }
        if let Some(sev) = need_icon {
            // WORKAROUND(rune): recreate tray until official icon-update API
            // lands (severity changed: the pixels differ, so must the Tray).
            self.swap_icon(sev, false);
        }
        self.apply_tooltip();
        self.apply_menu();
    }

    /// Rebuilds the whole flat menu and applies it (`tray.rebuildMenu` +
    /// `tray.updateMenu` collapse into one `set_menu`; see the PORT-NOTE in
    /// the module docs).
    fn menu_items(&self, v: &View) -> Vec<TrayItem> {
        let hooks = self.shared.hooks.lock().unwrap();
        let on_test = self.shared.on_test.lock().unwrap().is_some();

        let mut m = vec![
            TrayItem::action("summary", v.summary()).enabled(false),
            TrayItem::Separator,
        ];
        if v.sessions.is_empty() {
            m.push(TrayItem::action("no-sessions", "No sessions yet").enabled(false));
        }
        for s in &v.sessions {
            m.push(
                TrayItem::action(
                    format!("session:{}/{}", s.host, s.name),
                    session_label(s),
                )
                .enabled(false),
            );
            for a in &s.agents {
                m.push(
                    TrayItem::action(
                        format!("agent:{}/{}:{}", s.host, s.name, a.name),
                        format!("{}/{} · {}", s.host, s.name, agent_label(a)),
                    )
                    .enabled(false),
                );
            }
            if s.agents.is_empty() && s.up {
                m.push(
                    TrayItem::action(
                        format!("empty:{}/{}", s.host, s.name),
                        "(no agents)",
                    )
                    .enabled(false),
                );
            }
        }
        m.push(TrayItem::Separator);
        if hooks.on_setup.is_some() {
            m.push(TrayItem::action("setup", "Set up agent-notify…"));
        }
        if on_test {
            m.push(TrayItem::action("test", "Test notification"));
        }
        if hooks.on_check_update.is_some() {
            m.push(TrayItem::action("check-update", "Check for updates…"));
        }
        if hooks.update_known && hooks.on_update_restart.is_some() {
            m.push(TrayItem::action("update-restart", "Update && restart"));
        }
        m.push(TrayItem::action("quit", "Quit"));
        m
    }

    fn apply_tooltip(&self) {
        let g = self.inner.lock().unwrap();
        let tooltip = g.tooltip.clone();
        if let Some(t) = &g.rune_tray {
            t.set_tooltip(tooltip);
        }
    }

    fn apply_menu(&self) {
        let g = self.inner.lock().unwrap();
        let menu = g.menu.clone();
        if let Some(t) = &g.rune_tray {
            t.set_menu(menu);
        }
    }

    /// Destroys and rebuilds the native tray with `sev`'s icon — filled
    /// disc, or hollow ring when `hollow`. The new tray re-applies the
    /// current menu and tooltip (`tray.blinkLoop`/`tray.render`'s
    /// `systray.SetIcon`).
    // WORKAROUND(rune): recreate tray until official icon-update API lands.
    fn swap_icon(&self, sev: &str, hollow: bool) {
        let _serial = self.swap.lock().unwrap();
        let (menu, tooltip, old) = {
            let mut g = self.inner.lock().unwrap();
            (g.menu.clone(), g.tooltip.clone(), g.rune_tray.take())
        };
        // Drop the old tray outside the inner lock: dropping joins its
        // worker thread, which may be inside this very handler dispatch.
        drop(old);
        let pixels = if hollow {
            icon::rgba_hollow(sev, ICON_SIZE)
        } else {
            icon::rgba(sev, ICON_SIZE)
        };
        let spec = TraySpec::new(
            TRAY_ID,
            TRAY_TITLE,
            TrayIcon::rgba(ICON_SIZE as u32, ICON_SIZE as u32, pixels),
        )
        .tooltip(tooltip)
        .menu(menu);
        match RuneTray::new(spec, self.make_handler()) {
            Ok(t) => self.inner.lock().unwrap().rune_tray = Some(t),
            Err(err) => {
                eprintln!("agent-notify: tray icon unavailable: {err}");
            }
        }
    }

    /// The tray event handler, shared by every tray incarnation.
    fn make_handler(&self) -> impl Fn(TrayEvent) + Send + Sync + 'static {
        let shared = Arc::clone(&self.shared);
        let ctl = self.shared.fly.lock().unwrap().clone();
        move |ev| match ev {
            TrayEvent::Activate { token } => {
                // tray left click toggles the panel (tray.InitFlyout's
                // SetOnTapped); a Wayland activation token is passed through
                // for compositor-authorized focus.
                if let Some(ctl) = &ctl {
                    if ctl.is_visible() {
                        ctl.hide();
                    } else if let Some(token) = token {
                        ctl.activate(token);
                    } else {
                        ctl.show();
                    }
                }
            }
            TrayEvent::Menu(id) => dispatch(&shared, ctl.as_ref(), &id),
            _ => {} // TrayEvent is #[non_exhaustive]
        }
    }

    /// Renders the view on state changes, debouncing bursts
    /// (`tray.refreshLoop`, tray.go:141).
    fn refresh_loop(&self, stop: &AtomicBool) {
        let state_rx = self.eng.state_change();
        loop {
            // Go: select { ctx.Done | eng.StateChange | updateCh }.
            loop {
                if stop.load(Ordering::Relaxed) {
                    self.shutdown();
                    return;
                }
                match state_rx.lock().unwrap().recv_timeout(STOP_POLL) {
                    Ok(()) => break,
                    Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {}
                }
                match self.update_rx.lock().unwrap().recv_timeout(STOP_POLL) {
                    Ok(()) => break,
                    Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {}
                }
            }
            // Debounce: any further signal pushes the deadline out again
            // (Go: timer.Reset(debounce)).
            let mut deadline = Instant::now() + DEBOUNCE;
            loop {
                if stop.load(Ordering::Relaxed) {
                    self.shutdown();
                    return;
                }
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let remaining = deadline - now;
                match self.update_rx.lock().unwrap().recv_timeout(remaining) {
                    Ok(()) => {
                        deadline = Instant::now() + DEBOUNCE;
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {}
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match state_rx.lock().unwrap().recv_timeout(remaining) {
                    Ok(()) => deadline = Instant::now() + DEBOUNCE,
                    Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {}
                }
            }
            self.render();
            // fly.notify(): a repaint tick for the open panel
            if let Some(ctl) = self.shared.fly.lock().unwrap().clone() {
                ctl.notify();
            }
        }
    }

    /// Alternates the tray icon between the filled severity disc and a
    /// hollow ring while the fleet wants attention (`tray.blinkLoop`,
    /// tray.go:268). Steady otherwise.
    fn blink_loop(&self, stop: &AtomicBool) {
        let mut phase = false;
        loop {
            for _ in 0..(BLINK_EVERY.as_millis() / STOP_POLL.as_millis()) {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(STOP_POLL);
            }
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let (sev, wait_till) = {
                let g = self.inner.lock().unwrap();
                (g.cur_sev.clone(), g.wait_blink_till)
            };
            let sev = sev.unwrap_or_else(|| "idle".to_string());
            if !should_blink(&sev, wait_till, SystemTime::now()) {
                if phase {
                    // attention ended: settle back to the steady icon
                    phase = false;
                    if cfg!(target_os = "windows") {
                        self.apply_tooltip(); // undo the pulse's prefix
                    } else {
                        self.swap_icon(&sev, false);
                    }
                }
                continue;
            }
            phase = !phase;
            if cfg!(target_os = "windows") {
                // WORKAROUND(rune): recreating the tray at 2/s (drop = NIM
                // delete, new = NIM add) swallowed clicks and right-clicks
                // on Windows — the icon was briefly gone twice a second.
                // The pulse lives in the tooltip instead: set_tooltip is
                // in place, the icon keeps its severity color. Linux keeps
                // the icon blink (SNI churn is cheap there).
                let tip = {
                    let g = self.inner.lock().unwrap();
                    let dot = if phase { "●" } else { "○" };
                    format!("{dot} {}", g.tooltip)
                };
                let g = self.inner.lock().unwrap();
                if let Some(t) = &g.rune_tray {
                    t.set_tooltip(tip);
                }
            } else {
                self.swap_icon(&sev, !phase);
            }
        }
    }

    /// Ends the run on stop (Go's `<-ctx.Done(): systray.Quit()`).
    fn shutdown(&self) {
        self.quit_app();
    }
}

/// Routes a menu pick to its injected hook (`rebuildMenu`'s per-item
/// goroutines). Hooks are cloned out before they run: a hook may call back
/// into the tray (e.g. `set_update_known`) without deadlocking.
fn dispatch(shared: &Shared, ctl: Option<&FlyControl>, id: &str) {
    match id {
        "setup" => {
            let f = shared.hooks.lock().unwrap().on_setup.clone();
            if let Some(f) = f {
                f();
            }
        }
        "test" => {
            let f = shared.on_test.lock().unwrap().clone();
            if let Some(f) = f {
                f();
            }
        }
        "check-update" => {
            let f = shared.hooks.lock().unwrap().on_check_update.clone();
            if let Some(f) = f {
                f();
            }
        }
        "update-restart" => {
            let f = {
                let h = shared.hooks.lock().unwrap();
                if h.update_known {
                    h.on_update_restart.clone()
                } else {
                    None
                }
            };
            if let Some(f) = f {
                f();
            }
        }
        "quit" => {
            if let Some(ctl) = ctl {
                ctl.quit();
            }
            if let Some(stop) = shared.stop.lock().unwrap().clone() {
                stop.store(true, Ordering::Relaxed);
            }
        }
        _ => {}
    }
}

/// The sink half: the tray is in cli.buildSinks's list.
impl crate::sink::Sink for Tray {
    fn name(&self) -> &'static str {
        Tray::name(self)
    }

    fn deliver(&self, ev: &Event, deadline: Duration) -> anyhow::Result<()> {
        Tray::deliver(self, ev, deadline)
    }
}

/// `tray.sessionLabel` (tray.go:390).
fn session_label(s: &engine::SessionView) -> String {
    let mut label = format!("{}/{}", s.host, s.name);
    if !s.up {
        label += " · offline";
    }
    label
}

/// `tray.agentLabel` (tray.go:398).
fn agent_label(a: &engine::AgentView) -> String {
    let mut status = engine::row_severity(&a.status, a.wants);
    if a.status.is_empty() {
        status = "unknown";
    }
    let mut label = format!("{status} · {}", a.name);
    if !a.project.is_empty() {
        label += &format!(" — {}", a.project);
    }
    if a.focused {
        label += " ✱";
    }
    label
}

/// Stable structural key of a View: the menu rebuild only fires when this
/// changes (`tray.structuralKey`, tray.go:197). In rune it feeds the
/// fingerprint only (see the module PORT-NOTE on rebuild vs in-place).
pub fn structural_key(v: &View) -> String {
    let mut b = String::new();
    b.push_str(&v.sessions.len().to_string());
    for s in &v.sessions {
        b.push('|');
        b.push_str(&s.host);
        b.push('/');
        b.push_str(&s.name);
        b.push(':');
        b.push(if s.up { 'u' } else { 'd' });
        b.push_str(&s.agents.len().to_string());
    }
    b
}

/// Captures everything the tray renders; identical fingerprint ⇒ identical
/// pixels, skip the redraw (`tray.fingerprint`, tray.go:218).
fn fingerprint(v: &View) -> String {
    let mut b = String::new();
    b.push_str(v.severity());
    b.push('|');
    b.push_str(&v.summary());
    b.push('|');
    b.push_str(&structural_key(v));
    for s in &v.sessions {
        for a in &s.agents {
            b.push('|');
            b.push_str(&agent_label(a));
            b.push(' ');
            b.push_str(&a.title);
        }
    }
    b
}

/// Whether the blink should be visible right now (`tray.shouldBlink`,
/// tray.go:254). Blocked blinks until resolved; a stuck agent cannot unstick
/// itself. Waiting blinks only within its window after the fleet entered the
/// state.
pub fn should_blink(severity: &str, wait_till: Option<SystemTime>, now: SystemTime) -> bool {
    match severity {
        "blocked" => true,
        "waiting" => wait_till.is_some_and(|t| now < t),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_blink_matches_the_go_matrix() {
        let now = SystemTime::now();
        let in_a_minute = now + Duration::from_secs(60);
        let a_minute_ago = now - Duration::from_secs(60);
        let cases: [(&str, &str, Option<SystemTime>, bool); 7] = [
            ("blocked always blinks", "blocked", None, true),
            ("waiting inside window blinks", "waiting", Some(in_a_minute), true),
            ("waiting after window is solid", "waiting", Some(a_minute_ago), false),
            ("waiting never armed is solid", "waiting", None, false),
            ("working is solid", "working", Some(in_a_minute), false),
            ("idle is solid", "idle", Some(in_a_minute), false),
            ("down is solid", "down", Some(in_a_minute), false),
        ];
        for (name, sev, wait_till, want) in cases {
            assert_eq!(
                should_blink(sev, wait_till, now),
                want,
                "{name}: shouldBlink({sev:?}, {wait_till:?})"
            );
        }
    }

    #[test]
    fn arm_wait_blink_follows_the_go_matrix() {
        let mut tr = Inner::default();

        // Starting directly in waiting (agents idle before launch): solid.
        tr.arm_wait_blink("waiting");
        assert!(tr.wait_blink_till.is_none(), "startup-in-waiting must not arm the blink");

        // working → waiting: armed for the window.
        tr.arm_wait_blink("working");
        tr.cur_sev = Some("working".to_string());
        tr.arm_wait_blink("waiting");
        let till = tr.wait_blink_till.expect("entering waiting should arm the blink");
        let elapsed = till.duration_since(SystemTime::now()).unwrap_or_default();
        assert!(
            elapsed <= WAITING_BLINK_WINDOW + Duration::from_secs(1),
            "entering waiting should arm ~{WAITING_BLINK_WINDOW:?}, got {elapsed:?}"
        );

        // Still waiting: the running window is kept, not re-armed.
        tr.cur_sev = Some("waiting".to_string());
        std::thread::sleep(Duration::from_millis(20));
        tr.arm_wait_blink("waiting");
        assert_eq!(
            tr.wait_blink_till,
            Some(till),
            "persistent waiting must keep the original window"
        );

        // blocked → waiting (a blocked agent got unstuck and stopped): armed.
        tr.cur_sev = Some("blocked".to_string());
        tr.arm_wait_blink("waiting");
        let till2 = tr.wait_blink_till.expect("blocked → waiting should arm the blink");
        assert!(till2 >= till, "re-arm from blocked must move forward");

        // down → waiting (offline recovery): never re-arms, and the previous
        // window survives untouched; a poll flap must not reset the blink.
        tr.cur_sev = Some("waiting".to_string());
        std::thread::sleep(Duration::from_millis(20));
        tr.arm_wait_blink("down");
        tr.cur_sev = Some("down".to_string());
        tr.arm_wait_blink("waiting");
        assert_eq!(
            tr.wait_blink_till,
            Some(till2),
            "down → waiting must neither re-arm nor clear the window"
        );

        // idle → waiting (idle agents appeared): not a finish event.
        tr.cur_sev = Some("idle".to_string());
        tr.wait_blink_till = None;
        tr.arm_wait_blink("idle");
        tr.arm_wait_blink("waiting");
        assert!(tr.wait_blink_till.is_none(), "idle → waiting should not arm the blink");
    }

    #[test]
    fn structural_key_tracks_session_layout() {
        let mut v = View::default();
        assert_eq!(structural_key(&v), "0");
        v.sessions.push(engine::SessionView {
            host: "wsl".into(),
            name: "default".into(),
            up: true,
            agents: vec![engine::AgentView::default()],
        });
        v.sessions.push(engine::SessionView {
            host: "wsl".into(),
            name: "down".into(),
            up: false,
            agents: vec![],
        });
        assert_eq!(structural_key(&v), "2|wsl/default:u1|wsl/down:d0");
    }

    #[test]
    fn agent_and_session_labels_match_the_go() {
        let s = engine::SessionView {
            host: "wsl".into(),
            name: "default".into(),
            up: false,
            agents: vec![],
        };
        assert_eq!(session_label(&s), "wsl/default · offline");
        let mut a = engine::AgentView {
            name: "pi".into(),
            status: "working".into(),
            project: "Nervefall".into(),
            focused: true,
            ..engine::AgentView::default()
        };
        assert_eq!(agent_label(&a), "working · pi — Nervefall ✱");
        a.status = String::new();
        assert_eq!(agent_label(&a), "unknown · pi — Nervefall ✱");
    }

    /// The flat menu's shape: summary disabled on top, session headers
    /// disabled, agents flattened with a session prefix (user decision #2),
    /// then the action rows the hooks unhide.
    #[test]
    fn menu_items_follow_the_flat_layout() {
        let eng = Arc::new(Engine::with_backends(engine::Config::default(), vec![]));
        let tray = Tray::new(Arc::clone(&eng), None);
        let mut v = View::default();
        v.sessions.push(engine::SessionView {
            host: "wsl".into(),
            name: "default".into(),
            up: true,
            agents: vec![engine::AgentView {
                name: "pi".into(),
                status: "working".into(),
                title: "Refine shapes".into(),
                ..engine::AgentView::default()
            }],
        });
        let items = tray.menu_items(&v);
        let label = |i: usize| match &items[i] {
            TrayItem::Action { label, .. } => label.clone(),
            TrayItem::Separator => "<sep>".to_string(),
            _ => unreachable!(), // TrayItem is #[non_exhaustive]
        };
        assert_eq!(label(0), "1 working");
        assert!(matches!(items[1], TrayItem::Separator));
        assert_eq!(label(2), "wsl/default");
        assert_eq!(label(3), "wsl/default · working · pi");
        assert!(matches!(items[4], TrayItem::Separator));
        assert_eq!(label(5), "Quit");
        // every disabled row is disabled, Quit is enabled
        let disabled = |i: usize| match &items[i] {
            TrayItem::Action { enabled, .. } => !*enabled,
            _ => false, // separators and future variants
        };
        assert!(disabled(0) && disabled(2) && disabled(3));
        assert!(!disabled(5));
        // the update row only appears when known + hooked
        tray.set_update_hooks(Box::new(|| {}), Box::new(|| {}));
        let items = tray.menu_items(&v);
        let labels: Vec<String> = items
            .iter()
            .map(|i| match i {
                TrayItem::Action { label, .. } => label.clone(),
                TrayItem::Separator => "<sep>".to_string(),
                _ => unreachable!(),
            })
            .collect();
        assert!(labels.contains(&"Check for updates…".to_string()));
        assert!(!labels.contains(&"Update && restart".to_string()));
        tray.set_update_known(true);
        let labels: Vec<String> = tray
            .menu_items(&v)
            .iter()
            .map(|i| match i {
                TrayItem::Action { label, .. } => label.clone(),
                TrayItem::Separator => "<sep>".to_string(),
                _ => unreachable!(),
            })
            .collect();
        assert!(labels.contains(&"Update && restart".to_string()));
    }


    // PORT-NOTE: the full run() path (native tray + rune event loop + panel
    // window) cannot be exercised under `cargo test`: winit requires the
    // event loop on the main thread, and the test harness owns that thread.
    // The contract is that `Tray::run` runs on the main OS thread (the Go
    // cli pins it with runtime.LockOSThread; src/main.rs documents the same
    // rule). The headless PNG smoke in flyout::tests covers the view.

}
