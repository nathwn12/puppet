//! P2b schema truth: the declared tool surface is truthful and the
//! element fingerprint is stable across geometry changes.
//!
//! These tests fail on the pre-P2b tree (no annotations,
//! `do.actions.items` untyped, `fp` includes rect) and pass after. Only
//! `tools/list` runs: no desktop touch, no injection.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{Map, Value};

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

fn tool<'a>(tools: &'a [Value], name: &str) -> &'a Map<String, Value> {
    tools
        .iter()
        .find(|t| t.get("name").and_then(|n| n.as_str()) == Some(name))
        .and_then(|t| t.as_object())
        .unwrap_or_else(|| panic!("tools/list must contain {name}"))
}

fn input_schema(t: &Map<String, Value>) -> &Map<String, Value> {
    t.get("inputSchema")
        .and_then(|s| s.as_object())
        .unwrap_or_else(|| panic!("{} must carry inputSchema", t["name"]))
}

/// Every variant of a oneOf/anyOf (following $defs/$ref when present).
fn union_variants<'a>(schema: &'a Value, root: &'a Value) -> Vec<&'a Map<String, Value>> {
    let resolve = |v: &'a Value| -> Option<&'a Map<String, Value>> {
        let o = v.as_object()?;
        if let Some(r) = o.get("$ref").and_then(|r| r.as_str()) {
            let path = r.strip_prefix("#/").unwrap_or(r);
            let mut cur = root;
            for seg in path.split('/') {
                cur = cur.get(seg)?;
            }
            return cur.as_object();
        }
        Some(o)
    };
    let mut out = Vec::new();
    if let Some(o) = schema.as_object() {
        for key in ["oneOf", "anyOf"] {
            if let Some(arr) = o.get(key).and_then(|v| v.as_array()) {
                for v in arr {
                    if let Some(m) = resolve(v) {
                        out.push(m);
                    }
                }
            }
        }
    }
    out
}

/// The declared `type` value(s) of one action variant.
fn variant_types(v: &Map<String, Value>) -> Vec<String> {
    let t = v
        .get("properties")
        .and_then(|p| p.get("type"))
        .unwrap_or_else(|| panic!("action variant must declare a type property, got: {v:?}"));
    if let Some(c) = t.get("const").and_then(|c| c.as_str()) {
        return vec![c.to_string()];
    }
    if let Some(e) = t.get("enum").and_then(|e| e.as_array()) {
        return e
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect();
    }
    panic!("action variant type must use const/enum, got: {t}");
}

/// Recursively collect every `required` array in a schema.
fn collect_required(schema: &Value, into: &mut Vec<BTreeSet<String>>) {
    match schema {
        Value::Object(o) => {
            if let Some(req) = o.get("required").and_then(|r| r.as_array()) {
                into.push(
                    req.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect(),
                );
            }
            for (k, v) in o {
                if k == "required" {
                    continue;
                }
                collect_required(v, into);
            }
        }
        Value::Array(a) => {
            for v in a {
                collect_required(v, into);
            }
        }
        _ => {}
    }
}

#[test]
fn every_tool_carries_truthful_annotations() {
    let tools = list_tools();
    assert_eq!(tools.len(), 6, "six tools expected");
    for t in &tools {
        let o = t.as_object().expect("tool object");
        let name = o.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let title = o.get("title").and_then(|v| v.as_str()).unwrap_or("");
        assert!(!title.is_empty(), "{name} must keep its title");
        let a = o
            .get("annotations")
            .and_then(|v| v.as_object())
            .unwrap_or_else(|| panic!("{name} must carry annotations"));
        assert_eq!(
            a.get("title").and_then(|v| v.as_str()),
            Some(title),
            "{name} annotations.title must match"
        );
        for hint in [
            "readOnlyHint",
            "destructiveHint",
            "idempotentHint",
            "openWorldHint",
        ] {
            assert!(
                a.get(hint).and_then(|v| v.as_bool()).is_some(),
                "{name} annotations must carry boolean {hint}"
            );
        }
        let (ro, dest, idem, open) = (
            a["readOnlyHint"].as_bool().unwrap(),
            a["destructiveHint"].as_bool().unwrap(),
            a["idempotentHint"].as_bool().unwrap(),
            a["openWorldHint"].as_bool().unwrap(),
        );
        assert!(!open, "{name} never touches the open world");
        match name {
            "screenshot" | "list_windows" | "cursor" | "elements" => {
                assert!(ro, "{name} is read-only");
                assert!(!dest, "{name} is not destructive");
                assert!(idem, "{name} is idempotent");
            }
            "window" | "do" => {
                assert!(!ro, "{name} mutates");
                assert!(dest, "{name} is destructive");
                assert!(!idem, "{name} is not idempotent");
            }
            other => panic!("unexpected tool {other}"),
        }
    }
}

#[test]
fn do_actions_schema_declares_all_sixteen_types() {
    let tools = list_tools();
    let t = tool(&tools, "do");
    let schema = input_schema(t);
    let actions = &schema["properties"]["actions"];
    assert_eq!(
        actions.get("minItems"),
        Some(&Value::from(1)),
        "do.actions must require a non-empty batch"
    );
    let items = actions.get("items").expect("do.actions.items");
    let root = Value::Object(t.clone());
    let variants = union_variants(items, &root);
    let mut types: Vec<String> = variants.iter().flat_map(|v| variant_types(v)).collect();
    types.sort();
    let want = [
        "click",
        "clipboard_get",
        "clipboard_set",
        "down",
        "drag",
        "focus_window",
        "hotkey",
        "invoke",
        "key",
        "move",
        "scroll",
        "set_value",
        "type",
        "up",
        "wait",
        "wait_for",
    ];
    assert_eq!(
        types, want,
        "do.actions.items must cover exactly the 16 runtime types"
    );
    for v in &variants {
        let req = v
            .get("required")
            .and_then(|r| r.as_array())
            .unwrap_or_else(|| panic!("variant must require type, got: {v:?}"));
        assert!(
            req.iter().any(|x| x.as_str() == Some("type")),
            "variant must require type, got: {v:?}"
        );
    }
    let desktop = &schema["properties"]["desktop"];
    assert!(
        serde_json::to_string(desktop).unwrap().contains("hidden"),
        "do.desktop must declare the hidden enum value, got: {desktop}"
    );
}

#[test]
fn window_action_enum_with_move_resize_conditionals() {
    let tools = list_tools();
    let t = tool(&tools, "window");
    let schema = input_schema(t);
    let action = &schema["properties"]["action"];
    let mut kinds: Vec<String> = action
        .get("enum")
        .and_then(|e| e.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    kinds.sort();
    assert_eq!(
        kinds,
        ["close", "focus", "maximize", "minimize", "move", "resize", "restore", "topmost"],
        "window.action must enumerate the 8 runtime actions"
    );
    let mut reqs = Vec::new();
    collect_required(&Value::Object(schema.clone()), &mut reqs);
    let has_xy = reqs.iter().any(|r| {
        r.contains("x") && r.contains("y") && !r.contains("width") && !r.contains("height")
    });
    let has_wh = reqs
        .iter()
        .any(|r| r.contains("width") && r.contains("height"));
    assert!(
        has_xy,
        "window schema must conditionally require x,y for move, got: {reqs:?}"
    );
    assert!(
        has_wh,
        "window schema must conditionally require width,height for resize, got: {reqs:?}"
    );
}

#[test]
fn screenshot_schema_states_real_bounds() {
    let tools = list_tools();
    let t = tool(&tools, "screenshot");
    let schema = input_schema(t);
    let fmt = &schema["properties"]["fmt"];
    let mut kinds: Vec<String> = fmt
        .get("enum")
        .and_then(|e| e.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    kinds.sort();
    assert_eq!(kinds, ["jpeg", "jpg", "png"], "screenshot fmt enum");
    let q = &schema["properties"]["quality"];
    assert_eq!(q.get("minimum"), Some(&Value::from(1)), "quality floor");
    assert_eq!(q.get("maximum"), Some(&Value::from(100)), "quality ceiling");
    let region = serde_json::to_string(&schema["properties"]["region"]).unwrap();
    assert!(
        region.contains("\"minItems\":4") || region.contains("\"minItems\": 4"),
        "region must declare length 4, got: {region}"
    );
    assert!(
        region.contains("\"maxItems\":4") || region.contains("\"maxItems\": 4"),
        "region must declare length 4, got: {region}"
    );
    for (k, want) in [
        ("display", "integer"),
        ("grid", "boolean"),
        ("allow_crop_fallback", "boolean"),
        ("elements", "boolean"),
    ] {
        let s = serde_json::to_string(&schema["properties"][k]).unwrap();
        assert!(
            s.contains(want),
            "screenshot.{k} must declare {want}, got: {s}"
        );
    }
}

#[test]
fn elements_schema_requires_a_target() {
    let tools = list_tools();
    let t = tool(&tools, "elements");
    let schema = input_schema(t);
    let any = schema
        .get("anyOf")
        .and_then(|v| v.as_array())
        .expect("elements must carry anyOf title/hw");
    let mut reqs = Vec::new();
    collect_required(&Value::Array(any.clone()), &mut reqs);
    assert!(
        reqs.iter().any(|r| r.contains("title")),
        "elements must accept title, got: {reqs:?}"
    );
    assert!(
        reqs.iter().any(|r| r.contains("hwnd")),
        "elements must accept hwnd, got: {reqs:?}"
    );
    assert_eq!(
        schema["properties"]["detail"].get("type"),
        Some(&Value::from("boolean")),
        "elements.detail stays boolean"
    );
}

#[test]
fn p2a_output_schemas_unchanged_in_shape() {
    let tools = list_tools();
    let expect: &[(&str, &[&str])] = &[
        (
            "screenshot",
            &[
                "image", "source", "scale", "monitors", "cursor", "capture", "formula",
            ],
        ),
        ("list_windows", &["windows"]),
        ("window", &["ok"]),
        (
            "do",
            &[
                "results",
                "total_ms",
                "sendinput_calls",
                "sendinput_ms",
                "events",
                "focus_before",
                "focus_after",
            ],
        ),
        ("cursor", &["x", "y", "screens"]),
        (
            "elements",
            &[
                "backend",
                "truncated",
                "rect_space",
                "elements",
                "uia_available",
                "empty",
                "depth_capped",
                "depth_limit",
            ],
        ),
    ];
    for (name, required) in expect {
        let t = tool(&tools, name);
        let out = t
            .get("outputSchema")
            .and_then(|v| v.as_object())
            .unwrap_or_else(|| panic!("{name} must keep its outputSchema"));
        let props = out
            .get("properties")
            .and_then(|v| v.as_object())
            .unwrap_or_else(|| panic!("{name} outputSchema needs properties"));
        assert!(
            !props.is_empty(),
            "{name} outputSchema.properties non-empty"
        );
        let mut req: Vec<&str> = out
            .get("required")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        req.sort();
        let mut want: Vec<&str> = required.to_vec();
        want.sort();
        assert_eq!(req, want, "{name} outputSchema.required shape");
        assert!(
            !serde_json::to_string(t)
                .unwrap()
                .contains("x-fastmcp-wrap-result"),
            "{name} must not regain x-fastmcp-wrap-result"
        );
    }
}

/// Frozen P2a contract: every tool's `outputSchema` from `tools/list`
/// must equal `tests/fixtures/output_schemas_p2a.json` value-for-value
/// (`serde_json::Value` equality is order-insensitive for objects, so
/// key order is free but every field, type, and required entry is
/// pinned). The fixture was captured from the P2b tree's own binary,
/// whose `outputSchema`s were verified value-identical across the
/// rewrite; any future edit to an envelope fails here first.
#[test]
fn p2a_output_schemas_match_frozen_fixture() {
    let frozen: Value = serde_json::from_str(include_str!("fixtures/output_schemas_p2a.json"))
        .expect("frozen P2a outputSchema fixture must parse");
    let frozen = frozen.as_object().expect("fixture is a name->schema map");
    assert_eq!(
        {
            let mut names: Vec<&str> = frozen.keys().map(String::as_str).collect();
            names.sort();
            names
        },
        [
            "cursor",
            "do",
            "elements",
            "list_windows",
            "screenshot",
            "window"
        ],
        "fixture must pin exactly the six tools"
    );
    let tools = list_tools();
    for (name, want) in frozen {
        let t = tool(&tools, name);
        let got = t
            .get("outputSchema")
            .unwrap_or_else(|| panic!("{name} must keep its outputSchema"));
        assert_eq!(
            got, want,
            "{name} outputSchema must equal the frozen P2a value"
        );
    }
}
