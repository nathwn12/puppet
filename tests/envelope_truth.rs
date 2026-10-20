//! P2a envelope truth: every tool result is machine-readable — a real
//! `structuredContent` object (never a JSON string), a matching per-tool
//! `outputSchema`, and correct `isError` on tool failures.
//!
//! These tests fail on the pre-P2a tree (double-encoded
//! `structuredContent.result` string; screenshot domain errors with
//! `isError: false`) and pass after. Safe for the owner's live desktop:
//! only read-only tools and error paths (unknown window/action) run; the
//! `do` case is `dry_run` with an unknown action type, so nothing injects.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(15);

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

    fn call(&mut self, id: i64, name: &str, arguments: Value) -> Value {
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
        self.rx
            .recv_timeout(TIMEOUT)
            .ok()
            .map(|l| serde_json::from_str(&l).expect("response is JSON"))
            .expect("response in time")
    }

    fn result(&mut self, id: i64, name: &str, arguments: Value) -> Value {
        let resp = self.call(id, name, arguments);
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

fn list_tools() -> Vec<Value> {
    let mut s = Session::spawn();
    let frame = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {},
    });
    let line = serde_json::to_string(&frame).unwrap();
    s.stdin.write_all(line.as_bytes()).expect("write");
    s.stdin.write_all(b"\n").expect("newline");
    s.stdin.flush().expect("flush");
    let resp: Value =
        s.rx.recv_timeout(TIMEOUT)
            .ok()
            .map(|l| serde_json::from_str(&l).expect("response is JSON"))
            .expect("tools/list in time");
    resp.get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_else(|| panic!("tools/list must carry tools, got: {resp}"))
}

#[test]
fn tools_list_every_tool_has_real_output_schema() {
    let tools = list_tools();
    assert_eq!(tools.len(), 6, "six tools expected, got: {tools:?}");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    for want in [
        "screenshot",
        "list_windows",
        "window",
        "do",
        "cursor",
        "elements",
    ] {
        assert!(
            names.contains(&want),
            "tools/list must contain {want}, got: {names:?}"
        );
    }
    for t in &tools {
        let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let schema = t
            .get("outputSchema")
            .and_then(|s| s.as_object())
            .unwrap_or_else(|| panic!("{name} must carry an outputSchema object, got: {t}"));
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap_or_else(|| panic!("{name} outputSchema needs properties, got: {schema:?}"));
        assert!(
            !props.is_empty(),
            "{name} outputSchema.properties must be non-empty"
        );
        assert!(
            !serde_json::to_string(t)
                .unwrap()
                .contains("x-fastmcp-wrap-result"),
            "{name} must not carry x-fastmcp-wrap-result"
        );
    }
}

#[test]
fn list_windows_structured_is_real_object() {
    let mut s = Session::spawn();
    let r = s.result(2, "list_windows", serde_json::json!({}));
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(false)),
        "list_windows must succeed, got: {r}"
    );
    let sc = r
        .get("structuredContent")
        .expect("structuredContent present");
    assert!(
        sc.is_object(),
        "structuredContent must be an object, got: {sc}"
    );
    assert!(
        sc.get("result").is_none(),
        "double-encoded structuredContent.result must be gone, got: {sc}"
    );
    let wins = sc
        .get("windows")
        .and_then(|w| w.as_array())
        .expect("windows array");
    // Text block stays byte-identical: the same array, JSON-encoded.
    let text = r
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        .expect("text block");
    let arr: Value = serde_json::from_str(text).expect("text is a JSON array");
    assert_eq!(
        arr.as_array().map(|a| a.len()),
        Some(wins.len()),
        "text and structuredContent.windows must agree, got: {r}"
    );
}

#[test]
fn cursor_structured_has_xyz_screens() {
    let mut s = Session::spawn();
    let r = s.result(3, "cursor", serde_json::json!({}));
    assert_eq!(r.get("isError"), Some(&Value::from(false)));
    let sc = r.get("structuredContent").expect("structuredContent");
    assert!(sc.is_object(), "cursor structuredContent must be an object");
    assert!(
        sc.get("x").and_then(|v| v.as_i64()).is_some(),
        "cursor needs x, got: {sc}"
    );
    assert!(
        sc.get("y").and_then(|v| v.as_i64()).is_some(),
        "cursor needs y, got: {sc}"
    );
    assert!(
        sc.get("screens").and_then(|v| v.as_array()).is_some(),
        "cursor needs screens, got: {sc}"
    );
}

#[test]
fn screenshot_unknown_window_is_error_without_image() {
    let mut s = Session::spawn();
    let r = s.result(
        4,
        "screenshot",
        serde_json::json!({"window": "NoSuchWindowXYZ"}),
    );
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(true)),
        "unknown-window screenshot must be isError:true, got: {r}"
    );
    let content = r
        .get("content")
        .and_then(|c| c.as_array())
        .expect("content blocks");
    assert!(
        content
            .iter()
            .all(|b| b.get("type").and_then(|t| t.as_str()) != Some("image")),
        "error must carry no image block, got: {r}"
    );
    let text: String = content
        .iter()
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect();
    assert!(
        text.contains("error:"),
        "error text block unchanged, got: {text}"
    );
}

#[test]
fn window_missing_target_is_error() {
    let mut s = Session::spawn();
    let r = s.result(
        5,
        "window",
        serde_json::json!({"action": "focus", "title": "NoSuchWindowXYZ12345"}),
    );
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(true)),
        "missing-target window call must be isError:true, got: {r}"
    );
    let sc = r.get("structuredContent").expect("structuredContent");
    assert_eq!(
        sc.get("ok"),
        Some(&Value::from(false)),
        "structuredContent carries ok:false, got: {sc}"
    );
}

#[test]
fn window_unknown_action_is_error() {
    // An unknown action needs a resolvable target to reach the
    // unknown-action branch: reuse a live hwnd when one is listed,
    // else fall back to the (still isError:true) resolve path.
    let mut s = Session::spawn();
    let lw = s.result(6, "list_windows", serde_json::json!({}));
    let hwnd = lw
        .get("structuredContent")
        .and_then(|sc| sc.get("windows"))
        .and_then(|w| w.as_array())
        .and_then(|a| a.first())
        .and_then(|w| w.get("hwnd"))
        .and_then(|h| h.as_i64());
    let args = match hwnd {
        Some(h) => serde_json::json!({"action": "frobnicate", "hwnd": h}),
        None => serde_json::json!({"action": "frobnicate", "title": "NoSuchWindowXYZ12345"}),
    };
    let r = s.result(7, "window", args);
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(true)),
        "unknown-action window call must be isError:true, got: {r}"
    );
    if hwnd.is_some() {
        let text: String = r
            .get("content")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            text.contains("unknown action"),
            "unknown action named in text, got: {text}"
        );
    }
}

#[test]
fn do_bad_action_stays_ok_with_per_action_error() {
    let mut s = Session::spawn();
    let r = s.result(
        8,
        "do",
        serde_json::json!({"actions": [{"type": "nope_not_a_type"}], "dry_run": true}),
    );
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(false)),
        "do continue-always stays isError:false, got: {r}"
    );
    let sc = r.get("structuredContent").expect("structuredContent");
    let first = sc
        .get("results")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .expect("first per-action result");
    assert_eq!(first.get("ok"), Some(&Value::from(false)));
    let err = first.get("error").and_then(|e| e.as_str()).unwrap_or("");
    assert!(
        err.contains("unknown action type"),
        "per-action error typed, got: {err}"
    );
}
