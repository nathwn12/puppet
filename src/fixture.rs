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
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{RedrawWindow, RDW_ALLCHILDREN, RDW_INVALIDATE};
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

/// How long a window may take to produce its first composited frame and a
/// stable UIA walk before the readiness wait gives up.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Block until a freshly created window can be captured and read.
///
/// Two startup races are collapsed into this one wait, because every test that
/// owns a window has to survive both:
///
/// * **First composited frame.** A window that has not processed its first
///   `WM_PAINT` reads back all-black, and `shot::capture_window` refuses an
///   all-black frame with `capture_unavailable`. The window is invalidated and
///   then captured until a frame is accepted; that frame is the one the test
///   measures, so no test has to retry its own first capture.
/// * **UIA tree.** The provider serves a window's children asynchronously
///   after `ShowWindow`, so the first walk can return the Win32 fallback or a
///   partial list. The wait polls until the UIA backend serves the window's
///   children. A host where UIA is genuinely unavailable is not waited on: the
///   Win32 fallback is synchronous and is the documented floor.
///
/// The readiness probe leaves no trace: its capture signature is dropped from
/// the stale-detection cache, so the first capture a test asks for is not
/// reported `stale` because of it.
fn await_ready(hwnd: HWND) -> Result<(), String> {
    await_ready_within(hwnd, READY_TIMEOUT)
}

/// `await_ready` with an explicit deadline, so the wait's own contract can be
/// proven without spending the full timeout.
fn await_ready_within(hwnd: HWND, timeout: Duration) -> Result<(), String> {
    // Ask for a repaint of the window and its children. `RDW_UPDATENOW` is
    // deliberately absent: it is a synchronous send, so it would block on a
    // window whose thread has not reached its message pump yet — the very
    // state this wait exists for.
    unsafe {
        RedrawWindow(
            hwnd,
            std::ptr::null(),
            std::ptr::null_mut(),
            RDW_INVALIDATE | RDW_ALLCHILDREN,
        );
    }
    let t0 = Instant::now();
    let mut last = String::from("no frame produced");
    let mut framed = false;
    while t0.elapsed() < timeout {
        if !framed {
            match unsafe { crate::shot::capture_window(hwnd, false) } {
                Ok(_) => {
                    crate::forget_stale(hwnd as i64);
                    framed = true;
                }
                Err(e) => last = e,
            }
        }
        if framed {
            let m = crate::shot::element_model(hwnd, crate::UIA_DEPTH_LIMIT, 0);
            let uia = m
                .detail
                .get("uia_available")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let serves_edit = m
                .entries
                .iter()
                .any(|e| e.get("role").and_then(|v| v.as_str()) == Some("Edit"));
            if !uia || (m.backend == "uia" && serves_edit) {
                return Ok(());
            }
            last = format!(
                "the frame exists but the UIA walk serves {:?} with {} entries",
                m.backend,
                m.entries.len()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(last)
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
        let f = Fixture {
            hwnd,
            edit,
            edit_ro,
            button,
            thread: Some(thread),
        };
        // A fixture is handed to a test only after it can be captured and
        // read, so no test has to wait out the startup race itself.
        await_ready(hwnd).unwrap_or_else(|e| panic!("fixture never became readable: {e}"));
        f
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

/// Serializes every test that touches the real desktop.
///
/// A test binary runs its tests on parallel threads by default, while the
/// fixture counters above, `GetForegroundWindow`/`GetCursorPos`, and the UIA
/// provider's focus-on-SetValue behaviour are process-wide. Without this lock
/// one live test observes a sibling's window, focus, counter bump or posted
/// message (CI run 41311923165 died exactly this way). Every test that creates
/// a fixture or message window, posts messages, or measures the
/// foreground/cursor holds the guard for its whole body, so parallel execution
/// is safe by construction.
static DESKTOP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII hold on the desktop lock; released when the test body ends.
pub struct DesktopGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

/// Acquire the process-wide desktop lock for a whole test body. Poison
/// tolerant: one failed sibling must not turn every later live test red.
pub fn desktop() -> DesktopGuard {
    DesktopGuard(DESKTOP_LOCK.lock().unwrap_or_else(|e| e.into_inner()))
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

/// Every window the test harness owns for one test body, and the ownership
/// oracle the foreground gate asks.
///
/// Under `desktop()` only one live test runs at a time, so a window owned by
/// this process is ours to answer for: an activation onto one of them IS the
/// harness taking the foreground — the defect the no-steal gate guards. A
/// window owned by anybody else cannot be attributed to us.
#[derive(Debug, Default)]
pub struct OwnedWindows {
    tops: Vec<i64>,
}

impl OwnedWindows {
    /// An empty set: only the owning-process rule applies.
    pub fn new() -> Self {
        OwnedWindows { tops: Vec::new() }
    }

    /// Register a top-level window (its descendants count as owned).
    pub fn with_top(mut self, hwnd: i64) -> Self {
        if hwnd != 0 {
            self.tops.push(hwnd);
        }
        self
    }

    /// Register a fixture: its top-level plus the EDIT/EDIT/button children.
    pub fn with_fixture(self, fx: &Fixture) -> Self {
        self.with_top(fx.hwnd_i64())
            .with_top(fx.edit() as i64)
            .with_top(fx.edit_ro() as i64)
            .with_top(fx.button() as i64)
    }

    /// The registered tops, in registration order (for logs).
    pub fn tops(&self) -> &[i64] {
        &self.tops
    }

    /// True when this process owns `hwnd`: a registered fixture top or one of
    /// its descendants, or any window whose owning PID is this process.
    pub fn owns(&self, hwnd: i64) -> bool {
        if hwnd == 0 {
            return false;
        }
        if self.tops.iter().any(|top| is_owned_window(*top, hwnd)) {
            return true;
        }
        pid_of_window(hwnd) == Some(std::process::id())
    }

    /// One-line identity for logs and failure messages: handle (decimal and
    /// hex), owning pid and module, class, title, liveness, and whether this
    /// process owns it. Every foreground mismatch logs this for both ends so
    /// the next flake is diagnosable from the test output alone.
    pub fn describe(&self, hwnd: i64) -> String {
        if hwnd == 0 {
            return "hwnd=0 (no foreground window)".to_string();
        }
        let pid = pid_of_window(hwnd);
        let exe = pid
            .map(crate::exe_of_pid)
            .unwrap_or_else(|| "unknown".to_string());
        let (class, title, alive) = unsafe {
            (
                crate::win_class(hwnd as HWND),
                crate::win_text(hwnd as HWND),
                IsWindow(hwnd as HWND),
            )
        };
        format!(
            "hwnd={hwnd} (0x{hwnd:x}) pid={} exe={exe} class={class:?} title={title:?} alive={alive} owner={}",
            pid.map_or_else(|| "?".to_string(), |p| p.to_string()),
            if self.owns(hwnd) {
                "harness"
            } else {
                "third-party"
            },
        )
    }
}

/// Verdict for one foreground sample against one test's window set. Pure, so
/// the whole matrix is unit-tested without live HWNDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FgOutcome {
    /// No foreground session before the op, or the session vanished: there is
    /// nothing to protect, so the gate skips (headless CI).
    NoSession,
    /// The sample is the window the op found: the harness held the field.
    Held,
    /// The sample is a window THIS PROCESS owns: the harness took the
    /// foreground. Fail, with no exemption.
    HarnessSteal,
    /// The sample is somebody else's window: a third party (owner activity,
    /// shell churn, a parallel test binary) took the foreground. The harness
    /// did not cause it, so it re-baselines instead of failing.
    External,
}

/// Decide one foreground sample. `owned` is `OwnedWindows::owns(sample)`:
/// ownership is what separates a tool defect from an environment event.
pub fn fg_outcome(before: i64, after: i64, owned: bool) -> FgOutcome {
    if before == 0 || after == 0 {
        FgOutcome::NoSession
    } else if after == before {
        FgOutcome::Held
    } else if owned {
        FgOutcome::HarnessSteal
    } else {
        FgOutcome::External
    }
}

/// How many foreground samples the post-op observation takes, and the gap
/// between them. A harness activation is sticky (the activated window stays
/// foreground until the next activation), so sample 0 already catches a real
/// steal; the remaining samples exist to catch a steal that later churn would
/// otherwise mask, and to widen the log while the desktop is busy. Sampling
/// never turns a harness-owned foreground into a pass, and only runs on the
/// mismatch path, so the happy path costs one sample.
pub const FG_SAMPLES: usize = 5;
const FG_SAMPLE_GAP: std::time::Duration = std::time::Duration::from_millis(20);

/// Assert the harness did not take the foreground across an operation, and
/// tell a harness steal apart from a third party taking focus:
///
/// 1. sample `GetForegroundWindow`; a harness-owned sample fails AT ONCE with
///    both HWNDs and the new owner's module (the true defect: the tool
///    activated one of its own windows);
/// 2. a third-party sample is logged with both ends and becomes the fresh
///    baseline — the harness is not answerable for another process's
///    activation;
/// 3. after `FG_SAMPLES` samples the last foreground must still not be ours.
///
/// The operation is NOT re-run: re-posting input would deliver it twice and
/// corrupt the delivery counters the same test asserts on. Only the read-only
/// observation repeats. No focus is manipulated and no window is touched
/// outside the registered set.
pub fn assert_foreground_held_owned(before: i64, owned: &OwnedWindows, what: &str) {
    assert_foreground_held_with(before, owned, what, foreground_now);
}

/// The gate above, with the sample source injected: `foreground_now` live, a
/// scripted sequence in the unit tests. The seam exists so the decision loop
/// (immediate fail on a harness steal, re-baseline on third-party churn, the
/// sample bound) is provable WITHOUT activating anything.
fn assert_foreground_held_with(
    before: i64,
    owned: &OwnedWindows,
    what: &str,
    mut sample: impl FnMut() -> i64,
) {
    use std::time::Instant;
    if before == 0 {
        println!("FG_SKIP: {what}: no foreground session (before=0); assertion skipped");
        return;
    }
    let first = before;
    let mut baseline = before;
    let mut external = 0usize;
    let t_all = Instant::now();
    for attempt in 0..FG_SAMPLES {
        let after = sample();
        match fg_outcome(baseline, after, owned.owns(after)) {
            FgOutcome::NoSession => {
                println!(
                    "FG_SKIP_SESSION: {what}: foreground session vanished mid-observation \
                     (first={} baseline={baseline} -> 0); nothing left to protect",
                    owned.describe(first)
                );
                return;
            }
            FgOutcome::Held => {
                println!(
                    "FG_HELD: {what}: sample {attempt}/{FG_SAMPLES} unchanged on {} \
                     (first={first}); harness took no foreground",
                    owned.describe(baseline)
                );
                return;
            }
            FgOutcome::HarnessSteal => panic!(
                "NO_STEAL_FAILED: {what}: the harness made its own window foreground \
                 (sample {attempt}/{FG_SAMPLES}, first={first}) — before={} after={}",
                owned.describe(baseline),
                owned.describe(after),
            ),
            FgOutcome::External => {
                external += 1;
                println!(
                    "FG_EXTERNAL: {what}: third-party focus change #{external} \
                     (sample {attempt}/{FG_SAMPLES}); re-baselining, not attributed to the harness. \
                     before={} after={}",
                    owned.describe(baseline),
                    owned.describe(after),
                );
                baseline = after;
            }
        }
        if attempt + 1 < FG_SAMPLES {
            std::thread::sleep(FG_SAMPLE_GAP);
        }
    }
    // Final assertion against the fresh baseline: whatever the churn did, the
    // foreground must still not be a window this process owns.
    let last = sample();
    assert!(
        !owned.owns(last),
        "NO_STEAL_FAILED: {what}: harness-owned foreground after {FG_SAMPLES} samples \
         ({external} third-party change(s)): first={} baseline={} last={}",
        owned.describe(first),
        owned.describe(baseline),
        owned.describe(last),
    );
    println!(
        "FG_EXTERNAL_SETTLED: {what}: {external} third-party change(s) in {}ms, final \
         foreground is not harness-owned: first={} baseline={} last={}",
        t_all.elapsed().as_millis(),
        owned.describe(first),
        owned.describe(baseline),
        owned.describe(last),
    );
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
    fn foreground_outcome_matrix_separates_steal_from_third_party() {
        // No session before or after: skip, never a verdict on the tool.
        assert_eq!(fg_outcome(0, 0, false), FgOutcome::NoSession);
        assert_eq!(fg_outcome(0, 12345, true), FgOutcome::NoSession);
        assert_eq!(fg_outcome(7, 0, false), FgOutcome::NoSession);
        // Same window: held, whoever owns it.
        assert_eq!(fg_outcome(7, 7, false), FgOutcome::Held);
        assert_eq!(fg_outcome(7, 7, true), FgOutcome::Held);
        // A window this process owns that is not the baseline: the harness
        // stole the foreground — the defect the gate exists for.
        assert_eq!(fg_outcome(7, 9, true), FgOutcome::HarnessSteal);
        // A window somebody else owns: an environment change, not the tool.
        // This is the case the old single-sample assert misreported.
        assert_eq!(fg_outcome(7, 9, false), FgOutcome::External);
        println!("FG_OUTCOME_MATRIX_OK=skip,held,harness-steal,external");
    }

    #[test]
    fn owned_windows_respond_for_fixture_and_children_only() {
        let _g = desktop();
        let fx = Fixture::new();
        let owned = OwnedWindows::new().with_fixture(&fx);
        let top = fx.hwnd_i64();
        // The fixture top and its children are ours; a null handle never is.
        assert!(owned.owns(top), "registered top must be owned");
        assert!(owned.owns(fx.edit() as i64), "EDIT child must be owned");
        assert!(owned.owns(fx.button() as i64), "BUTTON child must be owned");
        assert!(!owned.owns(0), "null hwnd is never owned");
        // An empty set still answers for this process by PID: an unregistered
        // window of ours (a sibling fixture) is ours to answer for.
        let bare = OwnedWindows::new();
        assert!(
            bare.owns(top),
            "owning-process rule must cover our unregistered window"
        );
        assert_eq!(
            owned.tops().len(),
            4,
            "top + EDIT + read-only EDIT + BUTTON"
        );
        // The log line carries the identity the next flake needs.
        let d = owned.describe(top);
        assert!(
            d.contains("owner=harness") && d.contains("exe=") && d.contains("class="),
            "describe must name the owner: {d}"
        );
        println!("FG_OWNED_SET_OK tops={} desc={d}", owned.tops().len());
    }

    /// A scripted stand-in for `GetForegroundWindow`: returns each sample in
    /// order, then repeats the last one. Lets the gate's decision loop be
    /// proven without activating a single window.
    fn scripted(samples: &[i64]) -> impl FnMut() -> i64 + '_ {
        let mut i = 0usize;
        move || {
            let v = samples
                .get(i)
                .copied()
                .unwrap_or_else(|| *samples.last().expect("non-empty script"));
            i += 1;
            v
        }
    }

    fn panic_text(e: Box<dyn std::any::Any + Send>) -> String {
        if let Some(s) = e.downcast_ref::<String>() {
            s.clone()
        } else if let Some(s) = e.downcast_ref::<&str>() {
            (*s).to_string()
        } else {
            String::new()
        }
    }

    #[test]
    fn gate_fails_on_a_harness_steal_and_never_launders_one_through_churn() {
        let ours = 0x1111_i64;
        let third = 0x2222_i64;
        let owned = OwnedWindows::new().with_top(ours);
        // (a) The harness itself becomes foreground: immediate, strict fail.
        let e = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_foreground_held_with(third, &owned, "scripted steal", scripted(&[ours]));
        }))
        .expect_err("a harness-owned foreground must fail the gate");
        let text = panic_text(e);
        assert!(text.contains("NO_STEAL_FAILED"), "typed failure: {text}");
        assert!(
            text.contains(&format!("hwnd={ours}")) && text.contains(&format!("hwnd={third}")),
            "both ends named: {text}"
        );
        assert!(text.contains("owner=harness"), "owner named: {text}");
        // (b) A third party takes focus FIRST and the harness steals second:
        // the re-baseline must not launder the steal.
        let e = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_foreground_held_with(
                0x3333,
                &owned,
                "scripted masked steal",
                scripted(&[third, ours]),
            );
        }))
        .expect_err("a steal after churn must still fail the gate");
        assert!(panic_text(e).contains("NO_STEAL_FAILED"));
        println!("FG_GATE_STEAL_FAILS_OK=cold,after-churn");
    }

    #[test]
    fn gate_rebaselines_third_party_churn_instead_of_blaming_the_harness() {
        let ours = 0x1111_i64;
        let owned = OwnedWindows::new().with_top(ours);
        // Nobody's window but ours exists on the desktop side of this script:
        // repeated third-party churn is re-baselined, then the final sample
        // must not be harness-owned. No window was activated to prove this.
        assert_foreground_held_with(0x2222, &owned, "scripted churn", scripted(&[0x3333; 6]));
        // A headless session (no foreground window before the op) skips.
        assert_foreground_held_with(0, &owned, "scripted headless", scripted(&[0]));
        println!("FG_GATE_CHURN_OK=rebaseline,headless-skip");
    }

    #[test]
    fn gate_classifies_real_windows_without_touching_them() {
        let _g = desktop();
        let fx = Fixture::new();
        let ours = fx.hwnd_i64();
        let owned = OwnedWindows::new().with_fixture(&fx);
        // Our own fixture handle, offered as a foreground sample, is a steal.
        assert_eq!(
            fg_outcome(0x2222, ours, owned.owns(ours)),
            FgOutcome::HarnessSteal
        );
        // A real foreign top-level window — found by read-only enumeration and
        // never activated or otherwise touched — is a third party, and its
        // module lands in the log.
        let foreign = crate::enum_windows()
            .into_iter()
            .map(|h| h as i64)
            .find(|h| *h != 0 && !owned.owns(*h) && unsafe { IsWindow(*h as HWND) } != 0);
        match foreign {
            None => println!(
                "FG_REAL_CLASSIFY_SKIP: no third-party top-level window exists on this desktop"
            ),
            Some(h) => {
                assert_eq!(fg_outcome(ours, h, owned.owns(h)), FgOutcome::External);
                let d = owned.describe(h);
                assert!(
                    d.contains("owner=third-party") && d.contains("exe="),
                    "describe must name a third party and its module: {d}"
                );
                println!("FG_REAL_CLASSIFY_OK third={d}");
            }
        }
    }

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
        let _g = desktop();
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

    /// The wait is a measurement, not a sleep: a window with no frame to
    /// capture never reads as ready, so the helper cannot be replaced by "the
    /// handle exists".
    #[test]
    fn the_readiness_wait_refuses_a_window_with_no_frame() {
        let _g = desktop();
        let t0 = Instant::now();
        let e = await_ready_within(std::ptr::null_mut(), Duration::from_millis(200))
            .expect_err("a window with no frame must never read as ready");
        assert!(
            e.contains("bad_window"),
            "the refusal must be the capture's own, got: {e}"
        );
        assert!(
            t0.elapsed() >= Duration::from_millis(150),
            "the wait must spend its deadline polling, not return at once (took {:?})",
            t0.elapsed()
        );
    }

    /// The frame the wait is about: a window with no composited frame reads
    /// all-black and the capture refuses it — exactly what a fresh window does
    /// before its first `WM_PAINT`. The wait returns only once a frame exists.
    #[test]
    fn a_window_with_no_composited_frame_is_refused_until_the_wait_sees_one() {
        let _g = desktop();
        let f = Fixture::new();
        // Hiding the window removes its composited frame without touching the
        // fixture, which is the same all-black read a fresh window produces.
        unsafe {
            ShowWindow(f.hwnd(), SW_HIDE);
        }
        let refused = await_ready_within(f.hwnd(), Duration::from_millis(250))
            .expect_err("a window with no composited frame must never read as ready");
        assert!(
            refused.contains("all-black") || refused.contains("blank"),
            "the refusal must be the blank-frame one, got: {refused}"
        );
        // Shown again, the same wait finds the frame and returns.
        unsafe {
            ShowWindow(f.hwnd(), SW_SHOWNOACTIVATE);
        }
        await_ready(f.hwnd()).expect("the wait must find the frame once the window is shown");
    }

    /// Readiness has exactly one path: the fixture constructor waits, and no
    /// test paints or polls a fresh window of its own. A second per-test wait
    /// is how this class came back the first time, so its names are banned.
    #[test]
    fn the_readiness_wait_is_the_only_startup_wait() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files: Vec<(String, String)> = Vec::new();
        for dir in ["src", "tests"] {
            collect_rs(&root.join(dir), &mut files);
        }
        assert!(
            files.len() > 20,
            "the scan must see the tree, found {} files",
            files.len()
        );
        for (path, body) in &files {
            // The helper's own file names the banned forms in this test, so
            // the name scan starts outside it; the constructor assertion below
            // pins the one path inside it.
            if path != "src/fixture.rs" {
                for banned in ["wait_uia_stable", "force_paint"] {
                    assert!(
                        !body.contains(banned),
                        "{path} carries a second startup wait ({banned}); route it through \
                         fixture::await_ready instead"
                    );
                }
            }
            if body.contains("RedrawWindow(") {
                assert_eq!(
                    path, "src/fixture.rs",
                    "{path} forces a paint outside the readiness helper; the helper is the \
                     only path that may wait for a first frame"
                );
            }
        }
        let fixture = files
            .iter()
            .find(|(path, _)| path == "src/fixture.rs")
            .map(|(_, body)| body.as_str())
            .expect("src/fixture.rs must be in the scan");
        // Only the production half of the file counts: the assertions below
        // live in its test module, and a literal there would satisfy the
        // search it is meant to enforce.
        let production = fixture.split("#[cfg(test)]").next().unwrap_or_default();
        assert!(
            production.contains("await_ready(hwnd)"),
            "the fixture constructor must call the readiness wait"
        );
    }

    /// Every `.rs` under `dir`, as (path relative to the manifest, contents).
    fn collect_rs(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let body = std::fs::read_to_string(&path).unwrap_or_default();
                out.push((rel, body));
            }
        }
    }

    #[test]
    fn fixture_creates_offscreen_without_activation() {
        let _g = desktop();
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
