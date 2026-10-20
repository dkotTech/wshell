//! Supervisor: app registry, worker process lifecycle, call bridge.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use shell_core::config::{CgroupMode, Config};
use shell_core::control::{AppState, AppStatus, Metrics, PackageInfo, StopReason};
use shell_core::manifest::BackendKind;
use shell_core::source::Source;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use wasmtime::Engine;

use crate::cgroup::{AppCgroup, Cgroups, Limits};
use crate::ipc::{self, FromWorker, HostEvent, Init, ToWorker};
use crate::lifecycle::{Activity, eviction_key};
use crate::logs::{AppLog, Audit};
use crate::package::{self, Installed, Paths};
use crate::util::unix_now;

const TOKEN_TTL: Duration = Duration::from_secs(300);
/// How many concurrent sessions we keep per access scope.
#[cfg(any(feature = "ui-server", feature = "dashboard"))]
const MAX_SESSIONS_PER_SCOPE: usize = 32;
/// Time for loading the AOT image and instantiation on top of the hook timeout.
const STARTUP_GRACE: Duration = Duration::from_secs(5);
/// How long a cold start waits for an ongoing start or stop to finish.
const COLD_START_WAIT: Duration = Duration::from_secs(15);
/// Upper bound on the pause before a crashed app is cold-started again.
const MAX_RESTART_BACKOFF: Duration = Duration::from_secs(300);

pub struct Supervisor {
    pub cfg: Config,
    pub paths: Paths,
    pub engine: Engine,
    pub audit: Audit,
    pub http_client: reqwest::Client,
    /// For OCI registries: follows redirects to their CDNs, longer timeouts.
    registry_client: reqwest::Client,
    /// `registry.keys`, loaded at start.
    trusted_keys: Vec<crate::signature::TrustedKey>,
    pub(crate) cgroups: Option<Cgroups>,
    apps: Mutex<BTreeMap<String, App>>,
    /// One-time token → access scope and expiry.
    tokens: Mutex<HashMap<String, (Scope, Instant)>>,
    /// Session (cookie value) → access scope.
    sessions: Mutex<HashMap<String, Scope>>,
    next_job: AtomicU64,
    install_lock: tokio::sync::Mutex<()>,
    /// One pass over the desired-state file at a time.
    pub reconcile_lock: tokio::sync::Mutex<()>,
    /// D-Bus connection for `system.services`, opened on first use.
    pub dbus: tokio::sync::OnceCell<zbus::Connection>,
    /// System bus connection for `system.network` (NetworkManager), opened on first use.
    pub system_bus: tokio::sync::OnceCell<zbus::Connection>,
    #[cfg_attr(not(any(feature = "dashboard", feature = "metrics")), allow(dead_code))]
    started_at: u64,
    /// Wakes those waiting for a start or stop to finish (cold start).
    state_changed: tokio::sync::Notify,
}

struct App {
    installed: Arc<Installed>,
    state: AppState,
    worker: Option<Arc<Worker>>,
    log: Arc<AppLog>,
    starts: u32,
    failures: u32,
    last_exit: Option<String>,
    last_exit_at: Option<Instant>,
    /// The last metrics snapshot the app published; kept while it is stopped.
    metrics: Option<Arc<PublishedMetrics>>,
    /// An update or rollback is switching versions: no other start, calls wait.
    updating: bool,
}

/// An app's metrics snapshot, validated.
#[cfg_attr(not(feature = "metrics"), allow(dead_code))]
pub struct PublishedMetrics {
    pub snapshot: shell_core::metrics::Snapshot,
    /// Unix seconds.
    pub at: u64,
}

impl App {
    fn new(installed: Installed, log: Arc<AppLog>) -> Self {
        App {
            installed: Arc::new(installed),
            state: AppState::Stopped,
            worker: None,
            log,
            starts: 0,
            failures: 0,
            last_exit: None,
            last_exit_at: None,
            metrics: None,
            updating: false,
        }
    }
}

/// Access scope of a token or session: one app's UI or the dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    App(String),
    Dashboard,
}

/// A UI event already serialized into a `{"event", "payload"}` WebSocket frame:
/// once per event, not once per subscriber.
pub type UiEvent = Arc<str>;

#[derive(Debug)]
pub enum CallError {
    NotRunning,
    Timeout,
    TooLarge,
    /// Too many calls of the app in flight (`bridge_max_pending`).
    Busy,
    /// Cold start failed or is not allowed right now.
    StartFailed(String),
    App(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::NotRunning => f.write_str("app is not running"),
            CallError::Timeout => f.write_str("call timed out"),
            CallError::TooLarge => f.write_str("message too large"),
            CallError::Busy => f.write_str("too many calls in flight"),
            CallError::StartFailed(e) => write!(f, "failed to start app: {e}"),
            CallError::App(e) => f.write_str(e),
        }
    }
}

/// A running worker process of an app.
pub struct Worker {
    pub app_id: String,
    pub installed: Arc<Installed>,
    pub log: Arc<AppLog>,
    pub pid: u32,
    /// Start time, Unix seconds.
    pub started_at: u64,
    pub events: broadcast::Sender<UiEvent>,
    pub exited: watch::Receiver<bool>,
    pub cgroup: Option<AppCgroup>,
    pub jobs: Mutex<HashMap<u64, oneshot::Sender<()>>>,
    pub active_exec: AtomicUsize,
    tx: mpsc::UnboundedSender<ToWorker>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<String, String>>>>,
    started: Mutex<Option<oneshot::Sender<Result<(), String>>>>,
    next_call: AtomicU64,
    visible: AtomicUsize,
    rate: Mutex<RateLimiter>,
    /// Reason for abnormal termination, reported by the worker itself.
    fatal: Mutex<Option<String>>,
    /// Activity and `activity.busy` locks.
    pub activity: Activity,
    /// Why Shell is stopping the app (for the log and `last_exit`).
    stop_reason: Mutex<Option<StopReason>>,
}

impl Worker {
    pub fn send(&self, msg: ToWorker) {
        let _ = self.tx.send(msg);
    }

    pub fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Relaxed) > 0
    }

    /// Accounts for a visibility change of one UI tab.
    #[cfg(feature = "ui-server")]
    pub fn set_visible(&self, was: bool, now: bool) {
        if was == now {
            return;
        }
        self.activity.touch();
        // Going between 0 and 1 visible tabs changes the app's state.
        let changed = if now {
            self.visible.fetch_add(1, Ordering::Relaxed) == 0
        } else {
            self.visible.fetch_sub(1, Ordering::Relaxed) == 1
        };
        if changed {
            self.log.write("shell", if now { "state: running (UI visible)" } else { "state: suspended (UI hidden)" });
            self.send(ToWorker::Event(ipc::HostEvent::UiVisible(now)));
        }
    }

    pub fn metrics(&self) -> Option<Metrics> {
        match &self.cgroup {
            Some(cg) => Some(cg.metrics()),
            None => crate::cgroup::proc_metrics(self.pid),
        }
    }

    fn kill(&self) {
        match &self.cgroup {
            Some(cg) => cg.destroy(),
            None => crate::process::kill(self.pid),
        }
    }
}

struct RateLimiter {
    per_s: u32,
    tokens: f64,
    last: Instant,
    dropped: u64,
}

impl RateLimiter {
    fn allow(&mut self) -> bool {
        if self.per_s == 0 {
            return true;
        }
        let now = Instant::now();
        let refill = now.duration_since(self.last).as_secs_f64() * f64::from(self.per_s);
        self.tokens = (self.tokens + refill).min(f64::from(self.per_s));
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            self.dropped += 1;
            false
        }
    }
}

impl Supervisor {
    pub fn new(cfg: Config) -> Result<Arc<Self>> {
        let paths = Paths { root: cfg.data_dir() };
        paths.create().with_context(|| format!("data directory {}", paths.root.display()))?;
        let engine = crate::engine::engine()?;

        let cgroups = match cfg.cgroups.mode {
            CgroupMode::Off => None,
            mode => match Cgroups::init() {
                Ok(cg) => Some(cg),
                Err(e) if mode == CgroupMode::Auto => {
                    tracing::warn!("cgroups unavailable, OS limits are not applied: {e:#}");
                    None
                }
                Err(e) => return Err(e.context("cgroups.mode = required")),
            },
        };

        let mut apps = BTreeMap::new();
        for (id, loaded) in package::load_all(&paths) {
            match loaded {
                Ok(installed) => {
                    let log = Arc::new(AppLog::new(paths.log(&id)));
                    apps.insert(id, App::new(installed, log));
                }
                Err(e) => tracing::error!("app {id} is corrupted, skipped: {e:#}"),
            }
        }
        tracing::info!("installed apps: {}", apps.len());

        // A key that does not load is a configuration error: refusing to start beats
        // installing unsigned packages.
        let trusted_keys = cfg
            .registry
            .keys
            .iter()
            .map(|k| crate::signature::load_key(k))
            .collect::<Result<Vec<_>>>()
            .context("registry.keys")?;
        if !trusted_keys.is_empty() {
            tracing::info!("registry packages must be signed by one of {} trusted key(s)", trusted_keys.len());
        }
        let http_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("wshell/", env!("CARGO_PKG_VERSION")))
            .build()?;

        Ok(Arc::new(Supervisor {
            audit: Audit::new(&paths.audit()),
            cfg,
            paths,
            engine,
            http_client,
            registry_client: crate::oci::client()?,
            trusted_keys,
            cgroups,
            apps: Mutex::new(apps),
            tokens: Mutex::default(),
            sessions: Mutex::default(),
            next_job: AtomicU64::new(1),
            install_lock: tokio::sync::Mutex::new(()),
            reconcile_lock: tokio::sync::Mutex::new(()),
            dbus: tokio::sync::OnceCell::new(),
            system_bus: tokio::sync::OnceCell::new(),
            started_at: unix_now(),
            state_changed: tokio::sync::Notify::new(),
        }))
    }

    pub fn next_job_id(&self) -> u64 {
        self.next_job.fetch_add(1, Ordering::Relaxed)
    }

    #[cfg(any(feature = "ui-server", feature = "dashboard"))]
    pub fn is_installed(&self, id: &str) -> bool {
        self.apps.lock().unwrap().contains_key(id)
    }

    pub fn installed(&self, id: &str) -> Option<Arc<Installed>> {
        self.apps.lock().unwrap().get(id).map(|a| a.installed.clone())
    }

    pub fn worker(&self, id: &str) -> Option<Arc<Worker>> {
        self.apps.lock().unwrap().get(id).and_then(|a| a.worker.clone())
    }

    /// `metrics.publish`: validates and stores the app's snapshot. Accepted even with
    /// `/metrics` disabled (nothing reads it then), so an app behaves the same everywhere.
    pub fn publish_metrics(&self, id: &str, text: &str) -> Result<(), String> {
        let limits = shell_core::metrics::Limits {
            max_series: self.cfg.metrics.max_series_per_app,
            max_bytes: self.cfg.metrics.max_snapshot_kb * 1024,
        };
        let snapshot = shell_core::metrics::parse(text, &limits)?;
        let dropped = snapshot.dropped;
        let published = Arc::new(PublishedMetrics { snapshot, at: unix_now() });
        if let Some(app) = self.apps.lock().unwrap().get_mut(id) {
            app.metrics = Some(published);
        }
        match dropped {
            0 => Ok(()),
            n => Err(format!("stored without {n} series over the limit of {}", limits.max_series)),
        }
    }

    /// Published snapshots by app id.
    #[cfg(feature = "metrics")]
    pub fn metrics_snapshots(&self) -> Vec<(String, Arc<PublishedMetrics>)> {
        let apps = self.apps.lock().unwrap();
        apps.iter().filter_map(|(id, a)| Some((id.clone(), a.metrics.clone()?))).collect()
    }

    pub fn app_log(&self, id: &str) -> Option<Arc<AppLog>> {
        self.apps.lock().unwrap().get(id).map(|a| a.log.clone())
    }

    pub fn list(&self) -> Vec<AppStatus> {
        let max_busy = self.max_busy();
        let snapshot: Vec<(AppStatus, Option<Arc<Worker>>)> = {
            let apps = self.apps.lock().unwrap();
            apps.iter()
                .map(|(id, a)| {
                    let m = &a.installed.manifest;
                    // Suspended means "UI hidden"; a program (kind = "command") has no Shell UI.
                    let state = match (&a.state, &a.worker) {
                        (AppState::Running, Some(w)) if !w.is_visible() && m.backend.kind.has_bridge() => {
                            AppState::Suspended
                        }
                        (s, _) => *s,
                    };
                    let (priority, idle) = self.effective_lifecycle(&a.installed);
                    let status = AppStatus {
                        id: id.clone(),
                        name: m.app.name.clone(),
                        version: m.app.version.clone(),
                        state,
                        pid: a.worker.as_ref().map(|w| w.pid),
                        permissions: a.installed.permission_infos(),
                        has_ui: m.ui.is_some(),
                        started_at: a.worker.as_ref().map(|w| w.started_at),
                        starts: a.starts,
                        failures: a.failures,
                        last_exit: a.last_exit.clone(),
                        memory_limit_mb: self.memory_limit_mb(&a.installed),
                        metrics: None,
                        hints: m.lifecycle,
                        priority,
                        idle,
                        busy: a.worker.as_ref().map(|w| w.activity.busy_info(max_busy)).unwrap_or_default(),
                        last_activity: a.worker.as_ref().map(|w| w.activity.last_unix()),
                        evict_rank: None,
                        on_demand: self.on_demand(id) && m.backend.kind.has_bridge(),
                        kind: m.backend.kind,
                        ports: a.installed.grants.listen.clone(),
                        previous_version: a.installed.previous.clone(),
                        rolled_back: a.installed.rolled_back.clone(),
                    };
                    (status, a.worker.clone())
                })
                .collect()
        };
        // Metrics mean reading cgroup//proc files: do it outside the registry lock.
        let mut statuses: Vec<AppStatus> = snapshot
            .into_iter()
            .map(|(mut status, worker)| {
                status.metrics = worker.and_then(|w| w.metrics());
                status
            })
            .collect();

        // Position in the eviction queue: same key as under memory pressure.
        let mut queue: Vec<(_, usize)> = statuses
            .iter()
            .enumerate()
            .filter(|(_, s)| matches!(s.state, AppState::Running | AppState::Suspended))
            .map(|(i, s)| {
                let busy = s.busy.iter().any(|b| !b.expired);
                let memory = s.metrics.as_ref().map_or(0, |m| m.memory_bytes);
                (eviction_key(s.priority, s.state == AppState::Running, busy, memory), i)
            })
            .collect();
        queue.sort_by_key(|q| q.0);
        for (rank, (_, i)) in queue.into_iter().enumerate() {
            statuses[i].evict_rank = Some(rank as u32 + 1);
        }
        statuses
    }

    /// Running apps that are not currently being stopped.
    pub(crate) fn running_workers(&self) -> Vec<Arc<Worker>> {
        let apps = self.apps.lock().unwrap();
        apps.values().filter(|a| a.state == AppState::Running).filter_map(|a| a.worker.clone()).collect()
    }

    #[cfg(any(feature = "dashboard", feature = "metrics"))]
    pub fn shell_info(&self) -> shell_core::control::ShellInfo {
        let ui = self.cfg.features.ui_server && cfg!(feature = "ui-server");
        shell_core::control::ShellInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            profile: self.cfg.profile.name.clone(),
            cgroups: self.cgroups.is_some(),
            started_at: self.started_at,
            ui_port: ui.then_some(self.cfg.http.port),
            cpu_max_percent: self.cfg.profile.cpu_max_percent,
            memory_mb: self.cfg.profile.memory_mb,
            update_probation_s: self.cfg.update.probation_s,
        }
    }

    /// WASM memory limit: the minimum of the profile, the policy and the app's request.
    fn memory_limit_mb(&self, installed: &Installed) -> u32 {
        let profile = self.cfg.profile.memory_mb;
        let policy = self.cfg.policy(&installed.manifest.app.id).memory_mb;
        [Some(profile), policy, installed.manifest.resources.memory_mb].into_iter().flatten().min().unwrap_or(profile)
    }

    /// A local package file or directory for the source, and the source pinned to what was
    /// read: a registry package is pulled (or taken from the cache).
    /// Also the trusted key that signed a registry package (when keys are configured).
    pub async fn fetch(&self, source: &Source) -> Result<(PathBuf, Source, Option<String>)> {
        match source {
            Source::Path(p) => Ok((p.clone(), source.clone(), None)),
            Source::Oci(r) => {
                let cache = self.paths.registry_cache();
                let pulled =
                    crate::oci::pull(&self.registry_client, &self.cfg.registry, &self.trusted_keys, &cache, r).await?;
                if r.digest.is_none() {
                    tracing::info!("{r} is {}", pulled.pinned);
                }
                let signer = pulled.signer.map(|k| k.display().to_string());
                Ok((pulled.path, Source::Oci(pulled.pinned), signer))
            }
        }
    }

    pub async fn inspect(self: &Arc<Self>, source: Source) -> Result<PackageInfo> {
        let (path, source, signed_by) = self.fetch(&source).await?;
        let this = self.clone();
        let staged = tokio::task::spawn_blocking(move || package::stage(&path, &this.paths, &this.cfg, &this.engine))
            .await??;
        let current = self.installed(&staged.manifest.app.id);
        let mut info = package::package_info(&staged, source, current.as_deref());
        info.signed_by = signed_by;
        Ok(info)
    }

    pub async fn install(self: &Arc<Self>, source: Source, grant_optional: Vec<String>) -> Result<(String, String)> {
        let fetched = self.fetch(&source).await?;
        self.install_fetched(fetched, grant_optional, None).await
    }

    /// What `fetch` returned. `expect`: the app the package must hold, as a desired-state plan
    /// read it (a local file may have changed since).
    pub async fn install_fetched(
        self: &Arc<Self>,
        (path, source, signed_by): (PathBuf, Source, Option<String>),
        grant_optional: Vec<String>,
        expect: Option<String>,
    ) -> Result<(String, String)> {
        let _guard = self.install_lock.lock().await;
        // Checked and compiled while the installed version still runs.
        let this = self.clone();
        let staged = tokio::task::spawn_blocking(move || -> Result<package::Staged> {
            let staged = package::stage(&path, &this.paths, &this.cfg, &this.engine)?;
            let id = &staged.manifest.app.id;
            if let Some(expect) = expect
                && expect != *id
            {
                bail!("the package now holds app {id}, not {expect}");
            }
            this.check_ports(&staged.manifest)?;
            Ok(staged)
        })
        .await??;
        let id = staged.manifest.app.id.clone();
        let version = staged.manifest.app.version.clone();

        // An update: the installed version is set aside, the new one tried and rolled back
        // if it fails.
        let old = self.installed(&id).map(|i| i.manifest.app.version.clone());
        let _updating = old.is_some().then(|| self.begin_update(&id));
        let was_running = self.worker(&id).is_some();
        if was_running {
            self.app_log(&id).inspect(|l| l.write("shell", &format!("stopping for the update to {version}")));
            self.stop(&id, StopReason::Update, false).await?;
        }
        let this = self.clone();
        let installed = tokio::task::spawn_blocking(move || {
            // An install clears a rollback's mark; a failure below sets it again.
            package::set_rolled_back(&this.paths, &staged.manifest.app.id, None)?;
            package::install(&staged, &grant_optional, &this.paths, &this.engine, &this.audit)
        })
        .await??;
        let log = self.put_installed(installed);
        log.write("shell", &format!("installed version {version} from {source}"));
        if let Source::Oci(_) = source {
            // Which artifact exactly: the digest, not a tag that may move.
            let signed = signed_by.map(|k| format!(", signed by {k}")).unwrap_or_default();
            self.audit.record(&id, "source", &format!("{source}{signed}"));
        }

        if let Some(old) = old
            && let Err(e) = self.try_version(&id, was_running).await
        {
            log.write("shell", &format!("update to {version} failed: {e:#}; rolling back to {old}"));
            let restored = self.switch_back_after_failure(&id, &version, was_running).await;
            self.audit.record(&id, "rollback", &format!("{version} → {old}: {e:#}"));
            return Err(match restored {
                Ok(()) => anyhow!("update to {version} failed: {e:#}; rolled back to {old}"),
                Err(r) => anyhow!("update to {version} failed: {e:#}; rolling back to {old} failed too: {r:#}"),
            });
        }
        Ok((id, version))
    }

    /// Back to the version set aside, its data included; the failed one is dropped and
    /// remembered, so the desired state does not install it again.
    async fn switch_back_after_failure(self: &Arc<Self>, id: &str, failed: &str, start: bool) -> Result<()> {
        let (paths, id_owned, failed_owned) = (self.paths.clone(), id.to_string(), failed.to_string());
        tokio::task::spawn_blocking(move || -> Result<()> {
            package::swap_prev(&paths, &id_owned, true)?;
            package::drop_prev(&paths, &id_owned);
            package::set_rolled_back(&paths, &id_owned, Some(&failed_owned))
        })
        .await??;
        self.reload(id).await?;
        if start {
            self.start_inner(id, true).await?;
        }
        Ok(())
    }

    /// `shellctl rollback`: the installed version and the one set aside swap places (with
    /// their data unless `keep_data`), so rolling back again returns. The version rolled
    /// back from is remembered: the desired state does not install it again.
    pub async fn rollback(self: &Arc<Self>, id: &str, keep_data: bool) -> Result<(String, String)> {
        let _guard = self.install_lock.lock().await;
        let installed = self.installed(id).with_context(|| format!("app {id} is not installed"))?;
        let from = installed.manifest.app.version.clone();
        let to = installed.previous.clone().with_context(|| format!("{id} has no previous version"))?;
        let _updating = self.begin_update(id);
        let was_running = self.worker(id).is_some();
        if was_running {
            self.stop(id, StopReason::Update, false).await?;
        }
        // Set before the switch, which reads the app again (not after: it may be running then).
        package::set_rolled_back(&self.paths, id, Some(&from))?;
        self.swap_prev(id, !keep_data).await?;
        if let Err(e) = self.try_version(id, was_running).await {
            package::set_rolled_back(&self.paths, id, installed.rolled_back.as_deref())?;
            self.swap_prev(id, !keep_data).await?;
            if was_running {
                self.start_inner(id, true).await?;
            }
            bail!("{to} did not run: {e:#}; {from} is kept");
        }
        let data = if keep_data { ", data kept" } else { "" };
        self.audit.record(id, "rollback", &format!("{from} → {to}{data}"));
        if let Some(log) = self.app_log(id) {
            log.write("shell", &format!("rolled back from {from} to {to}{data}"));
        }
        Ok((from, to))
    }

    async fn swap_prev(self: &Arc<Self>, id: &str, data: bool) -> Result<()> {
        let (paths, id_owned) = (self.paths.clone(), id.to_string());
        tokio::task::spawn_blocking(move || package::swap_prev(&paths, &id_owned, data)).await??;
        self.reload(id).await
    }

    /// Reads the installed app again after its files changed (it is stopped).
    async fn reload(self: &Arc<Self>, id: &str) -> Result<()> {
        let (paths, id_owned) = (self.paths.clone(), id.to_string());
        let installed = tokio::task::spawn_blocking(move || Installed::load(&paths, &id_owned)).await??;
        self.put_installed(installed);
        Ok(())
    }

    /// The app's new files in the registry, keeping its counters and log.
    fn put_installed(&self, installed: Installed) -> Arc<AppLog> {
        let id = installed.manifest.app.id.clone();
        let mut apps = self.apps.lock().unwrap();
        let log = apps.get(&id).map(|a| a.log.clone()).unwrap_or_else(|| Arc::new(AppLog::new(self.paths.log(&id))));
        let mut app = App::new(installed, log.clone());
        if let Some(old) = apps.get(&id) {
            (app.starts, app.failures, app.last_exit) = (old.starts, old.failures, old.last_exit.clone());
            app.updating = old.updating;
        }
        apps.insert(id, app);
        log
    }

    /// Holds other starts off while versions are switched; calls wait for the end.
    fn begin_update(self: &Arc<Self>, id: &str) -> UpdateGuard {
        if let Some(app) = self.apps.lock().unwrap().get_mut(id) {
            app.updating = true;
        }
        UpdateGuard { sup: self.clone(), id: id.to_string() }
    }

    /// Starts the version just switched to and watches it for `[update] probation_s`: it must
    /// keep running and its `net.listen` ports must accept connections. An app that was not
    /// running is stopped again afterwards.
    async fn try_version(self: &Arc<Self>, id: &str, keep_running: bool) -> Result<()> {
        let result = self.trial(id).await;
        if self.worker(id).is_some() && (result.is_err() || !keep_running) {
            self.stop(id, StopReason::Update, result.is_err()).await?;
        }
        result
    }

    async fn trial(self: &Arc<Self>, id: &str) -> Result<()> {
        self.start_inner(id, true).await?;
        let worker = self.worker(id).context("not running after its start")?;
        let probation = Duration::from_secs(self.cfg.update.probation_s.into());
        let deadline = tokio::time::Instant::now() + probation;
        let mut waiting: Vec<u16> = if probation.is_zero() { Vec::new() } else { listen_ports(&worker.installed.manifest) };
        let mut exited = worker.exited.clone();
        loop {
            let mut still = Vec::new();
            for port in waiting {
                if !accepts(port).await {
                    still.push(port);
                }
            }
            waiting = still;
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            let pause = if waiting.is_empty() { deadline - now } else { Duration::from_millis(250).min(deadline - now) };
            if tokio::time::timeout(pause, exited.wait_for(|e| *e)).await.is_ok() {
                let why = self.apps.lock().unwrap().get(id).and_then(|a| a.last_exit.clone()).unwrap_or_default();
                bail!("it exited during the trial run: {why}");
            }
        }
        ensure!(
            waiting.is_empty(),
            "port {} not accepting connections after {} s",
            waiting.iter().map(u16::to_string).collect::<Vec<_>>().join(", "),
            probation.as_secs()
        );
        Ok(())
    }

    /// `net.listen` ports must not collide with Shell's ports or other apps' ports.
    fn check_ports(&self, manifest: &shell_core::manifest::Manifest) -> Result<()> {
        let id = &manifest.app.id;
        let mut taken: Vec<(u16, String)> = vec![(self.cfg.http.port, "Shell UI server".into())];
        if self.cfg.dashboard.enabled {
            taken.push((self.cfg.dashboard.port, "Shell dashboard".into()));
        }
        for (other, app) in self.apps.lock().unwrap().iter().filter(|(other, _)| *other != id) {
            taken.extend(listen_ports(&app.installed.manifest).into_iter().map(|p| (p, other.clone())));
        }
        for port in listen_ports(manifest) {
            if let Some((_, owner)) = taken.iter().find(|(p, _)| *p == port) {
                bail!("port {port} is already taken: {owner}");
            }
        }
        Ok(())
    }

    pub async fn uninstall(&self, id: &str) -> Result<()> {
        let _guard = self.install_lock.lock().await;
        {
            let apps = self.apps.lock().unwrap();
            let app = apps.get(id).with_context(|| format!("app {id} is not installed"))?;
            if app.worker.is_some() {
                bail!("app {id} is running; stop it first");
            }
        }
        let (paths, app_id) = (self.paths.clone(), id.to_string());
        tokio::task::spawn_blocking(move || {
            package::uninstall(&paths, &app_id)?;
            let _ = std::fs::remove_file(paths.log(&app_id));
            anyhow::Ok(())
        })
        .await??;
        self.apps.lock().unwrap().remove(id);
        let scope = Scope::App(id.to_string());
        self.sessions.lock().unwrap().retain(|_, s| *s != scope);
        self.tokens.lock().unwrap().retain(|_, (s, _)| *s != scope);
        self.audit.record(id, "uninstall", "-");
        Ok(())
    }

    pub async fn start(self: &Arc<Self>, id: &str) -> Result<()> {
        self.start_inner(id, false).await
    }

    /// `during_update`: the update's own trial start.
    async fn start_inner(self: &Arc<Self>, id: &str, during_update: bool) -> Result<()> {
        let (installed, log) = {
            let mut apps = self.apps.lock().unwrap();
            let app = apps.get_mut(id).with_context(|| format!("app {id} is not installed"))?;
            if app.updating && !during_update {
                bail!("app {id} is being updated");
            }
            match app.state {
                AppState::Stopped | AppState::Failed => {}
                s => bail!("app {id} is already in state {s}"),
            }
            app.state = AppState::Starting;
            (app.installed.clone(), app.log.clone())
        };

        match self.spawn_worker(installed, log.clone()).await {
            Ok(worker) => {
                let mut apps = self.apps.lock().unwrap();
                if let Some(app) = apps.get_mut(id) {
                    if *worker.exited.borrow() {
                        app.state = AppState::Failed;
                        app.failures += 1;
                        app.last_exit_at = Some(Instant::now());
                        drop(apps);
                        self.state_changed.notify_waiters();
                        bail!("app {id} exited right after starting");
                    }
                    app.state = AppState::Running;
                    app.worker = Some(worker.clone());
                    app.starts += 1;
                }
                drop(apps);
                self.state_changed.notify_waiters();
                log.write("shell", &format!("started, pid {}", worker.pid));
                self.spawn_idle_watch(worker);
                Ok(())
            }
            Err(e) => {
                if let Some(app) = self.apps.lock().unwrap().get_mut(id) {
                    app.state = AppState::Failed;
                    app.failures += 1;
                    app.last_exit = Some(format!("start failed: {e:#}"));
                    app.last_exit_at = Some(Instant::now());
                }
                self.state_changed.notify_waiters();
                log.write("shell", &format!("start failed: {e:#}"));
                Err(e)
            }
        }
    }

    async fn spawn_worker(self: &Arc<Self>, installed: Arc<Installed>, log: Arc<AppLog>) -> Result<Arc<Worker>> {
        let id = installed.manifest.app.id.clone();
        // Installed while allowed, then disabled in the config: such a plugin does not run.
        if installed.manifest.backend.kind == BackendKind::Plugin && !self.cfg.features.plugins {
            bail!("plugins are disabled by the device configuration ([features] plugins = false)");
        }
        // The AOT image is checked and rebuilt if needed before the worker starts:
        // recompilation may take long and must not eat into the start timeout.
        let (cwasm_sha256, rebuilt) = {
            let (this, installed) = (self.clone(), installed.clone());
            tokio::task::spawn_blocking(move || package::ensure_cwasm(&installed, &this.engine, this.cfg.shell.compile_threads)).await??
        };
        if rebuilt {
            log.write("shell", "AOT image corrupted or built by another engine version; recompiled");
        }
        let profile = &self.cfg.profile;
        let memory_bytes = u64::from(self.memory_limit_mb(&installed)) << 20;

        let cgroup = match &self.cgroups {
            Some(cg) => Some(cg.create_app(
                &id,
                Limits {
                    memory_bytes: memory_bytes + (u64::from(profile.runtime_overhead_mb) << 20),
                    cpu_percent: profile.cpu_max_percent,
                    pids_max: profile.pids_max,
                },
            )?),
            None => None,
        };

        // IPC goes over the worker's stdin/stdout, guest output over stderr.
        let mut worker_cmd = tokio::process::Command::new(std::env::current_exe()?);
        worker_cmd
            .arg("worker")
            .args(["--sandbox", self.cfg.sandbox.mode.as_str()])
            .arg("--pkg")
            .arg(&installed.pkg_dir);
        for port in &installed.grants.listen {
            worker_cmd.args(["--listen", &port.to_string()]);
        }
        let mut child_proc = worker_cmd
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("starting worker process")?;
        let pid = child_proc.id().unwrap_or(0);
        // The worker does nothing until it receives Init, so moving it into the cgroup after
        // starting leaves no window in which it would run without limits.
        if let Some(cg) = &cgroup
            && let Err(e) = cg.add(pid)
        {
            crate::process::kill(pid);
            return Err(e.context("moving worker into the app's cgroup"));
        }
        let mut wr = child_proc.stdin.take().context("worker stdin")?;
        let mut rd = child_proc.stdout.take().context("worker stdout")?;

        let (tx, mut rx) = mpsc::unbounded_channel::<ToWorker>();
        let (exited_tx, exited_rx) = watch::channel(false);
        let (started_tx, started_rx) = oneshot::channel();
        let (events, _) = broadcast::channel(64);
        let worker = Arc::new(Worker {
            app_id: id.clone(),
            installed: installed.clone(),
            log: log.clone(),
            pid,
            started_at: unix_now(),
            events,
            exited: exited_rx,
            cgroup,
            jobs: Mutex::default(),
            active_exec: AtomicUsize::new(0),
            tx,
            pending: Mutex::default(),
            started: Mutex::new(Some(started_tx)),
            next_call: AtomicU64::new(1),
            visible: AtomicUsize::new(0),
            fatal: Mutex::new(None),
            activity: Activity::new(),
            stop_reason: Mutex::new(None),
            rate: Mutex::new(RateLimiter {
                per_s: profile.bridge_events_per_s,
                tokens: f64::from(profile.bridge_events_per_s),
                last: Instant::now(),
                dropped: 0,
            }),
        });

        // Writing to the socket.
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let Ok(frame) = ipc::encode(&msg) else { continue };
                if wr.write_all(&frame).await.is_err() {
                    break;
                }
            }
        });

        // Backend output (guest stdout and stderr, worker panics) → app log.
        log_lines(child_proc.stderr.take().context("worker stderr")?, log.clone(), "stderr");

        // Reading worker messages.
        {
            let sup = self.clone();
            let worker = worker.clone();
            tokio::spawn(async move {
                while let Ok(msg) = ipc::read_async::<FromWorker>(&mut rd).await {
                    sup.on_message(&worker, msg);
                }
            });
        }

        // Waiting for the process to exit.
        {
            let sup = self.clone();
            let worker = worker.clone();
            tokio::spawn(async move {
                let status = child_proc.wait().await;
                sup.on_worker_exit(&worker, status);
                let _ = exited_tx.send(true);
            });
        }

        let g = &installed.grants;
        worker.send(ToWorker::Init(Init {
            wasm_path: installed.wasm_path().to_string_lossy().into_owned(),
            cwasm_path: installed.cwasm_path().to_string_lossy().into_owned(),
            cwasm_sha256,
            memory_bytes,
            fuel_per_call: profile.fuel_per_call,
            kind: installed.manifest.backend.kind,
            grants: ipc::Grants {
                storage: g.storage_quota_mb.is_some(),
                http: g.http.is_some(),
                exec: g.exec.keys().cloned().collect(),
                listen: g.listen.clone(),
                apps: g.apps,
                services: g.services.is_some(),
                network: g.network.is_some(),
            },
        }));

        let deadline = Duration::from_millis(profile.hook_timeout_ms) + STARTUP_GRACE;
        match tokio::time::timeout(deadline, started_rx).await {
            Ok(Ok(Ok(()))) => Ok(worker),
            Ok(Ok(Err(e))) => {
                worker.kill();
                bail!("on-start: {e}")
            }
            Ok(Err(_)) => bail!("worker exited during startup"),
            Err(_) => {
                worker.kill();
                bail!("start timed out ({} ms)", deadline.as_millis())
            }
        }
    }

    fn on_message(self: &Arc<Self>, worker: &Arc<Worker>, msg: FromWorker) {
        match msg {
            FromWorker::Started(r) => {
                if let Some(tx) = worker.started.lock().unwrap().take() {
                    let _ = tx.send(r);
                }
            }
            FromWorker::CallResult { id, result } => {
                if let Some(tx) = worker.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(result);
                }
            }
            FromWorker::HostCall { id, call } => {
                let sup = self.clone();
                let worker = worker.clone();
                tokio::spawn(async move {
                    let result = crate::hostsvc::handle(&sup, &worker, call).await;
                    worker.send(ToWorker::HostReply { id, result });
                });
            }
            FromWorker::Emit { name, payload } => {
                if !worker.is_visible() {
                    return;
                }
                let mut rate = worker.rate.lock().unwrap();
                if rate.allow() {
                    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap_or(serde_json::Value::String(payload));
                    let frame = serde_json::json!({ "event": name, "payload": payload }).to_string();
                    let _ = worker.events.send(frame.into());
                } else if rate.dropped.is_power_of_two() {
                    worker.log.write("shell", &format!("UI event limit exceeded, dropped: {}", rate.dropped));
                }
            }
            FromWorker::Log { level, message } => worker.log.write(level.as_str(), &message),
            FromWorker::Fatal(reason) => *worker.fatal.lock().unwrap() = Some(reason),
            FromWorker::Busy { id, reason } => {
                worker.log.write("shell", &format!("busy: {reason}"));
                worker.activity.busy_begin(id, reason);
            }
            FromWorker::BusyDone { id } => worker.activity.busy_end(id),
            FromWorker::RequestStop => {
                let sup = self.clone();
                let id = worker.app_id.clone();
                // The app itself asks to stop, so there is no point waiting for its locks.
                tokio::spawn(async move {
                    if let Err(e) = sup.stop(&id, StopReason::SelfRequested, true).await {
                        tracing::debug!("stopping {id} at the app's request: {e:#}");
                    }
                });
            }
        }
    }

    fn on_worker_exit(&self, worker: &Arc<Worker>, status: std::io::Result<std::process::ExitStatus>) {
        for (_, tx) in worker.pending.lock().unwrap().drain() {
            let _ = tx.send(Err("app stopped".into()));
        }
        for (_, kill) in worker.jobs.lock().unwrap().drain() {
            let _ = kill.send(());
        }
        let oom = worker.cgroup.as_ref().is_some_and(AppCgroup::oom_killed);
        if let Some(cg) = &worker.cgroup {
            cg.destroy();
        }

        let fatal = worker.fatal.lock().unwrap().take();
        let stop_reason = worker.stop_reason.lock().unwrap().take();
        let reason = match &status {
            _ if oom => "memory limit exceeded (OOM)".to_string(),
            Ok(s) if fatal.is_some() => format!("{} ({s})", fatal.unwrap()),
            Ok(s) if stop_reason.is_some() => format!("stopped: {} ({s})", stop_reason.unwrap()),
            Ok(s) => s.to_string(),
            Err(e) => e.to_string(),
        };
        worker.log.write("shell", &format!("process exited: {reason}"));

        let mut apps = self.apps.lock().unwrap();
        if let Some(app) = apps.get_mut(&worker.app_id)
            && app.worker.as_ref().is_some_and(|w| Arc::ptr_eq(w, worker))
        {
            // Stop on command: `stop()` switches to Stopping beforehand.
            let clean = status.as_ref().is_ok_and(|s| s.success());
            if app.state == AppState::Stopping || clean {
                // Stop on command or normal program exit (exit 0).
                app.state = AppState::Stopped;
            } else {
                app.state = AppState::Failed;
                app.failures += 1;
                tracing::warn!("app {} terminated abnormally: {reason}", worker.app_id);
            }
            app.worker = None;
            app.last_exit = Some(reason);
            app.last_exit_at = Some(Instant::now());
        }
        drop(apps);
        self.state_changed.notify_waiters();
    }

    /// Graceful stop. If the app is busy (`activity.busy`) and not `force`,
    /// it gets `stop-requested` and up to `stop_grace_ms` to finish its work;
    /// then `on-stop` with a hard timeout and forced termination.
    pub async fn stop(&self, id: &str, reason: StopReason, force: bool) -> Result<()> {
        let (worker, already_stopping) = {
            let mut apps = self.apps.lock().unwrap();
            let app = apps.get_mut(id).with_context(|| format!("app {id} is not installed"))?;
            let Some(worker) = app.worker.clone() else {
                if app.state == AppState::Failed {
                    app.state = AppState::Stopped;
                    return Ok(());
                }
                bail!("app {id} is not running");
            };
            let already = app.state == AppState::Stopping;
            app.state = AppState::Stopping;
            (worker, already)
        };
        if already_stopping {
            // Already stopping: wait for the same termination.
            let _ = worker.exited.clone().wait_for(|e| *e).await;
            return Ok(());
        }
        *worker.stop_reason.lock().unwrap() = Some(reason);
        worker.log.write("shell", &format!("stopping: {reason}"));

        let busy = worker.activity.busy_reasons(self.max_busy());
        if !force && !busy.is_empty() {
            let grace_ms = self.cfg.lifecycle.stop_grace_ms;
            worker.log.write("shell", &format!("busy ({}): stop-requested, waiting up to {grace_ms} ms", busy.join("; ")));
            let grace = u32::try_from(grace_ms).unwrap_or(u32::MAX);
            worker.send(ToWorker::Event(HostEvent::StopRequested { reason, grace_ms: grace }));
            let deadline = tokio::time::Instant::now() + Duration::from_millis(grace_ms);
            if !worker.activity.wait_free(self.max_busy(), deadline).await {
                worker.log.write("shell", "app did not become free in the allotted time");
            }
        }

        let mut exited = worker.exited.clone();
        if worker.installed.manifest.backend.kind == BackendKind::Command {
            // A program has no on-stop hook (WASI has no signals): terminate right away.
            worker.kill();
            let _ = exited.wait_for(|e| *e).await;
            return Ok(());
        }
        worker.send(ToWorker::Stop);
        let hook = Duration::from_millis(self.cfg.profile.hook_timeout_ms);
        if tokio::time::timeout(hook, exited.wait_for(|e| *e)).await.is_err() {
            worker.log.write("shell", "on-stop timed out, forcing stop");
            worker.kill();
            let _ = exited.wait_for(|e| *e).await;
        }
        Ok(())
    }

    #[cfg(feature = "dashboard")]
    pub async fn restart(self: &Arc<Self>, id: &str) -> Result<()> {
        if self.worker(id).is_some() {
            self.stop(id, StopReason::User, false).await?;
        }
        self.start(id).await
    }

    /// Stops all apps in parallel: total time is one `hook_timeout`, not N.
    pub async fn stop_all(self: &Arc<Self>) {
        let ids: Vec<String> = {
            let apps = self.apps.lock().unwrap();
            apps.iter().filter(|(_, a)| a.worker.is_some()).map(|(id, _)| id.clone()).collect()
        };
        let mut tasks = tokio::task::JoinSet::new();
        for id in ids {
            let sup = self.clone();
            tasks.spawn(async move {
                if let Err(e) = sup.stop(&id, StopReason::Shutdown, false).await {
                    tracing::warn!("stopping {id}: {e:#}");
                }
            });
        }
        tasks.join_all().await;
    }

    pub async fn autostart(self: &Arc<Self>) {
        let ids: Vec<String> = self.apps.lock().unwrap().keys().cloned().collect();
        for id in ids.into_iter().filter(|id| self.cfg.policy(id).autostart) {
            if let Err(e) = self.start(&id).await {
                tracing::error!("autostart {id}: {e:#}");
            }
        }
    }

    /// Whether cold start is allowed for the app.
    pub fn on_demand(&self, id: &str) -> bool {
        self.cfg.policy(id).on_demand.unwrap_or(self.cfg.lifecycle.start_on_demand)
    }

    /// The app's running worker; a stopped app is started (cold start).
    ///
    /// Concurrent calls wait for a single start. A crashed app is woken no sooner
    /// than 2^failures seconds (at most `MAX_RESTART_BACKOFF`) after the crash.
    pub async fn ensure_running(self: &Arc<Self>, id: &str, trigger: &str) -> Result<Arc<Worker>, CallError> {
        enum Next {
            Start,
            Wait,
        }
        if !self.on_demand(id) {
            return self.worker(id).ok_or(CallError::NotRunning);
        }
        let deadline = tokio::time::Instant::now() + COLD_START_WAIT;
        loop {
            // Subscribe before checking the state so as not to miss a transition.
            let changed = self.state_changed.notified();
            let next = {
                let apps = self.apps.lock().unwrap();
                let app = apps.get(id).ok_or(CallError::NotRunning)?;
                match (app.state, &app.worker) {
                    (AppState::Running, Some(w)) => return Ok(w.clone()),
                    _ if app.updating => Next::Wait,
                    (AppState::Stopped, _) => Next::Start,
                    (AppState::Failed, _) => {
                        let backoff = Duration::from_secs(1 << app.failures.min(9)).min(MAX_RESTART_BACKOFF);
                        let wait = app.last_exit_at.map_or(Duration::ZERO, |t| backoff.saturating_sub(t.elapsed()));
                        if !wait.is_zero() {
                            return Err(CallError::StartFailed(format!(
                                "app crashed, restart possible in {} s",
                                wait.as_secs().max(1)
                            )));
                        }
                        Next::Start
                    }
                    _ => Next::Wait,
                }
            };
            match next {
                Next::Start => {
                    if let Some(log) = self.app_log(id) {
                        log.write("shell", &format!("cold start: {trigger}"));
                    }
                    if let Err(e) = self.start(id).await {
                        // Lost the race to another start: wait for it; otherwise this is a real failure.
                        let failed = self.apps.lock().unwrap().get(id).is_none_or(|a| a.state == AppState::Failed);
                        if failed {
                            return Err(CallError::StartFailed(format!("{e:#}")));
                        }
                    }
                }
                Next::Wait => {
                    if tokio::time::timeout_at(deadline, changed).await.is_err() {
                        return Err(CallError::Timeout);
                    }
                }
            }
        }
    }

    /// Calls the backend's `bridge.handle`; a stopped app is woken by a cold start.
    pub async fn call(self: &Arc<Self>, id: &str, method: String, payload: String) -> Result<String, CallError> {
        if self.installed(id).is_some_and(|i| i.manifest.backend.kind == BackendKind::Command) {
            return Err(CallError::App("a program (kind = \"command\") has no bridge".into()));
        }
        let worker = match self.worker(id) {
            Some(w) => w,
            None => self.ensure_running(id, &format!("call {method}")).await?,
        };
        let limit = self.cfg.http.max_message_kb as usize * 1024;
        if payload.len() + method.len() > limit {
            return Err(CallError::TooLarge);
        }
        let call_id = worker.next_call.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            // The worker runs calls one by one: a UI that sends faster only grows the queue.
            let mut pending = worker.pending.lock().unwrap();
            if pending.len() >= self.cfg.profile.bridge_max_pending as usize {
                return Err(CallError::Busy);
            }
            pending.insert(call_id, tx);
        }
        worker.activity.touch();
        worker.send(ToWorker::Call { id: call_id, method, payload });
        let timeout = Duration::from_millis(self.cfg.profile.call_timeout_ms);
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(r))) if r.len() > limit => Err(CallError::TooLarge),
            Ok(Ok(r)) => r.map_err(CallError::App),
            Ok(Err(_)) => Err(CallError::NotRunning),
            Err(_) => {
                worker.pending.lock().unwrap().remove(&call_id);
                Err(CallError::Timeout)
            }
        }
    }

    // --- UI access: one-time tokens and sessions ---

    pub fn origin(&self, id: &str) -> String {
        format!("http://{id}.localhost:{}", self.cfg.http.port)
    }

    /// One-time URL for opening the app's UI.
    pub fn issue_url(&self, id: &str) -> Result<String> {
        let installed = self.installed(id).with_context(|| format!("app {id} is not installed"))?;
        if installed.manifest.ui.is_none() {
            bail!("app {id} has no UI");
        }
        if !self.cfg.features.ui_server || !cfg!(feature = "ui-server") {
            bail!("UI HTTP server is disabled");
        }
        let token = self.new_token(Scope::App(id.to_string()));
        Ok(format!("{}/_shell/auth?token={token}", self.origin(id)))
    }

    /// One-time dashboard URL.
    pub fn issue_dashboard_url(&self) -> Result<String> {
        if !self.cfg.dashboard.enabled || !cfg!(feature = "dashboard") {
            bail!("dashboard is disabled");
        }
        let token = self.new_token(Scope::Dashboard);
        Ok(format!("http://localhost:{}/_auth?token={token}", self.cfg.dashboard.port))
    }

    fn new_token(&self, scope: Scope) -> String {
        let token = crate::util::token();
        let mut tokens = self.tokens.lock().unwrap();
        let now = Instant::now();
        tokens.retain(|_, (_, exp)| *exp > now);
        tokens.insert(token.clone(), (scope, now + TOKEN_TTL));
        token
    }

    /// Exchanges a one-time token for a session with the same access scope.
    #[cfg(any(feature = "ui-server", feature = "dashboard"))]
    pub fn redeem_token(&self, scope: &Scope, token: &str) -> Option<String> {
        let (issued_for, exp) = self.tokens.lock().unwrap().remove(token)?;
        if issued_for != *scope || exp < Instant::now() {
            return None;
        }
        let session = crate::util::token();
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.values().filter(|s| *s == scope).count() >= MAX_SESSIONS_PER_SCOPE {
            // Evict an arbitrary old session of this scope.
            if let Some(old) = sessions.iter().find(|(_, s)| *s == scope).map(|(k, _)| k.clone()) {
                sessions.remove(&old);
            }
        }
        sessions.insert(session.clone(), scope.clone());
        Some(session)
    }

    #[cfg(any(feature = "ui-server", feature = "dashboard"))]
    pub fn session_valid(&self, scope: &Scope, session: &str) -> bool {
        self.sessions.lock().unwrap().get(session) == Some(scope)
    }
}

/// worker stdout/stderr lines → app log.
fn log_lines(pipe: impl AsyncRead + Unpin + Send + 'static, log: Arc<AppLog>, source: &'static str) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            log.write(source, &line);
        }
    });
}

/// Ports declared by the `net.listen` permission in the manifest.
struct UpdateGuard {
    sup: Arc<Supervisor>,
    id: String,
}

impl Drop for UpdateGuard {
    fn drop(&mut self) {
        if let Some(app) = self.sup.apps.lock().unwrap().get_mut(&self.id) {
            app.updating = false;
        }
        self.sup.state_changed.notify_waiters();
    }
}

/// Whether something accepts TCP connections on the port of this host.
async fn accepts(port: u16) -> bool {
    let connect = |ip: std::net::IpAddr| async move {
        let addr = std::net::SocketAddr::new(ip, port);
        matches!(tokio::time::timeout(Duration::from_millis(500), tokio::net::TcpStream::connect(addr)).await, Ok(Ok(_)))
    };
    connect(std::net::Ipv4Addr::LOCALHOST.into()).await || connect(std::net::Ipv6Addr::LOCALHOST.into()).await
}

fn listen_ports(manifest: &shell_core::manifest::Manifest) -> Vec<u16> {
    use shell_core::manifest::PermissionKind;
    manifest
        .permissions
        .iter()
        .filter_map(|p| match &p.kind {
            PermissionKind::NetListen { ports } => Some(ports.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}
