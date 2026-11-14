//! Wall-clock bound for one blocking COM / PrintWindow call.
//!
//! Why this exists: `uia_element_list`, `uia_activate`, `uia_set_value` and
//! `shot::capture_window` make cross-process COM / `PrintWindow` calls that do
//! not return until the target's UI thread answers. A provider that never
//! answers blocks its worker forever; four of those make every later
//! `tools/call` wait indefinitely while `ping`, `initialize` and `tools/list`
//! still answer. `burst` already bounds its `SendMessageW` with
//! `SMTO_ABORTIFHUNG` ("never wedge a worker on a hung window"); this module
//! is the same discipline for the COM and GDI paths.
//!
//! ## Apartment model (why a helper thread is sound here)
//!
//! `uia::ensure_com` initialises the calling thread `COINIT_MULTITHREADED`
//! and every UIA entry point creates its own `IUIAutomation` through
//! `uia::automation()` — no interface is cached across calls, so no UIA
//! object outlives the call that made it. `CUIAutomation` is registered
//! `ThreadingModel=Both`, so in an MTA thread it is created directly: no
//! proxy, no message pump, no marshal. A helper thread that calls
//! `CoInitializeEx(NULL, COINIT_MULTITHREADED)` itself and creates, uses and
//! releases every interface inside one closure is therefore in exactly the
//! apartment model the worker had, and nothing has to be marshalled.
//!
//! Only plain data crosses the channel back (`serde_json` values, `bool`,
//! `String`, an owned pixel buffer): no COM pointer is ever sent, received or
//! dropped on a thread that did not create it.
//!
//! On timeout the helper is abandoned, never joined: it stays parked inside
//! the blocking call and unwinds its own COM objects on the thread that made
//! them whenever (if ever) the call returns. Abandoning means *detaching*, so
//! the timeout arm lets the thread handle fall away rather than setting it
//! aside: while a helper is still parked it costs its thread stack and one
//! kernel handle, and the OS releases both when it returns. Nothing outlives
//! it. Keeping the handle instead would pin one kernel thread object (and its
//! packet allocation) per timed-out call for the life of the process.
//! Abandoning a mutation also means its effect is unverified, which the message
//! says.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

/// A read-only UIA walk of one window subtree; normally milliseconds.
pub(crate) const UIA_TIMEOUT: Duration = Duration::from_secs(5);
/// One PrintWindow frame plus its geometry reads; normally tens of ms.
pub(crate) const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);
/// A configured deadline can never be shorter than this (a 0 would make every
/// call fail instantly) nor unbounded (which is the defect this module exists
/// to close).
const MIN_TIMEOUT: Duration = Duration::from_millis(50);
const MAX_TIMEOUT: Duration = Duration::from_secs(120);

/// Which family of operation is bounded: picks the typed code, the default
/// deadline, the override env var and the test-stall env var.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Family {
    Uia,
    Capture,
}

impl Family {
    /// Typed error code carried in the `error:` line.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Family::Uia => "uia_timeout",
            Family::Capture => "capture_timeout",
        }
    }

    fn default_limit(self) -> Duration {
        match self {
            Family::Uia => UIA_TIMEOUT,
            Family::Capture => CAPTURE_TIMEOUT,
        }
    }

    fn limit_var(self) -> &'static str {
        match self {
            Family::Uia => "PUPPET_UIA_TIMEOUT_MS",
            Family::Capture => "PUPPET_CAPTURE_TIMEOUT_MS",
        }
    }

    /// Test-only stall hook, consumed once per process so the call after a
    /// timeout runs at full speed and proves the worker is still usable.
    fn stall_var(self) -> &'static str {
        match self {
            Family::Uia => "PUPPET_TEST_STALL_UIA_MS",
            Family::Capture => "PUPPET_TEST_STALL_CAPTURE_MS",
        }
    }

    fn stall_armed(self) -> &'static AtomicBool {
        static UIA: AtomicBool = AtomicBool::new(true);
        static CAPTURE: AtomicBool = AtomicBool::new(true);
        match self {
            Family::Uia => &UIA,
            Family::Capture => &CAPTURE,
        }
    }
}

/// The timeout line: typed code, the operation and its deadline, the cause and
/// the recovery. Same `error: <code>: <message>` shape every other tool error
/// uses, so `isError` and the `ok:false` envelope rule keep working unchanged.
pub(crate) fn timeout_text(f: Family, op: &str, limit: Duration) -> String {
    format!(
        "error: {}: {} did not return within {}ms: the target's UI thread is not \
         responding; retry, or capture the desktop instead. The abandoned call may \
         still complete, so its effect is unverified.",
        f.code(),
        op,
        limit.as_millis()
    )
}

/// Clamp a configured deadline into `[MIN_TIMEOUT, MAX_TIMEOUT]`; absent or
/// unparsable falls back to the family default (never to "unbounded").
fn parse_limit(raw: Option<&str>, default: Duration) -> Duration {
    match raw.map(str::trim).and_then(|s| s.parse::<u64>().ok()) {
        Some(ms) => Duration::from_millis(ms).clamp(MIN_TIMEOUT, MAX_TIMEOUT),
        None => default,
    }
}

/// The deadline in force for a family (env-overridable, clamped). Exposed so
/// a path that cannot use `bound` (a call that must stay on its own thread,
/// like the hidden-desktop capture) still takes its deadline from the same
/// family instead of a bespoke constant.
pub(crate) fn limit(f: Family) -> Duration {
    let raw = std::env::var(f.limit_var()).ok();
    parse_limit(raw.as_deref(), f.default_limit())
}

/// `Some(ms)` only when the test hook is set AND still armed for this process.
fn stall_for(f: Family) -> Option<u64> {
    let ms = std::env::var(f.stall_var())
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    if f.stall_armed().swap(false, Ordering::SeqCst) {
        Some(ms)
    } else {
        None
    }
}

/// Run `work` on a helper thread and wait at most `f`'s deadline for it.
///
/// `Ok` carries the closure's own result unchanged. `Err` is the typed timeout
/// line, or an `internal` line when the helper could not be started or died
/// without a result. The caller decides how its own error shape wraps it.
pub(crate) fn bound<T, F>(f: Family, op: &str, work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    bound_with(f, op, limit(f), stall_for(f), work)
}

/// The mechanism with the deadline and the test stall passed in, so a unit
/// test can exercise the real spawn/wait/abandon path without touching process
/// env (which other in-process tests share).
fn bound_with<T, F>(
    f: Family,
    op: &str,
    limit: Duration,
    stall: Option<u64>,
    work: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<T>();
    let handle = match std::thread::Builder::new()
        .name(format!("puppet-{op}"))
        .spawn(move || {
            // Same apartment as the worker had (see the module note): MTA, so
            // every UIA object created below is free-threaded and released
            // here, on this thread.
            crate::uia::ensure_com();
            if let Some(ms) = stall {
                std::thread::sleep(Duration::from_millis(ms));
            }
            let _ = tx.send(work());
        }) {
        Ok(h) => h,
        Err(e) => {
            return Err(format!(
                "error: internal: could not start the {op} helper thread: {e}"
            ))
        }
    };
    match rx.recv_timeout(limit) {
        Ok(v) => {
            let _ = handle.join();
            Ok(v)
        }
        Err(RecvTimeoutError::Timeout) => {
            // Deliberately not joined: joining is the wedge. Dropping the
            // handle is what detaches (it never blocks); the thread owns every
            // COM object it created and releases them itself, and the OS frees
            // its stack and kernel handle when it returns.
            drop(handle);
            Err(timeout_text(f, op, limit))
        }
        Err(RecvTimeoutError::Disconnected) => {
            // The helper panicked or aborted before sending: reap it so the
            // panic is not lost, then report it as an internal failure.
            let _ = handle.join();
            Err(format!(
                "error: internal: the {op} helper thread ended without a result"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_the_documented_vocabulary() {
        assert_eq!(Family::Uia.code(), "uia_timeout");
        assert_eq!(Family::Capture.code(), "capture_timeout");
    }

    #[test]
    fn deadline_defaults_when_unset_or_garbage() {
        assert_eq!(parse_limit(None, UIA_TIMEOUT), UIA_TIMEOUT);
        assert_eq!(parse_limit(None, CAPTURE_TIMEOUT), CAPTURE_TIMEOUT);
        assert_eq!(parse_limit(Some("not-a-number"), UIA_TIMEOUT), UIA_TIMEOUT);
        assert_eq!(parse_limit(Some(""), UIA_TIMEOUT), UIA_TIMEOUT);
    }

    #[test]
    fn deadline_override_is_clamped_not_unbounded() {
        assert_eq!(
            parse_limit(Some("250"), UIA_TIMEOUT),
            Duration::from_millis(250)
        );
        assert_eq!(
            parse_limit(Some(" 250 "), UIA_TIMEOUT),
            Duration::from_millis(250)
        );
        assert_eq!(parse_limit(Some("0"), UIA_TIMEOUT), MIN_TIMEOUT);
        assert_eq!(parse_limit(Some("999999999"), CAPTURE_TIMEOUT), MAX_TIMEOUT);
    }

    #[test]
    fn timeout_text_names_code_deadline_cause_and_recovery() {
        let t = timeout_text(Family::Capture, "capture_window", Duration::from_secs(10));
        assert!(
            t.starts_with("error: capture_timeout: capture_window did not return within 10000ms"),
            "{t}"
        );
        assert!(
            t.contains("the target's UI thread is not responding"),
            "{t}"
        );
        assert!(t.contains("retry, or capture the desktop instead"), "{t}");
        assert!(t.contains("unverified"), "{t}");
    }

    /// The mechanism itself: work that overruns the deadline yields the typed
    /// line, the timeout is honoured, and the caller keeps running.
    #[test]
    fn bound_returns_the_typed_line_when_the_work_overruns() {
        let started = std::time::Instant::now();
        let err = bound_with(
            Family::Uia,
            "uia_activate",
            Duration::from_millis(150),
            None,
            || {
                std::thread::sleep(Duration::from_secs(2));
                7u8
            },
        )
        .expect_err("an overrunning helper must not yield a value");
        assert!(err.starts_with("error: uia_timeout: uia_activate"), "{err}");
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "the deadline must fire, not the sleep: {:?}",
            started.elapsed()
        );
    }

    /// Under the deadline the closure's own value comes back unchanged, and the
    /// helper is reaped rather than leaked.
    #[test]
    fn bound_returns_the_value_when_the_work_finishes() {
        let got = bound_with(
            Family::Capture,
            "capture_window",
            Duration::from_secs(5),
            None,
            || 41u8 + 1,
        )
        .expect("work under the deadline returns its value");
        assert_eq!(got, 42);
    }

    /// A panicking helper is reported, never a hang or a silent success.
    #[test]
    fn bound_reports_a_helper_that_dies_without_a_result() {
        let err = bound_with(
            Family::Uia,
            "uia_element_list",
            Duration::from_secs(5),
            None,
            || panic!("helper dies"),
        )
        .expect_err("a dead helper cannot produce a result");
        assert!(
            err.starts_with(
                "error: internal: the uia_element_list helper thread ended without a result"
            ),
            "{err}"
        );
    }
}
