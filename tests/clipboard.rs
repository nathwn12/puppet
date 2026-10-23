//! P3a/P6 clipboard proof: `do` `clipboard_set`/`clipboard_get` round-trip
//! through CF_UNICODETEXT, `clipboard_set` is deny-by-default (it needs
//! `allow_clipboard:true`), and `dry_run` for `clipboard_set` reports
//! without touching the clipboard.
//!
//! The refusal proof runs everywhere (nothing is touched). The round-trip,
//! dry-run and live hash proofs need a real interactive session clipboard
//! and are gated by PUPPET_LIVE_DESKTOP=1; without it they print a skip line
//! and pass so CI stays green. The live hash proof keeps the owner's saved
//! text in an RAII guard, so even a panicking assertion puts it back, and it
//! prints its success marker only once a readback confirms the restore.

use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(30);

/// Live clipboard tests share ONE owner clipboard, so they must not run
/// concurrently: a parallel write would be indistinguishable from a gate
/// failure. Every test that reads or writes the clipboard holds this lock.
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

    /// One `tools/call` round-trip, fallible. Cleanup paths (a `Drop` that
    /// puts the owner's clipboard back) must be able to report a failure
    /// instead of panicking inside an unwind, where a second panic aborts.
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

    /// First per-action result of a `do` burst, fallible for the same reason.
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

/// The owner's clipboard is put back ONLY when the restoration is proven to
/// have landed: the permitted `clipboard_set` reported `ok:true` AND a
/// readback returned exactly the saved text. A reported-ok write that did
/// not land, a readback that cannot be read, or a readback with no `text`
/// field all fail — none of them may stand in for a confirmed restore. Pure,
/// so every one of those refusal paths is unit-tested without a live
/// clipboard.
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

/// Owns the session AND the owner's saved clipboard text for a live test that
/// seeds the clipboard. The guarded text goes back on EVERY exit path:
/// `Drop` is the finally for a panicking assertion, and `restore_confirmed`
/// is the happy path — it is the only route to a success marker, because it
/// fails the test unless `confirm_restored` proves the readback. Declare this
/// after `CLIP_LOCK`'s guard so the restore happens while the clipboard is
/// still ours alone.
struct ClipboardRestore {
    session: Session,
    saved: String,
    next_id: i64,
    /// True until a restore has been confirmed; `Drop` acts only while armed.
    armed: bool,
}

impl ClipboardRestore {
    fn new(session: Session, saved: String) -> Self {
        Self {
            session,
            saved,
            next_id: 0,
            armed: true,
        }
    }

    fn next_id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }

    /// One `do` round-trip on the owned session.
    fn call(&mut self, arguments: Value) -> Value {
        let id = self.next_id();
        self.session.do_first(id, arguments)
    }

    /// Best-effort restore that reports instead of panicking, so it is safe
    /// from `Drop`. Disarms only on a confirmation.
    fn restore(&mut self) -> Result<(), String> {
        if !self.armed {
            return Ok(());
        }
        let want = self.saved.clone();
        let set_id = self.next_id();
        let set = self.session.try_do_first(
            set_id,
            serde_json::json!({"actions": [{"type": "clipboard_set", "text": want}], "allow_clipboard": true}),
        )?;
        let read_id = self.next_id();
        let read = self.session.try_do_first(
            read_id,
            serde_json::json!({"actions": [{"type": "clipboard_get"}]}),
        )?;
        let outcome = confirm_restored(&self.saved, &set, &read);
        if outcome.is_ok() {
            self.armed = false;
        }
        outcome
    }

    /// Happy-path restore: an unconfirmed restore fails the test here, before
    /// any success marker can print.
    fn restore_confirmed(&mut self) {
        if let Err(e) = self.restore() {
            panic!("clipboard restoration failed, the owner's clipboard may still hold the test seed: {e}");
        }
    }
}

impl Drop for ClipboardRestore {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // This can run while a panic unwinds, so it must never panic itself.
        // It also must never claim success: a failure here fails the test that
        // is already failing, and is named loudly for a human to finish by
        // hand.
        match self.restore() {
            Ok(()) => eprintln!("clipboard restored during unwind"),
            Err(e) => eprintln!(
                "WARN clipboard restoration on drop failed ({e}); the owner's clipboard may hold the test seed"
            ),
        }
    }
}

#[test]
fn clipboard_set_without_flag_is_refused_and_touches_nothing() {
    // No live gate: the refusal happens at the gate, before any clipboard
    // API exists on the path, so this is safe on a headless runner and it
    // is the proof that a refused call cannot have touched the clipboard.
    let mut s = Session::spawn();
    let refused = s.do_first(
        1,
        serde_json::json!({"actions": [{"type": "clipboard_set", "text": "SHOULD-NOT-LAND"}]}),
    );
    assert_eq!(
        refused.get("ok"),
        Some(&Value::from(false)),
        "clipboard_set without allow_clipboard must fail, got: {refused}"
    );
    assert_eq!(
        refused.get("code").and_then(|c| c.as_str()),
        Some("clipboard_write_required"),
        "refusal must be typed clipboard_write_required, got: {refused}"
    );
    assert!(
        refused
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("")
            .contains("allow_clipboard:true"),
        "refusal must name the exact flag, got: {refused}"
    );
    // A refused write must not abort the burst: the next action still runs.
    let batch = s.result(
        2,
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
    // dry_run is a no-op, but it is gated identically (a dry run that says ok
    // while the live call would refuse is a lie) - and still touches nothing.
    let dry_unflagged = s.result(
        3,
        "do",
        serde_json::json!({"actions": [{"type": "clipboard_set", "text": "X"}], "dry_run": true}),
    );
    assert_eq!(
        dry_unflagged["structuredContent"]["results"][0]["code"].as_str(),
        Some("clipboard_write_required"),
        "dry_run must refuse identically, got: {dry_unflagged}"
    );
    println!("CLIPBOARD_GATE_OK clipboard_write_required before any clipboard access");
}

#[test]
fn clipboard_roundtrip_and_dry_run_leaves_clipboard() {
    if !live() {
        return;
    }
    let _guard = clip_guard();
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
        serde_json::json!({"actions": [{"type": "clipboard_set", "text": marker}], "allow_clipboard": true}),
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
        serde_json::json!({"actions": [{"type": "clipboard_set", "text": "SHOULD-NOT-LAND"}], "dry_run": true, "allow_clipboard": true}),
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
        serde_json::json!({"actions": [{"type": "clipboard_set", "text": big}], "allow_clipboard": true}),
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
        serde_json::json!({"actions": [{"type": "clipboard_set", "text": restore_text}], "allow_clipboard": true}),
    );
    assert_eq!(
        back.get("ok"),
        Some(&Value::from(true)),
        "clipboard restore must succeed, got: {back}"
    );
    println!("CLIPBOARD_OK marker round-tripped, dry_run clean, saved restored");
}

/// Live: the gate must fire BEFORE any clipboard access. Hash the clipboard
/// immediately before a REFUSED `clipboard_set` and immediately after; the
/// two hashes (and the bytes) must be identical, so the refusal cannot have
/// opened, emptied or written the clipboard on its way out.
#[test]
fn refused_clipboard_set_leaves_the_clipboard_byte_identical() {
    if !live() {
        return;
    }
    let _lock = clip_guard();
    let mut s = Session::spawn();
    // Preserve whatever the owner had BEFORE anything is written. Nothing has
    // been changed yet, so a failure to read it can still fail here safely.
    let before_seed = s.do_first(
        0,
        serde_json::json!({"actions": [{"type": "clipboard_get"}]}),
    );
    let saved = if before_seed.get("ok") == Some(&Value::from(true)) {
        before_seed
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string()
    } else {
        // No text to put back. An empty write is the closest restoration the
        // clipboard API offers (there is no "clear" action), and the readback
        // still has to confirm it landed.
        println!(
            "NOTE clipboard: initially unreadable ({}); restoring empty text",
            before_seed
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("no error text")
        );
        String::new()
    };
    // From here on the clipboard holds our seed: the guard owns the session so
    // that EVERY exit path — including a panicking assertion below — puts the
    // owner's text back before this test returns.
    let mut clip = ClipboardRestore::new(s, saved);
    // Seed a known value with a PERMITTED write, so the readback is not empty
    // (an empty clipboard would make "unchanged" too weak).
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!("puppet-clip-gate-{}-{nanos}", std::process::id());
    let seeded = clip.call(
        serde_json::json!({"actions": [{"type": "clipboard_set", "text": seed}], "allow_clipboard": true}),
    );
    assert_eq!(
        seeded.get("ok"),
        Some(&Value::from(true)),
        "seeding needs allow_clipboard:true, got: {seeded}"
    );
    // Immediately before: read and hash.
    let before = clip.call(serde_json::json!({"actions": [{"type": "clipboard_get"}]}));
    let before_text = before
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    assert_eq!(before_text, seed, "seed must read back, got: {before}");
    let before_hash = clipboard_hash(&before_text);
    // The refused write asks for a DIFFERENT value with no flag.
    let refused = clip.call(
        serde_json::json!({"actions": [{"type": "clipboard_set", "text": "PUPPET-REFUSED-WRITE"}]}),
    );
    assert_eq!(
        refused.get("code").and_then(|c| c.as_str()),
        Some("clipboard_write_required"),
        "unflagged write must be refused, got: {refused}"
    );
    // Immediately after: read and hash.
    let after = clip.call(serde_json::json!({"actions": [{"type": "clipboard_get"}]}));
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
    // Restore what the owner had and PROVE it landed. A failed restore panics
    // here, so the marker below is unreachable unless the readback matched.
    clip.restore_confirmed();
    println!(
        "CLIPBOARD_GATE_LIVE_OK hash={before_hash:#x} unchanged across a refused write, owner clipboard restored"
    );
}

/// The restore proof itself: nothing but a reported-ok write plus an exact
/// readback may count as a confirmed restoration. Touches no clipboard, so it
/// runs in the default (non-live) gate.
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
    // The write reported failure: nothing was restored.
    let failed_set =
        serde_json::json!({"type": "clipboard_set", "ok": false, "error": "OpenClipboard failed"});
    assert!(
        confirm_restored(saved, &failed_set, &ok_read).is_err(),
        "a restore whose clipboard_set did not report ok must not be confirmed"
    );
    // The write reported ok but the clipboard still holds the test seed.
    let stale_read =
        serde_json::json!({"type": "clipboard_get", "ok": true, "text": "puppet-clip-gate-1-2"});
    assert!(
        confirm_restored(saved, &ok_set, &stale_read).is_err(),
        "a readback that is not the saved text must not be confirmed"
    );
    // The readback could not read the clipboard at all.
    let unreadable =
        serde_json::json!({"type": "clipboard_get", "ok": false, "error": "OpenClipboard failed"});
    assert!(
        confirm_restored(saved, &ok_set, &unreadable).is_err(),
        "an unreadable clipboard must not be confirmed as restored"
    );
    // An ok readback with no text field proves nothing, not even emptiness.
    let no_text = serde_json::json!({"type": "clipboard_get", "ok": true});
    assert!(
        confirm_restored("", &ok_set, &no_text).is_err(),
        "a readback without a text field must not be confirmed"
    );
    // The empty-owner case is confirmed by an ok readback of "".
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
    // A foreign clipboard larger than the cap reads back truncated to
    // exactly MAX_CLIP_CHARS (seeded via PowerShell, cleared after).
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
