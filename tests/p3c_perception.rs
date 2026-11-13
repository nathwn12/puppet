//! W3 perception truth: what a reply says about the pixels it returned.
//!
//! The synthetic-PNG cases need no desktop and run everywhere; the capture
//! cases are gated by `PUPPET_LIVE_DESKTOP=1` and drive only the dedicated
//! fixture app, never an owner window.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use base64::Engine as _;
use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(60);

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

    fn result(&mut self, id: i64, name: &str, arguments: Value) -> Value {
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
        let resp: Value = self
            .rx
            .recv_timeout(TIMEOUT)
            .ok()
            .map(|l| serde_json::from_str(&l).expect("response is JSON"))
            .expect("response in time");
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

// ---------------------------------------------------------------- synthetic PNGs

/// One RGB8 PNG written with the same colour type the server decodes.
fn write_png(path: &Path, w: u32, h: u32, px: &[u8]) {
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().expect("png header");
    writer.write_image_data(px).expect("png data");
}

fn read_png(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().expect("png info");
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let info = reader.next_frame(&mut buf).expect("png frame");
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

fn rgb_at(px: &[u8], w: u32, x: u32, y: u32) -> (u8, u8, u8) {
    let i = ((y * w + x) * 3) as usize;
    (px[i], px[i + 1], px[i + 2])
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("puppet-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn path_operand(p: &Path) -> Value {
    serde_json::json!({"path": p.to_string_lossy().to_string()})
}

fn sc_of(r: &Value, what: &str) -> Value {
    r.get("structuredContent")
        .cloned()
        .unwrap_or_else(|| panic!("{what} must carry structuredContent, got: {r}"))
}

/// The base64 image block of a reply, if it returned one.
fn image_block_bytes(r: &Value) -> Option<Vec<u8>> {
    r.get("content")
        .and_then(|c| c.as_array())
        .and_then(|blocks| {
            blocks
                .iter()
                .find_map(|b| b.get("data").and_then(|d| d.as_str()))
        })
        .map(|b64| {
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .expect("image block is base64")
        })
}

/// SPEC W3.1 + AMENDMENT-01 A1/A5: a diff reports the image it returned, the
/// scale that produced it, and that its box is native — so a caller can put
/// the box on the image and read the changed pixel there.
#[test]
fn a_diff_reports_the_scale_and_image_it_returned() {
    let dir = temp_dir("diff-scale");
    let base_p = dir.join("base.png");
    let next_p = dir.join("next.png");
    const W: u32 = 2000;
    const H: u32 = 2000;
    let grey = vec![128u8; (W * H * 3) as usize];
    let mut altered = grey.clone();
    // One 4x4 block at (1000,1200), solid red.
    for y in 1200..1204u32 {
        for x in 1000..1004u32 {
            let i = ((y * W + x) * 3) as usize;
            altered[i] = 255;
            altered[i + 1] = 0;
            altered[i + 2] = 0;
        }
    }
    write_png(&base_p, W, H, &grey);
    write_png(&next_p, W, H, &altered);

    let mut s = Session::spawn();
    let r = s.result(
        1,
        "diff",
        serde_json::json!({"base": path_operand(&base_p), "next": path_operand(&next_p)}),
    );
    assert_ne!(
        r.get("isError"),
        Some(&Value::from(true)),
        "diff must succeed, got: {r}"
    );
    let sc = sc_of(&r, "diff");
    assert_eq!(sc.get("changed"), Some(&Value::from(16)), "one 4x4 block");
    assert_eq!(
        sc.get("bbox"),
        Some(&serde_json::json!([1000, 1200, 4, 4])),
        "the box is the native block"
    );
    assert_eq!(
        sc.get("bbox_space").and_then(|v| v.as_str()),
        Some("native"),
        "the box must say which space it is in, got: {sc}"
    );
    let scale = sc
        .get("scale")
        .and_then(|v| v.as_f64())
        .unwrap_or_else(|| panic!("diff must report the applied scale, got: {sc}"));
    assert!(scale < 1.0, "the long-edge clamp must bite, got {scale}");
    let image = sc
        .get("image")
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| panic!("diff must report the image it returned, got: {sc}"));
    let iw = image.get("w").and_then(|v| v.as_i64()).expect("image.w");
    let ih = image.get("h").and_then(|v| v.as_i64()).expect("image.h");
    assert_eq!(
        iw,
        (W as f64 * scale).round() as i64,
        "image.w must be the scaled native width, got {iw} at scale {scale}"
    );
    assert_eq!(ih, (H as f64 * scale).round() as i64, "image.h");
    assert_eq!(
        image.get("format").and_then(|v| v.as_str()),
        Some("png"),
        "the diff image is lossless"
    );
    assert_eq!(
        image.get("quality"),
        Some(&Value::Null),
        "quality has no meaning for a png diff image"
    );
    // A file operand has no origin on the screen: null, never an invented one.
    assert_eq!(sc.get("base_source"), Some(&Value::Null));
    assert_eq!(sc.get("next_source"), Some(&Value::Null));

    // The mapped read: native box * scale lands on the returned image.
    let bytes = image_block_bytes(&r).expect("a changed diff returns its image");
    let (pw, ph, px) = read_png(&bytes);
    assert_eq!((pw as i64, ph as i64), (iw, ih), "the block is image.w/h");
    let mx = (1000.0 * scale).floor() as u32;
    let my = (1200.0 * scale).floor() as u32;
    let (r0, g0, b0) = rgb_at(&px, pw, mx, my);
    assert!(
        r0 > 150 && r0 > 2 * g0.max(b0),
        "the pixel at floor(bbox*scale) = ({mx},{my}) must be red, got ({r0},{g0},{b0})"
    );

    // Identical: no image is returned, so no image and no scale are claimed.
    let r = s.result(
        2,
        "diff",
        serde_json::json!({"base": path_operand(&base_p), "next": path_operand(&base_p)}),
    );
    let sc = sc_of(&r, "identical diff");
    assert_eq!(sc.get("identical"), Some(&Value::from(true)));
    assert!(
        sc.get("image").is_none(),
        "an identical diff returns no image block, so it claims none, got: {sc}"
    );
    assert!(
        sc.get("scale").is_none(),
        "no image means no scale to report, got: {sc}"
    );
    assert_eq!(
        sc.get("bbox_space").and_then(|v| v.as_str()),
        Some("native")
    );
    assert_eq!(sc.get("total"), Some(&Value::from((W * H) as i64)));

    let _ = std::fs::remove_dir_all(&dir);
}
