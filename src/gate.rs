//! The no-steal input gate.
//!
//! Owner contract: the server must never move the owner's pointer, inject
//! keys into their session, or steal focus unless the CALL ITSELF asked for
//! it. This module is the only place the forbidden APIs appear
//! (`SendInput`, `SetCursorPos`, `SetForegroundWindow`, `BringWindowToTop`,
//! `mouse_event`), and each wrapper demands a permit token that can only be
//! minted from the matching explicit flag:
//!
//! | token              | minted by                       |
//! |--------------------|---------------------------------|
//! | `ForegroundPermit` | `allow_foreground: true`        |
//! | `CursorPermit`     | `allow_cursor: true`            |
//! | `ClipboardPermit`  | `allow_clipboard: true`         |
//!
//! Writing the owner's global clipboard is a side effect outside the target
//! window (every application shares that clipboard), so it is an explicit
//! opt-in like the other two; reads (`clipboard_get`) stay free.
//!
//! There are exactly three constructors, all flag-only. The explicit
//! `window` action=focus verb is double-explicit: the verb AND
//! `allow_foreground:true` are required, so no code path can reach a focus
//! call without the flag.
//!
//! So the foreground path is not merely "unused by default": it is not
//! reachable from code that never received the flag. `tests/no_steal.rs`
//! pins this by scanning the sources: those symbols appear only here.
//!
//! `Gate::decide` is the pure policy above that — verb in, delivery or typed
//! refusal out. It is unit-tested against the full matrix, and every refusal
//! happens BEFORE any message or event is produced.

use serde_json::{Map, Value};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, SetCursorPos,
    SetForegroundWindow,
};

pub const FOREGROUND_REQUIRED: &str = "foreground_required";
pub const CURSOR_REQUIRED: &str = "cursor_move_required";
/// Writing the owner's clipboard is refused unless `allow_clipboard:true`.
pub const CLIPBOARD_REQUIRED: &str = "clipboard_write_required";
pub const BACKGROUND_UNAVAILABLE: &str = "background_unavailable";
pub const TARGET_REQUIRED: &str = "target_required";

/// Proof the caller passed `allow_foreground: true`. Not `Clone`, so it
/// cannot be stashed and reused to reach the foreground path lazily.
pub struct ForegroundPermit(());

/// Proof the caller passed `allow_cursor: true`.
pub struct CursorPermit(());

/// Proof the caller passed `allow_clipboard: true`.
pub struct ClipboardPermit(());

impl ForegroundPermit {
    /// The ONLY constructor. `None` means the call did not ask to touch the
    /// owner's foreground session.
    pub fn grant(allow_foreground: bool) -> Option<Self> {
        if allow_foreground {
            Some(ForegroundPermit(()))
        } else {
            None
        }
    }
}

impl CursorPermit {
    /// The ONLY constructor. `None` means the call did not ask to move the
    /// owner's pointer.
    pub fn grant(allow_cursor: bool) -> Option<Self> {
        if allow_cursor {
            Some(CursorPermit(()))
        } else {
            None
        }
    }
}

impl ClipboardPermit {
    /// The ONLY constructor. `None` means the call did not ask to overwrite
    /// the owner's global clipboard.
    pub fn grant(allow_clipboard: bool) -> Option<Self> {
        if allow_clipboard {
            Some(ClipboardPermit(()))
        } else {
            None
        }
    }
}

/// `SendInput` behind a foreground permit. Returns the events accepted.
pub fn send_input(_permit: &ForegroundPermit, events: &[INPUT]) -> u32 {
    if events.is_empty() {
        return 0;
    }
    unsafe {
        // FORBIDDEN unless allow_foreground:true - the permit is the flag.
        SendInput(
            events.len() as u32,
            events.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    }
}

/// `SetCursorPos` behind a cursor permit. Only the gated SendInput path uses
/// it; the background path never needs the pointer.
pub fn set_cursor_pos(_permit: &CursorPermit, x: i32, y: i32) -> bool {
    // FORBIDDEN unless allow_cursor:true - the permit is the flag.
    unsafe { SetCursorPos(x, y) != 0 }
}

/// `SetForegroundWindow` (+ `BringWindowToTop`) behind a foreground permit,
/// with the AttachThreadInput dance the unfocused case needs. The sole
/// non-explicit callers are `window` action=focus and `do` focus_window /
/// focus_guard, each of which passes a permit minted from its own flag.
pub fn set_foreground(_permit: &ForegroundPermit, hwnd: i64) -> bool {
    if hwnd <= 0 {
        return false;
    }
    unsafe {
        let h: HWND = hwnd as HWND;
        if windows_sys::Win32::UI::WindowsAndMessaging::IsWindow(h) == 0 {
            return false;
        }
        let cur_tid = GetCurrentThreadId();
        let fg = GetForegroundWindow();
        let mut fg_tid: u32 = 0;
        let mut tgt_tid: u32 = 0;
        if !fg.is_null() {
            GetWindowThreadProcessId(fg, &mut fg_tid);
        }
        GetWindowThreadProcessId(h, &mut tgt_tid);
        let mut attached: Vec<u32> = Vec::new();
        if fg_tid != 0 && fg_tid != cur_tid && AttachThreadInput(cur_tid, fg_tid, 1) != 0 {
            attached.push(fg_tid);
        }
        if tgt_tid != 0
            && tgt_tid != cur_tid
            && tgt_tid != fg_tid
            && AttachThreadInput(cur_tid, tgt_tid, 1) != 0
        {
            attached.push(tgt_tid);
        }
        // FORBIDDEN unless allow_foreground:true, or the explicit focus verb
        // (the only other permit mint). Nothing else can reach this line.
        BringWindowToTop(h);
        SetForegroundWindow(h);
        for tid in attached {
            AttachThreadInput(cur_tid, tid, 0);
        }
        GetForegroundWindow() == h
    }
}

/// Which delivery path a verb takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// UIA pattern on a locked target (Invoke/Toggle/Expand/SelectionItem/
    /// ValuePattern/Scroll) — no pointer, no foreground, no messages.
    Uia,
    /// `PostMessage`/`SendMessage` to the locked hwnd with exact client coords.
    Posted,
    /// The legacy SendInput path — reachable only with the permit that the
    /// gate already verified.
    Input,
    /// Nothing to deliver (wait, clipboard, explicit focus).
    None,
}

/// Typed gate refusal, rendered as `"<code>: <message>"`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateError {
    pub code: &'static str,
    pub message: String,
}

impl GateError {
    pub fn text(&self) -> String {
        format!("{}: {}", self.code, self.message)
    }

    pub fn to_map(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("ok".to_string(), Value::from(false));
        m.insert("code".to_string(), Value::from(self.code));
        m.insert("error".to_string(), Value::from(self.text()));
        m
    }
}

fn foreground_refusal(verb: &str) -> GateError {
    GateError {
        code: FOREGROUND_REQUIRED,
        message: format!(
            "{verb} would inject into the foreground session (SendInput/SetForegroundWindow); \
             pass target:{{...}} for background delivery, or allow_foreground:true to act on \
             the foreground window on purpose. Nothing was sent."
        ),
    }
}

/// UIA text entry (`ValuePattern.SetValue`) can make the provider focus the
/// control, so it is opt-in even though nothing is posted.
fn uia_text_refusal(verb: &str) -> GateError {
    GateError {
        code: FOREGROUND_REQUIRED,
        message: format!(
            "{verb} writes text through UIA ValuePattern.SetValue, whose provider may focus the \
             target control; pass allow_foreground:true to accept that focus change. Nothing was \
             written."
        ),
    }
}

fn keyboard_refusal(verb: &str) -> GateError {
    GateError {
        code: FOREGROUND_REQUIRED,
        message: format!(
            "{verb} injects keyboard input; posted WM_CHAR/WM_KEYDOWN are not delivered reliably \
             (they bypass the input queue, desync the target and are ignored by many controls), \
             so none are posted. Use type with an element (UIA ValuePattern.SetValue) for \
             background text, or pass allow_foreground:true for real SendInput. Nothing was sent."
        ),
    }
}

fn cursor_refusal(verb: &str) -> GateError {
    GateError {
        code: CURSOR_REQUIRED,
        message: format!(
            "{verb} would move the system pointer (SetCursorPos/SendInput); pass \
             allow_cursor:true to move the owner's cursor on purpose, or target:{{...}} for an \
             exact posted click with no pointer. Nothing was sent."
        ),
    }
}

/// Overwriting the owner's global clipboard is a side effect OUTSIDE the
/// target window: every application shares that clipboard, so it is an
/// explicit opt-in like foreground and cursor input.
pub(crate) fn clipboard_refusal(verb: &str) -> GateError {
    GateError {
        code: CLIPBOARD_REQUIRED,
        message: format!(
            "{verb} would overwrite the owner's global clipboard, a side effect outside the \
             target window (every application shares it); pass allow_clipboard:true to \
             overwrite it on purpose. Nothing was written."
        ),
    }
}

fn target_refusal(verb: &str) -> GateError {
    GateError {
        code: TARGET_REQUIRED,
        message: format!(
            "{verb} needs an explicit target window ({{\"hwnd\":..}} or another target field); \
             this server never falls back to the foreground window. Nothing was sent."
        ),
    }
}

fn background_refusal(verb: &str, why: &str) -> GateError {
    GateError {
        code: BACKGROUND_UNAVAILABLE,
        message: format!(
            "{verb} has no background delivery to a locked target: {why}. Nothing was sent."
        ),
    }
}

/// The flags a call carries, and whether it named a target.
#[derive(Clone, Copy, Debug)]
pub struct Gate {
    pub has_target: bool,
    pub allow_foreground: bool,
    pub allow_cursor: bool,
    /// True only when the call passed `allow_clipboard: true`. Writing the
    /// owner's global clipboard is a side effect outside the target window,
    /// so it is refused unless asked for by name.
    pub allow_clipboard: bool,
    /// True only for `desktop:"hidden"`: a private desktop the server created
    /// for one launched program. Posted keyboard is allowed there because
    /// there is no owner session to desync and no other way to drive it; on
    /// the owner's desktop posted keyboard is never used.
    pub private_desktop: bool,
}

impl Gate {
    pub fn new(has_target: bool, allow_foreground: bool, allow_cursor: bool) -> Self {
        Gate {
            has_target,
            allow_foreground,
            allow_cursor,
            allow_clipboard: false,
            private_desktop: false,
        }
    }

    /// Mark this call as permitted to overwrite the owner's global clipboard.
    /// The default (never calling this) refuses `clipboard_set`.
    pub fn allow_clipboard(mut self, allow: bool) -> Self {
        self.allow_clipboard = allow;
        self
    }

    /// Mark this call as driving a PRIVATE desktop (the server's own hidden
    /// desktop), the one place posted keyboard input remains allowed.
    pub fn private_desktop(mut self, private: bool) -> Self {
        self.private_desktop = private;
        self
    }

    /// A pointer act delivered through SendInput is BOTH a pointer move and
    /// foreground input, so it needs both flags. Refusing on the missing
    /// cursor flag first names the more specific act; the foreground refusal
    /// then names why a cursor-only call still cannot inject.
    fn pointer_input(&self, verb: &str) -> Result<Delivery, GateError> {
        if !self.allow_cursor {
            return Err(cursor_refusal(verb));
        }
        if !self.allow_foreground {
            return Err(foreground_refusal(verb));
        }
        Ok(Delivery::Input)
    }

    /// Decide the delivery path for one action, or refuse before acting.
    ///
    /// `has_coords`: x/y supplied. `has_element`: an element selector
    /// supplied. `has_ref`: a `rect`/ref point supplied.
    pub fn decide(
        &self,
        verb: &str,
        has_coords: bool,
        has_element: bool,
    ) -> Result<Delivery, GateError> {
        match verb {
            "move" => {
                if self.has_target {
                    // A cursor move is not representable as a window message.
                    Err(background_refusal(
                        "move",
                        "the pointer is not part of a window's message stream; use click x/y \
                         (posted) instead",
                    ))
                } else {
                    self.pointer_input("move")
                }
            }
            "click" => {
                if has_element {
                    if self.has_target {
                        Ok(Delivery::Uia)
                    } else {
                        Err(target_refusal("click on an element"))
                    }
                } else if has_coords {
                    if self.has_target {
                        Ok(Delivery::Posted)
                    } else {
                        self.pointer_input("click x/y")
                    }
                } else if self.has_target {
                    Err(background_refusal(
                        "click",
                        "a posted click needs exact client x/y (or an element)",
                    ))
                } else if self.allow_foreground {
                    Ok(Delivery::Input)
                } else {
                    Err(foreground_refusal("click"))
                }
            }
            "down" | "up" => {
                if self.has_target {
                    if has_coords {
                        Ok(Delivery::Posted)
                    } else {
                        Err(background_refusal(
                            verb,
                            "a posted button message needs exact client x/y",
                        ))
                    }
                } else if self.allow_foreground {
                    Ok(Delivery::Input)
                } else {
                    Err(foreground_refusal(verb))
                }
            }
            "drag" => {
                if self.has_target {
                    Ok(Delivery::Posted)
                } else {
                    self.pointer_input("drag")
                }
            }
            "scroll" => {
                if has_element {
                    if self.has_target {
                        Ok(Delivery::Uia)
                    } else {
                        Err(target_refusal("scroll on an element"))
                    }
                } else if has_coords {
                    if self.has_target {
                        Ok(Delivery::Posted)
                    } else {
                        self.pointer_input("scroll x/y")
                    }
                } else if self.has_target {
                    Err(background_refusal(
                        "scroll",
                        "a posted wheel message needs the exact point (x/y)",
                    ))
                } else if self.allow_foreground {
                    Ok(Delivery::Input)
                } else {
                    Err(foreground_refusal("scroll"))
                }
            }
            "type" | "key" | "hotkey" => {
                // `type` on an element is UIA ValuePattern.SetValue. The
                // provider may FOCUS the control to write it (measured on the
                // fixture EDIT), so UIA text entry is an explicit opt-in even
                // though it is not posted keyboard. UIA Invoke/expand/scroll
                // stay background-safe and are NOT gated this way.
                if verb == "type" && has_element && self.has_target {
                    if self.allow_foreground {
                        return Ok(Delivery::Uia);
                    }
                    return Err(uia_text_refusal(verb));
                }
                // Posted WM_CHAR / WM_KEYDOWN are not delivered reliably:
                // they bypass the input queue, desync the target and are
                // ignored by many controls (pywinauto tracks the same), so
                // keyboard input is never posted to the owner's desktop.
                if self.private_desktop && self.has_target {
                    return Ok(Delivery::Posted);
                }
                if self.allow_foreground {
                    // Explicit escalation: real SendInput into the foreground
                    // session.
                    return Ok(Delivery::Input);
                }
                Err(keyboard_refusal(verb))
            }
            "invoke" | "set_value" => {
                if !self.has_target {
                    return Err(target_refusal(verb));
                }
                // Invoke/toggle/expand are background-safe (no focus change
                // observed or documented); ValuePattern.SetValue is not: the
                // provider may focus the control, so it needs the explicit
                // foreground opt-in.
                if verb == "set_value" && !self.allow_foreground {
                    return Err(uia_text_refusal(verb));
                }
                Ok(Delivery::Uia)
            }
            "focus_window" => {
                if self.allow_foreground {
                    Ok(Delivery::Input)
                } else {
                    Err(foreground_refusal("focus_window"))
                }
            }
            // Writing the clipboard mutates owner-global state that lives
            // outside the target window; reads (clipboard_get) stay free.
            "clipboard_set" => {
                if self.allow_clipboard {
                    Ok(Delivery::None)
                } else {
                    Err(clipboard_refusal(verb))
                }
            }
            _ => Ok(Delivery::None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(has_target: bool, fg: bool, cur: bool) -> Gate {
        Gate::new(has_target, fg, cur)
    }

    fn code(r: Result<Delivery, GateError>) -> Option<&'static str> {
        r.err().map(|e| e.code)
    }

    #[test]
    fn default_gate_refuses_every_injecting_verb() {
        let gate = g(false, false, false);
        // No target, no flags: the whole foreground family is refused, typed.
        assert_eq!(
            code(gate.decide("move", true, false)),
            Some(CURSOR_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("move", false, false)),
            Some(CURSOR_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("click", true, false)),
            Some(CURSOR_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("click", false, false)),
            Some(FOREGROUND_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("down", false, false)),
            Some(FOREGROUND_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("up", false, false)),
            Some(FOREGROUND_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("drag", false, false)),
            Some(CURSOR_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("scroll", true, false)),
            Some(CURSOR_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("scroll", false, false)),
            Some(FOREGROUND_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("type", false, false)),
            Some(FOREGROUND_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("key", false, false)),
            Some(FOREGROUND_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("hotkey", false, false)),
            Some(FOREGROUND_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("focus_window", false, false)),
            Some(FOREGROUND_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("invoke", false, true)),
            Some(TARGET_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("set_value", false, true)),
            Some(TARGET_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("click", false, true)),
            Some(TARGET_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("scroll", false, true)),
            Some(TARGET_REQUIRED)
        );
        // Read-only verbs never need a permit.
        assert_eq!(gate.decide("wait", false, false), Ok(Delivery::None));
        assert_eq!(gate.decide("wait_for", false, false), Ok(Delivery::None));
        assert_eq!(
            gate.decide("clipboard_get", false, false),
            Ok(Delivery::None)
        );
        // Writing the owner's clipboard is a side effect outside the target
        // window: refused by default, like the foreground family.
        assert_eq!(
            code(gate.decide("clipboard_set", false, false)),
            Some(CLIPBOARD_REQUIRED)
        );
    }

    #[test]
    fn clipboard_write_needs_the_explicit_flag() {
        let e = g(false, false, false)
            .decide("clipboard_set", false, false)
            .unwrap_err();
        assert_eq!(e.code, CLIPBOARD_REQUIRED);
        assert!(
            e.text().contains("allow_clipboard:true"),
            "refusal must name the exact flag: {}",
            e.text()
        );
        assert!(
            e.text().starts_with(CLIPBOARD_REQUIRED) && e.text().contains("Nothing was written."),
            "refusal must be typed and state nothing changed: {}",
            e.text()
        );
        // Neither of the other flags buys clipboard access.
        for gate in [g(false, true, true), g(true, true, true)] {
            assert_eq!(
                code(gate.decide("clipboard_set", false, false)),
                Some(CLIPBOARD_REQUIRED),
                "allow_foreground/allow_cursor must not authorise a clipboard write"
            );
        }
        // The flag opens exactly the clipboard write, nothing else.
        let allow = g(false, false, false).allow_clipboard(true);
        assert_eq!(
            allow.decide("clipboard_set", false, false),
            Ok(Delivery::None)
        );
        assert_eq!(
            allow.decide("clipboard_get", false, false),
            Ok(Delivery::None)
        );
        assert_eq!(
            code(allow.decide("type", false, false)),
            Some(FOREGROUND_REQUIRED)
        );
        assert_eq!(
            code(allow.decide("move", true, false)),
            Some(CURSOR_REQUIRED)
        );
    }

    #[test]
    fn foreground_flag_mints_only_the_foreground_path() {
        let gate = g(false, true, false);
        assert_eq!(gate.decide("type", false, false), Ok(Delivery::Input));
        assert_eq!(gate.decide("key", false, false), Ok(Delivery::Input));
        assert_eq!(gate.decide("hotkey", false, false), Ok(Delivery::Input));
        assert_eq!(gate.decide("down", false, false), Ok(Delivery::Input));
        assert_eq!(gate.decide("click", false, false), Ok(Delivery::Input));
        assert_eq!(
            gate.decide("focus_window", false, false),
            Ok(Delivery::Input)
        );
        // ...but it never authorises a pointer move.
        assert_eq!(
            code(gate.decide("move", true, false)),
            Some(CURSOR_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("click", true, false)),
            Some(CURSOR_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("drag", false, false)),
            Some(CURSOR_REQUIRED)
        );
        assert_eq!(
            code(gate.decide("scroll", true, false)),
            Some(CURSOR_REQUIRED)
        );
    }

    #[test]
    fn a_pointer_act_needs_both_flags() {
        // allow_cursor alone is NOT enough: SendInput is foreground input,
        // so the pointer path demands the foreground flag too.
        let cursor_only = g(false, false, true);
        for (verb, coords) in [
            ("move", true),
            ("click", true),
            ("drag", false),
            ("scroll", true),
        ] {
            assert_eq!(
                code(cursor_only.decide(verb, coords, false)),
                Some(FOREGROUND_REQUIRED),
                "{verb} with allow_cursor alone must still be refused"
            );
        }
        // ...and the keyboard path never opens from the cursor flag.
        assert_eq!(
            code(cursor_only.decide("type", false, false)),
            Some(FOREGROUND_REQUIRED)
        );
        // Both flags: the pointer path opens.
        let both = g(false, true, true);
        assert_eq!(both.decide("move", true, false), Ok(Delivery::Input));
        assert_eq!(both.decide("click", true, false), Ok(Delivery::Input));
        assert_eq!(both.decide("drag", false, false), Ok(Delivery::Input));
        assert_eq!(both.decide("scroll", true, false), Ok(Delivery::Input));
    }

    #[test]
    fn a_target_switches_to_uia_and_posted_delivery_without_flags() {
        let gate = g(true, false, false);
        assert_eq!(gate.decide("click", true, false), Ok(Delivery::Posted));
        assert_eq!(gate.decide("click", false, true), Ok(Delivery::Uia));
        assert_eq!(gate.decide("down", true, false), Ok(Delivery::Posted));
        assert_eq!(gate.decide("up", true, false), Ok(Delivery::Posted));
        // A posted button message needs an exact point.
        let e = gate.decide("down", false, false).unwrap_err();
        assert_eq!(e.code, BACKGROUND_UNAVAILABLE);
        assert_eq!(gate.decide("drag", false, false), Ok(Delivery::Posted));
        assert_eq!(gate.decide("scroll", true, false), Ok(Delivery::Posted));
        assert_eq!(gate.decide("scroll", false, true), Ok(Delivery::Uia));
        // Keyboard input is NEVER posted to the owner's desktop, even with a
        // target: a posted WM_CHAR/WM_KEYDOWN is not reliable. `type` on an
        // element is UIA SetValue, but that provider may focus the control,
        // so it is an explicit opt-in too.
        assert_eq!(
            code(gate.decide("type", false, true)),
            Some(crate::gate::FOREGROUND_REQUIRED)
        );
        for verb in ["type", "key", "hotkey"] {
            let e = gate.decide(verb, false, false).unwrap_err();
            assert_eq!(e.code, FOREGROUND_REQUIRED, "{verb}");
            assert!(
                e.text().contains("not delivered reliably"),
                "{verb} must name the reason: {}",
                e.text()
            );
        }
        // The private-desktop (hidden) mode is the one exception: the server
        // owns that desktop and posting is how it drives the launched app.
        let private = g(true, false, false).private_desktop(true);
        assert_eq!(private.decide("type", false, false), Ok(Delivery::Posted));
        assert_eq!(private.decide("key", false, false), Ok(Delivery::Posted));
        // UIA invoke/toggle/expand/scroll stay background-safe; UIA text
        // entry does not (the provider may focus the control).
        assert_eq!(gate.decide("invoke", false, true), Ok(Delivery::Uia));
        assert_eq!(
            code(gate.decide("set_value", false, true)),
            Some(crate::gate::FOREGROUND_REQUIRED)
        );
        let with_flag = g(true, true, false);
        assert_eq!(
            with_flag.decide("set_value", false, true),
            Ok(Delivery::Uia)
        );
        assert_eq!(with_flag.decide("type", false, true), Ok(Delivery::Uia));
        // Verbs with no message equivalent name the reason instead of
        // silently escalating to the pointer.
        let e = gate.decide("move", true, false).unwrap_err();
        assert_eq!(e.code, BACKGROUND_UNAVAILABLE);
        assert!(e.text().contains("message stream"), "{}", e.text());
        let e = gate.decide("click", false, false).unwrap_err();
        assert_eq!(e.code, BACKGROUND_UNAVAILABLE);
        assert!(e.text().contains("needs exact client x/y"), "{}", e.text());
        let e = gate.decide("scroll", false, false).unwrap_err();
        assert_eq!(e.code, BACKGROUND_UNAVAILABLE);
    }

    #[test]
    fn permits_only_exist_when_the_flag_is_true() {
        assert!(ForegroundPermit::grant(false).is_none());
        assert!(ForegroundPermit::grant(true).is_some());
        assert!(CursorPermit::grant(false).is_none());
        assert!(CursorPermit::grant(true).is_some());
        assert!(ClipboardPermit::grant(false).is_none());
        assert!(ClipboardPermit::grant(true).is_some());
        // No unflagged constructor exists: the ONLY mints are the three flag
        // checks above (also pinned by the source scan in tests/no_steal.rs).
        let gate = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/gate.rs"))
            .expect("gate.rs readable");
        // Count only the production half: this test's own literals live
        // below the `#[cfg(test)]` marker.
        let prod = gate.split("#[cfg(test)]").next().unwrap_or("");
        assert_eq!(
            prod.matches("Some(ForegroundPermit(())").count(),
            1,
            "ForegroundPermit has exactly one, flag-only constructor"
        );
        assert_eq!(
            prod.matches("Some(CursorPermit(())").count(),
            1,
            "CursorPermit has exactly one, flag-only constructor"
        );
        assert_eq!(
            prod.matches("Some(ClipboardPermit(())").count(),
            1,
            "ClipboardPermit has exactly one, flag-only constructor"
        );
    }

    #[test]
    fn refusals_name_the_recovery_in_every_code() {
        for (gate, verb) in [
            (g(false, false, false), "type"),
            (g(false, false, false), "move"),
            (g(true, false, false), "move"),
            (g(false, false, false), "invoke"),
        ] {
            let e = gate.decide(verb, true, false).unwrap_err();
            let t = e.text();
            assert!(
                t.starts_with(e.code),
                "refusal text must start with its code: {t}"
            );
            assert!(
                t.contains("Nothing was sent."),
                "refusal must state no input: {t}"
            );
        }
    }
}
