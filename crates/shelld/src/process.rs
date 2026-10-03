//! Process management without `unsafe`: system calls via `rustix`.
//!
//! Instead of `pre_exec` (which is always `unsafe`), child processes configure themselves
//! after starting: the worker at the start of `shelld worker`, `exec` utilities via
//! the `shelld exec-helper` helper, which then does a plain `exec`.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::Path;

use anyhow::{Context, Result};
use rustix::process::{Pid, Signal};

/// Forbid privilege escalation and die together with shelld (SIGKILL on parent death).
/// Both flags are preserved across `execve`.
pub fn harden_self() -> Result<()> {
    rustix::thread::set_no_new_privs(true).context("PR_SET_NO_NEW_PRIVS")?;
    rustix::process::set_parent_process_death_signal(Some(Signal::KILL)).context("PR_SET_PDEATHSIG")?;
    Ok(())
}

pub fn kill(pid: u32) {
    if let Some(pid) = to_pid(pid) {
        let _ = rustix::process::kill_process(pid, Signal::KILL);
    }
}

/// Kill a process group created by `process_group(0)` (the leader is `pid`).
pub fn kill_group(pid: u32) {
    if let Some(pid) = to_pid(pid) {
        let _ = rustix::process::kill_process_group(pid, Signal::KILL);
    }
}

fn to_pid(pid: u32) -> Option<Pid> {
    Pid::from_raw(i32::try_from(pid).ok()?)
}

/// Helper subcommand name; arguments are parsed manually, without interpreting the utility's options.
pub const EXEC_HELPER: &str = "exec-helper";

/// Arguments for `shelld exec-helper <cgroup|-> <binary> [args…]`.
pub fn exec_helper_args(cgroup: Option<&Path>, binary: &str, argv: &[String]) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![EXEC_HELPER.into(), cgroup.map_or_else(|| "-".into(), |p| p.as_os_str().to_owned()), binary.into()];
    args.extend(argv.iter().map(OsString::from));
    args
}

/// `shelld exec-helper`: enters the app's cgroup, protects itself and replaces itself
/// with the utility. Environment, working directory, stdio and process group are already set by the supervisor.
pub fn run_exec_helper(mut args: impl Iterator<Item = OsString>) -> Result<()> {
    let cgroup = args.next().context("exec-helper: missing cgroup")?;
    let binary = args.next().context("exec-helper: missing binary")?;
    if cgroup != "-" {
        // "0" moves the writing process itself into the group, before exec of the utility, with no race.
        std::fs::write(Path::new(&cgroup).join("cgroup.procs"), "0").context("entering the app's cgroup")?;
    }
    harden_self()?;
    // exec returns only on error.
    let err = std::process::Command::new(&binary).args(args).exec();
    Err(err).with_context(|| format!("exec {}", Path::new(&binary).display()))
}
