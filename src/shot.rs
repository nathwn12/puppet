use crate::cap::{
    bitblt_grab, blank_check_json, detector_json, detector_tag, detector_verdict, printwindow_grab,
    Img,
};
use crate::{
    cloaked, dwm_rect, enum_child_windows, exe_of_pid, pid_of, win_class, win_text, ELEMENT_CAP,
    MODE_B_NOTE, STALE, UIA_DEPTH_LIMIT, WDA_EXCLUDEFROMCAPTURE,
};
use serde_json::{Map, Value};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Instant;
use windows_sys::Win32::Foundation::{GetLastError, HWND, RECT};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub struct CapOk {
    pub img: Img,
    pub backend: String,
    pub ms: f64,
    pub print_flags: Option<i64>,
    pub rect_source: String,
    pub rect_delta: [i32; 4],
    pub detector: Map<String, Value>,
    /// W3.4: the blank-frame measurement the capture made, reported on every
    /// success so a partial-black frame is visibly measured, never assumed.
    pub blank_check: Map<String, Value>,
    pub stale: bool,
    /// True when the DWM frame rect changed between the pre-capture read and
    /// the final read: `dwm` is then the POST-capture rect, so the mapping
    /// always names a current origin. A pure translation is recaptured once.
    pub moved_during_capture: bool,
    pub dwm: [i32; 4],
    pub hwnd: i64,
    pub pid: u32,
    pub exe: String,
    pub title: String,
    pub cloaked: i64,
    pub note: Option<String>,
}

fn frame_sig(img: &Img, title: &str, dwm: &[i32; 4]) -> (String, (i32, i32, i32, i32), u64) {
    let mut h = DefaultHasher::new();
    img.px.hash(&mut h);
    (
        title.to_string(),
        (dwm[0], dwm[1], dwm[2], dwm[3]),
        h.finish(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RectChange {
    Unchanged,
    Translated,
    Resized,
}

pub(crate) fn classify_rect_change(before: [i32; 4], after: [i32; 4]) -> RectChange {
    if before == after {
        RectChange::Unchanged
    } else if after[2] - after[0] == before[2] - before[0]
        && after[3] - after[1] == before[3] - before[1]
    {
        RectChange::Translated
    } else {
        RectChange::Resized
    }
}

pub(crate) fn reported_rect(before: [i32; 4], after: [i32; 4]) -> [i32; 4] {
    if after == before {
        before
    } else {
        after
    }
}

pub(crate) fn translation_rect(
    before: [i32; 4],
    post: [i32; 4],
    recaptured: bool,
) -> ([i32; 4], Option<String>) {
    if recaptured {
        return (post, None);
    }
    (
        before,
        Some(format!(
            "the window moved during the capture (rendered at [{},{},{},{}], now at \
             [{},{},{},{}]) but the recapture failed, so `source` is the PRE-capture rect the \
             image was rendered at and must not be mapped to the current window position; \
             capture again to get current coordinates",
            before[0], before[1], before[2], before[3], post[0], post[1], post[2], post[3]
        )),
    )
}

unsafe fn render_frame(hwnd: HWND, ww: i32, wh: i32) -> Result<(Img, u32), String> {
    let mut last_err = String::from("PrintWindow refused");
    for flags in [2u32, 0u32] {
        match printwindow_grab(hwnd, ww, wh, flags) {
            Ok(im) => return Ok((im, flags)),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// # Safety
/// `hwnd` must be a live window handle; it is passed to the Win32 window APIs as a raw pointer.
pub unsafe fn capture_window(hwnd: HWND, allow_crop_fallback: bool) -> Result<CapOk, String> {
    let h = hwnd as i64;
    crate::deadline::bound(
        crate::deadline::Family::Capture,
        "capture_window",
        move || unsafe { capture_window_inner(h as HWND, allow_crop_fallback) },
    )?
}

/// # Safety
/// `hwnd` must be a live window handle; it is passed to the Win32 window APIs as a raw pointer.
unsafe fn capture_window_inner(hwnd: HWND, allow_crop_fallback: bool) -> Result<CapOk, String> {
    let t0 = Instant::now();
    let h = hwnd as i64;
    unsafe {
        if IsWindow(hwnd) == 0 {
            return Err(format!(
                "error: bad_window: hwnd recycled/closed (hwnd={h})"
            ));
        }
    }
    let title = unsafe { win_text(hwnd) };
    if unsafe { IsIconic(hwnd) } != 0 {
        return Err("error: window_minimized: the target window is minimized; capturing it would need a restore, which puppet refuses. Restore it yourself, or capture another window.".to_string());
    }
    let cloak = cloaked(hwnd);
    if cloak != 0 {
        return Err(format!("error: bad_window: cloaked (cloak={cloak})"));
    }
    let (dwm_v, dwm_fb) = dwm_rect(hwnd);
    let mut rect_source = if dwm_fb { "gwr_fallback" } else { "dwm" }.to_string();
    let (gl, gt, gr, gb) = unsafe {
        let mut rc = RECT::default();
        if GetWindowRect(hwnd, &mut rc) != 0 {
            (rc.left, rc.top, rc.right, rc.bottom)
        } else {
            let e = crate::win32_text(GetLastError());
            return Err(format!(
                "error: bad_window: cannot read rect of hwnd={h}: {e}"
            ));
        }
    };
    let mut dwm = [dwm_v[0], dwm_v[1], dwm_v[2], dwm_v[3]];
    let (ww, wh) = (dwm[2] - dwm[0], dwm[3] - dwm[1]);
    if ww <= 0 || wh <= 0 {
        return Err("error: bad_window: zero-size".to_string());
    }
    unsafe {
        let mut aff: u32 = 0;
        if GetWindowDisplayAffinity(hwnd, &mut aff) != 0 && aff == WDA_EXCLUDEFROMCAPTURE {
            return Err("error: capture_unavailable: the window is excluded from capture (WDA_EXCLUDEFROMCAPTURE)".to_string());
        }
    }
    let mut img: Option<Img> = None;
    let mut print_flags: Option<u32> = None;
    let mut last_err = String::from("PrintWindow refused");
    for flags in [2u32, 0u32] {
        match printwindow_grab(hwnd, ww, wh, flags) {
            Ok(im) => {
                img = Some(im);
                print_flags = Some(flags);
                break;
            }
            Err(e) => {
                last_err = e;
            }
        }
    }
    if let Some(im) = img {
        if im.w != ww || im.h != wh {
            return Err(format!(
                "error: capture_unavailable: PrintWindow returned {}x{} for a {ww}x{wh} window",
                im.w, im.h
            ));
        }
        let verdict = detector_verdict(&im);
        if verdict.rejected {
            return Err(format!(
                "error: capture_unavailable: {}: {} frame rejected (blank/black frame)",
                detector_tag(&verdict),
                verdict.reason.clone().unwrap()
            ));
        }
        let mut stale = false;
        let mut sig = frame_sig(&im, &title, &dwm);
        let mut img = im;
        let prev = STALE.lock().unwrap().get(&h).cloned();
        if let Some(p) = prev {
            if p == sig {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let pf = print_flags.unwrap();
                match printwindow_grab(hwnd, ww, wh, pf) {
                    Ok(im2) => {
                        let v2 = detector_verdict(&im2);
                        if v2.rejected {
                            return Err(format!(
                                "error: capture_unavailable: {}: {} frame rejected (blank/black frame)",
                                detector_tag(&v2),
                                v2.reason.clone().unwrap()
                            ));
                        }
                        img = im2;
                        sig = frame_sig(&img, &title, &dwm);
                        stale = sig == p;
                    }
                    Err(e) => {
                        return Err(format!(
                            "error: capture_unavailable: stale retry failed: {e}"
                        ));
                    }
                }
            }
        }
        let (post_v, post_fb) = dwm_rect(hwnd);
        let post = [post_v[0], post_v[1], post_v[2], post_v[3]];
        let mut moved = false;
        let mut translation_note: Option<String> = None;
        match classify_rect_change(dwm, post) {
            RectChange::Unchanged => {}
            RectChange::Translated => {
                moved = true;
                let (pww, pwh) = (post[2] - post[0], post[3] - post[1]);
                let mut recaptured: Option<([i32; 4], String)> = None;
                if pww > 0 && pwh > 0 {
                    if let Ok((im2, pf2)) = unsafe { render_frame(hwnd, pww, pwh) } {
                        if im2.w == pww && im2.h == pwh && !detector_verdict(&im2).rejected {
                            img = im2;
                            print_flags = Some(pf2);
                            let mut rect = post;
                            let mut source =
                                if post_fb { "gwr_fallback" } else { "dwm" }.to_string();
                            let (last_v, last_fb) = dwm_rect(hwnd);
                            let last = [last_v[0], last_v[1], last_v[2], last_v[3]];
                            if last != post {
                                rect = reported_rect(post, last);
                                source = if last_fb { "gwr_fallback" } else { "dwm" }.to_string();
                            }
                            recaptured = Some((rect, source));
                        }
                    }
                }
                match recaptured {
                    Some((rect, source)) => {
                        dwm = rect;
                        rect_source = source;
                    }
                    None => {
                        let (rect, note) = translation_rect(dwm, post, false);
                        dwm = rect;
                        translation_note = note;
                    }
                }
            }
            RectChange::Resized => {
                moved = true;
                dwm = reported_rect(dwm, post);
                rect_source = if post_fb { "gwr_fallback" } else { "dwm" }.to_string();
            }
        }
        let rect_delta = [dwm[0] - gl, dwm[1] - gt, dwm[2] - gr, dwm[3] - gb];
        sig = frame_sig(&img, &title, &dwm);
        {
            let mut st = STALE.lock().unwrap();
            if st.len() > 64 {
                st.clear();
            }
            st.insert(h, sig);
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let pid = pid_of(hwnd);
        let _fg = unsafe { GetForegroundWindow() == hwnd };
        return Ok(CapOk {
            img,
            backend: "printwindow".to_string(),
            ms,
            print_flags: print_flags.map(|f| f as i64),
            rect_source,
            rect_delta,
            detector: detector_json(&verdict),
            blank_check: blank_check_json(verdict.max_channel),
            stale,
            moved_during_capture: moved,
            dwm,
            hwnd: h,
            pid,
            exe: exe_of_pid(pid),
            title,
            cloaked: cloak,
            note: Some(match translation_note {
                Some(n) => format!("{n} {MODE_B_NOTE}"),
                None => MODE_B_NOTE.to_string(),
            }),
        });
    }
    let fg = unsafe { GetForegroundWindow() == hwnd };
    if allow_crop_fallback && fg {
        match bitblt_grab(dwm[0], dwm[1], ww, wh) {
            Ok(crop) => {
                let verdict = detector_verdict(&crop);
                if verdict.rejected {
                    return Err(format!(
                        "error: capture_unavailable: {}: {} frame rejected (blank/black frame)",
                        detector_tag(&verdict),
                        verdict.reason.clone().unwrap()
                    ));
                }
                let (crop_v, crop_fb) = dwm_rect(hwnd);
                let crop_post = [crop_v[0], crop_v[1], crop_v[2], crop_v[3]];
                let crop_moved = crop_post != dwm;
                let crop_dwm = reported_rect(dwm, crop_post);
                let crop_delta = [
                    crop_dwm[0] - gl,
                    crop_dwm[1] - gt,
                    crop_dwm[2] - gr,
                    crop_dwm[3] - gb,
                ];
                let crop_source = if crop_fb { "gwr_fallback" } else { "dwm" }.to_string();
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                let pid = pid_of(hwnd);
                return Ok(CapOk {
                    img: crop,
                    backend: "desktop-crop:gdi".to_string(),
                    ms,
                    print_flags: None,
                    rect_source: crop_source,
                    rect_delta: crop_delta,
                    detector: detector_json(&verdict),
                    blank_check: blank_check_json(verdict.max_channel),
                    stale: false,
                    moved_during_capture: crop_moved,
                    dwm: crop_dwm,
                    hwnd: h,
                    pid,
                    exe: exe_of_pid(pid),
                    title,
                    cloaked: cloak,
                    note: Some(format!(
                        "PrintWindow refused ({last_err}); foreground-only desktop crop to dwm_rect. {MODE_B_NOTE}"
                    )),
                });
            }
            Err(e) => {
                return Err(format!(
                    "error: capture_unavailable: crop fallback failed: {e}"
                ));
            }
        }
    }
    let why = format!("PrintWindow refused ({last_err})");
    Err(format!(
        "error: capture_unavailable: PrintWindow refused ({why}); refusing an occlusion-prone desktop crop (the window is not foreground, or allow_crop_fallback is false). Capture without activation does not fall back to the foreground. {MODE_B_NOTE}"
    ))
}

fn role_of_class(cls: &str) -> &str {
    match cls {
        "Button" => "Button",
        "Edit" => "Edit",
        "Static" => "Text",
        "ComboBox" => "ComboBox",
        "ListBox" => "List",
        "SysListView32" => "List",
        "SysTreeView32" => "Tree",
        "SysTabControl32" => "Tab",
        "msctls_statusbar32" => "StatusBar",
        "msctls_progress32" => "ProgressBar",
        "msctls_trackbar32" => "Slider",
        "ScrollBar" => "ScrollBar",
        "" => "Unknown",
        c => c,
    }
}

type ElementPage = (Vec<Map<String, Value>>, bool, Option<usize>);

fn win32_element_list(hwnd: HWND, offset: usize) -> Result<ElementPage, String> {
    let kids = enum_child_windows(hwnd)?;
    let mut pager = crate::uia::Pager::new(offset, ELEMENT_CAP);
    let mut entries: Vec<Map<String, Value>> = Vec::new();
    for (i, h) in kids.iter().enumerate() {
        let name = unsafe { win_text(*h) };
        let cls = unsafe { win_class(*h) };
        let cid = unsafe { GetDlgCtrlID(*h) };
        let aid = if cid > 0 {
            cid.to_string()
        } else {
            String::new()
        };
        let inv = cls == "Button";
        match pager.observe(!name.is_empty() || !aid.is_empty() || inv) {
            crate::uia::PageAction::Collect => {
                let role = role_of_class(&cls).to_string();
                let rect = unsafe {
                    let mut rc = RECT::default();
                    if GetWindowRect(*h, &mut rc) != 0 {
                        vec![
                            rc.left as i64,
                            rc.top as i64,
                            (rc.right - rc.left) as i64,
                            (rc.bottom - rc.top) as i64,
                        ]
                    } else {
                        vec![0, 0, 0, 0]
                    }
                };
                let mut e = Map::new();
                e.insert("win".to_string(), Value::from(hwnd as i64));
                e.insert("role".to_string(), Value::from(role.clone()));
                e.insert("name".to_string(), Value::from(name.clone()));
                e.insert("auto_id".to_string(), Value::from(aid.clone()));
                e.insert("rect".to_string(), Value::from(rect.clone()));
                e.insert(
                    "offscreen".to_string(),
                    Value::from(rect[2] <= 0 || rect[3] <= 0),
                );
                e.insert("invokable".to_string(), Value::from(inv));
                e.insert("path".to_string(), Value::from(i.to_string()));
                e.insert("hwnd".to_string(), Value::from(*h as i64));
                e.insert(
                    "ref".to_string(),
                    Value::from(format!("{}.{i}", hwnd as i64)),
                );
                e.insert(
                    "fp".to_string(),
                    Value::from(element_fp(&role, &name, &aid, inv)),
                );
                entries.push(e);
            }
            crate::uia::PageAction::Stop => break,
            crate::uia::PageAction::Skip => {}
        }
    }
    let truncated = pager.truncated;
    let next_offset = pager.next_offset();
    Ok((entries, truncated, next_offset))
}

pub struct ElementModel {
    pub entries: Vec<Map<String, Value>>,
    pub truncated: bool,
    pub backend: String,
    pub detail: Map<String, Value>,
    pub next_offset: Option<usize>,
}

/// Stable per-element identity fingerprint (gap 1): FNV-1a 64 over
/// role/name/auto_id/invokable, rendered as 16 lowercase hex digits.
/// Deliberately NOT DefaultHasher (random seed per process): a fingerprint
/// captured in one `elements` snapshot stays comparable against a later
/// re-walk. Geometry (rect) is NOT part of the identity: a move, resize,
/// or scroll must not invalidate the fingerprint, so `expect_fp` refuses
/// only a genuinely changed control, never one that merely moved. The
/// path is the enumeration address, never part of the identity either.
/// Element text cap (P4c): at most this many chars per entry, with a
/// `text_truncated` flag when the source was longer. Applied only when
/// `detail:true` requests text; identity (`fp`) never covers text.
pub const ELEMENT_TEXT_CAP: usize = 4000;

/// Cap element text at `ELEMENT_TEXT_CAP` chars, reporting truncation.
/// Char-based (matches the documented "4000 chars" contract).
pub fn cap_text(s: &str) -> (String, bool) {
    if s.chars().count() <= ELEMENT_TEXT_CAP {
        (s.to_string(), false)
    } else {
        (s.chars().take(ELEMENT_TEXT_CAP).collect(), true)
    }
}

pub fn element_fp(role: &str, name: &str, auto_id: &str, invokable: bool) -> String {
    let s = format!("{role}\0{name}\0{auto_id}\0{invokable}");
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Prefix of every fingerprint refusal: the control the caller verified is not
/// the control the call reached. The pre-activation gate and the activation
/// itself share it, so an agent reads one code for one condition.
pub const STALE_REF: &str = "stale_ref";

/// The act-time refusal: the element resolved for this call is no longer the
/// control whose fingerprint was verified. Names both fingerprints so the
/// retry costs one call.
pub fn stale_ref_text(path: &str, want: &str, now: &str) -> String {
    format!(
        "{STALE_REF}: element at path {} is no longer the control whose fingerprint was verified \
         (expected fp {want}, now {now}). Take a fresh `elements` snapshot and retry.",
        crate::py_repr_str(path)
    )
}

/// The identity fingerprint of one Win32 child, read from the window itself:
/// the same four properties `win32_element_list` hashes. Read at click time on
/// the control about to be clicked, so the Win32 fallback cannot click a
/// replacement that took the old path. The handle crosses as the plain integer
/// the activation paths already use.
pub fn win32_identity_fp(hwnd: i64) -> String {
    let h = hwnd as HWND;
    let name = unsafe { win_text(h) };
    let cls = unsafe { win_class(h) };
    let cid = unsafe { GetDlgCtrlID(h) };
    let aid = if cid > 0 {
        cid.to_string()
    } else {
        String::new()
    };
    let inv = cls == "Button";
    element_fp(role_of_class(&cls), &name, &aid, inv)
}

pub fn element_model(hwnd: HWND, max_depth: i64, offset: usize) -> ElementModel {
    let uia_avail = crate::uia::uia_available();
    let mut uia_error: Option<String> = None;
    match crate::uia::uia_element_list(hwnd, max_depth, offset) {
        Ok((entries, trunc, dcapped, next)) => {
            if !entries.is_empty() {
                let mut detail = Map::new();
                detail.insert("uia_available".to_string(), Value::from(true));
                detail.insert("uia_error".to_string(), Value::Null);
                detail.insert("fallback".to_string(), Value::Null);
                detail.insert("empty".to_string(), Value::from(false));
                detail.insert("depth_capped".to_string(), Value::from(dcapped));
                detail.insert("depth_limit".to_string(), Value::from(max_depth));
                return ElementModel {
                    entries,
                    truncated: trunc,
                    backend: "uia".to_string(),
                    detail,
                    next_offset: next,
                };
            }
            let _ = dcapped;
        }
        Err(e) => {
            uia_error = Some(e);
        }
    }
    let mut detail = Map::new();
    detail.insert("uia_available".to_string(), Value::from(uia_avail));
    detail.insert(
        "uia_error".to_string(),
        uia_error.clone().map(Value::from).unwrap_or(Value::Null),
    );
    detail.insert("fallback".to_string(), Value::from("win32"));
    detail.insert("depth_capped".to_string(), Value::from(false));
    detail.insert("depth_limit".to_string(), Value::from(max_depth));
    match win32_element_list(hwnd, offset) {
        Err(e) => {
            detail.insert("empty".to_string(), Value::from(true));
            detail.insert(
                "empty_reason".to_string(),
                Value::from(format!("win32 fallback failed: {e}")),
            );
            ElementModel {
                entries: vec![],
                truncated: false,
                backend: "win32".to_string(),
                detail,
                next_offset: None,
            }
        }
        Ok((entries, trunc, next)) => {
            if entries.is_empty() {
                let reason = format!(
                    "UIA yielded no entries{} and the win32 fallback found no children",
                    uia_error.map(|s| format!(" ({s})")).unwrap_or_default()
                );
                detail.insert("empty".to_string(), Value::from(true));
                detail.insert("empty_reason".to_string(), Value::from(reason));
                ElementModel {
                    entries: vec![],
                    truncated: trunc,
                    backend: "win32".to_string(),
                    detail,
                    next_offset: next,
                }
            } else {
                detail.insert("empty".to_string(), Value::from(false));
                ElementModel {
                    entries,
                    truncated: trunc,
                    backend: "win32".to_string(),
                    detail,
                    next_offset: next,
                }
            }
        }
    }
}

/// Compact default per-entry shape for the standalone `elements` tool:
/// everything except the raw `win` (and `hwnd` on the Win32 fallback)
/// identifiers, which `detail=true` retains.
pub const COMPACT_ELEMENT_KEYS: [&str; 9] = [
    "ref",
    "role",
    "name",
    "auto_id",
    "rect",
    "offscreen",
    "invokable",
    "path",
    "fp",
];

/// Project one full walk entry to the compact default shape.
pub fn compact_entry(e: &Map<String, Value>) -> Map<String, Value> {
    let mut m = Map::new();
    for k in COMPACT_ELEMENT_KEYS {
        if let Some(v) = e.get(k) {
            m.insert(k.to_string(), v.clone());
        }
    }
    m
}

/// Assemble the top-level elements object. `elements_block` (screenshot
/// path) and the standalone `elements` tool both build through here, so
/// the two stay the same top-level object by construction; only the
/// entries slice differs (full walk entries vs compact projections).
/// `next_offset` is the paging continuation (integer) or null when no
/// more controls remain past the returned page.
pub fn elements_object(
    backend: &str,
    truncated: bool,
    entries: Vec<Value>,
    detail: &Map<String, Value>,
    next_offset: Option<usize>,
) -> Map<String, Value> {
    let mut o = Map::new();
    o.insert("backend".to_string(), Value::from(backend));
    o.insert("truncated".to_string(), Value::from(truncated));
    o.insert("rect_space".to_string(), Value::from("screen"));
    o.insert("elements".to_string(), Value::from(entries));
    o.insert(
        "uia_available".to_string(),
        detail.get("uia_available").cloned().unwrap_or(Value::Null),
    );
    o.insert(
        "uia_error".to_string(),
        detail.get("uia_error").cloned().unwrap_or(Value::Null),
    );
    o.insert(
        "fallback".to_string(),
        detail.get("fallback").cloned().unwrap_or(Value::Null),
    );
    o.insert(
        "empty".to_string(),
        detail.get("empty").cloned().unwrap_or(Value::from(false)),
    );
    if detail.get("empty") == Some(&Value::from(true)) {
        if let Some(r) = detail.get("empty_reason") {
            o.insert("empty_reason".to_string(), r.clone());
        }
    }
    o.insert(
        "depth_capped".to_string(),
        detail
            .get("depth_capped")
            .cloned()
            .unwrap_or(Value::from(false)),
    );
    o.insert(
        "depth_limit".to_string(),
        detail.get("depth_limit").cloned().unwrap_or(Value::from(8)),
    );
    o.insert(
        "next_offset".to_string(),
        next_offset
            .map(|n| Value::from(n as i64))
            .unwrap_or(Value::Null),
    );
    o
}

pub fn elements_block(hwnd: HWND) -> String {
    let m = element_model(hwnd, UIA_DEPTH_LIMIT, 0);
    let entries: Vec<Value> = m.entries.into_iter().map(Value::Object).collect();
    let o = elements_object(&m.backend, m.truncated, entries, &m.detail, m.next_offset);
    crate::py_dumps_compact(&Value::Object(o))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_win32_entry() -> Map<String, Value> {
        let mut e = Map::new();
        e.insert("win".to_string(), Value::from(199210));
        e.insert("role".to_string(), Value::from("Button"));
        e.insert("name".to_string(), Value::from("OK"));
        e.insert("auto_id".to_string(), Value::from("2"));
        e.insert(
            "rect".to_string(),
            Value::from(vec![
                Value::from(10),
                Value::from(20),
                Value::from(80),
                Value::from(24),
            ]),
        );
        e.insert("invokable".to_string(), Value::from(true));
        e.insert("offscreen".to_string(), Value::from(false));
        e.insert("path".to_string(), Value::from("3"));
        e.insert("hwnd".to_string(), Value::from(199340));
        e.insert("ref".to_string(), Value::from("199210.3"));
        e.insert(
            "fp".to_string(),
            Value::from(element_fp("Button", "OK", "2", true)),
        );
        e
    }

    fn test_detail(empty: bool) -> Map<String, Value> {
        let mut d = Map::new();
        d.insert("uia_available".to_string(), Value::from(false));
        d.insert(
            "uia_error".to_string(),
            Value::from("UIA unavailable in test"),
        );
        d.insert("fallback".to_string(), Value::from("win32"));
        d.insert("empty".to_string(), Value::from(empty));
        if empty {
            d.insert(
                "empty_reason".to_string(),
                Value::from("UIA yielded no entries and the win32 fallback found no children"),
            );
        }
        d.insert("depth_capped".to_string(), Value::from(false));
        d.insert("depth_limit".to_string(), Value::from(8));
        d
    }

    #[test]
    fn compact_entry_carries_documented_keys_only() {
        let full = full_win32_entry();
        let c = compact_entry(&full);
        let keys: Vec<&str> = c.keys().map(|k| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "ref",
                "role",
                "name",
                "auto_id",
                "rect",
                "offscreen",
                "invokable",
                "path",
                "fp"
            ],
            "compact default must carry exactly the documented keys, got: {keys:?}"
        );
        assert!(!c.contains_key("win"), "compact must omit win");
        assert!(!c.contains_key("hwnd"), "compact must omit hwnd");
    }

    #[test]
    fn element_fp_stable_and_identity_sensitive() {
        let a = element_fp("Button", "OK", "2", true);
        assert_eq!(a.len(), 16, "fp is 16 hex chars, got: {a}");
        assert_eq!(a, element_fp("Button", "OK", "2", true));
        assert_ne!(
            a,
            element_fp("Button", "Cancel", "2", true),
            "name is identity"
        );
        assert_ne!(a, element_fp("Edit", "OK", "2", true), "role is identity");
        assert_ne!(
            a,
            element_fp("Button", "OK", "3", true),
            "auto_id is identity"
        );
        assert_ne!(
            a,
            element_fp("Button", "OK", "2", false),
            "invokable is identity"
        );
    }

    #[test]
    fn element_fp_ignores_geometry() {
        let mut e = full_win32_entry();
        let ident = |e: &Map<String, Value>| {
            (
                e.get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                e.get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                e.get("auto_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                e.get("invokable")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                e.get("rect").cloned().unwrap_or(Value::Null),
            )
        };
        let (role, name, aid, inv, rect_before) = ident(&e);
        let fp1 = element_fp(&role, &name, &aid, inv);
        assert_eq!(
            e.get("fp").and_then(|v| v.as_str()),
            Some(fp1.as_str()),
            "the walk must store exactly what element_fp returns"
        );
        e.insert(
            "rect".to_string(),
            Value::from(vec![
                Value::from(510),
                Value::from(320),
                Value::from(80),
                Value::from(24),
            ]),
        );
        let (role2, name2, aid2, inv2, rect_after) = ident(&e);
        assert_ne!(
            rect_before, rect_after,
            "probe needs two different rects, got: {rect_before} vs {rect_after}"
        );
        assert_eq!(
            (role.as_str(), name.as_str(), aid.as_str(), inv),
            (role2.as_str(), name2.as_str(), aid2.as_str(), inv2)
        );
        let fp2 = element_fp(&role2, &name2, &aid2, inv2);
        assert_eq!(
            fp1, fp2,
            "same role/name/auto_id/invokable at a new rect must hash equal"
        );
        e.insert("name".to_string(), Value::from("Cancel"));
        let (role3, name3, aid3, inv3, _) = ident(&e);
        let fp3 = element_fp(&role3, &name3, &aid3, inv3);
        assert_ne!(
            fp1, fp3,
            "a changed name must re-hash even at the same rect"
        );
    }

    #[test]
    fn element_fp_changes_with_name() {
        assert_ne!(
            element_fp("Button", "OK", "2", true),
            element_fp("Button", "Cancel", "2", true),
            "a changed name must re-hash"
        );
    }

    #[test]
    fn rect_change_classifies_translation_and_resize() {
        let pre = [100, 200, 900, 800];
        assert_eq!(classify_rect_change(pre, pre), RectChange::Unchanged);
        assert_eq!(
            classify_rect_change(pre, [140, 260, 940, 860]),
            RectChange::Translated
        );
        assert_eq!(
            classify_rect_change(pre, [100, 200, 1000, 800]),
            RectChange::Resized
        );
        assert_eq!(
            classify_rect_change(pre, [100, 200, 900, 760]),
            RectChange::Resized
        );
    }

    #[test]
    fn reported_rect_is_the_post_capture_rect_when_the_window_moved() {
        let pre = [100, 200, 900, 800];
        let translated = [140, 260, 940, 860];
        assert_eq!(reported_rect(pre, translated), translated);
        assert_ne!(
            reported_rect(pre, translated),
            pre,
            "a stale pre-capture origin must never be reported as current"
        );
        let resized = [100, 200, 1100, 900];
        assert_eq!(reported_rect(pre, resized), resized);
        assert_eq!(
            reported_rect(pre, pre),
            pre,
            "an unmoved window reports its one rect"
        );
    }

    #[test]
    fn translation_rect_keeps_the_pre_capture_origin_when_the_recapture_failed() {
        let pre = [100, 200, 900, 800];
        let post = [140, 260, 940, 860];
        assert_eq!(translation_rect(pre, post, true), (post, None));
        let (rect, note) = translation_rect(pre, post, false);
        assert_eq!(
            rect, pre,
            "a failed recapture must not report an origin the image predates"
        );
        let note = note.expect("a failed recapture must say so");
        assert!(note.contains("recapture failed"), "{note}");
        assert!(note.contains("PRE-capture rect"), "{note}");
        assert!(note.contains("[100,200,900,800]"), "{note}");
        assert!(note.contains("[140,260,940,860]"), "{note}");
    }

    #[test]
    fn live_stationary_capture_reports_the_current_rect() {
        if std::env::var("PUPPET_LIVE_DESKTOP").as_deref() != Ok("1") {
            println!(
                "SKIP live_stationary_capture_reports_the_current_rect: set PUPPET_LIVE_DESKTOP=1 to run (needs an interactive Windows session)"
            );
            return;
        }
        let _g = crate::fixture::desktop();
        let f = crate::fixture::Fixture::new();
        let cap = unsafe { capture_window(f.hwnd(), false) }.expect("fixture captures");
        assert!(
            !cap.moved_during_capture,
            "a stationary window must not be flagged moved_during_capture"
        );
        let (now, _) = dwm_rect(f.hwnd());
        assert_eq!(
            cap.dwm.to_vec(),
            now,
            "the reported rect must be the post-capture rect"
        );
    }

    #[test]
    fn element_fp_changes_with_invokable() {
        assert_ne!(
            element_fp("Button", "OK", "2", true),
            element_fp("Button", "OK", "2", false),
            "a changed invokable must re-hash"
        );
    }

    #[test]
    fn cap_text_bounds_and_flag() {
        let (s, t) = cap_text("");
        assert_eq!((s.as_str(), t), ("", false));
        let (s, t) = cap_text("hello");
        assert_eq!((s.as_str(), t), ("hello", false));
        let exact: String = "x".repeat(ELEMENT_TEXT_CAP);
        let (s, t) = cap_text(&exact);
        assert_eq!((s, t), (exact, false));
        let long: String = "y".repeat(ELEMENT_TEXT_CAP + 1);
        let (s, t) = cap_text(&long);
        assert_eq!(s.chars().count(), ELEMENT_TEXT_CAP);
        assert!(t, "over-cap text must set the truncated flag");
    }

    #[test]
    fn element_fp_excludes_text() {
        let mut e = full_win32_entry();
        let before = element_fp(
            e.get("role").and_then(|v| v.as_str()).unwrap_or(""),
            e.get("name").and_then(|v| v.as_str()).unwrap_or(""),
            e.get("auto_id").and_then(|v| v.as_str()).unwrap_or(""),
            e.get("invokable")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        );
        assert_eq!(
            e.get("fp").and_then(|v| v.as_str()),
            Some(before.as_str()),
            "fixture entry carries the production fp"
        );
        e.insert("text".to_string(), Value::from("hello"));
        e.insert("text_truncated".to_string(), Value::from(false));
        let after = element_fp(
            e.get("role").and_then(|v| v.as_str()).unwrap_or(""),
            e.get("name").and_then(|v| v.as_str()).unwrap_or(""),
            e.get("auto_id").and_then(|v| v.as_str()).unwrap_or(""),
            e.get("invokable")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        );
        assert_eq!(before, after, "text must not participate in fp");
    }

    #[test]
    fn element_fp_same_control_across_rewalks() {
        let _g = crate::fixture::desktop();
        let f = crate::fixture::Fixture::new();
        let first = element_model(f.hwnd(), UIA_DEPTH_LIMIT, 0);
        let second = element_model(f.hwnd(), UIA_DEPTH_LIMIT, 0);
        let fp_of = |m: &ElementModel| {
            m.entries
                .iter()
                .find(|e| {
                    e.get("role").and_then(|v| v.as_str()) == Some("Button")
                        && e.get("name").and_then(|v| v.as_str()) == Some("OK")
                })
                .and_then(|e| e.get("fp").cloned())
        };
        let a = fp_of(&first).expect("first walk lists the OK button");
        let b = fp_of(&second).expect("second walk lists the OK button");
        assert_eq!(a, b, "same control must carry the same fp across walks");
    }

    #[test]
    fn detail_entry_retains_win_and_hwnd() {
        let full = full_win32_entry();
        assert_eq!(full.get("win").and_then(|v| v.as_i64()), Some(199210));
        assert_eq!(full.get("hwnd").and_then(|v| v.as_i64()), Some(199340));
        assert_eq!(
            full.get("ref").and_then(|v| v.as_str()),
            Some("199210.3"),
            "detail keeps ref as a per-enumeration correlation token"
        );
    }

    #[test]
    fn top_level_key_set_matches_screenshot_block_shape() {
        let entries = vec![Value::Object(compact_entry(&full_win32_entry()))];
        let o = elements_object("win32", false, entries, &test_detail(false), None);
        let keys: Vec<&str> = o.keys().map(|k| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "backend",
                "truncated",
                "rect_space",
                "elements",
                "uia_available",
                "uia_error",
                "fallback",
                "empty",
                "depth_capped",
                "depth_limit",
                "next_offset",
            ],
            "top-level keys must match the documented elements object, got: {keys:?}"
        );
        assert!(
            !o.contains_key("empty_reason"),
            "empty_reason appears only when empty:true"
        );
        let o_empty = elements_object("win32", false, vec![], &test_detail(true), None);
        assert!(
            o_empty.contains_key("empty_reason"),
            "empty:true must carry empty_reason"
        );
        assert_eq!(
            o_empty.get("next_offset"),
            Some(&Value::Null),
            "no next page without entries or offset"
        );
    }

    #[test]
    fn next_offset_round_trips_through_envelope() {
        let entries = vec![Value::Object(compact_entry(&full_win32_entry()))];
        let o = elements_object("win32", true, entries, &test_detail(false), Some(200));
        assert_eq!(
            o.get("next_offset"),
            Some(&Value::from(200)),
            "next_offset carries the offset of the next page"
        );
        assert_eq!(
            o.get("truncated"),
            Some(&Value::from(true)),
            "a capped page stays truncated"
        );
    }

    #[test]
    fn win32_offset_skips_leading_controls() {
        let _g = crate::fixture::desktop();
        let f = crate::fixture::Fixture::new();
        let (all, _, next_all) =
            win32_element_list(f.hwnd(), 0).expect("fixture window enumerates");
        assert_eq!(all.len(), 3, "fixture exposes three accepted controls");
        assert_eq!(next_all, None);
        let (page, trunc, next) =
            win32_element_list(f.hwnd(), 1).expect("fixture window enumerates");
        assert_eq!(page.len(), 2, "offset 1 skips the first control");
        assert!(!trunc);
        assert_eq!(next, None, "short tail has no next page");
        assert_eq!(page.len(), 2, "offset 1 skips the first control");
        assert!(!trunc);
        assert_eq!(next, None, "short tail has no next page");
        let paths_of = |es: &[Map<String, Value>]| {
            es.iter()
                .map(|e| {
                    e.get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            paths_of(&page),
            paths_of(&all[1..]),
            "page 2 must be the tail of page 1"
        );
        let (empty, _, next_empty) =
            win32_element_list(f.hwnd(), 99).expect("fixture window enumerates");
        assert!(empty.is_empty());
        assert_eq!(next_empty, None);
    }

    #[test]
    fn element_fp_is_identity_and_never_state() {
        let fp_of = |e: &Map<String, Value>| {
            element_fp(
                e["role"].as_str().expect("role"),
                e["name"].as_str().expect("name"),
                e["auto_id"].as_str().expect("auto_id"),
                e["invokable"].as_bool().expect("invokable"),
            )
        };
        let entry = |rect: [i64; 4], path: &str, text: &str| {
            let mut e = Map::new();
            e.insert("role".to_string(), Value::from("Button"));
            e.insert("name".to_string(), Value::from("OK"));
            e.insert("auto_id".to_string(), Value::from("101"));
            e.insert("invokable".to_string(), Value::from(true));
            e.insert(
                "rect".to_string(),
                Value::from(rect.iter().map(|v| Value::from(*v)).collect::<Vec<_>>()),
            );
            e.insert("path".to_string(), Value::from(path));
            e.insert("text".to_string(), Value::from(text));
            e
        };
        let base = fp_of(&entry([10, 10, 80, 24], "0/1", ""));
        assert_eq!(
            fp_of(&entry([400, 900, 4, 4], "0/1", "")),
            base,
            "rect must never participate in fp"
        );
        assert_eq!(
            fp_of(&entry([10, 10, 80, 24], "0/7", "")),
            base,
            "path must never participate in fp"
        );
        assert_eq!(
            fp_of(&entry([10, 10, 80, 24], "0/1", "the button's text changed")),
            base,
            "text must never participate in fp"
        );
        for (role, name, auto_id, invokable) in [
            ("Edit", "OK", "101", true),
            ("Button", "Cancel", "101", true),
            ("Button", "OK", "102", true),
            ("Button", "OK", "101", false),
        ] {
            assert_ne!(
                element_fp(role, name, auto_id, invokable),
                base,
                "({role}, {name}, {auto_id}, {invokable}) must be a different identity"
            );
        }
        let walk = include_str!("uia.rs");
        assert!(
            walk.contains("element_fp(&role, &name, &aid, inv)"),
            "the element walk must hash exactly role/name/auto_id/invokable"
        );
    }
}
