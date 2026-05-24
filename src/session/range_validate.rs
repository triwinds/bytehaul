use crate::error::DownloadError;
use crate::http::response::ResponseMeta;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RangeValidationMode {
    FreshProbe,
    ResumeProbe,
    Segment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ExpectedRange {
    pub start: u64,
    pub end_inclusive: u64,
    pub total_size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RangeValidationDecision {
    Accept,
    FallbackToSingle(FreshRangeFallbackReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FreshRangeFallbackReason {
    RangeNotSupported,
    EncodedResponse,
}

pub(super) fn validate_range_response(
    status: u16,
    retry_after_header: Option<&str>,
    meta: &ResponseMeta,
    mode: RangeValidationMode,
    expected: ExpectedRange,
) -> Result<RangeValidationDecision, DownloadError> {
    let status_decision = validate_range_status(status, retry_after_header, mode)?;
    if !matches!(status_decision, RangeValidationDecision::Accept) {
        return Ok(status_decision);
    }

    if !range_response_allowed(meta) {
        return match mode {
            RangeValidationMode::FreshProbe => Ok(RangeValidationDecision::FallbackToSingle(
                FreshRangeFallbackReason::EncodedResponse,
            )),
            RangeValidationMode::ResumeProbe | RangeValidationMode::Segment => Err(
                DownloadError::ResumeMismatch(
                    "range responses with content-encoding are not supported".into(),
                ),
            ),
        };
    }

    let response_total = resolve_response_total(meta, mode, expected.total_size)?;
    let expected_end = resolve_expected_end(expected, response_total, mode)
        .ok_or_else(mismatched_content_range_error)?;

    if meta.content_range_start != Some(expected.start)
        || meta.content_range_end != Some(expected_end)
    {
        return Err(mismatched_content_range_error());
    }

    let expected_len = expected_end
        .checked_sub(expected.start)
        .and_then(|len| len.checked_add(1))
        .ok_or_else(mismatched_content_range_error)?;
    if meta.content_length.is_some_and(|length| length != expected_len) {
        return match mode {
            RangeValidationMode::Segment => Err(DownloadError::Transport(
                crate::error::TransportError::new(
                    crate::error::TransportErrorKind::Body,
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "server returned a mismatched Content-Length for range response",
                    ),
                ),
            )),
            RangeValidationMode::FreshProbe | RangeValidationMode::ResumeProbe => Err(
                DownloadError::ResumeMismatch(
                    "server returned a mismatched Content-Length for range response".into(),
                ),
            ),
        };
    }

    Ok(RangeValidationDecision::Accept)
}

pub(super) fn range_response_allowed(meta: &ResponseMeta) -> bool {
    !matches!(
        meta.content_encoding.as_deref(),
        Some(encoding) if !encoding.eq_ignore_ascii_case("identity")
    )
}

fn validate_range_status(
    status: u16,
    retry_after_header: Option<&str>,
    mode: RangeValidationMode,
) -> Result<RangeValidationDecision, DownloadError> {
    if status == 200 {
        return match mode {
            RangeValidationMode::FreshProbe => Ok(RangeValidationDecision::FallbackToSingle(
                FreshRangeFallbackReason::RangeNotSupported,
            )),
            RangeValidationMode::ResumeProbe | RangeValidationMode::Segment => Err(
                DownloadError::HttpStatus {
                    status: 200,
                    message: "server returned 200 instead of 206; Range not supported".into(),
                },
            ),
        };
    }

    if status == 429 || status == 503 {
        let retry_after = retry_after_header.and_then(|value| value.trim().parse::<u64>().ok());
        let message = match retry_after {
            Some(secs) => format!("retry-after:{secs}"),
            None => format!("HTTP {status}"),
        };
        return Err(DownloadError::HttpStatus { status, message });
    }

    if status != 206 {
        let message = match mode {
            RangeValidationMode::FreshProbe => format!("expected 206 or 200, got {status}"),
            RangeValidationMode::ResumeProbe | RangeValidationMode::Segment => {
                format!("expected 206, got {status}")
            }
        };
        return Err(DownloadError::HttpStatus { status, message });
    }

    Ok(RangeValidationDecision::Accept)
}

fn resolve_response_total(
    meta: &ResponseMeta,
    mode: RangeValidationMode,
    expected_total_size: Option<u64>,
) -> Result<u64, DownloadError> {
    match mode {
        RangeValidationMode::FreshProbe => meta.content_range_total.ok_or_else(|| {
            DownloadError::ResumeMismatch(
                "range probe response missing Content-Range total".into(),
            )
        }),
        RangeValidationMode::ResumeProbe | RangeValidationMode::Segment => {
            let Some(expected_total_size) = expected_total_size else {
                return Err(mismatched_content_range_error());
            };
            match meta.content_range_total {
                Some(actual_total_size) if actual_total_size == expected_total_size => {
                    Ok(actual_total_size)
                }
                _ => Err(mismatched_content_range_error()),
            }
        }
    }
}

fn resolve_expected_end(
    expected: ExpectedRange,
    response_total: u64,
    mode: RangeValidationMode,
) -> Option<u64> {
    match mode {
        RangeValidationMode::FreshProbe => {
            let response_last_byte = response_total.checked_sub(1)?;
            if response_last_byte < expected.start {
                return None;
            }
            Some(expected.end_inclusive.min(response_last_byte))
        }
        RangeValidationMode::ResumeProbe | RangeValidationMode::Segment => {
            Some(expected.end_inclusive)
        }
    }
}

fn mismatched_content_range_error() -> DownloadError {
    DownloadError::ResumeMismatch("server returned a mismatched Content-Range".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_meta() -> ResponseMeta {
        ResponseMeta {
            content_length: Some(1000),
            content_range_start: Some(0),
            content_range_end: Some(999),
            content_range_total: Some(5000),
            accept_ranges: true,
            etag: None,
            last_modified: None,
            content_disposition: None,
            content_encoding: None,
        }
    }

    #[test]
    fn test_range_response_allowed_without_encoding() {
        let mut meta = test_meta();
        meta.content_encoding = None;
        assert!(range_response_allowed(&meta));
    }

    #[test]
    fn test_range_response_allowed_identity_encoding() {
        let mut meta = test_meta();
        meta.content_encoding = Some("identity".into());
        assert!(range_response_allowed(&meta));
    }

    #[test]
    fn test_range_response_disallowed_gzip_encoding() {
        let mut meta = test_meta();
        meta.content_encoding = Some("gzip".into());
        assert!(!range_response_allowed(&meta));
    }

    #[test]
    fn test_validate_range_response_accepts_segment_match() {
        let meta = test_meta();
        let decision = validate_range_response(
            206,
            None,
            &meta,
            RangeValidationMode::Segment,
            ExpectedRange {
                start: 0,
                end_inclusive: 999,
                total_size: Some(5000),
            },
        )
        .unwrap();

        assert_eq!(decision, RangeValidationDecision::Accept);
    }

    #[test]
    fn test_validate_range_response_fresh_200_falls_back_to_single() {
        let decision = validate_range_response(
            200,
            None,
            &test_meta(),
            RangeValidationMode::FreshProbe,
            ExpectedRange {
                start: 0,
                end_inclusive: 1023,
                total_size: None,
            },
        )
        .unwrap();

        assert_eq!(
            decision,
            RangeValidationDecision::FallbackToSingle(
                FreshRangeFallbackReason::RangeNotSupported,
            )
        );
    }

    #[test]
    fn test_validate_range_response_fresh_gzip_falls_back_to_single() {
        let mut meta = test_meta();
        meta.content_encoding = Some("gzip".into());

        let decision = validate_range_response(
            206,
            None,
            &meta,
            RangeValidationMode::FreshProbe,
            ExpectedRange {
                start: 0,
                end_inclusive: 999,
                total_size: None,
            },
        )
        .unwrap();

        assert_eq!(
            decision,
            RangeValidationDecision::FallbackToSingle(
                FreshRangeFallbackReason::EncodedResponse,
            )
        );
    }

    #[test]
    fn test_validate_range_response_fresh_requires_content_range_total() {
        let mut meta = test_meta();
        meta.content_range_total = None;

        let err = validate_range_response(
            206,
            None,
            &meta,
            RangeValidationMode::FreshProbe,
            ExpectedRange {
                start: 0,
                end_inclusive: 999,
                total_size: None,
            },
        )
        .unwrap_err();

        assert!(matches!(err, DownloadError::ResumeMismatch(_)));
    }

    #[test]
    fn test_validate_range_response_fresh_clamps_probe_end_to_total_size() {
        let meta = ResponseMeta {
            content_length: Some(500),
            content_range_start: Some(0),
            content_range_end: Some(499),
            content_range_total: Some(500),
            accept_ranges: true,
            etag: None,
            last_modified: None,
            content_disposition: None,
            content_encoding: None,
        };

        let decision = validate_range_response(
            206,
            None,
            &meta,
            RangeValidationMode::FreshProbe,
            ExpectedRange {
                start: 0,
                end_inclusive: 1023,
                total_size: None,
            },
        )
        .unwrap();

        assert_eq!(decision, RangeValidationDecision::Accept);
    }

    #[test]
    fn test_validate_range_response_resume_rejects_200() {
        let err = validate_range_response(
            200,
            None,
            &test_meta(),
            RangeValidationMode::ResumeProbe,
            ExpectedRange {
                start: 1000,
                end_inclusive: 1999,
                total_size: Some(5000),
            },
        )
        .unwrap_err();

        match err {
            DownloadError::HttpStatus { status, message } => {
                assert_eq!(status, 200);
                assert!(message.contains("Range not supported"));
            }
            _ => panic!("expected HttpStatus"),
        }
    }

    #[test]
    fn test_validate_range_response_rejects_content_length_mismatch() {
        let mut meta = test_meta();
        meta.content_length = Some(999);

        let err = validate_range_response(
            206,
            None,
            &meta,
            RangeValidationMode::ResumeProbe,
            ExpectedRange {
                start: 0,
                end_inclusive: 999,
                total_size: Some(5000),
            },
        )
        .unwrap_err();

        assert!(matches!(err, DownloadError::ResumeMismatch(message) if message.contains("Content-Length")));
    }

    #[test]
    fn test_validate_range_response_rejects_content_range_mismatch() {
        let mut meta = test_meta();
        meta.content_range_start = Some(1);

        let err = validate_range_response(
            206,
            None,
            &meta,
            RangeValidationMode::Segment,
            ExpectedRange {
                start: 0,
                end_inclusive: 999,
                total_size: Some(5000),
            },
        )
        .unwrap_err();

        assert!(matches!(err, DownloadError::ResumeMismatch(_)));
    }

    #[test]
    fn test_validate_range_response_treats_segment_content_length_mismatch_as_retryable() {
        let mut meta = test_meta();
        meta.content_length = Some(999);

        let err = validate_range_response(
            206,
            None,
            &meta,
            RangeValidationMode::Segment,
            ExpectedRange {
                start: 0,
                end_inclusive: 999,
                total_size: Some(5000),
            },
        )
        .unwrap_err();

        assert!(err.is_retryable());
        assert!(matches!(err, DownloadError::Transport(_)));
    }

    #[test]
    fn test_validate_range_response_preserves_retry_after_for_503() {
        let err = validate_range_response(
            503,
            Some("5"),
            &test_meta(),
            RangeValidationMode::Segment,
            ExpectedRange {
                start: 0,
                end_inclusive: 999,
                total_size: Some(5000),
            },
        )
        .unwrap_err();

        match err {
            DownloadError::HttpStatus { status, message } => {
                assert_eq!(status, 503);
                assert_eq!(message, "retry-after:5");
            }
            _ => panic!("expected HttpStatus"),
        }
    }
}