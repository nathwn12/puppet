//! Cancellation registry for in-flight `tools/call` requests (P1b).
//!
//! The stdio reader owns no blocking work: each `tools/call` is dispatched
//! to a worker with a canonical request-id key
//! (`serde_json::to_string(&id)`). A `notifications/cancelled` frame carrying
//! `params.requestId` flips the matching token; the worker drops the
//! response when the token is set. A cancel that arrives before the worker
//! starts, or while it waits on the input lock, also drops the call.
//!
//! The current token is thread-local so `burst` waits can poll
//! `cancelled()` without plumbing ids through every helper. Tokens are
//! removed on completion so a late cancel never kills a finished call.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

static REGISTRY: LazyLock<Mutex<HashMap<String, Arc<AtomicBool>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

thread_local! {
    static CURRENT: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// Poison-recovering registry lock: a panicked holder must never deny
/// future register/cancel/remove calls (the map itself stays valid; only
/// the panic payload is discarded).
fn reg() -> MutexGuard<'static, HashMap<String, Arc<AtomicBool>>> {
    REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Register (or replace) the token for a canonical request-id key.
pub fn register(id_key: &str) -> Arc<AtomicBool> {
    let token = Arc::new(AtomicBool::new(false));
    reg().insert(id_key.to_string(), token.clone());
    token
}

/// Remove the token once the call has completed (responded or dropped).
pub fn remove(id_key: &str) {
    reg().remove(id_key);
}

/// Flip the token for `id_key`, if it is still registered. Unknown or
/// already-completed ids are ignored and send no response.
pub fn cancel(id_key: &str) {
    if let Some(t) = reg().get(id_key) {
        t.store(true, Ordering::SeqCst);
    }
}

/// Fetch the live token for `id_key`, if still registered.
pub fn get(id_key: &str) -> Option<Arc<AtomicBool>> {
    reg().get(id_key).cloned()
}

/// Install the calling worker thread's current token.
pub fn set_current(token: Arc<AtomicBool>) {
    CURRENT.with(|c| *c.borrow_mut() = Some(token));
}

/// Clear the calling worker thread's current token.
pub fn clear_current() {
    CURRENT.with(|c| *c.borrow_mut() = None);
}

/// True when the calling worker thread's current call was cancelled.
pub fn cancelled() -> bool {
    CURRENT.with(|c| {
        c.borrow()
            .as_ref()
            .is_some_and(|t| t.load(Ordering::SeqCst))
    })
}
