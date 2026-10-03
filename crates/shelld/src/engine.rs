//! Wasmtime configuration shared by AOT compilation and execution (worker).
//!
//! Compilation runs in a separate process (`shelld compile`): Cranelift takes hundreds of MB and
//! all cores for a large component (CPython is ~18 MB of WASM). A separate process at nice 19
//! with a limited number of threads does not slow down the system, and its memory goes back to
//! the OS when it exits instead of staying in the supervisor's allocator.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail, ensure};
use shell_core::manifest::{BackendKind, Manifest, permission_for_interface};
use wasmtime::component::Component;
use wasmtime::{Config, Engine};

pub fn engine() -> Result<Engine> {
    let mut cfg = Config::new();
    cfg.wasm_component_model(true);
    // Fuel limits the duration of a single call without background wakeup timers.
    cfg.consume_fuel(true);
    cfg.cranelift_opt_level(wasmtime::OptLevel::SpeedAndSize);
    Ok(Engine::new(&cfg)?)
}

/// Subcommand name of the compiler process.
pub const COMPILE: &str = "compile";

/// Compiles `wasm` into the AOT image `cwasm` in a child process. Blocking.
pub fn compile(wasm: &Path, cwasm: &Path, threads: usize) -> Result<()> {
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg(COMPILE).arg(wasm).arg(cwasm).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    if threads > 0 {
        // Wasmtime compiles functions on the global rayon pool.
        cmd.env("RAYON_NUM_THREADS", threads.to_string());
    }
    let out = cmd.output().context("starting the compiler process")?;
    ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr).trim());
    Ok(())
}

/// `shelld compile <wasm> <cwasm>`: the compiler process.
pub fn run_compile(wasm: &Path, cwasm: &Path) -> Result<()> {
    crate::process::harden_self()?;
    // Below everything else: an install must not slow down the device or running apps.
    // Threads created later (the rayon pool) inherit the priority.
    rustix::process::nice(19).context("nice")?;
    let component = Component::from_file(&engine()?, wasm).map_err(anyhow::Error::from)?;
    crate::util::write_atomic(cwasm, &component.serialize()?)?;
    Ok(())
}

/// Loads an AOT image. The only `unsafe` in shelld: Wasmtime cannot verify
/// precompiled machine code and trusts that it produced it itself.
#[allow(unsafe_code)]
pub fn load_cwasm(engine: &Engine, cwasm: &Path) -> Result<Component> {
    // SAFETY: the image is created by `shelld compile` in the data directory with 0700
    // permissions; before a worker loads it, the SHA-256 recorded at install time is checked.
    // Engine compatibility (version, settings) is checked by Wasmtime itself.
    Ok(unsafe { Component::deserialize_file(engine, cwasm) }?)
}

/// WASI interfaces that are safe without a separate permission: the worker provides no
/// preopened directories, environment or network.
const FREE_WASI: &[&str] = &[
    "wasi:cli/environment",
    "wasi:cli/exit",
    "wasi:cli/stdin",
    "wasi:cli/stdout",
    "wasi:cli/stderr",
    "wasi:cli/terminal-input",
    "wasi:cli/terminal-output",
    "wasi:cli/terminal-stdin",
    "wasi:cli/terminal-stdout",
    "wasi:cli/terminal-stderr",
    "wasi:io/error",
    "wasi:io/poll",
    "wasi:io/streams",
    "wasi:clocks/monotonic-clock",
    "wasi:clocks/wall-clock",
    "wasi:random/random",
    "wasi:random/insecure",
    "wasi:random/insecure-seed",
    "wasi:filesystem/types",
    "wasi:filesystem/preopens",
    // Some language runtimes (CPython) import sockets whether they use them or not.
    // Access is decided at runtime: without `net.listen` every socket operation is denied.
    "wasi:sockets/network",
    "wasi:sockets/instance-network",
    "wasi:sockets/tcp",
    "wasi:sockets/tcp-create-socket",
    "wasi:sockets/udp",
    "wasi:sockets/udp-create-socket",
    "wasi:sockets/ip-name-lookup",
];

const FREE_SHELL: &[&str] = &["shell:app/log", "shell:app/events", "shell:app/activity", "shell:app/metrics"];
/// Required exports by backend kind.
fn required_exports(kind: BackendKind) -> &'static [&'static str] {
    match kind {
        BackendKind::App | BackendKind::Plugin => &["shell:app/lifecycle", "shell:app/bridge"],
        BackendKind::Command => &["wasi:cli/run"],
    }
}

fn strip_version(name: &str) -> &str {
    name.split_once('@').map_or(name, |(n, _)| n)
}

/// Static check: every import is either safe or covered by a permission from the manifest.
pub fn check_component(engine: &Engine, component: &Component, manifest: &Manifest) -> Result<()> {
    let ty = component.component_type();
    let mut errors = Vec::new();

    for (name, _) in ty.imports(engine) {
        let bare = strip_version(name);
        if FREE_WASI.contains(&bare) || FREE_SHELL.contains(&bare) {
            continue;
        }
        match permission_for_interface(bare) {
            Some(perm) if manifest.permissions.iter().any(|p| p.kind.id() == perm) => {}
            Some(perm) => errors.push(format!("import {name} requires permission {perm}, which is not declared in the manifest")),
            None => errors.push(format!("import {name} is not allowed")),
        }
    }

    let exports: BTreeSet<&str> = ty.exports(engine).map(|(n, _)| strip_version(n)).collect();
    for want in required_exports(manifest.backend.kind) {
        if !exports.contains(want) {
            errors.push(format!("component does not export {want}"));
        }
    }

    if !errors.is_empty() {
        bail!("component rejected:\n  - {}", errors.join("\n  - "));
    }
    Ok(())
}
