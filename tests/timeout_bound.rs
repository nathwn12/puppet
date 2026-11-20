use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(30);
const UIA_DEADLINE_MS: u64 = 1500;
const CAPTURE_DEADLINE_MS: u64 = 800;
const STALL_MS: u64 = 4000;

fn live() -> bool {
    if std::env::var("PUPPET_LIVE_DESKTOP").as_deref() == Ok("1") {
        true
    } else {
        println!("SKIP timeout_bound: set PUPPET_LIVE_DESKTOP=1 to run (needs an interactive Windows session)");
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
        Self::spawn_with_stall(true)
    }

    fn spawn_with_stall(stall: bool) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_puppet"));
        cmd.env("PUPPET_UIA_TIMEOUT_MS", UIA_DEADLINE_MS.to_string())
            .env("PUPPET_CAPTURE_TIMEOUT_MS", CAPTURE_DEADLINE_MS.to_string());
        if stall {
            cmd.env("PUPPET_TEST_STALL_UIA_MS", STALL_MS.to_string())
                .env("PUPPET_TEST_STALL_CAPTURE_MS", STALL_MS.to_string());
        }
        let mut child = cmd
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

    fn timed_call(&mut self, id: i64, name: &str, arguments: Value) -> (Value, Duration) {
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        });
        let started = Instant::now();
        self.write(&frame);
        let raw = self
            .rx
            .recv_timeout(TIMEOUT)
            .unwrap_or_else(|e| panic!("no response to {name} within {TIMEOUT:?}: {e}"));
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

struct Fixture {
    child: Child,
    title: String,
}

impl Fixture {
    fn spawn(title: &str) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_puppet_fixture_app"))
            .arg(title)
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

fn sc_of(r: &Value) -> Value {
    r.get("structuredContent")
        .cloned()
        .unwrap_or_else(|| panic!("must carry structuredContent, got: {r}"))
}

fn raw_text(r: &Value) -> String {
    serde_json::to_string(r).unwrap_or_default()
}

#[test]
fn uia_walk_times_out_typed_and_the_worker_recovers() {
    if !live() {
        return;
    }
    let fx = Fixture::spawn("timeout-bound-uia");
    std::thread::sleep(Duration::from_millis(600));
    let title = fx.title.clone();
    let mut s = Session::spawn();

    let (red, elapsed) = s.timed_call(1, "elements", serde_json::json!({"title": title}));
    let sc = sc_of(&red);
    let uia_error = sc
        .get("uia_error")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("a UIA timeout must be reported on uia_error, got: {sc}"))
        .to_string();
    assert!(
        uia_error.starts_with("error: uia_timeout: uia_element_list did not return within 1500ms"),
        "typed code + deadline must name the operation, got: {uia_error}"
    );
    assert!(
        uia_error.contains("the target's UI thread is not responding"),
        "the message must name the cause, got: {uia_error}"
    );
    assert!(
        uia_error.contains("retry, or capture the desktop instead"),
        "the message must name the recovery, got: {uia_error}"
    );
    assert!(
        elapsed >= Duration::from_millis(UIA_DEADLINE_MS),
        "the call must wait for its deadline ({UIA_DEADLINE_MS}ms) before giving up, returned in {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(UIA_DEADLINE_MS + 2500),
        "a stalled walk must be cut at the deadline, not at the {STALL_MS}ms stall: {elapsed:?}"
    );
    assert_eq!(
        sc.get("fallback"),
        Some(&Value::from("win32")),
        "a timed-out UIA walk falls back to Win32, got: {sc}"
    );
    assert!(
        !sc.get("elements")
            .and_then(|v| v.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(true),
        "the Win32 fallback must still list elements, got: {sc}"
    );

    let (green, second_elapsed) = s.timed_call(2, "elements", serde_json::json!({"title": title}));
    let sc2 = sc_of(&green);
    assert_eq!(
        sc2.get("backend"),
        Some(&Value::from("uia")),
        "the worker must be usable after a timeout (UIA walk must succeed), got: {sc2}"
    );
    assert_eq!(
        sc2.get("uia_error"),
        Some(&Value::Null),
        "no residual timeout may be reported, got: {sc2}"
    );
    assert!(
        second_elapsed < Duration::from_millis(UIA_DEADLINE_MS),
        "the recovered walk must not wait for a deadline: {second_elapsed:?}"
    );
}

#[test]
fn capture_times_out_typed_and_the_worker_recovers() {
    if !live() {
        return;
    }
    let fx = Fixture::spawn("timeout-bound-capture");
    std::thread::sleep(Duration::from_millis(600));
    let title = fx.title.clone();
    let mut s = Session::spawn();

    let (red, elapsed) = s.timed_call(
        1,
        "screenshot",
        serde_json::json!({"window": title, "fmt": "png"}),
    );
    let text = raw_text(&red);
    assert!(
        text.contains("error: capture_timeout: capture_window did not return within 800ms"),
        "typed code + deadline must name the operation, got: {text}"
    );
    assert!(
        text.contains("the target's UI thread is not responding"),
        "the message must name the cause, got: {text}"
    );
    assert!(
        elapsed >= Duration::from_millis(CAPTURE_DEADLINE_MS),
        "the capture must wait for its deadline ({CAPTURE_DEADLINE_MS}ms) before giving up, returned in {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(CAPTURE_DEADLINE_MS + 2500),
        "a stalled capture must be cut at the deadline, not at the {STALL_MS}ms stall: {elapsed:?}"
    );

    let (green, second_elapsed) = s.timed_call(
        2,
        "screenshot",
        serde_json::json!({"window": title, "fmt": "png"}),
    );
    let text2 = raw_text(&green);
    assert!(
        !text2.contains("capture_timeout"),
        "the worker must be usable after a timeout, got: {text2}"
    );
    let sc = sc_of(&green);
    assert_eq!(
        sc.get("isError").unwrap_or(&Value::from(false)),
        &Value::from(false),
        "the capture after a timeout must succeed, got: {sc}"
    );
    assert!(
        second_elapsed < Duration::from_millis(CAPTURE_DEADLINE_MS),
        "the recovered capture must not wait for a deadline: {second_elapsed:?}"
    );
}

#[link(name = "kernel32")]
extern "system" {
    fn GetProcessHandleCount(process: *mut core::ffi::c_void, count: *mut u32) -> i32;
}

fn handle_count(child: &Child) -> i64 {
    use std::os::windows::io::AsRawHandle;
    let mut n: u32 = 0;
    let ok = unsafe { GetProcessHandleCount(child.as_raw_handle() as *mut _, &mut n) };
    assert_eq!(ok, 1, "GetProcessHandleCount failed for the child");
    i64::from(n)
}

#[test]
fn a_returned_helper_leaves_no_handle_behind() {
    if !live() {
        return;
    }
    let fx = Fixture::spawn("timeout-bound-handle");
    std::thread::sleep(Duration::from_millis(600));
    let title = fx.title.clone();

    fn retained(title: &str, stall: bool) -> i64 {
        let mut s = Session::spawn_with_stall(stall);
        let before = handle_count(&s.child);
        let (first, _) = s.timed_call(1, "elements", serde_json::json!({"title": title}));
        let first_text = raw_text(&first);
        if stall {
            assert!(
                first_text.contains("error: uia_timeout"),
                "the stall arm must time out, got: {first_text}"
            );
        } else {
            assert!(
                !first_text.contains("uia_timeout"),
                "the control arm must not time out, got: {first_text}"
            );
        }
        let (second, _) = s.timed_call(2, "elements", serde_json::json!({"title": title}));
        assert!(
            !raw_text(&second).contains("uia_timeout"),
            "the worker must be usable after the first walk, got: {}",
            raw_text(&second)
        );
        std::thread::sleep(Duration::from_millis(STALL_MS + 2700));
        handle_count(&s.child) - before
    }

    let timed_out = retained(&title, true);
    let control = retained(&title, false);
    println!("HANDLE_DIFFERENTIAL_OK stall={timed_out} control={control}");
    assert_eq!(
        timed_out, control,
        "a timeout that later returns must not retain a handle: the timed-out child \
         ended {timed_out} handles above its own baseline, the control arm {control}"
    );
}

fn src_text(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn fn_body_slice(text: &str, name: &str) -> String {
    let sig = format!("fn {name}(");
    let at = text
        .find(&sig)
        .unwrap_or_else(|| panic!("src no longer declares `{sig}`: update this audit"));
    text[at..(at + 1200).min(text.len())].to_string()
}

#[test]
fn every_com_entry_point_is_bounded() {
    let uia = src_text("uia.rs");
    for name in [
        "uia_element_list",
        "uia_activate",
        "uia_set_value",
        "uia_is_enabled",
        "uia_expand_collapse",
        "uia_expand_state",
        "uia_range_value",
        "uia_scroll",
        "uia_element_text",
    ] {
        let body = fn_body_slice(&uia, name);
        assert!(
            body.contains("deadline::bound("),
            "{name} must route through deadline::bound, its body starts: {}",
            body.lines().take(6).collect::<Vec<_>>().join(" | ")
        );
        assert!(
            body.contains(&format!("\"{name}\"")),
            "{name} must name its own operation in the deadline line"
        );
    }
    let shot = src_text("shot.rs");
    assert!(
        fn_body_slice(&shot, "capture_window").contains("deadline::bound("),
        "shot::capture_window must route through deadline::bound"
    );
    let hidden = src_text("hidden.rs");
    assert!(
        hidden.contains("crate::deadline::limit(crate::deadline::Family::Capture)"),
        "the hidden-desktop capture wait must use the Capture family deadline"
    );
    assert!(
        hidden.contains("crate::deadline::timeout_text("),
        "a stalled hidden-desktop capture must return the typed line"
    );
    println!("COM_ENTRY_POINTS_BOUNDED_OK uia=9 capture=1 hidden=1");
}
