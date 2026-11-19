
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

pub(crate) const UIA_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);
const MIN_TIMEOUT: Duration = Duration::from_millis(50);
const MAX_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Family {
    Uia,
    Capture,
}

impl Family {
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

fn parse_limit(raw: Option<&str>, default: Duration) -> Duration {
    match raw.map(str::trim).and_then(|s| s.parse::<u64>().ok()) {
        Some(ms) => Duration::from_millis(ms).clamp(MIN_TIMEOUT, MAX_TIMEOUT),
        None => default,
    }
}

pub(crate) fn limit(f: Family) -> Duration {
    let raw = std::env::var(f.limit_var()).ok();
    parse_limit(raw.as_deref(), f.default_limit())
}

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

pub(crate) fn bound<T, F>(f: Family, op: &str, work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    bound_with(f, op, limit(f), stall_for(f), work)
}

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
            drop(handle);
            Err(timeout_text(f, op, limit))
        }
        Err(RecvTimeoutError::Disconnected) => {
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
