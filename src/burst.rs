//! `do` burst tool: queued input actions in one round-trip.
//!
//! Wire- and behavior-parity with `puppet.py:do` (2614-3418). Real-input
//! paths (SendInput, focus changes, pixel-click fallback) are implemented
//! for live use and marked `unconfirmed: real-input test needs owner
//! permission` - never exercised by the headless parity harness.

use crate::cap::{bitblt_grab, resolve_selector};
use crate::shot::element_model;
use crate::{
    enum_child_windows, py_repr_str, py_repr_value, py_round_n, virtual_rect, wrapped_text,
};
use base64::Engine as _;
use serde_json::{Map, Value};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
};
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// ---------------------------------------------------------------- constants (puppet.py:1512-1518, 1628-1656)

const M_WHEEL: u32 = 0x0800;
const M_HWHEEL: u32 = 0x1000;

const BG_NOTE: &str = "background PostMessage delivery is best-effort: it is NOT honoured by every app and is never equivalent to foreground input.";
const BG_SUFFIX: &str = ""; // Retained for message shape; there is no foreground delivery to omit to.
const MAX_CLICK_COUNT: i64 = 100;
const MAX_STABLE_EDGE: i64 = 4096;
/// Hard ceiling on one burst. Every action runs under the global input lock,
/// so a huge batch can hold the desktop for hours; 256 is far past any real
/// UI flow and small enough that a stuck burst is a bounded stall.
pub const BURST_MAX_ACTIONS: usize = 256;
/// Total wall-clock budget for one burst call. Checked before every action,
/// inside every pacing loop (chunked `type` pacing, `key` hold/gap repeats)
/// and against the trailing settle, so no sleep chain can hold the lock past
/// it.
pub const BURST_BUDGET: std::time::Duration = std::time::Duration::from_secs(300);
/// Test-only budget override, same idiom as `PUPPET_TEST_STALL_*_MS`: the
/// suite proves the pacing loops are bounded without a five-minute run. It
/// can only shorten the budget (clamped up to 500 ms, never past
/// `BURST_BUDGET`), so a test can never make a real burst run longer.
const BUDGET_TEST_VAR: &str = "PUPPET_TEST_BURST_BUDGET_MS";
const BUDGET_TEST_MIN: std::time::Duration = std::time::Duration::from_millis(500);

/// The burst budget in force for this call.
fn burst_budget() -> std::time::Duration {
    match std::env::var(BUDGET_TEST_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        Some(ms) => std::time::Duration::from_millis(ms).clamp(BUDGET_TEST_MIN, BURST_BUDGET),
        None => BURST_BUDGET,
    }
}

/// What is left of the burst budget `t_all` elapsed ago (never negative).
fn budget_left_ms(t_all: &Instant, budget: std::time::Duration) -> i64 {
    budget.saturating_sub(t_all.elapsed()).as_millis() as i64
}

/// One pacing pause (`chunk_ms`, `hold_ms`, `gap_ms`, `settle_ms`) clamped to
/// what is left of the burst budget. `None` means the budget is spent: the
/// caller must stop with a typed `burst_budget_exceeded` result instead of
/// sleeping, so a 1 MiB text at `chunk_chars:1, chunk_ms:60000` cannot hold
/// the global input lock for a million minutes.
fn bounded_pause(t_all: &Instant, budget: std::time::Duration, want_ms: u64) -> Option<u64> {
    if want_ms == 0 {
        return Some(0);
    }
    let left = budget_left_ms(t_all, budget);
    if left <= 0 {
        return None;
    }
    Some(want_ms.min(left as u64))
}

/// The typed per-action failure a pacing loop returns when the budget stops
/// it: same `burst_budget_exceeded` code every other budget refusal uses.
fn budget_stop(
    i: usize,
    typ_v: &Value,
    t0: &Instant,
    budget: std::time::Duration,
    what: &str,
) -> Map<String, Value> {
    let mut d = base_entry(i, typ_v, false, elapsed_ms(t0));
    d.insert("code".to_string(), Value::from("burst_budget_exceeded"));
    d.insert(
        "error".to_string(),
        Value::from(format!(
            "burst_budget_exceeded: the {}-second burst budget expired during {what}; the rest of the action was not sent",
            budget.as_secs_f64()
        )),
    );
    d
}

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

// ---------------------------------------------------------------- foreground input builders (deleted)

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

/// Virtual keys whose key messages carry the extended bit (bit 24) in
/// lParam: navigation/edit block, Win keys, Apps, Divide, NumLock and the
/// right-hand Ctrl/Menu.
fn is_extended_vk(vk: u16) -> bool {
    matches!(
        vk,
        0x21..=0x28 | 0x2D | 0x2E | 0x5B..=0x5D | 0x6F | 0x90 | 0xA3 | 0xA5
    )
}

/// lParam for one posted key message: repeat 1, scan code from
/// `MapVirtualKeyW`, extended bit for the extended set, context code when
/// Alt is held, previous-state + transition bits on key-up. Proven against
/// a live PotPlayer: Space down is `1 | (0x39 << 16)` = `0x00390001`,
/// Space up ORs bits 30/31 = `0xC0390001`.
fn posted_key_lparam(vk: u16, keyup: bool, alt: bool) -> LPARAM {
    let sc = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } & 0xFF;
    let mut lp: u32 = 1 | (sc << 16);
    if is_extended_vk(vk) {
        lp |= 1 << 24;
    }
    if alt {
        lp |= 1 << 29;
    }
    if keyup {
        lp |= (1 << 30) | (1 << 31);
    }
    lp as i32 as isize
}

/// Key-down half of one posted key: (msg, wparam, lparam) triples. Alt
/// travels as WM_SYSKEYDOWN/UP; a layout Shift wraps the key.
fn posted_key_downs(code: &KeyCode) -> Vec<(u32, u64, LPARAM)> {
    match code {
        KeyCode::Vk(vk, shift) => {
            let alt = *vk == 0x12;
            let dm = if alt { 0x0104 } else { 0x0100 };
            let mut v = Vec::new();
            if *shift {
                v.push((dm, VK_SHIFT as u64, posted_key_lparam(VK_SHIFT, false, alt)));
            }
            v.push((dm, *vk as u64, posted_key_lparam(*vk, false, alt)));
            v
        }
        KeyCode::Uni(ch) => utf16_units(&ch.to_string())
            .into_iter()
            .map(|u| (0x0102, u as u64, 1))
            .collect(),
    }
}

/// Key-up half of one posted key, in release order.
fn posted_key_ups(code: &KeyCode) -> Vec<(u32, u64, LPARAM)> {
    match code {
        KeyCode::Vk(vk, shift) => {
            let alt = *vk == 0x12;
            let um = if alt { 0x0105 } else { 0x0101 };
            let mut v = Vec::new();
            v.push((um, *vk as u64, posted_key_lparam(*vk, true, alt)));
            if *shift {
                v.push((um, VK_SHIFT as u64, posted_key_lparam(VK_SHIFT, true, alt)));
            }
            v
        }
        KeyCode::Uni(ch) => utf16_units(&ch.to_string())
            .into_iter()
            .map(|u| (0x0102, u as u64, 1))
            .collect(),
    }
}

/// Capture the locked target at native pixels for posted-key verify:
/// the same window lock, lossless PNG, no grid, no elements. A capture
/// failure is an error, never a guessed effect. One bounded retry rides
/// out a transient render race (a window that has not finished painting
/// reads back blank once); a persistent failure stays an error.
fn verify_capture(hwnd: i64) -> Result<crate::cap::Img, String> {
    match verify_capture_once(hwnd) {
        Ok(im) => Ok(im),
        Err(first) => {
            std::thread::sleep(std::time::Duration::from_millis(300));
            verify_capture_once(hwnd).map_err(|second| format!("{second} (first attempt: {first})"))
        }
    }
}

/// One verify capture attempt.
fn verify_capture_once(hwnd: i64) -> Result<crate::cap::Img, String> {
    let args = crate::screen::ShotArgs {
        display: -1,
        region: None,
        window: None,
        max_px: None,
        fmt: "png".to_string(),
        quality: None,
        grid: false,
        allow_crop_fallback: false,
        elements: false,
        out: None,
        target: Some(serde_json::json!({ "hwnd": hwnd })),
    };
    let reply: Value = serde_json::from_str(&crate::screen::dispatch_screenshot(&args))
        .map_err(|e| format!("verify capture unparseable: {e}"))?;
    if reply.get("isError") == Some(&Value::from(true)) {
        let detail = reply
            .get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join(" | ")
            })
            .unwrap_or_else(|| "capture failed".to_string());
        return Err(detail);
    }
    let b64 = reply
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|blocks| {
            blocks
                .iter()
                .find_map(|b| b.get("data").and_then(|d| d.as_str()))
        })
        .ok_or_else(|| "verify capture returned no image".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("verify capture is not base64: {e}"))?;
    crate::cap::decode_png(&bytes).map_err(|e| format!("verify capture: {e}"))
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

/// Writable-text probe: a text-input control that is not read-only.
/// Typing into a read-only control is a silent no-op, so the descendant
/// walk prefers a control that can actually receive the text.
fn bg_is_writable_text_control(h: HWND) -> bool {
    if !bg_is_text_control(h) {
        return false;
    }
    unsafe {
        // GetWindowLongPtrW carries the style; ES_READONLY marks the
        // control the walk must skip.
        const GWL_STYLE: i32 = -16;
        const ES_READONLY: u32 = 0x0800;
        let style =
            windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(h, GWL_STYLE) as u32;
        style & ES_READONLY == 0
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
/// receive typed text: the thread's focused control when it is itself
/// writable text input (GetGUIThreadInfo, the caret owner), else the first
/// visible + enabled + writable text control in the descendant tree (the
/// document), else the window itself. A bare focus window (tab strip,
/// toolbar) would swallow WM_CHAR, so focus only counts when it is text
/// input; a read-only control would swallow it silently, so writable wins.
/// The descendant walk is recursive because modern editors nest the document
/// deeper than one level. Never activates or focuses; PostMessage-only, so
/// the owner's desktop is untouched.
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
                    && bg_is_writable_text_control(f)
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
            if bg_is_writable_text_control(k) {
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
                "no_foreground_policy: keyboard input is never posted to the owner's desktop \
                 (WM_CHAR/WM_KEYDOWN bypass the input queue and are ignored by many controls), \
                 and no flag buys a foreground act: this cannot be done under the background-only \
                 policy. type with an element works only on the server's own hidden desktop. \
                 Nothing was posted."
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

/// Element-match outcome: the entry, a typed error, or disambiguation
/// candidates when the selector matches several controls.
type MatchOut = (
    Option<Map<String, Value>>,
    Option<String>,
    Option<Vec<Map<String, Value>>>,
);

/// `focus_window`'s legacy `title`/`hwnd` arguments resolved to the SAME
/// `TargetLock` every other verb uses. A bare hwnd is a recyclable handle:
/// `IsWindow` on it can be true for a *different* window than the caller
/// meant, so focus goes through the lock (pid + process creation time +
/// image + class) and is revalidated immediately before the focus call —
/// the same seam `screenshot`'s legacy `window:` selector uses.
fn focus_lock(
    title: Option<&Value>,
    hwnd: Option<&Value>,
) -> Result<crate::target::TargetLock, crate::target::TargetError> {
    let h = match hwnd {
        None | Some(Value::Null) => None,
        Some(v) => Some(py_int(v).map_err(|e| crate::target::TargetError {
            code: crate::target::BAD,
            message: format!("bad_window: bad hwnd {}: {e}", py_str(v)),
            candidates: None,
        })?),
    };
    let t = title.filter(|v| py_truthy(v)).map(py_str);
    crate::cap::lock_legacy(h, t.as_deref())
}

/// A `focus_window` refusal in the per-action shape: the target error's own
/// fields (`code`, `error`, `candidates`) inside the action envelope.
fn target_failure(
    i: usize,
    typ_v: &Value,
    t0: &Instant,
    e: &crate::target::TargetError,
) -> Map<String, Value> {
    let mut d = base_entry(i, typ_v, false, elapsed_ms(t0));
    for (k, v) in e.to_map() {
        if k != "ok" {
            d.insert(k, v);
        }
    }
    d.insert("via".to_string(), Value::from("none"));
    d
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
        "offscreen",
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

// ---------------------------------------------------------------- element geometry (one gate)

/// Why an element rect cannot become an injection point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RectRefusal {
    /// No area: the control is scrolled away, virtualized or collapsed, so
    /// its "centre" would be the target's client origin.
    Offscreen,
    /// Real geometry, but `ScreenToClient` failed (window moved or closed).
    Unconvertible,
}

/// The rect as `(left, top, width, height)`; a missing or short field reads 0.
fn element_rect(entry: &Map<String, Value>) -> (i64, i64, i64, i64) {
    let rect: Vec<i64> = match entry.get("rect") {
        Some(Value::Array(r)) => r.iter().map(|v| v.as_i64().unwrap_or(0)).collect(),
        _ => Vec::new(),
    };
    (
        rect.first().copied().unwrap_or(0),
        rect.get(1).copied().unwrap_or(0),
        rect.get(2).copied().unwrap_or(0),
        rect.get(3).copied().unwrap_or(0),
    )
}

/// The ONE gate between an element's rect and a pixel the server may inject
/// into, in SCREEN pixels. UIA reports `[0,0,0,0]` for a control with no
/// on-screen rectangle; `left + 0/2, top + 0/2` is then a real screen point —
/// the target's client origin — so a click posted at that "centre" lands on
/// the window frame instead of the control. Every verb that turns an element
/// rect into an injection point converts through here, so the guard cannot be
/// forgotten by a verb added later.
pub(crate) fn element_rect_centre(entry: &Map<String, Value>) -> Result<(i64, i64), RectRefusal> {
    let (rl, rt, rw, rh) = element_rect(entry);
    if rw <= 0 || rh <= 0 {
        return Err(RectRefusal::Offscreen);
    }
    Ok((rl + rw / 2, rt + rh / 2))
}

/// `element_rect_centre` plus the documented target convention: the rect is
/// SCREEN pixels and the click point is CLIENT pixels (physical).
pub(crate) fn element_centre_client(
    target_hwnd: i64,
    entry: &Map<String, Value>,
) -> Result<(i64, i64), RectRefusal> {
    let (sx, sy) = element_rect_centre(entry)?;
    crate::target::screen_point_to_client(target_hwnd, sx, sy).ok_or(RectRefusal::Unconvertible)
}

/// The typed refusal text for a rect that cannot be aimed at, flag and
/// recovery named (never a silent skip, never a guess).
pub(crate) fn rect_refusal_message(entry: &Map<String, Value>, r: RectRefusal) -> String {
    match r {
        RectRefusal::Offscreen => {
            let (rl, rt, rw, rh) = element_rect(entry);
            format!(
                "offscreen_element: the matched control has no on-screen rectangle \
                 (rect [{rl},{rt},{rw},{rh}]), so it has no pixel to click; scroll it into \
                 view, or use invoke, which acts through the UIA pattern and needs no \
                 geometry. Nothing was sent."
            )
        }
        RectRefusal::Unconvertible => "element rect is not convertible to client pixels \
                                        (window moved or closed mid-call)"
            .to_string(),
    }
}

/// W2.1: an element mutation with no fingerprint. Typed, and carrying the
/// fresh value so the retry costs one call.
const EXPECT_FP_REQUIRED: &str = "expect_fp_required";

/// One typed refusal from the fingerprint gate.
struct FpRefusal {
    code: &'static str,
    resolution: &'static str,
    text: String,
}

/// The re-walked entry's fingerprint must equal `expect_fp`. Returns the typed
/// refusal when it does not, None when the gate passes. `required` refuses an
/// absent fingerprint too: a mutation without one may act on a different
/// control than the one the caller saw (W2.1). Applies live and under dry_run
/// alike: a stale ref never acts.
fn check_expect_fp(
    a: &Map<String, Value>,
    entry: &Map<String, Value>,
    sel: &str,
    required: bool,
) -> Option<FpRefusal> {
    let got = entry_str(entry, "fp");
    let want = match a.get("expect_fp") {
        None | Some(Value::Null) => String::new(),
        Some(v) => py_str(v),
    };
    if want.trim().is_empty() {
        if !required {
            return None;
        }
        return Some(FpRefusal {
            code: EXPECT_FP_REQUIRED,
            resolution: "expect_fp_required",
            text: format!(
                "expect_fp_required: element \"{sel}\" was resolved fresh (fp {got}). Pass expect_fp \
                 with the fingerprint from your elements snapshot; a mutation without one may act \
                 on a different control."
            ),
        });
    }
    if got == want {
        return None;
    }
    Some(FpRefusal {
        code: "stale_ref",
        resolution: "stale",
        text: format!(
            "stale_ref: element \"{sel}\" changed since its snapshot (expected fp {want}, now {got}). Take a fresh `elements` snapshot and retry with the new ref/fp."
        ),
    })
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
                "element is not invokable under the Win32 backend: there is no pixel fallback, \
                 because fallback:\"click\" is refused with no_cursor_policy and the pointer is \
                 never moved"
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

// ---------------------------------------------------------------- UIPI / elevation (gap 2)

/// Pure refusal text (gap 2): the typed, actionable error naming the
/// elevated target. Separated from the live reader below for testability;
/// essentials are pinned by `uipi_refusal_text_names_target_and_recovery`.
fn format_uipi_refusal(exe: &str, pid: u32, hwnd: i64) -> String {
    format!(
        "uipi_blocked: target \"{exe}\" (pid {pid}, hwnd {hwnd}) runs elevated; input from this unelevated process would be silently dropped by UIPI. Relaunch this server elevated and retry; under dry_run this refusal is reported without injecting anything."
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
    /// Gap 2: set when the last flush short-sent against an elevated
    /// foreground (UIPI-suspected). Read by fail_since_flush at the call site.
    flush_uipi: bool,
}

impl Burst {
    /// No-op by policy: `SendInput` is never called, so nothing is ever
    /// queued and every flush is empty. Kept as a method because the posted
    /// branches below still sequence through it (harmless, always clean).
    fn flush(&mut self) -> (f64, Option<String>) {
        self.flush_uipi = false;
        self.pending.clear();
        self.marked.clear();
        (0.0, None)
    }
}

/// Typed code carried by a refusal, so the retro-marked action exposes
/// the same machine-readable code an in-branch refusal would.
fn gate_code_of(msg: &str) -> Option<&'static str> {
    [
        crate::gate::NO_FOREGROUND_POLICY,
        crate::gate::NO_CURSOR_POLICY,
        crate::gate::NO_CLIPBOARD_POLICY,
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

    /// Test-only foreground batch (dry_run). The flags are retained for
    /// schema compatibility but buy nothing: every injecting verb is
    /// refused under the background-only policy.
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
    let actions: &[Value] = if args.actions.len() > BURST_MAX_ACTIONS {
        // Refused below as a typed per-result failure; nothing runs.
        &[]
    } else {
        args.actions
    };
    let background = args.background.clone();
    let background_args = args.background_args.clone();
    let focus_guard = args.focus_guard;
    let settle_ms = args.settle_ms;
    let dry_run = args.dry_run;
    let desktop = args.desktop.clone();
    let t_all = Instant::now();
    let budget = burst_budget();
    let focus_before = fg_now();
    let cursor_before = cursor_now();

    // Boot phase (puppet.py:2693-2746). Top-level wire types are validated in
    // serve.rs; only the range clamps are live here.
    let mut boot_errors: Vec<Map<String, Value>> = Vec::new();
    if args.actions.len() > BURST_MAX_ACTIONS {
        // A burst is not just a list: every action runs under the global input
        // lock, so an unbounded batch can hold the desktop for hours. Refuse
        // the whole call as a per-action-shaped failure (same envelope as a
        // failed action) instead of silently truncating it.
        let mut m = Map::new();
        m.insert("index".to_string(), Value::from(0));
        m.insert("type".to_string(), Value::from("burst"));
        m.insert("ok".to_string(), Value::from(false));
        m.insert("ms".to_string(), Value::from(0.0));
        m.insert("code".to_string(), Value::from("burst_budget_exceeded"));
        m.insert(
            "error".to_string(),
            Value::from(format!(
                "burst_budget_exceeded: {} actions exceed the {BURST_MAX_ACTIONS}-action burst cap; nothing was run",
                args.actions.len()
            )),
        );
        boot_errors.push(m);
    }
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
        flush_uipi: false,
    };

    for (i, a) in actions.iter().enumerate() {
        // Wall-clock budget: checked before every action, so a chain of waits
        // cannot outlive the call. Reported as a per-action-shaped failure
        // naming the limit and what is left unrun.
        if t_all.elapsed() >= budget {
            let mut m = base_entry(i, &Value::from("burst"), false, 0.0);
            m.insert("code".to_string(), Value::from("burst_budget_exceeded"));
            m.insert(
                "error".to_string(),
                Value::from(format!(
                    "burst_budget_exceeded: the {}-second burst budget expired before action {i}; {} action(s) not run",
                    budget.as_secs_f64(),
                    actions.len() - i
                )),
            );
            st.results.push(m);
            break;
        }
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
        let has_element =
            matches!(ad.get("element"), Some(v) if py_truthy(v) && !py_str(v).trim().is_empty());
        // UIA text entry may take the foreground (the provider can focus
        // its control): refused under the background-only policy, naming
        // the control — no flag can buy a focus change. This runs before
        // the gate so the refusal names the element, under dry_run too, and
        // before the `allow_foreground` pre-check so the code does not depend
        // on a flag that buys nothing: the verb has no background delivery
        // whether or not the caller asks for the foreground.
        if typ == "set_value" || (typ == "type" && has_element) {
            let sel = ad.get("element").map(py_str).unwrap_or_default();
            let e = crate::gate::uia_text_unavailable(sel.trim());
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
        // The lock is revalidated before EVERY action: a target that closed
        // or whose handle was recycled is a typed refusal, never a retarget
        // and never a foreground fallback.
        // Background-only policy: escalation is refused per action BEFORE
        // anything else (even the revalidation read below) — and under
        // dry_run too, so a dry run that said ok while the live call would
        // refuse is a lie. No flag buys foreground, cursor or clipboard.
        if args.allow_foreground {
            let e = crate::gate::no_foreground_policy(typ);
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
        // Per-action delivery: `auto` (posted when a target is locked),
        // `posted` (requires a locked target), `sendinput` (deleted path:
        // refused, never acted on).
        let delivery = match ad.get("delivery") {
            None | Some(Value::Null) => "auto".to_string(),
            Some(Value::String(s)) => s.to_lowercase(),
            Some(v) => {
                let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                m.insert(
                    "error".to_string(),
                    Value::from(format!(
                        "unknown delivery {} (want auto|posted|sendinput)",
                        crate::py_repr_value(v)
                    )),
                );
                st.results.push(m);
                continue;
            }
        };
        if !["auto", "posted", "sendinput"].contains(&delivery.as_str()) {
            let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
            m.insert(
                "error".to_string(),
                Value::from(format!(
                    "unknown delivery \"{delivery}\" (want auto|posted|sendinput)"
                )),
            );
            st.results.push(m);
            continue;
        }
        if delivery == "sendinput" {
            let e = crate::gate::no_foreground_policy(&format!("{typ} delivery:\"sendinput\""));
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
        // posted character stream: it takes its own branch below (refused
        // under the background-only policy). `click` on an element is
        // UIA-pattern-first too (Invoke/SelectionItem/Toggle), with a posted
        // click at the element centre as the fallback. Posted keyboard to a
        // locked target is the keyboard path; the private (hidden) desktop
        // keeps the same posted path for the program it launched.
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
                // Deleted path: the pointer is never moved. The gate refused
                // the targetless case; this is defense in depth.
                let e = crate::gate::no_cursor_policy("move");
                let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                m.insert("code".to_string(), Value::from(e.code));
                m.insert("error".to_string(), Value::from(e.text()));
                return Ok(Some(m));
            }
            if typ == "click" && !has_element {
                // Deleted path: a targetless click needs the real cursor.
                // The gate refused it; this is defense in depth.
                let e = crate::gate::no_cursor_policy("click x/y");
                let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                m.insert("code".to_string(), Value::from(e.code));
                m.insert("error".to_string(), Value::from(e.text()));
                return Ok(Some(m));
            }
            if typ == "down" || typ == "up" {
                // Deleted path: a targetless button message needs the real
                // cursor. The gate refused it; this is defense in depth.
                let e = crate::gate::no_cursor_policy(typ);
                let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                m.insert("code".to_string(), Value::from(e.code));
                m.insert("error".to_string(), Value::from(e.text()));
                return Ok(Some(m));
            }
            if typ == "drag" {
                // Deleted path: a drag without a locked target needs the
                // real pointer. The gate refused it; this is defense in depth.
                let e = crate::gate::no_cursor_policy("drag");
                let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                m.insert("code".to_string(), Value::from(e.code));
                m.insert("error".to_string(), Value::from(e.text()));
                return Ok(Some(m));
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
                    // Deleted path: a targetless wheel needs the real
                    // cursor. The gate refused it; this is defense in depth.
                    let e = crate::gate::no_cursor_policy("scroll x/y");
                    let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    m.insert("code".to_string(), Value::from(e.code));
                    m.insert("error".to_string(), Value::from(e.text()));
                    return Ok(Some(m));
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
                    if let Some(se) = check_expect_fp(ad, &entry, &py_str(&elem_v), false) {
                        d.insert("ok".to_string(), Value::from(false));
                        d.insert("code".to_string(), Value::from(se.code));
                        d.insert("error".to_string(), Value::from(se.text));
                        d.insert("resolution".to_string(), Value::from(se.resolution));
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
                // Deleted path: a wheel without a locked point needs the
                // real cursor; without an element there is no UIA pattern.
                // The gate refused both cases; this is defense in depth.
                let e = crate::gate::no_cursor_policy("scroll");
                let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                m.insert("code".to_string(), Value::from(e.code));
                m.insert("error".to_string(), Value::from(e.text()));
                return Ok(Some(m));
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
                    true,
                ) {
                    d.insert("ok".to_string(), Value::from(false));
                    d.insert("code".to_string(), Value::from(se.code));
                    d.insert("error".to_string(), Value::from(se.text));
                    d.insert("resolution".to_string(), Value::from(se.resolution));
                    d.insert("via".to_string(), Value::from("none"));
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
                }
                d.insert("element".to_string(), Value::from(element_pub_full(&entry)));
                d.insert("resolution".to_string(), Value::from("found"));
                // Element rects are SCREEN pixels; the client conversion is
                // the documented target convention (physical pixels) and goes
                // through the one geometry gate, which refuses an empty rect.
                let centre = element_centre_client(tgt, &entry);
                // The guard runs BEFORE the dry-run return: a dry run that
                // reports ok for an element the live path refuses is a lie
                // about what the call would do.
                if dry_run {
                    if let Err(r) = centre {
                        // A UIA-backed control with a live pattern needs no
                        // pixel, so only the pixel path is refused here: the
                        // dry run reports the refusal the live call would hit.
                        if model.backend != "uia" {
                            d.insert("ok".to_string(), Value::from(false));
                            d.insert("via".to_string(), Value::from("none"));
                            d.insert(
                                "code".to_string(),
                                Value::from(if r == RectRefusal::Offscreen {
                                    crate::gate::OFFSCREEN_ELEMENT
                                } else {
                                    crate::gate::BACKGROUND_UNAVAILABLE
                                }),
                            );
                            d.insert(
                                "error".to_string(),
                                Value::from(rect_refusal_message(&entry, r)),
                            );
                            d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                            return Ok(Some(d));
                        }
                    }
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
                    match centre {
                        Ok((cx, cy)) => {
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
                                             UIA pattern; when the control has no pattern there \
                                             is no pixel fallback, and allow_foreground:true is \
                                             refused with no_foreground_policy."
                                            ));
                                            "unchanged"
                                        }
                                    }
                                    None => "unverified",
                                };
                            } else {
                                effect = "unverified";
                            }
                        }
                        Err(r) => {
                            // Refused BEFORE any injection: an empty rect is not
                            // geometry, and its "centre" is the client origin.
                            d.insert("ok".to_string(), Value::from(false));
                            d.insert("via".to_string(), Value::from("none"));
                            d.insert("delivered".to_string(), Value::from("none"));
                            d.insert("effect".to_string(), Value::from("unverified"));
                            d.insert(
                                "code".to_string(),
                                Value::from(if r == RectRefusal::Offscreen {
                                    crate::gate::OFFSCREEN_ELEMENT
                                } else {
                                    crate::gate::BACKGROUND_UNAVAILABLE
                                }),
                            );
                            d.insert(
                                "error".to_string(),
                                Value::from(rect_refusal_message(&entry, r)),
                            );
                            d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                            return Ok(Some(d));
                        }
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
                // Unreachable: the background-only pre-empt above refuses
                // every set_value/type-on-element naming the control. Kept
                // as defense in depth (never a provider call without it).
                {
                    let sel = ad.get("element").map(py_str).unwrap_or_default();
                    let e = crate::gate::uia_text_unavailable(sel.trim());
                    let mut dd = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    dd.insert("code".to_string(), Value::from(e.code));
                    dd.insert("error".to_string(), Value::from(e.text()));
                    return Ok(Some(dd));
                }
                // Deleted path (policy): the UIA write below is unreachable —
                // set_value/type-on-element are refused above, naming the
                // control. Wrapped (not run) so the deletion stays visible.
                #[allow(unreachable_code)]
                {
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
                        true,
                    ) {
                        d.insert("ok".to_string(), Value::from(false));
                        d.insert("code".to_string(), Value::from(se.code));
                        d.insert("error".to_string(), Value::from(se.text));
                        d.insert("resolution".to_string(), Value::from(se.resolution));
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
            }
            // Plain `type`: posted WM_CHAR to the locked target (the
            // text-input child when the target owns one, else the target
            // itself) — never activated, focused or raised. Delivery is
            // reported, never implied: `delivered:"posted"`,
            // `effect:"unverified"`.
            if typ == "type" {
                let text = match ad.get("text") {
                    None => String::new(),
                    Some(v) => py_str(v),
                };
                if let Err(e) = check_clip_len(text.chars().count()) {
                    let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    m.insert("error".to_string(), Value::from(e));
                    return Ok(Some(m));
                }
                let Some(lock) = target_lock.as_ref() else {
                    let e = crate::gate::GateError {
                        code: crate::gate::TARGET_REQUIRED,
                        message: "type needs an explicit target window; this server never \
                                  falls back to the foreground window. Nothing was sent."
                            .to_string(),
                    };
                    let mut d = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    d.insert("code".to_string(), Value::from(e.code));
                    d.insert("error".to_string(), Value::from(e.text()));
                    return Ok(Some(d));
                };
                let tgt = lock.hwnd;
                if let Some(msg) = uipi_refusal(tgt) {
                    let mut d = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    d.insert("error".to_string(), Value::from(msg));
                    d.insert("blocked_by_uipi".to_string(), Value::from(true));
                    return Ok(Some(d));
                }
                if dry_run {
                    let mut d = base_entry(i, &typ_v, true, elapsed_ms(&t0));
                    d.insert("dry_run".to_string(), Value::from(true));
                    d.insert("delivered".to_string(), Value::from("posted"));
                    d.insert("effect".to_string(), Value::from("unverified"));
                    d.insert("posted".to_string(), Value::from(0));
                    return Ok(Some(d));
                }
                // Keyboard input lands in the text control, not the frame.
                let dest = bg_text_target(tgt);
                let mut posted = 0i64;
                let mut failed = false;
                for unit in utf16_units(&text) {
                    if crate::cancel::cancelled() {
                        let mut d = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                        d.insert("delivered".to_string(), Value::from("posted"));
                        d.insert("effect".to_string(), Value::from("unverified"));
                        d.insert("error".to_string(), Value::from("cancelled"));
                        d.insert("posted".to_string(), Value::from(posted));
                        return Ok(Some(d));
                    }
                    if !bg_post(dest, 0x0102, unit as u64, 1) {
                        failed = true;
                        break;
                    }
                    posted += 1;
                }
                let mut d = base_entry(i, &typ_v, !failed, elapsed_ms(&t0));
                d.insert("delivered".to_string(), Value::from("posted"));
                d.insert("effect".to_string(), Value::from("unverified"));
                d.insert("posted".to_string(), Value::from(posted));
                if failed {
                    d.insert(
                        "error".to_string(),
                        Value::from("PostMessage failed: the target did not accept the text"),
                    );
                }
                d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                return Ok(Some(d));
            }
            // Posted-only keyboard: WM_KEYDOWN/UP (SYS variants when Alt
            // is held) straight to the locked target hwnd — never
            // activated, focused or raised. Verified by default: the
            // target is captured before and after and diffed with the
            // pixel engine (`changed`/`bbox`); `unchanged` is a result,
            // not an error — silence is never success.
            if typ == "key" {
                let name = match ad.get("name") {
                    None => String::new(),
                    Some(v) => py_str(v),
                };
                let code = vk_of(&name)?;
                let hold_ms = opt_py_int(ad, "hold_ms", 80)?;
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
                let verify = match ad.get("verify") {
                    None | Some(Value::Null) => true,
                    Some(Value::Bool(b)) => *b,
                    Some(v) => {
                        return Err(format!(
                            "verify must be a boolean (got {})",
                            crate::py_repr_value(v)
                        ));
                    }
                };
                // Posted requires a locked target; the gate already refused
                // the targetless case, so this is defense in depth.
                let Some(lock) = target_lock.as_ref() else {
                    let e = crate::gate::GateError {
                        code: crate::gate::TARGET_REQUIRED,
                        message: "key needs an explicit target window; this server never \
                                  falls back to the foreground window. Nothing was sent."
                            .to_string(),
                    };
                    let mut d = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    d.insert("code".to_string(), Value::from(e.code));
                    d.insert("error".to_string(), Value::from(e.text()));
                    return Ok(Some(d));
                };
                let tgt = lock.hwnd;
                // UIPI: a PostMessage to a higher-integrity window is
                // dropped silently — refuse with a typed error instead.
                if let Some(msg) = uipi_refusal(tgt) {
                    let mut d = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    d.insert("error".to_string(), Value::from(msg));
                    d.insert("blocked_by_uipi".to_string(), Value::from(true));
                    return Ok(Some(d));
                }
                if dry_run {
                    let mut d = base_entry(i, &typ_v, true, elapsed_ms(&t0));
                    d.insert("dry_run".to_string(), Value::from(true));
                    d.insert("delivered".to_string(), Value::from("posted"));
                    d.insert("effect".to_string(), Value::from("unverified"));
                    d.insert("posted".to_string(), Value::from(0));
                    return Ok(Some(d));
                }
                let before = match (verify, verify_capture(tgt)) {
                    (true, Ok(im)) => Some(im),
                    (true, Err(e)) => {
                        let mut d = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                        d.insert("delivered".to_string(), Value::from("none"));
                        d.insert("effect".to_string(), Value::from("unverified"));
                        d.insert("error".to_string(), Value::from(e));
                        return Ok(Some(d));
                    }
                    (false, _) => None,
                };
                let downs = posted_key_downs(&code);
                let ups = posted_key_ups(&code);
                let mut posted = 0i64;
                let mut failed = false;
                for r in 0..repeat {
                    for (msg, w, l) in &downs {
                        if !bg_post(tgt, *msg, *w, *l) {
                            failed = true;
                            break;
                        }
                        posted += 1;
                    }
                    if failed {
                        break;
                    }
                    // The hold between down and up (default 80 ms): bounded
                    // by the burst budget, cancellable — and a cancelled or
                    // expired hold still posts the ups below, so no exit
                    // path leaves a key held down.
                    if hold_ms > 0 {
                        match bounded_pause(&t_all, budget, hold_ms as u64) {
                            Some(h) => {
                                if sleep_cancellable(h) || crate::cancel::cancelled() {
                                    for (msg, w, l) in &ups {
                                        if bg_post(tgt, *msg, *w, *l) {
                                            posted += 1;
                                        }
                                    }
                                    let mut d = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                                    d.insert("delivered".to_string(), Value::from("posted"));
                                    d.insert("effect".to_string(), Value::from("unverified"));
                                    d.insert("error".to_string(), Value::from("cancelled"));
                                    d.insert("posted".to_string(), Value::from(posted));
                                    return Ok(Some(d));
                                }
                            }
                            None => {
                                for (msg, w, l) in &ups {
                                    if bg_post(tgt, *msg, *w, *l) {
                                        posted += 1;
                                    }
                                }
                                return Ok(Some(budget_stop(i, &typ_v, &t0, budget, "a held key")));
                            }
                        }
                    }
                    for (msg, w, l) in &ups {
                        if !bg_post(tgt, *msg, *w, *l) {
                            failed = true;
                            break;
                        }
                        posted += 1;
                    }
                    if failed {
                        break;
                    }
                    if r + 1 < repeat && gap_ms > 0 && !dry_run {
                        match bounded_pause(&t_all, budget, gap_ms as u64) {
                            Some(g) => {
                                if sleep_cancellable(g) || crate::cancel::cancelled() {
                                    let mut d = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                                    d.insert("delivered".to_string(), Value::from("posted"));
                                    d.insert("effect".to_string(), Value::from("unverified"));
                                    d.insert("error".to_string(), Value::from("cancelled"));
                                    d.insert("posted".to_string(), Value::from(posted));
                                    return Ok(Some(d));
                                }
                            }
                            None => {
                                return Ok(Some(budget_stop(
                                    i,
                                    &typ_v,
                                    &t0,
                                    budget,
                                    "a key repeat gap",
                                )));
                            }
                        }
                    }
                }
                let mut d = base_entry(i, &typ_v, !failed, elapsed_ms(&t0));
                d.insert("delivered".to_string(), Value::from("posted"));
                d.insert("posted".to_string(), Value::from(posted));
                if failed {
                    d.insert("effect".to_string(), Value::from("unverified"));
                    d.insert(
                        "error".to_string(),
                        Value::from("PostMessage failed: the target did not accept the key"),
                    );
                    return Ok(Some(d));
                }
                if !verify {
                    d.insert("effect".to_string(), Value::from("unverified"));
                    return Ok(Some(d));
                }
                let after = match verify_capture(tgt) {
                    Ok(im) => im,
                    Err(e) => {
                        d.insert("ok".to_string(), Value::from(false));
                        d.insert("effect".to_string(), Value::from("unverified"));
                        d.insert("error".to_string(), Value::from(e));
                        return Ok(Some(d));
                    }
                };
                let before = before.expect("verify:true captured before");
                match crate::cap::pixel_diff(&before, &after, 0) {
                    Ok(diff) => {
                        d.insert(
                            "effect".to_string(),
                            Value::from(if diff.identical {
                                "unchanged"
                            } else {
                                "changed"
                            }),
                        );
                        d.insert("changed".to_string(), Value::from(diff.changed));
                        d.insert(
                            "bbox".to_string(),
                            match diff.bbox {
                                Some(b) => Value::from(vec![
                                    Value::from(b[0]),
                                    Value::from(b[1]),
                                    Value::from(b[2]),
                                    Value::from(b[3]),
                                ]),
                                None => Value::Null,
                            },
                        );
                    }
                    Err(e) => {
                        d.insert("ok".to_string(), Value::from(false));
                        d.insert("effect".to_string(), Value::from("unverified"));
                        d.insert("error".to_string(), Value::from(e));
                    }
                }
                return Ok(Some(d));
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
                let hold_ms = opt_py_int(ad, "hold_ms", 80)?;
                if !(0..=60000).contains(&hold_ms) {
                    return Err(format!("hold_ms out of range 0..60000 (got {hold_ms})"));
                }
                let verify = match ad.get("verify") {
                    None | Some(Value::Null) => true,
                    Some(Value::Bool(b)) => *b,
                    Some(v) => {
                        return Err(format!(
                            "verify must be a boolean (got {})",
                            crate::py_repr_value(v)
                        ));
                    }
                };
                let Some(lock) = target_lock.as_ref() else {
                    let e = crate::gate::GateError {
                        code: crate::gate::TARGET_REQUIRED,
                        message: "hotkey needs an explicit target window; this server never \
                                  falls back to the foreground window. Nothing was sent."
                            .to_string(),
                    };
                    let mut d = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    d.insert("code".to_string(), Value::from(e.code));
                    d.insert("error".to_string(), Value::from(e.text()));
                    return Ok(Some(d));
                };
                let tgt = lock.hwnd;
                if let Some(msg) = uipi_refusal(tgt) {
                    let mut d = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    d.insert("error".to_string(), Value::from(msg));
                    d.insert("blocked_by_uipi".to_string(), Value::from(true));
                    return Ok(Some(d));
                }
                if dry_run {
                    let mut d = base_entry(i, &typ_v, true, elapsed_ms(&t0));
                    d.insert("dry_run".to_string(), Value::from(true));
                    d.insert("delivered".to_string(), Value::from("posted"));
                    d.insert("effect".to_string(), Value::from("unverified"));
                    d.insert("posted".to_string(), Value::from(0));
                    return Ok(Some(d));
                }
                let before = match (verify, verify_capture(tgt)) {
                    (true, Ok(im)) => Some(im),
                    (true, Err(e)) => {
                        let mut d = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                        d.insert("delivered".to_string(), Value::from("none"));
                        d.insert("effect".to_string(), Value::from("unverified"));
                        d.insert("error".to_string(), Value::from(e));
                        return Ok(Some(d));
                    }
                    (false, _) => None,
                };
                // Press in order, hold, release in reverse.
                let downs: Vec<(u32, u64, LPARAM)> =
                    codes.iter().flat_map(posted_key_downs).collect();
                let ups: Vec<(u32, u64, LPARAM)> =
                    codes.iter().rev().flat_map(posted_key_ups).collect();
                let mut posted = 0i64;
                let mut failed = false;
                // Real sequence: downs, hold, ups.
                for (msg, w, l) in &downs {
                    if !bg_post(tgt, *msg, *w, *l) {
                        failed = true;
                        break;
                    }
                    posted += 1;
                }
                if !failed && hold_ms > 0 {
                    match bounded_pause(&t_all, budget, hold_ms as u64) {
                        Some(h) => {
                            if sleep_cancellable(h) || crate::cancel::cancelled() {
                                for (msg, w, l) in &ups {
                                    if bg_post(tgt, *msg, *w, *l) {
                                        posted += 1;
                                    }
                                }
                                let mut d = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                                d.insert("delivered".to_string(), Value::from("posted"));
                                d.insert("effect".to_string(), Value::from("unverified"));
                                d.insert("error".to_string(), Value::from("cancelled"));
                                d.insert("posted".to_string(), Value::from(posted));
                                return Ok(Some(d));
                            }
                        }
                        None => {
                            for (msg, w, l) in &ups {
                                if bg_post(tgt, *msg, *w, *l) {
                                    posted += 1;
                                }
                            }
                            return Ok(Some(budget_stop(i, &typ_v, &t0, budget, "a held hotkey")));
                        }
                    }
                }
                if !failed {
                    for (msg, w, l) in &ups {
                        if !bg_post(tgt, *msg, *w, *l) {
                            failed = true;
                            break;
                        }
                        posted += 1;
                    }
                }
                let mut d = base_entry(i, &typ_v, !failed, elapsed_ms(&t0));
                d.insert("delivered".to_string(), Value::from("posted"));
                d.insert("posted".to_string(), Value::from(posted));
                if failed {
                    d.insert("effect".to_string(), Value::from("unverified"));
                    d.insert(
                        "error".to_string(),
                        Value::from("PostMessage failed: the target did not accept the hotkey"),
                    );
                    return Ok(Some(d));
                }
                if !verify {
                    d.insert("effect".to_string(), Value::from("unverified"));
                    return Ok(Some(d));
                }
                let after = match verify_capture(tgt) {
                    Ok(im) => im,
                    Err(e) => {
                        d.insert("ok".to_string(), Value::from(false));
                        d.insert("effect".to_string(), Value::from("unverified"));
                        d.insert("error".to_string(), Value::from(e));
                        return Ok(Some(d));
                    }
                };
                let before = before.expect("verify:true captured before");
                match crate::cap::pixel_diff(&before, &after, 0) {
                    Ok(diff) => {
                        d.insert(
                            "effect".to_string(),
                            Value::from(if diff.identical {
                                "unchanged"
                            } else {
                                "changed"
                            }),
                        );
                        d.insert("changed".to_string(), Value::from(diff.changed));
                        d.insert(
                            "bbox".to_string(),
                            match diff.bbox {
                                Some(b) => Value::from(vec![
                                    Value::from(b[0]),
                                    Value::from(b[1]),
                                    Value::from(b[2]),
                                    Value::from(b[3]),
                                ]),
                                None => Value::Null,
                            },
                        );
                    }
                    Err(e) => {
                        d.insert("ok".to_string(), Value::from(false));
                        d.insert("effect".to_string(), Value::from("unverified"));
                        d.insert("error".to_string(), Value::from(e));
                    }
                }
                return Ok(Some(d));
            }
            if typ == "clipboard_set" {
                // Refused under the background-only policy — no flag buys a
                // clipboard write. Before anything (no clipboard API is
                // reached, not even a read), under dry_run too.
                let e = crate::gate::no_clipboard_policy("clipboard_set");
                let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                m.insert("code".to_string(), Value::from(e.code));
                m.insert("error".to_string(), Value::from(e.text()));
                if dry_run {
                    m.insert("dry_run".to_string(), Value::from(true));
                }
                return Ok(Some(m));
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
                    true,
                ) {
                    d.insert("ok".to_string(), Value::from(false));
                    d.insert("code".to_string(), Value::from(se.code));
                    d.insert("error".to_string(), Value::from(se.text));
                    d.insert("resolution".to_string(), Value::from(se.resolution));
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
                let (ok_v, via, verr) = if let Some(want) = expand_req {
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
                    // Deleted path: the pixel click moves the system pointer
                    // and injects a button pair, so the background-only policy
                    // refuses it outright — no flag buys it, never escalates.
                    let refusal = crate::gate::no_cursor_policy("fallback:\"click\"");
                    d.insert("ok".to_string(), Value::from(false));
                    d.insert(
                        "code".to_string(),
                        Value::from(crate::gate::NO_CURSOR_POLICY),
                    );
                    d.insert("error".to_string(), Value::from(refusal.text()));
                    d.insert("via".to_string(), Value::from("none"));
                    d.insert("ms".to_string(), Value::from(elapsed_ms(&t0)));
                    return Ok(Some(d));
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
                // Unreachable: the background-only pre-empt above refuses
                // every set_value naming the element. Kept as defense in
                // depth (never a provider call without it).
                {
                    let sel = ad.get("element").map(py_str).unwrap_or_default();
                    let e = crate::gate::uia_text_unavailable(sel.trim());
                    let mut dd = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    dd.insert("code".to_string(), Value::from(e.code));
                    dd.insert("error".to_string(), Value::from(e.text()));
                    return Ok(Some(dd));
                }
                // Deleted path (policy): the UIA write below is unreachable —
                // set_value/type-on-element are refused above, naming the
                // control. Wrapped (not run) so the deletion stays visible.
                #[allow(unreachable_code)]
                {
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
                        true,
                    ) {
                        d.insert("ok".to_string(), Value::from(false));
                        d.insert("code".to_string(), Value::from(se.code));
                        d.insert("error".to_string(), Value::from(se.text));
                        d.insert("resolution".to_string(), Value::from(se.resolution));
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
                        if let Ok(v) = crate::uia::uia_range_value(tgt, &entry_str(&entry, "path"))
                        {
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
                // A wait that would outlive the burst budget is refused before
                // sleeping, with the same typed result a failed action carries.
                let left_ms = budget_left_ms(&t_all, budget);
                if wait_n > left_ms {
                    let mut m = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                    m.insert("code".to_string(), Value::from("burst_budget_exceeded"));
                    m.insert(
                        "error".to_string(),
                        Value::from(format!(
                            "burst_budget_exceeded: wait {wait_n}ms exceeds the remaining {left_ms}ms of the {}-second burst budget",
                            budget.as_secs_f64()
                        )),
                    );
                    return Ok(Some(m));
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
                // The poll loop is bounded by the remaining burst budget. When
                // that cut it short, name the budget instead of reporting a
                // plain unsatisfied timeout.
                let left_ms = budget_left_ms(&t_all, budget);
                let want_ms = opt_py_int(ad, "timeout_ms", 5000).unwrap_or(5000);
                let wf_action: Map<String, Value> = if want_ms > left_ms {
                    let mut m = ad.clone();
                    m.insert("timeout_ms".to_string(), Value::from(left_ms.max(0)));
                    m
                } else {
                    ad.clone()
                };
                let wr = wait_for(&wf_action, &t0);
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
                if want_ms > left_ms && wr.get("satisfied") != Some(&Value::from(true)) {
                    d.insert("ok".to_string(), Value::from(false));
                    d.insert("code".to_string(), Value::from("burst_budget_exceeded"));
                    d.insert(
                        "error".to_string(),
                        Value::from(format!(
                            "burst_budget_exceeded: wait_for timeout {want_ms}ms exceeds the remaining {left_ms}ms of the {}-second burst budget",
                            budget.as_secs_f64()
                        )),
                    );
                }
                return Ok(Some(d));
            }
            if typ == "focus_window" {
                // Focusing IS taking the foreground: refused under the
                // background-only policy, always. The lock still resolves
                // first so a dead handle reports its typed target error
                // instead of the policy one.
                let lock = match focus_lock(ad.get("title"), ad.get("hwnd")) {
                    Ok(l) => l,
                    Err(e) => return Ok(Some(target_failure(i, &typ_v, &t0, &e))),
                };
                if let Err(e) = lock.revalidate() {
                    return Ok(Some(target_failure(i, &typ_v, &t0, &e)));
                }
                let e = crate::gate::no_foreground_policy("focus_window");
                let mut d = base_entry(i, &typ_v, false, elapsed_ms(&t0));
                d.insert("code".to_string(), Value::from(e.code));
                d.insert("error".to_string(), Value::from(e.text()));
                d.insert("via".to_string(), Value::from("none"));
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
    // The trailing settle is a sleep like any other: clamped to what is left
    // of the budget, so `settle_ms:60000` at the end of a long burst cannot
    // hold the global input lock past the budget it already spent.
    let mut settle_applied: i64 = 0;
    if settle_n != 0 && !crate::cancel::cancelled() {
        if let Some(p) = bounded_pause(&t_all, budget, settle_n as u64) {
            settle_applied = p as i64;
            sleep_cancellable(p);
        }
    }
    let total_ms = t_all.elapsed().as_secs_f64() * 1000.0;
    let focus_after = fg_now();
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
        // The guard cannot restore: restoring IS taking the foreground
        // (SetForegroundWindow), refused under the background-only policy.
        // Nothing here changes focus (every verb is posted or refused), so
        // this arm only ever fires on an OS-side surprise — reported, never
        // acted on.
        focus_restored = false;
        focus_guard_refusal = Some(crate::gate::no_foreground_policy("focus_guard restore").text());
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
    // Envelope truth: `do` continues past a failed action, so a per-action
    // `ok` is not a tool verdict. These two fields are: `ok` is true only
    // when EVERY action succeeded, `failed` counts the ones that did not,
    // and the tool envelope reports isError only when NOTHING succeeded
    // (see `wrapped_text`). Per-action fields are untouched.
    let failed = st
        .results
        .iter()
        .filter(|r| r.get("ok") != Some(&Value::from(true)))
        .count();
    payload.insert("ok".to_string(), Value::from(failed == 0));
    payload.insert("failed".to_string(), Value::from(failed as i64));
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
    if settle_n != 0 {
        // Honest settle accounting: what was asked for and what the budget
        // allowed. Divergence is named, never silent.
        payload.insert("settle_requested_ms".to_string(), Value::from(settle_n));
        payload.insert("settle_ms".to_string(), Value::from(settle_applied));
    }
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

    /// The one geometry gate (pure): a rect with no area is refused, never
    /// turned into a "centre". On the pre-fix tree `[0,0,0,0]` fed straight
    /// into `screen_point_to_client` and produced a real client point.
    #[test]
    fn element_geometry_gate_refuses_empty_rects() {
        let mut e = Map::new();
        e.insert(
            "rect".to_string(),
            Value::from(vec![
                Value::from(0),
                Value::from(0),
                Value::from(0),
                Value::from(0),
            ]),
        );
        assert_eq!(element_rect_centre(&e), Err(RectRefusal::Offscreen));
        // A negative extent is not geometry either.
        e.insert(
            "rect".to_string(),
            Value::from(vec![
                Value::from(10),
                Value::from(10),
                Value::from(-5),
                Value::from(20),
            ]),
        );
        assert_eq!(element_rect_centre(&e), Err(RectRefusal::Offscreen));
        // Missing rect is the same class, not a panic.
        let bare = Map::new();
        assert_eq!(element_rect_centre(&bare), Err(RectRefusal::Offscreen));
        // The refusal names the flag and the recovery.
        let msg = rect_refusal_message(&bare, RectRefusal::Offscreen);
        assert!(msg.starts_with("offscreen_element: "), "{msg}");
        assert!(msg.contains("scroll it into view"), "{msg}");
        assert!(msg.contains("invoke"), "{msg}");
        // Real geometry still converts (screen-space centre).
        e.insert(
            "rect".to_string(),
            Value::from(vec![
                Value::from(100),
                Value::from(200),
                Value::from(40),
                Value::from(20),
            ]),
        );
        assert_eq!(element_rect_centre(&e), Ok((120, 210)));
    }

    /// The same gate through a real burst: an element whose rect has no area
    /// is refused with `offscreen_element` and nothing is delivered. On the
    /// pre-fix tree this call posted a button at the target's client origin
    /// (`msg_counts().0 == 1`), which is the wrong click the guard closes.
    #[test]
    fn click_on_an_element_without_a_rect_is_refused_and_never_posts() {
        let _g = crate::fixture::desktop();
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        // Collapse every child in place: no control then has a pixel to aim
        // at (UIA drops zero-size children, so the walk falls back to the
        // win32 backend, which reports the same zero-area rects). This is the
        // real class: a rect with no area, whichever backend measured it.
        for child in [fx.edit(), fx.edit_ro(), fx.button()] {
            let moved = unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::SetWindowPos(
                    child,
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                    0,
                    windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOMOVE
                        | windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOZORDER
                        | windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
                )
            };
            assert_ne!(moved, 0, "collapse a fixture child");
        }
        // The walk is asynchronous after ShowWindow, so poll for the entry
        // and click the one it actually produced.
        let (path, fp, rect) = {
            let t0 = std::time::Instant::now();
            loop {
                let m = element_model(fx.hwnd(), crate::UIA_DEPTH_LIMIT, 0);
                let hit = m
                    .entries
                    .iter()
                    .find(|e| e.get("offscreen") == Some(&Value::from(true)));
                if let Some(e) = hit {
                    break (
                        entry_str(e, "path"),
                        entry_str(e, "fp"),
                        e.get("rect").cloned().unwrap_or(Value::Null),
                    );
                }
                if t0.elapsed() > std::time::Duration::from_secs(10) {
                    panic!("no zero-area entry appeared: {:?}", m.entries);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };
        crate::fixture::reset_msg_counts();
        let actions = vec![
            serde_json::json!({"type": "click", "element": format!("path:{path}"), "expect_fp": fp}),
        ];
        let out = do_burst(&DoArgs {
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            ..DoArgs::new(&actions)
        });
        let payload = sc(&out);
        let r = payload["results"][0].clone();
        assert_eq!(r["ok"], Value::from(false), "refused, not clicked: {r}");
        assert_eq!(r["code"], Value::from("offscreen_element"), "{r}");
        let err = r["error"].as_str().unwrap_or("");
        assert!(err.contains("scroll it into view"), "names the fix: {err}");
        assert_ne!(
            r["via"],
            Value::from("posted-click"),
            "no click may be posted: {r}"
        );
        let (down, up, ..) = crate::fixture::msg_counts();
        assert_eq!((down, up), (0, 0), "no button message may be delivered");
        assert_eq!(
            payload["sendinput_calls"],
            Value::from(0),
            "no synthetic input may be sent"
        );
        println!("OFFSCREEN_CLICK_REFUSED_OK code=offscreen_element posts=0 rect={rect}");
    }

    /// P2 dry-run honesty: a dry run must report the refusal the live path
    /// gives. It used to return `ok:true` for an element the live click
    /// refuses with `offscreen_element`, so a caller who dry-ran a burst got a
    /// green light for a click that could never land.
    #[test]
    fn dry_run_click_on_an_element_without_a_rect_is_refused_like_the_live_path() {
        let _g = crate::fixture::desktop();
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        for child in [fx.edit(), fx.edit_ro(), fx.button()] {
            let moved = unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::SetWindowPos(
                    child,
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                    0,
                    windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOMOVE
                        | windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOZORDER
                        | windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
                )
            };
            assert_ne!(moved, 0, "collapse a fixture child");
        }
        let (path, fp) = {
            let t0 = std::time::Instant::now();
            loop {
                let m = element_model(fx.hwnd(), crate::UIA_DEPTH_LIMIT, 0);
                if let Some(e) = m
                    .entries
                    .iter()
                    .find(|e| e.get("offscreen") == Some(&Value::from(true)))
                {
                    break (entry_str(e, "path"), entry_str(e, "fp"));
                }
                if t0.elapsed() > std::time::Duration::from_secs(10) {
                    panic!("no zero-area entry appeared: {:?}", m.entries);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };
        let actions = vec![
            serde_json::json!({"type": "click", "element": format!("path:{path}"), "expect_fp": fp}),
        ];
        let out = do_burst(&DoArgs {
            dry_run: true,
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            ..DoArgs::new(&actions)
        });
        let payload = sc(&out);
        let r = payload["results"][0].clone();
        assert_eq!(
            r["ok"],
            Value::from(false),
            "a dry run must not report a click the live path would refuse: {r}"
        );
        assert_eq!(r["code"], Value::from("offscreen_element"), "{r}");
        assert_ne!(
            r["via"],
            Value::from("posted-click"),
            "no click may be claimed: {r}"
        );
        assert_eq!(
            payload["sendinput_calls"],
            Value::from(0),
            "a dry run sends nothing either way"
        );
        println!("OFFSCREEN_CLICK_DRY_RUN_REFUSED_OK code=offscreen_element");
    }

    /// P1 focus_window lock: a `hwnd` that names a window is refused with
    /// the POLICY code (`no_foreground_policy`) — focusing IS taking the
    /// foreground, and the gate fires before anything else. No window is
    /// touched, recycled handle or not.
    #[test]
    fn focus_window_refuses_with_the_policy_code() {
        let _g = crate::fixture::desktop();
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        let actions = vec![serde_json::json!({"type": "focus_window", "hwnd": hwnd})];
        let out = do_burst(&DoArgs::new(&actions));
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let payload = outer["structuredContent"].clone();
        let first = payload["results"][0].clone();
        assert_eq!(
            first.get("ok"),
            Some(&Value::from(false)),
            "nothing may be focused: {first}"
        );
        assert_eq!(
            first.get("code"),
            Some(&Value::from("no_foreground_policy")),
            "the refusal must carry the policy code: {first}"
        );
        assert_eq!(
            payload["focus_before"], payload["focus_after"],
            "a refused focus must not change the foreground window"
        );
    }

    fn burst_first(actions: &[Value]) -> Map<String, Value> {
        let out = do_burst(&DoArgs::dry(actions));
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
    fn click_count_bound_rejected_before_posting() {
        // The bound is checked in the posted path before any message: a
        // live count of 101 to a locked target fails typed and posts zero.
        let _g = crate::fixture::desktop();
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        crate::fixture::reset_msg_counts();
        let out = do_burst(&DoArgs {
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            ..DoArgs::new(&[serde_json::json!({"type": "click", "count": 101, "x": 5, "y": 5})])
        });
        let payload = sc(&out);
        let first = &payload["results"][0];
        assert_eq!(first.get("ok"), Some(&Value::from(false)));
        let err = first.get("error").and_then(|e| e.as_str()).unwrap_or("");
        assert!(
            err.contains("absurd"),
            "count error should name the bound, got: {err}"
        );
        let (down, up, ..) = crate::fixture::msg_counts();
        assert_eq!((down, up), (0, 0), "no button message may be delivered");
    }

    #[test]
    fn click_count_sane_passes_dry_run() {
        let _g = crate::fixture::desktop();
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        let out = do_burst(&DoArgs {
            dry_run: true,
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            ..DoArgs::new(&[serde_json::json!({"type": "click", "count": 2, "x": 5, "y": 5})])
        });
        let payload = sc(&out);
        assert_eq!(payload["results"][0].get("ok"), Some(&Value::from(true)));
    }

    fn burst_payload(actions: &[Value]) -> Map<String, Value> {
        let out = do_burst(&DoArgs::dry(actions));
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

    /// Posted-key lParam assembly: Space is the proven pair
    /// (down `0x00390001`, up `0xC0390001`); F5 carries scan `0x3F` with no
    /// extended bit; Right-arrow sets the extended bit; Alt selects the
    /// SYSKEY variants.
    #[test]
    fn posted_key_lparam_assembly() {
        assert_eq!(posted_key_lparam(0x20, false, false) as u32, 0x00390001);
        assert_eq!(posted_key_lparam(0x20, true, false) as u32, 0xC0390001);
        // F5: scan 0x3F, not an extended key.
        assert!(!is_extended_vk(0x74));
        assert_eq!(posted_key_lparam(0x74, false, false) as u32, 0x003F0001);
        assert_eq!(posted_key_lparam(0x74, true, false) as u32, 0xC03F0001);
        // Right arrow: extended bit set.
        assert!(is_extended_vk(0x27));
        let down = posted_key_lparam(0x27, false, false) as u32;
        assert_ne!(down & (1 << 24), 0, "extended bit must be set: {down:#x}");
        let up = posted_key_lparam(0x27, true, false) as u32;
        assert_ne!(up & (1 << 24), 0, "extended bit survives key-up: {up:#x}");
        assert_ne!(up & (1 << 30), 0);
        assert_ne!(up & (1 << 31), 0);
        // Plain keys never set the context bit; Alt does, with SYSKEY msgs.
        let down = posted_key_lparam(0x41, false, false) as u32;
        assert_eq!(down & (1 << 29), 0);
        let down = posted_key_lparam(0x41, false, true) as u32;
        assert_ne!(down & (1 << 29), 0, "Alt sets the context code: {down:#x}");
        assert_eq!(posted_key_downs(&KeyCode::Vk(0x12, false))[0].0, 0x0104);
        assert_eq!(posted_key_ups(&KeyCode::Vk(0x12, false))[0].0, 0x0105);
        assert_eq!(posted_key_downs(&KeyCode::Vk(0x41, false))[0].0, 0x0100);
        assert_eq!(posted_key_ups(&KeyCode::Vk(0x41, false))[0].0, 0x0101);
        println!("POSTED_KEY_LPARAM_OK space+f5+extended+alt-syskey");
    }

    #[test]
    fn key_shift_wraps_in_posted_order() {
        // `A` needs Shift: down order is Shift, key; up order is key, Shift.
        let downs = posted_key_downs(&KeyCode::Vk(0x41, true));
        assert_eq!(
            downs.iter().map(|(m, w, _)| (*m, *w)).collect::<Vec<_>>(),
            vec![(0x0100, VK_SHIFT as u64), (0x0100, 0x41)],
            "shift must wrap the key down"
        );
        let ups = posted_key_ups(&KeyCode::Vk(0x41, true));
        assert_eq!(
            ups.iter().map(|(m, w, _)| (*m, *w)).collect::<Vec<_>>(),
            vec![(0x0101, 0x41), (0x0101, VK_SHIFT as u64)],
            "shift must unwrap the key up"
        );
        // Hotkey order: presses in order, releases reversed.
        let codes = [KeyCode::Vk(0x11, false), KeyCode::Vk(0x41, true)];
        let downs: Vec<(u32, u64)> = codes
            .iter()
            .flat_map(posted_key_downs)
            .map(|(m, w, _)| (m, w))
            .collect();
        assert_eq!(
            downs,
            vec![(0x0100, 0x11), (0x0100, VK_SHIFT as u64), (0x0100, 0x41)],
            "hotkey must press in order, got: {downs:?}"
        );
        let ups: Vec<(u32, u64)> = codes
            .iter()
            .rev()
            .flat_map(posted_key_ups)
            .map(|(m, w, _)| (m, w))
            .collect();
        assert_eq!(
            ups,
            vec![(0x0101, 0x41), (0x0101, VK_SHIFT as u64), (0x0101, 0x11)],
            "hotkey must release in reverse, got: {ups:?}"
        );
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
        let _g = crate::fixture::desktop();
        // Dry-run must validate `state` before the early ok:true return:
        // bogus state fails typed even though the element resolves.
        let (fx, bg) = fixture_bg();
        let _ = fx.hwnd();
        let fp = live_fp(fx.hwnd(), "name", "OK");
        let actions = vec![
            serde_json::json!({"type": "invoke", "element": "name:OK", "state": "sideways", "expect_fp": fp}),
        ];
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
        let _g = crate::fixture::desktop();
        // Valid state passes dry-run resolution (ok, found, no firing).
        let (fx, bg) = fixture_bg();
        let _ = fx.hwnd();
        let fp = live_fp(fx.hwnd(), "name", "OK");
        let actions = vec![
            serde_json::json!({"type": "invoke", "element": "name:OK", "state": "expand", "expect_fp": fp}),
        ];
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
    fn scroll_axis_h_selects_horizontal_wheel_with_target() {
        // A locked target receives the horizontal wheel; the axis still
        // parses first, so a bogus axis fails typed with zero posts.
        let _g = crate::fixture::desktop();
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        crate::fixture::reset_msg_counts();
        let out = do_burst(&DoArgs {
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            ..DoArgs::new(&[
                serde_json::json!({"type": "scroll", "x": 5, "y": 5, "amount": 2, "axis": "h"}),
            ])
        });
        let payload = sc(&out);
        let results = payload["results"].as_array().expect("results");
        assert_eq!(results[0].get("ok"), Some(&Value::from(true)));
        let out = do_burst(&DoArgs {
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            ..DoArgs::new(&[
                serde_json::json!({"type": "scroll", "x": 5, "y": 5, "amount": 2, "axis": "bogus"}),
            ])
        });
        let payload = sc(&out);
        assert_eq!(payload["results"][0].get("ok"), Some(&Value::from(false)));
    }

    /// The pointer is never moved: every move refuses with the policy
    /// code, whatever the coordinates, and sends nothing.
    #[test]
    fn move_always_refuses_with_no_cursor_policy() {
        for action in [
            serde_json::json!({"type": "move", "dx": 30000, "dy": -30000}),
            serde_json::json!({"type": "move", "x": 10, "y": 10}),
            serde_json::json!({"type": "move", "x": 10, "y": 10, "dx": 1, "dy": 1}),
            serde_json::json!({"type": "move", "dx": 5}),
        ] {
            let first = burst_first(&[action]);
            assert_eq!(first.get("ok"), Some(&Value::from(false)));
            let code = first.get("code").and_then(|c| c.as_str());
            // Shape errors (mixed/incomplete) surface before the policy one.
            assert!(
                code == Some("no_cursor_policy") || first.get("error").is_some(),
                "move must refuse, got: {first:?}"
            );
        }
        let p = burst_payload(&[serde_json::json!({"type": "move", "x": 10, "y": 10})]);
        assert_eq!(
            p["results"][0]["code"],
            Value::from("no_cursor_policy"),
            "move must name the rule"
        );
        assert_eq!(p.get("sendinput_calls"), Some(&Value::from(0)));
        assert_eq!(p.get("events"), Some(&Value::from(0)));
    }

    #[test]
    fn key_repeat_and_hold_post_without_event_counts() {
        // Posted keys report delivery, not event counts: repeat=3 with a
        // locked target dry-runs ok with zero events and zero SendInput.
        let _g = crate::fixture::desktop();
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        let out = do_burst(&DoArgs {
            dry_run: true,
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            ..DoArgs::new(&[
                serde_json::json!({"type": "key", "name": "a", "repeat": 3, "hold_ms": 10}),
            ])
        });
        let payload = sc(&out);
        let results = payload["results"].as_array().expect("results");
        assert_eq!(results[0].get("ok"), Some(&Value::from(true)));
        assert_eq!(results[0].get("delivered"), Some(&Value::from("posted")));
        assert_eq!(payload.get("events"), Some(&Value::from(0)));
        assert_eq!(payload.get("sendinput_calls"), Some(&Value::from(0)));
    }

    #[test]
    fn key_hold_repeat_bounds_rejected() {
        let _g = crate::fixture::desktop();
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        for bad in [
            serde_json::json!({"type": "key", "name": "a", "repeat": 0}),
            serde_json::json!({"type": "key", "name": "a", "repeat": 101}),
            serde_json::json!({"type": "key", "name": "a", "hold_ms": 60001}),
            serde_json::json!({"type": "key", "name": "a", "gap_ms": 60001}),
            serde_json::json!({"type": "key", "name": "a", "hold_ms": -1}),
        ] {
            let out = do_burst(&DoArgs {
                dry_run: true,
                target: Some(serde_json::json!({ "hwnd": hwnd })),
                ..DoArgs::new(&[bad])
            });
            let payload = sc(&out);
            let first = &payload["results"][0];
            assert_eq!(
                first.get("ok"),
                Some(&Value::from(false)),
                "out-of-range hold/repeat must fail, got: {first:?}"
            );
        }
    }

    #[test]
    fn posted_key_dry_run_reports_posted_without_sending() {
        // A dry-run posted key assembles nothing and sends nothing, but
        // reports the path it WOULD take — never a silent ok.
        let _g = crate::fixture::desktop();
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        let out = do_burst(&DoArgs {
            dry_run: true,
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            ..DoArgs::new(&[serde_json::json!({"type": "key", "name": "space"})])
        });
        let payload = sc(&out);
        let r = &payload["results"][0];
        assert_eq!(r["ok"], Value::from(true), "dry-run posted key: {r}");
        assert_eq!(r["delivered"], Value::from("posted"), "{r}");
        assert_eq!(r["effect"], Value::from("unverified"), "{r}");
        assert_eq!(r["posted"], Value::from(0), "{r}");
        assert_eq!(r["dry_run"], Value::from(true), "{r}");
        assert_eq!(payload["sendinput_calls"], Value::from(0));
        assert_eq!(payload["events"], Value::from(0));
        println!("POSTED_KEY_DRY_RUN_OK delivered=posted effect=unverified");
    }

    #[test]
    fn scroll_element_dry_run_assembles_no_wheel() {
        let _g = crate::fixture::desktop();
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
        let _g = crate::fixture::desktop();
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
        let _g = crate::fixture::desktop();
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
        // The policy refusal lands before any length check or clipboard
        // API: the owner clipboard is never opened, even for over-cap text.
        let big = "x".repeat(1_048_576 + 1);
        let actions = vec![serde_json::json!({"type": "clipboard_set", "text": big})];
        let out = do_burst(&DoArgs::new(&actions));
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let first = outer
            .get("structuredContent")
            .and_then(|v| v.get("results"))
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .expect("first result");
        assert_eq!(first.get("ok"), Some(&Value::from(false)));
        assert_eq!(
            first.get("code").and_then(|c| c.as_str()),
            Some("no_clipboard_policy"),
            "over-cap set must name the policy, got: {first:?}"
        );
    }

    /// A burst over the 256-action cap is refused whole with a typed
    /// per-result failure naming the limit; nothing runs (no wait sleeps).
    #[test]
    fn burst_over_the_action_cap_is_a_typed_refusal() {
        let actions: Vec<Value> = (0..=BURST_MAX_ACTIONS)
            .map(|_| serde_json::json!({"type": "wait", "ms": 0}))
            .collect();
        assert_eq!(actions.len(), BURST_MAX_ACTIONS + 1);
        let out = do_burst(&DoArgs::new(&actions));
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let sc = outer.get("structuredContent").expect("payload");
        assert_eq!(sc.get("ok"), Some(&Value::from(false)));
        let results = sc["results"].as_array().expect("results");
        assert_eq!(results.len(), 1, "a refused burst runs nothing");
        assert_eq!(
            results[0].get("code"),
            Some(&Value::from("burst_budget_exceeded")),
            "the cap needs the typed code, got: {:?}",
            results[0]
        );
        assert!(
            results[0]
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .contains("256"),
            "the refusal must name the limit, got: {:?}",
            results[0]
        );
    }

    /// Exactly the cap still runs: the refusal is off-by-one-free.
    #[test]
    fn burst_at_the_action_cap_still_runs() {
        let actions: Vec<Value> = (0..BURST_MAX_ACTIONS)
            .map(|_| serde_json::json!({"type": "wait", "ms": 0}))
            .collect();
        let out = do_burst(&DoArgs {
            settle_ms: 0,
            ..DoArgs::new(&actions)
        });
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let sc = outer.get("structuredContent").expect("payload");
        assert_eq!(
            sc["results"].as_array().map(|a| a.len()),
            Some(BURST_MAX_ACTIONS),
            "a burst at the cap must run every action"
        );
        assert_eq!(sc.get("failed"), Some(&Value::from(0)));
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
            Some("no_clipboard_policy"),
            "clipboard_set is always refused, got: {first:?}"
        );
        assert!(
            first
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .contains("background-only policy"),
            "refusal must name the rule, got: {first:?}"
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
        let _g = crate::fixture::desktop();
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
        // Background-scoped runs: no flags (no flag buys anything under the
        // background-only policy).
        let out = do_burst(&DoArgs {
            dry_run,
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

    /// Synchronously paint a fixture window (plus children) before a
    /// verify capture: a fresh window may not have processed WM_PAINT yet,
    /// and PrintWindow of an unpainted frame reads back all-black.
    fn force_paint(hwnd: i64) {
        unsafe {
            windows_sys::Win32::Graphics::Gdi::RedrawWindow(
                hwnd as windows_sys::Win32::Foundation::HWND,
                std::ptr::null(),
                std::ptr::null_mut(),
                windows_sys::Win32::Graphics::Gdi::RDW_INVALIDATE
                    | windows_sys::Win32::Graphics::Gdi::RDW_UPDATENOW
                    | windows_sys::Win32::Graphics::Gdi::RDW_ALLCHILDREN,
            );
        }
    }

    /// Background-only policy: every set_value/type-on-element is refused
    /// with background_unavailable naming the element, before any provider
    /// call — dry_run included. The EDIT must be untouched.
    fn assert_set_value_refused(first: &Map<String, Value>, element: &str) {
        assert_eq!(first.get("ok"), Some(&Value::from(false)));
        assert_eq!(
            first.get("code").and_then(|c| c.as_str()),
            Some("background_unavailable"),
            "set_value must name the policy, got: {first:?}"
        );
        let e = first.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            e.contains(element),
            "refusal must name the element {element:?}, got: {e}"
        );
        assert!(
            e.contains("background-only policy"),
            "refusal must name the rule, got: {e}"
        );
    }

    #[test]
    fn set_value_missing_element_rejected() {
        let _g = crate::fixture::desktop();
        let (fx, bg) = fixture_bg();
        let _ = fx.hwnd();
        let actions = vec![serde_json::json!({"type": "set_value", "text": "hi"})];
        let (_, first) = burst_bg(&actions, &bg, true);
        assert_set_value_refused(&first, "");
        assert_eq!(fx.edit_text(), "", "no provider call may have written");
    }

    #[test]
    fn set_value_missing_text_rejected() {
        let _g = crate::fixture::desktop();
        // The policy refusal lands before any argument validation: naming
        // the element is the whole diagnosis.
        let (fx, bg) = fixture_bg();
        let _ = fx.hwnd();
        let actions = vec![serde_json::json!({"type": "set_value", "element": "name:OK"})];
        let (_, first) = burst_bg(&actions, &bg, true);
        assert_set_value_refused(&first, "name:OK");
    }

    #[test]
    fn set_value_ref_rejected_like_invoke() {
        let _g = crate::fixture::desktop();
        // set_value refuses before match_element ever sees the selector.
        let (fx, bg) = fixture_bg();
        let _ = fx.hwnd();
        let actions =
            vec![serde_json::json!({"type": "set_value", "element": "ref:123.0", "text": "hi"})];
        let (_, first) = burst_bg(&actions, &bg, true);
        assert_set_value_refused(&first, "ref:123.0");
    }

    #[test]
    fn set_value_not_found_typed() {
        let _g = crate::fixture::desktop();
        let (fx, bg) = fixture_bg();
        let _ = fx.hwnd();
        let actions = vec![
            serde_json::json!({"type": "set_value", "element": "name:NoSuchControlXYZ123", "text": "hi"}),
        ];
        let (_, first) = burst_bg(&actions, &bg, true);
        assert_set_value_refused(&first, "name:NoSuchControlXYZ123");
    }

    #[test]
    fn set_value_button_has_no_valuepattern() {
        let _g = crate::fixture::desktop();
        // The policy refusal lands before any ValuePattern probe: the OK
        // button is named, never written.
        let (fx, bg) = fixture_bg();
        let _ = fx.hwnd();
        wait_uia_stable(fx.hwnd());
        let actions =
            vec![serde_json::json!({"type": "set_value", "element": "name:OK", "text": "hi"})];
        let (_, first) = burst_bg(&actions, &bg, false);
        assert_set_value_refused(&first, "name:OK");
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

    /// The live fingerprint of one control from a fresh walk. W2.1 makes it
    /// mandatory for every element mutation, so a test that means to prove
    /// something else passes the current value.
    fn live_fp(hwnd: HWND, key: &str, want: &str) -> String {
        let m = element_model(hwnd, crate::UIA_DEPTH_LIMIT, 0);
        m.entries
            .iter()
            .find(|e| entry_str(e, key) == want)
            .map(|e| entry_str(e, "fp"))
            .unwrap_or_else(|| panic!("the walk must list {key}:{want}"))
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
        let _g = crate::fixture::desktop();
        let (fx, bg, _w, ro) = edit_paths();
        let _ = fx.hwnd();
        let actions = vec![
            serde_json::json!({"type": "set_value", "element": format!("path:{ro}"), "text": "hi"}),
        ];
        let (_, first) = burst_bg(&actions, &bg, false);
        assert_set_value_refused(&first, &format!("path:{ro}"));
        assert_eq!(fx.edit_text(), "", "the EDIT must be untouched");
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
        let _g = crate::fixture::desktop();
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
        let _g = crate::fixture::desktop();
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

    /// Gap-1 proof: the value is never written and never echoed. The refusal
    /// names the control, not the secret; the EDIT stays empty.
    #[test]
    fn set_value_success_writes_and_never_echoes() {
        let _g = crate::fixture::desktop();
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
        assert_set_value_refused(&first, &format!("path:{wpath}"));
        assert!(
            !out.contains(secret),
            "refusal must never echo the text value"
        );
        // Read-back proves nothing landed in the EDIT control.
        assert_eq!(fx.edit_text(), "");
        println!("SET_VALUE_READBACK= (refused, untouched)");
        // Foreground contract: the policy refusal runs before any provider
        // call, so the foreground is never touched.
        println!(
            "FG_INFO: set_value: before={fg_before} after={} fixture={} (refused)",
            fg_now(),
            fx.hwnd_i64()
        );
    }

    /// Puppet-side "no activation" gate: the policy refusal runs before any
    /// provider call, so the foreground contract IS stable here.
    #[test]
    fn set_value_dry_run_leaves_foreground_clean() {
        let _g = crate::fixture::desktop();
        let fg_before = crate::fixture::foreground_now();
        let (fx, bg, wpath, _ro) = edit_paths();
        let _ = fx.hwnd();
        let actions = vec![
            serde_json::json!({"type": "set_value", "element": format!("path:{wpath}"), "text": "no-focus-probe"}),
        ];
        let (_, first) = burst_bg(&actions, &bg, true);
        assert_set_value_refused(&first, &format!("path:{wpath}"));
        crate::fixture::check_foreground_left_clean(fx.hwnd_i64(), fg_before, "set_value:dry_run");
    }

    /// Gap-1 proof (fails before the fix, passes after): a fingerprint from
    /// an older snapshot must not silently address a different control.
    /// Resolve the live OK button, then invoke it with a WRONG expect_fp:
    /// the burst must refuse with a typed stale_ref (plus fresh-snapshot
    /// recovery advice), never act.
    #[test]
    fn invoke_stale_fp_refused_with_typed_error() {
        let _g = crate::fixture::desktop();
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

    /// Background-only policy: the UIA write never happens, so a stale
    /// fingerprint refuses with background_unavailable (not stale_ref) —
    /// the policy check runs before any resolution.
    #[test]
    fn set_value_stale_fp_refused_with_typed_error() {
        let _g = crate::fixture::desktop();
        // Same action shape, policy refusal first: wrong expect_fp never
        // reaches the freshness gate, dry_run included.
        let (fx, bg, wpath, _ro) = edit_paths();
        let _ = fx.hwnd();
        let _ = wpath;
        let actions = vec![
            serde_json::json!({"type": "set_value", "element": "name:OK", "text": "hi", "expect_fp": "definitely-wrong-fingerprint"}),
        ];
        let (_, first) = burst_bg(&actions, &bg, true);
        assert_set_value_refused(&first, "name:OK");
    }

    #[test]
    fn invoke_matching_fp_resolves() {
        let _g = crate::fixture::desktop();
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
        let _g = crate::fixture::desktop();
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
        let _g = crate::fixture::desktop();
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
        let _g = crate::fixture::desktop();
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
        let _g = crate::fixture::desktop();
        // Per-action ok:false never aborts the rest of the burst; both
        // refuse with the policy code and nothing is written.
        let (fx, bg, wpath, _ro) = edit_paths();
        let _ = fx.hwnd();
        let actions = vec![
            serde_json::json!({"type": "set_value", "element": "name:NoSuchXYZ", "text": "x"}),
            serde_json::json!({"type": "set_value", "element": format!("path:{wpath}"), "text": "after-failure"}),
        ];
        let out = do_burst(&DoArgs {
            background: Some(bg.to_string()),
            ..DoArgs::new(&actions)
        });
        let outer: Value = serde_json::from_str(&out).expect("outer");
        let payload = outer["structuredContent"].clone();
        let res = payload["results"].as_array().expect("results");
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].get("ok"), Some(&Value::from(false)));
        assert_eq!(res[1].get("ok"), Some(&Value::from(false)));
        assert_eq!(
            res[1].get("code").and_then(|c| c.as_str()),
            Some("background_unavailable")
        );
        assert_eq!(fx.edit_text(), "");
    }

    // ------------------------------------------------ no-steal (target lock + gate)

    fn sc(out: &str) -> Value {
        let outer: Value = serde_json::from_str(out).expect("outer frame");
        outer
            .get("structuredContent")
            .cloned()
            .expect("structuredContent payload")
    }

    /// The foreground handle is the property the tool CAN violate, so a
    /// harness-owned foreground fails strictly — with no exemption, and with
    /// both HWNDs plus the new owner's module in the message. A foreground
    /// change onto a window this process does NOT own is a third party taking
    /// focus (owner activity, shell churn, a parallel test binary): the gate
    /// re-baselines and samples again rather than blaming the tool, because
    /// attributing another process's activation to the harness is a false
    /// alarm, not evidence. See `fixture::assert_foreground_held_owned` for
    /// the decision rule, the sample bound and the log lines.
    fn assert_foreground_held(before: i64, owned: &crate::fixture::OwnedWindows, what: &str) {
        crate::fixture::assert_foreground_held_owned(before, owned, what);
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

    /// LIVE no-steal proof: set_value and type-on-element are refused with
    /// background_unavailable naming the control — no provider call, no
    /// value written. No flags, no pointer, no focus.
    #[test]
    fn uia_text_entry_is_refused_by_policy() {
        let _g = crate::fixture::desktop();
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
                Value::from("background_unavailable"),
                "action {k} must name the policy: {r}"
            );
            assert!(
                r["error"].as_str().unwrap_or("").contains("auto_id:100"),
                "action {k} must name the control: {r}"
            );
            assert!(r.get("via").is_none(), "no provider path may run: {r}");
        }
        assert_eq!(
            fx.edit_text(),
            "",
            "no provider call may have written the control"
        );
        assert_eq!(payload["sendinput_calls"], Value::from(0));
        assert_eq!(payload["events"], Value::from(0));
        println!("UIA_TEXT_POLICY_OK refusals=2 text=\"\"");
    }

    /// SAFETY: `desktop:"hidden"` plus an explicit ordinary target is just
    /// an ordinary target: posted keyboard goes to the locked window (never
    /// SendInput, never the foreground), like any target-locked burst. The
    /// private-desktop permission is still EARNED, never granted by the flag.
    #[test]
    fn hidden_flag_with_an_ordinary_target_earns_nothing() {
        let _g = crate::fixture::desktop();
        let fx = crate::fixture::Fixture::new();
        // Verify captures the locked target: use the EDIT child (a real
        // control PrintWindow renders) instead of the offscreen frame.
        let hwnd = fx.edit() as i64;
        // Paint before the verify captures: a fresh window may not have
        // processed WM_PAINT yet and would read back all-black.
        force_paint(hwnd);
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
            assert_eq!(r["ok"], Value::from(true), "action {k}: {r}");
            assert_eq!(
                r["delivered"],
                Value::from("posted"),
                "action {k} must post to the locked target: {r}"
            );
        }
        assert_eq!(payload["sendinput_calls"], Value::from(0));
        // The predicate itself: a desktop that was not created grants nothing,
        // and a window on a different desktop never matches the created name.
        assert!(!private_desktop_earned(false, "hidden-x", Some("hidden-x")));
        assert!(!private_desktop_earned(true, "hidden-x", Some("Default")));
        assert!(!private_desktop_earned(true, "", Some("hidden-x")));
        assert!(private_desktop_earned(true, "hidden-x", Some("hidden-x")));
        println!("HIDDEN_BYPASS_CLOSED_OK posted=3 sendinput=0");
    }

    /// The desktop-name read is real: an ordinary fixture lives on the
    /// owner's desktop, never on a made-up private one.
    #[test]
    fn window_desktop_name_reads_a_real_desktop() {
        let _g = crate::fixture::desktop();
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

    /// LIVE: a target-locked burst delivers a posted click and posted text
    /// to an offscreen fixture window (its own WndProc counts the click, the
    /// EDIT guard counts the swallowed WM_CHAR) while GetForegroundWindow
    /// and GetCursorPos stay exactly where they were. No flags, no pointer,
    /// no focus, no SendInput.
    #[test]
    fn live_target_post_delivers_without_stealing() {
        let _g = crate::fixture::desktop();
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        crate::fixture::reset_msg_counts();
        // The guard on the EDITs swallows posted WM_CHAR (proving they
        // arrived at a text control) instead of inserting them.
        crate::fixture::reset_swallowed_keys(fx.edit());
        crate::fixture::reset_swallowed_keys(fx.edit_ro());
        let fg_before = crate::fixture::foreground_now();
        let owned = crate::fixture::OwnedWindows::new().with_fixture(&fx);
        let cur_before = cursor_now();
        let actions = vec![
            serde_json::json!({"type": "click", "x": 20, "y": 90}),
            // Plain `type` posts WM_CHAR to the target's text control
            // (delivery, not proof of effect — the fixture guard swallows
            // it, which is exactly the counter below).
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
            Value::from(true),
            "plain type posts to the locked target: {results:?}"
        );
        assert_eq!(results[1]["delivered"], Value::from("posted"));
        assert_eq!(results[1]["effect"], Value::from("unverified"));
        assert_eq!(results[1]["posted"], Value::from(2), "h+i = 2 WM_CHAR");
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
            wait_until(|| {
                let (down, up, ..) = crate::fixture::msg_counts();
                down >= 1 && up >= 1
            }),
            "posted click pair must reach the target, got: {:?}",
            crate::fixture::msg_counts()
        );
        let (down, up, mv, _kd, _ch) = crate::fixture::msg_counts();
        assert!(
            down >= 1 && up >= 1,
            "down/up pair must arrive: {down}/{up}"
        );
        assert_eq!(mv, 0, "a plain click must not post WM_MOUSEMOVE");
        // The guard on an EDIT swallowed the two posted WM_CHAR (proving
        // they arrived at a text control) instead of inserting them. The
        // pump thread dispatches asynchronously, so wait like the click
        // pair above does.
        assert!(
            wait_until(|| {
                crate::fixture::swallowed_key_count(fx.edit())
                    + crate::fixture::swallowed_key_count(fx.edit_ro())
                    >= 2
            }),
            "posted WM_CHAR must reach a text control, swallowed(edit)={} swallowed(ro)={}",
            crate::fixture::swallowed_key_count(fx.edit()),
            crate::fixture::swallowed_key_count(fx.edit_ro())
        );
        assert_eq!(
            fx.edit_text(),
            "",
            "the guard swallows instead of inserting"
        );
        assert_foreground_held(fg_before, &owned, "post-delivery burst");
        report_pointer(cur_before, "post-delivery burst");
        assert_eq!(
            payload["sendinput_calls"],
            Value::from(0),
            "a target burst never calls SendInput"
        );
    }

    /// LIVE: plain `type` posts WM_CHAR to the locked target's text
    /// control — delivery, not proof of effect. The fixture guard swallows
    /// the characters (counter proof they arrived) instead of inserting
    /// them, so the EDIT stays empty while the post count is exact.
    #[test]
    fn live_type_posts_wm_char_to_the_locked_target() {
        let _g = crate::fixture::desktop();
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        crate::fixture::reset_msg_counts();
        crate::fixture::reset_swallowed_keys(fx.edit());
        crate::fixture::reset_swallowed_keys(fx.edit_ro());
        let actions = vec![serde_json::json!({
            "type": "type", "text": "hi"
        })];
        let out = do_burst(&DoArgs {
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            ..DoArgs::new(&actions)
        });
        let payload = sc(&out);
        let r = &payload["results"][0];
        assert_eq!(r["ok"], Value::from(true), "type posts: {r}");
        assert_eq!(r["delivered"], Value::from("posted"), "{r}");
        assert_eq!(r["effect"], Value::from("unverified"), "{r}");
        assert_eq!(r["posted"], Value::from(2), "h+i = 2 WM_CHAR: {r}");
        assert_eq!(
            fx.edit_text(),
            "",
            "the guard swallows instead of inserting"
        );
        assert!(
            wait_until(|| {
                crate::fixture::swallowed_key_count(fx.edit())
                    + crate::fixture::swallowed_key_count(fx.edit_ro())
                    >= 2
            }),
            "posted WM_CHAR must reach a text control, swallowed(edit)={} swallowed(ro)={}",
            crate::fixture::swallowed_key_count(fx.edit()),
            crate::fixture::swallowed_key_count(fx.edit_ro())
        );
        assert_eq!(payload["sendinput_calls"], Value::from(0));
        println!("TYPE_POSTED_OK text=\"\"");
    }

    /// LIVE: a posted element click is delivery, not proof. The fixture EDIT
    /// has no Invoke pattern, so the click is posted at its centre; its
    /// readable state (text/enabled) does not change, and the action must
    /// report `background_unavailable` with `delivered:"posted"` instead of
    /// claiming success.
    #[test]
    fn live_posted_element_click_with_unchanged_state_is_refused() {
        let _g = crate::fixture::desktop();
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        wait_uia_stable(fx.hwnd());
        let fp = live_fp(fx.hwnd(), "auto_id", "100");
        let actions = vec![serde_json::json!({
            "type": "click", "element": "auto_id:100", "expect_fp": fp
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
        let _g = crate::fixture::desktop();
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
        let _g = crate::fixture::desktop();
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
        // The fixture pumps on its own thread, so a read can see the first
        // messages of a posted group before the last one is dispatched. Wait
        // for the whole down/move/up group before measuring it.
        assert!(wait_until(|| {
            let (down, up, mv, _, _) = crate::fixture::msg_counts();
            down >= 1 && up >= 1 && mv >= 1
        }));
        let (down, up, mv, _, _) = crate::fixture::msg_counts();
        assert_eq!((down, up, mv), (1, 1, 1), "down, one move, up");
        assert!(wait_until(|| crate::fixture::wheel_count() >= 1));
        report_pointer(cur_before, "posted drag + wheel");
    }

    /// LIVE gate: no target and no flags refuses every injecting verb BEFORE
    /// acting - typed codes, zero events, desktop untouched.
    #[test]
    fn live_gate_refuses_without_target_or_flags() {
        let _g = crate::fixture::desktop();
        let fg_before = crate::fixture::foreground_now();
        let owned = crate::fixture::OwnedWindows::new();
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
                "no_cursor_policy",
                "target_required",
                "target_required",
                "no_cursor_policy",
                "target_required",
            ],
            "gate matrix, got: {results:?}"
        );
        for r in results {
            assert_eq!(r["ok"], Value::from(false));
        }
        assert_eq!(payload["sendinput_calls"], Value::from(0));
        assert_eq!(payload["events"], Value::from(0));
        assert_foreground_held(fg_before, &owned, "gate refusal");
        report_pointer(cur_before, "gate refusal");
    }

    /// LIVE: the explicit flags buy nothing - the same burst is refused per
    /// action with the policy code (dry_run proves it without touching the
    /// desktop). No flag reaches any injection API because none exists.
    #[test]
    fn live_explicit_flags_buy_nothing_dry_run() {
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
        assert_eq!(
            payload["results"][0]["code"],
            Value::from("no_foreground_policy"),
            "allow_foreground must refuse, not unlock: {payload:?}"
        );
        assert_eq!(
            payload["results"][1]["code"],
            Value::from("no_foreground_policy"),
            "allow_foreground must refuse, not unlock: {payload:?}"
        );
        assert_eq!(payload["sendinput_calls"], Value::from(0));
        assert_eq!(payload["events"], Value::from(0));
        println!("FLAGS_BUY_NOTHING_OK codes=no_foreground_policy events=0");
    }

    /// LIVE: a target that closed mid-call is `target_gone` on the next
    /// action (revalidation), never a retarget and never a foreground fall.
    #[test]
    fn live_target_gone_mid_burst_is_typed() {
        let _g = crate::fixture::desktop();
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
    /// GetCursorPos stay put; posted KEYS arrive too (the route is
    /// posted-only now), proven with verify:false; the default verify:true
    /// honestly reports unverified because a message-only window cannot be
    /// captured for the before/after diff.
    #[test]
    fn live_message_only_click_and_key_post_without_stealing() {
        let _g = crate::fixture::desktop();
        let f = crate::fixture::MessageWindow::new();
        let hwnd = f.hwnd_i64();
        crate::fixture::reset_msg_counts();
        let fg_before = crate::fixture::foreground_now();
        let owned = crate::fixture::OwnedWindows::new().with_top(hwnd);
        let cur_before = cursor_now();
        let actions = vec![
            serde_json::json!({"type": "click", "x": 4, "y": 6}),
            serde_json::json!({"type": "key", "name": "a", "verify": false}),
            serde_json::json!({"type": "hotkey", "keys": ["ctrl", "c"], "verify": false}),
            serde_json::json!({"type": "key", "name": "a"}),
        ];
        let out = do_burst(&DoArgs {
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            ..DoArgs::new(&actions)
        });
        let payload = sc(&out);
        let results = payload["results"].as_array().expect("results");
        assert_eq!(results[0]["ok"], Value::from(true), "click: {results:?}");
        // Posted keys land on the message-only target with verify:false.
        for k in [1, 2] {
            assert_eq!(
                results[k]["ok"],
                Value::from(true),
                "posted keyboard: {results:?}"
            );
            assert_eq!(results[k]["delivered"], Value::from("posted"));
            assert_eq!(results[k]["effect"], Value::from("unverified"));
        }
        // ...but the default verify:true cannot diff what cannot be
        // captured: honest unverified failure, posted bytes included.
        assert_eq!(results[3]["ok"], Value::from(false), "verify: {results:?}");
        assert_eq!(results[3]["delivered"], Value::from("none"));
        assert_eq!(results[3]["effect"], Value::from("unverified"));
        assert_eq!(payload["sendinput_calls"], Value::from(0));
        // Wait for the whole posted click pair (down AND up) before reading:
        // the fixture pumps on its own thread and may not have dispatched the
        // up yet when the down counter first turns non-zero.
        assert!(
            wait_until(|| {
                let (down, up, ..) = crate::fixture::msg_counts();
                down >= 1 && up >= 1
            }),
            "posted click pair must be received: {:?}",
            crate::fixture::msg_counts()
        );
        let (down, up, mv, kd, ch) = crate::fixture::msg_counts();
        assert_eq!((down, up, mv), (1, 1, 0), "one down, one up, no move");
        // Two posted keys (key a + ctrl+c) land as key messages.
        assert!(
            kd >= 3,
            "posted keys must arrive as WM_KEYDOWN: kd={kd} ch={ch}"
        );
        assert_eq!(
            results[0]["delivered"],
            Value::from("posted"),
            "a raw click reports delivery only: {results:?}"
        );
        assert_eq!(results[0]["effect"], Value::from("unverified"));
        assert_foreground_held(fg_before, &owned, "message-only delivery");
        report_pointer(cur_before, "message-only delivery");
    }

    /// BACKGROUND-ONLY POLICY refusal tests: `delivery:"sendinput"` and
    /// any unknown delivery never act — typed refusal, zero events, zero
    /// SendInput calls, under dry_run too.
    #[test]
    fn sendinput_delivery_and_unknown_delivery_are_refused() {
        let _g = crate::fixture::desktop();
        let fx = crate::fixture::Fixture::new();
        let hwnd = fx.hwnd_i64();
        for (action, want) in [
            (
                serde_json::json!({"type": "key", "name": "a", "delivery": "sendinput"}),
                "no_foreground_policy",
            ),
            (
                serde_json::json!({"type": "hotkey", "keys": ["ctrl", "c"], "delivery": "sendinput"}),
                "no_foreground_policy",
            ),
            (
                serde_json::json!({"type": "key", "name": "a", "delivery": "telepathy"}),
                "",
            ),
        ] {
            let out = do_burst(&DoArgs {
                dry_run: true,
                target: Some(serde_json::json!({ "hwnd": hwnd })),
                ..DoArgs::new(&[action])
            });
            let payload = sc(&out);
            let r = &payload["results"][0];
            assert_eq!(r["ok"], Value::from(false), "delivery refusal: {r}");
            if !want.is_empty() {
                assert_eq!(r["code"], Value::from(want), "typed refusal: {r}");
            }
            assert_eq!(payload["events"], Value::from(0));
            assert_eq!(payload["sendinput_calls"], Value::from(0));
        }
        println!("SENDINPUT_DELIVERY_REFUSED_OK code=no_foreground_policy events=0");
    }

    /// LIVE: `verify:true` on a key that changes nothing visible reports
    /// `effect:"unchanged"` — a result, not an error — with the exact zero
    /// count and a null bbox.
    #[test]
    fn verify_true_on_a_no_op_key_reports_unchanged() {
        let _g = crate::fixture::desktop();
        let fx = crate::fixture::Fixture::new();
        // Verify captures the locked target: use the EDIT child (a real
        // control PrintWindow renders) instead of the offscreen frame.
        let hwnd = fx.edit() as i64;
        // Paint before the verify captures: a fresh window may not have
        // processed WM_PAINT yet and would read back all-black.
        force_paint(hwnd);
        let fg_before = crate::fixture::foreground_now();
        let owned = crate::fixture::OwnedWindows::new().with_fixture(&fx);
        let out = do_burst(&DoArgs {
            target: Some(serde_json::json!({ "hwnd": hwnd })),
            ..DoArgs::new(&[serde_json::json!({"type": "key", "name": "F13", "verify": true})])
        });
        let payload = sc(&out);
        let r = &payload["results"][0];
        assert_eq!(r["ok"], Value::from(true), "verify on a no-op: {r}");
        assert_eq!(r["delivered"], Value::from("posted"), "{r}");
        assert_eq!(r["effect"], Value::from("unchanged"), "{r}");
        assert_eq!(r["changed"], Value::from(0), "{r}");
        assert!(r["bbox"].is_null(), "no pixels changed, no box: {r}");
        assert_eq!(payload["sendinput_calls"], Value::from(0));
        assert_foreground_held(fg_before, &owned, "verified posted key");
        println!("VERIFY_UNCHANGED_OK effect=unchanged changed=0");
    }

    /// LIVE: `click` on an element is UIA-pattern-first on the locked
    /// target (dry_run resolves and names the path without firing).
    #[test]
    fn live_click_on_element_resolves_to_uia_pattern() {
        let _g = crate::fixture::desktop();
        let (fx, bg) = fixture_bg();
        wait_uia_stable(fx.hwnd());
        let fp = live_fp(fx.hwnd(), "name", "OK");
        let actions = vec![serde_json::json!({
            "type": "click", "element": "name:OK", "expect_fp": fp
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
    /// never move the pointer: the policy refuses with no_cursor_policy
    /// BEFORE anything is posted: ok:false, code, zero events, zero posts.
    #[test]
    fn live_cursor_only_pointer_actions_are_typed_failures() {
        let _g = crate::fixture::desktop();
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
                Value::from("no_cursor_policy"),
                "action {k} must name the rule: {r}"
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

    /// LIVE: with BOTH flags every injecting verb still refuses — the flags
    /// buy nothing. The other half of "refuses or posts, never silent".
    #[test]
    fn live_both_flags_still_refuse() {
        let actions = vec![serde_json::json!({"type": "click", "x": 5, "y": 5})];
        let out = do_burst(&DoArgs {
            dry_run: true,
            allow_foreground: true,
            allow_cursor: true,
            ..DoArgs::new(&actions)
        });
        let payload = sc(&out);
        assert_eq!(payload["results"][0]["ok"], Value::from(false));
        assert_eq!(
            payload["results"][0]["code"],
            Value::from("no_foreground_policy"),
            "flags must refuse, not unlock: {payload:?}"
        );
        assert_eq!(payload["events"], Value::from(0));
        assert_eq!(payload["sendinput_calls"], Value::from(0));
    }

    /// The flush is a no-op by policy: `SendInput` is never called, so a
    /// flush never sends, never fails and never marks anything — the posted
    /// branches sequence through it harmlessly.
    #[test]
    fn flush_is_a_noop_and_sends_nothing() {
        let mut st = Burst {
            pending: Vec::new(),
            marked: vec![0],
            results: vec![base_entry(0, &Value::from("click"), true, 0.0)],
            calls: 0,
            events: 0,
            sendinput_ms: 0.0,
            flush_uipi: false,
        };
        let (dt, err) = st.flush();
        assert!(err.is_none(), "a flush never fails: {err:?}");
        assert_eq!(dt, 0.0);
        assert_eq!(st.events, 0, "nothing was sent");
        assert_eq!(st.calls, 0, "no SendInput call was made");
        assert_eq!(st.results[0]["ok"], Value::from(true), "nothing is marked");
    }

    /// LIVE, message-only: coordinates a message lParam cannot carry are a
    /// typed `bad_coordinates` refusal BEFORE posting; the 16-bit boundaries
    /// still post.
    #[test]
    fn live_bad_coordinates_refused_before_posting() {
        let _g = crate::fixture::desktop();
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
        let _g = crate::fixture::desktop();
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
        let _g = crate::fixture::desktop();
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
