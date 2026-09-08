//go:build windows

package tray

import (
	"fmt"
	"log/slog"
	"os"
	"sync"
	"time"
	"unicode/utf16"
	"unsafe"

	"golang.org/x/sys/windows"

	"github.com/jaltez/agent-notify/internal/engine"
	"github.com/jaltez/agent-notify/internal/icon"
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

	flyWidth      = 420
	flyPad        = 14
	sectionH      = 24
	rowH          = 21 // summary / single-line rows
	sepAfterSum   = 8
	titleH        = 20 // agent block: line 1 (title)
	infoH         = 16 // agent block: line 2 (agent · project)
	blockGap      = 5
	agentBlockH   = titleH + infoH + blockGap
	maxFitBlocks  = 10 // agent blocks before scrolling
	wheelStep     = 42 // px per wheel notch
	activateGrace = 600 * time.Millisecond
	flyClassName  = "agent-notify-flyout"

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
	procFillRgn               = gdi32.NewProc("FillRgn")
	procTrackMouseEvent       = user32.NewProc("TrackMouseEvent")
	procCreateSolidBrush      = gdi32.NewProc("CreateSolidBrush")
	procDeleteObject          = gdi32.NewProc("DeleteObject")
	procCreateFontIndirectW   = gdi32.NewProc("CreateFontIndirectW")
	procSelectObject          = gdi32.NewProc("SelectObject")
	procSetBkMode             = gdi32.NewProc("SetBkMode")
	procSetTextColor          = gdi32.NewProc("SetTextColor")
	procDrawTextW             = user32.NewProc("DrawTextW")
	procEllipse               = gdi32.NewProc("Ellipse")
	procCreateRoundRectRgn    = gdi32.NewProc("CreateRoundRectRgn")
	procSetWindowRgn          = user32.NewProc("SetWindowRgn")
	procSystemParametersInfoW = user32.NewProc("SystemParametersInfoW")
	procSetTimer              = user32.NewProc("SetTimer")
	procKillTimer             = user32.NewProc("KillTimer")
	procShowWindow            = user32.NewProc("ShowWindow")
	procFrameRect             = user32.NewProc("FrameRect")
	procGetModuleHandleW      = kernel32.NewProc("GetModuleHandleW")
	procGetWindowLongPtrW     = user32.NewProc("GetWindowLongPtrW")
	procPeekMessageW          = user32.NewProc("PeekMessageW")
	procTranslateMessage      = user32.NewProc("TranslateMessage")
	procDispatchMessageW      = user32.NewProc("DispatchMessageW")
	procGetWindowRect         = user32.NewProc("GetWindowRect")
	procGetForegroundWindow   = user32.NewProc("GetForegroundWindow")
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

	// scroll state (window thread)
	scroll     int32
	contentH   int32
	scrollable bool

	// hover highlight (window thread): index into last-painted rows
	hover    int32
	hoverSet bool
	tracking bool
	lastRows []flyRow
	lastGeom []rowGeom
	shownAt  time.Time // grace period before deactivate-dismiss applies
	// diagnostics, touched from the window thread (and flytest)
	paintPanics int
	lastPanic   string
	paints      int
	mu          sync.Mutex
	// activated tracks whether we actually hold foreground: the
	// deactivate-hide (click outside) may only fire when we had it,
	// otherwise a lost foreground race hides the panel instantly.
	activated bool
	font      windows.Handle
	bold      windows.Handle
	small     windows.Handle
}

var currentFly *flyout

func flyWndProc(hwnd windows.HWND, msg uint32, wParam, lParam uintptr) (rc uintptr) {
	// A panic here would take the whole tray process down (the callback
	// crosses the Win32 boundary); degrade to a visual glitch instead,
	// but remember it — a silent blank window is miserable to debug.
	defer func() {
		if r := recover(); r != nil {
			currentFly.mu.Lock()
			currentFly.paintPanics++
			currentFly.lastPanic = fmt.Sprint(r)
			currentFly.mu.Unlock()
		}
	}()
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
		if uint16(wParam) == 0 && time.Since(f.shownAt) < activateGrace {
			// activation was yanked during startup (another window held
			// foreground); keep the panel up — a real tray click grants
			// foreground and click-outside still dismisses afterwards
			f.activated = false
			return 0
		}
	case 0x0100: // WM_KEYDOWN
		if wParam == 0x1B { // VK_ESCAPE
			f.hide()
			return 0
		}
	case 0x000F: // WM_PAINT
		f.paints++
		f.paint(hwnd)
		return 0
	case 0x0200: // WM_MOUSEMOVE
		f.trackHover(uintptr(lParam))
		return 0
	case 0x02A3: // WM_MOUSELEAVE
		if f.hoverSet {
			f.hoverSet = false
			f.hover = -1
			procInvalidateRect.Call(uintptr(f.hwnd), 0, 1)
		}
		f.tracking = false
		return 0
	case 0x020A: // WM_MOUSEWHEEL
		delta := int16(uint32(wParam) >> 16)
		f.scroll -= int32(delta) / 120 * wheelStep
		f.clampScroll()
		procInvalidateRect.Call(uintptr(f.hwnd), 0, 1)
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
	f.small = createFont(-12, 400)
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

// trackHover hit-tests the mouse position against painted rows and
// repaints when the hovered row changes. Geoms hold absolute client
// coordinates as painted (scroll already applied).
func (f *flyout) trackHover(lParam uintptr) {
	if !f.visible || len(f.lastGeom) == 0 {
		return
	}
	my := int32(int16(uint32(lParam) >> 16))
	idx := int32(-1)
	for i, g := range f.lastGeom {
		if my >= g.y && my < g.y+g.h {
			idx = int32(i)
			break
		}
	}
	if idx != f.hover || !f.hoverSet {
		f.hover = idx
		f.hoverSet = true
		procInvalidateRect.Call(uintptr(f.hwnd), 0, 1)
	}
	if !f.tracking {
		f.tracking = true
		var tme struct {
			Size   uint32
			Flags  uint32
			Hwnd   windows.HWND
			HoverT uint32
		}
		tme.Size = uint32(unsafe.Sizeof(tme))
		tme.Flags = 0x00000002 // TME_LEAVE
		tme.Hwnd = f.hwnd
		procTrackMouseEvent.Call(uintptr(unsafe.Pointer(&tme)))
	}
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
	_, contentH := f.rowsAndHeight()
	f.scroll = 0
	fitH := f.fitHeight()
	scrollable := contentH > fitH
	h := contentH
	if scrollable {
		h = fitH
	}
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
	f.contentH = contentH
	f.scrollable = scrollable
	if !scrollable {
		// everything fits: auto-close as designed. A scrollable panel
		// stays open until dismissed.
		procSetTimer.Call(uintptr(f.hwnd), timerAutoclose, uintptr(autoCloseDelay.Milliseconds()), 0)
	}
	procSetTimer.Call(uintptr(f.hwnd), timerRefresh, uintptr(refreshEvery.Milliseconds()), 0)
	f.visible = true
}

// agentBlockH is the painted height of one agent row.
func (f *flyout) agentBlockH() int32 { return titleH + infoH + blockGap }

// clientHeight is the visible band when scrolling is active.
func (f *flyout) clientHeight() int32 {
	return int32(flyPad*2+sectionH+rowH+sepAfterSum) + int32(maxFitBlocks)*f.agentBlockH()
}

// fitHeight is the panel height holding header, summary and maxFitBlocks.
func (f *flyout) fitHeight() int32 { return f.clientHeight() }

func (f *flyout) clampScroll() {
	max := f.contentH - f.clientHeight()
	if max < 0 {
		max = 0
	}
	if f.scroll > max {
		f.scroll = max
	}
	if f.scroll < 0 {
		f.scroll = 0
	}
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

type rowGeom struct {
	y, h int32
}

type flyRow struct {
	dot  uint32 // COLORREF; 0 = no dot
	sub  string // secondary run (agent name), drawn dim in its own column
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
		for _, a := range s.Agents {
			title := a.Title
			info := a.Name
			if a.Project != "" {
				if title == "" {
					title = a.Project
				} else {
					info += " · " + a.Project
				}
			}
			rows = append(rows, flyRow{
				dot:  colorRef(icon.Color(a.Status)),
				text: orDash(title), // line 1: what it is doing
				sub:  info,          // line 2: runner · project, dim
			})
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
		if r.sub != "" {
			rh = f.agentBlockH()
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
	f.contentH = h
	if f.scrollable {
		f.clampScroll()
	}
	// live refresh can add rows after show(); grow the window to fit
	if cur := f.rectHeight(); cur != h {
		const (
			swpNoMove     = 0x0002
			swpNoZorder   = 0x0004
			swpNoActivate = 0x0010
		)
		procSetWindowPos.Call(uintptr(f.hwnd), 0, 0, 0, uintptr(flyWidth), uintptr(h),
			swpNoMove|swpNoZorder|swpNoActivate)
	}
	bg, _, _ := procCreateSolidBrush.Call(uintptr(colorBG))
	defer procDeleteObject.Call(bg)
	border, _, _ := procCreateSolidBrush.Call(uintptr(colorBorder))
	defer procDeleteObject.Call(border)
	frame := RECT{0, 0, flyWidth, h}
	procFrameRectCall(hdc, frame, border)
	client := RECT{1, 1, flyWidth - 1, h - 1}
	procFillRect.Call(hdc, uintptr(unsafe.Pointer(&client)), bg)

	procSetBkMode.Call(hdc, 1) // TRANSPARENT

	clientH := h
	if f.scrollable {
		clientH = f.clientHeight()
	}
	f.lastRows = rows
	f.lastGeom = f.lastGeom[:0]
	y := int32(flyPad) - f.scroll
	for i, r := range rows {
		rh := int32(rowH)
		if r.bold {
			rh = sectionH
		}
		if r.sub != "" {
			rh = f.agentBlockH() // two-line agent block
		}
		f.lastGeom = append(f.lastGeom, rowGeom{y: y + f.scroll, h: rh})

		hovered := f.hoverSet && f.hover == int32(i)
		if y+rh >= flyPad-2 && y <= clientH { // clip to the visible band
			x := int32(flyPad)
			w := int32(flyWidth - flyPad*2)

			if r.sub != "" {
				// whole-block wash: status color blended over the panel
				tint := blendColor(r.dot, 0.16, hovered)
				rgn, _, _ := procCreateRoundRectRgn.Call(
					uintptr(x-6), uintptr(y+1), uintptr(x+w+6), uintptr(y+rh-2), 8, 8)
				tbrush, _, _ := procCreateSolidBrush.Call(uintptr(tint))
				procFillRgn.Call(hdc, rgn, tbrush)
				procDeleteObject.Call(tbrush)
				procDeleteObject.Call(rgn)
			} else if hovered {
				hb, _, _ := procCreateSolidBrush.Call(uintptr(hoverColor()))
				hoverRect := RECT{x - 6, y, x + w + 6, y + rh - 2}
				procFillRect.Call(hdc, uintptr(unsafe.Pointer(&hoverRect)), hb)
				procDeleteObject.Call(hb)
			}

			if r.dot != 0 {
				brush, _, _ := procCreateSolidBrush.Call(uintptr(r.dot))
				hold, _, _ := procSelectObject.Call(hdc, brush)
				d := int32(9)
				cy := y + titleH/2 // dot aligns with the title line
				procEllipse.Call(hdc,
					uintptr(x), uintptr(cy-d/2),
					uintptr(x+d), uintptr(cy+d/2+1))
				procSelectObject.Call(hdc, hold)
				procDeleteObject.Call(brush)
				x += 17
				w -= 17
			}
			if r.sub != "" {
				// line 1: title; line 2: runner · project (dim, small)
				f.drawText(hdc, r.text, x, y, w, titleH, r.bold, r.dim)
				f.drawTextSmall(hdc, r.sub, x, y+titleH, w, infoH)
			} else {
				f.drawText(hdc, r.text, x, y, w, rh, r.bold, r.dim)
			}
		}
		y += rh
	}
	// scrollbar thumb when scrollable
	if f.scrollable && f.contentH > clientH {
		trackH := clientH - 8
		thumbH := int32(float64(clientH) * float64(clientH) / float64(f.contentH))
		if thumbH < 24 {
			thumbH = 24
		}
		maxScroll := f.contentH - clientH
		thumbY := 4 + int32(float64(trackH-thumbH)*float64(f.scroll)/float64(maxScroll))
		brush, _, _ := procCreateSolidBrush.Call(uintptr(0x00565656))
		bar := RECT{flyWidth - 6, thumbY, flyWidth - 3, thumbY + thumbH}
		procFillRect.Call(hdc, uintptr(unsafe.Pointer(&bar)), brush)
		procDeleteObject.Call(brush)
	}
}

func procFrameRectCall(hdc uintptr, rc RECT, brush uintptr) {
	procFrameRect.Call(uintptr(hdc), uintptr(unsafe.Pointer(&rc)), brush)
}

// a returns the status of the agent backing a row (rows with a sub are
// agent rows; From/To carry the transition but Status is what the tint
// mirrors, taken from the dot's own severity via the stored color).
func a(r flyRow) agentColorRef { return agentColorRef{} }

type agentColorRef struct{}

func (f *flyout) drawTextSmall(hdc uintptr, text string, x, y, w, h int32) {
	procSelectObject.Call(hdc, uintptr(f.small))
	procSetTextColor.Call(hdc, uintptr(colorDim))
	p, _ := windows.UTF16PtrFromString(text)
	const (
		dtSingleLine = 0x0020
		dtVCenter    = 0x0004
		dtEndEllips  = 0x8000
	)
	rc := RECT{x, y, x + w, y + h}
	procDrawTextW.Call(hdc, uintptr(unsafe.Pointer(p)), ^uintptr(0),
		uintptr(unsafe.Pointer(&rc)), uintptr(dtSingleLine|dtVCenter|dtEndEllips))
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

// SelfTest drives the show/hide cycle, pumping messages like the real
// tray does, and prints full window state — deterministic verification
// without an interactive click.
func (f *flyout) SelfTest() {
	if f.hwnd == 0 {
		fmt.Println("flytest: no window")
		return
	}
	fmt.Println("flytest: window", uint32(f.hwnd))
	f.show()
	f.pump(1200 * time.Millisecond)
	f.mu.Lock()
	fmt.Println("flytest: after show  visible=", f.visible, " activated=", f.activated,
		" paints=", f.paints, " paintPanics=", f.paintPanics, " lastPanic=", f.lastPanic)
	f.mu.Unlock()
	fmt.Println("flytest: rect=", f.rectString())
	fg, _, _ := procGetForegroundWindow.Call()
	fmt.Println("flytest: foreground=", fg == uintptr(f.hwnd))
	f.hide()
	f.pump(300 * time.Millisecond)
	fmt.Println("flytest: after hide  visible=", f.visible)
}

// pump dispatches window messages for d (must run on the window thread).
func (f *flyout) pump(d time.Duration) {
	deadline := time.Now().Add(d)
	var msg [7]uintptr // MSG (size on win64 = 48 bytes; use raw buffer)
	_ = msg
	deadlineMs := uint32(time.Now().Add(d).UnixMilli())
	for time.Now().Before(deadline) {
		const pmRemove = 0x0001
		have, _, _ := procPeekMessageW.Call(uintptr(unsafe.Pointer(&msg[0])), 0, 0, 0, pmRemove)
		if have != 0 {
			procTranslateMessage.Call(uintptr(unsafe.Pointer(&msg[0])))
			procDispatchMessageW.Call(uintptr(unsafe.Pointer(&msg[0])))
		} else {
			time.Sleep(5 * time.Millisecond)
		}
	}
	_ = deadlineMs
}

func (f *flyout) rectHeight() int32 {
	var rc RECT
	procGetWindowRect.Call(uintptr(f.hwnd), uintptr(unsafe.Pointer(&rc)))
	return rc.Bottom - rc.Top
}

func (f *flyout) rectString() string {
	var rc RECT
	procGetWindowRect.Call(uintptr(f.hwnd), uintptr(unsafe.Pointer(&rc)))
	return fmt.Sprintf("left=%d top=%d right=%d bottom=%d (w=%d h=%d)",
		rc.Left, rc.Top, rc.Right, rc.Bottom, rc.Right-rc.Left, rc.Bottom-rc.Top)
}

func wsVisible(hwnd windows.HWND) bool {
	const gwlStyle = ^uintptr(15) // GWL_STYLE (-16)
	st, _, _ := procGetWindowLongPtrW.Call(uintptr(hwnd), gwlStyle)
	return st&0x10000000 != 0 // WS_VISIBLE
}

// blendColor mixes a COLORREF over the panel background at ratio t; hover
// lifts the result toward white so the hovered row reads as selected.
func blendColor(dot uint32, t float64, hover bool) uint32 {
	const bgR, bgG, bgB = 0x20, 0x20, 0x20 // panel background
	mix := func(a, b uint8) uint8 {
		return uint8(float64(a)*(1-t) + float64(b)*t + 0.5)
	}
	r := mix(bgR, uint8(dot&0xFF))
	g := mix(bgG, uint8((dot>>8)&0xFF))
	b := mix(bgB, uint8((dot>>16)&0xFF))
	if hover {
		lift := func(v uint8) uint8 { return uint8(float64(v)*(1-0.10) + 240*0.10 + 0.5) }
		r, g, b = lift(r), lift(g), lift(b)
	}
	return uint32(r) | uint32(g)<<8 | uint32(b)<<16
}

// hoverColor is the neutral highlight for non-agent rows.
func hoverColor() uint32 { return 0x00303030 }
