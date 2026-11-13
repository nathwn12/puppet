//! W1.2 advice truth: a refusal may not recommend a door that is bricked.
//!
//! A refusal that ends "pass allow_foreground:true" sends the agent into a
//! loop: that flag is refused too, by policy, before anything is sent. The
//! same holds for a pixel fallback, which needs the pointer this server
//! never moves.
//!
//! The scan reads every `.rs` file under `src/` and requires each
//! recommendation of one of those doors to sit inside a refusal clause:
//! within 120 characters of `refus`, `no flag` or `no pixel fallback`.
//!
//! Two things are not recommendations, and the scan skips them the way
//! `tests/no_steal.rs` skips prose when it hunts call sites:
//!
//! - a comment line (`//`, `///`, `//!`), which no agent ever reads;
//! - code that names the flag without quoting it (`allow_cursor: true` in a
//!   struct literal), and a bare quoted needle (`"allow_cursor:true"` used
//!   by a test to assert the flag is *absent*), which are not advice.
//!
//! Fails on the pre-W1.2 tree: five refusals recommend a bricked door.

use std::path::{Path, PathBuf};

/// The forms that recommend a door, in source text.
const RECOMMENDATIONS: &[&str] = &[
    "allow_foreground:true",
    "allow_foreground: true",
    "allow_cursor:true",
    "allow_cursor: true",
    "allow_clipboard:true",
    "allow_clipboard: true",
    "fallback\\\":\\\"click",
    "fallback\":\"click",
];

/// The clauses that make a mention a refusal rather than advice.
const REFUSAL_MARKERS: &[&str] = &["refus", "no flag", "no pixel fallback"];

const WINDOW: usize = 120;

fn src_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            src_files(&p, out);
        } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

fn line_of(text: &str, at: usize) -> usize {
    text[..at].bytes().filter(|b| *b == b'\n').count() + 1
}

/// True when the byte at `at` sits inside a Rust string literal, raw or
/// escaped, so a wrapped string (`"… \` continued on the next line) counts
/// as prose. Comments are skipped: a quote in a comment opens nothing.
fn inside_string(text: &str, at: usize) -> bool {
    let b = text.as_bytes();
    let mut i = 0;
    let mut in_string = false;
    // Inside a raw string: how many `#` close it.
    let mut raw: Option<usize> = None;
    while i < at {
        if let Some(hashes) = raw {
            if b[i] == b'"'
                && b.len() > i + hashes
                && b[i + 1..i + 1 + hashes].iter().all(|c| *c == b'#')
            {
                raw = None;
                i += 1 + hashes;
                continue;
            }
            i += 1;
            continue;
        }
        if !in_string && b[i] == b'/' && i + 1 < at && b[i + 1] == b'/' {
            while i < at && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b[i] == b'"' {
            if !in_string {
                // `r"`, `r#"`, `r##"` … open a raw string.
                let mut j = i;
                let mut hashes = 0;
                while j > 0 && b[j - 1] == b'#' {
                    hashes += 1;
                    j -= 1;
                }
                if j > 0 && b[j - 1] == b'r' {
                    raw = Some(hashes);
                    i += 1;
                    continue;
                }
            }
            // An escaped quote does not close an escaped literal.
            let mut backslashes = 0;
            let mut j = i;
            while j > 0 && b[j - 1] == b'\\' {
                backslashes += 1;
                j -= 1;
            }
            if backslashes % 2 == 0 {
                in_string = !in_string;
            }
        }
        i += 1;
    }
    in_string || raw.is_some()
}

/// True when the token on this line is a bare quoted needle — the whole
/// literal, `"allow_cursor:true"` — which is a test asserting absence, not
/// a sentence telling a caller to pass it.
fn is_bare_needle(line: &str, token: &str) -> bool {
    let quoted = format!("\"{token}\"");
    line.contains(&quoted)
}

#[test]
fn no_refusal_recommends_a_bricked_door() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    src_files(&root, &mut files);
    assert!(!files.is_empty(), "src/ must contain Rust sources");

    let mut violations = Vec::new();
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let rel = path
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(path)
            .display()
            .to_string();
        for token in RECOMMENDATIONS {
            let mut from = 0;
            while let Some(i) = text[from..].find(token) {
                let at = from + i;
                let line_start = text[..at].rfind('\n').map(|n| n + 1).unwrap_or(0);
                let line_end = text[at..].find('\n').map(|n| at + n).unwrap_or(text.len());
                let line = &text[line_start..line_end];
                let quoted_prose = inside_string(&text, at) && !is_bare_needle(line, token);
                from = at + token.len();
                if !quoted_prose {
                    continue;
                }
                let lo = at.saturating_sub(WINDOW);
                let hi = (at + token.len() + WINDOW).min(text.len());
                let window = &text[lo..hi];
                if !REFUSAL_MARKERS.iter().any(|m| window.contains(m)) {
                    violations.push(format!(
                        "{rel}:{}: {token:?} recommends a door that always refuses",
                        line_of(&text, at)
                    ));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "every mention of a bricked door must be a refusal:\n{}",
        violations.join("\n")
    );
}
