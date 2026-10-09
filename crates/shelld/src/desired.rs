//! Keeps the installed apps at the desired-state file (`[desired] file`): on start, when the
//! file changes and on `shellctl reconcile`. Not continuously: an idle stop, an eviction or a
//! manual command holds until the next pass.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use shell_core::control::{ReconcileReport, StopReason};
use shell_core::desired::{self, Current, DesiredState, Step};

use crate::cgroup::EventsWatch;
use crate::supervisor::Supervisor;

/// Editors and `git` write a file in several steps: wait for them to finish.
const SETTLE: Duration = Duration::from_millis(300);

impl Supervisor {
    /// One pass. A broken file (unreadable, invalid, a missing package) changes nothing.
    pub async fn reconcile(self: &Arc<Self>) -> Result<ReconcileReport> {
        let file = self.cfg.desired.file.clone().context("no desired-state file: set [desired] file")?;
        let _guard = self.reconcile_lock.lock().await;
        let path = file.clone();
        let (desired, packages) = tokio::task::spawn_blocking(move || {
            let mut desired = DesiredState::load(&path)?;
            let packages = desired.read_packages()?;
            anyhow::Ok((desired, packages))
        })
        .await??;
        let current: Vec<Current> = self.list().iter().map(Current::from).collect();
        let plan = desired::plan(&desired, &packages, &current, self.cfg.desired.prune)?;

        let mut report = ReconcileReport { file, unlisted: plan.unlisted, ..Default::default() };
        let mut failed = BTreeSet::new();
        for step in plan.steps {
            if failed.contains(step.id()) {
                continue;
            }
            let result = match &step {
                Step::Stop { id } => self.stop(id, StopReason::User, false).await,
                Step::Start { id } => self.start(id).await,
                Step::Uninstall { id } => self.uninstall(id).await,
                Step::Install { package, grant, .. } => self.install(package.clone(), grant.clone()).await.map(drop),
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
    /// often replaced by a rename, so the file itself cannot be watched).
    pub fn spawn_desired(self: &Arc<Self>) {
        let Some(file) = self.cfg.desired.file.clone() else { return };
        let sup = self.clone();
        tokio::spawn(async move {
            sup.reconcile_logged().await;
            let dir = file.parent().unwrap_or(&file);
            use rustix::fs::inotify::WatchFlags as F;
            let flags = F::CLOSE_WRITE | F::MOVED_TO | F::CREATE;
            let mut watch = match EventsWatch::new(dir, flags) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!("desired state: changes of {} are not watched: {e:#}", file.display());
                    return;
                }
            };
            while watch.changed().await.is_ok() {
                tokio::time::sleep(SETTLE).await;
                watch.drain();
                sup.reconcile_logged().await;
            }
        });
    }

    async fn reconcile_logged(self: &Arc<Self>) {
        match self.reconcile().await {
            Ok(r) => {
                for id in &r.unlisted {
                    tracing::warn!("desired state: {id} is not in {} (kept: [desired] prune = false)", r.file.display());
                }
            }
            Err(e) => tracing::error!("desired state: nothing changed: {e:#}"),
        }
    }
}
