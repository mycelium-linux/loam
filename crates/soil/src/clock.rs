//! The one clock the host reads.
//!
//! A bus frame, an HTTP context, a credential expiry and an index entry all
//! mean the same thing by "now". One conversion means a module cannot quietly
//! pick a different epoch or resolution, and a test that stubs the clock has
//! one place to do it.

use std::time::{SystemTime, UNIX_EPOCH};

/// Unix milliseconds.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
