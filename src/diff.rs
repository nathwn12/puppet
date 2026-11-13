//! Pixel-diff tool: turn "looks right" into "only the intended pixels
//! changed" by comparing two captures pixel by pixel.
//!
//! Each side is either a PNG file (`{path:"<png>"}`) or a live capture
//! target (`{target|window, display, region}` with the same selector
//! grammar as `screenshot`, captured side-effect-free at native pixels).
//! Both sides must land on the SAME pixel dimensions: a mismatch is a
//! typed `size_mismatch` naming both sizes, never a silent scale. The
//! per-pixel rule is the port of
//! `Q:\PROJECTS\PERSONAL\gladius\src\visual.rs` (`png_diff`/`DiffOpts`)
//! in `crate::cap` (per-channel delta above `threshold` counts, union
//! bbox, ratio); the returned image marks changed pixels solid red over a
//! dimmed greyscale copy of `base`. Read-only: no activation, no input.

use crate::cap::{decode_png, diff_image, finalize, Img};
use crate::screen::{dispatch_screenshot, ShotArgs};
use base64::Engine as _;
use serde_json::{Map, Value};

pub struct DiffArgs {
    pub base: Value,
    pub next: Value,
    pub threshold: i64,
    pub max_px: Option<i64>,
    pub out: Option<String>,
    pub bbox: bool,
    /// `[[left,top,right,bottom],…]` in native operand pixels.
    pub mask: Option<Value>,
}

/// Parse the `mask` input into native-pixel rects. Shape errors are typed
/// `bad_region`, the same code the frame check uses, so a caller has one
/// failure to handle for the whole input.
fn parse_mask(v: &Value) -> Result<Vec<[i64; 4]>, String> {
    let arr = v.as_array().ok_or_else(|| {
        format!("error: bad_region: mask must be [[left,top,right,bottom],...] (got {v})")
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let r = item.as_array().ok_or_else(|| {
            format!("error: bad_region: a mask rect must be [left,top,right,bottom] (got {item})")
        })?;
        if r.len() != 4 {
            return Err(format!(
                "error: bad_region: a mask rect must have 4 values, got {} in {item}",
                r.len()
            ));
        }
        let mut rect = [0i64; 4];
        for (i, x) in r.iter().enumerate() {
            rect[i] = x.as_i64().ok_or_else(|| {
                format!("error: bad_region: a mask rect value must be an integer (got {x})")
            })?;
        }
        out.push(rect);
    }
    Ok(out)
}

/// Decode one `diff` operand to RGB pixels and its origin. `{path}` reads a
/// PNG file and has no origin (`null`); anything else is captured through
/// the screenshot path at native pixels (lossless PNG, no grid, no
/// elements) and decoded back, so the window lock, region crop and display
/// routing stay exactly the ones `screenshot` proves, and the capture's own
/// `source{x,y,w,h,kind}` comes back with the pixels instead of being
/// discarded.
fn resolve_operand(side: &str, v: &Value) -> Result<(Img, Value), String> {
    if let Value::Object(o) = v {
        if let Some(p) = o.get("path") {
            let path = p.as_str().ok_or_else(|| {
                format!("error: bad_diff: {side} path must be a string (got {p})")
            })?;
            let bytes = std::fs::read(path)
                .map_err(|e| format!("error: bad_diff: cannot read {side} {path}: {e}"))?;
            let img = decode_png(&bytes).map_err(|e| format!("error: bad_diff: {side}: {e}"))?;
            return Ok((img, Value::Null));
        }
    }
    // Capture target: a bare string is a window selector, an object
    // carries {target|window, display, region} like screenshot.
    let shot = match v {
        Value::String(s) => ShotArgs {
            display: -1,
            region: None,
            window: Some(s.clone()),
            max_px: None,
            fmt: "png".to_string(),
            quality: None,
            grid: false,
            allow_crop_fallback: false,
            elements: false,
            out: None,
            target: None,
        },
        Value::Object(o) => {
            let window = match o.get("window") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(x) => {
                    return Err(format!(
                        "error: bad_diff: {side} window must be a string (got {x})"
                    ));
                }
            };
            let target = match o.get("target") {
                None | Some(Value::Null) => None,
                Some(t @ (Value::Object(_) | Value::String(_))) => Some(t.clone()),
                Some(x) => {
                    return Err(format!(
                        "error: bad_diff: {side} target must be an object or selector string (got {x})"
                    ));
                }
            };
            let display = match o.get("display") {
                None | Some(Value::Null) => -1,
                Some(Value::Number(n)) => n.as_i64().ok_or_else(|| {
                    format!("error: bad_diff: {side} display must be an integer (got {n})")
                })?,
                Some(x) => {
                    return Err(format!(
                        "error: bad_diff: {side} display must be an integer (got {x})"
                    ));
                }
            };
            let region = match o.get("region") {
                None | Some(Value::Null) => None,
                Some(Value::Array(a)) => Some(a.clone()),
                Some(x) => {
                    return Err(format!(
                        "error: bad_diff: {side} region must be [left,top,right,bottom] (got {x})"
                    ));
                }
            };
            ShotArgs {
                display,
                region,
                window,
                max_px: None,
                fmt: "png".to_string(),
                quality: None,
                grid: false,
                allow_crop_fallback: false,
                elements: false,
                out: None,
                target,
            }
        }
        x => {
            return Err(format!(
                "error: bad_diff: {side} must be {{path:\"<png>\"}} or a capture target (got {x})"
            ));
        }
    };
    let reply: Value = serde_json::from_str(&dispatch_screenshot(&shot))
        .map_err(|e| format!("error: bad_diff: {side}: capture reply unparseable: {e}"))?;
    if reply.get("isError") == Some(&Value::from(true)) {
        let detail = reply
            .get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join(" | ")
            })
            .unwrap_or_else(|| "capture failed".to_string());
        return Err(format!("error: bad_diff: {side}: {detail}"));
    }
    let b64 = reply
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|blocks| {
            blocks
                .iter()
                .find_map(|b| b.get("data").and_then(|d| d.as_str()))
        })
        .ok_or_else(|| format!("error: bad_diff: {side}: capture returned no image"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("error: bad_diff: {side}: capture bytes are not base64: {e}"))?;
    let img = decode_png(&bytes).map_err(|e| format!("error: bad_diff: {side}: {e}"))?;
    let source = reply.get("source").cloned().unwrap_or(Value::Null);
    Ok((img, source))
}

fn err_payload(items: Vec<(&str, Value)>) -> String {
    let mut m = Map::new();
    m.insert("ok".to_string(), Value::from(false));
    for (k, v) in items {
        m.insert(k.to_string(), v);
    }
    crate::wrapped_text(&crate::py_dumps(&Value::Object(m)))
}

/// The `image{w,h,format,quality}` block for the bytes a diff returns, with
/// the same shape `screenshot` uses (`quality` is null unless the format
/// carries a quality at all).
fn image_object(iw: i64, ih: i64, fmt: &str, quality: i64) -> Value {
    let mut m = Map::new();
    m.insert("w".to_string(), Value::from(iw));
    m.insert("h".to_string(), Value::from(ih));
    m.insert("format".to_string(), Value::from(fmt.to_string()));
    m.insert(
        "quality".to_string(),
        if fmt == "jpeg" {
            Value::from(quality)
        } else {
            Value::Null
        },
    );
    Value::Object(m)
}

pub fn dispatch_diff(a: &DiffArgs) -> String {
    if !(0..=255).contains(&a.threshold) {
        return err_payload(vec![(
            "error",
            Value::from(format!(
                "error: bad_diff: threshold must be 0-255 (got {})",
                a.threshold
            )),
        )]);
    }
    let thr = a.threshold as u8;
    let (base, base_source) = match resolve_operand("base", &a.base) {
        Ok(v) => v,
        Err(e) => return err_payload(vec![("error", Value::from(e))]),
    };
    let (next, next_source) = match resolve_operand("next", &a.next) {
        Ok(v) => v,
        Err(e) => return err_payload(vec![("error", Value::from(e))]),
    };
    // W3.3: the mask is validated against the operand frame — a rect that
    // leaves it is a typed bad_region, never a silent clamp — and it is
    // applied before anything is counted.
    let mask = match a.mask.as_ref() {
        None => None,
        Some(v) => {
            match parse_mask(v).and_then(|rects| crate::cap::Mask::new(&rects, base.w, base.h)) {
                Ok(m) => Some(m),
                Err(e) => {
                    return err_payload(vec![
                        ("code", Value::from("bad_region")),
                        ("error", Value::from(e)),
                    ])
                }
            }
        }
    };
    let d = match crate::cap::pixel_diff(&base, &next, thr, mask.as_ref()) {
        Ok(d) => d,
        Err(e) => {
            // Typed refusal: both sizes named, no silent scale. The images
            // never meet, so there is no diff PNG to write or return.
            return err_payload(vec![
                ("code", Value::from("size_mismatch")),
                ("error", Value::from(e)),
                (
                    "base",
                    Value::from({
                        let mut m = Map::new();
                        m.insert("w".to_string(), Value::from(base.w as i64));
                        m.insert("h".to_string(), Value::from(base.h as i64));
                        m
                    }),
                ),
                (
                    "next",
                    Value::from({
                        let mut m = Map::new();
                        m.insert("w".to_string(), Value::from(next.w as i64));
                        m.insert("h".to_string(), Value::from(next.h as i64));
                        m
                    }),
                ),
            ]);
        }
    };
    let mut payload = Map::new();
    payload.insert("ok".to_string(), Value::from(true));
    payload.insert("changed".to_string(), Value::from(d.changed));
    payload.insert("total".to_string(), Value::from(d.total));
    payload.insert("ratio".to_string(), Value::from(d.ratio));
    payload.insert("masked_px".to_string(), Value::from(d.masked));
    payload.insert(
        "bbox".to_string(),
        match (a.bbox, d.bbox) {
            (true, Some(b)) => Value::from(vec![
                Value::from(b[0]),
                Value::from(b[1]),
                Value::from(b[2]),
                Value::from(b[3]),
            ]),
            _ => Value::Null,
        },
    );
    payload.insert("identical".to_string(), Value::from(d.identical));
    let mut base_o = Map::new();
    base_o.insert("w".to_string(), Value::from(base.w as i64));
    base_o.insert("h".to_string(), Value::from(base.h as i64));
    payload.insert("base".to_string(), Value::from(base_o));
    let mut next_o = Map::new();
    next_o.insert("w".to_string(), Value::from(next.w as i64));
    next_o.insert("h".to_string(), Value::from(next.h as i64));
    payload.insert("next".to_string(), Value::from(next_o));
    payload.insert("threshold".to_string(), Value::from(a.threshold));
    // W3.1/A1: the counts and the box are native operand pixels; `image` and
    // `scale` describe the bytes actually returned, and the operand origins
    // say where those pixels sit on the screen (null for a file operand,
    // which has no origin).
    payload.insert("bbox_space".to_string(), Value::from("native"));
    payload.insert("base_source".to_string(), base_source);
    payload.insert("next_source".to_string(), next_source);
    if d.identical {
        // No changed pixels, no diff image: an all-dimmed file still lands
        // when `out` is set so the path contract holds on both outcomes.
        if let Some(path) = a.out.as_deref() {
            let flat = Img {
                w: base.w,
                h: base.h,
                px: {
                    let n = base.w as usize * base.h as usize;
                    let mut px = vec![0u8; n * 3];
                    for (i, chunk) in px.chunks_exact_mut(3).enumerate() {
                        let dim = (crate::cap::gray_of(
                            base.px[3 * i],
                            base.px[3 * i + 1],
                            base.px[3 * i + 2],
                        ) as u32
                            * 3
                            / 5) as u8;
                        chunk[0] = dim;
                        chunk[1] = dim;
                        chunk[2] = dim;
                    }
                    px
                },
            };
            let (data, iw, ih, scale, f, q) = match finalize(flat, a.max_px, "png", None, false) {
                Ok(v) => v,
                Err(e) => return err_payload(vec![("error", Value::from(e))]),
            };
            payload.insert("image".to_string(), image_object(iw, ih, &f, q));
            payload.insert("scale".to_string(), Value::from(scale));
            match crate::cap::write_bytes_out(path, &data) {
                Ok(()) => {
                    payload.insert(
                        "diff_out".to_string(),
                        Value::from(crate::cap::out_object(path, data.len(), &f)),
                    );
                }
                Err(e) => return err_payload(vec![("error", Value::from(e))]),
            }
        } else {
            payload.insert("diff_out".to_string(), Value::Null);
        }
        return crate::wrapped_text(&crate::py_dumps(&Value::Object(payload)));
    }
    // Changed: the red-over-dimmed visualization, long-edge clamped for
    // the wire (full-res pixels decided the counts above, never the
    // clamped ones).
    let viz = match diff_image(&base, &next, thr, mask.as_ref()) {
        Ok(im) => im,
        Err(e) => return err_payload(vec![("error", Value::from(e))]),
    };
    let (data, iw, ih, scale, f, q) = match finalize(viz, a.max_px, "png", None, false) {
        Ok(v) => v,
        Err(e) => return err_payload(vec![("error", Value::from(e))]),
    };
    payload.insert("image".to_string(), image_object(iw, ih, &f, q));
    payload.insert("scale".to_string(), Value::from(scale));
    if let Some(path) = a.out.as_deref() {
        match crate::cap::write_bytes_out(path, &data) {
            Ok(()) => {
                payload.insert(
                    "diff_out".to_string(),
                    Value::from(crate::cap::out_object(path, data.len(), &f)),
                );
            }
            Err(e) => return err_payload(vec![("error", Value::from(e))]),
        }
    } else {
        payload.insert("diff_out".to_string(), Value::Null);
    }
    let mime = if f == "png" {
        "image/png"
    } else {
        "image/jpeg"
    };
    let blocks = vec![
        crate::image_block(
            &base64::engine::general_purpose::STANDARD.encode(&data),
            mime,
        ),
        crate::text_block(&crate::py_dumps(&Value::Object(payload.clone()))),
    ];
    crate::screenshot_ok(&blocks, &Value::Object(payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn solid_img(w: i32, h: i32, r: u8, g: u8, b: u8) -> crate::cap::Img {
        let mut px = vec![0u8; w as usize * h as usize * 3];
        for i in 0..w as usize * h as usize {
            px[3 * i] = r;
            px[3 * i + 1] = g;
            px[3 * i + 2] = b;
        }
        crate::cap::Img { w, h, px }
    }

    fn allowed_types(prop: &Value) -> Vec<String> {
        let mut out = Vec::new();
        let mut push_t = |t: &Value| match t {
            Value::String(s) => out.push(s.clone()),
            Value::Array(a) => {
                for x in a {
                    if let Some(s) = x.as_str() {
                        out.push(s.to_string());
                    }
                }
            }
            _ => {}
        };
        if let Some(t) = prop.get("type") {
            push_t(t);
        }
        for key in ["anyOf", "oneOf"] {
            if let Some(arr) = prop.get(key).and_then(|v| v.as_array()) {
                for variant in arr {
                    if let Some(t) = variant.get("type") {
                        push_t(t);
                    }
                }
            }
        }
        out
    }

    fn actual_type(v: &Value) -> &'static str {
        match v {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(n) => {
                if n.is_i64() || n.is_u64() {
                    "integer"
                } else {
                    "number"
                }
            }
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }

    fn assert_payload_valid_against_output_schema(payload: &Value) {
        let tool: Value =
            serde_json::from_str(crate::schemas::TOOL_DIFF).expect("TOOL_DIFF parses");
        let schema = tool.get("outputSchema").expect("diff outputSchema present");
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("outputSchema properties");
        let required: Vec<String> = schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        for r in &required {
            assert!(
                payload.get(r).is_some(),
                "missing required field {r}, payload: {payload}"
            );
        }
        let obj = payload.as_object().expect("payload is an object");
        for (k, v) in obj {
            let Some(prop) = props.get(k) else {
                continue;
            };
            let allowed = allowed_types(prop);
            let actual = actual_type(v);
            let ok = allowed.iter().any(|a| a == actual)
                || (actual == "integer" && allowed.iter().any(|a| a == "number"));
            assert!(
                ok,
                "field `{k}` value {v} (type {actual}) not allowed by schema {prop}"
            );
        }
    }

    fn write_png(path: &std::path::Path, img: &crate::cap::Img) {
        let bytes = crate::cap::encode_png(img).expect("encode png");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, bytes).expect("write png");
    }

    fn path_arg(path: &std::path::Path) -> Value {
        let mut m = Map::new();
        m.insert(
            "path".to_string(),
            Value::from(path.to_string_lossy().to_string()),
        );
        Value::Object(m)
    }

    fn structured_of_dispatch(a: &DiffArgs) -> Value {
        let outer_s = dispatch_diff(a);
        let outer: Value = serde_json::from_str(&outer_s).expect("outer envelope parses");
        assert_eq!(
            outer.get("isError"),
            Some(&Value::from(false)),
            "diff must succeed, got: {outer}"
        );
        outer
            .get("structuredContent")
            .cloned()
            .expect("structuredContent present")
    }

    /// The identical-case response (`bbox: null`, `diff_out: null`) and the
    /// differing-case response (real bbox) must both validate against the
    /// tool's own declared `outputSchema`.
    #[test]
    fn diff_payload_matches_declared_output_schema() {
        let dir = std::env::temp_dir().join(format!("puppet-diff-schema-{}", std::process::id()));
        let base_p = dir.join("base.png");
        let next_p = dir.join("next.png");
        let alt_p = dir.join("alt.png");

        // Identical: same pixels in both files.
        let solid = solid_img(10, 10, 60, 120, 200);
        write_png(&base_p, &solid);
        write_png(&next_p, &solid);
        let sc = structured_of_dispatch(&DiffArgs {
            base: path_arg(&base_p),
            next: path_arg(&next_p),
            threshold: 0,
            max_px: None,
            out: None,
            bbox: true,
            mask: None,
        });
        assert_eq!(sc.get("identical"), Some(&Value::from(true)));
        assert_eq!(sc.get("changed"), Some(&Value::from(0)));
        assert_eq!(sc.get("bbox"), Some(&Value::Null));
        assert_payload_valid_against_output_schema(&sc);

        // Differing: one pixel moved, so the bbox is a real 4-tuple.
        let mut altered = solid_img(10, 10, 60, 120, 200);
        let p = (5 * 10 + 3) as usize;
        altered.px[3 * p] = 255;
        altered.px[3 * p + 1] = 0;
        altered.px[3 * p + 2] = 0;
        write_png(&alt_p, &altered);
        let sc2 = structured_of_dispatch(&DiffArgs {
            base: path_arg(&base_p),
            next: path_arg(&alt_p),
            threshold: 0,
            max_px: None,
            out: None,
            bbox: true,
            mask: None,
        });
        assert_eq!(sc2.get("identical"), Some(&Value::from(false)));
        assert_eq!(sc2.get("changed"), Some(&Value::from(1)));
        let bbox = sc2.get("bbox").expect("bbox present");
        assert!(
            bbox.is_array() && bbox.as_array().map(|a| a.len()) == Some(4),
            "differing bbox is [x,y,w,h], got: {bbox}"
        );
        assert_payload_valid_against_output_schema(&sc2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
