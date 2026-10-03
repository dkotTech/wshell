//! Example app: ping and tracepath with streaming output to the UI.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::Instant;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;

use serde::Deserialize;
use serde_json::json;

wit_bindgen::generate!({
    world: "app",
    path: "../../wit",
});

use exports::shell::app::bridge::Guest as Bridge;
use exports::shell::app::lifecycle::{Guest as Lifecycle, HostEvent, StreamKind};
use shell::app::{activity, events, exec, log, metrics, storage};

const HISTORY_KEY: &str = "history";
const HISTORY_LEN: usize = 10;

#[derive(Default)]
struct State {
    ping: Option<exec::Command>,
    tracepath: Option<exec::Command>,
    store: Option<storage::Store>,
    /// Running utilities. While a utility runs, we hold a busy lock:
    /// Shell will not stop the app in the middle of a trace without warning.
    jobs: BTreeMap<u64, Job>,
    stats: Stats,
}

struct Job {
    _busy: activity::Busy,
    tool: &'static str,
    started: Instant,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct CheckLabels {
    tool: String,
    result: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ToolLabels {
    tool: String,
}

/// The app's own metrics, kept in its memory by the standard Prometheus client.
struct Stats {
    registry: Registry,
    checks: Family<CheckLabels, Counter>,
    duration: Family<ToolLabels, Histogram>,
}

impl Default for Stats {
    fn default() -> Self {
        let checks = Family::<CheckLabels, Counter>::default();
        let duration = Family::<ToolLabels, Histogram>::new_with_constructor(|| Histogram::new(exponential_buckets(0.25, 2.0, 9)));
        let mut registry = Registry::default();
        registry.register("ping_checks", "Checks run, by tool and result", checks.clone());
        registry.register("ping_check_duration_seconds", "Duration of a check", duration.clone());
        Stats { registry, checks, duration }
    }
}

impl Stats {
    fn finished(&self, tool: &str, result: &str, seconds: f64) {
        self.checks.get_or_create(&CheckLabels { tool: tool.into(), result: result.into() }).inc();
        self.duration.get_or_create(&ToolLabels { tool: tool.into() }).observe(seconds);
    }

    /// Hands the current snapshot to Shell (`/metrics` of the device).
    fn publish(&self) {
        let mut text = String::new();
        if prometheus_client::encoding::text::encode(&mut text, &self.registry).is_ok()
            && let Err(e) = metrics::publish(&text)
        {
            log::log(log::Level::Warn, &format!("metrics: {e}"));
        }
    }
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

#[derive(Deserialize)]
struct PingArgs {
    host: String,
    #[serde(default = "default_count")]
    count: u32,
}

fn default_count() -> u32 {
    4
}

#[derive(Deserialize)]
struct HostArgs {
    host: String,
}

#[derive(Deserialize)]
struct JobArgs {
    job: u64,
}

struct App;

impl Lifecycle for App {
    fn on_start() -> Result<(), String> {
        STATE.with_borrow_mut(|s| {
            s.ping = exec::open("ping");
            s.tracepath = exec::open("tracepath");
            s.store = storage::open();
        });
        if STATE.with_borrow(|s| s.ping.is_none()) {
            return Err("no exec:ping permission".into());
        }
        STATE.with_borrow(|s| s.stats.publish());
        log::log(log::Level::Info, "ping app started");
        Ok(())
    }

    fn on_stop() {
        log::log(log::Level::Info, "ping app stopping");
    }

    fn on_event(event: HostEvent) {
        match event {
            HostEvent::ExecOutput(out) => {
                let stream = match out.stream {
                    StreamKind::Stdout => "stdout",
                    StreamKind::Stderr => "stderr",
                };
                let text = String::from_utf8_lossy(&out.data);
                let payload = json!({ "job": out.job, "stream": stream, "text": text });
                events::emit("output", &payload.to_string());
            }
            HostEvent::ExecExit(exit) => {
                STATE.with_borrow_mut(|s| {
                    if let Some(job) = s.jobs.remove(&exit.job) {
                        let result = match (exit.timed_out, exit.exit_code) {
                            (true, _) => "timeout",
                            (_, Some(0)) => "ok",
                            (_, None) => "cancelled",
                            _ => "fail",
                        };
                        s.stats.finished(job.tool, result, job.started.elapsed().as_secs_f64());
                        s.stats.publish();
                    }
                });
                let payload = json!({
                    "job": exit.job,
                    "exitCode": exit.exit_code,
                    "timedOut": exit.timed_out,
                });
                events::emit("exit", &payload.to_string());
            }
            HostEvent::UiVisible(_) => {}
            HostEvent::StopRequested(req) => {
                // Shell wants to stop us: interrupt the utilities; their exit will release the locks.
                log::log(log::Level::Info, &format!("stop-requested {:?}, grace {} ms", req.reason, req.grace_ms));
                let jobs: Vec<u64> = STATE.with_borrow(|s| s.jobs.keys().copied().collect());
                for job in jobs {
                    exec::cancel(job);
                }
            }
        }
    }
}

impl Bridge for App {
    fn handle(method: String, payload: String) -> Result<String, String> {
        match method.as_str() {
            "capabilities" => STATE.with_borrow(|s| {
                Ok(json!({
                    "tracepath": s.tracepath.is_some(),
                    "history": s.store.is_some(),
                })
                .to_string())
            }),
            "ping" => {
                let args: PingArgs = parse(&payload)?;
                spawn("ping", |s| s.ping.as_ref(), &args.host, vec![("count".into(), args.count.to_string())])
            }
            "tracepath" => {
                let args: HostArgs = parse(&payload)?;
                spawn("tracepath", |s| s.tracepath.as_ref(), &args.host, Vec::new())
            }
            "cancel" => {
                let args: JobArgs = parse(&payload)?;
                exec::cancel(args.job);
                Ok("null".into())
            }
            "history" => Ok(serde_json::to_string(&history()).unwrap()),
            _ => Err(format!("unknown method: {method}")),
        }
    }
}

/// Runs a utility in the background against `host`; the host is remembered in the history.
fn spawn(
    tool: &'static str,
    command: impl FnOnce(&State) -> Option<&exec::Command>,
    host: &str,
    mut args: Vec<(String, String)>,
) -> Result<String, String> {
    args.push(("host".into(), host.to_string()));
    let job = STATE.with_borrow(|s| command(s).ok_or("permission not granted")?.spawn(&args))?;
    let busy = activity::Busy::new(&format!("checking {host}"));
    STATE.with_borrow_mut(|s| s.jobs.insert(job, Job { _busy: busy, tool, started: Instant::now() }));
    remember(host);
    Ok(json!({ "job": job }).to_string())
}

fn parse<T: for<'a> Deserialize<'a>>(payload: &str) -> Result<T, String> {
    serde_json::from_str(payload).map_err(|e| format!("invalid parameters: {e}"))
}

fn history() -> Vec<String> {
    STATE.with_borrow(|s| {
        let Some(store) = &s.store else { return Vec::new() };
        match store.get(HISTORY_KEY) {
            Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_default(),
            _ => Vec::new(),
        }
    })
}

fn remember(host: &str) {
    let mut hosts = history();
    hosts.retain(|h| h != host);
    hosts.insert(0, host.to_string());
    hosts.truncate(HISTORY_LEN);
    STATE.with_borrow(|s| {
        if let Some(store) = &s.store {
            let bytes = serde_json::to_vec(&hosts).unwrap();
            if let Err(e) = store.set(HISTORY_KEY, &bytes) {
                log::log(log::Level::Warn, &format!("history not saved: {e}"));
            }
        }
    });
}

export!(App);
