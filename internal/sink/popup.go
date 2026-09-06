package sink

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"time"

	"agent-notify/internal/config"
	"agent-notify/internal/event"
	"agent-notify/internal/proc"
	"agent-notify/internal/render"
)

// defaultPowerShellAUMID makes Windows attribute toasts to PowerShell,
// which ships an AppUserModelID that needs no Start Menu shortcut.
const defaultPowerShellAUMID = `{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe`

// toastScript raises a Windows toast using the WinRT toast API. The title,
// body and AppUserModelID travel in environment variables so no quoting or
// escaping of user content is ever needed.
const toastScript = `
try {
  [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
  $x = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02)
  $t = $x.GetElementsByTagName('text')
  [void]$t.Item(0).AppendChild($x.CreateTextNode($env:AN_TITLE))
  [void]$t.Item(1).AppendChild($x.CreateTextNode($env:AN_BODY))
  $n = [Windows.UI.Notifications.ToastNotification]::new($x)
  [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($env:AN_APPID).Show($n)
} catch { Write-Error $_; exit 1 }
`

// popupTimeout bounds one popup command.
const popupTimeout = 15 * time.Second

// Popup shows desktop notifications: a Windows toast via PowerShell
// (native Windows, or from WSL through interop), or notify-send elsewhere.
type Popup struct {
	mode   string // "powershell" | "notify-send"
	binary string
	appID  string
	expire time.Duration
	dir    string
	r      *render.Renderer
}

// NewPopup resolves the popup mechanism. With no explicit Binary the order
// is: native Windows → PowerShell; otherwise notify-send, then
// powershell.exe via WSL interop. It errors when nothing is available.
func NewPopup(cfg config.Sink, r *render.Renderer) (*Popup, error) {
	p := &Popup{
		appID:  cfg.AppID,
		expire: cfg.Expire.D(),
		dir:    proc.InteropDir(),
		r:      r,
	}
	if p.appID == "" {
		p.appID = defaultPowerShellAUMID
	}
	if cfg.Binary != "" {
		path, err := exec.LookPath(cfg.Binary)
		if err != nil {
			return nil, fmt.Errorf("popup binary %q: %w", cfg.Binary, err)
		}
		p.binary = path
		if strings.Contains(filepath.Base(path), "notify-send") {
			p.mode = "notify-send"
		} else {
			p.mode = "powershell"
		}
		return p, nil
	}
	if runtime.GOOS == "windows" {
		for _, c := range []string{"powershell.exe", "pwsh.exe"} {
			if path, err := exec.LookPath(c); err == nil {
				p.binary, p.mode = path, "powershell"
				return p, nil
			}
		}
		return nil, fmt.Errorf("no PowerShell found in PATH for popups")
	}
	if path, err := exec.LookPath("notify-send"); err == nil {
		p.binary, p.mode = path, "notify-send"
		return p, nil
	}
	if path, err := exec.LookPath("powershell.exe"); err == nil {
		p.binary, p.mode = path, "powershell"
		return p, nil
	}
	return nil, fmt.Errorf("no popup backend found (need notify-send or powershell.exe in PATH)")
}

// Mode reports the resolved mechanism ("powershell" or "notify-send").
func (p *Popup) Mode() string { return p.mode }

func (p *Popup) Name() string { return "popup" }

func (p *Popup) Deliver(ctx context.Context, ev event.Event) error {
	title, body := p.r.Render(ev)
	cctx, cancel := context.WithTimeout(ctx, popupTimeout)
	defer cancel()

	if p.mode == "notify-send" {
		argv := []string{p.binary, "-a", "agent-notify"}
		if p.expire > 0 {
			argv = append(argv, "-t", fmt.Sprintf("%d", p.expire.Milliseconds()))
		}
		argv = append(argv, title, body)
		_, err := proc.Run(cctx, argv, nil, "")
		return err
	}

	env := append(os.Environ(),
		"AN_TITLE="+title,
		"AN_BODY="+body,
		"AN_APPID="+p.appID,
	)
	if runtime.GOOS != "windows" {
		// WSL interop only forwards variables listed in WSLENV to the
		// Windows process; add ours (preserving any existing entries).
		env = append(env, "WSLENV="+strings.Join(wslenvExtras(os.Getenv("WSLENV")), ""))
	}
	var stderr bytes.Buffer
	cmd := exec.CommandContext(cctx, p.binary,
		"-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
		"-Command", toastScript)
	cmd.Env = env
	cmd.Dir = p.dir
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		snip := strings.TrimSpace(stderr.String())
		if len(snip) > 300 {
			snip = snip[:300]
		}
		if snip == "" {
			return fmt.Errorf("toast: %w", err)
		}
		return fmt.Errorf("toast: %w: %s", err, snip)
	}
	return nil
}

// wslenvExtras builds the WSLENV suffix that forwards the toast payload
// variables across the WSL→Win32 boundary, preserving existing entries.
func wslenvExtras(existing string) []string {
	const vars = "AN_TITLE:AN_BODY:AN_APPID"
	if strings.Contains(existing, "AN_TITLE") {
		return []string{existing}
	}
	if existing == "" {
		return []string{vars}
	}
	return []string{existing + ":" + vars}
}
