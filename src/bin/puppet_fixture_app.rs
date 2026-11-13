//! P3a test fixture: a minimal single-window GUI app for the hidden-desktop
//! proof. One top-level window (title from argv[1], else a default) with
//! one visible, enabled EDIT child holding known initial text. No file I/O,
//! no persistence, no registry, no tabs, no prompts, no session restore.
//! WM_CLOSE destroys the window and the process exits, so hidden-desktop
//! teardown needs nothing but the close post.
//! P3b paging tests pass argv[2] = N to append N named buttons after the
//! edit (overflow for the 200-entry page); without it the window is exactly
//! the single-edit fixture as before.
//!
//! Test-only in spirit (only the desktop live tests launch it), but
//! built as a regular `src/bin` target so integration tests resolve it
//! through `env!("CARGO_BIN_EXE_puppet_fixture_app")`.
//!
//! W6.2 adds the `caps` mode, the only mode that differs from the window
//! above: a frame hosting a child button at a known client rect, a
//! message-swallowing control, a checkbox, a listbox, a grid, the p4c
//! tree/trackbar the state object reads, and a stamped message log
//! readable through `elements` detail:text. Every capability is opt-in by
//! argv, so `p4c`, a numeric button count and the baseline window are
//! byte-for-byte what they were.
//!
//! W2.1 adds the p4c mode's live button and its activation counter: a posted
//! `WM_APP+1` renames the button (same auto_id, new name, so its fingerprint
//! moves while its selector keeps resolving), and every activation is
//! mirrored into a read-only STATIC, so a refused mutation can prove it
//! activated nothing.

#![windows_subsystem = "windows"]

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{GetSysColorBrush, MapWindowPoints, COLOR_WINDOW};
use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;
use windows_sys::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_BAR_CLASSES, ICC_LISTVIEW_CLASSES, ICC_TREEVIEW_CLASSES,
    INITCOMMONCONTROLSEX, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVIF_TEXT, LVITEMW, LVM_INSERTCOLUMNW,
    LVM_INSERTITEMW, LVS_REPORT, TBM_SETPOS, TBM_SETRANGE, TBS_AUTOTICKS, TVIF_TEXT,
    TVINSERTSTRUCTW, TVITEMW, TVI_LAST, TVI_ROOT, TVM_INSERTITEMW, TVS_HASBUTTONS, TVS_HASLINES,
    TVS_LINESATROOT,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// ---------------------------------------------------------------- W6.2 caps
// Control ids double as UIA automation ids, so the walk accepts every entry
// (name + auto_id + invokable rule, `src/uia.rs:714`).
const CAPS_LOG_ID: i32 = 4000;
const CAPS_SWALLOW_ID: i32 = 4001;
const CAPS_LIST_ID: i32 = 4002;
const CAPS_GRID_ID: i32 = 4003;
const CAPS_TOGGLE_ID: i32 = 4004;
const CAPS_FRAME_ID: i32 = 4005;
const CAPS_TRACK_ID: i32 = 4006;
const CAPS_TREE_ID: i32 = 4007;
const CAPS_FRAME_BTN_ID: i32 = 4100;

// ---------------------------------------------------------------- W2.1 p4c
// The p4c window's live button, the read-only STATIC that reports its
// activation count, and the posted message that re-renders (renames) it. A
// rename changes `name`, so the element fingerprint moves while `auto_id`
// keeps resolving: the selector still finds a control, just not the one the
// snapshot saw.
const P4C_LIVE_BTN_ID: i32 = 3003;
const P4C_COUNT_ID: i32 = 3004;
// W2.3: the control a wait snapshots (disabled, so an `enabled` wait cannot be
// satisfied by it) and the control that replaces it — a fresh one with the same
// name and a different automation id, so its fingerprint moves.
const P4C_WAIT_ID: i32 = 3005;
const P4C_WAIT_TAKER_ID: i32 = 3007;
const WM_P4C_RENAME: u32 = WM_APP + 1;
const WM_P4C_SWAP: u32 = WM_APP + 2;

/// Button hwnd, counter hwnd and activation count, all 0 until the p4c
/// window creates them: every other mode leaves them 0, so its behaviour is
/// unchanged.
static P4C_BTN: AtomicIsize = AtomicIsize::new(0);
static P4C_COUNT: AtomicIsize = AtomicIsize::new(0);
static P4C_ACT: AtomicIsize = AtomicIsize::new(0);
/// W2.3's pair: the disabled holder of the waited name and the control that
/// takes it, 0 until the p4c window creates them.
static P4C_WAIT: AtomicIsize = AtomicIsize::new(0);
static P4C_WAIT_NEW: AtomicIsize = AtomicIsize::new(0);

/// The stamped log's own window, 0 until it exists.
static CAPS_LOG_HWND: AtomicIsize = AtomicIsize::new(0);
/// Set by the caps window's first WM_PAINT: the harness's readiness signal.
static CAPS_PAINTED: AtomicBool = AtomicBool::new(false);
/// Process-relative clock: every stamp is milliseconds since boot.
static CAPS_START: OnceLock<Instant> = OnceLock::new();
static CAPS_LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn caps_ms() -> u128 {
    CAPS_START.get_or_init(Instant::now).elapsed().as_millis()
}

/// Append one stamped line and mirror the whole log into the read-only
/// RichEdit the harness reads through `elements` detail:text. Bounded so the
/// 4000-char text cap never eats the newest lines.
fn caps_push(line: String) {
    let text = {
        let mut lines = match CAPS_LINES.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        lines.push(format!("t={} {}", caps_ms(), line));
        while lines.len() > 60 {
            lines.remove(0);
        }
        lines.join("\r\n")
    };
    let h = CAPS_LOG_HWND.load(Ordering::SeqCst);
    if h != 0 {
        let w = wide(&text);
        unsafe { SetWindowTextW(h as HWND, w.as_ptr()) };
    }
}

/// GET_X_LPARAM/GET_Y_LPARAM: signed client coordinates out of an lParam.
fn caps_point(l: isize) -> (i32, i32) {
    let x = (l & 0xffff) as u16 as i16 as i32;
    let y = ((l >> 16) & 0xffff) as u16 as i16 as i32;
    (x, y)
}

fn caps_msg_name(msg: u32) -> Option<&'static str> {
    Some(match msg {
        WM_LBUTTONDOWN => "WM_LBUTTONDOWN",
        WM_LBUTTONUP => "WM_LBUTTONUP",
        WM_LBUTTONDBLCLK => "WM_LBUTTONDBLCLK",
        WM_RBUTTONDOWN => "WM_RBUTTONDOWN",
        WM_RBUTTONUP => "WM_RBUTTONUP",
        WM_MOUSEMOVE => "WM_MOUSEMOVE",
        WM_MOUSEWHEEL => "WM_MOUSEWHEEL",
        WM_CHAR => "WM_CHAR",
        WM_KEYDOWN => "WM_KEYDOWN",
        WM_CLOSE => "WM_CLOSE",
        _ => return None,
    })
}

fn caps_msg_line(name: &str, msg: u32, l: isize) -> String {
    match msg {
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN | WM_RBUTTONUP
        | WM_MOUSEMOVE => {
            let (x, y) = caps_point(l);
            format!("{name} x={x} y={y}")
        }
        _ => name.to_string(),
    }
}

fn caps_name_of(id: i32) -> &'static str {
    match id {
        CAPS_FRAME_BTN_ID => "dc-frame-btn",
        CAPS_TOGGLE_ID => "dc-toggle",
        _ => "control",
    }
}

/// The caps top-level: logs every message an agent can post, and counts
/// activations as `activate <name>` lines (an absent line is zero
/// activations, which is what W4.1/W4.2 assert).
unsafe extern "system" fn wndproc_caps(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    match msg {
        // Readiness signal: the first processed WM_PAINT is the moment this
        // window has a frame a capture can accept, so the harness waits for
        // this line instead of guessing with a sleep. A fresh window reads
        // all-black until then and the capture refuses that frame.
        WM_PAINT => {
            if !CAPS_PAINTED.swap(true, Ordering::SeqCst) {
                caps_push("painted".to_string());
            }
            return DefWindowProcW(hwnd, msg, w, l);
        }
        WM_CLOSE => {
            caps_push("WM_CLOSE".to_string());
            DestroyWindow(hwnd);
            return 0;
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            return 0;
        }
        WM_COMMAND => {
            let id = (w & 0xffff) as i32;
            let code = ((w >> 16) & 0xffff) as u32;
            if code == BN_CLICKED {
                caps_push(format!("activate {}", caps_name_of(id)));
            }
            return 0;
        }
        _ => {}
    }
    if let Some(name) = caps_msg_name(msg) {
        caps_push(caps_msg_line(name, msg, l));
    }
    DefWindowProcW(hwnd, msg, w, l)
}

/// The message-swallowing control: it records every button message it is
/// handed and returns 0, so it never activates and never reports anything
/// else. A fixture for W4.1's `effect:"ignored"`.
unsafe extern "system" fn wndproc_swallow(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    match msg {
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN | WM_RBUTTONUP => {
            let (x, y) = caps_point(l);
            let name = caps_msg_name(msg).unwrap_or("button");
            caps_push(format!("swallow {name} x={x} y={y}"));
            return 0;
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            return 0;
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, w, l)
}

/// The frame around the child button: plain default processing, so it is a
/// real container a hit-test can resolve instead of the top-level.
unsafe extern "system" fn wndproc_plain(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    DefWindowProcW(hwnd, msg, w, l)
}

/// One `layout <name> cx=… cy=… w=… h=…` line in the top-level's CLIENT
/// coordinates: the known rect a coordinate click aims at, measured rather
/// than assumed.
fn caps_layout(top: HWND, child: HWND, name: &str) {
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        if GetWindowRect(child, &mut r) == 0 {
            return;
        }
        let mut a = POINT {
            x: r.left,
            y: r.top,
        };
        let mut b = POINT {
            x: r.right,
            y: r.bottom,
        };
        MapWindowPoints(std::ptr::null_mut(), top, &mut a, 1);
        MapWindowPoints(std::ptr::null_mut(), top, &mut b, 1);
        caps_push(format!(
            "layout {name} cx={} cy={} w={} h={}",
            a.x,
            a.y,
            b.x - a.x,
            b.y - a.y
        ));
    }
}

/// Register a plain child-window class with a light background (a black
/// frame would read as a blank capture) and the given window proc.
fn caps_register_class(name: &str, proc_: WNDPROC) {
    let cls = wide(name);
    unsafe {
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = proc_;
        wc.hInstance = std::ptr::null_mut();
        wc.lpszClassName = cls.as_ptr();
        wc.hbrBackground = GetSysColorBrush(COLOR_WINDOW);
        wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        RegisterClassW(&wc);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    if msg == WM_CLOSE {
        DestroyWindow(hwnd);
        return 0;
    }
    if msg == WM_DESTROY {
        PostQuitMessage(0);
        return 0;
    }
    // W2.1: the p4c live button counts its activations into a read-only
    // STATIC (an absent activation reads as `activations=0`), and renames
    // itself on the posted re-render, which moves its fingerprint without
    // moving its selector.
    if msg == WM_COMMAND {
        let id = (w & 0xffff) as i32;
        let code = ((w >> 16) & 0xffff) as u32;
        if code == BN_CLICKED && id == P4C_LIVE_BTN_ID {
            let n = P4C_ACT.fetch_add(1, Ordering::SeqCst) + 1;
            let count = P4C_COUNT.load(Ordering::SeqCst);
            if count != 0 {
                SetWindowTextW(count as HWND, wide(&format!("activations={n}")).as_ptr());
            }
            return 0;
        }
    }
    if msg == WM_P4C_RENAME {
        let btn = P4C_BTN.load(Ordering::SeqCst);
        if btn != 0 {
            SetWindowTextW(btn as HWND, wide("p4c-live-2").as_ptr());
        }
        return 0;
    }
    // W2.3: a different control takes the name the wait is polling for. The
    // holder is DESTROYED rather than renamed: the Win32 UIA provider serves a
    // cached Name for an existing element for seconds after WM_SETTEXT, so a
    // rename would make the swap unobservable inside a wait. A destroyed
    // control leaves the tree at once and the new one is a fresh element.
    if msg == WM_P4C_SWAP {
        let holder = P4C_WAIT.load(Ordering::SeqCst);
        if holder != 0 {
            DestroyWindow(holder as HWND);
            P4C_WAIT.store(0, Ordering::SeqCst);
        }
        let newcomer = CreateWindowExW(
            0,
            wide("BUTTON").as_ptr(),
            wide("p4c-wait").as_ptr(),
            WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32,
            12,
            320,
            200,
            24,
            hwnd,
            P4C_WAIT_TAKER_ID as isize as *mut _,
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if !newcomer.is_null() {
            P4C_WAIT_NEW.store(newcomer as isize, Ordering::SeqCst);
        }
        return 0;
    }
    DefWindowProcW(hwnd, msg, w, l)
}

fn main() {
    let mut argv = std::env::args().skip(1);
    let title = argv
        .next()
        .unwrap_or_else(|| "puppet hidden fixture".to_string());
    // Opt-in stress mode for paging tests: a second argv element N creates
    // N named buttons after the baseline edit (no other behaviour changes;
    // without it the window is exactly the single-edit fixture as before).
    // A second argv element of exactly "p4c" instead builds the P4c
    // pattern-proof controls (tree + trackbar + multiline edit).
    let mode = argv.next().unwrap_or_default();
    let p4c = mode == "p4c";
    // W6.2 capability window: the p4c controls plus the frame/button,
    // swallowing control, checkbox, listbox, grid and stamped log.
    let caps = mode == "caps";
    let extra: usize = if p4c || caps {
        0
    } else {
        mode.parse().ok().map(|n: usize| n.min(1000)).unwrap_or(0)
    };
    unsafe {
        let cls_w = wide("PuppetFixtureApp");
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(if caps { wndproc_caps } else { wndproc });
        wc.style = if caps { CS_DBLCLKS } else { 0 };
        wc.hInstance = std::ptr::null_mut();
        wc.lpszClassName = cls_w.as_ptr();
        wc.hbrBackground = if caps {
            GetSysColorBrush(COLOR_WINDOW)
        } else {
            std::ptr::null_mut()
        };
        wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        if RegisterClassW(&wc) == 0 {
            return;
        }
        let (win_w, win_h) = if caps {
            (760, 620)
        } else if p4c {
            // W2.1's live button and activation counter sit under the
            // baseline edit; the extra height is the room for them.
            (480, 420)
        } else {
            (480, 320)
        };
        let title_w = wide(&title);
        let edit_cls = wide("EDIT");
        let base_w = wide("baseline");
        let hwnd = CreateWindowExW(
            0,
            cls_w.as_ptr(),
            title_w.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            100,
            100,
            win_w,
            win_h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return;
        }
        let edit = CreateWindowExW(
            0,
            edit_cls.as_ptr(),
            base_w.as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_BORDER,
            12,
            12,
            440,
            if caps { 50 } else { 240 },
            hwnd,
            100 as *mut _,
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if edit.is_null() {
            DestroyWindow(hwnd);
            return;
        }
        // W6.2 capabilities, all opt-in by the `caps` mode: a frame with a
        // child button at a known client rect (W4.2), a message-swallowing
        // control (W4.1), a checkbox, a listbox, a grid (W5.1), the p4c
        // tree/trackbar the state object reads, and a stamped message log
        // (W4.4, W4.5) readable through `elements` detail:text.
        if caps {
            let icc = INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_TREEVIEW_CLASSES | ICC_BAR_CLASSES | ICC_LISTVIEW_CLASSES,
            };
            InitCommonControlsEx(&icc);
            caps_register_class("PuppetCapsFrame", Some(wndproc_plain));
            caps_register_class("PuppetCapsSwallow", Some(wndproc_swallow));
            let btn_cls = wide("BUTTON");
            // A frame hosting a child Button: the point resolves to a child
            // window, never to the locked top-level.
            let frame_cls = wide("PuppetCapsFrame");
            let frame = CreateWindowExW(
                0,
                frame_cls.as_ptr(),
                wide("dc-frame").as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_BORDER,
                12,
                72,
                220,
                120,
                hwnd,
                CAPS_FRAME_ID as isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            let mut frame_btn: HWND = std::ptr::null_mut();
            if !frame.is_null() {
                frame_btn = CreateWindowExW(
                    0,
                    btn_cls.as_ptr(),
                    wide("dc-frame-btn").as_ptr(),
                    WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32,
                    20,
                    20,
                    120,
                    30,
                    frame,
                    CAPS_FRAME_BTN_ID as isize as *mut _,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                );
            }
            // A control that swallows button messages and never activates.
            let swallow_cls = wide("PuppetCapsSwallow");
            let swallow = CreateWindowExW(
                0,
                swallow_cls.as_ptr(),
                wide("dc-swallow").as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_BORDER,
                12,
                200,
                220,
                40,
                hwnd,
                CAPS_SWALLOW_ID as isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            // ToggleButton (TogglePattern) for the state object.
            let toggle = CreateWindowExW(
                0,
                btn_cls.as_ptr(),
                wide("dc-toggle").as_ptr(),
                WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32,
                12,
                248,
                220,
                24,
                hwnd,
                CAPS_TOGGLE_ID as isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            // ListBox: SelectionItem-shaped items.
            let list_cls = wide("LISTBOX");
            let list = CreateWindowExW(
                0,
                list_cls.as_ptr(),
                wide("dc-listbox").as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_BORDER | LBS_NOTIFY as u32,
                12,
                280,
                220,
                80,
                hwnd,
                CAPS_LIST_ID as isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if !list.is_null() {
                for item in ["dc-item-1", "dc-item-2", "dc-item-3"] {
                    SendMessageW(list, LB_ADDSTRING, 0, wide(item).as_ptr() as isize);
                }
                SendMessageW(list, LB_SETCURSEL, 0, 0);
            }
            // A GridItem-shaped table: a report-mode ListView with two
            // columns and three rows.
            let grid_cls = wide("SysListView32");
            let grid = CreateWindowExW(
                0,
                grid_cls.as_ptr(),
                wide("dc-grid").as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_BORDER | LVS_REPORT,
                244,
                72,
                300,
                120,
                hwnd,
                CAPS_GRID_ID as isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if !grid.is_null() {
                for (i, head) in ["dc-col-a", "dc-col-b"].iter().enumerate() {
                    let head_w = wide(head);
                    let mut col: LVCOLUMNW = std::mem::zeroed();
                    col.mask = LVCF_TEXT | LVCF_WIDTH;
                    col.cx = 120;
                    col.pszText = head_w.as_ptr() as *mut u16;
                    SendMessageW(grid, LVM_INSERTCOLUMNW, i, &col as *const _ as isize);
                }
                for row in 0..3 {
                    for col in 0..2 {
                        let cell = wide(&format!("dc-r{}c{}", row + 1, col + 1));
                        let mut item: LVITEMW = std::mem::zeroed();
                        item.mask = LVIF_TEXT;
                        item.iItem = row;
                        item.iSubItem = col;
                        item.pszText = cell.as_ptr() as *mut u16;
                        SendMessageW(grid, LVM_INSERTITEMW, 0, &item as *const _ as isize);
                    }
                }
            }
            // The tree and trackbar the W5.1 state readers are proven on.
            let tree_cls = wide("SysTreeView32");
            let tree = CreateWindowExW(
                0,
                tree_cls.as_ptr(),
                std::ptr::null(),
                WS_CHILD | WS_VISIBLE | WS_BORDER | TVS_HASBUTTONS | TVS_HASLINES | TVS_LINESATROOT,
                244,
                200,
                150,
                100,
                hwnd,
                CAPS_TREE_ID as isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if !tree.is_null() {
                let root_w = wide("p4c-root");
                let mut root_item: TVITEMW = std::mem::zeroed();
                root_item.mask = TVIF_TEXT;
                root_item.pszText = root_w.as_ptr() as *mut u16;
                let mut root_ins: TVINSERTSTRUCTW = std::mem::zeroed();
                root_ins.hParent = TVI_ROOT;
                root_ins.hInsertAfter = TVI_LAST;
                root_ins.Anonymous.item = root_item;
                let hroot = SendMessageW(tree, TVM_INSERTITEMW, 0, &root_ins as *const _ as isize);
                if hroot != 0 {
                    let child_w = wide("p4c-child");
                    let mut child_item: TVITEMW = std::mem::zeroed();
                    child_item.mask = TVIF_TEXT;
                    child_item.pszText = child_w.as_ptr() as *mut u16;
                    let mut child_ins: TVINSERTSTRUCTW = std::mem::zeroed();
                    child_ins.hParent = hroot;
                    child_ins.hInsertAfter = TVI_LAST;
                    child_ins.Anonymous.item = child_item;
                    SendMessageW(tree, TVM_INSERTITEMW, 0, &child_ins as *const _ as isize);
                }
            }
            let track_cls = wide("msctls_trackbar32");
            let track = CreateWindowExW(
                0,
                track_cls.as_ptr(),
                std::ptr::null(),
                WS_CHILD | WS_VISIBLE | TBS_AUTOTICKS,
                244,
                308,
                150,
                40,
                hwnd,
                CAPS_TRACK_ID as isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if !track.is_null() {
                SendMessageW(track, TBM_SETRANGE, 1, (100 << 16) as isize);
                SendMessageW(track, TBM_SETPOS, 1, 0);
            }
            // The stamped log, last: every earlier control can write to it.
            LoadLibraryW(wide("Msftedit.dll").as_ptr());
            let rich_cls = wide("RICHEDIT50W");
            let log = CreateWindowExW(
                0,
                rich_cls.as_ptr(),
                std::ptr::null(),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_BORDER
                    | WS_VSCROLL
                    | ES_MULTILINE as u32
                    | ES_READONLY as u32,
                12,
                368,
                532,
                120,
                hwnd,
                CAPS_LOG_ID as isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            CAPS_START.get_or_init(Instant::now);
            if !log.is_null() {
                CAPS_LOG_HWND.store(log as isize, Ordering::SeqCst);
            }
            caps_push("boot".to_string());
            caps_layout(hwnd, frame_btn, "dc-frame-btn");
            caps_layout(hwnd, swallow, "dc-swallow");
            caps_layout(hwnd, toggle, "dc-toggle");
        }
        // Paging stress controls: named buttons stacked below the edit.
        // They need no layout (the walk enumerates offscreen children) and
        // no input; their only job is to overflow the 200-entry page.
        // A shared-label pair brackets the numbered tail: the first dup
        // lands on page 1, the second past the 200-entry cap, so a
        // name:p3b-dup wait must see both (ambiguous) only with a full
        // cross-page scan.
        if extra > 0 {
            let btn_cls = wide("BUTTON");
            let dup = wide("p3b-dup");
            CreateWindowExW(
                0,
                btn_cls.as_ptr(),
                dup.as_ptr(),
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32,
                12,
                260,
                200,
                22,
                hwnd,
                2000_isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            for i in 0..extra {
                let label = wide(&format!("p3b-btn-{i}"));
                CreateWindowExW(
                    0,
                    btn_cls.as_ptr(),
                    label.as_ptr(),
                    WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32,
                    12,
                    260,
                    200,
                    22,
                    hwnd,
                    (1000 + i) as isize as *mut _,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                );
            }
            CreateWindowExW(
                0,
                btn_cls.as_ptr(),
                dup.as_ptr(),
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32,
                12,
                260,
                200,
                22,
                hwnd,
                2001_isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
        }
        // P4c pattern-proof controls: a SysTreeView32 with a root + child
        // node (ExpandCollapse), a trackbar (RangeValue), and a RichEdit
        // multiline edit (TextPattern). Control IDs double as UIA
        // automation ids so the walk accepts them (name + auto_id +
        // invokable rule).
        if p4c {
            let icc = INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_TREEVIEW_CLASSES | ICC_BAR_CLASSES,
            };
            InitCommonControlsEx(&icc);
            let tree_cls = wide("SysTreeView32");
            let tree = CreateWindowExW(
                0,
                tree_cls.as_ptr(),
                std::ptr::null(),
                WS_CHILD | WS_VISIBLE | WS_BORDER | TVS_HASBUTTONS | TVS_HASLINES | TVS_LINESATROOT,
                236,
                12,
                220,
                120,
                hwnd,
                3000isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if !tree.is_null() {
                let root_w = wide("p4c-root");
                let mut root_item: TVITEMW = std::mem::zeroed();
                root_item.mask = TVIF_TEXT;
                root_item.pszText = root_w.as_ptr() as *mut u16;
                let mut root_ins: TVINSERTSTRUCTW = std::mem::zeroed();
                root_ins.hParent = TVI_ROOT;
                root_ins.hInsertAfter = TVI_LAST;
                root_ins.Anonymous.item = root_item;
                let hroot = SendMessageW(tree, TVM_INSERTITEMW, 0, &root_ins as *const _ as isize);
                if hroot != 0 {
                    let child_w = wide("p4c-child");
                    let mut child_item: TVITEMW = std::mem::zeroed();
                    child_item.mask = TVIF_TEXT;
                    child_item.pszText = child_w.as_ptr() as *mut u16;
                    let mut child_ins: TVINSERTSTRUCTW = std::mem::zeroed();
                    child_ins.hParent = hroot;
                    child_ins.hInsertAfter = TVI_LAST;
                    child_ins.Anonymous.item = child_item;
                    SendMessageW(tree, TVM_INSERTITEMW, 0, &child_ins as *const _ as isize);
                }
            }
            let track_cls = wide("msctls_trackbar32");
            let track = CreateWindowExW(
                0,
                track_cls.as_ptr(),
                std::ptr::null(),
                WS_CHILD | WS_VISIBLE | TBS_AUTOTICKS,
                236,
                140,
                220,
                40,
                hwnd,
                3001isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if !track.is_null() {
                // Range 0..100, initial position 0.
                SendMessageW(track, TBM_SETRANGE, 1, (100 << 16) as isize);
                SendMessageW(track, TBM_SETPOS, 1, 0);
            }
            let rich_cls = wide("RICHEDIT50W");
            LoadLibraryW(wide("Msftedit.dll").as_ptr());
            let mut big = String::from("p4c-line1");
            for i in 2..=200 {
                big.push_str(&format!("\r\np4c-line{i}"));
            }
            let mledit = CreateWindowExW(
                0,
                rich_cls.as_ptr(),
                wide(&big).as_ptr(),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_BORDER
                    | WS_VSCROLL
                    | ES_MULTILINE as u32
                    | ES_AUTOVSCROLL as u32,
                236,
                188,
                220,
                100,
                hwnd,
                3002isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if mledit.is_null() {
                DestroyWindow(hwnd);
                return;
            }
            // W2.1: a live button whose selector survives a re-render. The
            // posted WM_P4C_RENAME renames it (fingerprint moves, auto_id
            // does not), and every activation is counted into the STATIC so
            // a refused mutation can prove zero activations.
            let btn_cls = wide("BUTTON");
            let live_btn = CreateWindowExW(
                0,
                btn_cls.as_ptr(),
                wide("p4c-live").as_ptr(),
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32,
                12,
                260,
                200,
                26,
                hwnd,
                P4C_LIVE_BTN_ID as isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            let count = CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                wide("activations=0").as_ptr(),
                WS_CHILD | WS_VISIBLE,
                12,
                292,
                200,
                20,
                hwnd,
                P4C_COUNT_ID as isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if live_btn.is_null() || count.is_null() {
                DestroyWindow(hwnd);
                return;
            }
            P4C_BTN.store(live_btn as isize, Ordering::SeqCst);
            P4C_COUNT.store(count as isize, Ordering::SeqCst);
            // W2.3's pair: a disabled holder of `p4c-wait`, so an `enabled`
            // wait cannot be satisfied by it, and (on the posted swap) a fresh
            // enabled control with the same name and a different automation id.
            let wait_btn = CreateWindowExW(
                0,
                btn_cls.as_ptr(),
                wide("p4c-wait").as_ptr(),
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32,
                12,
                320,
                200,
                24,
                hwnd,
                P4C_WAIT_ID as isize as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if wait_btn.is_null() {
                DestroyWindow(hwnd);
                return;
            }
            EnableWindow(wait_btn, 0);
            P4C_WAIT.store(wait_btn as isize, Ordering::SeqCst);
        }
        ShowWindow(hwnd, SW_SHOW);
        let mut msg: MSG = std::mem::zeroed();
        loop {
            let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            if r == 0 || r == -1 {
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
