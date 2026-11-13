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

/// Every action type `do.actions.items` must cover, and every verb the
/// claims source must classify.
const ACTION_TYPES: [&str; 16] = [
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

/// The claims source and its tables, parsed rather than copied (W1.1, W1.3).
const CLAIMS: &str = include_str!("../src/claims.rs");

/// The bracketed body of one `pub const NAME … = [ … ];` declaration.
fn table_body(name: &str) -> &'static str {
    let at = CLAIMS
        .find(&format!("pub const {name}:"))
        .unwrap_or_else(|| panic!("claims.rs must declare {name}"));
    let rest = &CLAIMS[at..];
    let eq = rest
        .find('=')
        .unwrap_or_else(|| panic!("{name} must be a declaration"));
    let open = eq
        + rest[eq..]
            .find('[')
            .unwrap_or_else(|| panic!("{name} must be a bracketed table"))
        + 1;
    let close = rest[open..]
        .find("\n];")
        .map(|i| open + i)
        .unwrap_or_else(|| panic!("{name} table must close with ];"));
    &rest[open..close]
}

/// Every double-quoted string in a fragment, in order.
fn quoted(fragment: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = fragment;
    while let Some(i) = rest.find('"') {
        let after = &rest[i + 1..];
        let Some(j) = after.find('"') else { break };
        out.push(after[..j].to_string());
        rest = &after[j + 1..];
    }
    out
}

/// A table of `arity`-field rows from the claims source.
fn claims_rows(name: &str, arity: usize) -> Vec<Vec<String>> {
    let values = quoted(table_body(name));
    assert_eq!(
        values.len() % arity,
        0,
        "{name} must be whole {arity}-field rows"
    );
    values.chunks(arity).map(|c| c.to_vec()).collect()
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

    /// One request/response round-trip on this session.
    fn call(&mut self, id: i64, method: &str, params: Value) -> Value {
        let frame = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        });
        let line = serde_json::to_string(&frame).unwrap();
        self.stdin.write_all(line.as_bytes()).expect("write");
        self.stdin.write_all(b"\n").expect("newline");
        self.stdin.flush().expect("flush");
        let resp: Value = self
            .rx
            .recv_timeout(TIMEOUT)
            .ok()
            .map(|l| serde_json::from_str(&l).expect("response is JSON"))
            .expect("response in time");
        resp.get("result")
            .cloned()
            .unwrap_or_else(|| panic!("{method} must succeed, got: {resp}"))
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
    assert_eq!(tools.len(), 7, "seven tools expected");
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
            "screenshot" | "diff" | "list_windows" | "cursor" | "elements" => {
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
    let mut want: Vec<String> = ACTION_TYPES.iter().map(|s| s.to_string()).collect();
    want.sort();
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

/// SPEC W3.2 and AMENDMENT-01 A3: the two capture defaults an agent inherits.
/// Lossless by default (`fmt` is `png`), and the preview clamp stays 1280 on
/// both capture tools — the native path is the caller's explicit
/// `max_px:null`, never a raised default.
#[test]
fn capture_defaults_are_lossless_and_bounded() {
    let tools = list_tools();
    let shot = input_schema(tool(&tools, "screenshot"));
    assert_eq!(
        shot["properties"]["fmt"].get("default"),
        Some(&Value::from("png")),
        "screenshot.fmt must default to png (W3.2)"
    );
    for name in ["screenshot", "diff"] {
        let schema = input_schema(tool(&tools, name));
        assert_eq!(
            schema["properties"]["max_px"].get("default"),
            Some(&Value::from(1280)),
            "{name}.max_px must default to 1280 (A3)"
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
        (
            "diff",
            &[
                "ok",
                "changed",
                "total",
                "ratio",
                "identical",
                "base",
                "next",
            ],
        ),
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
            "diff",
            "do",
            "elements",
            "list_windows",
            "screenshot",
            "window"
        ],
        "fixture must pin exactly the seven tools"
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

/// W1.3: the schema prose may not document a dead verb or a code that does
/// not exist. Every action type is classified in the claims source, and a
/// verb classified `refused` is only ever described inside a refusal
/// clause (`refused`/`refuses`/`refusal` within the same sentence).
#[test]
fn every_action_type_is_classified_and_refused_verbs_say_so() {
    let tools = list_tools();
    let do_desc = tool(&tools, "do")
        .get("description")
        .and_then(|v| v.as_str())
        .expect("do must carry a description");
    let rows = claims_rows("REFUSALS", 3);

    // Every action type the schema declares has a classification.
    let classified: Vec<String> = rows.iter().map(|r| r[0].clone()).collect();
    for verb in ACTION_TYPES {
        assert!(
            classified.iter().any(|v| v == verb),
            "claims REFUSALS must classify {verb}"
        );
    }

    // A refused verb is documented as refused, naming its code.
    for row in rows.iter().filter(|r| r[1] == "refused") {
        let (verb, code) = (&row[0], &row[2]);
        let mut documented = false;
        for (at, _) in do_desc.match_indices(verb.as_str()) {
            let before = do_desc[..at].chars().next_back();
            let after = do_desc[at + verb.len()..].chars().next();
            let word =
                |c: Option<char>| c.map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false);
            if word(before) || word(after) {
                continue;
            }
            let lo = at.saturating_sub(160);
            let hi = (at + verb.len() + 160).min(do_desc.len());
            let window = do_desc[lo..hi].to_lowercase();
            if window.contains("refus") && window.contains(code.as_str()) {
                documented = true;
                break;
            }
        }
        assert!(
            documented,
            "do's description must call {verb} refused with {code} (W1.3)"
        );
    }
}

/// W1.3: three codes never existed as behaviour and must not survive in any
/// tool description.
#[test]
fn no_description_documents_a_code_that_never_existed() {
    let dead = [
        "clipboard_write_required",
        "cursor_move_required",
        "foreground_required",
    ];
    let tools = list_tools();
    for t in &tools {
        let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let desc = t.get("description").and_then(|d| d.as_str()).unwrap_or("");
        for code in dead {
            assert!(
                !desc.contains(code),
                "{name} documents {code}, which no code emits"
            );
        }
        // The input schema's own prose counts too: an option description is
        // read by the agent as the tool's contract.
        let schema = serde_json::to_string(&t["inputSchema"]).unwrap();
        for code in dead {
            assert!(
                !schema.contains(code),
                "{name}'s inputSchema documents {code}, which no code emits"
            );
        }
    }
}

/// SPEC W1.4: each `TOOL_*` const stays under its budget, and the whole
/// surface under their sum. Measured on the consts the server serves, not on
/// a re-serialisation of them. AMENDMENT-02 raised `diff` from 2 400 to
/// 2 800 so the W3 mask and scale contract fits without deleting a claim;
/// the sum is unchanged.
const TOOL_BUDGETS: [(&str, usize); 7] = [
    ("screenshot", 4000),
    ("diff", 2800),
    ("list_windows", 1400),
    ("window", 3000),
    ("do", 12000),
    ("cursor", 700),
    ("elements", 3200),
];
const BUDGET_SUM: usize = 26700;

/// The `r#"…"#` value of one `pub const TOOL_*` line in `src/schemas.rs`.
fn tool_const(name: &str) -> String {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/schemas.rs"),
    )
    .expect("src/schemas.rs must be readable");
    let prefix = format!("pub const TOOL_{}: &str = r#\"", name.to_uppercase());
    let at = src
        .find(&prefix)
        .unwrap_or_else(|| panic!("schemas.rs must declare TOOL_{}", name.to_uppercase()));
    let rest = &src[at + prefix.len()..];
    let end = rest
        .find("\"#;")
        .unwrap_or_else(|| panic!("TOOL_{} must close with \"#;", name.to_uppercase()));
    rest[..end].to_string()
}

/// One `pub const NAME: &str = "value";` claim from the claims source.
fn claims_const(name: &str) -> String {
    let at = CLAIMS
        .find(&format!("pub const {name}:"))
        .unwrap_or_else(|| panic!("claims.rs must declare {name}"));
    quoted(&CLAIMS[at..])
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("{name} must carry a string value"))
}

#[test]
fn tool_consts_fit_their_byte_budgets() {
    let mut sum = 0;
    for (name, budget) in TOOL_BUDGETS {
        let len = tool_const(name).len();
        sum += len;
        assert!(
            len <= budget,
            "TOOL_{} is {len} bytes, over its {budget}-byte budget (W1.4)",
            name.to_uppercase()
        );
    }
    assert!(
        sum <= BUDGET_SUM,
        "the seven consts total {sum} bytes, over the {BUDGET_SUM}-byte surface budget"
    );
    println!("BUDGET_OK sum={sum}");
}

/// The budget may not delete truth: every load-bearing claim the claims
/// source states still reaches an agent through the contract it reads
/// (`initialize` instructions plus the seven tool descriptions).
#[test]
fn the_budget_cut_kept_every_load_bearing_claim() {
    let mut s = Session::spawn();
    let init = s.call(
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "schema_truth", "version": "0"},
        }),
    );
    let instructions = init
        .get("instructions")
        .and_then(|v| v.as_str())
        .expect("initialize must carry instructions")
        .to_string();
    let list = s.call(2, "tools/list", serde_json::json!({}));
    let tools = list["tools"].as_array().expect("tools/list tools").clone();
    let do_desc = tools
        .iter()
        .find(|t| t["name"] == "do")
        .and_then(|t| t["description"].as_str())
        .expect("do description")
        .to_string();
    let elements_desc = tools
        .iter()
        .find(|t| t["name"] == "elements")
        .and_then(|t| t["description"].as_str())
        .expect("elements description")
        .to_string();
    let surface = format!(
        "{instructions}\n{}\n",
        tools
            .iter()
            .filter_map(|t| t["description"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );

    // The coordinate rule, verbatim from the claims source.
    let coord = claims_const("COORD_RULE");
    assert!(
        surface.contains(&coord),
        "the byte cut must not drop the coordinate rule"
    );
    // `SendInput` is never called (I1), and the effect/satisfied readings
    // that keep `ok` from being mistaken for a result.
    let i1 = claims_rows("INVARIANTS", 2)
        .into_iter()
        .find(|r| r[0] == "I1")
        .expect("I1 must be claimed");
    assert!(
        surface.contains(&i1[1]),
        "the byte cut must not drop the no-SendInput invariant"
    );
    for row in claims_rows("LIMITS", 2) {
        match row[0].as_str() {
            "ok_vs_effect" => assert!(
                surface.contains(&row[1]),
                "the byte cut must not drop the effect semantics"
            ),
            "wait_for" => assert!(
                surface.contains(&row[1]),
                "the byte cut must not drop the satisfied-vs-ok reading"
            ),
            _ => {}
        }
    }
    // expect_fp is an agent-facing requirement, not a footnote: it has to
    // be readable on both tools that take it.
    assert!(
        do_desc.contains("expect_fp") && elements_desc.contains("expect_fp"),
        "do and elements must both state the expect_fp requirement"
    );
    // The refusal table survives in the tool an agent reads before acting.
    for row in claims_rows("REFUSALS", 3) {
        if row[1] != "refused" {
            continue;
        }
        assert!(
            do_desc.contains(&row[0]) && do_desc.contains(&row[2]),
            "do's description must keep the refusal {verb} -> {code}",
            verb = row[0],
            code = row[2]
        );
    }
}

/// SPEC W2.2: both tools that take an `fp` state the three exclusions and the
/// consequence, so an agent cannot read the fingerprint as covering geometry,
/// text or state — and cannot skip `expect_fp` after moving a slider.
#[test]
fn the_fingerprint_prose_names_the_three_exclusions() {
    let tools = list_tools();
    for name in ["elements", "do"] {
        let t = tool(&tools, name);
        let desc = t
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or_else(|| panic!("{name} must carry a description"));
        for exclusion in ["rect", "text", "state"] {
            assert!(
                desc.contains(exclusion),
                "{name}'s description must name {exclusion} as excluded from fp, got: {desc}"
            );
        }
        assert!(
            desc.contains("same control"),
            "{name}'s description must state the consequence of a match: the same control, \
             not the same condition, got: {desc}"
        );
    }
}
