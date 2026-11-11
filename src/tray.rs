//! The tray sink: severity-colored icon, per-session/per-agent menu, the
//! attention blink, and hooks for update/setup actions. Port of
//! `internal/tray/tray.go` — scaffold: signatures only, bodies `todo!()`.
//!
//! RUNE MAPPING (verified against the vendored rune source at rev
//! 003a5c7): the rune `Tray` runs on its own thread and exposes
//! [`rune::Tray::new(TraySpec, on_event)`], [`rune::Tray::set_menu`],
//! [`rune::Tray::set_tooltip`]. THE ICON CANNOT BE UPDATED AFTER CREATION —
//! there is no set_icon; a severity change means dropping the Tray (which
//! removes the icon) and building a new one. `TrayItem::action(id, label)`
//! rows come back as `TrayEvent::Menu(id)`; the icon's primary click is
//! `TrayEvent::Activate { token }`. On Linux this needs rune's default
//! `dbus` feature (`Tray::new` returns `TrayError::Unsupported` without
//! it). macOS is unsupported by rune's Tray.
//!
//! The Go tray uses fyne systray with a full rebuild-on-change menu; the
//! rune port can keep the menu hot with set_menu, but the icon itself must
//! be recreated per severity change (or the blink re-thought).

use std::sync::Arc;

use crate::engine;
use crate::event::Event;

/// The tray sink (`tray.Tray`). Also carries the update and setup hooks the
/// Go tray wires through its menu.
pub struct Tray;

impl Tray {
    /// Builds the tray sink (`tray.New`, tray.go:71). `popup` is the
    /// attention-popup sink it triggers; None disables popups.
    pub fn new(
        engine: Arc<engine::Engine>,
        popup: Option<Arc<dyn crate::sink::Sink>>,
    ) -> Tray {
        let _ = (engine, popup);
        todo!("port of tray.New (tray.go:71) — rune Tray + refresh + blink loops")
    }

    /// Installs the update-menu hooks (`tray.SetUpdateHooks`, tray.go:76).
    pub fn set_update_hooks(&self, on_check: Box<dyn Fn() + Send + Sync>, on_restart: Box<dyn Fn() + Send + Sync>) {
        let _ = (on_check, on_restart);
        todo!("port of tray.SetUpdateHooks (tray.go:76)")
    }

    /// Marks the update availability known/unknown (enables the menu item)
    /// (`tray.SetUpdateKnown`, tray.go:82).
    pub fn set_update_known(&self, known: bool) {
        let _ = known;
        todo!("port of tray.SetUpdateKnown (tray.go:82)")
    }

    /// Installs the first-run setup hook (`tray.SetSetupHook`, tray.go:91).
    pub fn set_setup_hook(&self, on_setup: Box<dyn Fn() + Send + Sync>) {
        let _ = on_setup;
        todo!("port of tray.SetSetupHook (tray.go:91)")
    }

    /// Quits the whole app (`tray.QuitApp`).
    pub fn quit_app(&self) {
        todo!("port of tray.QuitApp (tray.go:96) — rune AppHandle::quit")
    }

    /// The sink name (`tray.Name`).
    pub fn name(&self) -> &'static str {
        "tray"
    }

    /// Receives events (sink half of [`Tray`]): updates severity, menu,
    /// tooltip, blink state (`tray.Deliver`, tray.go:102).
    pub fn deliver(&self, ev: &Event) -> anyhow::Result<()> {
        let _ = ev;
        todo!("port of tray.Deliver (tray.go:102)")
    }

    /// Starts the tray loops until `stop` is set: refresh (poll view),
    /// blink (`tray.Run`, tray.go:111).
    pub fn run(&self, stop: Arc<std::sync::atomic::AtomicBool>, on_test: Box<dyn Fn() + Send + Sync>) {
        let _ = (stop, on_test);
        todo!("port of tray.Run (tray.go:111)")
    }

    /// Launches the flyout window (`tray.InitFlyout`, tray.go:130) — see
    /// [`crate::flyout`].
    pub fn init_flyout(&self) {
        todo!("port of tray.InitFlyout (tray.go:130)")
    }
}

/// Stable structural key of a View: the menu rebuild only fires when this
/// changes (`tray.structuralKey`, tray.go:197).
pub fn structural_key(v: &engine::View) -> String {
    let _ = v;
    todo!("port of tray.structuralKey (tray.go:197)")
}

/// Whether the blink should be visible right now (`tray.shouldBlink`,
/// tray.go:254): the waiting severity pulses between the filled disc and
/// the ring variant ([`crate::icon::rgba_hollow`]).
pub fn should_blink(severity: &str, wait_till: Option<std::time::SystemTime>, now: std::time::SystemTime) -> bool {
    let _ = (severity, wait_till, now);
    todo!("port of tray.shouldBlink (tray.go:254)")
}
