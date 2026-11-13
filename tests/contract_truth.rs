//! W1.1 contract truth: the text an agent reads first must describe this
//! server and no other.
//!
//! These tests fail on the pre-W1.1 tree: `instructions()` names six tools,
//! there is no claims source, and the `allow_*` clause reads as an
//! escalation path. Only `initialize` and `tools/list` run — no desktop
//! touch, no injection.
//!
//! The claims source is parsed, not duplicated: rows are read as the quoted
//! strings between brackets, so a claims string may not contain a double
//! quote.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
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

fn instructions_and_tools() -> (String, Vec<Value>) {
    let mut s = Session::spawn();
    let init = s.call(
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "contract_truth", "version": "0"},
        }),
    );
    let instructions = init
        .get("instructions")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("initialize must carry instructions, got: {init}"))
        .to_string();
    let list = s.call(2, "tools/list", serde_json::json!({}));
    let tools = list
        .get("tools")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_else(|| panic!("tools/list must carry tools, got: {list}"));
    (instructions, tools)
}

/// The claims source as the crate sees it, so the test pins the consts the
/// code actually uses rather than a copy.
const CLAIMS: &str = include_str!("../src/claims.rs");
const GATE: &str = include_str!("../src/gate.rs");

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

/// A flat string list, e.g. `ERROR_CODES`.
fn claims_list(name: &str) -> Vec<String> {
    quoted(table_body(name))
}

/// A table of `arity`-field tuples, e.g. `TOOL_LINES` (2), `REFUSALS` (3).
/// Rows are grouped by arity, so prose punctuation inside a claim cannot
/// move a row boundary.
fn claims_rows(name: &str, arity: usize) -> Vec<Vec<String>> {
    let values = quoted(table_body(name));
    assert_eq!(
        values.len() % arity,
        0,
        "{name} must be whole {arity}-field rows"
    );
    values.chunks(arity).map(|c| c.to_vec()).collect()
}

/// One `pub const NAME: &str = "value";` claim.
fn claims_const(name: &str) -> String {
    let at = CLAIMS
        .find(&format!("pub const {name}:"))
        .unwrap_or_else(|| panic!("claims.rs must declare {name}"));
    quoted(&CLAIMS[at..])
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("{name} must carry a string value"))
}

/// Every `.rs` file under `src/`, except the claims source itself: the
/// surface a code has to appear on to be a real code.
fn src_files_without_claims() -> Vec<String> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    files
        .into_iter()
        .filter(|p| p.file_name().and_then(|n| n.to_str()) != Some("claims.rs"))
        .filter_map(|p| std::fs::read_to_string(&p).ok())
        .collect()
}

/// `pub const NAME: &str = "value";` values from a source file.
fn string_consts(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in src.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("pub const ") else {
            continue;
        };
        let Some((name, rest)) = rest.split_once(':') else {
            continue;
        };
        if !rest.trim_start().starts_with("&str") {
            continue;
        }
        if let Some(value) = quoted(rest).into_iter().next() {
            out.push((name.trim().to_string(), value));
        }
    }
    out
}

#[test]
fn tool_lines_match_tools_list() {
    let (_, tools) = instructions_and_tools();
    let mut wire: Vec<String> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(str::to_string))
        .collect();
    wire.sort();
    let mut claimed: Vec<String> = claims_rows("TOOL_LINES", 2)
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    claimed.sort();
    assert_eq!(
        claimed, wire,
        "TOOL_LINES must name exactly the tools tools/list serves"
    );
    assert_eq!(claimed.len(), 7, "seven tools expected");
}

#[test]
fn every_error_code_is_a_real_code_and_every_real_code_is_listed() {
    let (instructions, _) = instructions_and_tools();
    let codes = claims_list("ERROR_CODES");
    assert!(!codes.is_empty(), "ERROR_CODES must not be empty");

    // (b) every refusing const in the gate is listed.
    let gate_codes: Vec<(String, String)> = string_consts(GATE)
        .into_iter()
        .filter(|(n, _)| n.ends_with("_POLICY") || n.ends_with("_UNAVAILABLE"))
        .collect();
    assert!(
        !gate_codes.is_empty(),
        "gate.rs must expose its refusing codes as string consts"
    );
    for (name, value) in &gate_codes {
        assert!(
            codes.contains(value),
            "gate.rs {name} = {value:?} must appear in ERROR_CODES"
        );
    }

    // (b, second half) every listed code is documented in the contract.
    for code in &codes {
        assert!(
            instructions.contains(code.as_str()),
            "the generated instructions must document {code:?}"
        );
    }

    // (c) every listed code exists as a literal outside claims.rs.
    let haystack = src_files_without_claims().join("\n");
    for code in &codes {
        assert!(
            haystack.contains(code.as_str()),
            "ERROR_CODES lists {code:?}, but no src file outside claims.rs emits it"
        );
    }

    // The refusal table may only name codes the server can emit.
    let mut refusal_codes: BTreeSet<String> = BTreeSet::new();
    for row in claims_rows("REFUSALS", 3) {
        if let Some(code) = row.get(2).filter(|c| !c.is_empty()) {
            refusal_codes.insert(code.clone());
        }
    }
    for code in &refusal_codes {
        assert!(
            codes.contains(code),
            "REFUSALS names {code:?}, which is not in ERROR_CODES"
        );
    }
}

#[test]
fn instructions_state_the_coordinate_rule_and_refuse_the_escalation_path() {
    let (instructions, _) = instructions_and_tools();
    // (d) the client-vs-screen rule, in the words the claims source uses.
    let coord_rule = claims_const("COORD_RULE");
    assert!(
        instructions.contains(&coord_rule),
        "instructions must carry the coordinate rule verbatim, got: {instructions}"
    );
    let lowered = coord_rule.to_lowercase();
    assert!(
        lowered.contains("client") && lowered.contains("screen"),
        "the coordinate rule must distinguish client from screen pixels, got: {coord_rule:?}"
    );
    // No `allow_*:true` outside a refusal clause (W1.2's rule).
    for token in [
        "allow_foreground:true",
        "allow_cursor:true",
        "allow_clipboard:true",
    ] {
        let mut from = 0;
        while let Some(i) = instructions[from..].find(token) {
            let at = from + i;
            let lo = at.saturating_sub(120);
            let hi = (at + token.len() + 120).min(instructions.len());
            let window = &instructions[lo..hi];
            assert!(
                window.contains("refus")
                    || window.contains("no flag")
                    || window.contains("no pixel fallback"),
                "{token:?} must sit inside a refusal clause, got: {window:?}"
            );
            from = at + token.len();
        }
    }
    // The six invariants and every limit are readable in the contract.
    for row in claims_rows("INVARIANTS", 2) {
        assert_eq!(row.len(), 2, "INVARIANTS rows are (id, line): {row:?}");
        assert!(
            instructions.contains(row[1].as_str()),
            "instructions must state invariant {}: {:?}",
            row[0],
            row[1]
        );
    }
    for row in claims_rows("LIMITS", 2) {
        assert_eq!(row.len(), 2, "LIMITS rows are (name, line): {row:?}");
        assert!(
            instructions.contains(row[1].as_str()),
            "instructions must state the limit {}: {:?}",
            row[0],
            row[1]
        );
    }
}
