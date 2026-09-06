//go:build windows

package tray

import (
	"fmt"
	"log/slog"
	"os"
	"time"
	"unicode/utf16"
	"unsafe"

	"golang.org/x/sys/windows"

	"agent-notify/internal/engine"
	"agent-notify/internal/icon"
)

// A small always-on-top flyout panel shown next to the tray on left
// click: fleet summary, one block per herdr space, one row per agent.
// It hides itself when deactivated (click outside), on Escape, and after
// autoCloseDelay. It lives on the systray message thread and paints with
// GDI — no extra processes, no console windows.
//
// The engine view is read on the UI thread inside WM_PAINT; engine.View()
// takes a short-held mutex, so this is safe.

const (
	autoCloseDelay = 8 * time.Second
	refreshEvery   = time.Second

	flyWidth     = 360
	flyPad       = 14
	sectionH     = 24
	rowH         = 21
	sepAfterSum  = 8
	maxRowsSpace = 8
	flyClassName = "agent-notify-flyout"

	wmAppToggle    = 0x8001 // WM_APP+1
	wmAppRefresh   = 0x8002 // WM_APP+2
	timerAutoclose = 1
	timerRefresh   = 2
)

// COLORREF values (0x00BBGGRR).
const (
	colorBG     = 0x00202020
	colorBorder = 0x00453C3C
	colorText   = 0x00F2F2F2
	colorDim    = 0x00A0A0A0
)

var (
	user32   = windows.NewLazySystemDLL("user32.dll")
	gdi32    = windows.NewLazySystemDLL("gdi32.dll")
	kernel32 = windows.NewLazySystemDLL("kernel32.dll")

	procRegisterClassExW      = user32.NewProc("RegisterClassExW")
	procCreateWindowExW       = user32.NewProc("CreateWindowExW")
	procDefWindowProcW        = user32.NewProc("DefWindowProcW")
	procSetWindowPos          = user32.NewProc("SetWindowPos")
	procSetForegroundWindow   = user32.NewProc("SetForegroundWindow")
	procBeginPaint            = user32.NewProc("BeginPaint")
	procEndPaint              = user32.NewProc("EndPaint")
	procFillRect              = user32.NewProc("FillRect")
	procCreateSolidBrush      = gdi32.NewProc("CreateSolidBrush")
	procDeleteObject          = gdi32.NewProc("DeleteObject")
	procCreateFontIndirectW   = gdi32.NewProc("CreateFontIndirectW")
	procSelectObject          = gdi32.NewProc("SelectObject")
	procSetBkMode             = gdi32.NewProc("SetBkMode")
	procSetTextColor          = gdi32.NewProc("SetTextColor")
	procDrawTextW             = gdi32.NewProc("DrawTextW")
	procEllipse               = gdi32.NewProc("Ellipse")
	procCreateRoundRectRgn    = gdi32.NewProc("CreateRoundRectRgn")
	procSetWindowRgn          = user32.NewProc("SetWindowRgn")
	procSystemParametersInfoW = user32.NewProc("SystemParametersInfoW")
	procSetTimer              = user32.NewProc("SetTimer")
	procKillTimer             = user32.NewProc("KillTimer")
	procShowWindow            = user32.NewProc("ShowWindow")
	procFrameRect             = gdi32.NewProc("FrameRect")
	procGetModuleHandleW      = kernel32.NewProc("GetModuleHandleW")
	procGetWindowLongPtrW     = user32.NewProc("GetWindowLongPtrW")
	procPostMessageW          = user32.NewProc("PostMessageW")
	procInvalidateRect        = user32.NewProc("InvalidateRect")
)

type RECT struct {
	Left, Top, Right, Bottom int32
}

// PAINTSTRUCT mirrors the Win32 layout exactly (RCPaint offset matters).
type PAINTSTRUCT struct {
	HDC        windows.Handle
	FErase     int32
	RCPaint    RECT
	FRestore   int32
	FIncUpdate int32
	Reserved   [32]byte
}

type WNDCLASSEX struct {
	Size       uint32
	Style      uint32
	WndProc    uintptr
	ClsExtra   int32
	WndExtra   int32
	Instance   windows.Handle
	Icon       windows.Handle
	Cursor     windows.Handle
	Background windows.Handle
	MenuName   *uint16
	ClassName  *uint16
	IconSm     windows.Handle
}

type flyout struct {
	eng     *engine.Engine
	log     *slog.Logger
	hwnd    windows.HWND
	visible bool
	// activated tracks whether we actually hold foreground: the
	// deactivate-hide (click outside) may only fire when we had it,
	// otherwise a lost foreground race hides the panel instantly.
	activated bool
	font      windows.Handle
	bold      windows.Handle
}

var currentFly *flyout

func flyWndProc(hwnd windows.HWND, msg uint32, wParam, lParam uintptr) uintptr {
	// A panic here would take the whole tray process down (the callback
	// crosses the Win32 boundary); degrade to a visual glitch instead.
	defer func() { _ = recover() }()
	f := currentFly
	if f == nil {
		// messages during window creation arrive before the instance
		// is wired up — WM_NCCREATE must reach DefWindowProc
		ret, _, _ := procDefWindowProcW.Call(uintptr(hwnd), uintptr(msg), wParam, lParam)
		return ret
	}
	switch msg {
	case wmAppToggle:
		f.toggle()
		return 0
	case wmAppRefresh:
		if f.visible {
			procInvalidateRect.Call(uintptr(f.hwnd), 0, 1)
		}
		return 0
	case 0x0006: // WM_ACTIVATE
		if uint16(wParam) == 0 { // WA_INACTIVE — click landed outside
			if f.activated {
				f.activated = false
				f.hide()
			}
		} else {
			f.activated = true
		}
	case 0x0100: // WM_KEYDOWN
		if wParam == 0x1B { // VK_ESCAPE
			f.hide()
			return 0
		}
	case 0x000F: // WM_PAINT
		f.paint(hwnd)
		return 0
	case 0x0113: // WM_TIMER
		switch wParam {
		case timerAutoclose:
			f.hide()
		case timerRefresh:
			procInvalidateRect.Call(uintptr(f.hwnd), 0, 1)
		}
	case 0x0010: // WM_CLOSE
		f.hide()
		return 0
	}
	ret, _, _ := procDefWindowProcW.Call(uintptr(hwnd), uintptr(msg), wParam, lParam)
	return ret
}

// newFlyout registers the class and creates the hidden panel. Call from
// the systray message thread (onReady). Failure degrades to the plain
// tray menu; the error says why.
func newFlyout(eng *engine.Engine, log *slog.Logger) (*flyout, error) {
	inst, _, _ := procGetModuleHandleW.Call(0)
	if inst == 0 {
		return nil, fmt.Errorf("GetModuleHandleW failed")
	}
	cls, err := windows.UTF16PtrFromString(flyClassName)
	if err != nil {
		return nil, err
	}
	cb := windows.NewCallback(flyWndProc)
	wc := WNDCLASSEX{
		Size:      uint32(unsafe.Sizeof(WNDCLASSEX{})),
		WndProc:   cb,
		Instance:  windows.Handle(inst),
		ClassName: cls,
	}
	if r, _, err2 := procRegisterClassExW.Call(uintptr(unsafe.Pointer(&wc))); r == 0 {
		return nil, fmt.Errorf("RegisterClassExW: %v", err2)
	}
	f := &flyout{eng: eng, log: log}
	f.font = createFont(-15, 400)
	f.bold = createFont(-15, 600)
	title, err := windows.UTF16PtrFromString("agent-notify")
	if err != nil {
		return nil, err
	}
	const wsPopup = 0x80000000
	hwnd, _, callErr := procCreateWindowExW.Call(
		0x00000080|0x00000020, // WS_EX_TOPMOST | WS_EX_TOOLWINDOW
		uintptr(unsafe.Pointer(cls)),
		uintptr(unsafe.Pointer(title)),
		uintptr(wsPopup),
		0, 0, 0, 0,
		0, 0, inst, 0)
	if hwnd == 0 {
		return nil, fmt.Errorf("CreateWindowExW: %v", callErr)
	}
	f.hwnd = windows.HWND(hwnd)
	currentFly = f
	if log != nil {
		const gwlplWndProc = ^uintptr(3) // GWLP_WNDPROC (-4)
		got, _, _ := procGetWindowLongPtrW.Call(uintptr(f.hwnd), gwlplWndProc)
		log.Debug(fmt.Sprintf("flyout created: pid=%d hwnd=%v callback=%#x wndproc=%#x",
			os.Getpid(), f.hwnd, cb, got))
	}
	return f, nil
}

// toggle shows or hides the panel (tray left click).
func (f *flyout) toggle() {
	f.debugf("flyout toggle: visible=%v", f.visible)
	if f.visible {
		f.hide()
		return
	}
	f.show()
}

func (f *flyout) show() {
	_, h := f.rowsAndHeight()
	var area RECT
	const spiGetWorkArea = 0x0030
	if r, _, err := procSystemParametersInfoW.Call(spiGetWorkArea, 0, uintptr(unsafe.Pointer(&area)), 0); r == 0 {
		f.debugf("flyout show: work area query failed: %v", err)
	}
	w := int32(flyWidth)
	x := area.Right - w - 12
	y := area.Bottom - h - 8
	rgn, _, _ := procCreateRoundRectRgn.Call(0, 0, uintptr(w+1), uintptr(h+1), 14, 14)
	procSetWindowRgn.Call(uintptr(f.hwnd), rgn, 1)
	const (
		hwndTopmost   = ^uintptr(0) // HWND_TOPMOST (-1)
		swpShowWindow = 0x0040
	)
	if r, _, err := procSetWindowPos.Call(uintptr(f.hwnd), hwndTopmost,
		uintptr(x), uintptr(y), uintptr(w), uintptr(h),
		swpShowWindow); r == 0 {
		f.debugf("flyout show: SetWindowPos failed: %v", err)
	}
	if r, _, _ := procSetForegroundWindow.Call(uintptr(f.hwnd)); r != 0 {
		f.activated = true
	} else {
		f.activated = false // foreground denied: rely on auto-close/toggle
	}
	procSetTimer.Call(uintptr(f.hwnd), timerAutoclose, uintptr(autoCloseDelay.Milliseconds()), 0)
	procSetTimer.Call(uintptr(f.hwnd), timerRefresh, uintptr(refreshEvery.Milliseconds()), 0)
	f.visible = true
}

func (f *flyout) hide() {
	if !f.visible {
		return
	}
	f.debugf("flyout hide")
	f.activated = false
	procKillTimer.Call(uintptr(f.hwnd), timerAutoclose)
	procKillTimer.Call(uintptr(f.hwnd), timerRefresh)
	procShowWindowCall(f.hwnd, 0) // SW_HIDE
	f.visible = false
}

func (f *flyout) debugf(format string, args ...any) {
	if f.log != nil {
		f.log.Debug(fmt.Sprintf(format, args...))
	}
}

func procShowWindowCall(hwnd windows.HWND, cmd int32) {
	procShowWindow.Call(uintptr(hwnd), uintptr(uint32(cmd)))
}

// colorRef renders an icon color in Windows COLORREF (BGR) layout.
func colorRef(c icon.RGB) uint32 {
	return uint32(c.R) | uint32(c.G)<<8 | uint32(c.B)<<16
}

// notify asks for a repaint from any goroutine (thread-safe post).
func (f *flyout) notify() {
	procPostMessageW.Call(uintptr(f.hwnd), wmAppRefresh, 0, 0)
}

type flyRow struct {
	dot  uint32 // COLORREF; 0 = no dot
	text string
	bold bool
	dim  bool
}

func (f *flyout) rows() []flyRow {
	v := f.eng.View()
	rows := []flyRow{
		{dot: colorRef(icon.Color(v.Severity())), text: "agent-notify", bold: true},
		{dot: 0, text: v.Summary(), dim: true},
	}
	if len(v.Sessions) == 0 {
		rows = append(rows, flyRow{dot: 0, text: "No herdr sessions seen yet — start a herdr session", dim: true})
		return rows
	}
	for _, s := range v.Sessions {
		state := ""
		if !s.Up {
			state = " · offline"
		}
		rows = append(rows, flyRow{
			dot:  0,
			text: fmt.Sprintf("%s/%s — %d agent%s%s", s.Host, s.Name, len(s.Agents), pluralS(len(s.Agents)), state),
			bold: true,
		})
		for i, a := range s.Agents {
			if i >= maxRowsSpace {
				rows = append(rows, flyRow{dot: 0, text: fmt.Sprintf("… and %d more", len(s.Agents)-maxRowsSpace), dim: true})
				break
			}
			text := fmt.Sprintf("%s · %s", a.Name, orDash(a.Project))
			if a.Title != "" {
				text += " — " + a.Title
			}
			rows = append(rows, flyRow{dot: colorRef(icon.Color(a.Status)), text: text})
		}
	}
	return rows
}

// rowsAndHeight returns the rows to paint and the matching panel height.
func (f *flyout) rowsAndHeight() ([]flyRow, int32) {
	rows := f.rows()
	h := int32(flyPad)
	for i, r := range rows {
		rh := int32(rowH)
		if r.bold {
			rh = sectionH
		}
		if i == 1 {
			rh += sepAfterSum
		}
		h += rh
	}
	return rows, h + flyPad
}

func pluralS(n int) string {
	if n == 1 {
		return ""
	}
	return "s"
}

func orDash(s string) string {
	if s == "" {
		return "—"
	}
	return s
}

func (f *flyout) paint(hwnd windows.HWND) {
	var ps PAINTSTRUCT
	hdc, _, _ := procBeginPaint.Call(uintptr(hwnd), uintptr(unsafe.Pointer(&ps)))
	if hdc == 0 {
		return
	}
	defer procEndPaint.Call(uintptr(hwnd), uintptr(unsafe.Pointer(&ps)))

	rows, h := f.rowsAndHeight()
	bg, _, _ := procCreateSolidBrush.Call(uintptr(colorBG))
	defer procDeleteObject.Call(bg)
	border, _, _ := procCreateSolidBrush.Call(uintptr(colorBorder))
	defer procDeleteObject.Call(border)
	frame := RECT{0, 0, flyWidth, h}
	procFrameRectCall(hdc, frame, border)
	client := RECT{1, 1, flyWidth - 1, h - 1}
	procFillRect.Call(hdc, uintptr(unsafe.Pointer(&client)), bg)

	procSetBkMode.Call(hdc, 1) // TRANSPARENT

	y := int32(flyPad)
	for _, r := range rows {
		rh := int32(rowH)
		if r.bold {
			rh = sectionH
		}
		if r.dot != 0 {
			brush, _, _ := procCreateSolidBrush.Call(uintptr(r.dot))
			hold, _, _ := procSelectObject.Call(hdc, brush)
			d := int32(9)
			cy := y + rowH/2
			procEllipse.Call(hdc,
				uintptr(flyPad), uintptr(cy-d/2),
				uintptr(flyPad+d), uintptr(cy+d/2+1))
			procSelectObject.Call(hdc, hold)
			procDeleteObject.Call(brush)
			f.drawText(hdc, r.text, flyPad+17, y, flyWidth-flyPad*2-17, rowH, r.bold, r.dim)
		} else {
			f.drawText(hdc, r.text, flyPad, y, flyWidth-flyPad*2, rowH, r.bold, r.dim)
		}
		y += rh
		if !r.bold {
			y += 2
		}
	}
}

func procFrameRectCall(hdc uintptr, rc RECT, brush uintptr) {
	procFrameRect.Call(uintptr(hdc), uintptr(unsafe.Pointer(&rc)), brush)
}

func (f *flyout) drawText(hdc uintptr, text string, x, y, w, h int32, bold, dim bool) {
	switch {
	case bold:
		procSelectObject.Call(hdc, uintptr(f.bold))
		procSetTextColor.Call(hdc, uintptr(colorText))
	case dim:
		procSelectObject.Call(hdc, uintptr(f.font))
		procSetTextColor.Call(hdc, uintptr(colorDim))
	default:
		procSelectObject.Call(hdc, uintptr(f.font))
		procSetTextColor.Call(hdc, uintptr(colorText))
	}
	p, _ := windows.UTF16PtrFromString(text)
	const (
		dtSingleLine = 0x0020
		dtVCenter    = 0x0004
		dtEndEllips  = 0x8000
	)
	rc := RECT{x, y, x + w, y + h}
	procDrawTextW.Call(hdc, uintptr(unsafe.Pointer(p)), ^uintptr(0), // cchText: -1
		uintptr(unsafe.Pointer(&rc)), uintptr(dtSingleLine|dtVCenter|dtEndEllips))
}

// logfontW mirrors the Win32 LOGFONTW layout (92 bytes).
type logfontW struct {
	Height         int32
	Width          int32
	Escapement     int32
	Orientation    int32
	Weight         int32
	Italic         uint8
	Underline      uint8
	StrikeOut      uint8
	CharSet        uint8
	OutPrecision   uint8
	ClipPrecision  uint8
	Quality        uint8
	PitchAndFamily uint8
	FaceName       [32]uint16
}

// createFont builds a font via CreateFontIndirectW. CreateFontW/
// CreateFontA are avoided deliberately: they deterministically access-
// violate on some systems once lfHeight is negative (observed on this
// machine with any arguments), while the indirect path works.
func createFont(height, weight int32) windows.Handle {
	lf := logfontW{
		Height:         height,
		Weight:         weight,
		CharSet:        1,    // DEFAULT_CHARSET
		Quality:        5,    // CLEARTYPE_QUALITY
		PitchAndFamily: 0x22, // DEFAULT_PITCH | FF_DONTCARE
	}
	copy(lf.FaceName[:], []uint16(utf16.Encode([]rune("Segoe UI"))))
	h, _, _ := procCreateFontIndirectW.Call(uintptr(unsafe.Pointer(&lf)))
	return windows.Handle(h)
}

// Flyout is the exported handle used by diagnostics (agent-notify flytest).
type Flyout = flyout

// SelfTest drives the show/hide cycle and prints real window state —
// deterministic verification without an interactive click.
func (f *flyout) SelfTest() {
	if f.hwnd == 0 {
		fmt.Println("flytest: no window")
		return
	}
	fmt.Println("flytest: window", uint32(f.hwnd))
	f.show()
	fmt.Println("flytest: after show  visible(flag)=", f.visible, " WS_VISIBLE=", wsVisible(f.hwnd))
	time.Sleep(700 * time.Millisecond)
	fmt.Println("flytest: @700ms      visible(flag)=", f.visible, " WS_VISIBLE=", wsVisible(f.hwnd), " activated=", f.activated)
	f.hide()
	fmt.Println("flytest: after hide  visible(flag)=", f.visible, " WS_VISIBLE=", wsVisible(f.hwnd))
}

func wsVisible(hwnd windows.HWND) bool {
	const gwlStyle = ^uintptr(15) // GWL_STYLE (-16)
	st, _, _ := procGetWindowLongPtrW.Call(uintptr(hwnd), gwlStyle)
	return st&0x10000000 != 0 // WS_VISIBLE
}
