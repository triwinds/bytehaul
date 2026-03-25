use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Once};
use std::time::Duration;

use bytehaul::{Checksum, DownloadError, DownloadSpec, FileAllocation, LogLevel};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use tokio::runtime::{Builder, Runtime};

create_exception!(_bytehaul, BytehaulError, PyException);
create_exception!(_bytehaul, CancelledError, BytehaulError);
create_exception!(_bytehaul, PausedError, BytehaulError);
create_exception!(_bytehaul, ConfigError, BytehaulError);
create_exception!(_bytehaul, ResumeError, BytehaulError);
create_exception!(_bytehaul, InternalError, BytehaulError);
create_exception!(_bytehaul, DownloadFailedError, BytehaulError);

static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();

fn shared_runtime() -> PyResult<&'static Runtime> {
    match RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| err.to_string())
    }) {
        Ok(runtime) => Ok(runtime),
        Err(message) => Err(BytehaulError::new_err(message.clone())),
    }
}

fn config_error(message: impl Into<String>) -> PyErr {
    ConfigError::new_err(message.into())
}

fn duration_from_secs(field: &str, value: f64) -> PyResult<Duration> {
    if !value.is_finite() {
        return Err(config_error(format!("{field} must be a finite number")));
    }
    if value < 0.0 {
        return Err(config_error(format!("{field} must be >= 0")));
    }
    Ok(Duration::from_secs_f64(value))
}

fn parse_socket_addr(field: &str, value: &str) -> PyResult<SocketAddr> {
    let value = value.trim();
    if value.is_empty() {
        return Err(config_error(format!("{field} entries cannot be empty")));
    }

    if let Ok(addr) = value.parse::<SocketAddr>() {
        return Ok(addr);
    }

    let ip_text = if value.starts_with('[') && value.ends_with(']') {
        &value[1..value.len() - 1]
    } else {
        value
    };

    if let Ok(ip) = ip_text.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, 53));
    }

    Err(config_error(format!(
        "{field} entries must be IPs or socket addresses, got: {value}"
    )))
}

fn parse_dns_servers(dns_servers: Option<Vec<String>>) -> PyResult<Vec<SocketAddr>> {
    match dns_servers {
        Some(dns_servers) => dns_servers
            .into_iter()
            .map(|server| parse_socket_addr("dns_servers", &server))
            .collect(),
        None => Ok(Vec::new()),
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_client_options(
    mut builder: bytehaul::DownloaderBuilder,
    connect_timeout: Option<f64>,
    proxy: Option<String>,
    http_proxy: Option<String>,
    https_proxy: Option<String>,
    dns_servers: Option<Vec<String>>,
    enable_ipv6: Option<bool>,
) -> PyResult<bytehaul::DownloaderBuilder> {
    if let Some(connect_timeout) = connect_timeout {
        builder = builder.connect_timeout(duration_from_secs("connect_timeout", connect_timeout)?);
    }
    if let Some(proxy) = proxy {
        builder = builder.all_proxy(proxy);
    }
    if let Some(http_proxy) = http_proxy {
        builder = builder.http_proxy(http_proxy);
    }
    if let Some(https_proxy) = https_proxy {
        builder = builder.https_proxy(https_proxy);
    }

    let dns_servers = parse_dns_servers(dns_servers)?;
    if !dns_servers.is_empty() {
        builder = builder.dns_servers(dns_servers);
    }
    if let Some(enable_ipv6) = enable_ipv6 {
        builder = builder.enable_ipv6(enable_ipv6);
    }

    Ok(builder)
}

fn non_zero_u32(field: &str, value: u32) -> PyResult<u32> {
    if value == 0 {
        return Err(config_error(format!("{field} must be >= 1")));
    }
    Ok(value)
}

fn non_zero_u64(field: &str, value: u64) -> PyResult<u64> {
    if value == 0 {
        return Err(config_error(format!("{field} must be >= 1")));
    }
    Ok(value)
}

fn non_zero_usize(field: &str, value: usize) -> PyResult<usize> {
    if value == 0 {
        return Err(config_error(format!("{field} must be >= 1")));
    }
    Ok(value)
}

fn parse_file_allocation(value: &str) -> PyResult<FileAllocation> {
    match value.to_ascii_lowercase().as_str() {
        "none" => Ok(FileAllocation::None),
        "prealloc" => Ok(FileAllocation::Prealloc),
        _ => Err(config_error(
            "file_allocation must be one of: 'none', 'prealloc'",
        )),
    }
}

fn parse_log_level(value: &str) -> PyResult<LogLevel> {
    value.parse::<LogLevel>().map_err(|_| {
        config_error("log_level must be one of: 'off', 'error', 'warn', 'info', 'debug', 'trace'")
    })
}

static TRACING_INIT: Once = Once::new();

fn init_tracing(level: LogLevel) {
    if matches!(level, LogLevel::Off) {
        return;
    }
    TRACING_INIT.call_once(|| {
        let filter = level.to_tracing_level_filter();
        tracing_subscriber::fmt()
            .with_max_level(filter)
            .with_target(true)
            .with_thread_ids(false)
            .init();
    });
}

#[allow(clippy::too_many_arguments)]
fn build_download_spec(
    url: String,
    output_path: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    headers: Option<HashMap<String, String>>,
    max_connections: Option<u32>,
    connect_timeout: Option<f64>,
    read_timeout: Option<f64>,
    memory_budget: Option<usize>,
    file_allocation: Option<String>,
    resume: Option<bool>,
    piece_size: Option<u64>,
    min_split_size: Option<u64>,
    max_retries: Option<u32>,
    retry_base_delay: Option<f64>,
    retry_max_delay: Option<f64>,
    max_retry_elapsed: Option<f64>,
    max_download_speed: Option<u64>,
    checksum_sha256: Option<String>,
    control_save_interval: Option<f64>,
) -> PyResult<DownloadSpec> {
    let mut spec = DownloadSpec::new(url);

    if let Some(output_path) = output_path {
        spec = spec.output_path(output_path);
    }
    if let Some(output_dir) = output_dir {
        spec = spec.output_dir(output_dir);
    }

    if let Some(headers) = headers {
        spec = spec.headers(headers);
    }
    if let Some(max_connections) = max_connections {
        spec = spec.max_connections(non_zero_u32("max_connections", max_connections)?);
    }
    if let Some(connect_timeout) = connect_timeout {
        spec = spec.connect_timeout(duration_from_secs("connect_timeout", connect_timeout)?);
    }
    if let Some(read_timeout) = read_timeout {
        spec = spec.read_timeout(duration_from_secs("read_timeout", read_timeout)?);
    }
    if let Some(memory_budget) = memory_budget {
        spec = spec.memory_budget(non_zero_usize("memory_budget", memory_budget)?);
    }
    if let Some(file_allocation) = file_allocation {
        spec = spec.file_allocation(parse_file_allocation(&file_allocation)?);
    }
    if let Some(resume) = resume {
        spec = spec.resume(resume);
    }
    if let Some(piece_size) = piece_size {
        spec = spec.piece_size(non_zero_u64("piece_size", piece_size)?);
    }
    if let Some(min_split_size) = min_split_size {
        spec = spec.min_split_size(non_zero_u64("min_split_size", min_split_size)?);
    }
    if let Some(max_retries) = max_retries {
        spec = spec.max_retries(max_retries);
    }
    if let Some(retry_base_delay) = retry_base_delay {
        spec = spec.retry_base_delay(duration_from_secs("retry_base_delay", retry_base_delay)?);
    }
    if let Some(retry_max_delay) = retry_max_delay {
        spec = spec.retry_max_delay(duration_from_secs("retry_max_delay", retry_max_delay)?);
    }
    if let Some(max_retry_elapsed) = max_retry_elapsed {
        spec = spec.max_retry_elapsed(duration_from_secs(
            "max_retry_elapsed",
            max_retry_elapsed,
        )?);
    }
    if let Some(max_download_speed) = max_download_speed {
        spec = spec.max_download_speed(max_download_speed);
    }
    if let Some(checksum_sha256) = checksum_sha256 {
        let checksum_sha256 = checksum_sha256.trim().to_string();
        if checksum_sha256.is_empty() {
            return Err(config_error("checksum_sha256 cannot be empty"));
        }
        spec = spec.checksum(Checksum::Sha256(checksum_sha256));
    }
    if let Some(interval) = control_save_interval {
        spec = spec.control_save_interval(duration_from_secs(
            "control_save_interval",
            interval,
        )?);
    }

    spec.validate().map_err(|err| config_error(err.to_string()))?;

    Ok(spec)
}

fn map_download_error(error: DownloadError) -> PyErr {
    match error {
        DownloadError::Cancelled => CancelledError::new_err("download cancelled"),
        DownloadError::Paused => PausedError::new_err("download paused"),
        DownloadError::InvalidConfig(message) => ConfigError::new_err(message),
        DownloadError::ResumeMismatch(message)
        | DownloadError::ControlFileCorrupted(message) => ResumeError::new_err(message),
        DownloadError::TaskFailed(message) | DownloadError::Internal(message) => {
            InternalError::new_err(message)
        }
        other => DownloadFailedError::new_err(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// ProgressSnapshot Python wrapper
// ---------------------------------------------------------------------------

#[pyclass(frozen, name = "ProgressSnapshot")]
struct PyProgressSnapshot {
    #[pyo3(get)]
    total_size: Option<u64>,
    #[pyo3(get)]
    downloaded: u64,
    #[pyo3(get)]
    state: String,
    #[pyo3(get)]
    speed: f64,
    #[pyo3(get)]
    eta_secs: Option<f64>,
    #[pyo3(get)]
    elapsed_secs: Option<f64>,
}

#[pymethods]
impl PyProgressSnapshot {
    fn __repr__(&self) -> String {
        format!(
            "ProgressSnapshot(state='{}', downloaded={}, total_size={}, speed={:.1}, eta_secs={}, elapsed_secs={})",
            self.state,
            self.downloaded,
            match self.total_size {
                Some(s) => s.to_string(),
                None => "None".to_string(),
            },
            self.speed,
            match self.eta_secs {
                Some(eta) => format!("{eta:.2}"),
                None => "None".to_string(),
            },
            match self.elapsed_secs {
                Some(e) => format!("{e:.2}"),
                None => "None".to_string(),
            },
        )
    }
}

fn snapshot_to_py(snap: &bytehaul::ProgressSnapshot) -> PyProgressSnapshot {
    use bytehaul::DownloadState;
    let state = match snap.state {
        DownloadState::Pending => "pending",
        DownloadState::Downloading => "downloading",
        DownloadState::Completed => "completed",
        DownloadState::Failed => "failed",
        DownloadState::Cancelled => "cancelled",
        DownloadState::Paused => "paused",
    };
    PyProgressSnapshot {
        total_size: snap.total_size,
        downloaded: snap.downloaded,
        state: state.to_string(),
        speed: snap.speed_bytes_per_sec,
        eta_secs: snap.eta_secs,
        elapsed_secs: snap.start_time.map(|t| t.elapsed().as_secs_f64()),
    }
}

// ---------------------------------------------------------------------------
// DownloadTask — wraps DownloadHandle
// ---------------------------------------------------------------------------

#[pyclass(name = "DownloadTask")]
struct PyDownloadTask {
    handle: Arc<Mutex<Option<bytehaul::DownloadHandle>>>,
}

#[pymethods]
impl PyDownloadTask {
    fn progress(&self) -> PyResult<PyProgressSnapshot> {
        let guard = self.handle.lock().unwrap();
        match guard.as_ref() {
            Some(h) => Ok(snapshot_to_py(&h.progress())),
            None => Err(BytehaulError::new_err("task already consumed by wait()")),
        }
    }

    fn cancel(&self) -> PyResult<()> {
        let guard = self.handle.lock().unwrap();
        match guard.as_ref() {
            Some(h) => {
                h.cancel();
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn pause(&self) -> PyResult<()> {
        let guard = self.handle.lock().unwrap();
        match guard.as_ref() {
            Some(h) => {
                h.pause();
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn on_progress(&self, callback: PyObject) -> PyResult<()> {
        let guard = self.handle.lock().unwrap();
        let handle = guard
            .as_ref()
            .ok_or_else(|| BytehaulError::new_err("task already consumed by wait()"))?;
        handle.on_progress(move |snap| {
            Python::with_gil(|py| {
                let py_snap = snapshot_to_py(&snap);
                if let Err(err) = callback.call1(py, (py_snap,)) {
                    err.write_unraisable(py, None);
                }
            });
        });
        Ok(())
    }

    fn wait(&self, py: Python<'_>) -> PyResult<()> {
        let handle = {
            let mut guard = self.handle.lock().unwrap();
            guard
                .take()
                .ok_or_else(|| BytehaulError::new_err("task already consumed by wait()"))?
        };
        let runtime = shared_runtime()?;
        py.allow_threads(move || runtime.block_on(handle.wait()).map_err(map_download_error))
    }
}

// ---------------------------------------------------------------------------
// Downloader — wraps Rust Downloader
// ---------------------------------------------------------------------------

#[pyclass(name = "Downloader")]
struct PyDownloader {
    inner: bytehaul::Downloader,
}

#[pymethods]
impl PyDownloader {
    #[new]
    #[pyo3(signature = (
        connect_timeout = None,
        proxy = None,
        http_proxy = None,
        https_proxy = None,
        dns_servers = None,
        enable_ipv6 = None,
        log_level = None
    ))]
    fn new(
        connect_timeout: Option<f64>,
        proxy: Option<String>,
        http_proxy: Option<String>,
        https_proxy: Option<String>,
        dns_servers: Option<Vec<String>>,
        enable_ipv6: Option<bool>,
        log_level: Option<String>,
    ) -> PyResult<Self> {
        let level = match &log_level {
            Some(s) => parse_log_level(s)?,
            None => LogLevel::Off,
        };
        init_tracing(level);
        let mut builder = apply_client_options(
            bytehaul::Downloader::builder(),
            connect_timeout,
            proxy,
            http_proxy,
            https_proxy,
            dns_servers,
            enable_ipv6,
        )?;
        builder = builder.log_level(level);
        let inner = builder.build().map_err(map_download_error)?;
        Ok(Self { inner })
    }

    #[pyo3(
        signature = (
            url,
            output_path = None,
            output_dir = None,
            headers = None,
            max_connections = None,
            connect_timeout = None,
            read_timeout = None,
            memory_budget = None,
            file_allocation = None,
            resume = None,
            piece_size = None,
            min_split_size = None,
            max_retries = None,
            retry_base_delay = None,
            retry_max_delay = None,
            max_retry_elapsed = None,
            max_download_speed = None,
            checksum_sha256 = None,
            control_save_interval = None
        )
    )]
    #[allow(clippy::too_many_arguments)]
    fn download(
        &self,
        url: String,
        output_path: Option<PathBuf>,
        output_dir: Option<PathBuf>,
        headers: Option<HashMap<String, String>>,
        max_connections: Option<u32>,
        connect_timeout: Option<f64>,
        read_timeout: Option<f64>,
        memory_budget: Option<usize>,
        file_allocation: Option<String>,
        resume: Option<bool>,
        piece_size: Option<u64>,
        min_split_size: Option<u64>,
        max_retries: Option<u32>,
        retry_base_delay: Option<f64>,
        retry_max_delay: Option<f64>,
        max_retry_elapsed: Option<f64>,
        max_download_speed: Option<u64>,
        checksum_sha256: Option<String>,
        control_save_interval: Option<f64>,
    ) -> PyResult<PyDownloadTask> {
        let spec = build_download_spec(
            url,
            output_path,
            output_dir,
            headers,
            max_connections,
            connect_timeout,
            read_timeout,
            memory_budget,
            file_allocation,
            resume,
            piece_size,
            min_split_size,
            max_retries,
            retry_base_delay,
            retry_max_delay,
            max_retry_elapsed,
            max_download_speed,
            checksum_sha256,
            control_save_interval,
        )?;
        let runtime = shared_runtime()?;
        let _guard = runtime.enter();
        let handle = self.inner.download(spec);
        Ok(PyDownloadTask {
            handle: Arc::new(Mutex::new(Some(handle))),
        })
    }
}

// ---------------------------------------------------------------------------
// Top-level download() convenience function (M1 API)
// ---------------------------------------------------------------------------

#[pyfunction(
    signature = (
        url,
        output_path = None,
        output_dir = None,
        headers = None,
        max_connections = None,
        connect_timeout = None,
        proxy = None,
        http_proxy = None,
        https_proxy = None,
        dns_servers = None,
        enable_ipv6 = None,
        read_timeout = None,
        memory_budget = None,
        file_allocation = None,
        resume = None,
        piece_size = None,
        min_split_size = None,
        max_retries = None,
        retry_base_delay = None,
        retry_max_delay = None,
        max_retry_elapsed = None,
        max_download_speed = None,
        checksum_sha256 = None,
        control_save_interval = None,
        log_level = None
    )
)]
#[allow(clippy::too_many_arguments)]
fn download(
    py: Python<'_>,
    url: String,
    output_path: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    headers: Option<HashMap<String, String>>,
    max_connections: Option<u32>,
    connect_timeout: Option<f64>,
    proxy: Option<String>,
    http_proxy: Option<String>,
    https_proxy: Option<String>,
    dns_servers: Option<Vec<String>>,
    enable_ipv6: Option<bool>,
    read_timeout: Option<f64>,
    memory_budget: Option<usize>,
    file_allocation: Option<String>,
    resume: Option<bool>,
    piece_size: Option<u64>,
    min_split_size: Option<u64>,
    max_retries: Option<u32>,
    retry_base_delay: Option<f64>,
    retry_max_delay: Option<f64>,
    max_retry_elapsed: Option<f64>,
    max_download_speed: Option<u64>,
    checksum_sha256: Option<String>,
    control_save_interval: Option<f64>,
    log_level: Option<String>,
) -> PyResult<()> {
    let level = match &log_level {
        Some(s) => parse_log_level(s)?,
        None => LogLevel::Off,
    };
    init_tracing(level);
    let spec = build_download_spec(
        url,
        output_path,
        output_dir,
        headers,
        max_connections,
        connect_timeout,
        read_timeout,
        memory_budget,
        file_allocation,
        resume,
        piece_size,
        min_split_size,
        max_retries,
        retry_base_delay,
        retry_max_delay,
        max_retry_elapsed,
        max_download_speed,
        checksum_sha256,
        control_save_interval,
    )?;
    let runtime = shared_runtime()?;

    py.allow_threads(move || {
        let mut builder = apply_client_options(
            bytehaul::Downloader::builder(),
            Some(spec.get_connect_timeout().as_secs_f64()),
            proxy,
            http_proxy,
            https_proxy,
            dns_servers,
            enable_ipv6,
        )?;
        builder = builder.log_level(level);

        let downloader = builder.build().map_err(map_download_error)?;

        runtime.block_on(async move {
            let handle = downloader.download(spec);
            handle.wait().await.map_err(map_download_error)
        })
    })
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

fn register_exceptions(py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("BytehaulError", py.get_type::<BytehaulError>())?;
    module.add("CancelledError", py.get_type::<CancelledError>())?;
    module.add("PausedError", py.get_type::<PausedError>())?;
    module.add("ConfigError", py.get_type::<ConfigError>())?;
    module.add("ResumeError", py.get_type::<ResumeError>())?;
    module.add("InternalError", py.get_type::<InternalError>())?;
    module.add("DownloadFailedError", py.get_type::<DownloadFailedError>())?;
    Ok(())
}

#[pymodule]
fn _bytehaul(py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    register_exceptions(py, module)?;
    module.add_function(wrap_pyfunction!(download, module)?)?;
    module.add_class::<PyDownloader>()?;
    module.add_class::<PyDownloadTask>()?;
    module.add_class::<PyProgressSnapshot>()?;
    Ok(())
}

#[cfg(test)]
mod lib_tests;
