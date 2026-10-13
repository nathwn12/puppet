//! Provenance smoke check: the built binary must carry a commit stamp.
//!
//! Safe for the owner's live desktop: `--version` returns from `main` before
//! any Win32/desktop call and before the stdio pump starts, so this only
//! spawns the binary and reads its stdout.

#[test]
fn version_flag_reports_version_and_full_commit() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_puppet"))
        .arg("--version")
        .output()
        .expect("puppet.exe --version must run");
    assert!(out.status.success(), "exit status: {:?}", out.status);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.trim_end();
    let head = format!("puppet {} (", env!("CARGO_PKG_VERSION"));
    let stamp = line
        .strip_prefix(&head)
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or_else(|| panic!("unexpected --version output: {line:?}"));

    let sha = stamp.strip_suffix("-dirty").unwrap_or(stamp);
    assert_eq!(sha.len(), 40, "expected a full commit sha, got {sha:?}");
    assert!(
        sha.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "expected a lowercase hex commit sha, got {sha:?}"
    );
}
