//! W6.2 fixture capabilities: the live fixture can now prove "ignored",
//! `routed_to`, double-click spacing and `settle_ms` ordering.
//!
//! The live tests are gated by PUPPET_LIVE_DESKTOP=1 and drive only the
//! dedicated fixture app (`src/bin/puppet_fixture_app` in `caps` mode): a
//! window with a frame hosting a child button at a known client rect, a
//! message-swallowing control, a checkbox, a listbox, a grid, a tree, a
//! trackbar, and a stamped message log readable through `elements`
//! detail:text. No owner data, no input beyond the fixture window itself.
//!
//! These tests fail on the pre-W6.2 tree (the `caps` mode, its controls and
//! its log do not exist) and pass after. Existing fixture modes are
//! untouched, so every other live suite keeps its exact window.

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
        println!("SKIP fixture_caps: set PUPPET_LIVE_DESKTOP=1 to run (needs an interactive Windows session)");
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
    /// Spawn the fixture app in the W6.2 `caps` mode.
    fn spawn_caps(title: &str) -> Self {
        let exe = env!("CARGO_BIN_EXE_puppet_fixture_app");
        let child = Command::new(exe)
            .arg(title)
            .arg("caps")
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
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn sc_of(r: &Value, what: &str) -> Value {
    r.get("structuredContent")
        .cloned()
        .unwrap_or_else(|| panic!("{what} must carry structuredContent, got: {r}"))
}

fn elements_detail(s: &mut Session, id: i64, title: &str) -> Vec<Value> {
    let r = s.result(
        id,
        "elements",
        serde_json::json!({"title": title, "detail": true}),
    );
    let sc = sc_of(&r, "elements detail");
    sc.get("elements")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn find_name<'a>(els: &'a [Value], name: &str) -> &'a Value {
    els.iter()
        .find(|e| e.get("name").and_then(|v| v.as_str()) == Some(name))
        .unwrap_or_else(|| panic!("elements must list name:{name}, got: {els:#?}"))
}

fn find_auto<'a>(els: &'a [Value], auto_id: &str) -> &'a Value {
    els.iter()
        .find(|e| e.get("auto_id").and_then(|v| v.as_str()) == Some(auto_id))
        .unwrap_or_else(|| panic!("elements must list auto_id:{auto_id}"))
}

/// The fixture's stamped message log, read the same way an agent would:
/// `elements` detail:text on the log control (auto_id 4000).
fn log_text(s: &mut Session, id: i64, title: &str) -> String {
    let els = elements_detail(s, id, title);
    let log = find_auto(&els, "4000");
    log.get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("log control must carry readable text, got: {log}"))
        // RichEdit stores line breaks as CR; normalize so the log parses
        // like any other text.
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// Wait for the fixture's own readiness signal.
///
/// A freshly spawned fixture window reads all-black until its first `WM_PAINT`
/// has been composited, and the capture refuses that frame; its children also
/// appear in the UIA tree a moment later. The caps window writes `painted`
/// into its stamped log on that first paint, so the log is evidence rather
/// than a guess: a fixed sleep is how this suite flaked once mid-run.
fn wait_painted(s: &mut Session, title: &str) {
    static NEXT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(900);
    let t0 = std::time::Instant::now();
    let mut last = String::from("no elements answer");
    while t0.elapsed() < Duration::from_secs(15) {
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let r = s.result(
            id,
            "elements",
            serde_json::json!({"title": title, "detail": true}),
        );
        let text = r
            .get("structuredContent")
            .and_then(|sc| sc.get("elements"))
            .and_then(|e| e.as_array())
            .and_then(|els| {
                els.iter()
                    .find(|e| e.get("auto_id").and_then(|v| v.as_str()) == Some("4000"))
                    .and_then(|log| log.get("text"))
                    .and_then(|t| t.as_str())
            })
            .map(|t| t.to_string());
        match text {
            Some(t) if t.contains("painted") => return,
            Some(t) => last = t,
            None => last = format!("elements did not serve the log yet: {r}"),
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("the caps fixture never reported a painted frame: {last}");
}

/// One `layout <name> cx=<n> cy=<n> w=<n> h=<n>` line the fixture writes at/// boot, in the top-level's client coordinates — the "known client rect" a
/// coordinate click can aim at.
fn layout_of(log: &str, name: &str) -> (i64, i64, i64, i64) {
    let key = format!("layout {name} ");
    let line = log
        .lines()
        .find(|l| l.contains(&key))
        .unwrap_or_else(|| panic!("log must carry a {key:?} line, got: {log:?}"));
    let num = |field: &str| -> i64 {
        line.split_whitespace()
            .find_map(|t| t.strip_prefix(field))
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("{line:?} must carry {field}<n>"))
    };
    (num("cx="), num("cy="), num("w="), num("h="))
}

fn centre(layout: (i64, i64, i64, i64)) -> (i64, i64) {
    let (cx, cy, w, h) = layout;
    (cx + w / 2, cy + h / 2)
}

fn click_at(s: &mut Session, id: i64, title: &str, x: i64, y: i64) -> Value {
    let r = s.result(
        id,
        "do",
        serde_json::json!({
            "actions": [{"type": "click", "x": x, "y": y}],
            "target": {"titleContains": title},
        }),
    );
    let sc = sc_of(&r, "do");
    sc.get("results")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or_else(|| panic!("do must return one result, got: {sc}"))
}

fn msg_count(log: &str, msg: &str) -> usize {
    log.lines().filter(|l| l.contains(msg)).count()
}

/// Every capability SPEC.md W6.2 requires, in one window: the frame's child
/// button, the swallowing control, the checkbox, the listbox, the grid, the
/// tree/trackbar the state object reads, and the stamped log.
#[test]
fn caps_fixture_exposes_every_control_and_a_stamped_log() {
    if !live() {
        return;
    }
    kill_stale_fixtures("caps-controls");
    let fx = Fixture::spawn_caps("caps-controls");
    let title = fx.title.clone();
    let mut s = Session::spawn();
    // The fixture reports when its first frame exists; the 600 ms sleep this
    // replaces was a guess that lost once mid-run.
    wait_painted(&mut s, &title);

    let els = elements_detail(&mut s, 1, &title);
    // Controls whose UIA name is their window text.
    for name in [
        "dc-frame-btn",
        "dc-swallow",
        "dc-toggle",
        "dc-item-1",
        "p4c-root",
    ] {
        let e = find_name(&els, name);
        assert!(
            e.get("fp").and_then(|v| v.as_str()).is_some(),
            "every fixture control must carry an fp, got: {e}"
        );
    }
    // Controls with no accessible name (listbox, grid, log) and the
    // trackbar are addressable by auto_id, which is how the later items
    // select them.
    for auto_id in ["4000", "4002", "4003", "4006"] {
        find_auto(&els, auto_id);
    }

    let log = log_text(&mut s, 2, &title);
    assert!(
        log.contains("boot"),
        "the log must open with a boot line, got: {log:?}"
    );
    // Known client rects for the coordinate clicks the later items need.
    for name in ["dc-frame-btn", "dc-swallow", "dc-toggle"] {
        let (cx, cy, w, h) = layout_of(&log, name);
        assert!(
            w > 0 && h > 0,
            "{name} must have a non-empty client rect, got: {cx},{cy} {w}x{h}"
        );
    }
    drop(fx);
    println!("CAPS_OK every W6.2 control and the stamped log are present");
}

/// The two observations W4.1 and W4.2 need: a posted click at a known client
/// point is recorded with its coordinates, and neither the frame's button nor
/// the swallowing control activates.
#[test]
fn caps_fixture_records_posted_clicks_and_never_activates() {
    if !live() {
        return;
    }
    kill_stale_fixtures("caps-posted");
    let fx = Fixture::spawn_caps("caps-posted");
    let title = fx.title.clone();
    let mut s = Session::spawn();
    // The fixture reports when its first frame exists; the 600 ms sleep this
    // replaces was a guess that lost once mid-run.
    wait_painted(&mut s, &title);

    let log = log_text(&mut s, 10, &title);
    let (bx, by) = centre(layout_of(&log, "dc-frame-btn"));
    let (sx, sy) = centre(layout_of(&log, "dc-swallow"));

    let r = click_at(&mut s, 11, &title, bx, by);
    assert_eq!(
        r.get("delivered").and_then(|v| v.as_str()),
        Some("posted"),
        "a coordinate click is posted, got: {r}"
    );
    let log = log_text(&mut s, 12, &title);
    assert!(
        log.contains(&format!("WM_LBUTTONDOWN x={bx} y={by}")),
        "the frame button's click must be logged at its client point, got: {log:?}"
    );
    assert!(
        log.contains(&format!("WM_LBUTTONUP x={bx} y={by}")),
        "the matching button-up must be logged, got: {log:?}"
    );
    assert_eq!(
        msg_count(&log, "activate dc-frame-btn"),
        0,
        "a posted click must not activate the child button, got: {log:?}"
    );

    click_at(&mut s, 13, &title, sx, sy);
    let log = log_text(&mut s, 14, &title);
    assert!(
        log.contains(&format!("WM_LBUTTONDOWN x={sx} y={sy}")),
        "the swallowing control's click must be logged, got: {log:?}"
    );
    assert_eq!(
        msg_count(&log, "activate dc-swallow"),
        0,
        "the swallowing control must never activate, got: {log:?}"
    );
    drop(fx);
    println!("CAPS_POSTED_OK posted clicks are stamped and nothing activates");
}

/// The two timing observations W4.4 and W4.5 need: both clicks of a burst
/// land before the trailing `settle_ms` pause, and a `count:2` click leaves
/// two stamped button-downs whose gap is computable from the log.
#[test]
fn caps_fixture_log_orders_a_burst_before_the_settle_pause() {
    if !live() {
        return;
    }
    kill_stale_fixtures("caps-timing");
    let fx = Fixture::spawn_caps("caps-timing");
    let title = fx.title.clone();
    let mut s = Session::spawn();
    // The fixture reports when its first frame exists; the 600 ms sleep this
    // replaces was a guess that lost once mid-run.
    wait_painted(&mut s, &title);

    let log = log_text(&mut s, 20, &title);
    let (tx, ty) = centre(layout_of(&log, "dc-toggle"));

    let r = s.result(
        21,
        "do",
        serde_json::json!({
            "actions": [
                {"type": "click", "x": tx, "y": ty},
                {"type": "click", "x": tx, "y": ty},
            ],
            "target": {"titleContains": title},
            "settle_ms": 200,
        }),
    );
    let sc = sc_of(&r, "do");
    let total_ms = sc.get("total_ms").and_then(|v| v.as_f64()).unwrap_or(0.0);
    assert!(
        total_ms >= 200.0,
        "settle_ms must be a trailing pause of at least 200 ms, got: {sc}"
    );
    let log = log_text(&mut s, 22, &title);
    let downs: Vec<i64> = log
        .lines()
        .filter(|l| l.contains("WM_LBUTTONDOWN"))
        .filter_map(|l| {
            l.split_whitespace()
                .find_map(|t| t.strip_prefix("t="))
                .and_then(|v| v.parse().ok())
        })
        .collect();
    assert_eq!(
        downs.len(),
        2,
        "both clicks of the burst must be logged, got: {log:?}"
    );
    assert!(
        downs[1] - downs[0] < 100,
        "both clicks must land before the 200 ms trailing pause, got stamps {downs:?}"
    );

    // `count:2` in one action: two stamped downs, so the gap between them is
    // observable (the W4.5 assertion reads exactly this).
    let before = downs.len();
    let r = s.result(
        23,
        "do",
        serde_json::json!({
            "actions": [{"type": "click", "x": tx, "y": ty, "count": 2}],
            "target": {"titleContains": title},
        }),
    );
    sc_of(&r, "do");
    let log = log_text(&mut s, 24, &title);
    let downs: Vec<i64> = log
        .lines()
        .filter(|l| l.contains("WM_LBUTTONDOWN"))
        .filter_map(|l| {
            l.split_whitespace()
                .find_map(|t| t.strip_prefix("t="))
                .and_then(|v| v.parse().ok())
        })
        .collect();
    assert_eq!(
        downs.len(),
        before + 2,
        "a count:2 click must leave two stamped button-downs, got: {log:?}"
    );
    assert!(
        downs[before + 1] >= downs[before],
        "the log stamps must be monotonic, got: {downs:?}"
    );
    drop(fx);
    println!("CAPS_TIMING_OK burst ordering and click spacing are observable");
}
