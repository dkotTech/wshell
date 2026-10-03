//! supervisor ↔ worker IPC: `u32 LE length + postcard` frames over the worker's stdin/stdout
//! (WASM guest output goes to stderr).
//!
//! The worker has no authority of its own: it forwards all host calls (storage, network,
//! running utilities) to the supervisor, which re-checks the permissions.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use shell_core::control::StopReason;
use shell_core::manifest::BackendKind;

pub const MAX_FRAME: usize = 16 << 20;

#[derive(Debug, Serialize, Deserialize)]
pub enum ToWorker {
    Init(Init),
    Call { id: u64, method: String, payload: String },
    Event(HostEvent),
    HostReply { id: u64, result: Result<HostReply, String> },
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Init {
    pub kind: BackendKind,
    pub wasm_path: String,
    pub cwasm_path: String,
    /// SHA-256 of the `.cwasm` recorded at install time: a corrupted image is not loaded.
    pub cwasm_sha256: String,
    pub memory_bytes: u64,
    pub fuel_per_call: u64,
    pub grants: Grants,
}

/// Permissions for which the worker hands out handles. Permission parameters are checked by the supervisor.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Grants {
    pub storage: bool,
    pub http: bool,
    pub exec: Vec<String>,
    /// `net.listen` ports.
    pub listen: Vec<u16>,
    /// `shell.apps` (plugins).
    pub apps: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostEvent {
    ExecOutput { job: u64, stderr: bool, data: Vec<u8> },
    ExecExit { job: u64, exit_code: Option<i32>, timed_out: bool },
    UiVisible(bool),
    /// Shell is about to stop a busy app (analogous to SIGTERM).
    StopRequested { reason: StopReason, grace_ms: u32 },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum FromWorker {
    Started(Result<(), String>),
    CallResult { id: u64, result: Result<String, String> },
    HostCall { id: u64, call: HostCall },
    Emit { name: String, payload: String },
    Log { level: LogLevel, message: String },
    /// Reason for abnormal termination (trap) before the worker exits.
    Fatal(String),
    /// An `activity.busy` handle was created.
    Busy { id: u32, reason: String },
    /// An `activity.busy` handle was released.
    BusyDone { id: u32 },
    /// `activity.request-stop()`.
    RequestStop,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Trace => "TRACE",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum HostCall {
    StorageGet { key: String },
    StorageSet { key: String, value: Vec<u8> },
    StorageDelete { key: String },
    StorageKeys,
    HttpSend(HttpRequest),
    ExecRun { name: String, args: Vec<(String, String)> },
    ExecSpawn { name: String, args: Vec<(String, String)> },
    ExecCancel { job: u64 },
    /// `metrics.publish`: the app's whole metrics snapshot (Prometheus text format).
    MetricsPublish { snapshot: String },
    /// `shell.apps`: the installed apps.
    AppsList,
    /// `shell.apps`: a one-time link to an app's UI.
    AppsUiLink { id: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum HostReply {
    Unit,
    Value(Option<Vec<u8>>),
    Keys(Vec<String>),
    Http(Result<HttpResponse, HttpError>),
    ExecOutput(ExecOutput),
    Job(u64),
    Apps(Vec<AppEntry>),
    Url(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AppEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub state: shell_core::control::AppState,
    pub has_ui: bool,
    pub plugin: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

/// Why a `net.http` request was not performed; mapped to `wasi:http` error codes.
#[derive(Debug, Serialize, Deserialize)]
pub enum HttpError {
    /// Permission not granted, host or method not allowed.
    Denied(String),
    Timeout,
    Dns(String),
    Connection(String),
    TooLarge,
    Other(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExecOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

pub fn encode<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    // Serialize right after the space reserved for the length and fill it in, without a second copy.
    let mut frame = postcard::to_extend(msg, vec![0u8; 4]).map_err(io::Error::other)?;
    let len = frame.len() - 4;
    if len > MAX_FRAME {
        return Err(io::Error::other("IPC message too large"));
    }
    frame[..4].copy_from_slice(&(len as u32).to_le_bytes());
    Ok(frame)
}

pub fn decode<T: DeserializeOwned>(body: &[u8]) -> io::Result<T> {
    postcard::from_bytes(body).map_err(io::Error::other)
}

pub fn write_sync<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    w.write_all(&encode(msg)?)
}

pub fn read_sync<T: DeserializeOwned>(r: &mut impl Read) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::other("IPC frame too large"));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    decode(&body)
}

pub async fn read_async<T: DeserializeOwned>(
    r: &mut (impl tokio::io::AsyncRead + Unpin),
) -> io::Result<T> {
    use tokio::io::AsyncReadExt;
    let len = r.read_u32_le().await? as usize;
    if len > MAX_FRAME {
        return Err(io::Error::other("IPC frame too large"));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).await?;
    decode(&body)
}
