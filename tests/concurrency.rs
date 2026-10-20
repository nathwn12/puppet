//! P1b concurrency: the stdio server stays non-blocking while a long tool
//! call runs. A `do` with a 5s `wait` must not block `ping`; a
//! `notifications/cancelled` must drop the call with no response; two `do`
//! bursts must serialize on the input lock; EOF with a call in flight must
//! exit promptly.
//!
//! These tests fail on the pre-P1b tree (ping blocked, cancel ignored) and
//! pass after.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::Value;

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
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
        Session {
            child,
            stdin: Some(stdin),
            rx,
        }
    }

    fn write_line(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(line.as_bytes()).expect("write frame");
        stdin.write_all(b"\n").expect("write newline");
        stdin.flush().expect("flush frame");
    }

    fn send(&mut self, frame: &Value) {
        self.write_line(&serde_json::to_string(frame).unwrap());
    }

    /// Drain every response that arrives within `wait`, returning them all.
    fn drain(&self, wait: Duration) -> Vec<Value> {
        let deadline = Instant::now() + wait;
        let mut out = Vec::new();
        loop {
            let left = match deadline.checked_duration_since(Instant::now()) {
                Some(d) if !d.is_zero() => d,
                _ => break,
            };
            match self.rx.recv_timeout(left) {
                Ok(line) => out.push(serde_json::from_str(&line).expect("response is JSON")),
                Err(_) => break,
            }
        }
        out
    }

    fn by_id(resps: &[Value], id: i64) -> Option<&Value> {
        resps
            .iter()
            .find(|v| v.get("id").and_then(|x| x.as_i64()) == Some(id))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn do_wait_frame(id: i64, ms: i64) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "do",
            "arguments": {"actions": [{"type": "wait", "ms": ms}]},
        },
    })
}

fn ping_frame(id: i64) -> Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "ping"})
}

fn do_wait_for_frame(id: i64, timeout_ms: i64) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "do",
            "arguments": {"actions": [{"type": "wait_for", "kind": "window_appear", "selector": "no-such-window-p1b", "timeout_ms": timeout_ms}]},
        },
    })
}

fn cancel_note(request_id: i64) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {"requestId": request_id},
    })
}

#[test]
fn ping_during_long_call() {
    let mut s = Session::spawn();
    // Long `do` (real 5s wait, no desktop touch) occupies one worker.
    s.send(&do_wait_frame(1, 5000));
    std::thread::sleep(Duration::from_millis(300));
    // Ping must answer in <1s while the do call is still in flight.
    let t0 = Instant::now();
    s.send(&ping_frame(2));
    let mut ping_resp: Option<Value> = None;
    let mut saw_do = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    while ping_resp.is_none() && Instant::now() < deadline {
        for r in s.drain(Duration::from_millis(200)) {
            match r.get("id").and_then(|x| x.as_i64()) {
                Some(2) => ping_resp = Some(r),
                Some(1) => saw_do = true,
                _ => {}
            }
        }
    }
    let ping = ping_resp.expect("ping must respond during long call");
    let lat = Instant::now().duration_since(t0);
    // (Arrival was just observed; the 200ms drain quantum bounds the error.)
    assert!(
        lat < Duration::from_millis(1200),
        "ping must return in <1s during a long call, took {lat:?}"
    );
    assert_eq!(
        ping.get("result"),
        Some(&serde_json::json!({})),
        "ping result must be empty object, got: {ping}"
    );
    assert!(!saw_do, "do call must still be in flight when ping answers");
    // Let the do call finish (or cancel it); it must eventually respond.
    let rest = s.drain(Duration::from_secs(10));
    assert!(
        Session::by_id(&rest, 1).is_some(),
        "long do call must eventually respond, got: {rest:?}"
    );
}

#[test]
fn cancelled_call_sends_no_response() {
    let mut s = Session::spawn();
    s.send(&do_wait_frame(11, 5000));
    std::thread::sleep(Duration::from_millis(300));
    s.send(&cancel_note(11));
    // No response for the cancelled id within 2s.
    let resps = s.drain(Duration::from_secs(2));
    assert!(
        Session::by_id(&resps, 11).is_none(),
        "cancelled call must send no response, got: {resps:?}"
    );
    // The server is still alive: ping responds.
    s.send(&ping_frame(12));
    let more = s.drain(Duration::from_secs(5));
    assert!(
        Session::by_id(&more, 12).is_some(),
        "ping must respond after a cancellation, got: {more:?}"
    );
    // No late do response after another 1s.
    let late = s.drain(Duration::from_secs(1));
    assert!(
        Session::by_id(&late, 11).is_none() && Session::by_id(&more, 11).is_none(),
        "no late response for a cancelled call, got late: {late:?}"
    );
}

#[test]
fn pre_start_cancel_drops_call() {
    let mut s = Session::spawn();
    s.send(&do_wait_frame(21, 3000));
    // Cancel immediately, before the worker plausibly starts.
    s.send(&cancel_note(21));
    let resps = s.drain(Duration::from_secs(4));
    assert!(
        Session::by_id(&resps, 21).is_none(),
        "pre-start cancel must drop the call, got: {resps:?}"
    );
    s.send(&ping_frame(22));
    let more = s.drain(Duration::from_secs(5));
    assert!(
        Session::by_id(&more, 22).is_some(),
        "ping must respond, got: {more:?}"
    );
}

#[test]
fn cancelled_waiter_never_executes() {
    // Deterministic waiter cancel: A holds the input lock with a long wait;
    // B is dispatched while A holds it, so B blocks in the cancellable lock
    // wait. Cancelling B there must drop it without executing, while A
    // still completes. (A racy pre-start cancel is covered separately.)
    let mut s = Session::spawn();
    s.send(&do_wait_frame(51, 4000));
    std::thread::sleep(Duration::from_millis(500));
    s.send(&do_wait_frame(52, 100));
    std::thread::sleep(Duration::from_millis(300));
    s.send(&cancel_note(52));
    let mut got51 = false;
    let mut got52 = false;
    let deadline = Instant::now() + Duration::from_secs(12);
    while !got51 && Instant::now() < deadline {
        for r in s.drain(Duration::from_millis(200)) {
            match r.get("id").and_then(|x| x.as_i64()) {
                Some(51) => got51 = true,
                Some(52) => got52 = true,
                _ => {}
            }
        }
    }
    assert!(got51, "un-cancelled A must still complete");
    assert!(!got52, "cancelled waiter B must never respond");
    // No late B response after another 1s.
    let late = s.drain(Duration::from_secs(1));
    assert!(
        Session::by_id(&late, 52).is_none(),
        "no late response for cancelled waiter, got: {late:?}"
    );
}

#[test]
fn two_do_bursts_serialize() {
    let mut s = Session::spawn();
    let t0 = Instant::now();
    // Two long waits; both hold the input lock, so they must serialize.
    s.send(&do_wait_frame(31, 1500));
    s.send(&do_wait_frame(32, 1500));
    let mut got31 = false;
    let mut got32 = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    while !(got31 && got32) && Instant::now() < deadline {
        for r in s.drain(Duration::from_millis(200)) {
            if r.get("id").and_then(|x| x.as_i64()) == Some(31) {
                got31 = true;
            }
            if r.get("id").and_then(|x| x.as_i64()) == Some(32) {
                got32 = true;
            }
        }
    }
    assert!(got31 && got32, "both do calls must respond");
    // Serialized: ~3s total. Concurrent would be ~1.5s; allow wide margin.
    assert!(
        t0.elapsed() >= Duration::from_millis(2500),
        "two 1500ms do calls must serialize (>=2500ms), took {:?}",
        t0.elapsed()
    );
}

#[test]
fn eof_with_call_in_flight_exits_promptly() {
    let mut s = Session::spawn();
    // A 300s `wait_for` stays in flight far past the 2s EOF drain: the pump
    // must still exit on stdin close, not hang on the wedged worker.
    s.send(&do_wait_for_frame(41, 300000));
    std::thread::sleep(Duration::from_millis(500));
    s.stdin.take();
    let t0 = Instant::now();
    let status = loop {
        match s.child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if t0.elapsed() > Duration::from_secs(3) {
                    panic!("process must exit within 3s of EOF with a call in flight");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    };
    assert!(
        status.success(),
        "EOF with a call in flight must exit code 0, got: {status}"
    );
    let elapsed = t0.elapsed();
    assert!(
        elapsed <= Duration::from_millis(2500),
        "EOF exit must respect the 2s drain bound, took {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_secs(1),
        "exit in {elapsed:?} is too fast: the 300s call was never in flight"
    );
}
