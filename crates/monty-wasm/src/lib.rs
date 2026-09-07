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
use monty_types::{
    CompileOptions, DEFAULT_MAX_RECURSION_DEPTH, DEFAULT_MAX_SUSPENSIONS, DictPairs, ExcType,
    ExtFunctionResult, MontyDate, MontyDateTime, MontyException, MontyFileHandle, MontyObject,
    MontyTime, MontyTimeDelta, MontyTimeZone, NameLookupResult, PrintWriter, PrintWriterCallback,
    ResourceLimits, ResourceTracker, stat_result,
};
use serde::{Deserialize, Serialize};

/// monty enforces `max_memory` in the global allocator, so the limit only
/// applies when this allocator is installed and armed via
/// `monty_alloc::set_limit` before each run.
#[global_allocator]
static ALLOC: monty_alloc::LimitedAllocator = monty_alloc::LimitedAllocator;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::Value as JsonValue;

// ---------------------------------------------------------------------------
// MontyObject <-> serde_json::Value conversion
// ---------------------------------------------------------------------------

/// Convert MontyObject to a plain JSON value (not the tagged enum format).
fn monty_to_json(obj: &MontyObject) -> JsonValue {
    match obj {
        MontyObject::None => JsonValue::Null,
        MontyObject::Bool(b) => JsonValue::Bool(*b),
        MontyObject::Int(i) => serde_json::json!(*i),
        MontyObject::BigInt(bi) => {
            // Fits in i64: a plain JSON number, which round-trips as an int.
            // Beyond that it is tagged, because a bare string would come back
            // as a `str` and silently stop being a number.
            if let Ok(v) = i64::try_from(bi) {
                serde_json::json!(v)
            } else {
                tagged("bigint", [("value", JsonValue::String(bi.to_string()))])
            }
        }
        MontyObject::Float(f) => serde_json::json!(*f),
        MontyObject::String(s) => JsonValue::String(s.clone()),
        // Tagged, not a bare int array: a host servicing `Path.write_bytes`
        // or a binary write has to be able to tell `bytes` from a
        // `list[int]`, and an array alone cannot say which it was. The
        // payload is base64 rather than a list of decimal ints, which would
        // inflate a file roughly fourfold on its way through the JSON
        // boundary and count that against the run's memory limit.
        MontyObject::Bytes(b) => tagged(
            "bytes",
            [("base64", JsonValue::String(BASE64.encode(b)))],
        ),
        MontyObject::List(items) => {
            JsonValue::Array(items.iter().map(monty_to_json).collect())
        }
        // A plain array, like `list`. Tagging it would tell the host which of
        // the two it got, but at the cost of every existing handler that
        // reads an argument as `[]any` — and upstream's own hosts see a list
        // here too. A host that needs the distinction can still *send* a
        // `tuple`; see the direction note on `json_to_tagged`.
        MontyObject::Tuple(items) => items_to_json(items),
        MontyObject::Dict(pairs) => {
            let mut map = serde_json::Map::new();
            let mut collides = false;
            for (k, v) in pairs {
                let key = match k {
                    MontyObject::String(s) => s.clone(),
                    other => format!("{:?}", other),
                };
                collides |= key == MONTY_TAG;
                map.insert(key, monty_to_json(v));
            }
            // A guest dict is data, and it must not be able to impersonate a
            // typed value just by carrying the reserved key. One that does is
            // sent in the tagged `dict` form instead, so the host can tell a
            // real `bytes` or `Path` from sandboxed code claiming to be one.
            if collides {
                let items = pairs
                    .into_iter()
                    .map(|(k, v)| JsonValue::Array(vec![monty_to_json(k), monty_to_json(v)]))
                    .collect();
                return tagged("dict", [("items", JsonValue::Array(items))]);
            }
            JsonValue::Object(map)
        }
        MontyObject::Set(items) | MontyObject::FrozenSet(items) => items_to_json(items),
        MontyObject::Ellipsis => JsonValue::String("...".to_owned()),
        // A bare string. Every filesystem OS call carries a path as its first
        // argument, and a host reads it as one — tagging it would break that
        // for no benefit, since a handler uses the path rather than echoing
        // it back.
        MontyObject::Path(p) => JsonValue::String(p.clone()),
        MontyObject::Exception { exc_type, arg } => {
            let name: &'static str = exc_type.into();
            let mut map = serde_json::Map::new();
            map.insert("exception".to_owned(), JsonValue::String(name.to_owned()));
            if let Some(a) = arg {
                map.insert("message".to_owned(), JsonValue::String(a.clone()));
            }
            JsonValue::Object(map)
        }
        MontyObject::NamedTuple {
            type_name,
            field_names,
            values,
        } => {
            let mut map = serde_json::Map::new();
            map.insert("__type__".to_owned(), JsonValue::String(type_name.clone()));
            for (name, val) in field_names.iter().zip(values.iter()) {
                map.insert(name.clone(), monty_to_json(val));
            }
            JsonValue::Object(map)
        }
        MontyObject::ClassInstance(instance) => {
            let mut map = serde_json::Map::new();
            map.insert(
                "__type__".to_owned(),
                JsonValue::String(instance.class_type.name.clone()),
            );
            // attrs preserve declaration order, so no field-name list is needed.
            for (k, v) in instance.attrs.iter() {
                if let MontyObject::String(key) = k {
                    map.insert(key.clone(), monty_to_json(v));
                }
            }
            JsonValue::Object(map)
        }
        // Everything below has no plain-JSON spelling, so it crosses in the
        // same tagged form a host uses to send one in (see `json_to_tagged`).
        // Before this existed these variants reached the host as a Rust
        // `{:?}` debug string, which no caller could do anything with.
        MontyObject::FileHandle(handle) => file_handle_to_json(handle),
        MontyObject::NotImplemented => tagged("notimplemented", []),
        MontyObject::Date(d) => tagged(
            "date",
            [
                ("year", serde_json::json!(d.year)),
                ("month", serde_json::json!(d.month)),
                ("day", serde_json::json!(d.day)),
            ],
        ),
        MontyObject::DateTime(dt) => tagged(
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
        MontyObject::Time(t) => tagged(
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
        MontyObject::TimeDelta(td) => tagged(
            "timedelta",
            [
                ("days", serde_json::json!(td.days)),
                ("seconds", serde_json::json!(td.seconds)),
                ("microseconds", serde_json::json!(td.microseconds)),
            ],
        ),
        MontyObject::TimeZone(tz) => tagged(
            "timezone",
            [
                ("offset_seconds", serde_json::json!(tz.offset_seconds)),
                ("name", opt_str(&tz.name)),
            ],
        ),
        MontyObject::Type(ty) => {
            let name: &'static str = ty.into();
            tagged("type", [("name", JsonValue::String(name.to_owned()))])
        }
        MontyObject::BuiltinFunction(func) => {
            let name: &'static str = func.into();
            tagged("builtin", [("name", JsonValue::String(name.to_owned()))])
        }
        MontyObject::Function { name, docstring } => tagged(
            "function",
            [
                ("name", JsonValue::String(name.clone())),
                ("docstring", opt_str(docstring)),
            ],
        ),
        MontyObject::Repr(repr) => tagged("repr", [("repr", JsonValue::String(repr.clone()))]),
        // A cycle marker: the repr monty would print, plus the depth at which
        // the value referred back to itself.
        MontyObject::Cycle(depth, repr) => tagged(
            "cycle",
            [
                ("depth", serde_json::json!(depth)),
                ("repr", JsonValue::String(repr.clone())),
            ],
        ),
    }
}

/// Convert a plain JSON value to a MontyObject.
fn json_to_monty(val: &JsonValue) -> MontyObject {
    match val {
        JsonValue::Null => MontyObject::None,
        JsonValue::Bool(b) => MontyObject::Bool(*b),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                MontyObject::Int(i)
            } else if let Some(f) = n.as_f64() {
                MontyObject::Float(f)
            } else {
                MontyObject::None
            }
        }
        JsonValue::String(s) => MontyObject::String(s.clone()),
        JsonValue::Array(items) => {
            MontyObject::List(items.iter().map(json_to_monty).collect())
        }
        JsonValue::Object(map) => {
            // A tagged object is a typed Monty value (bytes, datetime, a file
            // handle, ...); anything else is an ordinary dict.
            if let Some(obj) = json_to_tagged(map) {
                return obj;
            }
            let pairs: Vec<(MontyObject, MontyObject)> = map
                .iter()
                .map(|(k, v)| (MontyObject::String(k.clone()), json_to_monty(v)))
                .collect();
            MontyObject::Dict(DictPairs::from(pairs))
        }
    }
}

/// Reserved key marking a JSON object as a typed Monty value rather than a
/// plain `dict`. Its value names the type; sibling keys carry the payload.
///
/// JSON on its own can express only the handful of shapes that map to
/// [`MontyObject`] one-for-one (null, bool, number, string, array, object), so
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

/// A list of `MontyObject`s as a JSON array.
fn items_to_json(items: &[MontyObject]) -> JsonValue {
    JsonValue::Array(items.iter().map(monty_to_json).collect())
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
/// | `bytes` | `data`: array of 0-255 ints, **or** `text`: a string (encoded UTF-8) | `bytes` |
/// | `tuple` / `set` / `frozenset` | `items`: array | `tuple` / `set` / `frozenset` |
/// | `namedtuple` | `type`: string, `fields`: array of `[name, value]` pairs | a named tuple |
/// | `stat` | the `st_*` fields (`st_mode`, `st_size`, `st_mtime`, ...) | `os.stat_result` |
/// | `path` | `path`: string | `pathlib.Path` |
/// | `date` | `year`, `month`, `day` | `datetime.date` |
/// | `datetime` | `year`..`microsecond`, optional `offset_seconds`, `timezone_name` | `datetime.datetime` |
/// | `time` | `hour`..`microsecond`, optional `offset_seconds`, `timezone_name`, `fold` | `datetime.time` |
/// | `timedelta` | `days`, `seconds`, `microseconds` | `datetime.timedelta` |
/// | `timezone` | `offset_seconds`, optional `name` | `datetime.timezone` |
/// | `exception` | `type`: an exception name, optional `message` | an exception *value* |
/// | `bigint` | `value`: the digits as a string | an arbitrary-precision `int` |
/// | `file` | `path`, `mode`, optional `position` | an `_io` file object |
/// | `type` | `name`: a type name | a `type` |
/// | `builtin` | `name`: a builtin's name | a builtin function |
/// | `function` | `name`, optional `docstring` | a function |
/// | `repr` | `repr`: string | an opaque object with that repr |
/// | `ellipsis` | — | `Ellipsis` |
/// | `notimplemented` | — | `NotImplemented` |
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
/// carry faithfully — `bytes`, the `datetime` family, a file handle, an `int`
/// past i64, and the type/builtin/function/repr values that used to cross as a
/// Rust debug string. A `tuple`, `set`, `Path`, named tuple, exception value
/// or `...` keeps its plain JSON shape on the way out, because a host reads
/// those as the array, string or object they already look like — every
/// filesystem OS call passes its path as a string — and tagging them would
/// break that for no gain. So a host can send more shapes than it receives,
/// which is the useful asymmetry: it is answering calls, not echoing them.
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
    };

    match kind {
        "bytes" => {
            // base64 is the form this bridge sends; `text` and `data` are
            // accepted conveniences for a host building one by hand.
            if let Some(encoded) = s("base64") {
                return BASE64.decode(encoded).ok().map(MontyObject::Bytes);
            }
            if let Some(text) = s("text") {
                return Some(MontyObject::Bytes(text.as_bytes().to_vec()));
            }
            let data = map.get("data")?.as_array()?;
            let mut out = Vec::with_capacity(data.len());
            for byte in data {
                out.push(u8::try_from(byte.as_u64()?).ok()?);
            }
            Some(MontyObject::Bytes(out))
        }
        "tuple" => Some(MontyObject::Tuple(items("items").unwrap_or_default())),
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
            Some(MontyObject::Dict(DictPairs::from(out)))
        }
        "set" => Some(MontyObject::Set(items("items").unwrap_or_default())),
        "frozenset" => Some(MontyObject::FrozenSet(items("items").unwrap_or_default())),
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
            Some(MontyObject::NamedTuple {
                type_name: opt_string("type").unwrap_or_else(|| "namedtuple".to_owned()),
                field_names,
                values,
            })
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
        "path" => Some(MontyObject::Path(opt_string("path")?)),
        "date" => Some(MontyObject::Date(MontyDate {
            year: i32::try_from(i("year")?).ok()?,
            month: u8::try_from(u("month")?).ok()?,
            day: u8::try_from(u("day")?).ok()?,
        })),
        "datetime" => Some(MontyObject::DateTime(MontyDateTime {
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
        "time" => Some(MontyObject::Time(MontyTime {
            hour: u8::try_from(u("hour").unwrap_or(0)).ok()?,
            minute: u8::try_from(u("minute").unwrap_or(0)).ok()?,
            second: u8::try_from(u("second").unwrap_or(0)).ok()?,
            microsecond: u32::try_from(u("microsecond").unwrap_or(0)).ok()?,
            offset_seconds: i("offset_seconds").and_then(|v| i32::try_from(v).ok()),
            timezone_name: opt_string("timezone_name"),
            fold: u8::try_from(u("fold").unwrap_or(0)).ok()?,
        })),
        "timedelta" => Some(MontyObject::TimeDelta(MontyTimeDelta {
            days: i32::try_from(i("days").unwrap_or(0)).ok()?,
            seconds: i32::try_from(i("seconds").unwrap_or(0)).ok()?,
            microseconds: i32::try_from(i("microseconds").unwrap_or(0)).ok()?,
        })),
        "timezone" => Some(MontyObject::TimeZone(MontyTimeZone {
            offset_seconds: i32::try_from(i("offset_seconds").unwrap_or(0)).ok()?,
            name: opt_string("name"),
        })),
        "exception" => Some(MontyObject::Exception {
            exc_type: s("type")?.parse().ok()?,
            arg: opt_string("message"),
        }),
        "bigint" => Some(MontyObject::BigInt(s("value")?.parse().ok()?)),
        "file" => Some(MontyObject::FileHandle(MontyFileHandle {
            path: opt_string("path")?,
            mode: s("mode")?.parse().ok()?,
            position: u("position").unwrap_or(0),
        })),
        "type" => Some(MontyObject::Type(s("name")?.parse().ok()?)),
        "builtin" => Some(MontyObject::BuiltinFunction(s("name")?.parse().ok()?)),
        "function" => Some(MontyObject::Function {
            name: opt_string("name")?,
            docstring: opt_string("docstring"),
        }),
        "repr" => Some(MontyObject::Repr(opt_string("repr")?)),
        "ellipsis" => Some(MontyObject::Ellipsis),
        "notimplemented" => Some(MontyObject::NotImplemented),
        _ => None,
    }
}

fn monty_args_to_json(args: &[MontyObject]) -> JsonValue {
    JsonValue::Array(args.iter().map(monty_to_json).collect())
}

fn monty_kwargs_to_json(kwargs: &[(MontyObject, MontyObject)]) -> JsonValue {
    let mut map = serde_json::Map::new();
    for (k, v) in kwargs {
        let key = match k {
            MontyObject::String(s) => s.clone(),
            other => format!("{:?}", other),
        };
        map.insert(key, monty_to_json(v));
    }
    JsonValue::Object(map)
}

/// Merge positional args and kwargs into a single JSON object.
/// Positional args are mapped to parameter names by index.
fn merge_args(
    func_name: &str,
    args: &[MontyObject],
    kwargs: &[(MontyObject, MontyObject)],
    param_registry: &HashMap<String, Vec<String>>,
) -> JsonValue {
    let mut map = serde_json::Map::new();
    // Map positional args to parameter names.
    if let Some(param_names) = param_registry.get(func_name) {
        for (i, arg) in args.iter().enumerate() {
            if i < param_names.len() {
                map.insert(param_names[i].clone(), monty_to_json(arg));
            }
        }
    }
    // Merge kwargs (overrides positional — Python semantics).
    for (k, v) in kwargs {
        if let MontyObject::String(key) = k {
            map.insert(key.clone(), monty_to_json(v));
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
    static EMPTY: std::sync::LazyLock<HashMap<String, Vec<String>>> =
        std::sync::LazyLock::new(HashMap::new);
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

#[derive(Serialize)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_call_ids: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    print_output: Option<String>,
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
    #[serde(default)]
    max_duration_ms: Option<u64>,
    #[serde(default)]
    max_memory: Option<usize>,
    #[serde(default)]
    max_recursion_depth: Option<usize>,
}

// ---------------------------------------------------------------------------
// Print writer that collects output
// ---------------------------------------------------------------------------

struct CollectPrintWriter {
    buf: String,
}

impl CollectPrintWriter {
    fn new() -> Self {
        Self { buf: String::new() }
    }
}

impl PrintWriterCallback for CollectPrintWriter {
    fn stdout_write(&mut self, output: Cow<'_, str>) -> Result<(), MontyException> {
        self.buf.push_str(&output);
        Ok(())
    }

    fn stdout_push(&mut self, end: char) -> Result<(), MontyException> {
        self.buf.push(end);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// RunProgress -> ProgressResult conversion
// ---------------------------------------------------------------------------

fn progress_to_result(
    progress: RunProgress,
    print_output: String,
    param_registry: &HashMap<String, Vec<String>>,
) -> ProgressResult {
    let print_out = if print_output.is_empty() {
        None
    } else {
        Some(print_output)
    };

    match progress {
        RunProgress::Complete(value) => ProgressResult {
            status: "complete",
            value: Some(monty_to_json(&value)),
            print_output: print_out,
            snapshot_handle: None,
            function_name: None,
            os_function: None,
            args: None,
            kwargs: None,
            call_id: None,
            method_call: None,
            pending_call_ids: None,
            error: None,
        },
        RunProgress::FunctionCall(call) => {
            let merged = merge_args(&call.function_name, &call.args, &call.kwargs, param_registry);
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
                value: None,
                snapshot_handle: Some(handle),
                function_name: Some(function_name),
                os_function: None,
                args: Some(merged),
                kwargs: None,
                call_id: Some(call_id),
                method_call: Some(method_call),
                pending_call_ids: None,
                error: None,
                print_output: print_out,
            }
        }
        RunProgress::OsCall(call) => {
            let (pos_args, kw_args) = call.function_call.clone().to_args();
            let args_json = monty_args_to_json(&pos_args);
            let kwargs_json = monty_kwargs_to_json(&kw_args);
            let function_name: &'static str = (&call.function_call).into();
            let function_str = function_name.to_string();
            let call_id = call.call_id;
            let handle = with_state(|s| {
                let h = s.next_handle();
                s.snapshots.insert(h, SnapshotState::OsCall(call));
                h
            });
            ProgressResult {
                status: "os_call",
                value: None,
                snapshot_handle: Some(handle),
                function_name: None,
                os_function: Some(function_str),
                args: Some(args_json),
                kwargs: Some(kwargs_json),
                call_id: Some(call_id),
                method_call: None,
                pending_call_ids: None,
                error: None,
                print_output: print_out,
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
                value: None,
                snapshot_handle: Some(handle),
                function_name: None,
                os_function: None,
                args: None,
                kwargs: None,
                call_id: None,
                method_call: None,
                pending_call_ids: Some(pending),
                error: None,
                print_output: print_out,
            }
        }
        // NameLookup should be resolved internally by `drive_progress` before
        // reaching this point. If one slips through we treat it as an error.
        RunProgress::NameLookup(lookup) => ProgressResult {
            status: "error",
            value: None,
            snapshot_handle: None,
            function_name: None,
            os_function: None,
            args: None,
            kwargs: None,
            call_id: None,
            method_call: None,
            pending_call_ids: None,
            error: Some(format!(
                "internal error: unhandled name lookup for '{}'",
                lookup.name
            )),
            print_output: print_out,
        },
    }
}

/// Drives a RunProgress chain, auto-resolving NameLookup events against the
/// registered external function set. Returns a `(status_code, ProgressResult)`
/// pair suitable for the WASM wire format. NameLookup events whose name is
/// present in `param_registry` resolve to a `MontyObject::Function`; unknown
/// names resolve to `Undefined`, which the VM surfaces as `NameError`.
fn drive_progress(
    mut result: Result<RunProgress, MontyException>,
    print_writer: &mut CollectPrintWriter,
    param_registry: &HashMap<String, Vec<String>>,
) -> (u32, ProgressResult) {
    loop {
        match result {
            Ok(RunProgress::NameLookup(lookup)) => {
                let nlr = if param_registry.contains_key(&lookup.name) {
                    NameLookupResult::Value(MontyObject::Function {
                        name: lookup.name.clone(),
                        docstring: None,
                    })
                } else {
                    NameLookupResult::Undefined
                };
                let pw = PrintWriter::Callback(print_writer);
                result = lookup.resume(nlr, pw);
            }
            // A call to a name that's not a registered external function is a
            // NameError — monty's bytecode compiler emits `LoadGlobalCallable`
            // / `LoadLocalCallable` which bypass `NameLookup` and yield a
            // `FunctionCall` directly, so we have to catch the undeclared case
            // here and auto-resume with a NameError exception.
            Ok(RunProgress::FunctionCall(call)) if !param_registry.contains_key(&call.function_name) => {
                let exc = MontyException::new(
                    ExcType::NameError,
                    Some(format!("name '{}' is not defined", call.function_name)),
                );
                let pw = PrintWriter::Callback(print_writer);
                result = call.resume(ExtFunctionResult::Error(exc), pw);
            }
            Ok(progress) => {
                let print_output = print_writer.buf.clone();
                let pr = progress_to_result(progress, print_output, param_registry);
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
                let print_output = print_writer.buf.clone();
                return (0, error_result(&e, print_output));
            }
        }
    }
}

fn error_result(err: &MontyException, print_output: String) -> ProgressResult {
    str_error_result(&format!("{err}"), print_output)
}

fn str_error_result(msg: &str, print_output: String) -> ProgressResult {
    let print_out = if print_output.is_empty() {
        None
    } else {
        Some(print_output)
    };
    ProgressResult {
        status: "error",
        value: None,
        snapshot_handle: None,
        function_name: None,
        os_function: None,
        args: None,
        kwargs: None,
        call_id: None,
        method_call: None,
        pending_call_ids: None,
        error: Some(msg.to_owned()),
        print_output: print_out,
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
        let runner = MontyRun::new(
            "x + 1".to_owned(),
            "check.py",
            vec!["x".to_owned()],
            CompileOptions::default(),
        )
            .ok()?;
        let result = runner.run_no_limits(vec![MontyObject::Int(41)]).ok()?;
        match result {
            MontyObject::Int(v) => Some(v as u32),
            _ => None,
        }
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

/// Turn a host's JSON reply into the result the interpreter resumes with.
///
/// Normally that is [`ExtFunctionResult::Return`] carrying the converted value.
/// The exception is the tagged `"raise"` form —
/// `{"__monty__": "raise", "type": "FileNotFoundError", "message": "..."}` —
/// which resumes with [`ExtFunctionResult::Error`] so the guest sees a real
/// Python exception it can `try`/`except`. Without it a host that cannot
/// service a call had no way to say so: the only options were to return a
/// value the guest would misread, or to tear the whole run down.
///
/// An unparseable `type` falls back to `OSError` rather than being returned as
/// a value, because a reply asking to raise must never resume as a success.
fn parse_ext_result(json_str: &str) -> ExtFunctionResult {
    if json_str.is_empty() {
        return ExtFunctionResult::Return(MontyObject::None);
    }
    let value: JsonValue = serde_json::from_str(json_str).unwrap_or(JsonValue::Null);
    if let Some(map) = value.as_object() {
        if map.get(MONTY_TAG).and_then(JsonValue::as_str) == Some("future") {
            // Marked so `monty_resume` can turn it into
            // `ExtFunctionResult::Future` with the call id it holds; nothing
            // else should ever see this sentinel as a value.
            return ExtFunctionResult::Return(MontyObject::Repr(FUTURE_SENTINEL.to_owned()));
        }
        if map.get(MONTY_TAG).and_then(JsonValue::as_str) == Some("raise") {
            let exc_type = map
                .get("type")
                .and_then(JsonValue::as_str)
                .and_then(|name| name.parse::<ExcType>().ok())
                .unwrap_or(ExcType::OSError);
            let message = map
                .get("message")
                .and_then(JsonValue::as_str)
                .map(str::to_owned);
            return ExtFunctionResult::Error(MontyException::new(exc_type, message));
        }
    }
    ExtFunctionResult::Return(json_to_monty(&value))
}

/// Marker `parse_ext_result` leaves behind for a `"future"` reply, recognised
/// by [`is_future_request`]. It is deliberately an implausible repr string so
/// it cannot collide with a value a host meant to return.
const FUTURE_SENTINEL: &str = "\u{0}__monty_future__";

/// Whether a parsed result is the future marker rather than a real value.
fn is_future_request(result: &ExtFunctionResult) -> bool {
    matches!(result, ExtFunctionResult::Return(MontyObject::Repr(r)) if r == FUTURE_SENTINEL)
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
            set_result_json(&error_result(&e, String::new()));
            0
        }
    }
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
            set_result_json(&str_error_result("invalid runner handle", String::new()));
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
        max_duration: limits_input.max_duration_ms.map(Duration::from_millis),
        max_memory: limits_input.max_memory,
        gc_interval: None,
        max_recursion_depth: limits_input
            .max_recursion_depth
            .unwrap_or(DEFAULT_MAX_RECURSION_DEPTH),
        max_suspensions: limits_input
            .max_suspensions
            .unwrap_or(DEFAULT_MAX_SUSPENSIONS),
    };
    // max_memory is enforced by the allocator, not the VM: without arming
    // monty-alloc the limit is silently ignored.
    let _ = monty_alloc::set_limit(resource_limits.max_memory, false);
    let tracker = ResourceTracker::new(resource_limits);

    let mut print_writer = CollectPrintWriter::new();
    let initial = {
        let pw = PrintWriter::Callback(&mut print_writer);
        runner.start(inputs, tracker, pw)
    };

    let (status, result) =
        with_param_registry(|reg| drive_progress(initial, &mut print_writer, reg));
    set_result_json(&result);
    status
}

/// Resume execution after a function call or OS call.
#[no_mangle]
pub extern "C" fn monty_resume(
    snapshot_handle: u32,
    return_value_ptr: u32,
    return_value_len: u32,
) -> u32 {
    let snapshot = with_state(|s| s.snapshots.remove(&snapshot_handle));

    let return_json = unsafe { read_str(return_value_ptr, return_value_len) };
    // A host may answer with a value or ask for a Python exception to be
    // raised at the call site; `parse_ext_result` distinguishes the two.
    let return_value = parse_ext_result(&return_json);

    let mut print_writer = CollectPrintWriter::new();

    // Dispatch on the stored variant. Each per-variant struct (FunctionCall,
    // OsCall) has its own `resume()` method that consumes self.
    let initial = {
        let pw = PrintWriter::Callback(&mut print_writer);
        match snapshot {
            Some(SnapshotState::FunctionCall(call)) => {
                // `{"__monty__": "future"}` means the host is servicing this
                // call asynchronously: the guest gets an awaitable, and the
                // result arrives later through `monty_resume_futures`. The
                // call id correlating the two is only known here, which is why
                // this is not folded into `parse_ext_result`.
                let result = if is_future_request(&return_value) {
                    ExtFunctionResult::Future(call.call_id)
                } else {
                    return_value
                };
                call.resume(result, pw)
            }
            // No future arm here: an OS call has no await point in the guest
            // (`Path.read_text()` is a plain call), so a future would only
            // leave a coroutine nobody awaits. The Go bridge rejects Pending
            // from an OS-call handler with that explanation.
            Some(SnapshotState::OsCall(call)) => call.resume(return_value, pw),
            _ => {
                set_result_json(&str_error_result("invalid snapshot handle", String::new()));
                return 0;
            }
        }
    };

    let (status, result) =
        with_param_registry(|reg| drive_progress(initial, &mut print_writer, reg));
    set_result_json(&result);
    status
}

/// Resume execution after resolving futures.
#[no_mangle]
pub extern "C" fn monty_resume_futures(
    snapshot_handle: u32,
    results_ptr: u32,
    results_len: u32,
) -> u32 {
    let snapshot = with_state(|s| s.snapshots.remove(&snapshot_handle));
    let snapshot = match snapshot {
        Some(SnapshotState::ResolveFutures(s)) => s,
        _ => {
            set_result_json(&str_error_result(
                "invalid future snapshot handle",
                String::new(),
            ));
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

    let results: Vec<(u32, ExtFunctionResult)> = pairs
        .into_iter()
        .map(|(id, val)| {
            // Each resolved future carries a value or a request to raise, the
            // same as a plain resume.
            let json = serde_json::to_string(&val).unwrap_or_default();
            (id, parse_ext_result(&json))
        })
        .collect();

    let mut print_writer = CollectPrintWriter::new();
    let initial = {
        let pw = PrintWriter::Callback(&mut print_writer);
        snapshot.resume(results, pw)
    };

    let (status, result) =
        with_param_registry(|reg| drive_progress(initial, &mut print_writer, reg));
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
