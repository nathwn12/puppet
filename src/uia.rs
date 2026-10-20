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
//! ExpandCollapse: Expand(3), Collapse(4), CurrentExpandCollapseState(5).
//! RangeValue: SetValue(f64)(3), CurrentValue(4), CurrentIsReadOnly(5),
//! CurrentMaximum(6), CurrentMinimum(7). TextPattern: DocumentRange(7);
//! TextRange.GetText(maxLen, out BSTR)(12). Slot indices taken verbatim
//! from the `windows` 0.61 generated `_Vtbl` structs.
//! IIDs are the `define_interface!` GUIDs from the same source.
//! Error text uses `crate::hr_text`, which replicates `windows::core::Error`'s
//! Display (`"{message} (0x{HR:08X})"`) via the identical FormatMessage call.

use crate::ELEMENT_CAP;
use serde_json::{Map, Value};
use std::ffi::c_void;
use windows_sys::core::{GUID, HRESULT};
use windows_sys::Win32::Foundation::{SysAllocStringLen, SysFreeString, HWND, RECT};
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows_sys::Win32::UI::Accessibility::{
    CUIAutomation, UIA_ExpandCollapsePatternId, UIA_InvokePatternId, UIA_RangeValuePatternId,
    UIA_ScrollPatternId, UIA_SelectionItemPatternId, UIA_TextPatternId, UIA_TogglePatternId,
    UIA_ValuePatternId,
};

const IID_IUIAUTOMATION: GUID = GUID::from_u128(0x30cbe57d_d9d0_452a_ab13_7ac5ac4825ee);
const IID_INVOKE_PATTERN: GUID = GUID::from_u128(0xfb377fbe_8ea6_46d5_9c73_6499642d3059);
const IID_SELECTIONITEM_PATTERN: GUID = GUID::from_u128(0xa8efa66a_0fda_421a_9194_38021f3578ea);
const IID_TOGGLE_PATTERN: GUID = GUID::from_u128(0x94cf8058_9b8d_4ab9_8bfd_4cd0a33c8c70);
const IID_VALUE_PATTERN: GUID = GUID::from_u128(0xa94cd8b1_0844_4cd6_9d2d_640537ab39e9);
// IUIAutomationExpandCollapsePattern (windows 0.61 generated _Vtbl:
// base IUnknown(0,1,2) + Expand(3), Collapse(4), CurrentExpandCollapseState(5)).
const IID_EXPANDCOLLAPSE_PATTERN: GUID = GUID::from_u128(0x619be086_1f4e_4ee4_bafa_210128738730);
// IUIAutomationRangeValuePattern (_Vtbl: base + SetValue(f64)(3),
// CurrentValue(4), CurrentIsReadOnly(5), CurrentMaximum(6),
// CurrentMinimum(7), ...).
const IID_RANGEVALUE_PATTERN: GUID = GUID::from_u128(0x59213f4f_7346_49e5_b120_80555987a148);
// IUIAutomationTextPattern (_Vtbl: base + RangeFromPoint(3),
// RangeFromChild(4), GetSelection(5), GetVisibleRanges(6), DocumentRange(7),
// ...). NOTE: DocumentRange is slot 7, not 3 — the _Vtbl field order is
// the vtable order (verified against windows 0.61 generated source).
const IID_TEXT_PATTERN: GUID = GUID::from_u128(0x32eba289_3583_42c9_9c59_3b6d9a1e9b6a);
// IUIAutomationScrollPattern (_Vtbl: base + Scroll(h,v)(3),
// SetScrollPercent(4), Current* getters(5+)). ScrollAmount values:
// LargeDecrement=0, SmallDecrement=1, NoAmount=2, LargeIncrement=3,
// SmallIncrement=4 (verified against windows 0.61 generated _Vtbl).
const IID_SCROLL_PATTERN: GUID = GUID::from_u128(0x88f4d42a_e881_459d_a77c_73bbbb7e02dc);

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
const IDX_VALUE_SET: usize = 3;
const IDX_VALUE_IS_READONLY: usize = 5;
/// ExpandCollapse vtbl (see IID note): Expand(3), Collapse(4),
/// CurrentExpandCollapseState(5).
const IDX_EXPAND: usize = 3;
const IDX_COLLAPSE: usize = 4;
const IDX_CURRENT_EXPANDCOLLAPSE_STATE: usize = 5;
/// RangeValue vtbl: SetValue(f64)(3), CurrentValue(4),
/// CurrentIsReadOnly(5), CurrentMaximum(6), CurrentMinimum(7).
const IDX_RANGE_SET_VALUE: usize = 3;
const IDX_RANGE_VALUE: usize = 4;
const IDX_RANGE_IS_READONLY: usize = 5;
const IDX_RANGE_MAXIMUM: usize = 6;
const IDX_RANGE_MINIMUM: usize = 7;
/// TextPattern.DocumentRange is slot 7, NOT 3. TextRange.GetText is slot 12
/// (_Vtbl: base + Clone(3), Compare(4), CompareEndpoints(5),
/// ExpandToEnclosingUnit(6), FindAttribute(7), FindText(8),
/// GetAttributeValue(9), GetBoundingRectangles(10), GetEnclosingElement(11),
/// GetText(12)). Verified against windows 0.61 generated source.
const IDX_TEXT_DOCUMENT_RANGE: usize = 7;
const IDX_TEXTRANGE_GET_TEXT: usize = 12;
/// ScrollPattern vtbl: Scroll(horizontalAmount, verticalAmount)(3),
/// SetScrollPercent(4). Amounts are the i32 ScrollAmount enum.
const IDX_SCROLL_SCROLL: usize = 3;
/// ScrollAmount enum values (windows 0.61 generated source).
const SCROLL_NO_AMOUNT: i32 = 2;
const SCROLL_SMALL_DECREMENT: i32 = 1;
const SCROLL_SMALL_INCREMENT: i32 = 4;
/// IUIAutomationElement::CurrentIsEnabled dedicated getter (slot 28 in the
/// windows 0.58 `IUIAutomationElement_Vtbl` above: base IUnknown(0,1,2) then
/// SetFocus(3) through GetCurrentPattern(16), the cached variants, and the
/// per-property getters up to CurrentIsEnabled(28)).
const IDX_CURRENT_IS_ENABLED: usize = 28;

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
        type Efh = unsafe extern "system" fn(*mut c_void, HWND, *mut *mut c_void) -> HRESULT;
        let f: Efh = auto.slot(IDX_ELEMENT_FROM_HANDLE);
        let mut out: *mut c_void = std::ptr::null_mut();
        let hr = f(auto.0, hwnd, &mut out);
        adopt_out(out, hr)
    }
}

fn raw_view_walker(auto: &Iface) -> Result<Iface, HRESULT> {
    unsafe {
        type Rvw = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
        let f: Rvw = auto.slot(IDX_RAW_VIEW_WALKER);
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
        type Gct = unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT;
        let f: Gct = el.slot(IDX_CONTROL_TYPE);
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

/// Two-argument vtable call mirroring `fire_pattern`: ValuePattern.SetValue
/// lives at the same slot 3 but takes a BSTR (IUIAutomationValuePattern_Vtbl:
/// base IUnknown + SetValue(3), CurrentValue(4), CurrentIsReadOnly(5) per the
/// windows 0.58 generated vtbl). Allocates with SysAllocStringLen so empty
/// clears via a null BSTR; frees with SysFreeString like BSTR.
fn value_set(pat: &Iface, text: &str) -> HRESULT {
    unsafe {
        type SetV = unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT;
        let f: SetV = pat.slot(IDX_VALUE_SET);
        let wide: Vec<u16> = text.encode_utf16().collect();
        let bstr: *const u16 = if wide.is_empty() {
            std::ptr::null()
        } else {
            SysAllocStringLen(wide.as_ptr(), wide.len() as u32)
        };
        if !wide.is_empty() && bstr.is_null() {
            return -2147024882; // E_OUTOFMEMORY
        }
        let hr = f(pat.0, bstr);
        if !bstr.is_null() {
            SysFreeString(bstr);
        }
        hr
    }
}

fn value_is_readonly(pat: &Iface) -> Option<bool> {
    unsafe {
        type Gir = unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT;
        let f: Gir = pat.slot(IDX_VALUE_IS_READONLY);
        let mut v: i32 = 0;
        if f(pat.0, &mut v) != 0 {
            None
        } else {
            Some(v != 0)
        }
    }
}

/// ExpandCollapse actuation: Expand(3) or Collapse(4). Mirrors
/// `fire_pattern`.
fn expand_fire(pat: &Iface, expand: bool) -> HRESULT {
    unsafe {
        type Fire = unsafe extern "system" fn(*mut c_void) -> HRESULT;
        let f: Fire = pat.slot(if expand { IDX_EXPAND } else { IDX_COLLAPSE });
        f(pat.0)
    }
}

/// CurrentExpandCollapseState(5) as the raw i32. windows 0.61 enum:
/// Collapsed=0, Expanded=1, PartiallyExpanded=2, LeafNode=3.
fn expand_state_raw(pat: &Iface) -> Result<i32, HRESULT> {
    unsafe {
        type Ges = unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT;
        let f: Ges = pat.slot(IDX_CURRENT_EXPANDCOLLAPSE_STATE);
        let mut v: i32 = -1;
        let hr = f(pat.0, &mut v);
        if hr != 0 {
            Err(hr)
        } else {
            Ok(v)
        }
    }
}

/// Pure ScrollPattern direction: amount sign selects a small increment or
/// decrement on the requested axis, NoAmount on the other. Only the sign
/// matters (a scroll step, not a distance); axis must be v|h.
pub fn scroll_pattern_args(axis: &str, amt: i64) -> Result<(i32, i32), String> {
    let step = match amt.cmp(&0) {
        std::cmp::Ordering::Greater => SCROLL_SMALL_INCREMENT,
        std::cmp::Ordering::Less => SCROLL_SMALL_DECREMENT,
        std::cmp::Ordering::Equal => SCROLL_NO_AMOUNT,
    };
    match axis {
        "v" => Ok((SCROLL_NO_AMOUNT, step)),
        "h" => Ok((step, SCROLL_NO_AMOUNT)),
        _ => Err(format!("unknown scroll axis \"{axis}\" (want v|h)")),
    }
}

fn scroll_fire(pat: &Iface, h: i32, v: i32) -> HRESULT {
    unsafe {
        type Fire = unsafe extern "system" fn(*mut c_void, i32, i32) -> HRESULT;
        let f: Fire = pat.slot(IDX_SCROLL_SCROLL);
        f(pat.0, h, v)
    }
}

/// Human name for an ExpandCollapseState value (windows 0.61 numbering).
pub fn expand_state_name(state: i32) -> &'static str {
    match state {
        0 => "Collapsed",
        1 => "Expanded",
        2 => "PartiallyExpanded",
        3 => "LeafNode",
        _ => "Unknown",
    }
}

/// RangeValue.SetValue(f64) at slot 3.
fn range_set(pat: &Iface, value: f64) -> HRESULT {
    unsafe {
        type SetV = unsafe extern "system" fn(*mut c_void, f64) -> HRESULT;
        let f: SetV = pat.slot(IDX_RANGE_SET_VALUE);
        f(pat.0, value)
    }
}

/// RangeValue scalar getter (CurrentValue(4), CurrentMaximum(6),
/// CurrentMinimum(7)) or read-only flag (CurrentIsReadOnly(5)).
fn range_f64(pat: &Iface, index: usize) -> Result<f64, HRESULT> {
    unsafe {
        type G = unsafe extern "system" fn(*mut c_void, *mut f64) -> HRESULT;
        let f: G = pat.slot(index);
        let mut v: f64 = 0.0;
        let hr = f(pat.0, &mut v);
        if hr != 0 {
            Err(hr)
        } else {
            Ok(v)
        }
    }
}

fn range_is_readonly(pat: &Iface) -> Option<bool> {
    unsafe {
        type Gir = unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT;
        let f: Gir = pat.slot(IDX_RANGE_IS_READONLY);
        let mut v: i32 = 0;
        if f(pat.0, &mut v) != 0 {
            None
        } else {
            Some(v != 0)
        }
    }
}

/// Pure RangeValue argument check: numeric, finite, within [min, max].
/// Typed errors name the bound, mirroring the scroll clamp style.
pub fn parse_range_value(text: &str, min: f64, max: f64) -> Result<f64, String> {
    let t = text.trim();
    let v: f64 = t
        .parse()
        .map_err(|_| format!("range value {text:?} is not a number (want {min}..{max})"))?;
    if !v.is_finite() {
        return Err(format!(
            "range value {text:?} is not finite (want {min}..{max})"
        ));
    }
    if v < min || v > max {
        return Err(format!("range value {v} out of range {min}..{max}"));
    }
    Ok(v)
}

/// TextPattern.DocumentRange(7): the whole-document text range.
fn text_document_range(pat: &Iface) -> Result<Iface, HRESULT> {
    unsafe {
        type Dr = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
        let f: Dr = pat.slot(IDX_TEXT_DOCUMENT_RANGE);
        let mut out: *mut c_void = std::ptr::null_mut();
        let hr = f(pat.0, &mut out);
        adopt_out(out, hr)
    }
}

/// TextRange.GetText(-1): the full range text. The out BSTR is length-
/// prefixed (u32 byte count before the pointer); read the prefix instead
/// of SysStringLen so no extra import is needed. Always SysFreeString'd.
fn text_range_get_text(range: &Iface) -> Result<String, HRESULT> {
    unsafe {
        type Gt = unsafe extern "system" fn(*mut c_void, i32, *mut *mut c_void) -> HRESULT;
        let f: Gt = range.slot(IDX_TEXTRANGE_GET_TEXT);
        let mut out: *mut c_void = std::ptr::null_mut();
        let hr = f(range.0, -1, &mut out);
        if hr != 0 {
            return Err(hr);
        }
        if out.is_null() {
            return Ok(String::new());
        }
        let nbytes = *(out as *const u32).offset(-1) as usize;
        let units = std::slice::from_raw_parts(out as *const u16, nbytes / 2);
        let s = String::from_utf16_lossy(units);
        SysFreeString(out as *const u16);
        Ok(s)
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
    pager: Pager,
    depth_capped: bool,
    win: i64,
    walker: Iface,
    max_depth: i64,
}

/// Page policy for the element walk: skip the first `offset` accepted
/// entries, collect at most `cap`, and report whether more accepted entries
/// remain past the returned page (`next_offset`). Pure query policy over an
/// accepted/not-accepted stream — no COM, unit-tested without a window.
/// Shared by the UIA walk and the Win32 fallback so pagination means the
/// same on both backends.
pub(crate) struct Pager {
    pub(crate) offset: usize,
    pub(crate) cap: usize,
    pub(crate) seen: usize,
    pub(crate) collected: usize,
    pub(crate) truncated: bool,
    pub(crate) finished: bool,
}

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub(crate) enum PageAction {
    Skip,
    Collect,
    Stop,
}

impl Pager {
    pub(crate) fn new(offset: usize, cap: usize) -> Self {
        Pager {
            offset,
            cap,
            seen: 0,
            collected: 0,
            truncated: false,
            finished: false,
        }
    }

    /// One accepted/non-accepted visit in walk order. `Collect` means build
    /// and store the entry; `Skip` means keep walking (children still count,
    /// pre-order); `Stop` means more remain — unwind everything.
    pub(crate) fn observe(&mut self, accepted: bool) -> PageAction {
        // After Stop the walk unwinds on `finished`; any further observation
        // is skipped by contract so a caller that keeps iterating cannot
        // re-trigger collection.
        if !accepted || self.finished {
            return PageAction::Skip;
        }
        self.seen += 1;
        if self.seen <= self.offset {
            return PageAction::Skip;
        }
        if self.collected < self.cap {
            self.collected += 1;
            if self.collected >= self.cap {
                // Cap hit: legacy `truncated` semantics (true even when the
                // page is exactly full); keep walking to learn whether more
                // remain, which is what `next_offset` reports.
                self.truncated = true;
            }
            return PageAction::Collect;
        }
        self.truncated = true;
        self.finished = true;
        PageAction::Stop
    }

    /// Offset of the next page when more accepted entries remain past the
    /// returned one, else null. Only `Some` once the walk observed an
    /// accepted entry past the full page.
    pub(crate) fn next_offset(&self) -> Option<usize> {
        if self.finished {
            Some(self.offset + self.collected)
        } else {
            None
        }
    }
}

impl Walk {
    fn walk(&mut self, el: &Iface, path: &str, depth: i64) {
        if self.pager.finished {
            return;
        }
        let kids = raw_children(&self.walker, el);
        for (i, k) in kids.iter().enumerate() {
            if self.pager.finished {
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
            let inv = el_invokable(k);
            match self
                .pager
                .observe(!name.is_empty() || !aid.is_empty() || inv)
            {
                PageAction::Skip => {}
                PageAction::Collect => {
                    let rect = el_rect(k);
                    let mut e = Map::new();
                    e.insert("win".to_string(), Value::from(self.win));
                    e.insert("role".to_string(), Value::from(role.clone()));
                    e.insert("name".to_string(), Value::from(name.clone()));
                    e.insert("auto_id".to_string(), Value::from(aid.clone()));
                    e.insert(
                        "rect".to_string(),
                        Value::from(rect.iter().map(|v| Value::from(*v)).collect::<Vec<_>>()),
                    );
                    e.insert("invokable".to_string(), Value::from(inv));
                    e.insert("path".to_string(), Value::from(p.clone()));
                    e.insert(
                        "ref".to_string(),
                        Value::from(format!("{}.{}", self.win, p)),
                    );
                    e.insert(
                        "fp".to_string(),
                        Value::from(crate::shot::element_fp(&role, &name, &aid, inv)),
                    );
                    self.entries.push(e);
                }
                PageAction::Stop => {
                    return;
                }
            }
            if depth < self.max_depth {
                self.walk(k, &p, depth + 1);
            } else {
                // At the cut-off the child is listed but its children would
                // vanish: probe so truncated never lies (puppet.py:1186-1195).
                let sub = raw_children(&self.walker, k);
                if !sub.is_empty() {
                    self.pager.truncated = true;
                    self.depth_capped = true;
                }
            }
        }
    }
}

/// One page of the UIA walk: entries, legacy truncated flag, depth-capped
/// flag, and the next-page offset (Some only when more accepted entries
/// remain past the returned page).
type UiaPage = (Vec<Map<String, Value>>, bool, bool, Option<usize>);

/// Depth-first UIA walk of hwnd's subtree (puppet.py:1097-1201).
/// Returns (entries, truncated, depth_capped, next_offset); raises (Err)
/// when UIA is unusable, with the `UIA ControlFromHandle failed:` prefix.
/// `max_depth` bounds recursion (the applied depth limit, reported back as
/// `depth_limit`); `offset` skips that many accepted entries for paging.
/// `next_offset` is Some(offset + returned) only when more accepted entries
/// remain past the returned 200-entry page.
pub fn uia_element_list(hwnd: HWND, max_depth: i64, offset: usize) -> Result<UiaPage, String> {
    let auto = automation()?;
    let root = element_from_handle(&auto, hwnd)
        .map_err(|hr| format!("UIA ControlFromHandle failed: {}", crate::hr_text(hr)))?;
    let walker = raw_view_walker(&auto)
        .map_err(|hr| format!("UIA RawViewWalker failed: {}", crate::hr_text(hr)))?;
    let win = hwnd as i64;
    let mut w = Walk {
        entries: Vec::new(),
        pager: Pager::new(offset, ELEMENT_CAP),
        depth_capped: false,
        win,
        walker,
        max_depth,
    };
    w.walk(&root, "", 0);
    Ok((
        w.entries,
        w.pager.truncated,
        w.depth_capped,
        w.pager.next_offset(),
    ))
}

/// UIA CurrentIsEnabled for the control at `path` (slot 28, same vtbl
/// provenance as above). Read-only: never activates or focuses. Err on any
/// COM failure; the caller treats that as transient and keeps polling.
pub fn uia_is_enabled(hwnd: HWND, path: &str) -> Result<bool, String> {
    let auto = automation().map_err(|e| format!("UIA resolve failed: {e}"))?;
    let el = element_from_handle(&auto, hwnd)
        .map_err(|hr| format!("UIA resolve failed: {}", crate::hr_text(hr)))?;
    let walker = raw_view_walker(&auto)
        .map_err(|hr| format!("UIA resolve failed: {}", crate::hr_text(hr)))?;
    let el = resolve_path_el(&walker, el, path)?;
    unsafe {
        type GetEnabled = unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT;
        let f: GetEnabled = el.slot(IDX_CURRENT_IS_ENABLED);
        let mut v: i32 = 0;
        let hr = f(el.0, &mut v);
        if hr != 0 {
            return Err(format!("UIA IsEnabled read failed: {}", crate::hr_text(hr)));
        }
        Ok(v != 0)
    }
}

/// Shared path-walk resolver: follow `path` (`"0.1.2"`) from `el` through
/// the raw-view walker, transferring the chosen child's ref at each level.
/// Err carries the already-formatted resolve message (`UIA resolve failed:`
/// or the `went stale` form) so both activation and set_value stay closed.
fn resolve_path_el(walker: &Iface, mut el: Iface, path: &str) -> Result<Iface, String> {
    if !path.is_empty() {
        for seg in path.split('.') {
            let kids = raw_children(walker, &el);
            let idx: i64 = match seg.trim().parse() {
                Ok(n) => n,
                Err(e) => return Err(format!("UIA resolve failed: {e}")),
            };
            if idx < 0 || idx >= kids.len() as i64 {
                return Err(format!(
                    "UIA path {} went stale (child {seg} missing)",
                    crate::py_repr_str(path)
                ));
            }
            // Transfer the chosen child's ref (clone = AddRef); the parent
            // and the unchosen siblings release here, mirroring the windows
            // build's clone-and-reassign.
            let next = kids[idx as usize].clone();
            drop(el);
            el = next;
        }
    }
    Ok(el)
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
    let el = match element_from_handle(&auto, hwnd as HWND) {
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
    let el = match resolve_path_el(&walker, el, path) {
        Ok(e) => e,
        Err(msg) => return (false, String::new(), Some(msg)),
    };
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

/// Expand or collapse the UIA element at `path` via ExpandCollapse.
/// Only that pattern is tried: a SplitButton supports both Invoke and
/// ExpandCollapse, and a state request must never blindly fire Invoke
/// (which could open a menu instead of collapsing it). `unconfirmed:
/// activation needs owner permission` — implemented but never exercised
/// by the headless parity harness.
pub fn uia_expand_collapse(hwnd: i64, path: &str, expand: bool) -> (bool, String, Option<String>) {
    let via = if expand { "uia-expand" } else { "uia-collapse" };
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
    let el = match element_from_handle(&auto, hwnd as HWND) {
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
    let el = match resolve_path_el(&walker, el, path) {
        Ok(e) => e,
        Err(msg) => return (false, String::new(), Some(msg)),
    };
    let unk = match el_get_pattern(&el, UIA_ExpandCollapsePatternId) {
        Ok(u) => u,
        Err(_) => {
            return (
                false,
                via.to_string(),
                Some(
                    "element has no ExpandCollapsePattern (control cannot expand/collapse)"
                        .to_string(),
                ),
            )
        }
    };
    let pat = match unsafe { unk.query(&IID_EXPANDCOLLAPSE_PATTERN) } {
        Ok(p) => p,
        Err(hr) => {
            return (
                false,
                via.to_string(),
                Some(format!(
                    "uia-expand pattern call failed: {}",
                    crate::hr_text(hr)
                )),
            )
        }
    };
    let hr = expand_fire(&pat, expand);
    if hr == 0 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        (true, via.to_string(), None)
    } else {
        (
            false,
            via.to_string(),
            Some(format!("{via} pattern call failed: {}", crate::hr_text(hr))),
        )
    }
}

/// Read CurrentExpandCollapseState for the UIA element at `path`.
/// Read-only (no focus/input); Err on any COM failure.
pub fn uia_expand_state(hwnd: i64, path: &str) -> Result<i32, String> {
    let auto = automation().map_err(|e| format!("UIA resolve failed: {e}"))?;
    let el = element_from_handle(&auto, hwnd as HWND)
        .map_err(|hr| format!("UIA resolve failed: {}", crate::hr_text(hr)))?;
    let walker = raw_view_walker(&auto)
        .map_err(|hr| format!("UIA resolve failed: {}", crate::hr_text(hr)))?;
    let el = resolve_path_el(&walker, el, path)?;
    let unk = el_get_pattern(&el, UIA_ExpandCollapsePatternId)
        .map_err(|_| "element has no ExpandCollapsePattern".to_string())?;
    let pat = unsafe { unk.query(&IID_EXPANDCOLLAPSE_PATTERN) }
        .map_err(|hr| format!("uia-expand pattern call failed: {}", crate::hr_text(hr)))?;
    expand_state_raw(&pat)
        .map_err(|hr| format!("uia-expand state read failed: {}", crate::hr_text(hr)))
}

/// Read RangeValue.CurrentValue for the UIA element at `path`.
/// Read-only (no focus/input); Err on any COM failure or missing pattern.
pub fn uia_range_value(hwnd: i64, path: &str) -> Result<f64, String> {
    let auto = automation().map_err(|e| format!("UIA resolve failed: {e}"))?;
    let el = element_from_handle(&auto, hwnd as HWND)
        .map_err(|hr| format!("UIA resolve failed: {}", crate::hr_text(hr)))?;
    let walker = raw_view_walker(&auto)
        .map_err(|hr| format!("UIA resolve failed: {}", crate::hr_text(hr)))?;
    let el = resolve_path_el(&walker, el, path)?;
    let unk = el_get_pattern(&el, UIA_RangeValuePatternId)
        .map_err(|_| "element has no RangeValuePattern".to_string())?;
    let pat = unsafe { unk.query(&IID_RANGEVALUE_PATTERN) }
        .map_err(|hr| format!("uia-set-value pattern call failed: {}", crate::hr_text(hr)))?;
    range_f64(&pat, IDX_RANGE_VALUE)
        .map_err(|hr| format!("uia-set-value value read failed: {}", crate::hr_text(hr)))
}

/// Scroll a UIA element via ScrollPattern.Scroll(horizontal, vertical).
/// Read-only in the input sense (no mouse, no focus change). `unconfirmed:
/// activation needs owner permission` — implemented for live use, never
/// exercised by the headless parity harness.
pub fn uia_scroll(hwnd: i64, path: &str, h: i32, v: i32) -> (bool, String, Option<String>) {
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
    let el = match element_from_handle(&auto, hwnd as HWND) {
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
    let el = match resolve_path_el(&walker, el, path) {
        Ok(e) => e,
        Err(msg) => return (false, String::new(), Some(msg)),
    };
    let unk = match el_get_pattern(&el, UIA_ScrollPatternId) {
        Ok(u) => u,
        Err(_) => {
            return (
                false,
                "uia-scroll".to_string(),
                Some("element has no ScrollPattern (control is not scrollable)".to_string()),
            )
        }
    };
    let pat = match unsafe { unk.query(&IID_SCROLL_PATTERN) } {
        Ok(p) => p,
        Err(hr) => {
            return (
                false,
                "uia-scroll".to_string(),
                Some(format!(
                    "uia-scroll pattern call failed: {}",
                    crate::hr_text(hr)
                )),
            )
        }
    };
    let hr = scroll_fire(&pat, h, v);
    if hr == 0 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        (true, "uia-scroll".to_string(), None)
    } else {
        (
            false,
            "uia-scroll".to_string(),
            Some(format!(
                "uia-scroll pattern call failed: {}",
                crate::hr_text(hr)
            )),
        )
    }
}

/// Read the full TextPattern document text for the UIA element at `path`.
/// Read-only (no focus/input); Err when the pattern is missing or the
/// read fails. Callers cap the length (see `shot::cap_text`).
pub fn uia_element_text(hwnd: i64, path: &str) -> Result<String, String> {
    let auto = automation().map_err(|e| format!("UIA resolve failed: {e}"))?;
    let el = element_from_handle(&auto, hwnd as HWND)
        .map_err(|hr| format!("UIA resolve failed: {}", crate::hr_text(hr)))?;
    let walker = raw_view_walker(&auto)
        .map_err(|hr| format!("UIA resolve failed: {}", crate::hr_text(hr)))?;
    let el = resolve_path_el(&walker, el, path)?;
    let unk = el_get_pattern(&el, UIA_TextPatternId)
        .map_err(|_| "element has no TextPattern (control exposes no readable text)".to_string())?;
    let pat = unsafe { unk.query(&IID_TEXT_PATTERN) }
        .map_err(|hr| format!("uia text read failed: {}", crate::hr_text(hr)))?;
    let range = text_document_range(&pat)
        .map_err(|hr| format!("uia text read failed: {}", crate::hr_text(hr)))?;
    text_range_get_text(&range)
        .map_err(|hr| format!("uia text read failed: {}", crate::hr_text(hr)))
}
/// RangeValue fallback for `uia_set_value`: numeric controls (sliders)
/// expose RangeValue instead of ValuePattern. Parses `text` as f64,
/// honors read-only and [min, max]; every failure is a typed error.
/// Via stays `uia-set-value` like the Value path.
fn uia_set_value_range(el: &Iface, text: &str) -> (bool, String, Option<String>) {
    let via = "uia-set-value".to_string();
    let unk = match el_get_pattern(el, UIA_RangeValuePatternId) {
        Ok(u) => u,
        Err(_) => {
            return (
                false,
                via,
                Some(
                    "element has no ValuePattern (control is not editable via ValuePattern)"
                        .to_string(),
                ),
            )
        }
    };
    let pat = match unsafe { unk.query(&IID_RANGEVALUE_PATTERN) } {
        Ok(p) => p,
        Err(hr) => {
            return (
                false,
                via,
                Some(format!(
                    "uia-set-value pattern call failed: {}",
                    crate::hr_text(hr)
                )),
            )
        }
    };
    if range_is_readonly(&pat) == Some(true) {
        return (
            false,
            via,
            Some("element is read-only (RangeValue.IsReadOnly)".to_string()),
        );
    }
    let (min, max) = match (
        range_f64(&pat, IDX_RANGE_MINIMUM),
        range_f64(&pat, IDX_RANGE_MAXIMUM),
    ) {
        (Ok(lo), Ok(hi)) => (lo, hi),
        _ => {
            return (
                false,
                via,
                Some("uia-set-value range read failed (cannot read RangeValue bounds)".to_string()),
            )
        }
    };
    let v = match parse_range_value(text, min, max) {
        Ok(v) => v,
        Err(e) => return (false, via, Some(e)),
    };
    let hr = range_set(&pat, v);
    if hr == 0 {
        (true, via, None)
    } else {
        (
            false,
            via,
            Some(format!(
                "uia-set-value pattern call failed: {}",
                crate::hr_text(hr)
            )),
        )
    }
}
/// Write `text` into the UIA element at `path` via ValuePattern.SetValue,
/// falling back to RangeValue.SetValue for numeric controls (sliders)
/// when ValuePattern is absent.
/// resolve failures (`UIA resolve failed:` / `went stale`), no ValuePattern,
/// read-only, and pattern-call failure. Via is `uia-set-value` for every
/// pattern-level outcome, empty for resolve failures (the caller maps empty
/// to `none`, mirroring `uia_activate`).
pub fn uia_set_value(hwnd: i64, path: &str, text: &str) -> (bool, String, Option<String>) {
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
    let el = match element_from_handle(&auto, hwnd as HWND) {
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
    let el = match resolve_path_el(&walker, el, path) {
        Ok(e) => e,
        Err(msg) => return (false, String::new(), Some(msg)),
    };
    let unk = match el_get_pattern(&el, UIA_ValuePatternId) {
        Ok(u) => u,
        Err(_) => return uia_set_value_range(&el, text),
    };
    let pat = match unsafe { unk.query(&IID_VALUE_PATTERN) } {
        Ok(p) => p,
        Err(hr) => {
            return (
                false,
                "uia-set-value".to_string(),
                Some(format!(
                    "uia-set-value pattern call failed: {}",
                    crate::hr_text(hr)
                )),
            )
        }
    };
    if value_is_readonly(&pat) == Some(true) {
        return (
            false,
            "uia-set-value".to_string(),
            Some("element is read-only (ValuePattern.IsReadOnly)".to_string()),
        );
    }
    let hr = value_set(&pat, text);
    if hr == 0 {
        (true, "uia-set-value".to_string(), None)
    } else {
        (
            false,
            "uia-set-value".to_string(),
            Some(format!(
                "uia-set-value pattern call failed: {}",
                crate::hr_text(hr)
            )),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a pager through an accepted-run with holes (unaccepted visits
    /// interleaved, as anonymous layout nodes appear in a real walk).
    fn run(pager: &mut Pager, pattern: &[bool]) -> Vec<PageAction> {
        pattern.iter().map(|&a| pager.observe(a)).collect()
    }

    #[test]
    fn pager_collects_first_page_and_reports_no_more() {
        let mut p = Pager::new(0, 200);
        let acts = run(&mut p, &[true; 200]);
        assert!(acts.iter().all(|a| *a == PageAction::Collect));
        assert!(
            p.truncated,
            "a full page sets truncated (legacy cap semantics)"
        );
        assert_eq!(p.next_offset(), None, "exactly-full page has no next");
        assert!(!p.finished);
    }

    #[test]
    fn pager_201st_accepted_stops_with_next_offset() {
        let mut p = Pager::new(0, 200);
        run(&mut p, &[true; 200]);
        assert_eq!(p.observe(true), PageAction::Stop);
        assert!(p.truncated);
        assert!(p.finished);
        assert_eq!(p.next_offset(), Some(200));
        // Anything after Stop stays skipped; the walk unwinds on `finished`.
        assert_eq!(p.observe(true), PageAction::Skip);
        assert_eq!(p.next_offset(), Some(200));
    }

    #[test]
    fn pager_skips_offset_then_collects() {
        let mut p = Pager::new(5, 200);
        let acts = run(&mut p, &[true; 10]);
        assert_eq!(&acts[..5], &[PageAction::Skip; 5]);
        assert_eq!(&acts[5..], &[PageAction::Collect; 5]);
        assert!(!p.truncated);
        assert_eq!(p.next_offset(), None);
        assert_eq!(p.seen, 10);
        assert_eq!(p.collected, 5);
    }

    #[test]
    fn pager_offset_page_reports_next_when_more_remain() {
        let mut p = Pager::new(5, 200);
        run(&mut p, &[true; 205]);
        assert_eq!(p.observe(true), PageAction::Stop);
        assert_eq!(p.next_offset(), Some(205));
    }

    #[test]
    fn pager_offset_past_end_collects_nothing() {
        let mut p = Pager::new(1000, 200);
        let acts = run(&mut p, &[true; 3]);
        assert!(acts.iter().all(|a| *a == PageAction::Skip));
        assert!(!p.truncated);
        assert_eq!(p.next_offset(), None);
    }

    #[test]
    fn pager_ignores_unaccepted_without_counting() {
        let mut p = Pager::new(0, 200);
        let acts = run(&mut p, &[false, false, true, false, true]);
        assert_eq!(
            acts,
            vec![
                PageAction::Skip,
                PageAction::Skip,
                PageAction::Collect,
                PageAction::Skip,
                PageAction::Collect,
            ]
        );
        assert_eq!(p.seen, 2, "unaccepted visits must not advance the page");
        assert_eq!(p.collected, 2);
    }

    #[test]
    fn pager_offset_page_exactly_full_has_no_next() {
        // Offset analogue of the exactly-200 case: 5 skipped + 200
        // collected with nothing after keeps legacy truncated=true but
        // offers no next page.
        let mut p = Pager::new(5, 200);
        let acts = run(&mut p, &[true; 205]);
        assert_eq!(&acts[..5], &[PageAction::Skip; 5]);
        assert_eq!(&acts[5..], &[PageAction::Collect; 200]);
        assert!(p.truncated);
        assert!(!p.finished);
        assert_eq!(
            p.next_offset(),
            None,
            "exactly-full offset page has no next"
        );
    }

    #[test]
    fn pager_empty_stream_stays_clean() {
        let p = Pager::new(0, 200);
        assert_eq!(p.next_offset(), None);
        assert!(!p.truncated);
        assert!(!p.finished);
    }

    #[test]
    fn parse_range_value_table() {
        // Valid in-range values pass through exactly.
        assert_eq!(parse_range_value("42", 0.0, 100.0), Ok(42.0));
        assert_eq!(parse_range_value(" 3.5 ", 0.0, 100.0), Ok(3.5));
        assert_eq!(parse_range_value("0", 0.0, 100.0), Ok(0.0));
        assert_eq!(parse_range_value("100", 0.0, 100.0), Ok(100.0));
        // Non-numeric text is a typed error, never a silent 0.
        assert!(
            parse_range_value("abc", 0.0, 100.0).is_err(),
            "non-numeric must fail"
        );
        assert!(
            parse_range_value("", 0.0, 100.0).is_err(),
            "empty must fail"
        );
        // Non-finite floats are rejected even though they parse.
        assert!(
            parse_range_value("NaN", 0.0, 100.0).is_err(),
            "NaN must fail"
        );
        assert!(
            parse_range_value("inf", 0.0, 100.0).is_err(),
            "inf must fail"
        );
        // Out-of-range values name the bound.
        let e = parse_range_value("200", 0.0, 100.0).unwrap_err();
        assert!(
            e.contains("out of range") && e.contains("0..100"),
            "range error must name the bound, got: {e}"
        );
        assert!(parse_range_value("-1", 0.0, 100.0).is_err());
    }

    #[test]
    fn expand_state_names_match_windows_numbering() {
        // windows 0.61 ExpandCollapseState: Collapsed=0, Expanded=1,
        // PartiallyExpanded=2, LeafNode=3.
        assert_eq!(expand_state_name(0), "Collapsed");
        assert_eq!(expand_state_name(1), "Expanded");
        assert_eq!(expand_state_name(2), "PartiallyExpanded");
        assert_eq!(expand_state_name(3), "LeafNode");
        assert_eq!(expand_state_name(99), "Unknown");
    }

    #[test]
    fn scroll_pattern_args_map_sign_to_small_step() {
        // Positive/negative amounts become small inc/dec on the requested
        // axis, NoAmount on the other; zero scrolls nothing.
        assert_eq!(scroll_pattern_args("v", 1), Ok((2, 4)));
        assert_eq!(scroll_pattern_args("v", -3), Ok((2, 1)));
        assert_eq!(scroll_pattern_args("h", 2), Ok((4, 2)));
        assert_eq!(scroll_pattern_args("h", -1), Ok((1, 2)));
        assert_eq!(scroll_pattern_args("v", 0), Ok((2, 2)));
        // Axis vocabulary matches the wheel path.
        assert!(scroll_pattern_args("x", 1).is_err());
    }
}
