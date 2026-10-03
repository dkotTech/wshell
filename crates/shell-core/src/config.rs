//! Shell configuration: TOML layers "build defaults → device config → overrides".

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const DEFAULTS: &str = include_str!("../defaults.toml");
pub const SYSTEM_CONFIG: &str = "/etc/wshell/shell.toml";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub shell: ShellSection,
    pub features: Features,
    pub http: HttpSection,
    pub dashboard: DashboardSection,
    pub metrics: MetricsSection,
    pub profile: Profile,
    pub exec: ExecSection,
    pub cgroups: CgroupsSection,
    pub lifecycle: LifecycleSection,
    #[serde(default)]
    pub control: ControlSection,
    #[serde(default)]
    pub policy: PolicySection,
}

/// Control sockets besides `control.sock`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlSection {
    /// `<runtime_dir>/<name>.sock` accepting only the listed requests: narrow access for
    /// other components of the device, e.g. the Renderer getting links to a launcher plugin.
    #[serde(default)]
    pub sockets: BTreeMap<String, ExtraSocket>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtraSocket {
    /// File mode, e.g. `0o660` for a component running as another user in Shell's group.
    #[serde(default = "default_socket_mode")]
    pub mode: u32,
    /// Allowed requests: `<request>` or `<request>:<app-id>`, e.g. `"url:org.wshell.launcher"`, `"list"`.
    pub allow: Vec<String>,
}

fn default_socket_mode() -> u32 {
    0o600
}

/// Device rules for stopping apps.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleSection {
    /// Cold start: a stopped app is started on a bridge call
    /// or when its UI is opened.
    pub start_on_demand: bool,
    /// Stop an app after this many seconds of inactivity; 0 disables it.
    pub idle_stop_after_s: u64,
    /// Respect `idle = "keep"` from the manifest (can be turned off on constrained devices).
    pub honor_keep: bool,
    /// How long to wait for a busy app to become free after `stop-requested`.
    pub stop_grace_ms: u64,
    /// Busy state longer than this is not counted (protection against a lock held forever).
    pub max_busy_s: u64,
    /// Total memory budget of all apps (cgroup `memory.high` on `apps/`).
    /// When exceeded, apps are stopped by priority; 0 disables it.
    pub memory_high_mb: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShellSection {
    /// Installed packages, app data, logs. Empty means the per-user default.
    #[serde(default)]
    pub data_dir: Option<PathBuf>,
    /// Control socket directory.
    #[serde(default)]
    pub runtime_dir: Option<PathBuf>,
    pub log_level: String,
    pub isolation: Isolation,
    /// Threads for AOT compilation of components; 0 means all cores.
    pub compile_threads: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Isolation {
    Process,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Features {
    pub ui_server: bool,
    /// Plugins (`kind = "plugin"`): apps with access to Shell itself (`shell.*` permissions).
    pub plugins: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpSection {
    pub port: u16,
    pub max_message_kb: u32,
}

/// Dashboard: app status and management, a separate port on loopback.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardSection {
    pub enabled: bool,
    pub port: u16,
    /// Size limit for a package uploaded via the dashboard.
    pub max_upload_mb: u32,
}

/// `/metrics` for a Prometheus (or agent) on the device: Shell's own metrics and the
/// snapshots apps publish through `shell:app/metrics`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsSection {
    pub enabled: bool,
    pub listen: std::net::SocketAddr,
    /// `Authorization: Bearer` required on `/metrics`; mandatory off loopback.
    pub token: String,
    /// Series kept from one app's snapshot; the rest is dropped and counted.
    pub max_series_per_app: usize,
    pub max_snapshot_kb: usize,
}

/// Resource profile (a single one in the MVP).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    /// Share of one core per app, 0 means no limit.
    pub cpu_max_percent: u32,
    /// WASM linear memory limit per app.
    pub memory_mb: u32,
    /// Extra on top of `memory_mb` for the worker process cgroup (the runtime itself).
    pub runtime_overhead_mb: u32,
    pub pids_max: u32,
    /// Fuel per backend call (≈ number of wasm instructions).
    pub fuel_per_call: u64,
    /// Hard timeout for lifecycle hooks.
    pub hook_timeout_ms: u64,
    /// Timeout of a single bridge call (including waiting for host calls).
    pub call_timeout_ms: u64,
    pub bridge_events_per_s: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecSection {
    /// Privileges apps may request for `exec`.
    pub allowed_privileges: Vec<String>,
    pub max_concurrent: u32,
    pub max_output_kb: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CgroupsSection {
    /// `auto`: use if a cgroup is delegated; `off`; `required`.
    pub mode: CgroupMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CgroupMode {
    Auto,
    Off,
    Required,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySection {
    #[serde(default)]
    pub app: BTreeMap<String, AppPolicy>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppPolicy {
    #[serde(default)]
    pub autostart: bool,
    /// Reducing memory below the profile.
    pub memory_mb: Option<u32>,
    /// Overrides `lifecycle.priority` from the manifest.
    pub priority: Option<crate::manifest::Priority>,
    /// Overrides `lifecycle.idle` from the manifest (including forbidding `keep`).
    pub idle: Option<crate::manifest::IdleMode>,
    /// Allow cold start for this app (defaults to `lifecycle.start_on_demand`).
    pub on_demand: Option<bool>,
}

impl Config {
    /// Loads defaults, the system config (if any) and additional files in order.
    pub fn load(extra: &[PathBuf]) -> Result<Self> {
        let mut merged: toml::Table = toml::from_str(DEFAULTS).context("built-in defaults")?;
        let mut layers: Vec<PathBuf> = Vec::new();
        if Path::new(SYSTEM_CONFIG).exists() {
            layers.push(SYSTEM_CONFIG.into());
        }
        layers.extend(extra.iter().cloned());
        for path in &layers {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let layer: toml::Table =
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            merge(&mut merged, layer);
        }
        let cfg: Config = toml::Value::Table(merged)
            .try_into()
            .with_context(|| format!("configuration (layers: {layers:?})"))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        let p = &self.profile;
        ensure!(p.cpu_max_percent <= 100 * 1024, "profile.cpu_max_percent is too large");
        ensure!(p.memory_mb >= 4, "profile.memory_mb must be at least 4");
        ensure!(p.pids_max >= 1, "profile.pids_max must be at least 1");
        ensure!(p.fuel_per_call > 0, "profile.fuel_per_call must be greater than 0");
        ensure!(self.http.max_message_kb > 0, "http.max_message_kb must be greater than 0");
        ensure!(
            !self.dashboard.enabled || self.dashboard.port != self.http.port,
            "dashboard.port must differ from http.port"
        );
        for id in self.policy.app.keys() {
            crate::manifest::validate_app_id(id).with_context(|| format!("policy.app.{id}"))?;
        }
        let m = &self.metrics;
        ensure!(
            m.listen.ip().is_loopback() || !m.token.is_empty(),
            "metrics.listen {} is not loopback: set metrics.token",
            m.listen
        );
        ensure!(m.max_series_per_app > 0 && m.max_snapshot_kb > 0, "metrics limits must be greater than 0");
        for (name, socket) in &self.control.sockets {
            ensure!(
                !name.is_empty() && name != "control" && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "control.sockets.{name}: the name must be [a-z0-9-] and not \"control\""
            );
            ensure!(socket.mode & !0o777 == 0, "control.sockets.{name}.mode: only permission bits (0o777)");
            ensure!(!socket.allow.is_empty(), "control.sockets.{name}.allow is empty");
            for rule in &socket.allow {
                crate::control::check_rule(rule).with_context(|| format!("control.sockets.{name}.allow"))?;
            }
        }
        Ok(())
    }

    pub fn data_dir(&self) -> PathBuf {
        self.shell.data_dir.clone().unwrap_or_else(default_data_dir)
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.shell.runtime_dir.clone().unwrap_or_else(default_runtime_dir)
    }

    pub fn policy(&self, app_id: &str) -> AppPolicy {
        self.policy.app.get(app_id).cloned().unwrap_or_default()
    }
}

fn merge(base: &mut toml::Table, layer: toml::Table) {
    for (k, v) in layer {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(l)) => merge(b, l),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

pub fn is_root() -> bool {
    std::fs::metadata("/proc/self").map(|m| m.uid() == 0).unwrap_or(false)
}

pub fn default_data_dir() -> PathBuf {
    if is_root() {
        return "/var/lib/wshell".into();
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| "/tmp".into())
        .join("wshell")
}

pub fn default_runtime_dir() -> PathBuf {
    if is_root() {
        return "/run/wshell".into();
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("wshell")
}

pub fn control_socket(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("control.sock")
}

/// Path of an extra control socket from `[control.sockets]`.
pub fn extra_socket(runtime_dir: &Path, name: &str) -> PathBuf {
    runtime_dir.join(format!("{name}.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let cfg: Config = toml::from_str(DEFAULTS).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn layers_override_nested_keys() {
        let mut base: toml::Table = toml::from_str(DEFAULTS).unwrap();
        merge(&mut base, toml::from_str("[profile]\nmemory_mb = 16").unwrap());
        let cfg: Config = toml::Value::Table(base).try_into().unwrap();
        assert_eq!(cfg.profile.memory_mb, 16);
        assert!(cfg.profile.pids_max > 0, "neighbouring keys are preserved");
    }

    fn with(layer: &str) -> Result<Config> {
        let mut base: toml::Table = toml::from_str(DEFAULTS).unwrap();
        merge(&mut base, toml::from_str(layer).unwrap());
        let cfg: Config = toml::Value::Table(base).try_into()?;
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn metrics_off_loopback_needs_token() {
        assert!(with("[metrics]\nlisten = \"0.0.0.0:9470\"").is_err());
        assert!(with("[metrics]\nlisten = \"0.0.0.0:9470\"\ntoken = \"s\"").is_ok());
        assert!(with("[metrics]\nlisten = \"[::1]:9470\"").is_ok());
    }

    #[test]
    fn extra_sockets_validated() {
        assert!(with("[control.sockets.render]\nallow = [\"url:org.wshell.launcher\"]").is_ok());
        assert!(with("[control.sockets.render]\nallow = [\"launch\"]").is_err());
        assert!(with("[control.sockets.control]\nallow = [\"list\"]").is_err());
        assert!(with("[control.sockets.render]\nmode = 0o4777\nallow = [\"list\"]").is_err());
    }

    #[test]
    fn unknown_keys_rejected() {
        let mut base: toml::Table = toml::from_str(DEFAULTS).unwrap();
        merge(&mut base, toml::from_str("[profile]\nmemroy_mb = 16").unwrap());
        assert!(toml::Value::Table(base).try_into::<Config>().is_err());
    }
}
