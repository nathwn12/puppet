//! JSON-RPC stdio loop + per-tool validation (pydantic-shaped) + dispatch.

use crate::burst::do_burst;
use crate::cap::resolve_selector;
use crate::schemas::{
    TOOL_CURSOR, TOOL_DO, TOOL_ELEMENTS, TOOL_LIST_WINDOWS, TOOL_SCREENSHOT, TOOL_WINDOW,
};
use crate::screen::{dispatch_screenshot, ShotArgs};
use crate::shot::{compact_entry, element_model, elements_object};
use crate::winop::{do_cursor, do_list_windows, do_window, WinArgs};
use crate::{
    check_no_extra, coerce_int, coerce_str, display_monitors, err_result_text, missing_arg,
    opt_int, opt_str, py_dumps, req_bool, req_int, req_str, verr_text, wrapped_text,
    LATEST_VERSION, SUPPORTED_VERSIONS,
};
use serde_json::{Map, Value};
use std::io::{BufRead, Write};

fn write_result(out: &mut impl Write, id: &Value, result: &str) -> std::io::Result<()> {
    let ids = serde_json::to_string(id).unwrap();
    writeln!(
        out,
        "{{\"jsonrpc\":\"2.0\",\"id\":{ids},\"result\":{result}}}"
    )?;
    out.flush()
}

fn write_error(out: &mut impl Write, id: &Value, code: i64, msg: &str) -> std::io::Result<()> {
    let ids = serde_json::to_string(id).unwrap();
    let ms = serde_json::to_string(msg).unwrap();
    writeln!(
        out,
        "{{\"jsonrpc\":\"2.0\",\"id\":{ids},\"error\":{{\"code\":{code},\"message\":{ms}}}}}"
    )?;
    out.flush()
}

fn init_result(params: &Value) -> String {
    let v = params
        .get("protocolVersion")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    let echo = if SUPPORTED_VERSIONS.contains(&v) {
        v
    } else {
        LATEST_VERSION
    };
    format!(
        "{{\"protocolVersion\":{},\"capabilities\":{{\"logging\":{{}},\"prompts\":{{\"listChanged\":true}},\"resources\":{{\"subscribe\":false,\"listChanged\":true}},\"tools\":{{\"listChanged\":true}}}},\"serverInfo\":{{\"name\":\"puppet\",\"version\":\"4.3.0\"}},\"instructions\":{}}}",
        serde_json::to_string(echo).unwrap(),
        serde_json::to_string(INIT_INSTRUCTIONS).unwrap()
    )
}

/// Usage contract for a client/model that has never seen this server (gap
/// 3): the six tools in one line each, the coordinate rule with the
/// image-to-screen formula, batch + dry_run, and read-only-first on the
/// owner's real desktop.
const INIT_INSTRUCTIONS: &str = "puppet desktop-control MCP (Windows). Input hits the owner's real desktop: start every task with read-only calls (screenshot, elements, list_windows, cursor), batch work into one do burst per turn, then screenshot to verify.\nTools (one line each):\n- screenshot: capture the whole virtual desktop (display=-1), one monitor, a window, or a region; returns the image plus source{x,y,w,h}, scale, the monitor list, the cursor, and the image-to-screen formula. Coordinates are screen/virtual-desktop pixels: screen_x = source.x + round((img_x + 0.5) * source.w / image.w) (same for y).\n- list_windows: read-only visible top-level windows (hwnd/pid/exe/title/rects/elevated); never changes focus.\n- window: focus|move|resize|minimize|maximize|restore|close|topmost by selector or hwnd.\n- do: BURST tool: one call runs an ordered actions[] batch (move|click|down|up|drag|scroll|type|key|hotkey|wait|wait_for|focus_window|invoke|set_value); per-action ok, never aborts the rest. dry_run=true validates and reports without injecting anything. Coordinate actions take virtual-desktop pixels.\n- cursor: read-only cursor position plus monitor rects.\n- elements: read-only structured UI elements for a window (zero pixels, no focus change); each entry carries ref plus fp, a stable identity fingerprint. Prefer elements over pixel guessing; pass expect_fp to invoke/set_value to refuse stale refs instead of acting on the wrong control. When text is illegible, re-request the same window with a tight region plus max_px:null (full-resolution zoom).";

fn tools_list_result() -> String {
    format!("{{\"tools\":[{TOOL_SCREENSHOT},{TOOL_LIST_WINDOWS},{TOOL_WINDOW},{TOOL_DO},{TOOL_CURSOR},{TOOL_ELEMENTS}]}}")
}

fn call_window(args: &Map<String, Value>) -> String {
    let mut errs = Vec::new();
    check_no_extra(
        args,
        &mut errs,
        &["action", "title", "hwnd", "x", "y", "width", "height"],
    );
    let action = match args.get("action") {
        None => {
            errs.push(missing_arg("window", args, "action"));
            None
        }
        Some(Value::Null) => {
            errs.push(crate::VErr {
                loc: "action".to_string(),
                msg: "Input should be a valid string".to_string(),
                typ: "string_type".to_string(),
                iv: "None".to_string(),
                it: "NoneType".to_string(),
            });
            None
        }
        Some(v) => match coerce_str(v) {
            Ok(s) => Some(s),
            Err((msg, typ)) => {
                errs.push(crate::VErr {
                    loc: "action".to_string(),
                    msg,
                    typ,
                    iv: crate::render_iv(v),
                    it: crate::input_type_of(v).to_string(),
                });
                None
            }
        },
    };
    let title = opt_str(args, &mut errs, "title");
    let hwnd = opt_int(args, &mut errs, "hwnd");
    let x = opt_int(args, &mut errs, "x");
    let y = opt_int(args, &mut errs, "y");
    let width = opt_int(args, &mut errs, "width");
    let height = opt_int(args, &mut errs, "height");
    if !errs.is_empty() {
        return err_result_text(&verr_text("window", &errs));
    }
    do_window(&WinArgs {
        action: action.unwrap(),
        title: title.unwrap(),
        hwnd: hwnd.unwrap(),
        x: x.unwrap(),
        y: y.unwrap(),
        width: width.unwrap(),
        height: height.unwrap(),
    })
}

/// Zero-pixel structured enumeration: resolve the window selector, then reuse
/// the screenshot element walk (`shot::element_model`, the same object as
/// `shot::elements_block`) and return it as structured JSON.
fn call_elements(args: &Map<String, Value>) -> String {
    let mut errs = Vec::new();
    check_no_extra(args, &mut errs, &["title", "hwnd", "detail"]);
    let title = opt_str(args, &mut errs, "title");
    let hwnd = opt_int(args, &mut errs, "hwnd");
    let detail = req_bool(args, &mut errs, "detail");
    if !errs.is_empty() {
        return err_result_text(&verr_text("elements", &errs));
    }
    let (t, h, d) = (title.unwrap(), hwnd.unwrap(), detail.unwrap());
    // Same branches as `cap::resolve_window`, spelled through
    // `resolve_selector` directly: explicit hwnd wins, else title text.
    let (hwnd_r, err, cands) = match (h, t) {
        (Some(v), _) => resolve_selector(&format!("hwnd:{v}")),
        (None, Some(v)) if !v.is_empty() => resolve_selector(&v),
        _ => (
            None,
            Some("bad_window: need title or hwnd".to_string()),
            None,
        ),
    };
    if hwnd_r.is_none() {
        let mut payload = Map::new();
        payload.insert("ok".to_string(), Value::from(false));
        payload.insert("error".to_string(), Value::from(err.unwrap()));
        if let Some(c) = cands {
            payload.insert(
                "candidates".to_string(),
                Value::from(c.into_iter().map(Value::Object).collect::<Vec<_>>()),
            );
        }
        return wrapped_text(&py_dumps(&Value::Object(payload)));
    }
    let model = element_model(hwnd_r.unwrap());
    let entries: Vec<Value> = if d {
        model.entries.into_iter().map(Value::Object).collect()
    } else {
        model
            .entries
            .iter()
            .map(compact_entry)
            .map(Value::Object)
            .collect()
    };
    // Same top-level object as `shot::elements_block`, as structured JSON
    // (both assemble through `shot::elements_object`; only the entries
    // slice differs: full walk entries under detail=true, compact
    // projections by default).
    let o = elements_object(&model.backend, model.truncated, entries, &model.detail);
    wrapped_text(&py_dumps(&Value::Object(o)))
}

fn call_screenshot(args: &Map<String, Value>) -> String {
    let mut errs = Vec::new();
    check_no_extra(
        args,
        &mut errs,
        &[
            "display",
            "region",
            "window",
            "max_px",
            "fmt",
            "quality",
            "grid",
            "allow_crop_fallback",
            "elements",
        ],
    );
    let display = req_int(args, &mut errs, "display", -1);
    let region: Option<Option<Vec<Value>>> = match args.get("region") {
        None | Some(Value::Null) => Some(None),
        Some(Value::Array(a)) => Some(Some(a.clone())),
        Some(v) => {
            errs.push(crate::VErr {
                loc: "region".to_string(),
                msg: "Input should be a valid array".to_string(),
                typ: "list_type".to_string(),
                iv: crate::render_iv(v),
                it: crate::input_type_of(v).to_string(),
            });
            None
        }
    };
    let window = opt_str(args, &mut errs, "window");
    let max_px: Option<Option<i64>> = match args.get("max_px") {
        None => Some(Some(1280)),
        Some(Value::Null) => Some(None),
        Some(v) => match coerce_int(v) {
            Ok(i) => Some(Some(i)),
            Err((msg, typ)) => {
                errs.push(crate::VErr {
                    loc: "max_px".to_string(),
                    msg,
                    typ,
                    iv: crate::render_iv(v),
                    it: crate::input_type_of(v).to_string(),
                });
                None
            }
        },
    };
    let fmt = req_str(args, &mut errs, "fmt", "jpeg");
    let quality = req_int(args, &mut errs, "quality", 80);
    let grid = req_bool(args, &mut errs, "grid");
    let allow_crop_fallback = req_bool(args, &mut errs, "allow_crop_fallback");
    let elements = req_bool(args, &mut errs, "elements");
    if !errs.is_empty() {
        return err_result_text(&verr_text("screenshot", &errs));
    }
    dispatch_screenshot(&ShotArgs {
        display: display.unwrap(),
        region: region.unwrap(),
        window: window.unwrap(),
        max_px: max_px.unwrap(),
        fmt: fmt.unwrap(),
        quality: Some(quality.unwrap()),
        grid: grid.unwrap(),
        allow_crop_fallback: allow_crop_fallback.unwrap(),
        elements: elements.unwrap(),
    })
}

fn call_do(args: &Map<String, Value>) -> String {
    let mut errs = Vec::new();
    check_no_extra(
        args,
        &mut errs,
        &[
            "actions",
            "background",
            "focus_guard",
            "settle_ms",
            "dry_run",
            "desktop",
        ],
    );
    let actions: Option<Vec<Value>> = match args.get("actions") {
        None => {
            errs.push(missing_arg("do", args, "actions"));
            None
        }
        Some(Value::Array(a)) => Some(a.clone()),
        Some(v) => {
            errs.push(crate::VErr {
                loc: "actions".to_string(),
                msg: "Input should be a valid list".to_string(),
                typ: "list_type".to_string(),
                iv: crate::render_iv(v),
                it: crate::input_type_of(v).to_string(),
            });
            None
        }
    };
    // background is `str | None`: strict pydantic string, no numeric
    // coercion (probed: int/float/bool all fail string_type).
    let background: Option<Option<String>> = match args.get("background") {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(s)) => Some(Some(s.clone())),
        Some(v) => {
            errs.push(crate::VErr {
                loc: "background".to_string(),
                msg: "Input should be a valid string".to_string(),
                typ: "string_type".to_string(),
                iv: crate::render_iv(v),
                it: crate::input_type_of(v).to_string(),
            });
            None
        }
    };
    let focus_guard = do_bool(args, &mut errs, "focus_guard");
    let settle_ms = req_int(args, &mut errs, "settle_ms", 0);
    let dry_run = do_bool(args, &mut errs, "dry_run");
    // desktop is `str | None`, opt-in only: the sole accepted value is
    // "hidden" (case-insensitive). Plain `do` omits it and keeps its exact
    // behaviour.
    let desktop: Option<Option<String>> = opt_str(args, &mut errs, "desktop");
    if let Some(Some(d)) = desktop.as_ref() {
        if !d.trim().eq_ignore_ascii_case("hidden") {
            errs.push(crate::VErr {
                loc: "desktop".to_string(),
                msg: "Input should be 'hidden'".to_string(),
                typ: "literal_error".to_string(),
                iv: crate::render_iv(&Value::String(d.clone())),
                it: "str".to_string(),
            });
        }
    }
    if !errs.is_empty() {
        return err_result_text(&verr_text("do", &errs));
    }
    do_burst(
        &actions.unwrap(),
        background.unwrap(),
        focus_guard.unwrap(),
        settle_ms.unwrap(),
        dry_run.unwrap(),
        desktop.unwrap(),
    )
}

/// Pydantic v2 lax bool, probed verbatim against the live python server:
/// 0/1 and 0.0/1.0 coerce; other ints/whole floats fail bool_parsing;
/// fractional floats fail bool_type; the true/false word sets coerce;
/// other strings fail bool_parsing; null fails bool_type.
fn do_bool(args: &Map<String, Value>, errs: &mut Vec<crate::VErr>, name: &str) -> Option<bool> {
    let parsing = |v: &Value| crate::VErr {
        loc: name.to_string(),
        msg: "Input should be a valid boolean, unable to interpret input".to_string(),
        typ: "bool_parsing".to_string(),
        iv: crate::render_iv(v),
        it: crate::input_type_of(v).to_string(),
    };
    let btype = |v: &Value| crate::VErr {
        loc: name.to_string(),
        msg: "Input should be a valid boolean".to_string(),
        typ: "bool_type".to_string(),
        iv: crate::render_iv(v),
        it: crate::input_type_of(v).to_string(),
    };
    match args.get(name) {
        None => Some(false),
        Some(Value::Null) => {
            errs.push(btype(&Value::Null));
            None
        }
        Some(Value::Bool(b)) => Some(*b),
        Some(v @ Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                if i == 0 {
                    Some(false)
                } else if i == 1 {
                    Some(true)
                } else {
                    errs.push(parsing(v));
                    None
                }
            } else if let Some(u) = n.as_u64() {
                if u <= 1 {
                    Some(u == 1)
                } else {
                    errs.push(parsing(v));
                    None
                }
            } else if let Some(f) = n.as_f64() {
                if f == 0.0 {
                    Some(false)
                } else if f == 1.0 {
                    Some(true)
                } else if f.is_finite() && f.fract() == 0.0 {
                    errs.push(parsing(v));
                    None
                } else {
                    errs.push(btype(v));
                    None
                }
            } else {
                errs.push(btype(v));
                None
            }
        }
        Some(v @ Value::String(s)) => match s.trim().to_lowercase().as_str() {
            "true" | "1" | "yes" | "y" | "on" | "t" => Some(true),
            "false" | "0" | "no" | "n" | "off" | "f" => Some(false),
            _ => {
                errs.push(parsing(v));
                None
            }
        },
        Some(v) => {
            errs.push(btype(v));
            None
        }
    }
}

fn call_noargs(tool: &str, args: &Map<String, Value>) -> Option<String> {
    let mut errs = Vec::new();
    check_no_extra(args, &mut errs, &[]);
    if !errs.is_empty() {
        return Some(err_result_text(&verr_text(tool, &errs)));
    }
    None
}

fn tools_call(params: &Value) -> String {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args: Map<String, Value> = match params.get("arguments") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    match name {
        "screenshot" => call_screenshot(&args),
        "list_windows" => match call_noargs("list_windows", &args) {
            Some(e) => e,
            None => do_list_windows(),
        },
        "window" => call_window(&args),
        "do" => call_do(&args),
        "cursor" => match call_noargs("cursor", &args) {
            Some(e) => e,
            None => do_cursor(),
        },
        "elements" => call_elements(&args),
        _ => err_result_text(&format!("Unknown tool: '{name}'")),
    }
}

fn handle(v: &Value, out: &mut impl Write) -> std::io::Result<()> {
    let method = v.get("method").and_then(|m| m.as_str());
    let id = v.get("id").cloned();
    match (method, id) {
        (None, _) => Ok(()),
        (Some(_), None) => Ok(()),
        (Some(m), Some(id)) => match m {
            "initialize" => {
                let p = v.get("params").cloned().unwrap_or(Value::Null);
                write_result(out, &id, &init_result(&p))
            }
            "tools/list" => write_result(out, &id, &tools_list_result()),
            "tools/call" => {
                let p = v.get("params").cloned().unwrap_or(Value::Null);
                match p.get("arguments") {
                    None | Some(Value::Null) | Some(Value::Object(_)) => {
                        write_result(out, &id, &tools_call(&p))
                    }
                    Some(_) => write_error(
                        out,
                        &id,
                        -32602,
                        "Invalid params: arguments must be an object",
                    ),
                }
            }
            "ping" => write_result(out, &id, "{}"),
            "prompts/list" => write_result(out, &id, "{\"prompts\":[]}"),
            "resources/list" => write_result(out, &id, "{\"resources\":[]}"),
            "resources/templates/list" => write_result(out, &id, "{\"resourceTemplates\":[]}"),
            _ if m.starts_with("notifications/") => Ok(()),
            _ => write_error(out, &id, -32601, &format!("Method not found: {m}")),
        },
    }
}

pub fn run() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let code = pump(stdin.lock(), &mut out);
    if code != 0 {
        std::process::exit(code);
    }
}

/// Stdio pump, split from `run` for testability. Returns the process exit
/// code: 0 on clean stdin EOF, 1 when a response could not be written
/// (dead client: stop consuming stdin instead of spinning forever).
fn pump(reader: impl BufRead, out: &mut impl Write) -> i32 {
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if !t.starts_with('{') && !t.starts_with('[') {
            if write_error(out, &Value::Null, -32700, "Parse error").is_err() {
                return 1;
            }
            continue;
        }
        let v: Value = match serde_json::from_str(t) {
            Ok(v) => v,
            Err(_) => {
                if write_error(out, &Value::Null, -32700, "Parse error").is_err() {
                    return 1;
                }
                continue;
            }
        };
        if v.is_array() {
            if write_error(
                out,
                &Value::Null,
                -32600,
                "Batch requests are not supported",
            )
            .is_err()
            {
                return 1;
            }
            continue;
        }
        if handle(&v, out).is_err() {
            return 1;
        }
    }
    0
}

pub fn monitor_count() -> i64 {
    display_monitors().len() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn run_pump(input: &[u8]) -> (i32, String) {
        let mut out: Vec<u8> = Vec::new();
        let code = pump(input, &mut out);
        (code, String::from_utf8(out).expect("utf8"))
    }

    #[test]
    fn parse_error_gets_reply() {
        let (code, s) = run_pump(b"not-json\n");
        assert_eq!(code, 0);
        assert!(
            s.contains("-32700"),
            "unparseable frame must get a Parse error reply, got: {s}"
        );
    }

    #[test]
    fn batch_array_rejected() {
        let (code, s) = run_pump(b"[1, 2]\n");
        assert_eq!(code, 0);
        assert!(
            s.contains("-32600"),
            "batch array must be explicitly rejected, got: {s}"
        );
    }

    #[test]
    fn non_object_arguments_rejected() {
        let frame = br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"cursor","arguments":42}}"#;
        let mut framed = frame.to_vec();
        framed.push(b'\n');
        let (code, s) = run_pump(&framed);
        assert_eq!(code, 0);
        assert!(
            s.contains("-32602"),
            "non-object arguments must get Invalid params, got: {s}"
        );
    }

    #[test]
    fn clean_eof_exits_zero() {
        let frame = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let mut framed = frame.to_vec();
        framed.push(b'\n');
        let (code, s) = run_pump(&framed);
        assert_eq!(code, 0);
        assert!(s.contains("\"result\":{}"), "ping must answer, got: {s}");
    }

    struct FailWriter;

    impl Write for FailWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
    }

    #[test]
    fn dead_client_stops_pump_nonzero() {
        let frame = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let mut framed = frame.to_vec();
        framed.push(b'\n');
        let mut out = FailWriter;
        let code = pump(&framed[..], &mut out);
        assert_eq!(code, 1, "broken stdout must stop the pump with code 1");
    }

    /// Gap-3 proof (fails before the fix, passes after): the initialize
    /// result must carry an `instructions` string with the usage contract --
    /// the six tools, the coordinate rule, batch + dry_run, read-only first.
    /// Essentials are asserted, never a byte-exact string.
    #[test]
    fn init_result_carries_usage_instructions() {
        let params = serde_json::json!({"protocolVersion": "2025-11-25"});
        let s = init_result(&params);
        let v: Value = serde_json::from_str(&s).expect("init result must be JSON");
        let ins = v
            .get("instructions")
            .and_then(|x| x.as_str())
            .expect("initialize result must carry instructions");
        for tool in [
            "screenshot",
            "list_windows",
            "window",
            "do",
            "cursor",
            "elements",
        ] {
            assert!(
                ins.contains(tool),
                "instructions must name the {tool} tool"
            );
        }
        assert!(
            ins.contains("dry_run"),
            "instructions must document dry_run"
        );
        assert!(
            ins.contains("source.x"),
            "instructions must carry the image-to-screen formula"
        );
        assert!(
            ins.contains("read-only"),
            "instructions must tell read-only calls to come first"
        );
    }

    fn elements_args(pairs: &[(&str, Value)]) -> Map<String, Value> {
        let mut m = Map::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.clone());
        }
        m
    }

    #[test]
    fn elements_rejects_unknown_arg_like_neighbours() {
        let args = elements_args(&[("title", Value::from("x")), ("bogus", Value::from(1))]);
        let s = call_elements(&args);
        assert!(
            s.contains("validation error for call[elements]"),
            "unknown arg must fail validation, got: {s}"
        );
        assert!(
            s.contains("unexpected_keyword_argument"),
            "unknown arg needs the shared extra-key type, got: {s}"
        );
    }

    #[test]
    fn elements_rejects_overflow_hwnd_with_shared_error() {
        let v = serde_json::json!("9223372036854775808"); // i64::MAX + 1
        let s = call_elements(&elements_args(&[("hwnd", v.clone())]));
        assert!(
            s.contains("validation error for call[elements]"),
            "overflow hwnd must fail validation, got: {s}"
        );
        assert!(
            s.contains("does not fit in i64") && s.contains("int_parsing"),
            "overflow hwnd needs the shared range message, got: {s}"
        );
    }

    #[test]
    fn elements_tool_schema_has_expected_input_schema() {
        let t: Value = serde_json::from_str(crate::schemas::TOOL_ELEMENTS)
            .expect("TOOL_ELEMENTS must be JSON");
        assert_eq!(t.get("name").and_then(|v| v.as_str()), Some("elements"));
        let props = t
            .get("inputSchema")
            .and_then(|s| s.get("properties"))
            .and_then(|p| p.as_object())
            .expect("elements inputSchema.properties");
        for k in ["title", "hwnd", "detail"] {
            assert!(props.contains_key(k), "elements schema needs {k}");
        }
        assert_eq!(
            t.get("inputSchema")
                .and_then(|s| s.get("additionalProperties")),
            Some(&Value::from(false))
        );
        assert_eq!(props["detail"].get("default"), Some(&Value::from(false)));
    }

    fn elements_payload(args: &Map<String, Value>) -> Value {
        let out = call_elements(args);
        let outer: Value = serde_json::from_str(&out).expect("outer frame");
        let inner = outer
            .get("structuredContent")
            .and_then(|s| s.get("result"))
            .and_then(|r| r.as_str())
            .expect("structured result");
        serde_json::from_str(inner).expect("elements payload")
    }

    #[test]
    fn elements_live_offscreen_fixture_lists_edit_and_ok() {
        // Live resolve path against a real HWND the test owns (offscreen,
        // never activated): proves `elements` beyond unit/schema assertions.
        // Foreground contract is environment-independent: record BEFORE any
        // fixture work; a zero-pixel read must not CHANGE it (skips with
        // FG_SKIP when the environment has no foreground session).
        let fg_before = crate::fixture::foreground_now();
        let f = crate::fixture::Fixture::new();
        let hwnd = f.hwnd() as i64;
        // Poll own fixture only until the UIA backend stabilizes (first walk
        // can race ShowWindow and fall back to win32).
        let payload = {
            let t0 = std::time::Instant::now();
            loop {
                let p = elements_payload(&elements_args(&[("hwnd", Value::from(hwnd))]));
                let be = p.get("backend").and_then(|v| v.as_str()).unwrap_or("");
                let n = p
                    .get("elements")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                if (be == "uia" && n >= 3) || t0.elapsed() > std::time::Duration::from_secs(5) {
                    break p;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };
        assert_eq!(payload.get("uia_available"), Some(&Value::from(true)));
        let els = payload
            .get("elements")
            .and_then(|v| v.as_array())
            .expect("elements");
        assert!(
            !els.is_empty(),
            "offscreen fixture must list a non-empty element list"
        );
        let has_edit = els
            .iter()
            .any(|e| e.get("role").and_then(|v| v.as_str()) == Some("Edit"));
        let has_ok = els.iter().any(|e| {
            e.get("name").and_then(|v| v.as_str()) == Some("OK")
                && e.get("role").and_then(|v| v.as_str()) == Some("Button")
        });
        assert!(
            has_edit,
            "element list must contain the EDIT control, got: {payload}"
        );
        assert!(
            has_ok,
            "element list must contain the OK button, got: {payload}"
        );
        for e in els {
            let o = e.as_object().expect("entry object");
            assert!(
                o.contains_key("ref"),
                "compact entry must carry ref, got: {o:?}"
            );
            assert!(
                !o["ref"].as_str().unwrap_or("").is_empty(),
                "ref must be populated"
            );
            for k in crate::shot::COMPACT_ELEMENT_KEYS {
                assert!(
                    o.contains_key(k),
                    "compact entry must carry {k}, got: {o:?}"
                );
            }
            assert!(!o.contains_key("win"), "compact default must omit win");
        }
        assert_eq!(payload.get("backend").and_then(|v| v.as_str()), Some("uia"));
        // Foreground untouched by a zero-pixel read: fail only if a fixture
        // window is LEFT activated (skips with FG_SKIP when the environment
        // provides no foreground session, e.g. headless CI).
        crate::fixture::check_foreground_left_clean(hwnd, fg_before, "elements");
        // Evidence for the report (captured with -- --nocapture).
        println!("ELEMENTS_COMPACT_FIXTURE_JSON={payload}");
    }

    #[test]
    fn elements_live_detail_keeps_win() {
        let f = crate::fixture::Fixture::new();
        let hwnd = f.hwnd() as i64;
        let payload = elements_payload(&elements_args(&[
            ("hwnd", Value::from(hwnd)),
            ("detail", Value::from(true)),
        ]));
        let els = payload
            .get("elements")
            .and_then(|v| v.as_array())
            .expect("elements");
        assert!(!els.is_empty());
        let first = els[0].as_object().expect("entry");
        assert_eq!(
            first.get("win").and_then(|v| v.as_i64()),
            Some(hwnd),
            "detail must keep raw win"
        );
        println!("ELEMENTS_DETAIL_FIXTURE_JSON={payload}");
    }
}
