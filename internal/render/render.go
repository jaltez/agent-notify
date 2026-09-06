// Package render turns events into notification titles and bodies using
// configurable Go text/templates.
package render

import (
	"bytes"
	"text/template"

	"agent-notify/internal/config"
	"agent-notify/internal/event"
)

// Renderer renders events through title and body templates.
type Renderer struct {
	title, body *template.Template
}

// New parses the given templates, falling back to the defaults when empty.
func New(titleSpec, bodySpec string) (*Renderer, error) {
	if titleSpec == "" {
		titleSpec = config.DefaultTitleTemplate
	}
	if bodySpec == "" {
		bodySpec = config.DefaultBodyTemplate
	}
	title, err := template.New("title").Parse(titleSpec)
	if err != nil {
		return nil, err
	}
	body, err := template.New("body").Parse(bodySpec)
	if err != nil {
		return nil, err
	}
	return &Renderer{title: title, body: body}, nil
}

// Render produces the notification title and body for an event. Template
// execution errors yield the raw spec and an error the caller may log.
func (r *Renderer) Render(ev event.Event) (title, body string) {
	return render(r.title, ev), render(r.body, ev)
}

func render(t *template.Template, ev event.Event) string {
	var buf bytes.Buffer
	if err := t.Execute(&buf, ev); err != nil {
		return t.Root.String()
	}
	return buf.String()
}
