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
	"sync"
	"time"

	"agent-notify/internal/config"
	"agent-notify/internal/event"
	"agent-notify/internal/icon"
	"agent-notify/internal/proc"
	"agent-notify/internal/render"
)

// defaultPowerShellAUMID makes Windows attribute toasts to PowerShell,
// which ships an AppUserModelID that needs no Start Menu shortcut.
const defaultPowerShellAUMID = `{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe`

// toastScript raises a Windows toast using the WinRT toast API. The
// payload travels in environment variables so no quoting or escaping of
// user content is ever needed. AN_IMG selects the image+3-line template
// (Windows-native only, where the file path is Windows-visible);
// AN_EXTRA adds a third text line (space + fleet summary).
const toastScript = `
try {
  [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
  $tpl = [Windows.UI.Notifications.ToastTemplateType]::ToastText02
  if ($env:AN_IMG) { $tpl = [Windows.UI.Notifications.ToastTemplateType]::ToastImageAndText04 }
  elseif ($env:AN_EXTRA) { $tpl = [Windows.UI.Notifications.ToastTemplateType]::ToastText04 }
  $x = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent($tpl)
  if ($env:AN_IMG) {
    $img = $x.GetElementsByTagName('image').Item(0)
    [void]$img.SetAttribute('src', $env:AN_IMG)
    [void]$img.SetAttribute('placement', 'appLogoOverride')
    [void]$img.SetAttribute('crop', 'circle')
  }
  $t = $x.GetElementsByTagName('text')
  [void]$t.Item(0).AppendChild($x.CreateTextNode($env:AN_TITLE))
  [void]$t.Item(1).AppendChild($x.CreateTextNode($env:AN_BODY))
  if ($env:AN_EXTRA -and $t.Length -gt 2) { [void]$t.Item(2).AppendChild($x.CreateTextNode($env:AN_EXTRA)) }
  $n = [Windows.UI.Notifications.ToastNotification]::new($x)
  [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($env:AN_APPID).Show($n)
} catch { Write-Error $_; exit 1 }
`

// popupTimeout bounds one popup command.
const popupTimeout = 15 * time.Second

// toastImageSize is the logo disc size for toasts.
const toastImageSize = 96

// Popup shows desktop notifications: a Windows toast via PowerShell
// (native Windows, or from WSL through interop), or notify-send elsewhere.
type Popup struct {
	mode   string // "powershell" | "notify-send"
	binary string
	appID  string
	expire time.Duration
	dir    string
	image  bool // severity logo in toasts (native Windows only)
	r      *render.Renderer

	// Extra optionally supplies a third toast line (space + fleet
	// summary); wired by the CLI to the engine's live view.
	extra func(event.Event) string

	imgMu   sync.Mutex
	imgPath map[string]string // severity → rendered PNG path
}

// NewPopup resolves the popup mechanism. With no explicit Binary the order
// is: native Windows → PowerShell; otherwise notify-send, then
// powershell.exe via WSL interop. It errors when nothing is available.
func NewPopup(cfg config.Sink, r *render.Renderer) (*Popup, error) {
	p := &Popup{
		appID:   cfg.AppID,
		expire:  cfg.Expire.D(),
		dir:     proc.InteropDir(),
		r:       r,
		imgPath: map[string]string{},
	}
	if cfg.Image == nil || *cfg.Image {
		p.image = runtime.GOOS == "windows" // toast images need a Windows-visible path
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

// SetExtra wires the optional third toast line (space + fleet summary).
func (p *Popup) SetExtra(fn func(event.Event) string) { p.extra = fn }

// Mode reports the resolved mechanism ("powershell" or "notify-send").
func (p *Popup) Mode() string { return p.mode }

func (p *Popup) Name() string { return "popup" }

func (p *Popup) Deliver(ctx context.Context, ev event.Event) error {
	title, body := p.r.Render(ev)
	extra := ""
	if p.extra != nil {
		extra = p.extra(ev)
	}
	cctx, cancel := context.WithTimeout(ctx, popupTimeout)
	defer cancel()

	if p.mode == "notify-send" {
		if extra != "" {
			body += "\n" + extra
		}
		argv := []string{p.binary, "-a", "agent-notify"}
		if p.expire > 0 {
			argv = append(argv, "-t", fmt.Sprintf("%d", p.expire.Milliseconds()))
		}
		argv = append(argv, title, body)
		_, err := proc.Run(cctx, argv, nil, "")
		return err
	}

	img := ""
	if p.image {
		img = p.toastImage(kindSeverity(ev.Kind))
	}
	cmd := exec.CommandContext(cctx, p.binary,
		"-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
		"-Command", toastScript)
	cmd.SysProcAttr = proc.ChildSysProcAttr() // windows: no console flash
	cmd.Env = p.toastEnv(title, body, extra, img)
	cmd.Dir = p.dir
	var stderr bytes.Buffer
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

// toastEnv builds the child process environment. Under WSL interop only
// WSLENV-listed variables cross the boundary, so ours are added there.
func (p *Popup) toastEnv(title, body, extra, img string) []string {
	env := append(os.Environ(),
		"AN_TITLE="+title,
		"AN_BODY="+body,
		"AN_APPID="+p.appID,
	)
	if extra != "" {
		env = append(env, "AN_EXTRA="+extra)
	}
	if img != "" {
		env = append(env, "AN_IMG="+img)
	}
	if runtime.GOOS != "windows" {
		env = append(env, "WSLENV="+mergeWSLENV(os.Getenv("WSLENV"),
			"AN_TITLE:AN_BODY:AN_APPID:AN_EXTRA:AN_IMG"))
	}
	return env
}

func mergeWSLENV(existing, extra string) string {
	if existing == "" {
		return extra
	}
	return existing + ":" + extra
}

// kindSeverity maps an event kind to the icon color it represents.
func kindSeverity(k event.Kind) string {
	switch k {
	case event.KindAgentBlocked:
		return "blocked"
	case event.KindAgentWorking:
		return "working"
	case event.KindAgentIdle, event.KindAgentDone:
		return "waiting"
	case event.KindSessionDown:
		return "down"
	case event.KindTest:
		return "working"
	default:
		return "idle"
	}
}

// toastImage renders (and caches) the severity logo as a PNG in the temp
// directory. Only called on native Windows, where os.TempDir is a
// Windows-visible path the toast can read.
func (p *Popup) toastImage(severity string) string {
	sev := severity
	p.imgMu.Lock()
	defer p.imgMu.Unlock()
	if path, ok := p.imgPath[sev]; ok {
		return path
	}
	path := filepath.Join(os.TempDir(), "agent-notify-toast-"+sev+".png")
	if err := os.WriteFile(path, icon.PNG(icon.Color(sev), toastImageSize), 0o644); err != nil {
		return ""
	}
	p.imgPath[sev] = path
	return path
}
