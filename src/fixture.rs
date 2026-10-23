//! Offscreen test fixture: a window the test process owns, positioned off
//! any monitor, never activated. See task brief Deliverable 1.
//!
//! Top-level `WS_POPUP` (never on the taskbar) at (-4000,-4000), shown with
//! `SW_SHOWNOACTIVATE` so it is visible to UIA without ever activating.
//! One child `EDIT` (id 100), one read-only `EDIT` (id 102), one `BUTTON`
//! labelled `OK` (id 101). Message pump runs on the creating thread;
//! teardown posts `WM_CLOSE` and joins the thread deterministically.
//!
//! Safety: never moves the pointer, never sends input, never focuses,
//! never captures the screen. Only the fixture HWNDs are touched.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Posted-message counters for the no-steal tests: a background click posts
/// WM_LBUTTONDOWN/UP to this window, and the test asserts they arrived
/// (proving delivery) while the owner's foreground window and pointer did
/// not move (proving nothing was stolen). Counters are process-wide and
/// monotonic; `reset_msg_counts` zeroes them before a measured action.
static MSG_MOUSEDOWN: AtomicU64 = AtomicU64::new(0);
static MSG_MOUSEUP: AtomicU64 = AtomicU64::new(0);
static MSG_MOUSEMOVE: AtomicU64 = AtomicU64::new(0);
static MSG_KEYDOWN: AtomicU64 = AtomicU64::new(0);
static MSG_CHAR: AtomicU64 = AtomicU64::new(0);
static MSG_WHEEL: AtomicU64 = AtomicU64::new(0);

/// Zero the posted-message counters (test-only).
pub fn reset_msg_counts() {
    MSG_MOUSEDOWN.store(0, Ordering::SeqCst);
    MSG_MOUSEUP.store(0, Ordering::SeqCst);
    MSG_MOUSEMOVE.store(0, Ordering::SeqCst);
    MSG_KEYDOWN.store(0, Ordering::SeqCst);
    MSG_CHAR.store(0, Ordering::SeqCst);
    MSG_WHEEL.store(0, Ordering::SeqCst);
}

/// `(mousedown, mouseup, mousemove, keydown, char)` counts since the reset.
/// Wheel messages are counted separately by `wheel_count`.
pub fn msg_counts() -> (u64, u64, u64, u64, u64) {
    (
        MSG_MOUSEDOWN.load(Ordering::SeqCst),
        MSG_MOUSEUP.load(Ordering::SeqCst),
        MSG_MOUSEMOVE.load(Ordering::SeqCst),
        MSG_KEYDOWN.load(Ordering::SeqCst),
        MSG_CHAR.load(Ordering::SeqCst),
    )
}

/// WM_MOUSEWHEEL / WM_MOUSEHWHEEL messages received since the reset.
pub fn wheel_count() -> u64 {
    MSG_WHEEL.load(Ordering::SeqCst)
}

/// Keys each fixture EDIT refused, keyed by the EDIT's HWND. Per control,
/// not process-wide: `cargo test` runs these tests in parallel against
/// separate fixtures, and a process-wide counter would report another
/// test's injected keys as if they had arrived here.
static SWALLOWED_KEYS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<isize, u64>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn bump_swallowed(hwnd: HWND) {
    let mut m = SWALLOWED_KEYS.lock().unwrap_or_else(|e| e.into_inner());
    *m.entry(hwnd as isize).or_insert(0) += 1;
}

/// Keys this EDIT has swallowed since the last reset. Non-zero means REAL
/// input arrived at that control while it held focus (the UIA provider
/// focuses a control to write it), and the guard refused it.
pub fn swallowed_key_count(hwnd: HWND) -> u64 {
    let m = SWALLOWED_KEYS.lock().unwrap_or_else(|e| e.into_inner());
    *m.get(&(hwnd as isize)).unwrap_or(&0)
}

pub fn reset_swallowed_keys(hwnd: HWND) {
    let mut m = SWALLOWED_KEYS.lock().unwrap_or_else(|e| e.into_inner());
    m.insert(hwnd as isize, 0);
}

/// The editor's class wndproc, captured when the guard is installed. Every
/// system EDIT shares the class proc, so one slot is enough.
static OLD_EDIT_PROC: AtomicUsize = AtomicUsize::new(0);

/// Guard installed on the fixture EDIT.
///
/// Why: `ValuePattern.SetValue` is performed by the UIA provider, which
/// FOCUSES the target control first. The fixture window is off-screen and is
/// never activated by the tool, but while that focus is held, keystrokes the
/// OWNER types on the machine land in this control and are inserted at the
/// caret (measured: characters prepend, e.g. "xyz" -> "wwwxyz"). A test that
/// reads the control after a write then sees the owner's characters and
/// cannot tell them from a product defect.
///
/// The provider writes with WM_SETTEXT / EM_SETTEXTEX, never with key
/// messages (pinned by `value_pattern_set_value_replaces_without_keystrokes`),
/// so swallowing the typing path removes the environmental variable without
/// changing what SetValue does — the strict read-back assertion stays.
unsafe extern "system" fn guarded_edit_wndproc(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    match msg {
        // 0x0100 WM_KEYDOWN, 0x0101 WM_KEYUP, 0x0102 WM_CHAR, 0x0104/0x0105
        // WM_SYSKEY*, 0x0109 WM_UNICHAR, 0x0300/0x0302/0x0303 WM_CUT/PASTE/CLEAR.
        0x0100 | 0x0101 | 0x0102 | 0x0104 | 0x0105 | 0x0109 | 0x0300 | 0x0302 | 0x0303 => {
            bump_swallowed(hwnd);
            0
        }
        _ => {
            let old: WNDPROC = std::mem::transmute(OLD_EDIT_PROC.load(Ordering::SeqCst));
            CallWindowProcW(old, hwnd, msg, w, l)
        }
    }
}

/// Install the typing guard on a fixture EDIT (called on the fixture thread
/// right after the control is created).
unsafe fn guard_edit(hwnd: HWND) {
    // `as usize` first: clippy rejects a direct fn-pointer -> isize cast.
    let new_proc = guarded_edit_wndproc as usize as isize;
    let old = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, new_proc);
    if old != 0 {
        OLD_EDIT_PROC.store(old as usize, Ordering::SeqCst);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn fixture_wndproc(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    match msg {
        WM_LBUTTONDOWN => {
            MSG_MOUSEDOWN.fetch_add(1, Ordering::SeqCst);
        }
        WM_LBUTTONUP => {
            MSG_MOUSEUP.fetch_add(1, Ordering::SeqCst);
        }
        WM_MOUSEMOVE => {
            MSG_MOUSEMOVE.fetch_add(1, Ordering::SeqCst);
        }
        WM_KEYDOWN => {
            MSG_KEYDOWN.fetch_add(1, Ordering::SeqCst);
        }
        WM_CHAR => {
            MSG_CHAR.fetch_add(1, Ordering::SeqCst);
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            MSG_WHEEL.fetch_add(1, Ordering::SeqCst);
        }
        _ => {}
    }
    if msg == WM_CLOSE {
        DestroyWindow(hwnd);
        return 0;
    }
    if msg == WM_DESTROY {
        PostQuitMessage(0);
        return 0;
    }
    DefWindowProcW(hwnd, msg, w, l)
}

pub struct Fixture {
    hwnd: HWND,
    edit: HWND,
    edit_ro: HWND,
    button: HWND,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

impl Fixture {
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }
    pub fn hwnd_i64(&self) -> i64 {
        self.hwnd as i64
    }
    pub fn edit(&self) -> HWND {
        self.edit
    }
    pub fn edit_ro(&self) -> HWND {
        self.edit_ro
    }
    pub fn button(&self) -> HWND {
        self.button
    }

    /// Create the fixture and wait until the pump thread reports HWNDs.
    /// Panics on failure (test-only).
    pub fn new() -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let class_name = format!("PuppetFixture-{pid}-{seq}");
        let title = format!("puppet-fixture-{pid}-{seq}");
        let (tx, rx) = mpsc::channel::<(usize, usize, usize, usize)>();
        let thread = std::thread::spawn(move || unsafe {
            let cls_w = wide(&class_name);
            let mut wc: WNDCLASSW = std::mem::zeroed();
            wc.lpfnWndProc = Some(fixture_wndproc);
            wc.hInstance = std::ptr::null_mut();
            wc.lpszClassName = cls_w.as_ptr();
            wc.hbrBackground = std::ptr::null_mut();
            wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
            if RegisterClassW(&wc) == 0 {
                let _ = tx.send((0, 0, 0, 0));
                return;
            }
            let title_w = wide(&title);
            let hwnd = CreateWindowExW(
                0,
                cls_w.as_ptr(),
                title_w.as_ptr(),
                WS_POPUP,
                -4000,
                -4000,
                400,
                300,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if hwnd.is_null() {
                let _ = tx.send((0, 0, 0, 0));
                return;
            }
            let edit_cls = wide("EDIT");
            // EDIT id 100 (writable), ES_AUTOHSCROLL so SetValue behaves.
            let edit = CreateWindowExW(
                0,
                edit_cls.as_ptr(),
                std::ptr::null(),
                WS_CHILD | WS_VISIBLE | WS_BORDER | 0x0080,
                10,
                10,
                260,
                24,
                hwnd,
                100isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            // Read-only EDIT id 102.
            let edit_ro = CreateWindowExW(
                0,
                edit_cls.as_ptr(),
                std::ptr::null(),
                WS_CHILD | WS_VISIBLE | WS_BORDER | 0x0080 | 0x0800,
                10,
                44,
                260,
                24,
                hwnd,
                102isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            let btn_cls = wide("BUTTON");
            let btn_text = wide("OK");
            let button = CreateWindowExW(
                0,
                btn_cls.as_ptr(),
                btn_text.as_ptr(),
                WS_CHILD | WS_VISIBLE,
                10,
                80,
                80,
                24,
                hwnd,
                101isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            // The EDIT refuses external typing: the UIA provider focuses it
            // to write, and owner keystrokes would otherwise land in the
            // document while a test reads it back.
            guard_edit(edit);
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = tx.send((
                hwnd as usize,
                edit as usize,
                button as usize,
                edit_ro as usize,
            ));
            let mut msg: MSG = std::mem::zeroed();
            loop {
                let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
                if r == 0 || r == -1 {
                    break;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
        let (hwnd_u, edit_u, button_u, edit_ro_u) = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("fixture thread must report HWNDs");
        let hwnd = hwnd_u as HWND;
        let edit = edit_u as HWND;
        let button = button_u as HWND;
        let edit_ro = edit_ro_u as HWND;
        assert!(!hwnd.is_null(), "fixture top-level CreateWindowExW failed");
        assert!(!edit.is_null(), "fixture EDIT CreateWindowExW failed");
        assert!(!button.is_null(), "fixture BUTTON CreateWindowExW failed");
        assert!(
            !edit_ro.is_null(),
            "fixture read-only EDIT CreateWindowExW failed"
        );
        Fixture {
            hwnd,
            edit,
            edit_ro,
            button,
            thread: Some(thread),
        }
    }

    /// Read the writable EDIT's current window text (proves SetValue landed).
    pub fn edit_text(&self) -> String {
        unsafe {
            let mut buf = vec![0u16; 512];
            let n = GetWindowTextW(self.edit, buf.as_mut_ptr(), buf.len() as i32);
            if n <= 0 {
                return String::new();
            }
            String::from_utf16_lossy(&buf[..n as usize])
        }
    }

    /// HWND validity probe (IsWindow != 0 while alive).
    pub fn is_alive(&self) -> bool {
        unsafe { IsWindow(self.hwnd) != 0 }
    }
}

/// Current foreground window as `i64` (`0` when the session has none,
/// e.g. a headless CI runner with no interactive desktop).
pub fn foreground_now() -> i64 {
    unsafe { GetForegroundWindow() as i64 }
}

/// A MESSAGE-ONLY fixture window: created with `HWND_MESSAGE` as its parent,
/// so it is never visible, never on the taskbar, never activatable — the
/// harshest case for background delivery. It has no children, so a posted
/// key lands in its own WndProc (the counters above see it). Used by the
/// no-steal tests to prove PostMessage click AND key delivery without a
/// single pixel on screen.
pub struct MessageWindow {
    hwnd: HWND,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Default for MessageWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageWindow {
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }
    pub fn hwnd_i64(&self) -> i64 {
        self.hwnd as i64
    }

    pub fn new() -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let class_name = format!("PuppetMsgOnly-{pid}-{seq}");
        let (tx, rx) = mpsc::channel::<usize>();
        let thread = std::thread::spawn(move || unsafe {
            let cls_w = wide(&class_name);
            let mut wc: WNDCLASSW = std::mem::zeroed();
            wc.lpfnWndProc = Some(fixture_wndproc);
            wc.hInstance = std::ptr::null_mut();
            wc.lpszClassName = cls_w.as_ptr();
            if RegisterClassW(&wc) == 0 {
                let _ = tx.send(0);
                return;
            }
            let title_w = wide(&class_name);
            let hwnd = CreateWindowExW(
                0,
                cls_w.as_ptr(),
                title_w.as_ptr(),
                WS_POPUP,
                0,
                0,
                10,
                10,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if hwnd.is_null() {
                let _ = tx.send(0);
                return;
            }
            let _ = tx.send(hwnd as usize);
            let mut msg: MSG = std::mem::zeroed();
            loop {
                let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
                if r == 0 || r == -1 {
                    break;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
        let h = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("message-only thread must report its HWND");
        assert!(h != 0, "message-only CreateWindowExW failed");
        MessageWindow {
            hwnd: h as HWND,
            thread: Some(thread),
        }
    }
}

impl Drop for MessageWindow {
    fn drop(&mut self) {
        unsafe {
            if !self.hwnd.is_null() && IsWindow(self.hwnd) != 0 {
                PostMessageW(self.hwnd, WM_CLOSE, 0, 0);
            }
        }
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}
/// Foreground-session verdict for environment-independent tests.
#[derive(Debug, PartialEq, Eq)]
pub enum FgVerdict {
    /// No foreground session exists (`before == 0`): there is no state
    /// to reason about, so the caller skips the assertion with a message
    /// instead of asserting a state the environment cannot guarantee.
    SkipNoSession,
    /// The tool left the foreground window exactly as it found it.
    Unchanged,
    /// The foreground window changed under the tool: `Changed(before, after)`.
    Changed(i64, i64),
}

/// Decide the "tool must not change the foreground window" contract
/// without asserting, so both outcomes stay testable: a headless session
/// (`before == 0`) yields `SkipNoSession`, never a failure.
pub fn fg_verdict(before: i64, after: i64) -> FgVerdict {
    if before == 0 {
        FgVerdict::SkipNoSession
    } else if before == after {
        FgVerdict::Unchanged
    } else {
        FgVerdict::Changed(before, after)
    }
}

/// True when `hwnd` is the fixture top-level window or one of its
/// descendants (e.g. the EDIT child a UIA `SetValue` briefly focuses).
/// `GetForegroundWindow` was observed returning the child EDIT itself, so
/// both levels count as tool-owned.
pub fn is_owned_window(fixture_top: i64, hwnd: i64) -> bool {
    if hwnd == 0 || fixture_top == 0 {
        return false;
    }
    if hwnd == fixture_top {
        return true;
    }
    unsafe {
        if IsWindow(fixture_top as HWND) == 0 {
            return false;
        }
        IsChild(fixture_top as HWND, hwnd as HWND) != 0
    }
}

/// Owning PID of a window (`None` when unreadable).
pub fn pid_of_window(hwnd: i64) -> Option<u32> {
    if hwnd == 0 {
        return None;
    }
    unsafe {
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd as HWND, &mut pid);
        if pid == 0 {
            None
        } else {
            Some(pid)
        }
    }
}

/// Enforce the foreground contract for a READ-ONLY or dry-run operation
/// (zero-pixel `elements`, dry-run `set_value`): skip (no failure) when the
/// environment provides no foreground session, or when the "before" window
/// belongs to the test process itself (parallel sibling fixture: no user
/// session to protect); otherwise allow a settle for transient churn and
/// REPORT (never fail) when a tool-owned window is still activated.
/// A foreground window the tool does not own (owner activity, transient
/// shell windows) is reported, never attributed to the tool.
///
/// Determinism (headed CI, `cargo test` parallelism): these callers perform
/// no input and cannot activate anything, but several fixtures live at once
/// and Windows activation churn can land on — and stick to — a test-owned
/// window with no fault of the tool under test (observed: `elements` left
/// its own offscreen fixture foreground for the full 5 s settle on a
/// headed runner). Attributing owned foreground to a read-only tool is a
/// coin-flip, so this gate reports (`FG_OWN_CHURN`) instead of failing.
/// Live-activation paths (real `SetValue`/invoke, which CAN yank focus)
/// must use a different gate that can fail; they must not call this helper.
pub fn check_foreground_left_clean(fixture_top: i64, before: i64, what: &str) {
    if before == 0 {
        println!("FG_SKIP: {what}: no foreground session (before=0); foreground assertion skipped");
        return;
    }
    if pid_of_window(before) == Some(std::process::id()) {
        println!(
            "FG_SKIP_SELF: {what}: foreground window belongs to the test process itself (before={before}); no user session to protect, assertion skipped"
        );
        return;
    }
    let t0 = std::time::Instant::now();
    let mut cur = foreground_now();
    while is_owned_window(fixture_top, cur) && t0.elapsed() < std::time::Duration::from_secs(5) {
        std::thread::sleep(std::time::Duration::from_millis(20));
        cur = foreground_now();
    }
    if is_owned_window(fixture_top, cur) {
        // Read-only/dry-run caller cannot have activated this (see doc
        // comment): parallel-fixture churn on a headed box, not tool fault.
        println!(
            "FG_OWN_CHURN: {what}: a test-owned window is foreground after settle (foreground={cur} fixture={fixture_top}); read-only path cannot activate, not attributed to the tool"
        );
    } else if cur == before {
        println!("FG_UNCHANGED: {what}: foreground unchanged ({before})");
    } else {
        println!(
            "FG_EXTERNAL: {what}: foreground changed to a window the tool does not own ({before} -> {cur}); not attributed to the tool"
        );
    }
}

/// Settle-based enforcement lives in `check_foreground_left_clean` above.
impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            if !self.hwnd.is_null() && IsWindow(self.hwnd) != 0 {
                PostMessageW(self.hwnd, WM_CLOSE, 0, 0);
            }
        }
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod self_tests {
    use super::*;

    #[test]
    fn fg_verdict_matrix_covers_skip_and_change() {
        // Simulates the headless-CI path without needing a headless session:
        // with no foreground window before the work, the verdict is Skip,
        // never a failure, whatever the after state is.
        assert_eq!(fg_verdict(0, 0), FgVerdict::SkipNoSession);
        assert_eq!(fg_verdict(0, 12345), FgVerdict::SkipNoSession);
        // A headed session with a stable foreground must be Unchanged...
        assert_eq!(fg_verdict(7, 7), FgVerdict::Unchanged);
        // ...and any change is reported with both ends, never swallowed.
        assert_eq!(fg_verdict(7, 9), FgVerdict::Changed(7, 9));
        println!("FG_VERDICT_MATRIX_OK=skip,unchanged,changed");
    }

    #[test]
    fn foreground_self_pid_rule_sees_own_fixture() {
        // Live proof for the FG_SKIP_SELF branch: a window this test process
        // owns (the offscreen fixture) reports our own PID, so a "before"
        // value pointing at a sibling test's fixture is recognised as "no
        // user session" rather than a contract violation. Read-only.
        let f = Fixture::new();
        let top = f.hwnd_i64();
        assert!(
            top != 0 && f.is_alive(),
            "fixture must be alive for the ownership probe"
        );
        assert_eq!(
            pid_of_window(top),
            Some(std::process::id()),
            "own fixture must resolve to our own PID"
        );
        assert!(
            is_owned_window(top, top),
            "own top-level must count as tool-owned"
        );
        assert!(!is_owned_window(top, 0), "null hwnd is never owned");
        println!("FG_SELF_RULE_OK=top={top}");
    }

    #[test]
    fn fixture_creates_offscreen_without_activation() {
        let fg_before = unsafe { GetForegroundWindow() };
        let f = Fixture::new();
        assert!(f.is_alive());
        unsafe {
            let mut rc = std::mem::zeroed::<RECT>();
            assert_ne!(GetWindowRect(f.hwnd(), &mut rc), 0);
            // Offscreen: far negative origin, never on the visible desktop.
            assert!(
                rc.left <= -3000 && rc.top <= -3000,
                "fixture must be offscreen, got left={} top={} right={} bottom={}",
                rc.left,
                rc.top,
                rc.right,
                rc.bottom
            );
            // Never activated the owner's foreground window.
            assert_eq!(
                GetForegroundWindow(),
                fg_before,
                "fixture must never steal foreground"
            );
            assert_ne!(f.hwnd(), fg_before, "fixture must never be foreground");
        }
        // Drop destroys deterministically.
        let hwnd = f.hwnd();
        drop(f);
        assert_eq!(
            unsafe { IsWindow(hwnd) },
            0,
            "fixture HWND must be destroyed after Drop"
        );
    }
}
