//! Produces event titles and bodies from configured templates. Port of
//! `internal/render/render.go`.
//!
//! The Go side uses `text/template`. This port ships a minimal template
//! engine that replicates exactly the constructions the real templates use
//! (see `internal/config/example.toml` and the sink configs — everything the
//! shipped default config can express):
//!
//! - literal text
//! - `{{.Field}}` — one of the [`crate::event::Event`] fields (Go capital
//!   names), plus `.Verb` (a method in Go) and `.Kind` (prints its kind
//!   string, e.g. `agent_done`)
//! - `{{if .Field}}…{{else if .Field}}…{{else}}…{{end}}` — with Go's
//!   truthiness (empty string / false are false; any other field is true)
//!
//! Anything else (pipelines, functions, range/with, `{{-` trimming, method
//! arguments) is a **parse error** — Go would accept some of those. Missing
//! fields are an **execution** error, like Go; execution errors yield the
//! raw spec (Go's `t.Root.String()` fallback). Both limitations are listed
//! in the port deviations.

use crate::config;
use crate::event::Event;

// ---------------------------------------------------------------------------
// Template

/// One parsed template (`text/template.Template` narrowed to the supported
/// grammar).
pub struct Template {
    name: String,
    spec: String,
    nodes: Vec<Node>,
}

enum Node {
    Text(String),
    Field(String),
    If {
        field: String,
        then: Vec<Node>,
        els: Vec<Node>,
    },
}

impl Template {
    /// Parses a template spec (`template.New(name).Parse(spec)`). Errors on
    /// unclosed actions, mismatched if/end, or unsupported constructs.
    pub fn parse(name: &str, spec: &str) -> Result<Template, String> {
        let items = tokenize(spec).map_err(|msg| format!("template: {name}: {msg}"))?;
        let mut pos = 0;
        let nodes =
            build_list(&items, &mut pos, Stop::Top).map_err(|msg| format!("template: {name}: {msg}"))?;
        Ok(Template { name: name.to_string(), spec: spec.to_string(), nodes })
    }

    /// The original spec this template was parsed from.
    pub fn spec(&self) -> &str {
        &self.spec
    }

    /// The template name, for error messages.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Executes the template over `ev` (`t.Execute`). Unknown fields error
    /// like Go's `can't evaluate field X in type event.Event`.
    pub fn execute(&self, ev: &Event) -> Result<String, String> {
        let mut out = String::new();
        exec_nodes(&self.nodes, ev, &mut out)?;
        Ok(out)
    }

    /// Executes the template, returning the raw spec when execution fails —
    /// Go's `t.Root.String()` fallback (`render.go`, the inner `render`).
    pub fn render(&self, ev: &Event) -> String {
        self.execute(ev).unwrap_or_else(|_| self.spec.clone())
    }
}

// ---------------------------------------------------------------------------
// Parse: tokenize, then build the node tree

enum Item {
    Text(String),
    Field(String),
    If(String),
    Else,
    End,
}

/// Splits the spec into literal text and one item per `{{…}}` action.
fn tokenize(spec: &str) -> Result<Vec<Item>, String> {
    let mut items = Vec::new();
    let mut rest = spec;
    while let Some(open) = rest.find("{{") {
        if open > 0 {
            items.push(Item::Text(rest[..open].to_string()));
        }
        let after = &rest[open + 2..];
        let close = after
            .find("}}")
            .ok_or_else(|| "unclosed action".to_string())?;
        let action = after[..close].trim();
        if action == "else" {
            items.push(Item::Else);
        } else if action == "end" {
            items.push(Item::End);
        } else if let Some(cond) = action
            .strip_prefix("if")
            .filter(|rest| rest.starts_with(char::is_whitespace))
        {
            items.push(Item::If(parse_field_ref(cond.trim())?));
        } else if let Some(rest) = action
            .strip_prefix("else")
            .filter(|tail| tail.starts_with(char::is_whitespace))
        {
            // `else if .F` is one Go action: an else whose branch is the
            // nested if, sharing the same {{end}} — emit both items.
            let cond = rest.trim();
            let f = cond
                .strip_prefix("if")
                .filter(|tail| tail.starts_with(char::is_whitespace))
                .map(|tail| tail.trim())
                .ok_or_else(|| {
                    format!("unexpected \"{action}\" in else branch: this port supports else/if/else if/end")
                })?;
            items.push(Item::Else);
            items.push(Item::If(parse_field_ref(f)?));
        } else {
            items.push(Item::Field(parse_field_ref(action)?));
        }
        rest = &after[close + 2..];
    }
    if !rest.is_empty() {
        items.push(Item::Text(rest.to_string()));
    }
    Ok(items)
}

/// Validates a `.Field` reference, returning the field name.
fn parse_field_ref(action: &str) -> Result<String, String> {
    let name = action.strip_prefix('.').ok_or_else(|| {
        format!("unexpected \"{action}\": this port supports .Field, if/else/end over event fields")
    })?;
    if name.is_empty() {
        return Err("empty field reference".to_string());
    }
    let ident = name.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect::<String>();
    if ident.len() != name.len() {
        return Err(format!(
            "unexpected \".{name}\": this port supports plain .Field references only"
        ));
    }
    Ok(ident)
}

/// What `build_list` should stop at.
enum Stop {
    /// The whole template: runs to the end; `else`/`end` here are errors.
    Top,
    /// An if-branch body: stops at (and leaves) `else`/`end`; EOF is an error.
    Branch,
}

/// Builds nodes from `items[*pos]` until the stop condition (see [`Stop`]).
fn build_list(items: &[Item], pos: &mut usize, stop: Stop) -> Result<Vec<Node>, String> {
    let mut nodes = Vec::new();
    while *pos < items.len() {
        match &items[*pos] {
            Item::Text(s) => {
                nodes.push(Node::Text(s.clone()));
                *pos += 1;
            }
            Item::Field(f) => {
                nodes.push(Node::Field(f.clone()));
                *pos += 1;
            }
            Item::If(f) => {
                let f = f.clone();
                *pos += 1;
                nodes.push(build_if(items, pos, f)?);
            }
            Item::Else | Item::End => match stop {
                Stop::Top => {
                    return Err(format!(
                        "unexpected {{{{{}}}}} at top level",
                        match items[*pos] {
                            Item::Else => "else",
                            _ => "end",
                        }
                    ));
                }
                Stop::Branch => return Ok(nodes),
            },
        }
    }
    match stop {
        Stop::Top => Ok(nodes),
        Stop::Branch => Err("unexpected EOF: missing {{end}}".to_string()),
    }
}

/// Builds one `if` node: the caller consumed `{{if .F}}`; this consumes
/// through the matching `{{end}}` (shared across an `else if` chain, like
/// Go's parser desugars it).
fn build_if(items: &[Item], pos: &mut usize, field: String) -> Result<Node, String> {
    let then = build_list(items, pos, Stop::Branch)?;
    let mut els = Vec::new();
    match items.get(*pos) {
        Some(Item::End) => {
            *pos += 1;
        }
        Some(Item::Else) => {
            *pos += 1;
            if let Some(Item::If(f2)) = items.get(*pos) {
                // `else if`: the nested if is the else branch and shares the
                // same closing {{end}}.
                let f2 = f2.clone();
                *pos += 1;
                els.push(build_if(items, pos, f2)?);
            } else {
                els = build_list(items, pos, Stop::Branch)?;
                match items.get(*pos) {
                    Some(Item::End) => *pos += 1,
                    _ => return Err("unexpected EOF: missing {{end}}".to_string()),
                }
            }
        }
        _ => return Err("unexpected EOF: missing {{end}}".to_string()),
    }
    Ok(Node::If { field, then, els })
}

// ---------------------------------------------------------------------------
// Execute

/// A field value, mirroring what text/template can see on event.Event.
enum Val {
    S(String),
    B(bool),
    /// time.Time: a struct, so always truthy in Go templates.
    T(crate::event::Time),
}

/// Looks an (Go-capitalized) field up on the event; `None` is Go's
/// "can't evaluate field" case. `.Verb` is the Event method Go's template
/// machinery auto-calls.
fn field_value(ev: &Event, name: &str) -> Option<Val> {
    Some(match name {
        "Kind" => Val::S(ev.kind.as_str().to_string()),
        "Verb" => Val::S(ev.verb().to_string()),
        "Time" => Val::T(ev.time),
        "Source" => Val::S(ev.source.clone()),
        "Host" => Val::S(ev.host.clone()),
        "Session" => Val::S(ev.session.clone()),
        "Agent" => Val::S(ev.agent.clone()),
        "From" => Val::S(ev.from.clone()),
        "To" => Val::S(ev.to.clone()),
        "Title" => Val::S(ev.title.clone()),
        "Project" => Val::S(ev.project.clone()),
        "PaneID" => Val::S(ev.pane_id.clone()),
        "Focused" => Val::B(ev.focused),
        _ => return None,
    })
}

/// Go template truthiness: the empty string and false are false; a struct
/// (time.Time) is always true.
fn truthy(v: &Val) -> bool {
    match v {
        Val::S(s) => !s.is_empty(),
        Val::B(b) => *b,
        Val::T(_) => true,
    }
}

/// Renders the value the way fmt %v would (time.Time here prints RFC 3339,
/// not Go's String() layout; no shipped template prints .Time).
fn val_string(v: &Val) -> String {
    match v {
        Val::S(s) => s.clone(),
        Val::B(b) => b.to_string(),
        Val::T(t) => t.to_rfc3339(),
    }
}

fn eval(ev: &Event, field: &str) -> Result<Val, String> {
    field_value(ev, field)
        .ok_or_else(|| format!("can't evaluate field {field} in type event.Event"))
}

fn exec_nodes(nodes: &[Node], ev: &Event, out: &mut String) -> Result<(), String> {
    for n in nodes {
        match n {
            Node::Text(s) => out.push_str(s),
            Node::Field(f) => out.push_str(&val_string(&eval(ev, f)?)),
            Node::If { field, then, els } => {
                let branch = if truthy(&eval(ev, field)?) { then } else { els };
                exec_nodes(branch, ev, out)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Renderer (render.go)

/// Renders events through title and body templates (`render.Renderer`).
pub struct Renderer {
    title: Template,
    body: Template,
    /// the configured (or default) title/body specs, kept for diagnostics.
    pub title_spec: String,
    pub body_spec: String,
}

impl Renderer {
    /// Compiles the two templates, falling back to the defaults when empty;
    /// errors on bad specs (`render.New`, render.go:19).
    pub fn new(title_spec: &str, body_spec: &str) -> anyhow::Result<Renderer> {
        let title_spec =
            if title_spec.is_empty() { config::DEFAULT_TITLE_TEMPLATE } else { title_spec };
        let body_spec =
            if body_spec.is_empty() { config::DEFAULT_BODY_TEMPLATE } else { body_spec };
        let title = Template::parse("title", title_spec).map_err(anyhow::Error::msg)?;
        let body = Template::parse("body", body_spec).map_err(anyhow::Error::msg)?;
        Ok(Renderer {
            title,
            body,
            title_spec: title_spec.to_string(),
            body_spec: body_spec.to_string(),
        })
    }

    /// Produces the notification title and body for an event (`render.Render`,
    /// render.go:39). Template execution errors yield the raw spec, like the
    /// Go `t.Root.String()` fallback.
    pub fn render(&self, ev: &Event) -> (String, String) {
        (self.title.render(ev), self.body.render(ev))
    }
}

// ---------------------------------------------------------------------------
// Tests (port of internal/render/render_test.go)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Kind, Time};

    fn ev(kind: Kind) -> Event {
        Event {
            kind,
            time: Time::ZERO,
            source: String::new(),
            host: String::new(),
            session: String::new(),
            agent: String::new(),
            from: String::new(),
            to: String::new(),
            title: String::new(),
            project: String::new(),
            pane_id: String::new(),
            focused: false,
        }
    }

    /// TestDefaultTemplates
    #[test]
    fn default_templates() {
        let r = Renderer::new("", "").unwrap();
        let mut e = ev(Kind::AgentDone);
        e.host = "local".into();
        e.session = "work".into();
        e.agent = "claude".into();
        e.title = "Refactor auth module".into();
        e.project = "api".into();
        let (title, body) = r.render(&e);
        assert_eq!(title, "claude finished", "title");
        assert_eq!(body, "Refactor auth module", "body");

        // session event: no agent → session name; no title → project;
        // neither → host/session
        let mut e = ev(Kind::SessionDown);
        e.session = "work".into();
        e.host = "wsl".into();
        let (title, body) = r.render(&e);
        assert_eq!(title, "work went offline", "session title");
        assert_eq!(body, "wsl/work", "session body");

        let mut e = ev(Kind::AgentBlocked);
        e.session = "work".into();
        e.agent = "codex".into();
        e.project = "website".into();
        let (title, body) = r.render(&e);
        assert_eq!(title, "codex is blocked");
        assert_eq!(body, "website", "fallback body");
    }

    /// TestCustomTemplates
    #[test]
    fn custom_templates() {
        let r = Renderer::new("{{.Session}}: {{.Kind}}", "{{.Verb}} {{.Project}} {{.Focused}}")
            .unwrap();
        let mut e = ev(Kind::AgentIdle);
        e.session = "work".into();
        e.agent = "claude".into();
        e.project = "api".into();
        e.focused = true;
        let (title, body) = r.render(&e);
        assert_eq!(title, "work: agent_idle", "title");
        assert_eq!(body, "went idle api true", "body");
    }

    /// TestBadTemplateErrors
    #[test]
    fn bad_template_errors() {
        assert!(Renderer::new("{{.Nope", "").is_err(), "bad title template accepted");
        assert!(Renderer::new("", "{{.Missing").is_err(), "bad body template accepted");
    }

    /// else-if chains desugar like Go's parser (the chain shares one {{end}}).
    #[test]
    fn else_if_chain() {
        let t = Template::parse(
            "t",
            "{{if .Title}}{{.Title}}{{else if .Project}}{{.Project}}{{else}}{{.Host}}/{{.Session}}{{end}}",
        )
        .unwrap();
        let mut e = ev(Kind::AgentDone);
        assert_eq!(t.render(&e), "/");
        e.project = "api".into();
        assert_eq!(t.render(&e), "api");
        e.title = "T".into();
        assert_eq!(t.render(&e), "T");
    }

    /// Unknown fields parse but fail at execution → the raw spec comes back.
    #[test]
    fn unknown_field_falls_back_to_spec() {
        let t = Template::parse("t", "a {{.Nope}} b").unwrap();
        assert_eq!(t.render(&ev(Kind::Test)), "a {{.Nope}} b");
        assert!(t.execute(&ev(Kind::Test)).is_err());
    }

    /// Mismatched if/end are parse errors, like Go.
    #[test]
    fn mismatched_blocks_error() {
        assert!(Template::parse("t", "{{if .Agent}}x").is_err(), "missing end");
        assert!(Template::parse("t", "{{end}}").is_err(), "stray end");
        assert!(Template::parse("t", "{{else}}").is_err(), "stray else");
        assert!(Template::parse("t", "{{.Agent | printf}}").is_err(), "pipelines unsupported");
    }
}
