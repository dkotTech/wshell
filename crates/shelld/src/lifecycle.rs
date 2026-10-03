//! Stopping apps by Shell's decision: when idle, under memory pressure, on command.
//!
//! The app only gives hints: `[lifecycle]` in the manifest (whether it may be stopped
//! when idle, priority) and `activity.busy` handles ("unsafe to stop right now").
//! The device policy may override them; being busy only postpones the stop
//! by `stop_grace_ms` and stops counting after `max_busy_s`.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use shell_core::control::{BusyInfo, StopReason};
use shell_core::manifest::{IdleMode, Priority};
use tokio::sync::Notify;

use crate::package::Installed;
use crate::supervisor::{Supervisor, Worker};
use crate::util::unix_now;

/// Activity of one running app.
pub struct Activity {
    state: Mutex<State>,
    /// Wakes those waiting for locks to be released.
    changed: Notify,
}

struct State {
    last: Instant,
    last_unix: u64,
    busy: BTreeMap<u32, Busy>,
}

struct Busy {
    reason: String,
    since: Instant,
    since_unix: u64,
}

impl Activity {
    pub fn new() -> Self {
        Activity {
            state: Mutex::new(State { last: Instant::now(), last_unix: unix_now(), busy: BTreeMap::new() }),
            changed: Notify::new(),
        }
    }

    /// Mark activity: a bridge call, a UI visibility change, a utility starting or exiting.
    pub fn touch(&self) {
        let mut s = self.state.lock().unwrap();
        s.last = Instant::now();
        s.last_unix = unix_now();
    }

    pub fn last(&self) -> Instant {
        self.state.lock().unwrap().last
    }

    pub fn last_unix(&self) -> u64 {
        self.state.lock().unwrap().last_unix
    }

    pub fn busy_begin(&self, id: u32, reason: String) {
        let busy = Busy { reason, since: Instant::now(), since_unix: unix_now() };
        self.state.lock().unwrap().busy.insert(id, busy);
        self.touch();
        self.changed.notify_waiters();
    }

    pub fn busy_end(&self, id: u32) {
        self.state.lock().unwrap().busy.remove(&id);
        self.touch();
        self.changed.notify_waiters();
    }

    /// Reasons for busy state that still counts (lasting no longer than `max`).
    pub fn busy_reasons(&self, max: Duration) -> Vec<String> {
        let s = self.state.lock().unwrap();
        s.busy.values().filter(|b| b.since.elapsed() < max).map(|b| b.reason.clone()).collect()
    }

    pub fn busy_info(&self, max: Duration) -> Vec<BusyInfo> {
        let s = self.state.lock().unwrap();
        s.busy
            .values()
            .map(|b| BusyInfo { reason: b.reason.clone(), since: b.since_unix, expired: b.since.elapsed() >= max })
            .collect()
    }

    /// Waits until the counted busy state ends, but no longer than `deadline`.
    /// Returns `false` if time ran out.
    pub async fn wait_free(&self, max: Duration, deadline: tokio::time::Instant) -> bool {
        loop {
            // Subscribe before checking: notify_waiters in between will not be lost.
            let changed = self.changed.notified();
            if self.busy_reasons(max).is_empty() {
                return true;
            }
            tokio::select! {
                _ = changed => {}
                _ = tokio::time::sleep_until(deadline) => return false,
            }
        }
    }
}

/// Eviction queue key, smaller goes first: low priority first, then
/// apps with a hidden UI, then non-busy ones, then those heavier on memory.
pub fn eviction_key(priority: Priority, visible: bool, busy: bool, memory: u64) -> (Priority, bool, bool, Reverse<u64>) {
    (priority, visible, busy, Reverse(memory))
}

impl Supervisor {
    /// Priority and idle mode applied by Shell: the device policy
    /// on top of the manifest hints.
    pub fn effective_lifecycle(&self, installed: &Installed) -> (Priority, IdleMode) {
        let hints = installed.manifest.lifecycle;
        let policy = self.cfg.policy(&installed.manifest.app.id);
        let idle = match policy.idle {
            Some(idle) => idle,
            None if hints.idle == IdleMode::Keep && !self.cfg.lifecycle.honor_keep => IdleMode::Stop,
            None => hints.idle,
        };
        (policy.priority.unwrap_or(hints.priority), idle)
    }

    pub fn max_busy(&self) -> Duration {
        Duration::from_secs(self.cfg.lifecycle.max_busy_s)
    }

    /// Whether the app is active: visible UI, running utilities or counted busy state.
    pub fn is_active(&self, worker: &Worker) -> bool {
        worker.is_visible() || !worker.jobs.lock().unwrap().is_empty() || !worker.activity.busy_reasons(self.max_busy()).is_empty()
    }

    /// Stops the app after `idle_stop_after_s` of inactivity, if allowed.
    /// The timer is armed for the idle period: no periodic polling.
    pub fn spawn_idle_watch(self: &Arc<Self>, worker: Arc<Worker>) {
        let after = Duration::from_secs(self.cfg.lifecycle.idle_stop_after_s);
        // Shell sees no activity of a program (kind = "command"), so it is not stopped when idle.
        let command = worker.installed.manifest.backend.kind == shell_core::manifest::BackendKind::Command;
        if after.is_zero() || command || self.effective_lifecycle(&worker.installed).1 == IdleMode::Keep {
            return;
        }
        let sup = self.clone();
        tokio::spawn(async move {
            let mut exited = worker.exited.clone();
            loop {
                let deadline = tokio::time::Instant::from_std(worker.activity.last() + after);
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => {}
                    _ = async { exited.wait_for(|e| *e).await.map(|_| ()) } => return,
                }
                if sup.is_active(&worker) {
                    // Active right now: the idle countdown will start over.
                    worker.activity.touch();
                    continue;
                }
                if worker.activity.last().elapsed() < after {
                    continue;
                }
                if let Err(e) = sup.stop(&worker.app_id, StopReason::Idle, false).await {
                    tracing::debug!("idle stop {}: {e:#}", worker.app_id);
                }
                return;
            }
        });
    }

    /// The first app in the eviction queue.
    fn eviction_victim(&self) -> Option<Arc<Worker>> {
        let max_busy = self.max_busy();
        self.running_workers()
            .into_iter()
            .map(|w| {
                let priority = self.effective_lifecycle(&w.installed).0;
                let busy = !w.activity.busy_reasons(max_busy).is_empty();
                let memory = w.metrics().map_or(0, |m| m.memory_bytes);
                (eviction_key(priority, w.is_visible(), busy, memory), w)
            })
            .min_by(|a, b| a.0.cmp(&b.0))
            .map(|(_, w)| w)
    }

    /// Memory budget of all apps: `memory.high` on the `apps/` cgroup.
    ///
    /// Having hit the budget, the kernel reclaims memory from the apps itself (and they slow down),
    /// recording a `high` event in `memory.events`; a file change is reported via
    /// inotify, with no polling. On each such spike, if app memory is still
    /// close to the budget, Shell stops the first app in the eviction queue.
    pub fn spawn_memory_guard(self: &Arc<Self>) -> Result<()> {
        let budget_mb = self.cfg.lifecycle.memory_high_mb;
        if budget_mb == 0 {
            return Ok(());
        }
        let Some(cgroups) = &self.cgroups else {
            tracing::warn!("lifecycle.memory_high_mb is set, but cgroups are unavailable; eviction disabled");
            return Ok(());
        };
        let budget = u64::from(budget_mb) << 20;
        cgroups.set_apps_memory_high(budget)?;
        let mut events = cgroups.watch_apps_memory_events()?;
        tracing::info!("app memory budget: {budget_mb} MB");

        let sup = self.clone();
        tokio::spawn(async move {
            let cgroups = sup.cgroups.as_ref().expect("checked above");
            let mut seen = cgroups.apps_memory_high_events();
            while events.changed().await.is_ok() {
                let high = cgroups.apps_memory_high_events();
                if high <= seen {
                    continue;
                }
                seen = high;
                let used = cgroups.apps_memory_used();
                // The kernel has already reclaimed memory down to the budget, so "used" is almost always
                // slightly below it. The threshold filters out events from stopped apps' page cache.
                tracing::debug!("memory.high: event {high}, app memory {} KB", used >> 10);
                if used * 4 < budget * 3 {
                    continue;
                }
                let Some(victim) = sup.eviction_victim() else { continue };
                let detail = format!("app memory {:.1} of {budget_mb} MB", used as f64 / 1048576.0);
                tracing::warn!("evicting {}: {detail}", victim.app_id);
                sup.audit.record(&victim.app_id, "evict", &detail);
                let _ = sup.stop(&victim.app_id, StopReason::MemoryPressure, false).await;
                // Events accumulated while stopping relate to load that is already gone.
                seen = cgroups.apps_memory_high_events();
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eviction_order() {
        // (priority, UI visible, busy, memory) → eviction order.
        let mut apps = [
            ("high-hidden", eviction_key(Priority::High, false, false, 10)),
            ("normal-visible", eviction_key(Priority::Normal, true, false, 10)),
            ("normal-busy", eviction_key(Priority::Normal, false, true, 10)),
            ("normal-small", eviction_key(Priority::Normal, false, false, 5)),
            ("normal-big", eviction_key(Priority::Normal, false, false, 50)),
            ("low-busy-visible", eviction_key(Priority::Low, true, true, 1)),
        ];
        apps.sort_by_key(|a| a.1);
        let order: Vec<_> = apps.iter().map(|a| a.0).collect();
        assert_eq!(order, ["low-busy-visible", "normal-big", "normal-small", "normal-busy", "normal-visible", "high-hidden"]);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn busy_guards_and_wait() {
        let a = Activity::new();
        let max = Duration::from_secs(60);
        a.busy_begin(1, "writing".into());
        a.busy_begin(2, "syncing".into());
        a.busy_end(1);
        assert_eq!(a.busy_reasons(max), ["syncing"], "nested locks are independent");

        // Time ran out while the app is busy.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        assert!(!a.wait_free(max, deadline).await);

        // Release wakes the waiter.
        let a = std::sync::Arc::new(a);
        let waiter = tokio::spawn({
            let a = a.clone();
            async move { a.wait_free(max, tokio::time::Instant::now() + Duration::from_secs(10)).await }
        });
        tokio::task::yield_now().await;
        a.busy_end(2);
        assert!(waiter.await.unwrap());

        // Busy state longer than max is not counted.
        a.busy_begin(3, "forever".into());
        assert!(a.busy_reasons(Duration::ZERO).is_empty());
    }
}
