include!(concat!(env!("OUT_DIR"), "/skills_generated.rs"));

use serde_json::{json, Value};

pub fn files() -> &'static [EmbeddedSkillFile] {
    SKILL_FILES
}

pub fn find_file(uri: &str) -> Option<&'static EmbeddedSkillFile> {
    SKILL_FILES.iter().find(|f| f.uri == uri)
}

fn folder_of(uri: &str) -> Option<&str> {
    let rest = uri.strip_prefix("skill://")?;
    let idx = rest.find('/')?;
    Some(&rest[..idx])
}

pub fn skill_folders() -> Vec<String> {
    let mut folders: Vec<String> = Vec::new();
    for f in SKILL_FILES {
        if let Some(folder) = folder_of(f.uri) {
            if !folders.iter().any(|x| x == folder) {
                folders.push(folder.to_string());
            }
        }
    }
    folders.sort();
    folders
}

fn skill_md_bytes(folder: &str) -> Option<&'static [u8]> {
    let uri = format!("skill://{}/SKILL.md", folder);
    find_file(&uri).map(|f| f.bytes)
}

fn parse_frontmatter(bytes: &[u8]) -> (String, String) {
    let text = String::from_utf8_lossy(bytes);
    if !text.starts_with("---\n") {
        return (String::new(), String::new());
    }
    let rest = &text[4..];
    let end = rest.find("\n---").unwrap_or(rest.len());
    let yaml = &rest[..end];
    let mut name = String::new();
    let mut description = String::new();
    for line in yaml.lines() {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            let val = unquote(v.trim());
            match key {
                "name" => name = val,
                "description" => description = val,
                _ => {}
            }
        }
    }
    (name, description)
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

pub fn skill_resources(folder: &str) -> Vec<Value> {
    let prefix = format!("skill://{}/", folder);
    let mut out: Vec<Value> = Vec::new();
    for f in SKILL_FILES {
        if f.uri.starts_with(&prefix) {
            let tail = &f.uri[prefix.len()..];
            if tail.is_empty() || tail.contains("..") {
                continue;
            }
            out.push(json!({
                "uri": f.uri,
                "digest": f.digest,
                "size": f.size,
            }));
        }
    }
    out.sort_by(|a, b| {
        a["uri"]
            .as_str()
            .unwrap_or("")
            .cmp(b["uri"].as_str().unwrap_or(""))
    });
    out
}

pub fn skill_object(folder: &str) -> Option<Value> {
    let uri = format!("skill://{}/SKILL.md", folder);
    let bytes = skill_md_bytes(folder)?;
    let (name, description) = parse_frontmatter(bytes);
    let resources = skill_resources(folder);
    Some(json!({
        "uri": uri,
        "frontmatter": {
            "name": name,
            "description": description,
        },
        "resources": resources,
    }))
}

pub fn list_skills_value() -> Value {
    let mut skills: Vec<Value> = Vec::new();
    for folder in skill_folders() {
        if let Some(obj) = skill_object(&folder) {
            skills.push(obj);
        }
    }
    skills.sort_by(|a, b| {
        a["uri"]
            .as_str()
            .unwrap_or("")
            .cmp(b["uri"].as_str().unwrap_or(""))
    });
    json!({
        "skills": skills,
        "ttlMs": 0,
        "cacheScope": "private",
    })
}

pub fn list_skills_result() -> String {
    serde_json::to_string(&list_skills_value()).unwrap()
}

pub fn get_skill_value(uri: &str) -> Option<Value> {
    let folder = uri.strip_prefix("skill://")?.split('/').next()?;
    let expected = format!("skill://{}/SKILL.md", folder);
    if uri != expected {
        return None;
    }
    skill_object(folder)
}

pub fn get_skill_result(uri: &str) -> Option<String> {
    let skill = get_skill_value(uri)?;
    Some(
        serde_json::to_string(&json!({
            "skill": skill,
            "ttlMs": 0,
            "cacheScope": "private",
        }))
        .unwrap(),
    )
}

pub fn read_resource(uri: &str) -> Option<Value> {
    let f = find_file(uri)?;
    if let Ok(text) = std::str::from_utf8(f.bytes) {
        Some(json!({
            "contents": [{
                "uri": uri,
                "mimeType": "text/markdown",
                "text": text,
            }]
        }))
    } else {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(f.bytes);
        Some(json!({
            "contents": [{
                "uri": uri,
                "mimeType": "application/octet-stream",
                "blob": b64,
            }]
        }))
    }
}
