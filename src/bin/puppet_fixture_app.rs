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

#![windows_subsystem = "windows"]

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;
use windows_sys::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_BAR_CLASSES, ICC_TREEVIEW_CLASSES, INITCOMMONCONTROLSEX, TBM_SETPOS,
    TBM_SETRANGE, TBS_AUTOTICKS, TVIF_TEXT, TVINSERTSTRUCTW, TVITEMW, TVI_LAST, TVI_ROOT,
    TVM_INSERTITEMW, TVS_HASBUTTONS, TVS_HASLINES, TVS_LINESATROOT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

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
    let extra: usize = if p4c {
        0
    } else {
        mode.parse().ok().map(|n: usize| n.min(1000)).unwrap_or(0)
    };
    unsafe {
        let cls_w = wide("PuppetFixtureApp");
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = std::ptr::null_mut();
        wc.lpszClassName = cls_w.as_ptr();
        wc.hbrBackground = std::ptr::null_mut();
        wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        if RegisterClassW(&wc) == 0 {
            return;
        }
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
            480,
            320,
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
            240,
            hwnd,
            100 as *mut _,
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if edit.is_null() {
            DestroyWindow(hwnd);
            return;
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
