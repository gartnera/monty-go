package montygo

import (
	"encoding/base64"
	"encoding/json"
	"math/big"
	"time"
)

// OsPolicy decides how the sandbox answers the calls that read the outside
// world without touching the filesystem: the clocks behind `time.time()`,
// `datetime.now()` and `date.today()`, the local time zone, `time.sleep()` /
// `asyncio.sleep()`, `time.process_time()`, and where an unseeded `random`
// generator gets its first state.
//
// The zero value is monty's own default, which needs nothing from the host:
// the system clock, UTC as the local zone, sleeps that really wait (capped at
// ten seconds each), and OS entropy for `random`. Each field can instead hand
// that decision to the host's [OsCallFunc] or pin it to a fixed answer, which
// is how a host makes a run deterministic.
//
// Under the default policy these calls never reach the OS-call handler. The
// exceptions are a sleep, which the bridge performs itself (see [SleepSystem]),
// and `os.urandom(n)`, which always asks the handler — answer it with
// n [Bytes] of entropy, or refuse it with a [PyError].
type OsPolicy struct {
	// DateTime is where the clocks read the instant. Zero is the system clock.
	DateTime DateTimeSource
	// TimeZone is the sandbox's local zone: what a naive `datetime.now()`,
	// `date.today()`, `time.localtime()` and `astimezone()` use. Zero is UTC.
	TimeZone SandboxTimeZone
	// Sleep is what `time.sleep()` and `asyncio.sleep()` do. Zero is
	// [SleepSystem] with monty's ten-second cap.
	Sleep SleepMode
	// ProcessTimeElapsed makes `time.process_time()` and
	// `time.thread_time()` report the run's execution time so far. By default
	// they report zero, so a run on a fixed clock cannot observe time passing.
	ProcessTimeElapsed bool
	// Random is where an unseeded `random` generator starts. Zero is OS
	// entropy.
	Random RandomStart
}

// MarshalJSON implements [json.Marshaler], producing the shim's os_policy
// shape. Fields left at their zero value are omitted so the shim keeps
// monty's default for them.
func (p OsPolicy) MarshalJSON() ([]byte, error) {
	out := map[string]any{}
	if p.DateTime.kind != "" {
		out["datetime"] = map[string]any{
			"kind": p.DateTime.kind, "unix_seconds": p.DateTime.unixSeconds,
			"microsecond": p.DateTime.microsecond,
		}
	}
	if p.TimeZone.set {
		tz := map[string]any{}
		if p.TimeZone.name != "" {
			tz["name"] = p.TimeZone.name
		}
		if p.TimeZone.fixed {
			tz["offset_seconds"] = p.TimeZone.offsetSeconds
		}
		out["timezone"] = tz
	}
	if p.Sleep.kind != "" {
		sleep := map[string]any{"kind": p.Sleep.kind}
		if p.Sleep.kind == "system" && p.Sleep.max > 0 {
			sleep["max_ms"] = p.Sleep.max.Milliseconds()
		}
		out["sleep"] = sleep
	}
	if p.ProcessTimeElapsed {
		out["process_time"] = "elapsed"
	}
	if p.Random.kind != "" {
		random := map[string]any{"kind": p.Random.kind}
		for k, v := range p.Random.seed {
			random[k] = v
		}
		out["random"] = random
	}
	return json.Marshal(out)
}

// DateTimeSource is where the sandbox's clocks read the current instant. Use
// [DateTimeSystem], [DateTimeHost] or [DateTimeFixed].
type DateTimeSource struct {
	kind        string
	unixSeconds int64
	microsecond uint32
}

var (
	// DateTimeSystem reads the real clock. It is the default.
	DateTimeSystem = DateTimeSource{kind: "system"}
	// DateTimeHost asks the OS-call handler on every clock read:
	// "date.today", "datetime.now" (with the requested zone, or nil, as its
	// argument) and "time.time" (with the calling function's name, such as
	// "time.monotonic", as its argument). Answer "time.time" with a float of
	// seconds since the Unix epoch, and the other two with a [Date] or
	// [DateTime].
	DateTimeHost = DateTimeSource{kind: "host"}
)

// DateTimeFixed makes every clock read return t, so the program sees time
// stand still — useful for reproducible runs.
func DateTimeFixed(t time.Time) DateTimeSource {
	return DateTimeSource{kind: "fixed", unixSeconds: t.Unix(), microsecond: uint32(t.Nanosecond() / 1000)}
}

// SandboxTimeZone is the sandbox's local time zone. Use [TimeZoneNamed] or
// [TimeZoneFixed]; the zero value is UTC.
type SandboxTimeZone struct {
	set           bool
	fixed         bool
	name          string
	offsetSeconds int
}

// TimeZoneNamed is an IANA zone, such as "Europe/London", with its daylight
// saving transitions. The zone rules are compiled into the interpreter, so
// this does not depend on the host's tz database; an unknown name makes
// Execute fail before the program starts.
func TimeZoneNamed(name string) SandboxTimeZone {
	return SandboxTimeZone{set: true, name: name}
}

// TimeZoneFixed is a fixed offset east of UTC, in seconds, with no daylight
// saving. name is what `%Z` and `tzname` report; it may be empty.
func TimeZoneFixed(offsetSeconds int, name string) SandboxTimeZone {
	return SandboxTimeZone{set: true, fixed: true, name: name, offsetSeconds: offsetSeconds}
}

// SleepMode is what `time.sleep()` and `asyncio.sleep()` do. Use
// [SleepSystem], [SleepHost] or [SleepZero].
type SleepMode struct {
	kind string
	max  time.Duration
}

var (
	// SleepHost hands every sleep to the OS-call handler as "time.sleep" or
	// "asyncio.sleep", with the delay in seconds as a float argument and no
	// cap. The handler does the waiting (or not); its answer is discarded.
	// "asyncio.sleep" may be answered with [Pending] and completed through
	// the [FutureResolver], which lets sibling tasks run meanwhile.
	SleepHost = SleepMode{kind: "host"}
	// SleepZero makes every sleep return at once.
	SleepZero = SleepMode{kind: "zero"}
)

// SleepSystem makes sleeps really wait, each capped at maxPerCall (zero means
// monty's default of ten seconds); a longer request is shortened, not
// refused. The bridge performs the wait itself, without the OS-call handler,
// and stops it when the context is cancelled. Time spent sleeping does not
// count towards [Limits.MaxDuration]; bound it with [Limits.MaxTotalSleep].
func SleepSystem(maxPerCall time.Duration) SleepMode {
	return SleepMode{kind: "system", max: maxPerCall}
}

// RandomStart is where an unseeded `random` generator gets its first state.
// Use [RandomSystem], [RandomHost] or one of the RandomSeed constructors.
type RandomStart struct {
	kind string
	seed map[string]any
}

var (
	// RandomSystem seeds from the sandbox's own entropy, which the bridge
	// draws from crypto/rand. It is the default.
	RandomSystem = RandomStart{kind: "system"}
	// RandomHost asks the OS-call handler for the state on the first draw,
	// as an "os.urandom" call for 2496 bytes.
	RandomHost = RandomStart{kind: "host"}
)

// RandomSeedInt starts the module-level generator exactly as
// `random.seed(seed)` would; unseeded `random.Random()` instances take
// deterministic states derived from the same seed.
func RandomSeedInt(seed *big.Int) RandomStart {
	return RandomStart{kind: "seed", seed: map[string]any{"int": seed.String()}}
}

// RandomSeedString is [RandomSeedInt] for `random.seed(str)`.
func RandomSeedString(seed string) RandomStart {
	return RandomStart{kind: "seed", seed: map[string]any{"str": seed}}
}

// RandomSeedBytes is [RandomSeedInt] for `random.seed(bytes)`.
func RandomSeedBytes(seed []byte) RandomStart {
	return RandomStart{kind: "seed", seed: map[string]any{"bytes": base64.StdEncoding.EncodeToString(seed)}}
}

// runConfig is what monty_configure receives.
type runConfig struct {
	Cwd      string    `json:"cwd,omitempty"`
	OsPolicy *OsPolicy `json:"os_policy,omitempty"`
}

// WithCwd sets the sandbox's working directory, an absolute POSIX path. It is
// what `os.getcwd()` reports, what `os.chdir()` starts from, and what relative
// paths resolve against: the interpreter makes every path absolute before an
// OS call reaches the handler, so OsCall.Args never carries a relative path.
// The default is "/".
//
// The directory is virtual — the sandbox never touches it — so it only has
// to mean something to the OS-call handler, which is what gives paths their
// meaning.
func WithCwd(dir string) ExecuteOption {
	return func(c *executeConfig) { c.run.Cwd = dir }
}

// WithOsPolicy sets how clocks, sleeps and random state are answered; see
// [OsPolicy].
func WithOsPolicy(p OsPolicy) ExecuteOption {
	return func(c *executeConfig) { c.run.OsPolicy = &p }
}

// WithStderrFunc sets a callback for output written to stderr, which is what
// `print(..., file=sys.stderr)` does. Without one, stderr output goes to the
// [WithPrintFunc] callback, so nothing printed is lost.
func WithStderrFunc(fn func(string)) ExecuteOption {
	return func(c *executeConfig) { c.stderrFunc = fn }
}
