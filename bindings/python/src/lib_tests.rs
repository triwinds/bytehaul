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
    INIT.call_once(pyo3::prepare_freethreaded_python);
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
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let status_line = status_line.to_string();

    let handle = thread::spawn(move || {
        let mut served_any = false;
        let mut idle_deadline = Instant::now() + Duration::from_secs(5);

        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    served_any = true;
                    idle_deadline = Instant::now() + Duration::from_millis(250);

                    let mut request = [0u8; 4096];
                    let _ = stream.read(&mut request);

                    let response = format!(
                        "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).unwrap();
                    stream.write_all(&body).unwrap();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if served_any && Instant::now() >= idle_deadline {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("test server accept failed: {error}"),
            }
        }
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
fn test_python_test_helpers_and_config_error() {
    init_python();
    let err = config_error("bad config");
    assert!(err.to_string().contains("bad config"));

    let path_a = unique_path("a");
    let path_b = unique_path("b");
    assert_ne!(path_a, path_b);

    let value = with_python(|py| py.version().to_string());
    assert!(!value.is_empty());
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
    assert!(parse_doh_servers(None).unwrap().is_empty());
    assert_eq!(
        parse_doh_servers(Some(vec![" https://dns.google/dns-query ".into()])).unwrap(),
        vec!["https://dns.google/dns-query".to_string()]
    );
    assert!(parse_doh_servers(Some(vec!["   ".into()]))
        .unwrap_err()
        .to_string()
        .contains("doh_servers"));

    assert_eq!(parse_file_allocation("none").unwrap(), FileAllocation::None);
    assert_eq!(
        parse_file_allocation("PREALLOC").unwrap(),
        FileAllocation::Prealloc
    );
    assert!(parse_file_allocation("invalid")
        .unwrap_err()
        .to_string()
        .contains("file_allocation"));

    // log_level parsing
    assert_eq!(parse_log_level("off").unwrap(), LogLevel::Off);
    assert_eq!(parse_log_level("error").unwrap(), LogLevel::Error);
    assert_eq!(parse_log_level("warn").unwrap(), LogLevel::Warn);
    assert_eq!(parse_log_level("info").unwrap(), LogLevel::Info);
    assert_eq!(parse_log_level("debug").unwrap(), LogLevel::Debug);
    assert_eq!(parse_log_level("TRACE").unwrap(), LogLevel::Trace);
    assert!(parse_log_level("invalid")
        .unwrap_err()
        .to_string()
        .contains("log_level"));
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
        Some(vec!["https://127.0.0.1/dns-query".into()]),
        Some(false),
    )
    .unwrap();
    builder.build().unwrap();

    let mut headers = HashMap::new();
    headers.insert("Authorization".into(), "Bearer test".into());
    let output_path = unique_path("spec");
    let spec = build_download_spec(
        "https://example.com/file.bin".into(),
        Some(output_path.clone()),
        Some(std::env::temp_dir()),
        Some(headers.clone()),
        Some(3),
        Some(1.25),
        Some("http://127.0.0.1:8080".into()),
        Some("http://127.0.0.1:8081".into()),
        Some("http://127.0.0.1:8443".into()),
        Some(3.5),
        Some(8192),
        Some("none".into()),
        Some(false),
        Some(4096),
        Some(2048),
        Some(7),
        Some(0.5),
        Some(2.0),
        Some(4.0),
        Some(12345),
        Some(" abc123 ".into()),
        None,
        None,
        None,
    )
    .unwrap();

    assert_eq!(spec.get_url(), "https://example.com/file.bin");
    assert_eq!(spec.get_output_path(), Some(output_path.as_path()));
    assert_eq!(spec.get_output_dir(), Some(std::env::temp_dir().as_path()));
    assert_eq!(spec.get_headers(), &headers);
    assert_eq!(spec.get_max_connections(), 3);
    assert_eq!(spec.get_connect_timeout(), Duration::from_secs_f64(1.25));
    assert_eq!(spec.get_all_proxy(), Some("http://127.0.0.1:8080"));
    assert_eq!(spec.get_http_proxy(), Some("http://127.0.0.1:8081"));
    assert_eq!(spec.get_https_proxy(), Some("http://127.0.0.1:8443"));
    assert_eq!(spec.get_read_timeout(), Duration::from_secs_f64(3.5));
    assert_eq!(spec.get_memory_budget(), 8192);
    assert_eq!(spec.get_file_allocation(), FileAllocation::None);
    assert!(!spec.get_resume());
    assert_eq!(spec.get_piece_size(), 4096);
    assert_eq!(spec.get_min_split_size(), 2048);
    assert_eq!(spec.get_max_retries(), 7);
    assert_eq!(spec.get_retry_base_delay(), Duration::from_secs_f64(0.5));
    assert_eq!(spec.get_retry_max_delay(), Duration::from_secs_f64(2.0));
    assert_eq!(spec.get_max_retry_elapsed(), Some(Duration::from_secs_f64(4.0)));
    assert_eq!(spec.get_max_download_speed(), 12345);
    assert!(matches!(
        spec.get_checksum(),
        Some(Checksum::Sha256(ref checksum)) if checksum == "abc123"
    ));

    assert!(build_download_spec(
        "https://example.com/file.bin".into(),
        Some(unique_path("empty-checksum")),
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
        Some("   ".into()),
        None,
        None,
        None,
    )
    .unwrap_err()
    .to_string()
    .contains("checksum_sha256"));
}

#[test]
fn test_checksum_parsing_and_invalid_config_mapping() {
    init_python();

    assert!(matches!(
        parse_checksum_string("sha256:abc123").unwrap(),
        Checksum::Sha256(ref checksum) if checksum == "abc123"
    ));
    assert!(matches!(
        parse_checksum_string("SHA1:def456").unwrap(),
        Checksum::Sha1(ref checksum) if checksum == "def456"
    ));
    assert!(matches!(
        parse_checksum_string("md5:7890").unwrap(),
        Checksum::Md5(ref checksum) if checksum == "7890"
    ));
    assert!(matches!(
        parse_checksum_string("sha512:feedbeef").unwrap(),
        Checksum::Sha512(ref checksum) if checksum == "feedbeef"
    ));

    assert!(parse_checksum_string("sha256")
        .unwrap_err()
        .to_string()
        .contains("algorithm:hex_digest"));
    assert!(parse_checksum_string("sha256:   ")
        .unwrap_err()
        .to_string()
        .contains("cannot be empty"));
    assert!(parse_checksum_string("crc32:abcd")
        .unwrap_err()
        .to_string()
        .contains("unsupported checksum algorithm"));

    assert!(map_download_error(DownloadError::InvalidConfig("bad config".into()))
        .to_string()
        .contains("ConfigError"));
}

#[test]
fn test_build_download_spec_with_checksum_and_control_interval() {
    init_python();

    let spec = build_download_spec(
        "https://example.com/file.bin".into(),
        Some(unique_path("checksum-spec")),
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
        Some("sha512: deadbeef ".into()),
        Some(3.0),
        Some(4),
    )
    .unwrap();

    assert!(matches!(
        spec.get_checksum(),
        Some(Checksum::Sha512(checksum)) if checksum == "deadbeef"
    ));
    assert_eq!(
        spec.get_control_save_interval(),
        Duration::from_secs_f64(3.0)
    );
    assert_eq!(spec.get_autosave_sync_every(), 4);
}

#[test]
fn test_error_mapping_snapshot_conversion_and_repr() {
    init_python();
    assert!(map_download_error(DownloadError::Cancelled)
        .to_string()
        .contains("CancelledError"));
    assert!(map_download_error(DownloadError::Paused)
        .to_string()
        .contains("PausedError"));
    assert!(map_download_error(DownloadError::Internal("boom".into()))
        .to_string()
        .contains("InternalError"));
    assert!(map_download_error(DownloadError::ResumeMismatch("resume mismatch".into()))
        .to_string()
        .contains("ResumeError"));
    assert!(map_download_error(DownloadError::RetryBudgetExceeded {
        elapsed: Duration::from_secs(3),
        limit: Duration::from_secs(2),
    })
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
            eta_secs: Some(2.5),
            start_time: Some(Instant::now()),
        };
        let py_snapshot = snapshot_to_py(&snapshot);
        assert_eq!(py_snapshot.state, expected);
        assert_eq!(py_snapshot.total_size, Some(64));
        assert_eq!(py_snapshot.downloaded, 32);
        assert_eq!(py_snapshot.speed, 12.5);
        assert_eq!(py_snapshot.eta_secs, Some(2.5));
        assert!(py_snapshot.elapsed_secs.is_some());
    }

    let with_values = PyProgressSnapshot {
        total_size: Some(10),
        downloaded: 5,
        state: "completed".into(),
        speed: 1.5,
        eta_secs: Some(1.25),
        elapsed_secs: Some(2.25),
    };
    assert!(with_values.__repr__().contains("completed"));

    let without_values = PyProgressSnapshot {
        total_size: None,
        downloaded: 0,
        state: "pending".into(),
        speed: 0.0,
        eta_secs: None,
        elapsed_secs: None,
    };
    let rendered = without_values.__repr__();
    assert!(rendered.contains("None"));
    assert!(rendered.contains("pending"));
}

#[test]
fn test_download_task_methods_and_consumption_errors() {
    init_python();
    let downloader = PyDownloader::new(None, None, None, None, None, None, None, None).unwrap();
    let task = downloader
        .download(
            "http://127.0.0.1:1/unreachable".into(),
            Some(unique_path("task-error")),
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
    task.pause().unwrap();

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
    none_task.pause().unwrap();
    assert!(with_python(|py| none_task.wait(py).unwrap_err())
        .to_string()
        .contains("already consumed"));
}

#[test]
fn test_download_task_pause_maps_to_paused_error() {
    init_python();
    let downloader = PyDownloader::new(None, None, None, None, None, None, None, None).unwrap();
    let task = downloader
        .download(
            "http://127.0.0.1:1/unreachable".into(),
            Some(unique_path("task-paused")),
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
            Some(true),
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

    task.pause().unwrap();

    let wait_err = with_python(|py| task.wait(py).unwrap_err());
    assert!(wait_err.to_string().contains("PausedError"));
}

#[test]
fn test_py_downloader_with_log_level() {
    init_python();
    // Test with explicit log_level
    let d =
        PyDownloader::new(None, None, None, None, None, None, None, Some("debug".into()))
            .unwrap();
    drop(d);
    // Test invalid log_level
    let result =
        PyDownloader::new(None, None, None, None, None, None, None, Some("invalid".into()));
    assert!(result.is_err());
    assert!(result.err().unwrap().to_string().contains("log_level"));
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
        Some(vec!["https://127.0.0.1/dns-query".into()]),
        Some(false),
        None,
    )
    .unwrap();
    drop(configured);

    let body = b"hello from python bindings".to_vec();
    let (base_url, server) = spawn_static_server("200 OK", body.clone());
    let output_path = unique_path("py-downloader-success");
    cleanup(&output_path);

    let mut headers = HashMap::new();
    headers.insert("X-Test".into(), "1".into());
    let downloader = PyDownloader::new(None, None, None, None, None, None, None, None).unwrap();
    let task = downloader
        .download(
            format!("{base_url}/file.bin"),
            Some(output_path.clone()),
            None,
            Some(headers),
            Some(2),
            Some(1.0),
            None,
            None,
            None,
            Some(2.0),
            Some(4096),
            Some("none".into()),
            Some(false),
            Some(1024),
            Some(2048),
            Some(1),
            Some(0.25),
            Some(0.5),
            Some(2.0),
            Some(0),
            None,
            None,
            None,
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
            "PausedError",
            "ConfigError",
            "ResumeError",
            "InternalError",
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
            Some(output_path.clone()),
            None,
            None,
            Some(2),
            Some(1.0),
            None,
            None,
            None,
            Some(vec!["1.1.1.1".into()]),
            Some(vec!["https://127.0.0.1/dns-query".into()]),
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
            Some(2.0),
            Some(0),
            None,
            None,
            None,
            None,
            Some("debug".into()),
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
            Some(unique_path("top-level-error")),
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
