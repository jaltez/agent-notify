package event

import "testing"

func TestParseSetDefaults(t *testing.T) {
	ks, err := ParseSet(nil)
	if err != nil {
		t.Fatalf("nil set: %v", err)
	}
	if len(ks) != len(Attention) {
		t.Fatalf("nil set = %v, want attention set", ks)
	}
	ks, err = ParseSet([]string{})
	if err != nil {
		t.Fatalf("empty set: %v", err)
	}
	if len(ks) != len(Attention) {
		t.Fatalf("empty set = %v, want attention set", ks)
	}
}

func TestParseSetExpansion(t *testing.T) {
	ks, err := ParseSet([]string{"attention"})
	if err != nil {
		t.Fatalf("attention: %v", err)
	}
	want := map[Kind]bool{KindAgentIdle: true, KindAgentDone: true, KindAgentBlocked: true}
	if len(ks) != len(want) {
		t.Fatalf("attention = %v", ks)
	}
	for _, k := range ks {
		if !want[k] {
			t.Fatalf("attention set contains %q", k)
		}
	}
	ks, _ = ParseSet([]string{"none"})
	if len(ks) != 0 {
		t.Fatalf("none = %v", ks)
	}
	ks, _ = ParseSet([]string{"all"})
	if len(ks) != len(All) {
		t.Fatalf("all = %v (want %d kinds)", ks, len(All))
	}
	// duplicates collapse
	ks, _ = ParseSet([]string{"agent_idle", "agent_idle"})
	if len(ks) != 1 {
		t.Fatalf("duplicates not collapsed: %v", ks)
	}
}

func TestParseSetUnknown(t *testing.T) {
	_, err := ParseSet([]string{"agent_explosion"})
	if err == nil {
		t.Fatal("unknown kind accepted")
	}
	if _, ok := err.(*UnknownKindError); !ok {
		t.Fatalf("error type %T, want *UnknownKindError", err)
	}
}

func TestEveryKindHasVerb(t *testing.T) {
	for _, k := range All {
		if v := k.Verb(); v == "" || v == string(k) {
			t.Errorf("kind %q has no distinct verb (%q)", k, v)
		}
	}
}
