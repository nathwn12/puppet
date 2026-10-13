//! Window-selector resolution + screen capture (desktop + PrintWindow paths).

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

pub fn resolve_window(title: Option<&str>, hwnd: Option<i64>) -> Resolve {
    if let Some(h) = hwnd {
        return resolve_selector(&format!("hwnd:{h}"));
    }
    if let Some(t) = title {
        if !t.is_empty() {
            return resolve_selector(t);
        }
    }
    (
        None,
        Some("bad_window: need title or hwnd".to_string()),
        None,
    )
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
}

fn gray(r: u8, g: u8, b: u8) -> u8 {
    ((r as u32 * 299 + g as u32 * 587 + b as u32 * 114 + 500) / 1000) as u8
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
    };
    if hi < 8 {
        return Verdict {
            rejected: true,
            reason: Some("all-black".to_string()),
            mean: stats.mean,
            var: stats.var,
            black_ratio: stats.black_ratio,
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
        "detector{{mean={},var={},black_ratio={}}}",
        v.mean, v.var, v.black_ratio
    )
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
    ((ss >> PREC_BITS).max(0).min(255)) as u8
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
                    for xx in 0..dwu {
                        let (xmin, xmax) = b[xx];
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
                    for xx in 0..dwu {
                        let (xmin, xmax) = b[xx];
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

/// Shared long-edge clamp + grid + encode. Returns (data, iw, ih, scale, f, q).
pub fn finalize(
    mut img: Img,
    max_px: Option<i64>,
    fmt: &str,
    quality: Option<i64>,
    grid: bool,
) -> Result<(Vec<u8>, i64, i64, f64, String, i64), String> {
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
    let mut q: i64 = match quality {
        None => 80,
        Some(qq) => qq,
    };
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
}
