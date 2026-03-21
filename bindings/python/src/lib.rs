use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bytehaul::{Checksum, DownloadError, DownloadSpec, FileAllocation};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use tokio::runtime::{Builder, Runtime};

create_exception!(_bytehaul, BytehaulError, PyException);
create_exception!(_bytehaul, CancelledError, BytehaulError);
create_exception!(_bytehaul, ConfigError, BytehaulError);
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

#[allow(clippy::too_many_arguments)]
fn build_download_spec(
    url: String,
    output_path: PathBuf,
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
    max_download_speed: Option<u64>,
    checksum_sha256: Option<String>,
) -> PyResult<DownloadSpec> {
    let mut spec = DownloadSpec::new(url, output_path);

    if let Some(headers) = headers {
        spec.headers = headers;
    }
    if let Some(max_connections) = max_connections {
        spec.max_connections = non_zero_u32("max_connections", max_connections)?;
    }
    if let Some(connect_timeout) = connect_timeout {
        spec.connect_timeout = duration_from_secs("connect_timeout", connect_timeout)?;
    }
    if let Some(read_timeout) = read_timeout {
        spec.read_timeout = duration_from_secs("read_timeout", read_timeout)?;
    }
    if let Some(memory_budget) = memory_budget {
        spec.memory_budget = non_zero_usize("memory_budget", memory_budget)?;
    }
    if let Some(file_allocation) = file_allocation {
        spec.file_allocation = parse_file_allocation(&file_allocation)?;
    }
    if let Some(resume) = resume {
        spec.resume = resume;
    }
    if let Some(piece_size) = piece_size {
        spec.piece_size = non_zero_u64("piece_size", piece_size)?;
    }
    if let Some(min_split_size) = min_split_size {
        spec.min_split_size = non_zero_u64("min_split_size", min_split_size)?;
    }
    if let Some(max_retries) = max_retries {
        spec.max_retries = max_retries;
    }
    if let Some(retry_base_delay) = retry_base_delay {
        spec.retry_base_delay = duration_from_secs("retry_base_delay", retry_base_delay)?;
    }
    if let Some(retry_max_delay) = retry_max_delay {
        spec.retry_max_delay = duration_from_secs("retry_max_delay", retry_max_delay)?;
    }
    if let Some(max_download_speed) = max_download_speed {
        spec.max_download_speed = max_download_speed;
    }
    if let Some(checksum_sha256) = checksum_sha256 {
        let checksum_sha256 = checksum_sha256.trim().to_string();
        if checksum_sha256.is_empty() {
            return Err(config_error("checksum_sha256 cannot be empty"));
        }
        spec.checksum = Some(Checksum::Sha256(checksum_sha256));
    }

    Ok(spec)
}

fn map_download_error(error: DownloadError) -> PyErr {
    match error {
        DownloadError::Cancelled => CancelledError::new_err("download cancelled"),
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
    elapsed_secs: Option<f64>,
}

#[pymethods]
impl PyProgressSnapshot {
    fn __repr__(&self) -> String {
        format!(
            "ProgressSnapshot(state='{}', downloaded={}, total_size={}, speed={:.1}, elapsed_secs={})",
            self.state,
            self.downloaded,
            match self.total_size {
                Some(s) => s.to_string(),
                None => "None".to_string(),
            },
            self.speed,
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
            None => Err(BytehaulError::new_err(
                "task already consumed by wait()",
            )),
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

    fn wait(&self, py: Python<'_>) -> PyResult<()> {
        let handle = {
            let mut guard = self.handle.lock().unwrap();
            guard.take().ok_or_else(|| {
                BytehaulError::new_err("task already consumed by wait()")
            })?
        };
        let runtime = shared_runtime()?;
        py.allow_threads(move || {
            runtime.block_on(handle.wait()).map_err(map_download_error)
        })
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
    #[pyo3(signature = (connect_timeout = None))]
    fn new(connect_timeout: Option<f64>) -> PyResult<Self> {
        let mut builder = bytehaul::Downloader::builder();
        if let Some(ct) = connect_timeout {
            builder = builder.connect_timeout(duration_from_secs("connect_timeout", ct)?);
        }
        let inner = builder.build().map_err(map_download_error)?;
        Ok(Self { inner })
    }

    #[pyo3(
        signature = (
            url,
            output_path,
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
            max_download_speed = None,
            checksum_sha256 = None
        )
    )]
    #[allow(clippy::too_many_arguments)]
    fn download(
        &self,
        url: String,
        output_path: PathBuf,
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
        max_download_speed: Option<u64>,
        checksum_sha256: Option<String>,
    ) -> PyResult<PyDownloadTask> {
        let spec = build_download_spec(
            url,
            output_path,
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
            max_download_speed,
            checksum_sha256,
        )?;
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
        output_path,
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
        max_download_speed = None,
        checksum_sha256 = None
    )
)]
#[allow(clippy::too_many_arguments)]
fn download(
    py: Python<'_>,
    url: String,
    output_path: PathBuf,
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
    max_download_speed: Option<u64>,
    checksum_sha256: Option<String>,
) -> PyResult<()> {
    let spec = build_download_spec(
        url,
        output_path,
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
        max_download_speed,
        checksum_sha256,
    )?;
    let runtime = shared_runtime()?;

    py.allow_threads(move || {
        let downloader = bytehaul::Downloader::builder()
            .connect_timeout(spec.connect_timeout)
            .build()
            .map_err(map_download_error)?;

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
    module.add("ConfigError", py.get_type::<ConfigError>())?;
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
