//! libcurl transport backend (P2 driver, P3 compatibility work).
//!
//! See `docs/libcurl-migration-plan.zh-CN.md`. `driver` is the production
//! transfer thread; the runtime feature report feeds diagnostics, and the pool
//! measurements are test-gated evidence for P3.

pub(crate) mod driver;
pub(crate) mod ip_policy;
#[cfg(test)]
mod pool_semantics;
pub(crate) mod runtime;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod transport;

pub(crate) use runtime::log_runtime_features;
pub(crate) use transport::CurlTransport;
