//! Resource limits via cgroups v2.
//!
//! The supervisor must run in a delegated cgroup (systemd `Delegate=yes`
//! or `systemd-run --user --scope -p Delegate=yes`). Layout:
//!
//! ```text
//! <own cgroup>/
//! ├── supervisor/     # shelld itself (the "no processes in internal nodes" rule)
//! └── apps/<app-id>/  # the app's worker and the utilities it launched
//! ```

use std::fs;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use shell_core::control::Metrics;

const CGROUP_FS: &str = "/sys/fs/cgroup";
const CONTROLLERS: &[&str] = &["cpu", "memory", "pids"];

pub struct Cgroups {
    apps: PathBuf,
    /// Whether the kernel supports `cpu.max` (`CONFIG_CFS_BANDWIDTH`): the cpu
    /// controller can be present without it, e.g. in the arm64 defconfig.
    cpu_max: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub memory_bytes: u64,
    /// Share of one core in percent, 0 means no limit.
    pub cpu_percent: u32,
    pub pids_max: u32,
}

impl Cgroups {
    pub fn init() -> Result<Self> {
        let own = fs::read_to_string("/proc/self/cgroup").context("reading /proc/self/cgroup")?;
        let rel = own
            .lines()
            .find_map(|l| l.strip_prefix("0::"))
            .context("cgroup v2 not found (unified hierarchy required)")?;
        let base = Path::new(CGROUP_FS).join(rel.trim_start_matches('/'));

        let available = fs::read_to_string(base.join("cgroup.controllers")).unwrap_or_default();
        for c in CONTROLLERS {
            if !available.split_whitespace().any(|a| a == *c) {
                bail!("controller {c} is not delegated to {}", base.display());
            }
        }

        let sup = base.join("supervisor");
        if !base.ends_with("supervisor") {
            fs::create_dir_all(&sup).with_context(|| format!("creating {}", sup.display()))?;
            write(&sup.join("cgroup.procs"), "0")
                .context("moving shelld into a child cgroup (Delegate=yes required)")?;
        }
        let base = if base.ends_with("supervisor") { base.parent().unwrap().to_path_buf() } else { base };

        let enable = CONTROLLERS.iter().map(|c| format!("+{c}")).collect::<Vec<_>>().join(" ");
        write(&base.join("cgroup.subtree_control"), &enable)
            .context("enabling controllers (foreign processes in the cgroup?)")?;
        let apps = base.join("apps");
        fs::create_dir_all(&apps)?;
        write(&apps.join("cgroup.subtree_control"), &enable)?;
        let cpu_max = apps.join("cpu.max").exists();
        if !cpu_max {
            tracing::warn!("cpu.max is unavailable (kernel without CONFIG_CFS_BANDWIDTH): CPU limits are not applied");
        }
        Ok(Cgroups { apps, cpu_max })
    }

    /// Total memory budget of all apps: when exceeded, the kernel starts
    /// aggressively reclaiming their memory and records a `high` event in `memory.events`.
    pub fn set_apps_memory_high(&self, bytes: u64) -> Result<()> {
        write(&self.apps.join("memory.high"), &bytes.to_string())
    }

    /// How many times app memory hit `memory.high` (the kernel started reclaiming memory).
    pub fn apps_memory_high_events(&self) -> u64 {
        fs::read_to_string(self.apps.join("memory.events"))
            .ok()
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("high ")).and_then(|v| v.trim().parse().ok()))
            .unwrap_or(0)
    }

    /// Memory of all apps, including those still starting: the sum of `memory.current`
    /// of child groups. Page cache of removed groups is moved into `apps/` itself
    /// and is not counted here.
    pub fn apps_memory_used(&self) -> u64 {
        let Ok(rd) = fs::read_dir(&self.apps) else { return 0 };
        rd.flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| fs::read_to_string(e.path().join("memory.current")).ok())
            .filter_map(|v| v.trim().parse::<u64>().ok())
            .sum()
    }

    /// Subscription to `apps/memory.events` changes via inotify.
    pub fn watch_apps_memory_events(&self) -> Result<EventsWatch> {
        EventsWatch::new(&self.apps.join("memory.events"))
    }

    /// Creates an app cgroup with limits.
    pub fn create_app(&self, id: &str, limits: Limits) -> Result<AppCgroup> {
        let dir = self.apps.join(id);
        // Leftover from a previous run: kill the processes; an empty directory is reused.
        if dir.exists() {
            kill_all(&dir);
        }
        fs::create_dir_all(&dir)?;
        write(&dir.join("memory.max"), &limits.memory_bytes.to_string())?;
        let _ = write(&dir.join("memory.swap.max"), "0");
        write(&dir.join("pids.max"), &limits.pids_max.to_string())?;
        if self.cpu_max {
            let cpu = if limits.cpu_percent == 0 {
                "max 100000".to_string()
            } else {
                format!("{} 100000", u64::from(limits.cpu_percent) * 1000)
            };
            write(&dir.join("cpu.max"), &cpu)?;
        }
        Ok(AppCgroup { dir })
    }
}

pub struct AppCgroup {
    dir: PathBuf,
}

impl AppCgroup {
    /// Group directory: `shelld exec-helper` enters it itself before `exec`ing the utility.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Move a process into the group. The worker is moved before it receives `Init`,
    /// i.e. before it starts doing anything.
    pub fn add(&self, pid: u32) -> Result<()> {
        write(&self.dir.join("cgroup.procs"), &pid.to_string())
    }

    pub fn metrics(&self) -> Metrics {
        let read = |f: &str| fs::read_to_string(self.dir.join(f)).unwrap_or_default();
        let cpu_usage_usec = read("cpu.stat")
            .lines()
            .find_map(|l| l.strip_prefix("usage_usec "))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        Metrics {
            memory_bytes: read("memory.current").trim().parse().unwrap_or(0),
            memory_max_bytes: read("memory.max").trim().parse().ok(),
            cpu_usage_usec,
            pids: read("pids.current").trim().parse().unwrap_or(0),
            source: "cgroup".into(),
        }
    }

    /// Whether there was an OOM kill in this cgroup.
    pub fn oom_killed(&self) -> bool {
        fs::read_to_string(self.dir.join("memory.events"))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("oom_kill "))
                    .and_then(|v| v.trim().parse::<u64>().ok())
            })
            .is_some_and(|n| n > 0)
    }

    /// Kills all processes of the group and removes it without blocking the thread.
    /// If processes are still exiting, the directory stays and will be reused
    /// by the next `create_app` of this app.
    pub fn destroy(&self) {
        kill_all(&self.dir);
        if let Err(e) = fs::remove_dir(&self.dir) {
            tracing::debug!("{} not removed yet: {e}", self.dir.display());
        }
    }
}

/// Changes of a cgroup file (e.g. `memory.events`): the kernel sends `IN_MODIFY`
/// on every new event, so waiting requires no polling.
pub struct EventsWatch {
    fd: tokio::io::unix::AsyncFd<OwnedFd>,
}

impl EventsWatch {
    fn new(path: &Path) -> Result<Self> {
        use rustix::fs::inotify;
        let fd = inotify::init(inotify::CreateFlags::NONBLOCK | inotify::CreateFlags::CLOEXEC).context("inotify_init1")?;
        inotify::add_watch(&fd, path, inotify::WatchFlags::MODIFY).with_context(|| format!("inotify {}", path.display()))?;
        Ok(EventsWatch { fd: tokio::io::unix::AsyncFd::new(fd)? })
    }

    /// Waits for the next file change.
    pub async fn changed(&mut self) -> std::io::Result<()> {
        let mut buf = [0u8; 1024];
        loop {
            let mut guard = self.fd.readable().await?;
            match rustix::io::read(self.fd.get_ref(), &mut buf) {
                Ok(n) if n > 0 => return Ok(()),
                Ok(_) | Err(rustix::io::Errno::AGAIN) => guard.clear_ready(),
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// Metrics of a single process from /proc, when cgroups are unavailable.
pub fn proc_metrics(pid: u32) -> Option<Metrics> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let field = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
    };
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesized process name: utime and stime are the 12th and 13th.
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let ticks: u64 = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
    let hz = rustix::param::clock_ticks_per_second().max(1);
    Some(Metrics {
        memory_bytes: field("VmRSS:")? * 1024,
        memory_max_bytes: None,
        cpu_usage_usec: ticks * 1_000_000 / hz,
        pids: field("Threads:").unwrap_or(1) as u32,
        source: "proc".into(),
    })
}

fn kill_all(dir: &Path) {
    let _ = write(&dir.join("cgroup.kill"), "1");
}

fn write(path: &Path, value: &str) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    f.write_all(value.as_bytes()).with_context(|| format!("writing {value:?} to {}", path.display()))
}
