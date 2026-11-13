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

// ---------------------------------------------------------------- live fixture

fn live() -> bool {
    if std::env::var("PUPPET_LIVE_DESKTOP").as_deref() == Ok("1") {
        true
    } else {
        println!(
            "SKIP p3c_perception live case: set PUPPET_LIVE_DESKTOP=1 to run \
             (needs an interactive Windows session)"
        );
        false
    }
}

/// Kill leftover fixture processes from a previous (possibly panicked) run.
/// Scoped strictly to our own fixture executable plus the exact test window
/// title — never touches an owner app.
fn kill_stale_fixtures(title: &str) {
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

/// Owning handle for one fixture process: `Drop` kills and reaps, so a panic
/// still leaves no window behind for the next run.
struct Fixture {
    child: Child,
    title: String,
}

impl Fixture {
    /// The baseline window (one visible EDIT) unless `mode` names another.
    fn spawn(title: &str, mode: Option<&str>) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_puppet_fixture_app"));
        cmd.arg(title)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(m) = mode {
            cmd.arg(m);
        }
        let child = cmd.spawn().expect("spawn fixture app");
        Fixture {
            child,
            title: title.to_string(),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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

/// SPEC W3.2 (D4): a default capture is lossless, so two captures of a static
/// window are byte-identical and the reply says which format it used.
#[test]
fn default_captures_are_lossless() {
    if !live() {
        return;
    }
    kill_stale_fixtures("p3c-lossless");
    let fx = Fixture::spawn("p3c-lossless", None);
    // The fixture's first composited frame is a startup race the harness
    // waits out (src/fixture.rs `await_ready`), and a spawned app has the
    // same race from outside: let it paint before the pair is measured.
    std::thread::sleep(Duration::from_millis(500));
    let mut s = Session::spawn();
    let shot = serde_json::json!({"window": fx.title});
    let first = s.result(1, "screenshot", shot.clone());
    let second = s.result(2, "screenshot", shot);
    assert_ne!(
        first.get("isError"),
        Some(&Value::from(true)),
        "a default capture of the fixture must succeed, got: {first}"
    );
    let sc = sc_of(&first, "default screenshot");
    let image = sc.get("image").expect("image block");
    assert_eq!(
        image.get("format").and_then(|v| v.as_str()),
        Some("png"),
        "fmt must default to png, got: {image}"
    );
    assert_eq!(
        image.get("quality"),
        Some(&Value::Null),
        "quality has no meaning for the default format, got: {image}"
    );
    let a = image_block_bytes(&first).expect("first capture carries its image");
    let b = image_block_bytes(&second).expect("second capture carries its image");
    assert_eq!(
        a,
        b,
        "two captures of a static window must be byte-identical ({} vs {} bytes)",
        a.len(),
        b.len()
    );
}

/// AMENDMENT-01 A2: a request that was altered announces itself, and a
/// default call carries no `warnings` key at all — the channel is for
/// requests the server changed, not for every reply.
#[test]
fn a_clamp_that_altered_the_request_says_so() {
    if !live() {
        return;
    }
    kill_stale_fixtures("p3c-clamp");
    let fx = Fixture::spawn("p3c-clamp", None);
    std::thread::sleep(Duration::from_millis(500));
    let mut s = Session::spawn();

    let r = s.result(
        1,
        "screenshot",
        serde_json::json!({"window": fx.title, "fmt": "jpeg", "quality": 500}),
    );
    let sc = sc_of(&r, "clamped screenshot");
    let w = sc
        .get("warnings")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("quality:500 must warn, got: {sc}"));
    assert_eq!(
        w.len(),
        1,
        "one warning for one altered request, got: {w:?}"
    );
    assert_eq!(
        w[0].get("code").and_then(|v| v.as_str()),
        Some("quality_clamped")
    );
    assert_eq!(w[0].get("field").and_then(|v| v.as_str()), Some("quality"));
    assert_eq!(w[0].get("requested"), Some(&Value::from(500)));
    assert_eq!(w[0].get("applied"), Some(&Value::from(100)));

    let r = s.result(
        2,
        "screenshot",
        serde_json::json!({"window": fx.title, "fmt": "png", "quality": 50}),
    );
    let sc = sc_of(&r, "png with quality");
    let w = sc
        .get("warnings")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("quality with fmt:png must warn, got: {sc}"));
    assert_eq!(w.len(), 1, "one warning, got: {w:?}");
    assert_eq!(
        w[0].get("code").and_then(|v| v.as_str()),
        Some("quality_ignored")
    );
    assert_eq!(w[0].get("requested"), Some(&Value::from(50)));
    assert_eq!(w[0].get("applied"), Some(&Value::Null));

    // The default path is silent: no key, not an empty array.
    let r = s.result(3, "screenshot", serde_json::json!({"window": fx.title}));
    let sc = sc_of(&r, "default screenshot");
    assert!(
        sc.get("warnings").is_none(),
        "a default capture must carry no warnings key, got: {sc}"
    );
}

/// SPEC W3.3: a diff can exclude a region, so a caret blink or a clock does
/// not force `changed` as the default verdict. Masked pixels leave the counts,
/// the ratio and the box, and are reported as `masked_px`.
#[test]
fn a_diff_can_exclude_a_region() {
    let dir = temp_dir("diff-mask");
    let base_p = dir.join("base.png");
    let next_p = dir.join("next.png");
    const W: u32 = 100;
    const H: u32 = 100;
    let grey = vec![90u8; (W * H * 3) as usize];
    let mut altered = grey.clone();
    // One 10x10 block at (20,30): the "caret".
    for y in 30..40u32 {
        for x in 20..30u32 {
            let i = ((y * W + x) * 3) as usize;
            altered[i] = 255;
            altered[i + 1] = 255;
            altered[i + 2] = 255;
        }
    }
    write_png(&base_p, W, H, &grey);
    write_png(&next_p, W, H, &altered);
    let mut s = Session::spawn();

    // The whole change is inside the mask: identical, nothing changed, and the
    // exclusion is visible in the reply.
    let r = s.result(
        1,
        "diff",
        serde_json::json!({
            "base": path_operand(&base_p), "next": path_operand(&next_p),
            "mask": [[20, 30, 30, 40]],
        }),
    );
    assert_ne!(
        r.get("isError"),
        Some(&Value::from(true)),
        "a masked diff must succeed, got: {r}"
    );
    let sc = sc_of(&r, "masked diff");
    assert_eq!(
        sc.get("identical"),
        Some(&Value::from(true)),
        "a change only inside the mask must not count, got: {sc}"
    );
    assert_eq!(sc.get("changed"), Some(&Value::from(0)));
    assert_eq!(
        sc.get("masked_px"),
        Some(&Value::from(100)),
        "the rect's area is reported as masked, got: {sc}"
    );
    assert_eq!(
        sc.get("total"),
        Some(&Value::from((W * H - 100) as i64)),
        "total is the unmasked total, got: {sc}"
    );
    assert_eq!(sc.get("ratio"), Some(&Value::from(0.0)));
    assert_eq!(sc.get("bbox"), Some(&Value::Null));
    assert!(
        image_block_bytes(&r).is_none(),
        "an identical diff returns no image block"
    );

    // Overlapping rects union silently: 100 + 100 - 25.
    let r = s.result(
        2,
        "diff",
        serde_json::json!({
            "base": path_operand(&base_p), "next": path_operand(&base_p),
            "mask": [[0, 0, 10, 10], [5, 5, 15, 15]],
        }),
    );
    let sc = sc_of(&r, "overlapping mask");
    assert_eq!(
        sc.get("masked_px"),
        Some(&Value::from(175)),
        "overlapping rects must union, not sum, got: {sc}"
    );
    assert_eq!(sc.get("total"), Some(&Value::from((W * H - 175) as i64)));

    // A change outside the mask still counts, in native pixels.
    let r = s.result(
        3,
        "diff",
        serde_json::json!({
            "base": path_operand(&base_p), "next": path_operand(&next_p),
            "mask": [[0, 0, 10, 10]],
        }),
    );
    let sc = sc_of(&r, "masked elsewhere");
    assert_eq!(sc.get("identical"), Some(&Value::from(false)));
    assert_eq!(sc.get("changed"), Some(&Value::from(100)));
    assert_eq!(
        sc.get("bbox"),
        Some(&serde_json::json!([20, 30, 10, 10])),
        "the box stays native and excludes the mask, got: {sc}"
    );

    // A rect outside the frame is a typed refusal, never a silent clamp.
    let r = s.result(
        4,
        "diff",
        serde_json::json!({
            "base": path_operand(&base_p), "next": path_operand(&next_p),
            "mask": [[90, 90, 120, 120]],
        }),
    );
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(true)),
        "a mask rect outside the frame must refuse, got: {r}"
    );
    let sc = sc_of(&r, "mask outside frame");
    assert_eq!(
        sc.get("code").and_then(|v| v.as_str()),
        Some("bad_region"),
        "the refusal must be typed bad_region, got: {sc}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The joined text of a tool result's text blocks (a refusal carries its
/// detail there).
fn result_text(r: &Value) -> String {
    r.get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .unwrap_or_default()
}

/// SPEC W3.4: a successful capture reports the blank-frame measurement, so a
/// frame with a black region is visibly measured content; a fully blank frame
/// still refuses, and the refusal carries the measurement that refused it.
#[test]
fn a_capture_reports_the_blank_frame_measurement() {
    if !live() {
        return;
    }
    kill_stale_fixtures("p3c-blackrect");
    let fx = Fixture::spawn("p3c-blackrect", Some("blackrect"));
    std::thread::sleep(Duration::from_millis(500));
    let mut s = Session::spawn();
    let r = s.result(1, "screenshot", serde_json::json!({"window": fx.title}));
    assert_ne!(
        r.get("isError"),
        Some(&Value::from(true)),
        "content with a black region is a legal frame, got: {}",
        result_text(&r)
    );
    let sc = sc_of(&r, "blackrect screenshot");
    let bc = sc
        .get("capture")
        .and_then(|c| c.get("blank_check"))
        .unwrap_or_else(|| panic!("every capture reports blank_check, got: {sc}"));
    assert_eq!(
        bc.get("verdict").and_then(|v| v.as_str()),
        Some("content"),
        "a partially black frame is content, got: {bc}"
    );
    let max = bc.get("max_channel").and_then(|v| v.as_i64()).unwrap_or(-1);
    assert!(
        max >= 8,
        "the measured maximum must clear the threshold, got: {bc}"
    );
    assert_eq!(
        bc.get("threshold"),
        Some(&Value::from(8)),
        "the threshold is reported with the measurement, got: {bc}"
    );

    // A window that paints nothing at all still refuses, and says what it
    // measured.
    kill_stale_fixtures("p3c-blank");
    let blank = Fixture::spawn("p3c-blank", Some("blank"));
    std::thread::sleep(Duration::from_millis(500));
    let r = s.result(2, "screenshot", serde_json::json!({"window": blank.title}));
    assert_eq!(
        r.get("isError"),
        Some(&Value::from(true)),
        "an all-black frame must still refuse, got: {r}"
    );
    let text = result_text(&r);
    assert!(
        text.contains("capture_unavailable"),
        "the refusal keeps its typed code, got: {text}"
    );
    assert!(
        text.contains("blank_check{max_channel=0,verdict=full_blank,threshold=8}"),
        "the refusal must carry the measurement that refused it, got: {text}"
    );

    // The desktop path reports the measurement too: every successful capture
    // does, whichever backend produced the frame.
    let r = s.result(
        3,
        "screenshot",
        serde_json::json!({"display": -1, "region": [0, 0, 32, 32]}),
    );
    let sc = sc_of(&r, "region capture");
    let bc = sc
        .get("capture")
        .and_then(|c| c.get("blank_check"))
        .unwrap_or_else(|| panic!("a region capture reports blank_check too, got: {sc}"));
    assert_eq!(
        bc.get("threshold"),
        Some(&Value::from(8)),
        "the threshold travels with the measurement, got: {bc}"
    );
    assert!(
        bc.get("max_channel").and_then(|v| v.as_i64()).is_some(),
        "the measurement is a number, got: {bc}"
    );
    let verdict = bc.get("verdict").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        verdict == "content" || verdict == "full_blank",
        "the verdict is one of the two the contract names, got: {bc}"
    );
}
