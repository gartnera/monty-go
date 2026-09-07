// Package montygo provides a Go wrapper around the Pydantic Monty Python
// interpreter compiled to WebAssembly. It uses wazero (pure Go, no CGO) to
// execute Python code in a sandboxed environment with pause/resume support
// for external function calls.
package montygo

import (
	"context"
	_ "embed"
	"encoding/json"
	"fmt"
	"time"

	"github.com/tetratelabs/wazero"
	"github.com/tetratelabs/wazero/imports/wasi_snapshot_preview1"
)

//go:embed monty.wasm
var montyWasm []byte

// Runner is a compiled Monty WASM runtime ready to execute Python code.
// Create one Runner and reuse it across multiple Execute calls.
// Each Execute call gets its own isolated WASM instance.
type Runner struct {
	runtime  wazero.Runtime
	compiled wazero.CompiledModule
}

// DefaultMaxSuspensions is the number of host calls one execution may make
// when Limits.MaxSuspensions is zero, matching monty's own default.
const DefaultMaxSuspensions = 1000

// Limits configures resource limits for Python execution.
type Limits struct {
	MaxMemoryBytes uint64        `json:"max_memory,omitempty"`
	MaxDuration    time.Duration `json:"-"`
	// MaxAllocations is retained for source compatibility and has no effect:
	// monty dropped allocation counting in favour of MaxMemoryBytes and
	// MaxSuspensions.
	MaxAllocations    uint64 `json:"max_allocations,omitempty"`
	MaxRecursionDepth uint32 `json:"max_recursion_depth,omitempty"`
	// MaxSuspensions bounds how many host calls (external functions and OS
	// calls) one execution may make. It backstops code that loops on host
	// calls, which does not advance MaxDuration. Zero uses monty's default
	// of 1000.
	MaxSuspensions uint64 `json:"max_suspensions,omitempty"`
}

// MarshalJSON implements custom JSON marshaling for Limits.
func (l Limits) MarshalJSON() ([]byte, error) {
	type alias struct {
		MaxAllocations    *uint64 `json:"max_allocations,omitempty"`
		MaxDurationMs     *uint64 `json:"max_duration_ms,omitempty"`
		MaxMemory         *uint64 `json:"max_memory,omitempty"`
		MaxRecursionDepth *uint32 `json:"max_recursion_depth,omitempty"`
		MaxSuspensions    *uint64 `json:"max_suspensions,omitempty"`
	}
	a := alias{}
	if l.MaxAllocations > 0 {
		v := l.MaxAllocations
		a.MaxAllocations = &v
	}
	if l.MaxDuration > 0 {
		v := uint64(l.MaxDuration.Milliseconds())
		a.MaxDurationMs = &v
	}
	if l.MaxMemoryBytes > 0 {
		v := l.MaxMemoryBytes
		a.MaxMemory = &v
	}
	if l.MaxRecursionDepth > 0 {
		v := l.MaxRecursionDepth
		a.MaxRecursionDepth = &v
	}
	if l.MaxSuspensions > 0 {
		v := l.MaxSuspensions
		a.MaxSuspensions = &v
	}
	return json.Marshal(a)
}

// FunctionCall contains information about an external function call from Python.
// Args contains all arguments merged into a single map — positional args are
// mapped to parameter names (registered via FuncDef) and kwargs are merged in.
type FunctionCall struct {
	Name   string
	Args   map[string]any
	CallID uint32
}

// ArgsJSON returns Args serialized as a JSON string, suitable for passing
// directly to tool handlers that accept JSON argument strings.
//
// The typed values (see values.go) are flattened to their plain JSON shapes
// first — bytes to a base64 string, a tuple or set to an array, a date to its
// ISO form — because a tool handler expects ordinary JSON, not this bridge's
// internal tagging. Read fc.Args directly when the distinction matters.
func (fc *FunctionCall) ArgsJSON() string {
	if len(fc.Args) == 0 {
		return "{}"
	}
	plain := make(map[string]any, len(fc.Args))
	for k, v := range fc.Args {
		plain[k] = plainValue(v)
	}
	b, err := json.Marshal(plain)
	if err != nil {
		return "{}"
	}
	return string(b)
}

// FuncDef defines an external Python function with its parameter names.
// Parameter names enable positional-to-keyword argument mapping in the WASM layer.
type FuncDef struct {
	Name   string   `json:"name"`
	Params []string `json:"params,omitempty"`
}

// Func creates a FuncDef with the given name and parameter names.
func Func(name string, params ...string) FuncDef {
	return FuncDef{Name: name, Params: params}
}

// OsCall contains information about an OS-level operation from Python.
type OsCall struct {
	Function string
	Args     []any
	Kwargs   map[string]any
	CallID   uint32
}

// ExternalFunc is called when Python code calls an external function.
type ExternalFunc func(ctx context.Context, call *FunctionCall) (any, error)

// OsCallFunc is called when Python code performs an OS operation.
type OsCallFunc func(ctx context.Context, call *OsCall) (any, error)

// FutureResolver is called when every branch of the sandboxed program is
// blocked awaiting host work, so the interpreter can make no further progress
// without results.
//
// It receives the call IDs of every external call still outstanding — the ones
// whose handler returned [Pending] — and must return a value for at least one
// of them, keyed by call ID. Resolving a subset is fine and often the point:
// return whichever finished first and the resolver is called again for the
// rest. A value may be a [PyError] to make that awaited call raise inside the
// guest.
//
// Returning no results at all would leave the program blocked forever, so it
// is treated as an error rather than looping.
type FutureResolver func(ctx context.Context, callIDs []uint32) (map[uint32]any, error)

// Pending is what an [ExternalFunc] returns to service a call
// asynchronously instead of answering it now.
//
// The guest gets an awaitable rather than a value, and execution carries on
// until it actually needs the result. When everything is blocked, the
// [FutureResolver] configured with [WithFutureResolver] is asked for results
// by call ID. This is what makes `asyncio.gather` over host calls concurrent:
// several calls go out, all return Pending, and the host completes them in
// whatever order it likes.
//
// Returning Pending without a resolver configured is an error — nothing would
// ever complete the call.
//
//	func handle(ctx context.Context, call *montygo.FunctionCall) (any, error) {
//	    go doWork(call.CallID)   // completes later
//	    return montygo.Pending{}, nil
//	}
type Pending struct{}

// MarshalJSON implements [json.Marshaler].
func (Pending) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("future", nil))
}

// ExecuteOption configures a single Execute call.
type ExecuteOption func(*executeConfig)

type executeConfig struct {
	externalFunc   ExternalFunc
	osCallFunc     OsCallFunc
	futureResolver FutureResolver
	limits         Limits
	printFunc      func(string)
	extFuncs       []FuncDef
}

// WithExternalFunc sets the callback for external function calls.
// Each FuncDef declares a function name and its parameter names (for
// positional-to-keyword argument mapping).
func WithExternalFunc(fn ExternalFunc, funcs ...FuncDef) ExecuteOption {
	return func(c *executeConfig) {
		c.externalFunc = fn
		c.extFuncs = funcs
	}
}

// WithOsCallFunc sets the callback for OS-level operations.
func WithOsCallFunc(fn OsCallFunc) ExecuteOption {
	return func(c *executeConfig) { c.osCallFunc = fn }
}

// WithFutureResolver sets the callback that completes external calls whose
// handler returned [Pending]. Required only if a handler ever does.
func WithFutureResolver(fn FutureResolver) ExecuteOption {
	return func(c *executeConfig) { c.futureResolver = fn }
}

// WithLimits sets resource limits for the execution.
func WithLimits(l Limits) ExecuteOption {
	return func(c *executeConfig) { c.limits = l }
}

// WithPrintFunc sets a callback for Python print() output.
func WithPrintFunc(fn func(string)) ExecuteOption {
	return func(c *executeConfig) { c.printFunc = fn }
}

// New creates a new Monty WASM runner.
// The WASM module is compiled once and reused across Execute calls.
func New() (*Runner, error) {
	ctx := context.Background()
	config := wazero.NewRuntimeConfig().
		WithCloseOnContextDone(true)
	r := wazero.NewRuntimeWithConfig(ctx, config)

	// Instantiate WASI (provides clock, random, fd_write for the WASM module).
	if _, err := wasi_snapshot_preview1.Instantiate(ctx, r); err != nil {
		r.Close(ctx)
		return nil, fmt.Errorf("montygo: failed to instantiate WASI: %w", err)
	}

	compiled, err := r.CompileModule(ctx, montyWasm)
	if err != nil {
		r.Close(ctx)
		return nil, fmt.Errorf("montygo: failed to compile WASM module: %w", err)
	}

	return &Runner{
		runtime:  r,
		compiled: compiled,
	}, nil
}

// Execute runs Python code with the given inputs and returns the result.
// Each call creates an isolated WASM instance that is cleaned up when done.
func (r *Runner) Execute(ctx context.Context, code string, inputs map[string]any, opts ...ExecuteOption) (any, error) {
	cfg := &executeConfig{}
	for _, opt := range opts {
		opt(cfg)
	}

	// Instantiate a fresh module for this execution.
	//
	// The module must be given the host's real clocks. wazero's default
	// ModuleConfig supplies a *fake* nanotime that advances by a fixed step on
	// every reading, so without WithSysNanotime the guest's sense of elapsed
	// time races far ahead of the wall clock and Limits.MaxDuration fires
	// almost immediately: a 10s limit would trip after ~120ms of real work.
	// WithSysWalltime is the same story for datetime/time in Python.
	mod, err := r.runtime.InstantiateModule(ctx, r.compiled,
		wazero.NewModuleConfig().
			WithName("").
			WithSysNanotime().
			WithSysWalltime())
	if err != nil {
		return nil, fmt.Errorf("montygo: failed to instantiate module: %w", err)
	}
	defer mod.Close(ctx)

	inst := &instance{mod: mod}
	if err := inst.resolveExports(); err != nil {
		return nil, err
	}

	return inst.execute(ctx, code, inputs, cfg)
}

// Close releases all WASM resources.
func (r *Runner) Close() error {
	return r.runtime.Close(context.Background())
}
