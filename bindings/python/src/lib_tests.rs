use super::*;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pyo3::types::PyModule;

fn init_python() {
    static INIT: Once = Once::new();
    INIT.call_once(|| pyo3::prepare_freethreaded_python());
}

fn with_python<T>(f: impl for<'py> FnOnce(Python<'py>) -> T) -> T {
    init_python();
    Python::with_gil(f)
}

fn unique_path(name: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "bytehaul-python-tests-{name}-{}-{suffix}.bin",
        std::process::id()
    ))
}

fn spawn_static_server(status_line: &str, body: Vec<u8>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let status_line = status_line.to_string();

    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request);

        let response = format!(
            "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.write_all(&body).unwrap();
    });

    (format!("http://{addr}"), handle)
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[test]
fn test_runtime_duration_and_non_zero_helpers() {
    init_python();
    assert!(std::ptr::eq(
        shared_runtime().unwrap(),
        shared_runtime().unwrap()
    ));

    assert_eq!(
        duration_from_secs("connect_timeout", 1.5).unwrap(),
        Duration::from_secs_f64(1.5)
    );
    assert!(duration_from_secs("connect_timeout", f64::NAN)
        .unwrap_err()
        .to_string()
        .contains("finite"));
    assert!(duration_from_secs("connect_timeout", -1.0)
        .unwrap_err()
        .to_string()
        .contains(">= 0"));

    assert_eq!(non_zero_u32("max_connections", 2).unwrap(), 2);
    assert!(non_zero_u32("max_connections", 0)
        .unwrap_err()
        .to_string()
        .contains(">= 1"));
    assert_eq!(non_zero_u64("piece_size", 16).unwrap(), 16);
    assert!(non_zero_u64("piece_size", 0)
        .unwrap_err()
        .to_string()
        .contains(">= 1"));
    assert_eq!(non_zero_usize("memory_budget", 32).unwrap(), 32);
    assert!(non_zero_usize("memory_budget", 0)
        .unwrap_err()
        .to_string()
        .contains(">= 1"));
}

#[test]
fn test_socket_dns_and_file_allocation_parsing() {
    init_python();
    assert!(parse_socket_addr("dns_servers", "   ")
        .unwrap_err()
        .to_string()
        .contains("cannot be empty"));

    assert_eq!(
        parse_socket_addr("dns_servers", "8.8.8.8:5353").unwrap(),
        SocketAddr::from(([8, 8, 8, 8], 5353))
    );
    assert_eq!(
        parse_socket_addr("dns_servers", "8.8.4.4").unwrap(),
        SocketAddr::from(([8, 8, 4, 4], 53))
    );
    assert_eq!(
        parse_socket_addr("dns_servers", "[::1]").unwrap(),
        SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 53)
    );
    assert!(parse_socket_addr("dns_servers", "not-an-ip")
        .unwrap_err()
        .to_string()
        .contains("IPs or socket addresses"));

    assert!(parse_dns_servers(None).unwrap().is_empty());
    assert_eq!(
        parse_dns_servers(Some(vec!["1.1.1.1".into(), "8.8.8.8:5353".into()])).unwrap(),
        vec![
            SocketAddr::from(([1, 1, 1, 1], 53)),
            SocketAddr::from(([8, 8, 8, 8], 5353))
        ]
    );

    assert_eq!(parse_file_allocation("none").unwrap(), FileAllocation::None);
    assert_eq!(
        parse_file_allocation("PREALLOC").unwrap(),
        FileAllocation::Prealloc
    );
    assert!(parse_file_allocation("invalid")
        .unwrap_err()
        .to_string()
        .contains("file_allocation"));
}

#[test]
fn test_apply_client_options_and_build_download_spec() {
    init_python();
    let builder = apply_client_options(
        bytehaul::Downloader::builder(),
        Some(2.5),
        Some("http://127.0.0.1:8080".into()),
        Some("http://127.0.0.1:8081".into()),
        Some("http://127.0.0.1:8443".into()),
        Some(vec!["1.1.1.1".into(), "[::1]".into()]),
        Some(false),
    )
    .unwrap();
    builder.build().unwrap();

    let mut headers = HashMap::new();
    headers.insert("Authorization".into(), "Bearer test".into());
    let output_path = unique_path("spec");
    let spec = build_download_spec(
        "https://example.com/file.bin".into(),
        output_path.clone(),
        Some(headers.clone()),
        Some(3),
        Some(1.25),
        Some(3.5),
        Some(8192),
        Some("none".into()),
        Some(false),
        Some(4096),
        Some(2048),
        Some(7),
        Some(0.5),
        Some(2.0),
        Some(12345),
        Some(" abc123 ".into()),
    )
    .unwrap();

    assert_eq!(spec.url, "https://example.com/file.bin");
    assert_eq!(spec.output_path, output_path);
    assert_eq!(spec.headers, headers);
    assert_eq!(spec.max_connections, 3);
    assert_eq!(spec.connect_timeout, Duration::from_secs_f64(1.25));
    assert_eq!(spec.read_timeout, Duration::from_secs_f64(3.5));
    assert_eq!(spec.memory_budget, 8192);
    assert_eq!(spec.file_allocation, FileAllocation::None);
    assert!(!spec.resume);
    assert_eq!(spec.piece_size, 4096);
    assert_eq!(spec.min_split_size, 2048);
    assert_eq!(spec.max_retries, 7);
    assert_eq!(spec.retry_base_delay, Duration::from_secs_f64(0.5));
    assert_eq!(spec.retry_max_delay, Duration::from_secs_f64(2.0));
    assert_eq!(spec.max_download_speed, 12345);
    assert!(matches!(
        spec.checksum,
        Some(Checksum::Sha256(ref checksum)) if checksum == "abc123"
    ));

    assert!(build_download_spec(
        "https://example.com/file.bin".into(),
        unique_path("empty-checksum"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some("   ".into()),
    )
    .unwrap_err()
    .to_string()
    .contains("checksum_sha256"));
}

#[test]
fn test_error_mapping_snapshot_conversion_and_repr() {
    init_python();
    assert!(map_download_error(DownloadError::Cancelled)
        .to_string()
        .contains("CancelledError"));
    assert!(map_download_error(DownloadError::Other("boom".into()))
        .to_string()
        .contains("DownloadFailedError"));

    let cases = [
        (bytehaul::DownloadState::Pending, "pending"),
        (bytehaul::DownloadState::Downloading, "downloading"),
        (bytehaul::DownloadState::Completed, "completed"),
        (bytehaul::DownloadState::Failed, "failed"),
        (bytehaul::DownloadState::Cancelled, "cancelled"),
        (bytehaul::DownloadState::Paused, "paused"),
    ];

    for (state, expected) in cases {
        let snapshot = bytehaul::ProgressSnapshot {
            total_size: Some(64),
            downloaded: 32,
            state,
            speed_bytes_per_sec: 12.5,
            start_time: Some(Instant::now()),
        };
        let py_snapshot = snapshot_to_py(&snapshot);
        assert_eq!(py_snapshot.state, expected);
        assert_eq!(py_snapshot.total_size, Some(64));
        assert_eq!(py_snapshot.downloaded, 32);
        assert_eq!(py_snapshot.speed, 12.5);
        assert!(py_snapshot.elapsed_secs.is_some());
    }

    let with_values = PyProgressSnapshot {
        total_size: Some(10),
        downloaded: 5,
        state: "completed".into(),
        speed: 1.5,
        elapsed_secs: Some(2.25),
    };
    assert!(with_values.__repr__().contains("completed"));

    let without_values = PyProgressSnapshot {
        total_size: None,
        downloaded: 0,
        state: "pending".into(),
        speed: 0.0,
        elapsed_secs: None,
    };
    let rendered = without_values.__repr__();
    assert!(rendered.contains("None"));
    assert!(rendered.contains("pending"));
}

#[test]
fn test_download_task_methods_and_consumption_errors() {
    init_python();
    let downloader = PyDownloader::new(None, None, None, None, None, None).unwrap();
    let task = downloader
        .download(
            "http://127.0.0.1:1/unreachable".into(),
            unique_path("task-error"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();

    let snapshot = task.progress().unwrap();
    assert!(matches!(
        snapshot.state.as_str(),
        "pending" | "downloading" | "failed"
    ));

    task.cancel().unwrap();

    let wait_err = with_python(|py| task.wait(py).unwrap_err());
    assert!(wait_err.to_string().contains("Error"));
    assert!(with_python(|py| task.wait(py).unwrap_err())
        .to_string()
        .contains("already consumed"));
    match task.progress() {
        Ok(_) => panic!("expected progress() to fail after wait()"),
        Err(err) => assert!(err.to_string().contains("already consumed")),
    }
    task.cancel().unwrap();

    let none_task = PyDownloadTask {
        handle: Arc::new(Mutex::new(None)),
    };
    match none_task.progress() {
        Ok(_) => panic!("expected progress() to fail for an empty task"),
        Err(err) => assert!(err.to_string().contains("already consumed")),
    }
    none_task.cancel().unwrap();
    assert!(with_python(|py| none_task.wait(py).unwrap_err())
        .to_string()
        .contains("already consumed"));
}

#[test]
fn test_py_downloader_download_success_and_module_registration() {
    init_python();
    let configured = PyDownloader::new(
        Some(1.0),
        Some("http://127.0.0.1:8080".into()),
        Some("http://127.0.0.1:8081".into()),
        Some("http://127.0.0.1:8443".into()),
        Some(vec!["1.1.1.1".into()]),
        Some(false),
    )
    .unwrap();
    drop(configured);

    let body = b"hello from python bindings".to_vec();
    let (base_url, server) = spawn_static_server("200 OK", body.clone());
    let output_path = unique_path("py-downloader-success");
    cleanup(&output_path);

    let mut headers = HashMap::new();
    headers.insert("X-Test".into(), "1".into());
    let downloader = PyDownloader::new(None, None, None, None, None, None).unwrap();
    let task = downloader
        .download(
            format!("{base_url}/file.bin"),
            output_path.clone(),
            Some(headers),
            Some(2),
            Some(1.0),
            Some(2.0),
            Some(4096),
            Some("none".into()),
            Some(false),
            Some(1024),
            Some(2048),
            Some(1),
            Some(0.25),
            Some(0.5),
            Some(0),
            None,
        )
        .unwrap();

    with_python(|py| task.wait(py).unwrap());
    server.join().unwrap();
    assert_eq!(std::fs::read(&output_path).unwrap(), body);
    cleanup(&output_path);

    with_python(|py| {
        let module = PyModule::new(py, "_bytehaul_test").unwrap();
        _bytehaul(py, &module).unwrap();

        assert_eq!(
            module
                .getattr("__version__")
                .unwrap()
                .extract::<String>()
                .unwrap(),
            env!("CARGO_PKG_VERSION")
        );
        for name in [
            "BytehaulError",
            "CancelledError",
            "ConfigError",
            "DownloadFailedError",
            "download",
            "Downloader",
            "DownloadTask",
            "ProgressSnapshot",
        ] {
            assert!(
                module.getattr(name).is_ok(),
                "missing module attribute {name}"
            );
        }
    });
}

#[test]
fn test_top_level_download_success_and_failure() {
    init_python();
    let body = b"top level download body".to_vec();
    let (base_url, server) = spawn_static_server("200 OK", body.clone());
    let output_path = unique_path("top-level-success");
    cleanup(&output_path);

    with_python(|py| {
        download(
            py,
            format!("{base_url}/ok"),
            output_path.clone(),
            None,
            Some(2),
            Some(1.0),
            None,
            None,
            None,
            Some(vec!["1.1.1.1".into()]),
            Some(false),
            Some(2.0),
            Some(8192),
            Some("prealloc".into()),
            Some(false),
            Some(1024),
            Some(2048),
            Some(1),
            Some(0.25),
            Some(0.5),
            Some(0),
            None,
        )
        .unwrap();
    });
    server.join().unwrap();
    assert_eq!(std::fs::read(&output_path).unwrap(), body);
    cleanup(&output_path);

    let err = with_python(|py| {
        download(
            py,
            "http://127.0.0.1:1/fail".into(),
            unique_path("top-level-error"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_err()
    });
    assert!(err.to_string().contains("DownloadFailedError"));
}
