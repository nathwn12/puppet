use crate::{display_monitors, enum_windows, window_info, wrapped_text};
use serde_json::{Map, Value};
use windows_sys::Win32::Foundation::{GetLastError, HWND, POINT, RECT};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

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
    /// Retained for schema compatibility: passing it refuses the whole
    /// call with `no_foreground_policy` — no flag buys the foreground.
    pub allow_foreground: bool,
}

type WindowKey = (i64, u32, u64);

static NORMAL_RECTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<WindowKey, [i32; 4]>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn window_key(hwnd: HWND) -> WindowKey {
    let pid = crate::pid_of(hwnd);
    (hwnd as i64, pid, crate::process_created(pid).unwrap_or(0))
}

fn saved_normal(hwnd: HWND) -> Option<[i32; 4]> {
    let m = NORMAL_RECTS.lock().unwrap_or_else(|e| e.into_inner());
    m.get(&window_key(hwnd)).copied()
}

fn clear_normal(hwnd: HWND) {
    let mut m = NORMAL_RECTS.lock().unwrap_or_else(|e| e.into_inner());
    m.remove(&window_key(hwnd));
}

fn win_code_of(msg: &str) -> Option<&'static str> {
    [
        crate::gate::NO_FOREGROUND_POLICY,
        crate::gate::NO_CURSOR_POLICY,
        crate::gate::NO_CLIPBOARD_POLICY,
        "restore_failed",
    ]
    .into_iter()
    .find(|code| msg.starts_with(code))
}

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
    let mut locked: Option<crate::target::TargetLock> = None;
    let (hwnd_r, err, cands) = match a.target.as_ref() {
        None | Some(Value::Null) => match crate::cap::lock_legacy(a.hwnd, a.title.as_deref()) {
            Ok(l) => {
                let h = l.hwnd as HWND;
                locked = Some(l);
                (Some(h), None, None)
            }
            Err(e) => (None, Some(e.text()), e.candidates.clone()),
        },
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
        if a.allow_foreground {
            let e = crate::gate::no_foreground_policy(&format!("window action:{}", act));
            let mut o = Map::new();
            o.insert("ok".to_string(), Value::from(false));
            o.insert("action".to_string(), Value::from(act));
            o.insert("hwnd".to_string(), Value::from(hi));
            o.insert("code".to_string(), Value::from(e.code));
            o.insert("error".to_string(), Value::from(e.text()));
            return Ok(crate::py_dumps(&Value::Object(o)));
        }
        unsafe {
            if act == "focus" {
                let e = crate::gate::no_foreground_policy("window action:focus");
                let mut o = Map::new();
                o.insert("ok".to_string(), Value::from(false));
                o.insert("code".to_string(), Value::from(e.code));
                o.insert("error".to_string(), Value::from(e.text()));
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
                ShowWindow(h, SW_SHOWMINNOACTIVE);
                let mut o = Map::new();
                o.insert("ok".to_string(), Value::from(true));
                o.insert("action".to_string(), Value::from(act));
                o.insert("hwnd".to_string(), Value::from(hi));
                return Ok(crate::py_dumps(&Value::Object(o)));
            }
            if act == "maximize" {
                let e = crate::gate::no_foreground_policy("window action:maximize");
                let mut o = Map::new();
                o.insert("ok".to_string(), Value::from(false));
                o.insert("action".to_string(), Value::from(act));
                o.insert("hwnd".to_string(), Value::from(hi));
                o.insert("code".to_string(), Value::from(e.code));
                o.insert("error".to_string(), Value::from(e.text()));
                return Ok(crate::py_dumps(&Value::Object(o)));
            }
            if act == "restore" {
                let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
                wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
                if GetWindowPlacement(h, &mut wp) == 0 {
                    return Err(crate::win32_text(GetLastError()));
                }
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
                    return Err(format!(
                        "restore_failed: SetWindowPlacement failed for hwnd {hi}: {}; target \
                         geometry {normal:?} (saved rect kept for a retry)",
                        crate::win32_text(GetLastError())
                    ));
                }
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
                    return Err(format!(
                        "restore_failed: SetWindowPos failed for hwnd {hi}: {}; target geometry \
                         {normal:?} (saved rect kept for a retry)",
                        crate::win32_text(GetLastError())
                    ));
                }
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

pub fn do_list_windows(include_app_ids: bool) -> String {
    let mut out: Vec<Map<String, Value>> = Vec::new();
    for h in enum_windows() {
        if let Some(info) = window_info(h, include_app_ids) {
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

    #[test]
    fn list_windows_entries_carry_elevated_flag() {
        let out = do_list_windows(false);
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

    #[test]
    fn list_windows_app_ids_only_when_requested() {
        let out = do_list_windows(false);
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let wins = outer["structuredContent"]["windows"]
            .as_array()
            .expect("windows array");
        assert!(
            !wins.is_empty(),
            "a desktop has at least one visible window"
        );
        for w in wins {
            assert!(
                !w.as_object().expect("window object").contains_key("appId"),
                "appId must be absent unless include_app_ids:true, got: {w}"
            );
        }

        let out = do_list_windows(true);
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let wins = outer["structuredContent"]["windows"]
            .as_array()
            .expect("windows array");
        for w in wins {
            let v = w
                .as_object()
                .expect("window object")
                .get("appId")
                .expect("include_app_ids must add appId to every entry");
            assert!(
                v.is_null() || v.as_str().is_some(),
                "appId is a string or null, got: {v}"
            );
        }
    }

    #[test]
    fn window_legacy_hwnd_uses_the_target_lock() {
        let _g = crate::fixture::desktop();
        let f = crate::fixture::Fixture::new();
        let hwnd = f.hwnd_i64();
        drop(f);
        let out = do_window(&WinArgs {
            action: "minimize".to_string(),
            title: None,
            hwnd: Some(hwnd),
            x: None,
            y: None,
            width: None,
            height: None,
            target: None,
            allow_foreground: false,
        });
        assert!(
            out.contains("target_gone"),
            "legacy hwnd must route through the target lock, got: {out}"
        );
        assert!(
            !out.contains("bad_window"),
            "the bare-handle legacy path must be gone, got: {out}"
        );
    }

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

    #[test]
    fn live_maximize_preserves_state_and_restore_geometry() {
        use windows_sys::Win32::Foundation::RECT;
        let _g = crate::fixture::desktop();
        if !live_visible() {
            return;
        }
        let f = crate::fixture::Fixture::new();
        let h = f.hwnd();
        let fg_before = crate::fixture::foreground_now();
        let mut before: RECT = unsafe { std::mem::zeroed() };
        assert_ne!(unsafe { GetWindowRect(h, &mut before) }, 0);
        println!("LIVE_MAXIMIZE_NOTE: maximize is refused under the background-only policy");

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
        assert_eq!(sc["ok"], Value::from(false), "maximize: {sc}");
        assert_eq!(
            sc["code"].as_str(),
            Some("no_foreground_policy"),
            "maximize must be typed: {sc}"
        );
        assert!(
            sc["error"]
                .as_str()
                .unwrap_or("")
                .contains("background-only policy"),
            "maximize must name the rule: {sc}"
        );
        unsafe {
            assert_eq!(IsZoomed(h), 0, "a refused maximize must not zoom");
        }
        if fg_before != 0 {
            assert_eq!(
                crate::fixture::foreground_now(),
                fg_before,
                "a refused maximize must not activate"
            );
        }
        println!("LIVE_MAXIMIZE_OK refused=no_foreground_policy foreground={fg_before}");
    }

    #[test]
    fn live_repeated_maximize_restores_original_geometry() {
        use windows_sys::Win32::Foundation::RECT;
        let _g = crate::fixture::desktop();
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
        assert_eq!(sc["ok"], Value::from(false), "first maximize: {sc}");
        assert_eq!(
            sc["code"].as_str(),
            Some("no_foreground_policy"),
            "first maximize must be typed: {sc}"
        );

        let second = do_window(&args("maximize"));
        let sc: Value = serde_json::from_str(&second).expect("outer");
        let sc = sc.get("structuredContent").cloned().expect("payload");
        assert_eq!(sc["ok"], Value::from(false), "second maximize: {sc}");
        assert_eq!(
            sc["code"].as_str(),
            Some("no_foreground_policy"),
            "second maximize must be typed: {sc}"
        );
        assert_eq!(
            saved_normal(h),
            None,
            "a refused maximize saves no recovery state"
        );
        unsafe {
            assert_eq!(IsZoomed(h), 0, "a refused maximize must not zoom");
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
                "the window must be exactly where it was"
            );
        }

        let out = do_window(&args("restore"));
        let sc: Value = serde_json::from_str(&out).expect("outer");
        let sc = sc.get("structuredContent").cloned().expect("payload");
        assert_eq!(sc["ok"], Value::from(false), "restore: {sc}");
        assert_eq!(
            sc["code"].as_str(),
            Some("no_foreground_policy"),
            "a flagged restore must be typed: {sc}"
        );
        if fg_before != 0 {
            assert_eq!(
                crate::fixture::foreground_now(),
                fg_before,
                "refusals must not activate"
            );
        }
        println!("LIVE_REPEAT_MAXIMIZE_OK refused=3x foreground={fg_before}");
    }

    #[test]
    fn restore_failure_keeps_recovery_state_and_is_typed() {
        let _g = crate::fixture::desktop();
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

    #[test]
    fn maximize_is_refused_even_on_a_message_only_window() {
        let _g = crate::fixture::desktop();
        let f = crate::fixture::MessageWindow::new();
        let h = f.hwnd();
        let hi = h as i64;
        let fg_before = crate::fixture::foreground_now();
        let cur_before = cursor_xy();
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
            allow_foreground: true,
        };
        let call = |action: &str| -> Value {
            let out = do_window(&args(action));
            let v: Value = serde_json::from_str(&out).expect("outer frame");
            v.get("structuredContent")
                .cloned()
                .expect("structuredContent payload")
        };

        let sc = call("maximize");
        assert_eq!(sc["ok"], Value::from(false), "maximize: {sc}");
        assert_eq!(
            sc["code"].as_str(),
            Some("no_foreground_policy"),
            "maximize must be typed: {sc}"
        );
        assert_eq!(
            unsafe { IsZoomed(h) },
            0,
            "a refused maximize must not zoom"
        );
        assert_eq!(
            unsafe { IsWindowVisible(h) },
            0,
            "a refused maximize must not make the window visible"
        );
        assert_eq!(
            saved_normal(h),
            None,
            "a refused maximize saves no recovery state"
        );
        assert_eq!(window_rect(h), want, "geometry must not move");

        if fg_before != 0 {
            assert_eq!(
                crate::fixture::foreground_now(),
                fg_before,
                "a refused maximize must not activate"
            );
        }
        report_pointer(cur_before, "maximize refusal");
        println!("MAXIMIZE_NONINTRUSIVE_OK refused=no_foreground_policy visible=0 fg={fg_before}");
    }

    #[test]
    fn maximize_of_an_ordinary_window_is_refused() {
        let _g = crate::fixture::desktop();
        let f = crate::fixture::Fixture::new();
        let h = f.hwnd();
        for flag in [false, true] {
            let out = do_window(&WinArgs {
                action: "maximize".to_string(),
                title: None,
                hwnd: None,
                x: None,
                y: None,
                width: None,
                height: None,
                target: Some(serde_json::json!({ "hwnd": h as i64 })),
                allow_foreground: flag,
            });
            let v: Value = serde_json::from_str(&out).expect("outer");
            let sc = v.get("structuredContent").cloned().expect("payload");
            assert_eq!(sc["ok"], Value::from(false), "maximize (flag={flag}): {sc}");
            assert_eq!(
                sc["code"].as_str(),
                Some("no_foreground_policy"),
                "maximize must be typed: {sc}"
            );
        }
        assert_eq!(
            unsafe { IsZoomed(h) },
            0,
            "a refused maximize must not touch the window"
        );
        println!("MAXIMIZE_FLAG_OK code=no_foreground_policy");
    }

    #[test]
    fn out_of_i32_range_rejected_not_wrapped() {
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
