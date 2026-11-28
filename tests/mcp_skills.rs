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

#[test]
fn mcp_skills_roundtrip() {
    let mut s = Session::spawn();

    // initialize advertises resources + skills extension
    let resp = s.send(&frame(
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "t", "version": "0"},
        }),
    ));
    let result = resp.get("result").expect("initialize result");
    assert_eq!(
        result.get("capabilities").and_then(|c| c.get("resources")),
        Some(&serde_json::json!({}))
    );
    assert_eq!(
        result
            .get("capabilities")
            .and_then(|c| c.get("extensions"))
            .and_then(|e| e.get("io.modelcontextprotocol/skills")),
        Some(&serde_json::json!({}))
    );

    // skills/list
    let resp = s.send(&frame(2, "skills/list", serde_json::json!({})));
    let result = resp.get("result").expect("skills/list result");
    assert_eq!(result.get("ttlMs").and_then(|v| v.as_i64()), Some(0));
    assert_eq!(
        result.get("cacheScope").and_then(|v| v.as_str()),
        Some("private")
    );
    let skills = result
        .get("skills")
        .and_then(|v| v.as_array())
        .expect("skills array");
    assert_eq!(skills.len(), 1);
    let skill = &skills[0];
    assert_eq!(
        skill.get("uri").and_then(|v| v.as_str()),
        Some("skill://windows/SKILL.md")
    );
    assert_eq!(
        skill
            .get("frontmatter")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str()),
        Some("windows")
    );
    let desc = skill
        .get("frontmatter")
        .and_then(|f| f.get("description"))
        .and_then(|v| v.as_str())
        .unwrap();
    assert!(!desc.is_empty() && desc.len() <= 1024);

    let resources = skill
        .get("resources")
        .and_then(|v| v.as_array())
        .expect("resources array");
    // sorted by uri
    let uris: Vec<&str> = resources
        .iter()
        .filter_map(|r| r.get("uri").and_then(|v| v.as_str()))
        .collect();
    let mut sorted = uris.clone();
    sorted.sort();
    assert_eq!(uris, sorted);
    // includes SKILL.md
    assert!(uris.contains(&"skill://windows/SKILL.md"));

    // digest/size check for SKILL.md entry
    let skillmd_entry = resources
        .iter()
        .find(|r| r.get("uri").and_then(|v| v.as_str()) == Some("skill://windows/SKILL.md"))
        .unwrap();
    let digest = skillmd_entry
        .get("digest")
        .and_then(|v| v.as_str())
        .unwrap();
    assert!(digest.starts_with("sha256:"));
    assert_eq!(digest.len(), "sha256:".len() + 64);
    // lowercase hex
    assert!(digest[7..]
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));

    // resources/read returns same bytes
    let resp = s.send(&frame(
        3,
        "resources/read",
        serde_json::json!({"uri": "skill://windows/SKILL.md"}),
    ));
    let result = resp.get("result").expect("resources/read result");
    let contents = result
        .get("contents")
        .and_then(|v| v.as_array())
        .expect("contents");
    assert_eq!(contents.len(), 1);
    let text = contents[0].get("text").and_then(|v| v.as_str()).unwrap();
    assert_eq!(
        contents[0].get("mimeType").and_then(|v| v.as_str()),
        Some("text/markdown")
    );
    // size matches
    let size = skillmd_entry.get("size").and_then(|v| v.as_u64()).unwrap() as usize;
    assert_eq!(text.len(), size);

    // skills/get
    let resp = s.send(&frame(
        4,
        "skills/get",
        serde_json::json!({"uri": "skill://windows/SKILL.md"}),
    ));
    let result = resp.get("result").expect("skills/get result");
    assert_eq!(
        result.get("cacheScope").and_then(|v| v.as_str()),
        Some("private")
    );
    let gskill = result.get("skill").expect("skill");
    assert_eq!(
        gskill.get("uri").and_then(|v| v.as_str()),
        Some("skill://windows/SKILL.md")
    );

    // unknown skill uri -> -32602
    let resp = s.send(&frame(
        5,
        "skills/get",
        serde_json::json!({"uri": "skill://nope/SKILL.md"}),
    ));
    assert_eq!(
        resp.get("error")
            .and_then(|e| e.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32602)
    );

    // unknown resources/read uri -> -32602
    let resp = s.send(&frame(
        6,
        "resources/read",
        serde_json::json!({"uri": "skill://nope/SKILL.md"}),
    ));
    assert_eq!(
        resp.get("error")
            .and_then(|e| e.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32602)
    );
}
