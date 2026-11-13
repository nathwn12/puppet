//! Window-selector resolution + screen capture (desktop + PrintWindow paths).

use crate::target::{TargetError, TargetLock, TargetSpec};
use crate::{cand_public, candidates, py_repr_str, py_round_half_even, py_round_n, Cand};
use serde_json::{Map, Value};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, SRCCOPY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// ---------------------------------------------------------------- selectors

type Resolve = (
    Option<HWND>,
    Option<String>,
    Option<Vec<Map<String, Value>>>,
);

fn pubc_json(pubc: &[Map<String, Value>]) -> String {
    crate::py_dumps(&Value::Array(
        pubc.iter().cloned().map(Value::Object).collect(),
    ))
}

pub fn resolve_selector(sel: &str) -> Resolve {
    let (kind, val) = crate::parse_selector(sel);
    match kind.as_str() {
        "hwnd" => match crate::parse_int_loose(&val) {
            None => (
                None,
                Some(format!("bad_window: bad hwnd {}", py_repr_str(&val))),
                None,
            ),
            Some(h) => unsafe {
                let hwnd: HWND = h as HWND;
                if IsWindow(hwnd) != 0 {
                    (Some(hwnd), None, None)
                } else {
                    (
                        None,
                        Some(format!("bad_window: hwnd recycled/closed (hwnd={h})")),
                        None,
                    )
                }
            },
        },
        "pid" => match crate::parse_int_loose(&val) {
            None => (
                None,
                Some(format!("bad_window: bad pid {}", py_repr_str(&val))),
                None,
            ),
            Some(want) => {
                let cands = candidates();
                let hits: Vec<&Cand> = cands.iter().filter(|c| c.pid as i64 == want).collect();
                if hits.is_empty() {
                    (
                        None,
                        Some(format!("bad_window: no window with pid={want}")),
                        None,
                    )
                } else if hits.len() > 1 {
                    let pubc: Vec<Map<String, Value>> =
                        hits.iter().map(|c| cand_public(c)).collect();
                    let err = format!(
                        "ambiguous_window: {} windows share pid={want}: {}",
                        hits.len(),
                        pubc_json(&pubc)
                    );
                    (None, Some(err), Some(pubc))
                } else {
                    (Some(hits[0].hwnd), None, None)
                }
            }
        },
        "exe" => {
            if val.is_empty() {
                return (
                    None,
                    Some("bad_window: empty exe: selector".to_string()),
                    None,
                );
            }
            let want = crate::exe_stem(&val);
            let cands = candidates();
            let hits: Vec<&Cand> = cands
                .iter()
                .filter(|c| crate::exe_stem(&c.exe) == want)
                .collect();
            if hits.is_empty() {
                (
                    None,
                    Some(format!(
                        "bad_window: no window with exe={}",
                        py_repr_str(&val)
                    )),
                    None,
                )
            } else if hits.len() > 1 {
                let pubc: Vec<Map<String, Value>> = hits.iter().map(|c| cand_public(c)).collect();
                let err = format!(
                    "ambiguous_window: {} windows share exe={}: {}",
                    hits.len(),
                    py_repr_str(&val),
                    pubc_json(&pubc)
                );
                (None, Some(err), Some(pubc))
            } else {
                (Some(hits[0].hwnd), None, None)
            }
        }
        "title" => {
            if val.is_empty() {
                return (
                    None,
                    Some("bad_window: empty title: selector".to_string()),
                    None,
                );
            }
            let needle = val.to_lowercase();
            let cands = candidates();
            let hits: Vec<&Cand> = cands
                .iter()
                .filter(|c| c.title.to_lowercase().contains(&needle))
                .collect();
            if hits.is_empty() {
                (
                    None,
                    Some(format!("bad_window: no window matching title \"{val}\"")),
                    None,
                )
            } else if hits.len() > 1 {
                let pubc: Vec<Map<String, Value>> = hits.iter().map(|c| cand_public(c)).collect();
                let err = format!(
                    "ambiguous_window: {} windows match title \"{val}\": {}",
                    hits.len(),
                    pubc_json(&pubc)
                );
                (None, Some(err), Some(pubc))
            } else {
                (Some(hits[0].hwnd), None, None)
            }
        }
        "class" => {
            if val.is_empty() {
                return (
                    None,
                    Some("bad_window: empty class: selector".to_string()),
                    None,
                );
            }
            let needle = val.to_lowercase();
            let cands = candidates();
            let hits: Vec<&Cand> = cands
                .iter()
                .filter(|c| c.class.to_lowercase().contains(&needle))
                .collect();
            if hits.is_empty() {
                (
                    None,
                    Some(format!("bad_window: no window matching class \"{val}\"")),
                    None,
                )
            } else if hits.len() > 1 {
                let pubc: Vec<Map<String, Value>> = hits.iter().map(|c| cand_public(c)).collect();
                let err = format!(
                    "ambiguous_window: {} windows match class \"{val}\": {}",
                    hits.len(),
                    pubc_json(&pubc)
                );
                (None, Some(err), Some(pubc))
            } else {
                (Some(hits[0].hwnd), None, None)
            }
        }
        _ => {
            let text = val;
            if text.is_empty() {
                return (
                    None,
                    Some("bad_window: need title or hwnd".to_string()),
                    None,
                );
            }
            let cands = candidates();
            let low = text.to_lowercase();
            let exact: Vec<&Cand> = cands.iter().filter(|c| c.title == text).collect();
            if exact.len() > 1 {
                let pubc: Vec<Map<String, Value>> = exact.iter().map(|c| cand_public(c)).collect();
                let err = format!(
                    "ambiguous_window: {} windows share title {}: {}",
                    exact.len(),
                    py_repr_str(&text),
                    pubc_json(&pubc)
                );
                return (None, Some(err), Some(pubc));
            }
            if exact.len() == 1 {
                return (Some(exact[0].hwnd), None, None);
            }
            let stem = crate::exe_stem(&text);
            let sole: Vec<&Cand> = cands
                .iter()
                .filter(|c| crate::exe_stem(&c.exe) == stem)
                .collect();
            if sole.len() > 1 {
                let pubc: Vec<Map<String, Value>> = sole.iter().map(|c| cand_public(c)).collect();
                let err = format!(
                    "ambiguous_window: {} windows share exe={}: {}",
                    sole.len(),
                    py_repr_str(&text),
                    pubc_json(&pubc)
                );
                return (None, Some(err), Some(pubc));
            }
            if sole.len() == 1 {
                return (Some(sole[0].hwnd), None, None);
            }
            let sub_t: Vec<&Cand> = cands
                .iter()
                .filter(|c| c.title.to_lowercase().contains(&low))
                .collect();
            if sub_t.len() > 1 {
                let pubc: Vec<Map<String, Value>> = sub_t.iter().map(|c| cand_public(c)).collect();
                let err = format!(
                    "ambiguous_window: {} windows match \"{text}\": {}",
                    sub_t.len(),
                    pubc_json(&pubc)
                );
                return (None, Some(err), Some(pubc));
            }
            if sub_t.len() == 1 {
                return (Some(sub_t[0].hwnd), None, None);
            }
            let sub_e: Vec<&Cand> = cands
                .iter()
                .filter(|c| c.exe.to_lowercase().contains(&low))
                .collect();
            if sub_e.len() > 1 {
                let pubc: Vec<Map<String, Value>> = sub_e.iter().map(|c| cand_public(c)).collect();
                let err = format!(
                    "ambiguous_window: {} windows match \"{text}\": {}",
                    sub_e.len(),
                    pubc_json(&pubc)
                );
                return (None, Some(err), Some(pubc));
            }
            if sub_e.len() == 1 {
                return (Some(sub_e[0].hwnd), None, None);
            }
            (
                None,
                Some(format!("bad_window: no window matching \"{text}\"")),
                None,
            )
        }
    }
}

/// The legacy `hwnd`/`title` arguments are the SAME selector grammar as
/// `target`: `hwnd` -> `{hwnd}`, `title` -> `{titleContains}`. Routing them
/// through `TargetSpec` is what gives the old verbs the full target lock
/// (pid + process creation time + image + class, revalidated before acting)
/// instead of a bare `IsWindow` check on a recyclable handle.
pub fn legacy_spec(hwnd: Option<i64>, title: Option<&str>) -> Result<TargetSpec, TargetError> {
    if let Some(h) = hwnd {
        return Ok(TargetSpec {
            hwnd: Some(h),
            ..TargetSpec::default()
        });
    }
    if let Some(t) = title {
        if !t.trim().is_empty() {
            return Ok(TargetSpec {
                title_contains: Some(t.to_string()),
                ..TargetSpec::default()
            });
        }
    }
    Err(TargetError {
        code: crate::target::BAD,
        message: "bad_window: need title or hwnd".to_string(),
        candidates: None,
    })
}

/// Resolve a legacy `hwnd`/`title` pair to a full `TargetLock`, so a caller
/// that used the old arguments still revalidates pid/creation time/image/class
/// before every action and gets the typed `target_gone` on a recycled handle.
pub fn lock_legacy(hwnd: Option<i64>, title: Option<&str>) -> Result<TargetLock, TargetError> {
    crate::target::lock(&legacy_spec(hwnd, title)?)
}

// ---------------------------------------------------------------- image

pub struct Img {
    pub w: i32,
    pub h: i32,
    pub px: Vec<u8>, // RGB
}

fn null_hwnd() -> HWND {
    std::ptr::null_mut()
}

fn grab_bits(
    w: i32,
    h: i32,
    paint: impl FnOnce(windows_sys::Win32::Graphics::Gdi::HDC) -> Result<(), String>,
) -> Result<Img, String> {
    use windows_sys::Win32::Graphics::Gdi::HDC;
    use windows_sys::Win32::Graphics::Gdi::{GetDC, ReleaseDC};
    unsafe {
        let sdc: HDC = GetDC(null_hwnd());
        if sdc.is_null() {
            return Err("GetDC(NULL) failed".to_string());
        }
        let memdc = CreateCompatibleDC(sdc);
        if memdc.is_null() {
            ReleaseDC(null_hwnd(), sdc);
            return Err("CreateCompatibleDC failed".to_string());
        }
        let hbm = CreateCompatibleBitmap(sdc, w, h);
        if hbm.is_null() {
            DeleteDC(memdc);
            ReleaseDC(null_hwnd(), sdc);
            return Err("CreateCompatibleBitmap failed".to_string());
        }
        let old = SelectObject(memdc, hbm);
        let pr = paint(memdc);
        SelectObject(memdc, old);
        let out = if let Err(e) = pr {
            Err(e)
        } else {
            let mut bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: 0,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [Default::default()],
            };
            let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
            let lines = GetDIBits(
                memdc,
                hbm,
                0,
                h as u32,
                buf.as_mut_ptr() as *mut _,
                &mut bmi,
                DIB_RGB_COLORS,
            );
            if lines != h {
                Err(format!("GetDIBits returned {lines}/{h} lines"))
            } else {
                let n = (w as usize) * (h as usize);
                let mut px = vec![0u8; n * 3];
                for i in 0..n {
                    px[3 * i] = buf[4 * i + 2];
                    px[3 * i + 1] = buf[4 * i + 1];
                    px[3 * i + 2] = buf[4 * i];
                }
                Ok(Img { w, h, px })
            }
        };
        DeleteObject(hbm);
        DeleteDC(memdc);
        ReleaseDC(null_hwnd(), sdc);
        out
    }
}

/// # Safety
/// `hwnd` must be a live window handle; it is passed to `PrintWindow` as a raw pointer.
pub unsafe fn printwindow_grab(hwnd: HWND, w: i32, h: i32, flags: u32) -> Result<Img, String> {
    use windows_sys::Win32::Storage::Xps::PrintWindow;
    if w <= 0 || h <= 0 {
        return Err("zero-size bitmap request".to_string());
    }
    grab_bits(w, h, |memdc| unsafe {
        if PrintWindow(hwnd, memdc, flags) != 0 {
            Ok(())
        } else {
            Err(format!("PrintWindow refused (flags={flags:#x})"))
        }
    })
}

pub fn bitblt_grab(l: i32, t: i32, w: i32, h: i32) -> Result<Img, String> {
    use windows_sys::Win32::Graphics::Gdi::{GetDC, ReleaseDC};
    grab_bits(w, h, |memdc| unsafe {
        let sdc = GetDC(null_hwnd());
        if sdc.is_null() {
            return Err("GetDC(NULL) failed".to_string());
        }
        let r = BitBlt(memdc, 0, 0, w, h, sdc, l, t, SRCCOPY);
        ReleaseDC(null_hwnd(), sdc);
        if r == 0 {
            return Err("BitBlt failed".to_string());
        }
        Ok(())
    })
}

// ---------------------------------------------------------------- detector

pub struct Verdict {
    pub rejected: bool,
    pub reason: Option<String>,
    pub mean: f64,
    pub var: f64,
    pub black_ratio: f64,
    /// The largest channel value anywhere in the frame: what the blank rule
    /// measured, reported so a partial-black frame is visibly measured rather
    /// than silently passed.
    pub max_channel: u8,
}

/// The full-blank threshold: a frame whose largest channel value is under
/// this is refused, and every capture reports the value it measured against
/// it (W3.4).
pub const BLANK_THRESHOLD: u8 = 8;

/// The largest channel value in an image, in one pass (no thumbnail, no
/// histogram): the measurement behind `blank_check` on a capture path that
/// does not otherwise run the detector.
pub fn max_channel_of(img: &Img) -> u8 {
    let mut hi = 0u8;
    for i in 0..(img.w as usize * img.h as usize) {
        for c in 0..3 {
            let v = img.px[3 * i + c];
            if v > hi {
                hi = v;
            }
        }
    }
    hi
}

/// The `blank_check{max_channel,verdict,threshold}` object every successful
/// capture reports (W3.4).
pub fn blank_check_json(max_channel: u8) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("max_channel".to_string(), Value::from(max_channel));
    m.insert(
        "verdict".to_string(),
        Value::from(if max_channel < BLANK_THRESHOLD {
            "full_blank"
        } else {
            "content"
        }),
    );
    m.insert("threshold".to_string(), Value::from(BLANK_THRESHOLD));
    m
}

fn gray(r: u8, g: u8, b: u8) -> u8 {
    ((r as u32 * 299 + g as u32 * 587 + b as u32 * 114 + 500) / 1000) as u8
}

/// The detector grey for external visualizers (same weights).
pub(crate) fn gray_of(r: u8, g: u8, b: u8) -> u8 {
    gray(r, g, b)
}

pub fn detector_verdict(img: &Img) -> Verdict {
    let w = img.w as usize;
    let h = img.h as usize;
    if w == 0 || h == 0 {
        return Verdict {
            rejected: true,
            reason: Some("empty_crop".to_string()),
            mean: 0.0,
            var: 0.0,
            black_ratio: 1.0,
            max_channel: 0,
        };
    }
    let n = w * h;
    let mut hi: u8 = 0;
    let mut hist = [0u64; 256];
    for i in 0..n {
        let (r, g, b) = (img.px[3 * i], img.px[3 * i + 1], img.px[3 * i + 2]);
        if r > hi {
            hi = r;
        }
        if g > hi {
            hi = g;
        }
        if b > hi {
            hi = b;
        }
        hist[gray(r, g, b) as usize] += 1;
    }
    let nf = n as f64;
    let mut mean = 0.0;
    for (i, c) in hist.iter().enumerate() {
        mean += i as f64 * *c as f64;
    }
    mean /= nf;
    let mut var = 0.0;
    for (i, c) in hist.iter().enumerate() {
        let d = i as f64 - mean;
        var += *c as f64 * d * d;
    }
    var /= nf;
    // 64x64 BICUBIC thumbnail (Pillow default resample), black = R,G,B all < 8.
    let thumb = pillow_resample(img, 64, 64, ResF::Bicubic);
    let mut black = 0u32;
    for i in 0..64 * 64 {
        if thumb.px[3 * i] < 8 && thumb.px[3 * i + 1] < 8 && thumb.px[3 * i + 2] < 8 {
            black += 1;
        }
    }
    let stats = Verdict {
        rejected: false,
        reason: None,
        mean: py_round_n(mean, 2),
        var: py_round_n(var, 2),
        black_ratio: py_round_n(black as f64 / 4096.0, 4),
        max_channel: hi,
    };
    if hi < BLANK_THRESHOLD {
        return Verdict {
            rejected: true,
            reason: Some("all-black".to_string()),
            mean: stats.mean,
            var: stats.var,
            black_ratio: stats.black_ratio,
            max_channel: hi,
        };
    }
    stats
}

pub fn detector_json(v: &Verdict) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("rejected".to_string(), Value::from(v.rejected));
    m.insert(
        "reason".to_string(),
        match &v.reason {
            Some(r) => Value::from(r.clone()),
            None => Value::Null,
        },
    );
    m.insert("mean".to_string(), Value::from(v.mean));
    m.insert("var".to_string(), Value::from(v.var));
    m.insert("black_ratio".to_string(), Value::from(v.black_ratio));
    m
}

pub fn detector_tag(v: &Verdict) -> String {
    format!(
        "detector{{mean={},var={},black_ratio={}}} blank_check{}",
        v.mean,
        v.var,
        v.black_ratio,
        tag_of(&blank_check_json(v.max_channel))
    )
}

/// One `{k=v,k=v}` tag body, in insertion order, for the refusal text a
/// caller can assert on.
fn tag_of(m: &Map<String, Value>) -> String {
    let body: Vec<String> = m
        .iter()
        .map(|(k, v)| match v {
            Value::String(s) => format!("{k}={s}"),
            other => format!("{k}={other}"),
        })
        .collect();
    format!("{{{}}}", body.join(","))
}

// ---------------------------------------------------------------- Pillow-compatible resample
//
// Faithful port of Pillow's Resample.c two-pass fixed-point path (8-bit):
// separable coeffs, weight normalization, INT32 rounding with a 1<<21 bias,
// arithmetic-shift clip. Used for BILINEAR main resizes and the BICUBIC
// 64x64 detector thumbnail. The ybox row-crop optimization is skipped (it
// only skips rows the vertical pass never reads); values are identical.

const PREC_BITS: i64 = 22;

fn bicubic_kernel(x: f64) -> f64 {
    let a = -0.5;
    let mut x = x;
    if x < 0.0 {
        x = -x;
    }
    if x < 1.0 {
        return ((a + 2.0) * x - (a + 3.0)) * x * x + 1.0;
    }
    if x < 2.0 {
        return (((x - 5.0) * x + 8.0) * x - 4.0) * a;
    }
    0.0
}

fn bilinear_kernel(x: f64) -> f64 {
    let mut x = x;
    if x < 0.0 {
        x = -x;
    }
    if x < 1.0 {
        return 1.0 - x;
    }
    0.0
}

#[derive(Clone, Copy)]
pub enum ResF {
    Bilinear,
    Bicubic,
}

fn kernel_support(f: ResF) -> f64 {
    match f {
        ResF::Bilinear => 1.0,
        ResF::Bicubic => 2.0,
    }
}

fn kernel_fn(f: ResF, x: f64) -> f64 {
    match f {
        ResF::Bilinear => bilinear_kernel(x),
        ResF::Bicubic => bicubic_kernel(x),
    }
}

/// (bounds[(xmin,count)], flat normalized coeffs, ksize)
fn precompute(in_size: i32, out_size: i32, f: ResF) -> (Vec<(i32, i32)>, Vec<i32>, usize) {
    let scale = in_size as f64 / out_size as f64;
    let filterscale = if scale < 1.0 { 1.0 } else { scale };
    let support = kernel_support(f) * filterscale;
    let ksize = (support.ceil() as usize) * 2 + 1;
    let inv = 1.0 / filterscale;
    let mut bounds = Vec::with_capacity(out_size as usize);
    let mut raw = vec![0.0f64; out_size as usize * ksize];
    for o in 0..out_size as usize {
        let center = (o as f64 + 0.5) * scale;
        let mut xmin = (center - support + 0.5) as i32;
        if xmin < 0 {
            xmin = 0;
        }
        let mut xmax = (center + support + 0.5) as i32;
        if xmax > in_size {
            xmax = in_size;
        }
        xmax -= xmin;
        let mut ww = 0.0;
        for x in 0..xmax {
            let w = kernel_fn(f, ((x + xmin) as f64 - center + 0.5) * inv);
            raw[o * ksize + x as usize] = w;
            ww += w;
        }
        if ww != 0.0 {
            for x in 0..xmax {
                let w = raw[o * ksize + x as usize] / ww;
                raw[o * ksize + x as usize] = w;
            }
        }
        bounds.push((xmin, xmax));
    }
    let mut kk = vec![0i32; out_size as usize * ksize];
    for (i, w) in raw.iter().enumerate() {
        kk[i] = if *w < 0.0 {
            (-0.5 + *w * (1i64 << PREC_BITS) as f64) as i32
        } else {
            (0.5 + *w * (1i64 << PREC_BITS) as f64) as i32
        };
    }
    (bounds, kk, ksize)
}

fn clip8(ss: i64) -> u8 {
    (ss >> PREC_BITS).clamp(0, 255) as u8
}

/// Row-parallel driver for the resample passes below. Rows are independent
/// (each output byte is a function of its own row's inputs only), so
/// disjoint row chunks processed on scoped threads yield bit-identical
/// bytes. Small images run inline to avoid spawn overhead (this keeps the
/// window-path detector thumbnail and small crops on the old timing).
fn par_rows(dst: &mut [u8], rows: usize, stride: usize, f: impl Fn(&mut [u8], usize) + Sync) {
    let nt = if rows < 256 { 1 } else { 8.min(rows) };
    if nt == 1 {
        f(dst, 0);
        return;
    }
    let chunk_rows = rows.div_ceil(nt);
    let fr = &f;
    std::thread::scope(|s| {
        let mut rest = dst;
        let mut y0 = 0;
        while y0 < rows {
            let nr = chunk_rows.min(rows - y0);
            let (head, tail) = rest.split_at_mut(nr * stride);
            rest = tail;
            s.spawn(move || fr(head, y0));
            y0 += nr;
        }
    });
}

pub fn pillow_resample(src: &Img, dw: i32, dh: i32, f: ResF) -> Img {
    let (sw, sh) = (src.w, src.h);
    if dw == sw && dh == sh {
        return Img {
            w: dw,
            h: dh,
            px: src.px.clone(),
        };
    }
    // Hot-loop optimization (identical arithmetic to the Pillow port above):
    // per-channel accumulation order is unchanged, so every i64 partial sum
    // and every clipped byte is bit-identical; only the bounds checks are
    // elided (all indices derive from precompute() bounds, verified safe).
    let horizontal_first = !((sh - dh > 0) && (sh - dh) > (sw - dw) * 2);
    // Horizontal pass -> intermediate (full width when horizontal_first).
    let tmp: Img;
    let src_h: &Img;
    if horizontal_first {
        let (b, k, ks) = precompute(sw, dw, f);
        let mut px = vec![0u8; dw as usize * sh as usize * 3];
        {
            let swu = sw as usize;
            let dwu = dw as usize;
            let stride = dwu * 3;
            par_rows(&mut px, sh as usize, stride, |chunk, y0| unsafe {
                let s = src.px.as_ptr();
                let kk = k.as_ptr();
                let dc = chunk.as_mut_ptr();
                for ry in 0..chunk.len() / stride {
                    let y = y0 + ry;
                    for (xx, &(xmin, xmax)) in b.iter().enumerate() {
                        let mut s0: i64 = 1 << (PREC_BITS - 1);
                        let mut s1: i64 = 1 << (PREC_BITS - 1);
                        let mut s2: i64 = 1 << (PREC_BITS - 1);
                        let kr = kk.add(xx * ks);
                        let mut sp = s.add((y * swu + xmin as usize) * 3);
                        for x in 0..xmax as usize {
                            let kv = *kr.add(x) as i64;
                            s0 += *sp as i64 * kv;
                            s1 += *sp.add(1) as i64 * kv;
                            s2 += *sp.add(2) as i64 * kv;
                            sp = sp.add(3);
                        }
                        let dp = dc.add(ry * stride + xx * 3);
                        *dp = clip8(s0);
                        *dp.add(1) = clip8(s1);
                        *dp.add(2) = clip8(s2);
                    }
                }
            });
        }
        tmp = Img { w: dw, h: sh, px };
        src_h = &tmp;
        let (b, k, ks) = precompute(sh, dh, f);
        let mut px = vec![0u8; dw as usize * dh as usize * 3];
        {
            let dwu = dw as usize;
            let stride = dwu * 3;
            par_rows(&mut px, dh as usize, stride, |chunk, yy0| unsafe {
                let s = src_h.px.as_ptr();
                let kk = k.as_ptr();
                let dc = chunk.as_mut_ptr();
                for ryy in 0..chunk.len() / stride {
                    let yy = yy0 + ryy;
                    for xx in 0..dwu {
                        let (ymin, ymax) = b[yy];
                        let mut s0: i64 = 1 << (PREC_BITS - 1);
                        let mut s1: i64 = 1 << (PREC_BITS - 1);
                        let mut s2: i64 = 1 << (PREC_BITS - 1);
                        let kr = kk.add(yy * ks);
                        let mut sp = s.add((ymin as usize * dwu + xx) * 3);
                        for y in 0..ymax as usize {
                            let kv = *kr.add(y) as i64;
                            s0 += *sp as i64 * kv;
                            s1 += *sp.add(1) as i64 * kv;
                            s2 += *sp.add(2) as i64 * kv;
                            sp = sp.add(dwu * 3);
                        }
                        let dp = dc.add(ryy * stride + xx * 3);
                        *dp = clip8(s0);
                        *dp.add(1) = clip8(s1);
                        *dp.add(2) = clip8(s2);
                    }
                }
            });
        }
        Img { w: dw, h: dh, px }
    } else {
        let (b, k, ks) = precompute(sh, dh, f);
        let mut px = vec![0u8; sw as usize * dh as usize * 3];
        {
            let swu = sw as usize;
            let stride = swu * 3;
            par_rows(&mut px, dh as usize, stride, |chunk, yy0| unsafe {
                let s = src.px.as_ptr();
                let kk = k.as_ptr();
                let dc = chunk.as_mut_ptr();
                for ryy in 0..chunk.len() / stride {
                    let yy = yy0 + ryy;
                    for xx in 0..swu {
                        let (ymin, ymax) = b[yy];
                        let mut s0: i64 = 1 << (PREC_BITS - 1);
                        let mut s1: i64 = 1 << (PREC_BITS - 1);
                        let mut s2: i64 = 1 << (PREC_BITS - 1);
                        let kr = kk.add(yy * ks);
                        let mut sp = s.add((ymin as usize * swu + xx) * 3);
                        for y in 0..ymax as usize {
                            let kv = *kr.add(y) as i64;
                            s0 += *sp as i64 * kv;
                            s1 += *sp.add(1) as i64 * kv;
                            s2 += *sp.add(2) as i64 * kv;
                            sp = sp.add(swu * 3);
                        }
                        let dp = dc.add(ryy * stride + xx * 3);
                        *dp = clip8(s0);
                        *dp.add(1) = clip8(s1);
                        *dp.add(2) = clip8(s2);
                    }
                }
            });
        }
        tmp = Img { w: sw, h: dh, px };
        src_h = &tmp;
        let (b, k, ks) = precompute(sw, dw, f);
        let mut px = vec![0u8; dw as usize * dh as usize * 3];
        {
            let swu = sw as usize;
            let dwu = dw as usize;
            let stride = dwu * 3;
            par_rows(&mut px, dh as usize, stride, |chunk, y0| unsafe {
                let s = src_h.px.as_ptr();
                let kk = k.as_ptr();
                let dc = chunk.as_mut_ptr();
                for ry in 0..chunk.len() / stride {
                    let y = y0 + ry;
                    for (xx, &(xmin, xmax)) in b.iter().enumerate() {
                        let mut s0: i64 = 1 << (PREC_BITS - 1);
                        let mut s1: i64 = 1 << (PREC_BITS - 1);
                        let mut s2: i64 = 1 << (PREC_BITS - 1);
                        let kr = kk.add(xx * ks);
                        let mut sp = s.add((y * swu + xmin as usize) * 3);
                        for x in 0..xmax as usize {
                            let kv = *kr.add(x) as i64;
                            s0 += *sp as i64 * kv;
                            s1 += *sp.add(1) as i64 * kv;
                            s2 += *sp.add(2) as i64 * kv;
                            sp = sp.add(3);
                        }
                        let dp = dc.add(ry * stride + xx * 3);
                        *dp = clip8(s0);
                        *dp.add(1) = clip8(s1);
                        *dp.add(2) = clip8(s2);
                    }
                }
            });
        }
        Img { w: dw, h: dh, px }
    }
}

pub fn draw_grid(img: &mut Img) {
    let (iw, ih) = (img.w as usize, img.h as usize);
    let set = |img: &mut Img, x: usize, y: usize, r: u8, g: u8, b: u8| {
        if x < iw && y < ih {
            img.px[(y * iw + x) * 3] = r;
            img.px[(y * iw + x) * 3 + 1] = g;
            img.px[(y * iw + x) * 3 + 2] = b;
        }
    };
    let step = 128usize;
    let mut gx = step;
    while gx < iw {
        for y in 0..ih {
            set(img, gx, y, 192, 192, 192);
        }
        gx += step;
    }
    let mut gy = step;
    while gy < ih {
        for x in 0..iw {
            set(img, x, gy, 192, 192, 192);
        }
        gy += step;
    }
    gx = 0;
    while gx < iw {
        for k in 0..8 {
            set(img, gx, k, 255, 255, 255);
            if ih > k {
                set(img, gx, ih - 1 - k, 255, 255, 255);
            }
        }
        gx += step;
    }
    gy = 0;
    while gy < ih {
        for k in 0..8 {
            set(img, k, gy, 255, 255, 255);
            if iw > k {
                set(img, iw - 1 - k, gy, 255, 255, 255);
            }
        }
        gy += step;
    }
}

pub fn encode_png(img: &Img) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let mut enc = png::Encoder::new(&mut buf, img.w as u32, img.h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(|e| format!("{e}"))?;
    w.write_image_data(&img.px).map_err(|e| format!("{e}"))?;
    drop(w);
    Ok(buf)
}

pub fn encode_jpeg(img: &Img, q: u8) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let enc = jpeg_encoder::Encoder::new(&mut buf, q);
    enc.encode(
        &img.px,
        img.w as u16,
        img.h as u16,
        jpeg_encoder::ColorType::Rgb,
    )
    .map_err(|e| format!("{e}"))?;
    Ok(buf)
}

// ---------------------------------------------------------------- PNG decode + pixel diff
//
// Ported from `Q:\PROJECTS\PERSONAL\gladius\src\visual.rs`
// (`png_diff`/`DiffOpts`): per-channel absolute delta above `threshold`
// counts, union bbox, ratio. Gladius runs that rule inside a headless
// engine over canvas ImageData; puppet runs the same rule natively on
// `Img` pixels so a `diff` needs no browser, no window and no input.
// There is no anti-aliasing rule here: the per-channel threshold is the
// only tolerance, 0 by default (a diff needs lossless PNG pixels).

/// Decode PNG bytes to RGB pixels. RGB and RGBA (alpha dropped) plus
/// 8-bit grayscale decode; anything else is a typed refusal, never a
/// guess (a palette/misdepth frame must not diff against raw RGB).
pub fn decode_png(bytes: &[u8]) -> Result<Img, String> {
    use std::io::Cursor;
    let dec = png::Decoder::new(Cursor::new(bytes));
    let mut reader = dec.read_info().map_err(|e| format!("{e}"))?;
    let buf_len = reader
        .output_buffer_size()
        .ok_or_else(|| "error: bad_png: cannot size the decode buffer".to_string())?;
    let mut buf = vec![0u8; buf_len];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("{e}"))?;
    if info.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "error: bad_png: only 8-bit PNGs diff (got {:?})",
            info.bit_depth
        ));
    }
    let (w, h) = (info.width as i32, info.height as i32);
    let n = (w as usize) * (h as usize);
    let px: Vec<u8> = match info.color_type {
        png::ColorType::Rgb => {
            if buf.len() < n * 3 {
                return Err(format!(
                    "error: bad_png: short RGB frame ({} bytes for a {w}x{h} image)",
                    buf.len()
                ));
            }
            buf[..n * 3].to_vec()
        }
        png::ColorType::Rgba => {
            if buf.len() < n * 4 {
                return Err(format!(
                    "error: bad_png: short RGBA frame ({} bytes for a {w}x{h} image)",
                    buf.len()
                ));
            }
            let mut px = vec![0u8; n * 3];
            for i in 0..n {
                px[3 * i] = buf[4 * i];
                px[3 * i + 1] = buf[4 * i + 1];
                px[3 * i + 2] = buf[4 * i + 2];
            }
            px
        }
        png::ColorType::Grayscale => {
            if buf.len() < n {
                return Err(format!(
                    "error: bad_png: short grayscale frame ({} bytes for a {w}x{h} image)",
                    buf.len()
                ));
            }
            let mut px = vec![0u8; n * 3];
            for i in 0..n {
                px[3 * i] = buf[i];
                px[3 * i + 1] = buf[i];
                px[3 * i + 2] = buf[i];
            }
            px
        }
        png::ColorType::GrayscaleAlpha => {
            if buf.len() < n * 2 {
                return Err(format!(
                    "error: bad_png: short grayscale+alpha frame ({} bytes for a {w}x{h} image)",
                    buf.len()
                ));
            }
            let mut px = vec![0u8; n * 3];
            for i in 0..n {
                px[3 * i] = buf[2 * i];
                px[3 * i + 1] = buf[2 * i];
                px[3 * i + 2] = buf[2 * i];
            }
            px
        }
        t => {
            return Err(format!(
                "error: bad_png: unsupported PNG color type ({t:?}); save an RGB/RGBA PNG"
            ));
        }
    };
    Ok(Img { w, h, px })
}

/// Pixel-diff outcome over two same-dimension images.
#[derive(Debug)]
pub struct PixelDiff {
    pub changed: i64,
    pub total: i64,
    pub ratio: f64,
    /// Union box of changed pixels as [x,y,w,h], None when identical.
    pub bbox: Option<[i64; 4]>,
    pub identical: bool,
    /// Pixels the mask excluded from every count above.
    pub masked: i64,
}

/// A native-pixel exclusion mask for a diff: `[left,top,right,bottom]` rects
/// in the operand's own pixel grid, half-open like `region`. Overlapping
/// rects union silently; a rect that leaves the frame or has no area is a
/// typed `bad_region`, never a silent clamp.
pub struct Mask {
    w: i32,
    bits: Vec<bool>,
    count: i64,
}

impl Mask {
    pub fn new(rects: &[[i64; 4]], w: i32, h: i32) -> Result<Mask, String> {
        if w <= 0 || h <= 0 {
            return Err(format!(
                "error: bad_region: mask needs a {w}x{h} frame to mask"
            ));
        }
        let mut bits = vec![false; (w as usize) * (h as usize)];
        let mut count = 0i64;
        for &[l, t, r, b] in rects.iter() {
            if l < 0 || t < 0 || r > w as i64 || b > h as i64 {
                return Err(format!(
                    "error: bad_region: mask rect [{l},{t},{r},{b}] is outside the {w}x{h} frame"
                ));
            }
            if r <= l || b <= t {
                return Err(format!(
                    "error: bad_region: mask rect [{l},{t},{r},{b}] is empty (need right>left and bottom>top)"
                ));
            }
            for y in t..b {
                let row = y as usize * w as usize;
                for x in l..r {
                    let i = row + x as usize;
                    if !bits[i] {
                        bits[i] = true;
                        count += 1;
                    }
                }
            }
        }
        Ok(Mask { w, bits, count })
    }

    /// Pixels this mask excludes (the union, not the sum).
    pub fn count(&self) -> i64 {
        self.count
    }

    fn covers(&self, x: i64, y: i64) -> bool {
        self.bits[y as usize * self.w as usize + x as usize]
    }
}

/// Per-pixel compare: a pixel counts when its largest per-channel
/// absolute delta exceeds `threshold` (0-255, default 0). Dimensions must
/// already match; a mismatch is a typed refusal that names both sizes —
/// never a silent scale. Masked pixels are excluded from `changed`, `total`,
/// `ratio` and `bbox`, and reported in `masked`.
pub fn pixel_diff(
    base: &Img,
    next: &Img,
    threshold: u8,
    mask: Option<&Mask>,
) -> Result<PixelDiff, String> {
    if base.w != next.w || base.h != next.h {
        return Err(format!(
            "error: size_mismatch: base {}x{} vs next {}x{} (same pixel dimensions required; refusing to scale silently)",
            base.w, base.h, next.w, next.h
        ));
    }
    let n = (base.w as usize) * (base.h as usize);
    let mut changed: i64 = 0;
    let (mut x0, mut y0) = (base.w as i64, base.h as i64);
    let (mut x1, mut y1) = (-1i64, -1i64);
    let w = base.w as usize;
    for p in 0..n {
        let (x, y) = ((p % w) as i64, (p / w) as i64);
        if mask.is_some_and(|m| m.covers(x, y)) {
            continue;
        }
        let mut d: u8 = 0;
        for c in 0..3 {
            let a = base.px[3 * p + c];
            let b = next.px[3 * p + c];
            let dd = a.abs_diff(b);
            if dd > d {
                d = dd;
            }
        }
        if d > threshold {
            changed += 1;
            if x < x0 {
                x0 = x;
            }
            if y < y0 {
                y0 = y;
            }
            if x > x1 {
                x1 = x;
            }
            if y > y1 {
                y1 = y;
            }
        }
    }
    let masked = mask.map(Mask::count).unwrap_or(0);
    let total = n as i64 - masked;
    let identical = changed == 0;
    Ok(PixelDiff {
        changed,
        total,
        ratio: if total > 0 {
            changed as f64 / total as f64
        } else {
            0.0
        },
        bbox: if identical {
            None
        } else {
            Some([x0, y0, x1 - x0 + 1, y1 - y0 + 1])
        },
        identical,
        masked,
    })
}

/// Diff visualization: changed pixels solid red over a dimmed greyscale
/// copy of `base` (same 299/587/114 weights as `detector_verdict`'s gray,
/// dimmed 3/5 so red reads against it). Same-dimension input only. A masked
/// pixel is never painted red: the counts exclude it, so the picture must
/// too, or the visualization would claim a change the numbers deny.
pub fn diff_image(
    base: &Img,
    next: &Img,
    threshold: u8,
    mask: Option<&Mask>,
) -> Result<Img, String> {
    if base.w != next.w || base.h != next.h {
        return Err(format!(
            "error: size_mismatch: base {}x{} vs next {}x{} (same pixel dimensions required; refusing to scale silently)",
            base.w, base.h, next.w, next.h
        ));
    }
    let n = (base.w as usize) * (base.h as usize);
    let w = base.w as usize;
    let mut px = vec![0u8; n * 3];
    for p in 0..n {
        let (r, g, b) = (base.px[3 * p], base.px[3 * p + 1], base.px[3 * p + 2]);
        let mut d: u8 = 0;
        if !mask.is_some_and(|m| m.covers((p % w) as i64, (p / w) as i64)) {
            for c in 0..3 {
                let dd = base.px[3 * p + c].abs_diff(next.px[3 * p + c]);
                if dd > d {
                    d = dd;
                }
            }
        }
        if d > threshold {
            px[3 * p] = 255;
            px[3 * p + 1] = 0;
            px[3 * p + 2] = 0;
        } else {
            let dim = (gray(r, g, b) as u32 * 3 / 5) as u8;
            px[3 * p] = dim;
            px[3 * p + 1] = dim;
            px[3 * p + 2] = dim;
        }
    }
    Ok(Img {
        w: base.w,
        h: base.h,
        px,
    })
}

/// Write already-encoded capture bytes to `path` (parent dirs created).
/// `path` must be absolute; a write failure is a typed refusal, never a
/// silent drop of the file half of the call.
pub fn write_bytes_out(path: &str, bytes: &[u8]) -> Result<(), String> {
    let p = std::path::Path::new(path);
    if !p.is_absolute() {
        return Err(format!(
            "error: bad_out: out must be an absolute path (got {path})"
        ));
    }
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("error: bad_out: cannot create {}: {e}", parent.display()))?;
        }
    }
    std::fs::write(p, bytes)
        .map_err(|e| format!("error: bad_out: cannot write {}: {e}", p.display()))?;
    Ok(())
}

/// The `out:{path,bytes,fmt}` payload object for a written capture.
pub fn out_object(path: &str, bytes_len: usize, fmt: &str) -> Map<String, Value> {
    let mut o = Map::new();
    o.insert("path".to_string(), Value::from(path.to_string()));
    o.insert("bytes".to_string(), Value::from(bytes_len as i64));
    o.insert("fmt".to_string(), Value::from(fmt.to_string()));
    o
}
/// Finalize output: encoded bytes, image w/h, applied scale, resolved
/// format, clamped quality.
type Finalized = (Vec<u8>, i64, i64, f64, String, i64);

/// Shared long-edge clamp + grid + encode. Returns (data, iw, ih, scale, f, q).
pub fn finalize(
    mut img: Img,
    max_px: Option<i64>,
    fmt: &str,
    quality: Option<i64>,
    grid: bool,
) -> Result<Finalized, String> {
    let mp = match max_px {
        None => None,
        Some(m) => {
            if m <= 0 {
                return Err("error: bad_region: max_px must be a positive int or null".to_string());
            }
            if m > 8192 {
                return Err(format!("error: bad_region: max_px absurd (>8192): {m}"));
            }
            Some(m)
        }
    };
    let mut f = fmt.to_lowercase();
    if f.is_empty() {
        f = "jpeg".to_string();
    }
    if f == "jpg" {
        f = "jpeg".to_string();
    }
    if f != "png" && f != "jpeg" {
        return Err("error: bad_fmt: fmt must be \"png\" or \"jpeg\"".to_string());
    }
    let mut q: i64 = quality.unwrap_or(80);
    q = q.clamp(1, 100);
    let (sw, sh) = (img.w, img.h);
    let (mut iw, mut ih) = (sw as i64, sh as i64);
    if let Some(m) = mp {
        if sw.max(sh) as i64 > m {
            let k = m as f64 / sw.max(sh) as f64;
            iw = py_round_half_even(sw as f64 * k) as i64;
            ih = py_round_half_even(sh as f64 * k) as i64;
            if iw < 1 {
                iw = 1;
            }
            if ih < 1 {
                ih = 1;
            }
            img = pillow_resample(&img, iw as i32, ih as i32, ResF::Bilinear);
        }
    }
    let scale = iw as f64 / sw as f64;
    if grid {
        draw_grid(&mut img);
    }
    let data = if f == "jpeg" {
        encode_jpeg(&img, q as u8).map_err(|e| format!("error: capture_failed: {e}"))?
    } else {
        encode_png(&img).map_err(|e| format!("error: capture_failed: {e}"))?
    };
    Ok((data, iw, ih, scale, f, q))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny() -> Img {
        Img {
            w: 4,
            h: 4,
            px: vec![128u8; 4 * 4 * 3],
        }
    }

    #[test]
    fn finalize_rejects_absurd_max_px() {
        let r = finalize(tiny(), Some(1_000_000_000_000), "jpeg", Some(80), false);
        assert!(r.is_err(), "absurd max_px must be rejected, got Ok");
    }

    #[test]
    fn finalize_rejects_nonpositive_max_px() {
        assert!(finalize(tiny(), Some(0), "jpeg", Some(80), false).is_err());
        assert!(finalize(tiny(), Some(-5), "jpeg", Some(80), false).is_err());
    }

    #[test]
    fn finalize_accepts_sane_max_px() {
        let (data, iw, ih, _, f, q) =
            finalize(tiny(), Some(1280), "jpeg", Some(80), false).expect("sane max_px");
        assert_eq!((iw, ih), (4, 4));
        assert_eq!(f, "jpeg");
        assert_eq!(q, 80);
        assert!(!data.is_empty());
    }

    fn solid_rgb(w: i32, h: i32, r: u8, g: u8, b: u8) -> Img {
        let mut px = vec![0u8; w as usize * h as usize * 3];
        for i in 0..w as usize * h as usize {
            px[3 * i] = r;
            px[3 * i + 1] = g;
            px[3 * i + 2] = b;
        }
        Img { w, h, px }
    }

    /// Two generated PNGs, round-tripped through encode/decode: identical
    /// bytes diff to identical, and one 3x3 block change reports its exact
    /// bbox and count.
    #[test]
    fn diff_identical_pngs_are_identical() {
        let a = solid_rgb(10, 10, 60, 120, 200);
        let b = solid_rgb(10, 10, 60, 120, 200);
        let da = decode_png(&encode_png(&a).expect("encode a")).expect("decode a");
        let db = decode_png(&encode_png(&b).expect("encode b")).expect("decode b");
        let d = pixel_diff(&da, &db, 0, None).expect("same sizes diff");
        assert!(d.identical, "identical PNGs must be identical");
        assert_eq!(d.changed, 0);
        assert_eq!(d.total, 100);
        assert!(d.bbox.is_none(), "identical diff has no bbox");
    }

    #[test]
    fn diff_three_by_three_block_reports_exact_bbox_and_count() {
        let base = solid_rgb(10, 10, 60, 120, 200);
        let mut altered = solid_rgb(10, 10, 60, 120, 200);
        // Paint a 3x3 block at x=4..6, y=2..4 solid red.
        for y in 2..5 {
            for x in 4..7 {
                let p = (y * 10 + x) as usize;
                altered.px[3 * p] = 255;
                altered.px[3 * p + 1] = 0;
                altered.px[3 * p + 2] = 0;
            }
        }
        let db = decode_png(&encode_png(&base).expect("encode base")).expect("decode base");
        let dn = decode_png(&encode_png(&altered).expect("encode next")).expect("decode next");
        let d = pixel_diff(&db, &dn, 0, None).expect("same sizes diff");
        assert!(!d.identical);
        assert_eq!(d.changed, 9);
        assert_eq!(d.bbox, Some([4, 2, 3, 3]));
    }

    #[test]
    fn diff_size_mismatch_names_both_sizes() {
        let a = solid_rgb(4, 4, 1, 2, 3);
        let b = solid_rgb(5, 4, 1, 2, 3);
        let e = pixel_diff(&a, &b, 0, None).expect_err("different widths must refuse");
        assert!(
            e.contains("size_mismatch"),
            "refusal must be typed size_mismatch, got: {e}"
        );
        assert!(
            e.contains("4x4") && e.contains("5x4"),
            "refusal must name both sizes, got: {e}"
        );
        let c = solid_rgb(4, 6, 1, 2, 3);
        let e2 = pixel_diff(&a, &c, 0, None).expect_err("different heights must refuse");
        assert!(e2.contains("size_mismatch"), "got: {e2}");
        assert!(e2.contains("4x4") && e2.contains("4x6"), "got: {e2}");
    }

    /// The legacy `hwnd`/`title` arguments are the target lock, not a bare
    /// IsWindow check: the lock carries pid + creation time + image + class,
    /// and a forged recycled creation time is refused with the same typed
    /// `target_gone` the `target:` path produces.
    #[test]
    fn legacy_selectors_share_the_target_lock() {
        let _g = crate::fixture::desktop();
        let f = crate::fixture::Fixture::new();
        let top = f.hwnd_i64();
        let by_hwnd = lock_legacy(Some(top), None).expect("legacy hwnd locks");
        assert_eq!(by_hwnd.hwnd, top);
        assert_eq!(by_hwnd.pid, std::process::id());
        assert_ne!(by_hwnd.created, 0, "the lock carries the creation time");
        by_hwnd.revalidate().expect("a fresh legacy lock validates");

        // title -> {titleContains}: the fixture title is unique per pid+seq.
        let title = unsafe { crate::win_text(f.hwnd()) };
        let by_title = lock_legacy(None, Some(title.as_str())).expect("legacy title locks");
        assert_eq!(by_title.hwnd, top);
        assert_eq!(by_title.class, by_hwnd.class);
        assert_eq!(by_title.created, by_hwnd.created);

        // A forged creation time is a recycled PID: the SAME typed refusal.
        let mut forged = by_hwnd.clone();
        forged.created = by_hwnd.created.wrapping_add(1);
        let e = forged
            .revalidate()
            .expect_err("a recycled pid must be refused");
        assert_eq!(e.code, crate::target::GONE, "{}", e.text());
        assert!(
            e.text().contains("creation time"),
            "the refusal must name the creation time: {}",
            e.text()
        );

        // Empty legacy arguments stay a typed bad_target, never a guess.
        let e = lock_legacy(None, None).expect_err("no selector is an error");
        assert_eq!(e.code, crate::target::BAD);
    }
}
