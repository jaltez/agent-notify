//! Produces event titles and bodies from configured templates. Port of
//! `internal/render/render.go` — scaffold: signatures only, body `todo!()`.
//!
//! The Go side uses Go text/template. The Rust port must pick an equivalent
//! (a small hand-rolled template over [`crate::event::Event`] fields, or a
//! crate) — the config ships Go template syntax
//! ([`crate::config::DEFAULT_TITLE_TEMPLATE`]), so keep that syntax working.

use crate::event::Event;

/// Renders event titles and bodies (`render.Renderer`).
pub struct Renderer {
    /// the configured (or default) title/body specs, kept for diagnostics.
    pub title_spec: String,
    pub body_spec: String,
}

impl Renderer {
    /// Compiles the two templates; errors on bad specs
    /// (`render.New`, render.go:19).
    pub fn new(title_spec: &str, body_spec: &str) -> anyhow::Result<Renderer> {
        let _ = (title_spec, body_spec);
        todo!("port of render.New (render.go:19)")
    }

    /// Renders one event into (title, body) (`render.Render`, render.go:39).
    pub fn render(&self, ev: &Event) -> (String, String) {
        let _ = ev;
        todo!("port of render.Render (render.go:39)")
    }
}
