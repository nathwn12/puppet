//! screenshot tool handlers (window + desktop paths).

use crate::cap::{bitblt_grab, finalize, resolve_selector, resolve_window, Img};
use crate::dxgi::dxgi_grab;
use crate::shot::{capture_window, elements_block};
use crate::{
    display_monitors, image_block, ok_blocks, py_round_n, text_block, virtual_rect, FORMULA,
};
use base64::Engine as _;
use serde_json::{Map, Value};
use std::time::Instant;
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub struct ShotArgs {
    pub display: i64,
    pub region: Option<Vec<Value>>,
    pub window: Option<String>,
    pub max_px: Option<i64>,
    pub fmt: String,
    pub quality: Option<i64>,
    pub grid: bool,
    pub allow_crop_fallback: bool,
    pub elements: bool,
}

/// Screenshot error-as-data: fastmcp aggregates a bare list result into ONE
/// text block holding the compact-JSON list (pydantic dump_json).
pub fn shot_err_list(items: Vec<String>) -> String {
    ok_blocks(&[text_block(&serde_json::to_string(&items).unwrap())])
}

fn py_int_elem(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i)
            } else if let Some(u) = n.as_u64() {
                if u <= i64::MAX as u64 {
                    Some(u as i64)
                } else {
                    None
                }
            } else if let Some(f) = n.as_f64() {
                if f.is_finite() && f >= -9.0e18 && f <= 9.0e18 {
                    Some(f.trunc() as i64)
                } else {
                    None
                }
            } else {
                None
            }
        }
        Value::Bool(b) => Some(if *b { 1 } else { 0 }),
        Value::String(s) => {
            let t = s.trim().replace('_', "");
            let t = t.as_str();
            let body = t.strip_prefix('+').unwrap_or(t);
            let neg = t.starts_with('-');
            let digits = if neg { &body[1..] } else { body };
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let mut val: i64 = 0;
            for b in digits.bytes() {
                val = val.checked_mul(10)?.checked_add((b - b'0') as i64)?;
            }
            Some(if neg { -val } else { val })
        }
        _ => None,
    }
}

fn parse_region(region: &Option<Vec<Value>>) -> Result<Option<[i64; 4]>, String> {
    match region {
        None => Ok(None),
        Some(v) => {
            if v.len() != 4 {
                return Err("error: bad_region: region must be [left,top,right,bottom] in virtual-desktop px".to_string());
            }
            let mut r = [0i64; 4];
            for (i, x) in v.iter().enumerate() {
                match py_int_elem(x) {
                    Some(n) => r[i] = n,
                    None => return Err("error: bad_region: region must be [left,top,right,bottom] in virtual-desktop px".to_string()),
                }
            }
            if r[2] <= r[0] || r[3] <= r[1] {
                return Err(
                    "error: bad_region: region must have right>left and bottom>top".to_string(),
                );
            }
            Ok(Some(r))
        }
    }
}

fn cursor_xy(fallback: (i64, i64)) -> (i64, i64) {
    unsafe {
        let mut pt = std::mem::zeroed::<POINT>();
        if GetCursorPos(&mut pt) != 0 {
            (pt.x as i64, pt.y as i64)
        } else {
            fallback
        }
    }
}

fn monitors_block() -> (Vec<[i32; 4]>, i64, (i32, i32, i32, i32)) {
    let mons = display_monitors();
    let (vx, vy, vr, vb) = virtual_rect();
    let n = mons.len() as i64;
    (mons, n, (vx, vy, vr, vb))
}

fn cmaps_monitors(n: i64, v: (i32, i32, i32, i32)) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("count".to_string(), Value::from(n));
    m.insert(
        "origin".to_string(),
        Value::from(vec![v.0 as i64, v.1 as i64]),
    );
    m.insert(
        "virtual".to_string(),
        Value::from(vec![v.0 as i64, v.1 as i64, v.2 as i64, v.3 as i64]),
    );
    m
}

pub fn screenshot_window(a: &ShotArgs) -> String {
    let sel = a.window.as_ref().unwrap();
    let (hwnd, err, cands) = resolve_selector(sel);
    if hwnd.is_none() {
        let mut items = vec![format!("error: {}", err.unwrap())];
        if let Some(c) = cands {
            let mut obj = Map::new();
            obj.insert(
                "candidates".to_string(),
                Value::Array(c.into_iter().map(Value::Object).collect()),
            );
            items.push(crate::py_dumps(&Value::Object(obj)));
        }
        return shot_err_list(items);
    }
    let hwnd = hwnd.unwrap();
    let region = match parse_region(&a.region) {
        Ok(r) => r,
        Err(e) => return shot_err_list(vec![e]),
    };
    let cap = match capture_window(hwnd, a.allow_crop_fallback) {
        Ok(c) => c,
        Err(e) => return shot_err_list(vec![e]),
    };
    let (dl, dt) = (cap.dwm[0] as i64, cap.dwm[1] as i64);
    let (sw, sh) = (cap.img.w as i64, cap.img.h as i64);
    let (kind, cx0, cy0, cx1, cy1) = match region {
        Some(r) => (
            "region",
            dl.max(r[0]),
            dt.max(r[1]),
            (dl + sw).min(r[2]),
            (dt + sh).min(r[3]),
        ),
        None => ("window", dl, dt, dl + sw, dt + sh),
    };
    if cx1 <= cx0 || cy1 <= cy0 {
        return shot_err_list(vec![
            "error: empty_crop: crop rect is empty/outside the capture".to_string(),
        ]);
    }
    let (bx0, by0, bx1, by1) = (
        (cx0 - dl) as usize,
        (cy0 - dt) as usize,
        (cx1 - dl) as usize,
        (cy1 - dt) as usize,
    );
    let img = if (bx0, by0, bx1, by1) != (0, 0, sw as usize, sh as usize) {
        let cw = bx1 - bx0;
        let ch = by1 - by0;
        let mut px = vec![0u8; cw * ch * 3];
        for y in 0..ch {
            let s = ((by0 + y) * sw as usize + bx0) * 3;
            let d = y * cw * 3;
            px[d..d + cw * 3].copy_from_slice(&cap.img.px[s..s + cw * 3]);
        }
        Img {
            w: (cx1 - cx0) as i32,
            h: (cy1 - cy0) as i32,
            px,
        }
    } else {
        cap.img
    };
    let (src_w, src_h) = (cx1 - cx0, cy1 - cy0);
    let (data, iw, ih, scale, f, q) = match finalize(img, a.max_px, &a.fmt, a.quality, a.grid) {
        Ok(v) => v,
        Err(e) => return shot_err_list(vec![e]),
    };
    let (curx, cury) = cursor_xy((dl, dt));
    let (_, n_mon, vrt) = monitors_block();
    let mut cmap = Map::new();
    let mut im = Map::new();
    im.insert("w".to_string(), Value::from(iw));
    im.insert("h".to_string(), Value::from(ih));
    im.insert("format".to_string(), Value::from(f.clone()));
    im.insert(
        "quality".to_string(),
        if f == "jpeg" {
            Value::from(q)
        } else {
            Value::Null
        },
    );
    im.insert("grid".to_string(), Value::from(a.grid));
    cmap.insert("image".to_string(), Value::from(im));
    let mut src = Map::new();
    src.insert("x".to_string(), Value::from(cx0));
    src.insert("y".to_string(), Value::from(cy0));
    src.insert("w".to_string(), Value::from(src_w));
    src.insert("h".to_string(), Value::from(src_h));
    src.insert("kind".to_string(), Value::from(kind));
    cmap.insert("source".to_string(), Value::from(src));
    let mut win = Map::new();
    win.insert("hwnd".to_string(), Value::from(cap.hwnd));
    win.insert("pid".to_string(), Value::from(cap.pid as i64));
    win.insert("exe".to_string(), Value::from(cap.exe));
    win.insert("title".to_string(), Value::from(cap.title));
    win.insert(
        "dwm_rect".to_string(),
        Value::from(
            cap.dwm
                .iter()
                .map(|v| Value::from(*v as i64))
                .collect::<Vec<_>>(),
        ),
    );
    win.insert("cloaked".to_string(), Value::from(cap.cloaked));
    win.insert("minimized".to_string(), Value::from(false));
    win.insert(
        "foreground".to_string(),
        Value::from(unsafe { GetForegroundWindow() as i64 == cap.hwnd }),
    );
    cmap.insert("window".to_string(), Value::from(win));
    cmap.insert("scale".to_string(), Value::from(scale));
    cmap.insert(
        "monitors".to_string(),
        Value::from(cmaps_monitors(n_mon, vrt)),
    );
    let mut cur = Map::new();
    cur.insert("x".to_string(), Value::from(curx));
    cur.insert("y".to_string(), Value::from(cury));
    cmap.insert("cursor".to_string(), Value::from(cur));
    let mut capt = Map::new();
    capt.insert("backend".to_string(), Value::from(cap.backend));
    capt.insert("ms".to_string(), Value::from(py_round_n(cap.ms, 3)));
    capt.insert("rect_source".to_string(), Value::from(cap.rect_source));
    capt.insert(
        "rect_delta_px".to_string(),
        Value::from(
            cap.rect_delta
                .iter()
                .map(|v| Value::from(*v as i64))
                .collect::<Vec<_>>(),
        ),
    );
    capt.insert("detector".to_string(), Value::from(cap.detector));
    capt.insert("stale".to_string(), Value::from(cap.stale));
    capt.insert(
        "note".to_string(),
        match cap.note {
            Some(n) => Value::from(n),
            None => Value::Null,
        },
    );
    if let Some(pf) = cap.print_flags {
        capt.insert("print_flags".to_string(), Value::from(pf));
    }
    cmap.insert("capture".to_string(), Value::from(capt));
    cmap.insert("formula".to_string(), Value::from(FORMULA));
    let mime = if f == "png" {
        "image/png"
    } else {
        "image/jpeg"
    };
    let mut blocks = vec![
        image_block(
            &base64::engine::general_purpose::STANDARD.encode(&data),
            mime,
        ),
        text_block(&crate::py_dumps(&Value::Object(cmap))),
    ];
    if a.elements {
        blocks.push(text_block(&elements_block(hwnd)));
    }
    ok_blocks(&blocks)
}

pub fn screenshot_desktop(a: &ShotArgs) -> String {
    let (vx, vy, vr, vb) = virtual_rect();
    let mons = display_monitors();
    let n_mon = mons.len() as i64;
    let (gl, gt, gw, gh, kind, grab_scope): (i64, i64, i64, i64, String, String) =
        if a.display == -1 {
            (
                vx as i64,
                vy as i64,
                (vr - vx) as i64,
                (vb - vy) as i64,
                "virtual".to_string(),
                "virtual".to_string(),
            )
        } else {
            let d = a.display;
            let idx = 1 + d;
            if d < 0 || idx >= mons.len() as i64 {
                return shot_err_list(vec![format!(
                    "error: bad_display: display {d} out of range (0..{})",
                    n_mon - 1
                )]);
            }
            let m = mons[idx as usize];
            (
                m[0] as i64,
                m[1] as i64,
                (m[2] - m[0]) as i64,
                (m[3] - m[1]) as i64,
                "monitor".to_string(),
                "monitor".to_string(),
            )
        };
    let region = match parse_region(&a.region) {
        Ok(r) => r,
        Err(e) => return shot_err_list(vec![e]),
    };
    let (cx0, cy0, cx1, cy1, kind): (i64, i64, i64, i64, String) = match region {
        Some(r) => (
            gl.max(r[0]),
            gt.max(r[1]),
            (gl + gw).min(r[2]),
            (gt + gh).min(r[3]),
            "region".to_string(),
        ),
        None => (gl, gt, gl + gw, gt + gh, kind),
    };
    if cx1 <= cx0 || cy1 <= cy0 {
        return shot_err_list(vec![
            "error: empty_crop: crop rect is empty/outside the capture".to_string(),
        ]);
    }
    let note: Option<String> = if n_mon > 1 && grab_scope == "virtual" {
        if kind == "virtual" {
            Some(format!(
                "virtual span across {n_mon} monitors is per-output for DXGI; used mss directly"
            ))
        } else {
            Some(format!("crop on a {n_mon}-monitor desktop; mss grabs the rect directly (DXGI is per-output full-frame)"))
        }
    } else {
        None
    };
    let t0 = Instant::now();
    // Backend per scope (puppet.py _grab_rect parity): DXGI serves whole
    // outputs, so a virtual-desktop span across >1 monitor goes straight to
    // GDI; otherwise try the DXGI fast path and fall back to GDI per call.
    let force_gdi = n_mon > 1 && grab_scope == "virtual";
    let (mut img, backend): (Img, String) = if force_gdi {
        match bitblt_grab(gl as i32, gt as i32, gw as i32, gh as i32) {
            Ok(im) => (im, "gdi".to_string()),
            Err(e) => return shot_err_list(vec![format!("error: capture_failed: {e}")]),
        }
    } else {
        let output = if grab_scope == "monitor" {
            a.display as u32
        } else {
            0
        };
        match dxgi_grab(output, gw as i32, gh as i32) {
            Ok(im) => (im, "dxgi".to_string()),
            Err(fb) => {
                eprintln!("puppet: dxgi unavailable ({fb}), using gdi");
                match bitblt_grab(gl as i32, gt as i32, gw as i32, gh as i32) {
                    Ok(im) => (im, "gdi".to_string()),
                    Err(e) => return shot_err_list(vec![format!("error: capture_failed: {e}")]),
                }
            }
        }
    };
    let cap_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let (curx, cury) = cursor_xy((gl, gt));
    if (img.w as i64, img.h as i64) != (gw, gh) {
        return shot_err_list(vec![format!(
            "error: capture_failed: backend {backend} returned {}x{} for a {gw}x{gh} request",
            img.w, img.h
        )]);
    }
    let (bx0, by0, bx1, by1) = (
        (cx0 - gl).max(0) as usize,
        (cy0 - gt).max(0) as usize,
        ((cx1 - gl).min(img.w as i64)) as usize,
        ((cy1 - gt).min(img.h as i64)) as usize,
    );
    if bx1 <= bx0 || by1 <= by0 {
        return shot_err_list(vec![
            "error: empty_crop: crop rect is empty/outside the capture".to_string(),
        ]);
    }
    if (bx0, by0, bx1, by1) != (0, 0, img.w as usize, img.h as usize) {
        let cw = bx1 - bx0;
        let ch = by1 - by0;
        let mut px = vec![0u8; cw * ch * 3];
        for y in 0..ch {
            let s = ((by0 + y) * img.w as usize + bx0) * 3;
            let d = y * cw * 3;
            px[d..d + cw * 3].copy_from_slice(&img.px[s..s + cw * 3]);
        }
        img = Img {
            w: cw as i32,
            h: ch as i32,
            px,
        };
    }
    let (sw, sh) = (img.w, img.h);
    let (data, iw, ih, scale, f, q) = match finalize(img, a.max_px, &a.fmt, a.quality, a.grid) {
        Ok(v) => v,
        Err(e) => return shot_err_list(vec![e]),
    };
    if (scale - ih as f64 / sh as f64).abs() * (sw.max(sh) as f64) > 1.0 {
        return shot_err_list(vec![
            "error: capture_failed: non-uniform scale after resize".to_string(),
        ]);
    }
    let mut cmap = Map::new();
    let mut im = Map::new();
    im.insert("w".to_string(), Value::from(iw));
    im.insert("h".to_string(), Value::from(ih));
    im.insert("format".to_string(), Value::from(f.clone()));
    im.insert(
        "quality".to_string(),
        if f == "jpeg" {
            Value::from(q)
        } else {
            Value::Null
        },
    );
    im.insert("grid".to_string(), Value::from(a.grid));
    cmap.insert("image".to_string(), Value::from(im));
    let mut src = Map::new();
    src.insert("x".to_string(), Value::from(cx0));
    src.insert("y".to_string(), Value::from(cy0));
    src.insert("w".to_string(), Value::from(sw as i64));
    src.insert("h".to_string(), Value::from(sh as i64));
    src.insert("kind".to_string(), Value::from(kind));
    cmap.insert("source".to_string(), Value::from(src));
    cmap.insert("scale".to_string(), Value::from(scale));
    cmap.insert(
        "monitors".to_string(),
        Value::from(cmaps_monitors(n_mon, (vx, vy, vr, vb))),
    );
    let mut cur = Map::new();
    cur.insert("x".to_string(), Value::from(curx));
    cur.insert("y".to_string(), Value::from(cury));
    cmap.insert("cursor".to_string(), Value::from(cur));
    let mut capt = Map::new();
    capt.insert("backend".to_string(), Value::from(backend));
    capt.insert("ms".to_string(), Value::from(py_round_n(cap_ms, 3)));
    if let Some(n) = note {
        capt.insert("note".to_string(), Value::from(n));
    }
    cmap.insert("capture".to_string(), Value::from(capt));
    cmap.insert("formula".to_string(), Value::from(FORMULA));
    let mime = if f == "png" {
        "image/png"
    } else {
        "image/jpeg"
    };
    let mut blocks = vec![
        image_block(
            &base64::engine::general_purpose::STANDARD.encode(&data),
            mime,
        ),
        text_block(&crate::py_dumps(&Value::Object(cmap))),
    ];
    if a.elements {
        let fg = unsafe { GetForegroundWindow() };
        if fg.is_null() {
            return shot_err_list(vec![
                "error: bad_window: elements needs a window (no foreground window)".to_string(),
            ]);
        }
        blocks.push(text_block(&elements_block(fg)));
    }
    ok_blocks(&blocks)
}

pub fn bad_display_type(n_mon: i64) -> String {
    shot_err_list(vec![format!(
        "error: bad_display: display must be -1 or 0..{}",
        n_mon - 1
    )])
}

pub fn dispatch_screenshot(a: &ShotArgs) -> String {
    if a.window.is_some() {
        screenshot_window(a)
    } else {
        screenshot_desktop(a)
    }
}

pub fn resolve_window_pub(
    title: Option<&str>,
    hwnd: Option<i64>,
) -> (
    Option<windows_sys::Win32::Foundation::HWND>,
    Option<String>,
    Option<Vec<Map<String, Value>>>,
) {
    resolve_window(title, hwnd)
}
