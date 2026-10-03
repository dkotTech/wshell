//! `/metrics` for a Prometheus or an agent on the device: Shell's own metrics and the
//! snapshots apps publish (`shell:app/metrics`). Everything is computed at scrape;
//! nothing runs between scrapes.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use shell_core::control::AppState;
use shell_core::metrics::{CONTENT_TYPE, Kind, Sample, Writer};

use crate::supervisor::Supervisor;

const STATES: [AppState; 6] = [
    AppState::Stopped,
    AppState::Starting,
    AppState::Running,
    AppState::Suspended,
    AppState::Stopping,
    AppState::Failed,
];

pub async fn serve(sup: Arc<Supervisor>) -> Result<()> {
    let addr = sup.cfg.metrics.listen;
    let app = Router::new().route("/metrics", get(metrics)).with_state(sup);
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("metrics: bind {addr}"))?;
    tracing::info!("metrics: http://{addr}/metrics");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn metrics(State(sup): State<Arc<Supervisor>>, headers: HeaderMap) -> Response {
    let token = &sup.cfg.metrics.token;
    if !token.is_empty() {
        let given = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if !given.is_some_and(|g| token_eq(g, token)) {
            return (StatusCode::UNAUTHORIZED, "Authorization: Bearer <metrics.token> required\n").into_response();
        }
    }
    // Reads cgroup files of every app.
    match tokio::task::spawn_blocking(move || render(&sup)).await {
        Ok(body) => ([(header::CONTENT_TYPE, CONTENT_TYPE)], body).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Constant-time comparison: the token must not leak through response timing.
fn token_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn render(sup: &Supervisor) -> String {
    let mut w = Writer::default();
    let info = sup.shell_info();

    w.family("wshell_build_info", Kind::Gauge, "Shell version and power profile.");
    w.sample("wshell_build_info", &[("version", &info.version), ("profile", &info.profile)], 1.0);
    w.family("wshell_start_time_seconds", Kind::Gauge, "Start time of shelld, Unix seconds.");
    w.sample("wshell_start_time_seconds", &[], info.started_at as f64);
    w.family("wshell_cgroups", Kind::Gauge, "1 if apps run in delegated cgroups (CPU and memory limits).");
    w.sample("wshell_cgroups", &[], f64::from(u8::from(info.cgroups)));

    // shelld itself (the supervisor), without the apps' workers: they are counted per app.
    // Not the usual process_* names: an app's snapshot may carry its own.
    if let Some(own) = crate::cgroup::proc_metrics(std::process::id()) {
        w.family("wshell_process_resident_memory_bytes", Kind::Gauge, "Resident memory of shelld (the supervisor).");
        w.sample("wshell_process_resident_memory_bytes", &[], own.memory_bytes as f64);
        w.family("wshell_process_cpu_seconds_total", Kind::Counter, "CPU time of shelld (the supervisor).");
        w.sample("wshell_process_cpu_seconds_total", &[], own.cpu_usage_usec as f64 / 1e6);
        w.family("wshell_process_threads", Kind::Gauge, "Threads of shelld (the supervisor).");
        w.sample("wshell_process_threads", &[], f64::from(own.pids));
    }
    if let Ok(fds) = std::fs::read_dir("/proc/self/fd") {
        w.family("wshell_process_open_fds", Kind::Gauge, "Open file descriptors of shelld (the supervisor).");
        w.sample("wshell_process_open_fds", &[], fds.count() as f64);
    }
    if let Some(cg) = &sup.cgroups {
        w.family("wshell_apps_memory_bytes", Kind::Gauge, "Memory of all apps (cgroup).");
        w.sample("wshell_apps_memory_bytes", &[], cg.apps_memory_used() as f64);
        w.family("wshell_apps_memory_high_events_total", Kind::Counter, "Times the apps' memory budget was exceeded.");
        w.sample("wshell_apps_memory_high_events_total", &[], cg.apps_memory_high_events() as f64);
    }
    if sup.cfg.lifecycle.memory_high_mb > 0 {
        w.family("wshell_apps_memory_high_bytes", Kind::Gauge, "Memory budget of all apps: above it apps are evicted.");
        w.sample("wshell_apps_memory_high_bytes", &[], f64::from(sup.cfg.lifecycle.memory_high_mb) * 1048576.0);
    }

    let apps = sup.list();
    let per_app = |w: &mut Writer, name: &str, kind: Kind, help: &str, value: &dyn Fn(&shell_core::control::AppStatus) -> Option<f64>| {
        w.family(name, kind, help);
        for a in &apps {
            if let Some(v) = value(a) {
                w.sample(name, &[("app", &a.id)], v);
            }
        }
    };

    w.family("wshell_app_info", Kind::Gauge, "Installed apps and plugins.");
    for a in &apps {
        let kind = format!("{:?}", a.kind).to_lowercase();
        let priority = format!("{:?}", a.priority).to_lowercase();
        w.sample("wshell_app_info", &[("app", &a.id), ("version", &a.version), ("kind", &kind), ("priority", &priority)], 1.0);
    }
    w.family("wshell_app_state", Kind::Gauge, "App state: 1 for the current one.");
    for a in &apps {
        for s in STATES {
            w.sample("wshell_app_state", &[("app", &a.id), ("state", &s.to_string())], f64::from(u8::from(a.state == s)));
        }
    }
    per_app(&mut w, "wshell_app_starts_total", Kind::Counter, "Starts since shelld started.", &|a| Some(f64::from(a.starts)));
    per_app(&mut w, "wshell_app_failures_total", Kind::Counter, "Abnormal exits since shelld started.", &|a| {
        Some(f64::from(a.failures))
    });
    per_app(&mut w, "wshell_app_memory_limit_bytes", Kind::Gauge, "WASM linear memory limit.", &|a| {
        Some(f64::from(a.memory_limit_mb) * 1048576.0)
    });
    per_app(&mut w, "wshell_app_memory_bytes", Kind::Gauge, "Memory of the app's process (cgroup or /proc).", &|a| {
        a.metrics.as_ref().map(|m| m.memory_bytes as f64)
    });
    per_app(&mut w, "wshell_app_cpu_seconds_total", Kind::Counter, "CPU time of the app's process.", &|a| {
        a.metrics.as_ref().map(|m| m.cpu_usage_usec as f64 / 1e6)
    });
    per_app(&mut w, "wshell_app_pids", Kind::Gauge, "Processes of the app.", &|a| a.metrics.as_ref().map(|m| f64::from(m.pids)));
    per_app(&mut w, "wshell_app_busy", Kind::Gauge, "Held activity.busy locks.", &|a| Some(a.busy.len() as f64));

    // Apps' own metrics: families merged by name, the `app` label put first.
    let snapshots = sup.metrics_snapshots();
    let mut merged: BTreeMap<&str, (Kind, &str, Vec<(&str, &Sample)>)> = BTreeMap::new();
    let mut conflicts: BTreeMap<&str, usize> = BTreeMap::new();
    for (id, published) in &snapshots {
        for f in &published.snapshot.families {
            let entry = merged.entry(&f.name).or_insert_with(|| (f.kind, f.help.as_deref().unwrap_or(""), Vec::new()));
            if entry.0 != f.kind {
                // Another app declared this name with another type: Prometheus would reject the scrape.
                *conflicts.entry(id).or_default() += f.samples.len();
                continue;
            }
            entry.2.extend(f.samples.iter().map(|s| (id.as_str(), s)));
        }
    }

    w.family("wshell_app_metrics_published_timestamp_seconds", Kind::Gauge, "When the app last published its metrics.");
    for (id, p) in &snapshots {
        w.sample("wshell_app_metrics_published_timestamp_seconds", &[("app", id)], p.at as f64);
    }
    w.family("wshell_app_metrics_series", Kind::Gauge, "Series kept from the app's last snapshot.");
    for (id, p) in &snapshots {
        let kept = p.snapshot.series - conflicts.get(id.as_str()).copied().unwrap_or(0);
        w.sample("wshell_app_metrics_series", &[("app", id)], kept as f64);
    }
    w.family(
        "wshell_app_metrics_dropped",
        Kind::Gauge,
        "Series of the app's last snapshot not served: over the limit or a type conflict with another app.",
    );
    for (id, p) in &snapshots {
        let dropped = p.snapshot.dropped + conflicts.get(id.as_str()).copied().unwrap_or(0);
        w.sample("wshell_app_metrics_dropped", &[("app", id)], dropped as f64);
    }

    for (name, (kind, help, samples)) in &merged {
        w.family(name, *kind, help);
        for (app, s) in samples {
            let mut labels: Vec<(&str, &str)> = vec![("app", app)];
            labels.extend(s.labels.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            w.sample(&s.name, &labels, s.value);
        }
    }
    w.out
}
