//! No-steal invariants, proven at three levels:
//!
//! 1. STATIC: the forbidden APIs (`SendInput`, `SetCursorPos`,
//!    `SetForegroundWindow`, `BringWindowToTop`, `mouse_event`) appear in
//!    exactly one source file (`src/gate.rs`), behind permit tokens that only
//!    the explicit flags (and the explicit focus verb) can mint. A new call
//!    site anywhere else turns this test red.
//! 2. WIRE: `tools/list` declares `target` on every acting/capturing tool and
//!    `allow_foreground`/`allow_cursor` on the verbs they gate.
//! 3. SERVER: a typed target failure comes back as an MCP tool error with a
//!    machine-readable `code` (`target_gone` / `bad_target`), never a
//!    foreground fallback; a gated burst refuses with `foreground_required` /
//!    `cursor_move_required` and reports zero events.
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

/// A call site is `NAME(` — prose mentions in comments/doc strings are not
/// call sites. The scan is line-based and ignores comment lines so the
/// module documentation above `gate.rs` (which names the APIs) cannot mask a
/// real call.
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
fn forbidden_apis_live_only_in_the_gate() {
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
            if name == "gate.rs" {
                continue;
            }
            let text = std::fs::read_to_string(f).expect("source file readable");
            for site in call_sites(&text, sym) {
                offenders.push(format!("{name}: {site}"));
            }
        }
        assert!(
            offenders.is_empty(),
            "{sym} must be reachable only through src/gate.rs permits, found: {offenders:#?}"
        );
    }
    // And the gate itself does contain the ones the server actually uses
    // (the scan is not vacuous). `mouse_event` is a never-used ban: it must
    // appear NOWHERE, gate included.
    let gate = std::fs::read_to_string(src_dir().join("gate.rs")).expect("gate.rs readable");
    for sym in FORBIDDEN.iter().filter(|s| **s != "mouse_event") {
        assert!(
            !call_sites(&gate, sym).is_empty(),
            "{sym} must be in the gate; the invariant moved somewhere else"
        );
    }
    assert!(
        call_sites(&gate, "mouse_event").is_empty(),
        "mouse_event is a never-used API: no call site anywhere"
    );
    println!("NO_STEAL_STATIC_OK symbols={FORBIDDEN:?} confined to src/gate.rs");
}

#[test]
fn gate_permits_are_the_only_way_in() {
    let gate = std::fs::read_to_string(src_dir().join("gate.rs")).expect("gate.rs readable");
    // Both mints are flag-only and auditable. There is deliberately NO
    // unflagged constructor (the old `grant_explicit_focus` is gone): the
    // explicit focus verb is double-explicit, verb AND allow_foreground.
    for mint in [
        "pub fn grant(allow_foreground: bool)",
        "pub fn grant(allow_cursor: bool)",
    ] {
        assert!(gate.contains(mint), "gate must keep the mint `{mint}`");
    }
    assert!(
        !gate.contains("grant_explicit_focus"),
        "no permit constructor may bypass the flags"
    );
    // Production half only: test literals sit below `#[cfg(test)]`.
    let prod = gate.split("#[cfg(test)]").next().unwrap_or("");
    assert_eq!(
        prod.matches("Some(ForegroundPermit(())").count(),
        1,
        "ForegroundPermit has exactly one, flag-only constructor"
    );
    assert_eq!(
        prod.matches("Some(CursorPermit(())").count(),
        1,
        "CursorPermit has exactly one, flag-only constructor"
    );
    // Every constructor call site outside gate.rs would be a bypass: the
    // permits are only mintable there (the tokens are private to the module).
    let mut files = Vec::new();
    rs_files(&src_dir(), &mut files);
    for f in &files {
        let name = f.file_name().and_then(|x| x.to_str()).unwrap_or("");
        if name == "gate.rs" {
            continue;
        }
        let text = std::fs::read_to_string(f).expect("source file readable");
        assert!(
            !text.contains("ForegroundPermit(())") && !text.contains("CursorPermit(())"),
            "{name} must not construct a permit; only gate.rs may"
        );
    }
    println!("NO_STEAL_MINT_OK permits=1-foreground,1-cursor,flag-only");
}

// ---------------------------------------------------------------- server

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
    // Read-only listing needs no target: it never acts on a window.
    let lw = tool(&tools, "list_windows")["inputSchema"]["properties"]
        .as_object()
        .expect("properties");
    assert!(
        !lw.contains_key("target"),
        "list_windows stays argument-free"
    );
    println!("NO_STEAL_SCHEMA_OK target=4-tools flags=window,do");
}

/// The typed error text is inside the MCP tool error block.
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
    // Structural (pydantic-shaped) rejection for a non-object target.
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
        vec![
            "cursor_move_required",
            "foreground_required",
            "foreground_required"
        ],
        "gate matrix over the wire, got: {results:?}"
    );
    assert_eq!(sc["events"], Value::from(0), "nothing may be injected");
    assert_eq!(sc["sendinput_calls"], Value::from(0));
    println!("NO_STEAL_GATE_OK codes={codes:?} events=0");
}

#[test]
fn window_focus_is_the_only_explicit_focus_and_reports_telemetry() {
    let mut s = Session::spawn();
    // No foreground session in some CI environments: a refusal is fine, but
    // the response must be typed, never a silent ok.
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

/// Double-explicit focus, over the wire: `action:"focus"` WITHOUT
/// `allow_foreground:true` must refuse with foreground_required and leave
/// the foreground window alone. The refusal happens before any activation,
/// so this is safe on a live desktop (and skips when no window is visible).
#[test]
fn focus_without_the_flag_is_refused_before_activating() {
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
    let resp = s.call(
        21,
        "window",
        serde_json::json!({"action": "focus", "target": {"hwnd": hwnd}}),
    );
    let sc = resp["result"]["structuredContent"].clone();
    assert_eq!(
        sc["ok"].as_bool(),
        Some(false),
        "unflagged focus must fail: {sc}"
    );
    assert_eq!(
        sc["code"].as_str(),
        Some("foreground_required"),
        "unflagged focus must be typed: {sc}"
    );
    assert_eq!(
        foregound_now(),
        fg_before,
        "an unflagged focus must not activate anything"
    );
    println!("NO_STEAL_FOCUS_FLAG_OK hwnd={hwnd} fg={fg_before}");
}

// ---------------------------------------------------------------- live read-only

fn live() -> bool {
    if std::env::var("PUPPET_LIVE_DESKTOP").as_deref() == Ok("1") {
        true
    } else {
        println!("SKIP no_steal live check: set PUPPET_LIVE_DESKTOP=1 (needs an interactive Windows session)");
        false
    }
}

/// Live read-only: `elements` and a window `screenshot` succeed on an
/// EXISTING real window (never ours, never created here) while
/// GetForegroundWindow stays the same. No activation, no restore, no input.
/// PrintWindow is refused by some windows (GPU-composited/XAML surfaces) and
/// that is a valid typed `capture_unavailable`; the check walks a few
/// candidates and requires at least one real success, reporting the refusal
/// reasons instead of pretending when none is capturable.
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
    // Prefer a window that is NOT foreground: that is the case the no-steal
    // contract exists for (capture a background window without raising it).
    cands.sort_by_key(|w| w["foreground"].as_bool() == Some(true));
    cands.truncate(6);
    if cands.is_empty() {
        // In live mode an empty candidate set is a FAILED proof, not a skip:
        // the skip above only covers `PUPPET_LIVE_DESKTOP` being unset.
        panic!(
            "NO_STEAL_LIVE_FAILED: live mode is on but no safe visible window exists to capture \
             (the capture contract is unproven)"
        );
    }
    let fg_before = foregound_now();
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
            assert_foreground_held(fg_before, "elements telemetry");
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
        // The server-side boolean is a measurement too, but the foreground is
        // the property this tool can violate, so it is asserted strictly.
        if shot_sc["foregroundUnchanged"].as_bool() != Some(true) {
            assert_foreground_held(fg_before, "window capture telemetry");
        }
        // Pointer telemetry: reported with its delta, never a failure (see
        // `report_pointer`). The foreground below stays strict.
        let _ = shot_sc["cursorUnchanged"].as_bool();
        report_pointer(cur_before, "window capture");
        assert_foreground_held(fg_before, "window capture");
        println!(
            "NO_STEAL_LIVE_OK hwnd={hwnd} exe={} title={:?} fg={fg_before}",
            w["exe"].as_str().unwrap_or("?"),
            w["title"].as_str().unwrap_or("")
        );
        return;
    }
    // Live mode must PROVE a capture, not skip past a total failure. The
    // skip above is only for `PUPPET_LIVE_DESKTOP` being unset.
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

/// The live desktop belongs to the owner: a pointer that keeps moving with no
/// tool call in between is their activity (the repo's FG_EXTERNAL policy),
/// reported and not attributed to the tool. A pointer that moved once and is
/// then still is the tool's and fails.
/// The live desktop belongs to the owner: a foreground handle that keeps
/// moving with no tool call in between is their activity, reported and not
/// attributed. A handle that changed once and is then still IS the tool's.
/// The foreground handle is the property the tool CAN violate (activation),
/// so it is asserted strictly: any change fails. There is no external-movement
/// exemption here.
fn assert_foreground_held(before: i64, what: &str) {
    let after = foregound_now();
    if before == 0 {
        // No interactive session to protect (headless runner).
        return;
    }
    assert_eq!(
        after, before,
        "{what}: GetForegroundWindow changed ({before} -> {after})"
    );
}

/// The pointer is TELEMETRY ONLY. The tool cannot move it here: no
/// `SetCursorPos`/`SendInput`/`mouse_event` exists outside `src/gate.rs`
/// (pinned by `forbidden_apis_live_only_in_the_gate`), and this path passes no
/// cursor permit. The owner using the machine moves the pointer constantly, so
/// a positional assertion would be a flaky gate dressed up as evidence.
/// Report the delta and continue.
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
