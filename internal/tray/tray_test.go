package tray

import (
	"testing"
	"time"
)

func TestShouldBlink(t *testing.T) {
	now := time.Now()
	cases := []struct {
		name     string
		sev      string
		waitTill time.Time
		want     bool
	}{
		{"blocked always blinks", "blocked", time.Time{}, true},
		{"waiting inside window blinks", "waiting", now.Add(time.Minute), true},
		{"waiting after window is solid", "waiting", now.Add(-time.Minute), false},
		{"waiting never armed is solid", "waiting", time.Time{}, false},
		{"working is solid", "working", now.Add(time.Minute), false},
		{"idle is solid", "idle", now.Add(time.Minute), false},
		{"down is solid", "down", now.Add(time.Minute), false},
	}
	for _, c := range cases {
		if got := shouldBlink(c.sev, c.waitTill, now); got != c.want {
			t.Errorf("%s: shouldBlink(%q, %v) = %v, want %v", c.name, c.sev, c.waitTill, got, c.want)
		}
	}
}

func TestArmWaitBlink(t *testing.T) {
	tr := &Tray{}
	armed := func() time.Time {
		till, _ := tr.waitBlinkTill.Load().(time.Time)
		return till
	}

	// Starting directly in waiting (agents idle before launch): solid.
	tr.armWaitBlink("waiting")
	if !armed().IsZero() {
		t.Error("startup-in-waiting must not arm the blink")
	}

	// working → waiting: armed for the window.
	tr.armWaitBlink("working")
	tr.curSev.Store("working")
	tr.armWaitBlink("waiting")
	till := armed()
	if till.IsZero() || time.Until(till) > waitingBlinkWindow+time.Second {
		t.Errorf("entering waiting should arm ~%s, got %v", waitingBlinkWindow, till)
	}

	// Still waiting: the running window is kept, not re-armed.
	tr.curSev.Store("waiting")
	time.Sleep(20 * time.Millisecond)
	tr.armWaitBlink("waiting")
	if got := armed(); !got.Equal(till) {
		t.Error("persistent waiting must keep the original window")
	}

	// blocked → waiting (a blocked agent got unstuck and stopped): armed.
	tr.curSev.Store("blocked")
	tr.armWaitBlink("waiting")
	till = armed()
	if till.IsZero() {
		t.Error("blocked → waiting should arm the blink")
	}

	// down → waiting (offline recovery): never re-arms, and the previous
	// window survives untouched — a poll flap must not reset the blink.
	tr.curSev.Store("waiting")
	time.Sleep(20 * time.Millisecond)
	tr.armWaitBlink("down")
	tr.curSev.Store("down")
	tr.armWaitBlink("waiting")
	if got := armed(); !got.Equal(till) {
		t.Error("down → waiting must neither re-arm nor clear the window")
	}

	// idle → waiting (idle agents appeared): not a finish event.
	tr.curSev.Store("idle")
	tr.waitBlinkTill.Store(time.Time{})
	tr.armWaitBlink("idle")
	tr.armWaitBlink("waiting")
	if !armed().IsZero() {
		t.Error("idle → waiting should not arm the blink")
	}
}
