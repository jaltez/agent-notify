package sink

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"text/template"
	"time"

	"agent-notify/internal/config"
	"agent-notify/internal/event"
)

// defaultWebhookTimeout bounds a webhook request when no explicit timeout
// is configured.
const defaultWebhookTimeout = 5 * time.Second

// Webhook POSTs every event to an HTTP endpoint — the JSON event by
// default, or a rendered body_template for services with fixed payload
// shapes (Discord, Slack, ntfy, Home Assistant, ...).
type Webhook struct {
	url         string
	method      string
	headers     map[string]string
	contentType string
	bodyTmpl    *template.Template
	client      *http.Client
}

// NewWebhook builds the sink from config.
func NewWebhook(cfg config.Sink) (*Webhook, error) {
	timeout := cfg.Timeout.D()
	if timeout <= 0 {
		timeout = defaultWebhookTimeout
	}
	w := &Webhook{
		url:         cfg.URL,
		method:      cfg.Method,
		headers:     cfg.Headers,
		contentType: cfg.ContentType,
		client:      &http.Client{Timeout: timeout},
	}
	if w.method == "" {
		w.method = http.MethodPost
	}
	if cfg.BodyTemplate != "" {
		t, err := template.New("body").Parse(cfg.BodyTemplate)
		if err != nil {
			return nil, fmt.Errorf("webhook body_template: %w", err)
		}
		w.bodyTmpl = t
		if w.contentType == "" {
			w.contentType = "application/json"
		}
	}
	return w, nil
}

func (w *Webhook) Name() string { return "webhook" }

func (w *Webhook) Deliver(ctx context.Context, ev event.Event) error {
	var body []byte
	contentType := "application/json"
	if w.bodyTmpl != nil {
		var buf bytes.Buffer
		if err := w.bodyTmpl.Execute(&buf, ev); err != nil {
			return fmt.Errorf("webhook body_template: %w", err)
		}
		body = buf.Bytes()
		if w.contentType != "" {
			contentType = w.contentType
		}
	} else {
		data, err := json.Marshal(ev)
		if err != nil {
			return err
		}
		body = data
	}

	req, err := http.NewRequestWithContext(ctx, w.method, w.url, bytes.NewReader(body))
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", contentType)
	req.Header.Set("User-Agent", "agent-notify")
	for k, v := range w.headers {
		req.Header.Set(k, v)
	}
	resp, err := w.client.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode >= 400 {
		return fmt.Errorf("webhook %s: HTTP %s", w.url, resp.Status)
	}
	return nil
}
