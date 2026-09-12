//! Runtime feature report for the vendored libcurl build.
//!
//! The migration plan requires the runtime to record which libcurl, TLS and
//! resolver features are actually linked, rather than inferring them from
//! Cargo features or build scripts.

use crate::config::LogLevel;

/// Facts about the linked libcurl that change transport behaviour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeFeatures {
    pub version: String,
    pub version_num: u32,
    pub vendored: bool,
    pub host: String,
    pub ssl_version: Option<String>,
    pub libz_version: Option<String>,
    pub http2: bool,
    pub http3: bool,
    pub https_proxy: bool,
    pub ipv6: bool,
    pub async_dns: bool,
    pub unix_socket: bool,
}

impl RuntimeFeatures {
    /// Reads the linked library through `curl_version_info`.
    pub(crate) fn detect() -> Self {
        let version = curl::Version::get();
        Self {
            version: version.version().to_string(),
            version_num: version.version_num(),
            vendored: version.vendored(),
            host: version.host().to_string(),
            ssl_version: version.ssl_version().map(str::to_string),
            libz_version: version.libz_version().map(str::to_string),
            http2: version.feature_http2(),
            http3: version.feature_http3(),
            https_proxy: version.feature_https_proxy(),
            ipv6: version.feature_ipv6(),
            async_dns: version.feature_async_dns(),
            unix_socket: version.feature_unix_domain_socket(),
        }
    }

    /// One-line summary used in logs and diagnostics.
    pub(crate) fn summary(&self) -> String {
        format!(
            "libcurl {} (vendored={}) host={} tls={} zlib={} http2={} http3={} https_proxy={} ipv6={} async_dns={}",
            self.version,
            self.vendored,
            self.host,
            self.ssl_version.as_deref().unwrap_or("none"),
            self.libz_version.as_deref().unwrap_or("none"),
            self.http2,
            self.http3,
            self.https_proxy,
            self.ipv6,
            self.async_dns,
        )
    }
}

/// Emits the detected libcurl features at debug level.
///
/// The Hyper backend logs the equivalent connector facts when it builds a
/// client, so this keeps both backends observable from the same place.
pub(crate) fn log_runtime_features(log_level: LogLevel) {
    let features = RuntimeFeatures::detect();
    // Coverage builds expand `log_debug!` to a level check only, so keep the
    // detected value referenced outside the macro as well.
    let _ = &features;
    log_debug!(
        log_level,
        libcurl_version = %features.version,
        libcurl_vendored = features.vendored,
        libcurl_host = %features.host,
        tls_backend = features.ssl_version.as_deref().unwrap_or("none"),
        zlib_version = features.libz_version.as_deref().unwrap_or("none"),
        http2 = features.http2,
        http3 = features.http3,
        https_proxy = features.https_proxy,
        ipv6 = features.ipv6,
        async_dns = features.async_dns,
        summary = %features.summary(),
        "libcurl runtime features"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_reports_linked_library() {
        let features = RuntimeFeatures::detect();

        assert!(!features.version.is_empty());
        // The workspace pins `curl-sys` with `static-curl`, so the vendored
        // build is the expected default and must be reported as such.
        assert!(features.vendored, "expected the vendored libcurl build");
        assert!(features.version_num >= 0x08_00_00);
        assert!(!features.host.is_empty());
        let tls = features
            .ssl_version
            .as_deref()
            .expect("vendored libcurl must link a TLS backend");
        assert!(!tls.is_empty());
        assert!(!features.http2, "P1 keeps HTTP/1.1 only");
    }

    #[test]
    fn test_summary_contains_runtime_facts() {
        let features = RuntimeFeatures::detect();
        let summary = features.summary();
        // Visible with `cargo test -- --nocapture`; recorded in
        // docs/libcurl-pool-semantics.zh-CN.md.
        println!("{summary}");

        assert!(summary.contains(&features.version));
        assert!(summary.contains(&features.host));
        assert!(summary.contains("tls="));
    }

    #[test]
    fn test_log_runtime_features_is_silent_when_logging_is_off() {
        // Must not panic and must not require a subscriber.
        log_runtime_features(LogLevel::Off);
    }
}
