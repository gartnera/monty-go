package montygo

import (
	"context"
	"testing"
	"time"
)

// TestMaxDurationTracksWallClock guards against the module being instantiated
// without the host's clocks. wazero's default ModuleConfig hands the guest a
// fake nanotime that advances a fixed step per reading, which made monty's
// elapsed-time accounting run ~75x ahead of the wall clock: a limit far larger
// than the work would still trip, and the same program passed or failed
// depending only on how generous the limit was.
func TestMaxDurationTracksWallClock(t *testing.T) {
	r := newRunner(t)

	// A loop that takes well under a second in real time. Every limit here is
	// comfortably above that, so none of them may trip.
	code := "n = 0\nwhile n < 500000:\n    n = n + 1\nn"
	for _, limit := range []time.Duration{2 * time.Second, 5 * time.Second, 30 * time.Second} {
		start := time.Now()
		got, err := r.Execute(context.Background(), code, nil,
			WithLimits(Limits{MaxDuration: limit}))
		elapsed := time.Since(start)
		if err != nil {
			t.Fatalf("MaxDuration=%v tripped after %v of wall time: %v", limit, elapsed, err)
		}
		if got != float64(500000) {
			t.Fatalf("MaxDuration=%v: got %v, want 500000", limit, got)
		}
		if elapsed > limit {
			t.Fatalf("MaxDuration=%v: test workload took %v, too slow to be conclusive", limit, elapsed)
		}
	}
}

// TestMaxDurationStillEnforced checks the limit has not simply become inert:
// a program that never finishes must still be stopped, and roughly when the
// limit says rather than immediately.
func TestMaxDurationStillEnforced(t *testing.T) {
	r := newRunner(t)

	const limit = 500 * time.Millisecond
	start := time.Now()
	_, err := r.Execute(context.Background(), "n = 0\nwhile True:\n    n = n + 1", nil,
		WithLimits(Limits{MaxDuration: limit}))
	elapsed := time.Since(start)
	if err == nil {
		t.Fatal("expected an infinite loop to hit the duration limit")
	}
	// Generous bounds: this asserts the limit is anchored to real time, not
	// that monty checks it on any particular schedule.
	if elapsed < limit/2 {
		t.Fatalf("limit %v tripped after only %v of wall time; the guest clock is running fast", limit, elapsed)
	}
	if elapsed > 10*limit {
		t.Fatalf("limit %v took %v to trip", limit, elapsed)
	}
}
