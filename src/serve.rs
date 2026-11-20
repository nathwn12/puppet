use crate::burst::do_burst;
use crate::schemas::{
    TOOL_CURSOR, TOOL_DIFF, TOOL_DO, TOOL_ELEMENTS, TOOL_LIST_WINDOWS, TOOL_SCREENSHOT, TOOL_WINDOW,
};
use crate::screen::{dispatch_screenshot, ShotArgs};
use crate::shot::{compact_entry, element_model, elements_object};
use crate::winop::{do_cursor, do_window, WinArgs};
use crate::{
    check_no_extra, coerce_int, coerce_str, display_monitors, err_result_text, missing_arg,
    opt_int, opt_str, py_dumps, req_bool, req_int, req_str, verr_text, wrapped_text, ALL_VERSIONS,
    LATEST_VERSION, MODERN_VERSION, SUPPORTED_VERSIONS,
};
use serde_json::{Map, Value};
use std::io::{BufRead, Write};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc, LazyLock, Mutex,
};
use std::time::{Duration, Instant};

const WORKER_COUNT: usize = 4;
const MAX_IN_FLIGHT: usize = 8;

static INPUT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn lock_input_cancellable() -> Option<std::sync::MutexGuard<'static, ()>> {
    loop {
        match INPUT_LOCK.try_lock() {
            Ok(g) => return Some(g),
            Err(std::sync::TryLockError::Poisoned(e)) => return Some(e.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {
                if crate::cancel::cancelled() {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn emit_line<W: Write>(sink: &Mutex<W>, line: String) -> std::io::Result<()> {
    let mut g = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    writeln!(g, "{line}")?;
    g.flush()
}

fn result_line(id: &Value, result: &str) -> String {
    let ids = serde_json::to_string(id).unwrap();
    format!("{{\"jsonrpc\":\"2.0\",\"id\":{ids},\"result\":{result}}}")
}

fn error_line(id: &Value, code: i64, msg: &str) -> String {
    let ids = serde_json::to_string(id).unwrap();
    let ms = serde_json::to_string(msg).unwrap();
    format!("{{\"jsonrpc\":\"2.0\",\"id\":{ids},\"error\":{{\"code\":{code},\"message\":{ms}}}}}")
}

fn error_data_line(id: &Value, code: i64, msg: &str, data: &Value) -> String {
    let ids = serde_json::to_string(id).unwrap();
    let ms = serde_json::to_string(msg).unwrap();
    let ds = serde_json::to_string(data).unwrap();
    format!("{{\"jsonrpc\":\"2.0\",\"id\":{ids},\"error\":{{\"code\":{code},\"message\":{ms},\"data\":{ds}}}}}")
}

fn write_result<W: Write>(out: &Mutex<W>, id: &Value, result: &str) -> std::io::Result<()> {
    emit_line(out, result_line(id, result))
}

fn write_error<W: Write>(out: &Mutex<W>, id: &Value, code: i64, msg: &str) -> std::io::Result<()> {
    emit_line(out, error_line(id, code, msg))
}

fn write_error_with_data<W: Write>(
    out: &Mutex<W>,
    id: &Value,
    code: i64,
    msg: &str,
    data: &Value,
) -> std::io::Result<()> {
    emit_line(out, error_data_line(id, code, msg, data))
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
        "{{\"protocolVersion\":{},\"capabilities\":{{\"tools\":{{\"listChanged\":false}}}},\"serverInfo\":{{\"name\":\"puppet\",\"version\":{}}},\"instructions\":{}}}",
        serde_json::to_string(echo).unwrap(),
        serde_json::to_string(env!("CARGO_PKG_VERSION")).unwrap(),
        serde_json::to_string(&crate::claims::instructions()).unwrap()
    )
}

fn tools_list_result() -> String {
    format!("{{\"tools\":[{TOOL_SCREENSHOT},{TOOL_DIFF},{TOOL_LIST_WINDOWS},{TOOL_WINDOW},{TOOL_DO},{TOOL_CURSOR},{TOOL_ELEMENTS}]}}")
}

fn discover_result() -> String {
    let supported: Vec<Value> = ALL_VERSIONS.iter().map(|v| Value::from(*v)).collect();
    format!(
        "{{\"supportedVersions\":{},\"capabilities\":{{\"tools\":{{\"listChanged\":false}}}},\"serverInfo\":{{\"name\":\"puppet\",\"version\":{}}},\"instructions\":{},\"resultType\":\"complete\"}}",
        serde_json::to_string(&Value::Array(supported)).unwrap(),
        serde_json::to_string(env!("CARGO_PKG_VERSION")).unwrap(),
        serde_json::to_string(&crate::claims::instructions()).unwrap()
    )
}

fn meta_version(v: &Value) -> Option<Value> {
    v.get("params")?
        .as_object()?
        .get("_meta")?
        .as_object()?
        .get("io.modelcontextprotocol/protocolVersion")
        .cloned()
}

fn with_result_type(result: &str) -> String {
    let t = result.trim_end();
    match t.strip_suffix('}') {
        Some(body) => format!("{body},\"resultType\":\"complete\"}}"),
        None => result.to_string(),
    }
}

fn call_window(args: &Map<String, Value>) -> String {
    let mut errs = Vec::new();
    check_no_extra(
        args,
        &mut errs,
        &[
            "action",
            "title",
            "hwnd",
            "x",
            "y",
            "width",
            "height",
            "target",
            "allow_foreground",
        ],
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
    let target = opt_target(args, &mut errs);
    let allow_foreground = req_bool(args, &mut errs, "allow_foreground");
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
        target: target.unwrap(),
        allow_foreground: allow_foreground.unwrap(),
    })
}

fn call_elements(args: &Map<String, Value>) -> String {
    let mut errs = Vec::new();
    let fg_before =
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() as i64 };
    let mut cur_before = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
    let cur_ok =
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cur_before) != 0 };
    check_no_extra(
        args,
        &mut errs,
        &["title", "hwnd", "detail", "max_depth", "offset", "target"],
    );
    let title = opt_str(args, &mut errs, "title");
    let hwnd = opt_int(args, &mut errs, "hwnd");
    let target = opt_target(args, &mut errs);
    let detail = req_bool(args, &mut errs, "detail");
    let max_depth = req_int(args, &mut errs, "max_depth", 8);
    let offset = req_int(args, &mut errs, "offset", 0);
    if !errs.is_empty() {
        return err_result_text(&verr_text("elements", &errs));
    }
    let (t, h, tg, d, md, off) = (
        title.unwrap(),
        hwnd.unwrap(),
        target.unwrap(),
        detail.unwrap(),
        max_depth.unwrap(),
        offset.unwrap(),
    );
    if !(1..=32).contains(&md) {
        let mut payload = Map::new();
        payload.insert("ok".to_string(), Value::from(false));
        payload.insert(
            "error".to_string(),
            Value::from(format!("max_depth must be an integer 1..32 (got {md})")),
        );
        return wrapped_text(&py_dumps(&Value::Object(payload)));
    }
    if off < 0 {
        let mut payload = Map::new();
        payload.insert("ok".to_string(), Value::from(false));
        payload.insert(
            "error".to_string(),
            Value::from(format!("offset must be an integer >= 0 (got {off})")),
        );
        return wrapped_text(&py_dumps(&Value::Object(payload)));
    }
    let mut locked: Option<crate::target::TargetLock> = None;
    let (hwnd_r, err, cands) = match tg {
        Some(v) => match crate::target::TargetSpec::parse(&v) {
            Err(e) => (None, Some(e.text()), e.candidates.clone()),
            Ok(spec) => match crate::target::lock(&spec) {
                Err(e) => (None, Some(e.text()), e.candidates.clone()),
                Ok(l) => {
                    let h = l.hwnd as windows_sys::Win32::Foundation::HWND;
                    locked = Some(l);
                    (Some(h), None, None)
                }
            },
        },
        None => match crate::cap::lock_legacy(h, t.as_deref()) {
            Ok(l) => {
                let hw = l.hwnd as windows_sys::Win32::Foundation::HWND;
                locked = Some(l);
                (Some(hw), None, None)
            }
            Err(e) => (None, Some(e.text()), e.candidates.clone()),
        },
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
    if let Err(e) = crate::screen::revalidate_locked(&locked) {
        let mut payload = Map::new();
        payload.insert("ok".to_string(), Value::from(false));
        payload.insert("code".to_string(), Value::from("target_gone"));
        payload.insert("error".to_string(), Value::from(e));
        return wrapped_text(&py_dumps(&Value::Object(payload)));
    }
    let model = element_model(hwnd_r.unwrap(), md, off as usize);
    let entries: Vec<Value> = if d {
        model
            .entries
            .into_iter()
            .map(|mut e| {
                if model.backend == "uia" {
                    if let Some(path) = e.get("path").and_then(|v| v.as_str()) {
                        if let Ok(full) = crate::uia::uia_element_text(hwnd_r.unwrap() as i64, path)
                        {
                            let (text, truncated) = crate::shot::cap_text(&full);
                            e.insert("text".to_string(), Value::from(text));
                            e.insert("text_truncated".to_string(), Value::from(truncated));
                        }
                    }
                }
                Value::Object(e)
            })
            .collect()
    } else {
        model
            .entries
            .iter()
            .map(compact_entry)
            .map(Value::Object)
            .collect()
    };
    let o = elements_object(
        &model.backend,
        model.truncated,
        entries,
        &model.detail,
        model.next_offset,
    );
    let mut o = o;
    let mut cur_after = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
    let cur_ok2 =
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cur_after) != 0 };
    o.insert(
        "foregroundUnchanged".to_string(),
        Value::from(
            unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() as i64 }
                == fg_before,
        ),
    );
    o.insert(
        "cursorUnchanged".to_string(),
        Value::from(
            !cur_ok || !cur_ok2 || (cur_after.x == cur_before.x && cur_after.y == cur_before.y),
        ),
    );
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
            "target",
            "out",
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
    let target = opt_target(args, &mut errs);
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
    let out = opt_str(args, &mut errs, "out");
    let fmt = req_str(args, &mut errs, "fmt", "png");
    let quality_supplied = !matches!(args.get("quality"), None | Some(Value::Null));
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
        quality: if quality_supplied {
            Some(quality.unwrap())
        } else {
            None
        },
        grid: grid.unwrap(),
        allow_crop_fallback: allow_crop_fallback.unwrap(),
        elements: elements.unwrap(),
        out: out.unwrap(),
        target: target.unwrap(),
    })
}

fn call_diff(args: &Map<String, Value>) -> String {
    let mut errs = Vec::new();
    check_no_extra(
        args,
        &mut errs,
        &["base", "next", "threshold", "max_px", "out", "bbox", "mask"],
    );
    let base = match args.get("base") {
        None => {
            errs.push(missing_arg("diff", args, "base"));
            None
        }
        Some(v) => Some(v.clone()),
    };
    let next = match args.get("next") {
        None => {
            errs.push(missing_arg("diff", args, "next"));
            None
        }
        Some(v) => Some(v.clone()),
    };
    let threshold = req_int(args, &mut errs, "threshold", 0);
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
    let out = opt_str(args, &mut errs, "out");
    let bbox: Option<bool> = match args.get("bbox") {
        None => Some(true),
        Some(Value::Bool(b)) => Some(*b),
        Some(v) => {
            errs.push(crate::VErr {
                loc: "bbox".to_string(),
                msg: "Input should be a valid boolean".to_string(),
                typ: "bool_type".to_string(),
                iv: crate::render_iv(v),
                it: crate::input_type_of(v).to_string(),
            });
            None
        }
    };
    if !errs.is_empty() {
        return err_result_text(&verr_text("diff", &errs));
    }
    crate::diff::dispatch_diff(&crate::diff::DiffArgs {
        base: base.unwrap(),
        next: next.unwrap(),
        threshold: threshold.unwrap(),
        max_px: max_px.unwrap(),
        out: out.unwrap(),
        bbox: bbox.unwrap(),
        mask: match args.get("mask") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.clone()),
        },
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
            "background_args",
            "focus_guard",
            "settle_ms",
            "dry_run",
            "desktop",
            "target",
            "allow_foreground",
            "allow_cursor",
            "allow_clipboard",
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
    let background_args: Option<Option<Vec<String>>> = match args.get("background_args") {
        None | Some(Value::Null) => Some(None),
        Some(Value::Array(a)) => {
            let mut out: Vec<String> = Vec::with_capacity(a.len());
            let mut bad: Option<&Value> = None;
            for v in a {
                match v.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => {
                        bad = Some(v);
                        break;
                    }
                }
            }
            match bad {
                Some(v) => {
                    errs.push(crate::VErr {
                        loc: "background_args".to_string(),
                        msg: "Input should be a valid string".to_string(),
                        typ: "string_type".to_string(),
                        iv: crate::render_iv(v),
                        it: crate::input_type_of(v).to_string(),
                    });
                    None
                }
                None => Some(Some(out)),
            }
        }
        Some(v) => {
            errs.push(crate::VErr {
                loc: "background_args".to_string(),
                msg: "Input should be a valid list".to_string(),
                typ: "list_type".to_string(),
                iv: crate::render_iv(v),
                it: crate::input_type_of(v).to_string(),
            });
            None
        }
    };
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
    let target = opt_target(args, &mut errs);
    let allow_foreground = do_bool(args, &mut errs, "allow_foreground");
    let allow_cursor = do_bool(args, &mut errs, "allow_cursor");
    let allow_clipboard = do_bool(args, &mut errs, "allow_clipboard");
    if !errs.is_empty() {
        return err_result_text(&verr_text("do", &errs));
    }
    do_burst(&crate::burst::DoArgs {
        actions: &actions.unwrap(),
        background: background.unwrap(),
        background_args: background_args.unwrap(),
        focus_guard: focus_guard.unwrap(),
        settle_ms: settle_ms.unwrap(),
        dry_run: dry_run.unwrap(),
        desktop: desktop.unwrap(),
        target: target.unwrap(),
        allow_foreground: allow_foreground.unwrap(),
        allow_cursor: allow_cursor.unwrap(),
        allow_clipboard: allow_clipboard.unwrap(),
    })
}

fn opt_target(args: &Map<String, Value>, errs: &mut Vec<crate::VErr>) -> Option<Option<Value>> {
    match args.get("target") {
        None | Some(Value::Null) => Some(None),
        Some(v @ Value::Object(_)) | Some(v @ Value::String(_)) => Some(Some(v.clone())),
        Some(v) => {
            errs.push(crate::VErr {
                loc: "target".to_string(),
                msg: "Input should be a valid dictionary or string".to_string(),
                typ: "model_type".to_string(),
                iv: crate::render_iv(v),
                it: crate::input_type_of(v).to_string(),
            });
            None
        }
    }
}

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

fn call_list_windows(args: &Map<String, Value>) -> String {
    let mut errs = Vec::new();
    check_no_extra(args, &mut errs, &["include_app_ids"]);
    let include_app_ids = req_bool(args, &mut errs, "include_app_ids");
    if !errs.is_empty() {
        return err_result_text(&verr_text("list_windows", &errs));
    }
    crate::winop::do_list_windows(include_app_ids.unwrap())
}

fn tools_call(params: &Value) -> String {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args: Map<String, Value> = match params.get("arguments") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    match name {
        "screenshot" => call_screenshot(&args),
        "diff" => call_diff(&args),
        "list_windows" => call_list_windows(&args),
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

struct ToolJob {
    id: Value,
    id_key: String,
    params: Value,
    modern: bool,
}

struct AdmissionGuard {
    in_flight: Arc<AtomicUsize>,
}

impl AdmissionGuard {
    fn new(in_flight: &Arc<AtomicUsize>) -> Self {
        Self {
            in_flight: in_flight.clone(),
        }
    }
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        crate::cancel::clear_current();
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

enum Action {
    Result(String),
    InvalidParams,
}

enum JobOutcome {
    Emit(String),
    Drop,
}

fn production_execute(job: &ToolJob) -> Action {
    match job.params.get("arguments") {
        None | Some(Value::Null) | Some(Value::Object(_)) => {
            let r = tools_call(&job.params);
            Action::Result(if job.modern { with_result_type(&r) } else { r })
        }
        Some(_) => Action::InvalidParams,
    }
}

fn run_tool_job<W, F>(
    job: ToolJob,
    sink: &Mutex<W>,
    in_flight: Arc<AtomicUsize>,
    client_dead: &AtomicBool,
    execute: F,
) where
    W: Write,
    F: FnOnce(&ToolJob) -> Action,
{
    let id = job.id.clone();
    let id_key = job.id_key.clone();
    let token = crate::cancel::get(&id_key);
    let _admission = AdmissionGuard::new(&in_flight);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_tool_job_inner(&job, token.clone(), execute)
    }));
    match outcome {
        Ok(JobOutcome::Emit(line)) => {
            let mut g = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let cancelled = token.as_ref().is_some_and(|t| t.load(Ordering::SeqCst));
            if !cancelled && writeln!(g, "{line}").and_then(|_| g.flush()).is_err() {
                client_dead.store(true, Ordering::SeqCst);
            }
            crate::cancel::remove(&id_key);
        }
        Ok(JobOutcome::Drop) => {
            crate::cancel::remove(&id_key);
        }
        Err(_) => {
            let mut g = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let cancelled = token.as_ref().is_some_and(|t| t.load(Ordering::SeqCst));
            if !cancelled && token.is_some() {
                let line = error_line(&id, -32603, "Internal error");
                if writeln!(g, "{line}").and_then(|_| g.flush()).is_err() {
                    client_dead.store(true, Ordering::SeqCst);
                }
            }
            if token.is_some() {
                crate::cancel::remove(&id_key);
            }
        }
    }
}

fn run_tool_job_inner<F>(job: &ToolJob, token: Option<Arc<AtomicBool>>, execute: F) -> JobOutcome
where
    F: FnOnce(&ToolJob) -> Action,
{
    let Some(token) = token else {
        return JobOutcome::Drop;
    };
    crate::cancel::set_current(token.clone());
    if token.load(Ordering::SeqCst) {
        return JobOutcome::Drop;
    }
    let name = job
        .params
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("");
    let _input = if matches!(name, "do" | "window") {
        match lock_input_cancellable() {
            Some(g) => Some(g),
            None => {
                return JobOutcome::Drop;
            }
        }
    } else {
        None
    };
    if token.load(Ordering::SeqCst) {
        return JobOutcome::Drop;
    }
    let action = execute(job);
    if token.load(Ordering::SeqCst) {
        return JobOutcome::Drop;
    }
    let line = match action {
        Action::Result(r) => result_line(&job.id, &r),
        Action::InvalidParams => error_line(
            &job.id,
            -32602,
            "Invalid params: arguments must be an object",
        ),
    };
    JobOutcome::Emit(line)
}

fn handle_fast<W: Write>(
    v: &Value,
    sink: &Mutex<W>,
    tx: &mpsc::Sender<ToolJob>,
    in_flight: &AtomicUsize,
) -> bool {
    let id = v.get("id").cloned();
    let method_ok = v.get("method").and_then(|m| m.as_str()).is_some();
    let jsonrpc_ok = v.get("jsonrpc").and_then(|j| j.as_str()) == Some("2.0");
    if !method_ok || !jsonrpc_ok {
        return match id {
            None => false,
            Some(id) => write_error(sink, &id, -32600, "Invalid Request").is_err(),
        };
    }
    let m = v.get("method").and_then(|m| m.as_str()).unwrap();
    let Some(id) = id else {
        if m == "notifications/cancelled" {
            if let Some(rid) = v.get("params").and_then(|p| p.get("requestId")) {
                let key = serde_json::to_string(rid).unwrap();
                let _g = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                crate::cancel::cancel(&key);
            }
        }
        return false;
    };
    if let Some(req) = meta_version(v) {
        let known = req.as_str().is_some_and(|s| ALL_VERSIONS.contains(&s));
        if !known {
            let data = serde_json::json!({
                "supported": ALL_VERSIONS,
                "requested": req,
            });
            return write_error_with_data(sink, &id, -32022, "Unsupported protocol version", &data)
                .is_err();
        }
    }
    let modern = meta_version(v).and_then(|r| r.as_str().map(|s| s.to_string()))
        == Some(MODERN_VERSION.to_string());
    match m {
        "initialize" => {
            let p = v.get("params").cloned().unwrap_or(Value::Null);
            write_result(sink, &id, &init_result(&p)).is_err()
        }
        "server/discover" => write_result(sink, &id, &discover_result()).is_err(),
        "tools/list" => {
            let r = tools_list_result();
            if modern {
                write_result(sink, &id, &with_result_type(&r)).is_err()
            } else {
                write_result(sink, &id, &r).is_err()
            }
        }
        "tools/call" => {
            if in_flight.load(Ordering::SeqCst) >= MAX_IN_FLIGHT {
                return write_error(sink, &id, -32603, "Server busy").is_err();
            }
            let id_key = serde_json::to_string(&id).unwrap();
            crate::cancel::register(&id_key);
            in_flight.fetch_add(1, Ordering::SeqCst);
            let job = ToolJob {
                id,
                id_key: id_key.clone(),
                params: v.get("params").cloned().unwrap_or(Value::Null),
                modern,
            };
            if tx.send(job).is_err() {
                crate::cancel::remove(&id_key);
                in_flight.fetch_sub(1, Ordering::SeqCst);
            }
            false
        }
        "ping" => write_result(sink, &id, "{}").is_err(),
        _ if m.starts_with("notifications/") => false,
        _ => write_error(sink, &id, -32601, &format!("Method not found: {m}")).is_err(),
    }
}

pub fn run() {
    let stdin = std::io::stdin();
    let sink = Arc::new(Mutex::new(std::io::stdout()));
    let code = pump(std::io::BufReader::new(stdin), sink);
    if code != 0 {
        std::process::exit(code);
    }
}

/// Stdio pump, split from `run` for testability. The reader loop stays
/// free: fast frames are answered inline, `tools/call` jobs go to a
/// 4-worker pool, and responses are written atomically through the shared
/// sink. A dedicated reader thread owns `reader` so a blocking stdin can
/// never wedge the dead-client check: frames are forwarded over a channel
/// (sender drop = EOF) while this thread polls it with a 20ms timeout and
/// observes `client_dead` between polls. Returns the process exit code: 0
/// on clean stdin EOF (after waiting up to 2s for in-flight calls) or when
/// a worker observed a dead client; 1 when a reader-thread (inline)
/// response could not be written (dead client: stop consuming stdin
/// instead of spinning forever).
fn pump<R: BufRead + Send + 'static, W: Write + Send + 'static>(
    reader: R,
    sink: Arc<Mutex<W>>,
) -> i32 {
    let (tx, rx) = mpsc::channel::<ToolJob>();
    let rx = Arc::new(Mutex::new(rx));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let client_dead = Arc::new(AtomicBool::new(false));
    for _ in 0..WORKER_COUNT {
        let rx = rx.clone();
        let sink = sink.clone();
        let in_flight = in_flight.clone();
        let client_dead = client_dead.clone();
        std::thread::spawn(move || loop {
            let job = {
                rx.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .recv()
            };
            match job {
                Ok(j) => run_tool_job(
                    j,
                    &sink,
                    in_flight.clone(),
                    &client_dead,
                    production_execute,
                ),
                Err(_) => break,
            }
        });
    }
    let (line_tx, line_rx) = mpsc::channel::<std::io::Result<String>>();
    std::thread::spawn(move || {
        let reader = reader;
        for line in reader.lines() {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let sink_ref: &Mutex<W> = &sink;
    loop {
        if client_dead.load(Ordering::SeqCst) {
            return 0;
        }
        let line = match line_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(line) => line,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(t) {
            Ok(v) => v,
            Err(_) => {
                if write_error(sink_ref, &Value::Null, -32700, "Parse error").is_err() {
                    return 1;
                }
                continue;
            }
        };
        if v.is_array() {
            if write_error(sink_ref, &Value::Null, -32600, "Invalid Request").is_err() {
                return 1;
            }
            continue;
        }
        if !v.is_object() {
            if write_error(sink_ref, &Value::Null, -32600, "Invalid Request").is_err() {
                return 1;
            }
            continue;
        }
        if handle_fast(&v, sink_ref, &tx, &in_flight) {
            return 1;
        }
    }
    drop(tx);
    let deadline = Instant::now() + Duration::from_secs(2);
    while in_flight.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
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
        let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let code = pump(std::io::Cursor::new(input.to_vec()), sink.clone());
        let s = String::from_utf8(sink.lock().unwrap().clone()).expect("utf8");
        (code, s)
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
        let sink: Arc<Mutex<FailWriter>> = Arc::new(Mutex::new(FailWriter));
        let code = pump(std::io::Cursor::new(framed), sink);
        assert_eq!(code, 1, "broken stdout must stop the pump with code 1");
    }

    fn dispatch_test_job(in_flight: &Arc<AtomicUsize>, id: i64, tool: &str) -> ToolJob {
        in_flight.fetch_add(1, Ordering::SeqCst);
        let idv = Value::from(id);
        let id_key = serde_json::to_string(&idv).unwrap();
        crate::cancel::register(&id_key);
        ToolJob {
            id: idv,
            id_key,
            params: serde_json::json!({"name": tool, "arguments": {}}),
            modern: false,
        }
    }

    #[test]
    fn panic_keeps_token_releases_admission() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let job = dispatch_test_job(&in_flight, 5001, "cursor");
        let id_key = job.id_key.clone();
        let token = crate::cancel::get(&id_key);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_tool_job_inner(&job, token, |_| panic!("probe panic"))
        }));
        assert!(r.is_err(), "probe closure must unwind");
        assert!(
            crate::cancel::get(&id_key).is_some(),
            "token must survive the panic for the outer emit decision"
        );
        assert_eq!(in_flight.load(Ordering::SeqCst), 1);
        crate::cancel::remove(&id_key);
        in_flight.fetch_sub(1, Ordering::SeqCst);
    }

    #[test]
    fn panic_emits_internal_error_and_worker_survives() {
        let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let client_dead = AtomicBool::new(false);
        let job = dispatch_test_job(&in_flight, 5002, "cursor");
        let id_key = job.id_key.clone();
        run_tool_job(job, &sink, in_flight.clone(), &client_dead, |_| {
            panic!("probe tool panic")
        });
        let out = String::from_utf8(sink.lock().unwrap().clone()).expect("utf8");
        assert!(
            out.contains("-32603") && out.contains("5002"),
            "panic must emit -32603 Internal error for the id, got: {out}"
        );
        assert_eq!(
            in_flight.load(Ordering::SeqCst),
            0,
            "admission must be released after a contained panic"
        );
        assert!(
            crate::cancel::get(&id_key).is_none(),
            "token must be removed after the panic emit decision"
        );
        let job2 = dispatch_test_job(&in_flight, 5003, "cursor");
        let id_key2 = job2.id_key.clone();
        run_tool_job(job2, &sink, in_flight.clone(), &client_dead, |_| {
            Action::Result("{}".to_string())
        });
        let out2 = String::from_utf8(sink.lock().unwrap().clone()).expect("utf8");
        assert!(
            out2.contains("5003") && out2.contains("\"result\":{}"),
            "worker must process the next job after a panic, got: {out2}"
        );
        assert_eq!(
            in_flight.load(Ordering::SeqCst),
            0,
            "admission must be 0 after each dispatched job"
        );
        assert!(
            crate::cancel::get(&id_key2).is_none(),
            "second token must be removed after normal completion"
        );
    }

    #[test]
    fn panic_after_processed_cancel_sends_no_response() {
        let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let client_dead = AtomicBool::new(false);
        let job = dispatch_test_job(&in_flight, 5004, "cursor");
        let id_key = job.id_key.clone();
        run_tool_job(job, &sink, in_flight.clone(), &client_dead, |j| {
            crate::cancel::cancel(&serde_json::to_string(&j.id).unwrap());
            panic!("panic after a processed cancel")
        });
        let out = String::from_utf8(sink.lock().unwrap().clone()).expect("utf8");
        assert!(
            out.is_empty(),
            "no -32603 may follow a processed cancel, got: {out}"
        );
        assert_eq!(
            in_flight.load(Ordering::SeqCst),
            0,
            "admission must be released on the cancelled-panic path"
        );
        assert!(
            crate::cancel::get(&id_key).is_none(),
            "token must be removed on the cancelled-panic path"
        );
    }

    #[test]
    fn cancelled_waiter_never_executes_through_dispatch() {
        let _held = INPUT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let executed = Arc::new(AtomicBool::new(false));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let job = dispatch_test_job(&in_flight, 5005, "do");
        let id_key = job.id_key.clone();
        let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let ex = executed.clone();
        let s2 = sink.clone();
        let f2 = in_flight.clone();
        let h = std::thread::spawn(move || {
            let client_dead = AtomicBool::new(false);
            run_tool_job(job, &s2, f2, &client_dead, |_| {
                ex.store(true, Ordering::SeqCst);
                Action::Result("{}".to_string())
            });
        });
        std::thread::sleep(Duration::from_millis(300));
        crate::cancel::cancel(&id_key);
        h.join().expect("worker thread survives");
        assert!(
            !executed.load(Ordering::SeqCst),
            "cancelled waiter must never execute the body"
        );
        assert!(
            sink.lock().unwrap().is_empty(),
            "cancelled waiter must emit nothing"
        );
        assert_eq!(
            in_flight.load(Ordering::SeqCst),
            0,
            "admission must be released on the waiter-cancel path"
        );
        assert!(
            crate::cancel::get(&id_key).is_none(),
            "token must be removed on the waiter-cancel path"
        );
    }

    struct GateWriter {
        entered: Arc<AtomicBool>,
        open: Arc<AtomicBool>,
        buf: Vec<u8>,
    }

    impl Write for GateWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.entered.store(true, Ordering::SeqCst);
            while !self.open.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.buf.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn admission_covers_emit_eof_cannot_overtake_response() {
        let entered = Arc::new(AtomicBool::new(false));
        let open = Arc::new(AtomicBool::new(false));
        let sink: Arc<Mutex<GateWriter>> = Arc::new(Mutex::new(GateWriter {
            entered: entered.clone(),
            open: open.clone(),
            buf: Vec::new(),
        }));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let job = dispatch_test_job(&in_flight, 5006, "cursor");
        let id_key = job.id_key.clone();
        let s2 = sink.clone();
        let f2 = in_flight.clone();
        let h = std::thread::spawn(move || {
            let client_dead = AtomicBool::new(false);
            run_tool_job(job, &s2, f2, &client_dead, |_| {
                Action::Result("{}".to_string())
            });
        });
        let t0 = Instant::now();
        while !entered.load(Ordering::SeqCst) {
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "worker must reach the emit"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            in_flight.load(Ordering::SeqCst),
            1,
            "in_flight must cover the emit; EOF must not overtake the response"
        );
        open.store(true, Ordering::SeqCst);
        h.join().expect("worker finishes after the gate opens");
        assert_eq!(
            in_flight.load(Ordering::SeqCst),
            0,
            "admission must be released after the emit"
        );
        let out = String::from_utf8(sink.lock().unwrap().buf.clone()).expect("utf8");
        assert!(
            out.contains("5006") && out.contains("\"result\":{}"),
            "response must land after the gate opens, got: {out}"
        );
        assert!(
            crate::cancel::get(&id_key).is_none(),
            "token must be removed after the emit"
        );
    }

    #[test]
    fn dispatch_send_failure_releases_token_and_admission() {
        let (tx, rx) = mpsc::channel::<ToolJob>();
        drop(rx);
        let in_flight = AtomicUsize::new(0);
        let sink: Mutex<Vec<u8>> = Mutex::new(Vec::new());
        let v = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 6001,
            "method": "tools/call",
            "params": {"name": "cursor", "arguments": {}},
        });
        let stopped = handle_fast(&v, &sink, &tx, &in_flight);
        assert!(!stopped, "send failure is not an inline write failure");
        assert_eq!(
            in_flight.load(Ordering::SeqCst),
            0,
            "admission must be released on send failure"
        );
        let key = serde_json::to_string(&Value::from(6001)).unwrap();
        assert!(
            crate::cancel::get(&key).is_none(),
            "token must be removed on send failure"
        );
    }

    #[test]
    fn failing_tool_writer_sets_client_dead_with_cleanup() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let job = dispatch_test_job(&in_flight, 4242, "cursor");
        let id_key = job.id_key.clone();
        let client_dead = AtomicBool::new(false);
        let sink: Mutex<FailWriter> = Mutex::new(FailWriter);
        run_tool_job(
            job,
            &sink,
            in_flight.clone(),
            &client_dead,
            production_execute,
        );
        assert!(
            client_dead.load(Ordering::SeqCst),
            "failed tool write must raise the dead-client flag"
        );
        assert_eq!(
            in_flight.load(Ordering::SeqCst),
            0,
            "admission must be released even when the write fails"
        );
        assert!(
            crate::cancel::get(&id_key).is_none(),
            "token must be removed even when the write fails"
        );
    }

    struct OneFrameThenBlock {
        sent: bool,
    }

    impl std::io::Read for OneFrameThenBlock {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.sent {
                self.sent = true;
                let mut full = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"cursor","arguments":{}}}"#.to_vec();
                full.push(b'\n');
                let n = full.len().min(buf.len());
                buf[..n].copy_from_slice(&full[..n]);
                return Ok(n);
            }
            loop {
                std::thread::park();
            }
        }
    }

    #[test]
    fn pump_blocked_reader_exits_zero_on_dead_client() {
        let reader = std::io::BufReader::new(OneFrameThenBlock { sent: false });
        let sink: Arc<Mutex<FailWriter>> = Arc::new(Mutex::new(FailWriter));
        let t0 = Instant::now();
        let code = pump(reader, sink);
        assert_eq!(code, 0, "dead client must exit 0 while reader still open");
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "exit must come from client_dead, not a hang"
        );
    }

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
            assert!(ins.contains(tool), "instructions must name the {tool} tool");
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
    fn screenshot_absurd_region_edge_refused() {
        let mut m = Map::new();
        m.insert("display".to_string(), Value::from(0));
        m.insert(
            "region".to_string(),
            Value::from(vec![
                Value::from(0),
                Value::from(0),
                Value::from(9000),
                Value::from(100),
            ]),
        );
        let s = call_screenshot(&m);
        assert!(
            s.contains("8192"),
            "9000px region edge must name the 8192 bound, got: {s}"
        );
    }

    #[test]
    fn screenshot_region_i64_extremes_typed_error() {
        let mut m = Map::new();
        m.insert("display".to_string(), Value::from(0));
        m.insert(
            "region".to_string(),
            Value::from(vec![
                Value::from(i64::MIN),
                Value::from(0),
                Value::from(i64::MAX),
                Value::from(100),
            ]),
        );
        let s = call_screenshot(&m);
        assert!(
            s.contains("bad_region"),
            "MIN/MAX region must be a typed bad_region error, got: {s}"
        );
    }

    #[test]
    fn screenshot_region_grabbed_directly() {
        let mut m = Map::new();
        m.insert("display".to_string(), Value::from(0));
        m.insert(
            "region".to_string(),
            Value::from(vec![
                Value::from(0),
                Value::from(0),
                Value::from(200),
                Value::from(100),
            ]),
        );
        let s = call_screenshot(&m);
        let outer: Value = serde_json::from_str(&s).expect("screenshot result is JSON");
        let sc = outer
            .get("structuredContent")
            .expect("screenshot carries structuredContent");
        let (w, h) = (
            sc.get("image")
                .and_then(|i| i.get("w"))
                .and_then(|v| v.as_i64()),
            sc.get("image")
                .and_then(|i| i.get("h"))
                .and_then(|v| v.as_i64()),
        );
        assert_eq!((w, h), (Some(200), Some(100)));
        let note = sc
            .get("capture")
            .and_then(|c| c.get("note"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            note.contains("directly"),
            "region grabs must note the direct GDI rect path, got: {note}"
        );
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
    fn elements_legacy_hwnd_uses_the_target_lock() {
        let _g = crate::fixture::desktop();
        let f = crate::fixture::Fixture::new();
        let hwnd = f.hwnd_i64();
        drop(f);
        let s = call_elements(&elements_args(&[("hwnd", Value::from(hwnd))]));
        assert!(
            s.contains("target_gone"),
            "elements hwnd must route through the target lock, got: {s}"
        );
        assert!(
            !s.contains("bad_window"),
            "the bare-handle legacy path must be gone, got: {s}"
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
    fn do_type_action_schema_declares_element_index_and_expect_fp() {
        let t: Value = serde_json::from_str(crate::schemas::TOOL_DO).expect("TOOL_DO JSON");
        let actions = &t["inputSchema"]["properties"]["actions"];
        assert_eq!(
            actions.get("maxItems"),
            Some(&Value::from(256)),
            "the action cap must be declared, not only enforced at runtime"
        );
        let variants = actions["items"]["oneOf"]
            .as_array()
            .expect("do.actions.items.oneOf");
        let ty = variants
            .iter()
            .find(|v| v["properties"]["type"]["const"].as_str() == Some("type"))
            .expect("a type action variant");
        for k in ["element", "index", "expect_fp"] {
            assert!(
                ty["properties"]
                    .as_object()
                    .expect("type properties")
                    .contains_key(k),
                "the type action schema must declare {k}: {ty}"
            );
        }
    }

    #[test]
    fn do_wait_for_action_schema_declares_expect_fp() {
        let t: Value = serde_json::from_str(crate::schemas::TOOL_DO).expect("TOOL_DO JSON");
        let variants = t["inputSchema"]["properties"]["actions"]["items"]["oneOf"]
            .as_array()
            .expect("do.actions.items.oneOf");
        let wf = variants
            .iter()
            .find(|v| v["properties"]["type"]["const"].as_str() == Some("wait_for"))
            .expect("a wait_for action variant");
        let props = wf["properties"].as_object().expect("wait_for properties");
        assert!(
            props.contains_key("expect_fp"),
            "the wait_for schema must declare expect_fp: {wf}"
        );
        assert!(
            props.contains_key("element") && props.contains_key("state"),
            "the wait_for schema must keep element and state: {wf}"
        );
    }

    #[test]
    fn tool_descriptions_declare_the_new_payload_fields() {
        let el: Value = serde_json::from_str(crate::schemas::TOOL_ELEMENTS).expect("elements");
        let d = el["description"].as_str().expect("description");
        assert!(
            d.contains("offscreen"),
            "elements must describe the offscreen flag: {d}"
        );
        let ss: Value = serde_json::from_str(crate::schemas::TOOL_SCREENSHOT).expect("screenshot");
        let d = ss["description"].as_str().expect("description");
        for k in ["moved_during_capture", "elements_window"] {
            assert!(d.contains(k), "screenshot must describe {k}: {d}");
        }
        let lw: Value =
            serde_json::from_str(crate::schemas::TOOL_LIST_WINDOWS).expect("list_windows");
        assert_eq!(
            lw["inputSchema"]["properties"]["include_app_ids"]["default"],
            Value::from(false),
            "include_app_ids must be declared opt-in"
        );
        let d = lw["description"].as_str().expect("description");
        assert!(
            d.contains("include_app_ids") && d.contains("appId"),
            "list_windows must document the flag: {d}"
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
        outer
            .get("structuredContent")
            .cloned()
            .expect("structuredContent is the elements object")
    }

    #[test]
    fn elements_live_offscreen_fixture_lists_edit_and_ok() {
        let _g = crate::fixture::desktop();
        let fg_before = crate::fixture::foreground_now();
        let f = crate::fixture::Fixture::new();
        let hwnd = f.hwnd() as i64;
        let payload = elements_payload(&elements_args(&[("hwnd", Value::from(hwnd))]));
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
        let backend = payload
            .get("backend")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            backend == "uia" || backend == "win32",
            "backend must be uia or the deterministic win32 fallback, got: {backend} in {payload}"
        );
        crate::fixture::check_foreground_left_clean(hwnd, fg_before, "elements");
        println!("ELEMENTS_COMPACT_FIXTURE_JSON={payload}");
    }

    #[test]
    fn elements_marks_a_control_without_a_rect_offscreen() {
        let _g = crate::fixture::desktop();
        let f = crate::fixture::Fixture::new();
        let hwnd = f.hwnd() as i64;
        for child in [f.edit(), f.edit_ro(), f.button()] {
            let collapsed = unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::SetWindowPos(
                    child,
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                    0,
                    windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOMOVE
                        | windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOZORDER
                        | windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
                )
            };
            assert_ne!(collapsed, 0, "collapse a fixture child");
        }
        let entry = {
            let t0 = std::time::Instant::now();
            loop {
                let p = elements_payload(&elements_args(&[("hwnd", Value::from(hwnd))]));
                let found = p
                    .get("elements")
                    .and_then(|v| v.as_array())
                    .and_then(|els| {
                        els.iter()
                            .find(|e| e.get("offscreen") == Some(&Value::from(true)))
                            .cloned()
                    });
                if let Some(e) = found {
                    break e;
                }
                if t0.elapsed() > std::time::Duration::from_secs(10) {
                    panic!("no zero-area entry appeared in elements: {p}");
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };
        assert_eq!(
            entry.get("offscreen"),
            Some(&Value::from(true)),
            "a control with no rect must be flagged offscreen, got: {entry}"
        );
        assert!(
            entry.get("rect").and_then(|r| r.as_array()).is_some(),
            "rect stays present for shape stability: {entry}"
        );
        let w = entry["rect"][2].as_i64().unwrap_or(-1);
        let h = entry["rect"][3].as_i64().unwrap_or(-1);
        assert!(
            w <= 0 || h <= 0,
            "the flagged entry must really have no area, got: {entry}"
        );
        println!("ELEMENTS_OFFSCREEN_FLAG_OK entry={entry}");
    }

    #[test]
    fn elements_live_detail_keeps_win() {
        let _g = crate::fixture::desktop();
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
