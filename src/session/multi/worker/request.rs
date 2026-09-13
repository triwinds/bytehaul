//! Ordinary finite-range response validation and cross-piece body consumption.
//! This module has no recovery coordinator: the executor supplies request
//! geometry, writer resources and phase observations.
use super::*;

pub(super) struct RequestContext<'a> {
    pub(super) worker: &'a HttpWorker,
    pub(super) read_timeout: Duration,
    pub(super) segment: &'a Segment,
    pub(super) request_end: u64,
    pub(super) write_tx: &'a mpsc::Sender<WriterCommand>,
    pub(super) received: &'a Arc<AtomicU64>,
    pub(super) budget: &'a Arc<MemoryBudget>,
    pub(super) speed: &'a SpeedLimit,
    pub(super) total: u64,
    pub(super) validator: Option<&'a str>,
    pub(super) require_validator: bool,
    pub(super) observation: &'a Arc<Mutex<Observation>>,
    pub(super) wire: &'a AtomicU64,
    pub(super) timing: &'a Arc<TimingDiagnostics>,
    pub(super) log_level: LogLevel,
    pub(super) download_id: u64,
}

pub(super) async fn checked_response(
    ctx: &RequestContext<'_>,
    response: Option<(HttpResponse, ResponseMeta)>,
    request_end: u64,
) -> Result<HttpResponse, CandidateFailure> {
    let (response, meta) = match response {
        Some(response) => response,
        None => {
            ctx.worker
                .clone()
                .with_attempt(
                    ctx.segment.owner_worker_id,
                    ctx.segment.piece_id,
                    ctx.segment.lease_key().lease_id,
                )
                .send_range(ctx.segment.start, request_end - 1)
                .await?
        }
    };
    let diagnostics = response
        .extensions()
        .get::<crate::http::worker::RequestDiagnostics>();
    log_debug!(ctx.log_level, download_id = ctx.download_id,
        worker_id = ctx.segment.owner_worker_id,
        request_id = ?diagnostics.map(|d| d.id), lease = ?ctx.segment.lease_key(),
        start = ctx.segment.start, end = request_end,
        headers_ms = ?diagnostics.map(|d| d.headers_elapsed.as_millis() as u64),
        "adaptive response associated with lease");
    // A positively different validator is an identity failure even when the
    // same response also has an invalid range/total. Missing metadata on an
    // already malformed response remains an optional-candidate failure.
    if ctx.validator.is_some_and(|expected| {
        meta.etag
            .as_deref()
            .is_some_and(|actual| actual != expected)
    }) {
        return Err(CandidateFailure {
            error: DownloadError::ResumeMismatch(
                "object validator changed during adaptive download".into(),
            ),
            identity_changed: true,
        });
    }
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok());
    validate_range_response(
        response.status().as_u16(),
        retry_after,
        &meta,
        RangeValidationMode::Segment,
        ExpectedRange {
            start: ctx.segment.start,
            end_inclusive: request_end - 1,
            total_size: Some(ctx.total),
        },
    )?;
    if ctx.require_validator
        && ctx
            .validator
            .is_some_and(|etag| meta.etag.as_deref() != Some(etag))
    {
        return Err(CandidateFailure {
            error: DownloadError::ResumeMismatch(
                "object validator changed during adaptive download".into(),
            ),
            identity_changed: true,
        });
    }
    Ok(response)
}
pub(super) struct BodyActivity(Option<Arc<TimingDiagnostics>>);

impl BodyActivity {
    pub(super) fn new(timing: Arc<TimingDiagnostics>) -> Self {
        timing.body_started();
        Self(Some(timing))
    }

    fn finish(&mut self) {
        if let Some(timing) = self.0.take() {
            timing.body_finished();
        }
    }
}

impl Drop for BodyActivity {
    fn drop(&mut self) {
        self.finish();
    }
}

pub(super) struct RequestStream {
    body: crate::http::HttpBody,
    buffered: bytes::Bytes,
    pub(super) wire: u64,
    expected: u64,
    pub(super) consumed: u64,
    body_activity: BodyActivity,
}

impl RequestStream {
    fn finish_body(&mut self) {
        self.body_activity.finish();
    }
}

pub(super) async fn primary(
    ctx: &RequestContext<'_>,
    response: Option<(HttpResponse, ResponseMeta)>,
    stop: &mut watch::Receiver<StopSignal>,
    stream: &mut Option<RequestStream>,
) -> Result<(), DownloadError> {
    if stream.is_none() {
        let response = checked_response(ctx, response, ctx.request_end)
            .await
            .map_err(|failure| failure.error)?;
        ctx.observation.lock().request_id = response
            .extensions()
            .get::<crate::http::worker::RequestDiagnostics>()
            .map(|d| d.id);
        let body = response.into_body();
        *stream = Some(RequestStream {
            body,
            buffered: bytes::Bytes::new(),
            wire: 0,
            consumed: 0,
            expected: ctx.request_end - ctx.segment.start,
            body_activity: BodyActivity::new(ctx.timing.clone()),
        });
    }
    let stream = stream.as_mut().expect("initialized request body");
    loop {
        let forwarded = ctx.observation.lock().forwarded;
        if forwarded == ctx.segment.end - ctx.segment.start && ctx.segment.end < ctx.request_end {
            // A frame may contain the next piece. Keep its suffix across the
            // flush/complete barrier instead of issuing another HTTP request.
            ctx.observation.lock().phase(Phase::WriterBarrier);
            return Ok(());
        }
        if stream.buffered.is_empty() {
            ctx.observation.lock().phase(Phase::Reading);
            let chunk = next_data_chunk(&mut stream.body, ctx.read_timeout).await?;
            let Some(data) = chunk else {
                if stream.wire != stream.expected {
                    return Err(DownloadError::Transport(crate::error::TransportError::new(
                        crate::error::TransportErrorKind::Body,
                        std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "range body ended early",
                        ),
                    )));
                }
                stream.finish_body();
                ctx.observation.lock().phase(Phase::WriterBarrier);
                return Ok(());
            };
            stream.wire = stream.wire.saturating_add(data.len() as u64);
            ctx.wire.fetch_add(data.len() as u64, Ordering::Relaxed);
            ctx.observation.lock().wire += data.len() as u64;
            if stream.wire > stream.expected {
                return Err(DownloadError::ResumeMismatch(
                    "server overran requested range".into(),
                ));
            }
            stream.buffered = data;
            if stream.buffered.is_empty() {
                continue;
            }
        }
        let remaining = ctx.segment.end - ctx.segment.start - forwarded;
        let len = stream
            .buffered
            .len()
            .min(ctx.budget.max_chunk)
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        if len == 0 {
            return Err(DownloadError::ResumeMismatch(
                "server overran final piece".into(),
            ));
        }
        let data = stream.buffered.split_to(len);
        ctx.budget
            .forward_observed(
                data,
                ctx.segment.start + forwarded,
                Some(ctx.segment.lease_key()),
                ctx.write_tx,
                stop,
                ctx.speed,
                |len| {
                    stream.consumed += len;
                    let mut sample = ctx.observation.lock();
                    sample.forwarded += len;
                    sample.enqueued += len;
                    ctx.received.fetch_add(len, Ordering::Relaxed);
                },
                |phase| {
                    use crate::session::flow::ForwardPhase;
                    ctx.observation.lock().phase(match phase {
                        ForwardPhase::RateLimited => Phase::RateLimited,
                        ForwardPhase::MemoryBlocked => Phase::MemoryBlocked,
                        ForwardPhase::ChannelBlocked => Phase::ChannelBlocked,
                    });
                },
            )
            .await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use warp::Filter;

    #[tokio::test]
    async fn short_and_oversized_bodies_do_not_complete_a_piece() {
        for body_len in [2, 5] {
            let route = warp::any().map(move || vec![0xAC; body_len]);
            let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
            let server = tokio::spawn(server);
            let spec = DownloadSpec::new(format!("http://{addr}/piece"));
            let worker = HttpWorker::new(
                crate::network::ClientNetworkConfig::default()
                    .build_client()
                    .unwrap(),
                &spec,
            );
            let response = worker.send_get().await.unwrap().0;
            let scheduler = SchedulerState::new(PieceMap::new(4, 4));
            let scheduler = Arc::new(Mutex::new(scheduler));
            let segment = scheduler.lock().assign().unwrap();
            let (tx, mut rx) = mpsc::channel(4);
            let (_stop, mut stop_rx) = watch::channel(StopSignal::Running);
            let received = Arc::new(AtomicU64::new(0));
            let timing = Arc::new(TimingDiagnostics::new(Instant::now()));
            let observation = Arc::new(Mutex::new(Observation::new(
                Instant::now(),
                Duration::from_secs(5),
            )));
            let budget = Arc::new(MemoryBudget::new(64));
            let wire = AtomicU64::new(0);
            let speed = SpeedLimit::Unlimited;
            let context = RequestContext {
                worker: &worker,
                read_timeout: Duration::from_secs(2),
                segment: &segment,
                request_end: 4,
                write_tx: &tx,
                received: &received,
                budget: &budget,
                speed: &speed,
                total: 4,
                validator: None,
                require_validator: false,
                observation: &observation,
                wire: &wire,
                timing: &timing,
                log_level: LogLevel::Off,
                download_id: 0,
            };
            // Response validation is covered separately; exercise the actual
            // ordinary body consumer with both lengths and its writer output.
            let mut stream = Some(RequestStream {
                body: response.into_body(),
                buffered: bytes::Bytes::new(),
                wire: 0,
                expected: 4,
                consumed: 0,
                body_activity: BodyActivity::new(timing.clone()),
            });
            let error = primary(&context, None, &mut stop_rx, &mut stream)
                .await
                .unwrap_err();
            server.abort();
            let accepted = observation.lock().forwarded;
            if body_len == 2 {
                assert!(
                    matches!(error, DownloadError::Transport(ref e) if e.kind() == crate::error::TransportErrorKind::Body)
                );
                assert_eq!(accepted, 2);
            } else {
                assert!(matches!(error, DownloadError::ResumeMismatch(_)));
                assert!(accepted <= 4);
            }
            let mut forwarded = Vec::new();
            while let Ok(command) = rx.try_recv() {
                let WriterCommand::Data {
                    data,
                    offset,
                    lease_key,
                } = command
                else {
                    panic!("incomplete body must not flush its lease");
                };
                assert_eq!(offset, forwarded.len() as u64);
                assert_eq!(lease_key, Some(segment.lease_key()));
                forwarded.extend_from_slice(&data);
            }
            assert_eq!(forwarded, vec![0xAC; accepted as usize]);
            assert_eq!(received.load(Ordering::Relaxed), accepted);
            assert!(!scheduler.lock().all_done());
        }
    }
}
