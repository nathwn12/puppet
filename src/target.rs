//! Target lock: resolve ONE window from a structured selector, snapshot its
//! identity, and revalidate it before every action.
//!
//! Owner contract this module exists for: control must be exact and
//! background-safe. A call names its window once (`target`), the lock keeps
//! `hwnd` + `pid` + image name + class (+ AppUserModelID when asked for), and
//! every action revalidates that identity first. There is NO fallback to the
//! foreground window: an ambiguous or vanished target is a typed error
//! (`ambiguous_target` / `target_gone`) and the call does nothing.
//!
//! Coordinate convention (documented in README + tool descriptions):
//! 1. a `target` call takes `x`/`y` as CLIENT-AREA PHYSICAL PIXELS of the
//!    locked window (`GetClientRect` space, origin at the client top-left);
//! 2. a call without a target takes `x`/`y` as virtual-desktop screen pixels
//!    (the legacy SendInput path, still gated behind `allow_cursor`);
//! 3. window/element rects reported by the tools stay in SCREEN pixels, so an
//!    element centre is converted here with `screen_point_to_client`.
//!
//! This process is per-monitor DPI aware (main.rs), so client pixels are
//! physical device pixels on every monitor; `dpi` is reported for callers
//! that want logical units, and `scale_at_dpi` is the one conversion used.

use serde_json::{Map, Value};
use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub const AMBIGUOUS: &str = "ambiguous_target";
pub const GONE: &str = "target_gone";
pub const BAD: &str = "bad_target";

/// Typed target failure. `code` is the machine-readable kind, `message` the
/// human reason; `text()` is what tool results carry (`"<code>: <message>"`).
#[derive(Debug, Clone)]
pub struct TargetError {
    pub code: &'static str,
    pub message: String,
    pub candidates: Option<Vec<Map<String, Value>>>,
}

impl TargetError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        TargetError {
            code,
            message: message.into(),
            candidates: None,
        }
    }

    pub fn text(&self) -> String {
        format!("{}: {}", self.code, self.message)
    }

    /// JSON shape shared by every refusal (`code`, `error`, `candidates`).
    pub fn to_map(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("ok".to_string(), Value::from(false));
        m.insert("code".to_string(), Value::from(self.code));
        m.insert("error".to_string(), Value::from(self.text()));
        if let Some(c) = &self.candidates {
            m.insert(
                "candidates".to_string(),
                Value::Array(c.iter().cloned().map(Value::Object).collect()),
            );
        }
        m
    }
}

/// Structured target selector:
/// `{ hwnd?, pid?, exe?, appId?, titleContains?, className? }`.
/// Every present field must match (AND); `exe` is case-insensitive with or
/// without `.exe`, `appId` comes from the window's AppUserModelID property
/// store, `titleContains` is a case-insensitive substring and `className` a
/// case-insensitive equality. A plain selector string (`hwnd:1234`,
/// `exe:notepad.exe`, `title:...`, `class:...`, `pid:...`, bare digits) is
/// accepted as sugar and parsed with the same grammar the older tools use.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TargetSpec {
    pub hwnd: Option<i64>,
    pub pid: Option<u32>,
    pub exe: Option<String>,
    pub app_id: Option<String>,
    pub title_contains: Option<String>,
    pub class_name: Option<String>,
}

impl TargetSpec {
    pub fn parse(v: &Value) -> Result<Self, TargetError> {
        match v {
            Value::String(s) => Self::parse_selector_string(s),
            Value::Object(o) => Self::parse_object(o),
            _ => Err(TargetError::new(
                BAD,
                "target must be an object {hwnd?, pid?, exe?, appId?, titleContains?, className?} or a selector string",
            )),
        }
    }

    fn parse_selector_string(s: &str) -> Result<Self, TargetError> {
        let t = s.trim();
        if t.is_empty() {
            return Err(TargetError::new(BAD, "target string is empty"));
        }
        let (kind, val) = crate::parse_selector(t);
        let mut spec = TargetSpec::default();
        let want_int = |val: &str| -> Result<i64, TargetError> {
            crate::parse_int_loose(val).ok_or_else(|| {
                TargetError::new(BAD, format!("target has a non-integer value {val:?}"))
            })
        };
        match kind.as_str() {
            "hwnd" => spec.hwnd = Some(want_int(&val)?),
            "pid" => {
                let n = want_int(&val)?;
                if n < 0 || n > u32::MAX as i64 {
                    return Err(TargetError::new(BAD, format!("pid out of range: {n}")));
                }
                spec.pid = Some(n as u32);
            }
            "exe" => spec.exe = Some(val),
            "title" => spec.title_contains = Some(val),
            "class" => spec.class_name = Some(val),
            _ => {
                // Bare text: the legacy grammar treats bare digits as hwnd.
                match crate::parse_int_loose(t) {
                    Some(h) => spec.hwnd = Some(h),
                    None => spec.title_contains = Some(t.to_string()),
                }
            }
        }
        if spec.is_empty() {
            return Err(TargetError::new(BAD, "target selects nothing"));
        }
        Ok(spec)
    }

    fn parse_object(o: &Map<String, Value>) -> Result<Self, TargetError> {
        for k in o.keys() {
            if !["hwnd", "pid", "exe", "appId", "titleContains", "className"].contains(&k.as_str())
            {
                return Err(TargetError::new(
                    BAD,
                    format!("unknown target field {k:?} (want hwnd|pid|exe|appId|titleContains|className)"),
                ));
            }
        }
        let mut spec = TargetSpec::default();
        if let Some(v) = o.get("hwnd") {
            if !v.is_null() {
                spec.hwnd = Some(int_of(v, "hwnd")?);
            }
        }
        if let Some(v) = o.get("pid") {
            if !v.is_null() {
                let n = int_of(v, "pid")?;
                if n < 0 || n > u32::MAX as i64 {
                    return Err(TargetError::new(BAD, format!("pid out of range: {n}")));
                }
                spec.pid = Some(n as u32);
            }
        }
        if let Some(v) = o.get("exe") {
            if !v.is_null() {
                let s = string_of(v, "exe")?;
                if s.trim().is_empty() {
                    return Err(TargetError::new(BAD, "exe is empty"));
                }
                spec.exe = Some(s);
            }
        }
        if let Some(v) = o.get("appId") {
            if !v.is_null() {
                let s = string_of(v, "appId")?;
                if s.trim().is_empty() {
                    return Err(TargetError::new(BAD, "appId is empty"));
                }
                spec.app_id = Some(s);
            }
        }
        for (key, slot) in [
            ("titleContains", &mut spec.title_contains),
            ("className", &mut spec.class_name),
        ] {
            if let Some(v) = o.get(key) {
                if !v.is_null() {
                    let s = string_of(v, key)?;
                    if s.is_empty() {
                        return Err(TargetError::new(BAD, format!("{key} is empty")));
                    }
                    *slot = Some(s);
                }
            }
        }
        if spec.is_empty() {
            return Err(TargetError::new(
                BAD,
                "target needs at least one of hwnd|pid|exe|appId|titleContains|className",
            ));
        }
        Ok(spec)
    }

    pub fn is_empty(&self) -> bool {
        self.hwnd.is_none()
            && self.pid.is_none()
            && self.exe.is_none()
            && self.app_id.is_none()
            && self.title_contains.is_none()
            && self.class_name.is_none()
    }

    /// Stable, human-readable description used in results/notes.
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(h) = self.hwnd {
            parts.push(format!("hwnd={h}"));
        }
        if let Some(p) = self.pid {
            parts.push(format!("pid={p}"));
        }
        if let Some(e) = &self.exe {
            parts.push(format!("exe={e}"));
        }
        if let Some(a) = &self.app_id {
            parts.push(format!("appId={a}"));
        }
        if let Some(t) = &self.title_contains {
            parts.push(format!("titleContains={t}"));
        }
        if let Some(c) = &self.class_name {
            parts.push(format!("className={c}"));
        }
        parts.join(" ")
    }

    fn matches_row(&self, r: &Row) -> bool {
        if let Some(h) = self.hwnd {
            if r.hwnd != h {
                return false;
            }
        }
        if let Some(p) = self.pid {
            if r.pid != p {
                return false;
            }
        }
        if let Some(e) = &self.exe {
            if crate::exe_stem(&r.exe) != crate::exe_stem(e) {
                return false;
            }
        }
        if let Some(t) = &self.title_contains {
            if !r.title.to_lowercase().contains(&t.to_lowercase()) {
                return false;
            }
        }
        if let Some(c) = &self.class_name {
            if !r.class.eq_ignore_ascii_case(c) {
                return false;
            }
        }
        if let Some(a) = &self.app_id {
            match &r.app_id {
                Some(got) if got.eq_ignore_ascii_case(a) => {}
                _ => return false,
            }
        }
        true
    }
}

fn int_of(v: &Value, key: &str) -> Result<i64, TargetError> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_u64().and_then(|u| i64::try_from(u).ok()))
            .ok_or_else(|| TargetError::new(BAD, format!("{key} must be an integer"))),
        Value::String(s) => crate::parse_int_loose(s)
            .ok_or_else(|| TargetError::new(BAD, format!("{key} must be an integer"))),
        _ => Err(TargetError::new(BAD, format!("{key} must be an integer"))),
    }
}

fn string_of(v: &Value, key: &str) -> Result<String, TargetError> {
    match v {
        Value::String(s) => Ok(s.clone()),
        _ => Err(TargetError::new(BAD, format!("{key} must be a string"))),
    }
}

/// One enumerable top-level window with the identity fields the lock keeps.
#[derive(Clone, Debug)]
pub struct Row {
    pub hwnd: i64,
    pub pid: u32,
    /// Process creation time; `None` when unreadable.
    pub created: Option<u64>,
    pub exe: String,
    pub title: String,
    pub class: String,
    pub app_id: Option<String>,
}

fn row_of(hwnd: i64) -> Option<Row> {
    if hwnd == 0 {
        return None;
    }
    let h = hwnd as HWND;
    unsafe {
        if IsWindow(h) == 0 {
            return None;
        }
        let pid = crate::pid_of(h);
        Some(Row {
            hwnd,
            pid,
            created: crate::process_created(pid),
            exe: crate::exe_of_pid(pid),
            title: crate::win_text(h),
            class: crate::win_class(h),
            app_id: None,
        })
    }
}

/// Visible top-level windows, z-order. Hidden/tool windows are excluded so
/// `exe:`/`pid:` never picks a window the caller cannot see; an explicit
/// `hwnd` bypasses enumeration (fixtures, message-only windows).
pub fn rows() -> Vec<Row> {
    let mut out = Vec::new();
    for h in crate::enum_windows() {
        if unsafe { IsWindowVisible(h) } == 0 {
            continue;
        }
        if let Some(r) = row_of(h as i64) {
            out.push(r);
        }
    }
    out
}

fn candidate_map(r: &Row) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("hwnd".to_string(), Value::from(r.hwnd));
    m.insert("pid".to_string(), Value::from(r.pid as i64));
    m.insert("exe".to_string(), Value::from(r.exe.clone()));
    m.insert("title".to_string(), Value::from(r.title.clone()));
    m.insert("class".to_string(), Value::from(r.class.clone()));
    if let Some(a) = &r.app_id {
        m.insert("appId".to_string(), Value::from(a.clone()));
    }
    m
}

/// A resolved, identity-snapshotted window. Holding one is the only way the
/// acting tools learn an hwnd, so no verb can quietly retarget itself.
#[derive(Clone, Debug)]
pub struct TargetLock {
    pub hwnd: i64,
    pub pid: u32,
    /// Process creation time (FILETIME as u64) read with `GetProcessTimes`
    /// at lock time; 0 when unreadable. PIDs are recycled, so the pid alone
    /// cannot prove the process is the same one.
    pub created: u64,
    /// Image name as read at lock time (`notepad.exe`), compared by stem.
    pub exe: String,
    pub class: String,
    pub app_id: Option<String>,
    pub spec: TargetSpec,
}

impl TargetLock {
    /// Re-check identity before an action. A recycled hwnd (same handle,
    /// different process/class) is `target_gone`, never silently retargeted.
    pub fn revalidate(&self) -> Result<(), TargetError> {
        let h = self.hwnd as HWND;
        if self.hwnd == 0 || unsafe { IsWindow(h) } == 0 {
            return Err(TargetError::new(
                GONE,
                format!("target hwnd {} no longer exists", self.hwnd),
            ));
        }
        let pid = crate::pid_of(h);
        if pid != self.pid {
            return Err(TargetError::new(
                GONE,
                format!(
                    "target hwnd {} changed owner pid ({} -> {pid}); handle was recycled",
                    self.hwnd, self.pid
                ),
            ));
        }
        // PID alone is not identity: Windows recycles PIDs, so a reused pid
        // would otherwise pass every check below. `GetProcessTimes` gives the
        // creation time, which cannot be reused while the old process lives.
        // Unreadable at lock time (0) fails open, and an unreadable NOW is a
        // gone process (the window check above already caught the rest).
        if self.created != 0 {
            let now = crate::process_created(pid).unwrap_or(0);
            if now != self.created {
                return Err(TargetError::new(
                    GONE,
                    format!(
                        "target hwnd {} pid {pid} is a DIFFERENT process (creation time {} != \
                         {}); the pid was recycled",
                        self.hwnd, now, self.created
                    ),
                ));
            }
        }
        let exe = crate::exe_of_pid(pid);
        if crate::exe_stem(&exe) != crate::exe_stem(&self.exe) {
            return Err(TargetError::new(
                GONE,
                format!(
                    "target hwnd {} now belongs to {exe}, expected {}",
                    self.hwnd, self.exe
                ),
            ));
        }
        let class = unsafe { crate::win_class(h) };
        if class != self.class {
            return Err(TargetError::new(
                GONE,
                format!(
                    "target hwnd {} changed window class ({:?} -> {:?})",
                    self.hwnd, self.class, class
                ),
            ));
        }
        if let Some(want) = &self.app_id {
            match app_id_of(self.hwnd) {
                Some(got) if got.eq_ignore_ascii_case(want) => {}
                other => {
                    return Err(TargetError::new(
                        GONE,
                        format!(
                            "target hwnd {} changed AppUserModelID ({want:?} -> {other:?})",
                            self.hwnd
                        ),
                    ))
                }
            }
        }
        Ok(())
    }
}

/// Pick exactly one row from a candidate list. Ambiguity is an error with
/// the candidate list; nothing is guessed. Split from `lock` so the rule is
/// unit-testable with fabricated rows (no live windows needed).
pub fn resolve_from(spec: &TargetSpec, rows: Vec<Row>) -> Result<Row, TargetError> {
    let want_app_id = spec.app_id.is_some();
    let hits: Vec<Row> = rows
        .into_iter()
        .filter_map(|mut r| {
            if want_app_id && r.app_id.is_none() {
                r.app_id = app_id_of(r.hwnd);
            }
            if spec.matches_row(&r) {
                Some(r)
            } else {
                None
            }
        })
        .collect();
    match hits.len() {
        0 => Err(TargetError::new(
            GONE,
            format!(
                "no visible top-level window matches target ({})",
                spec.describe()
            ),
        )),
        1 => Ok(hits.into_iter().next().unwrap()),
        n => {
            let cands: Vec<Map<String, Value>> = hits.iter().map(candidate_map).collect();
            Err(TargetError {
                code: AMBIGUOUS,
                message: format!(
                    "{n} windows match target ({}); narrow it with hwnd/pid/titleContains/className",
                    spec.describe()
                ),
                candidates: Some(cands),
            })
        }
    }
}

/// Resolve a spec to exactly one window. Ambiguity is an error with the
/// candidate list; nothing is guessed and the foreground window is never a
/// fallback.
pub fn lock(spec: &TargetSpec) -> Result<TargetLock, TargetError> {
    if spec.is_empty() {
        return Err(TargetError::new(
            BAD,
            "target needs at least one of hwnd|pid|exe|appId|titleContains|className",
        ));
    }
    let row = match spec.hwnd {
        Some(h) => {
            let mut r = row_of(h).ok_or_else(|| {
                TargetError::new(
                    GONE,
                    format!("no window with hwnd {h} (closed or recycled)"),
                )
            })?;
            if spec.app_id.is_some() {
                r.app_id = app_id_of(h);
            }
            if !spec.matches_row(&r) {
                return Err(TargetError::new(
                    GONE,
                    format!(
                        "hwnd {h} does not match the rest of the target ({})",
                        spec.describe()
                    ),
                ));
            }
            r
        }
        None => resolve_from(spec, rows())?,
    };
    Ok(TargetLock {
        hwnd: row.hwnd,
        pid: row.pid,
        created: row.created.unwrap_or(0),
        exe: row.exe,
        class: row.class,
        app_id: row.app_id,
        spec: spec.clone(),
    })
}

/// `PKEY_AppUserModel_ID`: fmtid {9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3}, pid 5.
fn app_user_model_pkey() -> windows::Win32::Foundation::PROPERTYKEY {
    windows::Win32::Foundation::PROPERTYKEY {
        fmtid: windows::core::GUID::from_u128(0x9f4c_2855_9f79_4b39_a8d0_e1d4_2de1_d5f3),
        pid: 5,
    }
}

/// The window's AppUserModelID (property store). `None` when the store or
/// the property is unavailable (unpackaged apps often have none).
pub fn app_id_of(hwnd: i64) -> Option<String> {
    use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::PropertiesSystem::{
        IPropertyStore, SHGetPropertyStoreForWindow,
    };
    if hwnd == 0 {
        return None;
    }
    unsafe {
        // Best-effort COM init on whatever worker thread serves this call;
        // RPC_E_CHANGED_MODE (already MTA/STA) is fine — the call below does
        // not care which apartment the store marshals through.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let store: IPropertyStore = SHGetPropertyStoreForWindow::<IPropertyStore>(
            windows::Win32::Foundation::HWND(hwnd as *mut core::ffi::c_void),
        )
        .ok()?;
        let key = app_user_model_pkey();
        let mut pv: PROPVARIANT = store.GetValue(&key).ok()?;
        let vt = pv.Anonymous.Anonymous.vt;
        // VT_LPWSTR (31) is what PKEY_AppUserModel_ID stores; anything else
        // is reported as absent rather than guessed.
        let out = if vt == windows::Win32::System::Variant::VT_LPWSTR {
            let p = pv.Anonymous.Anonymous.Anonymous.pwszVal;
            if p.is_null() {
                None
            } else {
                Some(p.to_string().ok()?)
            }
        } else {
            None
        };
        let _ = PropVariantClear(&mut pv);
        out
    }
}

// ---------------------------------------------------------------- coordinates

/// Client rect in SCREEN coordinates, physical pixels:
/// `[left, top, right, bottom]` from `GetClientRect` + `ClientToScreen`.
pub fn client_rect_screen(hwnd: i64) -> Option<[i64; 4]> {
    if hwnd == 0 {
        return None;
    }
    unsafe {
        let h = hwnd as HWND;
        let mut rc = RECT::default();
        if GetClientRect(h, &mut rc) == 0 {
            return None;
        }
        let mut p0 = POINT { x: 0, y: 0 };
        let mut p1 = POINT {
            x: rc.right,
            y: rc.bottom,
        };
        if ClientToScreen(h, &mut p0) == 0 || ClientToScreen(h, &mut p1) == 0 {
            return None;
        }
        Some([p0.x as i64, p0.y as i64, p1.x as i64, p1.y as i64])
    }
}

/// Screen point -> client point (physical pixels). Exactly `ScreenToClient`.
pub fn screen_point_to_client(hwnd: i64, x: i64, y: i64) -> Option<(i64, i64)> {
    if hwnd == 0
        || !(i32::MIN as i64..=i32::MAX as i64).contains(&x)
        || !(i32::MIN as i64..=i32::MAX as i64).contains(&y)
    {
        return None;
    }
    unsafe {
        let mut pt = POINT {
            x: x as i32,
            y: y as i32,
        };
        if ScreenToClient(hwnd as HWND, &mut pt) == 0 {
            None
        } else {
            Some((pt.x as i64, pt.y as i64))
        }
    }
}

/// Client point -> screen point (physical pixels). Exactly `ClientToScreen`.
pub fn client_point_to_screen(hwnd: i64, cx: i64, cy: i64) -> Option<(i64, i64)> {
    if hwnd == 0
        || !(i32::MIN as i64..=i32::MAX as i64).contains(&cx)
        || !(i32::MIN as i64..=i32::MAX as i64).contains(&cy)
    {
        return None;
    }
    unsafe {
        let mut pt = POINT {
            x: cx as i32,
            y: cy as i32,
        };
        if ClientToScreen(hwnd as HWND, &mut pt) == 0 {
            None
        } else {
            Some((pt.x as i64, pt.y as i64))
        }
    }
}

pub fn dpi_of(hwnd: i64) -> u32 {
    if hwnd == 0 {
        return 96;
    }
    unsafe { GetDpiForWindow(hwnd as HWND) }
}

/// Pure conversion used by the tests and the docs: physical pixels per
/// logical pixel at `dpi` (96 = 1.0). Client coordinates stay physical; this
/// is only for callers that want logical units.
pub fn scale_at_dpi(dpi: u32) -> f64 {
    dpi as f64 / 96.0
}

/// Pure client-origin offset: screen = client origin + client point. The live
/// path gets the same answer from `ClientToScreen`; the unit test pins that
/// the arithmetic and the OS agree.
pub fn offset_point(origin: (i64, i64), p: (i64, i64)) -> (i64, i64) {
    (origin.0 + p.0, origin.1 + p.1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spec_parses_every_field() {
        let s = TargetSpec::parse(&json!({
            "hwnd": 4321,
            "pid": 99,
            "exe": "Notepad.EXE",
            "appId": "Microsoft.Windows.Notepad",
            "titleContains": "untitled",
            "className": "Notepad"
        }))
        .expect("object spec parses");
        assert_eq!(s.hwnd, Some(4321));
        assert_eq!(s.pid, Some(99));
        assert_eq!(s.exe.as_deref(), Some("Notepad.EXE"));
        assert_eq!(s.app_id.as_deref(), Some("Microsoft.Windows.Notepad"));
        assert_eq!(s.title_contains.as_deref(), Some("untitled"));
        assert_eq!(s.class_name.as_deref(), Some("Notepad"));
    }

    #[test]
    fn spec_rejects_unknown_and_empty() {
        let e = TargetSpec::parse(&json!({"windows": 1})).unwrap_err();
        assert_eq!(e.code, BAD);
        assert!(e.text().contains("unknown target field"), "{}", e.text());
        let e = TargetSpec::parse(&json!({})).unwrap_err();
        assert_eq!(e.code, BAD);
        assert!(e.text().contains("at least one"), "{}", e.text());
        let e = TargetSpec::parse(&json!(7)).unwrap_err();
        assert_eq!(e.code, BAD);
    }

    #[test]
    fn spec_accepts_selector_string_sugar() {
        let s = TargetSpec::parse(&json!("hwnd:1234")).unwrap();
        assert_eq!(s.hwnd, Some(1234));
        let s = TargetSpec::parse(&json!("exe:notepad.exe")).unwrap();
        assert_eq!(s.exe.as_deref(), Some("notepad.exe"));
        let s = TargetSpec::parse(&json!("title:Untitled")).unwrap();
        assert_eq!(s.title_contains.as_deref(), Some("Untitled"));
        let s = TargetSpec::parse(&json!("1234")).unwrap();
        assert_eq!(s.hwnd, Some(1234), "bare digits are an hwnd, as before");
    }

    fn row(hwnd: i64, pid: u32, exe: &str, title: &str, class: &str) -> Row {
        Row {
            hwnd,
            pid,
            created: None,
            exe: exe.to_string(),
            title: title.to_string(),
            class: class.to_string(),
            app_id: None,
        }
    }

    #[test]
    fn matching_is_and_case_insensitive() {
        let r = row(1, 2, "notepad.exe", "Untitled - Notepad", "Notepad");
        let s = TargetSpec {
            exe: Some("NOTEPAD".to_string()),
            title_contains: Some("untitled".to_string()),
            class_name: Some("notepad".to_string()),
            pid: Some(2),
            ..TargetSpec::default()
        };
        assert!(s.matches_row(&r));
        let s = TargetSpec { pid: Some(3), ..s };
        assert!(!s.matches_row(&r), "every present field must match");
    }

    #[test]
    fn app_id_filter_requires_an_exact_store_value() {
        let mut r = row(1, 2, "app.exe", "t", "C");
        let s = TargetSpec {
            app_id: Some("Microsoft.Windows.Notepad".to_string()),
            ..TargetSpec::default()
        };
        assert!(!s.matches_row(&r), "no appId on the window -> no match");
        r.app_id = Some("microsoft.windows.notepad".to_string());
        assert!(s.matches_row(&r), "appId comparison is case-insensitive");
    }

    #[test]
    fn ambiguity_reports_candidates_and_never_picks_one() {
        // Two fabricated rows share an image name: the resolver must refuse
        // with both candidates, never pick the first/foreground one.
        let rows = vec![row(1, 2, "a.exe", "A", "C1"), row(3, 4, "a.exe", "B", "C2")];
        let mut spec = TargetSpec {
            exe: Some("a.exe".to_string()),
            ..TargetSpec::default()
        };
        let e = resolve_from(&spec, rows.clone()).expect_err("must be ambiguous");
        assert_eq!(e.code, AMBIGUOUS);
        let cands = e.candidates.clone().expect("candidate list");
        assert_eq!(cands.len(), 2);
        let m = e.to_map();
        assert_eq!(m["code"], json!(AMBIGUOUS));
        assert_eq!(m["candidates"].as_array().map(|a| a.len()), Some(2));

        // Zero hits is `target_gone`, one hit is the answer.
        spec.exe = Some("nope.exe".to_string());
        assert_eq!(resolve_from(&spec, rows.clone()).unwrap_err().code, GONE);
        spec.exe = Some("a.exe".to_string());
        spec.title_contains = Some("b".to_string());
        let one = resolve_from(&spec, rows).expect("case-insensitive single hit");
        assert_eq!(one.hwnd, 3);
    }

    #[test]
    fn gone_and_bad_are_distinct_codes() {
        assert_eq!(TargetError::new(GONE, "x").text(), "target_gone: x");
        assert_eq!(TargetError::new(BAD, "y").text(), "bad_target: y");
    }

    #[test]
    fn coordinate_math_matches_the_documented_convention() {
        // Screen = client origin + client point; the live assertion lives in
        // the fixture test, which compares this to ClientToScreen.
        assert_eq!(offset_point((100, 50), (10, 20)), (110, 70));
        assert_eq!(offset_point((-1920, -10), (0, 0)), (-1920, -10));
        assert_eq!(scale_at_dpi(96), 1.0);
        assert_eq!(scale_at_dpi(144), 1.5);
        assert_eq!(scale_at_dpi(192), 2.0);
    }

    /// The PID alone is not identity: the same hwnd + pid with a different
    /// process creation time is a recycled PID and must be refused.
    #[test]
    fn pid_reuse_with_a_different_creation_time_is_rejected() {
        let f = crate::fixture::Fixture::new();
        let top = f.hwnd_i64();
        let spec = TargetSpec {
            hwnd: Some(top),
            ..TargetSpec::default()
        };
        let l = lock(&spec).expect("hwnd lock resolves");
        assert!(l.created != 0, "GetProcessTimes must yield a creation time");
        l.revalidate().expect("the real lock validates");
        // Same hwnd, same pid, different creation time: a forged recycled PID.
        let mut forged = l.clone();
        forged.created = l.created.wrapping_add(1);
        let e = forged
            .revalidate()
            .expect_err("a reused pid must be refused");
        assert_eq!(e.code, GONE, "{}", e.text());
        assert!(
            e.text().contains("creation time"),
            "the refusal must name the creation time: {}",
            e.text()
        );
        println!("PID_REUSE_OK created={} refusal={}", l.created, e.text());
    }

    /// LIVE: the lock is identity, not foreground state — it stays valid
    /// while the target is NOT the foreground window (the alt-tab case).
    #[test]
    fn live_lock_survives_the_target_being_backgrounded() {
        let f = crate::fixture::Fixture::new();
        let top = f.hwnd_i64();
        let l = lock(&TargetSpec {
            hwnd: Some(top),
            ..TargetSpec::default()
        })
        .expect("fixture locks");
        let fg = crate::fixture::foreground_now();
        assert_ne!(
            fg, top,
            "the fixture must not be foreground for this proof to mean anything"
        );
        l.revalidate()
            .expect("a backgrounded target still validates (identity, not focus)");
        println!("LOCK_BACKGROUND_OK target={top} foreground={fg}");
    }

    /// Live: the AppUserModelID property store is read, and an appId selector
    /// resolves through it. Tolerant by design — a machine with no packaged
    /// windows reports the skip instead of pretending.
    #[test]
    fn live_app_id_resolves_through_the_property_store() {
        let mut sample: Option<(String, i64)> = None;
        for r in rows().into_iter().take(40) {
            if let Some(a) = app_id_of(r.hwnd) {
                if !a.trim().is_empty() {
                    sample = Some((a, r.hwnd));
                    break;
                }
            }
        }
        let Some((app_id, hwnd)) = sample else {
            println!("APPID_SKIP: no visible window exposes an AppUserModelID on this desktop");
            return;
        };
        let spec = TargetSpec {
            app_id: Some(app_id.clone()),
            ..TargetSpec::default()
        };
        match lock(&spec) {
            Ok(l) => {
                assert_eq!(
                    app_id_of(l.hwnd).as_deref(),
                    Some(app_id.as_str()),
                    "resolved window must carry the requested appId"
                );
                println!("APPID_OK appId={app_id} hwnd={}", l.hwnd);
            }
            Err(e) => {
                // Several windows of the same packaged app share the id:
                // ambiguity is the correct typed answer, never a guess.
                assert_eq!(e.code, AMBIGUOUS, "{}", e.text());
                assert!(e.candidates.is_some());
                println!(
                    "APPID_AMBIGUOUS_OK appId={app_id} hwnd={hwnd} ({})",
                    e.text()
                );
            }
        }
    }

    /// Live: a real (offscreen, never-activated) fixture window. Proves the
    /// OS agrees with the documented conversion and that resolution by
    /// hwnd/pid/class finds the same window without touching the foreground.
    #[test]
    fn live_lock_resolves_and_revalidates_the_fixture() {
        let f = crate::fixture::Fixture::new();
        let top = f.hwnd_i64();
        let fg_before = crate::fixture::foreground_now();
        let spec = TargetSpec {
            hwnd: Some(top),
            ..TargetSpec::default()
        };
        let l = lock(&spec).expect("hwnd lock resolves");
        assert_eq!(l.hwnd, top);
        assert_eq!(l.pid, std::process::id());
        l.revalidate().expect("fresh lock revalidates");

        // pid + class describe the same single window.
        let by_class = TargetSpec {
            pid: Some(std::process::id()),
            class_name: Some(l.class.clone()),
            ..TargetSpec::default()
        };
        let again = lock(&by_class).expect("pid + exact class resolves the fixture");
        assert_eq!(again.hwnd, top);

        // Disagreement inside an hwnd target is refused, not retargeted.
        let bad = TargetSpec {
            hwnd: Some(top),
            pid: Some(std::process::id() + 1),
            ..TargetSpec::default()
        };
        let e = lock(&bad).expect_err("hwnd + wrong pid must not resolve");
        assert_eq!(e.code, GONE, "{}", e.text());

        // Gone: a destroyed window handle.
        drop(f);
        let e = lock(&TargetSpec {
            hwnd: Some(top),
            ..TargetSpec::default()
        })
        .expect_err("destroyed hwnd is gone");
        assert_eq!(e.code, GONE);

        if fg_before != 0 {
            assert_eq!(
                crate::fixture::foreground_now(),
                fg_before,
                "resolution must never change the foreground window"
            );
        }
    }

    /// Live: the coordinate conversion is the OS's, not an approximation.
    #[test]
    fn live_client_conversion_matches_client_to_screen() {
        let f = crate::fixture::Fixture::new();
        let top = f.hwnd_i64();
        let rect = client_rect_screen(top).expect("client rect readable");
        let origin = screen_point_to_client(top, rect[0], rect[1]).expect("origin maps");
        assert_eq!(origin, (0, 0), "client rect origin maps to client (0,0)");
        for (cx, cy) in [(0, 0), (10, 10), (120, 24)] {
            let screen = client_point_to_screen(top, cx, cy).expect("client->screen");
            assert_eq!(
                screen,
                offset_point((rect[0], rect[1]), (cx, cy)),
                "ClientToScreen must equal origin + client point"
            );
            let back = screen_point_to_client(top, screen.0, screen.1).expect("screen->client");
            assert_eq!(back, (cx, cy), "round trip is exact");
        }
        assert!(dpi_of(top) >= 48, "DPI is readable for a real window");
    }
}
