//! No-steal invariants, proven at three levels:
//!
//! 1. STATIC: the forbidden APIs (`SendInput`, `SetCursorPos`,
//!    `SetForegroundWindow`, `BringWindowToTop`, `mouse_event`) have ZERO
//!    call sites anywhere — including `src/gate.rs`, which holds no
//!    injection API at all. A new call site anywhere turns this test red.
//! 2. WIRE: `tools/list` declares `target` on every acting/capturing tool and
//!    keeps the (now inert) `allow_foreground`/`allow_cursor`/
//!    `allow_clipboard` flags so old callers get a typed refusal instead of
//!    an "unexpected keyword" error.
//! 3. SERVER: a typed target failure comes back as an MCP tool error with a
//!    machine-readable `code` (`target_gone` / `bad_target`), never a
//!    foreground fallback; a gated burst refuses with `no_foreground_policy` /
//!    `no_cursor_policy` / `no_clipboard_policy` and reports zero
//!    events and zero `sendinput_calls`.
//!
//! The live read-only check (an existing real window captured without
//! changing the foreground) is gated by `PUPPET_LIVE_DESKTOP=1`.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(30);

const FORBIDDEN: &[&str] = &[
    "SendInput",
    "SetCursorPos",
    "SetForegroundWindow",
    "BringWindowToTop",
    "mouse_event",
];

fn src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rs_files(dir: &PathBuf, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

fn call_sites(text: &str, symbol: &str) -> Vec<String> {
    let needle = format!("{symbol}(");
    text.lines()
        .filter(|line| {
            let t = line.trim_start();
            !(t.starts_with("//") || t.starts_with("///") || t.starts_with("//!"))
        })
        .filter(|line| line.contains(&needle))
        .map(|line| line.trim().to_string())
        .collect()
}

#[test]
fn forbidden_apis_live_nowhere() {
    let mut files = Vec::new();
    rs_files(&src_dir(), &mut files);
    assert!(
        files.len() >= 10,
        "expected the real source tree, got {files:?}"
    );
    for sym in FORBIDDEN {
        let mut offenders: Vec<String> = Vec::new();
        for f in &files {
            let name = f.file_name().and_then(|x| x.to_str()).unwrap_or("");
            let text = std::fs::read_to_string(f).expect("source file readable");
            for site in call_sites(&text, sym) {
                offenders.push(format!("{name}: {site}"));
            }
        }
        assert!(
            offenders.is_empty(),
            "{sym} must have no call site anywhere — the escalation path is deleted, found: {offenders:#?}"
        );
    }
    println!("NO_STEAL_STATIC_OK symbols={FORBIDDEN:?} banished from src/");
}

#[test]
fn gate_holds_no_permits_and_no_injection_apis() {
    let gate = std::fs::read_to_string(src_dir().join("gate.rs")).expect("gate.rs readable");
    let prod = gate.split("#[cfg(test)]").next().unwrap_or("");
    for mint in [
        "ForegroundPermit",
        "CursorPermit",
        "ClipboardPermit",
        "pub fn grant(",
        "grant_explicit_focus",
    ] {
        assert!(
            !prod.contains(mint),
            "gate production half must not contain {mint}: the escalation path is deleted"
        );
    }
    let prod = gate.split("#[cfg(test)]").next().unwrap_or("");
    for banned in [
        concat!("fn send", "_input"),
        concat!("fn set", "_cursor_pos"),
        concat!("fn set", "_foreground"),
        concat!("Send", "Input("),
        concat!("Set", "CursorPos("),
        concat!("Set", "ForegroundWindow("),
        concat!("Bring", "WindowToTop("),
    ] {
        assert!(
            !prod.contains(banned),
            "gate.rs production half must not contain {banned}"
        );
    }
    println!("NO_STEAL_MINT_OK permits=0 injection-apis=0");
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
                        if t.is_empty() || tx.send(t).is_err() {
                            continue;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Session { child, stdin, rx }
    }

    fn send(&mut self, frame: &Value) {
        let line = serde_json::to_string(frame).unwrap();
        self.stdin.write_all(line.as_bytes()).expect("write frame");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush frame");
    }

    fn call(&mut self, id: i64, name: &str, arguments: Value) -> Value {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        }));
        let resp: Value = self
            .rx
            .recv_timeout(TIMEOUT)
            .ok()
            .map(|l| serde_json::from_str(&l).expect("response is JSON"))
            .expect("response in time");
        resp
    }

    fn list_tools(&mut self) -> Vec<Value> {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {},
        }));
        let resp: Value = self
            .rx
            .recv_timeout(TIMEOUT)
            .ok()
            .map(|l| serde_json::from_str(&l).expect("response is JSON"))
            .expect("tools/list in time");
        resp.get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_else(|| panic!("tools/list must carry tools, got: {resp}"))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn tool<'a>(tools: &'a [Value], name: &str) -> &'a serde_json::Map<String, Value> {
    tools
        .iter()
        .find(|t| t.get("name").and_then(|n| n.as_str()) == Some(name))
        .and_then(|t| t.as_object())
        .unwrap_or_else(|| panic!("tools/list must contain {name}"))
}

#[test]
fn every_acting_tool_declares_target_and_the_gate_flags() {
    let mut s = Session::spawn();
    let tools = s.list_tools();
    for name in ["screenshot", "elements", "window", "do"] {
        let schema = tool(&tools, name)["inputSchema"].clone();
        let props = schema["properties"].as_object().expect("properties");
        assert!(
            props.contains_key("target"),
            "{name}.inputSchema must declare target, got: {:?}",
            props.keys().collect::<Vec<_>>()
        );
    }
    for name in ["window", "do"] {
        let props = tool(&tools, name)["inputSchema"]["properties"]
            .as_object()
            .expect("properties");
        assert!(
            props.contains_key("allow_foreground"),
            "{name} must declare allow_foreground"
        );
    }
    let props = tool(&tools, "do")["inputSchema"]["properties"]
        .as_object()
        .expect("properties");
    assert!(
        props.contains_key("allow_cursor"),
        "do must declare allow_cursor"
    );
    assert!(
        props.contains_key("allow_clipboard"),
        "do must declare allow_clipboard (clipboard_set is deny-by-default)"
    );
    let ab = props.get("allow_clipboard").expect("allow_clipboard");
    assert_eq!(
        ab.get("type").and_then(|t| t.as_str()),
        Some("boolean"),
        "allow_clipboard must be a boolean, got: {ab}"
    );
    assert_eq!(
        ab.get("default"),
        Some(&Value::from(false)),
        "allow_clipboard must default to false, got: {ab}"
    );
    assert!(
        ab.get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .contains("global clipboard"),
        "allow_clipboard must state what it permits, got: {ab}"
    );
    let lw = tool(&tools, "list_windows")["inputSchema"]["properties"]
        .as_object()
        .expect("properties");
    assert!(
        !lw.contains_key("target"),
        "list_windows stays argument-free"
    );
    println!("NO_STEAL_SCHEMA_OK target=4-tools flags=window,do");
}

fn error_text(resp: &Value) -> String {
    resp.get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[test]
fn dead_target_is_a_typed_whole_call_error() {
    let mut s = Session::spawn();
    let resp = s.call(10, "screenshot", serde_json::json!({"target": {"hwnd": 1}}));
    assert_eq!(
        resp["result"]["isError"],
        Value::from(true),
        "a dead target must be a tool error, got: {resp}"
    );
    let text = error_text(&resp);
    assert!(
        text.contains("target_gone"),
        "dead hwnd must be target_gone, got: {text}"
    );
    println!("NO_STEAL_TARGET_GONE_OK {text}");
}

#[test]
fn malformed_target_is_bad_target() {
    let mut s = Session::spawn();
    let resp = s.call(11, "elements", serde_json::json!({"target": {"nope": 1}}));
    let text = error_text(&resp);
    assert!(
        text.contains("bad_target"),
        "unknown target field must be bad_target, got: {text}"
    );
    let resp = s.call(12, "do", serde_json::json!({"actions": [], "target": 7}));
    let text = error_text(&resp);
    assert!(
        text.contains("target"),
        "a non-object target must be rejected by the argument validator, got: {text}"
    );
    println!("NO_STEAL_BAD_TARGET_OK {text}");
}

#[test]
fn gated_burst_refuses_with_typed_codes_and_zero_events() {
    let mut s = Session::spawn();
    let resp = s.call(
        13,
        "do",
        serde_json::json!({
            "actions": [
                {"type": "click", "x": 5, "y": 5},
                {"type": "type", "text": "hi"},
                {"type": "hotkey", "keys": ["ctrl", "c"]}
            ]
        }),
    );
    let sc = resp["result"]["structuredContent"].clone();
    let results = sc["results"].as_array().expect("results");
    let codes: Vec<&str> = results
        .iter()
        .map(|r| r["code"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        codes,
        vec!["no_cursor_policy", "target_required", "target_required"],
        "gate matrix over the wire, got: {results:?}"
    );
    assert_eq!(sc["events"], Value::from(0), "nothing may be injected");
    assert_eq!(sc["sendinput_calls"], Value::from(0));
    println!("NO_STEAL_GATE_OK codes={codes:?} events=0");
}

#[test]
fn clipboard_write_is_refused_over_the_wire() {
    let mut s = Session::spawn();
    let resp = s.call(
        15,
        "do",
        serde_json::json!({
            "actions": [
                {"type": "clipboard_set", "text": "SHOULD-NOT-LAND"},
                {"type": "wait", "ms": 1}
            ]
        }),
    );
    let sc = resp["result"]["structuredContent"].clone();
    let results = sc["results"].as_array().expect("results");
    assert_eq!(
        results[0]["code"].as_str(),
        Some("no_clipboard_policy"),
        "unflagged clipboard_set must be typed, got: {sc}"
    );
    assert_eq!(results[0]["ok"].as_bool(), Some(false));
    assert!(
        results[0]["error"]
            .as_str()
            .unwrap_or("")
            .contains("background-only policy"),
        "refusal must name the rule, got: {sc}"
    );
    assert_eq!(
        results[1]["ok"].as_bool(),
        Some(true),
        "continue-always: the next action must still run, got: {sc}"
    );
    println!("NO_STEAL_CLIPBOARD_OK code=no_clipboard_policy burst_continued");
}

#[test]
fn window_focus_is_the_only_explicit_focus_and_reports_telemetry() {
    let mut s = Session::spawn();
    let resp = s.call(
        14,
        "window",
        serde_json::json!({"action": "close", "target": {"hwnd": 1}}),
    );
    let text = error_text(&resp);
    assert!(
        text.contains("target_gone")
            || resp["result"]["structuredContent"]["ok"].as_bool() == Some(false),
        "a dead window verb must refuse, got: {resp}"
    );
    println!("NO_STEAL_WINDOW_OK {text}");
}

#[test]
fn focus_is_refused_with_or_without_the_flag() {
    let mut s = Session::spawn();
    let resp = s.call(20, "list_windows", serde_json::json!({}));
    let windows = resp["result"]["structuredContent"]["windows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let own_pid = std::process::id();
    let Some(w) = windows.iter().find(|w| {
        w["pid"].as_u64().map(|p| p as u32) != Some(own_pid)
            && w["title"].as_str().map(|t| !t.trim().is_empty()) == Some(true)
            && w["minimized"].as_bool() == Some(false)
    }) else {
        println!("SKIP focus-without-flag: no visible window to name");
        return;
    };
    let hwnd = w["hwnd"].as_i64().expect("hwnd");
    let fg_before = foregound_now();
    for (id, flag) in [(21, false), (22, true)] {
        let resp = s.call(
            id,
            "window",
            serde_json::json!({"action": "focus", "target": {"hwnd": hwnd}, "allow_foreground": flag}),
        );
        let sc = resp["result"]["structuredContent"].clone();
        assert_eq!(
            sc["ok"].as_bool(),
            Some(false),
            "focus (flag={flag}) must fail: {sc}"
        );
        assert_eq!(
            sc["code"].as_str(),
            Some("no_foreground_policy"),
            "focus (flag={flag}) must be typed: {sc}"
        );
    }
    assert_eq!(
        foregound_now(),
        fg_before,
        "a refused focus must not activate anything"
    );
    println!("NO_STEAL_FOCUS_FLAG_OK hwnd={hwnd} fg={fg_before}");
}


fn live() -> bool {
    if std::env::var("PUPPET_LIVE_DESKTOP").as_deref() == Ok("1") {
        true
    } else {
        println!("SKIP no_steal live check: set PUPPET_LIVE_DESKTOP=1 (needs an interactive Windows session)");
        false
    }
}

#[test]
fn live_read_only_target_capture_keeps_foreground() {
    if !live() {
        return;
    }
    let mut s = Session::spawn();
    let resp = s.call(20, "list_windows", serde_json::json!({}));
    let sc = resp["result"]["structuredContent"]["windows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let own_pid = std::process::id();
    let mut cands: Vec<Value> = sc
        .iter()
        .filter(|w| {
            w["pid"].as_u64().map(|p| p as u32) != Some(own_pid)
                && w["title"].as_str().map(|t| !t.trim().is_empty()) == Some(true)
                && w["minimized"].as_bool() == Some(false)
                && w["cloaked"].as_i64() == Some(0)
        })
        .cloned()
        .collect();
    cands.sort_by_key(|w| w["foreground"].as_bool() == Some(true));
    cands.truncate(6);
    if cands.is_empty() {
        panic!(
            "NO_STEAL_LIVE_FAILED: live mode is on but no safe visible window exists to capture \
             (the capture contract is unproven)"
        );
    }
    let mut fg_baseline = foregound_now();
    let tw = ToolWindows {
        own_pid,
        server_pid: s.child.id(),
        targets: cands
            .iter()
            .filter_map(|w| w["hwnd"].as_i64())
            .collect::<Vec<i64>>(),
    };
    let cur_before = cursor_now();
    let mut refusals: Vec<String> = Vec::new();
    for (k, w) in cands.iter().enumerate() {
        let hwnd = w["hwnd"].as_i64().expect("hwnd");
        let el = s.call(
            30 + k as i64,
            "elements",
            serde_json::json!({"target": {"hwnd": hwnd}}),
        );
        let el_sc = el["result"]["structuredContent"].clone();
        assert!(
            el_sc["backend"].is_string(),
            "elements must serve a target window, got: {el}"
        );
        if el_sc["foregroundUnchanged"].as_bool() != Some(true) {
            fg_baseline = assert_foreground_held(fg_baseline, &tw, "elements telemetry");
        }
        let shot = s.call(
            40 + k as i64,
            "screenshot",
            serde_json::json!({"target": {"hwnd": hwnd}, "max_px": 480, "fmt": "png"}),
        );
        if shot["result"]["isError"].as_bool() == Some(true) {
            refusals.push(error_text(&shot));
            continue;
        }
        let shot_sc = shot["result"]["structuredContent"].clone();
        if shot_sc["foregroundUnchanged"].as_bool() != Some(true) {
            fg_baseline = assert_foreground_held(fg_baseline, &tw, "window capture telemetry");
        }
        let _ = shot_sc["cursorUnchanged"].as_bool();
        report_pointer(cur_before, "window capture");
        fg_baseline = assert_foreground_held(fg_baseline, &tw, "window capture");
        println!(
            "NO_STEAL_LIVE_OK hwnd={hwnd} exe={} title={:?} fg={fg_baseline}",
            w["exe"].as_str().unwrap_or("?"),
            w["title"].as_str().unwrap_or("")
        );
        return;
    }
    panic!(
        "NO_STEAL_LIVE_FAILED: none of {} existing windows was captured without activation \
         (elements succeeded read-only, but the capture half of the contract is unproven). \
         Reasons: {refusals:#?}",
        cands.len()
    );
}

#[link(name = "user32")]
extern "system" {
    fn GetForegroundWindow() -> *mut core::ffi::c_void;
    fn GetCursorPos(pt: *mut CursorPoint) -> i32;
    fn GetWindowThreadProcessId(hwnd: *mut core::ffi::c_void, pid: *mut u32) -> u32;
    fn GetClassNameW(hwnd: *mut core::ffi::c_void, buf: *mut u16, max: i32) -> i32;
    fn GetWindowTextW(hwnd: *mut core::ffi::c_void, buf: *mut u16, max: i32) -> i32;
    fn IsChild(parent: *mut core::ffi::c_void, child: *mut core::ffi::c_void) -> i32;
}

#[repr(C)]
struct CursorPoint {
    x: i32,
    y: i32,
}

fn foregound_now() -> i64 {
    unsafe { GetForegroundWindow() as i64 }
}

fn cursor_now() -> (i32, i32) {
    let mut p = CursorPoint { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut p) } != 0 {
        (p.x, p.y)
    } else {
        (-1, -1)
    }
}

fn pid_of(hwnd: i64) -> Option<u32> {
    if hwnd == 0 {
        return None;
    }
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd as _, &mut pid) };
    if pid == 0 {
        None
    } else {
        Some(pid)
    }
}

fn window_class(hwnd: i64) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd as _, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn window_text(hwnd: i64) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(hwnd as _, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}


const FG_SAMPLES: usize = 5;
const FG_SAMPLE_GAP: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FgOutcome {
    NoSession,
    Held,
    HarnessSteal,
    External,
}

fn fg_outcome(before: i64, after: i64, attributable: bool) -> FgOutcome {
    if before == 0 || after == 0 {
        FgOutcome::NoSession
    } else if after == before {
        FgOutcome::Held
    } else if attributable {
        FgOutcome::HarnessSteal
    } else {
        FgOutcome::External
    }
}

/// The windows this live check must hold the tool answerable for.
///
/// The tool runs in its OWN process (`Session::spawn`), so unlike the burst
/// fixtures a window it activates is not owned by this test's PID — the burst
/// PID rule alone would call that a third party and let a real steal through.
/// Attribution here is therefore the union of:
///
/// * windows of the server process under test (anything it created),
/// * the candidate target the call was pointed at, and that target's
///   descendants (a capture that raises its target lands exactly there),
/// * windows of this test process (a sibling live test's window).
///
/// Everything else — the owner's apps, shell churn, another test binary — is a
/// third party and is never blamed on the tool.
struct ToolWindows {
    own_pid: u32,
    server_pid: u32,
    targets: Vec<i64>,
}

impl ToolWindows {
    fn is_tool(&self, hwnd: i64) -> bool {
        if hwnd == 0 {
            return false;
        }
        if let Some(pid) = pid_of(hwnd) {
            if pid == self.own_pid || pid == self.server_pid {
                return true;
            }
        }
        self.targets
            .iter()
            .any(|t| *t != 0 && (*t == hwnd || unsafe { IsChild(*t as _, hwnd as _) } != 0))
    }

    fn describe(&self, hwnd: i64) -> String {
        if hwnd == 0 {
            return "hwnd=0 (no foreground window)".to_string();
        }
        format!(
            "hwnd={hwnd} (0x{hwnd:x}) pid={} class={:?} title={:?} owner={}",
            pid_of(hwnd).map_or_else(|| "?".to_string(), |p| p.to_string()),
            window_class(hwnd),
            window_text(hwnd),
            if self.is_tool(hwnd) {
                "tool"
            } else {
                "third-party"
            },
        )
    }
}

fn assert_foreground_held(before: i64, tw: &ToolWindows, what: &str) -> i64 {
    assert_foreground_held_with(before, tw, what, foregound_now)
}

fn assert_foreground_held_with(
    before: i64,
    tw: &ToolWindows,
    what: &str,
    mut sample: impl FnMut() -> i64,
) -> i64 {
    if before == 0 {
        println!("FG_SKIP: {what}: no foreground session (before=0); assertion skipped");
        return 0;
    }
    let first = before;
    let mut baseline = before;
    let mut external = 0usize;
    for attempt in 0..FG_SAMPLES {
        let after = sample();
        match fg_outcome(baseline, after, tw.is_tool(after)) {
            FgOutcome::NoSession => {
                println!(
                    "FG_SKIP_SESSION: {what}: foreground session vanished mid-observation \
                     (first={}); nothing left to protect",
                    tw.describe(first)
                );
                return after;
            }
            FgOutcome::Held => {
                println!(
                    "FG_HELD: {what}: sample {attempt}/{FG_SAMPLES} unchanged on {}; \
                     nothing attributable took the foreground",
                    tw.describe(baseline)
                );
                return baseline;
            }
            FgOutcome::HarnessSteal => panic!(
                "NO_STEAL_FAILED: {what}: the tool made an attributable window foreground \
                 (sample {attempt}/{FG_SAMPLES}, first={}) — before={} after={}",
                tw.describe(first),
                tw.describe(baseline),
                tw.describe(after),
            ),
            FgOutcome::External => {
                external += 1;
                println!(
                    "FG_EXTERNAL: {what}: third-party focus change #{external} \
                     (sample {attempt}/{FG_SAMPLES}); re-baselining, not attributed to the tool. \
                     before={} after={}",
                    tw.describe(baseline),
                    tw.describe(after),
                );
                baseline = after;
            }
        }
        if attempt + 1 < FG_SAMPLES {
            std::thread::sleep(FG_SAMPLE_GAP);
        }
    }
    let last = sample();
    assert!(
        !tw.is_tool(last),
        "NO_STEAL_FAILED: {what}: attributable foreground after {FG_SAMPLES} samples \
         ({external} third-party change(s)): first={} baseline={} last={}",
        tw.describe(first),
        tw.describe(baseline),
        tw.describe(last),
    );
    println!(
        "FG_EXTERNAL_SETTLED: {what}: {external} third-party change(s); final foreground is \
         not attributable: first={} baseline={} last={}",
        tw.describe(first),
        tw.describe(baseline),
        tw.describe(last),
    );
    last
}

fn scripted(samples: &[i64]) -> impl FnMut() -> i64 + '_ {
    let mut i = 0usize;
    move || {
        let v = samples
            .get(i)
            .copied()
            .unwrap_or_else(|| *samples.last().expect("non-empty script"));
        i += 1;
        v
    }
}

fn panic_text(e: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = e.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = e.downcast_ref::<&str>() {
        (*s).to_string()
    } else {
        String::new()
    }
}

#[test]
fn foreground_gate_classifies_by_tool_ownership_not_by_change() {
    let tw = ToolWindows {
        own_pid: 0x00aa_aaaa,
        server_pid: 0x00bb_bbbb,
        targets: vec![0x1000, 0x2000],
    };
    assert!(
        tw.is_tool(0x1000),
        "a candidate target is the tool's to answer for"
    );
    assert!(tw.is_tool(0x2000), "every candidate target counts");
    assert!(!tw.is_tool(0x3000), "an unrelated handle is a third party");
    assert!(!tw.is_tool(0), "no foreground window is nobody's");

    let e = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_foreground_held_with(0x5000, &tw, "scripted tool steal", scripted(&[0x1000]));
    }))
    .expect_err("a foreground sample the tool answers for must fail");
    let text = panic_text(e);
    assert!(text.contains("NO_STEAL_FAILED"), "typed failure: {text}");
    assert!(text.contains("owner=tool"), "the owner is named: {text}");

    let before = 0x5000;
    let settled =
        assert_foreground_held_with(before, &tw, "scripted churn", scripted(&[0x6000; 6]));
    assert_eq!(
        settled, 0x6000,
        "the third-party window becomes the baseline instead of failing"
    );

    let e = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_foreground_held_with(
            before,
            &tw,
            "scripted masked steal",
            scripted(&[0x6000, 0x1000]),
        );
    }))
    .expect_err("a tool window after third-party churn must still fail");
    assert!(panic_text(e).contains("NO_STEAL_FAILED"));

    assert_eq!(
        assert_foreground_held(0, &tw, "scripted headless"),
        0,
        "no session before the op means nothing to protect"
    );

    println!(
        "NO_STEAL_FG_CLASSIFY_OK=owned-fails,third-party-rebaselines,no-laundering,headless-skips"
    );
}

#[test]
fn foreground_gate_matches_the_canonical_burst_gate() {
    let src = std::fs::read_to_string(src_dir().join("fixture.rs"))
        .expect("src/fixture.rs must be readable")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>();
    for needle in [
        format!("pubconstFG_SAMPLES:usize={FG_SAMPLES};"),
        format!(
            "FG_SAMPLE_GAP:std::time::Duration=std::time::Duration::from_millis({});",
            FG_SAMPLE_GAP.as_millis()
        ),
        "pubfnfg_outcome(before:i64,after:i64,owned:bool)->FgOutcome".to_string(),
        "ifbefore==0||after==0{FgOutcome::NoSession}elseifafter==before{FgOutcome::Held}elseifowned{FgOutcome::HarnessSteal}else{FgOutcome::External}".to_string(),
    ] {
        assert!(
            src.contains(&needle),
            "tests/no_steal.rs restates the burst foreground gate; src/fixture.rs no longer contains {needle:?} — update the copy or delete it"
        );
    }
    println!(
        "NO_STEAL_FG_CANONICAL_OK samples={FG_SAMPLES} gap_ms={} matrix=identical",
        FG_SAMPLE_GAP.as_millis()
    );
}

fn report_pointer(before: (i32, i32), what: &str) {
    let after = cursor_now();
    if after == before {
        return;
    }
    println!(
        "CURSOR_EXTERNAL: {what}: pointer moved before={before:?} after={after:?} delta=({}, {}); telemetry only (no cursor API on this path)",
        after.0 - before.0,
        after.1 - before.1
    );
}
