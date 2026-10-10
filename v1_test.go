package montygo

// What monty v1 added and the bridge now exposes: the sandbox working
// directory, the OS policy (clocks, local zone, sleeps, random state), stderr
// as its own stream, os.urandom, complex numbers, and the wider language and
// stdlib that came with them.

import (
	"context"
	"errors"
	"math/big"
	"reflect"
	"strings"
	"sync"
	"testing"
	"time"
)

// recordOsCalls returns an OsCallFunc that records every call and answers it
// with answer, plus the slice it records into.
func recordOsCalls(answer func(*OsCall) (any, error)) (OsCallFunc, *[]OsCall) {
	var mu sync.Mutex
	var calls []OsCall
	return func(_ context.Context, call *OsCall) (any, error) {
		mu.Lock()
		calls = append(calls, *call)
		mu.Unlock()
		return answer(call)
	}, &calls
}

func TestCwdResolvesRelativePaths(t *testing.T) {
	r := newRunner(t)
	handler, calls := recordOsCalls(func(*OsCall) (any, error) { return "hello", nil })
	code := `
from pathlib import Path
import os
[os.getcwd(), Path("a.txt").read_text(), Path("../b.txt").read_text()]
`
	got, err := r.Execute(context.Background(), code, nil, WithCwd("/work/dir"), WithOsCallFunc(handler))
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if want := []any{"/work/dir", "hello", "hello"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("got %#v, want %#v", got, want)
	}
	var paths []any
	for _, c := range *calls {
		paths = append(paths, c.Args[0])
	}
	if want := []any{"/work/dir/a.txt", "/work/b.txt"}; !reflect.DeepEqual(paths, want) {
		t.Fatalf("handler saw paths %#v, want %#v", paths, want)
	}
}

func TestCwdDefaultsToRoot(t *testing.T) {
	r := newRunner(t)
	assertResult(t, r, "import os\nos.getcwd()", nil, "/")
}

func TestCwdRejectsRelative(t *testing.T) {
	r := newRunner(t)
	assertMontyError(t, r, "1", nil, "cwd", WithCwd("relative/dir"))
}

func TestChdirStatsThroughHandler(t *testing.T) {
	r := newRunner(t)
	handler, calls := recordOsCalls(func(call *OsCall) (any, error) {
		if call.Function != "Path.stat" {
			return nil, errors.New("unexpected " + call.Function)
		}
		return StatResult{StMode: 0o040755}, nil
	})
	got, err := r.Execute(context.Background(), "import os\nos.chdir('sub')\nos.getcwd()", nil,
		WithCwd("/work"), WithOsCallFunc(handler))
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if got != "/work/sub" {
		t.Fatalf("getcwd after chdir = %v, want /work/sub", got)
	}
	if len(*calls) != 1 || (*calls)[0].Args[0] != "/work/sub" {
		t.Fatalf("unexpected OS calls: %#v", *calls)
	}
}

func TestStderrHasItsOwnCallback(t *testing.T) {
	r := newRunner(t)
	var stdout, stderr strings.Builder
	_, err := r.Execute(context.Background(), "import sys\nprint('out')\nprint('err', file=sys.stderr)", nil,
		WithPrintFunc(func(s string) { stdout.WriteString(s) }),
		WithStderrFunc(func(s string) { stderr.WriteString(s) }))
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if stdout.String() != "out\n" || stderr.String() != "err\n" {
		t.Fatalf("stdout=%q stderr=%q", stdout.String(), stderr.String())
	}
}

func TestStderrFallsBackToPrintFuncInOrder(t *testing.T) {
	r := newRunner(t)
	var out strings.Builder
	_, err := r.Execute(context.Background(),
		"import sys\nprint('a')\nprint('b', file=sys.stderr)\nprint('c')", nil,
		WithPrintFunc(func(s string) { out.WriteString(s) }))
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if out.String() != "a\nb\nc\n" {
		t.Fatalf("output = %q, want interleaved a b c", out.String())
	}
}

func TestOsPolicyFixedDateTime(t *testing.T) {
	r := newRunner(t)
	fixed := time.Unix(1_700_000_000, 0).UTC()
	code := `
import time
from datetime import date, datetime
[date.today().year, datetime.now().isoformat(), time.time()]
`
	got, err := r.Execute(context.Background(), code, nil,
		WithOsPolicy(OsPolicy{DateTime: DateTimeFixed(fixed)}))
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if want := []any{float64(2023), "2023-11-14T22:13:20", float64(1_700_000_000)}; !reflect.DeepEqual(got, want) {
		t.Fatalf("got %#v, want %#v", got, want)
	}
}

func TestOsPolicySystemClockIsReal(t *testing.T) {
	r := newRunner(t)
	got, err := r.Execute(context.Background(), "import time\ntime.time()", nil)
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	now := float64(time.Now().Unix())
	if f, ok := got.(float64); !ok || f < now-60 || f > now+60 {
		t.Fatalf("time.time() = %v, want about %v", got, now)
	}
}

func TestOsPolicyHostDateTime(t *testing.T) {
	r := newRunner(t)
	handler, calls := recordOsCalls(func(call *OsCall) (any, error) {
		return 1_700_000_000.5, nil
	})
	got, err := r.Execute(context.Background(), "import time\ntime.monotonic()", nil,
		WithOsPolicy(OsPolicy{DateTime: DateTimeHost}), WithOsCallFunc(handler))
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if got != 1_700_000_000.5 {
		t.Fatalf("got %v", got)
	}
	if len(*calls) != 1 || (*calls)[0].Function != "time.time" ||
		!reflect.DeepEqual((*calls)[0].Args, []any{"time.monotonic"}) {
		t.Fatalf("unexpected OS calls: %#v", *calls)
	}
}

func TestOsPolicyNamedTimeZone(t *testing.T) {
	r := newRunner(t)
	// Noon UTC on 1 July: New York is on daylight time, four hours behind.
	fixed := time.Date(2023, 7, 1, 12, 0, 0, 0, time.UTC)
	code := `
import time
from datetime import datetime
[datetime.now().hour, time.strftime("%Z"), time.timezone]
`
	got, err := r.Execute(context.Background(), code, nil, WithOsPolicy(OsPolicy{
		DateTime: DateTimeFixed(fixed),
		TimeZone: TimeZoneNamed("America/New_York"),
	}))
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if want := []any{float64(8), "EDT", float64(5 * 3600)}; !reflect.DeepEqual(got, want) {
		t.Fatalf("got %#v, want %#v", got, want)
	}
}

func TestOsPolicyFixedTimeZone(t *testing.T) {
	r := newRunner(t)
	fixed := time.Date(2023, 7, 1, 12, 0, 0, 0, time.UTC)
	got, err := r.Execute(context.Background(), "from datetime import datetime\ndatetime.now().hour", nil,
		WithOsPolicy(OsPolicy{DateTime: DateTimeFixed(fixed), TimeZone: TimeZoneFixed(2*3600, "X")}))
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if got != float64(14) {
		t.Fatalf("got %v, want 14", got)
	}
}

func TestOsPolicyUnknownTimeZone(t *testing.T) {
	r := newRunner(t)
	assertMontyError(t, r, "1", nil, "unknown timezone",
		WithOsPolicy(OsPolicy{TimeZone: TimeZoneNamed("Not/AZone")}))
}

func TestSleepSystemWaits(t *testing.T) {
	r := newRunner(t)
	start := time.Now()
	_, err := r.Execute(context.Background(), "import time\ntime.sleep(0.2)", nil)
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if elapsed := time.Since(start); elapsed < 200*time.Millisecond {
		t.Fatalf("time.sleep(0.2) returned after %v", elapsed)
	}
}

func TestSleepSystemCapsEachSleep(t *testing.T) {
	r := newRunner(t)
	start := time.Now()
	_, err := r.Execute(context.Background(), "import time\ntime.sleep(30)", nil,
		WithOsPolicy(OsPolicy{Sleep: SleepSystem(100 * time.Millisecond)}))
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if elapsed := time.Since(start); elapsed > 5*time.Second {
		t.Fatalf("a sleep capped at 100ms took %v", elapsed)
	}
}

func TestSleepSystemAsyncio(t *testing.T) {
	r := newRunner(t)
	code := `
import asyncio
async def main():
    return await asyncio.sleep(0.05, "done")
asyncio.run(main())
`
	assertResult(t, r, code, nil, "done")
}

func TestSleepSystemCancelledByContext(t *testing.T) {
	r := newRunner(t)
	ctx, cancel := context.WithTimeout(context.Background(), 200*time.Millisecond)
	defer cancel()
	start := time.Now()
	_, err := r.Execute(ctx, "import time\ntime.sleep(10)", nil)
	if err == nil {
		t.Fatal("expected the context deadline to stop the sleep")
	}
	if elapsed := time.Since(start); elapsed > 5*time.Second {
		t.Fatalf("cancellation took %v", elapsed)
	}
}

func TestMaxTotalSleepIsUncatchable(t *testing.T) {
	r := newRunner(t)
	code := `
import time
n = 0
try:
    for _ in range(10):
        time.sleep(0.05)
        n += 1
except BaseException:
    n = -1
n
`
	_, err := r.Execute(context.Background(), code, nil, WithLimits(Limits{MaxTotalSleep: 120 * time.Millisecond}))
	var me *MontyError
	if !errors.As(err, &me) {
		t.Fatalf("expected a MontyError, got %v", err)
	}
	if !strings.Contains(me.Message, "TimeoutError") || !strings.Contains(me.Message, "sleep limit exceeded") {
		t.Fatalf("unexpected error: %s", me.Message)
	}
}

func TestSleepZero(t *testing.T) {
	r := newRunner(t)
	start := time.Now()
	_, err := r.Execute(context.Background(), "import time\ntime.sleep(30)", nil,
		WithOsPolicy(OsPolicy{Sleep: SleepZero}))
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if elapsed := time.Since(start); elapsed > 5*time.Second {
		t.Fatalf("SleepZero waited %v", elapsed)
	}
}

func TestSleepHost(t *testing.T) {
	r := newRunner(t)
	handler, calls := recordOsCalls(func(*OsCall) (any, error) { return nil, nil })
	_, err := r.Execute(context.Background(), "import time\ntime.sleep(30)", nil,
		WithOsPolicy(OsPolicy{Sleep: SleepHost}), WithOsCallFunc(handler))
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if len(*calls) != 1 || (*calls)[0].Function != "time.sleep" ||
		!reflect.DeepEqual((*calls)[0].Args, []any{float64(30)}) {
		t.Fatalf("unexpected OS calls: %#v", *calls)
	}
}

func TestSleepHostAsyncioPending(t *testing.T) {
	r := newRunner(t)
	var sleepIDs []uint32
	code := `
import asyncio
async def main():
    a, b = await asyncio.gather(asyncio.sleep(1, "a"), asyncio.sleep(1, "b"))
    return a + b
asyncio.run(main())
`
	got, err := r.Execute(context.Background(), code, nil,
		WithOsPolicy(OsPolicy{Sleep: SleepHost}),
		WithOsCallFunc(func(_ context.Context, call *OsCall) (any, error) {
			if call.Function != "asyncio.sleep" {
				return nil, errors.New("unexpected " + call.Function)
			}
			sleepIDs = append(sleepIDs, call.CallID)
			return Pending{}, nil
		}),
		WithFutureResolver(func(_ context.Context, ids []uint32) (map[uint32]any, error) {
			out := map[uint32]any{}
			for _, id := range ids {
				out[id] = nil
			}
			return out, nil
		}))
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if got != "ab" || len(sleepIDs) != 2 {
		t.Fatalf("got %v with %d sleeps", got, len(sleepIDs))
	}
}

func TestPendingRefusedForPlainOsCall(t *testing.T) {
	r := newRunner(t)
	assertError(t, r, "from pathlib import Path\nPath('/x').read_text()", nil, "returned Pending",
		WithOsCallFunc(func(context.Context, *OsCall) (any, error) { return Pending{}, nil }))
}

func TestRandomIsSeededFromRealEntropy(t *testing.T) {
	r := newRunner(t)
	code := "import random\nrandom.getrandbits(64)"
	a, err := r.Execute(context.Background(), code, nil)
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	b, err := r.Execute(context.Background(), code, nil)
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if reflect.DeepEqual(a, b) {
		t.Fatalf("two runs drew the same 64 random bits (%v): the entropy source is deterministic", a)
	}
}

func TestRandomSeedMatchesCPython(t *testing.T) {
	r := newRunner(t)
	// CPython: random.seed(42); random.random() == 0.6394267984578837
	assertResult(t, r, "import random\nrandom.random()", nil, 0.6394267984578837,
		WithOsPolicy(OsPolicy{Random: RandomSeedInt(big.NewInt(42))}))
}

func TestRandomHostAsksForState(t *testing.T) {
	r := newRunner(t)
	handler, calls := recordOsCalls(func(call *OsCall) (any, error) {
		return make(Bytes, int(call.Args[0].(float64))), nil
	})
	_, err := r.Execute(context.Background(), "import random\nrandom.random()", nil,
		WithOsPolicy(OsPolicy{Random: RandomHost}), WithOsCallFunc(handler))
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if len(*calls) != 1 || (*calls)[0].Function != "os.urandom" {
		t.Fatalf("unexpected OS calls: %#v", *calls)
	}
}

func TestOsUrandom(t *testing.T) {
	r := newRunner(t)
	handler, calls := recordOsCalls(func(call *OsCall) (any, error) {
		return Bytes{1, 2, 3, 4}, nil
	})
	got, err := r.Execute(context.Background(), "import os\nos.urandom(4)", nil, WithOsCallFunc(handler))
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if !reflect.DeepEqual(got, Bytes{1, 2, 3, 4}) {
		t.Fatalf("got %#v", got)
	}
	if len(*calls) != 1 || !reflect.DeepEqual((*calls)[0].Args, []any{float64(4)}) {
		t.Fatalf("unexpected OS calls: %#v", *calls)
	}
}

func TestOsUrandomWrongLength(t *testing.T) {
	r := newRunner(t)
	assertMontyError(t, r, "import os\nos.urandom(4)", nil, "returned 2 bytes, expected 4",
		WithOsCallFunc(func(context.Context, *OsCall) (any, error) { return Bytes{1, 2}, nil }))
}

func TestComplexRoundTrip(t *testing.T) {
	r := newRunner(t)
	got, err := r.Execute(context.Background(), "z * complex(0, 1) + 1j", map[string]any{"z": Complex(complex(1, 2))})
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if got != Complex(complex(-2, 2)) {
		t.Fatalf("got %#v, want (-2+2j)", got)
	}
}

func TestEvalAndExec(t *testing.T) {
	r := newRunner(t)
	assertResult(t, r, "exec('y = 2')\neval('y * 21')", nil, float64(42))
}

func TestV1Stdlib(t *testing.T) {
	r := newRunner(t)
	code := `
import copy, itertools, random, binascii
nested = [[1], [2]]
deep = copy.deepcopy(nested)
deep[0].append(9)
rng = random.Random(7)
[
    nested[0],
    list(itertools.accumulate([1, 2, 3])),
    len(rng.sample(range(100), 5)),
    binascii.hexlify(b"hi").decode(),
    "%s-%d" % ("x", 3),
    format(3.14159, ".2f"),
]
`
	got, err := r.Execute(context.Background(), code, nil)
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	want := []any{
		[]any{float64(1)},
		[]any{float64(1), float64(3), float64(6)},
		float64(5),
		"6869",
		"x-3",
		"3.14",
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %#v, want %#v", got, want)
	}
}

func TestUndeclaredFunctionIsNameError(t *testing.T) {
	r := newRunner(t)
	code := `
try:
    missing_fn(1)
    r = "called"
except NameError as e:
    r = str(e)
r
`
	assertResult(t, r, code, nil, "name 'missing_fn' is not defined")
}

func TestGuestTrapError(t *testing.T) {
	trap := errors.New("montygo: monty_resume call failed: wasm error: unreachable")

	err := guestTrapError(trap, "monty worker: allocation of 300000000 bytes exceeds the memory limit\n")
	var me *MontyError
	if !errors.As(err, &me) || !strings.HasPrefix(me.Message, "MemoryError: memory limit exceeded (allocation of 300000000 bytes") {
		t.Fatalf("allocator trap: got %v", err)
	}

	err = guestTrapError(trap, "thread 'main' panicked at src/lib.rs:1:1")
	if !errors.Is(err, trap) || !strings.Contains(err.Error(), "guest stderr: thread 'main' panicked") {
		t.Fatalf("panic trap: got %v", err)
	}

	if err := guestTrapError(trap, ""); err != trap {
		t.Fatalf("no stderr: got %v", err)
	}
	if err := guestTrapError(context.Canceled, "monty worker: allocation of 1 bytes failed"); err != context.Canceled {
		t.Fatalf("cancellation: got %v", err)
	}
	py := &MontyError{Message: "ValueError: x"}
	if err := guestTrapError(py, "noise"); err != py {
		t.Fatalf("python error: got %v", err)
	}
}

func TestBoundedBuffer(t *testing.T) {
	b := &boundedBuffer{max: 5}
	for _, s := range []string{"abc", "defg", "h"} {
		if n, err := b.Write([]byte(s)); n != len(s) || err != nil {
			t.Fatalf("Write(%q) = %d, %v", s, n, err)
		}
	}
	if b.String() != "abcde" {
		t.Fatalf("got %q", b.String())
	}
}
