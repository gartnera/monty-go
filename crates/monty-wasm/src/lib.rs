//! WASM shim for the Monty Python interpreter.
//!
//! Exposes Monty's pause/resume API as C-ABI WASM exports for consumption
//! by Go via wazero. State (runners, snapshots) is stored in a global map
//! keyed by handles — safe because WASM is single-threaded and we create
//! one instance per Execute() call on the Go side.

use std::alloc::{alloc, dealloc, Layout};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use monty::{FunctionCall, MontyRun, OsCall, ResolveFutures, RunProgress};
use monty_types::unstable::{self, MontyNode};
use monty_types::{
    CallArgs, CompileOptions, DEFAULT_MAX_RECURSION_DEPTH, DEFAULT_MAX_SUSPENSIONS, DateTimeSource, ExcType,
    ExtFunctionResult, MontyDate, MontyDateTime, MontyException, MontyFileHandle, MontyObject, MontyTime,
    MontyTimeDelta, MontyTimeZone, MontyType, NameLookupResult, ObjectRef, OsFunctionCall, OsPolicy, PrintWriter,
    PrintWriterCallback, ProcessTime, RandomSeed, RandomStart, ResourceLimits, ResourceTracker, SandboxTimeZone,
    SleepMode, memory_limit_with_headroom, stat_result,
};
use serde::{Deserialize, Serialize};

/// monty enforces `max_memory` in two places: the interpreter raises
/// `MemoryError` at its checkpoints once live usage crosses the soft limit,
/// and this allocator traps the instance if the hard ceiling above it is
/// crossed between checkpoints. Both read the live-byte count this allocator
/// keeps, so it has to be installed for either to work, and armed via
/// `monty_alloc::set_hard_limit` before each run.
#[global_allocator]
static ALLOC: monty_alloc::LimitedAllocator = monty_alloc::LimitedAllocator;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::Value as JsonValue;

// ---------------------------------------------------------------------------
// MontyObject <-> serde_json::Value conversion
// ---------------------------------------------------------------------------

/// Convert a Monty value to a plain JSON value (not the tagged enum format).
///
/// Values are an arena of nodes (`MontyObject` is the arena plus its root),
/// so this walks the nodes through monty's `unstable` view: the stable
/// accessors cover only the common scalars and containers, and the bridge has
/// to tell every kind apart.
fn monty_to_json(obj: ObjectRef<'_>) -> JsonValue {
    match unstable::node(obj) {
        MontyNode::None => JsonValue::Null,
        MontyNode::Bool(b) => JsonValue::Bool(*b),
        MontyNode::Int(i) => serde_json::json!(*i),
        MontyNode::BigInt(bi) => {
            // Fits in i64: a plain JSON number, which round-trips as an int.
            // Beyond that it is tagged, because a bare string would come back
            // as a `str` and silently stop being a number.
            if let Ok(v) = i64::try_from(bi) {
                serde_json::json!(v)
            } else {
                tagged("bigint", [("value", JsonValue::String(bi.to_string()))])
            }
        }
        MontyNode::Float(f) => serde_json::json!(*f),
        // No JSON spelling, so tagged like the other typed values.
        MontyNode::Complex(c) => tagged(
            "complex",
            [("real", serde_json::json!(c.real)), ("imag", serde_json::json!(c.imag))],
        ),
        MontyNode::String(s) => JsonValue::String(s.clone()),
        // Tagged, not a bare int array: a host servicing `Path.write_bytes`
        // or a binary write has to be able to tell `bytes` from a
        // `list[int]`, and an array alone cannot say which it was. The
        // payload is base64 rather than a list of decimal ints, which would
        // inflate a file roughly fourfold on its way through the JSON
        // boundary and count that against the run's memory limit.
        MontyNode::Bytes(b) => tagged("bytes", [("base64", JsonValue::String(BASE64.encode(b)))]),
        // A tuple, set and frozenset are plain arrays, like `list`. Tagging
        // them would tell the host which it got, but at the cost of every
        // existing handler that reads an argument as `[]any` — and upstream's
        // own hosts see a list here too. A host that needs the distinction
        // can still *send* a `tuple`; see the direction note on
        // `json_to_tagged`.
        MontyNode::List(_) | MontyNode::Tuple(_) | MontyNode::Set(_) | MontyNode::FrozenSet(_) => {
            items_to_json(obj)
        }
        MontyNode::Dict(_) => {
            let pairs = obj.pairs().unwrap_or_default();
            let mut map = serde_json::Map::new();
            let mut collides = false;
            for (k, v) in &pairs {
                let key = key_string(*k);
                collides |= key == MONTY_TAG;
                map.insert(key, monty_to_json(*v));
            }
            // A guest dict is data, and it must not be able to impersonate a
            // typed value just by carrying the reserved key. One that does is
            // sent in the tagged `dict` form instead, so the host can tell a
            // real `bytes` or `Path` from sandboxed code claiming to be one.
            if collides {
                let items = pairs
                    .iter()
                    .map(|(k, v)| JsonValue::Array(vec![monty_to_json(*k), monty_to_json(*v)]))
                    .collect();
                return tagged("dict", [("items", JsonValue::Array(items))]);
            }
            JsonValue::Object(map)
        }
        MontyNode::Ellipsis => JsonValue::String("...".to_owned()),
        // A bare string. Every filesystem OS call carries a path as its first
        // argument, and a host reads it as one — tagging it would break that
        // for no benefit, since a handler uses the path rather than echoing
        // it back.
        MontyNode::Path(p) => JsonValue::String(p.clone()),
        MontyNode::Exception { exc_type, arg } => {
            let name: &'static str = exc_type.into();
            let mut map = serde_json::Map::new();
            map.insert("exception".to_owned(), JsonValue::String(name.to_owned()));
            if let Some(a) = arg {
                map.insert("message".to_owned(), JsonValue::String(a.clone()));
            }
            JsonValue::Object(map)
        }
        MontyNode::NamedTuple {
            type_name, field_names, ..
        } => {
            let mut map = serde_json::Map::new();
            map.insert("__type__".to_owned(), JsonValue::String(type_name.clone()));
            for (name, val) in field_names.iter().zip(obj.items().unwrap_or_default()) {
                map.insert(name.clone(), monty_to_json(val));
            }
            JsonValue::Object(map)
        }
        MontyNode::ClassInstance { .. } => {
            let mut map = serde_json::Map::new();
            map.insert("__type__".to_owned(), JsonValue::String(obj.type_name().to_owned()));
            // attrs preserve declaration order, so no field-name list is needed.
            for (k, v) in obj.pairs().unwrap_or_default() {
                if let Some(key) = k.as_str() {
                    map.insert(key.to_owned(), monty_to_json(v));
                }
            }
            JsonValue::Object(map)
        }
        // Everything below has no plain-JSON spelling, so it crosses in the
        // same tagged form a host uses to send one in (see `json_to_tagged`).
        MontyNode::FileHandle(handle) => file_handle_to_json(handle),
        MontyNode::NotImplemented => tagged("notimplemented", []),
        MontyNode::Date(d) => tagged(
            "date",
            [
                ("year", serde_json::json!(d.year)),
                ("month", serde_json::json!(d.month)),
                ("day", serde_json::json!(d.day)),
            ],
        ),
        MontyNode::DateTime(dt) => tagged(
            "datetime",
            [
                ("year", serde_json::json!(dt.year)),
                ("month", serde_json::json!(dt.month)),
                ("day", serde_json::json!(dt.day)),
                ("hour", serde_json::json!(dt.hour)),
                ("minute", serde_json::json!(dt.minute)),
                ("second", serde_json::json!(dt.second)),
                ("microsecond", serde_json::json!(dt.microsecond)),
                ("offset_seconds", opt_i32(dt.offset_seconds)),
                ("timezone_name", opt_str(&dt.timezone_name)),
            ],
        ),
        MontyNode::Time(t) => tagged(
            "time",
            [
                ("hour", serde_json::json!(t.hour)),
                ("minute", serde_json::json!(t.minute)),
                ("second", serde_json::json!(t.second)),
                ("microsecond", serde_json::json!(t.microsecond)),
                ("offset_seconds", opt_i32(t.offset_seconds)),
                ("timezone_name", opt_str(&t.timezone_name)),
                ("fold", serde_json::json!(t.fold)),
            ],
        ),
        MontyNode::TimeDelta(td) => tagged(
            "timedelta",
            [
                ("days", serde_json::json!(td.days)),
                ("seconds", serde_json::json!(td.seconds)),
                ("microseconds", serde_json::json!(td.microseconds)),
            ],
        ),
        MontyNode::TimeZone(tz) => tagged(
            "timezone",
            [
                ("offset_seconds", serde_json::json!(tz.offset_seconds)),
                ("name", opt_str(&tz.name)),
            ],
        ),
        MontyNode::Type(ty) => tagged("type", [("name", JsonValue::String(ty.name().to_owned()))]),
        // A class defined by the sandboxed code. Named like a builtin type:
        // the host can read which class it was, though sending the name back
        // cannot recreate it.
        MontyNode::ClassType(class) => tagged("type", [("name", JsonValue::String(class.name.clone()))]),
        MontyNode::BuiltinFunction(func) => {
            tagged("builtin", [("name", JsonValue::String(func.to_string()))])
        }
        MontyNode::Function { name, docstring } => tagged(
            "function",
            [
                ("name", JsonValue::String(name.clone())),
                ("docstring", opt_str(docstring)),
            ],
        ),
        MontyNode::Repr(repr) => tagged("repr", [("repr", JsonValue::String(repr.clone()))]),
        // A reference back to an enclosing container, as the placeholder its
        // repr shows (`[...]`, `{...}`, ...).
        MontyNode::Cycle(placeholder) => tagged("cycle", [("repr", JsonValue::String(placeholder.clone()))]),
    }
}

/// A dict or kwargs key as a JSON object key: a string as itself, anything
/// else as its Python repr.
fn key_string(key: ObjectRef<'_>) -> String {
    match key.as_str() {
        Some(s) => s.to_owned(),
        None => key.py_repr(),
    }
}

/// Convert a plain JSON value to a MontyObject.
fn json_to_monty(val: &JsonValue) -> MontyObject {
    match val {
        JsonValue::Null => MontyObject::none(),
        JsonValue::Bool(b) => MontyObject::bool(*b),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                MontyObject::int(i)
            } else if let Some(f) = n.as_f64() {
                MontyObject::float(f)
            } else {
                MontyObject::none()
            }
        }
        JsonValue::String(s) => MontyObject::string(s.clone()),
        JsonValue::Array(items) => MontyObject::list(items.iter().map(json_to_monty)),
        JsonValue::Object(map) => {
            // A tagged object is a typed Monty value (bytes, datetime, a file
            // handle, ...); anything else is an ordinary dict.
            if let Some(obj) = json_to_tagged(map) {
                return obj;
            }
            MontyObject::dict(
                map.iter()
                    .map(|(k, v)| (MontyObject::string(k.clone()), json_to_monty(v))),
            )
        }
    }
}

/// Reserved key marking a JSON object as a typed Monty value rather than a
/// plain `dict`. Its value names the type; sibling keys carry the payload.
///
/// JSON on its own can express only the handful of shapes that map to
/// a Python value one-for-one (null, bool, number, string, array, object), so
/// without a tag a host could never hand the guest a `bytes`, a `datetime`, a
/// `stat_result`, a file handle or an exception — the interpreter supports all
/// of them, but they have no JSON spelling. This tag is that spelling.
///
/// Each form is documented on [`json_to_tagged`], and the Go side of the
/// bridge has a typed constructor for every one of them.
pub const MONTY_TAG: &str = "__monty__";

/// Render a file handle as the tagged `"file"` form.
fn file_handle_to_json(handle: &MontyFileHandle) -> JsonValue {
    tagged(
        "file",
        [
            ("path", JsonValue::String(handle.path.clone())),
            ("mode", JsonValue::String(handle.mode.as_str().to_owned())),
            ("position", serde_json::json!(handle.position)),
        ],
    )
}

/// Build a tagged JSON object: `{"__monty__": <kind>, ...fields}`.
fn tagged<const N: usize>(kind: &str, fields: [(&str, JsonValue); N]) -> JsonValue {
    let mut map = serde_json::Map::new();
    map.insert(MONTY_TAG.to_owned(), JsonValue::String(kind.to_owned()));
    for (k, v) in fields {
        map.insert(k.to_owned(), v);
    }
    JsonValue::Object(map)
}

/// Wrap an optional string as a JSON value (`null` when absent).
fn opt_str(value: &Option<String>) -> JsonValue {
    match value {
        Some(s) => JsonValue::String(s.clone()),
        None => JsonValue::Null,
    }
}

/// The items of a sequence or set value as a JSON array.
fn items_to_json(obj: ObjectRef<'_>) -> JsonValue {
    JsonValue::Array(obj.items().unwrap_or_default().into_iter().map(monty_to_json).collect())
}

/// Wrap an optional i32 as a JSON value (`null` when absent).
fn opt_i32(value: Option<i32>) -> JsonValue {
    match value {
        Some(v) => serde_json::json!(v),
        None => JsonValue::Null,
    }
}

/// Decode a tagged JSON object (see [`MONTY_TAG`]) into its `MontyObject`.
///
/// Returns `None` when `map` is not tagged, or is tagged with a kind or
/// payload this build cannot make sense of — the caller then treats the object
/// as an ordinary `dict`, which keeps a host that happens to use the key for
/// its own data working instead of failing the run.
///
/// The forms, all with `"__monty__"` naming the kind:
///
/// | kind | payload | becomes |
/// |------|---------|---------|
/// | `bytes` | `base64`: a string, **or** `data`: array of 0-255 ints, **or** `text`: a string (encoded UTF-8) | `bytes` |
/// | `tuple` / `set` / `frozenset` | `items`: array | `tuple` / `set` / `frozenset` |
/// | `dict` | `items`: array of `[key, value]` pairs | `dict` (keys of any type) |
/// | `namedtuple` | `type`: string, `fields`: array of `[name, value]` pairs | a named tuple |
/// | `stat` | the `st_*` fields (`st_mode`, `st_size`, `st_mtime`, ...) | `os.stat_result` |
/// | `path` | `path`: string | `pathlib.Path` |
/// | `complex` | `real`, `imag` | `complex` |
/// | `date` | `year`, `month`, `day` | `datetime.date` |
/// | `datetime` | `year`..`microsecond`, optional `offset_seconds`, `timezone_name` | `datetime.datetime` |
/// | `time` | `hour`..`microsecond`, optional `offset_seconds`, `timezone_name`, `fold` | `datetime.time` |
/// | `timedelta` | `days`, `seconds`, `microseconds` | `datetime.timedelta` |
/// | `timezone` | `offset_seconds`, optional `name` | `datetime.timezone` |
/// | `exception` | `type`: an exception name, optional `message` | an exception *value* |
/// | `bigint` | `value`: the digits as a string | an arbitrary-precision `int` |
/// | `file` | `path`, `mode`, optional `position` | an `_io` file object |
/// | `type` | `name`: a builtin type name | a `type` |
/// | `builtin` | `name`: a builtin's name | a builtin function |
/// | `function` | `name`, optional `docstring` | a function |
/// | `ellipsis` | — | `Ellipsis` |
/// | `notimplemented` | — | `NotImplemented` |
///
/// `repr` and `cycle` only ever travel outwards: monty refuses either as an
/// input, since neither stands for a real value.
///
/// Raising an exception in the guest is a different thing from handing it this
/// `exception` *value*: to make a host failure catchable Python, resume the
/// call with [`ExtFunctionResult::Error`] instead (the Go bridge does this for
/// a handler that returns a `*PyError`).
///
/// # Direction
///
/// Every form above is accepted *from* the host. The reverse direction
/// ([`monty_to_json`]) is deliberately narrower: it tags only what JSON cannot
/// carry faithfully — `bytes`, `complex`, the `datetime` family, a file
/// handle, an `int` past i64, and the type/builtin/function/repr values. A
/// `tuple`, `set`, `Path`, named tuple, exception value or `...` keeps its
/// plain JSON shape on the way out, because a host reads those as the array,
/// string or object they already look like — every filesystem OS call passes
/// its path as a string — and tagging them would break that for no gain. So a
/// host can send more shapes than it receives, which is the useful asymmetry:
/// it is answering calls, not echoing them.
fn json_to_tagged(map: &serde_json::Map<String, JsonValue>) -> Option<MontyObject> {
    let kind = map.get(MONTY_TAG)?.as_str()?;

    // Field readers, all tolerant of a missing key so a partial payload
    // degrades to a default rather than failing the whole conversion.
    let s = |key: &str| map.get(key).and_then(JsonValue::as_str);
    let opt_string = |key: &str| s(key).map(str::to_owned);
    let i = |key: &str| map.get(key).and_then(JsonValue::as_i64);
    let u = |key: &str| map.get(key).and_then(JsonValue::as_u64);
    let f = |key: &str| map.get(key).and_then(JsonValue::as_f64);
    let items = |key: &str| {
        map.get(key)
            .and_then(JsonValue::as_array)
            .map(|a| a.iter().map(json_to_monty).collect::<Vec<_>>())
            .unwrap_or_default()
    };

    match kind {
        "bytes" => {
            // base64 is the form this bridge sends; `text` and `data` are
            // accepted conveniences for a host building one by hand.
            if let Some(encoded) = s("base64") {
                return BASE64.decode(encoded).ok().map(MontyObject::bytes);
            }
            if let Some(text) = s("text") {
                return Some(MontyObject::bytes(text.as_bytes().to_vec()));
            }
            let data = map.get("data")?.as_array()?;
            let mut out = Vec::with_capacity(data.len());
            for byte in data {
                out.push(u8::try_from(byte.as_u64()?).ok()?);
            }
            Some(MontyObject::bytes(out))
        }
        "tuple" => Some(MontyObject::tuple(items("items"))),
        // The escape hatch for a guest dict that carries the reserved key
        // (see `monty_to_json`); also lets a host send a dict with non-string
        // keys, which a plain JSON object cannot express.
        "dict" => {
            let pairs = map.get("items")?.as_array()?;
            let mut out = Vec::with_capacity(pairs.len());
            for pair in pairs {
                let pair = pair.as_array()?;
                out.push((
                    json_to_monty(pair.first()?),
                    json_to_monty(pair.get(1).unwrap_or(&JsonValue::Null)),
                ));
            }
            Some(MontyObject::dict(out))
        }
        "set" => Some(MontyObject::set(items("items"))),
        "frozenset" => Some(MontyObject::frozenset(items("items"))),
        "namedtuple" => {
            // Pairs, not an object: JSON objects have no defined key order and
            // a named tuple's fields are positional.
            let pairs = map.get("fields")?.as_array()?;
            let mut field_names = Vec::with_capacity(pairs.len());
            let mut values = Vec::with_capacity(pairs.len());
            for pair in pairs {
                let pair = pair.as_array()?;
                field_names.push(pair.first()?.as_str()?.to_owned());
                values.push(json_to_monty(pair.get(1).unwrap_or(&JsonValue::Null)));
            }
            Some(MontyObject::named_tuple(
                opt_string("type").unwrap_or_else(|| "namedtuple".to_owned()),
                field_names,
                values,
            ))
        }
        // `stat_result` builds the exact named tuple `Path.stat()` must return,
        // so a host does not have to know its field order.
        "stat" => Some(stat_result(
            i("st_mode").unwrap_or(0),
            i("st_ino").unwrap_or(0),
            i("st_dev").unwrap_or(0),
            i("st_nlink").unwrap_or(1),
            i("st_uid").unwrap_or(0),
            i("st_gid").unwrap_or(0),
            i("st_size").unwrap_or(0),
            f("st_atime").unwrap_or(0.0),
            f("st_mtime").unwrap_or(0.0),
            f("st_ctime").unwrap_or(0.0),
        )),
        "path" => Some(MontyObject::path(opt_string("path")?)),
        "complex" => Some(MontyObject::complex(f("real").unwrap_or(0.0), f("imag").unwrap_or(0.0))),
        "date" => Some(MontyObject::date(MontyDate {
            year: i32::try_from(i("year")?).ok()?,
            month: u8::try_from(u("month")?).ok()?,
            day: u8::try_from(u("day")?).ok()?,
        })),
        "datetime" => Some(MontyObject::datetime(MontyDateTime {
            year: i32::try_from(i("year")?).ok()?,
            month: u8::try_from(u("month")?).ok()?,
            day: u8::try_from(u("day")?).ok()?,
            hour: u8::try_from(u("hour").unwrap_or(0)).ok()?,
            minute: u8::try_from(u("minute").unwrap_or(0)).ok()?,
            second: u8::try_from(u("second").unwrap_or(0)).ok()?,
            microsecond: u32::try_from(u("microsecond").unwrap_or(0)).ok()?,
            offset_seconds: i("offset_seconds").and_then(|v| i32::try_from(v).ok()),
            timezone_name: opt_string("timezone_name"),
        })),
        "time" => Some(MontyObject::time(MontyTime {
            hour: u8::try_from(u("hour").unwrap_or(0)).ok()?,
            minute: u8::try_from(u("minute").unwrap_or(0)).ok()?,
            second: u8::try_from(u("second").unwrap_or(0)).ok()?,
            microsecond: u32::try_from(u("microsecond").unwrap_or(0)).ok()?,
            offset_seconds: i("offset_seconds").and_then(|v| i32::try_from(v).ok()),
            timezone_name: opt_string("timezone_name"),
            fold: u8::try_from(u("fold").unwrap_or(0)).ok()?,
        })),
        "timedelta" => Some(MontyObject::timedelta(MontyTimeDelta {
            days: i32::try_from(i("days").unwrap_or(0)).ok()?,
            seconds: i32::try_from(i("seconds").unwrap_or(0)).ok()?,
            microseconds: i32::try_from(i("microseconds").unwrap_or(0)).ok()?,
        })),
        "timezone" => Some(MontyObject::timezone(MontyTimeZone {
            offset_seconds: i32::try_from(i("offset_seconds").unwrap_or(0)).ok()?,
            name: opt_string("name"),
        })),
        "exception" => Some(MontyObject::exception(s("type")?.parse().ok()?, opt_string("message"))),
        "bigint" => Some(MontyObject::bigint(s("value")?.parse().ok()?)),
        "file" => Some(MontyObject::file_handle(MontyFileHandle {
            path: opt_string("path")?,
            mode: s("mode")?.parse().ok()?,
            position: u("position").unwrap_or(0),
        })),
        "type" => Some(MontyObject::type_object(MontyType::from_type_name(s("name")?)?)),
        "builtin" => MontyObject::builtin_function_from_name(s("name")?),
        "function" => Some(MontyObject::function(opt_string("name")?, opt_string("docstring"))),
        "ellipsis" => Some(MontyObject::ellipsis()),
        "notimplemented" => Some(MontyObject::not_implemented()),
        _ => None,
    }
}

/// An OS call's positional arguments as a JSON array.
fn call_args_to_json(args: &CallArgs) -> JsonValue {
    JsonValue::Array(args.args().map(monty_to_json).collect())
}

/// An OS call's keyword arguments as a JSON object.
fn call_kwargs_to_json(args: &CallArgs) -> JsonValue {
    let mut map = serde_json::Map::new();
    for (k, v) in args.kwargs() {
        map.insert(key_string(k), monty_to_json(v));
    }
    JsonValue::Object(map)
}

/// Merge positional args and kwargs into a single JSON object.
/// Positional args are mapped to parameter names by index.
fn merge_args(func_name: &str, args: &CallArgs, param_registry: &HashMap<String, Vec<String>>) -> JsonValue {
    let mut map = serde_json::Map::new();
    // Map positional args to parameter names.
    if let Some(param_names) = param_registry.get(func_name) {
        for (name, arg) in param_names.iter().zip(args.args()) {
            map.insert(name.clone(), monty_to_json(arg));
        }
    }
    // Merge kwargs (overrides positional — Python semantics).
    for (k, v) in args.kwargs() {
        if let Some(key) = k.as_str() {
            map.insert(key.to_owned(), monty_to_json(v));
        }
    }
    JsonValue::Object(map)
}

/// External function definition with parameter names.
#[derive(Deserialize)]
struct ExtFuncDef {
    name: String,
    #[serde(default)]
    params: Vec<String>,
}

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

static STATE: Mutex<Option<State>> = Mutex::new(None);
static RESULT_BUF: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static PARAM_REGISTRY: Mutex<Option<HashMap<String, Vec<String>>>> = Mutex::new(None);

struct State {
    next_id: u32,
    runners: HashMap<u32, MontyRun>,
    snapshots: HashMap<u32, SnapshotState>,
}

enum SnapshotState {
    FunctionCall(FunctionCall),
    OsCall(OsCall),
    ResolveFutures(ResolveFutures),
}

impl State {
    fn new() -> Self {
        Self {
            next_id: 1,
            runners: HashMap::new(),
            snapshots: HashMap::new(),
        }
    }

    fn next_handle(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

fn with_state<F, R>(f: F) -> R
where
    F: FnOnce(&mut State) -> R,
{
    let mut guard = STATE.lock().unwrap();
    let state = guard.get_or_insert_with(State::new);
    f(state)
}

fn with_param_registry<F, R>(f: F) -> R
where
    F: FnOnce(&HashMap<String, Vec<String>>) -> R,
{
    let guard = PARAM_REGISTRY.lock().unwrap();
    static EMPTY: std::sync::LazyLock<HashMap<String, Vec<String>>> = std::sync::LazyLock::new(HashMap::new);
    let registry = guard.as_ref().unwrap_or(&EMPTY);
    f(registry)
}

// ---------------------------------------------------------------------------
// Result buffer helpers
// ---------------------------------------------------------------------------

fn set_result(data: &[u8]) {
    let mut buf = RESULT_BUF.lock().unwrap();
    buf.clear();
    buf.extend_from_slice(data);
}

fn set_result_json<T: Serialize>(value: &T) {
    let json = serde_json::to_vec(value).unwrap_or_default();
    set_result(&json);
}

// ---------------------------------------------------------------------------
// JSON wire types (using plain JSON values, not MontyObject tagged enums)
// ---------------------------------------------------------------------------

#[derive(Serialize, Default)]
struct ProgressResult {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_handle: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    os_function: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    args: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kwargs: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    call_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    method_call: Option<bool>,
    /// Set on an OS call the host may answer with a future (`asyncio.sleep`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    accepts_future: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_call_ids: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Everything printed since the previous suspension, in order, as
    /// `[stream, text]` pairs (`stream` is `"stdout"` or `"stderr"`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    output: Vec<(&'static str, String)>,
}

#[derive(Deserialize, Default)]
struct LimitsInput {
    /// Accepted for wire compatibility; monty dropped allocation counting in
    /// favour of `max_memory` and `max_suspensions`.
    #[serde(default)]
    #[allow(dead_code)]
    max_allocations: Option<usize>,
    #[serde(default)]
    max_suspensions: Option<usize>,
    /// Execution time for the whole run, excluding time suspended on the host.
    #[serde(default)]
    max_duration_ms: Option<u64>,
    /// Execution time between two host round trips.
    #[serde(default)]
    max_turn_duration_ms: Option<u64>,
    #[serde(default)]
    max_memory: Option<usize>,
    #[serde(default)]
    max_recursion_depth: Option<usize>,
    /// Cumulative system sleep. Stored on the tracker for completeness; the
    /// host performs the sleeps, so it is the host that enforces it.
    #[serde(default)]
    max_total_sleep_ms: Option<u64>,
}

/// Per-run configuration set between compile and start (see `monty_configure`).
#[derive(Deserialize, Default)]
struct ConfigureInput {
    /// The sandbox working directory: what `os.getcwd()` reports and what
    /// relative paths resolve against before an OS call reaches the host.
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    os_policy: Option<OsPolicyInput>,
}

/// The host's choices for clocks, sleeps and random state; see [`OsPolicy`].
/// Every field is optional and falls back to monty's default.
#[derive(Deserialize, Default)]
struct OsPolicyInput {
    #[serde(default)]
    datetime: Option<DateTimeInput>,
    #[serde(default)]
    timezone: Option<TimeZoneInput>,
    #[serde(default)]
    sleep: Option<SleepInput>,
    /// `"zero"` (default) or `"elapsed"`.
    #[serde(default)]
    process_time: Option<String>,
    #[serde(default)]
    random: Option<RandomInput>,
}

/// `{"kind": "system" | "host" | "fixed", "unix_seconds", "microsecond"}`.
#[derive(Deserialize)]
struct DateTimeInput {
    kind: String,
    #[serde(default)]
    unix_seconds: i64,
    #[serde(default)]
    microsecond: u32,
}

/// `{"name": "Europe/London"}` for an IANA zone, or `{"offset_seconds", "name"}`
/// for a fixed offset.
#[derive(Deserialize)]
struct TimeZoneInput {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    offset_seconds: Option<i32>,
}

/// `{"kind": "system", "max_ms"}`, `{"kind": "host"}` or `{"kind": "zero"}`.
#[derive(Deserialize)]
struct SleepInput {
    kind: String,
    #[serde(default)]
    max_ms: Option<u64>,
}

/// `{"kind": "system" | "host"}`, or `{"kind": "seed", ...}` with exactly one
/// of `int` (digits, any size), `float`, `str` or `bytes` (base64).
#[derive(Deserialize)]
struct RandomInput {
    kind: String,
    #[serde(default)]
    int: Option<String>,
    #[serde(default)]
    float: Option<f64>,
    #[serde(default)]
    str: Option<String>,
    #[serde(default)]
    bytes: Option<String>,
}

impl OsPolicyInput {
    /// Builds the policy, or a message naming the field it could not accept.
    fn into_policy(self) -> Result<OsPolicy, String> {
        let mut policy = OsPolicy::default();
        if let Some(dt) = self.datetime {
            policy.datetime = match dt.kind.as_str() {
                "system" => DateTimeSource::System,
                "host" => DateTimeSource::CallHost,
                "fixed" => DateTimeSource::Fixed {
                    unix_seconds: dt.unix_seconds,
                    microsecond: dt.microsecond,
                },
                other => return Err(format!("unknown datetime source {other:?}")),
            };
        }
        if let Some(tz) = self.timezone {
            policy.timezone = match (tz.offset_seconds, tz.name) {
                (Some(offset_seconds), name) => SandboxTimeZone::Fixed { offset_seconds, name },
                (None, Some(name)) => SandboxTimeZone::named(&name).map_err(|e| e.to_string())?,
                (None, None) => SandboxTimeZone::utc(),
            };
        }
        if let Some(sleep) = self.sleep {
            policy.sleep = match sleep.kind.as_str() {
                "system" => SleepMode::System(
                    sleep.max_ms.map_or(SleepMode::DEFAULT_MAX, Duration::from_millis),
                ),
                "host" => SleepMode::CallHost,
                "zero" => SleepMode::Zero,
                other => return Err(format!("unknown sleep mode {other:?}")),
            };
        }
        if let Some(process_time) = self.process_time {
            policy.process_time = match process_time.as_str() {
                "zero" => ProcessTime::Zero,
                "elapsed" => ProcessTime::Elapsed,
                other => return Err(format!("unknown process_time {other:?}")),
            };
        }
        if let Some(random) = self.random {
            policy.random_start = match random.kind.as_str() {
                "system" => RandomStart::System,
                "host" => RandomStart::CallHost,
                "seed" => RandomStart::Seed(match (random.int, random.float, random.str, random.bytes) {
                    (Some(digits), None, None, None) => {
                        RandomSeed::Int(digits.parse().map_err(|_| format!("invalid int seed {digits:?}"))?)
                    }
                    (None, Some(f), None, None) => RandomSeed::Float(f),
                    (None, None, Some(s), None) => RandomSeed::Str(s),
                    (None, None, None, Some(b)) => {
                        RandomSeed::Bytes(BASE64.decode(b).map_err(|_| "invalid base64 bytes seed".to_owned())?)
                    }
                    _ => return Err("a random seed needs exactly one of int, float, str or bytes".to_owned()),
                }),
                other => return Err(format!("unknown random start {other:?}")),
            };
        }
        Ok(policy)
    }
}

// ---------------------------------------------------------------------------
// Print writer that collects output
// ---------------------------------------------------------------------------

/// Collects print output as ordered `(stream, text)` runs, so stdout and
/// stderr written in the same turn reach the host interleaved as they were
/// printed.
struct CollectPrintWriter {
    entries: Vec<(&'static str, String)>,
}

impl CollectPrintWriter {
    fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// The run of `stream` to append to, starting a new one when the stream
    /// changes.
    fn run(&mut self, stream: &'static str) -> &mut String {
        if self.entries.last().map(|(s, _)| *s) != Some(stream) {
            self.entries.push((stream, String::new()));
        }
        &mut self.entries.last_mut().expect("just pushed").1
    }
}

impl PrintWriterCallback for CollectPrintWriter {
    fn stdout_write(&mut self, output: Cow<'_, str>) -> Result<(), MontyException> {
        self.run("stdout").push_str(&output);
        Ok(())
    }

    fn stdout_push(&mut self, end: char) -> Result<(), MontyException> {
        self.run("stdout").push(end);
        Ok(())
    }

    fn stderr_write(&mut self, output: Cow<'_, str>) -> Result<(), MontyException> {
        self.run("stderr").push_str(&output);
        Ok(())
    }

    fn stderr_push(&mut self, end: char) -> Result<(), MontyException> {
        self.run("stderr").push(end);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// RunProgress -> ProgressResult conversion
// ---------------------------------------------------------------------------

fn progress_to_result(progress: RunProgress, param_registry: &HashMap<String, Vec<String>>) -> ProgressResult {
    match progress {
        RunProgress::Complete(value) => ProgressResult {
            status: "complete",
            value: Some(monty_to_json(value.as_ref())),
            ..Default::default()
        },
        RunProgress::FunctionCall(call) => {
            let merged = merge_args(&call.function_name, &call.args, param_registry);
            let function_name = call.function_name.clone();
            let call_id = call.call_id;
            let method_call = call.object_id.is_some();
            let handle = with_state(|s| {
                let h = s.next_handle();
                s.snapshots.insert(h, SnapshotState::FunctionCall(call));
                h
            });
            ProgressResult {
                status: "function_call",
                snapshot_handle: Some(handle),
                function_name: Some(function_name),
                args: Some(merged),
                call_id: Some(call_id),
                method_call: Some(method_call),
                ..Default::default()
            }
        }
        RunProgress::OsCall(call) => {
            // Paths in the projection are already absolute (relative ones are
            // resolved against the sandbox cwd) and lexically normalized.
            let args = call.function_call.clone().to_args();
            let function_name = call.function_call.name();
            let call_id = call.call_id;
            let accepts_future = OsFunctionCall::accepts_future(function_name);
            let handle = with_state(|s| {
                let h = s.next_handle();
                s.snapshots.insert(h, SnapshotState::OsCall(call));
                h
            });
            ProgressResult {
                status: "os_call",
                snapshot_handle: Some(handle),
                os_function: Some(function_name.to_owned()),
                args: Some(call_args_to_json(&args)),
                kwargs: Some(call_kwargs_to_json(&args)),
                call_id: Some(call_id),
                accepts_future,
                ..Default::default()
            }
        }
        RunProgress::ResolveFutures(state) => {
            let pending = state.pending_call_ids().to_vec();
            let handle = with_state(|s| {
                let h = s.next_handle();
                s.snapshots.insert(h, SnapshotState::ResolveFutures(state));
                h
            });
            ProgressResult {
                status: "resolve_futures",
                snapshot_handle: Some(handle),
                pending_call_ids: Some(pending),
                ..Default::default()
            }
        }
        // NameLookup should be resolved internally by `drive_progress` before
        // reaching this point. If one slips through we treat it as an error.
        RunProgress::NameLookup(lookup) => ProgressResult {
            status: "error",
            error: Some(format!("internal error: unhandled name lookup for '{}'", lookup.name)),
            ..Default::default()
        },
    }
}

/// Drives a RunProgress chain, auto-resolving NameLookup events against the
/// registered external function set. Returns a `(status_code, ProgressResult)`
/// pair suitable for the WASM wire format. NameLookup events whose name is
/// present in `param_registry` resolve to a host function; unknown names
/// resolve to `Undefined`, which the VM surfaces as `NameError`.
fn drive_progress(
    mut result: Result<RunProgress, MontyException>,
    print_writer: &mut CollectPrintWriter,
    param_registry: &HashMap<String, Vec<String>>,
) -> (u32, ProgressResult) {
    loop {
        match result {
            Ok(RunProgress::NameLookup(lookup)) => {
                let nlr = if param_registry.contains_key(&lookup.name) {
                    NameLookupResult::Value(MontyObject::function(lookup.name.clone(), None))
                } else {
                    NameLookupResult::Undefined
                };
                let pw = PrintWriter::Callback(print_writer);
                result = lookup.resume(nlr, pw);
            }
            // A call to a name that's not a registered external function is a
            // NameError — monty's bytecode compiler emits `LoadGlobalCallable`
            // / `LoadLocalCallable` which bypass `NameLookup` and yield a
            // `FunctionCall` directly, so the undeclared case is caught here
            // and answered with `NotFound`, which raises that NameError.
            Ok(RunProgress::FunctionCall(call)) if !param_registry.contains_key(&call.function_name) => {
                let name = call.function_name.clone();
                let pw = PrintWriter::Callback(print_writer);
                result = call.resume(ExtFunctionResult::NotFound(name), pw);
            }
            Ok(progress) => {
                let mut pr = progress_to_result(progress, param_registry);
                pr.output = std::mem::take(&mut print_writer.entries);
                let status = match pr.status {
                    "complete" => 1,
                    "function_call" => 2,
                    "os_call" => 3,
                    "resolve_futures" => 4,
                    _ => 0,
                };
                return (status, pr);
            }
            Err(e) => {
                let mut pr = error_result(&e);
                pr.output = std::mem::take(&mut print_writer.entries);
                return (0, pr);
            }
        }
    }
}

fn error_result(err: &MontyException) -> ProgressResult {
    str_error_result(&format!("{err}"))
}

fn str_error_result(msg: &str) -> ProgressResult {
    ProgressResult {
        status: "error",
        error: Some(msg.to_owned()),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Memory management exports
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn wasm_alloc(size: u32) -> u32 {
    if size == 0 {
        return 0;
    }
    let layout = Layout::from_size_align(size as usize, 1).unwrap();
    let ptr = unsafe { alloc(layout) };
    if ptr.is_null() {
        return 0;
    }
    ptr as u32
}

#[no_mangle]
pub extern "C" fn wasm_dealloc(ptr: u32, size: u32) {
    if ptr == 0 || size == 0 {
        return;
    }
    let layout = Layout::from_size_align(size as usize, 1).unwrap();
    unsafe {
        dealloc(ptr as *mut u8, layout);
    }
}

// ---------------------------------------------------------------------------
// Result buffer exports
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn monty_result_len() -> u32 {
    RESULT_BUF.lock().unwrap().len() as u32
}

#[no_mangle]
pub extern "C" fn monty_result_read(buf_ptr: u32, buf_cap: u32) -> u32 {
    let result = RESULT_BUF.lock().unwrap();
    let len = result.len().min(buf_cap as usize);
    unsafe {
        std::ptr::copy_nonoverlapping(result.as_ptr(), buf_ptr as *mut u8, len);
    }
    len as u32
}

// ---------------------------------------------------------------------------
// Feasibility check (kept from phase 1)
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn monty_check() -> u32 {
    let result = std::panic::catch_unwind(|| {
        let mut runner =
            MontyRun::new("x + 1".to_owned(), "check.py", vec!["x".to_owned()], CompileOptions::default()).ok()?;
        let result = runner.run_no_limits(vec![MontyObject::int(41)]).ok()?;
        result.as_ref().as_int().map(|v| v as u32)
    });
    match result {
        Ok(Some(v)) => v,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Core API exports
// ---------------------------------------------------------------------------

/// Read a UTF-8 string from WASM linear memory.
unsafe fn read_str(ptr: u32, len: u32) -> String {
    if ptr == 0 || len == 0 {
        return String::new();
    }
    let slice = std::slice::from_raw_parts(ptr as *const u8, len as usize);
    String::from_utf8_lossy(slice).into_owned()
}

/// Parse a JSON array of plain values into Vec<MontyObject>.
fn parse_inputs(json_str: &str) -> Vec<MontyObject> {
    if json_str.is_empty() {
        return vec![];
    }
    let values: Vec<JsonValue> = serde_json::from_str(json_str).unwrap_or_default();
    values.iter().map(json_to_monty).collect()
}

/// A host's JSON reply, as the result the interpreter resumes with.
enum HostReply {
    /// Resume normally with a value or a raised exception.
    Result(ExtFunctionResult),
    /// Service the call asynchronously; see `monty_resume`.
    Future,
    /// End the run with an exception sandboxed code cannot catch.
    Abort(MontyException),
}

/// Turn a host's JSON reply into what the interpreter resumes with.
///
/// Normally that is [`ExtFunctionResult::Return`] carrying the converted value.
/// The tagged forms say otherwise:
///
/// - `{"__monty__": "raise", "type": "FileNotFoundError", "message": "..."}`
///   resumes with [`ExtFunctionResult::Error`], so the guest sees a real
///   Python exception it can `try`/`except`. Without it a host that cannot
///   service a call had no way to say so: the only options were to return a
///   value the guest would misread, or to tear the whole run down.
/// - `{"__monty__": "abort", "type": ..., "message": ...}` ends the run with
///   that exception at the call site, uncatchably, with a traceback — for a
///   host limit (a sleep budget) that must not be swallowed by a bare
///   `except`.
/// - `{"__monty__": "future"}` leaves the call pending.
///
/// An unparseable `type` falls back to `OSError` rather than being returned as
/// a value, because a reply asking to raise must never resume as a success.
fn parse_host_reply(json_str: &str) -> HostReply {
    if json_str.is_empty() {
        return HostReply::Result(ExtFunctionResult::Return(MontyObject::none()));
    }
    let value: JsonValue = serde_json::from_str(json_str).unwrap_or(JsonValue::Null);
    if let Some(map) = value.as_object() {
        let exception = || {
            let exc_type = map
                .get("type")
                .and_then(JsonValue::as_str)
                .and_then(|name| name.parse::<ExcType>().ok())
                .unwrap_or(ExcType::OSError);
            let message = map.get("message").and_then(JsonValue::as_str).map(str::to_owned);
            MontyException::new(exc_type, message)
        };
        match map.get(MONTY_TAG).and_then(JsonValue::as_str) {
            Some("future") => return HostReply::Future,
            Some("raise") => return HostReply::Result(ExtFunctionResult::Error(exception())),
            Some("abort") => return HostReply::Abort(exception()),
            _ => {}
        }
    }
    HostReply::Result(ExtFunctionResult::Return(json_to_monty(&value)))
}

/// Compile Python code. Returns a runner handle (>0) on success, 0 on error.
#[no_mangle]
pub extern "C" fn monty_compile(
    code_ptr: u32,
    code_len: u32,
    input_names_ptr: u32,
    input_names_len: u32,
    ext_funcs_ptr: u32,
    ext_funcs_len: u32,
) -> u32 {
    let code = unsafe { read_str(code_ptr, code_len) };
    let input_names_json = unsafe { read_str(input_names_ptr, input_names_len) };
    let ext_funcs_json = unsafe { read_str(ext_funcs_ptr, ext_funcs_len) };

    let input_names: Vec<String> = if input_names_json.is_empty() {
        vec![]
    } else {
        serde_json::from_str(&input_names_json).unwrap_or_default()
    };

    let ext_func_defs: Vec<ExtFuncDef> = if ext_funcs_json.is_empty() {
        vec![]
    } else {
        serde_json::from_str(&ext_funcs_json).unwrap_or_default()
    };

    // Store the external function registry. Names double as the "known
    // externals" set used to resolve NameLookup events — monty >= 0.0.8
    // auto-detects external functions at call sites and yields a NameLookup
    // that the host resolves on demand.
    {
        let mut registry_guard = PARAM_REGISTRY.lock().unwrap();
        let registry = registry_guard.get_or_insert_with(HashMap::new);
        registry.clear();
        for def in &ext_func_defs {
            registry.insert(def.name.clone(), def.params.clone());
        }
    }

    match MontyRun::new(code, "script.py", input_names, CompileOptions::default()) {
        Ok(runner) => with_state(|s| {
            let handle = s.next_handle();
            s.runners.insert(handle, runner);
            handle
        }),
        Err(e) => {
            set_result_json(&error_result(&e));
            0
        }
    }
}

/// Configure a compiled runner before it starts: its working directory and
/// its OS policy (clocks, sleeps, random state). Returns 1 on success, 0 on
/// error with the message in the result buffer.
#[no_mangle]
pub extern "C" fn monty_configure(runner_handle: u32, config_ptr: u32, config_len: u32) -> u32 {
    let config_json = unsafe { read_str(config_ptr, config_len) };
    let config: ConfigureInput = if config_json.is_empty() {
        ConfigureInput::default()
    } else {
        match serde_json::from_str(&config_json) {
            Ok(config) => config,
            Err(e) => {
                set_result_json(&str_error_result(&format!("invalid configuration: {e}")));
                return 0;
            }
        }
    };
    let policy = match config.os_policy.map(OsPolicyInput::into_policy).transpose() {
        Ok(policy) => policy,
        Err(e) => {
            set_result_json(&str_error_result(&format!("invalid os policy: {e}")));
            return 0;
        }
    };
    if let Some(cwd) = &config.cwd {
        if let Err(e) = monty_types::validate_cwd(cwd) {
            set_result_json(&str_error_result(&format!("invalid {e}")));
            return 0;
        }
    }

    with_state(|s| {
        let Some(runner) = s.runners.remove(&runner_handle) else {
            set_result_json(&str_error_result("invalid runner handle"));
            return 0;
        };
        let mut runner = match policy {
            Some(policy) => runner.with_os_policy(policy),
            None => runner,
        };
        if let Some(cwd) = &config.cwd {
            runner.set_cwd(cwd);
        }
        s.runners.insert(runner_handle, runner);
        1
    })
}

/// Start execution. Returns a status code:
///   1 = complete, 2 = function_call, 3 = os_call, 4 = resolve_futures, 0 = error
#[no_mangle]
pub extern "C" fn monty_start(
    runner_handle: u32,
    inputs_ptr: u32,
    inputs_len: u32,
    limits_ptr: u32,
    limits_len: u32,
) -> u32 {
    let runner = with_state(|s| s.runners.remove(&runner_handle));
    let runner = match runner {
        Some(r) => r,
        None => {
            set_result_json(&str_error_result("invalid runner handle"));
            return 0;
        }
    };

    // Parse inputs as plain JSON -> MontyObject.
    let inputs_json = unsafe { read_str(inputs_ptr, inputs_len) };
    let inputs = parse_inputs(&inputs_json);

    // Parse limits.
    let limits_json = unsafe { read_str(limits_ptr, limits_len) };
    let limits_input: LimitsInput = if limits_json.is_empty() {
        LimitsInput::default()
    } else {
        serde_json::from_str(&limits_json).unwrap_or_default()
    };

    let resource_limits = ResourceLimits {
        max_feed_duration: limits_input.max_duration_ms.map(Duration::from_millis),
        max_turn_duration: limits_input.max_turn_duration_ms.map(Duration::from_millis),
        max_memory: limits_input.max_memory,
        gc_interval: None,
        max_recursion_depth: limits_input.max_recursion_depth.unwrap_or(DEFAULT_MAX_RECURSION_DEPTH),
        max_suspensions: limits_input.max_suspensions.unwrap_or(DEFAULT_MAX_SUSPENSIONS),
        max_total_sleep: limits_input.max_total_sleep_ms.map(Duration::from_millis),
    };
    // The interpreter raises MemoryError at its checkpoints once usage crosses
    // max_memory. The allocator's hard ceiling sits above that with headroom
    // for the exception machinery, and traps the instance if an allocation
    // between checkpoints overshoots it — or if the limit is unset, never.
    let _ = monty_alloc::set_hard_limit(memory_limit_with_headroom(resource_limits.max_memory, false));
    let tracker = ResourceTracker::new(resource_limits);

    let mut print_writer = CollectPrintWriter::new();
    let initial = {
        let pw = PrintWriter::Callback(&mut print_writer);
        runner.start(inputs, tracker, pw)
    };

    let (status, result) = with_param_registry(|reg| drive_progress(initial, &mut print_writer, reg));
    set_result_json(&result);
    status
}

/// Resume execution after a function call or OS call.
#[no_mangle]
pub extern "C" fn monty_resume(snapshot_handle: u32, return_value_ptr: u32, return_value_len: u32) -> u32 {
    let snapshot = with_state(|s| s.snapshots.remove(&snapshot_handle));

    let return_json = unsafe { read_str(return_value_ptr, return_value_len) };
    // A host may answer with a value, ask for a Python exception to be raised
    // at the call site, defer the answer, or abort the run.
    let reply = parse_host_reply(&return_json);

    let mut print_writer = CollectPrintWriter::new();

    // Dispatch on the stored variant. Each per-variant struct (FunctionCall,
    // OsCall) has its own `resume()` method that consumes self.
    let initial = {
        let pw = PrintWriter::Callback(&mut print_writer);
        match (snapshot, reply) {
            // `{"__monty__": "future"}` means the host is servicing this call
            // asynchronously: the guest gets an awaitable, and the result
            // arrives later through `monty_resume_futures`. The call id
            // correlating the two is only known here, which is why this is
            // not folded into `parse_host_reply`.
            (Some(SnapshotState::FunctionCall(call)), HostReply::Future) => {
                let call_id = call.call_id;
                call.resume(ExtFunctionResult::Future(call_id), pw)
            }
            (Some(SnapshotState::FunctionCall(call)), HostReply::Result(result)) => call.resume(result, pw),
            (Some(SnapshotState::FunctionCall(call)), HostReply::Abort(exc)) => call.abort(exc, pw),
            // Only `asyncio.sleep` has an await point behind it; any other OS
            // call is a plain call in the guest, so a future would leave a
            // coroutine nobody awaits. The Go bridge refuses Pending for those
            // before it gets here.
            (Some(SnapshotState::OsCall(call)), HostReply::Future) => {
                if OsFunctionCall::accepts_future(call.function_call.name()) {
                    let call_id = call.call_id;
                    call.resume(ExtFunctionResult::Future(call_id), pw)
                } else {
                    let exc = MontyException::new(
                        ExcType::RuntimeError,
                        Some(format!("'{}' cannot be answered with a future", call.function_call.name())),
                    );
                    call.abort(exc, pw)
                }
            }
            (Some(SnapshotState::OsCall(call)), HostReply::Result(result)) => call.resume(result, pw),
            (Some(SnapshotState::OsCall(call)), HostReply::Abort(exc)) => call.abort(exc, pw),
            _ => {
                set_result_json(&str_error_result("invalid snapshot handle"));
                return 0;
            }
        }
    };

    let (status, result) = with_param_registry(|reg| drive_progress(initial, &mut print_writer, reg));
    set_result_json(&result);
    status
}

/// Resume execution after resolving futures.
#[no_mangle]
pub extern "C" fn monty_resume_futures(snapshot_handle: u32, results_ptr: u32, results_len: u32) -> u32 {
    let snapshot = with_state(|s| s.snapshots.remove(&snapshot_handle));
    let snapshot = match snapshot {
        Some(SnapshotState::ResolveFutures(s)) => s,
        _ => {
            set_result_json(&str_error_result("invalid future snapshot handle"));
            return 0;
        }
    };

    // Parse results as array of [call_id, plain_json_value] pairs.
    let results_json = unsafe { read_str(results_ptr, results_len) };
    let pairs: Vec<(u32, JsonValue)> = if results_json.is_empty() {
        vec![]
    } else {
        serde_json::from_str(&results_json).unwrap_or_default()
    };

    let mut results = Vec::with_capacity(pairs.len());
    let mut abort = None;
    for (id, val) in pairs {
        // Each resolved future carries a value or a request to raise, the
        // same as a plain resume.
        let json = serde_json::to_string(&val).unwrap_or_default();
        match parse_host_reply(&json) {
            HostReply::Result(result) => results.push((id, result)),
            HostReply::Abort(exc) => abort = Some(exc),
            HostReply::Future => {
                set_result_json(&str_error_result("a resolved future cannot itself be a future"));
                return 0;
            }
        }
    }

    let mut print_writer = CollectPrintWriter::new();
    let initial = {
        let pw = PrintWriter::Callback(&mut print_writer);
        match abort {
            Some(exc) => snapshot.abort(exc, pw),
            None => snapshot.resume(results, pw),
        }
    };

    let (status, result) = with_param_registry(|reg| drive_progress(initial, &mut print_writer, reg));
    set_result_json(&result);
    status
}

/// Free a runner handle.
#[no_mangle]
pub extern "C" fn monty_free_runner(handle: u32) {
    with_state(|s| {
        s.runners.remove(&handle);
    });
}

/// Free a snapshot handle.
#[no_mangle]
pub extern "C" fn monty_free_snapshot(handle: u32) {
    with_state(|s| {
        s.snapshots.remove(&handle);
    });
}
