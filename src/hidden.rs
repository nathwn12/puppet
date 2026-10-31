//! Hidden-desktop target mode for the `do` tool (`desktop:"hidden"`).
//!
//! Opt-in only: plain `do` never reaches this module. When requested, one
//! dedicated thread owns the whole desktop lifecycle so the MCP server's own
//! thread desktop is never moved:
//!
//!   GetThreadDesktop(original) -> CreateDesktopW -> SetThreadDesktop(hidden)
//!   -> CreateProcessW(lpDesktop=hidden) -> EnumDesktopWindows (poll for the
//!   target window) -> [caller drives it through the existing PostMessage
//!   path] -> capture + readback on the hidden desktop -> WM_CLOSE/terminate
//!   -> SetThreadDesktop(original) -> CloseDesktop -> OpenDesktopW must
//!   return NULL (teardown proof).
//!
//! Teardown order is load-bearing: the thread is moved off the desktop BEFORE
//! CloseDesktop, otherwise CloseDesktop fails with ERROR_BUSY (170). The
//! target window is resolved with EnumDesktopWindows, never the owner's
//! EnumWindows, so the owner's desktop is never enumerated or touched. The
//! capture also runs on the hidden-desktop thread so the device context
//! belongs to that desktop.

use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows_sys::core::{BOOL, PWSTR};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HWND, LPARAM, RECT, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, CreateDesktopW, EnumDesktopWindows, GetThreadDesktop, OpenDesktopW,
    SetThreadDesktop, DESKTOP_READOBJECTS, HDESK,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetCurrentThreadId, OpenProcess, TerminateProcess, WaitForSingleObject,
    PROCESS_INFORMATION, PROCESS_TERMINATE, STARTUPINFOW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetClassNameW, GetGUIThreadInfo, GetWindow, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsChild, IsWindow, IsWindowVisible, PostMessageW,
    SendMessageTimeoutW, GUITHREADINFO, GW_OWNER, SMTO_ABORTIFHUNG, WM_CLOSE, WM_GETTEXT,
    WM_SETTEXT,
};

use crate::cap::{encode_png, Img};

/// `DESKTOP_ALL_ACCESS` (winnt.h). windows-sys exposes the individual
/// DESKTOP_* rights but not the combined mask, so it is spelled here.
const DESKTOP_ALL_ACCESS: u32 = 0x000F_01FF;
const SETUP_TIMEOUT_MS: u64 = 8000;
const TEARDOWN_TIMEOUT_MS: u32 = 3000;

/// Per-request desktop name with no module-level mutable state.
///
/// Previously a process-global `static DESKTOP_SEQ: AtomicU64` counter was
/// shared across requests to uniquify the hidden-desktop name. The name only
/// needs to be unique per `spawn()` call, so every component here is derived
/// from this call's context: process id + wall-clock nanos + calling thread
/// id + the per-call stack address of a local. Concurrent `spawn()` calls
/// share no mutable state (audit rule: no request state in globals); a
/// same-nanosecond collision on one thread only risks a `CreateDesktopW`
/// name clash, which surfaces as a setup error for that request alone.
fn unique_desktop_name() -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tid_dbg = format!("{:?}", std::thread::current().id());
    let tid_digits: String = tid_dbg
        .bytes()
        .filter(|b| b.is_ascii_digit())
        .map(|b| b as char)
        .collect();
    // Address of a call-local: distinct per live `spawn()` frame, adds
    // entropy when two calls land in the same nanosecond on one thread.
    let stack_entropy = &nanos as *const _ as usize;
    format!("puppet_hidden_{pid}_{nanos}_{tid_digits}_{stack_entropy:x}")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------- messages

/// Setup result, delivered once the desktop is up and the target window is
/// resolved (or the setup failed).
pub struct Ready {
    pub created: bool,
    pub set_desktop: bool,
    pub pid: Option<u32>,
    pub hwnd: Option<i64>,
    pub hwnd_class: String,
    pub hwnd_title: String,
    pub hwnd_pid: u32,
    pub on_desktop: bool,
    pub desktop_name: String,
    pub error: Option<String>,
}

/// Capture evidence, taken on the hidden-desktop thread before teardown.
pub struct Capture {
    pub ok: bool,
    pub flags: u32,
    pub w: i32,
    pub h: i32,
    pub min: u8,
    pub max: u8,
    pub png: Option<Vec<u8>>,
    pub error: Option<String>,
}

/// Teardown result, delivered after the caller signals it is done driving.
pub struct Done {
    pub alive: bool,
    pub text: Option<String>,
    pub capture: Option<Capture>,
    /// Same-fixture revert evidence: the target's text before any driven
    /// input, and again after the revert, both read with the readback
    /// function so the shapes match. `reverted` is their equality —
    /// comparing across launches would pass even with the revert removed,
    /// since every launch initializes the same content.
    pub pre_drive_text: Option<String>,
    pub post_revert_text: Option<String>,
    pub reverted: bool,
    pub close_posted: bool,
    pub process_exited: bool,
    pub desktop_empty: bool,
    pub terminated: bool,
    pub restored_desktop: bool,
    pub closed_desktop: bool,
    pub open_after_close_null: bool,
    pub teardown_proven: bool,
    pub error: Option<String>,
}

/// Owner thread + channels. `teardown()` must be called exactly once: it
/// signals the thread to run capture+teardown and joins it.
pub struct Hidden {
    pub ready: Ready,
    tx_go: mpsc::Sender<()>,
    rx_done: mpsc::Receiver<Done>,
    join: Option<JoinHandle<()>>,
}

impl Hidden {
    pub fn teardown(mut self) -> Done {
        let _ = self.tx_go.send(());
        let done = match self.rx_done.recv_timeout(Duration::from_secs(10)) {
            Ok(d) => d,
            Err(_) => Done {
                alive: false,
                text: None,
                capture: None,
                pre_drive_text: None,
                post_revert_text: None,
                reverted: false,
                close_posted: false,
                process_exited: false,
                desktop_empty: false,
                terminated: false,
                restored_desktop: false,
                closed_desktop: false,
                open_after_close_null: false,
                teardown_proven: false,
                error: Some("hidden-desktop teardown timed out after 10s".to_string()),
            },
        };
        if done.teardown_proven {
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
        } else {
            // Timed-out (or otherwise unproven) teardown: detach rather than
            // hang the caller on join; the owner thread finishes on its own.
            let _ = self.join.take();
        }
        done
    }
}

/// `background` doubles as the launch target under `desktop:"hidden"`: a
/// window selector cannot name a program that does not exist yet, so the
/// window-selector prefixes are refused with the reason.
pub fn program_from_background(s: &str) -> Result<String, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err(
            "bad_window: desktop=\"hidden\" needs background to name the program to launch (e.g. exe:notepad.exe)"
                .to_string(),
        );
    }
    let lower = t.to_lowercase();
    for p in ["hwnd:", "pid:", "title:", "class:"] {
        if lower.starts_with(p) {
            return Err(format!(
                "bad_window: desktop=\"hidden\" cannot launch from a window selector \"{t}\"; name the program (exe:notepad.exe or a path)"
            ));
        }
    }
    let prog = if lower.starts_with("exe:") {
        &t[4..]
    } else {
        t
    };
    let prog = prog.trim().trim_matches('"').trim();
    if prog.is_empty() {
        return Err(
            "bad_window: desktop=\"hidden\" needs background to name the program to launch (e.g. exe:notepad.exe)"
                .to_string(),
        );
    }
    Ok(prog.to_string())
}

/// Quote one command-line element per CommandLineToArgvW rules: bare when
/// it needs nothing, else double-quoted with backslash runs doubled ahead
/// of a quote (and trailing runs doubled). The program itself keeps its
/// historical always-quoted form; each `background_args` entry is encoded
/// with this quoter so paths with spaces survive as one argv element.
fn quote_cmd_arg(s: &str) -> String {
    if !s.is_empty()
        && !s
            .bytes()
            .any(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'"'))
    {
        return s.to_string();
    }
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    let mut backslashes = 0usize;
    for c in s.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        if c == '"' {
            o.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
        } else {
            o.extend(std::iter::repeat_n('\\', backslashes));
        }
        backslashes = 0;
        o.push(c);
    }
    o.extend(std::iter::repeat_n('\\', backslashes * 2));
    o.push('"');
    o
}

/// Build the CreateProcessW command line: the program exactly as before
/// (always double-quoted, so the zero-args launch is byte-identical),
/// then each argv element quoted as needed.
fn build_command_line(program: &str, args: &[String]) -> Vec<u16> {
    let mut cmd = format!("\"{program}\"");
    for a in args {
        cmd.push(' ');
        cmd.push_str(&quote_cmd_arg(a));
    }
    cmd.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Spawn the owner thread and block until setup is complete. `args` is
/// argv for the launched program (from `background_args`); empty keeps
/// the historical bare-program launch byte-identical.
pub fn spawn(program: &str, args: &[String]) -> Hidden {
    let (tx_ready, rx_ready) = mpsc::channel::<Ready>();
    let (tx_go, rx_go) = mpsc::channel::<()>();
    let (tx_done, rx_done) = mpsc::channel::<Done>();
    let name = unique_desktop_name();
    let prog = program.to_string();
    let argv: Vec<String> = args.to_vec();
    let nm = name.clone();
    let join = std::thread::spawn(move || owner(prog, argv, nm, tx_ready, rx_go, tx_done));
    // Worst-case setup is bounded: window discovery (8 s) + text-target
    // readiness (3 s) + title-stem wait (4 s), so 20 s leaves headroom over
    // the sum while still failing fast when the owner thread dies.
    let ready = match rx_ready.recv_timeout(Duration::from_secs(20)) {
        Ok(r) => r,
        Err(_) => Ready {
            created: false,
            set_desktop: false,
            pid: None,
            hwnd: None,
            hwnd_class: String::new(),
            hwnd_title: String::new(),
            hwnd_pid: 0,
            on_desktop: false,
            desktop_name: name,
            error: Some("hidden-desktop setup timed out after 15s".to_string()),
        },
    };
    Hidden {
        ready,
        tx_go,
        rx_done,
        join: Some(join),
    }
}

// ---------------------------------------------------------------- owner thread

fn owner(
    program: String,
    args: Vec<String>,
    name: String,
    tx_ready: mpsc::Sender<Ready>,
    rx_go: mpsc::Receiver<()>,
    tx_done: mpsc::Sender<Done>,
) {
    unsafe {
        let original = GetThreadDesktop(GetCurrentThreadId());
        let name_w = wide(&name);
        let hdesk = CreateDesktopW(
            name_w.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            DESKTOP_ALL_ACCESS,
            std::ptr::null(),
        );
        if hdesk.is_null() {
            let _ = tx_ready.send(Ready {
                created: false,
                set_desktop: false,
                pid: None,
                hwnd: None,
                hwnd_class: String::new(),
                hwnd_title: String::new(),
                hwnd_pid: 0,
                on_desktop: false,
                desktop_name: name,
                error: Some(format!(
                    "CreateDesktopW failed (GetLastError={})",
                    GetLastError()
                )),
            });
            return;
        }
        let set_ok = SetThreadDesktop(hdesk) != 0;

        // Launch the target with its desktop pinned to the hidden one.
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.lpDesktop = name_w.as_ptr() as PWSTR;
        let mut cmd = build_command_line(&program, &args);
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let created = CreateProcessW(
            std::ptr::null(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            0,
            std::ptr::null(),
            std::ptr::null(),
            &si,
            &mut pi,
        ) != 0;
        if !created {
            let e = GetLastError();
            if set_ok {
                SetThreadDesktop(original);
            }
            CloseDesktop(hdesk);
            let _ = tx_ready.send(Ready {
                created: false,
                set_desktop: set_ok,
                pid: None,
                hwnd: None,
                hwnd_class: String::new(),
                hwnd_title: String::new(),
                hwnd_pid: 0,
                on_desktop: false,
                desktop_name: name,
                error: Some(format!(
                    "CreateProcessW failed for \"{program}\" (GetLastError={e})"
                )),
            });
            return;
        }

        let pid = pi.dwProcessId;
        let hwnd = wait_for_window(hdesk, pid);
        // The top-level window turns visible before its content controls
        // exist; wait (bounded) for a text target so posted keyboard input
        // has a document to land in by the time the caller drives.
        if let Some(h) = hwnd {
            wait_for_text_target(h as HWND, 3000);
            wait_for_title_stem(h as HWND, &args, 4000);
        }
        // Snapshot the driven text controls BEFORE the caller drives, so
        // the evidence read below can be reverted before teardown (the
        // hidden target shares the owner's per-user persisted state).
        let mut pre_text: Vec<TextSnapshot> = Vec::new();
        if let Some(h) = hwnd {
            snapshot_edit_texts(h as HWND, &mut pre_text);
        }
        // Same-fixture baseline for the revert proof, read with the same
        // function as the evidence and post-revert reads.
        let pre_drive_text = hwnd.and_then(read_target_text);
        let (hwnd_class, hwnd_title, hwnd_pid) = match hwnd {
            Some(h) => (
                window_class(h as HWND),
                window_title(h as HWND),
                window_pid(h as HWND),
            ),
            None => (String::new(), String::new(), 0),
        };
        let _ = tx_ready.send(Ready {
            created: true,
            set_desktop: set_ok,
            pid: Some(pid),
            hwnd,
            hwnd_class,
            hwnd_title,
            hwnd_pid,
            on_desktop: hwnd.is_some(),
            desktop_name: name.clone(),
            error: None,
        });

        // The caller drives the target now; wait for it to finish.
        let _ = rx_go.recv();

        // Evidence while the target is still alive.
        let alive = match hwnd {
            Some(h) => IsWindow(h as HWND) != 0,
            None => false,
        };
        let text = hwnd.and_then(read_target_text);
        let capture = hwnd.map(capture_window);

        // Revert driven input before teardown so a persisting app writes
        // back the pre-drive content, not our evidence run.
        restore_edit_texts(&pre_text);
        // Same-fixture revert evidence: reread with the readback function
        // after the restore, before WM_CLOSE.
        let post_revert_text = hwnd.and_then(read_target_text);
        let reverted = post_revert_text == pre_drive_text;

        // Teardown: close the target, then move the thread off the desktop
        // BEFORE CloseDesktop (else ERROR_BUSY 170), then prove it is gone.
        // A launcher stub may hand the window to a different pid on the same
        // private desktop, so emptiness is judged by the desktop's windows,
        // not only by the launched process handle.
        let close_posted = match hwnd {
            Some(h) => PostMessageW(h as HWND, WM_CLOSE, 0, 0) != 0,
            None => false,
        };
        let mut process_exited =
            WaitForSingleObject(pi.hProcess, TEARDOWN_TIMEOUT_MS) == WAIT_OBJECT_0;
        let mut terminated = false;
        if !wait_desktop_empty(hdesk, 2000) {
            for p in desktop_pids(hdesk) {
                terminated |= kill_pid(p);
            }
            wait_desktop_empty(hdesk, 2000);
        }
        if !process_exited {
            terminated |= TerminateProcess(pi.hProcess, 1) != 0;
            process_exited = WaitForSingleObject(pi.hProcess, TEARDOWN_TIMEOUT_MS) == WAIT_OBJECT_0;
        }
        let desktop_empty = wait_desktop_empty(hdesk, 1000);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        let restored = SetThreadDesktop(original) != 0;
        let closed = CloseDesktop(hdesk) != 0;
        let open_null = wait_open_null(&name_w, 2000);
        let _ = tx_done.send(Done {
            alive,
            text,
            capture,
            pre_drive_text,
            post_revert_text,
            reverted,
            close_posted,
            process_exited,
            desktop_empty,
            terminated,
            restored_desktop: restored,
            closed_desktop: closed,
            open_after_close_null: open_null,
            teardown_proven: restored && closed && open_null && desktop_empty,
            error: None,
        });
    }
}

struct EnumCtx {
    pid: u32,
    hwnds: Vec<isize>,
}

unsafe extern "system" fn enum_cb(hwnd: HWND, lp: LPARAM) -> BOOL {
    let ctx = &mut *(lp as *mut EnumCtx);
    ctx.hwnds.push(hwnd as isize);
    1
}

fn window_class(hwnd: HWND) -> String {
    let mut buf = vec![0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

fn window_title(hwnd: HWND) -> String {
    let mut buf = vec![0u16; 512];
    let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

/// Class names every process emits that are never the target's main window
/// (dropdown list boxes, tooltips, menus, IME and shadow helpers).
fn is_noise_class(cls: &str) -> bool {
    matches!(
        cls,
        "ComboLBox"
            | "tooltips_class32"
            | "#32768"
            | "IME"
            | "MSCTFIME UI"
            | "Default IME"
            | "SysShadow"
            | "Windows.UI.Core.CoreWindow"
    )
}

fn window_pid(hwnd: HWND) -> u32 {
    let mut pid: u32 = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    pid
}

/// Process ids owning any window on the private desktop.
fn desktop_pids(hdesk: HDESK) -> Vec<u32> {
    let mut ctx = EnumCtx {
        pid: 0,
        hwnds: Vec::new(),
    };
    unsafe {
        EnumDesktopWindows(hdesk, Some(enum_cb), &mut ctx as *mut EnumCtx as LPARAM);
    }
    let mut pids: Vec<u32> = ctx
        .hwnds
        .iter()
        .map(|&h| window_pid(h as HWND))
        .filter(|p| *p != 0)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// Terminate one exact pid (used only for processes owning a window on the
/// private desktop we created). Returns whether the call succeeded.
fn kill_pid(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h.is_null() {
            return false;
        }
        let ok = TerminateProcess(h, 1) != 0;
        CloseHandle(h);
        ok
    }
}

fn wait_desktop_empty(hdesk: HDESK, ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        if desktop_pids(hdesk).is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Pick the target's top-level window on the private desktop, scored so a
/// hidden overlay (no title, zero-size) never wins over the real window.
/// Preference: visible + titled + non-zero rect + unowned, owned by the
/// launched pid first, then any such window (a launcher stub may hand off to
/// another pid on the same private desktop).
fn pick_window(ctx: &EnumCtx) -> Option<(i64, u8)> {
    let mut best: Option<(i64, u8)> = None;
    for &h in &ctx.hwnds {
        let hw = h as HWND;
        let cls = window_class(hw);
        if cls.is_empty() || is_noise_class(&cls) {
            continue;
        }
        let vis = unsafe { IsWindowVisible(hw) != 0 };
        let mut rc = RECT::default();
        let sized =
            unsafe { GetWindowRect(hw, &mut rc) != 0 } && rc.right > rc.left && rc.bottom > rc.top;
        let titled = !window_title(hw).is_empty();
        let unowned = unsafe { GetWindow(hw, GW_OWNER) }.is_null();
        let owned_by_launch = window_pid(hw) == ctx.pid;
        let mut score = 0u8;
        if vis {
            score = 1;
            if sized {
                score = 2;
                if unowned {
                    score = 3;
                    if titled {
                        score = 4;
                        if owned_by_launch {
                            score = 5;
                        }
                    }
                }
            }
        }
        match best {
            Some((_, bs)) if bs >= score => {}
            _ => best = Some((h as i64, score)),
        }
    }
    best
}

/// True when the hidden target already offers a text-input control: the
/// thread's focused control when it is text input, else any visible +
/// enabled Edit-like descendant. The top-level window becomes visible
/// (and `wait_for_window` returns) before XAML content finishes
/// initializing, so driving keyboard input immediately would resolve to
/// the frame and be swallowed; readiness gating keeps `bg_text_target`
/// honest by the time the caller drives.
fn text_target_ready(h: HWND) -> bool {
    unsafe {
        if IsWindow(h) == 0 {
            return false;
        }
        let thread = GetWindowThreadProcessId(h, std::ptr::null_mut());
        if thread != 0 {
            let mut info: GUITHREADINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
            if GetGUIThreadInfo(thread, &mut info) != 0 {
                let f = info.hwndFocus;
                if !f.is_null()
                    && IsChild(h, f) != 0
                    && window_class(f).to_lowercase().contains("edit")
                {
                    return true;
                }
            }
        }
        has_text_descendant(h)
    }
}

fn has_text_descendant(h: HWND) -> bool {
    let mut kids: Vec<isize> = Vec::new();
    unsafe {
        EnumChildWindows(h, Some(child_cb), &mut kids as *mut Vec<isize> as LPARAM);
    }
    for k in &kids {
        let kh = *k as HWND;
        unsafe {
            if IsWindowVisible(kh) == 0 || IsWindowEnabled(kh) == 0 {
                continue;
            }
        }
        if window_class(kh).to_lowercase().contains("edit") {
            return true;
        }
        if has_text_descendant(kh) {
            return true;
        }
    }
    false
}

/// Wait (bounded) for the hidden target's text control to exist. Returns
/// immediately when the window dies; times out silently (the caller drives
/// anyway and the per-action results plus readback tell the story).
fn wait_for_text_target(h: HWND, ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while !text_target_ready(h) {
        if unsafe { IsWindow(h) } == 0 || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Poll for the desktop's destruction after CloseDesktop: a dying target
/// process can transiently pin the desktop past our own close (its
/// windowless threads release on exit), so a single immediate reopen can
/// falsely report survival. Bounded; every successful reopen is closed so
/// the probe itself never pins the desktop.
fn wait_open_null(name_w: &[u16], ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        let reopen = unsafe { OpenDesktopW(name_w.as_ptr(), 0, 0, DESKTOP_READOBJECTS) };
        if reopen.is_null() {
            return true;
        }
        unsafe {
            CloseDesktop(reopen);
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Wait (bounded) for the window title to name the launched document: when
/// the first argv element is an existing file, its stem should appear in the
/// title once the app selects the document tab (Win11 Notepad restores its
/// persisted session first and opens the file tab async, so the text target
/// can be ready while the title still names a restored tab). Skips fast for
/// flags and non-paths. Silent timeout: the caller drives anyway and the
/// title evidence tells the story.
fn wait_for_title_stem(h: HWND, args: &[String], ms: u64) {
    let stem: Option<String> = args.first().and_then(|a| {
        let p = std::path::Path::new(a);
        if !p.is_file() {
            return None;
        }
        p.file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .filter(|s| !s.is_empty())
    });
    let Some(stem) = stem else {
        return;
    };
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        if unsafe { IsWindow(h) } == 0 {
            break;
        }
        if window_title(h).to_lowercase().contains(&stem) {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_window(hdesk: HDESK, pid: u32) -> Option<i64> {
    let deadline = Instant::now() + Duration::from_millis(SETUP_TIMEOUT_MS);
    let mut weak: Option<(i64, u8)> = None;
    loop {
        let mut ctx = EnumCtx {
            pid,
            hwnds: Vec::new(),
        };
        unsafe {
            EnumDesktopWindows(hdesk, Some(enum_cb), &mut ctx as *mut EnumCtx as LPARAM);
        }
        if let Some((h, score)) = pick_window(&ctx) {
            // A visible, titled, non-zero, unowned window is the target; a
            // weaker candidate is kept only until the deadline.
            if score >= 3 {
                return Some(h);
            }
            weak = Some((h, score));
        }
        if Instant::now() >= deadline {
            return weak.map(|(h, _)| h);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------- readback + capture

fn sm_get_text(hwnd: HWND) -> Option<String> {
    let mut buf = vec![0u16; 4096];
    let mut res: usize = 0;
    let ok = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_GETTEXT,
            buf.len(),
            buf.as_mut_ptr() as isize,
            SMTO_ABORTIFHUNG,
            500,
            &mut res,
        )
    };
    if ok == 0 {
        return None;
    }
    let n = res.min(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]))
}

unsafe extern "system" fn child_cb(hwnd: HWND, lp: LPARAM) -> BOOL {
    let v = &mut *(lp as *mut Vec<isize>);
    v.push(hwnd as isize);
    1
}

/// Best-effort readback of the target's text after driving: every
/// non-empty Edit-like control in the descendant tree is collected and
/// joined (multi-tab editors such as Win11 Notepad restore their persisted
/// per-user tab session even in a fresh hidden-desktop process, so the
/// typed text may sit in any tab's document rather than the first child in
/// Z-order — and modern editors nest the document deeper than one level),
/// else the window's own text. Never raises; `None` means nothing readable.
fn read_target_text(hwnd: i64) -> Option<String> {
    let h = hwnd as HWND;
    let mut parts: Vec<String> = Vec::new();
    collect_edit_texts(h, &mut parts);
    if !parts.is_empty() {
        return Some(parts.join("\n"));
    }
    sm_get_text(h)
}

/// Snapshot of one text control's content for post-evidence restore.
struct TextSnapshot {
    hwnd: isize,
    text: String,
}

/// Record every Edit-like descendant's current text (recursive, same walk
/// as the readback). The hidden target shares the owner's per-user
/// persisted state (Win11 Notepad restores its tab session even on a fresh
/// hidden desktop), so driven input must be reverted before teardown or
/// every headless run would accumulate into the owner's store.
fn snapshot_edit_texts(h: HWND, out: &mut Vec<TextSnapshot>) {
    let mut kids: Vec<isize> = Vec::new();
    unsafe {
        EnumChildWindows(h, Some(child_cb), &mut kids as *mut Vec<isize> as LPARAM);
    }
    for k in &kids {
        let kh = *k as HWND;
        if window_class(kh).to_lowercase().contains("edit") {
            out.push(TextSnapshot {
                hwnd: kh as isize,
                text: sm_get_text(kh).unwrap_or_default(),
            });
        }
        snapshot_edit_texts(kh, out);
    }
}

/// Best-effort revert of driven input: put every snapshotted control back
/// the way it was. Runs after evidence, before WM_CLOSE, so a persisting
/// app writes back the original content. Never fails the teardown.
fn restore_edit_texts(snaps: &[TextSnapshot]) {
    for s in snaps {
        let h = s.hwnd as HWND;
        unsafe {
            if IsWindow(h) == 0 {
                continue;
            }
        }
        let wide: Vec<u16> = s.text.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let mut _res: usize = 0;
            SendMessageTimeoutW(
                h,
                WM_SETTEXT,
                0,
                wide.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                500,
                &mut _res,
            );
        }
    }
}

/// Depth-first descendant walk collecting the text of every non-empty
/// Edit-like control.
fn collect_edit_texts(h: HWND, parts: &mut Vec<String>) {
    let mut kids: Vec<isize> = Vec::new();
    unsafe {
        EnumChildWindows(h, Some(child_cb), &mut kids as *mut Vec<isize> as LPARAM);
    }
    for k in &kids {
        let kh = *k as HWND;
        if window_class(kh).to_lowercase().contains("edit") {
            if let Some(t) = sm_get_text(kh) {
                if !t.is_empty() {
                    parts.push(t);
                }
            }
        }
        collect_edit_texts(kh, parts);
    }
}

fn px_min_max(img: &Img) -> (u8, u8) {
    let mut mn = 255u8;
    let mut mx = 0u8;
    for &b in &img.px {
        if b < mn {
            mn = b;
        }
        if b > mx {
            mx = b;
        }
    }
    (mn, mx)
}

/// PrintWindow into a 32-bpp DIB section. A compatible bitmap from
/// `GetDC(NULL)` on a desktop with no display surface can come back
/// monochrome, so the destination is created explicitly as a top-down
/// 32-bpp DIB section.
fn grab_dib(hwnd: HWND, w: i32, h: i32, flags: u32) -> Result<Img, String> {
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HGDIOBJ,
    };
    use windows_sys::Win32::Storage::Xps::PrintWindow;
    unsafe {
        let sdc = GetDC(std::ptr::null_mut());
        if sdc.is_null() {
            return Err("GetDC(NULL) failed".to_string());
        }
        let memdc = CreateCompatibleDC(sdc);
        if memdc.is_null() {
            ReleaseDC(std::ptr::null_mut(), sdc);
            return Err("CreateCompatibleDC failed".to_string());
        }
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let hbm = CreateDIBSection(
            sdc,
            &bmi,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        );
        if hbm.is_null() || bits.is_null() {
            DeleteDC(memdc);
            ReleaseDC(std::ptr::null_mut(), sdc);
            return Err("CreateDIBSection failed".to_string());
        }
        let old = SelectObject(memdc, hbm as HGDIOBJ);
        let ok = PrintWindow(hwnd, memdc, flags);
        SelectObject(memdc, old);
        let out = if ok == 0 {
            Err(format!("PrintWindow refused (flags={flags:#x})"))
        } else {
            let n = (w as usize) * (h as usize);
            let src = std::slice::from_raw_parts(bits as *const u8, n * 4);
            let mut px = vec![0u8; n * 3];
            for i in 0..n {
                px[3 * i] = src[4 * i + 2];
                px[3 * i + 1] = src[4 * i + 1];
                px[3 * i + 2] = src[4 * i];
            }
            Ok(Img { w, h, px })
        };
        DeleteObject(hbm as HGDIOBJ);
        DeleteDC(memdc);
        ReleaseDC(std::ptr::null_mut(), sdc);
        out
    }
}

/// PrintWindow capture on the hidden-desktop thread. Flag 2
/// (PW_RENDERFULLCONTENT) is used first; when it yields a flat bitmap (some
/// classic GDI windows only paint through WM_PRINTCLIENT) flag 0 is tried and
/// the non-flat result is kept.
fn capture_window(hwnd: i64) -> Capture {
    let h = hwnd as HWND;
    let mut rc = RECT::default();
    let (w, ht) = unsafe {
        if GetWindowRect(h, &mut rc) == 0 {
            (0, 0)
        } else {
            (rc.right - rc.left, rc.bottom - rc.top)
        }
    };
    if w <= 0 || ht <= 0 {
        return Capture {
            ok: false,
            flags: 2,
            w,
            h: ht,
            min: 0,
            max: 0,
            png: None,
            error: Some("zero-size hidden window rect".to_string()),
        };
    }
    let mut first: Option<(u32, Img, u8, u8)> = None;
    let mut last_err = String::from("PrintWindow refused");
    for flags in [2u32, 0u32] {
        match grab_dib(h, w, ht, flags) {
            Ok(img) => {
                let (mn, mx) = px_min_max(&img);
                if mn != mx {
                    return finish_capture(img, flags, w, ht, mn, mx);
                }
                if first.is_none() {
                    first = Some((flags, img, mn, mx));
                }
            }
            Err(e) => last_err = e,
        }
    }
    if let Some((flags, img, mn, mx)) = first {
        finish_capture(img, flags, w, ht, mn, mx)
    } else {
        Capture {
            ok: false,
            flags: 2,
            w,
            h: ht,
            min: 0,
            max: 0,
            png: None,
            error: Some(last_err),
        }
    }
}

fn finish_capture(img: Img, flags: u32, w: i32, h: i32, mn: u8, mx: u8) -> Capture {
    match encode_png(&img) {
        Ok(bytes) => Capture {
            ok: true,
            flags,
            w,
            h,
            min: mn,
            max: mx,
            png: Some(bytes),
            error: None,
        },
        Err(e) => Capture {
            ok: false,
            flags,
            w,
            h,
            min: mn,
            max: mx,
            png: None,
            error: Some(e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd_line(program: &str, args: &[&str]) -> String {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let w = build_command_line(program, &argv);
        String::from_utf16(&w[..w.len() - 1]).expect("command line is UTF-16")
    }

    #[test]
    fn command_line_bare_program_is_historically_quoted() {
        // Zero-args launch keeps the exact historical form: the program
        // alone, always double-quoted.
        assert_eq!(cmd_line("notepad.exe", &[]), "\"notepad.exe\"");
    }

    #[test]
    fn command_line_quotes_args_with_spaces() {
        assert_eq!(
            cmd_line("notepad.exe", &["C:\\a b\\c d.txt"]),
            "\"notepad.exe\" \"C:\\a b\\c d.txt\""
        );
        assert_eq!(
            cmd_line("notepad.exe", &["plain.txt"]),
            "\"notepad.exe\" plain.txt"
        );
        assert_eq!(cmd_line("notepad.exe", &[""]), "\"notepad.exe\" \"\"");
    }

    #[test]
    fn command_line_escapes_quotes_and_backslashes() {
        // CommandLineToArgvW rules: 2n+1 backslashes before a literal
        // quote, 2n trailing — inside a quoted element. An element with no
        // spaces stays bare (a trailing backslash outside quotes is
        // literal and needs no doubling).
        assert_eq!(cmd_line("prog", &["a\"b"]), "\"prog\" \"a\\\"b\"");
        assert_eq!(
            cmd_line("prog", &["C:\\dir with space\\"]),
            "\"prog\" \"C:\\dir with space\\\\\""
        );
        assert_eq!(cmd_line("prog", &["C:\\dir\\"]), "\"prog\" C:\\dir\\");
    }
}
