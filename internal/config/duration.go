package config

import (
	"fmt"
	"time"
)

// Duration wraps time.Duration so TOML configs can write it as a string
// ("1s", "250ms", ...). An empty value keeps the zero Duration.
type Duration struct{ time.Duration }

// UnmarshalText implements encoding.TextUnmarshaler (used by the TOML decoder).
func (d *Duration) UnmarshalText(b []byte) error {
	s := string(b)
	if s == "" {
		return nil
	}
	v, err := time.ParseDuration(s)
	if err != nil {
		return fmt.Errorf("invalid duration %q: %w", s, err)
	}
	d.Duration = v
	return nil
}

// D is a terse accessor for the wrapped time.Duration.
func (d Duration) D() time.Duration { return d.Duration }
