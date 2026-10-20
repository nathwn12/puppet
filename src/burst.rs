//! `do` burst tool: queued input actions in one round-trip.
//!
//! Wire- and behavior-parity with `puppet.py:do` (2614-3418). Real-input
//! paths (SendInput, focus changes, pixel-click fallback) are implemented
//! for live use and marked `unconfirmed: real-input test needs owner
//! permission` - never exercised by the headless parity harness.

use crate::cap::{bitblt_grab, resolve_selector};
use crate::shot::element_model;
use crate::winop::focus_hwmd;
use crate::{
    enum_child_windows, py_repr_str, py_repr_value, py_round_half_even, py_round_n, virtual_rect,
    wrapped_text,
};
use base64::Engine as _;
use serde_json::{Map, Value};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{GetLastError, GlobalFree, HGLOBAL, HWND, LPARAM, POINT};
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// ---------------------------------------------------------------- constants (puppet.py:1512-1518, 1628-1656)

const M_MOVE: u32 = 0x0001;
const M_ABS: u32 = 0x8000;
const M_VDESK: u32 = 0x4000;
const M_LDOWN: u32 = 0x0002;
const M_LUP: u32 = 0x0004;
const M_RDOWN: u32 = 0x0008;
const M_RUP: u32 = 0x0010;
const M_MDOWN: u32 = 0x0020;
const M_MUP: u32 = 0x0040;
const M_WHEEL: u32 = 0x0800;
const K_UP: u32 = 0x0002;
const K_UNICODE: u32 = 0x0004;

const BG_NOTE: &str = "background PostMessage delivery is best-effort: it is NOT honoured by every app and is NOT equivalent to real (SendInput) input.";
const BG_SUFFIX: &str = "; omit background for real SendInput delivery";
const MAX_CLICK_COUNT: i64 = 100;
const MAX_STABLE_EDGE: i64 = 16384;

fn bg_non_postable(typ: &str) -> Option<&'static str> {
    match typ {
        "move" => Some("move has no window-message equivalent (it drives the system cursor, not a window)"),
        "drag" => Some("drag has no window-message equivalent (it drives the system cursor across the desktop)"),
        "scroll" => Some("scroll has no window-message equivalent (wheel input needs a real cursor/focus path)"),
        _ => None,
    }
}

// ---------------------------------------------------------------- small value helpers (mirror CPython str/int/repr)

fn py_type_name(v: &Value) -> &'static str {
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

/// Python str(x): None->None, True/False, numbers, strings raw, containers repr.
fn py_str(v: &Value) -> String {
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
                n.to_string()
            }
        }
        Value::String(s) => s.clone(),
        Value::Array(_) | Value::Object(_) => py_repr_value(v),
    }
}

fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i != 0
            } else if let Some(u) = n.as_u64() {
                u != 0
            } else {
                n.as_f64().map(|f| f != 0.0).unwrap_or(false)
            }
        }
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

/// CPython int(x) with CPython-shaped error texts (puppet.py int(...) sites).
/// Overflow policy (F4): REJECT via the shared `crate::is_int_overflow`
/// gate with the shared `crate::int_overflow_err` message — never clamp.
fn py_int(v: &Value) -> Result<i64, String> {
    if crate::is_int_overflow(v) {
        return Err(crate::int_overflow_err(v));
    }
    match v {
        Value::Bool(b) => Ok(if *b { 1 } else { 0 }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i)
            } else if let Some(u) = n.as_u64() {
                Ok(u as i64)
            } else if let Some(f) = n.as_f64() {
                if f.is_nan() {
                    Err("cannot convert float NaN to integer".to_string())
                } else if f.is_infinite() {
                    Err("cannot convert float infinity to integer".to_string())
                } else {
                    Ok(f.trunc() as i64)
                }
            } else {
                Err("int() argument must be a real number".to_string())
            }
        }
        Value::String(s) => {
            let t = s.trim();
            let (neg, body) = match t.strip_prefix(['+', '-']) {
                Some(b) => (t.starts_with('-'), b),
                None => (false, t),
            };
            let digits = body.replace('_', "");
            let underscores_ok = !body.is_empty()
                && !body.starts_with('_')
                && !body.ends_with('_')
                && !body.contains("__");
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && underscores_ok {
                let mut val: i128 = 0;
                for b in digits.bytes() {
                    val = val * 10 + (b - b'0') as i128;
                }
                if neg {
                    val = -val;
                }
                // Overflow already rejected by the shared gate above; the
                // try_from is a no-clamp backstop with the same message.
                Ok(i64::try_from(val).map_err(|_| crate::int_overflow_err(v))?)
            } else {
                Err(format!(
                    "invalid literal for int() with base 10: {}",
                    py_repr_str(s)
                ))
            }
        }
        Value::Null => Err(
            "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"
                .to_string(),
        ),
        Value::Array(_) => Err(format!(
            "int() argument must be a string, a bytes-like object or a real number, not '{}'",
            py_type_name(v)
        )),
        Value::Object(_) => Err(format!(
            "int() argument must be a string, a bytes-like object or a real number, not '{}'",
            py_type_name(v)
        )),
    }
}

fn opt_py_int(a: &Map<String, Value>, key: &str, dflt: i64) -> Result<i64, String> {
    match a.get(key) {
        None => Ok(dflt),
        Some(v) => py_int(v),
    }
}

fn elapsed_ms(t: &Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

// ---------------------------------------------------------------- foreground input builders

fn mi_move(x: i64, y: i64) -> INPUT {
    let (vx, vy, vr, vb) = virtual_rect();
    let (vx, vy, vr, vb) = (vx as f64, vy as f64, vr as f64, vb as f64);
    let vw = vr - vx;
    let vh = vb - vy;
    let ax = py_round_half_even((x as f64 - vx) * 65535.0 / vw.max(vw - 1.0).max(1.0)) as i64;
    let ay = py_round_half_even((y as f64 - vy) * 65535.0 / vh.max(vh - 1.0).max(1.0)) as i64;
    let ax = ax.clamp(0, 65535) as i32;
    let ay = ay.clamp(0, 65535) as i32;
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: ax,
                dy: ay,
                mouseData: 0,
                dwFlags: M_MOVE | M_ABS | M_VDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mi_btn(flag: u32, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: data,
                dwFlags: flag,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn ki(vk: u16, scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn btn_pair(raw: Option<&Value>) -> Result<(u32, u32), String> {
    let btn = match raw {
        None | Some(Value::Null) => "left".to_string(),
        Some(Value::String(s)) => s.to_lowercase(),
        Some(v) => py_str(v).to_lowercase(),
    };
    match btn.as_str() {
        "left" => Ok((M_LDOWN, M_LUP)),
        "right" => Ok((M_RDOWN, M_RUP)),
        "middle" => Ok((M_MDOWN, M_MUP)),
        _ => {
            let disp = match raw {
                None => "None".to_string(),
                Some(v) => py_str(v),
            };
            Err(format!(
                "unknown button \"{disp}\" (want left|right|middle)"
            ))
        }
    }
}

fn require_on_desktop(x: i64, y: i64) -> Result<(), String> {
    let (vx, vy, vr, vb) = virtual_rect();
    let (vx, vy, vr, vb) = (vx as i64, vy as i64, vr as i64, vb as i64);
    if vx <= x && x < vr && vy <= y && y < vb {
        Ok(())
    } else {
        Err(format!(
            "coords ({x},{y}) outside virtual desktop [{vx},{vy},{vr},{vb}] (refused, not clamped)"
        ))
    }
}

fn key_err(key: &str) -> String {
    format!("'{key}'")
}

fn utf16_units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

// ---------------------------------------------------------------- _vk (puppet.py:1571-1623)

fn vk_of(name: &str) -> Result<u16, String> {
    let key = name.to_lowercase();
    let named: Option<u16> = match key.as_str() {
        "enter" | "return" => Some(0x0D),
        "tab" => Some(0x09),
        "esc" | "escape" => Some(0x1B),
        "space" => Some(0x20),
        "backspace" | "back" => Some(0x08),
        "delete" | "del" => Some(0x2E),
        "insert" | "ins" => Some(0x2D),
        "home" => Some(0x24),
        "end" => Some(0x23),
        "pageup" | "pgup" => Some(0x21),
        "pagedown" | "pgdn" => Some(0x22),
        "up" => Some(0x26),
        "down" => Some(0x28),
        "left" => Some(0x25),
        "right" => Some(0x27),
        "printscreen" | "prtsc" => Some(0x2C),
        "pause" => Some(0x13),
        "break" => Some(0x03),
        "capslock" | "caps" => Some(0x14),
        "numlock" => Some(0x90),
        "scrolllock" => Some(0x91),
        "shift" => Some(0x10),
        "ctrl" | "control" => Some(0x11),
        "alt" | "menu" => Some(0x12),
        "lwin" => Some(0x5B),
        "rwin" => Some(0x5C),
        "win" => Some(0x5B),
        "apps" => Some(0x5D),
        _ => None,
    };
    if let Some(v) = named {
        return Ok(v);
    }
    if key.starts_with('f') {
        if let Ok(i) = key[1..].parse::<u32>() {
            if (1..=24).contains(&i) {
                return Ok((0x6F + i) as u16);
            }
        }
    }
    if name.chars().count() == 1 {
        let ch = name.chars().next().unwrap() as u32;
        unsafe {
            let sc = VkKeyScanW(ch as u16);
            if sc != -1 {
                return Ok((sc as u16) & 0xFF);
            }
        }
    }
    Err(format!("unknown key name \"{name}\""))
}

// ---------------------------------------------------------------- clipboard (puppet.py:2547-2602)

fn clipboard_set(text: &str) -> Result<(), String> {
    unsafe {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = wide.len() * 2;
        let mut opened = false;
        let mut hmem: HGLOBAL = std::ptr::null_mut();
        let mut locked: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut owned = false;
        let r = (|| -> Result<(), String> {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return Err("OpenClipboard failed".to_string());
            }
            opened = true;
            if EmptyClipboard() == 0 {
                return Err("EmptyClipboard failed".to_string());
            }
            hmem = GlobalAlloc(2, bytes);
            if hmem.is_null() {
                return Err("GlobalAlloc failed".to_string());
            }
            locked = GlobalLock(hmem);
            if locked.is_null() {
                return Err("GlobalLock failed".to_string());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), locked as *mut u16, wide.len());
            GlobalUnlock(hmem);
            locked = std::ptr::null_mut();
            if SetClipboardData(13, hmem).is_null() {
                Err("SetClipboardData failed".to_string())
            } else {
                owned = true;
                Ok(())
            }
        })();
        if !locked.is_null() {
            GlobalUnlock(hmem);
        }
        if !owned && !hmem.is_null() {
            GlobalFree(hmem);
        }
        if opened {
            CloseClipboard();
        }
        r
    }
}

fn clipboard_get() -> Result<String, String> {
    unsafe {
        let mut opened = false;
        let r = (|| -> Result<String, String> {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return Err("OpenClipboard failed".to_string());
            }
            opened = true;
            if IsClipboardFormatAvailable(13) == 0 {
                return Err("no CF_UNICODETEXT on clipboard".to_string());
            }
            let h = GetClipboardData(13);
            if h.is_null() {
                return Err("GetClipboardData failed".to_string());
            }
            let p = GlobalLock(h);
            if p.is_null() {
                return Err("GlobalLock failed".to_string());
            }
            let wp = p as *const u16;
            let mut len = 0usize;
            while *wp.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(wp, len);
            let s = String::from_utf16_lossy(slice);
            GlobalUnlock(h);
            Ok(s)
        })();
        if opened {
            CloseClipboard();
        }
        r
    }
}

// ---------------------------------------------------------------- background PostMessage input (puppet.py:1659-1792)

fn bg_lparam(x: i64, y: i64) -> LPARAM {
    (((y as u32 & 0xFFFF) << 16) | (x as u32 & 0xFFFF)) as i32 as isize
}

fn bg_client(hwnd: i64, x: &Value, y: &Value) -> Result<(i64, i64), String> {
    let xi = py_int(x).map_err(|e| format!("ScreenToClient failed: {e}"))?;
    let yi = py_int(y).map_err(|e| format!("ScreenToClient failed: {e}"))?;
    unsafe {
        let mut pt = POINT {
            x: xi as i32,
            y: yi as i32,
        };
        if ScreenToClient(hwnd as HWND, &mut pt) != 0 {
            Ok((pt.x as i64, pt.y as i64))
        } else {
            Err("ScreenToClient failed: ScreenToClient returned FALSE".to_string())
        }
    }
}

fn bg_key_lparam(vk: u16, keyup: bool) -> LPARAM {
    unsafe {
        let sc = MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) & 0xFF;
        let mut lp: u32 = 1 | (sc << 16);
        if keyup {
            lp |= (1 << 30) | (1 << 31);
        }
        lp as i32 as isize
    }
}

fn bg_post(hwnd: i64, msg: u32, w: u64, l: LPARAM) -> bool {
    unsafe { PostMessageW(hwnd as HWND, msg, w as usize, l) != 0 }
}

fn bg_buttons(btn: &str) -> Option<(u32, u32, u64)> {
    match btn {
        "left" => Some((0x0201, 0x0202, 0x0001)),
        "right" => Some((0x0204, 0x0205, 0x0002)),
        "middle" => Some((0x0207, 0x0208, 0x0010)),
        _ => None,
    }
}

fn bg_btn_name(a: &Map<String, Value>) -> String {
    match a.get("button") {
        None => "left".to_string(),
        Some(v) => py_str(v).to_lowercase(),
    }
}

fn bg_btn_raw(a: &Map<String, Value>) -> String {
    match a.get("button") {
        None => "None".to_string(),
        Some(v) => py_str(v),
    }
}

/// Returns (ok, error, posted_count). Never activates/focuses/raises.
fn bg_execute(hwnd: i64, a: &Map<String, Value>) -> (bool, Option<String>, i64) {
    let typ = a.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if typ == "click" {
        let btn = bg_btn_name(a);
        let Some((dn, up, held)) = bg_buttons(&btn) else {
            return (
                false,
                Some(format!("unknown button \"{}\"", bg_btn_raw(a))),
                0,
            );
        };
        if !a.contains_key("x") || !a.contains_key("y") {
            return (false, Some("click needs x and y".to_string()), 0);
        }
        let count = match opt_py_int(a, "count", 1) {
            Ok(c) => c.max(1),
            Err(_) => return (false, Some("click count must be an int".to_string()), 0),
        };
        if count > MAX_CLICK_COUNT {
            return (
                false,
                Some(format!("click count absurd (>{MAX_CLICK_COUNT}): {count}")),
                0,
            );
        }
        let (cx, cy) = match bg_client(hwnd, &a["x"], &a["y"]) {
            Ok(p) => p,
            Err(e) => return (false, Some(e), 0),
        };
        let lp = bg_lparam(cx, cy);
        if !bg_post(hwnd, 0x0200, 0, lp) {
            return (
                false,
                Some("PostMessage failed (target may have closed)".to_string()),
                0,
            );
        }
        let mut n = 1i64;
        for _ in 0..count {
            if !bg_post(hwnd, dn, held, lp) {
                return (
                    false,
                    Some("PostMessage failed (target may have closed)".to_string()),
                    n,
                );
            }
            n += 1;
            if !bg_post(hwnd, up, 0, lp) {
                return (
                    false,
                    Some("PostMessage failed (target may have closed)".to_string()),
                    n,
                );
            }
            n += 1;
        }
        return (true, None, n);
    }
    if typ == "down" || typ == "up" {
        let btn = bg_btn_name(a);
        let Some((dn, up, held)) = bg_buttons(&btn) else {
            return (
                false,
                Some(format!("unknown button \"{}\"", bg_btn_raw(a))),
                0,
            );
        };
        let lp = if a.contains_key("x") && a.contains_key("y") {
            match bg_client(hwnd, &a["x"], &a["y"]) {
                Ok((cx, cy)) => bg_lparam(cx, cy),
                Err(e) => return (false, Some(e), 0),
            }
        } else {
            unsafe {
                let mut pt = POINT { x: 0, y: 0 };
                if GetCursorPos(&mut pt) != 0 {
                    match bg_cursor_client(hwnd, pt.x, pt.y) {
                        Some((cx, cy)) => bg_lparam(cx, cy),
                        None => 0,
                    }
                } else {
                    0
                }
            }
        };
        let msg = if typ == "down" { dn } else { up };
        let w = if typ == "down" { held } else { 0 };
        if !bg_post(hwnd, msg, w, lp) {
            return (
                false,
                Some("PostMessage failed (target may have closed)".to_string()),
                0,
            );
        }
        return (true, None, 1);
    }
    if typ == "type" {
        let text = match a.get("text") {
            None => String::new(),
            Some(v) => py_str(v),
        };
        let mut n = 0i64;
        for unit in utf16_units(&text) {
            if !bg_post(hwnd, 0x0102, unit as u64, 1) {
                return (
                    false,
                    Some("PostMessage failed (target may have closed)".to_string()),
                    n,
                );
            }
            n += 1;
        }
        return (true, None, n);
    }
    if typ == "key" {
        let name = match a.get("name") {
            None => String::new(),
            Some(v) => py_str(v),
        };
        let vk = match vk_of(&name) {
            Ok(v) => v,
            Err(e) => return (false, Some(e), 0),
        };
        let (dm, um) = if vk == 0x12 {
            (0x0104, 0x0105)
        } else {
            (0x0100, 0x0101)
        };
        if !bg_post(hwnd, dm, vk as u64, bg_key_lparam(vk, false)) {
            return (
                false,
                Some("PostMessage failed (target may have closed)".to_string()),
                0,
            );
        }
        if !bg_post(hwnd, um, vk as u64, bg_key_lparam(vk, true)) {
            return (
                false,
                Some("PostMessage failed (target may have closed)".to_string()),
                1,
            );
        }
        return (true, None, 2);
    }
    if typ == "hotkey" {
        let keys: Result<Vec<String>, String> = match a.get("keys") {
            None => Ok(vec![]),
            Some(Value::Array(arr)) => Ok(arr.iter().map(py_str).collect()),
            Some(Value::String(s)) => Ok(s.chars().map(|c| c.to_string()).collect()),
            Some(v) => Err(format!("'{}' object is not iterable", py_type_name(v))),
        };
        let keys = match keys {
            Ok(k) => k,
            Err(e) => return (false, Some(e), 0),
        };
        if keys.is_empty() {
            return (false, Some("hotkey needs non-empty keys".to_string()), 0);
        }
        let mut vks = Vec::with_capacity(keys.len());
        for k in &keys {
            match vk_of(k) {
                Ok(v) => vks.push(v),
                Err(e) => return (false, Some(e), 0),
            }
        }
        let (dm, um) = if vks.contains(&0x12) {
            (0x0104, 0x0105)
        } else {
            (0x0100, 0x0101)
        };
        let mut n = 0i64;
        for vk in &vks {
            if !bg_post(hwnd, dm, *vk as u64, bg_key_lparam(*vk, false)) {
                return (
                    false,
                    Some("PostMessage failed (target may have closed)".to_string()),
                    n,
                );
            }
            n += 1;
        }
        for vk in vks.iter().rev() {
            if !bg_post(hwnd, um, *vk as u64, bg_key_lparam(*vk, true)) {
                return (
                    false,
                    Some("PostMessage failed (target may have closed)".to_string()),
                    n,
                );
            }
            n += 1;
        }
        return (true, None, n);
    }
    (
        false,
        Some(format!(
            "unknown action type \"{typ}\" for background delivery"
        )),
        0,
    )
}

fn bg_cursor_client(hwnd: i64, x: i32, y: i32) -> Option<(i64, i64)> {
    unsafe {
        let mut pt = POINT { x, y };
        if ScreenToClient(hwnd as HWND, &mut pt) != 0 {
            Some((pt.x as i64, pt.y as i64))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------- wait_for (puppet.py:1795-1892)

fn wf_done(t0: &Instant, ok: bool, sat: bool, err: Option<String>) -> Map<String, Value> {
    let mut d = Map::new();
    d.insert("ok".to_string(), Value::from(ok));
    d.insert("satisfied".to_string(), Value::from(sat));
    d.insert(
        "waited_ms".to_string(),
        Value::from(py_round_n(elapsed_ms(t0), 3)),
    );
    if let Some(e) = err {
        d.insert("error".to_string(), Value::from(e));
    }
    d
}

fn wf_nap(t0: &Instant, timeout_s: f64, poll_s: f64) {
    let remain = timeout_s - t0.elapsed().as_secs_f64();
    let nap = if remain > 0.001 {
        poll_s.min(remain)
    } else {
        0.001
    };
    std::thread::sleep(Duration::from_secs_f64(nap.max(0.0)));
}

fn wait_for(a: &Map<String, Value>, t0: &Instant) -> Map<String, Value> {
    let kind = match a.get("kind") {
        None => String::new(),
        Some(v) => py_str(v),
    };
    let (timeout_ms, poll_ms) = match (
        opt_py_int(a, "timeout_ms", 5000),
        opt_py_int(a, "poll_ms", 50),
    ) {
        (Ok(t), Ok(p)) => (t, p),
        _ => {
            return wf_done(
                t0,
                false,
                false,
                Some("timeout_ms/poll_ms must be ints".to_string()),
            );
        }
    };
    if timeout_ms < 0 || poll_ms <= 0 {
        return wf_done(
            t0,
            false,
            false,
            Some("timeout_ms must be >= 0 and poll_ms > 0".to_string()),
        );
    }
    if timeout_ms > 300000 {
        return wf_done(
            t0,
            false,
            false,
            Some(format!("timeout_ms absurd (>300000): {timeout_ms}")),
        );
    }
    let timeout_s = timeout_ms as f64 / 1000.0;
    let poll_s = poll_ms as f64 / 1000.0;
    if kind == "window_appear" || kind == "window_disappear" {
        let raw_sel: Option<&Value> = match a.get("selector") {
            Some(v) => Some(v),
            None => a.get("title"),
        };
        // Falsy raw selectors (None/""/0/False/empty) count as absent,
        // exactly like `if not sel` on the raw value in python.
        let mut sel: Option<String> = None;
        if let Some(v) = raw_sel {
            if py_truthy(v) {
                sel = Some(py_str(v));
            }
        }
        if sel.is_none() {
            if let Some(h) = a.get("hwnd") {
                sel = Some(format!("hwnd:{}", py_str(h)));
            }
        }
        let sel = match sel {
            Some(s) if !s.is_empty() => s,
            _ => {
                return wf_done(
                    t0,
                    false,
                    false,
                    Some(format!("wait_for kind={kind} needs \"selector\"")),
                );
            }
        };
        let want_present = kind == "window_appear";
        loop {
            let (h, _, _) = resolve_selector(&sel);
            if h.is_some() == want_present {
                return wf_done(t0, true, true, None);
            }
            if t0.elapsed().as_secs_f64() >= timeout_s {
                return wf_done(t0, true, false, None);
            }
            wf_nap(t0, timeout_s, poll_s);
        }
    }
    if kind == "pixel" {
        let parsed: Result<(i64, i64, [i64; 3], i64), ()> = (|| {
            let x = py_int(a.get("x").unwrap_or(&Value::Null)).map_err(|_| ())?;
            let y = py_int(a.get("y").unwrap_or(&Value::Null)).map_err(|_| ())?;
            let color = match a.get("color") {
                Some(Value::Array(c)) if c.len() == 3 => {
                    let mut rgb = [0i64; 3];
                    for (k, v) in c.iter().enumerate() {
                        rgb[k] = py_int(v).map_err(|_| ())?;
                    }
                    rgb
                }
                _ => return Err(()),
            };
            let tol = match a.get("tolerance") {
                None => 12,
                Some(v) => py_int(v).map_err(|_| ())?,
            };
            Ok((x, y, color, tol))
        })();
        let (x, y, color, tol) = match parsed {
            Ok(p) => p,
            Err(_) => {
                return wf_done(
                    t0,
                    false,
                    false,
                    Some(
                        "wait_for kind=pixel needs x, y, color:[r,g,b] (ints) and optional tolerance"
                            .to_string(),
                    ),
                );
            }
        };
        if color.iter().any(|v| *v < 0 || *v > 255) || tol < 0 {
            return wf_done(
                t0,
                false,
                false,
                Some("color values must be 0-255 and tolerance >= 0".to_string()),
            );
        }
        loop {
            if let Ok(img) = bitblt_grab(x as i32, y as i32, 1, 1) {
                if img.px.len() >= 3 && (0..3).all(|k| (img.px[k] as i64 - color[k]).abs() <= tol) {
                    return wf_done(t0, true, true, None);
                }
            }
            if t0.elapsed().as_secs_f64() >= timeout_s {
                return wf_done(t0, true, false, None);
            }
            wf_nap(t0, timeout_s, poll_s);
        }
    }
    if kind == "stable" {
        let region = match a.get("region") {
            None => {
                let (vx, vy, vr, vb) = virtual_rect();
                vec![vx as i64, vy as i64, vr as i64, vb as i64]
            }
            Some(Value::Array(r)) if r.len() == 4 => {
                let mut out = [0i64; 4];
                let mut ok = true;
                for (k, v) in r.iter().enumerate() {
                    match py_int(v) {
                        Ok(n) => out[k] = n,
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    return wf_done(
                        t0,
                        false,
                        false,
                        Some("region must be [left,top,right,bottom] ints".to_string()),
                    );
                }
                out.to_vec()
            }
            _ => {
                return wf_done(
                    t0,
                    false,
                    false,
                    Some("region must be [left,top,right,bottom] ints".to_string()),
                );
            }
        };
        let (rl, rt, rr, rb) = (region[0], region[1], region[2], region[3]);
        if rr <= rl || rb <= rt {
            return wf_done(
                t0,
                false,
                false,
                Some("region must have right>left and bottom>top".to_string()),
            );
        }
        let (rw, rh) = (rr - rl, rb - rt);
        if rw > MAX_STABLE_EDGE || rh > MAX_STABLE_EDGE {
            return wf_done(
                t0,
                false,
                false,
                Some(format!(
                    "region must be at most {MAX_STABLE_EDGE}x{MAX_STABLE_EDGE} (got {rw}x{rh})"
                )),
            );
        }
        let mut prev: Option<Vec<u8>> = None;
        loop {
            match bitblt_grab(rl as i32, rt as i32, (rr - rl) as i32, (rb - rt) as i32) {
                Ok(img) => {
                    if prev.as_ref() == Some(&img.px) {
                        return wf_done(t0, true, true, None);
                    }
                    prev = Some(img.px);
                }
                Err(_) => {
                    prev = None;
                }
            }
            if t0.elapsed().as_secs_f64() >= timeout_s {
                return wf_done(t0, true, false, None);
            }
            wf_nap(t0, timeout_s, poll_s);
        }
    }
    wf_done(
        t0,
        false,
        false,
        Some(format!(
            "unknown wait_for kind \"{kind}\" (want window_appear|window_disappear|pixel|stable)"
        )),
    )
}

// ---------------------------------------------------------------- focus helpers (puppet.py:1895-1934)

fn cursor_now() -> (i64, i64) {
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) != 0 {
            (pt.x as i64, pt.y as i64)
        } else {
            (-1, -1)
        }
    }
}

fn fg_now() -> i64 {
    unsafe {
        let h = GetForegroundWindow();
        if h.is_null() {
            0
        } else {
            h as i64
        }
    }
}

fn restore_foreground(hwnd: i64) -> bool {
    unsafe {
        if hwnd <= 0 {
            return false;
        }
        let h: HWND = hwnd as HWND;
        if IsWindow(h) == 0 {
            return false;
        }
        let cur_tid = GetCurrentThreadId();
        let fg = GetForegroundWindow();
        let fg_tid = if fg.is_null() {
            0
        } else {
            GetWindowThreadProcessId(fg, std::ptr::null_mut())
        };
        let tgt_tid = GetWindowThreadProcessId(h, std::ptr::null_mut());
        let mut attached: Vec<u32> = Vec::new();
        if fg_tid != 0 && fg_tid != cur_tid {
            if AttachThreadInput(cur_tid, fg_tid, 1) != 0 {
                attached.push(fg_tid);
            }
        }
        if tgt_tid != 0 && tgt_tid != cur_tid && tgt_tid != fg_tid {
            if AttachThreadInput(cur_tid, tgt_tid, 1) != 0 {
                attached.push(tgt_tid);
            }
        }
        SetForegroundWindow(h);
        for tid in attached {
            AttachThreadInput(cur_tid, tid, 0);
        }
        GetForegroundWindow() as i64 == hwnd
    }
}

/// _resolve_window with untyped action values (puppet.py:625-636).
fn resolve_window_val(
    title: Option<&Value>,
    hwnd: Option<&Value>,
) -> (
    Option<HWND>,
    Option<String>,
    Option<Vec<Map<String, Value>>>,
) {
    if let Some(hv) = hwnd {
        match py_int(hv) {
            Ok(h) => return resolve_selector(&format!("hwnd:{h}")),
            Err(e) => {
                return (
                    None,
                    Some(format!("bad_window: bad hwnd {}: {}", py_str(hv), e)),
                    None,
                );
            }
        }
    }
    if let Some(tv) = title {
        if py_truthy(tv) {
            return resolve_selector(&py_str(tv));
        }
    }
    (
        None,
        Some("bad_window: need title or hwnd".to_string()),
        None,
    )
}

// ---------------------------------------------------------------- invoke matching (puppet.py:1334-1388)

fn entry_str(e: &Map<String, Value>, key: &str) -> String {
    match e.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(v) => py_str(v),
        None => String::new(),
    }
}

fn element_public(e: &Map<String, Value>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("name".to_string(), Value::from(entry_str(e, "name")));
    m.insert("role".to_string(), Value::from(entry_str(e, "role")));
    m.insert("auto_id".to_string(), Value::from(entry_str(e, "auto_id")));
    m.insert("ref".to_string(), Value::from(entry_str(e, "ref")));
    m.insert("fp".to_string(), Value::from(entry_str(e, "fp")));
    m.insert(
        "rect".to_string(),
        e.get("rect")
            .cloned()
            .unwrap_or(Value::from(vec![0, 0, 0, 0])),
    );
    m
}

fn match_element(
    entries: &[Map<String, Value>],
    spec: Option<&Value>,
    index: Option<&Value>,
) -> (
    Option<Map<String, Value>>,
    Option<String>,
    Option<Vec<Map<String, Value>>>,
) {
    let s = match spec {
        None => String::new(),
        Some(v) => {
            if py_truthy(v) {
                py_str(v)
            } else {
                String::new()
            }
        }
    };
    if s.trim().is_empty() {
        return (
            None,
            Some("invoke needs \"element\" (e.g. \"name:OK\")".to_string()),
            None,
        );
    }
    let (kind, val) = match s.find(':') {
        Some(p) => (s[..p].trim().to_lowercase(), s[p + 1..].trim().to_string()),
        None => ("name".to_string(), s.trim().to_string()),
    };
    let hits: Vec<Map<String, Value>> = if kind == "path" {
        entries
            .iter()
            .filter(|e| entry_str(e, "path") == val)
            .cloned()
            .collect()
    } else if ["auto_id", "autoid", "automationid", "id"].contains(&kind.as_str()) {
        entries
            .iter()
            .filter(|e| entry_str(e, "auto_id") == val)
            .cloned()
            .collect()
    } else if kind == "name" {
        let exact: Vec<Map<String, Value>> = entries
            .iter()
            .filter(|e| entry_str(e, "name") == val)
            .cloned()
            .collect();
        if exact.is_empty() {
            let low = val.to_lowercase();
            entries
                .iter()
                .filter(|e| entry_str(e, "name").to_lowercase().contains(&low))
                .cloned()
                .collect()
        } else {
            exact
        }
    } else {
        return (
            None,
            Some(format!(
                "unknown element selector kind \"{kind}\" (want name:|auto_id:|path:)"
            )),
            None,
        );
    };
    if hits.is_empty() {
        return (
            None,
            Some(format!(
                "no element matching \"{s}\" ({} elements listed)",
                entries.len()
            )),
            None,
        );
    }
    if index.is_none() {
        if hits.len() > 1 {
            let cands: Vec<Map<String, Value>> = hits.iter().map(element_public).collect();
            return (
                None,
                Some(format!(
                    "ambiguous element: {} elements match \"{s}\" (pass \"index\" 0..{} to choose)",
                    hits.len(),
                    hits.len() - 1
                )),
                Some(cands),
            );
        }
        return (Some(hits[0].clone()), None, None);
    }
    let n = match py_int(index.unwrap()) {
        Ok(n) => n,
        Err(_) => return (None, Some("element index must be an int".to_string()), None),
    };
    if n < 0 || n >= hits.len() as i64 {
        return (
            None,
            Some(format!(
                "element index {n} out of range: {} element(s) match \"{s}\"",
                hits.len()
            )),
            None,
        );
    }
    (Some(hits[n as usize].clone()), None, None)
}

fn element_pub_full(entry: &Map<String, Value>) -> Map<String, Value> {
    let mut m = Map::new();
    for k in [
        "win",
        "role",
        "name",
        "auto_id",
        "rect",
        "invokable",
        "path",
        "fp",
    ] {
        if let Some(v) = entry.get(k) {
            m.insert(k.to_string(), v.clone());
        }
    }
    m
}

/// Gap-1 freshness gate: when the action carries `expect_fp`, the freshly
/// re-walked entry's fingerprint must equal it. Returns the typed refusal on
/// mismatch, None when the gate passes (no/empty expect_fp, or a match).
/// Applies live and under dry_run alike: a stale ref never acts.
fn check_expect_fp(
    a: &Map<String, Value>,
    entry: &Map<String, Value>,
    sel: &str,
) -> Option<String> {
    let want = match a.get("expect_fp") {
        None | Some(Value::Null) => return None,
        Some(v) => py_str(v),
    };
    if want.trim().is_empty() {
        return None;
    }
    let got = entry_str(entry, "fp");
    if got == want {
        return None;
    }
    Some(format!(
        "stale_ref: element \"{sel}\" changed since its snapshot (expected fp {want}, now {got}). Take a fresh `elements` snapshot and retry with the new ref/fp."
    ))
}

// ---------------------------------------------------------------- invoke activation (UIA patterns + Win32 extent)

/// BM_CLICK a Win32 button child. No mouse, no focus change (puppet.py:1442-1462).
fn win32_activate(entry: &Map<String, Value>) -> (bool, String, Option<String>) {
    let inv = entry
        .get("invokable")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !inv {
        return (
            false,
            String::new(),
            Some(
                "element is not invokable under the Win32 backend (pass \"fallback\":\"click\" for a pixel click)"
                    .to_string(),
            ),
        );
    }
    let root = entry.get("win").and_then(|v| v.as_i64()).unwrap_or(0);
    let path = entry_str(entry, "path");
    let seg = path.split('.').next().unwrap_or("0");
    let idx = match py_int(&Value::String(seg.to_string())) {
        Ok(n) => n,
        Err(e) => {
            return (
                false,
                String::new(),
                Some(format!("win32-click failed: {e}")),
            )
        }
    };
    let kids = match enum_child_windows(root as HWND) {
        Ok(k) => k,
        Err(e) => {
            return (
                false,
                String::new(),
                Some(format!("win32-click failed: {e}")),
            )
        }
    };
    if idx < 0 || idx >= kids.len() as i64 {
        return (
            false,
            String::new(),
            Some(format!("Win32 path {} went stale", py_repr_str(&path))),
        );
    }
    unsafe {
        SendMessageW(kids[idx as usize], 0x00F5, 0, 0);
    }
    (true, "win32-click".to_string(), None)
}

/// Synchronous SendInput move + left click (puppet.py:1465-1469). Live only.
fn pixel_click_at(x: i64, y: i64) -> bool {
    let seq = vec![mi_move(x, y), mi_btn(M_LDOWN, 0), mi_btn(M_LUP, 0)];
    unsafe {
        SendInput(
            seq.len() as u32,
            seq.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        ) == seq.len() as u32
    }
}

// ---------------------------------------------------------------- UIPI / elevation (gap 2)

/// Pure refusal text (gap 2): the typed, actionable error naming the
/// elevated target. Separated from the live reader below for testability;
/// essentials are pinned by `uipi_refusal_text_names_target_and_recovery`.
fn format_uipi_refusal(exe: &str, pid: u32, hwnd: i64) -> String {
    format!(
        "uipi_blocked: target \"{exe}\" (pid {pid}, hwnd {hwnd}) runs elevated; input from this unelevated process would be silently dropped by UIPI (SendInput neither sends nor names it). Relaunch this server elevated and retry; under dry_run this refusal is reported without injecting anything."
    )
}

/// Typed, actionable UIPI refusal naming the elevated target (gap 2). None
/// when input may proceed: zero hwnd, non-elevated target, elevated self, or
/// unreadable elevation (fail-open). Never injects, never switches modes.
fn uipi_refusal(hwnd: i64) -> Option<String> {
    if hwnd == 0 {
        return None;
    }
    if !crate::uipi_blocked(crate::window_elevated(hwnd as HWND), crate::self_elevated()) {
        return None;
    }
    let pid = crate::pid_of(hwnd as HWND);
    Some(format_uipi_refusal(&crate::exe_of_pid(pid), pid, hwnd))
}

fn window_at(x: i64, y: i64) -> Option<i64> {
    let (x, y) = (
        x.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        y.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
    );
    unsafe {
        let h = WindowFromPoint(POINT { x, y });
        if h.is_null() {
            None
        } else {
            Some(h as i64)
        }
    }
}

/// Windows that would receive one foreground input action (gap 2): the real
/// recipient, not the cursor heuristic -- WindowFromPoint for coordinate
/// actions, the foreground window for keyboard/scroll-without-coords. Empty
/// when the action injects into no process (move), is unreadable, or is not
/// foreground input at all. Parse failures yield no targets here; the action
/// handler below reports its own argument error.
fn uipi_targets_for_action(typ: &str, a: &Map<String, Value>) -> Vec<i64> {
    let mut out = Vec::new();
    let mut push_opt = |h: Option<i64>| {
        if let Some(v) = h {
            out.push(v);
        }
    };
    match typ {
        "click" | "down" | "up" => match (a.get("x"), a.get("y")) {
            (Some(xv), Some(yv)) => match (py_int(xv), py_int(yv)) {
                (Ok(x), Ok(y)) => push_opt(window_at(x, y)),
                _ => {}
            },
            _ => {
                let (cx, cy) = cursor_now();
                push_opt(window_at(cx, cy));
            }
        },
        "drag" => {
            for key in ["from", "to"] {
                if let Some(v) = a.get(key) {
                    if let Ok((x, y)) = unpack2(v) {
                        push_opt(window_at(x, y));
                    }
                }
            }
        }
        "scroll" => match (a.get("x"), a.get("y")) {
            (Some(xv), Some(yv)) => match (py_int(xv), py_int(yv)) {
                (Ok(x), Ok(y)) => push_opt(window_at(x, y)),
                _ => {}
            },
            _ => {
                let f = fg_now();
                if f != 0 {
                    out.push(f);
                }
            }
        },
        "type" | "key" | "hotkey" => {
            let f = fg_now();
            if f != 0 {
                out.push(f);
            }
        }
        _ => {}
    }
    out
}

// ---------------------------------------------------------------- burst state + flush (puppet.py:2747-2791)

struct Burst {
    pending: Vec<INPUT>,
    marked: Vec<usize>,
    results: Vec<Map<String, Value>>,
    calls: i64,
    events: i64,
    sendinput_ms: f64,
    dry_run: bool,
    /// Gap 2: set when the last flush short-sent against an elevated
    /// foreground (UIPI-suspected). Read by fail_since_flush at the call site.
    flush_uipi: bool,
}

impl Burst {
    /// Empty pending -> (0, None), no call counted. Live partial sends
    /// retro-mark every covered action via fail_since_flush (caller).
    fn flush(&mut self) -> (f64, Option<String>) {
        self.flush_uipi = false;
        if self.pending.is_empty() {
            return (0.0, None);
        }
        let n = self.pending.len();
        if self.dry_run {
            let t = Instant::now();
            self.pending.clear();
            let dt = elapsed_ms(&t);
            self.calls += 1;
            self.events += n as i64;
            self.sendinput_ms += dt;
            return (dt, None);
        }
        let t = Instant::now();
        let sent = unsafe {
            SendInput(
                self.pending.len() as u32,
                self.pending.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        };
        let dt = elapsed_ms(&t);
        self.calls += 1;
        self.sendinput_ms += dt;
        if sent != n as u32 {
            let mut msg = format!("SendInput sent {sent}/{n} events");
            if sent == 0 {
                unsafe {
                    let code = GetLastError();
                    msg.push_str(&format!(": [WinError {code}]"));
                }
                // Gap 2 post-hoc: a zero-send against an elevated foreground
                // is UIPI-suspected (SendInput never names UIPI itself).
                // Pre-checks above refuse the known-elevated case before
                // injecting; this names the race that slips between.
                let fg = fg_now();
                if crate::uipi_blocked(crate::window_elevated(fg as HWND), crate::self_elevated())
                {
                    let pid = crate::pid_of(fg as HWND);
                    let exe = crate::exe_of_pid(pid);
                    msg.push_str(&format!(
                        "; UIPI-suspected: foreground \"{exe}\" (pid {pid}, hwnd {fg}) is elevated"
                    ));
                    self.flush_uipi = true;
                }
            }
            self.events += sent as i64;
            self.pending.clear();
            return (dt, Some(msg));
        }
        self.events += n as i64;
        self.pending.clear();
        (dt, None)
    }
}

fn fail_since_flush(
    results: &mut [Map<String, Value>],
    marked: &mut Vec<usize>,
    msg: &str,
    uipi: bool,
) {
    for &i in marked.iter() {
        if let Some(r) = results.get_mut(i) {
            let is_false = r.get("ok") == Some(&Value::from(false));
            if !is_false {
                r.insert("ok".to_string(), Value::from(false));
                r.insert("error".to_string(), Value::from(msg.to_string()));
                if uipi {
                    r.insert("blocked_by_uipi".to_string(), Value::from(true));
                }
            }
        }
    }
    marked.clear();
}

fn base_entry(index: usize, typ: &Value, ok: bool, ms: f64) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("index".to_string(), Value::from(index as i64));
    m.insert("type".to_string(), typ.clone());
    m.insert("ok".to_string(), Value::from(ok));
    m.insert("ms".to_string(), Value::from(ms));
    m
}

fn unpack2(v: &Value) -> Result<(i64, i64), String> {
    match v {
        Value::Array(a) if a.len() == 2 => Ok((py_int(&a[0])?, py_int(&a[1])?)),
        Value::Array(a) if a.len() < 2 => Err(format!(
            "not enough values to unpack (expected 2, got {})",
            a.len()
        )),
        Value::Array(_) => Err("too many values to unpack (expected 2)".to_string()),
        _ => Err(format!(
            "cannot unpack non-iterable {} object",
            py_type_name(v)
        )),
    }
}

// ---------------------------------------------------------------- the tool

pub fn do_burst(
    actions: &[Value],
    background: Option<String>,
    focus_guard: bool,
    settle_ms: i64,
    dry_run: bool,
    desktop: Option<String>,
) -> String {
    let t_all = Instant::now();
    let focus_before = fg_now();
    let cursor_before = cursor_now();

    // Boot phase (puppet.py:2693-2746). Top-level wire types are validated in
    // serve.rs; only the range clamps are live here.
    let mut boot_errors: Vec<Map<String, Value>> = Vec::new();
    let mut settle_n = settle_ms;
    if settle_n < 0 {
        settle_n = 0;
    } else if settle_n > 60000 {
        let mut m = Map::new();
        m.insert("index".to_string(), Value::from(0));
        m.insert("type".to_string(), Value::from("settle"));
        m.insert("ok".to_string(), Value::from(false));
        m.insert("ms".to_string(), Value::from(0.0));
        m.insert(
            "error".to_string(),
            Value::from(format!("settle_ms absurd (>60000): {settle_n}")),
        );
        boot_errors.push(m);
        settle_n = 0;
    }
    let hidden_mode = desktop
        .as_deref()
        .map(|d| d.trim().eq_ignore_ascii_case("hidden"))
        .unwrap_or(false);
    let mut hidden_owner: Option<crate::hidden::Hidden> = None;
    let mut hidden_created = false;
    let mut hidden_set_desktop = false;
    let mut hidden_pid: Option<u32> = None;
    let mut hidden_name = String::new();
    let mut hidden_hwnd: Option<i64> = None;
    let mut hidden_class = String::new();
    let mut hidden_title = String::new();
    let mut hidden_win_pid: u32 = 0;

    let bg_mode = background.is_some() || hidden_mode;
    let (bg_hwnd, bg_error): (Option<i64>, Option<String>) = if hidden_mode {
        // Opt-in mode: `background` names the program to launch on a private
        // desktop; its window is resolved with EnumDesktopWindows (never the
        // owner's EnumWindows) and then driven through the same PostMessage
        // branch below.
        let prog = match background.as_deref() {
            None => Err(
                "bad_window: desktop=\"hidden\" needs background to name the program to launch (e.g. exe:notepad.exe)"
                    .to_string(),
            ),
            Some(s) => crate::hidden::program_from_background(s),
        };
        match prog {
            Err(e) => (None, Some(e)),
            Ok(p) => {
                let h = crate::hidden::spawn(&p);
                hidden_created = h.ready.created;
                hidden_set_desktop = h.ready.set_desktop;
                hidden_pid = h.ready.pid;
                hidden_hwnd = h.ready.hwnd;
                hidden_class = h.ready.hwnd_class.clone();
                hidden_title = h.ready.hwnd_title.clone();
                hidden_win_pid = h.ready.hwnd_pid;
                hidden_name = h.ready.desktop_name.clone();
                let err = h.ready.error.clone();
                hidden_owner = Some(h);
                match err {
                    Some(e) => (None, Some(e)),
                    None => (hidden_hwnd, None),
                }
            }
        }
    } else if !bg_mode {
        (None, None)
    } else {
        let s = background.unwrap();
        if s.trim().is_empty() {
            (
                None,
                Some(
                    "bad_window: background must be a selector string (hwnd:|pid:|exe:|title:)"
                        .to_string(),
                ),
            )
        } else {
            let (h, err, _) = resolve_selector(&s);
            match h {
                Some(x) => (Some(x as i64), None),
                None => (
                    None,
                    Some(err.unwrap_or("bad_window: cannot resolve background target".to_string())),
                ),
            }
        }
    };
    let mut posted_total: i64 = 0;

    let mut st = Burst {
        pending: Vec::new(),
        marked: Vec::new(),
        results: boot_errors,
        calls: 0,
        events: 0,
        sendinput_ms: 0.0,
        dry_run,
        flush_uipi: false,
    };

    for (i, a) in actions.iter().enumerate() {
        let t0 = Instant::now();
        let ad = match a {
            Value::Object(m) if m.contains_key("type") => m,
            _ => {
                let typ = match a {
                    Value::Object(m) => m.get("type").cloned().unwrap_or(Value::Null),
                    _ => Value::Null,
                };
                let mut m = base_entry(i, &typ, false, 0.0);
                m.insert(
                    "error".to_string(),
                    Value::from("action must be a dict with 'type'"),
                );
                st.results.push(m);
                continue;
            }
        };
        let typ_v = ad["type"].clone();
        let typ = typ_v.as_str().unwrap_or("\u{0}");
        let is_bg_postable = bg_mode
            && matches!(
                typ,
                "click" | "type" | "key" | "hotkey" | "down" | "up" | "move" | "drag" | "scroll"
            );

        let step: Result<Option<Map<String, Value>>, String> = (|| {
            // Gap-2 UIPI pre-check for foreground input: resolve the window
            // that would receive this action and refuse BEFORE queueing
            // anything when it is elevated. Background/invoke/set_value check
            // their own explicit targets in their branches below. Reports
            // under dry_run too, without injecting anything.
            if !bg_mode {
                for t in uipi_targets_for_action(typ, ad) {
                    if let Some(msg) = uipi_refusal(t) {
                        let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                        m.insert("error".to_string(), Value::from(msg));
                        m.insert("blocked_by_uipi".to_string(), Value::from(true));
                        return Ok(Some(m));
                    }
                }
            }
            // Background PostMessage branch (puppet.py:2808-2876).
            if is_bg_postable {
                if let Some(note) = bg_non_postable(typ) {
                    let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    m.insert("mode".to_string(), Value::from("background"));
                    m.insert(
                        "bg_hwnd".to_string(),
                        bg_hwnd.map(Value::from).unwrap_or(Value::Null),
                    );
                    m.insert(
                        "error".to_string(),
                        Value::from(format!("{note}{BG_SUFFIX}")),
                    );
                    m.insert("note".to_string(), Value::from(BG_NOTE));
                    return Ok(Some(m));
                }
                if let Some(be) = bg_error.clone() {
                    let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    m.insert("mode".to_string(), Value::from("background"));
                    m.insert("bg_hwnd".to_string(), Value::Null);
                    m.insert("error".to_string(), Value::from(be));
                    m.insert("note".to_string(), Value::from(BG_NOTE));
                    return Ok(Some(m));
                }
                // Gap-2 UIPI pre-check for background delivery (PostMessage to
                // a higher-integrity window is dropped silently): refuse with
                // a typed error instead of posting, under dry_run too. Never
                // silently switches to foreground delivery.
                if let Some(bh) = bg_hwnd {
                    if let Some(msg) = uipi_refusal(bh) {
                        let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                        m.insert("mode".to_string(), Value::from("background"));
                        m.insert("bg_hwnd".to_string(), Value::from(bh));
                        m.insert("posted".to_string(), Value::from(0));
                        m.insert("error".to_string(), Value::from(msg));
                        m.insert("blocked_by_uipi".to_string(), Value::from(true));
                        m.insert("note".to_string(), Value::from(BG_NOTE));
                        if dry_run {
                            m.insert("dry_run".to_string(), Value::from(true));
                        }
                        return Ok(Some(m));
                    }
                }
                if dry_run {
                    let mut m = base_entry(i, &typ_v, true, elapsed_ms(&t0));
                    m.insert("mode".to_string(), Value::from("background"));
                    m.insert(
                        "bg_hwnd".to_string(),
                        bg_hwnd.map(Value::from).unwrap_or(Value::Null),
                    );
                    m.insert("posted".to_string(), Value::from(0));
                    m.insert("dry_run".to_string(), Value::from(true));
                    m.insert("note".to_string(), Value::from(BG_NOTE));
                    return Ok(Some(m));
                }
                let (ok_b, err_b, n_b) = bg_execute(bg_hwnd.unwrap_or(0), ad);
                posted_total += n_b;
                let mut m = base_entry(i, &typ_v, ok_b, elapsed_ms(&t0));
                m.insert("mode".to_string(), Value::from("background"));
                m.insert(
                    "bg_hwnd".to_string(),
                    bg_hwnd.map(Value::from).unwrap_or(Value::Null),
                );
                m.insert("posted".to_string(), Value::from(n_b));
                m.insert("note".to_string(), Value::from(BG_NOTE));
                if let Some(e) = err_b {
                    m.insert("error".to_string(), Value::from(e));
                }
                return Ok(Some(m));
            }
            if typ == "move" {
                let mx = py_int(ad.get("x").unwrap_or(&Value::Null)).map_err(|_| key_err("x"))?;
                let my = py_int(ad.get("y").unwrap_or(&Value::Null)).map_err(|_| key_err("y"))?;
                require_on_desktop(mx, my)?;
                st.pending.push(mi_move(mx, my));
                st.marked.push(i);
                return Ok(Some(base_entry(i, &typ_v, true, elapsed_ms(&t0))));
            }
            if typ == "click" {
                let (dn, up) = btn_pair(ad.get("button"))?;
                if ad.contains_key("x") && ad.contains_key("y") {
                    let cx = py_int(&ad["x"])?;
                    let cy = py_int(&ad["y"])?;
                    require_on_desktop(cx, cy)?;
                    st.pending.push(mi_move(cx, cy));
                }
                let count = match ad.get("count") {
                    None => 1,
                    Some(v) => py_int(v)?,
                }
                .max(1);
                if count > MAX_CLICK_COUNT {
                    return Err(format!("click count absurd (>{MAX_CLICK_COUNT}): {count}"));
                }
                for _ in 0..count {
                    st.pending.push(mi_btn(dn, 0));
                    st.pending.push(mi_btn(up, 0));
                }
                st.marked.push(i);
                return Ok(Some(base_entry(i, &typ_v, true, elapsed_ms(&t0))));
            }
            if typ == "down" || typ == "up" {
                let (dn, up) = btn_pair(ad.get("button"))?;
                st.pending
                    .push(mi_btn(if typ == "down" { dn } else { up }, 0));
                st.marked.push(i);
                return Ok(Some(base_entry(i, &typ_v, true, elapsed_ms(&t0))));
            }
            if typ == "drag" {
                let (dn, up) = btn_pair(ad.get("button"))?;
                let fv = ad.get("from").ok_or_else(|| key_err("from"))?;
                let tv = ad.get("to").ok_or_else(|| key_err("to"))?;
                let (fx, fy) = unpack2(fv)?;
                let (tx, ty) = unpack2(tv)?;
                require_on_desktop(fx, fy)?;
                require_on_desktop(tx, ty)?;
                st.pending.push(mi_move(fx, fy));
                st.pending.push(mi_btn(dn, 0));
                st.pending.push(mi_move(tx, ty));
                st.pending.push(mi_btn(up, 0));
                st.marked.push(i);
                return Ok(Some(base_entry(i, &typ_v, true, elapsed_ms(&t0))));
            }
            if typ == "scroll" {
                if ad.contains_key("x") && ad.contains_key("y") {
                    let sx = py_int(&ad["x"])?;
                    let sy = py_int(&ad["y"])?;
                    require_on_desktop(sx, sy)?;
                    st.pending.push(mi_move(sx, sy));
                }
                let amt = opt_py_int(ad, "amount", 1)?;
                let data = ((amt as i64).wrapping_mul(120) & 0xFFFFFFFF) as u32;
                st.pending.push(mi_btn(M_WHEEL, data));
                st.marked.push(i);
                return Ok(Some(base_entry(i, &typ_v, true, elapsed_ms(&t0))));
            }
            if typ == "type" {
                let text = match ad.get("text") {
                    None => String::new(),
                    Some(v) => py_str(v),
                };
                let cc_raw = ad.get("chunk_chars");
                let cm_raw = ad.get("chunk_ms").cloned().unwrap_or(Value::from(0));
                let cc_probe: Option<i64> = match cc_raw {
                    None => None,
                    Some(v) => Some(py_int(v).map_err(|_| {
                        format!("chunk_chars must be an int (got {})", py_repr_value(v))
                    })?),
                };
                if cc_probe.is_none_or(|c| c == 0)
                    || (cc_probe.is_some_and(|c| c > 0)
                        && text.chars().count() <= cc_probe.unwrap() as usize)
                {
                    if cc_probe.is_some_and(|c| c < 0) {
                        return Err(format!(
                            "chunk_chars must be positive (got {})",
                            cc_probe.unwrap()
                        ));
                    }
                    for unit in utf16_units(&text) {
                        st.pending.push(ki(0, unit, K_UNICODE));
                        st.pending.push(ki(0, unit, K_UNICODE | K_UP));
                    }
                    st.marked.push(i);
                    return Ok(Some(base_entry(i, &typ_v, true, elapsed_ms(&t0))));
                }
                let cc = py_int(cc_raw.unwrap()).map_err(|_| {
                    format!(
                        "chunk_chars must be an int (got {})",
                        py_repr_value(cc_raw.unwrap())
                    )
                })?;
                let cm = py_int(&cm_raw).map_err(|_| {
                    format!("chunk_ms must be an int (got {})", py_repr_value(&cm_raw))
                })?;
                if cc <= 0 {
                    return Err(format!("chunk_chars must be positive (got {cc})"));
                }
                if !(0..=60000).contains(&cm) {
                    return Err(format!("chunk_ms out of range 0..60000 (got {cm})"));
                }
                let mut d = base_entry(i, &typ_v, true, 0.0);
                st.marked.push(i);
                let chunks: Vec<String> = {
                    let chars: Vec<char> = text.chars().collect();
                    chars
                        .chunks(cc as usize)
                        .map(|c| c.iter().collect())
                        .collect()
                };
                for (ci, ch) in chunks.iter().enumerate() {
                    for unit in utf16_units(ch) {
                        st.pending.push(ki(0, unit, K_UNICODE));
                        st.pending.push(ki(0, unit, K_UNICODE | K_UP));
                    }
                    let (_, ferr) = st.flush();
                    if let Some(fe) = ferr {
                        st.marked.retain(|&m| m != i);
                        fail_since_flush(&mut st.results, &mut st.marked, &fe, st.flush_uipi);
                        d.insert("ok".to_string(), Value::from(false));
                        d.insert("error".to_string(), Value::from(fe));
                        break;
                    }
                    st.marked.clear();
                    if ci + 1 < chunks.len() {
                        st.marked.push(i);
                        if cm != 0 && !dry_run {
                            std::thread::sleep(Duration::from_millis(cm as u64));
                        }
                    }
                }
                d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                return Ok(Some(d));
            }
            if typ == "key" {
                let name = match ad.get("name") {
                    None => String::new(),
                    Some(v) => py_str(v),
                };
                let vk = vk_of(&name)?;
                st.pending.push(ki(vk, 0, 0));
                st.pending.push(ki(vk, 0, K_UP));
                st.marked.push(i);
                return Ok(Some(base_entry(i, &typ_v, true, elapsed_ms(&t0))));
            }
            if typ == "hotkey" {
                let keys: Vec<String> = match ad.get("keys") {
                    None => vec![],
                    Some(Value::Array(arr)) => arr.iter().map(py_str).collect(),
                    Some(Value::String(s)) => s.chars().map(|c| c.to_string()).collect(),
                    Some(v) => {
                        return Err(format!("'{}' object is not iterable", py_type_name(v)));
                    }
                };
                if keys.is_empty() {
                    return Err("hotkey needs non-empty keys".to_string());
                }
                let mut vks = Vec::with_capacity(keys.len());
                for k in &keys {
                    vks.push(vk_of(k)?);
                }
                for vk in &vks {
                    st.pending.push(ki(*vk, 0, 0));
                }
                for vk in vks.iter().rev() {
                    st.pending.push(ki(*vk, 0, K_UP));
                }
                st.marked.push(i);
                return Ok(Some(base_entry(i, &typ_v, true, elapsed_ms(&t0))));
            }
            if typ == "clipboard_set" {
                let (_, ferr) = st.flush();
                if let Some(fe) = ferr {
                    fail_since_flush(&mut st.results, &mut st.marked, &fe, st.flush_uipi);
                } else {
                    st.marked.clear();
                }
                if dry_run {
                    let mut m = base_entry(i, &typ_v, true, elapsed_ms(&t0));
                    m.insert("dry_run".to_string(), Value::from(true));
                    return Ok(Some(m));
                }
                let text = match ad.get("text") {
                    None => String::new(),
                    Some(v) => py_str(v),
                };
                match clipboard_set(&text) {
                    Ok(_) => return Ok(Some(base_entry(i, &typ_v, true, elapsed_ms(&t0)))),
                    Err(e) => {
                        let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                        m.insert("error".to_string(), Value::from(e));
                        return Ok(Some(m));
                    }
                }
            }
            if typ == "clipboard_get" {
                let (_, ferr) = st.flush();
                if let Some(fe) = ferr {
                    fail_since_flush(&mut st.results, &mut st.marked, &fe, st.flush_uipi);
                } else {
                    st.marked.clear();
                }
                match clipboard_get() {
                    Ok(txt) => {
                        let mut m = base_entry(i, &typ_v, true, elapsed_ms(&t0));
                        m.insert("text".to_string(), Value::from(txt));
                        if dry_run {
                            m.insert("dry_run".to_string(), Value::from(true));
                        }
                        return Ok(Some(m));
                    }
                    Err(e) => {
                        let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                        m.insert("error".to_string(), Value::from(e));
                        return Ok(Some(m));
                    }
                }
            }
            if typ == "invoke" {
                let (_, ferr) = st.flush();
                if let Some(fe) = ferr {
                    fail_since_flush(&mut st.results, &mut st.marked, &fe, st.flush_uipi);
                } else {
                    st.marked.clear();
                }
                let mut d = base_entry(i, &typ_v, false, 0.0);
                if bg_mode && (bg_error.is_some() || bg_hwnd.is_none()) {
                    d.insert(
                        "error".to_string(),
                        Value::from(
                            bg_error
                                .clone()
                                .unwrap_or("bad_window: no background target".to_string()),
                        ),
                    );
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                let tgt = match (bg_mode, bg_hwnd) {
                    (true, Some(h)) => h,
                    _ => fg_now(),
                };
                d.insert("target_hwnd".to_string(), Value::from(tgt));
                if tgt == 0 {
                    d.insert(
                        "error".to_string(),
                        Value::from(
                            "bad_window: invoke needs a background target or a foreground window",
                        ),
                    );
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                // Gap-2 UIPI pre-check on the invoke target (UIA cannot drive
                // an elevated window without UIAccess either): refuse with a
                // typed error before resolving or firing, under dry_run too.
                if let Some(msg) = uipi_refusal(tgt) {
                    d.insert("ok".to_string(), Value::from(false));
                    d.insert("error".to_string(), Value::from(msg));
                    d.insert("blocked_by_uipi".to_string(), Value::from(true));
                    d.insert("resolution".to_string(), Value::from("uipi_blocked"));
                    d.insert("via".to_string(), Value::from("none"));
                    if dry_run {
                        d.insert("dry_run".to_string(), Value::from(true));
                    }
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                // UIA-first element model (puppet.py:1253-1331); element_model
                // never raises here: an EnumChildWindows failure surfaces as
                // an empty win32 list with an empty_reason.
                let model = element_model(tgt as HWND);
                if dry_run {
                    d.insert("dry_run".to_string(), Value::from(true));
                }
                d.insert("backend".to_string(), Value::from(model.backend.clone()));
                let (entry, merr, mcands) =
                    match_element(&model.entries, ad.get("element"), ad.get("index"));
                if let Some(me) = merr {
                    d.insert("error".to_string(), Value::from(me));
                    d.insert(
                        "resolution".to_string(),
                        Value::from(if mcands.is_some() {
                            "ambiguous"
                        } else {
                            "not-found"
                        }),
                    );
                    if let Some(c) = mcands {
                        d.insert(
                            "candidates".to_string(),
                            Value::from(c.into_iter().map(Value::Object).collect::<Vec<_>>()),
                        );
                    }
                    if dry_run {
                        d.insert("via".to_string(), Value::from("none"));
                    }
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                let entry = entry.unwrap();
                // Gap-1 freshness gate (before the dry_run early-return so a
                // stale ref refuses under dry_run too, never resolving).
                if let Some(se) = check_expect_fp(ad, &entry, &py_str(ad.get("element").unwrap_or(&Value::Null))) {
                    d.insert("ok".to_string(), Value::from(false));
                    d.insert("error".to_string(), Value::from(se));
                    d.insert("resolution".to_string(), Value::from("stale"));
                    d.insert("via".to_string(), Value::from("none"));
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                if dry_run {
                    d.insert("ok".to_string(), Value::from(true));
                    d.insert("resolution".to_string(), Value::from("found"));
                    d.insert("element".to_string(), Value::from(element_pub_full(&entry)));
                    d.insert("via".to_string(), Value::from("none"));
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                d.insert("element".to_string(), Value::from(element_pub_full(&entry)));
                d.insert("resolution".to_string(), Value::from("found"));
                // puppet.py:3253-3258: UIA backend fires pattern activation,
                // Win32 backend BM_CLICKs. Pattern calls are implemented but
                // marked `unconfirmed: activation needs owner permission`.
                let (ok_v, mut via, mut verr) = if model.backend == "uia" {
                    crate::uia::uia_activate(tgt, &entry_str(&entry, "path"))
                } else {
                    win32_activate(&entry)
                };
                let fallback = match ad.get("fallback") {
                    None => String::new(),
                    Some(v) => py_str(v).to_lowercase(),
                };
                if !ok_v && fallback == "click" {
                    let rect: Vec<i64> = match entry.get("rect") {
                        Some(Value::Array(r)) => {
                            r.iter().map(|v| v.as_i64().unwrap_or(0)).collect()
                        }
                        _ => vec![0, 0, 0, 0],
                    };
                    let (rl, rt, rw, rh) = (
                        *rect.first().unwrap_or(&0),
                        *rect.get(1).unwrap_or(&0),
                        *rect.get(2).unwrap_or(&0),
                        *rect.get(3).unwrap_or(&0),
                    );
                    if rw > 0 && rh > 0 {
                        if pixel_click_at(rl + rw / 2, rt + rh / 2) {
                            via = "pixel-click".to_string();
                            verr = None;
                            d.insert("ok".to_string(), Value::from(true));
                        } else {
                            let suffix = "pixel-click SendInput failed";
                            verr = Some(match verr {
                                Some(v) => format!("{v}; {suffix}"),
                                None => suffix.to_string(),
                            });
                            if via.is_empty() {
                                via = "pixel-click".to_string();
                            }
                            d.insert("ok".to_string(), Value::from(false));
                        }
                    } else {
                        let suffix = "pixel-click refused: empty rect";
                        verr = Some(match verr {
                            Some(v) => format!("{v}; {suffix}"),
                            None => suffix.to_string(),
                        });
                        d.insert("ok".to_string(), Value::from(false));
                    }
                } else {
                    d.insert("ok".to_string(), Value::from(ok_v));
                }
                d.insert(
                    "via".to_string(),
                    Value::from(if via.is_empty() {
                        "none".to_string()
                    } else {
                        via
                    }),
                );
                let ok_now = d.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                if let Some(e) = verr {
                    if !ok_now {
                        d.insert("error".to_string(), Value::from(e));
                    }
                }
                d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                return Ok(Some(d));
            }
            if typ == "set_value" {
                let (_, ferr) = st.flush();
                if let Some(fe) = ferr {
                    fail_since_flush(&mut st.results, &mut st.marked, &fe, st.flush_uipi);
                } else {
                    st.marked.clear();
                }
                let mut d = base_entry(i, &typ_v, false, 0.0);
                if bg_mode && (bg_error.is_some() || bg_hwnd.is_none()) {
                    d.insert(
                        "error".to_string(),
                        Value::from(
                            bg_error
                                .clone()
                                .unwrap_or("bad_window: no background target".to_string()),
                        ),
                    );
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                let tgt = match (bg_mode, bg_hwnd) {
                    (true, Some(h)) => h,
                    _ => fg_now(),
                };
                d.insert("target_hwnd".to_string(), Value::from(tgt));
                if tgt == 0 {
                    d.insert(
                        "error".to_string(),
                        Value::from(
                            "bad_window: set_value needs a background target or a foreground window",
                        ),
                    );
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                // Gap-2 UIPI pre-check on the set_value target, mirroring
                // invoke: refuse with a typed error before resolving or
                // writing, under dry_run too.
                if let Some(msg) = uipi_refusal(tgt) {
                    d.insert("ok".to_string(), Value::from(false));
                    d.insert("error".to_string(), Value::from(msg));
                    d.insert("blocked_by_uipi".to_string(), Value::from(true));
                    d.insert("resolution".to_string(), Value::from("uipi_blocked"));
                    d.insert("via".to_string(), Value::from("none"));
                    if dry_run {
                        d.insert("dry_run".to_string(), Value::from(true));
                    }
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                let model = element_model(tgt as HWND);
                if dry_run {
                    d.insert("dry_run".to_string(), Value::from(true));
                }
                d.insert("backend".to_string(), Value::from(model.backend.clone()));
                // Argument validation: element must be present (same
                // name:|auto_id:|path: forms as invoke; ref: stays rejected
                // by match_element's unknown-kind path). Empty here gets the
                // set_value-typed message instead of invoke's.
                let elem_ok = match ad.get("element") {
                    Some(v) if py_truthy(v) && !py_str(v).trim().is_empty() => true,
                    _ => false,
                };
                if !elem_ok {
                    d.insert(
                        "error".to_string(),
                        Value::from("set_value needs \"element\" (e.g. \"name:OK\")"),
                    );
                    d.insert("resolution".to_string(), Value::from("not-found"));
                    if dry_run {
                        d.insert("via".to_string(), Value::from("none"));
                    }
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                let (entry, merr, mcands) =
                    match_element(&model.entries, ad.get("element"), ad.get("index"));
                if let Some(me) = merr {
                    d.insert("error".to_string(), Value::from(me));
                    d.insert(
                        "resolution".to_string(),
                        Value::from(if mcands.is_some() {
                            "ambiguous"
                        } else {
                            "not-found"
                        }),
                    );
                    if let Some(c) = mcands {
                        d.insert(
                            "candidates".to_string(),
                            Value::from(c.into_iter().map(Value::Object).collect::<Vec<_>>()),
                        );
                    }
                    if dry_run {
                        d.insert("via".to_string(), Value::from("none"));
                    }
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                let entry = entry.unwrap();
                // Gap-1 freshness gate (before text validation and the dry_run
                // early-return: identity outranks arguments, and a stale ref
                // refuses under dry_run too).
                if let Some(se) = check_expect_fp(ad, &entry, &py_str(ad.get("element").unwrap_or(&Value::Null))) {
                    d.insert("ok".to_string(), Value::from(false));
                    d.insert("error".to_string(), Value::from(se));
                    d.insert("resolution".to_string(), Value::from("stale"));
                    d.insert("via".to_string(), Value::from("none"));
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                // Text is required; present values coerce via py_str like the
                // `type` action (numbers become their string form). Missing or
                // null is an argument failure, never a silent clear.
                let text: Option<String> = match ad.get("text") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(py_str(v)),
                };
                let text = match text {
                    Some(t) => t,
                    None => {
                        d.insert("element".to_string(), Value::from(element_pub_full(&entry)));
                        d.insert("resolution".to_string(), Value::from("found"));
                        d.insert("via".to_string(), Value::from("none"));
                        d.insert(
                            "error".to_string(),
                            Value::from(
                                "set_value needs \"text\" (e.g. {\"type\":\"set_value\",\"element\":\"name:OK\",\"text\":\"hello\"})",
                            ),
                        );
                        d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                        return Ok(Some(d));
                    }
                };
                let value_len = text.chars().count() as i64;
                if dry_run {
                    d.insert("ok".to_string(), Value::from(true));
                    d.insert("resolution".to_string(), Value::from("found"));
                    d.insert("element".to_string(), Value::from(element_pub_full(&entry)));
                    d.insert("via".to_string(), Value::from("none"));
                    d.insert("value_len".to_string(), Value::from(value_len));
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                d.insert("element".to_string(), Value::from(element_pub_full(&entry)));
                d.insert("resolution".to_string(), Value::from("found"));
                if model.backend != "uia" {
                    d.insert("via".to_string(), Value::from("none"));
                    d.insert(
                        "error".to_string(),
                        Value::from(
                            "element has no ValuePattern under the Win32 backend (set_value needs the UIA backend)",
                        ),
                    );
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                let (ok_v, via, verr) =
                    crate::uia::uia_set_value(tgt, &entry_str(&entry, "path"), &text);
                d.insert(
                    "via".to_string(),
                    Value::from(if via.is_empty() {
                        "none".to_string()
                    } else {
                        via
                    }),
                );
                d.insert("ok".to_string(), Value::from(ok_v));
                if ok_v {
                    d.insert("value_len".to_string(), Value::from(value_len));
                }
                if let Some(e) = verr {
                    if !ok_v {
                        d.insert("error".to_string(), Value::from(e));
                    }
                }
                d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                return Ok(Some(d));
            }
            if typ == "wait" {
                let (_, ferr) = st.flush();
                if let Some(fe) = ferr {
                    fail_since_flush(&mut st.results, &mut st.marked, &fe, st.flush_uipi);
                } else {
                    st.marked.clear();
                }
                let raw = ad.get("ms").cloned().unwrap_or(Value::from(0));
                let mut wait_n = py_int(&raw)
                    .map_err(|_| format!("wait ms must be an int (got {})", py_repr_value(&raw)))?;
                if wait_n < 0 {
                    wait_n = 0;
                } else if wait_n > 60000 {
                    return Err(format!("wait ms absurd (>60000): {wait_n}"));
                }
                std::thread::sleep(Duration::from_millis(wait_n as u64));
                return Ok(Some(base_entry(i, &typ_v, true, elapsed_ms(&t0))));
            }
            if typ == "wait_for" {
                let (_, ferr) = st.flush();
                if let Some(fe) = ferr {
                    fail_since_flush(&mut st.results, &mut st.marked, &fe, st.flush_uipi);
                } else {
                    st.marked.clear();
                }
                let wr = wait_for(ad, &t0);
                let mut d = Map::new();
                d.insert("index".to_string(), Value::from(i as i64));
                d.insert("type".to_string(), typ_v.clone());
                d.insert("ok".to_string(), wr["ok"].clone());
                d.insert("satisfied".to_string(), wr["satisfied"].clone());
                d.insert("waited_ms".to_string(), wr["waited_ms"].clone());
                d.insert("ms".to_string(), wr["waited_ms"].clone());
                if let Some(e) = wr.get("error") {
                    d.insert("error".to_string(), e.clone());
                }
                return Ok(Some(d));
            }
            if typ == "focus_window" {
                let (_, ferr) = st.flush();
                if let Some(fe) = ferr {
                    fail_since_flush(&mut st.results, &mut st.marked, &fe, st.flush_uipi);
                } else {
                    st.marked.clear();
                }
                let (hr, err, cands) = resolve_window_val(ad.get("title"), ad.get("hwnd"));
                if hr.is_none() {
                    let mut d = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    d.insert("error".to_string(), Value::from(err.unwrap()));
                    if let Some(c) = cands {
                        d.insert(
                            "candidates".to_string(),
                            Value::from(c.into_iter().map(Value::Object).collect::<Vec<_>>()),
                        );
                    }
                    return Ok(Some(d));
                }
                let r = focus_hwmd(hr.unwrap());
                let mut d = base_entry(
                    i,
                    &typ_v,
                    r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
                    elapsed_ms(&t0),
                );
                if let Some(Value::String(e)) = r.get("error") {
                    d.insert("error".to_string(), Value::from(e.clone()));
                }
                return Ok(Some(d));
            }
            let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
            m.insert(
                "error".to_string(),
                Value::from(format!("unknown action type \"{}\"", py_str(&typ_v))),
            );
            Ok(Some(m))
        })();

        match step {
            Ok(Some(m)) => st.results.push(m),
            Ok(None) => {}
            Err(e) => {
                let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                m.insert("error".to_string(), Value::from(e));
                st.results.push(m);
            }
        }
    }

    let (_, ferr) = st.flush();
    if let Some(fe) = ferr {
        fail_since_flush(&mut st.results, &mut st.marked, &fe, st.flush_uipi);
    } else {
        st.marked.clear();
    }
    if settle_n != 0 {
        std::thread::sleep(Duration::from_millis(settle_n as u64));
    }
    let total_ms = t_all.elapsed().as_secs_f64() * 1000.0;
    let mut focus_after = fg_now();
    for r in st.results.iter_mut() {
        if !r.contains_key("mode") {
            r.insert(
                "mode".to_string(),
                Value::from(if bg_mode { "background" } else { "foreground" }),
            );
        }
        if bg_mode && !r.contains_key("bg_hwnd") {
            r.insert(
                "bg_hwnd".to_string(),
                bg_hwnd.map(Value::from).unwrap_or(Value::Null),
            );
        }
    }
    let mut focus_restored = true;
    if focus_guard && focus_after != focus_before && focus_before != 0 {
        focus_restored = restore_foreground(focus_before);
        focus_after = fg_now();
    }
    // Hidden-desktop finalization: capture + readback run on the hidden-desktop
    // thread, then the desktop is torn down and the proof recorded.
    let mut hidden_meta: Option<Map<String, Value>> = None;
    if let Some(owner) = hidden_owner.take() {
        let mut hd = Map::new();
        hd.insert("requested".to_string(), Value::from("hidden"));
        hd.insert("desktop_name".to_string(), Value::from(hidden_name.clone()));
        hd.insert("desktop_created".to_string(), Value::from(hidden_created));
        hd.insert(
            "set_thread_desktop".to_string(),
            Value::from(hidden_set_desktop),
        );
        hd.insert(
            "launched_pid".to_string(),
            hidden_pid
                .map(|p| Value::from(p as i64))
                .unwrap_or(Value::Null),
        );
        hd.insert(
            "window_hwnd".to_string(),
            hidden_hwnd.map(Value::from).unwrap_or(Value::Null),
        );
        hd.insert(
            "window_class".to_string(),
            Value::from(hidden_class.clone()),
        );
        hd.insert(
            "window_title".to_string(),
            Value::from(hidden_title.clone()),
        );
        hd.insert("window_pid".to_string(), Value::from(hidden_win_pid as i64));
        hd.insert(
            "window_on_hidden_desktop".to_string(),
            Value::from(hidden_hwnd.is_some()),
        );
        let owner_absent = match hidden_hwnd {
            Some(h) => !crate::enum_windows().iter().any(|w| *w as i64 == h),
            None => false,
        };
        hd.insert(
            "absent_from_owner_enum_windows".to_string(),
            Value::from(owner_absent),
        );
        let done = owner.teardown();
        let cursor_after = cursor_now();
        let fg_end = fg_now();
        hd.insert("window_alive".to_string(), Value::from(done.alive));
        hd.insert(
            "text_readback".to_string(),
            done.text.clone().map(Value::from).unwrap_or(Value::Null),
        );
        match done.capture.as_ref() {
            Some(c) => {
                let mut cap = Map::new();
                cap.insert("ok".to_string(), Value::from(c.ok));
                cap.insert("print_flags".to_string(), Value::from(c.flags as i64));
                cap.insert("w".to_string(), Value::from(c.w as i64));
                cap.insert("h".to_string(), Value::from(c.h as i64));
                cap.insert("min".to_string(), Value::from(c.min as i64));
                cap.insert("max".to_string(), Value::from(c.max as i64));
                cap.insert("uniform".to_string(), Value::from(c.min == c.max));
                if let Some(png) = c.png.as_ref() {
                    cap.insert("bytes".to_string(), Value::from(png.len() as i64));
                    cap.insert(
                        "png_b64".to_string(),
                        Value::from(base64::engine::general_purpose::STANDARD.encode(png)),
                    );
                }
                if let Some(e) = c.error.as_ref() {
                    cap.insert("error".to_string(), Value::from(e.clone()));
                }
                hd.insert("capture".to_string(), Value::Object(cap));
            }
            None => {
                hd.insert("capture".to_string(), Value::Null);
            }
        }
        let mut td = Map::new();
        td.insert("close_posted".to_string(), Value::from(done.close_posted));
        td.insert(
            "process_exited".to_string(),
            Value::from(done.process_exited),
        );
        td.insert("terminated".to_string(), Value::from(done.terminated));
        td.insert("desktop_empty".to_string(), Value::from(done.desktop_empty));
        td.insert(
            "set_thread_desktop_restored".to_string(),
            Value::from(done.restored_desktop),
        );
        td.insert(
            "close_desktop".to_string(),
            Value::from(done.closed_desktop),
        );
        td.insert(
            "open_after_close_null".to_string(),
            Value::from(done.open_after_close_null),
        );
        td.insert("proven".to_string(), Value::from(done.teardown_proven));
        if let Some(e) = done.error {
            td.insert("error".to_string(), Value::from(e));
        }
        hd.insert("teardown".to_string(), Value::Object(td));
        hd.insert(
            "owner_cursor_delta".to_string(),
            Value::from(if cursor_after == cursor_before { 0 } else { 1 }),
        );
        hd.insert(
            "cursor_before".to_string(),
            Value::from(vec![
                Value::from(cursor_before.0),
                Value::from(cursor_before.1),
            ]),
        );
        hd.insert(
            "cursor_after".to_string(),
            Value::from(vec![
                Value::from(cursor_after.0),
                Value::from(cursor_after.1),
            ]),
        );
        hd.insert(
            "owner_foreground_delta".to_string(),
            Value::from(if fg_end == focus_before { 0 } else { 1 }),
        );
        hd.insert("foreground_before".to_string(), Value::from(focus_before));
        hd.insert("foreground_after".to_string(), Value::from(fg_end));
        hidden_meta = Some(hd);
    }
    let mut payload = Map::new();
    payload.insert(
        "results".to_string(),
        Value::from(
            st.results
                .into_iter()
                .map(Value::Object)
                .collect::<Vec<_>>(),
        ),
    );
    payload.insert("total_ms".to_string(), Value::from(py_round_n(total_ms, 3)));
    payload.insert("sendinput_calls".to_string(), Value::from(st.calls));
    payload.insert(
        "sendinput_ms".to_string(),
        Value::from(py_round_n(st.sendinput_ms, 3)),
    );
    payload.insert("events".to_string(), Value::from(st.events));
    payload.insert("focus_before".to_string(), Value::from(focus_before));
    payload.insert("focus_after".to_string(), Value::from(focus_after));
    if focus_guard {
        payload.insert("focus_restored".to_string(), Value::from(focus_restored));
    }
    if bg_mode {
        payload.insert(
            "background_note".to_string(),
            Value::from(format!(
                "{BG_NOTE} The target window was never activated, focused, or raised."
            )),
        );
        payload.insert("posted_messages".to_string(), Value::from(posted_total));
        if let Some(be) = bg_error {
            payload.insert("background_error".to_string(), Value::from(be));
        }
    }
    if let Some(hd) = hidden_meta {
        payload.insert("hidden_desktop".to_string(), Value::Object(hd));
    }
    wrapped_text(&crate::py_dumps(&Value::Object(payload)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn burst_first(actions: &[Value]) -> Map<String, Value> {
        let out = do_burst(actions, None, false, 0, true, None);
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let inner = outer
            .get("structuredContent")
            .and_then(|s| s.get("result"))
            .and_then(|r| r.as_str())
            .expect("structured result");
        let payload: Value = serde_json::from_str(inner).expect("payload");
        let results = payload
            .get("results")
            .and_then(|r| r.as_array())
            .expect("results");
        results
            .first()
            .and_then(|r| r.as_object())
            .expect("first result")
            .clone()
    }

    #[test]
    fn click_count_bound_rejected_in_dry_run() {
        let actions = vec![serde_json::json!({"type": "click", "count": 101})];
        let first = burst_first(&actions);
        assert_eq!(first.get("ok"), Some(&Value::from(false)));
        let err = first.get("error").and_then(|e| e.as_str()).unwrap_or("");
        assert!(
            err.contains("absurd"),
            "count error should name the bound, got: {err}"
        );
    }

    #[test]
    fn click_count_sane_passes_dry_run() {
        let actions = vec![serde_json::json!({"type": "click", "count": 2})];
        let first = burst_first(&actions);
        assert_eq!(first.get("ok"), Some(&Value::from(true)));
    }

    #[test]
    fn bg_click_count_bound_rejected_before_post() {
        let mut a = Map::new();
        a.insert("type".to_string(), Value::from("click"));
        a.insert("x".to_string(), Value::from(1));
        a.insert("y".to_string(), Value::from(1));
        a.insert("count".to_string(), Value::from(1_000_000_000_000i64));
        let (ok, err, _) = bg_execute(0, &a);
        assert!(!ok);
        assert!(
            err.unwrap_or_default().contains("absurd"),
            "bg count error should name the bound"
        );
    }

    #[test]
    fn stable_huge_region_refused_before_grab() {
        let mut a = Map::new();
        a.insert("kind".to_string(), Value::from("stable"));
        a.insert(
            "region".to_string(),
            Value::from(vec![
                Value::from(0),
                Value::from(0),
                Value::from(100000),
                Value::from(100000),
            ]),
        );
        a.insert("timeout_ms".to_string(), Value::from(1000));
        let t0 = Instant::now();
        let r = wait_for(&a, &t0);
        assert_eq!(r.get("ok"), Some(&Value::from(false)));
        let err = r.get("error").and_then(|e| e.as_str()).unwrap_or("");
        assert!(
            err.contains("region must"),
            "huge region should hit the region guard, got: {err}"
        );
    }

    #[test]
    fn stable_empty_region_still_rejected() {
        let mut a = Map::new();
        a.insert("kind".to_string(), Value::from("stable"));
        a.insert(
            "region".to_string(),
            Value::from(vec![
                Value::from(10),
                Value::from(10),
                Value::from(5),
                Value::from(5),
            ]),
        );
        a.insert("timeout_ms".to_string(), Value::from(0));
        let t0 = Instant::now();
        let r = wait_for(&a, &t0);
        assert_eq!(r.get("ok"), Some(&Value::from(false)));
    }

    #[test]
    fn overflow_string_rejected_with_shared_error() {
        let v = serde_json::json!("9223372036854775808"); // i64::MAX + 1
        assert!(crate::is_int_overflow(&v));
        let err = py_int(&v).expect_err("MAX+1 must be rejected, never clamped");
        assert_eq!(err, crate::int_overflow_err(&v));
    }

    #[test]
    fn overflow_u64_rejected_with_shared_error() {
        let v = Value::from(u64::MAX);
        assert!(crate::is_int_overflow(&v));
        let err = py_int(&v).expect_err("u64::MAX must be rejected, never clamped");
        assert_eq!(err, crate::int_overflow_err(&v));
    }

    #[test]
    fn overflow_float_rejected_with_shared_error() {
        let v = serde_json::json!(1e19);
        assert!(crate::is_int_overflow(&v));
        let err = py_int(&v).expect_err("1e19 must be rejected, never clamped");
        assert_eq!(err, crate::int_overflow_err(&v));
    }

    #[test]
    fn in_range_values_still_accepted() {
        assert_eq!(py_int(&serde_json::json!(42)).unwrap(), 42);
        assert_eq!(
            py_int(&serde_json::json!("-9223372036854775808")).unwrap(),
            i64::MIN
        );
    }

    fn ref_entry(win: i64, path: &str, name: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("win".to_string(), Value::from(win));
        m.insert("role".to_string(), Value::from("Button"));
        m.insert("name".to_string(), Value::from(name));
        m.insert("auto_id".to_string(), Value::from(""));
        m.insert(
            "rect".to_string(),
            Value::from(vec![
                Value::from(0),
                Value::from(0),
                Value::from(10),
                Value::from(10),
            ]),
        );
        m.insert("invokable".to_string(), Value::from(true));
        m.insert("path".to_string(), Value::from(path));
        m.insert("ref".to_string(), Value::from(format!("{win}.{path}")));
        m
    }

    #[test]
    fn ref_selector_rejected_names_accepted_forms() {
        // The `ref:` invoke selector was removed (4.2.1): a ref token pins
        // only window + path and cannot promise element identity, so any
        // `ref:<x>` element string is rejected with the existing unknown-kind
        // error naming the accepted forms. `ref` remains a reporting field
        // on element entries (see element_public), never a selector.
        let entries = vec![
            ref_entry(199210, "0", "Cancel"),
            ref_entry(199210, "0.3.1", "OK"),
        ];
        for bad in ["ref:199210.0.3.1", "ref:199210.0.9", "ref:abc", "ref:"] {
            let (hit, err, _) = match_element(&entries, Some(&Value::from(bad)), None);
            assert!(hit.is_none(), "{bad} must not resolve");
            let e = err.expect("ref selector must be rejected, never silent");
            assert!(
                e.contains("unknown element selector kind"),
                "ref selector needs the unknown-kind shape, got: {e}"
            );
            assert!(
                e.contains("(want name:|auto_id:|path:)"),
                "rejection must name the accepted forms, got: {e}"
            );
            assert!(
                !e.contains("ref:"),
                "rejection must not advertise a ref: selector, got: {e}"
            );
        }
    }

    // ---------------------------------------------------------------- set_value (4.3.0)

    fn burst_bg(actions: &[Value], bg: &str, dry_run: bool) -> (String, Map<String, Value>) {
        let out = do_burst(actions, Some(bg.to_string()), false, 0, dry_run, None);
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let inner = outer
            .get("structuredContent")
            .and_then(|s| s.get("result"))
            .and_then(|r| r.as_str())
            .expect("structured result");
        let payload: Value = serde_json::from_str(inner).expect("payload");
        let first = payload
            .get("results")
            .and_then(|r| r.as_array())
            .expect("results")[0]
            .as_object()
            .expect("first")
            .clone();
        (out, first)
    }

    fn fixture_bg() -> (crate::fixture::Fixture, String) {
        let f = crate::fixture::Fixture::new();
        let bg = format!("hwnd:{}", f.hwnd_i64());
        (f, bg)
    }

    #[test]
    fn set_value_missing_element_rejected() {
        let (fx, bg) = fixture_bg();
        let _ = fx.hwnd();
        let actions = vec![serde_json::json!({"type": "set_value", "text": "hi"})];
        let (_, first) = burst_bg(&actions, &bg, true);
        assert_eq!(first.get("ok"), Some(&Value::from(false)));
        let e = first.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            e.contains("set_value needs \"element\""),
            "missing element must name set_value, got: {e}"
        );
        assert_eq!(first.get("resolution"), Some(&Value::from("not-found")));
    }

    #[test]
    fn set_value_missing_text_rejected() {
        let (fx, bg) = fixture_bg();
        let _ = fx.hwnd();
        // Resolve an existing control first (OK button by name), but omit text.
        let actions = vec![serde_json::json!({"type": "set_value", "element": "name:OK"})];
        let (_, first) = burst_bg(&actions, &bg, true);
        assert_eq!(first.get("ok"), Some(&Value::from(false)));
        let e = first.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            e.contains("set_value needs \"text\""),
            "missing text must be typed, got: {e}"
        );
    }

    #[test]
    fn set_value_ref_rejected_like_invoke() {
        // set_value resolves through the same match_element, so ref: stays
        // rejected with the unknown-kind shape naming the accepted forms.
        let (fx, bg) = fixture_bg();
        let _ = fx.hwnd();
        let actions =
            vec![serde_json::json!({"type": "set_value", "element": "ref:123.0", "text": "hi"})];
        let (_, first) = burst_bg(&actions, &bg, true);
        assert_eq!(first.get("ok"), Some(&Value::from(false)));
        let e = first.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            e.contains("unknown element selector kind"),
            "ref: must be unknown-kind, got: {e}"
        );
        assert!(
            e.contains("(want name:|auto_id:|path:)"),
            "must name accepted forms, got: {e}"
        );
    }

    #[test]
    fn set_value_not_found_typed() {
        let (fx, bg) = fixture_bg();
        let _ = fx.hwnd();
        let actions = vec![
            serde_json::json!({"type": "set_value", "element": "name:NoSuchControlXYZ123", "text": "hi"}),
        ];
        let (_, first) = burst_bg(&actions, &bg, true);
        assert_eq!(first.get("ok"), Some(&Value::from(false)));
        let e = first.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            e.contains("no element matching"),
            "not-found must be typed, got: {e}"
        );
        assert_eq!(first.get("resolution"), Some(&Value::from("not-found")));
    }

    #[test]
    fn set_value_button_has_no_valuepattern() {
        // Live (not dry_run): the OK button exposes Invoke, not ValuePattern.
        let (fx, bg) = fixture_bg();
        let _ = fx.hwnd();
        // Stabilize the UIA backend first (poll own fixture only): the first
        // enumeration right after ShowWindow can race the provider and fall
        // back to win32, which would give via:none instead of uia-set-value.
        wait_uia_stable(fx.hwnd());
        let actions =
            vec![serde_json::json!({"type": "set_value", "element": "name:OK", "text": "hi"})];
        let (_, first) = burst_bg(&actions, &bg, false);
        assert_eq!(first.get("ok"), Some(&Value::from(false)));
        let e = first.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            e.contains("has no ValuePattern"),
            "button must fail with no-ValuePattern, got: {e}"
        );
        assert_eq!(first.get("via"), Some(&Value::from("uia-set-value")));
    }

    fn wait_uia_stable(hwnd: HWND) {
        // Poll only the fixture HWND until the UIA backend serves Edit
        // controls (or 5s elapses and the caller asserts on the real state).
        let t0 = Instant::now();
        loop {
            let m = element_model(hwnd);
            let has_edit = m
                .entries
                .iter()
                .any(|e| e.get("role").and_then(|v| v.as_str()) == Some("Edit"));
            if m.backend == "uia" && has_edit {
                return;
            }
            if t0.elapsed() > std::time::Duration::from_secs(5) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn edit_paths() -> (crate::fixture::Fixture, String, String, String) {
        // Returns (fixture, bg, writable_path, readonly_path) by enumerating
        // the live model and splitting the two Edit roles by screen-y.
        let f = crate::fixture::Fixture::new();
        let bg = format!("hwnd:{}", f.hwnd_i64());
        wait_uia_stable(f.hwnd());
        let model = element_model(f.hwnd());
        let mut edits: Vec<(i64, String)> = Vec::new();
        for e in &model.entries {
            if e.get("role").and_then(|v| v.as_str()).unwrap_or("") == "Edit" {
                let top = e
                    .get("rect")
                    .and_then(|r| r.as_array())
                    .and_then(|a| a.get(1))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let path = e
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                edits.push((top, path));
            }
        }
        edits.sort();
        assert!(
            edits.len() >= 2,
            "fixture must expose 2 Edit controls via UIA, got {}",
            edits.len()
        );
        let w = edits[0].1.clone();
        let r = edits[1].1.clone();
        (f, bg, w, r)
    }

    #[test]
    fn set_value_readonly_typed() {
        let (fx, bg, _w, ro) = edit_paths();
        let _ = fx.hwnd();
        let actions = vec![
            serde_json::json!({"type": "set_value", "element": format!("path:{ro}"), "text": "hi"}),
        ];
        let (_, first) = burst_bg(&actions, &bg, false);
        assert_eq!(first.get("ok"), Some(&Value::from(false)));
        let e = first.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            e.contains("read-only"),
            "readonly EDIT must fail typed, got: {e}"
        );
    }

    #[test]
    fn set_value_success_writes_and_never_echoes() {
        // Environment-independent foreground contract: record the foreground
        // window BEFORE any fixture work; the tool must not CHANGE it. When
        // the environment provides no foreground session (headless CI), the
        // check skips with a message instead of asserting.
        let fg_before = crate::fixture::foreground_now();
        let (fx, bg, wpath, _ro) = edit_paths();
        let secret = "fixture-secret-4821";
        let actions = vec![
            serde_json::json!({"type": "set_value", "element": format!("path:{wpath}"), "text": secret}),
        ];
        let (out, first) = burst_bg(&actions, &bg, false);
        assert_eq!(
            first.get("ok"),
            Some(&Value::from(true)),
            "set_value must succeed, got: {first:?}"
        );
        assert_eq!(first.get("via"), Some(&Value::from("uia-set-value")));
        assert_eq!(
            first.get("value_len"),
            Some(&Value::from(secret.chars().count() as i64))
        );
        assert!(
            !out.contains(secret),
            "result must never echo the text value"
        );
        // Read-back proves the value landed in the EDIT control.
        assert_eq!(fx.edit_text(), secret);
        println!("SET_VALUE_READBACK={}", fx.edit_text());
        // Foreground observability note (informational only, never a verdict):
        // the live `SetValue` below hands control to the UIA provider, which
        // briefly focuses the EDIT itself (measured 25 ms to 5 s+, OS-timed,
        // self-restoring) -- so no stable foreground assertion exists on the
        // live success path. The "no activation" gate lives in
        // `set_value_dry_run_leaves_foreground_clean`, which runs all of
        // puppet's own set_value code without the provider call.
        println!(
            "FG_INFO: set_value: before={fg_before} after={} fixture={}",
            fg_now(),
            fx.hwnd_i64()
        );
    }

    /// Puppet-side "no activation" gate: a dry_run resolution runs all of
    /// puppet's own set_value code (background-target resolution, element
    /// match) without the UIA provider `SetValue` call that briefly focuses
    /// the EDIT on live runs -- so, unlike the live success path (see
    /// `set_value_success_writes_and_never_echoes`), the foreground contract
    /// IS stable here: skip with FG_SKIP when the environment provides no
    /// foreground session; owned-foreground after settle is reported
    /// (FG_OWN_CHURN), never failed — a dry run performs no input.
    #[test]
    fn set_value_dry_run_leaves_foreground_clean() {
        let fg_before = crate::fixture::foreground_now();
        let (fx, bg, wpath, _ro) = edit_paths();
        let _ = fx.hwnd();
        let actions = vec![
            serde_json::json!({"type": "set_value", "element": format!("path:{wpath}"), "text": "no-focus-probe"}),
        ];
        let (_, first) = burst_bg(&actions, &bg, true);
        assert_eq!(
            first.get("ok"),
            Some(&Value::from(true)),
            "dry_run set_value must resolve, got: {first:?}"
        );
        assert_eq!(
            first.get("resolution"),
            Some(&Value::from("found")),
            "dry_run set_value must report found, got: {first:?}"
        );
        crate::fixture::check_foreground_left_clean(fx.hwnd_i64(), fg_before, "set_value:dry_run");
    }

    /// Gap-1 proof (fails before the fix, passes after): a fingerprint from
    /// an older snapshot must not silently address a different control.
    /// Resolve the live OK button, then invoke it with a WRONG expect_fp:
    /// the burst must refuse with a typed stale_ref (plus fresh-snapshot
    /// recovery advice), never act.
    #[test]
    fn invoke_stale_fp_refused_with_typed_error() {
        let (fx, bg, _w, _ro) = edit_paths();
        let _ = fx.hwnd();
        let actions = vec![
            serde_json::json!({"type": "invoke", "element": "name:OK", "expect_fp": "definitely-wrong-fingerprint"}),
        ];
        let (_, first) = burst_bg(&actions, &bg, true);
        assert_eq!(
            first.get("ok"),
            Some(&Value::from(false)),
            "stale fp must refuse, got: {first:?}"
        );
        let e = first.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            e.contains("stale_ref"),
            "refusal must be typed stale_ref, got: {e}"
        );
        assert!(
            e.contains("fresh"),
            "refusal must advise a fresh snapshot, got: {e}"
        );
        assert_eq!(first.get("resolution"), Some(&Value::from("stale")));
    }

    #[test]
    fn set_value_stale_fp_refused_with_typed_error() {
        // Same mismatch path for set_value: wrong expect_fp refuses before
        // any provider call, dry_run included.
        let (fx, bg, wpath, _ro) = edit_paths();
        let _ = fx.hwnd();
        let _ = wpath;
        let actions = vec![
            serde_json::json!({"type": "set_value", "element": "name:OK", "text": "hi", "expect_fp": "definitely-wrong-fingerprint"}),
        ];
        let (_, first) = burst_bg(&actions, &bg, true);
        assert_eq!(
            first.get("ok"),
            Some(&Value::from(false)),
            "stale fp must refuse, got: {first:?}"
        );
        let e = first.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            e.contains("stale_ref"),
            "refusal must be typed stale_ref, got: {e}"
        );
        assert_eq!(first.get("resolution"), Some(&Value::from("stale")));
    }

    #[test]
    fn invoke_matching_fp_resolves() {
        // Positive control for the fp gate: the CURRENT fingerprint from a
        // fresh walk must resolve (dry_run, no activation).
        let (fx, bg, _w, _ro) = edit_paths();
        let _ = fx.hwnd();
        let model = crate::shot::element_model(fx.hwnd());
        let entry = model
            .entries
            .iter()
            .find(|e| e.get("name").and_then(|v| v.as_str()) == Some("OK"))
            .expect("fixture must list the OK button");
        let fp = entry
            .get("fp")
            .and_then(|v| v.as_str())
            .expect("entry must carry fp");
        let actions =
            vec![serde_json::json!({"type": "invoke", "element": "name:OK", "expect_fp": fp})];
        let (_, first) = burst_bg(&actions, &bg, true);
        assert_eq!(
            first.get("ok"),
            Some(&Value::from(true)),
            "matching fp must resolve, got: {first:?}"
        );
        assert_eq!(first.get("resolution"), Some(&Value::from("found")));
    }

    /// Gap-2 proof: the detection logic under test. Blocked exactly when the
    /// target is determinably elevated and this process is not -- elevated
    /// self may inject, unknown targets fail open.
    #[test]
    fn uipi_predicate_matrix() {
        assert!(crate::uipi_blocked(Some(true), false));
        assert!(!crate::uipi_blocked(Some(true), true));
        assert!(!crate::uipi_blocked(Some(false), false));
        assert!(!crate::uipi_blocked(Some(false), true));
        assert!(!crate::uipi_blocked(None, false));
        assert!(!crate::uipi_blocked(None, true));
        println!("UIPI_PREDICATE_MATRIX_OK=blockediff-target-elevated-and-self-not");
    }

    /// Gap-2 proof: the refusal text names the elevated target and the
    /// recovery, pinned by essentials (never byte-exact). The live
    /// end-to-end (medium server vs elevated target) is unproducible on a
    /// box with EnableLUA=0, where every process token reads elevated;
    /// reproduce it on a UAC-on box by running the server unelevated and
    /// aiming a dry_run background action at an elevated window.
    #[test]
    fn uipi_refusal_text_names_target_and_recovery() {
        let e = format_uipi_refusal("Taskmgr.exe", 1234, 5678);
        assert!(
            e.starts_with("uipi_blocked:"),
            "refusal must be typed, got: {e}"
        );
        for bit in ["Taskmgr.exe", "1234", "5678", "elevated", "dry_run"] {
            assert!(e.contains(bit), "refusal must name {bit}, got: {e}");
        }
    }

    #[test]
    fn uipi_own_process_readable_and_own_window_unblocked() {
        // Live handles, deterministic on any box, elevated runner or not:
        // our own token is always readable, and our own (necessarily
        // same-integrity) fixture window is never a refusal.
        let me_pid = std::process::id();
        assert!(
            crate::process_elevated(me_pid).is_some(),
            "own process token must be readable"
        );
        let f = crate::fixture::Fixture::new();
        assert!(
            uipi_refusal(f.hwnd_i64()).is_none(),
            "same-integrity target must never refuse"
        );
    }

    #[test]
    fn uipi_background_dry_run_reports_normally_when_not_elevated() {
        // No-regression companion: a dry_run background action at a
        // non-elevated target still reports ok:true with no blocked_by_uipi
        // and posts nothing.
        let (fx, bg) = fixture_bg();
        let _ = fx.hwnd();
        let actions = vec![serde_json::json!({"type": "click", "x": 1, "y": 1})];
        let (_, first) = burst_bg(&actions, &bg, true);
        assert_eq!(
            first.get("ok"),
            Some(&Value::from(true)),
            "non-elevated dry_run must still report ok, got: {first:?}"
        );
        assert!(
            first.get("blocked_by_uipi").is_none(),
            "non-elevated target must carry no UIPI flag, got: {first:?}"
        );
    }

    #[test]
    fn set_value_reresolve_failure_typed() {
        // Direct UIA call with a bogus path exercises the shared resolver's
        // stale branch without touching the owner's desktop.
        let f = crate::fixture::Fixture::new();
        let (ok, via, err) = crate::uia::uia_set_value(f.hwnd_i64(), "9999", "x");
        assert!(!ok);
        let e = err.expect("stale path must be typed, never silent");
        assert!(
            e.contains("went stale") || e.contains("resolve failed"),
            "re-resolution failure must be typed, got: {e}"
        );
        assert!(
            via.is_empty() || via == "none",
            "resolve failure carries empty via, got: {via}"
        );
    }

    #[test]
    fn set_value_burst_continues_after_failure() {
        // Per-action ok:false never aborts the rest of the burst.
        let (fx, bg, wpath, _ro) = edit_paths();
        let _ = fx.hwnd();
        let actions = vec![
            serde_json::json!({"type": "set_value", "element": "name:NoSuchXYZ", "text": "x"}),
            serde_json::json!({"type": "set_value", "element": format!("path:{wpath}"), "text": "after-failure"}),
        ];
        let out = do_burst(&actions, Some(bg.to_string()), false, 0, false, None);
        let outer: Value = serde_json::from_str(&out).expect("outer");
        let inner = outer["structuredContent"]["result"]
            .as_str()
            .expect("inner");
        let payload: Value = serde_json::from_str(inner).expect("payload");
        let res = payload["results"].as_array().expect("results");
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].get("ok"), Some(&Value::from(false)));
        assert_eq!(res[1].get("ok"), Some(&Value::from(true)));
        assert_eq!(fx.edit_text(), "after-failure");
    }
}
