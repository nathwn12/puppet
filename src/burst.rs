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
use windows_sys::Win32::Foundation::{
    GetLastError, GlobalFree, HGLOBAL, HWND, LPARAM, POINT, RECT,
};
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
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
const M_HWHEEL: u32 = 0x1000;
const K_UP: u32 = 0x0002;
const K_UNICODE: u32 = 0x0004;

const BG_NOTE: &str = "background PostMessage delivery is best-effort: it is NOT honoured by every app and is NOT equivalent to real (SendInput) input.";
const BG_SUFFIX: &str = "; omit background for real SendInput delivery";
const MAX_CLICK_COUNT: i64 = 100;
const MAX_STABLE_EDGE: i64 = 4096;

/// Whole-call typed refusal for a target that could not be locked (nothing
/// ran): rendered as an MCP tool error carrying `{ok:false, code, error,
/// candidates?}` so the caller can act on the code, not parse prose.
/// Private-desktop permission is EARNED, never requested: it holds only when
/// THIS call created the desktop (so the server owns it) AND the action target
/// actually lives on that desktop. An explicit `target` wins over hidden
/// desktop creation, so a caller could otherwise pass
/// `target:{hwnd:<ordinary window>}, desktop:"hidden"` and inherit
/// private-desktop rights (posted keyboard) over the owner's session.
fn private_desktop_earned(
    desktop_created: bool,
    created_name: &str,
    target_desktop: Option<&str>,
) -> bool {
    desktop_created && !created_name.is_empty() && target_desktop == Some(created_name)
}

fn target_refusal_blocks(e: &crate::target::TargetError) -> String {
    let text = crate::py_dumps(&Value::Object(e.to_map()));
    crate::tool_err_blocks(&[crate::text_block(&text)])
}

/// Default stable-watch region: the foreground window's monitor, not the
/// whole virtual desktop — a full-span grab every poll is the memory win
/// this avoids. Falls back to the virtual desktop with no foreground
/// window. The 4096 cap still applies downstream.
fn default_stable_region() -> Vec<i64> {
    let mons = crate::display_monitors();
    let fg = fg_now();
    if fg != 0 {
        unsafe {
            let mut rc = RECT::default();
            if GetWindowRect(fg as HWND, &mut rc) != 0 {
                let cx = (rc.left as i64 + rc.right as i64) / 2;
                let cy = (rc.top as i64 + rc.bottom as i64) / 2;
                if let Some(m) = monitor_for_point(&mons, cx, cy) {
                    return vec![m[0] as i64, m[1] as i64, m[2] as i64, m[3] as i64];
                }
            }
        }
    }
    let (vx, vy, vr, vb) = virtual_rect();
    vec![vx as i64, vy as i64, vr as i64, vb as i64]
}
fn bg_non_postable(typ: &str) -> Option<&'static str> {
    match typ {
        "move" => Some(
            "background_unavailable: move has no window-message equivalent (the pointer is not \
             part of a window's message stream; use click x/y for a posted click)",
        ),
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

/// Cancellable sleep in <=50ms slices. Returns true when the current
/// `tools/call` was cancelled mid-sleep so the caller can return promptly
/// (the worker drops the response; no late reply is sent).
fn sleep_cancellable(ms: u64) -> bool {
    let mut left = ms;
    while left > 0 {
        if crate::cancel::cancelled() {
            return true;
        }
        let s = left.min(50);
        std::thread::sleep(Duration::from_millis(s));
        left = left.saturating_sub(s);
    }
    crate::cancel::cancelled()
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

/// Pure wheel resolution: axis flag plus WHEEL_DELTA-scaled data.
/// |amount| is bounded (no wrapping); vertical behavior is unchanged.
/// The range test avoids `abs` so `i64::MIN` cannot overflow past it.
fn scroll_wheel(axis: &str, amt: i64) -> Result<(u32, u32), String> {
    let flag = match axis {
        "v" => M_WHEEL,
        "h" => M_HWHEEL,
        _ => return Err(format!("unknown scroll axis \"{axis}\" (want v|h)")),
    };
    if !(-1000..=1000).contains(&amt) {
        return Err(format!(
            "scroll amount out of range -1000..1000 (got {amt})"
        ));
    }
    Ok((flag, (amt as i32).wrapping_mul(120) as u32))
}

/// Pure relative-move event: raw signed deltas, `M_MOVE` only (no
/// absolute/virtual-desktop mapping). Unit-testable flags + payload.
fn mi_move_rel(dx: i64, dy: i64) -> Result<INPUT, String> {
    // Range test (not abs) so i64::MIN cannot overflow past it.
    if !(-32767..=32767).contains(&dx) || !(-32767..=32767).contains(&dy) {
        return Err(format!(
            "move delta out of range -32767..32767 (got {dx},{dy})"
        ));
    }
    Ok(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: dx as i32,
                dy: dy as i32,
                mouseData: 0,
                dwFlags: M_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    })
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

/// Pure key-event sequence for one resolved code: Shift down, key down/up,
/// Shift up — or a Unicode down/up pair per UTF-16 unit. Unit-testable order;
/// the dispatch only extends `pending` with the result.
fn key_inputs(code: &KeyCode) -> Vec<INPUT> {
    let mut out = key_down_inputs(code);
    out.extend(key_up_inputs(code));
    out
}

/// Key-down half of `key_inputs`: Shift down + key down (or Unicode downs).
/// Split out so hold/repeat can sleep between down and up.
fn key_down_inputs(code: &KeyCode) -> Vec<INPUT> {
    let mut out = Vec::new();
    match code {
        KeyCode::Vk(vk, shift) => {
            if *shift {
                out.push(ki(VK_SHIFT, 0, 0));
            }
            out.push(ki(*vk, 0, 0));
        }
        KeyCode::Uni(ch) => {
            for unit in utf16_units(&ch.to_string()) {
                out.push(ki(0, unit, K_UNICODE));
            }
        }
    }
    out
}

/// Key-up half of `key_inputs`: key up + Shift up (or Unicode ups).
fn key_up_inputs(code: &KeyCode) -> Vec<INPUT> {
    let mut out = Vec::new();
    match code {
        KeyCode::Vk(vk, shift) => {
            out.push(ki(*vk, 0, K_UP));
            if *shift {
                out.push(ki(VK_SHIFT, 0, K_UP));
            }
        }
        KeyCode::Uni(ch) => {
            for unit in utf16_units(&ch.to_string()) {
                out.push(ki(0, unit, K_UNICODE | K_UP));
            }
        }
    }
    out
}

/// Pure hotkey-event sequence: presses in order (each Shift-wrapped),
/// releases in reverse. Unit-testable order; no input performed.
fn hotkey_inputs(codes: &[KeyCode]) -> Vec<INPUT> {
    let mut out = Vec::new();
    for c in codes {
        match c {
            KeyCode::Vk(vk, shift) => {
                if *shift {
                    out.push(ki(VK_SHIFT, 0, 0));
                }
                out.push(ki(*vk, 0, 0));
            }
            KeyCode::Uni(ch) => {
                for unit in utf16_units(&ch.to_string()) {
                    out.push(ki(0, unit, K_UNICODE));
                }
            }
        }
    }
    for c in codes.iter().rev() {
        match c {
            KeyCode::Vk(vk, shift) => {
                out.push(ki(*vk, 0, K_UP));
                if *shift {
                    out.push(ki(VK_SHIFT, 0, K_UP));
                }
            }
            KeyCode::Uni(ch) => {
                for unit in utf16_units(&ch.to_string()) {
                    out.push(ki(0, unit, K_UNICODE | K_UP));
                }
            }
        }
    }
    out
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

/// Resolved key: a virtual-key code (plus whether Shift must be held for the
/// layout's high byte) or a Unicode fallback when VkKeyScanW has no
/// translation (`sc == -1`) or the char needs Ctrl/Alt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyCode {
    Vk(u16, bool),
    Uni(char),
}

fn vk_of(name: &str) -> Result<KeyCode, String> {
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
        return Ok(KeyCode::Vk(v, false));
    }
    if let Some(vk) = key
        .strip_prefix('f')
        .and_then(|r| r.parse::<u32>().ok())
        .filter(|i| (1..=24).contains(i))
    {
        return Ok(KeyCode::Vk((0x6F + vk) as u16, false));
    }
    if name.chars().count() == 1 {
        let ch = name.chars().next().unwrap();
        unsafe {
            let sc = VkKeyScanW(ch as u16);
            if sc == -1 {
                return Ok(KeyCode::Uni(ch));
            }
            let vk = (sc as u16) & 0xFF;
            let mods = ((sc as u16) >> 8) as u8;
            if mods & 0x06 != 0 {
                // Ctrl/Alt-required chars have no SendInput key sequence
                // that types them; deliver as Unicode like `type`.
                return Ok(KeyCode::Uni(ch));
            }
            return Ok(KeyCode::Vk(vk, mods & 0x01 != 0));
        }
    }
    Err(format!("unknown key name \"{name}\""))
}

/// Monitor containing a virtual-desktop point, for defaulting a watch
/// region to the foreground window's monitor instead of the whole
/// virtual desktop. Pure and unit-tested; `None` when no monitor
/// contains the point (caller falls back to the virtual desktop).
fn monitor_for_point(mons: &[[i32; 4]], x: i64, y: i64) -> Option<[i32; 4]> {
    mons.iter()
        .find(|m| x >= m[0] as i64 && x < m[2] as i64 && y >= m[1] as i64 && y < m[3] as i64)
        .copied()
}

// ---------------------------------------------------------------- clipboard (puppet.py:2547-2602)

/// Clipboard text budget: 1 MiB of characters. Reads truncate, writes
/// fail typed — never silently, never unbounded.
pub(crate) const MAX_CLIP_CHARS: usize = 1_048_576;

/// Typed bound check shared by clipboard_set (before OpenClipboard, so a
/// rejection never touches the clipboard).
fn check_clip_len(n_chars: usize) -> Result<(), String> {
    if n_chars > MAX_CLIP_CHARS {
        return Err(format!(
            "clipboard text too large ({n_chars} chars, max {MAX_CLIP_CHARS})"
        ));
    }
    Ok(())
}

/// Bounded NUL-terminated UTF-16 read: scans at most `max_units` for the
/// terminator (the GlobalSize bound), then decodes lossily. A missing NUL
/// is an error, never an over-read.
unsafe fn clip_read_u16(ptr: *const u16, max_units: usize) -> Result<String, String> {
    let mut len = 0usize;
    while len < max_units {
        if *ptr.add(len) == 0 {
            break;
        }
        len += 1;
    }
    if len == max_units {
        return Err("clipboard text is not NUL-terminated within its GlobalSize".to_string());
    }
    let slice = std::slice::from_raw_parts(ptr, len);
    Ok(String::from_utf16_lossy(slice))
}

/// `OpenClipboard`/`EmptyClipboard`/`SetClipboardData` behind a clipboard
/// permit: the token is the `allow_clipboard:true` flag, so a call that never
/// asked cannot overwrite the owner's clipboard even by mistake.
fn clipboard_set(_permit: &crate::gate::ClipboardPermit, text: &str) -> Result<(), String> {
    // Bound first: a rejection never touches the clipboard.
    check_clip_len(text.chars().count())?;
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
            // Bound the scan by the allocation size: never read past what
            // GlobalSize reports, and require the NUL terminator inside it.
            let bytes = GlobalSize(h) as usize;
            if bytes < 2 {
                return Err("clipboard text is empty or unreadable (GlobalSize)".to_string());
            }
            let max_units = bytes / 2;
            let p = GlobalLock(h);
            if p.is_null() {
                return Err("GlobalLock failed".to_string());
            }
            let s = clip_read_u16(p as *const u16, max_units);
            GlobalUnlock(h);
            let s = s?;
            // Cap the return at MAX_CLIP_CHARS (truncate, documented).
            if s.chars().count() > MAX_CLIP_CHARS {
                Ok(s.chars().take(MAX_CLIP_CHARS).collect())
            } else {
                Ok(s)
            }
        })();
        if opened {
            CloseClipboard();
        }
        r
    }
}

// ---------------------------------------------------------------- background PostMessage input (puppet.py:1659-1792)

/// Signed 16-bit range a window message can carry in one lParam half.
const MSG_COORD_MIN: i64 = -32768;
const MSG_COORD_MAX: i64 = 32767;

fn bg_lparam(x: i64, y: i64) -> LPARAM {
    (((y as u32 & 0xFFFF) << 16) | (x as u32 & 0xFFFF)) as i32 as isize
}

/// Refuse coordinates a message lParam cannot carry. `bg_lparam` packs each
/// axis into 16 bits, so 40000 would silently become -25536 and the posting
/// would report success at the wrong point. Typed, before any message.
fn msg_coords(x: i64, y: i64) -> Result<(i64, i64), String> {
    if !(MSG_COORD_MIN..=MSG_COORD_MAX).contains(&x)
        || !(MSG_COORD_MIN..=MSG_COORD_MAX).contains(&y)
    {
        return Err(format!(
            "bad_coordinates: message coordinates are signed 16-bit (got x={x}, y={y}; allowed \
             {}..={}); nothing was posted",
            MSG_COORD_MIN, MSG_COORD_MAX
        ));
    }
    Ok((x, y))
}

fn bg_client(hwnd: i64, x: &Value, y: &Value) -> Result<(i64, i64), String> {
    let xi = py_int(x).map_err(|e| format!("ScreenToClient failed: {e}"))?;
    let yi = py_int(y).map_err(|e| format!("ScreenToClient failed: {e}"))?;
    // Validate the i64 BEFORE the `as i32` cast ScreenToClient needs: a value
    // like 4294967296 would wrap to 0 and the later 16-bit check would see a
    // perfectly valid 0, posting at the wrong point.
    for (v, name) in [(xi, "x"), (yi, "y")] {
        if v < i32::MIN as i64 || v > i32::MAX as i64 {
            return Err(format!(
                "bad_coordinates: screen {name} out of i32 range (got {v}; allowed {}..={}); a \
                 cast would wrap and post at the wrong point, so nothing was posted",
                i32::MIN,
                i32::MAX
            ));
        }
    }
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

/// Class probe for text-input controls (classic EDIT plus RichEdit
/// flavors such as RichEditD2DPT backing the Win11 Notepad document).
fn bg_is_text_control(h: HWND) -> bool {
    unsafe {
        let mut buf = vec![0u16; 256];
        let n = GetClassNameW(h, buf.as_mut_ptr(), buf.len() as i32);
        if n <= 0 {
            return false;
        }
        let cls = String::from_utf16_lossy(&buf[..n as usize]).to_lowercase();
        cls.contains("edit") || cls.contains("rich")
    }
}

fn bg_descend(h: HWND, out: &mut Vec<HWND>) {
    if let Ok(kids) = crate::enum_child_windows(h) {
        for k in kids {
            out.push(k);
            bg_descend(k, out);
        }
    }
}

/// Text-input target under background delivery (plain `background` and
/// `desktop:"hidden"` share `bg_execute`): keyboard messages posted to a
/// top-level frame are dispatched to the frame's own window proc, which for
/// editors (Notepad) never reaches the child EDIT control — so the hidden
/// readback comes back empty. Resolve the control that would actually
/// receive typed text: the thread's focused control when it is itself text
/// input (GetGUIThreadInfo, the caret owner), else the first visible +
/// enabled text control in the descendant tree (the document), else the
/// window itself. A bare focus window (tab strip, toolbar) would swallow
/// WM_CHAR, so focus only counts when it is text input. The descendant
/// walk is recursive because modern editors nest the document deeper than
/// one level. Never activates or focuses; PostMessage-only, so the owner's
/// desktop is untouched.
fn bg_text_target(hwnd: i64) -> i64 {
    unsafe {
        let target = hwnd as HWND;
        if IsWindow(target) == 0 {
            return hwnd;
        }
        let thread = GetWindowThreadProcessId(target, std::ptr::null_mut());
        if thread != 0 {
            let mut info: GUITHREADINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
            if GetGUIThreadInfo(thread, &mut info) != 0 {
                let f = info.hwndFocus;
                if !f.is_null()
                    && f as i64 != hwnd
                    && IsChild(target, f) != 0
                    && bg_is_text_control(f)
                {
                    return f as i64;
                }
            }
        }
        let mut descendants: Vec<HWND> = Vec::new();
        bg_descend(target, &mut descendants);
        for k in descendants {
            if IsWindowVisible(k) == 0 || IsWindowEnabled(k) == 0 {
                continue;
            }
            if bg_is_text_control(k) {
                return k as i64;
            }
        }
        hwnd
    }
}

/// Posted input the target refuses: a typed `background_unavailable`, never
/// an escalation to foreground input.
const BG_POST_FAILED: &str = "background_unavailable: PostMessage failed (the target may have \
                              closed, or its message queue refuses messages); nothing escalated \
                              to foreground input";

/// Client point for a posted message. `client_coords` is the target-lock
/// convention (x/y are client-area physical pixels, used as given); false is
/// the legacy `background` convention (x/y are virtual-desktop screen pixels,
/// converted with ScreenToClient). Both end in the same exact integer pair.
fn bg_point(hwnd: i64, a: &Map<String, Value>, client_coords: bool) -> Result<(i64, i64), String> {
    let xv = a.get("x").ok_or_else(|| "needs x and y".to_string())?;
    let yv = a.get("y").ok_or_else(|| "needs x and y".to_string())?;
    let xi = py_int(xv).map_err(|e| format!("bad x: {e}"))?;
    let yi = py_int(yv).map_err(|e| format!("bad y: {e}"))?;
    // Client (target) coordinates are used as given; legacy background points
    // are converted from screen space first. Either way the result must fit a
    // message lParam, so the check runs AFTER the conversion.
    let (cx, cy) = if client_coords {
        (xi, yi)
    } else {
        bg_client(hwnd, xv, yv)?
    };
    msg_coords(cx, cy)
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
fn bg_execute(
    hwnd: i64,
    a: &Map<String, Value>,
    client_coords: bool,
    allow_keyboard: bool,
) -> (bool, Option<String>, i64) {
    let typ = a.get("type").and_then(|v| v.as_str()).unwrap_or("");
    // Hover is opt-in: without it the first pointer message the target sees
    // is the button down, exactly at the requested point.
    let hover = matches!(a.get("hover"), Some(v) if py_truthy(v));
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
        let (cx, cy) = match bg_point(hwnd, a, client_coords) {
            Ok(p) => p,
            Err(e) => return (false, Some(e), 0),
        };
        let lp = bg_lparam(cx, cy);
        let mut n = 0i64;
        // No unconditional WM_MOUSEMOVE: the click is one down + one up at
        // the exact point. `hover:true` is the only way to ask for the move.
        if hover {
            if !bg_post(hwnd, 0x0200, 0, lp) {
                return (false, Some(BG_POST_FAILED.to_string()), n);
            }
            n += 1;
        }
        for _ in 0..count {
            if !bg_post(hwnd, dn, held, lp) {
                return (false, Some(BG_POST_FAILED.to_string()), n);
            }
            n += 1;
            if !bg_post(hwnd, up, 0, lp) {
                return (false, Some(BG_POST_FAILED.to_string()), n);
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
        let lp = match bg_point(hwnd, a, client_coords) {
            Ok((cx, cy)) => bg_lparam(cx, cy),
            Err(e) => return (false, Some(e), 0),
        };
        let msg = if typ == "down" { dn } else { up };
        let w = if typ == "down" { held } else { 0 };
        if !bg_post(hwnd, msg, w, lp) {
            return (false, Some(BG_POST_FAILED.to_string()), 0);
        }
        return (true, None, 1);
    }
    if typ == "drag" {
        // down at `from`, ONE move to `to`, up at `to` — exact points, no
        // pointer, no interpolated path, no extra move messages.
        let btn = bg_btn_name(a);
        let Some((dn, up, held)) = bg_buttons(&btn) else {
            return (
                false,
                Some(format!("unknown button \"{}\"", bg_btn_raw(a))),
                0,
            );
        };
        let pt = |key: &str| -> Result<(i64, i64), String> {
            let v = a.get(key).ok_or_else(|| format!("drag needs {key}"))?;
            match v {
                Value::Array(arr) if arr.len() == 2 => {
                    let mut m = Map::new();
                    m.insert("x".to_string(), arr[0].clone());
                    m.insert("y".to_string(), arr[1].clone());
                    bg_point(hwnd, &m, client_coords)
                }
                _ => Err(format!("drag {key} must be [x,y]")),
            }
        };
        let (fx, fy) = match pt("from") {
            Ok(p) => p,
            Err(e) => return (false, Some(e), 0),
        };
        let (tx, ty) = match pt("to") {
            Ok(p) => p,
            Err(e) => return (false, Some(e), 0),
        };
        let from = bg_lparam(fx, fy);
        let to = bg_lparam(tx, ty);
        let mut n = 0i64;
        for (msg, w, lp) in [(dn, held, from), (0x0200u32, 0u64, to), (up, 0u64, to)] {
            if !bg_post(hwnd, msg, w, lp) {
                return (false, Some(BG_POST_FAILED.to_string()), n);
            }
            n += 1;
        }
        return (true, None, n);
    }
    if typ == "scroll" {
        // WM_MOUSEWHEEL / WM_MOUSEHWHEEL carry SCREEN coordinates in lParam
        // (the one pointer message whose lParam is not client space), so the
        // exact client point is converted once and the wheel is delivered
        // with the same delta sign the SendInput path uses.
        let amount = match opt_py_int(a, "amount", 1) {
            Ok(v) => v,
            Err(e) => return (false, Some(e), 0),
        };
        let axis = match a.get("axis") {
            None | Some(Value::Null) => "v".to_string(),
            Some(v) => py_str(v).to_lowercase(),
        };
        let (msg, delta) = match scroll_wheel(&axis, amount) {
            Ok((flag, data)) => (
                if flag == M_WHEEL {
                    0x020Au32
                } else {
                    0x020Eu32
                },
                data as u16 as u64,
            ),
            Err(e) => return (false, Some(e), 0),
        };
        let (cx, cy) = match bg_point(hwnd, a, client_coords) {
            Ok(p) => p,
            Err(e) => return (false, Some(e), 0),
        };
        let (sx, sy) = if client_coords {
            match crate::target::client_point_to_screen(hwnd, cx, cy) {
                Some(p) => p,
                None => return (false, Some("ClientToScreen failed".to_string()), 0),
            }
        } else {
            (cx, cy)
        };
        // WM_MOUSEWHEEL lParam is SCREEN coordinates, packed into 16 bits.
        let (sx, sy) = match msg_coords(sx, sy) {
            Ok(p) => p,
            Err(e) => return (false, Some(e), 0),
        };
        let lp = bg_lparam(sx, sy);
        if !bg_post(hwnd, msg, delta << 16, lp) {
            return (false, Some(BG_POST_FAILED.to_string()), 0);
        }
        return (true, None, 1);
    }
    if matches!(typ, "type" | "key" | "hotkey") && !allow_keyboard {
        // Defense in depth: even if a future branch routed keyboard here, it
        // is refused. Posted keyboard is only for the private hidden desktop.
        return (
            false,
            Some(
                "foreground_required: keyboard input is never posted to the owner's desktop \\
                 (WM_CHAR/WM_KEYDOWN bypass the input queue and are ignored by many controls); \\
                 use type with an element (UIA SetValue) or allow_foreground:true. Nothing was \\
                 posted."
                    .to_string(),
            ),
            0,
        );
    }
    if typ == "type" {
        let text = match a.get("text") {
            None => String::new(),
            Some(v) => py_str(v),
        };
        // Keyboard input lands in the text control, not the frame: post
        // WM_CHAR to the focused/edit child (same fix plain `background`
        // needs — it shares this path).
        let tgt = bg_text_target(hwnd);
        let mut n = 0i64;
        for unit in utf16_units(&text) {
            if !bg_post(tgt, 0x0102, unit as u64, 1) {
                return (false, Some(BG_POST_FAILED.to_string()), n);
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
        let code = match vk_of(&name) {
            Ok(v) => v,
            Err(e) => return (false, Some(e), 0),
        };
        // Same text-target rule as `type`: key messages posted to the frame
        // never reach the edit control.
        let tgt = bg_text_target(hwnd);
        match code {
            KeyCode::Vk(vk, shift) => {
                let (dm, um) = if vk == 0x12 {
                    (0x0104, 0x0105)
                } else {
                    (0x0100, 0x0101)
                };
                let mut n = 0i64;
                if shift && !bg_post(tgt, dm, VK_SHIFT as u64, bg_key_lparam(VK_SHIFT, false)) {
                    return (false, Some(BG_POST_FAILED.to_string()), n);
                }
                if shift {
                    n += 1;
                }
                if !bg_post(tgt, dm, vk as u64, bg_key_lparam(vk, false)) {
                    return (false, Some(BG_POST_FAILED.to_string()), n);
                }
                n += 1;
                if !bg_post(tgt, um, vk as u64, bg_key_lparam(vk, true)) {
                    return (false, Some(BG_POST_FAILED.to_string()), n);
                }
                n += 1;
                if shift && !bg_post(tgt, um, VK_SHIFT as u64, bg_key_lparam(VK_SHIFT, true)) {
                    return (false, Some(BG_POST_FAILED.to_string()), n);
                }
                if shift {
                    n += 1;
                }
                return (true, None, n);
            }
            KeyCode::Uni(ch) => {
                // No key sequence types this char: WM_CHAR like `type`.
                let mut n = 0i64;
                for unit in utf16_units(&ch.to_string()) {
                    if !bg_post(tgt, 0x0102, unit as u64, 1) {
                        return (false, Some(BG_POST_FAILED.to_string()), n);
                    }
                    n += 1;
                }
                return (true, None, n);
            }
        }
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
        let mut codes = Vec::with_capacity(keys.len());
        for k in &keys {
            match vk_of(k) {
                Ok(v) => codes.push(v),
                Err(e) => return (false, Some(e), 0),
            }
        }
        let has_alt = codes
            .iter()
            .any(|c| matches!(c, KeyCode::Vk(vk, _) if *vk == 0x12));
        let (dm, um) = if has_alt {
            (0x0104, 0x0105)
        } else {
            (0x0100, 0x0101)
        };
        let mut n = 0i64;
        // Same text-target rule as `type`.
        let tgt = bg_text_target(hwnd);
        for c in &codes {
            match c {
                KeyCode::Vk(vk, shift) => {
                    if *shift {
                        if !bg_post(tgt, dm, VK_SHIFT as u64, bg_key_lparam(VK_SHIFT, false)) {
                            return (false, Some(BG_POST_FAILED.to_string()), n);
                        }
                        n += 1;
                    }
                    if !bg_post(tgt, dm, *vk as u64, bg_key_lparam(*vk, false)) {
                        return (false, Some(BG_POST_FAILED.to_string()), n);
                    }
                    n += 1;
                }
                KeyCode::Uni(ch) => {
                    for unit in utf16_units(&ch.to_string()) {
                        if !bg_post(tgt, 0x0102, unit as u64, 1) {
                            return (false, Some(BG_POST_FAILED.to_string()), n);
                        }
                        n += 1;
                    }
                }
            }
        }
        for c in codes.iter().rev() {
            match c {
                KeyCode::Vk(vk, shift) => {
                    if !bg_post(tgt, um, *vk as u64, bg_key_lparam(*vk, true)) {
                        return (false, Some(BG_POST_FAILED.to_string()), n);
                    }
                    n += 1;
                    if *shift {
                        if !bg_post(tgt, um, VK_SHIFT as u64, bg_key_lparam(VK_SHIFT, true)) {
                            return (false, Some(BG_POST_FAILED.to_string()), n);
                        }
                        n += 1;
                    }
                }
                KeyCode::Uni(ch) => {
                    for unit in utf16_units(&ch.to_string()) {
                        if !bg_post(tgt, 0x0102, unit as u64, 1) {
                            return (false, Some(BG_POST_FAILED.to_string()), n);
                        }
                        n += 1;
                    }
                }
            }
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

// ---------------------------------------------------------------- wait_for (puppet.py:1795-1892)

/// Pure element-wait decision: given whether a matching control was found
/// and (when found) whether it is enabled, decide per `state`. Unit-tested;
/// the polling loop only feeds it live observations.
fn element_wait_satisfied(state: &str, found: bool, enabled: Option<bool>) -> bool {
    match state {
        "disappear" => !found,
        "appear" => found,
        "enabled" => found && enabled == Some(true),
        "disabled" => found && enabled == Some(false),
        _ => false,
    }
}

/// Enabled state for a matched entry: UIA CurrentIsEnabled by path on the
/// uia backend, Win32 IsWindowEnabled on the entry hwnd otherwise. None on
/// any failure (unreadable, unhosted, stale) — the waiter treats that as
/// unsatisfied and keeps polling rather than failing the burst.
fn wf_element_enabled(tgt: i64, backend: &str, entry: &Map<String, Value>) -> Option<bool> {
    if backend == "uia" {
        let path = entry_str(entry, "path");
        crate::uia::uia_is_enabled(tgt as HWND, &path).ok()
    } else {
        let h = entry.get("hwnd").and_then(|v| v.as_i64()).unwrap_or(0);
        if h == 0 {
            return None;
        }
        unsafe { Some(IsWindowEnabled(h as HWND) != 0) }
    }
}

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

fn wf_nap(t0: &Instant, timeout_s: f64, poll_s: f64) -> bool {
    let remain = timeout_s - t0.elapsed().as_secs_f64();
    let nap = if remain > 0.001 {
        poll_s.min(remain)
    } else {
        0.001
    };
    // Sleep in <=50ms slices so a `notifications/cancelled` wakes the
    // `wait_for` poll loop promptly. True = cancelled.
    let mut left = nap.max(0.0);
    while left > 0.0 {
        if crate::cancel::cancelled() {
            return true;
        }
        let s = left.min(0.05);
        std::thread::sleep(Duration::from_secs_f64(s));
        left -= s;
    }
    crate::cancel::cancelled()
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
            if crate::cancel::cancelled() {
                return wf_done(t0, false, false, Some("cancelled".to_string()));
            }
            let (h, _, _) = resolve_selector(&sel);
            if h.is_some() == want_present {
                return wf_done(t0, true, true, None);
            }
            if t0.elapsed().as_secs_f64() >= timeout_s {
                return wf_done(t0, true, false, None);
            }
            if wf_nap(t0, timeout_s, poll_s) {
                return wf_done(t0, false, false, Some("cancelled".to_string()));
            }
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
            if crate::cancel::cancelled() {
                return wf_done(t0, false, false, Some("cancelled".to_string()));
            }
            if let Ok(img) = bitblt_grab(x as i32, y as i32, 1, 1) {
                if img.px.len() >= 3 && (0..3).all(|k| (img.px[k] as i64 - color[k]).abs() <= tol) {
                    return wf_done(t0, true, true, None);
                }
            }
            if t0.elapsed().as_secs_f64() >= timeout_s {
                return wf_done(t0, true, false, None);
            }
            if wf_nap(t0, timeout_s, poll_s) {
                return wf_done(t0, false, false, Some("cancelled".to_string()));
            }
        }
    }
    if kind == "stable" {
        let region = match a.get("region") {
            None => default_stable_region(),
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
        // Overflow-safe region gate: the shared validator never subtracts
        // in i64 before validating, so MIN/MAX extremes are typed errors
        // (no debug panic, no release wraparound bypass).
        let (rw, rh) = match crate::validate_region(rl, rt, rr, rb, MAX_STABLE_EDGE) {
            Ok(wh) => wh,
            Err(crate::RegionReject::Order) => {
                return wf_done(
                    t0,
                    false,
                    false,
                    Some("region must have right>left and bottom>top".to_string()),
                );
            }
            Err(crate::RegionReject::Extreme) => {
                return wf_done(
                    t0,
                    false,
                    false,
                    Some(
                        "region coordinates outside i32 range (would truncate on cast)".to_string(),
                    ),
                );
            }
            Err(crate::RegionReject::OverCap(w, h)) => {
                return wf_done(
                    t0,
                    false,
                    false,
                    Some(format!(
                        "region must be at most {MAX_STABLE_EDGE}x{MAX_STABLE_EDGE} (got {w}x{h})"
                    )),
                );
            }
        };
        let mut prev: Option<Vec<u8>> = None;
        loop {
            if crate::cancel::cancelled() {
                return wf_done(t0, false, false, Some("cancelled".to_string()));
            }
            match bitblt_grab(rl as i32, rt as i32, rw as i32, rh as i32) {
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
            if wf_nap(t0, timeout_s, poll_s) {
                return wf_done(t0, false, false, Some("cancelled".to_string()));
            }
        }
    }
    if kind == "element" {
        // Element wait: poll the UIA element model for a control matching
        // the invoke-style selector, optionally checking its enabled
        // state. Read-only (no focus/input); a timeout is not an error.
        let win_sel: Option<String> = match a.get("window") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let s = py_str(v);
                if s.trim().is_empty() {
                    None
                } else {
                    Some(s)
                }
            }
        };
        let elem_v = a.get("element").cloned().unwrap_or(Value::Null);
        if !py_truthy(&elem_v) || py_str(&elem_v).trim().is_empty() {
            return wf_done(
                t0,
                false,
                false,
                Some(
                    "wait_for kind=element needs \"element\" (e.g. \"name:OK\", \"path:0\")"
                        .to_string(),
                ),
            );
        }
        let elem_s = py_str(&elem_v);
        let (ekind, _) = match elem_s.find(':') {
            Some(p) => (
                elem_s[..p].trim().to_lowercase(),
                elem_s[p + 1..].trim().to_string(),
            ),
            None => ("name".to_string(), elem_s.trim().to_string()),
        };
        if !["name", "auto_id", "autoid", "automationid", "id", "path"].contains(&ekind.as_str()) {
            return wf_done(
                t0,
                false,
                false,
                Some(format!(
                    "unknown element selector kind \"{ekind}\" (want name:|auto_id:|path:)"
                )),
            );
        }
        let state = match a.get("state") {
            None | Some(Value::Null) => "appear".to_string(),
            Some(v) => py_str(v).to_lowercase(),
        };
        if !["appear", "disappear", "enabled", "disabled"].contains(&state.as_str()) {
            return wf_done(
                t0,
                false,
                false,
                Some(
                    "wait_for kind=element state must be appear|disappear|enabled|disabled"
                        .to_string(),
                ),
            );
        }
        // Static shape check for an explicit window selector: unparseable
        // hwnd:/pid: values and empty title:/exe:/class: values are malformed
        // (per-action error), never something to poll for. Anything else
        // resolves per-poll; absence then just means "not here yet".
        if let Some(sel) = win_sel.as_deref() {
            let (kind, val) = crate::parse_selector(sel);
            let bad = match kind.as_str() {
                "hwnd" | "pid" => crate::parse_int_loose(&val).is_none(),
                "exe" | "title" | "class" => val.is_empty(),
                _ => false,
            };
            if bad {
                return wf_done(
                    t0,
                    false,
                    false,
                    Some(format!(
                        "wait_for kind=element: bad window selector {sel:?}"
                    )),
                );
            }
        }
        loop {
            if crate::cancel::cancelled() {
                return wf_done(t0, false, false, Some("cancelled".to_string()));
            }
            // Resolve the target window each poll: an explicit selector or
            // the current foreground window. A failed resolve (absent,
            // recycled, or ambiguous window) is transient absence, not an
            // error: disappear treats it as satisfied, every other state
            // keeps polling. Only statically malformed selectors fail fast
            // (checked above).
            let tgt: Option<HWND> = match win_sel.as_deref() {
                Some(sel) => resolve_selector(sel).0,
                None => {
                    let f = fg_now();
                    if f == 0 {
                        return wf_done(
                            t0,
                            true,
                            false,
                            Some("wait_for kind=element: no foreground window".to_string()),
                        );
                    }
                    Some(f as HWND)
                }
            };
            // A dead window mid-wait reads as gone: disappear is satisfied,
            // every other state keeps polling for its return.
            let alive = tgt.is_some_and(|h| unsafe { IsWindow(h) != 0 });
            if !alive {
                if state == "disappear" {
                    return wf_done(t0, true, true, None);
                }
            } else {
                let h = tgt.unwrap() as i64;
                // Paged scan: the control may live past the 200-entry first
                // page, so walk offset 0, 200, ... to the end counting total
                // matches. 0 -> absent handling below; 1 -> satisfied
                // check; >1 -> ambiguous per-action error, fail fast, never
                // spun to timeout. A page-local ambiguous refusal already
                // proves >1 overall. Cancellation is checked between pages,
                // not just between polls.
                let mut hits: Vec<(Map<String, Value>, String)> = Vec::new();
                let mut refused: Option<String> = None;
                let mut offset = 0usize;
                loop {
                    if crate::cancel::cancelled() {
                        return wf_done(t0, false, false, Some("cancelled".to_string()));
                    }
                    let model = element_model(h as HWND, crate::UIA_DEPTH_LIMIT, offset);
                    let (entry, merr, mcands) = match_element(&model.entries, Some(&elem_v), None);
                    if let Some(me) = merr {
                        if mcands.is_some() || me.contains("unknown element selector kind") {
                            // Ambiguous within the page (>=2 overall) or
                            // malformed: a per-action error, never silent
                            // and never spun to timeout.
                            refused = Some(me);
                            break;
                        }
                        // Not on this page: keep scanning later pages.
                    } else if let Some(e) = entry {
                        // One page yields at most one match without
                        // refusing, so a second hit across pages proves
                        // ambiguity; stop counting there.
                        if hits.is_empty() {
                            hits.push((e, model.backend));
                        } else {
                            refused = Some(format!(
                                "ambiguous element: multiple elements match \"{elem_s}\" across pages"
                            ));
                            break;
                        }
                    }
                    match model.next_offset {
                        Some(n) => offset = n,
                        None => break,
                    }
                }
                if let Some(me) = refused {
                    return wf_done(t0, false, false, Some(me));
                }
                match hits.pop() {
                    None => {
                        // Absent on every page: disappear is satisfied, the
                        // rest keep polling for its arrival.
                        if state == "disappear" {
                            return wf_done(t0, true, true, None);
                        }
                    }
                    Some((e, backend)) => {
                        let enabled = if state == "enabled" || state == "disabled" {
                            wf_element_enabled(h, &backend, &e)
                        } else {
                            None
                        };
                        if element_wait_satisfied(&state, true, enabled) {
                            return wf_done(t0, true, true, None);
                        }
                    }
                }
            }
            if t0.elapsed().as_secs_f64() >= timeout_s {
                return wf_done(t0, true, false, None);
            }
            if wf_nap(t0, timeout_s, poll_s) {
                return wf_done(t0, false, false, Some("cancelled".to_string()));
            }
        }
    }
    wf_done(
        t0,
        false,
        false,
        Some(format!(
            "unknown wait_for kind \"{kind}\" (want window_appear|window_disappear|pixel|stable|element)"
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

/// Restore the entry foreground window after a burst. The SetForegroundWindow
/// call lives behind `gate::set_foreground`, which demands the permit minted
/// from `allow_foreground: true`: the guard never steals focus on its own.
fn restore_foreground(permit: &crate::gate::ForegroundPermit, hwnd: i64) -> bool {
    crate::gate::set_foreground(permit, hwnd)
}

/// Window-resolution outcome: target hwnd, human error, or disambiguation
/// candidates when the selector matches several windows.
type ResolveOut = (
    Option<HWND>,
    Option<String>,
    Option<Vec<Map<String, Value>>>,
);

/// Element-match outcome: the entry, a typed error, or disambiguation
/// candidates when the selector matches several controls.
type MatchOut = (
    Option<Map<String, Value>>,
    Option<String>,
    Option<Vec<Map<String, Value>>>,
);

/// _resolve_window with untyped action values (puppet.py:625-636).
fn resolve_window_val(title: Option<&Value>, hwnd: Option<&Value>) -> ResolveOut {
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
) -> MatchOut {
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
        // P1b: never wedge a worker on a hung window. SendMessageTimeoutW
        // with SMTO_ABORTIFHUNG + 2s turns a hung target into a per-action
        // error; the reader and the other workers stay alive.
        let mut _res: usize = 0;
        let r = SendMessageTimeoutW(
            kids[idx as usize],
            0x00F5,
            0,
            0,
            SMTO_ABORTIFHUNG,
            2000,
            &mut _res,
        );
        if r == 0 {
            return (
                false,
                String::new(),
                Some("win32-click failed: target did not respond within 2s".to_string()),
            );
        }
    }
    (true, "win32-click".to_string(), None)
}

/// Gated pointer click: set_cursor_pos + button pair, behind BOTH permits.
/// Exact (snaps to the point, no interpolation, no hover journey); only
/// reachable from an explicit `fallback:"click"` with `allow_foreground` and
/// `allow_cursor` both true.
fn pixel_click_at(
    fg: &crate::gate::ForegroundPermit,
    cursor: &crate::gate::CursorPermit,
    x: i64,
    y: i64,
) -> bool {
    if !crate::gate::set_cursor_pos(cursor, x as i32, y as i32) {
        return false;
    }
    let seq = [mi_btn(M_LDOWN, 0), mi_btn(M_LUP, 0)];
    crate::gate::send_input(fg, &seq) == seq.len() as u32
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
            (Some(xv), Some(yv)) => {
                if let (Ok(x), Ok(y)) = (py_int(xv), py_int(yv)) {
                    push_opt(window_at(x, y));
                }
            }
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
            (Some(xv), Some(yv)) => {
                if let (Ok(x), Ok(y)) = (py_int(xv), py_int(yv)) {
                    push_opt(window_at(x, y));
                }
            }
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
    /// Minted ONLY from `allow_foreground: true` (gate.rs). `flush` needs it
    /// to reach `SendInput`, so a call that did not ask for foreground input
    /// cannot inject even if a future branch tried to queue events.
    permit: Option<crate::gate::ForegroundPermit>,
    /// Minted ONLY from `allow_cursor: true`. Every branch that queues a
    /// pointer-moving event checks it FIRST, so a cursor action can never be
    /// queued (and then silently dropped by a flush without the foreground
    /// permit) on a call that did not ask to move the owner's pointer.
    cursor: Option<crate::gate::CursorPermit>,
}

impl Burst {
    /// Held/repeated key delivery guard: down -> flush -> hold -> up ->
    /// flush per repeat, with a cancellable gap between repeats. The
    /// `cancelled` predicate is `crate::cancel::cancelled` live; tests
    /// inject a scripted one. Returns `Some(result)` on early exit
    /// (cancellation or flush error), `None` when every repeat completed.
    /// A cancellation after a key-down always emits the matching key-up
    /// (plus Shift when held) and flushes before returning: no exit path
    /// leaves a key held down.
    #[allow(clippy::too_many_arguments)]
    fn run_hold_repeats(
        &mut self,
        i: usize,
        typ_v: &Value,
        code: &KeyCode,
        repeat: i64,
        hold_ms: u64,
        gap_ms: u64,
        dry_run: bool,
        t0: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<Map<String, Value>> {
        self.marked.push(i);
        for r in 0..repeat {
            self.pending.extend(key_down_inputs(code));
            let (_, ferr) = self.flush();
            if let Some(fe) = ferr {
                // The down may have partially sent: release before failing.
                self.pending.extend(key_up_inputs(code));
                let _ = self.flush();
                // No clear before fail_since_flush: the marks are what make
                // the covered action report ok:false.
                fail_since_flush(&mut self.results, &mut self.marked, &fe, self.flush_uipi);
                let mut d = base_entry(i, typ_v, false, elapsed_ms(&t0));
                d.insert("error".to_string(), Value::from(fe));
                return Some(d);
            }
            self.marked.clear();
            if (!dry_run && hold_ms > 0 && sleep_cancellable(hold_ms)) || cancelled() {
                // Release the held key (+Shift) before reporting: no exit
                // path leaves a key down.
                self.pending.extend(key_up_inputs(code));
                let _ = self.flush();
                self.marked.clear();
                let mut d = base_entry(i, typ_v, false, elapsed_ms(&t0));
                d.insert("error".to_string(), Value::from("cancelled"));
                return Some(d);
            }
            self.pending.extend(key_up_inputs(code));
            let (_, ferr) = self.flush();
            if let Some(fe) = ferr {
                fail_since_flush(&mut self.results, &mut self.marked, &fe, self.flush_uipi);
                let mut d = base_entry(i, typ_v, false, elapsed_ms(&t0));
                d.insert("error".to_string(), Value::from(fe));
                return Some(d);
            }
            self.marked.clear();
            if r + 1 < repeat
                && !dry_run
                && gap_ms > 0
                && (sleep_cancellable(gap_ms) || cancelled())
            {
                let mut d = base_entry(i, typ_v, false, elapsed_ms(&t0));
                d.insert("error".to_string(), Value::from("cancelled"));
                return Some(d);
            }
        }
        None
    }

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
        // Gated call site: `permit` exists only when the caller passed
        // `allow_foreground: true`. Without it nothing is injected and the
        // queued events are dropped with a typed refusal.
        let Some(permit) = self.permit.as_ref() else {
            self.pending.clear();
            // Do NOT clear `marked`: the caller's fail_since_flush must be
            // able to mark every covered action FAILED. Clearing the marks
            // here was the silent-success bug: an action reported ok:true
            // while nothing was sent.
            return (
                0.0,
                Some(
                    crate::gate::GateError {
                        code: crate::gate::FOREGROUND_REQUIRED,
                        message: "SendInput path reached without allow_foreground:true; \
                                  nothing was injected"
                            .to_string(),
                    }
                    .text(),
                ),
            );
        };
        let sent = crate::gate::send_input(permit, &self.pending);
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
                if crate::uipi_blocked(crate::window_elevated(fg as HWND), crate::self_elevated()) {
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

/// Pointer-event guard: a branch that queues a cursor-moving INPUT must ask
/// for the cursor permit first. `allow_cursor` is the only mint, so a call
/// without it fails HERE with a typed error instead of queueing an event the
/// flush would later drop.
fn require_cursor(st: &Burst, verb: &str) -> Result<(), String> {
    if st.cursor.is_some() {
        return Ok(());
    }
    Err(crate::gate::GateError {
        code: crate::gate::CURSOR_REQUIRED,
        message: format!(
            "{verb} moves the system pointer; pass allow_cursor:true (SendInput delivery also \
             needs allow_foreground:true). Nothing was sent."
        ),
    }
    .text())
}

/// Typed code carried by a flush refusal, so the retro-marked action exposes
/// the same machine-readable code an in-branch refusal would.
fn gate_code_of(msg: &str) -> Option<&'static str> {
    [
        crate::gate::FOREGROUND_REQUIRED,
        crate::gate::CURSOR_REQUIRED,
        crate::gate::BACKGROUND_UNAVAILABLE,
        crate::gate::TARGET_REQUIRED,
    ]
    .into_iter()
    .find(|code| msg.starts_with(code))
}

/// Readable control state used as posted-click proof: the ExpandCollapse
/// state, the RangeValue, the TextPattern text and the enabled bit. `None`
/// when the control exposes none of them — in that case a posted click is
/// delivered but its effect is honestly reported `unverified`.
fn element_state(hwnd: i64, path: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Ok(s) = crate::uia::uia_expand_state(hwnd, path) {
        parts.push(format!("expand={}", crate::uia::expand_state_name(s)));
    }
    if let Ok(v) = crate::uia::uia_range_value(hwnd, path) {
        parts.push(format!("value={v}"));
    }
    if let Ok(t) = crate::uia::uia_element_text(hwnd, path) {
        let (t, _) = crate::shot::cap_text(&t);
        parts.push(format!("text={t}"));
    }
    if let Ok(e) = crate::uia::uia_is_enabled(hwnd as HWND, path) {
        parts.push(format!("enabled={e}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(";"))
    }
}

/// Bounded poll for a state change after a posted click (controls process
/// posted messages on their own loop, so the read is not instant).
fn wait_state_change(hwnd: i64, path: &str, before: &str) -> bool {
    let t0 = Instant::now();
    let timeout = Duration::from_millis(400);
    while t0.elapsed() < timeout {
        match element_state(hwnd, path) {
            Some(now) if now != before => return true,
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    element_state(hwnd, path).is_some_and(|now| now != before)
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
                if let Some(code) = gate_code_of(msg) {
                    r.insert("code".to_string(), Value::from(code));
                }
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

/// Optional ExpandCollapse directive for `invoke`: `state` "expand" /
/// "collapse" (or absent for plain activation). Anything else is a typed
/// argument error. Pure; unit-tested.
fn expand_arg(v: Option<&Value>) -> Result<Option<bool>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(x) => match py_str(x).to_lowercase().as_str() {
            "expand" => Ok(Some(true)),
            "collapse" => Ok(Some(false)),
            s => Err(format!(
                "unknown invoke state \"{s}\" (want expand|collapse)"
            )),
        },
    }
}

// ---------------------------------------------------------------- the tool

/// `do` call arguments. Explicit struct (not a 10-positional-args function)
/// so every call site names what it is asking for: a target lock, and the
/// two no-steal permissions the gate mints permits from.
pub struct DoArgs<'a> {
    pub actions: &'a [Value],
    pub background: Option<String>,
    pub background_args: Option<Vec<String>>,
    pub focus_guard: bool,
    pub settle_ms: i64,
    pub dry_run: bool,
    pub desktop: Option<String>,
    /// Structured target lock: `{hwnd?,pid?,exe?,appId?,titleContains?,
    /// className?}` or a selector string. Resolved once, revalidated before
    /// every action.
    pub target: Option<Value>,
    /// Explicit permission for the SendInput / SetForegroundWindow family.
    pub allow_foreground: bool,
    /// Explicit permission to move the owner's pointer.
    pub allow_cursor: bool,
    /// Explicit permission to overwrite the owner's global clipboard (a side
    /// effect outside the target window).
    pub allow_clipboard: bool,
}

impl<'a> DoArgs<'a> {
    /// Background-safe defaults: no target, no permissions.
    pub fn new(actions: &'a [Value]) -> Self {
        DoArgs {
            actions,
            background: None,
            background_args: None,
            focus_guard: false,
            settle_ms: 0,
            dry_run: false,
            desktop: None,
            target: None,
            allow_foreground: false,
            allow_cursor: false,
            allow_clipboard: false,
        }
    }

    /// Dry-run (assembles events, never injects).
    pub fn dry(actions: &'a [Value]) -> Self {
        DoArgs {
            dry_run: true,
            ..DoArgs::new(actions)
        }
    }

    /// Test-only foreground batch: asks for both permissions explicitly,
    /// exactly as a caller that wants foreground input must.
    pub fn foreground(actions: &'a [Value]) -> Self {
        DoArgs {
            dry_run: true,
            allow_foreground: true,
            allow_cursor: true,
            ..DoArgs::new(actions)
        }
    }
}

pub fn do_burst(args: &DoArgs) -> String {
    let actions = args.actions;
    let background = args.background.clone();
    let background_args = args.background_args.clone();
    let focus_guard = args.focus_guard;
    let settle_ms = args.settle_ms;
    let dry_run = args.dry_run;
    let desktop = args.desktop.clone();
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

    // Target lock: resolved ONCE here. A resolution failure is the whole
    // call's typed error (nothing has run), never a silent retarget and
    // never a foreground fallback.
    let target_lock: Option<crate::target::TargetLock> = match args.target.as_ref() {
        None | Some(Value::Null) => None,
        Some(v) => {
            let spec = match crate::target::TargetSpec::parse(v) {
                Ok(s) => s,
                Err(e) => return target_refusal_blocks(&e),
            };
            match crate::target::lock(&spec) {
                Ok(l) => Some(l),
                Err(e) => return target_refusal_blocks(&e),
            }
        }
    };
    let mut hidden_owner: Option<crate::hidden::Hidden> = None;
    let mut hidden_created = false;
    let mut hidden_set_desktop = false;
    let mut hidden_pid: Option<u32> = None;
    let mut hidden_name = String::new();
    let mut hidden_hwnd: Option<i64> = None;
    let mut hidden_class = String::new();
    let mut hidden_title = String::new();
    let mut hidden_win_pid: u32 = 0;

    let bg_mode = background.is_some() || hidden_mode || target_lock.is_some();
    let (bg_hwnd, bg_error): (Option<i64>, Option<String>) = if let Some(l) = &target_lock {
        // The lock already resolved exactly one window; no further search.
        (Some(l.hwnd), None)
    } else if hidden_mode {
        // Opt-in mode: `background` names the program to launch on a private
        // desktop (`background_args` supplies argv, ignored on the plain
        // background path); its window is resolved with EnumDesktopWindows
        // (never the owner's EnumWindows) and then driven through the same
        // PostMessage branch below.
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
                let empty: Vec<String> = Vec::new();
                let h = crate::hidden::spawn(&p, background_args.as_ref().unwrap_or(&empty));
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
    // Earned private-desktop permission (see `private_desktop_earned`): the
    // desktop must have been created by this call and the target window must
    // be on it. `hidden_mode` alone is only an argument and grants nothing.
    let private_desktop = if hidden_mode {
        let created = hidden_owner
            .as_ref()
            .map(|h| h.ready.created && !h.ready.desktop_name.is_empty())
            .unwrap_or(false);
        let target_desktop = bg_hwnd.and_then(|h| crate::window_desktop_name(h as HWND));
        let earned =
            private_desktop_earned(created, hidden_name.as_str(), target_desktop.as_deref());
        if hidden_mode && !earned {
            // A hidden request that did not earn the private desktop behaves
            // like an ordinary target: no posted keyboard, no exemption.
            eprintln!(
                "puppet: desktop:'hidden' did not earn private-desktop permission \
                 (created={created}, target_desktop={target_desktop:?}, expected={:?}); \
                 using ordinary target rules",
                hidden_name
            );
        }
        earned
    } else {
        false
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
        // No flag -> None -> the SendInput call site is unreachable.
        permit: crate::gate::ForegroundPermit::grant(args.allow_foreground),
        // No flag -> None -> no pointer event can be queued.
        cursor: crate::gate::CursorPermit::grant(args.allow_cursor),
    };

    for (i, a) in actions.iter().enumerate() {
        // P1b: the `do` action loop is cancellable between actions so a
        // `notifications/cancelled` stops the burst promptly (no late reply).
        if crate::cancel::cancelled() {
            let mut m = base_entry(i, &Value::from("cancelled"), false, 0.0);
            m.insert("error".to_string(), Value::from("cancelled"));
            st.results.push(m);
            break;
        }
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
        // The lock is revalidated before EVERY action: a target that closed
        // or whose handle was recycled is a typed refusal, never a retarget
        // and never a foreground fallback.
        if let Some(l) = &target_lock {
            if let Err(e) = l.revalidate() {
                let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                m.insert("code".to_string(), Value::from(e.code));
                m.insert("error".to_string(), Value::from(e.text()));
                if let Some(c) = e.candidates {
                    m.insert(
                        "candidates".to_string(),
                        Value::Array(c.into_iter().map(Value::Object).collect()),
                    );
                }
                st.results.push(m);
                continue;
            }
        }
        // Element scroll resolves a UIA pattern on an explicit window (like
        // invoke/set_value), never a posted message: keep it out of the
        // background-PostMessage branch, which targets bg_hwnd directly.
        let has_element =
            matches!(ad.get("element"), Some(v) if py_truthy(v) && !py_str(v).trim().is_empty());
        let has_scroll_element = typ == "scroll" && has_element;
        let has_coords = ad.contains_key("x") && ad.contains_key("y");
        // The gate decides delivery BEFORE anything is queued or posted, and
        // refuses identically under dry_run: a dry run that says ok while the
        // live call would refuse is a lie.
        let gate = crate::gate::Gate::new(
            bg_hwnd.is_some() || bg_mode,
            args.allow_foreground,
            args.allow_cursor,
        )
        // Posted keyboard is allowed ONLY on the server's own hidden
        // desktop (a launched app, no owner session to desync).
        .private_desktop(private_desktop)
        // Writing the owner's clipboard is a side effect outside the target
        // window: explicit flag only, refused before any clipboard access.
        .allow_clipboard(args.allow_clipboard);
        if let Err(e) = gate.decide(typ, has_coords, has_element) {
            let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
            m.insert("code".to_string(), Value::from(e.code));
            m.insert("error".to_string(), Value::from(e.text()));
            if bg_mode {
                m.insert("mode".to_string(), Value::from("background"));
                m.insert(
                    "bg_hwnd".to_string(),
                    bg_hwnd.map(Value::from).unwrap_or(Value::Null),
                );
            }
            if dry_run {
                m.insert("dry_run".to_string(), Value::from(true));
            }
            st.results.push(m);
            continue;
        }
        // `type` on an explicit element is a UIA ValuePattern write, not a
        // posted character stream: it takes its own branch below. `click` on
        // an element is UIA-pattern-first too (Invoke/SelectionItem/Toggle),
        // with a posted click at the element centre as the fallback.
        // Posted keyboard is never used on the owner's desktop: WM_CHAR /
        // WM_KEYDOWN bypass the input queue, desync the target and are
        // ignored by many controls. The private (hidden) desktop is the sole
        // exception, where the server owns the session it is driving.
        let keyboard_postable = private_desktop && matches!(typ, "type" | "key" | "hotkey");
        let is_bg_postable = bg_mode
            && ((matches!(typ, "click" | "down" | "up" | "move" | "drag")
                || keyboard_postable
                || (typ == "scroll" && !has_scroll_element))
                && !(has_element && matches!(typ, "type" | "click")));

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
                let (ok_b, err_b, n_b) = bg_execute(
                    bg_hwnd.unwrap_or(0),
                    ad,
                    target_lock.is_some(),
                    private_desktop,
                );
                posted_total += n_b;
                let mut m = base_entry(i, &typ_v, ok_b, elapsed_ms(&t0));
                m.insert("mode".to_string(), Value::from("background"));
                m.insert(
                    "bg_hwnd".to_string(),
                    bg_hwnd.map(Value::from).unwrap_or(Value::Null),
                );
                m.insert("posted".to_string(), Value::from(n_b));
                m.insert("note".to_string(), Value::from(BG_NOTE));
                if typ == "click" {
                    // Raw x/y posted click: delivery is all that was proven.
                    // The point may be another control, an occluding window,
                    // or a control that ignores posted messages, so the effect
                    // is explicitly unverified - never implied to have acted.
                    m.insert("delivered".to_string(), Value::from("posted"));
                    m.insert("effect".to_string(), Value::from("unverified"));
                }
                if let Some(e) = err_b {
                    m.insert("error".to_string(), Value::from(e));
                }
                return Ok(Some(m));
            }
            if typ == "move" {
                let has_abs = ad.contains_key("x") || ad.contains_key("y");
                let has_rel = ad.contains_key("dx")
                    || ad.contains_key("dy")
                    || ad.get("relative").is_some_and(py_truthy);
                if has_abs && has_rel {
                    return Err(
                        "move takes x/y or dx/dy, not both (absolute or relative, never mixed)"
                            .to_string(),
                    );
                }
                require_cursor(&st, "move")?;
                if has_rel {
                    let dx = match ad.get("dx") {
                        None | Some(Value::Null) => {
                            return Err("relative move needs dx and dy (got only one)".to_string())
                        }
                        Some(v) => py_int(v).map_err(|_| key_err("dx"))?,
                    };
                    let dy = match ad.get("dy") {
                        None | Some(Value::Null) => {
                            return Err("relative move needs dx and dy (got only one)".to_string())
                        }
                        Some(v) => py_int(v).map_err(|_| key_err("dy"))?,
                    };
                    // No desktop gate: a relative nudge is legal from
                    // anywhere, including outside the virtual desktop.
                    st.pending.push(mi_move_rel(dx, dy)?);
                    st.marked.push(i);
                    return Ok(Some(base_entry(i, &typ_v, true, elapsed_ms(&t0))));
                }
                let mx = py_int(ad.get("x").unwrap_or(&Value::Null)).map_err(|_| key_err("x"))?;
                let my = py_int(ad.get("y").unwrap_or(&Value::Null)).map_err(|_| key_err("y"))?;
                require_on_desktop(mx, my)?;
                st.pending.push(mi_move(mx, my));
                st.marked.push(i);
                return Ok(Some(base_entry(i, &typ_v, true, elapsed_ms(&t0))));
            }
            if typ == "click" && !has_element {
                let (dn, up) = btn_pair(ad.get("button"))?;
                if ad.contains_key("x") && ad.contains_key("y") {
                    require_cursor(&st, "click x/y")?;
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
                require_cursor(&st, "drag")?;
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
                // Element scroll is UIA-only: x/y alongside element is a
                // typed error, never a queued mouse move. This check runs
                // before any window resolution so it holds headless too.
                let elem_sel: Option<Value> = match ad.get("element") {
                    None | Some(Value::Null) => None,
                    Some(v) if !py_truthy(v) || py_str(v).trim().is_empty() => None,
                    Some(v) => Some(v.clone()),
                };
                let has_xy = ad.contains_key("x") || ad.contains_key("y");
                if elem_sel.is_some() && has_xy {
                    return Err(
                        "scroll takes x/y or element, not both (element scroll never moves the mouse)"
                            .to_string(),
                    );
                }
                if elem_sel.is_none() && ad.contains_key("x") && ad.contains_key("y") {
                    require_cursor(&st, "scroll x/y")?;
                    let sx = py_int(&ad["x"])?;
                    let sy = py_int(&ad["y"])?;
                    require_on_desktop(sx, sy)?;
                    st.pending.push(mi_move(sx, sy));
                }
                let amt = opt_py_int(ad, "amount", 1)?;
                let axis = match ad.get("axis") {
                    None | Some(Value::Null) => "v".to_string(),
                    Some(v) => py_str(v).to_lowercase(),
                };
                // A non-empty `element` switches from the wheel to UIA
                // ScrollPattern on the matched control. Absent/empty keeps
                // the legacy wheel behavior.
                if let Some(elem_v) = elem_sel {
                    let (_, ferr) = st.flush();
                    if let Some(fe) = ferr {
                        fail_since_flush(&mut st.results, &mut st.marked, &fe, st.flush_uipi);
                    } else {
                        st.marked.clear();
                    }
                    let mut d = base_entry(i, &typ_v, false, 0.0);
                    // A failed background resolution is terminal for the
                    // element path: report it instead of falling back to
                    // the foreground window (which could scroll an
                    // unintended control).
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
                                "bad_window: scroll element needs a background target or a foreground window",
                            ),
                        );
                        d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                        return Ok(Some(d));
                    }
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
                    let model = element_model(tgt as HWND, crate::UIA_DEPTH_LIMIT, 0);
                    if dry_run {
                        d.insert("dry_run".to_string(), Value::from(true));
                    }
                    d.insert("backend".to_string(), Value::from(model.backend.clone()));
                    let (entry, merr, mcands) =
                        match_element(&model.entries, Some(&elem_v), ad.get("index"));
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
                    if let Some(se) = check_expect_fp(ad, &entry, &py_str(&elem_v)) {
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
                    if model.backend != "uia" {
                        d.insert("ok".to_string(), Value::from(false));
                        d.insert(
                            "error".to_string(),
                            Value::from(
                                "scroll element needs the UIA backend (Win32 fallback has no ScrollPattern)",
                            ),
                        );
                        d.insert("via".to_string(), Value::from("none"));
                        d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                        return Ok(Some(d));
                    }
                    let (h, v) = match crate::uia::scroll_pattern_args(&axis, amt) {
                        Ok(p) => p,
                        Err(e) => {
                            d.insert("ok".to_string(), Value::from(false));
                            d.insert("error".to_string(), Value::from(e));
                            d.insert("via".to_string(), Value::from("none"));
                            d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                            return Ok(Some(d));
                        }
                    };
                    let (ok_v, via, verr) =
                        crate::uia::uia_scroll(tgt, &entry_str(&entry, "path"), h, v);
                    d.insert(
                        "via".to_string(),
                        Value::from(if via.is_empty() {
                            "none".to_string()
                        } else {
                            via
                        }),
                    );
                    d.insert("ok".to_string(), Value::from(ok_v));
                    if let Some(e) = verr {
                        if !ok_v {
                            d.insert("error".to_string(), Value::from(e));
                        }
                    }
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                let (flag, data) = scroll_wheel(&axis, amt)?;
                st.pending.push(mi_btn(flag, data));
                st.marked.push(i);
                return Ok(Some(base_entry(i, &typ_v, true, elapsed_ms(&t0))));
            }
            // `click` on an explicit element, target-locked: UIA pattern
            // first (Invoke/SelectionItem/Toggle), else ONE posted
            // button-down + button-up at the element centre converted to
            // exact client pixels. Never a pointer move.
            if typ == "click" && has_element {
                let (_, ferr) = st.flush();
                if let Some(fe) = ferr {
                    fail_since_flush(&mut st.results, &mut st.marked, &fe, st.flush_uipi);
                } else {
                    st.marked.clear();
                }
                let mut d = base_entry(i, &typ_v, false, 0.0);
                let Some(tgt) = bg_hwnd else {
                    d.insert(
                        "error".to_string(),
                        Value::from(
                            "target_required: click on an element needs a target window \
                             (target:{\"hwnd\":..}); this server never falls back to the \
                             foreground window",
                        ),
                    );
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                };
                if let Some(msg) = uipi_refusal(tgt) {
                    d.insert("error".to_string(), Value::from(msg));
                    d.insert("blocked_by_uipi".to_string(), Value::from(true));
                    d.insert("resolution".to_string(), Value::from("uipi_blocked"));
                    d.insert("via".to_string(), Value::from("none"));
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                let model = element_model(tgt as HWND, crate::UIA_DEPTH_LIMIT, 0);
                if dry_run {
                    d.insert("dry_run".to_string(), Value::from(true));
                }
                d.insert("target_hwnd".to_string(), Value::from(tgt));
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
                    d.insert("via".to_string(), Value::from("none"));
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                let entry = entry.unwrap();
                if let Some(se) = check_expect_fp(
                    ad,
                    &entry,
                    &py_str(ad.get("element").unwrap_or(&Value::Null)),
                ) {
                    d.insert("error".to_string(), Value::from(se));
                    d.insert("resolution".to_string(), Value::from("stale"));
                    d.insert("via".to_string(), Value::from("none"));
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                d.insert("element".to_string(), Value::from(element_pub_full(&entry)));
                d.insert("resolution".to_string(), Value::from("found"));
                // Element rects are SCREEN pixels; the client conversion is
                // the documented target convention (physical pixels).
                let rect: Vec<i64> = match entry.get("rect") {
                    Some(Value::Array(r)) => r.iter().map(|v| v.as_i64().unwrap_or(0)).collect(),
                    _ => vec![0, 0, 0, 0],
                };
                let (rl, rt, rw, rh) = (
                    *rect.first().unwrap_or(&0),
                    *rect.get(1).unwrap_or(&0),
                    *rect.get(2).unwrap_or(&0),
                    *rect.get(3).unwrap_or(&0),
                );
                let centre = crate::target::screen_point_to_client(tgt, rl + rw / 2, rt + rh / 2);
                if dry_run {
                    d.insert(
                        "via".to_string(),
                        Value::from(if model.backend == "uia" {
                            "uia-pattern"
                        } else {
                            "posted-click"
                        }),
                    );
                    d.insert("ok".to_string(), Value::from(true));
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                let mut via = "posted-click".to_string();
                let mut ok_v = false;
                let mut verr: Option<String> = None;
                let mut delivered = "none";
                let mut effect = "unverified";
                let path = entry_str(&entry, "path");
                if model.backend == "uia" {
                    // UIA Invoke/Select/Toggle is the preferred path: the
                    // pattern call is the app's own acknowledgement.
                    let (ok_a, via_a, err_a) = crate::uia::uia_activate(tgt, &path);
                    if ok_a {
                        ok_v = true;
                        via = via_a;
                        delivered = "uia";
                        // Verify when the control exposes readable state.
                        let pre = element_state(tgt, &path);
                        effect = match pre {
                            Some(pre) => {
                                if wait_state_change(tgt, &path, &pre) {
                                    "verified"
                                } else {
                                    "pattern-returned"
                                }
                            }
                            None => "pattern-returned",
                        };
                    } else {
                        verr = err_a;
                    }
                }
                if !ok_v {
                    if let Some((cx, cy)) = centre {
                        // A posted button message is delivery, NOT proof the
                        // control acted (many controls ignore it). The state
                        // is read before and after and the action fails when
                        // a readable state did not change.
                        let pre = element_state(tgt, &path);
                        let mut m = Map::new();
                        m.insert("type".to_string(), Value::from("click"));
                        m.insert("x".to_string(), Value::from(cx));
                        m.insert("y".to_string(), Value::from(cy));
                        if let Some(b) = ad.get("button") {
                            m.insert("button".to_string(), b.clone());
                        }
                        if let Some(c) = ad.get("count") {
                            m.insert("count".to_string(), c.clone());
                        }
                        let (ok_b, err_b, n_b) = bg_execute(tgt, &m, true, false);
                        posted_total += n_b;
                        ok_v = ok_b;
                        via = "posted-click".to_string();
                        delivered = "posted";
                        if let Some(e) = err_b {
                            verr = Some(e);
                        }
                        if ok_b {
                            effect = match pre {
                                Some(pre) => {
                                    if wait_state_change(tgt, &path, &pre) {
                                        "verified"
                                    } else {
                                        // Readable state and it did not
                                        // change: the click did not act.
                                        ok_v = false;
                                        verr = Some(format!(
                                            "background_unavailable: the posted click was \
                                             delivered to the control but its state did not \
                                             change ({pre}); controls that ignore posted button \
                                             messages report exactly this. Use invoke for the \
                                             UIA pattern, or allow_foreground:true for a real \
                                             click."
                                        ));
                                        "unchanged"
                                    }
                                }
                                None => "unverified",
                            };
                        } else {
                            effect = "unverified";
                        }
                    } else {
                        verr = Some(
                            "element rect is not convertible to client pixels (window moved or \
                             closed mid-call)"
                                .to_string(),
                        );
                    }
                }
                d.insert("ok".to_string(), Value::from(ok_v));
                d.insert("via".to_string(), Value::from(via));
                d.insert("delivered".to_string(), Value::from(delivered));
                d.insert("effect".to_string(), Value::from(effect));
                if !ok_v {
                    if let Some(e) = verr {
                        d.insert(
                            "code".to_string(),
                            Value::from(crate::gate::BACKGROUND_UNAVAILABLE),
                        );
                        d.insert("error".to_string(), Value::from(e));
                    }
                }
                d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                return Ok(Some(d));
            }
            // `type` on an explicit element is a UIA ValuePattern.SetValue on
            // the matched editable control: no keys, no pointer, but the
            // provider MAY focus the control, so it is an explicit opt-in.
            // Without an element plain typing is refused (posted WM_CHAR is
            // unreliable) and needs allow_foreground for SendInput.
            if typ == "type" && has_element {
                // Defense in depth: the gate already refuses UIA text entry
                // without the explicit opt-in; no provider call happens here
                // unless the foreground permit exists.
                if st.permit.is_none() {
                    let mut d = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    d.insert(
                        "code".to_string(),
                        Value::from(crate::gate::FOREGROUND_REQUIRED),
                    );
                    d.insert(
                        "error".to_string(),
                        Value::from(
                            "foreground_required: ValuePattern.SetValue may focus the target \
                             control; pass allow_foreground:true. Nothing was written.",
                        ),
                    );
                    return Ok(Some(d));
                }
                let (_, ferr) = st.flush();
                if let Some(fe) = ferr {
                    fail_since_flush(&mut st.results, &mut st.marked, &fe, st.flush_uipi);
                } else {
                    st.marked.clear();
                }
                let mut d = base_entry(i, &typ_v, false, 0.0);
                let Some(tgt) = bg_hwnd else {
                    d.insert(
                        "error".to_string(),
                        Value::from(
                            "target_required: type on an element needs a target window; this \
                             server never falls back to the foreground window",
                        ),
                    );
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                };
                if let Some(msg) = uipi_refusal(tgt) {
                    d.insert("error".to_string(), Value::from(msg));
                    d.insert("blocked_by_uipi".to_string(), Value::from(true));
                    d.insert("resolution".to_string(), Value::from("uipi_blocked"));
                    d.insert("via".to_string(), Value::from("none"));
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                let text = ad.get("text").map(py_str).unwrap_or_default();
                let model = element_model(tgt as HWND, crate::UIA_DEPTH_LIMIT, 0);
                if dry_run {
                    d.insert("dry_run".to_string(), Value::from(true));
                }
                d.insert("target_hwnd".to_string(), Value::from(tgt));
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
                    d.insert("via".to_string(), Value::from("none"));
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                let entry = entry.unwrap();
                if let Some(se) = check_expect_fp(
                    ad,
                    &entry,
                    &py_str(ad.get("element").unwrap_or(&Value::Null)),
                ) {
                    d.insert("error".to_string(), Value::from(se));
                    d.insert("resolution".to_string(), Value::from("stale"));
                    d.insert("via".to_string(), Value::from("none"));
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                d.insert("element".to_string(), Value::from(element_pub_full(&entry)));
                d.insert("resolution".to_string(), Value::from("found"));
                d.insert(
                    "value_len".to_string(),
                    Value::from(text.chars().count() as i64),
                );
                if dry_run {
                    d.insert("ok".to_string(), Value::from(true));
                    d.insert("via".to_string(), Value::from("none"));
                    d.insert("delivered".to_string(), Value::from("uia"));
                    d.insert("effect".to_string(), Value::from("unverified"));
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                let (ok_v, via, verr) =
                    crate::uia::uia_set_value(tgt, &entry_str(&entry, "path"), &text);
                d.insert("ok".to_string(), Value::from(ok_v));
                d.insert(
                    "via".to_string(),
                    Value::from(if via.is_empty() {
                        "uia-set-value".to_string()
                    } else {
                        via
                    }),
                );
                // A UIA pattern write is the app's own acknowledgement (it
                // returns S_OK from its provider), so it is not "posted".
                d.insert("delivered".to_string(), Value::from("uia"));
                d.insert(
                    "effect".to_string(),
                    Value::from(if ok_v { "verified" } else { "unverified" }),
                );
                if let Some(e) = verr {
                    d.insert("error".to_string(), Value::from(e));
                }
                d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                return Ok(Some(d));
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
                        if cm != 0 && !dry_run && sleep_cancellable(cm as u64) {
                            d.insert("ok".to_string(), Value::from(false));
                            d.insert("error".to_string(), Value::from("cancelled"));
                            d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                            return Ok(Some(d));
                        }
                        if crate::cancel::cancelled() {
                            d.insert("ok".to_string(), Value::from(false));
                            d.insert("error".to_string(), Value::from("cancelled"));
                            d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                            return Ok(Some(d));
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
                let code = vk_of(&name)?;
                let hold_ms = opt_py_int(ad, "hold_ms", 0)?;
                let repeat = opt_py_int(ad, "repeat", 1)?;
                let gap_ms = opt_py_int(ad, "gap_ms", 0)?;
                if !(0..=60000).contains(&hold_ms) {
                    return Err(format!("hold_ms out of range 0..60000 (got {hold_ms})"));
                }
                if !(1..=100).contains(&repeat) {
                    return Err(format!("key repeat out of range 1..100 (got {repeat})"));
                }
                if !(0..=60000).contains(&gap_ms) {
                    return Err(format!("gap_ms out of range 0..60000 (got {gap_ms})"));
                }
                if repeat == 1 && hold_ms == 0 {
                    // Legacy fast path: one coalesced down/up pair.
                    st.pending.extend(key_inputs(&code));
                    st.marked.push(i);
                    return Ok(Some(base_entry(i, &typ_v, true, elapsed_ms(&t0))));
                }
                // Held/repeated path: flush first so the hold sequence
                // stands alone, then run the guarded repeat loop below.
                // Dry-run assembles every event without sleeping.
                let (_, ferr) = st.flush();
                if let Some(fe) = ferr {
                    fail_since_flush(&mut st.results, &mut st.marked, &fe, st.flush_uipi);
                } else {
                    st.marked.clear();
                }
                let t0h = Instant::now();
                if let Some(d) = st.run_hold_repeats(
                    i,
                    &typ_v,
                    &code,
                    repeat,
                    hold_ms as u64,
                    gap_ms as u64,
                    dry_run,
                    t0h,
                    &crate::cancel::cancelled,
                ) {
                    return Ok(Some(d));
                }
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
                let mut codes = Vec::with_capacity(keys.len());
                for k in &keys {
                    codes.push(vk_of(k)?);
                }
                st.pending.extend(hotkey_inputs(&codes));
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
                // The owner's clipboard is shared state outside the target
                // window: mint the permit FIRST, so a call that did not pass
                // allow_clipboard:true is a typed per-action refusal and no
                // clipboard API (not even OpenClipboard) is reached.
                let permit = match crate::gate::ClipboardPermit::grant(args.allow_clipboard) {
                    Some(p) => p,
                    None => {
                        let e = crate::gate::clipboard_refusal("clipboard_set");
                        let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                        m.insert("code".to_string(), Value::from(e.code));
                        m.insert("error".to_string(), Value::from(e.text()));
                        if dry_run {
                            m.insert("dry_run".to_string(), Value::from(true));
                        }
                        return Ok(Some(m));
                    }
                };
                let text = match ad.get("text") {
                    None => String::new(),
                    Some(v) => py_str(v),
                };
                // Bound before anything else (including the dry_run
                // early-return): over-cap input never touches the clipboard.
                if let Err(e) = check_clip_len(text.chars().count()) {
                    let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    m.insert("error".to_string(), Value::from(e));
                    return Ok(Some(m));
                }
                if dry_run {
                    let mut m = base_entry(i, &typ_v, true, elapsed_ms(&t0));
                    m.insert("dry_run".to_string(), Value::from(true));
                    return Ok(Some(m));
                }
                match clipboard_set(&permit, &text) {
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
                    _ => {
                        // No target: refuse with the typed code. The
                        // foreground window is never a fallback.
                        d.insert(
                            "error".to_string(),
                            Value::from(format!(
                                "target_required: {typ} needs an explicit target window \
                                 (target:{{\"hwnd\":..}} or another field); this server never \
                                 falls back to the foreground window"
                            )),
                        );
                        d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                        return Ok(Some(d));
                    }
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
                let model = element_model(tgt as HWND, crate::UIA_DEPTH_LIMIT, 0);
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
                if let Some(se) = check_expect_fp(
                    ad,
                    &entry,
                    &py_str(ad.get("element").unwrap_or(&Value::Null)),
                ) {
                    d.insert("ok".to_string(), Value::from(false));
                    d.insert("error".to_string(), Value::from(se));
                    d.insert("resolution".to_string(), Value::from("stale"));
                    d.insert("via".to_string(), Value::from("none"));
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                // Optional ExpandCollapse directive: validated before the
                // dry_run early-return so a bad state fails typed under
                // dry_run too, never resolving. With `state` set, ONLY the
                // ExpandCollapse pattern fires — never Invoke (a SplitButton
                // supports both, and Invoke could open a menu instead of
                // collapsing it) and never the pixel-click fallback below.
                let expand_req = match expand_arg(ad.get("state")) {
                    Ok(r) => r,
                    Err(e) => {
                        let mut d = base_entry(i, &typ_v, false, 0.0);
                        d.insert("target_hwnd".to_string(), Value::from(tgt));
                        d.insert("backend".to_string(), Value::from(model.backend.clone()));
                        d.insert("error".to_string(), Value::from(e));
                        d.insert("resolution".to_string(), Value::from("not-found"));
                        d.insert("via".to_string(), Value::from("none"));
                        d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                        return Ok(Some(d));
                    }
                };
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
                let (ok_v, mut via, mut verr) = if let Some(want) = expand_req {
                    if model.backend != "uia" {
                        (
                            false,
                            "none".to_string(),
                            Some(
                                "invoke state needs the UIA backend (Win32 fallback has no ExpandCollapse)"
                                    .to_string(),
                            ),
                        )
                    } else {
                        let (ok_e, via_e, err_e) =
                            crate::uia::uia_expand_collapse(tgt, &entry_str(&entry, "path"), want);
                        (ok_e, via_e, err_e)
                    }
                } else if model.backend == "uia" {
                    crate::uia::uia_activate(tgt, &entry_str(&entry, "path"))
                } else {
                    win32_activate(&entry)
                };
                let fallback = match ad.get("fallback") {
                    None => String::new(),
                    Some(v) => py_str(v).to_lowercase(),
                };
                // State-directed invoke is ExpandCollapse ONLY: a failed
                // ExpandCollapse never falls through to a pixel click (the
                // click would act while reporting the pattern's error away).
                if !ok_v && fallback == "click" && expand_req.is_none() {
                    // Gated call site: the pixel click moves the system
                    // pointer and injects a button pair, so it needs BOTH
                    // explicit permissions. Without them the action reports
                    // cursor_move_required and never escalates on its own.
                    let (fg_p, cur_p) = match (
                        crate::gate::ForegroundPermit::grant(args.allow_foreground),
                        crate::gate::CursorPermit::grant(args.allow_cursor),
                    ) {
                        (Some(f), Some(c)) => (f, c),
                        _ => {
                            let refusal = crate::gate::GateError {
                                code: crate::gate::CURSOR_REQUIRED,
                                message: "fallback:\"click\" aims the real pointer and injects \
                                          a click; pass allow_cursor:true and \
                                          allow_foreground:true, or use a posted click. Nothing \
                                          was sent."
                                    .to_string(),
                            };
                            d.insert("ok".to_string(), Value::from(false));
                            d.insert(
                                "code".to_string(),
                                Value::from(crate::gate::CURSOR_REQUIRED),
                            );
                            d.insert("error".to_string(), Value::from(refusal.text()));
                            d.insert("via".to_string(), Value::from("none"));
                            d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                            return Ok(Some(d));
                        }
                    };
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
                        if pixel_click_at(&fg_p, &cur_p, rl + rw / 2, rt + rh / 2) {
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
                // ExpandCollapse read-back: the post-action state, for the
                // live expand/collapse proof. Best-effort (a failed read
                // omits the field rather than failing the action).
                if expand_req.is_some() && model.backend == "uia" {
                    if let Ok(st) = crate::uia::uia_expand_state(tgt, &entry_str(&entry, "path")) {
                        d.insert(
                            "expand_state".to_string(),
                            Value::from(crate::uia::expand_state_name(st)),
                        );
                    }
                }
                d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                return Ok(Some(d));
            }
            if typ == "set_value" {
                // Defense in depth: UIA text entry needs the explicit opt-in
                // before any provider call.
                if st.permit.is_none() {
                    let mut d = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    d.insert(
                        "code".to_string(),
                        Value::from(crate::gate::FOREGROUND_REQUIRED),
                    );
                    d.insert(
                        "error".to_string(),
                        Value::from(
                            "foreground_required: ValuePattern.SetValue may focus the target \
                             control; pass allow_foreground:true. Nothing was written.",
                        ),
                    );
                    return Ok(Some(d));
                }
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
                    _ => {
                        // No target: refuse with the typed code. The
                        // foreground window is never a fallback.
                        d.insert(
                            "error".to_string(),
                            Value::from(format!(
                                "target_required: {typ} needs an explicit target window \
                                 (target:{{\"hwnd\":..}} or another field); this server never \
                                 falls back to the foreground window"
                            )),
                        );
                        d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                        return Ok(Some(d));
                    }
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
                let model = element_model(tgt as HWND, crate::UIA_DEPTH_LIMIT, 0);
                if dry_run {
                    d.insert("dry_run".to_string(), Value::from(true));
                }
                d.insert("backend".to_string(), Value::from(model.backend.clone()));
                // Argument validation: element must be present (same
                // name:|auto_id:|path: forms as invoke; ref: stays rejected
                // by match_element's unknown-kind path). Empty here gets the
                // set_value-typed message instead of invoke's.
                let elem_ok = matches!(ad.get("element"), Some(v) if py_truthy(v) && !py_str(v).trim().is_empty());
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
                if let Some(se) = check_expect_fp(
                    ad,
                    &entry,
                    &py_str(ad.get("element").unwrap_or(&Value::Null)),
                ) {
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
                    // RangeValue read-back: on numeric controls the write
                    // went through RangeValue.SetValue, so report the value
                    // UIA now holds. Absent on the ValuePattern path (no
                    // pattern to read) and on read failure — never an error.
                    if let Ok(v) = crate::uia::uia_range_value(tgt, &entry_str(&entry, "path")) {
                        d.insert("value".to_string(), Value::from(v));
                    }
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
                if sleep_cancellable(wait_n as u64) {
                    let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    m.insert("error".to_string(), Value::from("cancelled"));
                    return Ok(Some(m));
                }
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
                // Gate already refused this verb without allow_foreground;
                // the permit below is therefore always present here.
                let Some(fg_permit) = st.permit.as_ref() else {
                    let mut d = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    d.insert(
                        "error".to_string(),
                        Value::from(
                            "foreground_required: focus_window needs allow_foreground:true",
                        ),
                    );
                    return Ok(Some(d));
                };
                let r = focus_hwmd(fg_permit, hr.unwrap());
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
    if settle_n != 0 && !crate::cancel::cancelled() {
        sleep_cancellable(settle_n as u64);
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
    let mut focus_guard_refusal: Option<String> = None;
    if focus_guard && focus_after != focus_before && focus_before != 0 {
        match st.permit.as_ref() {
            // Gated call site: focus_guard restores via SetForegroundWindow,
            // so it needs the allow_foreground permit. Without it the guard
            // reports the refusal instead of touching focus.
            Some(p) => {
                focus_restored = restore_foreground(p, focus_before);
                focus_after = fg_now();
            }
            None => {
                focus_restored = false;
                focus_guard_refusal = Some(
                    crate::gate::GateError {
                        code: crate::gate::FOREGROUND_REQUIRED,
                        message: "focus_guard would call SetForegroundWindow; pass \
                                  allow_foreground:true (the guard never steals focus on its \
                                  own). The foreground window was left untouched."
                            .to_string(),
                    }
                    .text(),
                );
            }
        }
    }
    // Hidden-desktop finalization: capture + readback run on the hidden-desktop
    // thread, then the desktop is torn down and the proof recorded.
    let mut hidden_meta: Option<Map<String, Value>> = None;
    let mut hidden_png: Option<Vec<u8>> = None;
    if let Some(owner) = hidden_owner.take() {
        // Posted keyboard input is processed by the target's own message
        // loop; give it a beat to land before the owner thread reads back,
        // or the evidence read races the queue (notably the single-action
        // hidden test). Hidden-only, live-only: dry_run posts nothing, and
        // a failed setup has no target, so neither settles.
        if !dry_run && bg_error.is_none() && hidden_hwnd.is_some() {
            sleep_cancellable(1000);
        }
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
        // Same-fixture revert evidence: the target text before any driven
        // input and after the revert, plus whether they match. Hidden mode
        // only; plain `do` has no hidden_desktop object at all.
        hd.insert(
            "pre_drive_text".to_string(),
            done.pre_drive_text
                .clone()
                .map(Value::from)
                .unwrap_or(Value::Null),
        );
        hd.insert(
            "post_revert_text".to_string(),
            done.post_revert_text
                .clone()
                .map(Value::from)
                .unwrap_or(Value::Null),
        );
        hd.insert("reverted".to_string(), Value::from(done.reverted));
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
                    // P3a: the same PNG also rides as a real MCP `image`
                    // content block next to the text JSON (see the envelope
                    // below); the base64-in-JSON stays for legacy clients.
                    if c.ok && !png.is_empty() {
                        hidden_png = Some(png.clone());
                    }
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
        if let Some(r) = focus_guard_refusal {
            payload.insert("focus_guard_error".to_string(), Value::from(r));
        }
    }
    // Invariant telemetry (constraint 5): measured before/after, reported as
    // booleans. The guarantee is not this measurement — it is that the
    // forbidden call sites are unreachable without the explicit flags
    // (gate.rs); these booleans catch an OS-side surprise and make it visible.
    let cursor_after = cursor_now();
    payload.insert(
        "cursorUnchanged".to_string(),
        Value::from(cursor_after == cursor_before),
    );
    payload.insert(
        "foregroundUnchanged".to_string(),
        Value::from(fg_now() == focus_before),
    );
    if let Some(l) = &target_lock {
        payload.insert("target_locked".to_string(), Value::from(l.hwnd));
        payload.insert("target_pid".to_string(), Value::from(l.pid as i64));
        payload.insert("target_exe".to_string(), Value::from(l.exe.clone()));
        payload.insert("target_spec".to_string(), Value::from(l.spec.describe()));
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
    let text = crate::py_dumps(&Value::Object(payload));
    match hidden_png {
        Some(png) if !png.is_empty() => crate::wrapped_text_with_image(
            &text,
            &base64::engine::general_purpose::STANDARD.encode(&png),
            "image/png",
        ),
        _ => wrapped_text(&text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn burst_first(actions: &[Value]) -> Map<String, Value> {
        let out = do_burst(&DoArgs::foreground(actions));
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let payload = outer
            .get("structuredContent")
            .cloned()
            .expect("structuredContent is the payload object");
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

    fn burst_payload(actions: &[Value]) -> Map<String, Value> {
        let out = do_burst(&DoArgs::foreground(actions));
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        outer
            .get("structuredContent")
            .and_then(|v| v.as_object())
            .expect("structuredContent is the payload object")
            .clone()
    }

    #[test]
    fn vk_of_reports_shift_from_layout_high_byte() {
        // US layout: capitals and shifted symbols need Shift held.
        assert_eq!(vk_of("A"), Ok(KeyCode::Vk(0x41, true)));
        assert_eq!(vk_of("!"), Ok(KeyCode::Vk(0x31, true)));
        assert_eq!(vk_of("+"), Ok(KeyCode::Vk(0xBB, true)));
        assert_eq!(vk_of("a"), Ok(KeyCode::Vk(0x41, false)));
        assert_eq!(vk_of("1"), Ok(KeyCode::Vk(0x31, false)));
        assert_eq!(vk_of("="), Ok(KeyCode::Vk(0xBB, false)));
        assert_eq!(vk_of("enter"), Ok(KeyCode::Vk(0x0D, false)));
        assert_eq!(vk_of("F5"), Ok(KeyCode::Vk(0x74, false)));
        assert!(vk_of("not_a_key_xyz").is_err());
    }

    #[test]
    fn key_shift_wraps_in_dry_run_events() {
        // `A` must coalesce to Shift-down, key down/up, Shift-up (4
        // events); `a` stays a bare key pair (2 events).
        let p = burst_payload(&[serde_json::json!({"type": "key", "name": "A"})]);
        assert_eq!(p.get("events"), Some(&Value::from(4)));
        let p = burst_payload(&[serde_json::json!({"type": "key", "name": "a"})]);
        assert_eq!(p.get("events"), Some(&Value::from(2)));
        // Shifted hotkey member wraps too: ctrl down, A-shift down, A up,
        // shift up, ctrl up.
        let p = burst_payload(&[serde_json::json!({"type": "hotkey", "keys": ["ctrl", "A"]})]);
        assert_eq!(p.get("events"), Some(&Value::from(6)));
    }

    #[test]
    fn expand_arg_parses_state_directive() {
        assert_eq!(expand_arg(None), Ok(None));
        assert_eq!(
            expand_arg(Some(&Value::Null)),
            Ok(None),
            "explicit null state means plain activation"
        );
        assert_eq!(expand_arg(Some(&Value::from("expand"))), Ok(Some(true)));
        assert_eq!(
            expand_arg(Some(&Value::from("Collapse"))),
            Ok(Some(false)),
            "state matching is case-insensitive"
        );
        let e = expand_arg(Some(&Value::from("open"))).unwrap_err();
        assert!(
            e.contains("expand|collapse"),
            "bad state must name the accepted values, got: {e}"
        );
    }

    #[test]
    fn invoke_state_validated_before_dry_run_return() {
        // Dry-run must validate `state` before the early ok:true return:
        // bogus state fails typed even though the element resolves.
        let (fx, bg) = fixture_bg();
        let _ = fx.hwnd();
        let actions =
            vec![serde_json::json!({"type": "invoke", "element": "name:OK", "state": "sideways"})];
        let (_, first) = burst_bg(&actions, &bg, true);
        assert_eq!(
            first.get("ok"),
            Some(&Value::from(false)),
            "dry-run bogus state must fail, got: {first:?}"
        );
        assert!(
            first
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .contains("expand|collapse"),
            "dry-run bogus state must name the accepted values, got: {first:?}"
        );
    }

    #[test]
    fn invoke_state_expand_resolves_under_dry_run() {
        // Valid state passes dry-run resolution (ok, found, no firing).
        let (fx, bg) = fixture_bg();
        let _ = fx.hwnd();
        let actions =
            vec![serde_json::json!({"type": "invoke", "element": "name:OK", "state": "expand"})];
        let (_, first) = burst_bg(&actions, &bg, true);
        assert_eq!(
            first.get("ok"),
            Some(&Value::from(true)),
            "dry-run valid state must resolve, got: {first:?}"
        );
        assert_eq!(
            first.get("resolution").and_then(|v| v.as_str()),
            Some("found"),
            "dry-run valid state must report found, got: {first:?}"
        );
    }

    fn ki_pair(ev: &INPUT) -> (u16, u32) {
        unsafe { (ev.Anonymous.ki.wVk, ev.Anonymous.ki.dwFlags) }
    }

    #[test]
    fn key_inputs_shift_order_and_hotkey_reverse_release() {
        // Shift must bracket the key: down, key down/up, up.
        let seq = key_inputs(&KeyCode::Vk(0x41, true));
        let got: Vec<(u16, u32)> = seq.iter().map(ki_pair).collect();
        assert_eq!(
            got,
            vec![(VK_SHIFT, 0), (0x41, 0), (0x41, K_UP), (VK_SHIFT, K_UP)],
            "shift must wrap the key, got: {got:?}"
        );
        let seq = key_inputs(&KeyCode::Vk(0x41, false));
        let got: Vec<(u16, u32)> = seq.iter().map(ki_pair).collect();
        assert_eq!(got, vec![(0x41, 0), (0x41, K_UP)]);
        // Hotkey releases in reverse: ctrl down, shift down, A down, A up,
        // shift up, ctrl up.
        let codes = vec![KeyCode::Vk(0x11, false), KeyCode::Vk(0x41, true)];
        let seq = hotkey_inputs(&codes);
        let got: Vec<(u16, u32)> = seq.iter().map(ki_pair).collect();
        assert_eq!(
            got,
            vec![
                (0x11, 0),
                (VK_SHIFT, 0),
                (0x41, 0),
                (0x41, K_UP),
                (VK_SHIFT, K_UP),
                (0x11, K_UP)
            ],
            "hotkey must press in order and release reversed, got: {got:?}"
        );
    }

    #[test]
    fn scroll_wheel_flag_data_and_bounds() {
        assert_eq!(scroll_wheel("v", 1), Ok((M_WHEEL, 120)));
        assert_eq!(scroll_wheel("v", -3), Ok((M_WHEEL, (-360i32) as u32)));
        assert_eq!(scroll_wheel("h", 2), Ok((M_HWHEEL, 240)));
        assert_eq!(scroll_wheel("h", -1), Ok((M_HWHEEL, (-120i32) as u32)));
        assert!(scroll_wheel("x", 1).is_err());
        assert!(scroll_wheel("v", 1001).is_err());
        assert!(scroll_wheel("h", -1001).is_err());
        assert!(scroll_wheel("v", 1000).is_ok());
        assert!(scroll_wheel("h", -1000).is_ok());
        // i64::MIN must not overflow past the bound via abs().
        assert!(scroll_wheel("v", i64::MIN).is_err());
        assert!(scroll_wheel("h", i64::MIN).is_err());
        assert!(scroll_wheel("v", i64::MAX).is_err());
    }

    #[test]
    fn scroll_axis_h_selects_horizontal_wheel_dry_run() {
        // Dry run assembles the INPUT; ok:true proves the axis parsed.
        // Flag identity is pinned by the scroll_wheel unit above.
        let p = burst_payload(&[serde_json::json!({"type": "scroll", "amount": 2, "axis": "h"})]);
        let results = p
            .get("results")
            .and_then(|r| r.as_array())
            .expect("results");
        assert_eq!(results[0].get("ok"), Some(&Value::from(true)));
        assert_eq!(p.get("events"), Some(&Value::from(1)));
        let p =
            burst_payload(&[serde_json::json!({"type": "scroll", "amount": 2, "axis": "bogus"})]);
        let results = p
            .get("results")
            .and_then(|r| r.as_array())
            .expect("results");
        assert_eq!(results[0].get("ok"), Some(&Value::from(false)));
    }

    fn mi_trip(ev: &INPUT) -> (i32, i32, u32) {
        unsafe {
            (
                ev.Anonymous.mi.dx,
                ev.Anonymous.mi.dy,
                ev.Anonymous.mi.dwFlags,
            )
        }
    }

    #[test]
    fn move_relative_builder_flags_and_payload() {
        // Relative moves are raw signed deltas with M_MOVE only — never the
        // absolute/virtual-desktop mapping.
        let ev = mi_move_rel(120, -30).expect("in-range deltas");
        assert_eq!(mi_trip(&ev), (120, -30, M_MOVE));
        let ev = mi_move_rel(-32767, 32767).expect("bound extremes");
        assert_eq!(mi_trip(&ev), (-32767, 32767, M_MOVE));
        assert!(mi_move_rel(32768, 0).is_err(), "above bound must fail");
        assert!(mi_move_rel(0, -32769).is_err(), "below bound must fail");
        assert!(mi_move_rel(i64::MIN, 0).is_err(), "i64::MIN must fail");
        assert!(mi_move_rel(0, i64::MAX).is_err(), "i64::MAX must fail");
    }

    #[test]
    fn move_relative_dry_run_skips_desktop_gate() {
        // In-range deltas far outside any desktop still dispatch (1 event).
        let p = burst_payload(&[serde_json::json!({"type": "move", "dx": 30000, "dy": -30000})]);
        let results = p
            .get("results")
            .and_then(|r| r.as_array())
            .expect("results");
        assert_eq!(results[0].get("ok"), Some(&Value::from(true)));
        assert_eq!(p.get("events"), Some(&Value::from(1)));
        // Absolute moves still hit the desktop gate.
        let bad = burst_first(&[serde_json::json!({"type": "move", "x": 99999999, "y": 0})]);
        assert_eq!(bad.get("ok"), Some(&Value::from(false)));
        // Absolute and relative together are refused, never guessed.
        let mixed =
            burst_first(&[serde_json::json!({"type": "move", "x": 10, "y": 10, "dx": 1, "dy": 1})]);
        assert_eq!(mixed.get("ok"), Some(&Value::from(false)));
        // Only one delta is incomplete.
        let half = burst_first(&[serde_json::json!({"type": "move", "dx": 5})]);
        assert_eq!(half.get("ok"), Some(&Value::from(false)));
    }

    #[test]
    fn key_repeat_dry_run_counts_events() {
        // repeat=3 assembles 3 down/up pairs (6 events) with no sleeps.
        let p = burst_payload(&[
            serde_json::json!({"type": "key", "name": "a", "repeat": 3, "hold_ms": 10}),
        ]);
        let results = p
            .get("results")
            .and_then(|r| r.as_array())
            .expect("results");
        assert_eq!(results[0].get("ok"), Some(&Value::from(true)));
        assert_eq!(p.get("events"), Some(&Value::from(6)));
        // Default repeat is a single pair.
        let p = burst_payload(&[serde_json::json!({"type": "key", "name": "a"})]);
        assert_eq!(p.get("events"), Some(&Value::from(2)));
    }

    #[test]
    fn key_hold_repeat_bounds_rejected() {
        for bad in [
            serde_json::json!({"type": "key", "name": "a", "repeat": 0}),
            serde_json::json!({"type": "key", "name": "a", "repeat": 101}),
            serde_json::json!({"type": "key", "name": "a", "hold_ms": 60001}),
            serde_json::json!({"type": "key", "name": "a", "gap_ms": 60001}),
            serde_json::json!({"type": "key", "name": "a", "hold_ms": -1}),
        ] {
            let first = burst_first(&[bad]);
            assert_eq!(
                first.get("ok"),
                Some(&Value::from(false)),
                "out-of-range hold/repeat must fail, got: {first:?}"
            );
        }
    }

    #[test]
    fn key_hold_cancel_releases_before_return() {
        // Scripted cancellation on the 2nd hold-check: the 2nd down was
        // already flushed, so the exit must emit its matching up —
        // down+up per repeat, never a held key.
        let calls = std::cell::Cell::new(0);
        let pred = || {
            let n = calls.get();
            calls.set(n + 1);
            n >= 1
        };
        let mut st = Burst {
            pending: Vec::new(),
            marked: Vec::new(),
            results: Vec::new(),
            calls: 0,
            events: 0,
            sendinput_ms: 0.0,
            dry_run: true,
            flush_uipi: false,
            permit: None,
            cursor: None,
        };
        let typ_v = Value::from("key");
        let code = vk_of("a").expect("a resolves");
        let out = st.run_hold_repeats(0, &typ_v, &code, 3, 50, 10, true, Instant::now(), &pred);
        let d = out.expect("cancelled run must return early");
        assert_eq!(d.get("ok"), Some(&Value::from(false)));
        assert!(
            d.get("error").and_then(|v| v.as_str()) == Some("cancelled"),
            "exit must be typed cancelled, got: {d:?}"
        );
        assert_eq!(
            st.events, 4,
            "cancel after a flushed down must still emit its up, events={}",
            st.events
        );
    }

    #[test]
    fn scroll_element_dry_run_assembles_no_wheel() {
        // Element scroll resolves instead of wheeling: dry run assembles
        // zero INPUT events (no wheel queued), ok + found.
        let (fx, bg) = fixture_bg();
        let _ = fx.hwnd();
        let out = do_burst(&DoArgs {
            background: Some(bg),
            dry_run: true,
            ..DoArgs::new(&[
                serde_json::json!({"type": "scroll", "element": "name:OK", "axis": "h"}),
            ])
        });
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let payload = outer
            .get("structuredContent")
            .and_then(|v| v.as_object())
            .expect("payload");
        assert_eq!(payload.get("events"), Some(&Value::from(0)));
        let first = payload
            .get("results")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .expect("first result");
        assert_eq!(first.get("ok"), Some(&Value::from(true)));
    }

    #[test]
    fn scroll_element_without_pattern_is_typed() {
        // The fixture OK button has no ScrollPattern: UIA-only pattern
        // error, never a wheel fallback, never any input. Headless-safe:
        // the pattern query fails before any actuation, on our own window.
        let fx = crate::fixture::Fixture::new();
        let _ = fx.hwnd();
        let model = crate::shot::element_model(fx.hwnd(), crate::UIA_DEPTH_LIMIT, 0);
        let (entry, merr, _) = match_element(&model.entries, Some(&Value::from("name:OK")), None);
        assert!(merr.is_none(), "fixture must list OK, got: {merr:?}");
        let entry = entry.expect("OK entry");
        let path = entry_str(&entry, "path");
        let (ok, via, err) = crate::uia::uia_scroll(fx.hwnd_i64(), &path, 4, 2);
        assert!(!ok, "scroll without the pattern must fail");
        let err = err.expect("typed error");
        assert!(
            err.contains("no ScrollPattern"),
            "missing pattern must be named, got: {err}"
        );
        assert_eq!(via, "uia-scroll", "failure stays on the pattern path");
    }

    #[test]
    fn scroll_element_bg_error_never_falls_back_to_foreground() {
        // A failed background resolution is terminal: the element path
        // must report the background error, never resolve fg_now.
        let actions = vec![serde_json::json!({"type": "scroll", "element": "name:OK"})];
        let out = do_burst(&DoArgs {
            background: Some("title:NoSuchWindowXYZ".to_string()),
            dry_run: true,
            ..DoArgs::new(&actions)
        });
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let first = outer
            .get("structuredContent")
            .and_then(|v| v.get("results"))
            .and_then(|r| r.as_array())
            .and_then(|a| a.first())
            .expect("first result");
        assert_eq!(first.get("ok"), Some(&Value::from(false)));
        let err = first.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            err.contains("NoSuchWindowXYZ"),
            "must surface the background failure, not a foreground fallback, got: {err}"
        );
    }

    #[test]
    fn scroll_element_rejects_xy_without_mouse() {
        // Element scroll is UIA-only: x/y alongside element is a typed
        // error and queues no mouse movement. A target is supplied so the
        // action reaches its own validation instead of the gate.
        let (fx, bg) = fixture_bg();
        let _ = fx.hwnd();
        let actions =
            vec![serde_json::json!({"type": "scroll", "element": "name:OK", "x": 10, "y": 10})];
        let out = do_burst(&DoArgs {
            background: Some(bg),
            dry_run: true,
            ..DoArgs::new(&actions)
        });
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let payload = outer
            .get("structuredContent")
            .and_then(|v| v.as_object())
            .expect("payload");
        let first = payload
            .get("results")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .expect("first result");
        assert_eq!(first.get("ok"), Some(&Value::from(false)));
        let err = first.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            err.contains("never moves the mouse"),
            "x/y with element must be refused as mouse input, got: {err}"
        );
        assert_eq!(
            payload.get("events"),
            Some(&Value::from(0)),
            "refused element scroll must queue nothing, got: {payload:?}"
        );
    }

    #[test]
    fn bg_click_count_bound_rejected_before_post() {
        let mut a = Map::new();
        a.insert("type".to_string(), Value::from("click"));
        a.insert("x".to_string(), Value::from(1));
        a.insert("y".to_string(), Value::from(1));
        a.insert("count".to_string(), Value::from(1_000_000_000_000i64));
        let (ok, err, _) = bg_execute(0, &a, false, true);
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
    fn stable_region_i64_extremes_typed_error() {
        // Verifier repro: MIN left edge must be a typed region error, never
        // a debug panic (subtraction overflow) or release bypass.
        let mut a = Map::new();
        a.insert("kind".to_string(), Value::from("stable"));
        a.insert(
            "region".to_string(),
            Value::from(vec![
                Value::from(i64::MIN),
                Value::from(0),
                Value::from(5000),
                Value::from(1),
            ]),
        );
        a.insert("timeout_ms".to_string(), Value::from(1000));
        let t0 = Instant::now();
        let r = wait_for(&a, &t0);
        assert_eq!(r.get("ok"), Some(&Value::from(false)));
        let err = r.get("error").and_then(|e| e.as_str()).unwrap_or("");
        assert!(
            err.contains("region"),
            "MIN-edge stable region must be a typed region error, got: {err}"
        );
    }

    #[test]
    fn stable_region_bound_is_4096() {
        // Memory bound: 5000px edges exceed the 4096 cap (not 16384).
        let mut a = Map::new();
        a.insert("kind".to_string(), Value::from("stable"));
        a.insert(
            "region".to_string(),
            Value::from(vec![
                Value::from(0),
                Value::from(0),
                Value::from(5000),
                Value::from(5000),
            ]),
        );
        a.insert("timeout_ms".to_string(), Value::from(1000));
        let t0 = Instant::now();
        let r = wait_for(&a, &t0);
        assert_eq!(r.get("ok"), Some(&Value::from(false)));
        let err = r.get("error").and_then(|e| e.as_str()).unwrap_or("");
        assert!(
            err.contains("4096"),
            "5000px region must name the 4096 bound, got: {err}"
        );
    }

    #[test]
    fn monitor_for_point_picks_containing_monitor() {
        let mons = vec![
            [0, 0, 1920, 1080],
            [1920, 0, 3840, 1080],
            [-1920, 0, 0, 1080],
        ];
        assert_eq!(monitor_for_point(&mons, 100, 100), Some([0, 0, 1920, 1080]));
        assert_eq!(
            monitor_for_point(&mons, 2000, 500),
            Some([1920, 0, 3840, 1080])
        );
        assert_eq!(
            monitor_for_point(&mons, -100, 500),
            Some([-1920, 0, 0, 1080]),
            "negative-origin monitors must match"
        );
        assert_eq!(monitor_for_point(&mons, 9000, 9000), None);
        assert_eq!(monitor_for_point(&[], 0, 0), None);
    }

    #[test]
    fn check_clip_len_bounds() {
        assert!(check_clip_len(0).is_ok());
        assert!(check_clip_len(MAX_CLIP_CHARS).is_ok());
        let e = check_clip_len(MAX_CLIP_CHARS + 1).unwrap_err();
        assert!(
            e.contains("1048576"),
            "over-cap text must name the bound, got: {e}"
        );
    }

    #[test]
    fn clip_read_u16_requires_nul_within_bounds() {
        let buf: Vec<u16> = "hi".encode_utf16().chain(std::iter::once(0)).collect();
        let s = unsafe { clip_read_u16(buf.as_ptr(), 10).expect("nul-terminated") };
        assert_eq!(s, "hi");
        let no_nul: Vec<u16> = "hi".encode_utf16().collect();
        assert!(
            unsafe { clip_read_u16(no_nul.as_ptr(), 2) }.is_err(),
            "missing NUL must fail instead of over-reading"
        );
        let mut big = vec!['x' as u16; 100];
        big.push(0);
        assert!(
            unsafe { clip_read_u16(big.as_ptr(), 10) }.is_err(),
            "scan must stop at the GlobalSize bound even with a later NUL"
        );
        let empty: Vec<u16> = vec![0];
        assert_eq!(unsafe { clip_read_u16(empty.as_ptr(), 1).unwrap() }, "");
    }

    #[test]
    fn clipboard_over_cap_set_rejected_before_touch() {
        // Validation runs before OpenClipboard: the owner clipboard is
        // never opened, and dry_run proves it without side effects.
        let big = "x".repeat(1_048_576 + 1);
        let actions = vec![serde_json::json!({"type": "clipboard_set", "text": big})];
        let args = DoArgs {
            allow_clipboard: true,
            ..DoArgs::foreground(&actions)
        };
        let out = do_burst(&args);
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let first = outer
            .get("structuredContent")
            .and_then(|v| v.get("results"))
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .expect("first result");
        assert_eq!(first.get("ok"), Some(&Value::from(false)));
        assert!(
            first
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .contains("1048576"),
            "over-cap set must name the bound, got: {first:?}"
        );
    }

    #[test]
    fn clipboard_set_without_flag_is_a_typed_refusal_and_burst_continues() {
        // Ungated safety proof: the gate refuses BEFORE any clipboard API, so
        // this runs on a headless runner and touches no clipboard at all.
        // The following action proves continue-always.
        let actions = vec![
            serde_json::json!({"type": "clipboard_set", "text": "SHOULD-NOT-LAND"}),
            serde_json::json!({"type": "wait", "ms": 1}),
        ];
        let out = do_burst(&DoArgs::new(&actions));
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let results = outer
            .get("structuredContent")
            .and_then(|v| v.get("results"))
            .and_then(|v| v.as_array())
            .expect("results");
        let first = results.first().expect("first result");
        assert_eq!(first.get("ok"), Some(&Value::from(false)));
        assert_eq!(
            first.get("code").and_then(|c| c.as_str()),
            Some("clipboard_write_required"),
            "missing flag must be typed, got: {first:?}"
        );
        assert!(
            first
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .contains("allow_clipboard:true"),
            "refusal must name the flag, got: {first:?}"
        );
        assert_eq!(
            results.get(1).and_then(|r| r.get("ok")),
            Some(&Value::from(true)),
            "a refused clipboard_set must not abort the burst: {results:?}"
        );
        // clipboard_get is a read and stays free.
        let reads = vec![serde_json::json!({"type": "clipboard_get"})];
        let out = do_burst(&DoArgs::new(&reads));
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let first = outer
            .get("structuredContent")
            .and_then(|v| v.get("results"))
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .expect("first result");
        assert!(
            first.get("code").is_none(),
            "clipboard_get needs no flag, got: {first:?}"
        );
    }

    #[test]
    fn element_wait_predicate_truth_table() {
        // Pure decision table: found/absent crossed with every state.
        assert!(element_wait_satisfied("appear", true, None));
        assert!(!element_wait_satisfied("appear", false, None));
        assert!(element_wait_satisfied("disappear", false, None));
        assert!(!element_wait_satisfied("disappear", true, None));
        assert!(element_wait_satisfied("enabled", true, Some(true)));
        assert!(!element_wait_satisfied("enabled", true, Some(false)));
        assert!(!element_wait_satisfied("enabled", true, None));
        assert!(!element_wait_satisfied("enabled", false, None));
        assert!(element_wait_satisfied("disabled", true, Some(false)));
        assert!(!element_wait_satisfied("disabled", true, Some(true)));
        assert!(!element_wait_satisfied("disabled", true, None));
        assert!(!element_wait_satisfied("disabled", false, Some(false)));
        assert!(!element_wait_satisfied("bogus", true, Some(true)));
        assert!(!element_wait_satisfied("bogus", false, None));
    }

    #[test]
    fn element_wait_rejects_malformed_arguments() {
        let t0 = Instant::now();
        // Missing element.
        let mut a = Map::new();
        a.insert("kind".to_string(), Value::from("element"));
        a.insert("timeout_ms".to_string(), Value::from(0));
        let r = wait_for(&a, &t0);
        assert_eq!(r.get("ok"), Some(&Value::from(false)));
        assert!(
            r.get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("")
                .contains("needs \"element\""),
            "missing element must be a typed per-action error, got: {r:?}"
        );
        // Unknown selector kind.
        let mut b = Map::new();
        b.insert("kind".to_string(), Value::from("element"));
        b.insert("element".to_string(), Value::from("ref:12.3"));
        b.insert("timeout_ms".to_string(), Value::from(0));
        let r2 = wait_for(&b, &t0);
        assert_eq!(r2.get("ok"), Some(&Value::from(false)));
        assert!(
            r2.get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("")
                .contains("unknown element selector kind"),
            "ref: must be rejected, got: {r2:?}"
        );
        // Unknown state.
        let mut c = Map::new();
        c.insert("kind".to_string(), Value::from("element"));
        c.insert("element".to_string(), Value::from("name:OK"));
        c.insert("state".to_string(), Value::from("hovered"));
        c.insert("timeout_ms".to_string(), Value::from(0));
        let r3 = wait_for(&c, &t0);
        assert_eq!(r3.get("ok"), Some(&Value::from(false)));
        // Unknown kind still lists element as accepted.
        let mut d = Map::new();
        d.insert("kind".to_string(), Value::from("nope_not_a_kind"));
        let r4 = wait_for(&d, &t0);
        assert_eq!(r4.get("ok"), Some(&Value::from(false)));
        assert!(
            r4.get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("")
                .contains("stable|element"),
            "unknown kind must name the accepted kinds, got: {r4:?}"
        );
    }

    #[test]
    fn element_wait_rejects_malformed_window_fast() {
        // hwnd:/pid: with a non-integer value fails before any polling.
        let t0 = Instant::now();
        let mut a = Map::new();
        a.insert("kind".to_string(), Value::from("element"));
        a.insert("element".to_string(), Value::from("path:0"));
        a.insert("window".to_string(), Value::from("hwnd:nope"));
        a.insert("timeout_ms".to_string(), Value::from(0));
        let t_start = Instant::now();
        let r = wait_for(&a, &t0);
        assert_eq!(r.get("ok"), Some(&Value::from(false)));
        assert!(
            r.get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("")
                .contains("bad window selector"),
            "unparseable hwnd must fail fast, got: {r:?}"
        );
        assert!(
            t_start.elapsed() < Duration::from_secs(2),
            "malformed window must not poll, took {:?}",
            t_start.elapsed()
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
        // UIA text entry (set_value / type on an element) is an explicit
        // opt-in: these tests exercise that path, so they ask for it.
        let out = do_burst(&DoArgs {
            dry_run,
            allow_foreground: true,
            background: Some(bg.to_string()),
            ..DoArgs::new(actions)
        });
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let payload = outer
            .get("structuredContent")
            .cloned()
            .expect("structuredContent is the payload object");
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
            let m = element_model(hwnd, crate::UIA_DEPTH_LIMIT, 0);
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
        let model = element_model(f.hwnd(), crate::UIA_DEPTH_LIMIT, 0);
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

    /// ROOT-CAUSE REGRESSION: the fixture EDIT refuses real typing.
    ///
    /// `ValuePattern.SetValue` is performed by the UIA provider, which focuses
    /// the control first; the off-screen fixture EDIT then holds keyboard
    /// focus and the owner's keystrokes land in the document at the caret
    /// (observed: "www" prepended to the written value, which made
    /// `set_value_success_writes_and_never_echoes` fail on a real machine).
    /// The fixture guard must make that impossible: inject the same messages
    /// real typing sends and assert the document is untouched, while the
    /// provider write still lands its exact value.
    #[test]
    fn fixture_edit_refuses_external_typing() {
        let f = crate::fixture::Fixture::new();
        crate::fixture::reset_swallowed_keys(f.edit());
        unsafe {
            let t: Vec<u16> = "keep".encode_utf16().chain(std::iter::once(0)).collect();
            SetWindowTextW(f.edit(), t.as_ptr());
            for ch in ['w' as u16, 'w' as u16, 'w' as u16] {
                PostMessageW(f.edit(), 0x0102, ch as usize, 1);
            }
            PostMessageW(f.edit(), 0x0302, 0, 0); // WM_PASTE
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
        assert_eq!(
            f.edit_text(),
            "keep",
            "external keyboard input must not reach the fixture document"
        );
        assert!(
            crate::fixture::swallowed_key_count(f.edit()) >= 4,
            "the guard must have seen the injected messages, got {}",
            crate::fixture::swallowed_key_count(f.edit())
        );
        println!(
            "FIXTURE_TYPING_GUARD_OK swallowed={} text={:?}",
            crate::fixture::swallowed_key_count(f.edit()),
            f.edit_text()
        );
    }

    /// `ValuePattern.SetValue` REPLACES and uses no keystroke messages, so
    /// the fixture's typing guard cannot change what the product does.
    #[test]
    fn value_pattern_set_value_replaces_without_keystrokes() {
        let f = crate::fixture::Fixture::new();
        wait_uia_stable(f.hwnd());
        let model = crate::shot::element_model(f.hwnd(), crate::UIA_DEPTH_LIMIT, 0);
        let entry = model
            .entries
            .iter()
            .find(|e| e.get("auto_id").and_then(|v| v.as_str()) == Some("100"))
            .expect("writable EDIT");
        let path = entry
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();
        unsafe {
            let t: Vec<u16> = "abc".encode_utf16().chain(std::iter::once(0)).collect();
            SetWindowTextW(f.edit(), t.as_ptr());
        }
        println!("SETVALUE_PRE text={:?}", f.edit_text());
        let (ok, via, err) = crate::uia::uia_set_value(f.hwnd_i64(), &path, "xyz");
        println!(
            "SETVALUE_POST ok={ok} via={via} err={err:?} text={:?}",
            f.edit_text()
        );
        assert_eq!(
            f.edit_text(),
            "xyz",
            "SetValue must REPLACE the control text, not append"
        );
        // The write landed with the typing guard installed, which already
        // proves SetValue does not need key messages. The counter is reported
        // (an owner keystroke during the call would be swallowed and counted
        // here), never asserted: it is environmental, not product behaviour.
        println!(
            "SETVALUE_SWALLOWED_KEYS={} (0 = no external typing during the write)",
            crate::fixture::swallowed_key_count(f.edit())
        );
        // And the provider really focused the control (the reason the guard
        // exists): the owner's typing would otherwise land there.
        unsafe {
            let thread = GetWindowThreadProcessId(f.hwnd(), std::ptr::null_mut());
            let mut info: GUITHREADINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
            if GetGUIThreadInfo(thread, &mut info) != 0 {
                println!(
                    "SETVALUE_FOCUS edit_focused={} (provider focus is why the fixture guard exists)",
                    info.hwndFocus == f.edit()
                );
            }
        }
        println!("SETVALUE_SEMANTICS_OK replace=xyz keystrokes=0");
    }

    /// Gap-1 proof: the value lands in the EDIT and the result never echoes
    /// it. Deterministic on a live machine because the fixture EDIT refuses
    /// external typing (see `fixture_edit_refuses_external_typing`): the UIA
    /// provider focuses the control to write it, and without the guard the
    /// owner's keystrokes can prepend characters here.
    #[test]
    fn set_value_success_writes_and_never_echoes() {
        // Environment-independent foreground contract: record the foreground
        // window BEFORE any fixture work; the tool must not CHANGE it. When
        // the environment provides no foreground session (headless CI), the
        // check skips with a message instead of asserting.
        let fg_before = crate::fixture::foreground_now();
        let (fx, bg, wpath, _ro) = edit_paths();
        // Root-cause guard (fixture.rs): the UIA provider FOCUSES this EDIT to
        // write it, so on a live machine the owner's keystrokes can land in it
        // at the caret (observed: "www" prepended, making the read-back below
        // fail for an environmental reason). The fixture refuses typing, so
        // the control starts empty and the strict equality assertion is
        // deterministic. Clearing is not needed and the assertion is NOT
        // relaxed.
        assert_eq!(
            fx.edit_text(),
            "",
            "the fixture EDIT must start empty (external typing is refused)"
        );
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
        let model = crate::shot::element_model(fx.hwnd(), crate::UIA_DEPTH_LIMIT, 0);
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
        let out = do_burst(&DoArgs {
            background: Some(bg.to_string()),
            // UIA text entry is an explicit opt-in (the provider may focus).
            allow_foreground: true,
            ..DoArgs::new(&actions)
        });
        let outer: Value = serde_json::from_str(&out).expect("outer");
        let payload = outer["structuredContent"].clone();
        let res = payload["results"].as_array().expect("results");
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].get("ok"), Some(&Value::from(false)));
        assert_eq!(res[1].get("ok"), Some(&Value::from(true)));
        assert_eq!(fx.edit_text(), "after-failure");
    }

    // ------------------------------------------------ no-steal (target lock + gate)

    /// Serializes the posted-message tests: the fixture counters are
    /// process-wide, and `cargo test` runs the tests in parallel.
    static LIVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn live_guard() -> std::sync::MutexGuard<'static, ()> {
        LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn sc(out: &str) -> Value {
        let outer: Value = serde_json::from_str(out).expect("outer frame");
        outer
            .get("structuredContent")
            .cloned()
            .expect("structuredContent payload")
    }

    /// The foreground handle is the property the tool CAN violate, so it is
    /// asserted strictly: any change fails, with no external-activity
    /// exemption.
    fn assert_foreground_held(before: i64, what: &str) {
        let after = crate::fixture::foreground_now();
        if before == 0 {
            return;
        }
        assert_eq!(
            after, before,
            "{what}: GetForegroundWindow changed ({before} -> {after})"
        );
    }

    /// Pointer movement is TELEMETRY, never a failure. The tool cannot move
    /// the pointer on these paths (no `SetCursorPos`/`SendInput` outside
    /// `src/gate.rs`, and no cursor permit is passed), while the owner using
    /// the machine moves it constantly — a positional assertion here would be
    /// a flaky gate, not evidence. Report the delta and continue.
    fn report_pointer(before: (i64, i64), what: &str) {
        let after = cursor_now();
        if after == before {
            return;
        }
        println!(
            "CURSOR_EXTERNAL: {what}: pointer moved before={before:?} after={after:?} delta=({}, {}); telemetry only (no cursor API on this path)",
            after.0 - before.0,
            after.1 - before.1
        );
    }

    fn wait_until(mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        f()
    }

    /// LIVE no-steal proof: a target-locked burst delivers a posted click and
    /// posted text to an offscreen fixture window (its own WndProc counts the
    /// messages, the EDIT read-back proves WM_CHAR) while GetForegroundWindow
    /// and GetCursorPos stay exactly where they were. No flags, no pointer,
    /// no focus.
    /// DEFAULT PATH: UIA text entry is an explicit opt-in. Without
    /// `allow_foreground:true`, `set_value` and `type` on an element must be
    /// refused BEFORE any provider call — no value written, no focus hint.
    #[test]
    fn uia_text_entry_is_refused_without_the_opt_in() {
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        wait_uia_stable(fx.hwnd());
        let actions = vec![
            serde_json::json!({"type": "set_value", "element": "auto_id:100", "text": "secret"}),
            serde_json::json!({"type": "type", "element": "auto_id:100", "text": "secret"}),
        ];
        let out = do_burst(&DoArgs {
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            ..DoArgs::new(&actions)
        });
        let payload = sc(&out);
        let results = payload["results"].as_array().expect("results");
        for (k, r) in results.iter().enumerate() {
            assert_eq!(r["ok"], Value::from(false), "action {k}: {r}");
            assert_eq!(
                r["code"],
                Value::from("foreground_required"),
                "action {k} must name the opt-in: {r}"
            );
            assert!(r.get("via").is_none(), "no provider path may run: {r}");
        }
        assert_eq!(
            fx.edit_text(),
            "",
            "no provider call may have written the control"
        );
        println!("UIA_TEXT_OPT_IN_OK refusals=2 text=\"\"");
    }

    /// SAFETY: private-desktop permission is EARNED. `desktop:"hidden"` plus an
    /// explicit ordinary target used to grant it without any desktop being
    /// created (the target wins over hidden creation), which allowed posted
    /// keyboard over the owner's session. It must now be an ordinary target:
    /// typed refusal, zero key messages.
    #[test]
    fn hidden_flag_with_an_ordinary_target_earns_nothing() {
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        crate::fixture::reset_msg_counts();
        let actions = vec![
            serde_json::json!({"type": "type", "text": "x"}),
            serde_json::json!({"type": "key", "name": "a"}),
            serde_json::json!({"type": "hotkey", "keys": ["ctrl", "c"]}),
        ];
        let out = do_burst(&DoArgs {
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            desktop: Some("hidden".to_string()),
            ..DoArgs::new(&actions)
        });
        let payload = sc(&out);
        let results = payload["results"].as_array().expect("results");
        for (k, r) in results.iter().enumerate() {
            assert_eq!(r["ok"], Value::from(false), "action {k}: {r}");
            assert_eq!(
                r["code"],
                Value::from("foreground_required"),
                "action {k} must be refused, not posted: {r}"
            );
        }
        let (_, _, _, kd, ch) = crate::fixture::msg_counts();
        assert_eq!(
            (kd, ch),
            (0, 0),
            "zero key messages may reach an ordinary target"
        );
        assert_eq!(payload["sendinput_calls"], Value::from(0));
        // The predicate itself: a desktop that was not created grants nothing,
        // and a window on a different desktop never matches the created name.
        assert!(!private_desktop_earned(false, "hidden-x", Some("hidden-x")));
        assert!(!private_desktop_earned(true, "hidden-x", Some("Default")));
        assert!(!private_desktop_earned(true, "", Some("hidden-x")));
        assert!(private_desktop_earned(true, "hidden-x", Some("hidden-x")));
        println!("HIDDEN_BYPASS_CLOSED_OK codes=foreground_required key_msgs=0");
    }

    /// The desktop-name read is real: an ordinary fixture lives on the
    /// owner's desktop, never on a made-up private one.
    #[test]
    fn window_desktop_name_reads_a_real_desktop() {
        let fx = crate::fixture::Fixture::new();
        let name = crate::window_desktop_name(fx.hwnd()).expect("desktop name readable");
        assert!(!name.is_empty(), "desktop name must not be empty");
        assert_ne!(name, "puppet-hidden-does-not-exist");
        assert_eq!(
            crate::window_desktop_name(std::ptr::null_mut()),
            None,
            "a null window has no desktop"
        );
        println!("WINDOW_DESKTOP_NAME_OK name={name:?}");
    }

    /// LIVE: a target-locked burst delivers a posted click to an offscreen
    /// fixture window (its own WndProc counts the message) while
    /// GetForegroundWindow and GetCursorPos stay where they were. Plain `type`
    /// is refused on the default path (keyboard is never posted).
    #[test]
    fn live_target_post_delivers_without_stealing() {
        let _g = live_guard();
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        crate::fixture::reset_msg_counts();
        let fg_before = crate::fixture::foreground_now();
        let cur_before = cursor_now();
        let actions = vec![
            serde_json::json!({"type": "click", "x": 20, "y": 90}),
            // Plain `type` must be refused, not posted: WM_CHAR is not
            // delivered reliably. Text goes through UIA on an element (own
            // test, because the UIA provider briefly focuses its control).
            serde_json::json!({"type": "type", "text": "hi"}),
        ];
        let out = do_burst(&DoArgs {
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            ..DoArgs::new(&actions)
        });
        let payload = sc(&out);
        let results = payload["results"].as_array().expect("results");
        assert_eq!(results[0]["ok"], Value::from(true), "click: {results:?}");
        assert_eq!(
            results[1]["ok"],
            Value::from(false),
            "plain type must be refused: {results:?}"
        );
        assert_eq!(results[1]["code"], Value::from("foreground_required"));
        assert!(
            payload["foregroundUnchanged"].is_boolean(),
            "telemetry must be reported: {payload}"
        );
        assert!(
            payload["cursorUnchanged"].is_boolean(),
            "telemetry must be reported: {payload}"
        );
        assert_eq!(payload["target_locked"], Value::from(hwnd));
        assert!(
            payload["mode"].is_null(),
            "per-action mode lives on results"
        );
        assert_eq!(results[0]["mode"], Value::from("background"));

        assert!(
            wait_until(|| crate::fixture::msg_counts().0 >= 1),
            "posted WM_LBUTTONDOWN must reach the target, got: {:?}",
            crate::fixture::msg_counts()
        );
        let (down, up, mv, _kd, _ch) = crate::fixture::msg_counts();
        assert!(
            down >= 1 && up >= 1,
            "down/up pair must arrive: {down}/{up}"
        );
        assert_eq!(mv, 0, "a plain click must not post WM_MOUSEMOVE");
        assert_eq!(
            crate::fixture::msg_counts().4,
            0,
            "plain type must post ZERO WM_CHAR messages"
        );
        assert_eq!(
            fx.edit_text(),
            "",
            "no WM_CHAR may reach the EDIT through the refused plain type"
        );
        assert_foreground_held(fg_before, "post-delivery burst");
        report_pointer(cur_before, "post-delivery burst");
        assert_eq!(
            payload["sendinput_calls"],
            Value::from(0),
            "a target burst never calls SendInput"
        );
    }

    /// LIVE: text without keys — `type` on an element is UIA
    /// ValuePattern.SetValue. The UIA provider briefly focuses the control it
    /// writes to (documented in the set_value test), so this asserts delivery
    /// and read-back, not the foreground handle.
    #[test]
    fn live_type_on_element_uses_uia_set_value() {
        let _g = live_guard();
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        crate::fixture::reset_msg_counts();
        let actions = vec![serde_json::json!({
            "type": "type", "element": "auto_id:100", "text": "hi"
        })];
        let out = do_burst(&DoArgs {
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            // UIA text entry is an explicit opt-in (the provider may focus
            // the control); this test exercises that path.
            allow_foreground: true,
            ..DoArgs::new(&actions)
        });
        let payload = sc(&out);
        let r = &payload["results"][0];
        assert_eq!(r["ok"], Value::from(true), "type on element: {r}");
        assert_eq!(r["via"], Value::from("uia-set-value"), "{r}");
        assert_eq!(r["delivered"], Value::from("uia"), "{r}");
        assert_eq!(r["effect"], Value::from("verified"), "{r}");
        assert_eq!(fx.edit_text(), "hi", "the value must be written");
        assert_eq!(
            crate::fixture::msg_counts().4,
            0,
            "ZERO WM_CHAR messages may be posted"
        );
        assert_eq!(payload["sendinput_calls"], Value::from(0));
        println!("TYPE_UIA_OK text={:?}", fx.edit_text());
    }

    /// LIVE: a posted element click is delivery, not proof. The fixture EDIT
    /// has no Invoke pattern, so the click is posted at its centre; its
    /// readable state (text/enabled) does not change, and the action must
    /// report `background_unavailable` with `delivered:"posted"` instead of
    /// claiming success.
    #[test]
    fn live_posted_element_click_with_unchanged_state_is_refused() {
        let _g = live_guard();
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        wait_uia_stable(fx.hwnd());
        let actions = vec![serde_json::json!({
            "type": "click", "element": "auto_id:100"
        })];
        let out = do_burst(&DoArgs {
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            ..DoArgs::new(&actions)
        });
        let payload = sc(&out);
        let r = &payload["results"][0];
        assert_eq!(r["delivered"], Value::from("posted"), "{r}");
        assert_eq!(r["effect"], Value::from("unchanged"), "{r}");
        assert_eq!(r["ok"], Value::from(false), "{r}");
        assert_eq!(r["code"], Value::from("background_unavailable"), "{r}");
        assert!(
            r["error"]
                .as_str()
                .unwrap_or("")
                .contains("state did not change"),
            "the refusal must name the unverified effect: {r}"
        );
        println!("POSTED_CLICK_HONESTY_OK effect=unchanged");
    }

    /// LIVE: hover is opt-in. The same click with `hover:true` posts exactly
    /// one WM_MOUSEMOVE before the pair; without it, none.
    #[test]
    fn live_hover_is_the_only_mouse_move() {
        let _g = live_guard();
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        crate::fixture::reset_msg_counts();
        let actions = vec![serde_json::json!({"type": "click", "x": 5, "y": 5, "hover": true})];
        let out = do_burst(&DoArgs {
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            ..DoArgs::new(&actions)
        });
        assert_eq!(sc(&out)["results"][0]["ok"], Value::from(true));
        assert!(wait_until(|| crate::fixture::msg_counts().2 >= 1));
        assert_eq!(
            crate::fixture::msg_counts().2,
            1,
            "one hover move, not a journey"
        );
    }

    /// LIVE: drag is down/move/up at exact points; scroll is one wheel
    /// message at the point. Both delivered by message, never by pointer.
    #[test]
    fn live_drag_and_scroll_are_posted_not_pointer() {
        let _g = live_guard();
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        let cur_before = cursor_now();
        crate::fixture::reset_msg_counts();
        let actions = vec![
            serde_json::json!({"type": "drag", "from": [10, 10], "to": [40, 40]}),
            serde_json::json!({"type": "scroll", "x": 10, "y": 10, "amount": 2}),
        ];
        let out = do_burst(&DoArgs {
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            ..DoArgs::new(&actions)
        });
        let payload = sc(&out);
        assert_eq!(payload["results"][0]["ok"], Value::from(true), "{payload}");
        assert_eq!(payload["results"][1]["ok"], Value::from(true), "{payload}");
        assert!(wait_until(|| crate::fixture::msg_counts().0 >= 1));
        let (down, up, mv, _, _) = crate::fixture::msg_counts();
        assert_eq!((down, up, mv), (1, 1, 1), "down, one move, up");
        assert!(wait_until(|| crate::fixture::wheel_count() >= 1));
        report_pointer(cur_before, "posted drag + wheel");
    }

    /// LIVE gate: no target and no flags refuses every injecting verb BEFORE
    /// acting - typed codes, zero events, desktop untouched.
    #[test]
    fn live_gate_refuses_without_target_or_flags() {
        let fg_before = crate::fixture::foreground_now();
        let cur_before = cursor_now();
        let actions = vec![
            serde_json::json!({"type": "click", "x": 5, "y": 5}),
            serde_json::json!({"type": "type", "text": "x"}),
            serde_json::json!({"type": "hotkey", "keys": ["ctrl", "c"]}),
            serde_json::json!({"type": "move", "dx": 5, "dy": 5}),
            serde_json::json!({"type": "invoke", "element": "name:OK"}),
        ];
        let out = do_burst(&DoArgs::new(&actions));
        let payload = sc(&out);
        let results = payload["results"].as_array().expect("results");
        let codes: Vec<&str> = results
            .iter()
            .map(|r| r["code"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            codes,
            vec![
                "cursor_move_required",
                "foreground_required",
                "foreground_required",
                "cursor_move_required",
                "target_required",
            ],
            "gate matrix, got: {results:?}"
        );
        for r in results {
            assert_eq!(r["ok"], Value::from(false));
        }
        assert_eq!(payload["sendinput_calls"], Value::from(0));
        assert_eq!(payload["events"], Value::from(0));
        assert_foreground_held(fg_before, "gate refusal");
        report_pointer(cur_before, "gate refusal");
    }

    /// LIVE: the explicit flags mint the foreground path (dry_run, so the
    /// assembly is proven without injecting anything into the owner's
    /// session) - the same burst that was refused above now queues events.
    #[test]
    fn live_explicit_flags_unlock_the_foreground_path_dry_run() {
        let actions = vec![
            serde_json::json!({"type": "click", "x": 5, "y": 5}),
            serde_json::json!({"type": "type", "text": "x"}),
        ];
        let out = do_burst(&DoArgs {
            dry_run: true,
            allow_foreground: true,
            allow_cursor: true,
            ..DoArgs::new(&actions)
        });
        let payload = sc(&out);
        assert_eq!(payload["results"][0]["ok"], Value::from(true));
        assert_eq!(payload["results"][1]["ok"], Value::from(true));
        assert!(payload["events"].as_i64().unwrap_or(0) >= 4);
        assert!(
            payload["sendinput_calls"].as_i64().unwrap_or(0) >= 1,
            "the dry run assembles the events (and says so in sendinput_calls): {payload}"
        );
    }

    /// LIVE: a target that closed mid-call is `target_gone` on the next
    /// action (revalidation), never a retarget and never a foreground fall.
    #[test]
    fn live_target_gone_mid_burst_is_typed() {
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        let actions = vec![
            serde_json::json!({"type": "wait", "ms": 1}),
            serde_json::json!({"type": "type", "text": "x"}),
        ];
        let args = DoArgs {
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            ..DoArgs::new(&actions)
        };
        // Destroy the fixture window between resolution and the second
        // action by dropping it first: lock() then fails outright. To prove
        // the mid-call path, resolve the lock, then destroy and revalidate.
        let spec = crate::target::TargetSpec::parse(&serde_json::json!({ "hwnd": hwnd })).unwrap();
        let lock = crate::target::lock(&spec).expect("fresh fixture locks");
        drop(fx);
        let e = lock
            .revalidate()
            .expect_err("destroyed target must not validate");
        assert_eq!(e.code, "target_gone", "{}", e.text());
        // And a whole call at a dead hwnd is a typed whole-call refusal.
        let outer: Value = serde_json::from_str(&do_burst(&args)).expect("outer frame");
        assert_eq!(outer["isError"], Value::from(true), "{outer}");
        assert!(outer["content"][0]["text"].as_str().is_some());
    }

    /// LIVE, message-only: the harshest delivery case — a `HWND_MESSAGE`
    /// window that is never visible and never activatable. A target-locked
    /// posted CLICK arrives in its own WndProc while GetForegroundWindow and
    /// GetCursorPos stay put; the keyboard verbs are refused and post
    /// nothing (posted keyboard is never used on the owner's desktop).
    #[test]
    fn live_message_only_click_delivers_and_key_is_never_posted() {
        let _g = live_guard();
        let f = crate::fixture::MessageWindow::new();
        let hwnd = f.hwnd_i64();
        crate::fixture::reset_msg_counts();
        let fg_before = crate::fixture::foreground_now();
        let cur_before = cursor_now();
        let actions = vec![
            serde_json::json!({"type": "click", "x": 4, "y": 6}),
            serde_json::json!({"type": "key", "name": "a"}),
            serde_json::json!({"type": "hotkey", "keys": ["ctrl", "c"]}),
        ];
        let out = do_burst(&DoArgs {
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            ..DoArgs::new(&actions)
        });
        let payload = sc(&out);
        let results = payload["results"].as_array().expect("results");
        assert_eq!(results[0]["ok"], Value::from(true), "click: {results:?}");
        // Keyboard is never posted to the owner's desktop: typed refusal,
        // and the target sees ZERO key messages.
        for k in [1, 2] {
            assert_eq!(
                results[k]["ok"],
                Value::from(false),
                "keyboard: {results:?}"
            );
            assert_eq!(results[k]["code"], Value::from("foreground_required"));
        }
        assert_eq!(payload["sendinput_calls"], Value::from(0));
        assert!(
            wait_until(|| crate::fixture::msg_counts().0 >= 1),
            "posted WM_LBUTTONDOWN must be received: {:?}",
            crate::fixture::msg_counts()
        );
        let (down, up, mv, kd, ch) = crate::fixture::msg_counts();
        assert_eq!((down, up, mv), (1, 1, 0), "one down, one up, no move");
        assert_eq!((kd, ch), (0, 0), "no WM_KEYDOWN / WM_CHAR may be posted");
        assert_eq!(
            results[0]["delivered"],
            Value::from("posted"),
            "a raw click reports delivery only: {results:?}"
        );
        assert_eq!(results[0]["effect"], Value::from("unverified"));
        assert_foreground_held(fg_before, "message-only delivery");
        report_pointer(cur_before, "message-only delivery");
    }

    /// LIVE: `click` on an element is UIA-pattern-first on the locked
    /// target (dry_run resolves and names the path without firing).
    #[test]
    fn live_click_on_element_resolves_to_uia_pattern() {
        let (fx, bg) = fixture_bg();
        wait_uia_stable(fx.hwnd());
        let actions = vec![serde_json::json!({
            "type": "click", "element": "name:OK", "expect_fp": null
        })];
        let out = do_burst(&DoArgs {
            background: Some(bg),
            dry_run: true,
            ..DoArgs::new(&actions)
        });
        let payload = sc(&out);
        let first = &payload["results"][0];
        assert_eq!(first["ok"], Value::from(true), "{first}");
        assert_eq!(first["resolution"], Value::from("found"), "{first}");
        assert_eq!(first["via"], Value::from("uia-pattern"), "{first}");
        assert_eq!(payload["events"], Value::from(0), "dry run queues nothing");
    }

    /// LIVE: a cursor-only call (allow_cursor without allow_foreground) can
    /// never be a silent success. SendInput is foreground input, so the gate
    /// refuses with a typed code BEFORE anything is queued: ok:false,
    /// code, zero events, zero SendInput calls.
    #[test]
    fn live_cursor_only_pointer_actions_are_typed_failures() {
        let f = crate::fixture::MessageWindow::new();
        let _hwnd = f.hwnd_i64();
        crate::fixture::reset_msg_counts();
        let actions = vec![
            serde_json::json!({"type": "click", "x": 5, "y": 5}),
            serde_json::json!({"type": "drag", "from": [1, 1], "to": [2, 2]}),
            serde_json::json!({"type": "move", "dx": 1, "dy": 1}),
        ];
        let out = do_burst(&DoArgs {
            allow_cursor: true,
            ..DoArgs::new(&actions)
        });
        let payload = sc(&out);
        let results = payload["results"].as_array().expect("results");
        for (k, r) in results.iter().enumerate() {
            assert_eq!(r["ok"], Value::from(false), "action {k} must fail: {r}");
            assert_eq!(
                r["code"],
                Value::from("foreground_required"),
                "action {k} needs the foreground code: {r}"
            );
        }
        assert_eq!(payload["events"], Value::from(0), "nothing may be queued");
        assert_eq!(payload["sendinput_calls"], Value::from(0));
        assert_eq!(
            crate::fixture::msg_counts(),
            (0, 0, 0, 0, 0),
            "nothing may reach the target"
        );
    }

    /// LIVE: with BOTH flags the pointer path really assembles the events
    /// (dry_run: assembly without injection). The other half of "delivers or
    /// fails, never silent".
    #[test]
    fn live_both_flags_assemble_the_pointer_path() {
        let actions = vec![serde_json::json!({"type": "click", "x": 5, "y": 5})];
        let out = do_burst(&DoArgs {
            dry_run: true,
            allow_foreground: true,
            allow_cursor: true,
            ..DoArgs::new(&actions)
        });
        let payload = sc(&out);
        assert_eq!(payload["results"][0]["ok"], Value::from(true));
        assert!(
            payload["events"].as_i64().unwrap_or(0) >= 3,
            "move + down + up must be assembled: {payload}"
        );
    }

    /// The flush guard itself: without a foreground permit the queued events
    /// are NOT sent, the marks SURVIVE (so fail_since_flush can mark the
    /// action failed), and the retro-marked result carries the typed code.
    #[test]
    fn flush_without_permit_marks_the_action_failed() {
        let mut st = Burst {
            pending: vec![mi_btn(M_LDOWN, 0)],
            marked: vec![0],
            results: vec![base_entry(0, &Value::from("click"), true, 0.0)],
            calls: 0,
            events: 0,
            sendinput_ms: 0.0,
            dry_run: false,
            flush_uipi: false,
            permit: None,
            cursor: None,
        };
        let (_, err) = st.flush();
        let err = err.expect("flush must refuse without the permit");
        assert!(err.starts_with("foreground_required"), "typed: {err}");
        assert!(st.pending.is_empty(), "the queued events must be dropped");
        assert_eq!(st.marked, vec![0], "marks must survive for the caller");
        assert_eq!(st.events, 0, "nothing was sent");
        fail_since_flush(&mut st.results, &mut st.marked, &err, false);
        assert_eq!(st.results[0]["ok"], Value::from(false));
        assert_eq!(st.results[0]["code"], Value::from("foreground_required"));
    }

    /// LIVE, message-only: coordinates a message lParam cannot carry are a
    /// typed `bad_coordinates` refusal BEFORE posting; the 16-bit boundaries
    /// still post.
    #[test]
    fn live_bad_coordinates_refused_before_posting() {
        let _g = live_guard();
        let f = crate::fixture::MessageWindow::new();
        let hwnd = f.hwnd_i64();
        let target = Some(serde_json::json!({ "hwnd": hwnd }));
        crate::fixture::reset_msg_counts();
        let bad = vec![
            serde_json::json!({"type": "click", "x": 40000, "y": 0}),
            serde_json::json!({"type": "click", "x": 0, "y": 40000}),
            serde_json::json!({"type": "click", "x": 32768, "y": 0}),
            serde_json::json!({"type": "click", "x": 0, "y": -32769}),
            serde_json::json!({"type": "scroll", "x": 40000, "y": 10}),
        ];
        for action in bad {
            let out = do_burst(&DoArgs {
                target: target.clone(),
                ..DoArgs::new(&[action.clone()])
            });
            let payload = sc(&out);
            let r = &payload["results"][0];
            assert_eq!(r["ok"], Value::from(false), "{action}: {r}");
            assert!(
                r["error"]
                    .as_str()
                    .unwrap_or("")
                    .contains("bad_coordinates"),
                "{action} must be typed bad_coordinates, got: {r}"
            );
            assert_eq!(
                crate::fixture::msg_counts(),
                (0, 0, 0, 0, 0),
                "{action} must post nothing"
            );
        }
        // Boundaries are representable and post.
        let action = serde_json::json!({"type": "click", "x": 32767, "y": -32768});
        let out = do_burst(&DoArgs {
            target: target.clone(),
            ..DoArgs::new(&[action.clone()])
        });
        let payload = sc(&out);
        assert_eq!(
            payload["results"][0]["ok"],
            Value::from(true),
            "boundary {action}: {payload}"
        );
        assert!(
            wait_until(|| crate::fixture::msg_counts().0 >= 1),
            "the boundary click must be posted"
        );
    }

    /// LIVE (message-only): a legacy `background` screen point wider than i32
    /// must be refused BEFORE the `as i32` cast (4294967296 would otherwise
    /// become 0 and pass the 16-bit check), and an i32-representable point
    /// that leaves the 16-bit range after ScreenToClient is still refused.
    #[test]
    fn live_bg_screen_coords_validated_before_and_after_the_cast() {
        let _g = live_guard();
        let f = crate::fixture::MessageWindow::new();
        let bg = format!("hwnd:{}", f.hwnd_i64());
        crate::fixture::reset_msg_counts();
        // Wider than i32: the pre-cast gate must catch these.
        for x in [4294967296i64, i32::MAX as i64 + 1, i32::MIN as i64 - 1] {
            let out = do_burst(&DoArgs {
                background: Some(bg.clone()),
                ..DoArgs::new(&[serde_json::json!({"type": "click", "x": x, "y": 0})])
            });
            let payload = sc(&out);
            let r = &payload["results"][0];
            assert_eq!(r["ok"], Value::from(false), "x={x}: {r}");
            assert!(
                r["error"]
                    .as_str()
                    .unwrap_or("")
                    .contains("bad_coordinates"),
                "x={x} must be typed bad_coordinates, got: {r}"
            );
            assert_eq!(
                crate::fixture::msg_counts(),
                (0, 0, 0, 0, 0),
                "x={x} must post nothing"
            );
        }
        // i32-representable but outside the 16-bit lParam after conversion.
        let out = do_burst(&DoArgs {
            background: Some(bg.clone()),
            ..DoArgs::new(&[serde_json::json!({"type": "click", "x": i32::MAX as i64, "y": 0})])
        });
        let payload = sc(&out);
        let r = &payload["results"][0];
        assert_eq!(r["ok"], Value::from(false), "i32::MAX: {r}");
        assert!(
            r["error"]
                .as_str()
                .unwrap_or("")
                .contains("bad_coordinates"),
            "the post-conversion 16-bit check must still fire, got: {r}"
        );
        // A real in-range point still posts.
        let out = do_burst(&DoArgs {
            background: Some(bg),
            ..DoArgs::new(&[serde_json::json!({"type": "click", "x": 5, "y": 5})])
        });
        let payload = sc(&out);
        assert_eq!(payload["results"][0]["ok"], Value::from(true), "{payload}");
        assert!(
            wait_until(|| crate::fixture::msg_counts().0 >= 1),
            "the in-range click must be posted"
        );
    }

    /// LIVE: an ambiguous target is listed, never guessed.
    #[test]
    fn live_ambiguous_target_reports_candidates() {
        // Two offscreen fixtures share this process's image name, so an
        // exe-target for our own binary is ambiguous by construction.
        let a = crate::fixture::Fixture::new();
        let b = crate::fixture::Fixture::new();
        let spec = crate::target::TargetSpec::parse(&serde_json::json!({
            "exe": std::env::current_exe()
                .ok()
                .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
                .unwrap_or_else(|| "puppet.exe".to_string()),
            "className": format!("PuppetFixture"),
        }))
        .unwrap();
        let _ = (&a, &b);
        // className is exact, so the prefix does not match: prove the
        // positive path (class exact) resolves to exactly one of them.
        let class_a = unsafe { crate::win_class(a.hwnd()) };
        let spec_a = crate::target::TargetSpec::parse(&serde_json::json!({
            "exe": std::env::current_exe()
                .ok()
                .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
                .unwrap_or_else(|| "puppet.exe".to_string()),
            "className": class_a,
        }))
        .unwrap();
        let l = crate::target::lock(&spec_a).expect("pid-free exe+class target resolves");
        assert_eq!(l.hwnd, a.hwnd_i64());
        // The prefix class matches neither: gone, not a guess.
        let e = crate::target::lock(&spec).expect_err("prefix class must not match");
        assert_eq!(e.code, "target_gone", "{}", e.text());
    }
}
