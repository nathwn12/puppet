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
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
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
    static SLOTS: RefCell<HashMap<u32, Slot>> = RefCell::new(HashMap::new());
}

enum GrabFail {
    Timeout,
    AccessLost,
    Other(String),
}

fn create_device() -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let levels = [D3D_FEATURE_LEVEL_11_0];
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut dev: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        let r = unsafe {
            D3D11CreateDevice(
                None,
                driver,
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
    }
    Err(windows::core::Error::from_win32())
}

fn open_session(output: u32) -> windows::core::Result<Session> {
    let (dev, ctx) = create_device()?;
    let dxgi_dev: IDXGIDevice = dev.cast()?;
    let adapter = unsafe { dxgi_dev.GetAdapter()? };
    let output = unsafe { adapter.EnumOutputs(output)? };
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

/// Grab the full `output` desktop (expected size `w`x`h`). Err means
/// "fall back to GDI" (never surfaced to the wire).
pub fn dxgi_grab(output: u32, w: i32, h: i32) -> Result<Img, String> {
    SLOTS.with(|cell| {
        let mut slots = cell.borrow_mut();
        if let Some(Slot::Dead(t)) = slots.get(&output) {
            if t.elapsed() < DEAD_COOLDOWN {
                return Err("dxgi unavailable (cooldown)".to_string());
            }
            slots.remove(&output);
        }
        if !slots.contains_key(&output) {
            match open_session(output) {
                Ok(s) => {
                    slots.insert(output, Slot::Live(s));
                }
                Err(e) => {
                    slots.insert(output, Slot::Dead(Instant::now()));
                    return Err(format!("dxgi session unavailable: {e}"));
                }
            }
        }
        // Cold session on a static screen: the first acquire may legitimately
        // find no new frame, so allow one settle + retry before GDI fallback.
        for attempt in 0..2 {
            let r = {
                let slot = slots.get_mut(&output).unwrap();
                let Slot::Live(ses) = slot else {
                    return Err("dxgi unavailable (cooldown)".to_string());
                };
                acquire(ses)
            };
            match r {
                Ok(img) => {
                    check_size(&img, w, h)?;
                    let slot = slots.get_mut(&output).unwrap();
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
                    slots.remove(&output);
                    return Err("dxgi access lost".to_string());
                }
                Err(GrabFail::Timeout) => {
                    let slot = slots.get_mut(&output).unwrap();
                    let Slot::Live(ses) = slot else {
                        return Err("dxgi unavailable (cooldown)".to_string());
                    };
                    if let Some(c) = &ses.cached {
                        return Ok(Img {
                            w: c.w,
                            h: c.h,
                            px: c.px.clone(),
                        });
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
