
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(20);
const BUDGET_MS: u64 = 2000;

struct Session {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
}

impl Session {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_puppet"))
            .env("PUPPET_TEST_BURST_BUDGET_MS", BUDGET_MS.to_string())
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

    fn write(&mut self, v: &Value) {
        let mut line = serde_json::to_string(v).unwrap();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).expect("write frame");
        self.stdin.flush().expect("flush frame");
    }

    fn call(&mut self, id: i64, args: Value) -> (Value, Duration) {
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": "do", "arguments": args},
        });
        let started = Instant::now();
        self.write(&frame);
        let raw = self
            .rx
            .recv_timeout(TIMEOUT)
            .unwrap_or_else(|e| panic!("no response to call {id} within {TIMEOUT:?}: {e}"));
        let resp: Value = serde_json::from_str(&raw).expect("response is JSON");
        assert_eq!(resp.get("id"), Some(&Value::from(id)), "wrong id: {resp}");
        let result = resp.get("result").cloned().unwrap_or(Value::Null);
        (result, started.elapsed())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn sc_of(r: &Value) -> Value {
    r.get("structuredContent")
        .cloned()
        .unwrap_or_else(|| panic!("must carry structuredContent, got: {r}"))
}

#[test]
fn settle_is_clamped_to_the_remaining_budget_and_frees_the_input_lock() {
    let mut s = Session::spawn();
    let (red, elapsed) = s.call(
        1,
        serde_json::json!({
            "actions": [{"type": "wait", "ms": 1500}],
            "settle_ms": 60000,
        }),
    );
    let sc = sc_of(&red);
    assert!(
        elapsed < Duration::from_secs(10),
        "a 60 s settle behind a {BUDGET_MS}ms budget must be clamped, took {elapsed:?}"
    );
    assert_eq!(
        sc.get("settle_requested_ms"),
        Some(&Value::from(60000i64)),
        "the request must be reported unchanged, got: {sc}"
    );
    let applied = sc
        .get("settle_ms")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| panic!("settle_ms must report what was applied, got: {sc}"));
    assert!(
        applied <= BUDGET_MS as i64,
        "the applied settle must not exceed the budget ({BUDGET_MS}ms), got {applied}ms"
    );
    assert!(
        applied < 60000,
        "the settle must be clamped, not honoured, got {applied}ms"
    );

    let (green, second) = s.call(
        2,
        serde_json::json!({"actions": [{"type": "wait", "ms": 0}]}),
    );
    assert!(
        second < Duration::from_secs(5),
        "the input lock must be free after a budget-clamped settle, took {second:?}"
    );
    let sc2 = sc_of(&green);
    assert_eq!(
        sc2.get("ok"),
        Some(&Value::from(true)),
        "the follow-up burst must run normally, got: {sc2}"
    );
}

#[test]
fn action_loop_still_refuses_past_the_budget_with_the_typed_code() {
    let mut s = Session::spawn();
    let (red, elapsed) = s.call(
        1,
        serde_json::json!({
            "actions": [
                {"type": "wait", "ms": 1500},
                {"type": "wait", "ms": 1500},
                {"type": "wait", "ms": 0},
            ],
        }),
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the burst must end at the budget, took {elapsed:?}"
    );
    let sc = sc_of(&red);
    let results = sc["results"].as_array().expect("results array");
    let stopped = results
        .iter()
        .find(|r| r.get("code") == Some(&Value::from("burst_budget_exceeded")))
        .unwrap_or_else(|| panic!("a budget-stopped action must carry the typed code: {sc}"));
    let err = stopped.get("error").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        err.contains("burst_budget_exceeded") && err.contains("not run")
            || err.contains("burst_budget_exceeded") && err.contains("exceeds the remaining"),
        "the refusal must name the limit and what is left unrun, got: {err}"
    );
    assert_eq!(
        sc.get("failed"),
        Some(&Value::from(1i64)),
        "exactly the budget-stopped action failed, got: {sc}"
    );
}
