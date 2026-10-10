# monty-go

**Run LLM-generated Python safely from Go — no containers, no CGO, no subprocess.**

A pure-Go wrapper around [Pydantic's Monty](https://github.com/pydantic/monty) Python interpreter, compiled to WebAssembly and loaded via [wazero](https://wazero.io). Your Go agent writes Python code, monty-go executes it in a sandboxed WASM instance with sub-millisecond startup, and pauses whenever the code calls an external function so your Go code can handle it.

```bash
go get github.com/fugue-labs/monty-go
```

## Why?

LLMs work faster, cheaper, and more reliably when they write code instead of making sequential tool calls. Instead of:

```
Agent → tool_call("search", {query: "weather london"}) → result
Agent → tool_call("search", {query: "weather tokyo"})  → result
Agent → tool_call("compare", {a: result1, b: result2}) → result
```

The LLM writes:

```python
london = search(query="weather london")
tokyo = search(query="weather tokyo")
compare(a=london, b=tokyo)
```

One model call instead of three. The Python code calls your Go functions, Monty pauses at each call, your Go code executes it, and Monty resumes. No containers. No sandbox services. No `exec()`. Just a 6MB WASM binary embedded in your Go binary.

For motivation, see:
- [Programmatic Tool Calling](https://platform.claude.com/docs/en/agents-and-tools/tool-use/programmatic-tool-calling) from Anthropic
- [Code Execution with MCP](https://www.anthropic.com/engineering/code-execution-with-mcp) from Anthropic
- [Code Mode](https://blog.cloudflare.com/code-mode/) from Cloudflare
- [Smol Agents](https://github.com/huggingface/smolagents) from Hugging Face

## Quick Start

```go
package main

import (
    "context"
    "fmt"
    "log"

    montygo "github.com/fugue-labs/monty-go"
)

func main() {
    runner, err := montygo.New()
    if err != nil {
        log.Fatal(err)
    }
    defer runner.Close()

    result, err := runner.Execute(context.Background(),
        "x * 2 + y",
        map[string]any{"x": 10, "y": 5},
    )
    if err != nil {
        log.Fatal(err)
    }
    fmt.Println(result) // 25
}
```

## External Functions (Pause/Resume)

The real power is external function calls. Monty pauses execution whenever Python code calls a function you've declared, your Go callback handles it, and Monty resumes with the return value:

```go
result, err := runner.Execute(ctx,
    `
london = get_weather("London")
tokyo = get_weather("Tokyo")
f"{london['city']}: {london['temp']}°C, {tokyo['city']}: {tokyo['temp']}°C"
    `,
    nil,
    montygo.WithExternalFunc(func(ctx context.Context, call *montygo.FunctionCall) (any, error) {
        city, _ := call.Args["city"].(string)
        // Your real implementation here — HTTP call, database query, anything.
        return map[string]any{"city": city, "temp": 22}, nil
    }, montygo.Func("get_weather", "city")),
)
// result: "London: 22°C, Tokyo: 22°C"
```

Multiple functions work the same way — register them all and dispatch by name:

```go
result, err := runner.Execute(ctx, code, nil,
    montygo.WithExternalFunc(func(ctx context.Context, call *montygo.FunctionCall) (any, error) {
        switch call.Name {
        case "search":
            return doSearch(call.Args)
        case "calculate":
            return doCalculate(call.Args)
        case "store":
            return doStore(call.Args)
        default:
            return nil, fmt.Errorf("unknown function: %s", call.Name)
        }
    },
        montygo.Func("search", "query"),
        montygo.Func("calculate", "expression"),
        montygo.Func("store", "key", "value"),
    ),
)
```

## Resource Limits

Prevent runaway code with memory, time, recursion, host-call and sleep limits:

```go
result, err := runner.Execute(ctx, code, inputs,
    montygo.WithLimits(montygo.Limits{
        MaxDuration:       5 * time.Second,  // execution time, excluding host calls and sleeps
        MaxTurnDuration:   time.Second,      // execution time between two host calls
        MaxMemoryBytes:    10 * 1024 * 1024, // 10 MB
        MaxRecursionDepth: 100,
        MaxSuspensions:    500,              // host calls per run
        MaxTotalSleep:     30 * time.Second, // time.sleep()/asyncio.sleep() per run
    }),
)
```

Infinite loops, memory bombs, and deep recursion all terminate cleanly with a `*MontyError`. Go's `context.Context` deadlines are also respected — cancel the context and the WASM instance stops, even mid-sleep.

## Print Capture

Capture Python `print()` output:

```go
var output strings.Builder
_, err := runner.Execute(ctx, `print("step 1 done")`, nil,
    montygo.WithPrintFunc(func(s string) { output.WriteString(s) }),
)
fmt.Print(output.String()) // "step 1 done\n"
```

`print(..., file=sys.stderr)` goes to the `WithStderrFunc` callback when one is
set, and to the print callback otherwise, so nothing printed is lost. Output
reaches the callbacks in the order it was printed.

## OS Calls

Python filesystem and environment access routes through your Go callback:

```go
result, err := runner.Execute(ctx,
    `
from pathlib import Path
data = Path("/config/settings.json").read_text()
data
    `,
    nil,
    montygo.WithOsCallFunc(func(ctx context.Context, call *montygo.OsCall) (any, error) {
        switch call.Function {
        case "Path.read_text":
            path, _ := call.Args[0].(string)
            return readFromYourStorage(path)
        case "Path.exists":
            path, _ := call.Args[0].(string)
            return existsInYourStorage(path), nil
        default:
            return nil, fmt.Errorf("blocked: %s", call.Function)
        }
    }),
)
```

No filesystem access happens unless your callback allows it.

Paths in `call.Args` are always absolute. The interpreter keeps a virtual
working directory — `/` unless you set one with `montygo.WithCwd("/work")` —
and resolves relative paths against it before a call leaves the sandbox, so
`Path("a.txt")` arrives as `/work/a.txt`. `os.getcwd()` reports it, and
`os.chdir()` moves it after checking the target with a `Path.stat` call
(answer with a `montygo.StatResult`).

## Clocks, Sleep and Randomness

`montygo.WithOsPolicy` decides how the sandbox answers `time.time()`,
`datetime.now()`, `date.today()`, `time.sleep()`, `asyncio.sleep()` and an
unseeded `random`. The default needs nothing from you: the real clock, UTC as
the local zone, sleeps that really wait (capped at 10s each; the bridge does the
waiting and stops when the context is cancelled), and fresh entropy for
`random`. Override any part to make a run deterministic or to hand it to your
handler:

```go
montygo.WithOsPolicy(montygo.OsPolicy{
    DateTime: montygo.DateTimeFixed(time.Unix(1_700_000_000, 0)), // or DateTimeHost
    TimeZone: montygo.TimeZoneNamed("Europe/London"),            // IANA rules are compiled in
    Sleep:    montygo.SleepSystem(time.Minute),                   // or SleepZero, SleepHost
    Random:   montygo.RandomSeedInt(big.NewInt(42)),              // or RandomHost
})
```

`os.urandom(n)` always asks your `OsCallFunc`, as an `os.urandom` call: answer
with `n` bytes of `montygo.Bytes` (monty rejects any other length).

## Gollem Integration

monty-go is designed to power **code-mode** in [Gollem](https://github.com/fugue-labs/gollem), the production agent framework for Go. Instead of sequential tool calls, the LLM writes Python that calls your tools as functions — Monty executes it safely, and Gollem orchestrates the whole thing.

Here's what this looks like with Gollem:

```go
import (
    "github.com/fugue-labs/gollem"
    "github.com/fugue-labs/gollem/provider/anthropic"
    montygo "github.com/fugue-labs/monty-go"
)

// Your existing Gollem tools — search, calculate, store, whatever.
searchTool := gollem.FuncTool[SearchParams]("search", "Search the knowledge base", doSearch)
calcTool := gollem.FuncTool[CalcParams]("calculate", "Run calculations", doCalc)

// Create a code-mode tool that wraps your toolset with Monty.
// The LLM writes Python code, Monty executes it, external function calls
// route to your Go tools.
codeMode := NewCodeModeTool(runner, searchTool, calcTool)

agent := gollem.NewAgent[Analysis](anthropic.New(),
    gollem.WithTools[Analysis](codeMode),
    gollem.WithSystemPrompt[Analysis](`You have a code execution tool.
Write Python code to call the available functions. Available functions:
- search(query: str) -> dict: Search the knowledge base
- calculate(expression: str) -> float: Evaluate math expressions
Write code that calls these functions and returns the result.`),
)

result, _ := agent.Run(ctx, "Compare Q3 and Q4 revenue and calculate the growth rate")
```

With one model call, the LLM writes:

```python
q3 = search(query="Q3 revenue")
q4 = search(query="Q4 revenue")
growth = calculate(expression=f"({q4['revenue']} - {q3['revenue']}) / {q3['revenue']} * 100")
{"q3": q3, "q4": q4, "growth_rate": growth}
```

Monty pauses three times (two searches, one calculation), your Go functions handle each one, and the final result flows back through Gollem's typed output pipeline. Three tool calls in one LLM round-trip.

Why Gollem + monty-go:

| | Traditional tool calling | Code-mode with monty-go |
|---|---|---|
| **LLM calls** | One per tool use | One for all tools |
| **Latency** | N × model round-trip | 1 × model round-trip + μs execution |
| **Cost** | N × input/output tokens | 1 × input/output tokens |
| **Logic** | LLM reasons step by step | LLM writes the logic once |
| **Control flow** | None (sequential only) | Loops, conditionals, variables |
| **Error handling** | LLM must react to each failure | try/except in Python |
| **Security** | ✅ (tools are Go functions) | ✅ (WASM sandbox + your callbacks) |

Gollem gives you compile-time type safety, structured output, guardrails, cost tracking, middleware, and multi-provider support. monty-go gives you secure embedded Python execution. Together, your agents do more work per model call.

**[github.com/fugue-labs/gollem](https://github.com/fugue-labs/gollem)** — The production agent framework for Go.

## How It Works

```
┌─────────────────────────────────────────────────────────┐
│  Your Go Application                                    │
│                                                         │
│  runner, _ := montygo.New()                             │
│  result, _ := runner.Execute(ctx, code, inputs, opts)   │
│       │                                                 │
│       ▼                                                 │
│  ┌──────────────────────────────────┐                   │
│  │  wazero (pure Go WASM runtime)  │                    │
│  │                                 │                    │
│  │  ┌───────────────────────────┐  │                    │
│  │  │  monty.wasm (6.0 MB)      │  │  ◄── go:embed      │
│  │  │  Monty Python Interpreter │  │                    │
│  │  │  compiled to wasm32-wasi  │  │                    │
│  │  └──────────┬────────────────┘  │                    │
│  │             │                   │                    │
│  │     pause on external call      │                    │
│  │             │                   │                    │
│  └─────────────┼───────────────────┘                    │
│                │                                        │
│                ▼                                        │
│  ExternalFunc callback ──► your Go code ──► resume      │
│  OsCallFunc callback   ──► your Go code ──► resume      │
│  PrintFunc callback    ──► your Go code                 │
└─────────────────────────────────────────────────────────┘
```

- **No CGO.** wazero is a pure-Go WebAssembly runtime.
- **No subprocess.** The WASM binary is embedded via `go:embed` and compiled once at startup.
- **Fresh instance per call.** Each `Execute()` gets an isolated WASM instance. No state leaks between calls.
- **JSON at the boundary.** All data crossing the Go↔WASM boundary is JSON. Go types map naturally: `int`→`float64`, `string`→`string`, `bool`→`bool`, `nil`→`None`, `[]any`→`list`, `map[string]any`→`dict`. Types with no JSON spelling — `bytes`, datetimes, a stat result, an open file — travel as a tagged object and surface as the typed Go values in [Types](#types).

## API

```go
// Create a reusable runner. Compiles the WASM module once.
runner, err := montygo.New()
defer runner.Close()

// Execute Python code with inputs and options.
result, err := runner.Execute(ctx, code, inputs, opts...)

// Options:
montygo.WithExternalFunc(fn,                     // register callable functions
    montygo.Func("search", "query", "limit"),    // with named parameters
    montygo.Func("calculate", "expression"),
)
montygo.WithOsCallFunc(fn)                       // handle filesystem/env access
montygo.WithFutureResolver(fn)                   // complete calls that returned Pending
montygo.WithLimits(montygo.Limits{...})          // resource limits
montygo.WithPrintFunc(fn)                        // capture print output
montygo.WithStderrFunc(fn)                       // capture print(..., file=sys.stderr)
montygo.WithCwd("/work")                         // sandbox working directory
montygo.WithOsPolicy(montygo.OsPolicy{...})      // clocks, local zone, sleep, random

// FunctionCall provides named args (positional mapped by param name):
call.Args["query"].(string)    // access by parameter name
call.ArgsJSON()                // pre-serialized JSON string
```

### Types

Values whose Python type maps onto JSON need no wrapper:

| Python | Go (result) | Go (input) |
|--------|------------|------------|
| `int` | `float64` | `int`, `float64` |
| `float` | `float64` | `float64` |
| `str` | `string` | `string` |
| `bool` | `bool` | `bool` |
| `None` | `nil` | `nil` |
| `list` | `[]any` | `[]any` |
| `dict` | `map[string]any` | `map[string]any` |

Everything else Monty supports has no JSON spelling, so it crosses as a typed
Go value. You can return any of these from a handler:

| Python | Go |
|--------|-----|
| `bytes` | `montygo.Bytes` |
| `complex` | `montygo.Complex` |
| `tuple` | `montygo.Tuple` |
| `set`, `frozenset` | `montygo.Set`, `montygo.FrozenSet` |
| a named tuple | `montygo.NamedTuple` |
| `os.stat_result` | `montygo.StatResult` |
| `pathlib.Path` | `montygo.Path` |
| `datetime.date` | `montygo.Date` |
| `datetime.datetime` | `montygo.DateTime` |
| `datetime.time` | `montygo.Time` |
| `datetime.timedelta` | `montygo.TimeDelta` |
| `datetime.timezone` | `montygo.TimeZone` |
| an open file (`_io.TextIOWrapper`, ...) | `montygo.FileHandle` |
| an `int` beyond int64 | `montygo.BigInt` |
| an exception value | `montygo.ExceptionValue` |
| `type`, a builtin, a function | `montygo.TypeValue`, `montygo.BuiltinFunction`, `montygo.Function` |
| `...`, `NotImplemented` | `montygo.Ellipsis`, `montygo.NotImplemented` |

The distinctions matter: `bytes` is not a list of ints, a `date` is not its
string form, and `Path.read_bytes`, `Path.stat`, `date.today` and
`datetime.now` are rejected or silently wrong if answered with the wrong one.

**The two directions are not symmetric.** Every type above can be *sent*; only
the ones JSON cannot carry faithfully come back in `OsCall.Args`,
`FunctionCall.Args` or `Execute`'s result — `Bytes`, `Complex`, the date/time types,
`FileHandle`, `BigInt` (only past int64), `TypeValue`, `BuiltinFunction`,
`Function`, `Repr`, `Cycle` and `NotImplemented`. A Python tuple, set, named
tuple, exception value or `Path` arrives as its plain shape (`[]any`,
`map[string]any`, a string), because that is what handlers want: every
filesystem OS call passes its path as a string.

> **Upgrading:** `Path.write_bytes` and other binary payloads now arrive as
> `montygo.Bytes` rather than a `[]any` of ints. A handler that did
> `call.Args[1].([]any)` needs `call.Args[1].(montygo.Bytes)`. Path arguments
> are unchanged — still plain strings.

### `open()`

`open()` works, and needs one thing from your `OsCallFunc`: answer the `open`
call with a `montygo.FileHandle`. Monty holds no file descriptor — it builds
its own file object from the handle, and each later read or write arrives as an
ordinary one-shot `Path.read_text` / `Path.write_text` call. So the handler
performs the open-time effect (truncate for `"w"`, create for `"a"`, an
existence check for `"r"`) and returns the handle:

```go
case "open":
    path, _ := call.Args[0].(string)
    mode, _ := call.Args[1].(string)
    if err := yourStorage.PrepareOpen(path, mode); err != nil {
        return nil, &montygo.PyError{Type: "FileNotFoundError", Message: path}
    }
    return montygo.FileHandle{Path: path, Mode: mode}, nil
```

Python then gets a real file: `read()`, `read(n)`, `readline()`,
`readlines()`, `write()`, `seek()`, `tell()`, `close()`, `with open(...) as f`,
and `.name` / `.mode` / `.closed`. Iterating a file (`for line in f`) is not
supported upstream — use `readlines()`.

### Raising Python exceptions from a handler

An ordinary `error` from a handler is a host-side fault: it ends the execution
and `Execute` returns it. To fail the *Python* call instead — so sandboxed code
can catch it — return a `*montygo.PyError`:

```go
return nil, &montygo.PyError{Type: "FileNotFoundError", Message: "no such file: " + path}
```

```python
try:
    data = open("missing.txt").read()
except FileNotFoundError:
    data = ""
```

`Type` is any Python exception name; an empty or unrecognized one raises
`OSError`. A `PyError` is found anywhere in the error chain, so wrapping it in
context still reaches Python as the right exception.

### Async host calls

A handler can service a call asynchronously by returning `montygo.Pending{}`:
the guest gets an awaitable and execution carries on. When every branch is
blocked, the `montygo.FutureResolver` you set with `WithFutureResolver` is
handed the outstanding call IDs and returns results for any of them. This is
what makes `asyncio.gather` over host calls actually concurrent — several calls
go out, all return `Pending`, and you complete them in any order. Resolving a
subset is fine; the resolver is called again for the rest. A `*PyError` as a
result raises at that `await`; any other `error` ends the run, as it does in a
synchronous handler.

`Pending` is for external functions, plus the one OS call the guest awaits:
`asyncio.sleep` under `montygo.SleepHost`. Any other OS call has no await point
in the sandboxed code — `Path.read_text()` is a plain call — so block in the
handler instead if the work is slow.

### Errors

Python exceptions become `*montygo.MontyError`:

```go
result, err := runner.Execute(ctx, "1 / 0", nil)
var me *montygo.MontyError
if errors.As(err, &me) {
    fmt.Println(me.Message) // "Traceback... ZeroDivisionError: division by zero"
}
```

## What Monty Can Do

Tracks upstream [Monty v1.1.0](https://github.com/pydantic/monty/releases/tag/v1.1.0).

- Arithmetic, string operations, f-strings, `%`-formatting, `format()`, slicing, `complex` numbers
- Functions, lambdas, closures, decorators on functions and classes; `eval()`, `exec()` and `locals()`
- `for`/`while` loops, `if`/`elif`/`else`, `break`/`continue`
- `try`/`except`/`finally`/`else`, `raise`, exception hierarchy
- List/dict/set comprehensions, dict/set view operators
- `range`, `len`, `sum`, `min`, `max`, `sorted`, `reversed`, `enumerate`, `zip`, `map`, `filter`, `all`, `any`, `getattr`
- `isinstance`, `type`, `int()`, `float()`, `str()`, `bool()`, `abs()`
- `print()` with `sep` and `end` kwargs
- PEP 448 generalized unpacking (`*args`, `**kwargs` in calls, literals, etc.)
- Nested and augmented subscript assignment (`a[i][j] = v`, `a[i] += 1`)
- Tuple comparison (`<`, `>`, `<=`, `>=`)
- Multi-module imports (`import a, b, c`)
- Stdlib modules: `asyncio`, `base64`, `binascii`, `collections`, `copy`, `dataclasses`, `datetime` (with time zones), `functools`, `itertools`, `json`, `math`, `os`, `pathlib`, `random`, `re`, `sys`, `time`, `typing`, `unicodedata` — several are partial; see upstream's [limitations](https://github.com/pydantic/monty/tree/v1.1.0/docs/limitations)
- `import os`, `from pathlib import Path`, and the `open()` builtin with real file objects (routed through OsCallFunc), relative to a working directory (`os.getcwd()`/`os.chdir()`)
- `print(..., file=sys.stderr)` as its own stream
- Async host calls: a handler returns `Pending` and `asyncio.gather` runs them concurrently
- Host failures as catchable Python exceptions (`PyError`)
- Class definitions and methods; dataclasses
- Context managers (`with ...`)
- Class instances flow through external function calls (args, returns, and method calls surface with `method_call=true`)
- Resource limits: time (per run and per turn), memory, recursion depth, suspensions (host calls per run) and total sleep

## What Monty Cannot Do

- Class inheritance (`class Foo(Bar):` is rejected at parse time), `super()`, and the `@classmethod`/`@staticmethod`/`@property` decorators
- Generator functions (`yield`; generator expressions materialize to lists), `match` statements, `del`, `async with`/`async for`, exception groups, PEP 695 type aliases
- Ordering comparisons between `Path` objects (`sorted(Path(".").iterdir())` raises `TypeError`; sort the strings)
- Rest of stdlib and all third-party libraries
- `float('inf')` / `float('nan')` (JSON serialization limitation in this bridge)

## Tests

End-to-end tests covering every testable scenario from Monty's core test suite:

```bash
make test
```

Covers: basic expressions, print variants, all exception types, data type round-tripping, external functions (args, kwargs, mixed, complex types, chaining, loops), input handling and scoping, resource limits (timeout, recursion, memory, allocations), OS calls, builtins, control flow, lambdas/closures, and execution isolation.

## Building from Source

Requires Rust 1.99+ with the `wasm32-wasip1` target, and Go 1.23+:

```bash
rustup target add wasm32-wasip1
make build  # compiles Rust → WASM, copies to monty.wasm
make test   # builds and runs Go tests
```

## Acknowledgments

monty-go exists because of [Monty](https://github.com/pydantic/monty), created by [Samuel Colvin](https://github.com/samuelcolvin) and the [Pydantic](https://github.com/pydantic) team. Monty is a genuinely novel piece of engineering — a minimal, secure Python interpreter written from scratch in Rust, purpose-built for AI agents. The insight that LLMs should write code instead of making sequential tool calls, and that you need a safe interpreter (not a container) to execute it, is what makes code-mode possible.

Samuel and the Pydantic team have a track record of building foundational tools that the whole ecosystem builds on — [Pydantic](https://github.com/pydantic/pydantic), [Pydantic AI](https://github.com/pydantic/pydantic-ai), [Logfire](https://github.com/pydantic/logfire), and now Monty. This project is a Go bridge to their work, and we're grateful they built it.

## License

MIT
