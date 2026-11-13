//! The claims source (D1).
//!
//! Every load-bearing claim an agent can act on is written here exactly once:
//! the tool lines, the coordinate rule, the refusal table, the authoritative
//! code list, the invariants and the named limits. `instructions()` is
//! assembled from these consts, and `tests/contract_truth.rs` pins them to
//! `tools/list` and to the source that emits them.
//!
//! Nothing in this module talks to Windows. A claim is a string; the code
//! that honours it lives in `gate`, `burst`, `uia`, `shot`, `cap`, `winop`.

/// The seven tools, one line each, in `tools_list_result` order.
pub const TOOL_LINES: [(&str, &str); 7] = [
    (
        "screenshot",
        "capture the whole virtual desktop (display=-1), one monitor, a window (window=<selector> or target={...}) or a region, without activating the window; returns the image, source{x,y,w,h}, scale, the monitor list and the image-to-screen formula",
    ),
    (
        "diff",
        "compare two captures (base/next: window selector, file path or the previous capture) pixel by pixel; returns changed, total, ratio, bbox, identical and the optional diff image",
    ),
    (
        "list_windows",
        "read-only visible top-level windows (hwnd/pid/exe/title/rects/elevated); never changes focus",
    ),
    (
        "window",
        "move|resize|minimize|restore|close|topmost by selector, hwnd or target, all no-steal (SWP_NOACTIVATE / SW_SHOWMINNOACTIVE); focus and maximize are refused with no_foreground_policy because they take the foreground or cover a monitor",
    ),
    (
        "do",
        "BURST tool: one call runs an ordered actions[] batch against one target through UIA patterns first, then PostMessage/SendMessage; per-action ok, never aborts the rest",
    ),
    ("cursor", "read-only cursor position plus monitor rects"),
    (
        "elements",
        "read-only structured UI elements for a window (target/title/hwnd; zero pixels, no focus change); each entry carries ref, fp, rect, offscreen and invokable",
    ),
];

/// Which space a coordinate is in. The single rule every tool obeys.
pub const COORD_RULE: &str = "`do` x/y under a `target` are CLIENT-area physical pixels of that window, used as given; without a target they are virtual-desktop SCREEN pixels, converted with ScreenToClient; rects from `elements` and `list_windows` are always SCREEN pixels; a screenshot image pixel maps to screen with screen_x = source.x + round((img_x + 0.5) * source.w / image.w) and the same for y";

/// Every code the server can emit, one entry each. The authoritative list:
/// a code that is not here does not exist, and a code that is here is
/// emitted somewhere outside this file.
pub const ERROR_CODES: &[&str] = &[
    "ambiguous_target",
    "ambiguous_window",
    "background_unavailable",
    "bad_coordinates",
    "bad_display",
    "bad_out",
    "bad_region",
    "bad_target",
    "bad_window",
    "burst_budget_exceeded",
    "capture_failed",
    "capture_timeout",
    "capture_unavailable",
    "empty_crop",
    "no_clipboard_policy",
    "no_cursor_policy",
    "no_foreground_policy",
    "offscreen_element",
    "size_mismatch",
    "stale_ref",
    "target_gone",
    "target_required",
    "uia_timeout",
    "uipi_blocked",
    "window_minimized",
];

/// The verb matrix, derived from `Gate::decide` (`src/gate.rs`): what each
/// action type can be delivered as, and the code that refuses it when its
/// precondition is missing.
///
/// `class` is one of `posted` (a window message to the locked target),
/// `pattern` (a UIA pattern on the locked target), `read` (nothing is
/// delivered) or `refused` (no flag unlocks it). A verb with two rows takes
/// both paths; the `refused` row of such a verb is its element form.
pub const REFUSALS: &[(&str, &str, &str)] = &[
    ("move", "refused", "no_cursor_policy"),
    ("click", "posted", "no_cursor_policy"),
    ("click", "pattern", "target_required"),
    ("down", "posted", "no_cursor_policy"),
    ("up", "posted", "no_cursor_policy"),
    ("drag", "posted", "no_cursor_policy"),
    ("scroll", "posted", "no_cursor_policy"),
    ("scroll", "pattern", "target_required"),
    ("type", "posted", "target_required"),
    ("type", "refused", "background_unavailable"),
    ("key", "posted", "target_required"),
    ("hotkey", "posted", "target_required"),
    ("wait", "read", ""),
    ("wait_for", "read", ""),
    ("focus_window", "refused", "no_foreground_policy"),
    ("invoke", "pattern", "target_required"),
    ("set_value", "refused", "background_unavailable"),
    ("clipboard_set", "refused", "no_clipboard_policy"),
    ("clipboard_get", "read", ""),
];

/// The invariants (I1–I6), as an agent reads them.
pub const INVARIANTS: &[(&str, &str)] = &[
    (
        "I1",
        "SendInput, SetCursorPos, SetForegroundWindow, BringWindowToTop and mouse_event have no call site anywhere: the mechanism to steal cannot be called",
    ),
    (
        "I2",
        "every refusal happens before anything is sent, under dry_run too",
    ),
    (
        "I3",
        "the target lock is revalidated before every action, so a vanished or replaced window refuses instead of being driven",
    ),
    (
        "I4",
        "nothing falls back to the foreground window: without a target an acting verb refuses",
    ),
    (
        "I5",
        "the server never elevates itself and never escalates a permission",
    ),
    (
        "I6",
        "a property that never appears means the control has no such pattern, never that it is false",
    ),
];

/// The named limits: what this server cannot do, and the readings an agent
/// gets wrong. A limit an agent cannot read is not a limit.
pub const LIMITS: &[(&str, &str)] = &[
    (
        "bursts",
        "a burst is not atomic across a service restart: actions already sent are not undone, so keep bursts short and idempotent",
    ),
    (
        "ok_vs_effect",
        "`ok` means the action was delivered, never that the target acted: read `effect` for what actually happened",
    ),
    (
        "wait_for",
        "`satisfied` is the condition's verdict and `ok` is the action's delivery: a timed-out wait_for reports satisfied:false with ok:true",
    ),
    (
        "clipboard",
        "clipboard_get truncates text past 1048576 chars and reports truncated:true",
    ),
    (
        "ime",
        "IME, dead-key and AltGr composition are not supported: there is no composition path on posted WM_CHAR",
    ),
    (
        "touch",
        "touch, pen and stylus input are not supported: there is no schema surface and no way to synthesize it in the background",
    ),
    (
        "drag_and_drop",
        "cross-window drag-and-drop is not supported: drag is three posted messages to one window",
    ),
    (
        "clamping",
        "a point outside the target client rect is posted into client message space and lands nowhere",
    ),
    (
        "hdr",
        "captures are SDR-composited, untagged bytes: there is no HDR or wide-gamut conversion",
    ),
    (
        "raw_input",
        "an app reading raw input (DirectInput) ignores posted messages and cannot be driven",
    ),
    (
        "elevation",
        "an elevated target cannot be driven: the refusal is typed and the server never elevates itself",
    ),
];

/// The usage contract a client that has never seen this server reads first.
/// Assembled from the consts above; no claim is typed twice.
pub fn instructions() -> String {
    let mut s = String::new();
    s.push_str(
        "puppet desktop-control MCP (Windows). Background-safe by default: every acting tool \
         takes a `target` selector {hwnd?,pid?,exe?,appId?,titleContains?,className?} that \
         resolves ONE window for the call and is revalidated before every action. No cursor \
         movement, no keyboard injection into the owner's session, no focus stealing, no raised \
         window, whether the target is foreground or alt-tabbed. An ambiguous or vanished target \
         is a typed error (ambiguous_target / target_gone). There is no escalation path: the \
         allow_foreground:true, allow_cursor:true and allow_clipboard:true options are accepted by \
         the schema and refused by the policy, so a refused call touches nothing.\n",
    );

    s.push_str("\nTools (one line each):\n");
    for (name, line) in TOOL_LINES {
        s.push_str(&format!("- {name}: {line}\n"));
    }

    s.push_str(&format!("\nCoordinates: {COORD_RULE}.\n"));

    // The verb matrix, grouped by class. Refused verbs name the code that
    // refuses them, so a caller never has to guess which door is bricked.
    s.push_str("\nVerbs by delivery class:\n");
    for class in ["posted", "pattern", "read", "refused"] {
        let verbs: Vec<String> = REFUSALS
            .iter()
            .filter(|(_, c, _)| *c == class)
            .map(|(verb, c, code)| {
                // The element form is the qualified one: a pattern row is
                // always element-driven, and a refused row of a verb that
                // also has a delivered row is that verb's element form.
                let element_form = *c == "pattern"
                    || (*c == "refused"
                        && REFUSALS
                            .iter()
                            .any(|(v, oc, _)| v == verb && *oc != "refused"));
                let name = if element_form {
                    format!("{verb} with an element")
                } else {
                    verb.to_string()
                };
                if *c == "refused" {
                    format!("{name} ({code})")
                } else {
                    name
                }
            })
            .collect();
        let meaning = match class {
            "posted" => "window messages to the locked target",
            "pattern" => "UIA patterns on the locked target",
            "read" => "nothing is delivered",
            _ => "no flag unlocks them",
        };
        s.push_str(&format!("- {class} ({meaning}): {}\n", verbs.join(", ")));
    }

    s.push_str("\nInvariants:\n");
    for (id, line) in INVARIANTS {
        s.push_str(&format!("- {id}: {line}\n"));
    }

    s.push_str("\nLimits:\n");
    for (name, line) in LIMITS {
        s.push_str(&format!("- {name}: {line}\n"));
    }

    s.push_str(&format!(
        "\nError codes (every code the server can emit): {}.\n",
        ERROR_CODES.join(", ")
    ));
    s.push_str(
        "dry_run=true validates exactly like the live call (every policy refusal included) but \
         posts nothing.",
    );
    s
}
