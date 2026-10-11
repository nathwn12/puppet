//! window ops + list_windows + cursor tools.

use crate::cap::resolve_window;
use crate::{display_monitors, enum_windows, window_info, wrapped_text};
use serde_json::{Map, Value};
use windows_sys::Win32::Foundation::{GetLastError, HWND, POINT};
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub(crate) fn focus_hwmd(hwnd: HWND) -> Map<String, Value> {
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        } else {
            ShowWindow(hwnd, SW_SHOW);
        }
        let fg = GetForegroundWindow();
        let cur_tid = GetCurrentThreadId();
        let mut fg_tid = 0u32;
        let mut tgt_tid = 0u32;
        GetWindowThreadProcessId(fg, &mut fg_tid);
        GetWindowThreadProcessId(hwnd, &mut tgt_tid);
        let mut attached: Vec<u32> = Vec::new();
        if fg_tid != 0 && fg_tid != cur_tid {
            if AttachThreadInput(cur_tid, fg_tid, 1) != 0 {
                attached.push(fg_tid);
            }
        }
        if tgt_tid != 0 && tgt_tid != cur_tid {
            if AttachThreadInput(cur_tid, tgt_tid, 1) != 0 {
                attached.push(tgt_tid);
            }
        }
        BringWindowToTop(hwnd);
        SetForegroundWindow(hwnd);
        for tid in attached {
            AttachThreadInput(cur_tid, tid, 0);
        }
        let fg_now = GetForegroundWindow();
        let ok = fg_now == hwnd;
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
}

pub fn do_window(a: &WinArgs) -> String {
    let (hwnd_r, err, cands) = resolve_window(a.title.as_deref(), a.hwnd);
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
                let mut m = focus_hwmd(h);
                let mut o = Map::new();
                o.insert("action".to_string(), Value::from("focus"));
                o.append(&mut m);
                return Ok(crate::py_dumps(&Value::Object(o)));
            }
            if act == "move" {
                match (a.x, a.y) {
                    (Some(xi), Some(yi)) => {
                        if SetWindowPos(
                            h,
                            std::ptr::null_mut(),
                            xi as i32,
                            yi as i32,
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
                        if SetWindowPos(
                            h,
                            std::ptr::null_mut(),
                            0,
                            0,
                            wi as i32,
                            hi2 as i32,
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
                ShowWindow(h, SW_MINIMIZE);
                let mut o = Map::new();
                o.insert("ok".to_string(), Value::from(true));
                o.insert("action".to_string(), Value::from(act));
                o.insert("hwnd".to_string(), Value::from(hi));
                return Ok(crate::py_dumps(&Value::Object(o)));
            }
            if act == "maximize" {
                ShowWindow(h, SW_SHOWMAXIMIZED);
                let mut o = Map::new();
                o.insert("ok".to_string(), Value::from(true));
                o.insert("action".to_string(), Value::from(act));
                o.insert("hwnd".to_string(), Value::from(hi));
                return Ok(crate::py_dumps(&Value::Object(o)));
            }
            if act == "restore" {
                ShowWindow(h, SW_RESTORE);
                let mut o = Map::new();
                o.insert("ok".to_string(), Value::from(true));
                o.insert("action".to_string(), Value::from(act));
                o.insert("hwnd".to_string(), Value::from(hi));
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
                if SetWindowPos(h, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE) == 0 {
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
    match out {
        Ok(s) => wrapped_text(&s),
        Err(e) => {
            let mut o = Map::new();
            o.insert("ok".to_string(), Value::from(false));
            o.insert("action".to_string(), Value::from(a.action.clone()));
            o.insert("hwnd".to_string(), Value::from(hi));
            o.insert("error".to_string(), Value::from(e));
            wrapped_text(&crate::py_dumps(&Value::Object(o)))
        }
    }
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
