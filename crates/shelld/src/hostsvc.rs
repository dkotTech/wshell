//! Host services. Every call is re-checked against the granted permissions and their
//! parameters: the worker is considered untrusted.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::oneshot;

use shell_core::manifest::BackendKind;

use crate::ipc::{AppEntry, ExecOutput, HostCall, HostEvent, HostReply, HttpError, HttpRequest, HttpResponse, ToWorker};
use crate::supervisor::{Supervisor, Worker};

const MAX_KEY: usize = 256;
const MAX_HTTP_BODY: usize = 8 << 20;

pub async fn handle(sup: &Arc<Supervisor>, worker: &Arc<Worker>, call: HostCall) -> Result<HostReply, String> {
    match call {
        HostCall::StorageGet { key } => {
            let dir = storage_dir(sup, worker)?.0;
            blocking(move || storage_get(&dir, &key)).await
        }
        HostCall::StorageSet { key, value } => {
            let (dir, quota) = storage_dir(sup, worker)?;
            blocking(move || storage_set(&dir, &key, &value, quota)).await
        }
        HostCall::StorageDelete { key } => {
            let dir = storage_dir(sup, worker)?.0;
            blocking(move || storage_delete(&dir, &key)).await
        }
        HostCall::StorageKeys => {
            let dir = storage_dir(sup, worker)?.0;
            blocking(move || storage_keys(&dir)).await
        }
        HostCall::HttpSend(req) => Ok(HostReply::Http(http_send(sup, worker, req).await)),
        HostCall::ExecRun { name, args } => exec_run(sup, worker, &name, &args).await,
        HostCall::ExecSpawn { name, args } => exec_spawn(sup, worker, &name, &args),
        HostCall::ExecCancel { job } => {
            if let Some(kill) = worker.jobs.lock().unwrap().remove(&job) {
                let _ = kill.send(());
            }
            Ok(HostReply::Unit)
        }
        HostCall::MetricsPublish { snapshot } => {
            sup.publish_metrics(&worker.installed.manifest.app.id, &snapshot)?;
            Ok(HostReply::Unit)
        }
        HostCall::AppsList => {
            shell_apps(worker)?;
            Ok(HostReply::Apps(apps_list(sup)))
        }
        HostCall::AppsUiLink { id } => {
            shell_apps(worker)?;
            let url = sup.issue_url(&id).map_err(|e| format!("{e:#}"))?;
            sup.audit.record(&worker.installed.manifest.app.id, "ui-link", &id);
            Ok(HostReply::Url(url))
        }
    }
}

// --- shell.apps (plugins) ---

fn shell_apps(worker: &Worker) -> Result<(), String> {
    let installed = &worker.installed;
    if installed.manifest.backend.kind == BackendKind::Plugin && installed.grants.apps {
        Ok(())
    } else {
        Err("permission shell.apps is not granted".into())
    }
}

fn apps_list(sup: &Supervisor) -> Vec<AppEntry> {
    sup.list()
        .into_iter()
        .map(|a| AppEntry {
            plugin: sup.installed(&a.id).is_some_and(|i| i.manifest.backend.kind == BackendKind::Plugin),
            id: a.id,
            name: a.name,
            version: a.version,
            state: a.state,
            has_ui: a.has_ui,
        })
        .collect()
}

async fn blocking<F>(f: F) -> Result<HostReply, String>
where
    F: FnOnce() -> std::io::Result<HostReply> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

// --- storage.private ---

fn storage_dir(sup: &Supervisor, worker: &Worker) -> Result<(PathBuf, u64), String> {
    let quota_mb = worker.installed.grants.storage_quota_mb.ok_or("storage.private permission not granted")?;
    Ok((sup.paths.app_data(&worker.app_id).join("kv"), u64::from(quota_mb) << 20))
}

fn key_path(dir: &Path, key: &str) -> std::io::Result<PathBuf> {
    if key.is_empty() || key.len() > MAX_KEY {
        return Err(std::io::Error::other(format!("key length must be 1..{MAX_KEY}")));
    }
    Ok(dir.join(crate::util::hex(key.as_bytes())))
}

fn storage_get(dir: &Path, key: &str) -> std::io::Result<HostReply> {
    match std::fs::read(key_path(dir, key)?) {
        Ok(v) => Ok(HostReply::Value(Some(v))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HostReply::Value(None)),
        Err(e) => Err(e),
    }
}

fn storage_set(dir: &Path, key: &str, value: &[u8], quota: u64) -> std::io::Result<HostReply> {
    let path = key_path(dir, key)?;
    std::fs::create_dir_all(dir)?;
    let current = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let used = dir_size(dir)?;
    let overhead = key.len() as u64;
    if used - current + value.len() as u64 + overhead > quota {
        return Err(std::io::Error::other(format!("storage quota exceeded ({} KB)", quota >> 10)));
    }
    crate::util::write_atomic(&path, value)?;
    Ok(HostReply::Unit)
}

fn storage_delete(dir: &Path, key: &str) -> std::io::Result<HostReply> {
    match std::fs::remove_file(key_path(dir, key)?) {
        Ok(()) => Ok(HostReply::Unit),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HostReply::Unit),
        Err(e) => Err(e),
    }
}

fn storage_keys(dir: &Path) -> std::io::Result<HostReply> {
    let mut keys = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return Ok(HostReply::Keys(keys)) };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(bytes) = name.to_str().and_then(crate::util::unhex) else { continue };
        if let Ok(k) = String::from_utf8(bytes) {
            keys.push(k);
        }
    }
    keys.sort();
    Ok(HostReply::Keys(keys))
}

fn dir_size(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(dir)?.flatten() {
        total += entry.metadata()?.len();
    }
    Ok(total)
}

// --- net.http ---

async fn http_send(sup: &Supervisor, worker: &Worker, req: HttpRequest) -> Result<HttpResponse, HttpError> {
    let deny = |reason: String| {
        sup.audit.record(&worker.app_id, "deny-call", &format!("net.http {reason}"));
        worker.log.write("shell", &format!("net.http: {reason}"));
        HttpError::Denied(reason)
    };
    let grant = worker.installed.grants.http.as_ref().ok_or_else(|| deny("permission not granted".into()))?;

    let url = reqwest::Url::parse(&req.url).map_err(|e| HttpError::Other(format!("invalid URL: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(deny(format!("scheme {} is not allowed", url.scheme())));
    }
    let host = url.host_str().ok_or_else(|| HttpError::Other("URL without a host".into()))?.to_ascii_lowercase();
    if !grant.hosts.iter().any(|h| host_matches(h, &host)) {
        return Err(deny(format!("host {host} is not allowed")));
    }
    let method = req.method.to_ascii_uppercase();
    if !grant.methods.contains(&method) {
        return Err(deny(format!("method {method} is not allowed")));
    }
    let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| HttpError::Other(e.to_string()))?;

    let mut builder = sup.http_client.request(method, url);
    for (k, v) in &req.headers {
        let lower = k.to_ascii_lowercase();
        if matches!(lower.as_str(), "host" | "connection" | "content-length" | "transfer-encoding" | "upgrade") {
            continue;
        }
        builder = builder.header(k, v);
    }
    if let Some(body) = req.body {
        if body.len() > MAX_HTTP_BODY {
            return Err(HttpError::TooLarge);
        }
        builder = builder.body(body);
    }

    let mut resp = builder.send().await.map_err(classify)?;
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_string())))
        .collect();
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(classify)? {
        if body.len() + chunk.len() > MAX_HTTP_BODY {
            return Err(HttpError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(HttpResponse { status, headers, body })
}

fn classify(e: reqwest::Error) -> HttpError {
    let text = format!("{e:#}");
    if e.is_timeout() {
        HttpError::Timeout
    } else if e.is_connect() && text.contains("dns error") {
        HttpError::Dns(text)
    } else if e.is_connect() {
        HttpError::Connection(text)
    } else {
        HttpError::Other(text)
    }
}

/// `pattern` is already lowercase (see `package::Grants`).
fn host_matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => host.len() > suffix.len() && host.ends_with(suffix) && host[..host.len() - suffix.len()].ends_with('.'),
        None => pattern == host,
    }
}

// --- exec ---

/// Accounting of concurrent runs; released on drop.
struct ExecSlot(Arc<Worker>);

impl ExecSlot {
    fn acquire(sup: &Supervisor, worker: &Arc<Worker>) -> Result<Self, String> {
        let max = sup.cfg.exec.max_concurrent as usize;
        let prev = worker.active_exec.fetch_add(1, Ordering::Relaxed);
        if prev >= max {
            worker.active_exec.fetch_sub(1, Ordering::Relaxed);
            return Err(format!("concurrent run limit exceeded ({max})"));
        }
        Ok(ExecSlot(worker.clone()))
    }
}

impl Drop for ExecSlot {
    fn drop(&mut self) {
        self.0.active_exec.fetch_sub(1, Ordering::Relaxed);
    }
}

fn prepare(sup: &Supervisor, worker: &Arc<Worker>, name: &str, args: &[(String, String)]) -> Result<(tokio::process::Child, Duration, ExecSlot), String> {
    let grant = worker.installed.grants.exec.get(name).ok_or_else(|| format!("permission exec:{name} not granted"))?;
    let spec = &grant.spec;
    let argv = grant.template.render(args).map_err(|e| {
        sup.audit.record(&worker.app_id, "deny-call", &format!("exec:{name} invalid arguments"));
        format!("{e:#}")
    })?;
    let slot = ExecSlot::acquire(sup, worker)?;

    // Via `shelld exec-helper`: it enters the app's cgroup itself, protects itself
    // and replaces itself with the utility, without pre_exec. PID and process group stay the same.
    let helper = std::env::current_exe().map_err(|e| e.to_string())?;
    let cgroup = worker.cgroup.as_ref().map(|c| c.dir());
    let mut cmd = tokio::process::Command::new(helper);
    cmd.args(crate::process::exec_helper_args(cgroup, &spec.binary, &argv))
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LANG", "C.UTF-8")
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let child = cmd.spawn().map_err(|e| format!("running {}: {e}", spec.binary))?;
    worker.activity.touch();
    worker.log.write("shell", &format!("exec:{name} pid {}", child.id().unwrap_or(0)));
    Ok((child, Duration::from_secs(spec.timeout_s), slot))
}

fn kill_group(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        crate::process::kill_group(pid);
    }
}

/// Reads a stream until EOF, passing each chunk read to `on_chunk`.
async fn pump(mut pipe: impl AsyncRead + Unpin, mut on_chunk: impl FnMut(&[u8])) {
    let mut buf = [0u8; 4096];
    while let Ok(n) = pipe.read(&mut buf).await {
        if n == 0 {
            break;
        }
        on_chunk(&buf[..n]);
    }
}

async fn read_capped(pipe: impl AsyncRead + Unpin, cap: usize) -> Vec<u8> {
    let mut out = Vec::new();
    pump(pipe, |chunk| {
        let room = cap.saturating_sub(out.len());
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
    })
    .await;
    out
}

async fn exec_run(sup: &Supervisor, worker: &Arc<Worker>, name: &str, args: &[(String, String)]) -> Result<HostReply, String> {
    let (mut child, timeout, _slot) = prepare(sup, worker, name, args)?;
    let cap = sup.cfg.exec.max_output_kb as usize * 1024;
    let stdout = tokio::spawn(read_capped(child.stdout.take().unwrap(), cap));
    let stderr = tokio::spawn(read_capped(child.stderr.take().unwrap(), cap));
    let (exit_code, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => (status.ok().and_then(|s| s.code()), false),
        Err(_) => {
            kill_group(&child);
            let _ = child.wait().await;
            (None, true)
        }
    };
    worker.activity.touch();
    Ok(HostReply::ExecOutput(ExecOutput {
        exit_code,
        stdout: stdout.await.unwrap_or_default(),
        stderr: stderr.await.unwrap_or_default(),
        timed_out,
    }))
}

fn exec_spawn(sup: &Arc<Supervisor>, worker: &Arc<Worker>, name: &str, args: &[(String, String)]) -> Result<HostReply, String> {
    let (mut child, timeout, slot) = prepare(sup, worker, name, args)?;
    let job = sup.next_job_id();
    let (kill_tx, kill_rx) = oneshot::channel();
    worker.jobs.lock().unwrap().insert(job, kill_tx);

    let cap = sup.cfg.exec.max_output_kb as usize * 1024;
    // Forwarded output limit shared by stdout and stderr.
    let budget = Arc::new(AtomicUsize::new(cap));
    let forward = |stderr: bool| {
        let worker = worker.clone();
        let budget = budget.clone();
        move |chunk: &[u8]| {
            let n = chunk.len();
            let take = budget
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| Some(left - n.min(left)))
                .map_or(0, |left| n.min(left));
            if take > 0 {
                let data = chunk[..take].to_vec();
                worker.send(ToWorker::Event(HostEvent::ExecOutput { job, stderr, data }));
            }
        }
    };
    let readers = [
        tokio::spawn(pump(child.stdout.take().unwrap(), forward(false))),
        tokio::spawn(pump(child.stderr.take().unwrap(), forward(true))),
    ];

    let worker = worker.clone();
    tokio::spawn(async move {
        let _slot = slot;
        let timeout = tokio::time::sleep(timeout);
        let (exit_code, timed_out) = tokio::select! {
            status = child.wait() => (status.ok().and_then(|s| s.code()), false),
            _ = timeout => {
                kill_group(&child);
                let _ = child.wait().await;
                (None, true)
            }
            _ = kill_rx => {
                kill_group(&child);
                let _ = child.wait().await;
                (None, false)
            }
        };
        worker.jobs.lock().unwrap().remove(&job);
        worker.activity.touch();
        // The exit event comes strictly after all output.
        for r in readers {
            let _ = tokio::time::timeout(Duration::from_secs(1), r).await;
        }
        worker.send(ToWorker::Event(HostEvent::ExecExit { job, exit_code, timed_out }));
    });
    Ok(HostReply::Job(job))
}

#[cfg(test)]
mod tests {
    use super::host_matches;

    #[test]
    fn host_patterns() {
        assert!(host_matches("api.example.com", "api.example.com"));
        assert!(!host_matches("api.example.com", "xapi.example.com"));
        assert!(host_matches("*.example.com", "a.example.com"));
        assert!(!host_matches("*.example.com", "example.com"));
        assert!(!host_matches("*.example.com", "badexample.com"));
    }
}
