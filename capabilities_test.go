package montygo

// Capabilities monty gained in v0.0.23 that the v0.0.11 shim could not run:
// class definitions, a wider stdlib, context managers, and the pathlib
// surface of the OS-call protocol.

import (
	"context"
	"reflect"
	"testing"
)

func TestCapabilityClassDefinition(t *testing.T) {
	r := newRunner(t)
	code := `
class Point:
    def __init__(self, x, y):
        self.x = x
        self.y = y
    def dist2(self):
        return self.x * self.x + self.y * self.y

Point(3, 4).dist2()
`
	assertResult(t, r, code, nil, float64(25))
}

func TestCapabilityStdlibImports(t *testing.T) {
	r := newRunner(t)
	code := `
import json, re, math
d = json.loads('{"a": [1, 2, 3]}')
m = re.findall(r"\d+", "a1b22c333")
[sum(d["a"]), len(m), math.floor(2.7)]
`
	result, err := r.Execute(context.Background(), code, nil)
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if !reflect.DeepEqual(result, []any{float64(6), float64(3), float64(2)}) {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestCapabilityWithStatement(t *testing.T) {
	r := newRunner(t)
	code := `
class Ctx:
    def __enter__(self):
        return 7
    def __exit__(self, *a):
        return False

with Ctx() as v:
    out = v * 2
out
`
	assertResult(t, r, code, nil, float64(14))
}

func TestCapabilityPathlibRoundTrip(t *testing.T) {
	r := newRunner(t)
	files := map[string]string{"/w/in.txt": "hello"}
	seen := []string{}
	code := `
from pathlib import Path
p = Path("/w/in.txt")
text = p.read_text()
Path("/w/out.txt").write_text(text.upper())
Path("/w/out.txt").read_text()
`
	result, err := r.Execute(context.Background(), code, nil,
		WithOsCallFunc(func(ctx context.Context, call *OsCall) (any, error) {
			seen = append(seen, call.Function)
			path, _ := call.Args[0].(string)
			switch call.Function {
			case "Path.read_text":
				return files[path], nil
			case "Path.write_text":
				content, _ := call.Args[1].(string)
				files[path] = content
				return len(content), nil
			}
			return nil, nil
		}))
	if err != nil {
		t.Fatalf("Execute failed: %v", err)
	}
	if result != "HELLO" {
		t.Fatalf("expected HELLO, got %v", result)
	}
	if len(seen) != 3 {
		t.Fatalf("expected 3 OS calls, got %v", seen)
	}
}
