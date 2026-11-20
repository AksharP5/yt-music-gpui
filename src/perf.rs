//! Startup timestamps for `bin/check-performance`, built only with `--cfg ytm_perf`.
use std::time::{SystemTime, UNIX_EPOCH};

/// Print a wall-clock mark, so the harness can measure from the moment it started the app.
pub fn mark(name: &str) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    eprintln!("ytm_perf {name} {}", now.as_micros());
}
