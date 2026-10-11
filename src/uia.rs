//! UIA element backend (puppet.py:1097-1201, 1391-1439).
//!
//! RawViewWalker walk mirroring `uiautomation` (which binds
//! `ViewWalker = IUIAutomation.RawViewWalker`), plus Invoke/SelectionItem/
//! Toggle activation. Enumeration is read-only (no focus/input) and is the
//! parity surface. Activation paths are implemented for live use and marked
//! `unconfirmed: activation needs owner permission` - never exercised by the
//! headless parity harness, which uses `dry_run` resolution only.
//!
//! Raw COM notes (windows-sys has no UIA interface definitions, only GUID and
//! pattern-ID constants): every call below dispatches through an explicit
//! vtable slot. Slot indices are taken verbatim from the `windows` 0.58
//! generated `_Vtbl` structs (the crate this file migrates from):
//! IUnknown = QueryInterface(0), AddRef(1), Release(2);
//! IUIAutomation: ElementFromHandle(6), RawViewWalker(16);
//! IUIAutomationTreeWalker: GetParentElement(3), GetFirstChildElement(4),
//! GetNextSiblingElement(6); IUIAutomationElement: GetCurrentPattern(16),
//! CurrentControlType(21), CurrentName(23), CurrentAutomationId(29),
//! CurrentBoundingRectangle(43); Invoke/Select/Toggle patterns: method(3).
//! IIDs are the `define_interface!` GUIDs from the same source.
//! Error text uses `crate::hr_text`, which replicates `windows::core::Error`'s
//! Display (`"{message} (0x{HR:08X})"`) via the identical FormatMessage call.

use crate::{ELEMENT_CAP, UIA_DEPTH_LIMIT};
use serde_json::{Map, Value};
use std::ffi::c_void;
use windows_sys::core::{GUID, HRESULT};
use windows_sys::Win32::Foundation::{SysFreeString, HWND, RECT};
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows_sys::Win32::UI::Accessibility::{
    CUIAutomation, UIA_InvokePatternId, UIA_SelectionItemPatternId, UIA_TogglePatternId,
};

const IID_IUIAUTOMATION: GUID = GUID::from_u128(0x30cbe57d_d9d0_452a_ab13_7ac5ac4825ee);
const IID_INVOKE_PATTERN: GUID = GUID::from_u128(0xfb377fbe_8ea6_46d5_9c73_6499642d3059);
const IID_SELECTIONITEM_PATTERN: GUID = GUID::from_u128(0xa8efa66a_0fda_421a_9194_38021f3578ea);
const IID_TOGGLE_PATTERN: GUID = GUID::from_u128(0x94cf8058_9b8d_4ab9_8bfd_4cd0a33c8c70);

// Vtable slots (see header note for provenance).
const IDX_QI: usize = 0;
const IDX_ADDREF: usize = 1;
const IDX_RELEASE: usize = 2;
const IDX_ELEMENT_FROM_HANDLE: usize = 6;
const IDX_RAW_VIEW_WALKER: usize = 16;
const IDX_FIRST_CHILD: usize = 4;
const IDX_NEXT_SIBLING: usize = 6;
const IDX_GET_PATTERN: usize = 16;
const IDX_CONTROL_TYPE: usize = 21;
const IDX_NAME: usize = 23;
const IDX_AUTOMATION_ID: usize = 29;
const IDX_BOUNDING_RECT: usize = 43;
const IDX_PATTERN_FIRE: usize = 3;

/// Owning COM interface pointer: AddRef on clone, Release on drop.
/// Mirrors `windows::core::Interface` ownership exactly (every successful
/// out-param carries one ref; every clone AddRefs).
struct Iface(*mut c_void);

/// Adopt one owned ref; a null out-param is a failure even when the
/// HRESULT says otherwise (the `windows` build mapped null through
/// `Type::from_abi` to Err the same way).
fn adopt_out(out: *mut c_void, hr: HRESULT) -> Result<Iface, HRESULT> {
    if hr == 0 && !out.is_null() {
        Ok(Iface(out))
    } else {
        Err(hr)
    }
}

impl Iface {
    /// Adopt one owned ref; null if the call failed.
    unsafe fn adopt(p: *mut c_void) -> Option<Self> {
        if p.is_null() {
            None
        } else {
            Some(Self(p))
        }
    }

    unsafe fn slot<F: Copy>(&self, index: usize) -> F {
        let vtbl = *(self.0 as *mut *const F);
        *vtbl.add(index)
    }

    unsafe fn query(&self, iid: &GUID) -> Result<Self, HRESULT> {
        type QI = unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
        let f: QI = self.slot(IDX_QI);
        let mut out: *mut c_void = std::ptr::null_mut();
        let hr = f(self.0, iid, &mut out);
        adopt_out(out, hr)
    }
}

impl Clone for Iface {
    fn clone(&self) -> Self {
        unsafe {
            type AR = unsafe extern "system" fn(*mut c_void) -> u32;
            let f: AR = self.slot(IDX_ADDREF);
            f(self.0);
        }
        Self(self.0)
    }
}

impl Drop for Iface {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                type Rel = unsafe extern "system" fn(*mut c_void) -> u32;
                let f: Rel = self.slot(IDX_RELEASE);
                f(self.0);
            }
        }
    }
}

fn ensure_com() {
    unsafe {
        CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32);
    }
}

fn automation() -> Result<Iface, String> {
    ensure_com();
    unsafe {
        let mut out: *mut c_void = std::ptr::null_mut();
        let hr = CoCreateInstance(
            &CUIAutomation,
            std::ptr::null_mut(),
            CLSCTX_ALL,
            &IID_IUIAUTOMATION,
            &mut out,
        );
        if hr == 0 {
            // CoCreateInstance success with a null out-param cannot happen;
            // adopt maps it to the same failure shape.
            Iface::adopt(out)
                .ok_or_else(|| format!("UIA CoCreateInstance failed: {}", crate::hr_text(hr)))
        } else {
            Err(format!(
                "UIA CoCreateInstance failed: {}",
                crate::hr_text(hr)
            ))
        }
    }
}

/// Import-check analogue of puppet.py `_uia_available` (no window touched).
pub fn uia_available() -> bool {
    automation().is_ok()
}

fn element_from_handle(auto: &Iface, hwnd: HWND) -> Result<Iface, HRESULT> {
    unsafe {
        type EFH = unsafe extern "system" fn(*mut c_void, HWND, *mut *mut c_void) -> HRESULT;
        let f: EFH = auto.slot(IDX_ELEMENT_FROM_HANDLE);
        let mut out: *mut c_void = std::ptr::null_mut();
        let hr = f(auto.0, hwnd, &mut out);
        adopt_out(out, hr)
    }
}

fn raw_view_walker(auto: &Iface) -> Result<Iface, HRESULT> {
    unsafe {
        type RVW = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
        let f: RVW = auto.slot(IDX_RAW_VIEW_WALKER);
        let mut out: *mut c_void = std::ptr::null_mut();
        let hr = f(auto.0, &mut out);
        adopt_out(out, hr)
    }
}

fn walker_child(walker: &Iface, index: usize, el: &Iface) -> Result<Iface, HRESULT> {
    unsafe {
        type WC = unsafe extern "system" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> HRESULT;
        let f: WC = walker.slot(index);
        let mut out: *mut c_void = std::ptr::null_mut();
        let hr = f(walker.0, el.0, &mut out);
        adopt_out(out, hr)
    }
}

fn el_bstr(el: &Iface, index: usize) -> String {
    unsafe {
        type GB = unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT;
        let f: GB = el.slot(index);
        let mut out: *mut u16 = std::ptr::null_mut();
        if f(el.0, &mut out) != 0 || out.is_null() {
            return String::new();
        }
        // BSTR layout: 4-byte byte-length prefix, then UTF-16 units
        // (SysStringLen equivalent; freed with SysFreeString like BSTR).
        let len = *((out as *const u8).sub(4) as *const u32) as usize / 2;
        let slice = std::slice::from_raw_parts(out, len);
        let s = String::from_utf16_lossy(slice);
        SysFreeString(out);
        s
    }
}

fn el_rect(el: &Iface) -> Vec<i64> {
    unsafe {
        type GR = unsafe extern "system" fn(*mut c_void, *mut RECT) -> HRESULT;
        let f: GR = el.slot(IDX_BOUNDING_RECT);
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if f(el.0, &mut rc) != 0 {
            return vec![0, 0, 0, 0];
        }
        vec![
            rc.left as i64,
            rc.top as i64,
            (rc.right - rc.left) as i64,
            (rc.bottom - rc.top) as i64,
        ]
    }
}

fn el_control_type(el: &Iface) -> Option<i32> {
    unsafe {
        type GCT = unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT;
        let f: GCT = el.slot(IDX_CONTROL_TYPE);
        let mut ct: i32 = 0;
        if f(el.0, &mut ct) != 0 {
            None
        } else {
            Some(ct)
        }
    }
}

fn el_get_pattern(el: &Iface, pattern_id: i32) -> Result<Iface, HRESULT> {
    unsafe {
        type GP = unsafe extern "system" fn(*mut c_void, i32, *mut *mut c_void) -> HRESULT;
        let f: GP = el.slot(IDX_GET_PATTERN);
        let mut out: *mut c_void = std::ptr::null_mut();
        let hr = f(el.0, pattern_id, &mut out);
        adopt_out(out, hr)
    }
}

fn fire_pattern(pat: &Iface) -> HRESULT {
    unsafe {
        type Fire = unsafe extern "system" fn(*mut c_void) -> HRESULT;
        let f: Fire = pat.slot(IDX_PATTERN_FIRE);
        f(pat.0)
    }
}

fn control_type_name(ct: i32) -> Option<&'static str> {
    Some(match ct {
        50040 => "AppBarControl",
        50000 => "ButtonControl",
        50001 => "CalendarControl",
        50002 => "CheckBoxControl",
        50003 => "ComboBoxControl",
        50025 => "CustomControl",
        50028 => "DataGridControl",
        50029 => "DataItemControl",
        50030 => "DocumentControl",
        50004 => "EditControl",
        50026 => "GroupControl",
        50034 => "HeaderControl",
        50035 => "HeaderItemControl",
        50005 => "HyperlinkControl",
        50006 => "ImageControl",
        50008 => "ListControl",
        50007 => "ListItemControl",
        50010 => "MenuBarControl",
        50009 => "MenuControl",
        50011 => "MenuItemControl",
        50033 => "PaneControl",
        50012 => "ProgressBarControl",
        50013 => "RadioButtonControl",
        50014 => "ScrollBarControl",
        50039 => "SemanticZoomControl",
        50038 => "SeparatorControl",
        50015 => "SliderControl",
        50016 => "SpinnerControl",
        50031 => "SplitButtonControl",
        50017 => "StatusBarControl",
        50018 => "TabControl",
        50019 => "TabItemControl",
        50036 => "TableControl",
        50020 => "TextControl",
        50027 => "ThumbControl",
        50037 => "TitleBarControl",
        50021 => "ToolBarControl",
        50022 => "ToolTipControl",
        50023 => "TreeControl",
        50024 => "TreeItemControl",
        50032 => "WindowControl",
        _ => return None,
    })
}

/// Raw-view children in walker order (puppet.py `Control.GetChildren`:
/// `GetFirstChildControl` + `GetNextSiblingControl` loop). Any COM failure
/// ends the level, mirroring the walk's `try/except: return`.
fn raw_children(walker: &Iface, el: &Iface) -> Vec<Iface> {
    let mut out = Vec::new();
    let first = match walker_child(walker, IDX_FIRST_CHILD, el) {
        Ok(e) => e,
        Err(_) => return out,
    };
    let mut cur = first;
    for _ in 0..100000 {
        let next = walker_child(walker, IDX_NEXT_SIBLING, &cur);
        out.push(cur);
        match next {
            Ok(nxt) => cur = nxt,
            Err(_) => break,
        }
    }
    out
}

fn el_name(el: &Iface) -> String {
    el_bstr(el, IDX_NAME)
}

fn el_aid(el: &Iface) -> String {
    el_bstr(el, IDX_AUTOMATION_ID)
}

fn el_invokable(el: &Iface) -> bool {
    // Each successful GetCurrentPattern hands back an owned IUnknown that is
    // released here (the windows build dropped its Result<IUnknown> the same
    // way); only the success bit is observed.
    el_get_pattern(el, UIA_InvokePatternId).is_ok()
        || el_get_pattern(el, UIA_SelectionItemPatternId).is_ok()
        || el_get_pattern(el, UIA_TogglePatternId).is_ok()
}

struct Walk {
    entries: Vec<Map<String, Value>>,
    truncated: bool,
    depth_capped: bool,
    win: i64,
    walker: Iface,
}

impl Walk {
    fn walk(&mut self, el: &Iface, path: &str, depth: i64) {
        let kids = raw_children(&self.walker, el);
        for (i, k) in kids.iter().enumerate() {
            if self.entries.len() >= ELEMENT_CAP {
                self.truncated = true;
                return;
            }
            let p = if path.is_empty() {
                i.to_string()
            } else {
                format!("{path}.{i}")
            };
            let name = el_name(k);
            let aid = el_aid(k);
            let ct = el_control_type(k);
            let ctn = ct.and_then(control_type_name).unwrap_or("");
            // puppet.py: role = ControlTypeName minus "Control", else
            // str(ControlType), else name-or-"Unknown" chain.
            let role = if ctn.ends_with("Control") && ctn.len() > "Control".len() {
                ctn[..ctn.len() - "Control".len()].to_string()
            } else if let Some(id) = ct {
                id.to_string()
            } else {
                ctn.to_string()
            };
            let role = if role.is_empty() {
                "Unknown".to_string()
            } else {
                role
            };
            let rect = el_rect(k);
            let inv = el_invokable(k);
            if !name.is_empty() || !aid.is_empty() || inv {
                let mut e = Map::new();
                e.insert("win".to_string(), Value::from(self.win));
                e.insert("role".to_string(), Value::from(role));
                e.insert("name".to_string(), Value::from(name));
                e.insert("auto_id".to_string(), Value::from(aid));
                e.insert(
                    "rect".to_string(),
                    Value::from(rect.into_iter().map(Value::from).collect::<Vec<_>>()),
                );
                e.insert("invokable".to_string(), Value::from(inv));
                e.insert("path".to_string(), Value::from(p.clone()));
                self.entries.push(e);
            }
            if depth < UIA_DEPTH_LIMIT {
                self.walk(k, &p, depth + 1);
            } else {
                // At the cut-off the child is listed but its children would
                // vanish: probe so truncated never lies (puppet.py:1186-1195).
                let sub = raw_children(&self.walker, k);
                if !sub.is_empty() {
                    self.truncated = true;
                    self.depth_capped = true;
                }
            }
            if self.entries.len() >= ELEMENT_CAP {
                self.truncated = true;
                return;
            }
        }
    }
}

/// Depth-first UIA walk of hwnd's subtree (puppet.py:1097-1201).
/// Returns (entries, truncated, depth_capped); raises (Err) when UIA is
/// unusable, with the `UIA ControlFromHandle failed:` prefix.
pub fn uia_element_list(hwnd: HWND) -> Result<(Vec<Map<String, Value>>, bool, bool), String> {
    let auto = automation()?;
    let root = element_from_handle(&auto, hwnd)
        .map_err(|hr| format!("UIA ControlFromHandle failed: {}", crate::hr_text(hr)))?;
    let walker = raw_view_walker(&auto)
        .map_err(|hr| format!("UIA RawViewWalker failed: {}", crate::hr_text(hr)))?;
    let win = hwnd as i64;
    let mut w = Walk {
        entries: Vec::new(),
        truncated: false,
        depth_capped: false,
        win,
        walker,
    };
    w.walk(&root, "", 0);
    Ok((w.entries, w.truncated, w.depth_capped))
}

/// Fire Invoke/SelectionItem/Toggle on the UIA element at path
/// (puppet.py:1391-1439). No mouse, no focus change.
/// `unconfirmed: activation needs owner permission` - implemented but never
/// exercised by the headless parity harness.
pub fn uia_activate(hwnd: i64, path: &str) -> (bool, String, Option<String>) {
    let auto = match automation() {
        Ok(a) => a,
        Err(e) => {
            return (
                false,
                String::new(),
                Some(format!("UIA resolve failed: {e}")),
            )
        }
    };
    let mut el = match element_from_handle(&auto, hwnd as HWND) {
        Ok(e) => e,
        Err(hr) => {
            return (
                false,
                String::new(),
                Some(format!("UIA resolve failed: {}", crate::hr_text(hr))),
            )
        }
    };
    let walker = match raw_view_walker(&auto) {
        Ok(w) => w,
        Err(hr) => {
            return (
                false,
                String::new(),
                Some(format!("UIA resolve failed: {}", crate::hr_text(hr))),
            )
        }
    };
    if !path.is_empty() {
        for seg in path.split('.') {
            let kids = raw_children(&walker, &el);
            let idx: i64 = match seg.trim().parse() {
                Ok(n) => n,
                Err(e) => {
                    return (
                        false,
                        String::new(),
                        Some(format!("UIA resolve failed: {e}")),
                    )
                }
            };
            if idx < 0 || idx >= kids.len() as i64 {
                return (
                    false,
                    String::new(),
                    Some(format!(
                        "UIA path {} went stale (child {seg} missing)",
                        crate::py_repr_str(path)
                    )),
                );
            }
            // Transfer the chosen child's ref (clone = AddRef); the parent
            // and the unchosen siblings release here, mirroring the windows
            // build's clone-and-reassign.
            let next = kids[idx as usize].clone();
            drop(el);
            el = next;
        }
    }
    // One arm per pattern (puppet.py:1391-1439): a failed GetCurrentPattern
    // silently tries the next pattern; QueryInterface and fire failures
    // return. The intermediate IUnknown releases on scope exit in all arms.
    if let Ok(unk) = el_get_pattern(&el, UIA_InvokePatternId) {
        match unsafe { unk.query(&IID_INVOKE_PATTERN) } {
            Ok(pat) => {
                let hr = fire_pattern(&pat);
                if hr == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    return (true, "uia-invoke".to_string(), None);
                }
                return (
                    false,
                    "uia-invoke".to_string(),
                    Some(format!(
                        "uia-invoke pattern call failed: {}",
                        crate::hr_text(hr)
                    )),
                );
            }
            Err(hr) => {
                return (
                    false,
                    "uia-invoke".to_string(),
                    Some(format!(
                        "uia-invoke pattern call failed: {}",
                        crate::hr_text(hr)
                    )),
                )
            }
        }
    }
    if let Ok(unk) = el_get_pattern(&el, UIA_SelectionItemPatternId) {
        match unsafe { unk.query(&IID_SELECTIONITEM_PATTERN) } {
            Ok(pat) => {
                let hr = fire_pattern(&pat);
                if hr == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    return (true, "uia-select".to_string(), None);
                }
                return (
                    false,
                    "uia-select".to_string(),
                    Some(format!(
                        "uia-select pattern call failed: {}",
                        crate::hr_text(hr)
                    )),
                );
            }
            Err(hr) => {
                return (
                    false,
                    "uia-select".to_string(),
                    Some(format!(
                        "uia-select pattern call failed: {}",
                        crate::hr_text(hr)
                    )),
                )
            }
        }
    }
    if let Ok(unk) = el_get_pattern(&el, UIA_TogglePatternId) {
        match unsafe { unk.query(&IID_TOGGLE_PATTERN) } {
            Ok(pat) => {
                let hr = fire_pattern(&pat);
                if hr == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    return (true, "uia-toggle".to_string(), None);
                }
                return (
                    false,
                    "uia-toggle".to_string(),
                    Some(format!(
                        "uia-toggle pattern call failed: {}",
                        crate::hr_text(hr)
                    )),
                );
            }
            Err(hr) => {
                return (
                    false,
                    "uia-toggle".to_string(),
                    Some(format!(
                        "uia-toggle pattern call failed: {}",
                        crate::hr_text(hr)
                    )),
                )
            }
        }
    }
    (
        false,
        String::new(),
        Some(
            "element supports neither Invoke, SelectionItem nor Toggle (pass \"fallback\":\"click\" for a pixel click)"
                .to_string(),
        ),
    )
}
