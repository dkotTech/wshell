//! Worker: a separate process per app, executing the WASM backend in Wasmtime.
//!
//! The process has no authority of its own: host interfaces are implemented as
//! requests to the supervisor over IPC via stdin/stdout, including standard `wasi:http`.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use tokio::sync::oneshot;
use wasmtime_wasi_http::{WasiBody, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};

use anyhow::{Context, Result, anyhow};
use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Store, StoreLimits, StoreLimitsBuilder};
use shell_core::manifest::BackendKind;
use wasmtime_wasi::p2::bindings::sync::{Command, CommandPre};
use wasmtime_wasi::sockets::SocketAddrUse;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::ipc::{self, FromWorker, Grants, HostCall, HostEvent, HostReply, Init, LogLevel, ToWorker};

/// Bindings of the `plugin` world: the `app` world plus `shell:app/apps`. Apps and plugins
/// run the same way (same exports); an app cannot import `apps` (the install check), and
/// the handle is given only with the `shell.apps` permission.
mod bindings {
    wasmtime::component::bindgen!({
        world: "plugin",
        path: "../../wit",
        with: {
            "shell:app/storage.store": super::StoreHandle,
            "shell:app/exec.command": super::CommandHandle,
            "shell:app/activity.busy": super::BusyHandle,
            "shell:app/apps.registry": super::RegistryHandle,
        },
    });
}

use bindings::exports::shell::app::lifecycle;
use bindings::shell::app::{activity, apps, events, exec, log, metrics, storage};

/// Limit on the body of an outgoing `wasi:http` request (same as in the supervisor).
const MAX_HTTP_BODY: usize = 8 << 20;

pub struct StoreHandle;
pub struct CommandHandle {
    name: String,
}
/// A busy lock; id is the resource number in the table, unique among live handles.
pub struct BusyHandle;
pub struct RegistryHandle;

/// Worker IPC over stdin/stdout. The reader thread dispatches replies to host calls
/// to their waiters: the main WASM thread and `wasi:http` requests, which run
/// on the wasmtime-wasi runtime in parallel with it; other messages go to the
/// main loop's queue.
#[derive(Clone)]
struct Ipc(Arc<IpcInner>);

type Reply = Result<HostReply, String>;

struct IpcInner {
    output: Mutex<std::io::Stdout>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    next_id: AtomicU64,
}

impl Ipc {
    fn start() -> (Ipc, mpsc::Receiver<ToWorker>) {
        let ipc = Ipc(Arc::new(IpcInner {
            output: Mutex::new(std::io::stdout()),
            pending: Mutex::default(),
            next_id: AtomicU64::new(1),
        }));
        let (inbox_tx, inbox) = mpsc::channel();
        let reader = ipc.clone();
        std::thread::spawn(move || {
            let mut input = std::io::stdin();
            loop {
                match ipc::read_sync::<ToWorker>(&mut input) {
                    Ok(ToWorker::HostReply { id, result }) => {
                        if let Some(tx) = reader.0.pending.lock().unwrap().remove(&id) {
                            let _ = tx.send(result);
                        }
                    }
                    Ok(msg) => {
                        if inbox_tx.send(msg).is_err() {
                            break;
                        }
                    }
                    // The supervisor closed the channel: no point in continuing.
                    Err(_) => std::process::exit(0),
                }
            }
        });
        (ipc, inbox)
    }

    fn send(&self, msg: &FromWorker) {
        let mut out = self.0.output.lock().unwrap();
        // stdout is line-buffered but frames are binary, so flush explicitly.
        if ipc::write_sync(&mut *out, msg).and_then(|()| out.flush()).is_err() {
            std::process::exit(0);
        }
    }

    /// Send a host call; the reply arrives on the returned channel.
    fn request(&self, call: HostCall) -> oneshot::Receiver<Reply> {
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.0.pending.lock().unwrap().insert(id, tx);
        self.send(&FromWorker::HostCall { id, call });
        rx
    }

    /// Synchronous host call from the main thread (`shell:app` imports).
    fn host_call(&self, call: HostCall) -> Reply {
        self.request(call).blocking_recv().unwrap_or_else(|_| std::process::exit(0))
    }
}

pub struct Host {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
    ipc: Ipc,
    grants: Grants,
    http: WasiHttpCtx,
    http_hooks: HttpHooks,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

impl WasiHttpView for Host {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView { ctx: &mut self.http, table: &mut self.table, hooks: &mut self.http_hooks }
    }
}

/// `wasi:http`: the outgoing request goes to the supervisor in full, which checks the
/// `net.http` permission (hosts, methods) and performs it. The worker has no network of its own.
struct HttpHooks {
    ipc: Ipc,
    granted: bool,
}

type HttpResponseFuture = Box<
    dyn Future<Output = wasmtime_wasi_http::Result<(http::Response<WasiBody>, Box<dyn Future<Output = wasmtime_wasi_http::Result<()>> + Send>)>>
        + Send,
>;

impl WasiHttpHooks for HttpHooks {
    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        _options: Option<wasmtime_wasi_http::RequestOptions>,
        _fut: Box<dyn Future<Output = wasmtime_wasi_http::Result<()>> + Send>,
    ) -> HttpResponseFuture {
        let (ipc, granted) = (self.ipc.clone(), self.granted);
        Box::new(async move {
            use wasmtime_wasi_http::Error as E;
            if !granted {
                return Err(E::HttpRequestDenied);
            }
            let (parts, body) = request.into_parts();
            let body = Limited::new(body, MAX_HTTP_BODY)
                .collect()
                .await
                .map_err(|e| E::InternalError(Some(format!("request body: {e}"))))?
                .to_bytes();
            let req = ipc::HttpRequest {
                method: parts.method.to_string(),
                url: parts.uri.to_string(),
                headers: parts.headers.iter().filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_string()))).collect(),
                body: (!body.is_empty()).then(|| body.to_vec()),
            };
            let resp = match ipc.request(HostCall::HttpSend(req)).await {
                Ok(Ok(HostReply::Http(Ok(r)))) => r,
                Ok(Ok(HostReply::Http(Err(e)))) => return Err(http_error(e)),
                Ok(Ok(other)) => return Err(E::InternalError(Some(unexpected(other)))),
                Ok(Err(e)) => return Err(E::InternalError(Some(e))),
                Err(_) => return Err(E::InternalError(Some("worker is stopping".into()))),
            };
            let mut builder = http::Response::builder().status(resp.status);
            for (k, v) in &resp.headers {
                builder = builder.header(k, v);
            }
            let body = Full::new(Bytes::from(resp.body)).map_err(|never| match never {}).boxed_unsync();
            let response = builder.body(body).map_err(|e| E::InternalError(Some(e.to_string())))?;
            Ok((response, Box::new(async { Ok(()) }) as Box<dyn Future<Output = _> + Send>))
        })
    }
}

fn http_error(e: ipc::HttpError) -> wasmtime_wasi_http::Error {
    use wasmtime_wasi_http::Error as E;
    match e {
        ipc::HttpError::Denied(_) => E::HttpRequestDenied,
        ipc::HttpError::Timeout => E::ConnectionTimeout,
        ipc::HttpError::Dns(_) => E::DnsError { rcode: None, info_code: None },
        ipc::HttpError::Connection(_) => E::ConnectionRefused,
        ipc::HttpError::TooLarge => E::HttpResponseBodySize(None),
        ipc::HttpError::Other(m) => E::InternalError(Some(m)),
    }
}

fn unexpected(reply: HostReply) -> String {
    format!("unexpected host reply: {reply:?}")
}

impl log::Host for Host {
    fn log(&mut self, level: log::Level, message: String) {
        let level = match level {
            log::Level::Trace => LogLevel::Trace,
            log::Level::Debug => LogLevel::Debug,
            log::Level::Info => LogLevel::Info,
            log::Level::Warn => LogLevel::Warn,
            log::Level::Error => LogLevel::Error,
        };
        self.ipc.send(&FromWorker::Log { level, message });
    }
}

impl events::Host for Host {
    fn emit(&mut self, name: String, payload: String) {
        self.ipc.send(&FromWorker::Emit { name, payload });
    }
}

impl metrics::Host for Host {
    fn publish(&mut self, snapshot: String) -> Result<(), String> {
        self.ipc.host_call(HostCall::MetricsPublish { snapshot }).map(drop)
    }
}

impl activity::Host for Host {
    fn request_stop(&mut self) {
        self.ipc.send(&FromWorker::RequestStop);
    }
}

impl activity::HostBusy for Host {
    fn new(&mut self, reason: String) -> Resource<BusyHandle> {
        // Can only fail if the resource table overflows (2^32 handles).
        let handle = self.table.push(BusyHandle).expect("resource table overflow");
        self.ipc.send(&FromWorker::Busy { id: handle.rep(), reason });
        handle
    }

    fn drop(&mut self, rep: Resource<BusyHandle>) -> wasmtime::Result<()> {
        let id = rep.rep();
        self.table.delete(rep)?;
        self.ipc.send(&FromWorker::BusyDone { id });
        Ok(())
    }
}

impl storage::Host for Host {
    fn open(&mut self) -> Option<Resource<StoreHandle>> {
        if !self.grants.storage {
            return None;
        }
        self.table.push(StoreHandle).ok()
    }
}

impl storage::HostStore for Host {
    fn get(&mut self, _: Resource<StoreHandle>, key: String) -> Result<Option<Vec<u8>>, String> {
        match self.ipc.host_call(HostCall::StorageGet { key })? {
            HostReply::Value(v) => Ok(v),
            r => Err(unexpected(r)),
        }
    }

    fn set(&mut self, _: Resource<StoreHandle>, key: String, value: Vec<u8>) -> Result<(), String> {
        self.ipc.host_call(HostCall::StorageSet { key, value }).map(drop)
    }

    fn delete(&mut self, _: Resource<StoreHandle>, key: String) -> Result<(), String> {
        self.ipc.host_call(HostCall::StorageDelete { key }).map(drop)
    }

    fn keys(&mut self, _: Resource<StoreHandle>) -> Result<Vec<String>, String> {
        match self.ipc.host_call(HostCall::StorageKeys)? {
            HostReply::Keys(k) => Ok(k),
            r => Err(unexpected(r)),
        }
    }

    fn drop(&mut self, rep: Resource<StoreHandle>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl exec::Host for Host {
    fn open(&mut self, name: String) -> Option<Resource<CommandHandle>> {
        if !self.grants.exec.contains(&name) {
            return None;
        }
        self.table.push(CommandHandle { name }).ok()
    }

    fn cancel(&mut self, job: u64) {
        let _ = self.ipc.host_call(HostCall::ExecCancel { job });
    }
}

impl exec::HostCommand for Host {
    fn run(&mut self, cmd: Resource<CommandHandle>, args: Vec<(String, String)>) -> Result<exec::Output, String> {
        let name = self.table.get(&cmd).map_err(|e| e.to_string())?.name.clone();
        match self.ipc.host_call(HostCall::ExecRun { name, args })? {
            HostReply::ExecOutput(o) => Ok(exec::Output {
                exit_code: o.exit_code,
                stdout: o.stdout,
                stderr: o.stderr,
                timed_out: o.timed_out,
            }),
            r => Err(unexpected(r)),
        }
    }

    fn spawn(&mut self, cmd: Resource<CommandHandle>, args: Vec<(String, String)>) -> Result<u64, String> {
        let name = self.table.get(&cmd).map_err(|e| e.to_string())?.name.clone();
        match self.ipc.host_call(HostCall::ExecSpawn { name, args })? {
            HostReply::Job(j) => Ok(j),
            r => Err(unexpected(r)),
        }
    }

    fn drop(&mut self, rep: Resource<CommandHandle>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl apps::Host for Host {
    fn open(&mut self) -> Option<Resource<RegistryHandle>> {
        if !self.grants.apps {
            return None;
        }
        self.table.push(RegistryHandle).ok()
    }
}

impl apps::HostRegistry for Host {
    fn list(&mut self, _: Resource<RegistryHandle>) -> Vec<apps::AppInfo> {
        use shell_core::control::AppState as S;
        let Ok(HostReply::Apps(list)) = self.ipc.host_call(HostCall::AppsList) else {
            return Vec::new();
        };
        list.into_iter()
            .map(|a| apps::AppInfo {
                id: a.id,
                name: a.name,
                version: a.version,
                state: match a.state {
                    S::Stopped => apps::State::Stopped,
                    S::Starting => apps::State::Starting,
                    S::Running => apps::State::Running,
                    S::Suspended => apps::State::Suspended,
                    S::Stopping => apps::State::Stopping,
                    S::Failed => apps::State::Failed,
                },
                has_ui: a.has_ui,
                plugin: a.plugin,
            })
            .collect()
    }

    fn ui_link(&mut self, _: Resource<RegistryHandle>, id: String) -> Result<String, String> {
        match self.ipc.host_call(HostCall::AppsUiLink { id })? {
            HostReply::Url(url) => Ok(url),
            r => Err(unexpected(r)),
        }
    }

    fn drop(&mut self, rep: Resource<RegistryHandle>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

pub fn linker(engine: &wasmtime::Engine) -> Result<Linker<Host>> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_sync(&mut linker)?;
    bindings::Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |s| s)?;
    Ok(linker)
}

/// Type-checks exports/imports without running (used at install time).
pub fn typecheck(engine: &wasmtime::Engine, component: &Component, kind: BackendKind) -> Result<()> {
    let pre = linker(engine)?.instantiate_pre(component)?;
    match kind {
        BackendKind::App | BackendKind::Plugin => drop(bindings::PluginPre::new(pre)?),
        BackendKind::Command => drop(CommandPre::new(pre)?),
    }
    Ok(())
}

/// Entry point of `shelld worker`.
pub fn run() -> Result<()> {
    crate::process::harden_self()?;
    let (ipc, inbox) = Ipc::start();
    let next = || inbox.recv().unwrap_or_else(|_| std::process::exit(0));

    let ToWorker::Init(init) = next() else {
        return Err(anyhow!("worker: expected an Init message"));
    };
    let fuel = init.fuel_per_call;

    let (engine, component, linker) = match prepare(&init) {
        Ok(v) => v,
        Err(e) => {
            ipc.send(&FromWorker::Started(Err(format!("{e:#}"))));
            return Ok(());
        }
    };
    let mut store = new_store(&engine, &init, ipc);
    if init.kind == BackendKind::Command {
        return run_command(store, &component, &linker);
    }
    store.set_fuel(fuel)?;
    let started = match bindings::Plugin::instantiate(&mut store, &component, &linker) {
        Err(e) => Err(format!("component instantiation: {e:#}")),
        Ok(app) => match app.shell_app_lifecycle().call_on_start(&mut store) {
            Ok(Ok(())) => Ok(app),
            Ok(Err(e)) => Err(e),
            Err(trap) => Err(format!("trap in on-start: {trap:#}")),
        },
    };
    let app = match started {
        Ok(app) => {
            store.data_mut().ipc.send(&FromWorker::Started(Ok(())));
            app
        }
        Err(e) => {
            store.data_mut().ipc.send(&FromWorker::Started(Err(e)));
            return Ok(());
        }
    };

    loop {
        let msg = next();
        store.set_fuel(fuel)?;
        match msg {
            ToWorker::Call { id, method, payload } => {
                match app.shell_app_bridge().call_handle(&mut store, &method, &payload) {
                    Ok(result) => store.data_mut().ipc.send(&FromWorker::CallResult { id, result }),
                    Err(trap) => fatal(&mut store, Some(id), trap),
                }
            }
            ToWorker::Event(ev) => {
                if let Err(trap) = app.shell_app_lifecycle().call_on_event(&mut store, &convert_event(ev)) {
                    fatal(&mut store, None, trap);
                }
            }
            ToWorker::Stop => {
                let _ = app.shell_app_lifecycle().call_on_stop(&mut store);
                return Ok(());
            }
            ToWorker::Init(_) | ToWorker::HostReply { .. } => {}
        }
    }
}

/// A program with `main` (`wasi:cli/run`): runs until it exits on its own or is
/// stopped by Shell. There is no bridge or hooks, so supervisor messages are not handled.
fn run_command(mut store: Store<Host>, component: &Component, linker: &Linker<Host>) -> Result<()> {
    // Fuel limits the duration of a single call; a long-lived program makes just one call,
    // so its CPU is limited by the cgroup's cpu.max.
    store.set_fuel(u64::MAX)?;
    let command = match Command::instantiate(&mut store, component, linker) {
        Ok(c) => c,
        Err(e) => {
            store.data().ipc.send(&FromWorker::Started(Err(format!("component instantiation: {e:#}"))));
            return Ok(());
        }
    };
    store.data().ipc.send(&FromWorker::Started(Ok(())));
    match command.wasi_cli_run().call_run(&mut store) {
        Ok(Ok(())) => std::process::exit(0),
        Ok(Err(())) => std::process::exit(1),
        Err(trap) => fatal(&mut store, None, trap),
    }
}

/// After a trap the component instance cannot be used: report and exit.
/// The caller gets only the reason; the full backtrace goes to the log.
fn fatal(store: &mut Store<Host>, call_id: Option<u64>, trap: wasmtime::Error) -> ! {
    let reason = match trap.downcast_ref::<wasmtime::Trap>() {
        Some(t) => format!("trap: {t}"),
        None => format!("trap: {}", trap.root_cause()),
    };
    let ipc = &mut store.data_mut().ipc;
    if let Some(id) = call_id {
        ipc.send(&FromWorker::CallResult { id, result: Err(reason.clone()) });
    }
    ipc.send(&FromWorker::Log { level: LogLevel::Error, message: format!("trap: {trap:?}") });
    ipc.send(&FromWorker::Fatal(reason));
    std::process::exit(70);
}

fn prepare(init: &Init) -> Result<(wasmtime::Engine, Component, Linker<Host>)> {
    let engine = crate::engine::engine()?;
    let component = load_component(&engine, init)?;
    let linker = linker(&engine)?;
    Ok((engine, component, linker))
}

/// Store without preopened directories, environment or network; memory per the app's limit.
fn new_store(engine: &wasmtime::Engine, init: &Init, ipc: Ipc) -> Store<Host> {
    let mut wasi = WasiCtx::builder();
    // The worker's stdout is the IPC channel, so guest output goes to stderr (→ app log).
    wasi.stdout(std::io::stderr()).stderr(std::io::stderr());
    wasi.allow_udp(false).allow_ip_name_lookup(false);
    // net.listen: bind/listen only on declared ports and accepting incoming connections.
    // Outgoing connections are forbidden; access to the program itself is not controlled by Shell.
    let ports = init.grants.listen.clone();
    wasi.allow_tcp(!ports.is_empty());
    wasi.socket_addr_check(move |addr, usage| {
        let allowed = match usage {
            SocketAddrUse::TcpBind | SocketAddrUse::TcpListen => ports.contains(&addr.port()),
            SocketAddrUse::TcpAccept => !ports.is_empty(),
            _ => false,
        };
        Box::pin(async move { allowed })
    });
    let limits = StoreLimitsBuilder::new()
        .memory_size(init.memory_bytes as usize)
        .instances(64)
        .tables(64)
        .memories(8)
        .build();
    let http_hooks = HttpHooks { ipc: ipc.clone(), granted: init.grants.http };
    let host = Host {
        wasi: wasi.build(),
        table: ResourceTable::new(),
        limits,
        ipc,
        grants: init.grants.clone(),
        http: WasiHttpCtx::new(),
        http_hooks,
    };
    let mut store = Store::new(engine, host);
    store.limiter(|h| &mut h.limits);
    store
}

/// The AOT image if it is intact and matches the engine; otherwise compile from `.wasm` (safe but slow).
fn load_component(engine: &wasmtime::Engine, init: &Init) -> Result<Component> {
    let intact = crate::package::sha256_file(Path::new(&init.cwasm_path)).is_ok_and(|h| h == init.cwasm_sha256);
    if intact && let Ok(c) = crate::engine::load_cwasm(engine, Path::new(&init.cwasm_path)) {
        return Ok(c);
    }
    Component::from_file(engine, &init.wasm_path)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("loading {}", init.wasm_path))
}

fn convert_event(ev: HostEvent) -> lifecycle::HostEvent {
    match ev {
        HostEvent::ExecOutput { job, stderr, data } => lifecycle::HostEvent::ExecOutput(lifecycle::ExecOutput {
            job,
            stream: if stderr { lifecycle::StreamKind::Stderr } else { lifecycle::StreamKind::Stdout },
            data,
        }),
        HostEvent::ExecExit { job, exit_code, timed_out } => {
            lifecycle::HostEvent::ExecExit(lifecycle::ExecExit { job, exit_code, timed_out })
        }
        HostEvent::UiVisible(v) => lifecycle::HostEvent::UiVisible(v),
        HostEvent::StopRequested { reason, grace_ms } => {
            use shell_core::control::StopReason as R;
            let reason = match reason {
                R::Idle => lifecycle::StopReason::Idle,
                R::MemoryPressure => lifecycle::StopReason::MemoryPressure,
                R::User => lifecycle::StopReason::User,
                R::SelfRequested => lifecycle::StopReason::SelfRequested,
                R::Shutdown => lifecycle::StopReason::Shutdown,
            };
            lifecycle::HostEvent::StopRequested(lifecycle::StopRequest { reason, grace_ms })
        }
    }
}
