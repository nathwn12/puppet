//! Window capture chain (Mode A + Mode C; B deferred) + element model.

use crate::cap::{
    bitblt_grab, detector_json, detector_tag, detector_verdict, printwindow_grab, Img,
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
    pub stale: bool,
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

/// # Safety
/// `hwnd` must be a live window handle; it is passed to the Win32 window APIs as a raw pointer.
pub unsafe fn capture_window(hwnd: HWND, allow_crop_fallback: bool) -> Result<CapOk, String> {
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
        return Err("error: bad_window: minimized (refusing to restore)".to_string());
    }
    let cloak = cloaked(hwnd);
    if cloak != 0 {
        return Err(format!("error: bad_window: cloaked (cloak={cloak})"));
    }
    let (dwm_v, dwm_fb) = dwm_rect(hwnd);
    let rect_source = if dwm_fb { "gwr_fallback" } else { "dwm" }.to_string();
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
    let dwm = [dwm_v[0], dwm_v[1], dwm_v[2], dwm_v[3]];
    let rect_delta = [dwm[0] - gl, dwm[1] - gt, dwm[2] - gr, dwm[3] - gb];
    let (ww, wh) = (dwm[2] - dwm[0], dwm[3] - dwm[1]);
    if ww <= 0 || wh <= 0 {
        return Err("error: bad_window: zero-size".to_string());
    }
    unsafe {
        let mut aff: u32 = 0;
        if GetWindowDisplayAffinity(hwnd, &mut aff) != 0 && aff == WDA_EXCLUDEFROMCAPTURE {
            return Err("error: capture_failed: excluded_from_capture".to_string());
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
                "error: capture_failed: PrintWindow returned {}x{} for a {ww}x{wh} window",
                im.w, im.h
            ));
        }
        let verdict = detector_verdict(&im);
        if verdict.rejected {
            return Err(format!(
                "error: capture_failed: {}: {} frame rejected",
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
                                "error: capture_failed: {}: {} frame rejected",
                                detector_tag(&v2),
                                v2.reason.clone().unwrap()
                            ));
                        }
                        img = im2;
                        sig = frame_sig(&img, &title, &dwm);
                        stale = sig == p;
                    }
                    Err(e) => {
                        return Err(format!("error: capture_failed: stale retry failed: {e}"));
                    }
                }
            }
        }
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
            stale,
            dwm,
            hwnd: h,
            pid,
            exe: exe_of_pid(pid),
            title,
            cloaked: cloak,
            note: Some(MODE_B_NOTE.to_string()),
        });
    }
    let fg = unsafe { GetForegroundWindow() == hwnd };
    if allow_crop_fallback && fg {
        match bitblt_grab(dwm[0], dwm[1], ww, wh) {
            Ok(crop) => {
                let verdict = detector_verdict(&crop);
                if verdict.rejected {
                    return Err(format!(
                        "error: capture_failed: {}: {} frame rejected",
                        detector_tag(&verdict),
                        verdict.reason.clone().unwrap()
                    ));
                }
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                let pid = pid_of(hwnd);
                return Ok(CapOk {
                    img: crop,
                    backend: "desktop-crop:gdi".to_string(),
                    ms,
                    print_flags: None,
                    rect_source,
                    rect_delta,
                    detector: detector_json(&verdict),
                    stale: false,
                    dwm,
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
                return Err(format!("error: capture_failed: crop fallback failed: {e}"));
            }
        }
    }
    let why = format!("PrintWindow refused ({last_err})");
    Err(format!(
        "error: capture_failed: window capture refused PrintWindow ({why}); refusing an occlusion-prone crop (pass allow_crop_fallback=true on the foreground window). {MODE_B_NOTE}"
    ))
}

// ---------------------------------------------------------------- element model (Win32 fallback)

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
        c if c.is_empty() => "Unknown",
        c => c,
    }
}

fn win32_element_list(hwnd: HWND) -> Result<(Vec<Map<String, Value>>, bool), String> {
    let kids = enum_child_windows(hwnd)?;
    let mut entries: Vec<Map<String, Value>> = Vec::new();
    let mut truncated = false;
    for (i, h) in kids.iter().enumerate() {
        if entries.len() >= ELEMENT_CAP {
            truncated = true;
            break;
        }
        let name = unsafe { win_text(*h) };
        let cls = unsafe { win_class(*h) };
        let cid = unsafe { GetDlgCtrlID(*h) };
        let aid = if cid > 0 {
            cid.to_string()
        } else {
            String::new()
        };
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
        let inv = cls == "Button";
        if !name.is_empty() || !aid.is_empty() || inv {
            let role = role_of_class(&cls).to_string();
            let mut e = Map::new();
            e.insert("win".to_string(), Value::from(hwnd as i64));
            e.insert("role".to_string(), Value::from(role.clone()));
            e.insert("name".to_string(), Value::from(name.clone()));
            e.insert("auto_id".to_string(), Value::from(aid.clone()));
            e.insert("rect".to_string(), Value::from(rect.clone()));
            e.insert("invokable".to_string(), Value::from(inv));
            e.insert("path".to_string(), Value::from(i.to_string()));
            e.insert("hwnd".to_string(), Value::from(*h as i64));
            e.insert(
                "ref".to_string(),
                Value::from(format!("{}.{i}", hwnd as i64)),
            );
            e.insert(
                "fp".to_string(),
                Value::from(element_fp(&role, &name, &aid, &rect, inv)),
            );
            entries.push(e);
        }
    }
    Ok((entries, truncated))
}

pub struct ElementModel {
    pub entries: Vec<Map<String, Value>>,
    pub truncated: bool,
    pub backend: String,
    pub detail: Map<String, Value>,
}

/// Stable per-element identity fingerprint (gap 1): FNV-1a 64 over
/// role/name/auto_id/rect/invokable, rendered as 16 lowercase hex digits.
/// Deliberately NOT DefaultHasher (random seed per process): a fingerprint
/// captured in one `elements` snapshot stays comparable against a later
/// re-walk. The path is the address, never part of the identity.
pub fn element_fp(role: &str, name: &str, auto_id: &str, rect: &[i64], invokable: bool) -> String {
    let r0 = rect.first().copied().unwrap_or(0);
    let r1 = rect.get(1).copied().unwrap_or(0);
    let r2 = rect.get(2).copied().unwrap_or(0);
    let r3 = rect.get(3).copied().unwrap_or(0);
    let s = format!("{role}\0{name}\0{auto_id}\0{r0},{r1},{r2},{r3}\0{invokable}");
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

pub fn element_model(hwnd: HWND) -> ElementModel {
    // puppet.py:1253-1331: UIA first (wins iff >=1 entry), else the Win32
    // EnumChildWindows fallback. detail carries uia_available (COM loadable),
    // uia_error (why UIA failed, else null), fallback (which fallback ran,
    // else null), empty + empty_reason, depth_capped + depth_limit.
    let uia_avail = crate::uia::uia_available();
    let mut uia_error: Option<String> = None;
    match crate::uia::uia_element_list(hwnd) {
        Ok((entries, trunc, dcapped)) => {
            if !entries.is_empty() {
                let mut detail = Map::new();
                detail.insert("uia_available".to_string(), Value::from(true));
                detail.insert("uia_error".to_string(), Value::Null);
                detail.insert("fallback".to_string(), Value::Null);
                detail.insert("empty".to_string(), Value::from(false));
                detail.insert("depth_capped".to_string(), Value::from(dcapped));
                detail.insert("depth_limit".to_string(), Value::from(UIA_DEPTH_LIMIT));
                return ElementModel {
                    entries,
                    truncated: trunc,
                    backend: "uia".to_string(),
                    detail,
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
    detail.insert("depth_limit".to_string(), Value::from(UIA_DEPTH_LIMIT));
    match win32_element_list(hwnd) {
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
            }
        }
        Ok((entries, trunc)) => {
            if entries.is_empty() {
                let reason = format!(
                    "UIA yielded no entries{} and the win32 fallback found no children",
                    uia_error.map(|s| format!(" ({s})")).unwrap_or_default()
                );
                detail.insert("empty".to_string(), Value::from(true));
                detail.insert("empty_reason".to_string(), Value::from(reason));
            } else {
                detail.insert("empty".to_string(), Value::from(false));
            }
            ElementModel {
                entries,
                truncated: trunc,
                backend: "win32".to_string(),
                detail,
            }
        }
    }
}

/// Compact default per-entry shape for the standalone `elements` tool:
/// everything except the raw `win` (and `hwnd` on the Win32 fallback)
/// identifiers, which `detail=true` retains.
pub const COMPACT_ELEMENT_KEYS: [&str; 8] = [
    "ref",
    "role",
    "name",
    "auto_id",
    "rect",
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
pub fn elements_object(
    backend: &str,
    truncated: bool,
    entries: Vec<Value>,
    detail: &Map<String, Value>,
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
    o
}

pub fn elements_block(hwnd: HWND) -> String {
    let m = element_model(hwnd);
    let entries: Vec<Value> = m.entries.into_iter().map(Value::Object).collect();
    let o = elements_object(&m.backend, m.truncated, entries, &m.detail);
    crate::py_dumps_compact(&Value::Object(o))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full Win32-fallback-style entry, keyed exactly as
    /// `win32_element_list` builds one (`win` + `hwnd` identifiers plus
    /// the shared `ref`/role/name/auto_id/rect/invokable/path fields).
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
        e.insert("path".to_string(), Value::from("3"));
        e.insert("hwnd".to_string(), Value::from(199340));
        e.insert("ref".to_string(), Value::from("199210.3"));
        e.insert(
            "fp".to_string(),
            Value::from(element_fp("Button", "OK", "2", &[10, 20, 80, 24], true)),
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
        // The stale-ref gate stands on this: same identity hashes equal
        // (deterministic across processes), any identity change re-hashes.
        let a = element_fp("Button", "OK", "2", &[10, 20, 80, 24], true);
        assert_eq!(a.len(), 16, "fp is 16 hex chars, got: {a}");
        assert_eq!(a, element_fp("Button", "OK", "2", &[10, 20, 80, 24], true));
        assert_ne!(
            a,
            element_fp("Button", "Cancel", "2", &[10, 20, 80, 24], true)
        );
        assert_ne!(a, element_fp("Button", "OK", "2", &[11, 20, 80, 24], true));
        assert_ne!(a, element_fp("Edit", "OK", "2", &[10, 20, 80, 24], true));
        assert_ne!(a, element_fp("Button", "OK", "3", &[10, 20, 80, 24], true));
    }

    #[test]
    fn detail_entry_retains_win_and_hwnd() {
        // The detail path returns the walk entry unprojected, so the raw
        // `win`/`hwnd` identifiers stay available for correlation.
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
        // Both the screenshot elements block (`elements_block`) and the
        // standalone `elements` tool assemble through `elements_object`,
        // so this key set is the contract for both.
        let entries = vec![Value::Object(compact_entry(&full_win32_entry()))];
        let o = elements_object("win32", false, entries, &test_detail(false));
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
            ],
            "top-level keys must match the documented elements object, got: {keys:?}"
        );
        assert!(
            !o.contains_key("empty_reason"),
            "empty_reason appears only when empty:true"
        );
        let o_empty = elements_object("win32", false, vec![], &test_detail(true));
        assert!(
            o_empty.contains_key("empty_reason"),
            "empty:true must carry empty_reason"
        );
    }
}
