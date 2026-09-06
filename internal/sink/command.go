package sink

import (
	"bytes"
	"context"
	"fmt"
	"os/exec"
	"runtime"
	"text/template"
	"time"

	"agent-notify/internal/config"
	"agent-notify/internal/event"
)

// defaultCommandTimeout bounds a command sink execution when no explicit
// timeout is configured.
const defaultCommandTimeout = 10 * time.Second

// Command runs an external command per event. Each argv element is a Go
// text/template over the event. With Shell = true the single command
// string runs through the OS shell.
type Command struct {
	argv    []*template.Template
	shell   bool
	timeout time.Duration
}

// NewCommand builds the sink from config, parsing templates eagerly.
func NewCommand(cfg config.Sink) (*Command, error) {
	c := &Command{
		shell:   cfg.Shell,
		timeout: cfg.Timeout.D(),
	}
	if c.timeout <= 0 {
		c.timeout = defaultCommandTimeout
	}
	for _, a := range cfg.Command {
		t, err := template.New("arg").Parse(a)
		if err != nil {
			return nil, fmt.Errorf("command template: %w", err)
		}
		c.argv = append(c.argv, t)
	}
	return c, nil
}

func (c *Command) Name() string { return "command" }

func (c *Command) Deliver(ctx context.Context, ev event.Event) error {
	args := make([]string, len(c.argv))
	for i, t := range c.argv {
		var buf bytes.Buffer
		if err := t.Execute(&buf, ev); err != nil {
			return fmt.Errorf("command template: %w", err)
		}
		args[i] = buf.String()
	}

	cctx, cancel := context.WithTimeout(ctx, c.timeout)
	defer cancel()

	var cmd *exec.Cmd
	if c.shell {
		if runtime.GOOS == "windows" {
			cmd = exec.CommandContext(cctx, "cmd", "/c", args[0])
		} else {
			cmd = exec.CommandContext(cctx, "sh", "-c", args[0])
		}
	} else {
		cmd = exec.CommandContext(cctx, args[0], args[1:]...)
	}
	out, err := cmd.CombinedOutput()
	if err != nil {
		snip := string(out)
		if len(snip) > 200 {
			snip = snip[:200]
		}
		if snip == "" {
			return fmt.Errorf("run %q: %w", args[0], err)
		}
		return fmt.Errorf("run %q: %w: %s", args[0], err, snip)
	}
	return nil
}
