use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DOWNLOAD_ID: AtomicU64 = AtomicU64::new(1);

/// Generate a monotonically increasing download ID.
pub(crate) fn next_download_id() -> u64 {
    NEXT_DOWNLOAD_ID.fetch_add(1, Ordering::Relaxed)
}

// Note: Under tarpaulin coverage runs, log macros expand to a simple level
// check without the tracing call.  This avoids phantom "uncovered" lines
// from tracing's internal macro expansion that tarpaulin's LLVM engine
// cannot instrument.

/// Emit an `error!` event only if the task-level `log_level` permits it.
macro_rules! log_error {
    ($log_level:expr, $($arg:tt)*) => {
        if $log_level.enabled(tracing::Level::ERROR) {
            #[cfg(not(tarpaulin))]
            { tracing::error!($($arg)*); }
        }
    };
}

/// Emit a `warn!` event only if the task-level `log_level` permits it.
macro_rules! log_warn {
    ($log_level:expr, $($arg:tt)*) => {
        if $log_level.enabled(tracing::Level::WARN) {
            #[cfg(not(tarpaulin))]
            { tracing::warn!($($arg)*); }
        }
    };
}

/// Emit an `info!` event only if the task-level `log_level` permits it.
macro_rules! log_info {
    ($log_level:expr, $($arg:tt)*) => {
        if $log_level.enabled(tracing::Level::INFO) {
            #[cfg(not(tarpaulin))]
            { tracing::info!($($arg)*); }
        }
    };
}

/// Emit a `debug!` event only if the task-level `log_level` permits it.
macro_rules! log_debug {
    ($log_level:expr, $($arg:tt)*) => {
        if $log_level.enabled(tracing::Level::DEBUG) {
            #[cfg(not(tarpaulin))]
            { tracing::debug!($($arg)*); }
        }
    };
}

/// Emit a `trace!` event only if the task-level `log_level` permits it.
macro_rules! log_trace {
    ($log_level:expr, $($arg:tt)*) => {
        if $log_level.enabled(tracing::Level::TRACE) {
            #[cfg(not(tarpaulin))]
            { tracing::trace!($($arg)*); }
        }
    };
}

pub(crate) use log_debug;
pub(crate) use log_error;
pub(crate) use log_info;
pub(crate) use log_trace;
pub(crate) use log_warn;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LogLevel;

    #[test]
    fn test_download_id_increments() {
        let id1 = next_download_id();
        let id2 = next_download_id();
        assert!(id2 > id1);
    }

    #[test]
    fn test_log_macros_compile_with_fields() {
        let level = LogLevel::Debug;
        log_error!(level, download_id = 1u64, "test error");
        log_warn!(level, download_id = 1u64, "test warn");
        log_info!(level, download_id = 1u64, "test info");
        log_debug!(level, download_id = 1u64, "test debug");
        // Trace should not fire at Debug level
        log_trace!(level, download_id = 1u64, "test trace");
    }

    #[test]
    fn test_log_macros_off_level() {
        // With Off, nothing should fire (just ensure no panics)
        let level = LogLevel::Off;
        log_error!(level, "should not appear");
        log_warn!(level, "should not appear");
        log_info!(level, "should not appear");
        log_debug!(level, "should not appear");
        log_trace!(level, "should not appear");
    }
}
