//! P4b additive input: relative mouse, key hold/repeat, UIA Scroll.
//!
//! The schema assertions run everywhere (no desktop touch). The live test
//! is gated by PUPPET_LIVE_DESKTOP=1 and drives only the dedicated fixture
//! app (`src/bin/puppet_fixture_app` in `p4c` mode): no owner input — the
//! element scroll acts on the fixture's own multiline edit through UIA.
//! Wheel fallback, relative moves, and held keys are proven by dry-run
//! unit tests (no desktop touch by design).
//!
//! These tests fail on the pre-P4b tree (no dx/dy, no hold/repeat, scroll
//! ignores element) and pass after.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(30);

fn live() -> bool {
    if std::env::var("PUPPET_LIVE_DESKTOP").as_deref() == Ok("1") {
        true
    } else {
        println!("SKIP p4b_input: set PUPPET_LIVE_DESKTOP=1 to run (needs an interactive Windows session)");
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
        self.send(&frame);
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

    fn send(&mut self, frame: &Value) {
        let line = serde_json::to_string(frame).unwrap();
        self.stdin.write_all(line.as_bytes()).expect("write frame");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush frame");
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn list_tools(s: &mut Session) -> Vec<Value> {
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

fn tool<'a>(tools: &'a [Value], name: &str) -> &'a serde_json::Map<String, Value> {
    tools
        .iter()
        .find(|t| t.get("name").and_then(|n| n.as_str()) == Some(name))
        .and_then(|t| t.as_object())
        .unwrap_or_else(|| panic!("tools/list must contain {name}"))
}

fn kill_stale_fixtures(title: &str) {
    let _ = Command::new("taskkill")
        .args([
            "/F",
            "/FI",
            "IMAGENAME eq puppet_fixture_app.exe",
            "/FI",
            &format!("WINDOWTITLE eq {title}"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

struct Fixture {
    child: Child,
    title: String,
}

impl Fixture {
    fn spawn(title: &str, mode: &str) -> Self {
        let exe = env!("CARGO_BIN_EXE_puppet_fixture_app");
        let mut cmd = Command::new(exe);
        cmd.arg(title);
        cmd.arg(mode);
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fixture app");
        Fixture {
            child,
            title: title.to_string(),
        }
    }

    fn kill_and_reap(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

fn sc_of(r: &Value, what: &str) -> Value {
    r.get("structuredContent")
        .cloned()
        .unwrap_or_else(|| panic!("{what} must carry structuredContent, got: {r}"))
}

fn do_one(s: &mut Session, id: i64, action: Value, bg: &str) -> Value {
    let r = s.result(
        id,
        "do",
        serde_json::json!({"actions": [action], "background": bg}),
    );
    let sc = sc_of(&r, "do");
    sc.get("results")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or_else(|| panic!("do must return one result, got: {sc}"))
}

#[test]
fn schema_declares_p4b_fields() {
    let mut s = Session::spawn();
    let tools = list_tools(&mut s);
    let t = tool(&tools, "do");
    let schema = t
        .get("inputSchema")
        .and_then(|v| v.as_object())
        .expect("do inputSchema");
    let items = &schema["properties"]["actions"]["items"];
    let variants = items
        .get("oneOf")
        .and_then(|v| v.as_array())
        .expect("do.actions.items.oneOf");
    let variant = |typ: &str| {
        variants
            .iter()
            .find(|v| {
                v.get("properties")
                    .and_then(|p| p.get("type"))
                    .and_then(|t| t.get("const"))
                    .and_then(|c| c.as_str())
                    == Some(typ)
            })
            .unwrap_or_else(|| panic!("{typ} variant present"))
    };
    // move: absolute or relative, never both, never neither.
    let mv = variant("move");
    let props = mv
        .get("properties")
        .and_then(|v| v.as_object())
        .expect("move properties");
    for k in ["dx", "dy"] {
        assert!(props.contains_key(k), "move must declare {k}");
    }
    // key: hold/repeat/gap.
    let key = variant("key");
    let props = key
        .get("properties")
        .and_then(|v| v.as_object())
        .expect("key properties");
    for k in ["hold_ms", "repeat", "gap_ms"] {
        assert!(props.contains_key(k), "key must declare {k}");
    }
    // scroll: element target.
    let sc = variant("scroll");
    let props = sc
        .get("properties")
        .and_then(|v| v.as_object())
        .expect("scroll properties");
    assert!(props.contains_key("element"), "scroll must declare element");
    // No new action type was added.
    assert_eq!(
        variants.len(),
        16,
        "do.actions.items.oneOf must still list exactly 16 types"
    );
}

#[test]
fn scroll_element_drives_uia_pattern_live() {
    // Element scroll on the fixture's own scrollable multiline edit: UIA
    // ScrollPattern, no mouse, no owner input.
    if !live() {
        return;
    }
    kill_stale_fixtures("p4b-scroll-live");
    let fx = Fixture::spawn("p4b-scroll-live", "p4c");
    let title = fx.title.clone();
    std::thread::sleep(Duration::from_millis(500));
    let mut s = Session::spawn();
    let bg = format!("title:{title}");
    let first = do_one(
        &mut s,
        10,
        serde_json::json!({"type": "scroll", "element": "auto_id:3002", "axis": "v", "amount": 3}),
        &bg,
    );
    assert_eq!(
        first.get("ok"),
        Some(&Value::from(true)),
        "element scroll must run, got: {first}"
    );
    assert_eq!(
        first.get("via").and_then(|v| v.as_str()),
        Some("uia-scroll"),
        "element scroll must use ScrollPattern, got: {first}"
    );
    drop(fx);
    println!("SCROLL_OK element scroll via UIA pattern green");
}

#[test]
fn scroll_element_rejects_xy_live() {
    // Element scroll is UIA-only live too: x/y alongside element is a
    // typed error, never a queued mouse move.
    if !live() {
        return;
    }
    kill_stale_fixtures("p4b-scroll-xy-live");
    let fx = Fixture::spawn("p4b-scroll-xy-live", "p4c");
    let title = fx.title.clone();
    std::thread::sleep(Duration::from_millis(500));
    let mut s = Session::spawn();
    let bg = format!("title:{title}");
    let first = do_one(
        &mut s,
        11,
        serde_json::json!({"type": "scroll", "element": "auto_id:3002", "x": 10, "y": 10}),
        &bg,
    );
    assert_eq!(
        first.get("ok"),
        Some(&Value::from(false)),
        "element scroll with x/y must fail, got: {first}"
    );
    assert!(
        first
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("")
            .contains("never moves the mouse"),
        "x/y with element must be refused as mouse input, got: {first}"
    );
    drop(fx);
    println!("SCROLL_XY_OK element scroll rejects x/y live green");
}
