//! P3b elements continuation + element wait: paging (`max_depth`/`offset`/
//! `next_offset`) and `wait_for` kind `element`.
//!
//! The schema assertions run everywhere (no desktop touch). The live tests
//! are gated by PUPPET_LIVE_DESKTOP=1 and drive only the dedicated fixture
//! app (`src/bin/puppet_fixture_app`): a window with one EDIT plus, in
//! paging mode, 250 named buttons. No Notepad, no owner data, no input —
//! `elements` and `wait_for` are read-only paths.
//!
//! These tests fail on the pre-P3b tree (no max_depth/offset, no element
//! kind) and pass after.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(30);

fn live() -> bool {
    if std::env::var("PUPPET_LIVE_DESKTOP").as_deref() == Ok("1") {
        true
    } else {
        println!("SKIP p3b_elements: set PUPPET_LIVE_DESKTOP=1 to run (needs an interactive Windows session)");
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

#[test]
fn elements_schema_declares_paging() {
    let mut s = Session::spawn();
    let tools = list_tools(&mut s);
    let t = tool(&tools, "elements");
    let schema = t
        .get("inputSchema")
        .and_then(|v| v.as_object())
        .expect("elements inputSchema");
    let props = schema
        .get("properties")
        .and_then(|v| v.as_object())
        .expect("properties");
    for (k, want) in [("max_depth", (1i64, 32i64)), ("offset", (0i64, i64::MAX))] {
        let p = props
            .get(k)
            .and_then(|v| v.as_object())
            .unwrap_or_else(|| panic!("elements schema must declare {k}"));
        assert_eq!(
            p.get("type").and_then(|v| v.as_str()),
            Some("integer"),
            "{k} must be an integer"
        );
        assert_eq!(
            p.get("minimum").and_then(|v| v.as_i64()),
            Some(want.0),
            "{k} minimum"
        );
        if want.1 != i64::MAX {
            assert_eq!(
                p.get("maximum").and_then(|v| v.as_i64()),
                Some(want.1),
                "{k} maximum"
            );
        }
        assert!(p.get("default").is_some(), "{k} must declare a default");
    }
}

#[test]
fn wait_for_schema_declares_element_kind() {
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
    let wf = variants
        .iter()
        .find(|v| {
            v.get("properties")
                .and_then(|p| p.get("type"))
                .and_then(|t| t.get("const"))
                .and_then(|c| c.as_str())
                == Some("wait_for")
        })
        .expect("wait_for variant present");
    let props = wf
        .get("properties")
        .and_then(|v| v.as_object())
        .expect("wait_for properties");
    let kinds: Vec<&str> = props
        .get("kind")
        .and_then(|k| k.get("enum"))
        .and_then(|e| e.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert!(
        kinds.contains(&"element"),
        "wait_for kind enum must include element, got: {kinds:?}"
    );
    for k in ["window", "element", "state"] {
        assert!(props.contains_key(k), "wait_for variant must declare {k}");
    }
}

#[test]
fn elements_args_rejected_before_any_walk() {
    // Type/shape validation happens in serve with no desktop touch.
    let mut s = Session::spawn();
    for (id, args, want) in [
        (
            10,
            serde_json::json!({"title": "NoSuchWindowXYZ", "max_depth": 99}),
            "1..32",
        ),
        (
            11,
            serde_json::json!({"title": "NoSuchWindowXYZ", "max_depth": 0}),
            "1..32",
        ),
        (
            12,
            serde_json::json!({"title": "NoSuchWindowXYZ", "offset": -1}),
            ">= 0",
        ),
        (
            13,
            serde_json::json!({"title": "NoSuchWindowXYZ", "offset": "nope"}),
            "valid integer",
        ),
    ] {
        let r = s.result(id, "elements", args);
        assert_eq!(
            r.get("isError"),
            Some(&Value::from(true)),
            "bad paging args must fail, got: {r}"
        );
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
            text.contains(want),
            "paging validation must name the bound ({want}), got: {text}"
        );
    }
}

/// Kill leftover fixture processes from a previous (possibly panicked) run.
/// Scoped strictly to our own fixture executable plus the exact test window
/// title — never touches owner apps.
fn kill_stale_fixtures(title: &str) {
    // Exact title, no wildcard: combined with the IMAGENAME filter this can
    // only hit our fixture binary showing exactly this test window.
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

/// Owning handle for one fixture process. `Drop` kills and reaps, so a
/// panic anywhere in the test still cleans up (no leaked windows to
/// contaminate reruns).
struct Fixture {
    child: Child,
    title: String,
}

impl Fixture {
    /// Spawn the fixture app (`argv[1]` = window title, `argv[2]` = extra
    /// button count or absent).
    fn spawn(title: &str, extra: Option<u32>) -> Self {
        let exe = env!("CARGO_BIN_EXE_puppet_fixture_app");
        let mut cmd = Command::new(exe);
        cmd.arg(title);
        if let Some(n) = extra {
            cmd.arg(n.to_string());
        }
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

#[test]
fn elements_paging_covers_different_controls() {
    if !live() {
        return;
    }
    kill_stale_fixtures("p3b-paging-live");
    let fx = Fixture::spawn("p3b-paging-live", Some(250));
    let title = fx.title.clone();
    // Give the process a beat to create its window before the first walk.
    std::thread::sleep(Duration::from_millis(500));
    let mut s = Session::spawn();
    // Page 1: the first 200 accepted entries.
    let r1 = s.result(
        20,
        "elements",
        serde_json::json!({"title": title, "max_depth": 8, "offset": 0}),
    );
    let sc1 = sc_of(&r1, "page 1");
    assert_eq!(
        sc1.get("isError").unwrap_or(&Value::from(false)),
        &Value::from(false),
        "page 1 must succeed, got: {r1}"
    );
    let e1 = sc1
        .get("elements")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(e1.len(), 200, "page 1 must fill the 200-entry cap");
    assert_eq!(
        sc1.get("next_offset"),
        Some(&Value::from(200)),
        "a full page with more remaining must point at offset 200, got: {sc1}"
    );
    assert_eq!(
        sc1.get("depth_limit"),
        Some(&Value::from(8)),
        "depth_limit must echo the applied max_depth"
    );
    // Page 2: the remainder, disjoint from page 1.
    let r2 = s.result(
        21,
        "elements",
        serde_json::json!({"title": title, "max_depth": 8, "offset": 200}),
    );
    let sc2 = sc_of(&r2, "page 2");
    let e2 = sc2
        .get("elements")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        !e2.is_empty(),
        "page 2 must hold the tail past offset 200, got: {r2}"
    );
    assert_eq!(
        sc2.get("next_offset"),
        Some(&Value::Null),
        "tail page must not offer a next page, got: {sc2}"
    );
    let fp1: std::collections::BTreeSet<String> = e1
        .iter()
        .filter_map(|e| e.get("fp").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    let fp2: std::collections::BTreeSet<String> = e2
        .iter()
        .filter_map(|e| e.get("fp").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    assert!(fp1.is_disjoint(&fp2), "pages must cover different controls");
    assert_eq!(
        fp1.len() + fp2.len(),
        fp1.union(&fp2).count(),
        "union of pages must not duplicate controls"
    );
    // Depth clamp still reported truthfully at a shallow limit.
    let r3 = s.result(
        22,
        "elements",
        serde_json::json!({"title": title, "max_depth": 1}),
    );
    let sc3 = sc_of(&r3, "depth-1 walk");
    assert_eq!(
        sc3.get("depth_limit"),
        Some(&Value::from(1)),
        "depth_limit must echo max_depth 1, got: {sc3}"
    );
    println!(
        "PAGING_OK page1={} next=200 page2={} next=null",
        e1.len(),
        e2.len()
    );
    // Fixture guard drops here and reaps the process.
    drop(fx);
}

#[test]
fn wait_for_element_states() {
    if !live() {
        return;
    }
    kill_stale_fixtures("p3b-wait-live");
    kill_stale_fixtures("p3b-wait-gone");
    let mut fx = Fixture::spawn("p3b-wait-live", None);
    let title = fx.title.clone();
    std::thread::sleep(Duration::from_millis(500));
    let mut s = Session::spawn();
    let win = serde_json::json!("title:p3b-wait-live");
    // appear: the single EDIT (path 0) is already there.
    let r = s.result(
        30,
        "do",
        serde_json::json!({"actions": [{"type": "wait_for", "kind": "element", "window": win, "element": "path:0", "state": "appear", "timeout_ms": 5000}]}),
    );
    let first = &r["structuredContent"]["results"][0];
    assert_eq!(
        first.get("ok"),
        Some(&Value::from(true)),
        "appear must run, got: {first}"
    );
    assert_eq!(
        first.get("satisfied"),
        Some(&Value::from(true)),
        "existing control must satisfy appear, got: {first}"
    );
    // enabled: the EDIT is enabled.
    let r = s.result(
        31,
        "do",
        serde_json::json!({"actions": [{"type": "wait_for", "kind": "element", "window": win, "element": "path:0", "state": "enabled", "timeout_ms": 5000}]}),
    );
    let first = &r["structuredContent"]["results"][0];
    assert_eq!(
        first.get("satisfied"),
        Some(&Value::from(true)),
        "enabled EDIT must satisfy enabled, got: {first}"
    );
    // disabled on an enabled control: polls to timeout, not an error.
    let t1 = Instant::now();
    let r = s.result(
        32,
        "do",
        serde_json::json!({"actions": [{"type": "wait_for", "kind": "element", "window": win, "element": "path:0", "state": "disabled", "timeout_ms": 1500}]}),
    );
    let first = &r["structuredContent"]["results"][0];
    assert_eq!(
        first.get("ok"),
        Some(&Value::from(true)),
        "timeout must stay ok:true, got: {first}"
    );
    assert_eq!(
        first.get("satisfied"),
        Some(&Value::from(false)),
        "enabled control must not satisfy disabled, got: {first}"
    );
    assert!(
        t1.elapsed() >= Duration::from_millis(1200),
        "disabled wait must poll to timeout, took {:?}",
        t1.elapsed()
    );
    // disappear: close the window mid-wait from the test side; the wait must
    // observe the transition and satisfy.
    let mut fx2 = Fixture::spawn("p3b-wait-gone", None);
    let title2 = fx2.title.clone();
    std::thread::sleep(Duration::from_millis(500));
    let win2 = serde_json::json!("title:p3b-wait-gone");
    let mut s2 = Session::spawn();
    let deadline = Instant::now() + Duration::from_secs(20);
    s2.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 40, "method": "tools/call",
        "params": {"name": "do", "arguments": {"actions": [{"type": "wait_for", "kind": "element", "window": win2, "element": "path:0", "state": "disappear", "timeout_ms": 15000}]}},
    }));
    std::thread::sleep(Duration::from_millis(800));
    fx2.kill_and_reap();
    let mut done: Option<Value> = None;
    while done.is_none() && Instant::now() < deadline {
        for resp in s2.drain(Duration::from_millis(300)) {
            if resp.get("id").and_then(|v| v.as_i64()) == Some(40) {
                done = Some(resp);
                break;
            }
        }
    }
    let _ = title2;
    let resp = done.expect("disappear wait must respond");
    let first = &resp["result"]["structuredContent"]["results"][0];
    assert_eq!(
        first.get("satisfied"),
        Some(&Value::from(true)),
        "closing the window must satisfy disappear, got: {first}"
    );
    drop(title);
    fx.kill_and_reap();
    drop(fx2);
    println!("WAIT_OK appear+enabled+timeout+disappear green");
}

#[test]
fn wait_for_element_finds_control_past_first_page() {
    // Defect-1 regression: the wait must scan the whole tree, not just
    // offset 0. `p3b-btn-249` sits on page 2 of the 251-control fixture
    // tail, so appear/enabled can only succeed with a full paged scan;
    // after the fixture closes, disappear must succeed the same way.
    if !live() {
        return;
    }
    kill_stale_fixtures("p3b-wait-page2");
    let mut fx = Fixture::spawn("p3b-wait-page2", Some(250));
    std::thread::sleep(Duration::from_millis(500));
    let mut s = Session::spawn();
    let win = serde_json::json!("title:p3b-wait-page2");
    // appear on a page-2 control.
    let r = s.result(
        50,
        "do",
        serde_json::json!({"actions": [{"type": "wait_for", "kind": "element", "window": win, "element": "name:p3b-btn-249", "state": "appear", "timeout_ms": 10000}]}),
    );
    let first = &r["structuredContent"]["results"][0];
    assert_eq!(
        first.get("satisfied"),
        Some(&Value::from(true)),
        "page-2 control must satisfy appear via paged scan, got: {first}"
    );
    // enabled on the same page-2 control (fixture buttons are enabled).
    let r = s.result(
        51,
        "do",
        serde_json::json!({"actions": [{"type": "wait_for", "kind": "element", "window": win, "element": "name:p3b-btn-249", "state": "enabled", "timeout_ms": 10000}]}),
    );
    let first = &r["structuredContent"]["results"][0];
    assert_eq!(
        first.get("satisfied"),
        Some(&Value::from(true)),
        "page-2 control must satisfy enabled via paged scan, got: {first}"
    );
    // disappear after the fixture closes: the full scan must find nothing.
    fx.kill_and_reap();
    std::thread::sleep(Duration::from_millis(500));
    let r = s.result(
        52,
        "do",
        serde_json::json!({"actions": [{"type": "wait_for", "kind": "element", "window": win, "element": "name:p3b-btn-249", "state": "disappear", "timeout_ms": 10000}]}),
    );
    let first = &r["structuredContent"]["results"][0];
    assert_eq!(
        first.get("satisfied"),
        Some(&Value::from(true)),
        "closed fixture must satisfy disappear, got: {first}"
    );
    println!("WAIT_PAGE2_OK appear+enabled+disappear past first page green");
}

#[test]
fn wait_for_element_cross_page_ambiguity_is_an_error() {
    // Cross-page ambiguity: `p3b-dup` names two buttons, one on page 1 and
    // one past the 200-entry cap. The wait must scan every page, count both
    // hits, and fail fast with an ambiguous per-action error — never report
    // satisfied from the first page alone, never spin to timeout.
    if !live() {
        return;
    }
    kill_stale_fixtures("p3b-wait-ambig");
    let fx = Fixture::spawn("p3b-wait-ambig", Some(250));
    std::thread::sleep(Duration::from_millis(500));
    let mut s = Session::spawn();
    let win = serde_json::json!("title:p3b-wait-ambig");
    let t0 = Instant::now();
    let r = s.result(
        60,
        "do",
        serde_json::json!({"actions": [{"type": "wait_for", "kind": "element", "window": win, "element": "name:p3b-dup", "state": "appear", "timeout_ms": 10000}]}),
    );
    let first = &r["structuredContent"]["results"][0];
    assert_eq!(
        first.get("ok"),
        Some(&Value::from(false)),
        "cross-page duplicate must be a per-action error, got: {first}"
    );
    assert_eq!(
        first.get("satisfied"),
        Some(&Value::from(false)),
        "ambiguous wait must not report satisfied, got: {first}"
    );
    let err = first.get("error").and_then(|e| e.as_str()).unwrap_or("");
    assert!(
        err.contains("ambiguous"),
        "cross-page duplicate must name ambiguity, got: {first}"
    );
    assert!(
        t0.elapsed() < Duration::from_secs(8),
        "ambiguity must fail fast, not spin to timeout, took {:?}",
        t0.elapsed()
    );
    drop(fx);
    println!("WAIT_AMBIG_OK cross-page duplicate refused as ambiguous");
}
