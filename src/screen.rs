//! screenshot tool handlers (window + desktop paths).

use crate::cap::{bitblt_grab, finalize, Img};
use crate::dxgi::{dxgi_grab, dxgi_outputs, pick_output};
use crate::shot::{capture_window, elements_block};
use crate::{
    display_monitor_handles, display_monitors, image_block, monitor_device, py_round_n,
    screenshot_ok, text_block, tool_err_blocks, virtual_rect, FORMULA,
};
use base64::Engine as _;
use serde_json::{Map, Value};
use std::time::Instant;
use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Grab-size bound: no single BitBlt/DXGI capture may exceed this edge.
/// Regions and small monitors stay well under it; a virtual-desktop span
/// across big monitors is refused instead of allocating gigabytes.
const MAX_GRAB_EDGE: i64 = 8192;

/// Virtual-desktop rectangle as (left, top, right, bottom) device pixels.
type VirtualRect = (i32, i32, i32, i32);

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
    /// Optional absolute path: the exact encoded bytes are also written to
    /// this path (parent dirs created) and the payload gains
    /// `out:{path,bytes,fmt}`. PNG when `out` is set and `fmt` is omitted
    /// (a diff needs lossless pixels). The inline image still returns.
    pub out: Option<String>,
    /// Structured target lock (object or selector string); wins over
    /// `window` when present. Captured without activation or restore.
    pub target: Option<Value>,
}

/// Revalidate a resolved target immediately before acting on its handle.
/// `Ok(())` when there is no lock (legacy `window=` selector) or when the
/// identity still matches; the typed `target_gone` text otherwise.
pub(crate) fn revalidate_locked(locked: &Option<crate::target::TargetLock>) -> Result<(), String> {
    match locked {
        Some(l) => l.revalidate().map_err(|e| e.text()),
        None => Ok(()),
    }
}

/// Resolved window, or a typed error, or disambiguation candidates.
type WindowResolve = (
    Option<windows_sys::Win32::Foundation::HWND>,
    Option<String>,
    Option<Vec<Map<String, Value>>>,
);

/// Screenshot domain error: the aggregated text block is unchanged for
/// legacy clients; P2a reports it as a real tool failure (`isError: true`).
pub fn shot_err_list(items: Vec<String>) -> String {
    tool_err_blocks(&[text_block(&serde_json::to_string(&items).unwrap())])
}

/// AMENDMENT-01 A2: a request the server altered announces itself, in one
/// channel reserved for that — `quality` outside 1..100 (`quality_clamped`,
/// with the applied value) and `quality` sent with a format that cannot use
/// it (`quality_ignored`, applied null). A call that requested nothing the
/// server changed returns an empty vec, and the caller emits no key at all:
/// an always-present empty array would train an agent to ignore the channel.
fn warnings_of(quality: Option<i64>, applied: i64, fmt: &str) -> Vec<Value> {
    let Some(req) = quality else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if !(1..=100).contains(&req) {
        let mut w = Map::new();
        w.insert("code".to_string(), Value::from("quality_clamped"));
        w.insert("field".to_string(), Value::from("quality"));
        w.insert("requested".to_string(), Value::from(req));
        w.insert("applied".to_string(), Value::from(applied));
        out.push(Value::Object(w));
    }
    if fmt == "png" {
        let mut w = Map::new();
        w.insert("code".to_string(), Value::from("quality_ignored"));
        w.insert("field".to_string(), Value::from("quality"));
        w.insert("requested".to_string(), Value::from(req));
        w.insert("applied".to_string(), Value::Null);
        out.push(Value::Object(w));
    }
    out
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
                if f.is_finite() && (-9223372036854775808.0..9223372036854775808.0).contains(&f) {
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
            let mut mag: u64 = 0;
            for b in digits.bytes() {
                mag = mag.checked_mul(10)?.checked_add((b - b'0') as u64)?;
            }
            if neg {
                if mag == 9223372036854775808u64 {
                    return Some(i64::MIN);
                }
                if mag <= i64::MAX as u64 {
                    return Some(-(mag as i64));
                }
                None
            } else if mag <= i64::MAX as u64 {
                Some(mag as i64)
            } else {
                None
            }
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
                // F4: overflow rejects with the one shared message so the
                // same out-of-range input errors identically in every tool;
                // other garbage keeps the legacy bad_region shape.
                if crate::is_int_overflow(x) {
                    return Err(crate::int_overflow_err(x));
                }
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

fn monitors_block() -> (Vec<[i32; 4]>, i64, VirtualRect) {
    let mons = display_monitors();
    let (vx, vy, vr, vb) = virtual_rect();
    let n = mons.len() as i64;
    (mons, n, (vx, vy, vr, vb))
}

fn cmaps_monitors(n: i64, v: VirtualRect, rects: &[[i32; 4]]) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("count".to_string(), Value::from(n));
    // Per-monitor rects, in the same [l,t,r,b] shape `cursor.screens` uses,
    // so a caller can map a monitor index without a second round-trip.
    m.insert(
        "rects".to_string(),
        Value::from(
            rects
                .iter()
                .map(|r| Value::from(vec![r[0] as i64, r[1] as i64, r[2] as i64, r[3] as i64]))
                .collect::<Vec<_>>(),
        ),
    );
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
    use windows_sys::Win32::Foundation::HWND;
    let fg_before = unsafe { GetForegroundWindow() as i64 };
    let mut cur0 = POINT { x: 0, y: 0 };
    let cur_before = unsafe {
        if GetCursorPos(&mut cur0) != 0 {
            (cur0.x as i64, cur0.y as i64)
        } else {
            (-1, -1)
        }
    };
    // Target lock first: one window, resolved once, and KEPT as a lock so the
    // identity can be revalidated immediately before the capture (the handle
    // alone would let a recycled hwnd be captured as somebody else's window).
    // `window:` is parsed with the SAME grammar as `target:` (a plain string
    // is a TargetSpec), so the two arguments differ only in spelling.
    let spec_v: Option<Value> = match (a.target.as_ref(), a.window.as_ref()) {
        (Some(v), _) => Some(v.clone()),
        (None, Some(sel)) => Some(Value::String(sel.clone())),
        (None, None) => None,
    };
    let mut locked: Option<crate::target::TargetLock> = None;
    let (hwnd, err, cands): WindowResolve = match spec_v {
        Some(v) => match crate::target::TargetSpec::parse(&v) {
            Err(e) => (None, Some(e.text()), e.candidates.clone()),
            Ok(spec) => match crate::target::lock(&spec) {
                Err(e) => (None, Some(e.text()), e.candidates.clone()),
                Ok(l) => {
                    let h = l.hwnd as HWND;
                    locked = Some(l);
                    (Some(h), None, None)
                }
            },
        },
        None => (
            None,
            Some("error: bad_window: need window or target".to_string()),
            None,
        ),
    };
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
    // Revalidate the lock immediately before the capture: the window may
    // have closed or been recycled between resolution and here.
    if let Err(e) = revalidate_locked(&locked) {
        return shot_err_list(vec![format!("error: {}", e)]);
    }
    let cap = match unsafe { capture_window(hwnd, a.allow_crop_fallback) } {
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
    // `out`: the exact returned bytes also land on disk (parents created);
    // a write failure refuses the call instead of returning pixels without
    // the file half. The inline image still returns below.
    let out_obj: Option<Map<String, Value>> = match a.out.as_deref() {
        None => None,
        Some(path) => match crate::cap::write_bytes_out(path, &data) {
            Ok(()) => Some(crate::cap::out_object(path, data.len(), &f)),
            Err(e) => return shot_err_list(vec![e]),
        },
    };
    let (curx, cury) = cursor_xy((dl, dt));
    let (mons, n_mon, vrt) = monitors_block();
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
        Value::from(cmaps_monitors(n_mon, vrt, &mons)),
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
        "moved_during_capture".to_string(),
        Value::from(cap.moved_during_capture),
    );
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
    let warnings = warnings_of(a.quality, q, &f);
    if !warnings.is_empty() {
        cmap.insert("warnings".to_string(), Value::from(warnings));
    }
    if let Some(o) = out_obj {
        cmap.insert("out".to_string(), Value::from(o));
    }
    // Invariant telemetry: a window capture resolves + PrintWindows the
    // target and never activates or restores it.
    let mut cur1 = POINT { x: 0, y: 0 };
    let cur_after = unsafe {
        if GetCursorPos(&mut cur1) != 0 {
            (cur1.x as i64, cur1.y as i64)
        } else {
            (-1, -1)
        }
    };
    cmap.insert(
        "foregroundUnchanged".to_string(),
        Value::from(unsafe { GetForegroundWindow() as i64 } == fg_before),
    );
    cmap.insert(
        "cursorUnchanged".to_string(),
        Value::from(cur_after == cur_before),
    );
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
        text_block(&crate::py_dumps(&Value::Object(cmap.clone()))),
    ];
    if a.elements {
        // The element walk runs after the capture: revalidate again so the
        // listing cannot describe a different window than the image.
        if let Err(e) = revalidate_locked(&locked) {
            return shot_err_list(vec![format!("error: {}", e)]);
        }
        blocks.push(text_block(&elements_block(hwnd)));
    }
    screenshot_ok(&blocks, &Value::Object(cmap))
}

/// 0-based monitor choice over the `EnumDisplayMonitors` list. Shared by the
/// GDI rect leg and the DXGI output leg so both index the same monitor.
pub fn monitor_rect(mons: &[[i32; 4]], display: i64) -> Result<[i32; 4], String> {
    if display < 0 || display >= mons.len() as i64 {
        return Err(format!(
            "error: bad_display: display {display} out of range (0..{})",
            mons.len() as i64 - 1
        ));
    }
    Ok(mons[display as usize])
}

/// Resolve a `display` index to a real DXGI (adapter, output) pair. Never
/// assumes `display` == output: a same-size wrong monitor would pass
/// `check_size` and poison the per-pair session cache, so anything but an
/// identity match is Err (caller falls back to GDI).
fn dxgi_match_output(display: i64, mons: &[[i32; 4]]) -> Result<(u32, u32), String> {
    let rect = monitor_rect(mons, display)?;
    let handles = display_monitor_handles();
    let (hmon, device) = handles
        .get(display as usize)
        .map(|(_, h)| (*h as i64, monitor_device(*h)))
        .unwrap_or((0, String::new()));
    let outputs = dxgi_outputs()?;
    pick_output(rect, hmon, &device, &outputs)
}

// Test seam: the foreground values `foreground_window()` returns, in order.
// The last value sticks once the script runs dry; an empty script means the
// real `GetForegroundWindow()`. Thread-local so each `#[test]` scripts only
// its own call, and so a test can force the ambient foreground to move
// mid-call and prove the reply never mixes two samples.
#[cfg(test)]
thread_local! {
    static FG_SCRIPT: std::cell::RefCell<Vec<i64>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn next_scripted_fg() -> Option<i64> {
    FG_SCRIPT.with(|s| {
        let mut q = s.borrow_mut();
        match q.len() {
            0 => None,
            1 => Some(q[0]),
            _ => Some(q.remove(0)),
        }
    })
}

#[cfg(test)]
fn script_foreground(values: &[i64]) {
    FG_SCRIPT.with(|s| *s.borrow_mut() = values.to_vec());
}

/// The window this call treats as foreground, sampled ONCE and carried.
///
/// A desktop/region capture's element block is the foreground window's, not
/// the image's, so the payload has to name that window too. Taking the handle
/// here and threading it through both the `elements_window` metadata and the
/// element walk makes "the name is the window the block describes" true by
/// construction: there is no second read for a foreground switch to land
/// between (a CI runner with nothing held open switches freely).
fn foreground_window() -> HWND {
    #[cfg(test)]
    if let Some(h) = next_scripted_fg() {
        return h as HWND;
    }
    unsafe { GetForegroundWindow() }
}

pub fn screenshot_desktop(a: &ShotArgs) -> String {
    // One sample for the whole call: the elements path below names and walks
    // this exact handle, and `foregroundUnchanged` measures against it.
    let fg_at_entry = foreground_window();
    let fg_before = fg_at_entry as i64;
    let mut cur0 = POINT { x: 0, y: 0 };
    let cur_before = unsafe {
        if GetCursorPos(&mut cur0) != 0 {
            (cur0.x as i64, cur0.y as i64)
        } else {
            (-1, -1)
        }
    };
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
            let m = match monitor_rect(&mons, a.display) {
                Ok(m) => m,
                Err(e) => return shot_err_list(vec![e]),
            };
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
    if let Some(r) = &region {
        // Overflow-safe region gate: the shared validator never subtracts
        // in i64 before validating, so MIN/MAX extremes are typed errors
        // (no debug panic, no release wraparound bypass). Checked
        // pre-intersection so a huge off-desktop rect cannot slip through.
        match crate::validate_region(r[0], r[1], r[2], r[3], MAX_GRAB_EDGE) {
            Ok(_) => {}
            Err(crate::RegionReject::Order) => {
                return shot_err_list(vec![
                    "error: bad_region: region must have right>left and bottom>top".to_string(),
                ]);
            }
            Err(crate::RegionReject::Extreme) => {
                return shot_err_list(vec![
                    "error: bad_region: region coordinates outside i32 range (would truncate on cast)"
                        .to_string(),
                ]);
            }
            Err(crate::RegionReject::OverCap(rw, rh)) => {
                return shot_err_list(vec![format!(
                    "error: bad_region: region edges absurd (>8192, got {rw}x{rh})"
                )]);
            }
        }
    }
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
    let mut note: Option<String> = if n_mon > 1 && grab_scope == "virtual" {
        if kind == "virtual" {
            Some(format!(
                "virtual span across {n_mon} monitors is per-output for DXGI; grabbed directly with GDI"
            ))
        } else {
            Some(format!("crop on a {n_mon}-monitor desktop; GDI grabs the rect directly (DXGI is per-output full-frame)"))
        }
    } else {
        None
    };
    let t0 = Instant::now();
    // Region captures grab the crop rect directly with GDI: no full-frame
    // grab followed by an in-memory crop, so peak memory is the region
    // itself rather than the whole monitor or desktop.
    // Otherwise (puppet.py _grab_rect parity): DXGI serves whole outputs, so
    // a virtual-desktop span across >1 monitor goes straight to GDI;
    // otherwise try the DXGI fast path and fall back to GDI per call.
    let force_gdi = n_mon > 1 && grab_scope == "virtual";
    let (mut img, backend): (Img, String) = if kind == "region" {
        let (cw, ch) = (cx1 - cx0, cy1 - cy0);
        note = Some("region grabbed directly via GDI (no full-frame grab)".to_string());
        match bitblt_grab(cx0 as i32, cy0 as i32, cw as i32, ch as i32) {
            Ok(im) => (im, "gdi".to_string()),
            Err(e) => return shot_err_list(vec![format!("error: capture_failed: {e}")]),
        }
    } else {
        if gw > MAX_GRAB_EDGE || gh > MAX_GRAB_EDGE {
            return shot_err_list(vec![format!(
                "error: capture_failed: request edges absurd (>8192, got {gw}x{gh})"
            )]);
        }
        if force_gdi {
            match bitblt_grab(gl as i32, gt as i32, gw as i32, gh as i32) {
                Ok(im) => (im, "gdi".to_string()),
                Err(e) => return shot_err_list(vec![format!("error: capture_failed: {e}")]),
            }
        } else {
            // No default pair: every scope resolves the real DXGI (adapter,
            // output) pair through matching. Monitor scope uses its rect;
            // single-monitor virtual scope matches the whole span. An unmatched
            // monitor goes straight to GDI with a note — a same-size wrong
            // output would pass check_size and poison the cache.
            let matched: Result<(u32, u32), String> = if grab_scope == "monitor" {
                dxgi_match_output(a.display, &mons)
            } else {
                let handles = display_monitor_handles();
                let (hmon, device) = handles
                    .first()
                    .map(|(_, h)| (*h as i64, monitor_device(*h)))
                    .unwrap_or((0, String::new()));
                match dxgi_outputs() {
                    Ok(outs) => pick_output(
                        [gl as i32, gt as i32, (gl + gw) as i32, (gt + gh) as i32],
                        hmon,
                        &device,
                        &outs,
                    ),
                    Err(e) => Err(e),
                }
            };
            match matched {
                Err(e) => {
                    note = Some(match note {
                        Some(n) => format!("{n}; dxgi output unmatched ({e}); gdi"),
                        None => format!("dxgi output unmatched ({e}); gdi"),
                    });
                    match bitblt_grab(gl as i32, gt as i32, gw as i32, gh as i32) {
                        Ok(im) => (im, "gdi".to_string()),
                        Err(e) => {
                            return shot_err_list(vec![format!("error: capture_failed: {e}")])
                        }
                    }
                }
                Ok((ai, oi)) => match dxgi_grab(ai, oi, gw as i32, gh as i32) {
                    Ok(im) => (im, "dxgi".to_string()),
                    Err(fb) => {
                        eprintln!("puppet: dxgi unavailable ({fb}), using gdi");
                        match bitblt_grab(gl as i32, gt as i32, gw as i32, gh as i32) {
                            Ok(im) => (im, "gdi".to_string()),
                            Err(e) => {
                                return shot_err_list(vec![format!("error: capture_failed: {e}")])
                            }
                        }
                    }
                },
            }
        }
    };
    // Region grabs are already cropped: rebase the grab origin onto the
    // crop origin so the shared size check and crop below are no-ops.
    let (gl, gt, gw, gh) = if kind == "region" {
        (cx0, cy0, cx1 - cx0, cy1 - cy0)
    } else {
        (gl, gt, gw, gh)
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
    // `out`: the exact returned bytes also land on disk (parents created);
    // a write failure refuses the call instead of returning pixels without
    // the file half. The inline image still returns below.
    let out_obj: Option<Map<String, Value>> = match a.out.as_deref() {
        None => None,
        Some(path) => match crate::cap::write_bytes_out(path, &data) {
            Ok(()) => Some(crate::cap::out_object(path, data.len(), &f)),
            Err(e) => return shot_err_list(vec![e]),
        },
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
        Value::from(cmaps_monitors(n_mon, (vx, vy, vr, vb), &mons)),
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
    // Invariant telemetry: a desktop capture is pixels-only (BitBlt/DXGI),
    // no activation and no pointer call anywhere on this path.
    let mut cur1 = POINT { x: 0, y: 0 };
    let cur_after = unsafe {
        if GetCursorPos(&mut cur1) != 0 {
            (cur1.x as i64, cur1.y as i64)
        } else {
            (-1, -1)
        }
    };
    cmap.insert(
        "foregroundUnchanged".to_string(),
        Value::from(foreground_window() as i64 == fg_before),
    );
    cmap.insert(
        "cursorUnchanged".to_string(),
        Value::from(cur_after == cur_before),
    );
    cmap.insert("formula".to_string(), Value::from(FORMULA));
    let warnings = warnings_of(a.quality, q, &f);
    if !warnings.is_empty() {
        cmap.insert("warnings".to_string(), Value::from(warnings));
    }
    if let Some(o) = out_obj {
        cmap.insert("out".to_string(), Value::from(o));
    }
    // The element block describes `fg_at_entry`, not the image: with
    // display/region the image is the desktop but the elements are that
    // window's. Naming it from the same handle the walk receives is the whole
    // point - a second read here is how the reply came to name one window and
    // enumerate another on a CI runner whose foreground moved mid-call.
    let elements_hwnd = if a.elements {
        if fg_at_entry.is_null() {
            return shot_err_list(vec![
                "error: bad_window: elements needs a window (no foreground window)".to_string(),
            ]);
        }
        Some(fg_at_entry)
    } else {
        None
    };
    if let Some(fg) = elements_hwnd {
        let mut ew = Map::new();
        ew.insert("hwnd".to_string(), Value::from(fg as i64));
        ew.insert(
            "exe".to_string(),
            Value::from(crate::exe_of_pid(crate::pid_of(fg))),
        );
        ew.insert(
            "title".to_string(),
            Value::from(unsafe { crate::win_text(fg) }),
        );
        cmap.insert("elements_window".to_string(), Value::from(ew));
    }
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
        text_block(&crate::py_dumps(&Value::Object(cmap.clone()))),
    ];
    if let Some(fg) = elements_hwnd {
        blocks.push(text_block(&elements_block(fg)));
    }
    screenshot_ok(&blocks, &Value::Object(cmap))
}

pub fn dispatch_screenshot(a: &ShotArgs) -> String {
    if a.window.is_some() || a.target.is_some() {
        screenshot_window(a)
    } else {
        screenshot_desktop(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TOCTOU guard, unit level: a lock whose window died is refused by the
    /// exact helper the capture and elements paths call immediately before
    /// acting, with the typed target_gone text.
    #[test]
    fn revalidate_locked_refuses_a_dead_lock() {
        let _g = crate::fixture::desktop();
        let f = crate::fixture::Fixture::new();
        let hwnd = f.hwnd_i64();
        let spec = crate::target::TargetSpec::parse(&serde_json::json!({"hwnd": hwnd})).unwrap();
        let lock = crate::target::lock(&spec).expect("fresh fixture locks");
        assert!(revalidate_locked(&Some(lock.clone())).is_ok());
        drop(f);
        let e = revalidate_locked(&Some(lock)).expect_err("dead lock must be refused");
        assert!(
            e.starts_with("target_gone"),
            "typed refusal expected, got: {e}"
        );
        assert!(
            revalidate_locked(&None).is_ok(),
            "no lock = legacy path, no-op"
        );
    }

    /// The revalidate calls are ON the acting path: source-pinned so a future
    /// refactor cannot drop the lock and act on the bare handle again.
    #[test]
    fn revalidate_runs_before_capture_and_before_the_element_walk() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/screen.rs"))
            .expect("screen.rs readable");
        let call = "if let Err(e) = revalidate_locked(&locked)";
        let cap = src.find("capture_window(hwnd").expect("capture call");
        let rev_cap = src.find(call).expect("capture-path revalidate");
        assert!(
            rev_cap < cap,
            "revalidate_locked must precede capture_window on the path"
        );
        let walk = src.find("elements_block(hwnd)").expect("element walk");
        let rev_walk = src[..walk].rfind(call).expect("elements-path revalidate");
        assert!(
            rev_walk < walk,
            "revalidate_locked must precede elements_block on the path"
        );
    }

    /// `window:` is parsed as a TargetSpec, so a dead handle is the typed
    /// target_gone the `target:` path already produced — never the old bare
    /// IsWindow bad_window, and never a capture of whatever reused the handle.
    #[test]
    fn window_selector_uses_the_target_lock() {
        let _g = crate::fixture::desktop();
        let f = crate::fixture::Fixture::new();
        let hwnd = f.hwnd_i64();
        drop(f);
        let out = screenshot_window(&ShotArgs {
            display: -1,
            region: None,
            window: Some(format!("hwnd:{hwnd}")),
            max_px: Some(320),
            fmt: "png".to_string(),
            quality: Some(80),
            grid: false,
            allow_crop_fallback: false,
            elements: false,
            target: None,
            out: None,
        });
        assert!(
            out.contains("target_gone"),
            "window: must route through the target lock, got: {out}"
        );
        assert!(
            !out.contains("bad_window"),
            "the bare-handle legacy path must be gone, got: {out}"
        );
    }

    /// A target that is already gone is a typed screenshot error, not a bare
    /// hwnd failure.
    #[test]
    fn screenshot_target_gone_is_typed() {
        let _g = crate::fixture::desktop();
        let f = crate::fixture::Fixture::new();
        let hwnd = f.hwnd_i64();
        drop(f);
        let out = screenshot_window(&ShotArgs {
            display: -1,
            region: None,
            window: None,
            max_px: Some(320),
            fmt: "png".to_string(),
            quality: Some(80),
            grid: false,
            allow_crop_fallback: false,
            elements: false,
            target: Some(serde_json::json!({"hwnd": hwnd})),
            out: None,
        });
        assert!(
            out.contains("target_gone"),
            "expected target_gone, got: {out}"
        );
    }

    fn two_mons() -> Vec<[i32; 4]> {
        vec![[0, 0, 1920, 1080], [1920, 0, 3840, 1080]]
    }

    /// `monitors` carries per-monitor rects in the same [l,t,r,b] shape
    /// `cursor.screens` uses, so a caller can map a display index without a
    /// second round-trip.
    #[test]
    fn monitor_block_carries_per_monitor_rects() {
        let rects = two_mons();
        let o = cmaps_monitors(2, (0, 0, 3840, 1080), &rects);
        assert_eq!(o.get("count"), Some(&Value::from(2)));
        assert_eq!(
            o.get("origin"),
            Some(&serde_json::json!([0, 0])),
            "origin is the virtual-desktop top-left"
        );
        assert_eq!(
            o.get("virtual"),
            Some(&serde_json::json!([0, 0, 3840, 1080]))
        );
        let got: Vec<Vec<i64>> = o["rects"]
            .as_array()
            .expect("monitors.rects")
            .iter()
            .map(|r| {
                r.as_array()
                    .expect("rect array")
                    .iter()
                    .map(|v| v.as_i64().expect("int edge"))
                    .collect()
            })
            .collect();
        let want: Vec<Vec<i64>> = rects
            .iter()
            .map(|r| vec![r[0] as i64, r[1] as i64, r[2] as i64, r[3] as i64])
            .collect();
        assert_eq!(
            got, want,
            "monitors.rects must mirror cursor.screens: [l,t,r,b] per monitor"
        );
    }

    /// A desktop/region capture with elements:true names the FOREGROUND window
    /// the element block describes, so it cannot be mistaken for the image's
    /// window. The foreground is forced through the seam, never read ambient:
    /// the assertion is about the handle the call carried, so it means the same
    /// thing on a CI runner whose foreground moves under it.
    #[test]
    fn desktop_screenshot_names_the_elements_window() {
        let _g = crate::fixture::desktop();
        let f = crate::fixture::Fixture::new();
        let hwnd = f.hwnd_i64();
        script_foreground(&[hwnd]);
        let out = screenshot_desktop(&ShotArgs {
            display: -1,
            region: Some(vec![
                Value::from(0),
                Value::from(0),
                Value::from(64),
                Value::from(64),
            ]),
            window: None,
            max_px: Some(64),
            fmt: "png".to_string(),
            quality: Some(80),
            grid: false,
            allow_crop_fallback: false,
            elements: true,
            target: None,
            out: None,
        });
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let sc = outer
            .get("structuredContent")
            .expect("structuredContent carries the coordinate map");
        let ew = sc
            .get("elements_window")
            .expect("elements:true must add elements_window to the payload");
        assert_eq!(
            ew.get("hwnd").and_then(|v| v.as_i64()),
            Some(hwnd),
            "elements_window must be the window the element block describes"
        );
        for k in ["exe", "title"] {
            assert!(
                ew.get(k).and_then(|v| v.as_str()).is_some(),
                "elements_window needs {k}: {ew}"
            );
        }
    }

    /// Regression, CI run 41311923165: the call sampled `GetForegroundWindow()`
    /// for the element walk and again for the `elements_window` name, so a
    /// runner whose foreground moved between the two reads described one
    /// window and named another. The foreground is now sampled ONCE and
    /// carried; the seam moves the ambient value after that first read, which
    /// is exactly what a second read would pick up.
    #[test]
    fn elements_window_keeps_the_handle_sampled_at_call_start() {
        let _g = crate::fixture::desktop();
        let f = crate::fixture::Fixture::new();
        let hwnd = f.hwnd_i64();
        let g = crate::fixture::Fixture::new();
        let moved = g.hwnd_i64();
        script_foreground(&[hwnd, moved]);
        let out = screenshot_desktop(&ShotArgs {
            display: -1,
            region: Some(vec![
                Value::from(0),
                Value::from(0),
                Value::from(64),
                Value::from(64),
            ]),
            window: None,
            max_px: Some(64),
            fmt: "png".to_string(),
            quality: Some(80),
            grid: false,
            allow_crop_fallback: false,
            elements: true,
            target: None,
            out: None,
        });
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let sc = outer
            .get("structuredContent")
            .expect("structuredContent carries the coordinate map");
        let ew = sc
            .get("elements_window")
            .expect("elements:true must add elements_window to the payload");
        assert_eq!(
            ew.get("hwnd").and_then(|v| v.as_i64()),
            Some(hwnd),
            "a foreground switch after the first sample must not re-name the block; \
             got {ew}"
        );
        // Same handle on both halves of the reply: the appended block
        // enumerates the window the payload named.
        let blocks = outer["content"].as_array().expect("content blocks");
        let block: Value = serde_json::from_str(
            blocks
                .last()
                .and_then(|b| b["text"].as_str())
                .expect("the element block is the last content block"),
        )
        .expect("element block is JSON");
        let entries = block["elements"].as_array().expect("elements array");
        assert!(
            !entries.is_empty(),
            "the fixture window has controls; an empty walk would make this \
             assertion vacuous"
        );
        for e in entries {
            assert_eq!(
                e["win"].as_i64(),
                Some(hwnd),
                "every entry must belong to the named window: {e}"
            );
        }
    }

    #[test]
    fn display_zero_selects_first_monitor() {
        assert_eq!(monitor_rect(&two_mons(), 0).unwrap(), [0, 0, 1920, 1080]);
    }

    #[test]
    fn display_one_selects_second_monitor() {
        assert_eq!(monitor_rect(&two_mons(), 1).unwrap(), [1920, 0, 3840, 1080]);
    }

    #[test]
    fn display_out_of_range_rejected() {
        assert!(monitor_rect(&two_mons(), 2).is_err());
        assert!(monitor_rect(&two_mons(), -1).is_err());
        assert!(monitor_rect(&[[0, 0, 1920, 1080]], 0).is_ok());
    }

    #[test]
    fn overflow_region_elem_rejected_with_shared_error() {
        let v = serde_json::json!("9223372036854775808"); // i64::MAX + 1
        assert!(crate::is_int_overflow(&v));
        let region = Some(vec![
            Value::from(0),
            Value::from(0),
            v.clone(),
            Value::from(100),
        ]);
        let err = parse_region(&region).expect_err("MAX+1 region must be rejected");
        assert_eq!(err, crate::int_overflow_err(&v));
    }

    #[test]
    fn overflow_region_u64_rejected_with_shared_error() {
        let v = Value::from(u64::MAX);
        assert!(crate::is_int_overflow(&v));
        let region = Some(vec![
            Value::from(0),
            Value::from(0),
            v.clone(),
            Value::from(10),
        ]);
        let err = parse_region(&region).expect_err("u64::MAX region must be rejected");
        assert_eq!(err, crate::int_overflow_err(&v));
    }

    #[test]
    fn overflow_region_float_rejected_with_shared_error() {
        let v = serde_json::json!(1e19);
        assert!(crate::is_int_overflow(&v));
        let region = Some(vec![
            Value::from(0),
            Value::from(0),
            v.clone(),
            Value::from(10),
        ]);
        let err = parse_region(&region).expect_err("1e19 region must be rejected");
        assert_eq!(err, crate::int_overflow_err(&v));
    }
}
