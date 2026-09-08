use super::*;

/// Hold the FIFO acknowledgement so the scheduler cannot infer completion from
/// enqueued byte counts; then exercise success, writer failure and task abort.
#[tokio::test]
async fn prefix_handoff_requires_live_writer_confirmation() {
    for outcome in ["confirm", "writer_failure", "cancel"] {
        let scheduler: Scheduler = Arc::new(parking_lot::Mutex::new(SchedulerState::new(
            PieceMap::new(32, 32),
        )));
        let mut segment = scheduler.lock().assign_to(0).unwrap();
        let key = segment.lease_key();
        let received = Arc::new(AtomicU64::new(12));
        let (write_tx, mut write_rx) = mpsc::channel(1);
        let task_scheduler = scheduler.clone();
        let task_received = received.clone();
        let task = tokio::spawn(async move {
            let result = settle_prefix(
                &write_tx,
                &task_scheduler,
                &mut segment,
                &task_received,
                12,
                true,
            )
            .await;
            (result, segment)
        });
        let Some(WriterCommand::FlushLease { lease_key, ack }) = write_rx.recv().await else {
            panic!("prefix handoff must await FlushLease");
        };
        assert_eq!(lease_key, key);
        assert!(!task.is_finished());
        assert_eq!(scheduler.lock().completed_bytes(), 0);
        assert_eq!(received.load(Ordering::Relaxed), 12);

        match outcome {
            "confirm" => {
                ack.send(()).unwrap();
                let (result, segment) = task.await.unwrap();
                result.unwrap();
                assert_eq!((segment.start, segment.end), (12, 32));
                assert_eq!(received.load(Ordering::Relaxed), 12);
            }
            "writer_failure" => {
                drop(ack);
                let (result, segment) = task.await.unwrap();
                assert!(matches!(result, Err(DownloadError::ChannelClosed)));
                assert_eq!((segment.start, segment.end), (0, 32));
                assert_eq!(received.load(Ordering::Relaxed), 0);
            }
            "cancel" => {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
                // A writer acknowledgement arriving after producer cancellation
                // cannot publish prefix completion through the dropped future.
                assert!(ack.send(()).is_err());
            }
            _ => unreachable!(),
        }
        let mut state = scheduler.lock();
        assert_eq!(state.completed_bytes(), 0);
        assert!(state.reclaim(key));
        let next = state.assign_to(1).unwrap();
        assert_eq!(next.start, if outcome == "confirm" { 12 } else { 0 });
        assert_eq!(next.end, 32);
        assert_ne!(next.lease_key(), key);
    }
}
