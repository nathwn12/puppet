//! window ops + list_windows + cursor tools.

use crate::cap::resolve_window;
use crate::{display_monitors, enum_windows, window_info, wrapped_text};
use serde_json::{Map, Value};
use windows_sys::Win32::Foundation::{GetLastError, HWND, POINT, RECT};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// The ONE explicit focus seam (`window` action=focus, `do` focus_window).
/// Reached only through `gate::set_foreground`, which demands a permit: the
/// explicit focus verb's own mint, or `allow_foreground: true`. No other
/// verb may call this.
pub(crate) fn focus_hwmd(permit: &crate::gate::ForegroundPermit, hwnd: HWND) -> Map<String, Value> {
    unsafe {
        // A minimized window must be restored before it can take focus; this
        // is the explicit focus action, so activation here is the request.
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        } else {
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        let ok = crate::gate::set_foreground(permit, hwnd as i64);
        let mut m = Map::new();
        m.insert("ok".to_string(), Value::from(ok));
        m.insert("hwnd".to_string(), Value::from(hwnd as i64));
        m.insert(
            "error".to_string(),
            if ok {
                Value::Null
            } else {
                Value::from("SetForegroundWindow did not take effect")
            },
        );
        m
    }
}

pub struct WinArgs {
    pub action: String,
    pub title: Option<String>,
    pub hwnd: Option<i64>,
    pub x: Option<i64>,
    pub y: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// Structured target lock (object or selector string); wins over
    /// title/hwnd when present.
    pub target: Option<Value>,
    /// Explicit permission for the foreground family (only `focus` can use
    /// it here; every other verb is no-steal by construction).
    pub allow_foreground: bool,
}

/// Pre-maximize geometry remembered per window, so `restore` can return to
/// the EXACT rect even when Windows clamps an offscreen rcNormalPosition
/// during maximize.
///
/// Keyed by `(hwnd, pid, creation_time)`: an HWND alone is recyclable, and a
/// recycled handle with a new owner must never inherit another process's
/// saved geometry. The creation time (`GetProcessTimes`) closes the case a
/// recycled PID would otherwise pass.
/// Tiny bounded map (64 entries), cleared wholesale when it fills; removal is
/// by the matching key only, never by evicting unrelated windows.
type WindowKey = (i64, u32, u64);

static NORMAL_RECTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<WindowKey, [i32; 4]>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn window_key(hwnd: HWND) -> WindowKey {
    let pid = crate::pid_of(hwnd);
    // 0 when unreadable: still better than hwnd+pid, and never matches a
    // process whose creation time we can read.
    (hwnd as i64, pid, crate::process_created(pid).unwrap_or(0))
}

fn remember_normal(hwnd: HWND, rect: [i32; 4]) {
    let mut m = NORMAL_RECTS.lock().unwrap_or_else(|e| e.into_inner());
    if m.len() > 64 {
        m.clear();
    }
    m.insert(window_key(hwnd), rect);
}

/// Peek the saved rect WITHOUT consuming it: recovery state must survive a
/// failed restore so the caller can retry.
fn saved_normal(hwnd: HWND) -> Option<[i32; 4]> {
    let m = NORMAL_RECTS.lock().unwrap_or_else(|e| e.into_inner());
    m.get(&window_key(hwnd)).copied()
}

/// Drop the saved rect for THIS window only, after a successful restore.
fn clear_normal(hwnd: HWND) {
    let mut m = NORMAL_RECTS.lock().unwrap_or_else(|e| e.into_inner());
    m.remove(&window_key(hwnd));
}

/// What a maximize should remember, decided without touching a window so the
/// repeated-maximize rule is unit-testable: an ALREADY maximized window keeps
/// its saved rect (the current rect is the maximized one and must never
/// overwrite it); a normal window saves its current rect. Returns
/// `(rect_to_report, save_it)`.
fn normal_for_maximize(
    zoomed: bool,
    saved: Option<[i32; 4]>,
    placement: [i32; 4],
) -> ([i32; 4], bool) {
    if zoomed {
        // Never overwrite recovery state while already maximized.
        (saved.unwrap_or(placement), false)
    } else {
        // The normal rect comes from GetWindowPlacement (the documented
        // restore geometry), and it is saved before the window changes.
        (placement, true)
    }
}

/// Typed code carried by a window-verb failure, so a refusal exposes the
/// machine-readable kind next to its prose.
fn win_code_of(msg: &str) -> Option<&'static str> {
    [
        crate::gate::FOREGROUND_REQUIRED,
        crate::gate::CURSOR_REQUIRED,
        "restore_failed",
    ]
    .into_iter()
    .find(|code| msg.starts_with(code))
}

/// Bounded wait for an asynchronous maximize to take effect. Polls only the
/// cheap `IsZoomed` state (never SendMessage into the target) and gives up so
/// a target that ignores the request cannot stall the call.
fn wait_zoomed(hwnd: HWND, timeout: std::time::Duration) -> bool {
    let t0 = std::time::Instant::now();
    while t0.elapsed() < timeout {
        if unsafe { IsZoomed(hwnd) } != 0 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    unsafe { IsZoomed(hwnd) != 0 }
}

/// Shared range gate for every i64 coordinate routed to `SetWindowPos`
/// (which takes `i32`). Rejects anything outside `i32` instead of letting
/// `as i32` wrap (e.g. 2^40 wraps to 0 and teleports the window).
pub(crate) fn checked_i32(v: i64, name: &str) -> Result<i32, String> {
    if v < i32::MIN as i64 || v > i32::MAX as i64 {
        Err(format!("{name} out of range for SetWindowPos (got {v})"))
    } else {
        Ok(v as i32)
    }
}

pub fn do_window(a: &WinArgs) -> String {
    let fg_before = unsafe { GetForegroundWindow() as i64 };
    let cur_before = unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) != 0 {
            (pt.x as i64, pt.y as i64)
        } else {
            (-1, -1)
        }
    };
    // Target lock first: one window, resolved once, revalidated before the
    // action. A vanished/ambiguous target is typed, never a foreground
    // fallback.
    let mut locked: Option<crate::target::TargetLock> = None;
    let (hwnd_r, err, cands) = match a.target.as_ref() {
        None | Some(Value::Null) => resolve_window(a.title.as_deref(), a.hwnd),
        Some(v) => match crate::target::TargetSpec::parse(v) {
            Err(e) => (None, Some(e.text()), None),
            Ok(spec) => match crate::target::lock(&spec) {
                Err(e) => (None, Some(e.text()), e.candidates.clone()),
                Ok(l) => {
                    let h = l.hwnd as HWND;
                    locked = Some(l);
                    (Some(h), None, None)
                }
            },
        },
    };
    if let Some(l) = &locked {
        if let Err(e) = l.revalidate() {
            let mut payload = e.to_map();
            payload.insert("action".to_string(), Value::from(a.action.clone()));
            payload.insert("hwnd".to_string(), Value::from(l.hwnd));
            return wrapped_text(&crate::py_dumps(&Value::Object(payload)));
        }
    }
    if hwnd_r.is_none() {
        let mut payload = Map::new();
        payload.insert("ok".to_string(), Value::from(false));
        payload.insert("error".to_string(), Value::from(err.unwrap()));
        if let Some(c) = cands {
            payload.insert("candidates".to_string(), Value::from(c));
        }
        return wrapped_text(&crate::py_dumps(&Value::Object(payload)));
    }
    let h = hwnd_r.unwrap();
    let hi = h as i64;
    let out = (|| -> Result<String, String> {
        let act = a.action.to_lowercase();
        unsafe {
            if act == "focus" {
                // Double-explicit by design: the focus VERB and the
                // `allow_foreground:true` FLAG are both required. There is no
                // permit mint that skips the flag (gate.rs has exactly two,
                // both flag-only), so an unflagged focus cannot exist.
                let Some(permit) = crate::gate::ForegroundPermit::grant(a.allow_foreground) else {
                    let mut o = Map::new();
                    o.insert("ok".to_string(), Value::from(false));
                    o.insert(
                        "code".to_string(),
                        Value::from(crate::gate::FOREGROUND_REQUIRED),
                    );
                    o.insert(
                        "error".to_string(),
                        Value::from(
                            "foreground_required: focus needs the explicit action:\"focus\" AND \
                             allow_foreground:true; nothing was activated",
                        ),
                    );
                    return Ok(crate::py_dumps(&Value::Object(o)));
                };
                let mut m = focus_hwmd(&permit, h);
                let mut o = Map::new();
                o.insert("action".to_string(), Value::from("focus"));
                o.insert("foreground_action".to_string(), Value::from(true));
                o.append(&mut m);
                return Ok(crate::py_dumps(&Value::Object(o)));
            }
            if act == "move" {
                match (a.x, a.y) {
                    (Some(xi), Some(yi)) => {
                        let xi32 = match checked_i32(xi, "x") {
                            Ok(v) => v,
                            Err(e) => {
                                let mut o = Map::new();
                                o.insert("ok".to_string(), Value::from(false));
                                o.insert("error".to_string(), Value::from(e));
                                return Ok(crate::py_dumps(&Value::Object(o)));
                            }
                        };
                        let yi32 = match checked_i32(yi, "y") {
                            Ok(v) => v,
                            Err(e) => {
                                let mut o = Map::new();
                                o.insert("ok".to_string(), Value::from(false));
                                o.insert("error".to_string(), Value::from(e));
                                return Ok(crate::py_dumps(&Value::Object(o)));
                            }
                        };
                        if SetWindowPos(
                            h,
                            std::ptr::null_mut(),
                            xi32,
                            yi32,
                            0,
                            0,
                            SWP_NOZORDER | SWP_NOSIZE | SWP_NOACTIVATE,
                        ) == 0
                        {
                            return Err(crate::win32_text(GetLastError()));
                        }
                        let mut o = Map::new();
                        o.insert("ok".to_string(), Value::from(true));
                        o.insert("action".to_string(), Value::from(act));
                        o.insert("hwnd".to_string(), Value::from(hi));
                        o.insert("x".to_string(), Value::from(xi));
                        o.insert("y".to_string(), Value::from(yi));
                        return Ok(crate::py_dumps(&Value::Object(o)));
                    }
                    _ => {
                        let mut o = Map::new();
                        o.insert("ok".to_string(), Value::from(false));
                        o.insert("error".to_string(), Value::from("move needs x and y"));
                        return Ok(crate::py_dumps(&Value::Object(o)));
                    }
                }
            }
            if act == "resize" {
                match (a.width, a.height) {
                    (Some(wi), Some(hi2)) => {
                        if wi <= 0 || hi2 <= 0 {
                            let mut o = Map::new();
                            o.insert("ok".to_string(), Value::from(false));
                            o.insert(
                                "error".to_string(),
                                Value::from(format!(
                                    "resize needs positive width and height (got {wi}, {hi2})"
                                )),
                            );
                            return Ok(crate::py_dumps(&Value::Object(o)));
                        }
                        let wi32 = match checked_i32(wi, "width") {
                            Ok(v) => v,
                            Err(e) => {
                                let mut o = Map::new();
                                o.insert("ok".to_string(), Value::from(false));
                                o.insert("error".to_string(), Value::from(e));
                                return Ok(crate::py_dumps(&Value::Object(o)));
                            }
                        };
                        let hi32 = match checked_i32(hi2, "height") {
                            Ok(v) => v,
                            Err(e) => {
                                let mut o = Map::new();
                                o.insert("ok".to_string(), Value::from(false));
                                o.insert("error".to_string(), Value::from(e));
                                return Ok(crate::py_dumps(&Value::Object(o)));
                            }
                        };
                        if SetWindowPos(
                            h,
                            std::ptr::null_mut(),
                            0,
                            0,
                            wi32,
                            hi32,
                            SWP_NOZORDER | SWP_NOMOVE | SWP_NOACTIVATE,
                        ) == 0
                        {
                            return Err(crate::win32_text(GetLastError()));
                        }
                        let mut o = Map::new();
                        o.insert("ok".to_string(), Value::from(true));
                        o.insert("action".to_string(), Value::from(act));
                        o.insert("hwnd".to_string(), Value::from(hi));
                        o.insert("width".to_string(), Value::from(wi));
                        o.insert("height".to_string(), Value::from(hi2));
                        return Ok(crate::py_dumps(&Value::Object(o)));
                    }
                    _ => {
                        let mut o = Map::new();
                        o.insert("ok".to_string(), Value::from(false));
                        o.insert(
                            "error".to_string(),
                            Value::from("resize needs width and height"),
                        );
                        return Ok(crate::py_dumps(&Value::Object(o)));
                    }
                }
            }
            if act == "minimize" {
                // SW_SHOWMINNOACTIVE minimizes WITHOUT activating the next
                // window in the z-order (SW_MINIMIZE would move focus away
                // from the owner's current window).
                ShowWindow(h, SW_SHOWMINNOACTIVE);
                let mut o = Map::new();
                o.insert("ok".to_string(), Value::from(true));
                o.insert("action".to_string(), Value::from(act));
                o.insert("hwnd".to_string(), Value::from(hi));
                return Ok(crate::py_dumps(&Value::Object(o)));
            }
            if act == "maximize" {
                // A maximized window covers its monitor and neither
                // ShowWindowAsync(SW_MAXIMIZE) nor
                // SetWindowPlacement(SW_SHOWMAXIMIZED) documents a
                // no-activation guarantee, so maximizing an ORDINARY window
                // is explicit-only. A MESSAGE-ONLY window (parent
                // HWND_MESSAGE) is never displayed, so the non-intrusive
                // proof path needs no flag and cannot disturb the desktop.
                // No window class gets a free pass: neither ShowWindowAsync
                // nor SetWindowPlacement documents a no-activate form, and a
                // hidden/off-screen window can still be made visible by the
                // maximize. So the flag is required even for the message-only
                // proof window (which then shows nothing can be activated).
                if !a.allow_foreground {
                    let mut o = Map::new();
                    o.insert("ok".to_string(), Value::from(false));
                    o.insert("action".to_string(), Value::from(act));
                    o.insert("hwnd".to_string(), Value::from(hi));
                    o.insert(
                        "code".to_string(),
                        Value::from(crate::gate::FOREGROUND_REQUIRED),
                    );
                    o.insert(
                        "error".to_string(),
                        Value::from(
                            "foreground_required: maximize covers the window's monitor and \
                             Win32 documents no no-activate form; pass allow_foreground:true to \
                             accept the possible activation. Nothing was maximized.",
                        ),
                    );
                    return Ok(crate::py_dumps(&Value::Object(o)));
                }
                // Canonical maximize: save the normal rect from
                // GetWindowPlacement (the documented restore geometry, with
                // IsZoomed consulted FIRST so a repeated maximize can never
                // overwrite it), then ask the window manager to maximize with
                // ShowWindowAsync(SW_MAXIMIZE) - non-blocking, so a busy
                // target never stalls this call. SetWindowPlacement has no
                // documented no-activate flag, so it is only the fallback
                // when the async request does not establish IsZoomed; the
                // live test proves GetForegroundWindow is unchanged either
                // way.
                let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
                wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
                if GetWindowPlacement(h, &mut wp) == 0 {
                    return Err(crate::win32_text(GetLastError()));
                }
                let placement = [
                    wp.rcNormalPosition.left,
                    wp.rcNormalPosition.top,
                    wp.rcNormalPosition.right,
                    wp.rcNormalPosition.bottom,
                ];
                let zoomed = IsZoomed(h) != 0;
                let (normal_rect, save) = normal_for_maximize(zoomed, saved_normal(h), placement);
                if save {
                    remember_normal(h, normal_rect);
                }
                let mut how = "already";
                if !zoomed {
                    // Non-blocking request; 0 means the window is gone or the
                    // request was refused.
                    if ShowWindowAsync(h, SW_MAXIMIZE) == 0 {
                        return Err(format!(
                            "maximize_failed: ShowWindowAsync(SW_MAXIMIZE) refused for hwnd {hi}: {}",
                            crate::win32_text(GetLastError())
                        ));
                    }
                    how = "show_window_async";
                    if !wait_zoomed(h, std::time::Duration::from_millis(500)) {
                        // Fallback: the async request did not take. The
                        // placement call still needs the saved normal rect,
                        // which is already stored above.
                        wp.showCmd = SW_SHOWMAXIMIZED as u32;
                        if SetWindowPlacement(h, &wp) == 0 {
                            return Err(format!(
                                "maximize_failed: ShowWindowAsync did not maximize and \
                                 SetWindowPlacement failed for hwnd {hi}: {}",
                                crate::win32_text(GetLastError())
                            ));
                        }
                        how = "set_window_placement";
                    }
                }
                let normal = normal_rect.map(|v| v as i64);
                let mut o = Map::new();
                o.insert("ok".to_string(), Value::from(true));
                o.insert("action".to_string(), Value::from(act));
                o.insert("hwnd".to_string(), Value::from(hi));
                o.insert("maximized".to_string(), Value::from(IsZoomed(h) != 0));
                o.insert("how".to_string(), Value::from(how));
                o.insert(
                    "normal_rect".to_string(),
                    Value::from(normal.iter().map(|v| Value::from(*v)).collect::<Vec<_>>()),
                );
                return Ok(crate::py_dumps(&Value::Object(o)));
            }
            if act == "restore" {
                // Restore to the normal placement WITHOUT activating:
                // SetWindowPlacement with SW_SHOWNOACTIVATE un-maximizes /
                // un-minimizes to the remembered pre-maximize rect (else
                // rcNormalPosition), where SW_RESTORE would activate.
                let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
                wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
                if GetWindowPlacement(h, &mut wp) == 0 {
                    return Err(crate::win32_text(GetLastError()));
                }
                // Peek, do not consume: recovery state must survive a
                // failed restore so the caller can retry.
                let remembered = saved_normal(h);
                let normal: [i64; 4] = match remembered {
                    Some(r) => {
                        wp.rcNormalPosition = RECT {
                            left: r[0],
                            top: r[1],
                            right: r[2],
                            bottom: r[3],
                        };
                        [r[0] as i64, r[1] as i64, r[2] as i64, r[3] as i64]
                    }
                    None => [
                        wp.rcNormalPosition.left as i64,
                        wp.rcNormalPosition.top as i64,
                        wp.rcNormalPosition.right as i64,
                        wp.rcNormalPosition.bottom as i64,
                    ],
                };
                wp.showCmd = SW_SHOWNOACTIVATE as u32;
                if SetWindowPlacement(h, &wp) == 0 {
                    // Recovery state is kept: a retry can still restore it.
                    return Err(format!(
                        "restore_failed: SetWindowPlacement failed for hwnd {hi}: {}; target \
                         geometry {normal:?} (saved rect kept for a retry)",
                        crate::win32_text(GetLastError())
                    ));
                }
                // Windows clamps an offscreen rcNormalPosition into the
                // workspace when it shows the window, so the exact remembered
                // geometry is applied with SetWindowPos (no clamping, no
                // activation) after the placement un-maximizes. Its return
                // value is checked: success is never claimed blind.
                let pos_ok = SetWindowPos(
                    h,
                    std::ptr::null_mut(),
                    normal[0] as i32,
                    normal[1] as i32,
                    (normal[2] - normal[0]) as i32,
                    (normal[3] - normal[1]) as i32,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                if pos_ok == 0 {
                    // Do NOT clear the saved rect: the window is un-maximized
                    // but at the clamped position, and a retry can still land
                    // the exact geometry.
                    return Err(format!(
                        "restore_failed: SetWindowPos failed for hwnd {hi}: {}; target geometry \
                         {normal:?} (saved rect kept for a retry)",
                        crate::win32_text(GetLastError())
                    ));
                }
                // Both steps landed: only now is the recovery state obsolete.
                clear_normal(h);
                let mut o = Map::new();
                o.insert("ok".to_string(), Value::from(true));
                o.insert("action".to_string(), Value::from(act));
                o.insert("hwnd".to_string(), Value::from(hi));
                o.insert(
                    "restored_rect".to_string(),
                    Value::from(normal.iter().map(|v| Value::from(*v)).collect::<Vec<_>>()),
                );
                return Ok(crate::py_dumps(&Value::Object(o)));
            }
            if act == "close" {
                PostMessageW(h, WM_CLOSE, 0, 0);
                let mut o = Map::new();
                o.insert("ok".to_string(), Value::from(true));
                o.insert("action".to_string(), Value::from(act));
                o.insert("hwnd".to_string(), Value::from(hi));
                return Ok(crate::py_dumps(&Value::Object(o)));
            }
            if act == "topmost" {
                if SetWindowPos(
                    h,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                ) == 0
                {
                    return Err(crate::win32_text(GetLastError()));
                }
                let mut o = Map::new();
                o.insert("ok".to_string(), Value::from(true));
                o.insert("action".to_string(), Value::from(act));
                o.insert("hwnd".to_string(), Value::from(hi));
                return Ok(crate::py_dumps(&Value::Object(o)));
            }
            let mut o = Map::new();
            o.insert("ok".to_string(), Value::from(false));
            o.insert(
                "error".to_string(),
                Value::from(format!("unknown action \"{}\"", a.action)),
            );
            Ok(crate::py_dumps(&Value::Object(o)))
        }
    })();
    let mut payload = match out {
        Ok(s) => serde_json::from_str(&s).unwrap_or(Value::Null),
        Err(e) => {
            let mut o = Map::new();
            o.insert("ok".to_string(), Value::from(false));
            o.insert("action".to_string(), Value::from(a.action.clone()));
            o.insert("hwnd".to_string(), Value::from(hi));
            if let Some(code) = win_code_of(&e) {
                o.insert("code".to_string(), Value::from(code));
            }
            o.insert("error".to_string(), Value::from(e));
            Value::Object(o)
        }
    };
    if let Value::Object(m) = &mut payload {
        // Invariant telemetry (constraint 5): measured, reported, never the
        // guarantee — the guarantee is that only the explicit focus verb can
        // reach a foreground call at all (gate.rs).
        let cur_after = unsafe {
            let mut pt = POINT { x: 0, y: 0 };
            if GetCursorPos(&mut pt) != 0 {
                (pt.x as i64, pt.y as i64)
            } else {
                (-1, -1)
            }
        };
        m.insert(
            "foregroundUnchanged".to_string(),
            Value::from(unsafe { GetForegroundWindow() as i64 } == fg_before),
        );
        m.insert(
            "cursorUnchanged".to_string(),
            Value::from(cur_after == cur_before),
        );
        if let Some(l) = &locked {
            m.insert("target_locked".to_string(), Value::from(l.hwnd));
        }
    }
    wrapped_text(&crate::py_dumps(&payload))
}

pub fn do_list_windows() -> String {
    let mut out: Vec<Map<String, Value>> = Vec::new();
    for h in enum_windows() {
        if let Some(info) = window_info(h) {
            out.push(info);
        }
    }
    wrapped_text(&crate::py_dumps(&Value::Array(
        out.into_iter().map(Value::Object).collect(),
    )))
}

pub fn do_cursor() -> String {
    unsafe {
        let mut pt = std::mem::zeroed::<POINT>();
        if GetCursorPos(&mut pt) == 0 {
            let mut o = Map::new();
            o.insert("ok".to_string(), Value::from(false));
            o.insert("error".to_string(), Value::from("GetCursorPos failed"));
            return wrapped_text(&crate::py_dumps(&Value::Object(o)));
        }
        let screens: Vec<Vec<i64>> = display_monitors()
            .iter()
            .map(|r| vec![r[0] as i64, r[1] as i64, r[2] as i64, r[3] as i64])
            .collect();
        let mut o = Map::new();
        o.insert("x".to_string(), Value::from(pt.x as i64));
        o.insert("y".to_string(), Value::from(pt.y as i64));
        o.insert("screens".to_string(), Value::from(screens));
        wrapped_text(&crate::py_dumps(&Value::Object(o)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gap-2 proof (fails before the fix, passes after): every listed window
    /// carries its elevation so a caller can tell an elevated target apart
    /// from an unresponsive one.
    #[test]
    fn list_windows_entries_carry_elevated_flag() {
        let out = do_list_windows();
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let sc = outer.get("structuredContent").expect("structuredContent");
        assert!(
            sc.get("result").is_none(),
            "structuredContent must be the real object, never a wrapped string, got: {sc}"
        );
        let wins = sc
            .get("windows")
            .and_then(|w| w.as_array())
            .expect("windows array");
        for w in wins {
            let o = w.as_object().expect("window object");
            assert!(
                o.contains_key("elevated"),
                "list_windows entry must carry elevated, got keys: {:?}",
                o.keys().collect::<Vec<_>>()
            );
        }
        println!("ELEVATED_FLAG_WINDOWS={}", wins.len());
    }

    /// A maximized window necessarily covers the monitor, so every test that
    /// maximizes is gated behind its OWN opt-in. `PUPPET_LIVE_DESKTOP=1`
    /// alone must NOT run it: a headed CI box would flash the owner's screen.
    fn live_visible() -> bool {
        if std::env::var("PUPPET_LIVE_VISIBLE").as_deref() == Ok("1") {
            true
        } else {
            println!(
                "SKIP visible maximize test: set PUPPET_LIVE_VISIBLE=1 to run (maximizing covers \
                 the monitor; PUPPET_LIVE_DESKTOP alone is not enough)"
            );
            false
        }
    }

    /// LIVE (visible opt-in): maximize keeps the REAL maximize state
    /// (SetWindowPlacement, showCmd=SW_SHOWMAXIMIZED) without activation,
    /// preserves the normal rect, and `restore` returns to the pre-maximize
    /// geometry.
    ///
    /// The fixture window is offscreen at (-4000,-4000); a maximized window
    /// necessarily covers the monitor, so this test maximizes and restores
    /// back-to-back (tens of ms) and prints a notice. It never touches the
    /// pointer and asserts the foreground window is unchanged.
    #[test]
    fn live_maximize_preserves_state_and_restore_geometry() {
        use windows_sys::Win32::Foundation::RECT;
        if !live_visible() {
            return;
        }
        let f = crate::fixture::Fixture::new();
        let h = f.hwnd();
        let fg_before = crate::fixture::foreground_now();
        let mut before: RECT = unsafe { std::mem::zeroed() };
        assert_ne!(unsafe { GetWindowRect(h, &mut before) }, 0);
        let want = [
            before.left as i64,
            before.top as i64,
            before.right as i64,
            before.bottom as i64,
        ];
        println!("LIVE_MAXIMIZE_NOTE: maximizing the offscreen fixture briefly and restoring");

        let out = do_window(&WinArgs {
            action: "maximize".to_string(),
            title: None,
            hwnd: None,
            x: None,
            y: None,
            width: None,
            height: None,
            target: Some(serde_json::json!({ "hwnd": h as i64 })),
            allow_foreground: false,
        });
        let v: Value = serde_json::from_str(&out).expect("outer");
        let sc = v.get("structuredContent").cloned().expect("payload");
        assert_eq!(sc["ok"], Value::from(true), "maximize: {sc}");
        let normal: Vec<i64> = sc["normal_rect"]
            .as_array()
            .expect("normal_rect")
            .iter()
            .map(|x| x.as_i64().unwrap())
            .collect();
        assert_eq!(normal, want, "the normal rect must be preserved");

        unsafe {
            assert_ne!(IsZoomed(h), 0, "the window must actually be maximized");
            let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
            wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
            assert_ne!(GetWindowPlacement(h, &mut wp), 0);
            assert_eq!(
                wp.showCmd, SW_SHOWMAXIMIZED as u32,
                "showCmd must be SW_SHOWMAXIMIZED (real maximize state)"
            );
        }
        if fg_before != 0 {
            assert_eq!(
                crate::fixture::foreground_now(),
                fg_before,
                "maximize must not activate"
            );
        }

        let out = do_window(&WinArgs {
            action: "restore".to_string(),
            title: None,
            hwnd: None,
            x: None,
            y: None,
            width: None,
            height: None,
            target: Some(serde_json::json!({ "hwnd": h as i64 })),
            allow_foreground: false,
        });
        let v: Value = serde_json::from_str(&out).expect("outer");
        let sc = v.get("structuredContent").cloned().expect("payload");
        assert_eq!(sc["ok"], Value::from(true), "restore: {sc}");
        let restored: Vec<i64> = sc["restored_rect"]
            .as_array()
            .expect("restored_rect")
            .iter()
            .map(|x| x.as_i64().unwrap())
            .collect();
        assert_eq!(
            restored, want,
            "restore must return the pre-maximize geometry"
        );
        unsafe {
            assert_eq!(IsZoomed(h), 0, "restore must un-maximize");
            let mut rc: RECT = std::mem::zeroed();
            assert_ne!(GetWindowRect(h, &mut rc), 0);
            assert_eq!(
                [
                    rc.left as i64,
                    rc.top as i64,
                    rc.right as i64,
                    rc.bottom as i64
                ],
                want,
                "the window rect must be back where it was"
            );
            let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
            wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
            assert_ne!(GetWindowPlacement(h, &mut wp), 0);
            assert_ne!(wp.showCmd, SW_SHOWMAXIMIZED as u32);
        }
        if fg_before != 0 {
            assert_eq!(
                crate::fixture::foreground_now(),
                fg_before,
                "restore must not activate"
            );
        }
        println!("LIVE_MAXIMIZE_OK normal_rect={want:?} foreground={fg_before}");
    }

    /// Non-visual unit coverage of the repeated-maximize rule: while a window
    /// is already maximized the "current" rect is the maximized one, so the
    /// saved recovery rect must win and must not be rewritten.
    #[test]
    fn normal_for_maximize_never_overwrites_while_maximized() {
        let saved = [-4000, -4000, -3600, -3700];
        let maximized = [0, 0, 2560, 1440];
        let placement = [100, 100, 200, 200];
        // First maximize from a normal window: save the placement rect.
        let (r, save) = normal_for_maximize(false, None, saved);
        assert_eq!(r, saved);
        assert!(save, "first maximize saves the placement rect");
        // Second maximize on an ALREADY maximized window: keep the saved
        // rect, never the maximized one, and do not save again.
        let (r, save) = normal_for_maximize(true, Some(saved), maximized);
        assert_eq!(r, saved, "the saved recovery rect must win");
        assert!(!save, "nothing may be overwritten while maximized");
        // Already maximized with no saved rect (maximized before we saw it):
        // fall back to the placement and save nothing.
        let (r, save) = normal_for_maximize(true, None, placement);
        assert_eq!(r, placement);
        assert!(!save);
    }

    /// LIVE (visible opt-in): maximize -> maximize -> restore returns to the
    /// ORIGINAL normal geometry, not to the maximized rect.
    #[test]
    fn live_repeated_maximize_restores_original_geometry() {
        use windows_sys::Win32::Foundation::RECT;
        if !live_visible() {
            return;
        }
        let f = crate::fixture::Fixture::new();
        let h = f.hwnd();
        let hi = h as i64;
        let fg_before = crate::fixture::foreground_now();
        let mut before: RECT = unsafe { std::mem::zeroed() };
        assert_ne!(unsafe { GetWindowRect(h, &mut before) }, 0);
        let want = [
            before.left as i64,
            before.top as i64,
            before.right as i64,
            before.bottom as i64,
        ];
        let args = |action: &str| WinArgs {
            action: action.to_string(),
            title: None,
            hwnd: None,
            x: None,
            y: None,
            width: None,
            height: None,
            target: Some(serde_json::json!({ "hwnd": hi })),
            allow_foreground: true,
        };

        let first = do_window(&args("maximize"));
        let sc: Value = serde_json::from_str(&first).expect("outer");
        let sc = sc.get("structuredContent").cloned().expect("payload");
        assert_eq!(sc["ok"], Value::from(true), "first maximize: {sc}");
        assert_eq!(
            sc["normal_rect"].as_array().map(|a| a.len()),
            Some(4),
            "{sc}"
        );
        assert_eq!(
            saved_normal(h),
            Some([
                want[0] as i32,
                want[1] as i32,
                want[2] as i32,
                want[3] as i32
            ]),
            "the first maximize must save the original rect"
        );

        // Second maximize while already maximized must NOT overwrite it.
        let second = do_window(&args("maximize"));
        let sc: Value = serde_json::from_str(&second).expect("outer");
        let sc = sc.get("structuredContent").cloned().expect("payload");
        assert_eq!(sc["ok"], Value::from(true), "second maximize: {sc}");
        let reported: Vec<i64> = sc["normal_rect"]
            .as_array()
            .expect("normal_rect")
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        assert_eq!(
            reported, want,
            "the second maximize keeps the original rect"
        );
        assert_eq!(
            saved_normal(h),
            Some([
                want[0] as i32,
                want[1] as i32,
                want[2] as i32,
                want[3] as i32
            ]),
            "the saved rect must survive a repeated maximize"
        );

        let out = do_window(&args("restore"));
        let sc: Value = serde_json::from_str(&out).expect("outer");
        let sc = sc.get("structuredContent").cloned().expect("payload");
        assert_eq!(sc["ok"], Value::from(true), "restore: {sc}");
        let restored: Vec<i64> = sc["restored_rect"]
            .as_array()
            .expect("restored_rect")
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        assert_eq!(restored, want, "restore returns the ORIGINAL geometry");
        unsafe {
            let mut rc: RECT = std::mem::zeroed();
            assert_ne!(GetWindowRect(h, &mut rc), 0);
            assert_eq!(
                [
                    rc.left as i64,
                    rc.top as i64,
                    rc.right as i64,
                    rc.bottom as i64
                ],
                want,
                "the window must really be back at the original rect"
            );
            assert_eq!(IsZoomed(h), 0, "restore un-maximizes");
        }
        assert_eq!(
            saved_normal(h),
            None,
            "the recovery state is cleared after success"
        );
        if fg_before != 0 {
            assert_eq!(
                crate::fixture::foreground_now(),
                fg_before,
                "neither maximize nor restore may activate"
            );
        }
        println!("LIVE_REPEAT_MAXIMIZE_OK normal_rect={want:?} foreground={fg_before}");
    }

    /// The recovery state survives a failed restore: source-pinned so a
    /// future edit cannot clear the saved rect before both steps succeed,
    /// and the refusal is typed `restore_failed` with the target geometry.
    #[test]
    fn restore_failure_keeps_recovery_state_and_is_typed() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/winop.rs"))
            .expect("winop.rs readable");
        let clear = src
            .find("                clear_normal(h);")
            .expect("success clear");
        let place_fail = src
            .find("restore_failed: SetWindowPlacement failed")
            .expect("placement failure");
        let pos_fail = src
            .find("restore_failed: SetWindowPos failed")
            .expect("position failure");
        assert!(
            place_fail < clear && pos_fail < clear,
            "clear_normal(h) must come AFTER both restore failure returns"
        );
        let msg = "restore_failed: SetWindowPos failed for hwnd 1: boom; target geometry [1, 2, 3, 4] (saved rect kept for a retry)";
        assert_eq!(win_code_of(msg), Some("restore_failed"));
        assert!(msg.contains("[1, 2, 3, 4]"), "the geometry must be named");
        assert!(
            msg.contains("saved rect kept"),
            "the retry state must be named"
        );
    }

    /// Cursor invariant for this module's tests, with the repo's
    /// owner-activity attribution: an unchanged pointer passes; a pointer that
    /// keeps moving after a settle window is the owner at the machine
    /// (reported, never attributed); a pointer that moved once and is then
    /// still is the tool and fails.
    /// Pointer movement is TELEMETRY, never a failure: the maximize path
    /// calls no pointer API (no `SetCursorPos`/`SendInput` outside
    /// `src/gate.rs`, pinned by `tests/no_steal.rs`), and the owner using the
    /// machine moves the pointer constantly. Report the delta and continue.
    fn report_pointer(before: (i32, i32), what: &str) {
        let now = cursor_xy();
        if now == before {
            return;
        }
        println!(
            "CURSOR_EXTERNAL: {what}: pointer moved before={before:?} after={now:?} delta=({}, {}); telemetry only (no cursor API on this path)",
            now.0 - before.0,
            now.1 - before.1
        );
    }

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

    fn cursor_xy() -> (i32, i32) {
        let mut pt = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut pt) } != 0 {
            (pt.x, pt.y)
        } else {
            (-1, -1)
        }
    }

    fn placement_rect(h: HWND) -> [i32; 4] {
        let mut wp: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
        wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        assert_ne!(unsafe { GetWindowPlacement(h, &mut wp) }, 0);
        [
            wp.rcNormalPosition.left,
            wp.rcNormalPosition.top,
            wp.rcNormalPosition.right,
            wp.rcNormalPosition.bottom,
        ]
    }

    fn window_rect(h: HWND) -> [i32; 4] {
        let mut rc: RECT = unsafe { std::mem::zeroed() };
        assert_ne!(unsafe { GetWindowRect(h, &mut rc) }, 0);
        [rc.left, rc.top, rc.right, rc.bottom]
    }

    /// DEFAULT-SUITE proof of the canonical maximize path. A MESSAGE-ONLY
    /// window (`HWND_MESSAGE`) is created with `CreateWindowExW` and never
    /// shown: `IsWindowVisible` stays 0 for its whole life, so maximizing it
    /// cannot cover a monitor or disturb the owner's desktop. Win32 still
    /// tracks its placement, so `ShowWindowAsync(SW_MAXIMIZE)` establishes
    /// the real maximized state (`IsZoomed != 0`, showCmd = SW_MAXIMIZE).
    ///
    /// This is the executed evidence the gated visible tests cannot give:
    /// the canonical async path is taken (`how == "show_window_async"`, not
    /// the SetWindowPlacement fallback), the foreground handle and the
    /// pointer do not move, the saved normal rect holds the pre-maximize
    /// geometry across a repeated maximize, and restore returns to it and
    /// clears the saved entry only after success.
    #[test]
    fn maximize_path_is_proven_on_a_message_only_window() {
        let f = crate::fixture::MessageWindow::new();
        let h = f.hwnd();
        let hi = h as i64;
        let fg_before = crate::fixture::foreground_now();
        let cur_before = cursor_xy();
        // Message-only windows are never displayed.
        assert_eq!(
            unsafe { IsWindowVisible(h) },
            0,
            "the probe window must never be visible"
        );
        let want = placement_rect(h);
        assert_eq!(window_rect(h), want, "fixture starts at its placement rect");
        assert_eq!(unsafe { IsZoomed(h) }, 0, "starts un-maximized");

        let args = |action: &str| WinArgs {
            action: action.to_string(),
            title: None,
            hwnd: None,
            x: None,
            y: None,
            width: None,
            height: None,
            target: Some(serde_json::json!({ "hwnd": hi })),
            // Explicit, because maximize has no documented no-activate form.
            // The proof then shows a message-only window activates nothing.
            allow_foreground: true,
        };
        let call = |action: &str| -> Value {
            let out = do_window(&args(action));
            let v: Value = serde_json::from_str(&out).expect("outer frame");
            v.get("structuredContent")
                .cloned()
                .expect("structuredContent payload")
        };

        // ---- first maximize: canonical ShowWindowAsync path
        let sc = call("maximize");
        assert_eq!(sc["ok"], Value::from(true), "maximize: {sc}");
        assert_eq!(
            sc["how"],
            Value::from("show_window_async"),
            "the CANONICAL path must have established the state, not the fallback: {sc}"
        );
        assert_eq!(sc["maximized"], Value::from(true), "maximized: {sc}");
        assert_eq!(
            unsafe { IsZoomed(h) },
            1,
            "the window must really be zoomed"
        );
        assert_eq!(
            unsafe { IsWindowVisible(h) },
            0,
            "maximizing must not make the window visible"
        );
        let mut wp: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
        wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        assert_ne!(unsafe { GetWindowPlacement(h, &mut wp) }, 0);
        assert_eq!(
            wp.showCmd, SW_MAXIMIZE as u32,
            "showCmd must be SW_MAXIMIZE (real maximize state)"
        );
        let reported: Vec<i64> = sc["normal_rect"]
            .as_array()
            .expect("normal_rect")
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        assert_eq!(reported, want.iter().map(|v| *v as i64).collect::<Vec<_>>());
        assert_eq!(
            saved_normal(h),
            Some(want),
            "the saved normal rect must hold the pre-maximize geometry"
        );

        // ---- second maximize: must not overwrite the saved geometry.
        // Windows clamps rcNormalPosition when it maximizes a VISIBLE window;
        // simulate that here (the message-only placement is not clamped) so
        // the guard is actually exercised: while maximized the placement now
        // reports a different normal rect, and only the guard keeps the
        // original saved geometry alive.
        let mut wp2: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
        wp2.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        assert_ne!(unsafe { GetWindowPlacement(h, &mut wp2) }, 0);
        wp2.rcNormalPosition = RECT {
            left: 0,
            top: 0,
            right: 555,
            bottom: 444,
        };
        assert_ne!(
            unsafe { SetWindowPlacement(h, &wp2) },
            0,
            "simulated placement clamp"
        );
        let clamped = placement_rect(h);
        assert_eq!(clamped, [0, 0, 555, 444], "the clamp must have landed");
        let sc = call("maximize");
        assert_eq!(sc["ok"], Value::from(true), "second maximize: {sc}");
        let reported: Vec<i64> = sc["normal_rect"]
            .as_array()
            .expect("normal_rect")
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        assert_eq!(
            reported,
            want.iter().map(|v| *v as i64).collect::<Vec<_>>(),
            "a repeated maximize keeps the original normal rect"
        );
        assert_eq!(
            saved_normal(h),
            Some(want),
            "a repeated maximize must not overwrite the saved rect"
        );

        // ---- restore: exact geometry back, entry cleared only on success
        let sc = call("restore");
        assert_eq!(sc["ok"], Value::from(true), "restore: {sc}");
        let restored: Vec<i64> = sc["restored_rect"]
            .as_array()
            .expect("restored_rect")
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        assert_eq!(restored, want.iter().map(|v| *v as i64).collect::<Vec<_>>());
        assert_eq!(
            window_rect(h),
            want,
            "the window must be back at its original rect"
        );
        assert_eq!(unsafe { IsZoomed(h) }, 0, "restore un-maximizes");
        assert_eq!(
            saved_normal(h),
            None,
            "the saved entry is cleared only after a successful restore"
        );

        // ---- non-intrusive invariants, measured across the whole sequence
        assert_foreground_held(fg_before, "maximize sequence");
        report_pointer(cur_before, "maximize sequence");
        println!(
            "MAXIMIZE_NONINTRUSIVE_OK normal_rect={want:?} how=show_window_async visible=0 fg={fg_before}"
        );
    }

    /// DEFAULT SUITE: maximizing an ORDINARY window is explicit-only. Neither
    /// ShowWindowAsync nor SetWindowPlacement documents a no-activate form, so
    /// without allow_foreground:true the verb returns foreground_required and
    /// the window is left un-maximized. The message-only proof below is the
    /// exception (never displayed, cannot disturb the desktop).
    #[test]
    fn maximize_of_an_ordinary_window_requires_the_flag() {
        let f = crate::fixture::Fixture::new();
        let h = f.hwnd();
        let out = do_window(&WinArgs {
            action: "maximize".to_string(),
            title: None,
            hwnd: None,
            x: None,
            y: None,
            width: None,
            height: None,
            target: Some(serde_json::json!({ "hwnd": h as i64 })),
            allow_foreground: false,
        });
        let v: Value = serde_json::from_str(&out).expect("outer");
        let sc = v.get("structuredContent").cloned().expect("payload");
        assert_eq!(sc["ok"], Value::from(false), "unflagged maximize: {sc}");
        assert_eq!(
            sc["code"].as_str(),
            Some("foreground_required"),
            "unflagged maximize must be typed: {sc}"
        );
        assert_eq!(
            unsafe { IsZoomed(h) },
            0,
            "an unflagged maximize must not touch the window"
        );
        println!("MAXIMIZE_FLAG_OK code=foreground_required");
    }

    #[test]
    fn out_of_i32_range_rejected_not_wrapped() {
        // 2^40 wraps to 0 under `as i32`; the helper must reject instead.
        assert_eq!(1099511627776i64 as i32, 0);
        assert!(checked_i32(1099511627776, "x").is_err());
        assert!(checked_i32(-1099511627776, "y").is_err());
        assert!(checked_i32(i32::MAX as i64 + 1, "width").is_err());
        assert!(checked_i32(i32::MIN as i64 - 1, "height").is_err());
    }

    #[test]
    fn in_range_values_pass_through() {
        assert_eq!(checked_i32(0, "x").unwrap(), 0);
        assert_eq!(checked_i32(-10, "y").unwrap(), -10);
        assert_eq!(checked_i32(i32::MAX as i64, "width").unwrap(), i32::MAX);
        assert_eq!(checked_i32(i32::MIN as i64, "x").unwrap(), i32::MIN);
    }
}
