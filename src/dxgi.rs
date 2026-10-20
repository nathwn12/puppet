//! DXGI Desktop Duplication fast path for full-output desktop grabs.
//!
//! Why: the GDI BitBlt + GetDIBits round-trip costs ~30-40 ms per
//! 2560x1440 grab on this box (measured; the mss backend in the python
//! incumbent measures the same floor), while a warm Desktop Duplication
//! acquire + GPU->CPU copy costs ~2-6 ms. The python incumbent serves its
//! single-monitor desktop path from bettercam (DXGI) for the same reason.
//!
//! Session/desktop constraint: IDXGIOutput1::DuplicateOutput requires the
//! calling process to run on the interactive desktop it duplicates. A
//! service in session 0, an RDP/Citrix shadow session, or a display driver
//! without duplication support fails session creation (or loses access
//! mid-run via access-lost). Every failure below is an internal Err that
//! the caller maps to the pre-existing GDI path, so a box without
//! duplication keeps working at the old speed with the old bytes on the
//! wire (backend label stays "gdi").
//!
//! Behavior contract (mirrors puppet.py _grab_rect for the DXGI scope):
//! - DXGI serves whole outputs only. The caller passes the full output
//!   size; crops happen after the grab, exactly like the GDI path.
//! - A duplication session is cached per output index (like the python
//!   camera cache): first call warms it, later calls reuse it.
//! - AcquireNextFrame(0) with no new frame is NOT an error when a frame
//!   is already cached: the cached frame is served (still desktop).
//!   With no cache yet, one 20 ms settle + retry runs (python parity),
//!   then the caller falls back to GDI.
//! - Returned frames must be B8G8R8A8_UNORM; anything else falls back.
//! - The hardware cursor is typically NOT part of the duplicated surface
//!   (pointer shape rides a side channel we do not consume); GDI BitBlt
//!   includes it. No gate can depend on cursor pixels (it moves), and the
//!   reported cursor position still comes from GetCursorPos as before.

use crate::cap::Img;
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};

struct Session {
    dev: ID3D11Device,
    dup: IDXGIOutputDuplication,
    ctx: ID3D11DeviceContext,
    staging: Option<ID3D11Texture2D>,
    sw: u32,
    sh: u32,
    cached: Option<Img>,
}

enum Slot {
    Live(Session),
    /// Creation failed; skip fast attempts until the cooldown lapses so a
    /// box without duplication support pays ~0 ms instead of retrying COM
    /// setup on every capture.
    Dead(Instant),
}

const DEAD_COOLDOWN: Duration = Duration::from_secs(5);

thread_local! {
    static SLOTS: RefCell<HashMap<(u32, u32), Slot>> = RefCell::new(HashMap::new());
}

enum GrabFail {
    Timeout,
    AccessLost,
    Other(String),
}

/// Driver type for device creation: an explicit adapter requires UNKNOWN
/// (HARDWARE with an adapter fails the probe with 0x80070057); the legacy
/// default path keeps HARDWARE.
fn driver_type_for(explicit_adapter: bool) -> D3D_DRIVER_TYPE {
    if explicit_adapter {
        D3D_DRIVER_TYPE_UNKNOWN
    } else {
        D3D_DRIVER_TYPE_HARDWARE
    }
}

/// Device creation, bound to one enumerated adapter. Failure is Err (the
/// caller falls back to GDI).
fn create_device_on(
    adapter: Option<&IDXGIAdapter>,
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let levels = [D3D_FEATURE_LEVEL_11_0];
    let mut dev: Option<ID3D11Device> = None;
    let mut ctx: Option<ID3D11DeviceContext> = None;
    let r = unsafe {
        D3D11CreateDevice(
            adapter,
            driver_type_for(adapter.is_some()),
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            Some(&mut ctx),
        )
    };
    if r.is_ok() {
        if let (Some(d), Some(c)) = (dev, ctx) {
            return Ok((d, c));
        }
    }
    Err(windows::core::Error::from_win32())
}

fn open_session(adapter_idx: u32, output_idx: u32) -> windows::core::Result<Session> {
    // Enumerate to the MATCHED adapter and create the device on it — never
    // the default adapter by flattened index (a matched monitor could
    // otherwise fail or address another adapter's output).
    let factory: windows::Win32::Graphics::Dxgi::IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let adapter1 = unsafe { factory.EnumAdapters1(adapter_idx)? };
    let adapter: IDXGIAdapter = adapter1.cast()?;
    let (dev, ctx) = create_device_on(Some(&adapter))?;
    let output = unsafe { adapter.EnumOutputs(output_idx)? };
    let out1: IDXGIOutput1 = output.cast()?;
    let dup = unsafe { out1.DuplicateOutput(&dev)? };
    Ok(Session {
        dev,
        dup,
        ctx,
        staging: None,
        sw: 0,
        sh: 0,
        cached: None,
    })
}

fn copy_frame(
    ses: &mut Session,
    desc_w: u32,
    desc_h: u32,
    tex: &ID3D11Texture2D,
) -> windows::core::Result<Img> {
    if ses.staging.is_none() || ses.sw != desc_w || ses.sh != desc_h {
        let mut d = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut d) };
        d.Usage = D3D11_USAGE_STAGING;
        d.BindFlags = 0;
        d.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        d.MiscFlags = 0;
        let mut st: Option<ID3D11Texture2D> = None;
        unsafe { ses.dev.CreateTexture2D(&d, None, Some(&mut st))? };
        ses.staging = st;
        ses.sw = desc_w;
        ses.sh = desc_h;
    }
    let staging = ses.staging.as_ref().unwrap();
    let src_res: ID3D11Resource = tex.cast()?;
    let dst_res: ID3D11Resource = staging.cast()?;
    unsafe { ses.ctx.CopyResource(&dst_res, &src_res) };
    let mut mapped = Default::default();
    unsafe {
        ses.ctx
            .Map(&dst_res, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?
    };
    let pitch = mapped.RowPitch as usize;
    let n = desc_w as usize * desc_h as usize;
    let mut px = vec![0u8; n * 3];
    unsafe {
        let base = mapped.pData as *const u8;
        for y in 0..desc_h as usize {
            let row = base.add(y * pitch);
            for x in 0..desc_w as usize {
                let s = row.add(x * 4);
                let d = px.as_mut_ptr().add((y * desc_w as usize + x) * 3);
                // BGRA -> RGB (same channel order the GDI path produces).
                *d = *s.add(2);
                *d.add(1) = *s.add(1);
                *d.add(2) = *s;
            }
        }
        ses.ctx.Unmap(&dst_res, 0);
    }
    Ok(Img {
        w: desc_w as i32,
        h: desc_h as i32,
        px,
    })
}

fn acquire(ses: &mut Session) -> Result<Img, GrabFail> {
    let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
    let mut res: Option<IDXGIResource> = None;
    let r = unsafe { ses.dup.AcquireNextFrame(0, &mut info, &mut res) };
    if let Err(e) = r {
        if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
            return Err(GrabFail::Timeout);
        }
        if e.code() == DXGI_ERROR_ACCESS_LOST {
            return Err(GrabFail::AccessLost);
        }
        return Err(GrabFail::Other(format!("dxgi acquire failed: {e}")));
    }
    let out = (|| {
        let tex: ID3D11Texture2D = res.as_ref().unwrap().cast()?;
        let mut d = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut d) };
        if d.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(windows::core::Error::from_win32());
        }
        copy_frame(ses, d.Width, d.Height, &tex)
    })();
    unsafe {
        let _ = ses.dup.ReleaseFrame();
    }
    match out {
        Ok(img) => Ok(img),
        Err(e) => Err(GrabFail::Other(format!("dxgi frame copy failed: {e}"))),
    }
}

fn check_size(img: &Img, w: i32, h: i32) -> Result<(), String> {
    if img.w != w || img.h != h {
        return Err(format!(
            "dxgi returned {}x{} for a {w}x{h} output",
            img.w, img.h
        ));
    }
    Ok(())
}

/// One enumerated DXGI output: desktop rect, owning monitor handle, and
/// device name, plus the (adapter, output) pair for adapter-aware session
/// creation and cache lookup — never a flattened index.
#[derive(Debug, Clone)]
pub struct DxgiOutput {
    pub adapter: u32,
    pub output: u32,
    pub rect: [i32; 4],
    pub hmon: i64,
    pub device: String,
}

/// Enumerate every DXGI output across adapters. Err means "no DXGI
/// enumeration" (caller falls back to GDI, never surfaced to the wire).
pub fn dxgi_outputs() -> Result<Vec<DxgiOutput>, String> {
    let factory: windows::Win32::Graphics::Dxgi::IDXGIFactory1 =
        unsafe { CreateDXGIFactory1().map_err(|e| format!("dxgi factory: {e}"))? };
    let mut out = Vec::new();
    let mut ai = 0u32;
    loop {
        let adapter = match unsafe { factory.EnumAdapters1(ai) } {
            Ok(a) => a,
            Err(_) => break,
        };
        let mut oi = 0u32;
        loop {
            let output = match unsafe { adapter.EnumOutputs(oi) } {
                Ok(o) => o,
                Err(_) => break,
            };
            let desc = unsafe {
                output
                    .GetDesc()
                    .map_err(|e| format!("dxgi output desc: {e}"))?
            };
            let r = desc.DesktopCoordinates;
            let raw = desc.DeviceName;
            let n = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
            out.push(DxgiOutput {
                adapter: ai,
                output: oi,
                rect: [r.left, r.top, r.right, r.bottom],
                hmon: desc.Monitor.0 as i64,
                device: String::from_utf16_lossy(&raw[..n]),
            });
            oi += 1;
        }
        ai += 1;
    }
    if out.is_empty() {
        return Err("dxgi: no outputs enumerated".to_string());
    }
    Ok(out)
}

/// Match a requested monitor to a DXGI output (adapter, output) pair.
/// Rules, in order:
/// (a) desktop-rect equality, (b) owning-monitor handle equality,
/// (c) device-name equality (case-insensitive), (d) unique size match
/// (either orientation — a 90/270-rotated output swaps w/h).
/// Every tier must match EXACTLY one output: clones sharing a rect, a
/// Monitor handle, or a device name are Err, as are duplicate sizes and
/// total misses. The caller falls back to GDI with a "dxgi output
/// unmatched" note rather than serve a wrong monitor (a stale cache hit
/// would poison the session).
pub fn pick_output(
    rect: [i32; 4],
    hmon: i64,
    device: &str,
    outputs: &[DxgiOutput],
) -> Result<(u32, u32), String> {
    let pair = |o: &DxgiOutput| (o.adapter, o.output);
    let rect_hits: Vec<&DxgiOutput> = outputs.iter().filter(|o| o.rect == rect).collect();
    match rect_hits.len() {
        1 => return Ok(pair(rect_hits[0])),
        n if n > 1 => {
            return Err(format!("dxgi rect {rect:?} matches {n} outputs (clone?)"));
        }
        _ => {}
    }
    if hmon != 0 {
        let hmon_hits: Vec<&DxgiOutput> = outputs
            .iter()
            .filter(|o| o.hmon != 0 && o.hmon == hmon)
            .collect();
        match hmon_hits.len() {
            1 => return Ok(pair(hmon_hits[0])),
            n if n > 1 => return Err(format!("dxgi monitor handle matches {n} outputs")),
            _ => {}
        }
    }
    if !device.is_empty() {
        let dev_hits: Vec<&DxgiOutput> = outputs
            .iter()
            .filter(|o| o.device.eq_ignore_ascii_case(device))
            .collect();
        match dev_hits.len() {
            1 => return Ok(pair(dev_hits[0])),
            n if n > 1 => {
                return Err(format!("dxgi device {device} matches {n} outputs"));
            }
            _ => {}
        }
    }
    let (gw, gh) = (rect[2] - rect[0], rect[3] - rect[1]);
    let mut sized: Vec<&DxgiOutput> = outputs
        .iter()
        .filter(|o| {
            let (w, h) = (o.rect[2] - o.rect[0], o.rect[3] - o.rect[1]);
            (w == gw && h == gh) || (w == gh && h == gw)
        })
        .collect();
    sized.dedup_by_key(|o| (o.adapter, o.output));
    match sized.len() {
        0 => Err(format!("no dxgi output matches rect {rect:?}")),
        1 => Ok((sized[0].adapter, sized[0].output)),
        _ => Err(format!(
            "dxgi outputs ambiguous for {gw}x{gh} ({} same-size outputs)",
            sized.len()
        )),
    }
}

/// Grab the full desktop of the matched `(adapter, output)` pair (expected
/// size `w`x`h`). The session is opened on the matched adapter and cached
/// per pair — never the default adapter by flattened index. Err means
/// "fall back to GDI" (never surfaced to the wire).
pub fn dxgi_grab(adapter: u32, output: u32, w: i32, h: i32) -> Result<Img, String> {
    let key = (adapter, output);
    SLOTS.with(|cell| {
        let mut slots = cell.borrow_mut();
        if let Some(Slot::Dead(t)) = slots.get(&key) {
            if t.elapsed() < DEAD_COOLDOWN {
                return Err("dxgi unavailable (cooldown)".to_string());
            }
            slots.remove(&key);
        }
        if let std::collections::hash_map::Entry::Vacant(e) = slots.entry(key) {
            match open_session(adapter, output) {
                Ok(s) => {
                    e.insert(Slot::Live(s));
                }
                Err(err) => {
                    e.insert(Slot::Dead(Instant::now()));
                    return Err(format!("dxgi session unavailable: {err}"));
                }
            }
        }
        // Cold session on a static screen: the first acquire may legitimately
        // find no new frame, so allow one settle + retry before GDI fallback.
        for attempt in 0..2 {
            let r = {
                let slot = slots.get_mut(&key).unwrap();
                let Slot::Live(ses) = slot else {
                    return Err("dxgi unavailable (cooldown)".to_string());
                };
                acquire(ses)
            };
            match r {
                Ok(img) => {
                    check_size(&img, w, h)?;
                    let slot = slots.get_mut(&key).unwrap();
                    let Slot::Live(ses) = slot else {
                        return Err("dxgi unavailable (cooldown)".to_string());
                    };
                    ses.cached = Some(Img {
                        w: img.w,
                        h: img.h,
                        px: img.px.clone(),
                    });
                    return Ok(img);
                }
                Err(GrabFail::AccessLost) => {
                    slots.remove(&key);
                    return Err("dxgi access lost".to_string());
                }
                Err(GrabFail::Timeout) => {
                    let slot = slots.get_mut(&key).unwrap();
                    let Slot::Live(ses) = slot else {
                        return Err("dxgi unavailable (cooldown)".to_string());
                    };
                    if let Some(c) = &ses.cached {
                        if check_size(c, w, h).is_ok() {
                            return Ok(Img {
                                w: c.w,
                                h: c.h,
                                px: c.px.clone(),
                            });
                        }
                        // Stale cache after a resolution change: drop it so the
                        // caller falls through to GDI instead of surfacing a
                        // size-mismatched frame as `capture_failed`.
                        ses.cached = None;
                    }
                    if attempt == 0 {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    return Err("dxgi grab returned None (no new frame after retry)".to_string());
                }
                Err(GrabFail::Other(e)) => return Err(e),
            }
        }
        Err("dxgi grab returned None (no new frame after retry)".to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: i32, h: i32) -> Img {
        Img {
            w,
            h,
            px: vec![0u8; (w as usize) * (h as usize) * 3],
        }
    }

    #[test]
    fn matching_size_passes() {
        assert!(check_size(&img(1920, 1080), 1920, 1080).is_ok());
    }

    #[test]
    fn driver_type_for_explicit_adapter_is_unknown() {
        // Probe result: HARDWARE with an explicit adapter fails device
        // creation (0x80070057); UNKNOWN succeeds (0x0).
        assert_eq!(driver_type_for(true), D3D_DRIVER_TYPE_UNKNOWN);
        assert_eq!(driver_type_for(false), D3D_DRIVER_TYPE_HARDWARE);
    }

    fn two_outputs() -> Vec<DxgiOutput> {
        vec![
            DxgiOutput {
                adapter: 0,
                output: 0,
                rect: [0, 0, 1920, 1080],
                hmon: 11,
                device: "\\\\.\\DISPLAY1".to_string(),
            },
            DxgiOutput {
                adapter: 0,
                output: 1,
                rect: [1920, 0, 3840, 1080],
                hmon: 22,
                device: "\\\\.\\DISPLAY2".to_string(),
            },
        ]
    }

    #[test]
    fn pick_output_exact_rect_wins() {
        let outs = two_outputs();
        assert_eq!(
            pick_output([1920, 0, 3840, 1080], 0, "", &outs),
            Ok((0, 1)),
            "exact desktop-rect match must select that output"
        );
    }

    #[test]
    fn pick_output_hmon_wins_without_rect() {
        // Stale rect (moved monitor): the handle still identifies it.
        let outs = two_outputs();
        assert_eq!(
            pick_output([0, 0, 1, 1], 22, "", &outs),
            Ok((0, 1)),
            "HMONITOR match must select that output"
        );
    }

    #[test]
    fn pick_output_device_name_wins_without_rect_or_hmon() {
        let outs = two_outputs();
        assert_eq!(
            pick_output([0, 0, 1, 1], 0, "\\\\.\\DISPLAY2", &outs),
            Ok((0, 1)),
            "device-name match (case-insensitive) must select that output"
        );
        assert_eq!(
            pick_output([0, 0, 1, 1], 0, "\\\\.\\display1", &outs),
            Ok((0, 0)),
            "device-name match must ignore case"
        );
    }

    #[test]
    fn pick_output_unique_size_matches() {
        let outs = vec![
            DxgiOutput {
                adapter: 0,
                output: 0,
                rect: [0, 0, 1920, 1080],
                hmon: 0,
                device: String::new(),
            },
            DxgiOutput {
                adapter: 0,
                output: 1,
                rect: [0, 0, 2560, 1440],
                hmon: 0,
                device: String::new(),
            },
        ];
        assert_eq!(
            pick_output([9, 9, 9 + 2560, 9 + 1440], 0, "", &outs),
            Ok((0, 1)),
            "a unique size must identify the output without identity data"
        );
    }

    #[test]
    fn pick_output_rotated_swap_matches() {
        // 90/270-degree rotation swaps w/h: a portrait request matches the
        // landscape output when it is the only size candidate.
        let outs = vec![DxgiOutput {
            adapter: 0,
            output: 0,
            rect: [0, 0, 1920, 1080],
            hmon: 0,
            device: String::new(),
        }];
        assert_eq!(
            pick_output([0, 0, 1080, 1920], 0, "", &outs),
            Ok((0, 0)),
            "swapped w/h must match a lone rotated output"
        );
    }

    #[test]
    fn pick_output_duplicate_size_is_err() {
        // The old display-index assumption would serve output 0 here and a
        // same-size wrong monitor would pass check_size: never do that.
        let outs = two_outputs();
        let r = pick_output([7, 7, 7 + 1920, 7 + 1080], 0, "", &outs);
        assert!(
            r.is_err(),
            "duplicate same-size outputs with no identity match must Err, got: {r:?}"
        );
    }

    #[test]
    fn pick_output_no_match_is_err() {
        let outs = two_outputs();
        let r = pick_output([0, 0, 800, 600], 0, "", &outs);
        assert!(r.is_err(), "nothing matching must Err, got: {r:?}");
    }

    fn two_adapters() -> Vec<DxgiOutput> {
        vec![
            DxgiOutput {
                adapter: 0,
                output: 0,
                rect: [0, 0, 1920, 1080],
                hmon: 11,
                device: "\\\\.\\DISPLAY1".to_string(),
            },
            // Same per-adapter output index on a second adapter: a
            // flattened index would address output 1 on adapter 0 instead.
            DxgiOutput {
                adapter: 1,
                output: 0,
                rect: [1920, 0, 3840, 1080],
                hmon: 22,
                device: "\\\\.\\DISPLAY2".to_string(),
            },
        ]
    }

    #[test]
    fn pick_output_preserves_adapter_pair() {
        let outs = two_adapters();
        assert_eq!(
            pick_output([1920, 0, 3840, 1080], 0, "", &outs),
            Ok((1, 0)),
            "rect match on adapter 1 output 0 must keep the pair, not a flattened index"
        );
        assert_eq!(
            pick_output([0, 0, 1, 1], 22, "", &outs),
            Ok((1, 0)),
            "hmon match must keep the adapter pair"
        );
        assert_eq!(
            pick_output([0, 0, 1, 1], 0, "\\\\.\\DISPLAY2", &outs),
            Ok((1, 0)),
            "device match must keep the adapter pair"
        );
    }

    #[test]
    fn pick_output_clone_rect_handle_device_is_err() {
        // Cloned outputs share rect, Monitor handle, and device name: every
        // identity tier is ambiguous, so the matcher must refuse (GDI
        // fallback) instead of serving the first pair.
        let outs = vec![
            DxgiOutput {
                adapter: 0,
                output: 0,
                rect: [0, 0, 1920, 1080],
                hmon: 11,
                device: "\\\\.\\DISPLAY1".to_string(),
            },
            DxgiOutput {
                adapter: 0,
                output: 1,
                rect: [0, 0, 1920, 1080],
                hmon: 11,
                device: "\\\\.\\DISPLAY1".to_string(),
            },
        ];
        for (r, h, d) in [
            ([0, 0, 1920, 1080], 0, ""),
            ([7, 7, 8, 8], 11, ""),
            ([7, 7, 8, 8], 0, "\\\\.\\DISPLAY1"),
        ] {
            let out = pick_output(r, h, d, &outs);
            assert!(
                out.is_err(),
                "clone tier {r:?}/{h}/{d} must Err, got: {out:?}"
            );
        }
    }

    #[test]
    fn pick_output_identity_beats_size_match() {
        // Two same-size outputs: a rect hit on one of them wins outright;
        // the size tier is never reached.
        let outs = vec![
            DxgiOutput {
                adapter: 0,
                output: 0,
                rect: [0, 0, 1920, 1080],
                hmon: 11,
                device: "\\\\.\\DISPLAY1".to_string(),
            },
            DxgiOutput {
                adapter: 1,
                output: 0,
                rect: [1920, 0, 3840, 1080],
                hmon: 22,
                device: "\\\\.\\DISPLAY2".to_string(),
            },
        ];
        assert_eq!(
            pick_output([0, 0, 1920, 1080], 0, "", &outs),
            Ok((0, 0)),
            "exact rect must beat the same-size rival without ambiguity"
        );
    }

    #[test]
    fn stale_cache_size_mismatch_rejected() {
        // Resolution change with a warm cache: the cached 1920x1080 frame
        // must NOT validate for a 2560x1440 output; the caller drops it
        // and falls through to GDI.
        assert!(check_size(&img(1920, 1080), 2560, 1440).is_err());
    }
}
