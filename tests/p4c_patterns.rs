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

/// The contract may not claim more than the hash proves. `fp` is FNV-1a over
/// role/name/auto_id/invokable only, so a replacement control carrying those
/// same four properties hashes identically: an `fp` match establishes identity
/// of those four properties, never uniqueness of the control. Both the
/// `fp_scope` limit an agent reads first and the two tool descriptions that
/// take an `fp` must say so, or an agent reads a match as proof it is acting
/// on the control it saw.
#[test]
fn the_fingerprint_prose_states_what_the_hash_proves() {
    let claims = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/claims.rs"))
        .expect("claims.rs must be readable");
    let at = claims
        .find("\"fp_scope\"")
        .unwrap_or_else(|| panic!("claims.rs must carry the fp_scope limit"));
    let row = &claims[at..];
    let line = &row[..row.find("\n    ),").unwrap_or(row.len())];
    for named in ["role", "name", "auto_id", "invokable"] {
        assert!(
            line.contains(named),
            "the fp_scope limit must name the property the hash covers ({named}): {line:?}"
        );
    }
    assert!(
        line.contains("replacement"),
        "the fp_scope limit must say a replacement with the same properties is \
         indistinguishable, never that a match proves the control: {line:?}"
    );
    let mut s = Session::spawn();
    let tools = list_tools(&mut s);
    for name in ["elements", "do"] {
        let desc = tool(&tools, name)
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or_else(|| panic!("{name} must carry a description"));
        assert!(
            desc.contains("replacement"),
            "{name} must say a replacement carrying the same properties has the same fp, \
             got: {desc}"
        );
        for named in ["role", "name", "auto_id"] {
            assert!(
                desc.contains(named),
                "{name} must name the property the hash covers ({named}), got: {desc}"
            );
        }
    }
    println!("FP_SCOPE_OK the contract states what the hash proves");
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

/// Same, but with the explicit `allow_foreground:true` that 5.0.0's contract
/// requires for UIA WRITES (`set_value`, and `type` on an element): their
/// provider may focus the control it writes, so an unflagged call is refused
/// with `foreground_required` before any provider call. Only the write
/// actions use this; `invoke`/expand/collapse stay background-safe and keep
/// going through `do_one` with no flag, which is exactly what those tests
/// prove.
fn do_one_fg(s: &mut Session, id: i64, action: Value, bg: &str) -> Value {
    let r = s.result(
        id,
        "do",
        serde_json::json!({"actions": [action], "background": bg, "allow_foreground": true}),
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
    let root = fp_of(&mut s, 9, &title, "name", "p4c-root");
    let first = do_one(
        &mut s,
        10,
        serde_json::json!({"type": "invoke", "element": "name:p4c-root", "state": "expand", "expect_fp": root}),
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
        serde_json::json!({"type": "invoke", "element": "name:p4c-root", "state": "collapse", "expect_fp": root}),
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
    let edit = fp_of(&mut s, 69, &title, "auto_id", "100");
    let first = do_one(
        &mut s,
        70,
        serde_json::json!({"type": "invoke", "element": "auto_id:100", "state": "expand", "fallback": "click", "expect_fp": edit}),
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
    let root = fp_of(&mut s, 70, &title, "name", "p4c-root");
    let first = do_one_dry(
        &mut s,
        71,
        serde_json::json!({"type": "invoke", "element": "name:p4c-root", "state": "bogus", "expect_fp": root}),
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
        serde_json::json!({"type": "invoke", "element": "name:p4c-root", "state": "expand", "expect_fp": root}),
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
    // Policy proof: the SAME write without the flag refuses typed with
    // background_unavailable naming the control, and the flagged write
    // refuses identically — no flag buys a UIA text write.
    let refused = do_one(
        &mut s,
        19,
        serde_json::json!({"type": "set_value", "element": "auto_id:3001", "text": "77"}),
        &bg,
    );
    assert_eq!(
        refused.get("ok"),
        Some(&Value::from(false)),
        "unflagged set_value must refuse, got: {refused}"
    );
    assert_eq!(
        refused.get("code").and_then(|c| c.as_str()),
        Some("background_unavailable"),
        "unflagged set_value must be typed background_unavailable, got: {refused}"
    );
    assert!(
        refused
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("")
            .contains("auto_id:3001"),
        "refusal must name the control, got: {refused}"
    );
    // A flagged write refuses the same way: the policy has no opt-in.
    // set_value is a UIA write (the provider may focus the control).
    for (id, text) in [(20, "42"), (21, "200"), (22, "loud")] {
        let first = do_one_fg(
            &mut s,
            id,
            serde_json::json!({"type": "set_value", "element": "auto_id:3001", "text": text}),
            &bg,
        );
        assert_eq!(
            first.get("ok"),
            Some(&Value::from(false)),
            "flagged set_value must refuse, got: {first}"
        );
        assert_eq!(
            first.get("code").and_then(|c| c.as_str()),
            Some("background_unavailable"),
            "flagged set_value must be typed, got: {first}"
        );
    }
    drop(fx);
    println!("RANGE_OK set_value refused, nothing written");
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

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Post the fixture's own re-render message to its top-level window: the p4c
/// fixture renames its live button, so `auto_id:3003` still resolves while the
/// fingerprint moves. The trigger comes from the harness, never from the
/// server under test.
fn rename_live_button(title: &str) {
    const WM_P4C_RENAME: u32 = 0x8000 + 1;
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};
    let cls = wide("PuppetFixtureApp");
    let t = wide(title);
    let hwnd = unsafe { FindWindowW(cls.as_ptr(), t.as_ptr()) };
    assert!(!hwnd.is_null(), "fixture window {title:?} must exist");
    let posted = unsafe { PostMessageW(hwnd, WM_P4C_RENAME, 0, 0) };
    assert!(posted != 0, "the re-render post must land");
}

/// Post the W2.3 swap: a different control takes the waited name.
fn swap_waited_name(title: &str) {
    const WM_P4C_SWAP: u32 = 0x8000 + 2;
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};
    let cls = wide("PuppetFixtureApp");
    let t = wide(title);
    let hwnd = unsafe { FindWindowW(cls.as_ptr(), t.as_ptr()) };
    assert!(!hwnd.is_null(), "fixture window {title:?} must exist");
    let posted = unsafe { PostMessageW(hwnd, WM_P4C_SWAP, 0, 0) };
    assert!(posted != 0, "the swap post must land");
}

/// The fingerprint the refusal hands back, so the retry costs one call.
fn fresh_fp(error: &str) -> String {
    let at = error
        .find("(fp ")
        .unwrap_or_else(|| panic!("refusal must carry the fresh fp, got: {error:?}"));
    error[at + 4..]
        .split(')')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// The live button's activation count, read back through `elements`: the
/// fixture mirrors it into a read-only STATIC (`activations=N`).
fn activations(s: &mut Session, id: i64, title: &str) -> String {
    let (els, _) = detail_text(s, id, title);
    find_auto(&els, "3004")
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// The fingerprint of one control, read from a fresh `elements` snapshot: the
/// value every element mutation now passes as `expect_fp` (W2.1).
fn fp_of(s: &mut Session, id: i64, title: &str, key: &str, value: &str) -> String {
    let (els, _) = detail_text(s, id, title);
    els.iter()
        .find(|e| e.get(key).and_then(|v| v.as_str()) == Some(value))
        .and_then(|e| e.get("fp"))
        .and_then(|v| v.as_str())
        .map(|fp| fp.to_string())
        .unwrap_or_else(|| panic!("elements must list {key}:{value} with an fp"))
}

/// W2.1: an element mutation with no `expect_fp` refuses and hands back the
/// fresh fingerprint; a stale one keeps `stale_ref`; the one-call retry acts.
/// The fixture re-renders between the snapshot and the invoke, so the
/// selector resolves to a control the snapshot never saw.
#[test]
fn invoke_without_expect_fp_refuses_and_the_one_call_retry_acts() {
    if !live() {
        return;
    }
    kill_stale_fixtures("p4c-fp-live");
    let fx = Fixture::spawn("p4c-fp-live", "p4c");
    let title = fx.title.clone();
    std::thread::sleep(Duration::from_millis(500));
    let mut s = Session::spawn();
    let bg = format!("title:{title}");
    // Snapshot the button, then re-render it under the same selector.
    let (els, _) = detail_text(&mut s, 40, &title);
    let fp_before = find_auto(&els, "3003")
        .get("fp")
        .cloned()
        .expect("entry carries fp");
    assert_eq!(activations(&mut s, 41, &title), "activations=0");
    rename_live_button(&title);
    std::thread::sleep(Duration::from_millis(200));
    // No expect_fp: refused before anything is activated, with the fresh
    // fingerprint in the refusal so no second round-trip is needed.
    let refused = do_one(
        &mut s,
        42,
        serde_json::json!({"type": "invoke", "element": "auto_id:3003"}),
        &bg,
    );
    assert_eq!(
        refused.get("ok"),
        Some(&Value::from(false)),
        "a mutation with no expect_fp must refuse, got: {refused}"
    );
    assert_eq!(
        refused.get("code").and_then(|c| c.as_str()),
        Some("expect_fp_required"),
        "refusal must be typed expect_fp_required, got: {refused}"
    );
    assert_eq!(
        refused.get("resolution").and_then(|r| r.as_str()),
        Some("expect_fp_required"),
        "refusal must name its resolution, got: {refused}"
    );
    let error = refused
        .get("error")
        .and_then(|e| e.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        error.contains("auto_id:3003"),
        "refusal must name the selector, got: {error:?}"
    );
    let fresh = fresh_fp(&error);
    assert_ne!(
        Some(fresh.as_str()),
        fp_before.as_str(),
        "the re-render must move the fingerprint, got: {fresh:?}"
    );
    assert_eq!(
        activations(&mut s, 43, &title),
        "activations=0",
        "a refused mutation must activate nothing"
    );
    // Under dry_run the same refusal fires: a dry run that resolved while the
    // live call refuses would be a lie.
    let dry = do_one_dry(
        &mut s,
        48,
        serde_json::json!({"type": "invoke", "element": "auto_id:3003"}),
        &bg,
    );
    assert_eq!(
        dry.get("code").and_then(|c| c.as_str()),
        Some("expect_fp_required"),
        "dry_run must refuse exactly like the live call, got: {dry}"
    );
    // The snapshot's fingerprint is now stale: same shape, fresh value added.
    let stale = do_one(
        &mut s,
        44,
        serde_json::json!({"type": "invoke", "element": "auto_id:3003", "expect_fp": fp_before}),
        &bg,
    );
    assert_eq!(
        stale.get("code").and_then(|c| c.as_str()),
        Some("stale_ref"),
        "a mismatched expect_fp must stay stale_ref, got: {stale}"
    );
    assert_eq!(
        stale.get("resolution").and_then(|r| r.as_str()),
        Some("stale"),
        "a mismatched expect_fp must keep resolution:stale, got: {stale}"
    );
    assert_eq!(
        activations(&mut s, 45, &title),
        "activations=0",
        "a stale ref must activate nothing"
    );
    // The one-call retry, with the fingerprint the refusal returned.
    let retried = do_one(
        &mut s,
        46,
        serde_json::json!({"type": "invoke", "element": "auto_id:3003", "expect_fp": fresh}),
        &bg,
    );
    assert_eq!(
        retried.get("ok"),
        Some(&Value::from(true)),
        "the one-call retry must act, got: {retried}"
    );
    // SPEC W2.1: the retry reports `effect`, and the value is read, not
    // assumed: a push button exposes no state an activation can move
    // (no ExpandCollapse, no RangeValue, no TextPattern), so the honest
    // reading is `unverified` - SPEC Appendix B's own words for "pattern
    // activations with no readable state". `ok` alone says the pattern was
    // delivered.
    assert_eq!(
        retried.get("effect").and_then(|e| e.as_str()),
        Some("unverified"),
        "a stateless control's invoke must not claim a change, got: {retried}"
    );
    assert_eq!(
        retried.get("delivered").and_then(|d| d.as_str()),
        Some("uia"),
        "a UIA pattern activation must report delivered:uia, got: {retried}"
    );
    assert_eq!(
        activations(&mut s, 47, &title),
        "activations=1",
        "the retry must activate the control exactly once"
    );
    drop(fx);
    println!("EXPECT_FP_OK refused, stale-refused, one-call retry activated once");
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
    // Rewrite refused: fp must not move and the text must not change.
    // A UIA write, so it is refused with background_unavailable.
    let first = do_one_fg(
        &mut s,
        31,
        serde_json::json!({"type": "set_value", "element": "auto_id:3002", "text": "changed"}),
        &bg,
    );
    assert_eq!(
        first.get("ok"),
        Some(&Value::from(false)),
        "edit rewrite must refuse, got: {first}"
    );
    assert_eq!(
        first.get("code").and_then(|c| c.as_str()),
        Some("background_unavailable"),
        "edit rewrite must be typed, got: {first}"
    );
    let (els, _) = detail_text(&mut s, 32, &title);
    let edit = find_auto(&els, "3002");
    assert_eq!(
        edit.get("fp"),
        Some(&fp_before),
        "a refused write must not change fp, got: {edit}"
    );
    let text = edit.get("text").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        !text.contains("changed"),
        "refused text must not read back, got: {text:?}"
    );
    drop(fx);
    println!("TEXT_OK extract+truncation-flag+fp-stability green");
}

/// SPEC W2.3: an element wait names the control that satisfied it. The
/// fixture renames a different control to the waited name 200 ms in, so the
/// wait is satisfied by a control the caller never snapshotted: `satisfied_by`
/// carries that winner's fingerprint and path, `fp_matched` reports the
/// mismatch as a fact, and the wait is still not an error.
#[test]
fn an_element_wait_reports_the_control_that_satisfied_it() {
    if !live() {
        return;
    }
    kill_stale_fixtures("p4c-wait-winner");
    let fx = Fixture::spawn("p4c-wait-winner", "p4c");
    let title = fx.title.clone();
    std::thread::sleep(Duration::from_millis(500));
    let mut s = Session::spawn();
    let bg = format!("title:{title}");
    // The caller's snapshot: the control that carries the name now, disabled,
    // so an `enabled` wait cannot be satisfied by it.
    let snapshot = fp_of(&mut s, 100, &title, "name", "p4c-wait");
    // The match half of the report: the wait is satisfied by the very control
    // the snapshot named, so `fp_matched` is true. Without this case an
    // implementation that always reported false passed the suite.
    let matched = do_one(
        &mut s,
        99,
        serde_json::json!({
            "type": "wait_for",
            "kind": "element",
            "element": "name:p4c-wait",
            "state": "appear",
            "window": bg.clone(),
            "timeout_ms": 2000,
            "expect_fp": snapshot.clone(),
        }),
        &bg,
    );
    assert_eq!(
        matched.get("satisfied"),
        Some(&Value::from(true)),
        "the control the snapshot named is present, got: {matched}"
    );
    assert_eq!(
        matched.get("fp_matched"),
        Some(&Value::from(true)),
        "a wait satisfied by the snapshotted control must report fp_matched:true, got: {matched}"
    );
    assert_eq!(
        matched
            .get("satisfied_by")
            .and_then(|b| b.get("fp"))
            .and_then(|v| v.as_str()),
        Some(snapshot.as_str()),
        "the winner must be the snapshotted control, got: {matched}"
    );
    // A different control takes the name 200 ms into the wait.
    let swap_title = title.clone();
    let swap = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        swap_waited_name(&swap_title);
    });
    let waited = do_one(
        &mut s,
        101,
        serde_json::json!({
            "type": "wait_for",
            "kind": "element",
            "element": "name:p4c-wait",
            "state": "enabled",
            "window": bg.clone(),
            "timeout_ms": 4000,
            "expect_fp": snapshot.clone(),
        }),
        &bg,
    );
    swap.join().expect("the swap thread must not panic");
    assert_eq!(
        waited.get("satisfied"),
        Some(&Value::from(true)),
        "the wait must be satisfied by the control that took the name, got: {waited}"
    );
    assert_eq!(
        waited.get("ok"),
        Some(&Value::from(true)),
        "a satisfied wait is never an error, got: {waited}"
    );
    let by = waited
        .get("satisfied_by")
        .and_then(|b| b.as_object())
        .unwrap_or_else(|| panic!("a satisfied element wait must report satisfied_by: {waited}"));
    assert_eq!(
        by.get("selector").and_then(|v| v.as_str()),
        Some("name:p4c-wait"),
        "satisfied_by must name the selector that won, got: {by:?}"
    );
    let winner = by
        .get("fp")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("satisfied_by must carry the winner's fp: {by:?}"));
    assert_ne!(
        winner, snapshot,
        "the winner is the control that took the name, not the snapshot's"
    );
    assert!(
        by.get("path").and_then(|v| v.as_str()).is_some(),
        "satisfied_by must carry the winner's path, got: {by:?}"
    );
    assert_eq!(
        waited.get("fp_matched"),
        Some(&Value::from(false)),
        "a supplied expect_fp is reported, never enforced, got: {waited}"
    );
    // A timeout is the same report with no winner: satisfied_by null, and the
    // action still delivered (ok:true).
    let timed_out = do_one(
        &mut s,
        102,
        serde_json::json!({
            "type": "wait_for",
            "kind": "element",
            "element": "name:never-appears",
            "window": bg.clone(),
            "timeout_ms": 200,
        }),
        &bg,
    );
    assert_eq!(
        timed_out.get("ok"),
        Some(&Value::from(true)),
        "a timeout is never an error, got: {timed_out}"
    );
    assert_eq!(
        timed_out.get("satisfied"),
        Some(&Value::from(false)),
        "got: {timed_out}"
    );
    assert_eq!(
        timed_out.get("satisfied_by"),
        Some(&Value::Null),
        "nothing won, so satisfied_by is null, got: {timed_out}"
    );
    // Every other kind is unchanged: no satisfied_by on its result.
    let window_wait = do_one(
        &mut s,
        103,
        serde_json::json!({
            "type": "wait_for",
            "kind": "window_appear",
            "selector": format!("title:{title}"),
            "timeout_ms": 1000,
        }),
        &bg,
    );
    assert_eq!(
        window_wait.get("satisfied"),
        Some(&Value::from(true)),
        "the fixture window exists, got: {window_wait}"
    );
    assert!(
        !window_wait
            .as_object()
            .expect("result object")
            .contains_key("satisfied_by"),
        "satisfied_by belongs to element waits only, got: {window_wait}"
    );
    drop(fx);
    println!("WAIT_WINNER_OK winner={winner} snapshot={snapshot} fp_matched=false");
}

/// SPEC W2.2 live: `fp` is identity, never state. A real state change (the
/// tree node expands, and the control reads back `Expanded`) leaves the
/// fingerprint where it was, so the snapshot's fp still gates the next
/// mutation: default-on `expect_fp` never refuses a toggle, and an agent
/// learns the condition from a readback, not from a fingerprint.
#[test]
fn fp_is_identity_not_state_across_a_real_state_change() {
    if !live() {
        return;
    }
    kill_stale_fixtures("p4c-fp-state");
    let fx = Fixture::spawn("p4c-fp-state", "p4c");
    let title = fx.title.clone();
    std::thread::sleep(Duration::from_millis(500));
    let mut s = Session::spawn();
    let bg = format!("title:{title}");
    let before = fp_of(&mut s, 90, &title, "name", "p4c-root");
    let expanded = do_one(
        &mut s,
        91,
        serde_json::json!({"type": "invoke", "element": "name:p4c-root", "state": "expand", "expect_fp": before.clone()}),
        &bg,
    );
    assert_eq!(
        expanded.get("ok"),
        Some(&Value::from(true)),
        "expand must run, got: {expanded}"
    );
    assert_eq!(
        expanded.get("expand_state").and_then(|v| v.as_str()),
        Some("Expanded"),
        "the state must actually move, got: {expanded}"
    );
    // The same verb on a control whose state an activation CAN move reads the
    // change: `effect` is derived from the readback, never a constant.
    assert_eq!(
        expanded.get("effect").and_then(|v| v.as_str()),
        Some("changed"),
        "an expand that moved the state must report effect:changed, got: {expanded}"
    );
    let after = fp_of(&mut s, 92, &title, "name", "p4c-root");
    assert_eq!(
        before, after,
        "a state change must leave the fingerprint alone"
    );
    // The fingerprint the first call used still matches after the state moved:
    // identity survived the toggle.
    let collapsed = do_one(
        &mut s,
        93,
        serde_json::json!({"type": "invoke", "element": "name:p4c-root", "state": "collapse", "expect_fp": after}),
        &bg,
    );
    assert_eq!(
        collapsed.get("ok"),
        Some(&Value::from(true)),
        "the snapshot fp must still match after a state change, got: {collapsed}"
    );
    drop(fx);
    println!("FP_STATE_OK fp={before} unchanged across expand/collapse");
}
