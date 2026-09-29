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

	// Leaving waiting clears it.
	tr.curSev.Store("waiting")
	tr.armWaitBlink("idle")
	if !armed().IsZero() {
		t.Error("leaving waiting must clear the window")
	}
}
