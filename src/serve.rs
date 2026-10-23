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

/// P1b worker pool: four workers drain `tools/call` jobs so one long call
/// (a 60s `wait`, a 300s `wait_for`, a 60s settle) never blocks `ping`,
/// `initialize`, `tools/list`, or a `notifications/cancelled`.
const WORKER_COUNT: usize = 4;
/// P1b queue cap: more than 8 tool calls in flight gets -32603
/// ("Server busy") instead of unbounded thread growth.
const MAX_IN_FLIGHT: usize = 8;

/// P1b input serialization: only mutating desktop tools (`do`, `window`)
/// hold this. Read-only `screenshot`, `list_windows`, `cursor`, `elements`
/// never take it, so a screenshot can run while a `do wait` sleeps. A waiter
/// polls with `try_lock` + 20ms sleeps so a cancel wakes it instead of
/// wedging on a hung holder.
static INPUT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Acquire the shared input lock, polling so a cancelled call wakes instead
/// of blocking forever on a hung holder. `None` = cancelled while waiting.
/// A poisoned lock is recovered (`into_inner`): a panicked holder must not
/// read as "held forever" to every future waiter.
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

/// One `writeln!` + flush under the sink lock: frames never interleave.
/// Poison is recovered: a panicked writer must not silence the server.
fn emit_line<W: Write>(sink: &Mutex<W>, line: String) -> std::io::Result<()> {
    let mut g = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    writeln!(g, "{line}")?;
    g.flush()
}

/// Frame builders, shared by the inline path and the worker's locked
/// check+write emission (DEFECT 3: the last cancel check and the write
/// happen under the same sink lock).
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
        serde_json::to_string(INIT_INSTRUCTIONS).unwrap()
    )
}

/// Usage contract for a client/model that has never seen this server (gap
/// 3): the six tools in one line each, the coordinate rule with the
/// image-to-screen formula, batch + dry_run, and read-only-first on the
/// owner's real desktop.
const INIT_INSTRUCTIONS: &str = "puppet desktop-control MCP (Windows). Background-safe by default: every acting tool takes a `target` selector {hwnd?,pid?,exe?,appId?,titleContains?,className?} that resolves ONE window for the call and is revalidated before every action - no cursor movement, no keyboard injection into the owner's session, no focus stealing, no raised window, whether the target is foreground or alt-tabbed. There is no fallback to the foreground window: an ambiguous or vanished target is a typed error (ambiguous_target / target_gone). Foreground input (SendInput/SetForegroundWindow), pointer movement, and overwriting the owner's global clipboard (clipboard_set) are refused with foreground_required / cursor_move_required / clipboard_write_required unless the call passes allow_foreground:true / allow_cursor:true / allow_clipboard:true, before anything is sent or written. Control is exact and instant: clicks are one down+up at the resolved point, no interpolated pointer journey.\nTools (one line each):\n- screenshot: capture the whole virtual desktop (display=-1), one monitor, a window (window=<selector> or target={...}), or a region, without activating the window; minimized returns window_minimized, a refused frame capture_unavailable; returns the image plus source{x,y,w,h}, scale, the monitor list, the cursor, the image-to-screen formula, and foregroundUnchanged/cursorUnchanged. Coordinates are screen/virtual-desktop pixels: screen_x = source.x + round((img_x + 0.5) * source.w / image.w) (same for y).\n- list_windows: read-only visible top-level windows (hwnd/pid/exe/title/rects/elevated); never changes focus.\n- window: focus|move|resize|minimize|maximize|restore|close|topmost by selector, hwnd or target; only action=focus may touch the foreground window, every other verb is no-steal (SWP_NOACTIVATE / SW_SHOWMINNOACTIVE).\n- do: BURST tool: one call runs an ordered actions[] batch (move|click|down|up|drag|scroll|type|key|hotkey|wait|wait_for|focus_window|invoke|set_value|clipboard_set|clipboard_get) against one `target` window through UIA patterns first, then PostMessage/SendMessage with exact client coordinates; per-action ok, never aborts the rest. Under target, x/y are CLIENT-area physical pixels; without a target they are virtual-desktop pixels on the gated foreground path. dry_run=true validates and reports without injecting anything.\n- cursor: read-only cursor position plus monitor rects.\n- elements: read-only structured UI elements for a window (target/title/hwnd; zero pixels, no focus change); each entry carries ref plus fp, a stable identity fingerprint. Prefer elements over pixel guessing; pass expect_fp to invoke/set_value to refuse stale refs instead of acting on the wrong control. When text is illegible, re-request the same window with a tight region plus max_px:null (full-resolution zoom).";

fn tools_list_result() -> String {
    format!("{{\"tools\":[{TOOL_SCREENSHOT},{TOOL_LIST_WINDOWS},{TOOL_WINDOW},{TOOL_DO},{TOOL_CURSOR},{TOOL_ELEMENTS}]}}")
}

/// `server/discover` (2026-07-28 discovery path): the full known-version
/// list, the tools-only capabilities, this crate's version, and the same
/// usage contract `initialize` carries. Discovery itself is a modern
/// surface, so its result always carries `resultType: complete`.
fn discover_result() -> String {
    let supported: Vec<Value> = ALL_VERSIONS.iter().map(|v| Value::from(*v)).collect();
    format!(
        "{{\"supportedVersions\":{},\"capabilities\":{{\"tools\":{{\"listChanged\":false}}}},\"serverInfo\":{{\"name\":\"puppet\",\"version\":{}}},\"instructions\":{},\"resultType\":\"complete\"}}",
        serde_json::to_string(&Value::Array(supported)).unwrap(),
        serde_json::to_string(env!("CARGO_PKG_VERSION")).unwrap(),
        serde_json::to_string(INIT_INSTRUCTIONS).unwrap()
    )
}

/// Per-request modern version from
/// `params._meta["io.modelcontextprotocol/protocolVersion"]` (2026-07-28
/// discovery path). `None` when params is not an object or carries no such
/// key (legacy peer); otherwise the raw value so unsupported versions can
/// be echoed back in the -32022 data.
fn meta_version(v: &Value) -> Option<Value> {
    v.get("params")?
        .as_object()?
        .get("_meta")?
        .as_object()?
        .get("io.modelcontextprotocol/protocolVersion")
        .cloned()
}

/// Additive modern marker for 2026-07-28 peers: `result` is already a JSON
/// object string, so splice one more top-level key before the final `}`.
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

/// Zero-pixel structured enumeration: resolve the window selector, then reuse
/// the screenshot element walk (`shot::element_model`, the same object as
/// `shot::elements_block`) and return it as structured JSON.
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
    // Target lock wins; the legacy title/hwnd pair stays for compatibility.
    // The LOCK is kept (not just its hwnd) so the identity is revalidated
    // immediately before the walk.
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
        None => match (h, t) {
            (Some(v), _) => resolve_selector(&format!("hwnd:{v}")),
            (None, Some(v)) if !v.is_empty() => resolve_selector(&v),
            _ => (
                None,
                Some("bad_window: need title or hwnd".to_string()),
                None,
            ),
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
    // Revalidate immediately before enumerating: a closed or recycled
    // handle must be a typed target_gone, not a walk of another window.
    if let Err(e) = crate::screen::revalidate_locked(&locked) {
        let mut payload = Map::new();
        payload.insert("ok".to_string(), Value::from(false));
        payload.insert("code".to_string(), Value::from("target_gone"));
        payload.insert("error".to_string(), Value::from(e));
        return wrapped_text(&py_dumps(&Value::Object(payload)));
    }
    let model = element_model(hwnd_r.unwrap(), md, off as usize);
    // P4c text read: detail listings on the UIA backend carry each entry's
    // TextPattern text (capped at ELEMENT_TEXT_CAP chars with a
    // `text_truncated` flag). Best-effort per entry: a missing pattern or
    // failed read omits the keys instead of failing the listing. Identity
    // (`fp`) never covers text, so extraction cannot invalidate refs.
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
    // Same top-level object as `shot::elements_block`, as structured JSON
    // (both assemble through `shot::elements_object`; only the entries
    // slice differs: full walk entries under detail=true, compact
    // projections by default).
    let o = elements_object(
        &model.backend,
        model.truncated,
        entries,
        &model.detail,
        model.next_offset,
    );
    let mut o = o;
    // Invariant telemetry: this walk is read-only and must leave the owner's
    // foreground window and pointer exactly where it found them.
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
        target: target.unwrap(),
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
    // background_args is `list[str] | None`, hidden-launch only: strict
    // strings, no coercion (mirrors background's strict string_type).
    // Ignored without desktop:"hidden"; validated whenever present.
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

/// `target`: object | selector string | null. The SHAPE is validated here
/// (non-object/string is a type error); the window is resolved in the
/// handler, where a vanished or ambiguous target is a typed result.
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

/// A dispatched `tools/call` job. `id_key` is the canonical registry key
/// (`serde_json::to_string(&id)`); `modern` marks 2026-07-28 peers whose
/// result carries `resultType: complete`.
struct ToolJob {
    id: Value,
    id_key: String,
    params: Value,
    modern: bool,
}

/// Admission guard: releases `in_flight` and clears the worker's
/// thread-local token on drop, including panic unwind. It deliberately does
/// NOT touch the cancel registry: the token stays registered until the
/// outer `run_tool_job` removes it AFTER the emit decision, so a cancel
/// racing a panic is still observed — no -32603 after a processed cancel.
///
/// Lifetime (DEFECT 1 fix): the guard is created in `run_tool_job` BEFORE
/// `catch_unwind` and dropped AFTER the emit decision + token removal, so
/// admission covers the whole job — body, unwind, sink-locked emit, and
/// cleanup. The EOF drain (which waits on `in_flight`) therefore cannot
/// finish before the final response is written and the token removed. The
/// inner never owns or drops the guard.
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

/// What a job's execution produced. The inner builds it but never writes;
/// the outer emits (or drops) under the sink lock, then removes the token.
enum Action {
    Result(String),
    InvalidParams,
}

/// Outcome of the fallible job body: either a response line is ready or
/// there is nothing to send.
enum JobOutcome {
    Emit(String),
    Drop,
}

/// The production executor: validates arguments and runs the tool.
/// Passed as a closure so tests can substitute panicking bodies while
/// exercising the real containment, ordering, and cleanup paths.
fn production_execute(job: &ToolJob) -> Action {
    match job.params.get("arguments") {
        None | Some(Value::Null) | Some(Value::Object(_)) => {
            let r = tools_call(&job.params);
            Action::Result(if job.modern { with_result_type(&r) } else { r })
        }
        Some(_) => Action::InvalidParams,
    }
}

/// Worker entry: panic containment boundary. The admission guard is
/// created BEFORE `catch_unwind` and dropped AFTER the emit decision +
/// token removal, so `in_flight` covers the whole job and the EOF drain
/// cannot overtake the final response. The fallible body runs inside
/// `catch_unwind`; on unwind the guard stays alive while the outer emits
/// -32603, and this worker stays alive for the next job. A panic never
/// aborts the process. Token removal happens here, AFTER the emit decision,
/// never in the guard or the inner: either the cancel is ordered before the
/// check under the sink lock (token set → silence) or the frame is written
/// first (valid race). A cancelled call — including a panic after a
/// processed cancel — stays silent.
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
    // Snapshot the token outside the unwind boundary so the panic branch
    // can still tell "cancelled, stay silent" from "died, report".
    let token = crate::cancel::get(&id_key);
    // Admission covers body + unwind + sink-locked emit + token removal.
    let _admission = AdmissionGuard::new(&in_flight);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_tool_job_inner(&job, token.clone(), execute)
    }));
    match outcome {
        Ok(JobOutcome::Emit(line)) => {
            // Ordered with `notifications/cancelled` under the sink lock:
            // the cancel handler takes this lock before setting the token,
            // so either the cancel lands first (silent drop) or this frame
            // is fully written first (client may ignore the racy late
            // response). The lock is held only across check+write, never
            // across tool execution.
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
            // Contained panic: the guard is still alive (admission held,
            // thread-local intact for the check below); the token stayed
            // registered, so this check observes any cancel ordered first.
            // The guard drops at fn end, after the emit + removal.
            let mut g = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let cancelled = token.as_ref().is_some_and(|t| t.load(Ordering::SeqCst));
            if !cancelled && token.is_some() {
                let line = error_line(&id, -32603, "Internal error");
                if writeln!(g, "{line}").and_then(|_| g.flush()).is_err() {
                    client_dead.store(true, Ordering::SeqCst);
                }
            }
            // `token: None` means the registry never held this id; nothing
            // to emit or remove.
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
    // No admission handling here: the outer's guard covers the whole job.
    // A `None` token means the registry never held this id; drop silently.
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
    // Only mutating desktop tools serialize; read-only tools run free.
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
    // Advisory only: the outer re-checks under the sink lock.
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

/// Inline fast frame: everything except `tools/call`. Returns true when the
/// caller should stop the pump with code 1 (dead client).
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
        // Strict JSON-RPC: a frame with an id but no valid method/jsonrpc
        // is Invalid Request (-32600); a notification (no id) stays silent.
        return match id {
            None => false,
            Some(id) => write_error(sink, &id, -32600, "Invalid Request").is_err(),
        };
    }
    let m = v.get("method").and_then(|m| m.as_str()).unwrap();
    let Some(id) = id else {
        // Notification: valid method but no id gets no response.
        // `notifications/cancelled` flips the matching token; unknown or
        // already-completed ids are ignored. The sink lock is taken before
        // setting the token — the same lock the worker holds across its
        // last cancel check + write — so a cancel is either ordered before
        // the response (silent drop) or processed after it was fully
        // written (client may ignore the racy late response).
        if m == "notifications/cancelled" {
            if let Some(rid) = v.get("params").and_then(|p| p.get("requestId")) {
                let key = serde_json::to_string(rid).unwrap();
                let _g = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                crate::cancel::cancel(&key);
            }
        }
        return false;
    };
    // Modern per-request negotiation (2026-07-28 discovery path): an
    // explicit per-request version outside the known set is rejected with
    // -32022 before dispatch.
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
                // Workers are gone; unwind the registration so the id does
                // not leak a token and the admission slot is released.
                // (Unreachable in practice: workers outlive the reader.)
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
        // The loop survives a contained worker panic (run_tool_job catches
        // it); the receiver lock is poison-recovered so capacity persists.
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
        // A worker that could not write its tool response raises this flag;
        // exit even while the reader is still open (blocked, no EOF).
        if client_dead.load(Ordering::SeqCst) {
            return 0;
        }
        let line = match line_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(line) => line,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            // Reader thread ended (EOF) or the read failed: drain and exit.
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
            // A valid JSON scalar is well-formed but never a request frame.
            if write_error(sink_ref, &Value::Null, -32600, "Invalid Request").is_err() {
                return 1;
            }
            continue;
        }
        if handle_fast(&v, sink_ref, &tx, &in_flight) {
            return 1;
        }
    }
    // Stdin EOF: stop accepting, wait up to 2s for in-flight calls, then
    // exit 0. Never hang forever on a wedged worker.
    drop(tx);
    let deadline = Instant::now() + Duration::from_secs(2);
    while in_flight.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    // A hung PrintWindow/UIA call occupies one worker at most; the reader
    // and ping stayed alive throughout, so EOF still exits here.
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

    /// Mirrors production dispatch (register + admission increment) so test
    /// accounting is exact: every dispatched test job starts at the same
    /// counter state a worker would see.
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

    /// DEFECT 1 (pivot) proof: the token stays registered across a panic so
    /// the outer — not the guard — decides emit vs silence after the unwind.
    /// Fails if token removal is reverted into the guard/inner (registry
    /// lookup finds nothing).
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
        // The inner never touches admission: the outer guard owns it.
        assert_eq!(in_flight.load(Ordering::SeqCst), 1);
        crate::cancel::remove(&id_key);
        in_flight.fetch_sub(1, Ordering::SeqCst);
    }

    /// Production containment through the real `run_tool_job` path with a
    /// panicking executor: -32603 for the id, admission released, token
    /// removed — and the same worker (thread) processes a subsequent normal
    /// job, proving survival.
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
        // Same thread processes a subsequent normal job: the worker lives.
        // Exact accounting: every dispatched job increments first.
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

    /// A cancel processed before the panic emit suppresses the -32603:
    /// the executor cancels through the registry, then panics; the outer
    /// observes the set token under the sink lock and stays silent.
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

    /// DEFECT 2 (pivot) proof through production dispatch: the input lock is
    /// held here, so the `do` job must wait on it; it is cancelled while
    /// waiting. The observing executor must never run, no frame is emitted,
    /// and admission + token are cleaned up. This exercises the real
    /// `run_tool_job` path, not the lock helper in isolation.
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
        // The worker is either at an entry check or in the lock wait; the
        // lock is held here, so the body cannot run before the cancel below.
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

    /// DEFECT 1 proof: admission covers the whole job including the
    /// sink-locked emit. The sink blocks mid-write; while it is blocked,
    /// `in_flight` must still be >0 (the EOF-drain condition: EOF must not
    /// overtake the response), and only after the response lands is it 0.
    /// Fails if the guard is dropped before the emit decision.
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
        // Wait until the worker is blocked inside the emit write.
        let t0 = Instant::now();
        while !entered.load(Ordering::SeqCst) {
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "worker must reach the emit"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // EOF-drain condition: admission still held while emitting.
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

    /// DEFECT 4 proof: when the worker queue is gone, dispatch unwinds the
    /// registration (token removed) as well as the admission slot.
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

    /// DEFECT 2 proof: a tool-response write failure is surfaced through the
    /// shared dead-client flag (not discarded), and cleanup still runs.
    #[test]
    fn failing_tool_writer_sets_client_dead_with_cleanup() {
        // Read-only `cursor`: no desktop mutation, exercises the emit path.
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

    /// DEFECT 2 exit behavior: the reader is still open (blocked, no EOF)
    /// when the worker observes the dead client, so exit 0 can only come
    /// from the `client_dead` flag — the EOF path is unreachable here.
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
            // Block forever without EOF: the pump must not wait for us.
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
        // Verifier repro: MIN/MAX coords must be a typed error, never a
        // debug panic (subtraction overflow) or release bypass.
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
        // Direct-rect grab: the image matches the region exactly and the
        // capture note says no full-frame grab happened.
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
        outer
            .get("structuredContent")
            .cloned()
            .expect("structuredContent is the elements object")
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
        // Determinism note (was flaky: failed once, passed on rerun): the
        // UIA provider exposes the fixture's children asynchronously after
        // ShowWindow, so the first walk can return the win32 fallback or a
        // partial UIA list. The old proxy poll (`backend == "uia" && n >= 3`,
        // 5 s) still asserted `backend == "uia"`, which flaked when UIA was
        // slow — a timing flake, not ordering or environment. Poll for the
        // asserted conditions themselves (Edit + OK present, either
        // backend); the win32 fallback lists the same children
        // synchronously, so it is the deterministic floor.
        let payload = {
            let t0 = std::time::Instant::now();
            loop {
                let p = elements_payload(&elements_args(&[("hwnd", Value::from(hwnd))]));
                let els = p
                    .get("elements")
                    .and_then(|v| v.as_array())
                    .map(|a| a.as_slice())
                    .unwrap_or(&[]);
                let has_edit = els
                    .iter()
                    .any(|e| e.get("role").and_then(|v| v.as_str()) == Some("Edit"));
                let has_ok = els.iter().any(|e| {
                    e.get("name").and_then(|v| v.as_str()) == Some("OK")
                        && e.get("role").and_then(|v| v.as_str()) == Some("Button")
                });
                if (has_edit && has_ok) || t0.elapsed() > std::time::Duration::from_secs(10) {
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
        let backend = payload
            .get("backend")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            backend == "uia" || backend == "win32",
            "backend must be uia or the deterministic win32 fallback, got: {backend} in {payload}"
        );
        // Foreground untouched by a zero-pixel read: owned-foreground after
        // settle is reported (FG_OWN_CHURN), never failed — a read cannot
        // activate (skips with FG_SKIP when the environment provides no
        // foreground session, e.g. headless CI).
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
