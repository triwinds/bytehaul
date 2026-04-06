use bytes::Bytes;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use tempfile::tempdir;

use bytehaul::bench::{ControlSnapshot, PieceMap, WriteBackCache};

fn bench_cache_insert_coalesce(c: &mut Criterion) {
    c.bench_function("cache_insert_1000_chunks", |b| {
        b.iter(|| {
            let mut cache = WriteBackCache::new();
            for i in 0u64..1000 {
                let offset = i * 4096;
                let data = Bytes::from(vec![0xABu8; 4096]);
                cache.insert(0, offset, data);
            }
            black_box(cache.total_bytes());
        });
    });

    c.bench_function("cache_drain_piece", |b| {
        b.iter_batched(
            || {
                let mut cache = WriteBackCache::new();
                for i in 0u64..500 {
                    cache.insert(0, i * 4096, Bytes::from(vec![0xCDu8; 4096]));
                }
                cache
            },
            |mut cache| {
                let blocks = cache.drain_piece(0);
                black_box(blocks.len());
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

fn bench_piece_map_serde(c: &mut Criterion) {
    let total_size = 1_000_000_000u64; // 1 GB
    let piece_size = 1_000_000u64; // 1 MB pieces → 1000 pieces

    c.bench_function("piece_map_to_bitset", |b| {
        let mut pm = PieceMap::new(total_size, piece_size);
        for i in (0..pm.piece_count()).step_by(2) {
            pm.mark_complete(i);
        }
        b.iter(|| {
            black_box(pm.to_bitset_bytes());
        });
    });

    c.bench_function("piece_map_from_bitset", |b| {
        let mut pm = PieceMap::new(total_size, piece_size);
        let count = pm.piece_count();
        for i in (0..count).step_by(2) {
            pm.mark_complete(i);
        }
        let bitset = pm.to_bitset_bytes();
        b.iter(|| {
            black_box(PieceMap::from_bitset(total_size, piece_size, &bitset, count));
        });
    });
}

fn bench_scheduler_snapshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("scheduler_snapshot");
    let piece_size = 1_000_000u64;

    for piece_count in [10_000usize, 100_000usize] {
        let total_size = piece_count as u64 * piece_size;
        group.bench_with_input(BenchmarkId::from_parameter(piece_count), &total_size, |b, &size| {
            b.iter(|| {
                black_box(bytehaul::bench::bench_scheduler_snapshot(size, piece_size));
            });
        });
    }

    group.finish();
}

fn bench_control_roundtrip(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    c.bench_function("control_save_load_roundtrip", |b| {
        let dir = tempdir().unwrap();
        let ctrl_path = dir.path().join("bench.bytehaul");
        let snapshot = ControlSnapshot {
            url: "https://example.com/large-file.bin".to_string(),
            total_size: 1_000_000_000,
            piece_size: 1_000_000,
            piece_count: 1000,
            completed_bitset: vec![0xFF; 125],
            downloaded_bytes: 500_000_000,
            etag: Some("\"etag-bench\"".to_string()),
            last_modified: Some("Thu, 01 Jan 2026 00:00:00 GMT".to_string()),
        };

        b.iter(|| {
            rt.block_on(async {
                snapshot.save(&ctrl_path).await.unwrap();
                let loaded = ControlSnapshot::load(&ctrl_path).await.unwrap();
                black_box(loaded.total_size);
            });
        });
    });
}

fn bench_control_save_only(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    c.bench_function("control_save_only", |b| {
        let dir = tempdir().unwrap();
        let ctrl_path = dir.path().join("autosave.bytehaul");
        let snapshot = ControlSnapshot {
            url: "https://example.com/large-file.bin".to_string(),
            total_size: 1_000_000_000,
            piece_size: 1_000_000,
            piece_count: 1000,
            completed_bitset: vec![0xFF; 125],
            downloaded_bytes: 500_000_000,
            etag: Some("\"etag-bench\"".to_string()),
            last_modified: Some("Thu, 01 Jan 2026 00:00:00 GMT".to_string()),
        };

        b.iter(|| {
            rt.block_on(async {
                snapshot.save(&ctrl_path).await.unwrap();
                black_box(std::fs::metadata(&ctrl_path).unwrap().len());
            });
        });
    });
}

fn bench_single_progress_reporting(c: &mut Criterion) {
    c.bench_function("single_progress_reporting_throttled", |b| {
        b.iter(|| {
            black_box(bytehaul::bench::bench_progress_reporting(1024, 8 * 1024, 10));
        });
    });
}

criterion_group!(
    benches,
    bench_cache_insert_coalesce,
    bench_piece_map_serde,
    bench_scheduler_snapshot,
    bench_control_save_only,
    bench_control_roundtrip,
    bench_single_progress_reporting
);
criterion_main!(benches);
