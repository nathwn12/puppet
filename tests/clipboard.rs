use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(30);

static CLIP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn clip_guard() -> std::sync::MutexGuard<'static, ()> {
    CLIP_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn clipboard_hash(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.as_bytes().hash(&mut h);
    h.finish()
}

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

    fn try_result(&mut self, id: i64, name: &str, arguments: Value) -> Result<Value, String> {
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        });
        let line = serde_json::to_string(&frame).map_err(|e| format!("encode frame: {e}"))?;
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|e| format!("write frame: {e}"))?;
        self.stdin
            .write_all(b"\n")
            .map_err(|e| format!("write newline: {e}"))?;
        self.stdin
            .flush()
            .map_err(|e| format!("flush frame: {e}"))?;
        let line = self
            .rx
            .recv_timeout(TIMEOUT)
            .map_err(|e| format!("response in time for tools/call {name}: {e}"))?;
        let resp: Value =
            serde_json::from_str(&line).map_err(|e| format!("response is JSON: {e} in {line}"))?;
        resp.get("result")
            .cloned()
            .ok_or_else(|| format!("tools/call {name} must succeed, got: {resp}"))
    }

    fn result(&mut self, id: i64, name: &str, arguments: Value) -> Value {
        self.try_result(id, name, arguments)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    fn try_do_first(&mut self, id: i64, arguments: Value) -> Result<Value, String> {
        let r = self.try_result(id, "do", arguments)?;
        r.get("structuredContent")
            .and_then(|sc| sc.get("results"))
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .cloned()
            .ok_or_else(|| format!("do must return per-action results, got: {r}"))
    }

    fn do_first(&mut self, id: i64, arguments: Value) -> Value {
        self.try_do_first(id, arguments)
            .unwrap_or_else(|e| panic!("{e}"))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn confirm_restored(want: &str, set: &Value, read: &Value) -> Result<(), String> {
    if set.get("ok") != Some(&Value::from(true)) {
        return Err(format!("restore clipboard_set did not report ok: {set}"));
    }
    if read.get("ok") != Some(&Value::from(true)) {
        return Err(format!("restored clipboard could not be read back: {read}"));
    }
    let got = read
        .get("text")
        .and_then(|t| t.as_str())
        .ok_or_else(|| format!("restored clipboard readback has no text field: {read}"))?;
    if got != want {
        return Err(format!("restored clipboard holds {got:?}, want {want:?}"));
    }
    Ok(())
}

#[test]
fn clipboard_set_is_always_refused_and_touches_nothing() {
    let mut s = Session::spawn();
    let refused = s.do_first(
        1,
        serde_json::json!({"actions": [{"type": "clipboard_set", "text": "SHOULD-NOT-LAND"}]}),
    );
    assert_eq!(
        refused.get("ok"),
        Some(&Value::from(false)),
        "clipboard_set must fail, got: {refused}"
    );
    assert_eq!(
        refused.get("code").and_then(|c| c.as_str()),
        Some("no_clipboard_policy"),
        "refusal must be typed no_clipboard_policy, got: {refused}"
    );
    assert!(
        refused
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("")
            .contains("background-only policy"),
        "refusal must name the rule, got: {refused}"
    );
    let flagged = s.do_first(
        2,
        serde_json::json!({"actions": [{"type": "clipboard_set", "text": "SHOULD-NOT-LAND"}], "allow_clipboard": true}),
    );
    assert_eq!(
        flagged.get("code").and_then(|c| c.as_str()),
        Some("no_clipboard_policy"),
        "allow_clipboard:true must not buy a write, got: {flagged}"
    );
    let batch = s.result(
        3,
        "do",
        serde_json::json!({"actions": [
            {"type": "clipboard_set", "text": "SHOULD-NOT-LAND"},
            {"type": "wait", "ms": 1}
        ]}),
    );
    let results = batch["structuredContent"]["results"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(results.len(), 2, "both actions must report, got: {batch}");
    assert_eq!(
        results[1].get("ok"),
        Some(&Value::from(true)),
        "the action after a refused write must still run, got: {batch}"
    );
    let dry_unflagged = s.result(
        4,
        "do",
        serde_json::json!({"actions": [{"type": "clipboard_set", "text": "X"}], "dry_run": true}),
    );
    assert_eq!(
        dry_unflagged["structuredContent"]["results"][0]["code"].as_str(),
        Some("no_clipboard_policy"),
        "dry_run must refuse identically, got: {dry_unflagged}"
    );
    println!("CLIPBOARD_GATE_OK no_clipboard_policy before any clipboard access");
}

#[test]
fn clipboard_get_reads_and_set_is_always_refused_live() {
    if !live() {
        return;
    }
    let _guard = clip_guard();
    let mut s = Session::spawn();
    let before = s.do_first(
        1,
        serde_json::json!({"actions": [{"type": "clipboard_get"}]}),
    );
    let before_text = before
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    let before_hash = clipboard_hash(&before_text);
    for (id, args) in [
        (
            2,
            serde_json::json!({"actions": [{"type": "clipboard_set", "text": "SHOULD-NOT-LAND"}]}),
        ),
        (
            3,
            serde_json::json!({"actions": [{"type": "clipboard_set", "text": "SHOULD-NOT-LAND"}], "allow_clipboard": true}),
        ),
        (
            4,
            serde_json::json!({"actions": [{"type": "clipboard_set", "text": "x".repeat(1_048_576 + 1)}], "allow_clipboard": true}),
        ),
    ] {
        let refused = s.do_first(id, args);
        assert_eq!(
            refused.get("ok"),
            Some(&Value::from(false)),
            "clipboard_set must refuse, got: {refused}"
        );
        assert_eq!(
            refused.get("code").and_then(|c| c.as_str()),
            Some("no_clipboard_policy"),
            "refusal must be typed, got: {refused}"
        );
    }
    let after = s.do_first(
        5,
        serde_json::json!({"actions": [{"type": "clipboard_get"}]}),
    );
    let after_text = after
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        clipboard_hash(&after_text),
        before_hash,
        "refused writes must leave the clipboard byte-identical: before={before_text:?} after={after_text:?}"
    );
    assert_eq!(
        before_text.as_bytes(),
        after_text.as_bytes(),
        "clipboard bytes must be identical across refused writes"
    );
    println!("CLIPBOARD_OK get reads, every set refused, hash={before_hash:#x} unchanged");
}

#[test]
fn refused_clipboard_set_leaves_the_clipboard_byte_identical() {
    if !live() {
        return;
    }
    let _lock = clip_guard();
    let mut s = Session::spawn();
    let before = s.do_first(
        1,
        serde_json::json!({"actions": [{"type": "clipboard_get"}]}),
    );
    let before_text = before
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    let before_hash = clipboard_hash(&before_text);
    let refused = s.do_first(
        2,
        serde_json::json!({"actions": [{"type": "clipboard_set", "text": "PUPPET-REFUSED-WRITE"}]}),
    );
    assert_eq!(
        refused.get("code").and_then(|c| c.as_str()),
        Some("no_clipboard_policy"),
        "unflagged write must be refused, got: {refused}"
    );
    let after = s.do_first(
        3,
        serde_json::json!({"actions": [{"type": "clipboard_get"}]}),
    );
    let after_text = after
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    let after_hash = clipboard_hash(&after_text);
    assert_eq!(
        before_hash, after_hash,
        "a refused clipboard_set must not change the clipboard: before={before_text:?} after={after_text:?}"
    );
    assert_eq!(
        before_text.as_bytes(),
        after_text.as_bytes(),
        "clipboard bytes must be identical across a refused write"
    );
    println!("CLIPBOARD_GATE_LIVE_OK hash={before_hash:#x} unchanged across a refused write");
}

#[test]
fn restore_confirmation_rejects_every_unproven_restore() {
    let ok_set = serde_json::json!({"type": "clipboard_set", "ok": true});
    let saved = "owner text before the test";
    let ok_read = serde_json::json!({"type": "clipboard_get", "ok": true, "text": saved});
    assert_eq!(
        confirm_restored(saved, &ok_set, &ok_read),
        Ok(()),
        "an ok write plus an exact readback is the only confirmed restore"
    );
    let failed_set =
        serde_json::json!({"type": "clipboard_set", "ok": false, "error": "OpenClipboard failed"});
    assert!(
        confirm_restored(saved, &failed_set, &ok_read).is_err(),
        "a restore whose clipboard_set did not report ok must not be confirmed"
    );
    let stale_read =
        serde_json::json!({"type": "clipboard_get", "ok": true, "text": "puppet-clip-gate-1-2"});
    assert!(
        confirm_restored(saved, &ok_set, &stale_read).is_err(),
        "a readback that is not the saved text must not be confirmed"
    );
    let unreadable =
        serde_json::json!({"type": "clipboard_get", "ok": false, "error": "OpenClipboard failed"});
    assert!(
        confirm_restored(saved, &ok_set, &unreadable).is_err(),
        "an unreadable clipboard must not be confirmed as restored"
    );
    let no_text = serde_json::json!({"type": "clipboard_get", "ok": true});
    assert!(
        confirm_restored("", &ok_set, &no_text).is_err(),
        "a readback without a text field must not be confirmed"
    );
    let empty_read = serde_json::json!({"type": "clipboard_get", "ok": true, "text": ""});
    assert_eq!(
        confirm_restored("", &ok_set, &empty_read),
        Ok(()),
        "restoring an empty owner clipboard is confirmed by an empty readback"
    );
    println!("CLIPBOARD_RESTORE_PROOF_OK restore confirmed only by ok write + exact readback");
}

#[test]
fn clipboard_big_readback_truncates_to_cap() {
    if !live() {
        return;
    }
    let _guard = clip_guard();
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
