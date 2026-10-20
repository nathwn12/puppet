//! P3a clipboard proof: `do` `clipboard_set`/`clipboard_get` round-trip
//! through CF_UNICODETEXT, and `dry_run` for `clipboard_set` reports
//! without touching the clipboard.
//!
//! Live-only: needs a real interactive session clipboard. Gated by
//! PUPPET_LIVE_DESKTOP=1; without it the tests print a skip line and pass
//! so CI stays green. The saved clipboard text is restored before return.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(30);

fn live() -> bool {
    if std::env::var("PUPPET_LIVE_DESKTOP").as_deref() == Ok("1") {
        true
    } else {
        println!(
            "SKIP clipboard: set PUPPET_LIVE_DESKTOP=1 to run (needs an interactive Windows session)"
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

    fn do_first(&mut self, id: i64, arguments: Value) -> Value {
        let r = self.result(id, "do", arguments);
        r.get("structuredContent")
            .and_then(|sc| sc.get("results"))
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .cloned()
            .unwrap_or_else(|| panic!("do must return per-action results, got: {r}"))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn clipboard_roundtrip_and_dry_run_leaves_clipboard() {
    if !live() {
        return;
    }
    let mut s = Session::spawn();
    // Save the current clipboard text (or note it is empty/unreadable).
    let cur = s.do_first(
        1,
        serde_json::json!({"actions": [{"type": "clipboard_get"}]}),
    );
    let saved: Option<String> = if cur.get("ok") == Some(&Value::from(true)) {
        cur.get("text").and_then(|t| t.as_str()).map(str::to_string)
    } else {
        println!(
            "NOTE clipboard: initially empty/unreadable ({})",
            cur.get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("no error text")
        );
        None
    };
    // Unique marker so no other writer can fake the readback.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let marker = format!("puppet-p3a-{}-{nanos}", std::process::id());
    let set = s.do_first(
        2,
        serde_json::json!({"actions": [{"type": "clipboard_set", "text": marker}]}),
    );
    assert_eq!(
        set.get("ok"),
        Some(&Value::from(true)),
        "clipboard_set must succeed, got: {set}"
    );
    let get = s.do_first(
        3,
        serde_json::json!({"actions": [{"type": "clipboard_get"}]}),
    );
    assert_eq!(
        get.get("ok"),
        Some(&Value::from(true)),
        "clipboard_get must succeed, got: {get}"
    );
    assert_eq!(
        get.get("text").and_then(|t| t.as_str()),
        Some(marker.as_str()),
        "clipboard_get must return the marker, got: {get}"
    );
    // dry_run clipboard_set must report ok WITHOUT changing the clipboard.
    let dry = s.do_first(
        4,
        serde_json::json!({"actions": [{"type": "clipboard_set", "text": "SHOULD-NOT-LAND"}], "dry_run": true}),
    );
    assert_eq!(
        dry.get("ok"),
        Some(&Value::from(true)),
        "dry_run clipboard_set must report ok, got: {dry}"
    );
    let again = s.do_first(
        5,
        serde_json::json!({"actions": [{"type": "clipboard_get"}]}),
    );
    assert_eq!(
        again.get("text").and_then(|t| t.as_str()),
        Some(marker.as_str()),
        "dry_run clipboard_set must not change the clipboard, got: {again}"
    );
    // Over-cap set must be refused before touching the clipboard: the
    // marker above must still read back afterwards.
    let big = "x".repeat(1_048_576 + 1);
    let over = s.do_first(
        6,
        serde_json::json!({"actions": [{"type": "clipboard_set", "text": big}]}),
    );
    assert_eq!(
        over.get("ok"),
        Some(&Value::from(false)),
        "over-cap clipboard_set must fail, got: {over}"
    );
    assert!(
        over.get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("")
            .contains("1048576"),
        "over-cap failure must name the bound, got: {over}"
    );
    let still = s.do_first(
        7,
        serde_json::json!({"actions": [{"type": "clipboard_get"}]}),
    );
    assert_eq!(
        still.get("text").and_then(|t| t.as_str()),
        Some(marker.as_str()),
        "rejected set must leave the clipboard untouched, got: {still}"
    );
    // Restore what was there (empty string when there was nothing; the
    // clipboard has no "remove" action, so an empty write is the closest
    // restoration and is noted, never asserted).
    let restore_text = saved.clone().unwrap_or_default();
    let back = s.do_first(
        8,
        serde_json::json!({"actions": [{"type": "clipboard_set", "text": restore_text}]}),
    );
    assert_eq!(
        back.get("ok"),
        Some(&Value::from(true)),
        "clipboard restore must succeed, got: {back}"
    );
    println!("CLIPBOARD_OK marker round-tripped, dry_run clean, saved restored");
}

#[test]
fn clipboard_big_readback_truncates_to_cap() {
    // A foreign clipboard larger than the cap reads back truncated to
    // exactly MAX_CLIP_CHARS (seeded via PowerShell, cleared after).
    if !live() {
        return;
    }
    let seed = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Set-Clipboard -Value ('Z' * 1100000)",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !seed.map(|s| s.success()).unwrap_or(false) {
        println!("SKIP clipboard_big_readback: powershell seeding failed");
        return;
    }
    let mut s = Session::spawn();
    let get = s.do_first(
        1,
        serde_json::json!({"actions": [{"type": "clipboard_get"}]}),
    );
    let text_len = get
        .get("text")
        .and_then(|t| t.as_str())
        .map(|t| t.chars().count())
        .unwrap_or(0);
    // Clear before asserting so a failure never leaves a 1MB clipboard.
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-Command", "Set-Clipboard -Value ''"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    assert_eq!(
        get.get("ok"),
        Some(&Value::from(true)),
        "big clipboard read must succeed, got: {get}"
    );
    assert_eq!(
        text_len, 1_048_576,
        "big clipboard must truncate to exactly MAX_CLIP_CHARS, got {text_len}"
    );
    println!("CLIPBOARD_BIG_OK truncated read green");
}
