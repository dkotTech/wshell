//! Keeps the installed apps at the desired-state file (`[desired] file`): on start, when the
//! file changes, on `shellctl reconcile` and, after a pass with errors, again later. Not
//! continuously: an idle stop, an eviction or a manual command holds until the next pass.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use shell_core::control::{ReconcileReport, StopReason};
use shell_core::desired::{self, Current, DesiredState, Package, Step};
use shell_core::source::Source;

use crate::cgroup::EventsWatch;
use crate::supervisor::Supervisor;

/// Editors and `git` write a file in several steps: wait for them to finish.
const SETTLE: Duration = Duration::from_millis(300);
/// After a pass with errors (a registry out of reach at boot): again after this, doubling.
const RETRY_FIRST: Duration = Duration::from_secs(30);
const RETRY_MAX: Duration = Duration::from_secs(600);

type Fetched = (PathBuf, Source, Option<String>);

impl Supervisor {
    /// One pass. A file that cannot be read or is invalid changes nothing; an app whose
    /// package cannot be read is left as it is, the others go on.
    pub async fn reconcile(self: &Arc<Self>) -> Result<ReconcileReport> {
        let file = self.cfg.desired.file.clone().context("no desired-state file: set [desired] file")?;
        let _guard = self.reconcile_lock.lock().await;
        let path = file.clone();
        let desired = tokio::task::spawn_blocking(move || load_unchanged(&path)).await??;
        let mut report = ReconcileReport { file, ..Default::default() };

        // Registry packages first (async), then everything is read from local files. What was
        // pulled is installed as it is: no second pull, no second signature check.
        let mut pulled: HashMap<String, Result<Fetched, String>> = HashMap::new();
        for app in desired.apps.values() {
            if let Source::Oci(r) = &app.package
                && !pulled.contains_key(&r.to_string())
            {
                let fetched = self.fetch(&app.package).await.map_err(|e| format!("{e:#}"));
                pulled.insert(r.to_string(), fetched);
            }
        }
        let (desired, packages, pulled) = tokio::task::spawn_blocking(move || {
            let mut desired = desired;
            let packages = desired.read_packages(|r| {
                let (path, pinned, _) = pulled.get(&r.to_string()).context("not pulled")?.clone().map_err(|e| anyhow!(e))?;
                let Source::Oci(pinned) = pinned else { unreachable!() };
                Ok((Package::read(&path)?, pinned))
            });
            (desired, packages, pulled)
        })
        .await?;
        let fetched: HashMap<String, Fetched> =
            pulled.into_values().flatten().map(|f| (f.1.to_string(), f)).collect();
        let mut readable = std::collections::BTreeMap::new();
        for (id, pkg) in packages {
            match pkg {
                Ok(pkg) => drop(readable.insert(id, pkg)),
                Err(e) => {
                    tracing::error!("desired state: {id} left as it is: {e:#}");
                    report.errors.push(format!("{e:#}"));
                }
            }
        }

        let current: Vec<Current> = self.list().iter().map(Current::from).collect();
        let plan = desired::plan(&desired, &readable, &current, self.cfg.desired.prune)?;
        report.unlisted = plan.unlisted;
        report.rolled_back = plan.rolled_back;
        let mut failed = BTreeSet::new();
        for step in plan.steps {
            if failed.contains(step.id()) {
                continue;
            }
            let result = match &step {
                Step::Stop { id } => self.stop(id, StopReason::User, false).await,
                Step::Start { id } => self.start(id).await,
                Step::Uninstall { id } => self.uninstall(id).await,
                Step::Install { id, package, grant, .. } => {
                    let pkg = match package {
                        Source::Path(p) => Ok((p.clone(), package.clone(), None)),
                        Source::Oci(_) => fetched.get(&package.to_string()).cloned().context("not pulled"),
                    };
                    match pkg {
                        Ok(pkg) => self.install_fetched(pkg, grant.clone(), Some(id.clone())).await.map(drop),
                        Err(e) => Err(e),
                    }
                }
            };
            match result {
                Ok(()) => {
                    tracing::info!("desired state: {step}");
                    report.done.push(step.to_string());
                }
                Err(e) => {
                    tracing::error!("desired state: {step}: {e:#}");
                    report.errors.push(format!("{step}: {e:#}"));
                    failed.insert(step.id().to_string());
                }
            }
        }
        Ok(report)
    }

    /// The first pass and then a pass on every change in the file's directory (the file is
    /// often replaced by a rename, so the file itself cannot be watched), and a retry while
    /// passes fail.
    pub fn spawn_desired(self: &Arc<Self>) {
        let Some(file) = self.cfg.desired.file.clone() else { return };
        let sup = self.clone();
        tokio::spawn(async move {
            let dir = file.parent().unwrap_or(&file);
            use rustix::fs::inotify::WatchFlags as F;
            let flags = F::CLOSE_WRITE | F::MOVED_TO | F::CREATE;
            let mut watch = EventsWatch::new(dir, flags)
                .inspect_err(|e| tracing::warn!("desired state: changes of {} are not watched: {e:#}", file.display()))
                .ok();
            let mut backoff = RETRY_FIRST;
            loop {
                let retry = if sup.reconcile_logged().await {
                    backoff = RETRY_FIRST;
                    None
                } else {
                    let delay = backoff;
                    backoff = (backoff * 2).min(RETRY_MAX);
                    tracing::info!("desired state: next pass in {} s", delay.as_secs());
                    Some(delay)
                };
                if watch.is_none() && retry.is_none() {
                    return;
                }
                let changed = async {
                    match &mut watch {
                        Some(w) => w.changed().await.is_ok(),
                        None => std::future::pending().await,
                    }
                };
                let changed = tokio::select! {
                    ok = changed => Some(ok),
                    _ = tokio::time::sleep(retry.unwrap_or(RETRY_MAX)), if retry.is_some() => None,
                };
                match changed {
                    Some(true) => {
                        tokio::time::sleep(SETTLE).await;
                        if let Some(w) = &mut watch {
                            w.drain();
                        }
                    }
                    Some(false) => watch = None,
                    None => {}
                }
            }
        });
    }

    /// Whether the pass went without errors.
    async fn reconcile_logged(self: &Arc<Self>) -> bool {
        match self.reconcile().await {
            Ok(r) => {
                for id in &r.unlisted {
                    tracing::warn!("desired state: {id} is not in {} (kept: [desired] prune = false)", r.file.display());
                }
                for app in &r.rolled_back {
                    tracing::warn!("desired state: {app} was rolled back: not installed again (shellctl install retries it)");
                }
                r.errors.is_empty()
            }
            Err(e) => {
                tracing::error!("desired state: nothing changed: {e:#}");
                false
            }
        }
    }
}

/// The file, if it did not change while it was read: a writer still at it (the change brings
/// another pass).
fn load_unchanged(path: &Path) -> Result<DesiredState> {
    let stamp = || std::fs::metadata(path).map(|m| (m.len(), m.modified().ok())).ok();
    let before = stamp();
    let desired = DesiredState::load(path)?;
    ensure!(stamp() == before, "{} changed while it was read", path.display());
    Ok(desired)
}
