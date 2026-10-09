//! Worker sandbox: kernel-level limits for the case a guest escapes Wasmtime.
//!
//! The worker already has no authority of its own (host calls go to the supervisor over IPC),
//! but the process itself could still do whatever the shelld user can. These layers take that
//! away:
//!
//! - **Namespaces.** A user namespace (no capabilities outside it), its own IPC and, without
//!   `net.listen`, its own empty network: no interfaces, no abstract UNIX sockets of the host.
//! - **Landlock.** Files: reading the app's package directory only. TCP: bind only on the
//!   `net.listen` ports, no connect. No signals or abstract UNIX sockets outside the sandbox.
//! - **seccomp.** System calls a worker never needs fail with `EPERM`: exec, ptrace, mount,
//!   bpf, kernel modules, new namespaces, signals to other processes, outgoing connections,
//!   non-IP sockets.
//!
//! Applied at the very start of `shelld worker`, while the process is single-threaded
//! (`unshare` of a user namespace requires that); every later thread inherits it.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use shell_core::config::SandboxMode;

pub struct Spec<'a> {
    pub mode: SandboxMode,
    /// The only directory the worker may read: the component and its AOT image.
    pub pkg_dir: &'a Path,
    /// `net.listen` ports; with none, the worker gets its own empty network.
    pub listen: &'a [u16],
}

/// Returns the layers the kernel lacks (`auto`); with `required` they are an error.
pub fn enter(spec: &Spec) -> Result<Vec<String>> {
    if spec.mode == SandboxMode::Off {
        return Ok(Vec::new());
    }
    let own_net = spec.listen.is_empty();
    let mut missing = Vec::new();
    if let Err(e) = namespaces(own_net) {
        missing.push(format!("namespaces: {e:#}"));
    }
    if let Err(e) = landlock(spec) {
        missing.push(format!("Landlock: {e:#}"));
    }
    if let Err(e) = seccomp(own_net) {
        missing.push(format!("seccomp: {e:#}"));
    }
    if spec.mode == SandboxMode::Required && !missing.is_empty() {
        bail!("sandbox (mode = \"required\"): {}", missing.join("; "));
    }
    Ok(missing)
}

/// `shelld check-sandbox`: enters the sandbox of an app without `net.listen` and tries what
/// it forbids. Shows what this kernel supports.
pub fn check() -> Result<()> {
    // shelld's own directory plays the package directory: nothing to create and clean up.
    let exe = std::env::current_exe()?;
    let pkg = exe.parent().context("shelld has no directory")?;
    let parent = rustix::process::getppid();
    let missing = enter(&Spec { mode: SandboxMode::Auto, pkg_dir: pkg, listen: &[] })?;
    for m in &missing {
        println!("not applied: {m}");
    }
    if missing.is_empty() {
        println!("all layers applied: namespaces, Landlock, seccomp");
    }

    let probes: [(&str, bool, std::io::Result<()>); 6] = [
        ("read the package directory", true, std::fs::read(&exe).map(drop)),
        ("read /etc/passwd", false, std::fs::read("/etc/passwd").map(drop)),
        ("write the temp directory", false, std::fs::write(std::env::temp_dir().join("wshell-probe"), "x")),
        ("run /bin/true", false, std::process::Command::new("/bin/true").status().map(drop)),
        ("open a TCP connection", false, std::net::TcpStream::connect("127.0.0.1:1").map(drop)),
        (
            "signal the parent process",
            false,
            parent.map_or(Ok(()), |p| rustix::process::test_kill_process(p).map_err(Into::into)),
        ),
    ];
    let mut wrong = 0;
    for (what, allowed, result) in probes {
        let verdict = match (&result, allowed) {
            (Ok(()), true) => "allowed".to_string(),
            (Err(e), false) => format!("denied ({e})"),
            (Ok(()), false) => {
                wrong += 1;
                "ALLOWED".to_string()
            }
            (Err(e), true) => {
                wrong += 1;
                format!("DENIED ({e})")
            }
        };
        println!("  {what}: {verdict}");
    }
    if wrong > 0 {
        bail!("{wrong} probe(s) did not behave as the sandbox should");
    }
    Ok(())
}

fn namespaces(own_net: bool) -> Result<()> {
    use nix::sched::{CloneFlags as F, unshare};
    // The user namespace comes first and owns the others, so they need no privileges. Even
    // under a root shelld the worker then has no capabilities outside it.
    let mut flags = F::CLONE_NEWUSER | F::CLONE_NEWIPC;
    if own_net {
        flags |= F::CLONE_NEWNET;
    }
    unshare(flags).context("unshare (are unprivileged user namespaces disabled?)")?;
    Ok(())
}

fn landlock(spec: &Spec) -> Result<()> {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, NetPort, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope,
        path_beneath_rules,
    };
    // Best effort: what an older kernel lacks is dropped and reported as partial.
    let abi = ABI::V6;
    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .handle_access(AccessNet::from_all(abi))?
        .scope(Scope::from_all(abi))?
        .create()?
        .add_rules(path_beneath_rules([spec.pkg_dir], AccessFs::from_read(abi)))?;
    for &port in spec.listen {
        ruleset = ruleset.add_rule(NetPort::new(port, AccessNet::BindTcp))?;
    }
    let status = ruleset.restrict_self()?;
    match status.ruleset {
        RulesetStatus::FullyEnforced => Ok(()),
        RulesetStatus::PartiallyEnforced => bail!("partially enforced ({:?})", status.landlock),
        RulesetStatus::NotEnforced => bail!("not supported by the kernel ({:?})", status.landlock),
    }
}

/// Never needed by a worker; the kernel's own checks stay in place for everything else.
const DENIED: &[libc::c_long] = &[
    libc::SYS_execve,
    libc::SYS_execveat,
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_kcmp,
    libc::SYS_pidfd_open,
    libc::SYS_pidfd_getfd,
    libc::SYS_pidfd_send_signal,
    libc::SYS_tkill,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    libc::SYS_move_mount,
    libc::SYS_open_tree,
    libc::SYS_fsopen,
    libc::SYS_fsmount,
    libc::SYS_fsconfig,
    libc::SYS_fspick,
    libc::SYS_mount_setattr,
    libc::SYS_setns,
    libc::SYS_unshare,
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    libc::SYS_userfaultfd,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_kexec_load,
    libc::SYS_kexec_file_load,
    libc::SYS_reboot,
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_acct,
    libc::SYS_quotactl,
    libc::SYS_syslog,
    libc::SYS_vhangup,
    libc::SYS_settimeofday,
    libc::SYS_clock_settime,
    libc::SYS_clock_adjtime,
    libc::SYS_adjtimex,
    libc::SYS_sethostname,
    libc::SYS_setdomainname,
    libc::SYS_open_by_handle_at,
    libc::SYS_name_to_handle_at,
    libc::SYS_fanotify_init,
    libc::SYS_mknodat,
    // Outgoing connections: wasi:http goes through the supervisor, net.listen only accepts.
    libc::SYS_connect,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_mknod,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_iopl,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_ioperm,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_modify_ldt,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_uselib,
];

/// Namespace flags of `clone`: threads never need them.
const NEW_NAMESPACES: &[libc::c_int] = &[
    libc::CLONE_NEWNS,
    libc::CLONE_NEWCGROUP,
    libc::CLONE_NEWUTS,
    libc::CLONE_NEWIPC,
    libc::CLONE_NEWUSER,
    libc::CLONE_NEWPID,
    libc::CLONE_NEWNET,
];

fn seccomp(own_net: bool) -> Result<()> {
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen as Len, SeccompCmpOp as Op, SeccompCondition as Cond,
        SeccompFilter, SeccompRule, TargetArch,
    };
    let arch = TargetArch::try_from(std::env::consts::ARCH)
        .map_err(|_| anyhow!("not supported on {}", std::env::consts::ARCH))?;
    let pid = u64::from(std::process::id());

    let mut deny: BTreeMap<i64, Vec<SeccompRule>> = DENIED.iter().map(|&n| (n, Vec::new())).collect();
    // Signals only to itself (pid 0 and negative pids are process groups, shelld's among them).
    let not_self = || SeccompRule::new(vec![Cond::new(0, Len::Dword, Op::Ne, pid)?]);
    for n in [libc::SYS_kill, libc::SYS_tgkill, libc::SYS_rt_sigqueueinfo, libc::SYS_rt_tgsigqueueinfo] {
        deny.insert(n, vec![not_self()?]);
    }
    let clone_rules = NEW_NAMESPACES
        .iter()
        .map(|&f| SeccompRule::new(vec![Cond::new(0, Len::Qword, Op::MaskedEq(f as u64), f as u64)?]))
        .collect::<Result<_, _>>()?;
    deny.insert(libc::SYS_clone, clone_rules);
    // TCP over IP only (net.listen); without it no sockets at all.
    let socket = if own_net {
        Vec::new()
    } else {
        vec![SeccompRule::new(vec![
            Cond::new(0, Len::Dword, Op::Ne, libc::AF_INET as u64)?,
            Cond::new(0, Len::Dword, Op::Ne, libc::AF_INET6 as u64)?,
        ])?]
    };
    deny.insert(libc::SYS_socket, socket);

    let eperm = SeccompAction::Errno(libc::EPERM as u32);
    let filter: BpfProgram = SeccompFilter::new(deny, SeccompAction::Allow, eperm, arch)?.try_into()?;
    // clone3 passes its flags in memory, out of a filter's reach: ENOSYS makes glibc fall back
    // to clone, whose flags are checked above.
    let enosys = SeccompAction::Errno(libc::ENOSYS as u32);
    let clone3 = BTreeMap::from([(libc::SYS_clone3, Vec::new())]);
    let clone3: BpfProgram = SeccompFilter::new(clone3, SeccompAction::Allow, enosys, arch)?.try_into()?;
    seccompiler::apply_filter(&filter)?;
    seccompiler::apply_filter(&clone3)?;
    Ok(())
}
