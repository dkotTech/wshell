//! What the kernel offers shelld: `shelld check-host` and a warning at start.
//!
//! The minimum is Linux 5.15 (an LTS release): every kernel feature shelld relies on is there,
//! `cgroup.kill` (5.14) being the newest. Linux 6.12 gives the full worker sandbox: Landlock
//! TCP rules (6.7) and its signal and abstract socket scoping (6.12). Older kernels or kernels
//! built without a feature run with less: see the notes of each check.

use std::fs;
use std::path::Path;

use anyhow::{Result, bail};

pub const MINIMUM: (u32, u32) = (5, 15);
pub const FULL_SANDBOX: (u32, u32) = (6, 12);

const CGROUP_FS: &str = "/sys/fs/cgroup";

pub struct Check {
    pub what: &'static str,
    pub ok: bool,
    /// Without it shelld still runs, with less isolation or fewer limits.
    pub optional: bool,
    pub note: String,
}

/// `6.18.42-1-MANJARO`.
fn kernel_release() -> String {
    fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim().to_string()
}

/// `6.18.42-1-MANJARO` → (6, 18).
fn parse_version(release: &str) -> Option<(u32, u32)> {
    let mut parts = release.split(|c: char| !c.is_ascii_digit());
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

pub fn checks() -> Vec<Check> {
    let release = kernel_release();
    let version = parse_version(&release);
    let at_least = |v: (u32, u32)| version.is_some_and(|k| k >= v);
    let mut checks = vec![Check {
        what: "Linux 5.15 or newer",
        ok: at_least(MINIMUM),
        optional: false,
        note: release,
    }];

    let own = own_cgroup();
    let controllers = fs::read_to_string(Path::new(CGROUP_FS).join("cgroup.controllers")).unwrap_or_default();
    let missing: Vec<&str> =
        ["memory", "pids", "cpu"].into_iter().filter(|c| !controllers.split_whitespace().any(|a| a == *c)).collect();
    checks.push(Check {
        what: "cgroup v2 with memory, pids, cpu",
        ok: own.is_some() && missing.is_empty(),
        optional: true,
        note: match (&own, missing.is_empty()) {
            (None, _) => "no unified hierarchy: apps run without OS limits".into(),
            (Some(_), false) => format!("missing {}: their limits are not applied", missing.join(", ")),
            (Some(_), true) => String::new(),
        },
    });
    // The file is in every cgroup but the root one.
    let kill = own.as_ref().filter(|c| *c != Path::new(CGROUP_FS)).map(|c| c.join("cgroup.kill").exists());
    let ok = kill.unwrap_or_else(|| at_least((5, 14)));
    checks.push(Check {
        what: "cgroup.kill (5.14)",
        ok,
        optional: true,
        note: if ok { String::new() } else { "processes `exec` utilities leave behind outlive their app".into() },
    });

    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    let ok = status.lines().any(|l| l.starts_with("Seccomp:"));
    checks.push(Check {
        what: "seccomp filters",
        ok,
        optional: true,
        note: if ok { String::new() } else { "workers run without the system call filter".into() },
    });

    let (userns, why) = user_namespaces();
    checks.push(Check { what: "unprivileged user namespaces", ok: userns, optional: true, note: why });

    let lsm = fs::read_to_string("/sys/kernel/security/lsm");
    let landlock = lsm.as_deref().is_ok_and(|l| l.split(',').any(|m| m.trim() == "landlock"));
    checks.push(Check {
        what: "Landlock",
        ok: landlock && at_least(FULL_SANDBOX),
        optional: true,
        note: match () {
            _ if lsm.is_err() => "unknown: /sys/kernel/security/lsm is unreadable (shelld check-sandbox tells)".into(),
            _ if !landlock => "not enabled (CONFIG_LSM / lsm= boot parameter): workers can read any file".into(),
            _ if at_least(FULL_SANDBOX) => "files, TCP, signals and abstract sockets".into(),
            _ if at_least((6, 7)) => "files and TCP; signals and abstract sockets need 6.12".into(),
            _ => "files only; TCP needs 6.7, signals and abstract sockets 6.12".into(),
        },
    });
    checks
}

/// Whether this process may create cgroups for apps: shelld needs the controllers in its own
/// cgroup and write access to it (`Delegate=yes` in a systemd unit).
fn delegation() -> Check {
    let what = "cgroups delegated to this process";
    let Some(own) = own_cgroup() else {
        return Check { what, ok: false, optional: true, note: "no unified hierarchy".into() };
    };
    let controllers = fs::read_to_string(own.join("cgroup.controllers")).unwrap_or_default();
    let missing: Vec<&str> = ["memory", "pids", "cpu"]
        .into_iter()
        .filter(|c| !controllers.split_whitespace().any(|a| a == *c))
        .collect();
    let writable = rustix::fs::access(&own, rustix::fs::Access::WRITE_OK).is_ok();
    let ok = missing.is_empty() && writable;
    let note = match () {
        _ if ok => String::new(),
        _ if !missing.is_empty() => format!("{} not delegated to {}", missing.join(", "), own.display()),
        _ => format!("{} is not writable", own.display()),
    };
    let note = if ok { note } else { format!("{note}: apps run without OS limits (run shelld with Delegate=yes)") };
    Check { what, ok, optional: true, note }
}

fn own_cgroup() -> Option<std::path::PathBuf> {
    let own = fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = own.lines().find_map(|l| l.strip_prefix("0::"))?;
    Some(Path::new(CGROUP_FS).join(rel.trim_start_matches('/')))
}

/// The sysctls distributions use to turn them off.
fn user_namespaces() -> (bool, String) {
    let read = |p: &str| fs::read_to_string(p).ok().map(|v| v.trim().to_string());
    if read("/proc/sys/user/max_user_namespaces").as_deref() == Some("0") {
        return (false, "user.max_user_namespaces = 0: workers share the host's namespaces".into());
    }
    if read("/proc/sys/kernel/unprivileged_userns_clone").as_deref() == Some("0") {
        return (false, "kernel.unprivileged_userns_clone = 0: workers share the host's namespaces".into());
    }
    if read("/proc/sys/kernel/apparmor_restrict_unprivileged_userns").as_deref() == Some("1") {
        return (false, "kernel.apparmor_restrict_unprivileged_userns = 1 (Ubuntu): allow shelld in an AppArmor profile".into());
    }
    (true, String::new())
}

/// `shelld check-host`. Fails when something required is missing.
pub fn report() -> Result<()> {
    let mut failed = 0;
    let mut checks = checks();
    // At start shelld reports this itself (and it depends on how shelld is started).
    checks.push(delegation());
    for c in checks {
        let mark = match (c.ok, c.optional) {
            (true, _) => "ok  ",
            (false, true) => "less",
            (false, false) => {
                failed += 1;
                "FAIL"
            }
        };
        let note = if c.note.is_empty() { String::new() } else { format!(": {}", c.note) };
        println!("{mark} {}{note}", c.what);
    }
    println!("(shelld check-sandbox tries the worker sandbox itself)");
    if failed > 0 {
        bail!("{failed} requirement(s) not met");
    }
    Ok(())
}

/// At start: one warning per missing feature; nothing is refused.
pub fn warn_at_start() {
    for c in checks().into_iter().filter(|c| !c.ok) {
        let note = if c.note.is_empty() { "not available" } else { &c.note };
        tracing::warn!("host: {}: {note} (shelld check-host)", c.what);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn version() {
        assert_eq!(super::parse_version("6.18.42-1-MANJARO"), Some((6, 18)));
        assert_eq!(super::parse_version("5.15.0-1034-raspi"), Some((5, 15)));
        assert_eq!(super::parse_version("6.1"), Some((6, 1)));
        assert_eq!(super::parse_version(""), None);
    }
}
