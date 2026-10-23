//! puppet-rs: native Rust MCP server, wire-parity with puppet.py (FastMCP 4.3.0).
//!
//! Tools: screenshot, list_windows, window, do, cursor, elements.
//! HEADLESS: windows_subsystem windows (never a console); stdout carries
//! protocol frames ONLY; diagnostics go to stderr.

#![windows_subsystem = "windows"]

mod schemas;

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use serde_json::{Map, Value};
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, FILETIME, HANDLE, HWND, LPARAM, POINT, RECT,
};
use windows_sys::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    GetThreadDesktop, GetUserObjectInformationW, UOI_NAME,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken, OpenThread,
    QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    THREAD_QUERY_INFORMATION,
};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwareness, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, PROCESS_PER_MONITOR_DPI_AWARE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// ---------------------------------------------------------------- constants

const MODE_B_NOTE: &str = "Mode B (Windows.Graphics.Capture) deferred: needs a WinRT + D3D11 frame pool in this Rust stack and is untestable on a single-monitor box here; Mode A covers the verifiable cases.";
const FORMULA: &str = "screen_x = source.x + round((img_x + 0.5) * source.w / image.w); screen_y = source.y + round((img_y + 0.5) * source.h / image.h)";
const SUPPORTED_VERSIONS: [&str; 2] = ["2025-06-18", "2025-11-25"];
const LATEST_VERSION: &str = "2025-11-25";
const MODERN_VERSION: &str = "2026-07-28";
// Batching is not implemented, so 2024-11-05 and 2025-03-26 are not advertised: do not serve what we do not support.
const ALL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-11-25", "2026-07-28"];
const ELEMENT_CAP: usize = 200;
const UIA_DEPTH_LIMIT: i64 = 8;
const WDA_EXCLUDEFROMCAPTURE: u32 = 0x11;

/// Dedupe map for stale-window reports: window handle -> (tag, rect, seen).
type StaleMap = HashMap<i64, (String, (i32, i32, i32, i32), u64)>;

static STALE: LazyLock<Mutex<StaleMap>> = LazyLock::new(|| Mutex::new(HashMap::new()));

// ---------------------------------------------------------------- python numerics

/// Python round() half-to-even to an integer value.
fn py_round_half_even(x: f64) -> f64 {
    if !x.is_finite() || x.abs() > 9.0e15 {
        return x;
    }
    let f = x.floor();
    let d = x - f;
    if d < 0.5 {
        f
    } else if d > 0.5 {
        f + 1.0
    } else if (f as i64) % 2 == 0 {
        f
    } else {
        f + 1.0
    }
}

fn py_round_n(x: f64, n: u32) -> f64 {
    let m = 10f64.powi(n as i32);
    py_round_half_even(x * m) / m
}

fn py_repr_str(s: &str) -> String {
    let needs_double = s.contains('\'') && !s.contains('"');
    let q = if needs_double { '"' } else { '\'' };
    let mut o = String::with_capacity(s.len() + 2);
    o.push(q);
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '\'' if q == '\'' => o.push_str("\\'"),
            '"' if q == '"' => o.push_str("\\\""),
            c => o.push(c),
        }
    }
    o.push(q);
    o
}

// ---------------------------------------------------------------- validation (pydantic-shaped)

struct VErr {
    loc: String,
    msg: String,
    typ: String,
    iv: String,
    it: String,
}

fn input_type_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "int"
            } else {
                "float"
            }
        }
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

fn render_iv(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => py_repr_str(s),
        Value::Array(_) | Value::Object(_) => py_repr_value(v),
    }
}

fn verr_text(tool: &str, errs: &[VErr]) -> String {
    let mut s = if errs.len() == 1 {
        format!("1 validation error for call[{tool}]")
    } else {
        format!("{} validation errors for call[{tool}]", errs.len())
    };
    for e in errs {
        s.push_str(&format!(
            "\n{}\n  {} [type={}, input_value={}, input_type={}]\n    For further information visit https://errors.pydantic.dev/2.14/v/{}",
            e.loc, e.msg, e.typ, e.iv, e.it, e.typ
        ));
    }
    s
}

/// Shared integer-overflow gate (F4): ONE policy for every int parser —
/// REJECT anything that does not fit in `i64` with one consistent message.
/// Each call site (`coerce_int`, `burst::py_int`, `screen::py_int_elem` via
/// `parse_region`) checks this first so the same out-of-range input is
/// rejected identically instead of Err vs clamp vs None.
pub(crate) fn int_overflow_err(v: &Value) -> String {
    format!("integer out of range: {v} does not fit in i64")
}

/// True when `v` parses as an integer but overflows `i64` (u64 > MAX,
/// integral float outside the i64 range, or a digit string whose checked
/// i64 accumulation overflows). Garbage (non-numeric, fractional floats,
/// bad literals) returns false so each caller keeps its own message there.
pub(crate) fn is_int_overflow(v: &Value) -> bool {
    match v {
        Value::Number(n) => {
            if n.as_i64().is_some() {
                false
            } else if let Some(u) = n.as_u64() {
                u > i64::MAX as u64
            } else if let Some(f) = n.as_f64() {
                if !f.is_finite() || f.fract() != 0.0 {
                    false
                } else {
                    const MAX_F: f64 = 9223372036854775808.0; // 2^63
                    const MIN_F: f64 = -9223372036854775808.0; // -2^63
                    !(MIN_F..MAX_F).contains(&f)
                }
            } else {
                false
            }
        }
        Value::String(s) => {
            let t = s.trim().replace('_', "");
            let t = t.as_str();
            let body = t.strip_prefix('+').unwrap_or(t);
            let neg = t.starts_with('-');
            let digits = if neg { &body[1..] } else { body };
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
            // Handle the sign edge explicitly: the magnitude of i64::MIN
            // (9223372036854775808) overflows a positive i64 accumulation,
            // so accumulate the magnitude as unsigned and compare once.
            let mut mag: u64 = 0;
            for b in digits.bytes() {
                mag = match mag
                    .checked_mul(10)
                    .and_then(|m| m.checked_add((b - b'0') as u64))
                {
                    Some(m) => m,
                    None => return true,
                };
            }
            if neg {
                mag > 9223372036854775808u64
            } else {
                mag > i64::MAX as u64
            }
        }
        _ => false,
    }
}

/// Checked region rejection reason — shared by the screenshot region
/// path and the `wait_for` stable path so both fail identically.
#[derive(Debug)]
pub(crate) enum RegionReject {
    Order,
    Extreme,
    OverCap(i128, i128),
}

/// Checked `[left,top,right,bottom]` validator. Never subtracts in `i64`
/// before validating, so `i64::MIN`/`MAX` inputs are typed errors in both
/// debug and release (no panic, no wraparound bypass). Checks, in order:
/// 1. `right>left` and `bottom>top` (inverted/empty rejected — comparison
///    only, no subtraction);
/// 2. every coordinate fits in `i32` (the BitBlt/GDI path casts to `i32` —
///    anything outside would truncate, so extremes are rejected);
/// 3. width/height computed in `i128`, rejected when over `max_edge`.
///
/// Returns `(w, h)` on success; the caller formats the cap message with
/// its own prefix so screenshot vs stable keep their typed shapes.
pub(crate) fn validate_region(
    l: i64,
    t: i64,
    r: i64,
    b: i64,
    max_edge: i64,
) -> Result<(i64, i64), RegionReject> {
    if r <= l || b <= t {
        return Err(RegionReject::Order);
    }
    for c in [l, t, r, b] {
        if c < i32::MIN as i64 || c > i32::MAX as i64 {
            return Err(RegionReject::Extreme);
        }
    }
    let w = (r as i128) - (l as i128);
    let h = (b as i128) - (t as i128);
    if w > max_edge as i128 || h > max_edge as i128 {
        return Err(RegionReject::OverCap(w, h));
    }
    Ok((w as i64, h as i64))
}

fn coerce_int(v: &Value) -> Result<i64, (String, String)> {
    if is_int_overflow(v) {
        return Err((int_overflow_err(v), "int_parsing".to_string()));
    }
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i)
            } else if let Some(u) = n.as_u64() {
                if u <= i64::MAX as u64 {
                    Ok(u as i64)
                } else {
                    Err((
                        "Input should be a valid integer".to_string(),
                        "int_type".to_string(),
                    ))
                }
            } else if let Some(f) = n.as_f64() {
                if f.is_finite()
                    && f.fract() == 0.0
                    && (-9223372036854775808.0..9223372036854775808.0).contains(&f)
                {
                    Ok(f.trunc() as i64)
                } else {
                    Err((
                        "Input should be a valid integer, got a number with a fractional part"
                            .to_string(),
                        "int_from_float".to_string(),
                    ))
                }
            } else {
                Err((
                    "Input should be a valid integer".to_string(),
                    "int_type".to_string(),
                ))
            }
        }
        Value::Bool(b) => Ok(if *b { 1 } else { 0 }),
        Value::String(s) => {
            let t = s.trim().replace('_', "");
            let t = t.as_str();
            let body = t.strip_prefix('+').unwrap_or(t);
            let neg = t.starts_with('-');
            let digits = if neg { &body[1..] } else { body };
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                let mut mag: u64 = 0;
                let mut ok = true;
                for b in digits.bytes() {
                    mag = match mag
                        .checked_mul(10)
                        .and_then(|m| m.checked_add((b - b'0') as u64))
                    {
                        Some(m) => m,
                        None => {
                            ok = false;
                            break;
                        }
                    };
                }
                if ok {
                    if neg {
                        if mag == 9223372036854775808u64 {
                            return Ok(i64::MIN);
                        }
                        if mag <= i64::MAX as u64 {
                            return Ok(-(mag as i64));
                        }
                    } else if mag <= i64::MAX as u64 {
                        return Ok(mag as i64);
                    }
                }
            }
            Err((
                "Input should be a valid integer, unable to parse string as an integer".to_string(),
                "int_parsing".to_string(),
            ))
        }
        _ => Err((
            "Input should be a valid integer".to_string(),
            "int_type".to_string(),
        )),
    }
}

fn coerce_bool(v: &Value) -> Result<bool, (String, String)> {
    const E: (String, String) = (String::new(), String::new());
    let _ = E;
    match v {
        Value::Bool(b) => Ok(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if i == 0 {
                    Ok(false)
                } else if i == 1 {
                    Ok(true)
                } else {
                    Err((
                        "Input should be a valid boolean".to_string(),
                        "bool_type".to_string(),
                    ))
                }
            } else if let Some(f) = n.as_f64() {
                if f == 0.0 {
                    Ok(false)
                } else if f == 1.0 {
                    Ok(true)
                } else {
                    Err((
                        "Input should be a valid boolean".to_string(),
                        "bool_type".to_string(),
                    ))
                }
            } else {
                Err((
                    "Input should be a valid boolean".to_string(),
                    "bool_type".to_string(),
                ))
            }
        }
        Value::String(s) => match s.trim().to_lowercase().as_str() {
            "true" | "1" | "yes" | "y" | "on" | "t" => Ok(true),
            "false" | "0" | "no" | "n" | "off" | "f" => Ok(false),
            _ => Err((
                "Input should be a valid boolean".to_string(),
                "bool_type".to_string(),
            )),
        },
        _ => Err((
            "Input should be a valid boolean".to_string(),
            "bool_type".to_string(),
        )),
    }
}

fn coerce_str(v: &Value) -> Result<String, (String, String)> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(true) => Ok("True".to_string()),
        Value::Bool(false) => Ok("False".to_string()),
        _ => Err((
            "Input should be a valid string".to_string(),
            "string_type".to_string(),
        )),
    }
}

fn opt_int(args: &Map<String, Value>, errs: &mut Vec<VErr>, name: &str) -> Option<Option<i64>> {
    match args.get(name) {
        None | Some(Value::Null) => Some(None),
        Some(v) => match coerce_int(v) {
            Ok(i) => Some(Some(i)),
            Err((msg, typ)) => {
                errs.push(VErr {
                    loc: name.to_string(),
                    msg,
                    typ,
                    iv: render_iv(v),
                    it: input_type_of(v).to_string(),
                });
                None
            }
        },
    }
}

fn opt_str(args: &Map<String, Value>, errs: &mut Vec<VErr>, name: &str) -> Option<Option<String>> {
    match args.get(name) {
        None | Some(Value::Null) => Some(None),
        Some(v) => match coerce_str(v) {
            Ok(s) => Some(Some(s)),
            Err((msg, typ)) => {
                errs.push(VErr {
                    loc: name.to_string(),
                    msg,
                    typ,
                    iv: render_iv(v),
                    it: input_type_of(v).to_string(),
                });
                None
            }
        },
    }
}

fn req_int(args: &Map<String, Value>, errs: &mut Vec<VErr>, name: &str, dflt: i64) -> Option<i64> {
    match args.get(name) {
        None => Some(dflt),
        Some(Value::Null) => {
            errs.push(VErr {
                loc: name.to_string(),
                msg: "Input should be a valid integer".to_string(),
                typ: "int_type".to_string(),
                iv: "None".to_string(),
                it: "NoneType".to_string(),
            });
            None
        }
        Some(v) => match coerce_int(v) {
            Ok(i) => Some(i),
            Err((msg, typ)) => {
                errs.push(VErr {
                    loc: name.to_string(),
                    msg,
                    typ,
                    iv: render_iv(v),
                    it: input_type_of(v).to_string(),
                });
                None
            }
        },
    }
}

fn req_str(
    args: &Map<String, Value>,
    errs: &mut Vec<VErr>,
    name: &str,
    dflt: &str,
) -> Option<String> {
    match args.get(name) {
        None => Some(dflt.to_string()),
        Some(Value::Null) => {
            errs.push(VErr {
                loc: name.to_string(),
                msg: "Input should be a valid string".to_string(),
                typ: "string_type".to_string(),
                iv: "None".to_string(),
                it: "NoneType".to_string(),
            });
            None
        }
        Some(v) => match coerce_str(v) {
            Ok(s) => Some(s),
            Err((msg, typ)) => {
                errs.push(VErr {
                    loc: name.to_string(),
                    msg,
                    typ,
                    iv: render_iv(v),
                    it: input_type_of(v).to_string(),
                });
                None
            }
        },
    }
}

fn req_bool(args: &Map<String, Value>, errs: &mut Vec<VErr>, name: &str) -> Option<bool> {
    match args.get(name) {
        None => Some(false),
        Some(Value::Null) => {
            errs.push(VErr {
                loc: name.to_string(),
                msg: "Input should be a valid boolean".to_string(),
                typ: "bool_type".to_string(),
                iv: "None".to_string(),
                it: "NoneType".to_string(),
            });
            None
        }
        Some(v) => match coerce_bool(v) {
            Ok(b) => Some(b),
            Err((msg, typ)) => {
                errs.push(VErr {
                    loc: name.to_string(),
                    msg,
                    typ,
                    iv: render_iv(v),
                    it: input_type_of(v).to_string(),
                });
                None
            }
        },
    }
}

fn check_no_extra(args: &Map<String, Value>, errs: &mut Vec<VErr>, known: &[&str]) {
    for (k, v) in args.iter() {
        if !known.contains(&k.as_str()) {
            errs.push(VErr {
                loc: k.clone(),
                msg: "Unexpected keyword argument".to_string(),
                typ: "unexpected_keyword_argument".to_string(),
                iv: render_iv(v),
                it: input_type_of(v).to_string(),
            });
        }
    }
}

fn missing_arg(_tool: &str, args: &Map<String, Value>, field: &str) -> VErr {
    VErr {
        loc: field.to_string(),
        msg: "Missing required argument".to_string(),
        typ: "missing_argument".to_string(),
        iv: py_repr_value(&Value::Object(args.clone())),
        it: "dict".to_string(),
    }
}

// ---------------------------------------------------------------- python-compatible JSON dumps

/// Float rendering matching Python repr() for the value ranges this server
/// emits (bounded magnitudes; never exponent-form, never non-finite).
fn py_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    let s = format!("{f}");
    if s.contains(['.', 'e', 'E']) {
        s
    } else {
        format!("{s}.0")
    }
}

/// json.dumps ensure_ascii string quoting (short escapes + \uXXXX, astral as
/// surrogate pairs, 0x7F escaped like the json module's [^\ -~] class).
fn py_json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '\u{08}' => o.push_str("\\b"),
            '\u{0C}' => o.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) == 0x7F => {
                o.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if (c as u32) < 0x7F => o.push(c),
            c => {
                let n = c as u32;
                if n <= 0xFFFF {
                    o.push_str(&format!("\\u{n:04x}"));
                } else {
                    let v = n - 0x10000;
                    o.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xD800 + (v >> 10),
                        0xDC00 + (v & 0x3FF)
                    ));
                }
            }
        }
    }
    o.push('"');
    o
}

fn py_dumps_sep(v: &Value, kv: &str, item: &str) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(true) => "true".to_string(),
        Value::Bool(false) => "false".to_string(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                py_float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(s) => py_json_str(s),
        Value::Array(a) => {
            let parts: Vec<String> = a.iter().map(|x| py_dumps_sep(x, kv, item)).collect();
            format!("[{}]", parts.join(item))
        }
        Value::Object(m) => {
            let parts: Vec<String> = m
                .iter()
                .map(|(k, x)| format!("{}{}{}", py_json_str(k), kv, py_dumps_sep(x, kv, item)))
                .collect();
            format!("{{{}}}", parts.join(item))
        }
    }
}

/// json.dumps default separators (", ", ": ") - tool strings, coordinate maps.
pub fn py_dumps(v: &Value) -> String {
    py_dumps_sep(v, ": ", ", ")
}

/// json.dumps compact separators (",", ":") - elements block only.
pub fn py_dumps_compact(v: &Value) -> String {
    py_dumps_sep(v, ":", ",")
}

/// Python repr() for error input_value rendering (single quotes, True/None).
pub fn py_repr_value(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                py_float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(s) => py_repr_str(s),
        Value::Array(a) => {
            let parts: Vec<String> = a.iter().map(py_repr_value).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(m) => {
            let parts: Vec<String> = m
                .iter()
                .map(|(k, x)| format!("{}: {}", py_repr_str(k), py_repr_value(x)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

// ---------------------------------------------------------------- JSON helpers

fn jstr(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

fn text_block(s: &str) -> String {
    format!("{{\"text\":{},\"type\":\"text\"}}", jstr(s))
}

/// Orderpending live probe of the python image block; current guess follows the
/// text-block payload-first pattern. Documented as an open question.
fn image_block(b64: &str, mime: &str) -> String {
    format!(
        "{{\"data\":{},\"mimeType\":{},\"type\":\"image\"}}",
        jstr(b64),
        jstr(mime)
    )
}

/// P2a envelope: `structuredContent` carries the real machine-readable value
/// parsed from the tool's JSON text (never a double-encoded string), while
/// the text block stays byte-identical for legacy clients. A top-level
/// array (only `list_windows`) is namespaced as `{"windows": [...]}`; any
/// other value passes through unchanged. `_meta.fastmcp` noise is gone.
///
/// No-fallback rule: only a JSON object or array payload yields a success
/// envelope. Anything else (unparseable text, a JSON string/number/bool)
/// is an internal error, never a `{"result": "<text>"}` string wrapper.
fn structured_of_text(s: &str) -> Option<Value> {
    match serde_json::from_str::<Value>(s) {
        Ok(Value::Array(a)) => {
            let mut m = Map::new();
            m.insert("windows".to_string(), Value::Array(a));
            Some(Value::Object(m))
        }
        Ok(v @ Value::Object(_)) => Some(v),
        _ => None,
    }
}

fn wrapped_text(s: &str) -> String {
    let Some(sc) = structured_of_text(s) else {
        return tool_err_blocks(&[text_block(
            "error: internal: tool payload is not a JSON object or array",
        )]);
    };
    // Tool-level failure is signalled by the payload itself: any object
    // with top-level `"ok": false` (window resolve/action errors, elements
    // resolve errors, cursor read errors) is `isError: true`. The `do`
    // burst payload has no top-level `ok` (per-action `ok` only), so its
    // continue-always contract stays `isError: false`.
    let is_err = sc.get("ok") == Some(&Value::Bool(false));
    format!(
        "{{\"content\":[{}],\"isError\":{},\"structuredContent\":{}}}",
        text_block(s),
        if is_err { "true" } else { "false" },
        serde_json::to_string(&sc).unwrap()
    )
}

fn err_result_text(s: &str) -> String {
    format!("{{\"content\":[{}],\"isError\":true}}", text_block(s))
}

/// P3a hidden-desktop capture: the `do` text envelope plus the hidden
/// PrintWindow PNG as a real MCP `image` content block alongside the JSON
/// text (not only base64 inside it). `structuredContent` and `isError`
/// follow the same rule as `wrapped_text`; the `do` payload has no
/// top-level `ok`, so `isError` stays false. Only the hidden path with a
/// successful capture uses this; every other `do` call keeps the exact
/// P2a text-only envelope.
pub(crate) fn wrapped_text_with_image(s: &str, png_b64: &str, mime: &str) -> String {
    let Some(sc) = structured_of_text(s) else {
        return tool_err_blocks(&[text_block(
            "error: internal: tool payload is not a JSON object or array",
        )]);
    };
    let is_err = sc.get("ok") == Some(&Value::Bool(false));
    format!(
        "{{\"content\":[{},{}],\"isError\":{},\"structuredContent\":{}}}",
        text_block(s),
        image_block(png_b64, mime),
        if is_err { "true" } else { "false" },
        serde_json::to_string(&sc).unwrap()
    )
}

/// P2a screenshot success: text/image blocks byte-identical, plus the
/// coordinate-map object as `structuredContent` (no base64 duplication;
/// the optional third elements block stays text-only).
pub fn screenshot_ok(blocks: &[String], cmap: &Value) -> String {
    format!(
        "{{\"content\":[{}],\"isError\":false,\"structuredContent\":{}}}",
        blocks.join(","),
        serde_json::to_string(cmap).unwrap()
    )
}

/// P2a tool-level failure with pre-rendered content blocks (screenshot
/// domain errors): text blocks byte-identical, `isError: true`.
pub fn tool_err_blocks(blocks: &[String]) -> String {
    format!("{{\"content\":[{}],\"isError\":true}}", blocks.join(","))
}

// ---------------------------------------------------------------- Win32 helpers

pub(crate) unsafe fn win_text(hwnd: HWND) -> String {
    let mut buf = vec![0u16; 512];
    let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

pub(crate) unsafe fn win_class(hwnd: HWND) -> String {
    let mut buf = vec![0u16; 256];
    let n = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

unsafe extern "system" fn enum_cb(hwnd: HWND, lp: LPARAM) -> BOOL {
    let v = &mut *(lp as *mut Vec<isize>);
    v.push(hwnd as isize);
    1
}

pub(crate) fn enum_windows() -> Vec<HWND> {
    let mut v: Vec<isize> = Vec::new();
    unsafe {
        EnumWindows(Some(enum_cb), &mut v as *mut Vec<isize> as LPARAM);
    }
    v.into_iter().map(|x| x as HWND).collect()
}

unsafe extern "system" fn child_cb(hwnd: HWND, lp: LPARAM) -> BOOL {
    let v = &mut *(lp as *mut Vec<isize>);
    v.push(hwnd as isize);
    1
}

fn enum_child_windows(hwnd: HWND) -> Result<Vec<HWND>, String> {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        FormatMessageW, FORMAT_MESSAGE_FROM_SYSTEM,
    };
    let mut v: Vec<isize> = Vec::new();
    unsafe {
        // Measured: EnumChildWindows returns FALSE with last-error 0 for a
        // valid window with no children, and pywin32 does not raise there
        // (python falls into the empty-list branch). Only a nonzero
        // last-error is a real failure.
        SetLastError(0);
        let ok = EnumChildWindows(hwnd, Some(child_cb), &mut v as *mut Vec<isize> as LPARAM);
        if ok == 0 {
            let code = GetLastError();
            if code != 0 {
                // pywin32 raises pywintypes.error (code, funcname, message);
                // str(e) renders as the tuple below (reading: pywin32
                // error-tuple str convention; only reachable when UIA also
                // failed on a live hwnd, i.e. effectively never).
                let mut buf = vec![0u16; 256];
                let n = FormatMessageW(
                    FORMAT_MESSAGE_FROM_SYSTEM,
                    std::ptr::null(),
                    code,
                    0,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    std::ptr::null(),
                );
                let msg = if n > 0 {
                    String::from_utf16_lossy(&buf[..n as usize])
                        .trim()
                        .to_string()
                } else {
                    "Unknown error".to_string()
                };
                return Err(format!(
                    "EnumChildWindows failed: ({code}, 'EnumChildWindows', '{msg}')"
                ));
            }
        }
        Ok(v.into_iter().map(|x| x as HWND).collect())
    }
}

unsafe extern "system" fn mon_cb(_hm: HMONITOR, _hdc: HDC, rc: *mut RECT, lp: LPARAM) -> BOOL {
    let v = &mut *(lp as *mut Vec<[i32; 4]>);
    let r = &*rc;
    v.push([r.left, r.top, r.right, r.bottom]);
    1
}

unsafe extern "system" fn mon_cb_h(hm: HMONITOR, _hdc: HDC, rc: *mut RECT, lp: LPARAM) -> BOOL {
    let v = &mut *(lp as *mut Vec<([i32; 4], HMONITOR)>);
    let r = &*rc;
    v.push(([r.left, r.top, r.right, r.bottom], hm));
    1
}

fn display_monitors() -> Vec<[i32; 4]> {
    let mut v: Vec<[i32; 4]> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(mon_cb),
            &mut v as *mut Vec<[i32; 4]> as LPARAM,
        );
    }
    v
}

/// Same enumeration as `display_monitors`, paired with each monitor's
/// `HMONITOR` handle. The existing signature stays (callers index rects);
/// DXGI output matching uses this sibling.
fn display_monitor_handles() -> Vec<([i32; 4], HMONITOR)> {
    let mut v: Vec<([i32; 4], HMONITOR)> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(mon_cb_h),
            &mut v as *mut Vec<([i32; 4], HMONITOR)> as LPARAM,
        );
    }
    v
}

/// `\\.\DISPLAYn` device name for a monitor handle, via `GetMonitorInfoW`.
/// Empty when unreadable; callers treat that as "no device match".
fn monitor_device(hmon: HMONITOR) -> String {
    unsafe {
        let mut info: MONITORINFOEXW = std::mem::zeroed();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(hmon, &mut info as *mut _ as *mut MONITORINFO) == 0 {
            return String::new();
        }
        let raw = info.szDevice;
        let n = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        String::from_utf16_lossy(&raw[..n])
    }
}

fn virtual_rect() -> (i32, i32, i32, i32) {
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        (vx, vy, vx + vw, vy + vh)
    }
}

pub(crate) fn exe_of_pid(pid: u32) -> String {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return "unknown".to_string();
        }
        let mut size: u32 = 512;
        let mut buf = vec![0u16; 512];
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut size);
        CloseHandle(h);
        if r != 0 {
            let path = String::from_utf16_lossy(&buf[..size as usize]);
            let base = path
                .replace('/', "\\")
                .rsplit('\\')
                .next()
                .unwrap_or("")
                .to_string();
            if base.is_empty() {
                "unknown".to_string()
            } else {
                base
            }
        } else {
            "unknown".to_string()
        }
    }
}

/// Process creation time (FILETIME as u64: 100 ns ticks since 1601) for a
/// PID, or `None` when the process cannot be opened. PIDs are recycled by
/// Windows, so this is what tells two processes with the same PID apart.
/// `GetProcessTimes` is the documented source; the two FILETIME halves are
/// joined into one comparable u64.
pub(crate) fn process_created(pid: u32) -> Option<u64> {
    if pid == 0 {
        return None;
    }
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut creation: FILETIME = std::mem::zeroed();
        let mut exit: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        let ok = GetProcessTimes(h, &mut creation, &mut exit, &mut kernel, &mut user);
        CloseHandle(h);
        if ok == 0 {
            return None;
        }
        Some(((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64)
    }
}

/// Desktop name a window lives on, read from the desktop of the window's
/// thread (`GetThreadDesktop` + `GetUserObjectInformationW(UOI_NAME)`).
///
/// Handle values are per-process, so a raw `HDESK` comparison across
/// processes is meaningless; the NAME is the comparable identity. Used to
/// decide whether a target really lives on the private desktop this call
/// created (see `burst::private_desktop_earned`).
pub(crate) fn window_desktop_name(hwnd: HWND) -> Option<String> {
    if hwnd.is_null() {
        return None;
    }
    unsafe {
        let tid = GetWindowThreadProcessId(hwnd, std::ptr::null_mut());
        if tid == 0 {
            return None;
        }
        let ht = OpenThread(THREAD_QUERY_INFORMATION, 0, tid);
        if ht.is_null() {
            return None;
        }
        let hdesk = GetThreadDesktop(tid);
        CloseHandle(ht);
        if hdesk.is_null() {
            return None;
        }
        let mut buf = vec![0u16; 256];
        let mut needed: u32 = 0;
        let ok = GetUserObjectInformationW(
            hdesk as *mut _,
            UOI_NAME,
            buf.as_mut_ptr() as *mut _,
            (buf.len() * 2) as u32,
            &mut needed,
        );
        if ok == 0 {
            return None;
        }
        let n = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..n]))
    }
}

pub(crate) fn pid_of(hwnd: HWND) -> u32 {
    unsafe {
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        pid
    }
}

fn dwm_rect(hwnd: HWND) -> (Vec<i32>, bool) {
    unsafe {
        let mut rc = RECT::default();
        let ok = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            &mut rc as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        );
        if ok == 0 && rc.right > rc.left && rc.bottom > rc.top {
            return (vec![rc.left, rc.top, rc.right, rc.bottom], false);
        }
    }
    unsafe {
        let mut rc = RECT::default();
        if GetWindowRect(hwnd, &mut rc) != 0 {
            (vec![rc.left, rc.top, rc.right, rc.bottom], true)
        } else {
            (vec![0, 0, 0, 0], true)
        }
    }
}

fn cloaked(hwnd: HWND) -> i64 {
    unsafe {
        let mut v: u32 = 0;
        let ok = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            &mut v as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        );
        if ok == 0 {
            v as i64
        } else {
            0
        }
    }
}

fn win_dpi(hwnd: HWND) -> i64 {
    unsafe {
        // GetDpiForWindow returns 0 on failure per contract (python try/except).
        GetDpiForWindow(hwnd) as i64
    }
}

// ---------------------------------------------------------------- elevation / UIPI (gap 2)

/// Elevation of one open token: Some(true) elevated, Some(false) not.
/// The caller owns `tok` (opened via OpenProcessToken); it is not closed here.
fn token_elevated(tok: HANDLE) -> Option<bool> {
    unsafe {
        let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len: u32 = 0;
        let ok = GetTokenInformation(
            tok,
            TokenElevation,
            &mut elev as *mut TOKEN_ELEVATION as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        if ok != 0 {
            Some(elev.TokenIsElevated != 0)
        } else {
            None
        }
    }
}

/// Elevation of a process id via its token (gap 2): Some(true/false) when
/// readable, None when not (fail-open: unknown is never NAMED elevated).
/// Openable with PROCESS_QUERY_LIMITED_INFORMATION, the same handle level
/// `exe_of_pid` already uses.
pub(crate) fn process_elevated(pid: u32) -> Option<bool> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut tok: HANDLE = std::ptr::null_mut();
        let v = if OpenProcessToken(h, TOKEN_QUERY, &mut tok) == 0 {
            None
        } else {
            let v = token_elevated(tok);
            CloseHandle(tok);
            v
        };
        CloseHandle(h);
        v
    }
}

/// Elevation of this server process (gap 2). Unknown reads as false so the
/// predicate below fails closed (refuse) rather than injecting blind.
pub(crate) fn self_elevated() -> bool {
    unsafe {
        let mut tok: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
            return false;
        }
        let v = token_elevated(tok).unwrap_or(false);
        CloseHandle(tok);
        v
    }
}

/// Pure UIPI predicate (gap 2): input from this process to the target is
/// UIPI-doomed exactly when the target is determinably elevated and we are
/// not. `SendInput` "neither GetLastError nor the return value will indicate
/// the failure was caused by UIPI blocking", so this pre-check is the only
/// signal that names the cause.
pub(crate) fn uipi_blocked(target_elevated: Option<bool>, self_elev: bool) -> bool {
    matches!(target_elevated, Some(true)) && !self_elev
}

/// Elevation of the process owning a window (gap 2).
pub(crate) fn window_elevated(hwnd: HWND) -> Option<bool> {
    let pid = pid_of(hwnd);
    if pid == 0 {
        return None;
    }
    process_elevated(pid)
}

fn client_rect_screen(hwnd: HWND) -> Vec<i32> {
    use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
    unsafe {
        let mut rc = RECT::default();
        if GetClientRect(hwnd, &mut rc) != 0 {
            let mut p0 = POINT { x: 0, y: 0 };
            let mut p1 = POINT {
                x: rc.right,
                y: rc.bottom,
            };
            if ClientToScreen(hwnd, &mut p0) != 0 && ClientToScreen(hwnd, &mut p1) != 0 {
                return vec![p0.x, p0.y, p1.x, p1.y];
            }
        }
        vec![0, 0, 0, 0]
    }
}

fn is_maximized(hwnd: HWND) -> bool {
    unsafe {
        let mut wp = WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        if GetWindowPlacement(hwnd, &mut wp) != 0 {
            wp.showCmd == SW_SHOWMAXIMIZED as u32
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------- window model

fn window_info(hwnd: HWND) -> Option<Map<String, Value>> {
    unsafe {
        if IsWindowVisible(hwnd) == 0 {
            return None;
        }
        let title = win_text(hwnd);
        let class = win_class(hwnd);
        let mut rc = RECT::default();
        if GetWindowRect(hwnd, &mut rc) == 0 {
            return None;
        }
        let pid = pid_of(hwnd);
        let (dwm, dwm_fb) = dwm_rect(hwnd);
        let mut m = Map::new();
        m.insert("hwnd".to_string(), Value::from(hwnd as i64));
        m.insert("pid".to_string(), Value::from(pid as i64));
        m.insert("exe".to_string(), Value::from(exe_of_pid(pid)));
        m.insert("title".to_string(), Value::from(title));
        m.insert("class".to_string(), Value::from(class));
        m.insert(
            "rect".to_string(),
            Value::from(vec![
                rc.left as i64,
                rc.top as i64,
                rc.right as i64,
                rc.bottom as i64,
            ]),
        );
        m.insert(
            "dwm_rect".to_string(),
            Value::from(
                dwm.iter()
                    .map(|v| Value::from(*v as i64))
                    .collect::<Vec<_>>(),
            ),
        );
        m.insert(
            "client_rect".to_string(),
            Value::from(
                client_rect_screen(hwnd)
                    .iter()
                    .map(|v| Value::from(*v as i64))
                    .collect::<Vec<_>>(),
            ),
        );
        m.insert("minimized".to_string(), Value::from(IsIconic(hwnd) != 0));
        m.insert("maximized".to_string(), Value::from(is_maximized(hwnd)));
        m.insert(
            "foreground".to_string(),
            Value::from(GetForegroundWindow() == hwnd),
        );
        m.insert("cloaked".to_string(), Value::from(cloaked(hwnd)));
        m.insert("visible".to_string(), Value::from(true));
        m.insert("dpi".to_string(), Value::from(win_dpi(hwnd)));
        // Gap 2: elevation names the UIPI condition. true = elevated target
        // (input from this server is UIPI-blocked), false = not, null =
        // unreadable (fail-open: unknown is never named elevated).
        m.insert(
            "elevated".to_string(),
            match window_elevated(hwnd) {
                Some(b) => Value::from(b),
                None => Value::Null,
            },
        );
        if dwm_fb {
            m.insert("dwm_fallback".to_string(), Value::from(true));
        }
        Some(m)
    }
}

struct Cand {
    hwnd: HWND,
    pid: u32,
    exe: String,
    title: String,
    class: String,
    foreground: bool,
}

fn candidates() -> Vec<Cand> {
    unsafe {
        let fg = GetForegroundWindow();
        let mut rows: Vec<Cand> = Vec::new();
        for h in enum_windows() {
            if IsWindowVisible(h) == 0 {
                continue;
            }
            let pid = pid_of(h);
            rows.push(Cand {
                hwnd: h,
                pid,
                exe: exe_of_pid(pid),
                title: win_text(h),
                class: win_class(h),
                foreground: h == fg,
            });
        }
        // Foreground-most first, stable (enum order already z-order).
        rows.sort_by_key(|c| !c.foreground);
        rows
    }
}

fn cand_public(c: &Cand) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("hwnd".to_string(), Value::from(c.hwnd as i64));
    m.insert("pid".to_string(), Value::from(c.pid as i64));
    m.insert("exe".to_string(), Value::from(c.exe.clone()));
    m.insert("title".to_string(), Value::from(c.title.clone()));
    m.insert("foreground".to_string(), Value::from(c.foreground));
    m
}

/// windows-core-compatible Win32/HRESULT error text.
///
/// Reading: `windows::core::Error`'s Display is `"{message} ({code:#010X})"`
/// (windows-result 0.2.0 `error.rs:227`, `hresult.rs:65-112,145-147`): the
/// message is FormatMessageW(FROM_SYSTEM|IGNORE_INSERTS) on the HRESULT value,
/// wide-trimmed; BOOL-call failures map via HRESULT::from_win32. This helper
/// makes the identical OS call with the identical input, so the text matches
/// by construction. Only reachable on failing OS calls (never on the parity
/// harness's live fixtures).
pub(crate) fn hr_text(hr: i32) -> String {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        FormatMessageW, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS,
    };
    unsafe {
        let mut buf = vec![0u16; 512];
        let n = FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            std::ptr::null(),
            hr as u32,
            0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            std::ptr::null(),
        );
        let msg = if n > 0 {
            String::from_utf16_lossy(&buf[..n as usize])
        } else {
            String::new()
        };
        let msg = msg.trim_end_matches(|c: char| c.is_whitespace() || c == '\0');
        format!("{msg} ({hr:#010X})")
    }
}

pub(crate) fn win32_text(code: u32) -> String {
    // HRESULT::from_win32 (windows-result 0.2.0 hresult.rs:117-124).
    let hr = if code as i32 <= 0 {
        code as i32
    } else {
        ((code & 0x0000_FFFF) | (7 << 16) | 0x8000_0000) as i32
    };
    hr_text(hr)
}

pub(crate) fn exe_stem(name: &str) -> String {
    let n = name.to_lowercase();
    if n.ends_with(".exe") {
        n[..n.len() - 4].to_string()
    } else {
        n
    }
}

pub(crate) fn parse_int_loose(val: &str) -> Option<i64> {
    let t = val.trim().replace('_', "");
    let t = t.as_str();
    let body = t.strip_prefix('+').unwrap_or(t);
    let neg = t.starts_with('-');
    let digits = if neg { &body[1..] } else { body };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut v: i64 = 0;
    for b in digits.bytes() {
        v = v.checked_mul(10)?.checked_add((b - b'0') as i64)?;
    }
    Some(if neg { -v } else { v })
}

pub(crate) fn parse_selector(sel: &str) -> (String, String) {
    let s = sel.trim().to_string();
    let low = s.to_lowercase();
    for prefix in ["hwnd:", "pid:", "exe:", "title:", "class:"] {
        if low.starts_with(prefix) {
            return (
                prefix[..prefix.len() - 1].to_string(),
                s[prefix.len()..].trim().to_string(),
            );
        }
    }
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        return ("hwnd".to_string(), s);
    }
    ("auto".to_string(), s)
}

pub mod burst;
pub mod cancel;
pub mod cap;
pub mod dxgi;
#[cfg(test)]
pub mod fixture;
pub mod gate;
pub mod hidden;
pub mod screen;
pub mod serve;
pub mod shot;
pub mod target;
pub mod uia;
pub mod winop;

// ---------------------------------------------------------------- provenance

/// Commit this binary was compiled from (`build.rs`), `unknown` when git was
/// unavailable at build time.
const GIT_SHA: &str = env!("PUPPET_GIT_SHA");
/// `-dirty` when the tree was modified at build time, else empty.
const GIT_DIRTY: &str = env!("PUPPET_GIT_DIRTY");

fn main() {
    // Provenance answers without touching the desktop: this prints and returns
    // before any Win32 call or the stdio pump. MCP clients pass no args, so
    // the server path below is unchanged.
    if std::env::args_os()
        .skip(1)
        .any(|a| a == "--version" || a == "-V")
    {
        println!(
            "puppet {} ({GIT_SHA}{GIT_DIRTY})",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE);
        }
    }
    serve::run();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn region_validator_table() {
        // Order: inverted or empty edges reject without any subtraction.
        assert!(matches!(
            validate_region(5000, 0, 0, 100, 8192),
            Err(RegionReject::Order)
        ));
        assert!(matches!(
            validate_region(0, 100, 100, 0, 8192),
            Err(RegionReject::Order)
        ));
        assert!(matches!(
            validate_region(0, 0, 0, 100, 8192),
            Err(RegionReject::Order)
        ));
        // Extremes: MIN/MAX (and any out-of-i32 coord) reject before cast.
        assert!(matches!(
            validate_region(i64::MIN, 0, i64::MAX, 100, 8192),
            Err(RegionReject::Extreme)
        ));
        assert!(matches!(
            validate_region(i64::MIN, 0, 5000, 1, 4096),
            Err(RegionReject::Extreme)
        ));
        assert!(matches!(
            validate_region(0, 0, i64::MAX, 100, 8192),
            Err(RegionReject::Extreme)
        ));
        // Over-cap: width and height each reject with their sizes.
        match validate_region(0, 0, 9000, 100, 8192) {
            Err(RegionReject::OverCap(w, h)) => assert_eq!((w, h), (9000, 100)),
            other => panic!("width>cap must reject OverCap, got: {other:?}"),
        }
        match validate_region(0, 0, 100, 5000, 4096) {
            Err(RegionReject::OverCap(w, h)) => assert_eq!((w, h), (100, 5000)),
            other => panic!("height>cap must reject OverCap, got: {other:?}"),
        }
        // Valid edge: exactly the cap passes.
        assert_eq!(
            validate_region(0, 0, 8192, 100, 8192).expect("edge==cap"),
            (8192, 100)
        );
        assert_eq!(
            validate_region(0, 0, 4096, 4096, 4096).expect("edge==cap"),
            (4096, 4096)
        );
    }

    #[test]
    fn overflow_string_rejected_with_shared_error() {
        let v = json!("9223372036854775808"); // i64::MAX + 1
        assert!(is_int_overflow(&v));
        let (msg, _) = coerce_int(&v).expect_err("MAX+1 must be rejected");
        assert_eq!(msg, int_overflow_err(&v));
    }

    #[test]
    fn overflow_u64_rejected_with_shared_error() {
        let v = Value::from(u64::MAX);
        assert!(is_int_overflow(&v));
        let (msg, _) = coerce_int(&v).expect_err("u64::MAX must be rejected");
        assert_eq!(msg, int_overflow_err(&v));
    }

    #[test]
    fn overflow_float_rejected_with_shared_error() {
        let v = json!(1e19);
        assert!(is_int_overflow(&v));
        let (msg, _) = coerce_int(&v).expect_err("1e19 must be rejected");
        assert_eq!(msg, int_overflow_err(&v));
    }

    #[test]
    fn in_range_values_still_accepted() {
        assert_eq!(coerce_int(&json!("9223372036854775807")).unwrap(), i64::MAX);
        assert_eq!(
            coerce_int(&json!("-9223372036854775808")).unwrap(),
            i64::MIN
        );
        assert_eq!(coerce_int(&json!(42)).unwrap(), 42);
        assert!(!is_int_overflow(&json!(42)));
        assert!(!is_int_overflow(&json!("-9223372036854775808")));
    }

    /// No-fallback rule: a non-JSON payload yields no structured value, and
    /// the wrapped path is an `isError: true` envelope — never a success
    /// envelope with a `{"result": "<text>"}` string payload.
    #[test]
    fn non_json_payload_is_error_never_string_result() {
        assert!(structured_of_text("not json").is_none());
        assert!(structured_of_text("\"just a string\"").is_none());
        let out = wrapped_text("not json");
        let v: Value = serde_json::from_str(&out).expect("outer frame");
        assert_eq!(v.get("isError"), Some(&Value::from(true)));
        assert!(
            v.get("structuredContent").is_none(),
            "error envelope carries no string-result structuredContent, got: {v}"
        );
    }
}
