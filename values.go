package montygo

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"math/big"
)

// Typed values crossing the Go <-> Python boundary.
//
// Everything between Go and the interpreter travels as JSON, and JSON can
// spell only the handful of shapes that map one-for-one onto Python: null,
// bool, number, string, list, dict. The interpreter, though, supports plenty
// that has no such spelling — bytes, datetimes, a stat result, an open file,
// an arbitrary-precision int. Values of those kinds cross as a JSON object
// carrying a reserved tag naming the type, which the WASM shim converts back
// into the real interpreter object (see json_to_tagged in
// crates/monty-wasm/src/lib.rs).
//
// The types below are that tag, typed. Return one from an OS-call or external
// function handler and Python receives the corresponding object. Handing back
// a bare map with the tag key works too, but these keep the spelling in one
// place.
//
// A plain Go value still means what it always did: string, bool, the numeric
// types, nil, []any and map[string]any need no wrapper.
//
// # Which types come back the other way
//
// The two directions are not symmetric, on purpose. Every type here can be
// *sent*; only the ones JSON cannot carry faithfully arrive in OsCall.Args,
// FunctionCall.Args or Execute's result:
//
//	Bytes, Date, DateTime, Time, TimeDelta, TimeZone, FileHandle,
//	BigInt (only past int64), TypeValue, BuiltinFunction, Function,
//	Repr, Cycle, NotImplemented
//
// A Python tuple, set, frozenset, named tuple, exception value, Path or `...`
// arrives as its plain shape instead — an []any, a map[string]any, a string —
// because that is what a handler wants: every filesystem OS call passes its
// path as a string, and typing those would break each handler that reads one
// while adding nothing a host needs. Sending montygo.Tuple or montygo.Path is
// still meaningful; you just will not get one back.
//
// The tag is not a capability. A dict built by sandboxed code that happens to
// carry the reserved key arrives as an ordinary map, so guest code cannot
// forge a Path or a FileHandle by naming one.

// montyTag is the reserved key marking a JSON object as a typed value rather
// than a plain dict. It must match MONTY_TAG in the Rust shim.
const montyTag = "__monty__"

// tagged builds the wire form for one typed value.
func tagged(kind string, fields map[string]any) map[string]any {
	out := make(map[string]any, len(fields)+1)
	out[montyTag] = kind
	for k, v := range fields {
		out[k] = v
	}
	return out
}

// Bytes is Python `bytes`. Returning a plain []byte would be ambiguous —
// encoding/json renders it as a base64 string, indistinguishable from a str —
// so bytes need the tag to arrive as bytes. `Path.read_bytes` and a file
// opened in binary mode ("rb") must answer with this type; a list of ints is
// not interchangeable and the interpreter rejects it.
type Bytes []byte

// MarshalJSON implements [json.Marshaler]. The payload is base64: a list of
// decimal ints would inflate the content roughly fourfold crossing the JSON
// boundary, and that inflation counts against the run's memory limit.
func (b Bytes) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("bytes", map[string]any{
		"base64": base64.StdEncoding.EncodeToString(b),
	}))
}

// Tuple is a Python `tuple`. A plain []any becomes a `list`.
type Tuple []any

// MarshalJSON implements [json.Marshaler].
func (t Tuple) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("tuple", map[string]any{"items": []any(t)}))
}

// Set is a Python `set`.
type Set []any

// MarshalJSON implements [json.Marshaler].
func (s Set) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("set", map[string]any{"items": []any(s)}))
}

// FrozenSet is a Python `frozenset`.
type FrozenSet []any

// MarshalJSON implements [json.Marshaler].
func (s FrozenSet) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("frozenset", map[string]any{"items": []any(s)}))
}

// Field is one named member of a [NamedTuple], in position order.
type Field struct {
	Name  string
	Value any
}

// NamedTuple is a Python named tuple: a tuple whose members also have names,
// like `os.stat_result`. Fields are positional, which is why they are a slice
// and not a map — a Go map has no order, and JSON objects do not preserve one.
type NamedTuple struct {
	// TypeName appears in the value's repr (e.g. "os.stat_result").
	TypeName string
	Fields   []Field
}

// MarshalJSON implements [json.Marshaler].
func (n NamedTuple) MarshalJSON() ([]byte, error) {
	pairs := make([][2]any, len(n.Fields))
	for i, f := range n.Fields {
		pairs[i] = [2]any{f.Name, f.Value}
	}
	return json.Marshal(tagged("namedtuple", map[string]any{
		"type":   n.TypeName,
		"fields": pairs,
	}))
}

// StatResult is what a `Path.stat` OS call must return: Python's
// `os.stat_result`. Building it through this type rather than a [NamedTuple]
// means the host does not have to know the field order, which is positional
// and fixed. The zero value is a valid, empty stat; set the fields that
// matter (usually StMode, StSize and StMtime) and leave the rest.
type StatResult struct {
	StMode  int64   // permission bits plus the file-type bits (see [ModeFile])
	StIno   int64   // inode number
	StDev   int64   // device id
	StNlink int64   // hard-link count; 0 is rendered as 1
	StUID   int64   // owning user id
	StGID   int64   // owning group id
	StSize  int64   // size in bytes
	StAtime float64 // access time, Unix seconds
	StMtime float64 // modification time, Unix seconds
	StCtime float64 // metadata-change time, Unix seconds
}

// File-type bits for [StatResult.StMode], matching the C `S_IF*` constants.
// Python's `stat.S_ISDIR` and friends read these, and so does `Path.is_dir()`
// when it is answered from a stat rather than its own OS call.
const (
	ModeFile    int64 = 0o100_000 // regular file
	ModeDir     int64 = 0o040_000 // directory
	ModeSymlink int64 = 0o120_000 // symbolic link
)

// MarshalJSON implements [json.Marshaler].
func (s StatResult) MarshalJSON() ([]byte, error) {
	nlink := s.StNlink
	if nlink == 0 {
		nlink = 1
	}
	return json.Marshal(tagged("stat", map[string]any{
		"st_mode":  s.StMode,
		"st_ino":   s.StIno,
		"st_dev":   s.StDev,
		"st_nlink": nlink,
		"st_uid":   s.StUID,
		"st_gid":   s.StGID,
		"st_size":  s.StSize,
		"st_atime": s.StAtime,
		"st_mtime": s.StMtime,
		"st_ctime": s.StCtime,
	}))
}

// Path is a `pathlib.Path`. A plain string stays a `str`, so `Path.resolve`
// and `Path.iterdir` should use this type for results Python will treat as
// paths.
type Path string

// MarshalJSON implements [json.Marshaler].
func (p Path) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("path", map[string]any{"path": string(p)}))
}

// Date is a `datetime.date`. `date.today` must answer with this rather than a
// formatted string: a string prints the same but has no .year, .month or .day.
type Date struct {
	Year  int
	Month int
	Day   int
}

// MarshalJSON implements [json.Marshaler].
func (d Date) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("date", map[string]any{
		"year": d.Year, "month": d.Month, "day": d.Day,
	}))
}

// DateTime is a `datetime.datetime`, the type `datetime.now` must answer with.
// Leave OffsetSeconds nil for a naive datetime; set it (and optionally
// TimeZoneName) for an aware one.
type DateTime struct {
	Year          int
	Month         int
	Day           int
	Hour          int
	Minute        int
	Second        int
	Microsecond   int
	OffsetSeconds *int
	TimeZoneName  string
}

// MarshalJSON implements [json.Marshaler].
func (d DateTime) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("datetime", map[string]any{
		"year": d.Year, "month": d.Month, "day": d.Day,
		"hour": d.Hour, "minute": d.Minute, "second": d.Second,
		"microsecond":    d.Microsecond,
		"offset_seconds": d.OffsetSeconds,
		"timezone_name":  optString(d.TimeZoneName),
	}))
}

// Time is a `datetime.time`.
type Time struct {
	Hour          int
	Minute        int
	Second        int
	Microsecond   int
	OffsetSeconds *int
	TimeZoneName  string
	Fold          int
}

// MarshalJSON implements [json.Marshaler].
func (t Time) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("time", map[string]any{
		"hour": t.Hour, "minute": t.Minute, "second": t.Second,
		"microsecond":    t.Microsecond,
		"offset_seconds": t.OffsetSeconds,
		"timezone_name":  optString(t.TimeZoneName),
		"fold":           t.Fold,
	}))
}

// TimeDelta is a `datetime.timedelta`.
type TimeDelta struct {
	Days         int
	Seconds      int
	Microseconds int
}

// MarshalJSON implements [json.Marshaler].
func (d TimeDelta) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("timedelta", map[string]any{
		"days": d.Days, "seconds": d.Seconds, "microseconds": d.Microseconds,
	}))
}

// TimeZone is a `datetime.timezone`.
type TimeZone struct {
	OffsetSeconds int
	Name          string
}

// MarshalJSON implements [json.Marshaler].
func (z TimeZone) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("timezone", map[string]any{
		"offset_seconds": z.OffsetSeconds,
		"name":           optString(z.Name),
	}))
}

// FileHandle is what an `open` OS call must return. The interpreter keeps no
// live file descriptor: it builds its own file object (`_io.TextIOWrapper` and
// friends) from this handle, and every later read or write on that object
// arrives as an ordinary one-shot `Path.read_text` / `Path.write_text` OS
// call. So a host serving `open` performs the open-time effect itself —
// truncate for "w", create-if-missing for "a", an existence check for "r" —
// and returns the handle; it never holds anything open between calls.
//
// Returning anything else from `open` leaves Python with a value that is not a
// file, which fails on the next line with "has no attribute 'read'".
type FileHandle struct {
	// Path is the path Python asked to open, echoed back. It is the path the
	// follow-up read and write calls will carry.
	Path string
	// Mode is the canonical CPython mode string, as handed to the OS call:
	// "r", "rb", "w", "wb", "a" or "ab". Update modes ("r+" and friends) are
	// rejected by the interpreter before the host ever sees them.
	Mode string
	// Position is the starting offset: a character index in text mode, a byte
	// index in binary mode. Zero for a freshly opened file.
	Position uint64
}

// MarshalJSON implements [json.Marshaler].
func (f FileHandle) MarshalJSON() ([]byte, error) {
	mode := f.Mode
	if mode == "" {
		mode = "r"
	}
	return json.Marshal(tagged("file", map[string]any{
		"path": f.Path, "mode": mode, "position": f.Position,
	}))
}

// BigInt is a Python `int` too large for int64.
type BigInt struct{ *big.Int }

// MarshalJSON implements [json.Marshaler].
func (b BigInt) MarshalJSON() ([]byte, error) {
	if b.Int == nil {
		return json.Marshal(tagged("bigint", map[string]any{"value": "0"}))
	}
	return json.Marshal(tagged("bigint", map[string]any{"value": b.String()}))
}

// ExceptionValue is an exception as a *value* — what `except E as e` binds, or
// what you would pass to Python to store and inspect. It does not raise
// anything. To make a handler's failure a raised, catchable Python exception,
// return a [PyError] as the handler's error instead.
type ExceptionValue struct {
	// Type is a Python exception name, e.g. "ValueError". An unknown name
	// makes the value arrive as a plain dict instead.
	Type    string
	Message string
}

// MarshalJSON implements [json.Marshaler].
func (e ExceptionValue) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("exception", map[string]any{
		"type": e.Type, "message": optString(e.Message),
	}))
}

// Ellipsis is Python's `...`.
type Ellipsis struct{}

// MarshalJSON implements [json.Marshaler].
func (Ellipsis) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("ellipsis", nil))
}

// NotImplemented is Python's `NotImplemented`.
type NotImplemented struct{}

// MarshalJSON implements [json.Marshaler].
func (NotImplemented) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("notimplemented", nil))
}

// TypeValue is a Python type object, named as Python spells it in lowercase
// ("str", "int", "list", ...).
type TypeValue struct{ Name string }

// MarshalJSON implements [json.Marshaler].
func (t TypeValue) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("type", map[string]any{"name": t.Name}))
}

// BuiltinFunction is one of Python's builtin functions, by name ("open",
// "len", ...).
type BuiltinFunction struct{ Name string }

// MarshalJSON implements [json.Marshaler].
func (b BuiltinFunction) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("builtin", map[string]any{"name": b.Name}))
}

// Function is a Python function object, carrying only its name and docstring —
// enough for a repr, not a callable body.
type Function struct {
	Name      string
	Docstring string
}

// MarshalJSON implements [json.Marshaler].
func (f Function) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("function", map[string]any{
		"name": f.Name, "docstring": optString(f.Docstring),
	}))
}

// Repr is an opaque object that prints as the given repr. It is how the
// interpreter describes a value it can name but not represent structurally,
// and it can be sent back the same way.
type Repr string

// MarshalJSON implements [json.Marshaler].
func (r Repr) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("repr", map[string]any{"repr": string(r)}))
}

// Cycle marks a value that referred back to itself while being converted, at
// the given depth. It only ever arrives from Python; sending one is not
// meaningful.
type Cycle struct {
	Depth int
	Repr  string
}

// MarshalJSON implements [json.Marshaler].
func (c Cycle) MarshalJSON() ([]byte, error) {
	return json.Marshal(tagged("cycle", map[string]any{
		"depth": c.Depth, "repr": c.Repr,
	}))
}

// optString renders "" as JSON null, matching the Option<String> the shim
// expects for a field that is genuinely absent rather than empty.
func optString(s string) any {
	if s == "" {
		return nil
	}
	return s
}

// PyError makes a handler's failure a Python exception raised at the call
// site, which the sandboxed code can catch.
//
// An OS-call or external-function handler that returns an ordinary error tears
// the whole execution down: Execute returns that error and the Python code
// never gets a say. That is right for a host-side bug, but wrong for the
// everyday answer "this file does not exist" — in CPython that is a
// FileNotFoundError, and code is entitled to catch it. Return a PyError and
// the guest sees exactly that:
//
//	return nil, &montygo.PyError{Type: "FileNotFoundError", Message: "no such file: " + path}
//
// which Python code can handle:
//
//	try:
//	    data = open("missing.txt").read()
//	except FileNotFoundError:
//	    data = ""
//
// Type is a Python exception name; an empty or unrecognized one raises
// OSError. Wrapping works — the resume path finds a PyError anywhere in the
// error chain with errors.As — so a handler can wrap one in context and still
// have it reach Python as the right exception.
type PyError struct {
	// Type is the Python exception to raise, e.g. "FileNotFoundError",
	// "PermissionError", "ValueError". Empty means OSError.
	Type string
	// Message is the exception's message — its str().
	Message string
	// Err is an optional underlying cause, reported by Unwrap.
	Err error
}

// Error implements the error interface.
func (e *PyError) Error() string {
	name := e.Type
	if name == "" {
		name = "OSError"
	}
	if e.Message == "" {
		return name
	}
	return fmt.Sprintf("%s: %s", name, e.Message)
}

// Unwrap returns the underlying cause, if any.
func (e *PyError) Unwrap() error { return e.Err }

// raiseValue is the wire form telling the shim to raise instead of return.
func (e *PyError) raiseValue() map[string]any {
	name := e.Type
	if name == "" {
		name = "OSError"
	}
	return tagged("raise", map[string]any{
		"type": name, "message": optString(e.Message),
	})
}

// decodeValue turns one JSON-decoded value from the interpreter back into a
// typed Go value, recursing through lists and dicts.
//
// It is the mirror of the MarshalJSON methods above: a tagged object becomes
// the matching Go type (Bytes, DateTime, FileHandle, ...) so a handler reading
// OsCall.Args or FunctionCall.Args gets a value it can switch on, rather than
// a map[string]any with a reserved key in it. Untagged values, and tagged ones
// this version does not know, are returned unchanged — an unrecognized tag is
// data, not an error.
func decodeValue(v any) any {
	switch v := v.(type) {
	case []any:
		for i, item := range v {
			v[i] = decodeValue(item)
		}
		return v
	case map[string]any:
		kind, ok := v[montyTag].(string)
		if !ok {
			for k, item := range v {
				v[k] = decodeValue(item)
			}
			return v
		}
		if decoded, ok := decodeTagged(kind, v); ok {
			return decoded
		}
		return v
	default:
		return v
	}
}

// decodeTagged builds the Go value for one tagged object, reporting false when
// the kind is unknown or the payload does not fit it.
func decodeTagged(kind string, m map[string]any) (any, bool) {
	switch kind {
	case "bytes":
		// base64 is what the interpreter sends; text and data are accepted so
		// a hand-built payload also works.
		if encoded, ok := m["base64"].(string); ok {
			out, err := base64.StdEncoding.DecodeString(encoded)
			if err != nil {
				return nil, false
			}
			return Bytes(out), true
		}
		if text, ok := m["text"].(string); ok {
			return Bytes(text), true
		}
		raw, ok := m["data"].([]any)
		if !ok {
			return nil, false
		}
		out := make(Bytes, 0, len(raw))
		for _, b := range raw {
			n, ok := b.(float64)
			if !ok || n < 0 || n > 255 {
				return nil, false
			}
			out = append(out, byte(n))
		}
		return out, true
	case "tuple":
		return Tuple(decodeItems(m)), true
	case "dict":
		// A guest dict that happened to carry the reserved key, or one with
		// non-string keys. Keys that are not strings are rendered with %v so
		// the result stays a Go map; the pairs are otherwise untouched.
		pairs, ok := m["items"].([]any)
		if !ok {
			return nil, false
		}
		out := make(map[string]any, len(pairs))
		for _, p := range pairs {
			pair, ok := p.([]any)
			if !ok || len(pair) != 2 {
				return nil, false
			}
			key, ok := pair[0].(string)
			if !ok {
				key = fmt.Sprintf("%v", decodeValue(pair[0]))
			}
			out[key] = decodeValue(pair[1])
		}
		return out, true
	case "set":
		return Set(decodeItems(m)), true
	case "frozenset":
		return FrozenSet(decodeItems(m)), true
	case "namedtuple":
		pairs, ok := m["fields"].([]any)
		if !ok {
			return nil, false
		}
		nt := NamedTuple{TypeName: str(m, "type"), Fields: make([]Field, 0, len(pairs))}
		for _, p := range pairs {
			pair, ok := p.([]any)
			if !ok || len(pair) != 2 {
				return nil, false
			}
			name, ok := pair[0].(string)
			if !ok {
				return nil, false
			}
			nt.Fields = append(nt.Fields, Field{Name: name, Value: decodeValue(pair[1])})
		}
		return nt, true
	case "path":
		return Path(str(m, "path")), true
	case "date":
		return Date{Year: num(m, "year"), Month: num(m, "month"), Day: num(m, "day")}, true
	case "datetime":
		return DateTime{
			Year: num(m, "year"), Month: num(m, "month"), Day: num(m, "day"),
			Hour: num(m, "hour"), Minute: num(m, "minute"), Second: num(m, "second"),
			Microsecond:   num(m, "microsecond"),
			OffsetSeconds: optNum(m, "offset_seconds"),
			TimeZoneName:  str(m, "timezone_name"),
		}, true
	case "time":
		return Time{
			Hour: num(m, "hour"), Minute: num(m, "minute"), Second: num(m, "second"),
			Microsecond:   num(m, "microsecond"),
			OffsetSeconds: optNum(m, "offset_seconds"),
			TimeZoneName:  str(m, "timezone_name"),
			Fold:          num(m, "fold"),
		}, true
	case "timedelta":
		return TimeDelta{
			Days: num(m, "days"), Seconds: num(m, "seconds"),
			Microseconds: num(m, "microseconds"),
		}, true
	case "timezone":
		return TimeZone{OffsetSeconds: num(m, "offset_seconds"), Name: str(m, "name")}, true
	case "exception":
		return ExceptionValue{Type: str(m, "type"), Message: str(m, "message")}, true
	case "bigint":
		i, ok := new(big.Int).SetString(str(m, "value"), 10)
		if !ok {
			return nil, false
		}
		return BigInt{Int: i}, true
	case "file":
		return FileHandle{
			Path:     str(m, "path"),
			Mode:     str(m, "mode"),
			Position: uint64(num(m, "position")),
		}, true
	case "type":
		return TypeValue{Name: str(m, "name")}, true
	case "builtin":
		return BuiltinFunction{Name: str(m, "name")}, true
	case "function":
		return Function{Name: str(m, "name"), Docstring: str(m, "docstring")}, true
	case "repr":
		return Repr(str(m, "repr")), true
	case "cycle":
		return Cycle{Depth: num(m, "depth"), Repr: str(m, "repr")}, true
	case "ellipsis":
		return Ellipsis{}, true
	case "notimplemented":
		return NotImplemented{}, true
	default:
		return nil, false
	}
}

// decodeItems decodes the "items" array of a tuple/set/frozenset payload.
func decodeItems(m map[string]any) []any {
	raw, _ := m["items"].([]any)
	out := make([]any, len(raw))
	for i, item := range raw {
		out[i] = decodeValue(item)
	}
	return out
}

// str reads a string field, returning "" when absent or null.
func str(m map[string]any, key string) string {
	s, _ := m[key].(string)
	return s
}

// num reads a numeric field as an int, returning 0 when absent or null.
func num(m map[string]any, key string) int {
	f, _ := m[key].(float64)
	return int(f)
}

// optNum reads a numeric field that may legitimately be absent, distinguishing
// "not set" (nil) from zero.
func optNum(m map[string]any, key string) *int {
	f, ok := m[key].(float64)
	if !ok {
		return nil
	}
	n := int(f)
	return &n
}

// plainValue flattens a typed value to the plain JSON shape an ordinary
// consumer expects, dropping this bridge's tagging.
//
// It is what [FunctionCall.ArgsJSON] hands to a tool handler: such a handler
// wants `{"data": "aGk="}`, not `{"data": {"__monty__": "bytes", ...}}`. The
// mapping is deliberately lossy — a tuple and a list both become an array, a
// date becomes a string — so use the typed values directly whenever the
// distinction matters.
func plainValue(v any) any {
	switch v := v.(type) {
	case Bytes:
		return base64.StdEncoding.EncodeToString(v)
	case Tuple:
		return plainSlice(v)
	case Set:
		return plainSlice(v)
	case FrozenSet:
		return plainSlice(v)
	case NamedTuple:
		out := make(map[string]any, len(v.Fields))
		for _, f := range v.Fields {
			out[f.Name] = plainValue(f.Value)
		}
		return out
	case StatResult:
		return map[string]any{
			"st_mode": v.StMode, "st_ino": v.StIno, "st_dev": v.StDev,
			"st_nlink": v.StNlink, "st_uid": v.StUID, "st_gid": v.StGID,
			"st_size": v.StSize, "st_atime": v.StAtime,
			"st_mtime": v.StMtime, "st_ctime": v.StCtime,
		}
	case Path:
		return string(v)
	case Date:
		return fmt.Sprintf("%04d-%02d-%02d", v.Year, v.Month, v.Day)
	case DateTime:
		return fmt.Sprintf("%04d-%02d-%02dT%02d:%02d:%02d", v.Year, v.Month, v.Day, v.Hour, v.Minute, v.Second)
	case Time:
		return fmt.Sprintf("%02d:%02d:%02d", v.Hour, v.Minute, v.Second)
	case TimeDelta:
		return map[string]any{"days": v.Days, "seconds": v.Seconds, "microseconds": v.Microseconds}
	case TimeZone:
		return map[string]any{"offset_seconds": v.OffsetSeconds, "name": v.Name}
	case FileHandle:
		return map[string]any{"path": v.Path, "mode": v.Mode, "position": v.Position}
	case BigInt:
		if v.Int == nil {
			return "0"
		}
		return v.String()
	case ExceptionValue:
		return map[string]any{"type": v.Type, "message": v.Message}
	case TypeValue:
		return v.Name
	case BuiltinFunction:
		return v.Name
	case Function:
		return v.Name
	case Repr:
		return string(v)
	case Cycle:
		return v.Repr
	case Ellipsis:
		return "..."
	case NotImplemented:
		return nil
	case []any:
		return plainSlice(v)
	case map[string]any:
		out := make(map[string]any, len(v))
		for k, item := range v {
			out[k] = plainValue(item)
		}
		return out
	default:
		return v
	}
}

// plainSlice flattens each element of a slice with [plainValue].
func plainSlice(items []any) []any {
	out := make([]any, len(items))
	for i, item := range items {
		out[i] = plainValue(item)
	}
	return out
}
