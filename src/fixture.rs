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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn fixture_wndproc(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
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
                (WS_CHILD | WS_VISIBLE | WS_BORDER) as u32 | 0x0080,
                10,
                10,
                260,
                24,
                hwnd,
                100 as isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            // Read-only EDIT id 102.
            let edit_ro = CreateWindowExW(
                0,
                edit_cls.as_ptr(),
                std::ptr::null(),
                (WS_CHILD | WS_VISIBLE | WS_BORDER) as u32 | 0x0080 | 0x0800,
                10,
                44,
                260,
                24,
                hwnd,
                102 as isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            let btn_cls = wide("BUTTON");
            let btn_text = wide("OK");
            let button = CreateWindowExW(
                0,
                btn_cls.as_ptr(),
                btn_text.as_ptr(),
                (WS_CHILD | WS_VISIBLE) as u32,
                10,
                80,
                80,
                24,
                hwnd,
                101 as isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
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
