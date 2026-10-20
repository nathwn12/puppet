//! P3a hidden-desktop proof with a dedicated fixture app: `desktop:"hidden"`
//! launches `puppet_fixture_app` (a single window + one EDIT control, no
//! persistence, no session restore, no tabs) with `background_args`, drives
//! it through the PostMessage path, and tears the desktop down with proof —
//! without touching the owner's desktop or any owner app state.
//!
//! Live-only: needs an interactive Windows session (hidden desktops).
//! Gated by PUPPET_LIVE_DESKTOP=1; without it the tests print a skip line
//! and pass so CI stays green. No owner files are read, referenced, or
//! modified anywhere in this file.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(60);

fn live() -> bool {
    if std::env::var("PUPPET_LIVE_DESKTOP").as_deref() == Ok("1") {
        true
    } else {
        println!(
            "SKIP hidden_desktop: set PUPPET_LIVE_DESKTOP=1 to run (needs an interactive Windows session)"
        );
        false
    }
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
}

impl Session {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_puppet"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn puppet binary");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let t = line.trim().to_string();
                        if t.is_empty() {
                            continue;
                        }
                        if tx.send(t).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Session { child, stdin, rx }
    }

    fn result(&mut self, id: i64, name: &str, arguments: Value) -> Value {
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        });
        let line = serde_json::to_string(&frame).unwrap();
        self.stdin.write_all(line.as_bytes()).expect("write frame");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush frame");
        let resp: Value = self
            .rx
            .recv_timeout(TIMEOUT)
            .ok()
            .map(|l| serde_json::from_str(&l).expect("response is JSON"))
            .expect("response in time");
        resp.get("result")
            .cloned()
            .unwrap_or_else(|| panic!("tools/call {name} must succeed, got: {resp}"))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fg_now() -> i64 {
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() as i64 }
}

fn pid_of_window(hwnd: i64) -> Option<u32> {
    if hwnd == 0 {
        return None;
    }
    unsafe {
        let mut pid: u32 = 0;
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
            hwnd as windows_sys::Win32::Foundation::HWND,
            &mut pid,
        );
        if pid == 0 {
            None
        } else {
            Some(pid)
        }
    }
}

fn check_hidden_proven(hd: &Value, what: &str) {
    assert_eq!(
        hd.get("teardown").and_then(|t| t.get("proven")),
        Some(&Value::from(true)),
        "{what} hidden teardown must be proven, got: {hd}"
    );
    assert_eq!(
        hd.get("absent_from_owner_enum_windows"),
        Some(&Value::from(true)),
        "{what} hidden window must never appear on the owner desktop, got: {hd}"
    );
}

#[test]
fn hidden_fixture_typing_with_teardown_proof() {
    if !live() {
        return;
    }
    // The dedicated fixture app: title comes from argv[1], so the title
    // assert proves the argv reached the hidden launch.
    let fixture = env!("CARGO_BIN_EXE_puppet_fixture_app");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let marker = format!("hello-p3a-{}-{nanos}", std::process::id());
    let mut s = Session::spawn();
    let fg_before = fg_now();

    let hidden = |s: &mut Session, id: i64, live: bool| -> (Value, Value) {
        let mut args = serde_json::json!({
            "actions": [{"type": "type", "text": marker.as_str()}],
            "background": fixture,
            "background_args": ["isolation-marker"],
            "desktop": "hidden",
        });
        if !live {
            args["dry_run"] = Value::from(true);
        }
        let r = s.result(id, "do", args);
        assert_eq!(
            r.get("isError"),
            Some(&Value::from(false)),
            "hidden do must stay isError:false (continue-always), got: {r}"
        );
        let hd = r
            .get("structuredContent")
            .and_then(|sc| sc.get("hidden_desktop"))
            .expect("hidden do carries hidden_desktop evidence")
            .clone();
        check_hidden_proven(&hd, if live { "live" } else { "baseline" });
        let title = hd
            .get("window_title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        assert!(
            title.contains("isolation-marker"),
            "hidden window title must prove argv delivery (title {title:?} must contain isolation-marker)"
        );
        (r, hd)
    };

    // Baseline: dry_run launch opens the fixture and reads it back.
    let (_, hd0) = hidden(&mut s, 10, false);
    let base = hd0
        .get("text_readback")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    assert!(
        base.contains("baseline"),
        "baseline readback must hold the fixture's initial text, got: {base:?}"
    );
    assert!(
        !base.contains("hello"),
        "baseline readback must be marker-free, got: {base:?}"
    );

    // Live: type the unique marker into the fixture's edit control.
    let (r, hd) = hidden(&mut s, 11, true);
    let sc = r
        .get("structuredContent")
        .expect("hidden do has structuredContent");
    let first = sc
        .get("results")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .expect("first per-action result");
    assert_eq!(
        first.get("ok"),
        Some(&Value::from(true)),
        "hidden type must post, got: {first}"
    );
    assert!(
        first.get("posted").and_then(|v| v.as_i64()).unwrap_or(0) > 0,
        "hidden type must post >0 messages, got: {first}"
    );
    let readback = hd
        .get("text_readback")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    assert!(
        readback.contains("baseline"),
        "live readback must hold the fixture's initial text, got: {readback:?}"
    );
    assert!(
        readback.contains(&marker),
        "live readback must contain the typed marker, got: {readback:?} (full: {hd})"
    );
    // The PrintWindow capture rides as a real MCP image block next to the
    // text JSON — not only base64 inside it.
    let capture_ok = hd
        .get("capture")
        .and_then(|c| c.get("ok"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    assert!(
        capture_ok,
        "fixture capture must succeed on the hidden desktop, got: {hd}"
    );
    let content = r
        .get("content")
        .and_then(|c| c.as_array())
        .expect("content blocks");
    assert!(
        content
            .iter()
            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("image")),
        "hidden capture must add an MCP image block, got types: {:?}",
        content
            .iter()
            .map(|b| b.get("type").cloned().unwrap_or(Value::Null))
            .collect::<Vec<_>>()
    );
    // The hidden path must not activate anything on the owner's desktop.
    // The box may be in active use (cursor and foreground move under owner
    // activity, which PostMessage/WM_CLOSE to a hidden window cannot cause),
    // so plain unchanged-ness is reported, not failed — while the actual
    // safety property IS asserted: the hidden window never became the
    // owner's foreground window and the launched pid never owned it.
    let fg_after = fg_now();
    let hidden_hwnd = hd.get("window_hwnd").and_then(|v| v.as_i64()).unwrap_or(0);
    if fg_before == 0 {
        println!("FG_SKIP hidden_fixture: no foreground session (before=0)");
    } else if fg_after == fg_before {
        println!("FG_UNCHANGED hidden_fixture: foreground unchanged ({fg_before})");
    } else {
        println!(
            "FG_EXTERNAL hidden_fixture: foreground changed under owner activity ({fg_before} -> {fg_after}); not attributed to the hidden path"
        );
    }
    assert_ne!(
        fg_after, hidden_hwnd,
        "hidden window must never become the owner foreground window"
    );
    if let Some(pid) = hd.get("launched_pid").and_then(|v| v.as_i64()) {
        if fg_after != 0 && pid != 0 {
            assert_ne!(
                pid_of_window(fg_after),
                Some(pid as u32),
                "launched pid {pid} must never own the owner foreground window"
            );
        }
    }
    // Isolation cross-check from the owner's side: the launched pid owns
    // no window visible to the owner after teardown.
    if let Some(pid) = hd.get("launched_pid").and_then(|v| v.as_i64()) {
        let lw = s.result(12, "list_windows", serde_json::json!({}));
        let wins = lw
            .get("structuredContent")
            .and_then(|x| x.get("windows"))
            .and_then(|w| w.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(
            wins.iter()
                .all(|w| w.get("pid").and_then(|p| p.as_i64()) != Some(pid)),
            "launched pid {pid} must own no owner-desktop window after teardown"
        );
    }
    // Revert proof, same fixture: pre_drive_text is the target text
    // before any driven input, post_revert_text is read after the revert
    // in the same teardown, and reverted is their equality. A cross-launch
    // comparison could not prove the restore ran (every launch initializes
    // the same content); this one fails if the revert is removed.
    let pre = hd
        .get("pre_drive_text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    assert!(
        pre.contains("baseline"),
        "pre_drive_text must hold the fixture's initial text, got: {pre:?}"
    );
    assert!(
        !pre.contains("hello"),
        "pre_drive_text must be marker-free, got: {pre:?}"
    );
    let post = hd
        .get("post_revert_text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    assert_eq!(
        post, pre,
        "post_revert_text must equal pre_drive_text (same fixture, same teardown), got: {post:?} vs {pre:?}"
    );
    assert!(
        !post.contains("hello"),
        "post_revert_text must be marker-free, got: {post:?}"
    );
    assert_eq!(
        hd.get("reverted"),
        Some(&Value::from(true)),
        "reverted must be true when post_revert_text equals pre_drive_text, got: {hd}"
    );
    assert_ne!(
        readback, post,
        "typed readback must differ from the reverted text, got both: {readback:?}"
    );
    // background_args must be a list of strings. These fail in serve
    // validation before any launch, so they are deterministic.
    for (id, bad_args, want) in [
        (20, serde_json::json!("notalist"), "valid list"),
        (21, serde_json::json!([123]), "valid string"),
    ] {
        let rv = s.result(
            id,
            "do",
            serde_json::json!({
                "actions": [{"type": "type", "text": "x"}],
                "background": fixture,
                "background_args": bad_args,
                "desktop": "hidden",
                "dry_run": true,
            }),
        );
        assert_eq!(
            rv.get("isError"),
            Some(&Value::from(true)),
            "bad background_args must be a validation error, got: {rv}"
        );
        let text: String = rv
            .get("content")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            text.contains(want),
            "background_args validation must name the shape ({want}), got: {text}"
        );
    }
    println!("HIDDEN_OK proven=true readback={readback:?} reverted=true");
}
