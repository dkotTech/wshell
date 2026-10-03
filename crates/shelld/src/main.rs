//! shelld: a shell for running isolated WASM applications.

// The only allowed place is loading the AOT image (`engine::load_cwasm`).
#![deny(unsafe_code)]

mod cgroup;
mod control;
#[cfg(feature = "dashboard")]
mod dashboard;
mod engine;
mod hostsvc;
mod ipc;
mod lifecycle;
mod logs;
#[cfg(feature = "metrics")]
mod metrics;
mod package;
mod process;
mod supervisor;
#[cfg(feature = "ui-server")]
mod ui;
mod util;
#[cfg(any(feature = "ui-server", feature = "dashboard"))]
mod web;
mod worker;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use shell_core::config::Config;
use tokio::signal::unix::{SignalKind, signal};

#[derive(Parser)]
#[command(version, about = "A shell for running isolated WASM applications")]
struct Cli {
    /// Additional configuration files (applied on top of /etc/wshell/shell.toml).
    #[arg(short, long)]
    config: Vec<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Check the configuration and print the effective values.
    CheckConfig,
    /// Internal mode: an app's worker process (started by the supervisor).
    #[command(hide = true)]
    Worker,
    /// Internal mode: AOT compilation of a component (started by the supervisor).
    #[command(hide = true, name = engine::COMPILE)]
    Compile { wasm: PathBuf, cwasm: PathBuf },
}

fn main() -> Result<()> {
    // Utility launch helper: the utility's arguments must not go through clap.
    let mut args = std::env::args_os().skip(1);
    if args.next().is_some_and(|a| a == process::EXEC_HELPER) {
        if let Err(e) = process::run_exec_helper(args) {
            eprintln!("shelld exec-helper: {e:#}");
            std::process::exit(127);
        }
        unreachable!("exec-helper exits via exec");
    }

    let cli = Cli::parse();
    match &cli.command {
        Some(Command::Worker) => return worker::run(),
        Some(Command::Compile { wasm, cwasm }) => {
            // The supervisor shows stderr to the user as is: one line, without `Debug`.
            if let Err(e) = engine::run_compile(wasm, cwasm) {
                eprintln!("{e:#}");
                std::process::exit(1);
            }
            return Ok(());
        }
        _ => {}
    }

    let cfg = Config::load(&cli.config)?;
    if let Some(Command::CheckConfig) = cli.command {
        print!("{}", toml::to_string_pretty(&cfg)?);
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&cfg.shell.log_level)),
        )
        .init();

    // Single-threaded event loop: no background wakeup threads while idle.
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(run(cfg))
}

async fn run(cfg: Config) -> Result<()> {
    if shell_core::config::is_root() {
        tracing::warn!("shelld is running as root: exec utilities will run as root (privilege helper comes after the MVP)");
    }
    let sup = supervisor::Supervisor::new(cfg)?;

    let control = tokio::spawn(control::serve(sup.clone()));

    #[cfg(feature = "ui-server")]
    let ui = if sup.cfg.features.ui_server {
        tokio::spawn(ui::serve(sup.clone()))
    } else {
        tokio::spawn(std::future::pending())
    };
    #[cfg(not(feature = "ui-server"))]
    let ui = tokio::spawn(std::future::pending::<Result<()>>());

    #[cfg(feature = "dashboard")]
    let dashboard = if sup.cfg.dashboard.enabled {
        tokio::spawn(dashboard::serve(sup.clone()))
    } else {
        tokio::spawn(std::future::pending())
    };
    #[cfg(not(feature = "dashboard"))]
    let dashboard = tokio::spawn(std::future::pending::<Result<()>>());

    #[cfg(feature = "metrics")]
    let metrics = if sup.cfg.metrics.enabled {
        tokio::spawn(metrics::serve(sup.clone()))
    } else {
        tokio::spawn(std::future::pending())
    };
    #[cfg(not(feature = "metrics"))]
    let metrics = {
        if sup.cfg.metrics.enabled {
            tracing::warn!("metrics.enabled is set, but shelld is built without the `metrics` feature");
        }
        tokio::spawn(std::future::pending::<Result<()>>())
    };

    sup.spawn_memory_guard()?;
    sup.autostart().await;

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let result = tokio::select! {
        r = control => r?,
        r = ui => r?,
        r = dashboard => r?,
        r = metrics => r?,
        _ = term.recv() => Ok(()),
        _ = int.recv() => Ok(()),
    };
    tracing::info!("stopping apps");
    sup.stop_all().await;
    let dir = sup.cfg.runtime_dir();
    let _ = std::fs::remove_file(shell_core::config::control_socket(&dir));
    for name in sup.cfg.control.sockets.keys() {
        let _ = std::fs::remove_file(shell_core::config::extra_socket(&dir, name));
    }
    result
}
