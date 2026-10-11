//! JSON-RPC stdio loop + per-tool validation (pydantic-shaped) + dispatch.

use crate::burst::do_burst;
use crate::schemas::{TOOL_CURSOR, TOOL_DO, TOOL_LIST_WINDOWS, TOOL_SCREENSHOT, TOOL_WINDOW};
use crate::screen::{dispatch_screenshot, ShotArgs};
use crate::winop::{do_cursor, do_list_windows, do_window, WinArgs};
use crate::{
    check_no_extra, coerce_int, coerce_str, display_monitors, err_result_text, missing_arg,
    opt_int, opt_str, req_bool, req_int, req_str, verr_text, LATEST_VERSION, SUPPORTED_VERSIONS,
};
use serde_json::{Map, Value};
use std::io::{BufRead, BufReader, Write};

fn write_result(out: &mut impl Write, id: &Value, result: &str) {
    let ids = serde_json::to_string(id).unwrap();
    let _ = writeln!(
        out,
        "{{\"jsonrpc\":\"2.0\",\"id\":{ids},\"result\":{result}}}"
    );
    let _ = out.flush();
}

fn write_error(out: &mut impl Write, id: &Value, code: i64, msg: &str) {
    let ids = serde_json::to_string(id).unwrap();
    let ms = serde_json::to_string(msg).unwrap();
    let _ = writeln!(
        out,
        "{{\"jsonrpc\":\"2.0\",\"id\":{ids},\"error\":{{\"code\":{code},\"message\":{ms}}}}}"
    );
    let _ = out.flush();
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
        "{{\"protocolVersion\":{},\"capabilities\":{{\"logging\":{{}},\"prompts\":{{\"listChanged\":true}},\"resources\":{{\"subscribe\":false,\"listChanged\":true}},\"tools\":{{\"listChanged\":true}}}},\"serverInfo\":{{\"name\":\"puppet\",\"version\":\"4.1.0\"}}}}",
        serde_json::to_string(echo).unwrap()
    )
}

fn tools_list_result() -> String {
    format!("{{\"tools\":[{TOOL_SCREENSHOT},{TOOL_LIST_WINDOWS},{TOOL_WINDOW},{TOOL_DO},{TOOL_CURSOR}]}}")
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
    if !errs.is_empty() {
        return err_result_text(&verr_text("do", &errs));
    }
    do_burst(
        &actions.unwrap(),
        background.unwrap(),
        focus_guard.unwrap(),
        settle_ms.unwrap(),
        dry_run.unwrap(),
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
        _ => err_result_text(&format!("Unknown tool: '{name}'")),
    }
}

fn handle(v: &Value, out: &mut impl Write) {
    let method = v.get("method").and_then(|m| m.as_str());
    let id = v.get("id").cloned();
    match (method, id) {
        (None, _) => {}
        (Some(_), None) => {}
        (Some(m), Some(id)) => match m {
            "initialize" => {
                let p = v.get("params").cloned().unwrap_or(Value::Null);
                write_result(out, &id, &init_result(&p));
            }
            "tools/list" => write_result(out, &id, &tools_list_result()),
            "tools/call" => {
                let p = v.get("params").cloned().unwrap_or(Value::Null);
                write_result(out, &id, &tools_call(&p));
            }
            "ping" => write_result(out, &id, "{}"),
            "prompts/list" => write_result(out, &id, "{\"prompts\":[]}"),
            "resources/list" => write_result(out, &id, "{\"resources\":[]}"),
            "resources/templates/list" => write_result(out, &id, "{\"resourceTemplates\":[]}"),
            _ if m.starts_with("notifications/") => {}
            _ => write_error(out, &id, -32601, &format!("Method not found: {m}")),
        },
    }
}

pub fn run() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let reader = BufReader::new(stdin.lock());
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
            continue;
        }
        let v: Value = match serde_json::from_str(t) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.is_array() {
            continue;
        }
        handle(&v, &mut out);
    }
}

pub fn monitor_count() -> i64 {
    display_monitors().len() as i64
}
