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
func (fc *FunctionCall) ArgsJSON() string {
	if len(fc.Args) == 0 {
		return "{}"
	}
	b, err := json.Marshal(fc.Args)
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

// ExecuteOption configures a single Execute call.
type ExecuteOption func(*executeConfig)

type executeConfig struct {
	externalFunc ExternalFunc
	osCallFunc   OsCallFunc
	limits       Limits
	printFunc    func(string)
	extFuncs     []FuncDef
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
	mod, err := r.runtime.InstantiateModule(ctx, r.compiled,
		wazero.NewModuleConfig().WithName(""))
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
