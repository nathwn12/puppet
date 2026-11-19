
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(15);

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

    fn call(&mut self, id: i64, name: &str, arguments: Value) -> Value {
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        });
        let line = serde_json::to_string(&frame).unwrap();
        self.stdin.write_all(line.as_bytes()).expect("write frame");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush frame");
        self.rx
            .recv_timeout(TIMEOUT)
            .ok()
            .map(|l| serde_json::from_str(&l).expect("response is JSON"))
            .expect("response in time")
    }

    fn result(&mut self, id: i64, name: &str, arguments: Value) -> Value {
        let resp = self.call(id, name, arguments);
        resp.get("result")
            .cloned()
            .unwrap_or_else(|| panic!("tools/call {name} must succeed, got: {resp}"))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn list_tools() -> Vec<Value> {
    let mut s = Session::spawn();
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

#[test]
fn tools_list_every_tool_has_real_output_schema() {
    let tools = list_tools();
    assert_eq!(tools.len(), 7, "seven tools expected, got: {tools:?}");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    for want in [
        "screenshot",
        "diff",
        "list_windows",
        "window",
        "do",
        "cursor",
        "elements",
    ] {
        assert!(
            names.contains(&want),
            "tools/list must contain {want}, got: {names:?}"
        );
    }
    for t in &tools {
        let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let schema = t
            .get("outputSchema")
            .and_then(|s| s.as_object())
            .unwrap_or_else(|| panic!("{name} must carry an outputSchema object, got: {t}"));
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap_or_else(|| panic!("{name} outputSchema needs properties, got: {schema:?}"));
        assert!(
            !props.is_empty(),
            "{name} outputSchema.properties must be non-empty"
        );
        assert!(
            !serde_json::to_string(t)
                .unwrap()
                .contains("x-fastmcp-wrap-result"),
            "{name} must not carry x-fastmcp-wrap-result"
        );
    }
}

#[test]
fn list_windows_structured_is_real_object() {
    let mut s = Session::spawn();
    let r = s.result(2, "list_windows", serde_json::json!({}));
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(false)),
        "list_windows must succeed, got: {r}"
    );
    let sc = r
        .get("structuredContent")
        .expect("structuredContent present");
    assert!(
        sc.is_object(),
        "structuredContent must be an object, got: {sc}"
    );
    assert!(
        sc.get("result").is_none(),
        "double-encoded structuredContent.result must be gone, got: {sc}"
    );
    let wins = sc
        .get("windows")
        .and_then(|w| w.as_array())
        .expect("windows array");
    let text = r
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        .expect("text block");
    let arr: Value = serde_json::from_str(text).expect("text is a JSON array");
    assert_eq!(
        arr.as_array().map(|a| a.len()),
        Some(wins.len()),
        "text and structuredContent.windows must agree, got: {r}"
    );
}

#[test]
fn cursor_structured_has_xyz_screens() {
    let mut s = Session::spawn();
    let r = s.result(3, "cursor", serde_json::json!({}));
    assert_eq!(r.get("isError"), Some(&Value::from(false)));
    let sc = r.get("structuredContent").expect("structuredContent");
    assert!(sc.is_object(), "cursor structuredContent must be an object");
    assert!(
        sc.get("x").and_then(|v| v.as_i64()).is_some(),
        "cursor needs x, got: {sc}"
    );
    assert!(
        sc.get("y").and_then(|v| v.as_i64()).is_some(),
        "cursor needs y, got: {sc}"
    );
    assert!(
        sc.get("screens").and_then(|v| v.as_array()).is_some(),
        "cursor needs screens, got: {sc}"
    );
}

#[test]
fn screenshot_unknown_window_is_error_without_image() {
    let mut s = Session::spawn();
    let r = s.result(
        4,
        "screenshot",
        serde_json::json!({"window": "NoSuchWindowXYZ"}),
    );
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(true)),
        "unknown-window screenshot must be isError:true, got: {r}"
    );
    let content = r
        .get("content")
        .and_then(|c| c.as_array())
        .expect("content blocks");
    assert!(
        content
            .iter()
            .all(|b| b.get("type").and_then(|t| t.as_str()) != Some("image")),
        "error must carry no image block, got: {r}"
    );
    let text: String = content
        .iter()
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect();
    assert!(
        text.contains("error:"),
        "error text block unchanged, got: {text}"
    );
}

#[test]
fn window_missing_target_is_error() {
    let mut s = Session::spawn();
    let r = s.result(
        5,
        "window",
        serde_json::json!({"action": "focus", "title": "NoSuchWindowXYZ12345"}),
    );
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(true)),
        "missing-target window call must be isError:true, got: {r}"
    );
    let sc = r.get("structuredContent").expect("structuredContent");
    assert_eq!(
        sc.get("ok"),
        Some(&Value::from(false)),
        "structuredContent carries ok:false, got: {sc}"
    );
}

#[test]
fn window_unknown_action_is_error() {
    let mut s = Session::spawn();
    let lw = s.result(6, "list_windows", serde_json::json!({}));
    let hwnd = lw
        .get("structuredContent")
        .and_then(|sc| sc.get("windows"))
        .and_then(|w| w.as_array())
        .and_then(|a| a.first())
        .and_then(|w| w.get("hwnd"))
        .and_then(|h| h.as_i64());
    let args = match hwnd {
        Some(h) => serde_json::json!({"action": "frobnicate", "hwnd": h}),
        None => serde_json::json!({"action": "frobnicate", "title": "NoSuchWindowXYZ12345"}),
    };
    let r = s.result(7, "window", args);
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(true)),
        "unknown-action window call must be isError:true, got: {r}"
    );
    if hwnd.is_some() {
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
            text.contains("unknown action"),
            "unknown action named in text, got: {text}"
        );
    }
}

#[test]
fn do_all_failed_actions_is_a_tool_error() {
    let mut s = Session::spawn();
    let r = s.result(
        8,
        "do",
        serde_json::json!({"actions": [{"type": "nope_not_a_type"}], "dry_run": true}),
    );
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(true)),
        "a burst where nothing succeeded is a tool error, got: {r}"
    );
    let sc = r.get("structuredContent").expect("structuredContent");
    assert_eq!(
        sc.get("ok"),
        Some(&Value::from(false)),
        "top-level ok is false when an action failed, got: {sc}"
    );
    assert_eq!(
        sc.get("failed"),
        Some(&Value::from(1)),
        "failed counts the failed actions, got: {sc}"
    );
    let first = sc
        .get("results")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .expect("first per-action result");
    assert_eq!(first.get("ok"), Some(&Value::from(false)));
    let err = first.get("error").and_then(|e| e.as_str()).unwrap_or("");
    assert!(
        err.contains("unknown action type"),
        "per-action error typed, got: {err}"
    );
}

#[test]
fn do_partial_failure_stays_a_success_envelope() {
    let mut s = Session::spawn();
    let r = s.result(
        9,
        "do",
        serde_json::json!({
            "actions": [{"type": "nope_not_a_type"}, {"type": "wait", "ms": 1}],
            "dry_run": true,
        }),
    );
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(false)),
        "a partially successful burst is not a tool error, got: {r}"
    );
    let sc = r.get("structuredContent").expect("structuredContent");
    assert_eq!(sc.get("ok"), Some(&Value::from(false)));
    assert_eq!(sc.get("failed"), Some(&Value::from(1)));
    let results = sc
        .get("results")
        .and_then(|v| v.as_array())
        .expect("results");
    assert_eq!(results.len(), 2, "both actions reported: {sc}");
    assert_eq!(results[0].get("ok"), Some(&Value::from(false)));
    assert_eq!(
        results[1].get("ok"),
        Some(&Value::from(true)),
        "the action after a failure still ran, got: {sc}"
    );
}

fn kill_stale_conformance_fixtures(title: &str) {
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

struct ConformanceFixture {
    child: Child,
}

impl ConformanceFixture {
    fn spawn(title: &str) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_puppet_fixture_app"))
            .arg(title)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fixture app");
        std::thread::sleep(Duration::from_millis(500));
        ConformanceFixture { child }
    }
}

impl Drop for ConformanceFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn schema_allowed_types(prop: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut push_t = |t: &Value| match t {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => {
            for x in a {
                if let Some(s) = x.as_str() {
                    out.push(s.to_string());
                }
            }
        }
        _ => {}
    };
    if let Some(t) = prop.get("type") {
        push_t(t);
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(arr) = prop.get(key).and_then(|v| v.as_array()) {
            for variant in arr {
                if let Some(t) = variant.get("type") {
                    push_t(t);
                }
            }
        }
    }
    out
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn assert_conforms(
    schemas: &HashMap<String, Value>,
    coverage: &mut HashMap<String, usize>,
    tool: &str,
    envelope: &Value,
    what: &str,
) {
    *coverage.entry(tool.to_string()).or_insert(0) += 1;
    let schema = schemas
        .get(tool)
        .unwrap_or_else(|| panic!("{what}: tool {tool} missing from tools/list"));
    let sc = envelope
        .get("structuredContent")
        .unwrap_or_else(|| panic!("{what}: {tool} must carry structuredContent, got: {envelope}"));
    assert!(
        sc.is_object(),
        "{what}: {tool} structuredContent must be an object, got: {sc}"
    );
    let props = schema
        .get("properties")
        .and_then(|p| p.as_object())
        .unwrap_or_else(|| panic!("{what}: {tool} outputSchema needs properties"));
    let is_err = envelope.get("isError") == Some(&Value::from(true));
    if !is_err {
        let empty: Vec<Value> = Vec::new();
        let required = schema
            .get("required")
            .and_then(|r| r.as_array())
            .unwrap_or(&empty);
        for r in required {
            let name = r.as_str().unwrap_or("?");
            assert!(
                sc.get(name).is_some(),
                "{what}: {tool} success payload missing required `{name}`, got: {sc}"
            );
        }
    } else {
        assert_eq!(
            sc.get("ok"),
            Some(&Value::from(false)),
            "{what}: {tool} error envelope must carry ok:false, got: {sc}"
        );
    }
    for (k, v) in sc.as_object().expect("object") {
        let Some(prop) = props.get(k) else {
            continue;
        };
        let allowed = schema_allowed_types(prop);
        if allowed.is_empty() {
            continue;
        }
        let actual = json_type(v);
        let ok = allowed.iter().any(|a| a == actual)
            || (actual == "integer" && allowed.iter().any(|a| a == "number"));
        assert!(
            ok,
            "{what}: field `{k}` value {v} (type {actual}) not allowed by {tool} outputSchema {prop}"
        );
    }
}

#[test]
fn payload_conforms_to_own_output_schema() {
    let tools = list_tools();
    let schemas: HashMap<String, Value> = tools
        .iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?.to_string();
            let schema = t.get("outputSchema")?.clone();
            Some((name, schema))
        })
        .collect();
    assert_eq!(
        schemas.len(),
        tools.len(),
        "every listed tool must declare an outputSchema"
    );
    let mut coverage: HashMap<String, usize> = HashMap::new();
    let mut s = Session::spawn();
    let mut id: i64 = 100;
    let mut next_id = || {
        id += 1;
        id
    };

    let r = s.result(next_id(), "cursor", serde_json::json!({}));
    assert_eq!(r.get("isError"), Some(&Value::from(false)));
    assert_conforms(&schemas, &mut coverage, "cursor", &r, "cursor empty args");

    let r = s.result(next_id(), "list_windows", serde_json::json!({}));
    assert_eq!(r.get("isError"), Some(&Value::from(false)));
    assert_conforms(
        &schemas,
        &mut coverage,
        "list_windows",
        &r,
        "list_windows empty args",
    );

    let r = s.result(
        next_id(),
        "screenshot",
        serde_json::json!({"display": -1, "region": [0, 0, 64, 64]}),
    );
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(false)),
        "desktop-region screenshot must succeed, got: {r}"
    );
    assert_conforms(
        &schemas,
        &mut coverage,
        "screenshot",
        &r,
        "screenshot out null",
    );

    let dir = std::env::temp_dir().join(format!("puppet-envelope-conform-{}", std::process::id()));
    let shot_p = dir.join("shot.png");
    let shot_path = shot_p.to_string_lossy().to_string();
    let r = s.result(
        next_id(),
        "screenshot",
        serde_json::json!({"display": -1, "region": [0, 0, 64, 64], "out": shot_path}),
    );
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(false)),
        "screenshot with out must succeed, got: {r}"
    );
    assert!(
        shot_p.exists(),
        "screenshot out must land on disk, got: {r}"
    );
    assert_conforms(
        &schemas,
        &mut coverage,
        "screenshot",
        &r,
        "screenshot out set",
    );

    let r = s.result(
        next_id(),
        "do",
        serde_json::json!({"actions": [{"type": "click", "x": 5, "y": 5}], "dry_run": true, "allow_foreground": true}),
    );
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(true)),
        "a burst where nothing succeeded is a tool error, got: {r}"
    );
    {
        let sc = r.get("structuredContent").expect("structuredContent");
        assert_eq!(
            sc.get("results")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|f| f.get("code"))
                .and_then(|c| c.as_str()),
            Some("no_foreground_policy"),
            "refused action must name no_foreground_policy, got: {sc}"
        );
        assert_eq!(
            sc.get("events"),
            Some(&Value::from(0)),
            "refused burst posts zero events, got: {sc}"
        );
    }
    assert_conforms(&schemas, &mut coverage, "do", &r, "do posted-key refusal");

    let ambig = "env-conform-ambig";
    kill_stale_conformance_fixtures(ambig);
    let _fx1 = ConformanceFixture::spawn(ambig);
    let _fx2 = ConformanceFixture::spawn(ambig);
    let r = s.result(
        next_id(),
        "window",
        serde_json::json!({"action": "focus", "title": ambig}),
    );
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(true)),
        "ambiguous window call must be isError:true, got: {r}"
    );
    {
        let sc = r.get("structuredContent").expect("structuredContent");
        assert!(
            sc.get("candidates")
                .and_then(|c| c.as_array())
                .map(|a| a.len())
                == Some(2),
            "ambiguous target must name both candidates, got: {sc}"
        );
    }
    assert_conforms(&schemas, &mut coverage, "window", &r, "window ambiguous");
    drop(_fx1);
    drop(_fx2);

    let elem_title = "env-conform-elem";
    kill_stale_conformance_fixtures(elem_title);
    let _fxe = ConformanceFixture::spawn(elem_title);
    let r = s.result(
        next_id(),
        "elements",
        serde_json::json!({"title": elem_title}),
    );
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(false)),
        "elements on the fixture window must succeed, got: {r}"
    );
    {
        let sc = r.get("structuredContent").expect("structuredContent");
        assert_eq!(
            sc.get("empty"),
            Some(&Value::from(false)),
            "fixture window must list controls, got: {sc}"
        );
        assert!(
            sc.get("elements")
                .and_then(|v| v.as_array())
                .is_some_and(|a| !a.is_empty()),
            "fixture window must list controls, got: {sc}"
        );
    }
    assert_conforms(
        &schemas,
        &mut coverage,
        "elements",
        &r,
        "elements non-empty",
    );
    let r = s.result(
        next_id(),
        "elements",
        serde_json::json!({"title": elem_title, "offset": 100000}),
    );
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(false)),
        "elements past the last page must succeed, got: {r}"
    );
    {
        let sc = r.get("structuredContent").expect("structuredContent");
        assert_eq!(
            sc.get("empty"),
            Some(&Value::from(true)),
            "offset past the end must report empty, got: {sc}"
        );
    }
    assert_conforms(&schemas, &mut coverage, "elements", &r, "elements empty");
    drop(_fxe);

    let a_p = dir.join("a.png");
    let b_p = dir.join("b.png");
    let a_path = a_p.to_string_lossy().to_string();
    let b_path = b_p.to_string_lossy().to_string();
    let r = s.result(
        next_id(),
        "screenshot",
        serde_json::json!({"display": -1, "region": [0, 0, 64, 64], "out": a_path}),
    );
    assert_eq!(r.get("isError"), Some(&Value::from(false)));
    let r = s.result(
        next_id(),
        "screenshot",
        serde_json::json!({"display": -1, "region": [0, 0, 64, 64], "grid": true, "out": b_path}),
    );
    assert_eq!(r.get("isError"), Some(&Value::from(false)));
    let pa = |p: &str| serde_json::json!({"path": p});
    let r = s.result(
        next_id(),
        "diff",
        serde_json::json!({"base": pa(&a_path), "next": pa(&a_path)}),
    );
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(false)),
        "identical diff must succeed, got: {r}"
    );
    {
        let sc = r.get("structuredContent").expect("structuredContent");
        assert_eq!(sc.get("identical"), Some(&Value::from(true)));
        assert_eq!(sc.get("bbox"), Some(&Value::Null));
        assert_eq!(
            sc.get("diff_out"),
            Some(&Value::Null),
            "identical diff with no out must carry diff_out:null, got: {sc}"
        );
    }
    assert_conforms(&schemas, &mut coverage, "diff", &r, "diff identical");
    let r = s.result(
        next_id(),
        "diff",
        serde_json::json!({"base": pa(&a_path), "next": pa(&b_path)}),
    );
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(false)),
        "differing diff must succeed, got: {r}"
    );
    {
        let sc = r.get("structuredContent").expect("structuredContent");
        assert_eq!(sc.get("identical"), Some(&Value::from(false)));
        let bbox = sc.get("bbox").expect("bbox present");
        assert!(
            bbox.is_array() && bbox.as_array().map(|a| a.len()) == Some(4),
            "differing bbox is [x,y,w,h], got: {bbox}"
        );
    }
    assert_conforms(&schemas, &mut coverage, "diff", &r, "diff differing");

    let r = s.result(
        next_id(),
        "diff",
        serde_json::json!({"base": pa(&a_path), "next": {"display": -1, "region": [0, 0, 32, 33]}}),
    );
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(true)),
        "size-mismatched diff must be a tool error, got: {r}"
    );
    {
        let sc = r.get("structuredContent").expect("structuredContent");
        assert_eq!(
            sc.get("code").and_then(|c| c.as_str()),
            Some("size_mismatch"),
            "mismatch must be typed size_mismatch, got: {sc}"
        );
    }
    assert_conforms(&schemas, &mut coverage, "diff", &r, "diff size-mismatch");

    let _ = std::fs::remove_dir_all(&dir);

    for name in schemas.keys() {
        assert!(
            coverage.get(name).copied().unwrap_or(0) >= 1,
            "tool {name} has no conformance row"
        );
    }
    assert_eq!(
        coverage.len(),
        schemas.len(),
        "coverage names a tool tools/list does not: {:?} vs {:?}",
        coverage.keys().collect::<Vec<_>>(),
        schemas.keys().collect::<Vec<_>>()
    );
}

#[test]
fn a_native_region_capture_is_scale_one_and_byte_stable() {
    let mut s = Session::spawn();
    let mut next_id = {
        let mut id = 0i64;
        move || {
            id += 1;
            id
        }
    };
    let args = serde_json::json!({
        "display": -1, "region": [0, 0, 64, 64], "max_px": null, "fmt": "png",
    });
    let first = s.result(next_id(), "screenshot", args.clone());
    assert_eq!(
        first.get("isError"),
        Some(&Value::from(false)),
        "a native region capture must succeed, got: {first}"
    );
    let sc = first
        .get("structuredContent")
        .expect("structuredContent")
        .clone();
    assert_eq!(
        sc.get("scale").and_then(|v| v.as_f64()),
        Some(1.0),
        "max_px:null must not resample, got: {sc}"
    );
    let image = sc.get("image").expect("image block");
    let source = sc.get("source").expect("source block");
    for k in ["w", "h"] {
        assert_eq!(
            image.get(k).and_then(|v| v.as_i64()),
            source.get(k).and_then(|v| v.as_i64()),
            "image.{k} must equal source.{k} at scale 1, got: {sc}"
        );
    }
    let image_b64 = |r: &Value| {
        r.get("content")
            .and_then(|c| c.as_array())
            .and_then(|blocks| {
                blocks
                    .iter()
                    .find_map(|b| b.get("data").and_then(|d| d.as_str()))
            })
            .map(str::to_string)
            .expect("capture carries its image")
    };
    let second = s.result(next_id(), "screenshot", args);
    assert_eq!(
        image_b64(&first),
        image_b64(&second),
        "two identical native region captures must be byte-identical"
    );

    let cap = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cap.rs"),
    )
    .expect("src/cap.rs must be readable");
    assert!(
        cap.contains("if sw.max(sh) as i64 > m {"),
        "finalize's resize must stay guarded by the > test (A4: no upscale path)"
    );
}
