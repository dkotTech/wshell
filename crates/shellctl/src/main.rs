//! shellctl: manages shelld over a local Unix socket.

#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use shell_core::control::{PackageInfo, Request, Response};

#[derive(Parser)]
#[command(version, about = "Manage the wshell shell")]
struct Cli {
    /// Path to the control socket (defaults to the runtime directory).
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Install or update an app from a directory or a .pkg archive.
    Install {
        path: PathBuf,
        /// Do not ask: accept the required permissions.
        #[arg(short, long)]
        yes: bool,
        /// Grant an optional permission (repeatable), e.g. --grant exec:tracepath.
        #[arg(long = "grant")]
        grant: Vec<String>,
        /// Grant all optional permissions.
        #[arg(long)]
        grant_all: bool,
    },
    /// Remove an app and its data.
    Uninstall { id: String },
    /// List installed apps.
    List,
    /// Start an app and print a one-time UI link.
    Start {
        id: String,
        /// Open the UI in a browser.
        #[arg(short, long)]
        open: bool,
    },
    /// Stop an app.
    Stop {
        id: String,
        /// Do not wait for a busy app (activity.busy) to become free.
        #[arg(short, long)]
        force: bool,
    },
    /// New one-time link to the app's UI.
    Url {
        id: String,
        /// Open in a browser instead of printing the link.
        #[arg(short, long)]
        open: bool,
    },
    /// One-time link to the dashboard.
    Dashboard {
        /// Open in a browser instead of printing the link.
        #[arg(short, long)]
        open: bool,
    },
    /// Call a backend method (like shell.call from the UI).
    Call {
        id: String,
        method: String,
        /// Parameters as JSON.
        #[arg(default_value = "null")]
        params: String,
    },
    /// App log.
    Logs {
        id: String,
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        #[arg(short, long)]
        follow: bool,
    },
}

struct Conn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Conn {
    fn open(socket: Option<PathBuf>) -> Result<Self> {
        let path = socket.unwrap_or_else(|| {
            shell_core::config::control_socket(&shell_core::config::default_runtime_dir())
        });
        let stream = UnixStream::connect(&path)
            .with_context(|| format!("cannot connect to shelld ({}). Is shelld running?", path.display()))?;
        Ok(Conn { reader: BufReader::new(stream.try_clone()?), writer: stream })
    }

    fn send(&mut self, req: &Request) -> Result<()> {
        let mut line = serde_json::to_vec(req)?;
        line.push(b'\n');
        self.writer.write_all(&line)?;
        Ok(())
    }

    fn recv(&mut self) -> Result<Response> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            bail!("shelld closed the connection");
        }
        match serde_json::from_str(&line)? {
            Response::Error { message } => bail!("{message}"),
            r => Ok(r),
        }
    }

    fn request(&mut self, req: &Request) -> Result<Response> {
        self.send(req)?;
        self.recv()
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let mut conn = Conn::open(cli.socket)?;
    match cli.command {
        Cmd::Install { path, yes, grant, grant_all } => {
            let path = std::fs::canonicalize(&path).with_context(|| format!("{}", path.display()))?;
            install(&mut conn, path, yes, grant, grant_all)
        }
        Cmd::Uninstall { id } => {
            conn.request(&Request::Uninstall { id: id.clone() })?;
            println!("{id} removed");
            Ok(())
        }
        Cmd::List => list(&mut conn),
        Cmd::Start { id, open } => {
            let resp = conn.request(&Request::Start { id: id.clone() })?;
            println!("{id} started");
            if let Response::Url { url } = resp {
                show_url("UI: ", &url, open);
            }
            Ok(())
        }
        Cmd::Stop { id, force } => {
            conn.request(&Request::Stop { id: id.clone(), force })?;
            println!("{id} stopped");
            Ok(())
        }
        Cmd::Dashboard { open } => {
            if let Response::Url { url } = conn.request(&Request::DashboardUrl)? {
                show_url("", &url, open);
            }
            Ok(())
        }
        Cmd::Url { id, open } => {
            if let Response::Url { url } = conn.request(&Request::Url { id })? {
                show_url("", &url, open);
            }
            Ok(())
        }
        Cmd::Call { id, method, params } => {
            if let Response::CallResult { result } = conn.request(&Request::Call { id, method, payload: params })? {
                println!("{result}");
            }
            Ok(())
        }
        Cmd::Logs { id, lines, follow } => {
            conn.send(&Request::Logs { id, lines, follow })?;
            loop {
                match conn.recv()? {
                    Response::Log { line } => println!("{line}"),
                    _ => return Ok(()),
                }
            }
        }
    }
}

fn install(conn: &mut Conn, path: PathBuf, yes: bool, grant: Vec<String>, grant_all: bool) -> Result<()> {
    let Response::Package(info) = conn.request(&Request::Inspect { path: path.clone() })? else {
        bail!("unexpected response from shelld");
    };
    print_package(&info);

    for key in &grant {
        if !info.permissions.iter().any(|p| &p.key == key && p.optional) {
            bail!("{key} is not an optional permission of this package");
        }
    }
    let interactive = !yes && std::io::stdin().is_terminal();
    let mut granted = grant;
    for p in info.permissions.iter().filter(|p| p.optional) {
        if granted.contains(&p.key) {
            continue;
        }
        // On update, a previously granted permission is kept by default.
        let had = p.granted;
        let give = grant_all
            || if interactive {
                let q = if had { "Keep the granted permission" } else { "Grant the optional permission" };
                ask(&format!("{q} {}?", p.key), had)?
            } else {
                had
            };
        if give {
            granted.push(p.key.clone());
        }
    }
    if !yes {
        if !interactive {
            bail!("--yes is required for non-interactive installation");
        }
        let verb = if info.installed_version.is_some() { "Update" } else { "Install" };
        if !ask(&format!("{verb} {} {}?", info.id, info.version), true)? {
            bail!("installation cancelled");
        }
    }

    match conn.request(&Request::Install { path, grant_optional: granted })? {
        Response::Installed { id, version } => println!("installed: {id} {version}"),
        _ => bail!("unexpected response from shelld"),
    }
    Ok(())
}

/// Opens a one-time link in a browser or prints it.
fn show_url(label: &str, url: &str, open: bool) {
    if !open {
        println!("{label}{url}");
        return;
    }
    let opened = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if opened {
        println!("{label}opened in browser");
    } else {
        eprintln!("failed to open a browser (xdg-open), link:");
        println!("{label}{url}");
    }
}

fn print_package(info: &PackageInfo) {
    match &info.installed_version {
        Some(v) => println!("{} ({}) {} → {}", info.name, info.id, v, info.version),
        None => println!("{} ({}) {}", info.name, info.id, info.version),
    }
    if info.permissions.is_empty() {
        println!("Permissions: none required");
    } else {
        println!("Permissions:");
        let upgrade = info.installed_version.is_some();
        for p in &info.permissions {
            let mut tags = Vec::new();
            if p.optional {
                tags.push("optional");
            }
            if upgrade && !p.granted {
                tags.push("NEW");
            }
            let tags = if tags.is_empty() { String::new() } else { format!(" [{}]", tags.join(", ")) };
            println!("  • {}{tags}: {}", p.key, p.description);
            if let Some(reason) = &p.reason {
                println!("      why: {reason}");
            }
        }
    }
    for w in &info.warnings {
        println!("warning: {w}");
    }
}

fn ask(question: &str, default: bool) -> Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    print!("{question} {hint} ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(match answer.trim().to_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    })
}

fn list(conn: &mut Conn) -> Result<()> {
    let Response::Apps { apps } = conn.request(&Request::List)? else {
        bail!("unexpected response from shelld");
    };
    if apps.is_empty() {
        println!("no installed apps");
        return Ok(());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let w = apps.iter().map(|a| a.id.len()).max().unwrap_or(2).max(2);
    println!(
        "{:<w$}  {:<8} {:<10} {:<7} {:>7} {:>5} {:>6} {:>9} {:>8}  BUSY",
        "ID", "VERSION", "STATE", "PRIO", "STARTS", "FAILS", "AGE", "MEMORY", "PID"
    );
    for a in apps {
        let dash = || "-".to_string();
        let pid = a.pid.map(|p| p.to_string()).unwrap_or_else(dash);
        let age = a.started_at.map(|t| age(now.saturating_sub(t))).unwrap_or_else(dash);
        let mem = a.metrics.as_ref().map(|m| format!("{:.1}Mi", m.memory_bytes as f64 / 1048576.0)).unwrap_or_else(dash);
        let busy: Vec<&str> = a.busy.iter().filter(|b| !b.expired).map(|b| b.reason.as_str()).collect();
        let priority = format!("{:?}", a.priority).to_lowercase();
        println!(
            "{:<w$}  {:<8} {:<10} {:<7} {:>7} {:>5} {:>6} {:>9} {:>8}  {}",
            a.id,
            a.version,
            a.state.to_string(),
            priority,
            a.starts,
            a.failures,
            age,
            mem,
            pid,
            busy.join("; ")
        );
    }
    Ok(())
}

/// Age in kubectl style: 45s, 12m, 3h5m, 2d4h.
fn age(secs: u64) -> String {
    match secs {
        s if s < 120 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h{}m", s / 3600, s % 3600 / 60),
        s => format!("{}d{}h", s / 86_400, s % 86_400 / 3600),
    }
}
