package montygo

import (
	"context"
	"errors"
	"math/big"
	"strings"
	"testing"
)

// run executes prog with the given OS-call handler and returns printed output.
func run(t *testing.T, r *Runner, prog string, os OsCallFunc, opts ...ExecuteOption) (string, error) {
	t.Helper()
	var out strings.Builder
	opts = append(opts,
		// The print callback's text already carries its trailing newline.
		WithPrintFunc(func(s string) { out.WriteString(s) }),
	)
	if os != nil {
		opts = append(opts, WithOsCallFunc(os))
	}
	_, err := r.Execute(context.Background(), prog, nil, opts...)
	return out.String(), err
}

// TestOpenReadWrite covers the builtin open(). The interpreter implements real
// file objects but never held one before: it answers an `open` OS call by
// building its file wrapper from a FileHandle, and without a way to send one
// the host could only return a plain value, which failed on the next line with
// "'str' object has no attribute 'read'".
func TestOpenReadWrite(t *testing.T) {
	r := newRunner(t)
	files := map[string]string{"data.txt": "line one\nline two\nline three\n"}

	handler := func(ctx context.Context, call *OsCall) (any, error) {
		path, _ := call.Args[0].(string)
		switch call.Function {
		case "open":
			mode, _ := call.Args[1].(string)
			switch {
			case strings.HasPrefix(mode, "w"):
				files[path] = ""
			case strings.HasPrefix(mode, "a"):
				if _, ok := files[path]; !ok {
					files[path] = ""
				}
			default:
				if _, ok := files[path]; !ok {
					return nil, &PyError{Type: "FileNotFoundError", Message: path}
				}
			}
			return FileHandle{Path: path, Mode: mode}, nil
		case "Path.read_text":
			return files[path], nil
		case "Path.read_bytes":
			return Bytes(files[path]), nil
		case "Path.write_text":
			data, _ := call.Args[1].(string)
			files[path] = data
			return len(data), nil
		case "Path.append_text":
			data, _ := call.Args[1].(string)
			files[path] += data
			return len(data), nil
		}
		return nil, nil
	}

	cases := []struct {
		name string
		prog string
		want string
	}{
		{"read", `print(open("data.txt").read())`, "line one\nline two\nline three\n"},
		{"type", `print(type(open("data.txt")).__name__)`, "TextIOWrapper"},
		{"context manager", `with open("data.txt") as f: print(f.read())`, "line one\nline two\nline three\n"},
		{"readline", `f = open("data.txt")
print(repr(f.readline()))`, `'line one\n'`},
		{"readlines", `print(open("data.txt").readlines())`, `['line one\n', 'line two\n', 'line three\n']`},
		{"sized read and tell", `f = open("data.txt")
print(repr(f.read(4)), f.tell())`, `'line' 4`},
		{"attributes", `f = open("data.txt")
print(f.name, f.mode, f.closed)`, "data.txt r False"},
		{"binary read", `print(open("data.txt", "rb").read(8))`, `b'line one'`},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			out, err := run(t, r, c.prog, handler)
			if err != nil {
				t.Fatalf("execute: %v", err)
			}
			if got := strings.TrimSuffix(out, "\n"); got != c.want {
				t.Errorf("got %q, want %q", got, c.want)
			}
		})
	}

	t.Run("write", func(t *testing.T) {
		if _, err := run(t, r, `with open("out.txt", "w") as f: f.write("written")`, handler); err != nil {
			t.Fatalf("execute: %v", err)
		}
		if files["out.txt"] != "written" {
			t.Errorf("host file is %q, want %q", files["out.txt"], "written")
		}
	})
}

// TestPyErrorIsCatchable covers a host failure arriving as a real Python
// exception. Before, any error from a handler tore the whole execution down,
// so sandboxed code could not catch even an ordinary missing file.
func TestPyErrorIsCatchable(t *testing.T) {
	r := newRunner(t)
	handler := func(ctx context.Context, call *OsCall) (any, error) {
		return nil, &PyError{Type: "FileNotFoundError", Message: "no such file"}
	}

	prog := `from pathlib import Path
try:
    Path("missing.txt").read_text()
except FileNotFoundError as e:
    print("caught:", e)`
	out, err := run(t, r, prog, handler)
	if err != nil {
		t.Fatalf("a PyError should be catachable in the guest, not fail the run: %v", err)
	}
	if want := "caught: no such file"; !strings.Contains(out, want) {
		t.Errorf("got %q, want it to contain %q", out, want)
	}
}

// TestPlainErrorStillFailsRun pins the other half of the contract: an ordinary
// error is a host-side fault and ends the execution.
func TestPlainErrorStillFailsRun(t *testing.T) {
	r := newRunner(t)
	handler := func(ctx context.Context, call *OsCall) (any, error) {
		return nil, context.DeadlineExceeded
	}
	prog := `from pathlib import Path
try:
    Path("x").read_text()
except Exception:
    print("should not be reached")`
	if _, err := run(t, r, prog, handler); err == nil {
		t.Fatal("expected the run to fail, got nil")
	}
}

// TestTypedReturnValues covers the values that have no plain-JSON spelling.
// Each of these used to be impossible to send: bytes arrived as a list or a
// str, and a date or a stat result could only be faked with a string that had
// none of the attributes Python then asks for.
func TestTypedReturnValues(t *testing.T) {
	r := newRunner(t)
	cases := []struct {
		name string
		prog string
		ret  any
		want string
	}{
		{
			name: "bytes",
			prog: "from pathlib import Path\nb = Path('f').read_bytes()\nprint(type(b).__name__, b)",
			ret:  Bytes("hi"),
			want: "bytes b'hi'",
		},
		{
			name: "stat result",
			prog: "from pathlib import Path\ns = Path('f').stat()\nprint(s.st_size, s.st_mode, s.st_mtime)",
			ret:  StatResult{StMode: ModeFile | 0o644, StSize: 1234, StMtime: 99},
			want: "1234 33188 99.0",
		},
		{
			name: "date",
			prog: "from datetime import date\nd = date.today()\nprint(type(d).__name__, d.year, d.month, d.day)",
			ret:  Date{Year: 2026, Month: 9, Day: 7},
			want: "date 2026 9 7",
		},
		{
			name: "datetime",
			prog: "from datetime import datetime\nd = datetime.now()\nprint(type(d).__name__, d.year, d.hour, d.minute)",
			ret:  DateTime{Year: 2026, Month: 9, Day: 7, Hour: 13, Minute: 45},
			want: "datetime 2026 13 45",
		},
		{
			name: "path",
			prog: "from pathlib import Path\np = Path('f').resolve()\nprint(type(p).__name__, p.name)",
			ret:  Path("/abs/dir/f"),
			want: "PosixPath f",
		},
		{
			name: "tuple",
			prog: "import os\nv = os.getenv('X')\nprint(type(v).__name__, v)",
			ret:  Tuple{1, "a"},
			want: "tuple (1, 'a')",
		},
		{
			name: "frozenset",
			prog: "import os\nv = os.getenv('X')\nprint(type(v).__name__, sorted(v))",
			ret:  FrozenSet{2, 1},
			want: "frozenset [1, 2]",
		},
		{
			name: "namedtuple",
			prog: "import os\nv = os.getenv('X')\nprint(v.alpha, v.beta)",
			ret:  NamedTuple{TypeName: "Pair", Fields: []Field{{"alpha", 1}, {"beta", "two"}}},
			want: "1 two",
		},
		{
			name: "bigint",
			prog: "import os\nv = os.getenv('X')\nprint(type(v).__name__, v + 1)",
			ret:  BigInt{new(big.Int).SetUint64(1 << 63)},
			want: "int 9223372036854775809",
		},
		{
			name: "timedelta",
			prog: "import os\nv = os.getenv('X')\nprint(type(v).__name__, v.days, v.seconds)",
			ret:  TimeDelta{Days: 3, Seconds: 60},
			want: "timedelta 3 60",
		},
		{
			name: "exception value",
			prog: "import os\nv = os.getenv('X')\nprint(type(v).__name__, v)",
			ret:  ExceptionValue{Type: "ValueError", Message: "bad"},
			want: "ValueError bad",
		},
		{
			name: "ellipsis",
			prog: "import os\nv = os.getenv('X')\nprint(v is ...)",
			ret:  Ellipsis{},
			want: "True",
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			out, err := run(t, r, c.prog, func(ctx context.Context, call *OsCall) (any, error) {
				return c.ret, nil
			})
			if err != nil {
				t.Fatalf("execute: %v", err)
			}
			if got := strings.TrimSuffix(out, "\n"); got != c.want {
				t.Errorf("got %q, want %q", got, c.want)
			}
		})
	}
}

// TestTypedArgsRoundTrip covers the other direction: a value Python hands the
// host arrives as the matching Go type rather than a Rust debug string, which
// is what a datetime or a file handle used to become.
func TestTypedArgsRoundTrip(t *testing.T) {
	r := newRunner(t)
	var got any
	prog := `from pathlib import Path
from datetime import timedelta
Path("f").write_bytes(b"xy")`
	_, err := run(t, r, prog, func(ctx context.Context, call *OsCall) (any, error) {
		if call.Function == "Path.write_bytes" {
			got = call.Args[1]
		}
		return 2, nil
	})
	if err != nil {
		t.Fatalf("execute: %v", err)
	}
	b, ok := got.(Bytes)
	if !ok {
		t.Fatalf("write_bytes payload is %T, want montygo.Bytes", got)
	}
	if string(b) != "xy" {
		t.Errorf("got %q, want %q", b, "xy")
	}
}

// TestUnknownTagIsPlainDict pins that the reserved key is not a trap: a host
// dict that happens to carry it, or a tag from a newer build, is still data.
func TestUnknownTagIsPlainDict(t *testing.T) {
	r := newRunner(t)
	out, err := run(t, r, "import os\nv = os.getenv('X')\nprint(type(v).__name__, v['__monty__'])",
		func(ctx context.Context, call *OsCall) (any, error) {
			return map[string]any{"__monty__": "something-from-the-future"}, nil
		})
	if err != nil {
		t.Fatalf("execute: %v", err)
	}
	if want := "dict something-from-the-future"; strings.TrimSuffix(out, "\n") != want {
		t.Errorf("got %q, want %q", out, want)
	}
}

// TestPendingFutures covers async host calls: a handler returns Pending, the
// guest gets an awaitable, and the resolver completes the calls when the
// program can make no further progress. The Rust shim always supported this;
// the Go side used to reject the resulting suspension outright with "async
// futures not yet supported".
func TestPendingFutures(t *testing.T) {
	r := newRunner(t)

	// Both fetches go out before either is answered, which is what makes this
	// a concurrency test rather than two sequential calls.
	prog := `import asyncio

async def main():
    a, b = await asyncio.gather(fetch(1), fetch(2))
    return a + b

print(asyncio.run(main()))`

	var pendingSeen [][]uint32
	out, err := run(t, r, prog, nil,
		WithExternalFunc(func(ctx context.Context, call *FunctionCall) (any, error) {
			return Pending{}, nil
		}, Func("fetch", "n")),
		WithFutureResolver(func(ctx context.Context, callIDs []uint32) (map[uint32]any, error) {
			pendingSeen = append(pendingSeen, callIDs)
			results := make(map[uint32]any, len(callIDs))
			for _, id := range callIDs {
				results[id] = 10
			}
			return results, nil
		}),
	)
	if err != nil {
		t.Fatalf("execute: %v", err)
	}
	if want := "20"; strings.TrimSuffix(out, "\n") != want {
		t.Errorf("got %q, want %q", out, want)
	}
	if len(pendingSeen) == 0 {
		t.Fatal("the resolver was never called")
	}
	if len(pendingSeen[0]) != 2 {
		t.Errorf("first resolve saw %d pending calls, want both outstanding at once: %v",
			len(pendingSeen[0]), pendingSeen)
	}
}

// TestPendingWithoutResolver pins the diagnostic for the mistake this API
// makes easy: returning Pending with nothing configured to complete it.
func TestPendingWithoutResolver(t *testing.T) {
	r := newRunner(t)
	prog := `import asyncio

async def main():
    return await fetch(1)

print(asyncio.run(main()))`
	_, err := run(t, r, prog, nil,
		WithExternalFunc(func(ctx context.Context, call *FunctionCall) (any, error) {
			return Pending{}, nil
		}, Func("fetch", "n")),
	)
	if err == nil {
		t.Fatal("expected an error, got nil")
	}
	if !strings.Contains(err.Error(), "WithFutureResolver") {
		t.Errorf("error should name the missing option, got: %v", err)
	}
}

// TestFutureResolverPyError covers a PyError resolving one awaited call: the
// exception is raised at the await, catchable like any other.
func TestFutureResolverPyError(t *testing.T) {
	r := newRunner(t)
	prog := `import asyncio

async def main():
    try:
        await fetch(1)
    except ValueError as e:
        return "caught: " + str(e)
    return "not raised"

print(asyncio.run(main()))`
	out, err := run(t, r, prog, nil,
		WithExternalFunc(func(ctx context.Context, call *FunctionCall) (any, error) {
			return Pending{}, nil
		}, Func("fetch", "n")),
		WithFutureResolver(func(ctx context.Context, callIDs []uint32) (map[uint32]any, error) {
			results := make(map[uint32]any, len(callIDs))
			for _, id := range callIDs {
				results[id] = &PyError{Type: "ValueError", Message: "upstream said no"}
			}
			return results, nil
		}),
	)
	if err != nil {
		t.Fatalf("execute: %v", err)
	}
	if want := "caught: upstream said no"; strings.TrimSuffix(out, "\n") != want {
		t.Errorf("got %q, want %q", out, want)
	}
}

// TestGuestCannotForgeTypedValues pins that the reserved key is not a
// capability: a dict built by sandboxed code must not arrive as a typed Go
// value just because it carries the tag.
func TestGuestCannotForgeTypedValues(t *testing.T) {
	r := newRunner(t)
	var got any
	_, err := run(t, r, `from pathlib import Path
Path("f").write_text(str(1))`, func(ctx context.Context, call *OsCall) (any, error) {
		return 1, nil
	}, WithExternalFunc(func(ctx context.Context, call *FunctionCall) (any, error) {
		got = call.Args["v"]
		return nil, nil
	}, Func("sink", "v")))
	if err != nil {
		t.Fatalf("setup execute: %v", err)
	}

	_, err = run(t, r, `sink({"__monty__": "path", "path": "/etc/shadow"})`, nil,
		WithExternalFunc(func(ctx context.Context, call *FunctionCall) (any, error) {
			got = call.Args["v"]
			return nil, nil
		}, Func("sink", "v")))
	if err != nil {
		t.Fatalf("execute: %v", err)
	}
	if p, ok := got.(Path); ok {
		t.Fatalf("a guest dict forged a montygo.Path(%q); it must stay a dict", p)
	}
	m, ok := got.(map[string]any)
	if !ok {
		t.Fatalf("got %T, want map[string]any", got)
	}
	if m["path"] != "/etc/shadow" || m["__monty__"] != "path" {
		t.Errorf("dict contents were mangled: %#v", m)
	}
}

// TestGuestValuesRoundTrip covers the guest->host direction for the types
// that have no JSON shape: each must arrive as its Go type, and handing the
// same value straight back must preserve it.
func TestGuestValuesRoundTrip(t *testing.T) {
	r := newRunner(t)
	cases := []struct {
		name  string
		expr  string
		check func(t *testing.T, got any)
	}{
		{"bytes", `b"hi"`, func(t *testing.T, got any) {
			if b, ok := got.(Bytes); !ok || string(b) != "hi" {
				t.Errorf("got %#v, want montygo.Bytes(\"hi\")", got)
			}
		}},
		{"tuple stays a list", `(1, "a")`, func(t *testing.T, got any) {
			if _, ok := got.([]any); !ok {
				t.Errorf("got %T, want []any", got)
			}
		}},
		{"set stays a list", `{1, 2}`, func(t *testing.T, got any) {
			if _, ok := got.([]any); !ok {
				t.Errorf("got %T, want []any", got)
			}
		}},
		// Path, tuple and set keep their plain JSON shapes on the way out
		// (see the direction note in values.go): every OS call carries a path
		// as a string, and hosts read it as one.
		{"path stays a string", `Path("/a/b")`, func(t *testing.T, got any) {
			if s, ok := got.(string); !ok || s != "/a/b" {
				t.Errorf("got %#v, want the string \"/a/b\"", got)
			}
		}},
		{"date", `date(2026, 9, 7)`, func(t *testing.T, got any) {
			d, ok := got.(Date)
			if !ok {
				t.Fatalf("got %T, want montygo.Date", got)
			}
			if d.Year != 2026 || d.Month != 9 || d.Day != 7 {
				t.Errorf("got %+v", d)
			}
		}},
		{"timedelta", `timedelta(days=2)`, func(t *testing.T, got any) {
			d, ok := got.(TimeDelta)
			if !ok {
				t.Fatalf("got %T, want montygo.TimeDelta", got)
			}
			if d.Days != 2 {
				t.Errorf("got %+v", d)
			}
		}},
		{"bigint", `2 ** 70`, func(t *testing.T, got any) {
			b, ok := got.(BigInt)
			if !ok {
				t.Fatalf("got %T, want montygo.BigInt", got)
			}
			if want := "1180591620717411303424"; b.String() != want {
				t.Errorf("got %s, want %s", b, want)
			}
		}},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			// Sent out through an external call, then returned unchanged and
			// printed, so both directions are exercised at once.
			prog := "from pathlib import Path\nfrom datetime import date, timedelta\nprint(echo(" + c.expr + "))"
			var got any
			out, err := run(t, r, prog, nil,
				WithExternalFunc(func(ctx context.Context, call *FunctionCall) (any, error) {
					got = call.Args["v"]
					return got, nil
				}, Func("echo", "v")),
			)
			if err != nil {
				t.Fatalf("execute: %v", err)
			}
			c.check(t, got)
			if strings.TrimSpace(out) == "" {
				t.Error("the value did not survive the trip back to Python")
			}
		})
	}
}

// TestArgsJSONIsPlain pins that ArgsJSON keeps its documented contract: plain
// JSON for a tool handler, not this bridge's tagged wire form.
func TestArgsJSONIsPlain(t *testing.T) {
	r := newRunner(t)
	var argsJSON string
	_, err := run(t, r, `echo(b"hi")`, nil,
		WithExternalFunc(func(ctx context.Context, call *FunctionCall) (any, error) {
			argsJSON = call.ArgsJSON()
			return nil, nil
		}, Func("echo", "v")),
	)
	if err != nil {
		t.Fatalf("execute: %v", err)
	}
	if strings.Contains(argsJSON, montyTag) {
		t.Errorf("ArgsJSON leaked the internal tag: %s", argsJSON)
	}
	if want := `{"v":"aGk="}`; argsJSON != want {
		t.Errorf("got %s, want %s", argsJSON, want)
	}
}

// TestFutureResolverPlainErrorFailsRun pins that the async path treats a
// non-PyError the same as the synchronous one: a host fault, not a value.
func TestFutureResolverPlainErrorFailsRun(t *testing.T) {
	r := newRunner(t)
	prog := `import asyncio

async def main():
    return await fetch(1)

print(asyncio.run(main()))`
	_, err := run(t, r, prog, nil,
		WithExternalFunc(func(ctx context.Context, call *FunctionCall) (any, error) {
			return Pending{}, nil
		}, Func("fetch", "n")),
		WithFutureResolver(func(ctx context.Context, callIDs []uint32) (map[uint32]any, error) {
			results := make(map[uint32]any, len(callIDs))
			for _, id := range callIDs {
				results[id] = errors.New("network exploded")
			}
			return results, nil
		}),
	)
	if err == nil {
		t.Fatal("expected the run to fail, got nil")
	}
	if !strings.Contains(err.Error(), "network exploded") {
		t.Errorf("error should carry the cause, got: %v", err)
	}
}

// TestOsCallPendingIsRejected pins the diagnostic for Pending from an OS-call
// handler. There is no await point behind an OS call, so a future there leaves
// a coroutine nobody awaits — previously the guest silently printed
// "<coroutine external_future(0)>" instead of the file's contents.
func TestOsCallPendingIsRejected(t *testing.T) {
	r := newRunner(t)
	_, err := run(t, r, `from pathlib import Path
print(Path("f").read_text())`,
		func(ctx context.Context, call *OsCall) (any, error) {
			return Pending{}, nil
		},
	)
	if err == nil {
		t.Fatal("expected an error, got nil")
	}
	if !strings.Contains(err.Error(), "only external functions support") {
		t.Errorf("error should explain why, got: %v", err)
	}
}
