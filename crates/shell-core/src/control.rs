//! Control Unix socket protocol (`shellctl` ↔ `shelld`): JSON, one message per line.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::manifest::{BackendKind, IdleMode, LifecycleHints, Priority};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    /// Check a package and show its permissions before installing.
    Inspect { path: PathBuf },
    /// Install a package. `grant_optional`: keys of optional permissions granted by the user.
    Install { path: PathBuf, grant_optional: Vec<String> },
    Uninstall { id: String },
    List,
    Start { id: String },
    /// Graceful stop; `force`: do not wait for a busy app to become free.
    Stop {
        id: String,
        #[serde(default)]
        force: bool,
    },
    /// Issue a new one-time URL for opening the UI.
    Url { id: String },
    Logs { id: String, lines: usize, follow: bool },
    /// Call the backend's `bridge.handle` bypassing the UI (debugging, headless). `payload` is JSON.
    Call { id: String, method: String, payload: String },
    /// One-time dashboard URL.
    DashboardUrl,
}

impl Request {
    /// The request name as on the wire (`"url"`, `"dashboard-url"`, …).
    pub fn name(&self) -> &'static str {
        match self {
            Request::Inspect { .. } => "inspect",
            Request::Install { .. } => "install",
            Request::Uninstall { .. } => "uninstall",
            Request::List => "list",
            Request::Start { .. } => "start",
            Request::Stop { .. } => "stop",
            Request::Url { .. } => "url",
            Request::Logs { .. } => "logs",
            Request::Call { .. } => "call",
            Request::DashboardUrl => "dashboard-url",
        }
    }

    /// The app the request is about, if any.
    pub fn app_id(&self) -> Option<&str> {
        match self {
            Request::Uninstall { id }
            | Request::Start { id }
            | Request::Stop { id, .. }
            | Request::Url { id }
            | Request::Logs { id, .. }
            | Request::Call { id, .. } => Some(id),
            _ => None,
        }
    }

    /// Whether an extra socket's rule (`<request>` or `<request>:<app-id>`) allows this request.
    pub fn allowed_by(&self, rule: &str) -> bool {
        match rule.split_once(':') {
            Some((name, id)) => name == self.name() && self.app_id() == Some(id),
            None => rule == self.name(),
        }
    }
}

const REQUEST_NAMES: &[&str] =
    &["inspect", "install", "uninstall", "list", "start", "stop", "url", "logs", "call", "dashboard-url"];

/// Validates an extra socket's rule.
pub fn check_rule(rule: &str) -> anyhow::Result<()> {
    let (name, id) = match rule.split_once(':') {
        Some((n, id)) => (n, Some(id)),
        None => (rule, None),
    };
    anyhow::ensure!(REQUEST_NAMES.contains(&name), "{rule:?}: unknown request {name:?}");
    if let Some(id) = id {
        crate::manifest::validate_app_id(id).map_err(|e| anyhow::anyhow!("{rule:?}: {e}"))?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Response {
    Ok,
    Error { message: String },
    Package(PackageInfo),
    Installed { id: String, version: String },
    Apps { apps: Vec<AppStatus> },
    Url { url: String },
    /// A log line; with `follow`, lines keep coming until the client disconnects.
    Log { line: String },
    /// Backend response (JSON).
    CallResult { result: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub has_ui: bool,
    /// Currently installed version, if this is an update.
    pub installed_version: Option<String>,
    pub permissions: Vec<PermissionInfo>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionInfo {
    pub key: String,
    pub description: String,
    pub reason: Option<String>,
    pub optional: bool,
    /// Granted to the installed app. In `PackageInfo` on update,
    /// `false` means "new permission" (or a previously ungranted optional one).
    pub granted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppStatus {
    pub id: String,
    pub name: String,
    pub version: String,
    pub state: AppState,
    pub pid: Option<u32>,
    /// All permissions from the manifest; granted ones are marked `granted`.
    pub permissions: Vec<PermissionInfo>,
    pub has_ui: bool,
    /// Start time of the current worker, Unix seconds.
    pub started_at: Option<u64>,
    /// How many times the app has started since shelld started.
    pub starts: u32,
    /// How many times the app has terminated abnormally since shelld started.
    pub failures: u32,
    /// Reason for the last exit.
    pub last_exit: Option<String>,
    /// WASM linear memory limit.
    pub memory_limit_mb: u32,
    pub metrics: Option<Metrics>,
    /// What the app declared in the manifest.
    pub hints: LifecycleHints,
    /// What Shell applies, taking the device policy into account.
    pub priority: Priority,
    pub idle: IdleMode,
    /// Current `activity.busy` locks: the app asks not to be stopped.
    pub busy: Vec<BusyInfo>,
    /// Last activity (UI, calls, utilities, busy state), Unix seconds.
    pub last_activity: Option<u64>,
    /// Position in the stop queue under memory pressure (1 = first).
    pub evict_rank: Option<u32>,
    /// A stopped app will start when the UI is opened or on a call (cold start).
    pub on_demand: bool,
    /// `app`: a component with a bridge and hooks, `command`: a program with `main`.
    pub kind: BackendKind,
    /// `net.listen` ports on which the program accepts connections.
    pub ports: Vec<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusyInfo {
    pub reason: String,
    /// Unix seconds.
    pub since: u64,
    /// Busy for longer than `max_busy_s` and no longer counted.
    pub expired: bool,
}

/// Why Shell stops an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StopReason {
    Idle,
    MemoryPressure,
    User,
    SelfRequested,
    Shutdown,
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            StopReason::Idle => "idle",
            StopReason::MemoryPressure => "memory pressure",
            StopReason::User => "user command",
            StopReason::SelfRequested => "at the app's request",
            StopReason::Shutdown => "Shell shutdown",
        })
    }
}

/// Resource usage of the app's process (cgroup or /proc).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub memory_bytes: u64,
    /// cgroup `memory.max`, if any.
    pub memory_max_bytes: Option<u64>,
    /// Total CPU time, µs; CPU% is the difference between samples.
    pub cpu_usage_usec: u64,
    pub pids: u32,
    /// Source: `cgroup` or `proc`.
    pub source: String,
}

/// Shell overview for the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellInfo {
    pub version: String,
    pub profile: String,
    pub cgroups: bool,
    pub started_at: u64,
    pub ui_port: Option<u16>,
    pub cpu_max_percent: u32,
    pub memory_mb: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppState {
    /// Installed, not running.
    Stopped,
    Starting,
    Running,
    /// UI hidden or closed: events are not sent to the UI.
    Suspended,
    Stopping,
    /// Stopped due to an error or a limit being exceeded.
    Failed,
}

impl std::fmt::Display for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            AppState::Stopped => "stopped",
            AppState::Starting => "starting",
            AppState::Running => "running",
            AppState::Suspended => "suspended",
            AppState::Stopping => "stopping",
            AppState::Failed => "failed",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules() {
        let url = Request::Url { id: "org.wshell.launcher".into() };
        assert!(url.allowed_by("url:org.wshell.launcher"));
        assert!(url.allowed_by("url"));
        assert!(!url.allowed_by("url:org.wshell.ping"));
        assert!(!url.allowed_by("list"));
        assert!(Request::List.allowed_by("list"));
        assert!(!Request::List.allowed_by("list:org.wshell.ping"));
        // The wire name and `name()` agree.
        let wire = serde_json::to_value(Request::DashboardUrl).unwrap();
        assert_eq!(wire["cmd"], Request::DashboardUrl.name());
        assert!(check_rule("url:org.wshell.launcher").is_ok());
        assert!(check_rule("install").is_ok());
        assert!(check_rule("launch").is_err());
        assert!(check_rule("url:Bad").is_err());
    }
}
