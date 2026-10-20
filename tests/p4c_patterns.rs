//! P4c agent-grade UIA patterns: ExpandCollapse on a tree node, RangeValue
//! fallback on a trackbar, TextPattern read on a multiline edit.
//!
//! The schema assertions run everywhere (no desktop touch). The live tests
//! are gated by PUPPET_LIVE_DESKTOP=1 and drive only the dedicated fixture
//! app (`src/bin/puppet_fixture_app` in `p4c` mode): a window with a
//! SysTreeView32, a trackbar, and a multiline edit. No owner data, no input
//! beyond the fixture window itself.
//!
//! These tests fail on the pre-P4c tree (no invoke state, no RangeValue
//! fallback, no entry text) and pass after.

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
        println!("SKIP p4c_patterns: set PUPPET_LIVE_DESKTOP=1 to run (needs an interactive Windows session)");
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

/// Kill leftover fixture processes from a previous (possibly panicked) run.
/// Scoped strictly to our own fixture executable plus the exact test window
/// title — never touches owner apps.
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

/// Owning handle for one fixture process. `Drop` kills and reaps, so a
/// panic anywhere in the test still cleans up (no leaked windows to
/// contaminate reruns).
struct Fixture {
    child: Child,
    title: String,
}

impl Fixture {
    /// Spawn the fixture app (`argv[1]` = window title, `argv[2]` = mode).
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

#[test]
fn invoke_schema_declares_expand_state() {
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
    let invoke = variants
        .iter()
        .find(|v| {
            v.get("properties")
                .and_then(|p| p.get("type"))
                .and_then(|t| t.get("const"))
                .and_then(|c| c.as_str())
                == Some("invoke")
        })
        .expect("invoke variant present");
    let props = invoke
        .get("properties")
        .and_then(|v| v.as_object())
        .expect("invoke properties");
    let state = props
        .get("state")
        .unwrap_or_else(|| panic!("invoke variant must declare state, got: {props:?}"));
    let kinds: Vec<&str> = state
        .get("enum")
        .and_then(|e| e.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        kinds,
        vec!["expand", "collapse"],
        "invoke state must be exactly expand|collapse, got: {kinds:?}"
    );
    // No new action type: the schema_truth gate still sees 16 types.
    assert!(
        schema
            .get("properties")
            .and_then(|p| p.get("actions"))
            .is_some(),
        "do.actions must stay the single batched array"
    );
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

fn do_one_dry(s: &mut Session, id: i64, action: Value, bg: &str) -> Value {
    let r = s.result(
        id,
        "do",
        serde_json::json!({"actions": [action], "background": bg, "dry_run": true}),
    );
    let sc = sc_of(&r, "do");
    sc.get("results")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or_else(|| panic!("do must return one result, got: {sc}"))
}

#[test]
fn invoke_expand_collapse_tree_node() {
    if !live() {
        return;
    }
    kill_stale_fixtures("p4c-tree-live");
    let fx = Fixture::spawn("p4c-tree-live", "p4c");
    let title = fx.title.clone();
    std::thread::sleep(Duration::from_millis(500));
    let mut s = Session::spawn();
    let bg = format!("title:{title}");
    // Expand the root node: only ExpandCollapse may fire (never Invoke).
    let first = do_one(
        &mut s,
        10,
        serde_json::json!({"type": "invoke", "element": "name:p4c-root", "state": "expand"}),
        &bg,
    );
    assert_eq!(
        first.get("ok"),
        Some(&Value::from(true)),
        "expand must run, got: {first}"
    );
    assert_eq!(
        first.get("via").and_then(|v| v.as_str()),
        Some("uia-expand"),
        "expand must go through ExpandCollapse, got: {first}"
    );
    assert_eq!(
        first.get("expand_state").and_then(|v| v.as_str()),
        Some("Expanded"),
        "expanded node must read back Expanded, got: {first}"
    );
    // Collapse returns the node to Collapsed.
    let first = do_one(
        &mut s,
        11,
        serde_json::json!({"type": "invoke", "element": "name:p4c-root", "state": "collapse"}),
        &bg,
    );
    assert_eq!(
        first.get("ok"),
        Some(&Value::from(true)),
        "collapse must run, got: {first}"
    );
    assert_eq!(
        first.get("via").and_then(|v| v.as_str()),
        Some("uia-collapse"),
        "collapse must go through ExpandCollapse, got: {first}"
    );
    assert_eq!(
        first.get("expand_state").and_then(|v| v.as_str()),
        Some("Collapsed"),
        "collapsed node must read back Collapsed, got: {first}"
    );
    drop(fx);
    println!("EXPAND_OK expand+collapse with state read-back green");
}

#[test]
fn invoke_state_without_pattern_is_typed_error_no_click() {
    // A plain button has no ExpandCollapse: state expand must fail typed
    // (ok:false, no-pattern error, via uia-expand) and must NOT fall back
    // to a pixel click even when fallback:click is passed.
    if !live() {
        return;
    }
    kill_stale_fixtures("p4c-state-nopattern");
    let fx = Fixture::spawn("p4c-state-nopattern", "p4c");
    let title = fx.title.clone();
    std::thread::sleep(Duration::from_millis(500));
    let mut s = Session::spawn();
    let bg = format!("title:{title}");
    // The baseline EDIT (auto_id 100) is ValuePattern-only.
    let first = do_one(
        &mut s,
        70,
        serde_json::json!({"type": "invoke", "element": "auto_id:100", "state": "expand", "fallback": "click"}),
        &bg,
    );
    assert_eq!(
        first.get("ok"),
        Some(&Value::from(false)),
        "state on a pattern-less control must fail, got: {first}"
    );
    let err = first.get("error").and_then(|e| e.as_str()).unwrap_or("");
    assert!(
        err.contains("ExpandCollapse"),
        "failure must name the missing pattern, got: {first}"
    );
    assert_eq!(
        first.get("via").and_then(|v| v.as_str()),
        Some("uia-expand"),
        "no-pattern failure must not reroute to pixel-click, got: {first}"
    );
    drop(fx);
    println!("NOPATTERN_OK state without pattern is a typed error, no click green");
}

#[test]
fn invoke_state_dry_run_validation() {
    // Dry-run validates state before resolving: bogus state fails typed,
    // valid state resolves like plain invoke (ok, found, no firing).
    if !live() {
        return;
    }
    kill_stale_fixtures("p4c-state-dryrun");
    let fx = Fixture::spawn("p4c-state-dryrun", "p4c");
    let title = fx.title.clone();
    std::thread::sleep(Duration::from_millis(500));
    let mut s = Session::spawn();
    let bg = format!("title:{title}");
    let first = do_one_dry(
        &mut s,
        71,
        serde_json::json!({"type": "invoke", "element": "name:p4c-root", "state": "bogus"}),
        &bg,
    );
    assert_eq!(
        first.get("ok"),
        Some(&Value::from(false)),
        "dry-run bogus state must fail, got: {first}"
    );
    assert!(
        first
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("")
            .contains("expand|collapse"),
        "dry-run bogus state must name the accepted values, got: {first}"
    );
    let first = do_one_dry(
        &mut s,
        72,
        serde_json::json!({"type": "invoke", "element": "name:p4c-root", "state": "expand"}),
        &bg,
    );
    assert_eq!(
        first.get("ok"),
        Some(&Value::from(true)),
        "dry-run valid state must resolve, got: {first}"
    );
    assert_eq!(
        first.get("resolution").and_then(|v| v.as_str()),
        Some("found"),
        "dry-run valid state must report found, got: {first}"
    );
    drop(fx);
    println!("DRYRUN_OK state validated before dry-run return green");
}

#[test]
fn set_value_range_trackbar() {
    if !live() {
        return;
    }
    kill_stale_fixtures("p4c-range-live");
    let fx = Fixture::spawn("p4c-range-live", "p4c");
    let title = fx.title.clone();
    std::thread::sleep(Duration::from_millis(500));
    let mut s = Session::spawn();
    let bg = format!("title:{title}");
    // Numeric write through the RangeValue fallback, with read-back.
    let first = do_one(
        &mut s,
        20,
        serde_json::json!({"type": "set_value", "element": "auto_id:3001", "text": "42"}),
        &bg,
    );
    assert_eq!(
        first.get("ok"),
        Some(&Value::from(true)),
        "range set must run, got: {first}"
    );
    assert_eq!(
        first.get("value").and_then(|v| v.as_f64()),
        Some(42.0),
        "range write must read back the live value, got: {first}"
    );
    // Out-of-range is a typed error, not a silent clamp.
    let first = do_one(
        &mut s,
        21,
        serde_json::json!({"type": "set_value", "element": "auto_id:3001", "text": "200"}),
        &bg,
    );
    assert_eq!(
        first.get("ok"),
        Some(&Value::from(false)),
        "out-of-range must fail, got: {first}"
    );
    assert!(
        first
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("")
            .contains("out of range"),
        "out-of-range must name the bound, got: {first}"
    );
    // Non-numeric text is a typed error, never a silent 0.
    let first = do_one(
        &mut s,
        22,
        serde_json::json!({"type": "set_value", "element": "auto_id:3001", "text": "loud"}),
        &bg,
    );
    assert_eq!(
        first.get("ok"),
        Some(&Value::from(false)),
        "non-numeric must fail, got: {first}"
    );
    assert!(
        first
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("")
            .contains("not a number"),
        "non-numeric must name the problem, got: {first}"
    );
    drop(fx);
    println!("RANGE_OK set+readback+range-errors green");
}

fn detail_text(s: &mut Session, id: i64, title: &str) -> (Vec<Value>, Value) {
    let r = s.result(
        id,
        "elements",
        serde_json::json!({"title": title, "detail": true}),
    );
    let sc = sc_of(&r, "elements detail");
    let els = sc
        .get("elements")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    (els, sc)
}

fn find_auto<'a>(els: &'a [Value], auto_id: &str) -> &'a Value {
    els.iter()
        .find(|e| e.get("auto_id").and_then(|v| v.as_str()) == Some(auto_id))
        .unwrap_or_else(|| panic!("elements must list auto_id:{auto_id}"))
}

#[test]
fn elements_detail_text_and_fp_stability() {
    if !live() {
        return;
    }
    kill_stale_fixtures("p4c-text-live");
    let fx = Fixture::spawn("p4c-text-live", "p4c");
    let title = fx.title.clone();
    std::thread::sleep(Duration::from_millis(500));
    let mut s = Session::spawn();
    let bg = format!("title:{title}");
    // Text present, untruncated, on the multiline edit.
    let (els, _) = detail_text(&mut s, 30, &title);
    let edit = find_auto(&els, "3002");
    assert_eq!(
        edit.get("text_truncated"),
        Some(&Value::from(false)),
        "short text must not be truncated, got: {edit}"
    );
    let text = edit.get("text").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        text.contains("p4c-line1") && text.contains("p4c-line3"),
        "multiline text must be extracted, got: {text:?}"
    );
    let fp_before = edit.get("fp").cloned().expect("entry carries fp");
    // Rewrite the content: fp must not move (text is not identity).
    let first = do_one(
        &mut s,
        31,
        serde_json::json!({"type": "set_value", "element": "auto_id:3002", "text": "changed"}),
        &bg,
    );
    assert_eq!(
        first.get("ok"),
        Some(&Value::from(true)),
        "edit rewrite must run, got: {first}"
    );
    let (els, _) = detail_text(&mut s, 32, &title);
    let edit = find_auto(&els, "3002");
    assert_eq!(
        edit.get("fp"),
        Some(&fp_before),
        "text edit must not change fp, got: {edit}"
    );
    let text = edit.get("text").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        text.contains("changed"),
        "rewritten text must read back, got: {text:?}"
    );
    drop(fx);
    println!("TEXT_OK extract+truncation-flag+fp-stability green");
}
