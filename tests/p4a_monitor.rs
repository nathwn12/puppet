//! P4a monitor-match live probe: monitor-scoped screenshots must serve the
//! requested display (DXGI output matched by rect/handle/device/size, never
//! the display-index assumption), falling back to GDI with a note when no
//! output matches. Read-only; no owner data.
//!
//! Gated by PUPPET_LIVE_DESKTOP=1. Single-monitor boxes exercise display 0
//! (regression: the old code assumed output 0); multi-monitor boxes also
//! exercise display 1. Fewer than 2 displays skips with a line.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(60);

fn live() -> bool {
    if std::env::var("PUPPET_LIVE_DESKTOP").as_deref() == Ok("1") {
        true
    } else {
        println!("SKIP p4a_monitor: set PUPPET_LIVE_DESKTOP=1 to run (needs an interactive Windows session)");
        false
    }
}

struct Session {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
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

fn shot_of(r: &Value, what: &str) -> Value {
    r.get("structuredContent")
        .cloned()
        .unwrap_or_else(|| panic!("{what} must carry structuredContent, got: {r}"))
}

fn check_display(s: &mut Session, id: i64, display: i64) -> Option<Value> {
    let r = s.result(id, "screenshot", serde_json::json!({"display": display}));
    if serde_json::to_string(&r)
        .unwrap_or_default()
        .contains("bad_display")
    {
        return None;
    }
    let sc = shot_of(&r, "screenshot");
    assert_ne!(
        r.get("isError"),
        Some(&Value::from(true)),
        "display {display} screenshot must not be isError, got: {r}"
    );
    let w = sc
        .get("image")
        .and_then(|i| i.get("w"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    assert!(w > 0, "display {display} must return image bytes, got: {r}");
    let backend = sc
        .get("capture")
        .and_then(|c| c.get("backend"))
        .and_then(|b| b.as_str())
        .unwrap_or("?");
    assert!(
        backend == "dxgi" || backend == "gdi",
        "backend must be dxgi or gdi, got: {backend}"
    );
    Some(sc)
}

#[test]
fn monitor_scope_serves_requested_display() {
    if !live() {
        return;
    }
    let mut s = Session::spawn();
    let sc0 = check_display(&mut s, 1, 0).expect("display 0 must exist");
    let b0 = sc0
        .get("capture")
        .and_then(|c| c.get("backend"))
        .and_then(|b| b.as_str())
        .unwrap_or("?");
    // Driver-type probe: with the explicit adapter created UNKNOWN the
    // session opens and display 0 is served by DXGI, not GDI.
    assert_eq!(
        b0, "dxgi",
        "display 0 must be dxgi after the adapter fix, got backend={b0}, shot={sc0}"
    );
    match check_display(&mut s, 2, 1) {
        None => println!("SKIP p4a_monitor: <2 displays (display 0 backend={b0})"),
        Some(sc1) => {
            let b1 = sc1
                .get("capture")
                .and_then(|c| c.get("backend"))
                .and_then(|b| b.as_str())
                .unwrap_or("?");
            println!("MONITOR_OK display0 backend={b0} display1 backend={b1}");
        }
    }
}
