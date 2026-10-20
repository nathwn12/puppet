//! P1a conformance: handshake, capabilities, version truth, 2026-07-28
//! discovery path, and strict JSON-RPC framing over stdio.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(15);
const SHORT: Duration = Duration::from_secs(2);
const MODERN: &str = "2026-07-28";

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

    fn write_line(&mut self, line: &str) {
        self.stdin.write_all(line.as_bytes()).expect("write frame");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush frame");
    }

    fn recv(&self, timeout: Duration) -> Option<Value> {
        self.rx
            .recv_timeout(timeout)
            .ok()
            .map(|line| serde_json::from_str(&line).expect("response is JSON"))
    }

    fn send_raw(&mut self, line: &str) -> Value {
        self.write_line(line);
        self.recv(TIMEOUT).expect("response in time")
    }

    fn send(&mut self, frame: &Value) -> Value {
        self.send_raw(&serde_json::to_string(frame).unwrap())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn frame(id: i64, method: &str, params: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

fn init_params(version: &str) -> Value {
    serde_json::json!({
        "protocolVersion": version,
        "capabilities": {},
        "clientInfo": {"name": "t", "version": "0"},
    })
}

fn assert_tools_only_caps(result: &Value, what: &str) {
    assert_eq!(
        result.get("capabilities"),
        Some(&serde_json::json!({"tools": {"listChanged": false}})),
        "{what} capabilities must be tools-only, got: {result}"
    );
}

fn assert_server_version(result: &Value, what: &str) {
    assert_eq!(
        result
            .get("serverInfo")
            .and_then(|i| i.get("name"))
            .and_then(|v| v.as_str()),
        Some("puppet"),
        "{what} serverInfo.name must be puppet, got: {result}"
    );
    assert_eq!(
        result
            .get("serverInfo")
            .and_then(|i| i.get("version"))
            .and_then(|v| v.as_str()),
        Some(env!("CARGO_PKG_VERSION")),
        "{what} serverInfo.version must be the crate version, got: {result}"
    );
}

fn assert_instructions(result: &Value, what: &str) {
    let ins = result
        .get("instructions")
        .and_then(|v| v.as_str())
        .expect("instructions present");
    assert!(!ins.is_empty(), "{what} instructions must be non-empty");
    assert!(
        ins.contains("puppet desktop-control MCP"),
        "{what} instructions must carry the product marker"
    );
    assert!(
        ins.contains("screen_x = source.x + round"),
        "{what} instructions must carry the coordinate formula"
    );
}

#[test]
fn initialize_version_matrix() {
    let mut s = Session::spawn();
    // (requested, expected echo)
    for (n, req, echo) in [
        (1, "2025-06-18", "2025-06-18"),
        (2, "2025-11-25", "2025-11-25"),
        (3, "2026-07-28", "2025-11-25"),
        (4, "2099-01-01", "2025-11-25"),
    ] {
        let resp = s.send(&frame(n, "initialize", init_params(req)));
        let result = resp
            .get("result")
            .unwrap_or_else(|| panic!("initialize({req}) must succeed, got: {resp}"));
        assert_eq!(
            result.get("protocolVersion").and_then(|v| v.as_str()),
            Some(echo),
            "initialize({req}) must negotiate {echo}, got: {result}"
        );
        assert_tools_only_caps(result, &format!("initialize({req})"));
        assert_server_version(result, &format!("initialize({req})"));
        assert!(
            result.get("resultType").is_none(),
            "legacy initialize({req}) must not carry resultType, got: {result}"
        );
    }
    assert_instructions(
        &s.send(&frame(5, "initialize", init_params("2025-11-25")))
            .get("result")
            .cloned()
            .unwrap(),
        "initialize",
    );

    // Legacy tools/list carries no modern marker.
    let resp = s.send(&frame(6, "tools/list", serde_json::json!({})));
    let result = resp.get("result").expect("tools/list has result");
    assert!(
        result.get("resultType").is_none(),
        "legacy tools/list must not carry resultType, got: {result}"
    );
    assert!(result.get("tools").is_some(), "tools/list needs tools");
}

#[test]
fn discover_lists_modern_and_legacy_with_tools_only() {
    let mut s = Session::spawn();
    let resp = s.send(&frame(1, "server/discover", serde_json::json!({})));
    let result = resp.get("result").expect("server/discover has result");
    let supported = result
        .get("supportedVersions")
        .and_then(|v| v.as_array())
        .expect("supportedVersions array");
    let strs: Vec<&str> = supported.iter().filter_map(|v| v.as_str()).collect();
    for v in ["2025-06-18", "2025-11-25", "2026-07-28"] {
        assert!(
            strs.contains(&v),
            "supportedVersions must contain {v}, got: {strs:?}"
        );
    }
    for v in ["2024-11-05", "2025-03-26"] {
        assert!(
            !strs.contains(&v),
            "supportedVersions must not advertise {v} (batching not implemented), got: {strs:?}"
        );
    }
    assert_tools_only_caps(result, "server/discover");
    assert_server_version(result, "server/discover");
    assert_instructions(result, "server/discover");
    assert_eq!(
        result.get("resultType").and_then(|v| v.as_str()),
        Some("complete"),
        "modern server/discover must carry resultType complete, got: {result}"
    );
}

fn meta_params(extra: Value) -> Value {
    let mut m = extra.as_object().cloned().unwrap_or_default();
    m.insert(
        "_meta".to_string(),
        serde_json::json!({"io.modelcontextprotocol/protocolVersion": MODERN}),
    );
    Value::Object(m)
}

#[test]
fn tools_list_modern_carries_result_type() {
    let mut s = Session::spawn();
    let params = meta_params(serde_json::json!({}));
    let resp = s.send(&frame(1, "tools/list", params));
    let result = resp.get("result").expect("tools/list has result");
    assert_eq!(
        result.get("resultType").and_then(|v| v.as_str()),
        Some("complete"),
        "modern tools/list must carry resultType complete, got: {result}"
    );
}

#[test]
fn tools_call_result_type_modern_only() {
    let mut s = Session::spawn();
    // Legacy peer (no _meta): no marker.
    let resp = s.send(&frame(
        2,
        "tools/call",
        serde_json::json!({"name": "list_windows", "arguments": {}}),
    ));
    let result = resp.get("result").expect("tools/call has result");
    assert!(
        result.get("resultType").is_none(),
        "legacy tools/call must not carry resultType, got: {result}"
    );
    // Modern peer: marker plus content.
    let params = meta_params(serde_json::json!({
        "name": "list_windows",
        "arguments": {},
    }));
    let resp = s.send(&frame(3, "tools/call", params));
    let result = resp.get("result").expect("tools/call has result");
    assert_eq!(
        result.get("resultType").and_then(|v| v.as_str()),
        Some("complete"),
        "modern tools/call must carry resultType complete, got: {result}"
    );
    assert!(
        result.get("content").and_then(|v| v.as_array()).is_some(),
        "modern tools/call must carry a content block, got: {result}"
    );
}

#[test]
fn unsupported_per_request_version_rejected() {
    let mut s = Session::spawn();
    let mut m = serde_json::Map::new();
    m.insert(
        "_meta".to_string(),
        serde_json::json!({"io.modelcontextprotocol/protocolVersion": "2099-01-01"}),
    );
    let resp = s.send(&frame(4, "tools/list", Value::Object(m)));
    let err = resp.get("error").expect("unsupported version is an error");
    assert_eq!(err.get("code").and_then(|v| v.as_i64()), Some(-32022));
    let data = err.get("data").expect("error carries data");
    assert_eq!(
        data.get("requested").and_then(|v| v.as_str()),
        Some("2099-01-01"),
        "data.requested must echo the rejected version, got: {data}"
    );
    let supported = data
        .get("supported")
        .and_then(|v| v.as_array())
        .expect("data.supported array");
    assert!(!supported.is_empty(), "data.supported must be non-empty");
}

#[test]
fn removed_capabilities_are_method_not_found() {
    let mut s = Session::spawn();
    for (n, method) in [
        (5, "prompts/list"),
        (6, "resources/list"),
        (7, "resources/templates/list"),
        (8, "logging/setLevel"),
    ] {
        let resp = s.send(&frame(n, method, serde_json::json!({})));
        let err = resp
            .get("error")
            .unwrap_or_else(|| panic!("{method} must be an error, got: {resp}"));
        assert_eq!(
            err.get("code").and_then(|v| v.as_i64()),
            Some(-32601),
            "{method} must be Method not found, got: {resp}"
        );
    }
}

fn assert_invalid_request(resp: &Value, what: &str) {
    assert_eq!(
        resp.get("error")
            .and_then(|e| e.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32600),
        "{what} must be Invalid Request, got: {resp}"
    );
}

#[test]
fn strict_framing_rejects_batch_bad_jsonrpc_and_scalar() {
    let mut s = Session::spawn();
    // Batch array -> single -32600 with null id.
    let resp = s.send_raw(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#);
    assert_invalid_request(&resp, "batch array");
    assert_eq!(resp.get("id"), Some(&Value::Null));
    // Wrong jsonrpc version -> -32600.
    let resp = s.send_raw(r#"{"jsonrpc":"1.0","id":21,"method":"ping"}"#);
    assert_invalid_request(&resp, "jsonrpc 1.0 frame");
    // Valid JSON scalar -> -32600, not a parse error.
    let resp = s.send_raw("42");
    assert_invalid_request(&resp, "scalar frame");
    // Malformed JSON stays -32700.
    let resp = s.send_raw("{bad json");
    assert_eq!(
        resp.get("error")
            .and_then(|e| e.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32700),
        "malformed JSON must be a Parse error, got: {resp}"
    );
}

#[test]
fn strict_framing_method_shape_never_dropped() {
    let mut s = Session::spawn();
    // Id but no method -> -32600, fast (bounded wait, never silent).
    s.write_line(r#"{"jsonrpc":"2.0","id":9}"#);
    let resp = s
        .recv(SHORT)
        .expect("method-less frame with id must get a reply, not silence");
    assert_invalid_request(&resp, "method-less frame");
    // Non-string method -> -32600.
    s.write_line(r#"{"jsonrpc":"2.0","id":22,"method":5}"#);
    let resp = s
        .recv(SHORT)
        .expect("non-string method must get a reply, not silence");
    assert_invalid_request(&resp, "non-string method");
}

#[test]
fn notification_gets_no_response() {
    let mut s = Session::spawn();
    // Valid notification (no id): must stay silent; the following ping is
    // the only line the server may emit.
    s.write_line(r#"{"jsonrpc":"2.0","method":"ping"}"#);
    s.write_line(r#"{"jsonrpc":"2.0","id":30,"method":"ping"}"#);
    let resp = s.recv(TIMEOUT).expect("ping after notification replies");
    assert_eq!(
        resp.get("id").and_then(|v| v.as_i64()),
        Some(30),
        "only the ping response may arrive, got: {resp}"
    );
    assert!(
        s.recv(SHORT).is_none(),
        "notification must not produce an extra line"
    );
}
