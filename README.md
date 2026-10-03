# wshell

A shell for running isolated WASM applications with a web UI on Linux devices.
Specification: "Specification: a shell for running WASM applications" (v0.1).

## Contents

| Path | What it is |
| --- | --- |
| `wit/shell-app.wit` | WIT worlds `shell:app/app@0.1.0` (applications) and `shell:app/plugin@0.1.0` (plugins) |
| `crates/shell-core` | `app.toml` manifest, configuration, `exec` templates, `shellctl` protocol |
| `crates/shelld` | Supervisor, worker (Wasmtime), host services, UI HTTP server and bridge, dashboard |
| `crates/shellctl` | Management CLI over a Unix socket |
| `render` | Separate headless HTML renderer (Servo / WPE WebKit): streams pages as frames over WebSocket, home screen from the launcher plugin; not built by default: `cargo build -p render-rs`, runs independently of `shelld` |
| `dashboard` | Dashboard: Preact + `@preact/signals`, Vite, TypeScript |
| `apps/ping` | Example: ping/tracepath with streaming output to the UI |
| `apps/fetch` | Example: HTTP requests via standard `wasi:http` (`wstd`) |
| `apps/hello-server` | Example program with `main`: its own HTTP server on a port from `net.listen` |
| `apps/hello-axum` | The same with a web framework: axum on single-threaded tokio |
| `apps/hello-go` | Experiment: a Go `net/http` server (TinyGo) via a hand-written network driver |
| `apps/todo-py` | Example in Python (`componentize-py`): a todo list with UI, state in `storage.private` |
| `apps/stress` | Test application for checking limits (fuel, memory) |
| `plugins/launcher` | Plugin: the device's home screen for the Renderer (list of apps, opening their UI) |

## Build and run

```sh
rustup target add wasm32-wasip2
cargo build                        # also builds the dashboard (npm), see below
./scripts/build-apps.sh            # → target/apps/*.pkg
./scripts/dev-run.sh               # shelld in a user scope with a delegated cgroup
./scripts/eco-run.sh               # or: release build with the `strict` power profile (data in target/eco)
./scripts/demo.sh                  # demo: shelld + Renderer + example apps, prints links (--servo, --clean)
```

In another terminal:

```sh
shellctl install target/apps/ping.pkg      # shows permissions and asks about optional ones
shellctl start org.wshell.ping --open      # starts and opens the UI (without --open, prints the link)
shellctl list
shellctl logs org.wshell.ping -f
shellctl call org.wshell.ping capabilities # call the backend bypassing the UI
shellctl url org.wshell.ping               # new one-time link
shellctl stop org.wshell.ping
```

A link like `http://org.wshell.ping.localhost:8470/_shell/auth?token=…` is opened in a browser
on the same device. To check the effective configuration: `shelld check-config`.

## Dashboard

```sh
shellctl dashboard --open          # opens the dashboard in a browser (without --open, prints the link)
```

The dashboard runs on a separate port (`[dashboard] port = 8471`, loopback only) and shows
applications roughly like `kubectl get pods`: status, number of starts and failures, age, CPU, memory
relative to the limit, PID. From it you can start, stop, restart and remove
applications, open their UI and watch the log in real time. A package can be installed by
dragging and dropping a `.pkg`; before installing, the dashboard shows the permissions and lets you choose optional ones.
The reason for the last failure (e.g. `trap: all fuel consumed`) is visible in the application card.

- **Access.** One-time token → `HttpOnly; SameSite=Strict` cookie, strict `Host` check;
  mutating requests require the dashboard `Origin`. Dashboard logins are written to the audit log.
- **Power.** The dashboard polls `/api/overview` every 2 s only while the tab is visible; the log
  is streamed (SSE) while the "Log" tab is open.
- **Metrics.** Taken from the cgroup (`memory.current`, `cpu.stat`, `pids.current`), and without a
  delegated cgroup, from `/proc/<pid>`.
- Disabled with `[dashboard] enabled = false` in the config or by building without the `dashboard` Cargo feature.

**Frontend build.** With the `dashboard` feature, `crates/shelld/build.rs` runs `npm ci` (if there is no
`node_modules`) and `npm run build` in `dashboard/`; the resulting `dashboard/dist` is embedded in the binary
(`rust-embed`). `SKIP_DASHBOARD_BUILD=1` uses an already built `dist`. Building without the `dashboard` feature
does not require Node.js.

**UI development with hot reload:**

```sh
./scripts/dev-run.sh                          # shelld with the dashboard on :8471
cd dashboard && npm run dev                   # Vite on :5173, /api and /_auth are proxied to shelld
shellctl dashboard                            # get a token and open http://localhost:5173/_auth?token=…
```

## Plugins

A plugin is a WASM package that extends Shell itself, the way apps extend the device. Shell
does not depend on any plugin: without them it is complete (apps, UI, dashboard, `shellctl`).

- **The same sandbox as apps.** `kind = "plugin"`, world `shell:app/plugin@0.1.0` = the
  `shell:app/app` world (hooks, bridge, UI, `storage`, `exec`, …) **plus** interfaces that give
  access to Shell. They sit behind `shell.*` permissions, which only plugins may declare: an
  app stays isolated from other apps. The permissions are shown and granted at install, like any
  other; every call goes to the supervisor, which re-checks the grant.
- **`shell.apps`** (`shell:app/apps`): the installed apps and their state, and one-time links
  to an app's UI (like `shellctl url`; each link is written to the audit log).
- Plugins are listed with a `plugin` tag in the dashboard and `shellctl list`;
  `[features] plugins = false` forbids installing and starting them.
- Built by `scripts/build-apps.sh` from `plugins/` into `target/plugins/*.pkg`.

### Launcher (`plugins/launcher`) and the Renderer

The device screen is driven by the Renderer (`render/`), a separate process: Shell only serves
UI over HTTP. Its home screen is the **launcher plugin** `org.wshell.launcher`:

- Its UI is an ordinary plugin page `http://org.wshell.launcher.localhost:8470/`: the list of
  apps with a UI (arrows + Enter, or a click), via `shell.call("apps")` to its backend. Opening an
  app asks the backend for a one-time link (`shell.apps`), and the page navigates there: the
  Renderer's webview becomes the app. An app that left the screen closes its events WebSocket
  and Shell suspends it as "UI hidden".
- The Renderer opens the launcher and brings it back (the `AppSwitch` key) with one-time links to
  the plugin's UI. It gets them from an **extra control socket** that allows nothing else:

  ```toml
  [control.sockets.render]            # → <runtime_dir>/render.sock
  mode = 0o660                        # the Renderer runs as another user in Shell's group
  allow = ["url:org.wshell.launcher"]
  ```

  Extra sockets are a general mechanism: the same protocol as `control.sock`, only the listed
  requests (`<request>` or `<request>:<app-id>`); the runtime directory is 0711 so that another
  user can reach their socket.

## Metrics (Prometheus)

`/metrics` in the Prometheus text format for a Prometheus or an agent on the device (`prometheus
--agent`, vmagent). Computed at scrape, nothing runs between scrapes; off by default:

```toml
[metrics]
enabled = true
listen = "127.0.0.1:9470"      # off loopback only with `token` (Authorization: Bearer)
max_series_per_app = 200
max_snapshot_kb = 256
```

```yaml
# prometheus.yml
scrape_configs:
  - job_name: wshell
    scrape_interval: 30s          # 60s on battery
    static_configs: [{ targets: ["127.0.0.1:9470"] }]
```

- **Shell:** `wshell_build_info`, `wshell_start_time_seconds`, `wshell_cgroups`, apps' memory and its
  budget; shelld itself (the supervisor, without the workers): `wshell_process_resident_memory_bytes`,
  `wshell_process_cpu_seconds_total`, `wshell_process_threads`, `wshell_process_open_fds`; per app (`app` label): `wshell_app_info`, `wshell_app_state` (one-hot),
  `wshell_app_starts_total`, `wshell_app_failures_total`, memory and its limit, CPU, processes,
  `busy` locks.
- **Apps' own metrics** (`shell:app/metrics`, no permission needed): the app keeps its metrics
  however it likes and publishes the **whole snapshot** in the Prometheus text format, so any
  standard client renders it (`apps/ping` uses Rust's `prometheus-client`). Shell validates it
  (names, labels, size; `wshell_*` names and the `app` label are reserved), keeps the first
  `max_series_per_app` series, adds `app="<id>"` and serves the last snapshot, also while the app
  is stopped. Both the 0.0.4 text format and OpenMetrics are accepted; timestamps and `_created`
  are dropped. `wshell_app_metrics_series`, `…_dropped` and `…_published_timestamp_seconds` show
  how it went. A family declared with different types by two apps is served from the first one.
- Python: the official `prometheus_client` does not load under componentize-py yet: its package
  imports `ssl`, which CPython on WASI does not have.

## How it works

```
shellctl ──unix socket──▶ shelld (supervisor, 1 tokio thread)
browser ──HTTP/WS, loopback──▶ │  ├─ package manager: manifest and import checks, AOT (.cwasm)
                              │  ├─ host services: storage, wasi:http, exec (permissions checked on every call)
                              │  └─ cgroups v2: <scope>/supervisor, <scope>/apps/<id>
                              ├─ stdin/stdout (postcard) ──▶ shelld worker (one process per app, Wasmtime)
                              └─ shelld exec-helper ──exec──▶ ping, tracepath… (in the app's cgroup)
```

- **Unprivileged worker.** The worker only executes WASM; all host calls go over IPC to the
  supervisor, which re-checks them against the granted permissions. The worker has no network. `exec` utilities
  are launched by the supervisor via `shelld exec-helper`: the helper enters the app's cgroup, sets
  `NO_NEW_PRIVS` and `PDEATHSIG` and replaces itself with the utility.
- **Permissions are handles.** `storage.open()`, `exec.open(name)` return a resource only if the
  permission is granted, otherwise `none`.
- **HTTP is standard `wasi:http`.** The application uses an ordinary client (`wstd`, `waki`,
  SDKs for other languages). The worker intercepts the request (`WasiHttpHooks::send_request`) and passes it
  to the supervisor, which checks the host and method against the `net.http` permission and performs the request. A denial
  reaches the guest as the standard `HttpRequestDenied` and is written to the app log and the audit log.
  Example: `apps/fetch`.
- **Static checking.** At install time the component's imports are checked against the manifest permissions:
  importing `shell:app/storage` without `storage.private` or `wasi:http/*` without `net.http` fails the
  installation. Of the rest of WASI, only cli/io/clocks/random/filesystem/sockets are allowed (without
  preopened directories or environment; sockets work only with `net.listen`).
- **AOT compilation** runs in a separate `shelld compile` process at nice 19 with
  `[shell] compile_threads` threads (4 by default): compiling a large component (CPython, ~18 MB)
  takes ~10 CPU-seconds and a few hundred MB, which go back to the OS when the process exits.
  The permission prompt and the install use the same image, so a package is compiled once.
- **No `unsafe`.** System calls go through `rustix`; child processes configure themselves
  (no `pre_exec`). The only exception is loading the AOT image (`Component::deserialize_file`):
  Wasmtime cannot verify ready-made machine code. Before starting, the supervisor checks the image's SHA-256
  and the engine fingerprint recorded at install time, and on mismatch (corruption,
  shelld update) recompiles the image from `.wasm`. New `unsafe` is forbidden by lints
  (`forbid` in `shell-core` and `shellctl`, `deny` in `shelld`).
- **Limits.** Fuel per call (infinite loop → trap → app `failed`), WASM linear memory
  limit, cgroup `memory.max` (WASM memory + `runtime_overhead_mb`), `cpu.max`,
  `pids.max`. Without a delegated cgroup (`cgroups.mode = "auto"`) OS limits are not applied;
  there will be a warning in the log. The same applies to `cpu.max` alone on kernels without
  `CONFIG_CFS_BANDWIDTH` (e.g. the arm64 defconfig): the other limits still work.
- **Bridge.** One-time token → `HttpOnly; SameSite=Strict` cookie; every request checks
  `Host` (DNS rebinding protection), `Origin` and the session bound to the app. All responses get
  CSP, COOP, CORP, `nosniff`.
- **States.** `running`: there is a visible UI tab; `suspended`: the UI is hidden or closed, events
  are not sent to the UI.

## Programs with `main` (kind = "command")

Besides `shell:app` world applications (UI bridge, hooks, events), Shell runs ordinary programs:
standard WASI `wasi:cli/command` components with `fn main()`. For example, your own HTTP server:

```toml
[backend]
kind = "command"
world = "wasi:cli/command"

[[permissions]]
id = "net.listen"
ports = [8480]
```

```rust
fn main() {
    let listener = std::net::TcpListener::bind(("0.0.0.0", 8480)).unwrap();
    for stream in listener.incoming() { /* … */ }
}
```

- **Network.** Shell only opens ports: the program may `bind`/`listen` on ports from
  `net.listen` and accept incoming connections (from any address). Who accesses the server and how
  is not controlled by Shell. Outgoing connections and UDP are forbidden (use `net.http` for outbound HTTP requests).
  `wasi:sockets/*` may be imported without `net.listen` (language runtimes such as CPython import it
  unconditionally), but then every socket operation is denied.
- **Ports.** At install time Shell checks that the port is not taken by another app or by Shell's own ports.
  The port on the device is the same as in the manifest (no remapping); ports below 1024 require privileges.
- **Lifecycle.** The program runs until it exits on its own (`exit 0` → `stopped`, otherwise
  `failed`) or is stopped. There are no hooks, so stopping is immediately forced. There is no bridge:
  `shell.call`/`shellctl call` are unavailable, and there is no cold start or idle stop;
  it is started manually or by the `autostart` policy. Fuel does not limit `main`; CPU is limited by the cgroup.
- **Output.** The program's stdout and stderr go to the app log. The dashboard shows a
  "program" label and links to its ports.
- **Frameworks.** axum works (`apps/hello-axum`): tokio provides networking on WASI with
  `--cfg tokio_unstable` (set in `apps/.cargo/config.toml`); a single-threaded runtime is required
  (`flavor = "current_thread"`). actix-web does not build for WASI: `actix-rt` requires
  `tokio::signal`, and `actix-server` requires `socket2`/`mio`, which have no WASI support.
- **Go (experiment, `apps/hello-go`).** A Go server cannot yet be built with standard tools:
  official Go only supports `wasip1` (not a component, no `listen`), and TinyGo `-target=wasip2`
  builds a component, but its `net` package works through a pluggable driver (netdev), which
  does not exist for wasip2. The example uses a hand-written `wasinet` driver (~250 lines) on top of `wasi:sockets`,
  whose bindings are generated by `wit-bindgen-go`; the server itself is plain `net/http`. TinyGo
  is single-threaded, so a single reactor goroutine waits on all sockets at once
  (`wasi:io/poll.poll`): a silent or slow client does not block the others, keep-alive works,
  and no CPU is spent while idle. Limitations: IPv4 and incoming connections only. Build requires
  `tinygo` + `wasm-tools` (`cargo install wasm-tools`). TinyGo has no visible plans for networking on wasip2;
  the path to standard Go is the `GOOS=wasip3` proposal (golang/go#77141).

## Python apps

`apps/todo-py` is an ordinary `shell:app` app (UI + bridge) written in Python and built by the
standard `componentize-py` (`pipx install componentize-py`); CPython is embedded into the component.

```python
class Bridge(exports.Bridge):
    def handle(self, method, payload):
        state = load()          # JSON from storage.private
        ...
        save(state)             # store.set(...) + events.emit("changed")
        return json.dumps(state["items"])
```

- **Own world.** `componentize-py` keeps every import of the target world, even unused ones, and each
  import is checked against the manifest (`shell:app/exec` would require `exec`). So the app declares its own
  world in `world.wit` with only what it uses: `log`, `events`, `storage` + the `lifecycle`/`bridge` exports.
- **State.** The list is stored in `storage.private`, so the app survives idle stops, restarts and
  updates (`idle = "stop"`: it can be stopped at any time and is cold-started on the next call).
- **Cost.** The component is ~18 MB, RSS ~55 MB, cold start ~1 s; `memory_mb = 64`.
- Build: `scripts/build-apps.sh` (skipped without `componentize-py`).

## Lifecycle: app hints, Shell decisions

An application can tell Shell about itself, but it is always Shell that stops it, and the device policy can
override any hint.

**In the manifest**: static hints:

```toml
[lifecycle]
idle = "stop"        # stop | keep: whether it may be stopped when idle
priority = "normal"  # low | normal | high: stop order under memory pressure
```

**At runtime**: "it is unsafe to stop me right now" (interface `shell:app/activity`, requires no
permission):

```rust
let _guard = activity::Busy::new("writing report"); // while the handle is alive, the app is busy
activity::request_stop();                           // ask Shell to stop the app
```

**How Shell stops an application** (analogous to SIGTERM → SIGKILL):

1. If the app is not busy: `on-stop` immediately with a hard timeout, then forcibly.
2. If busy: a `stop-requested { reason, grace-ms }` event in `on-event`; Shell waits for the
   app to release `activity.busy`, but no longer than `stop_grace_ms`, then step 1.
   `shellctl stop --force` and "Stop now" in the dashboard skip the wait.

**When Shell stops an application on its own:**

- **Idle** (`idle_stop_after_s`): no visible UI, bridge calls, running utilities or
  busy locks. The timer is armed only for the idle period, with no background polling.
- **Memory pressure** (`memory_high_mb`): the budget for all apps is set as `memory.high` on
  the `apps/` cgroup. When it is hit, the kernel reclaims memory from the apps itself and reports it via
  `memory.events` (inotify). Shell then stops the first app in the queue: low
  priority first, then those with a hidden UI, then non-busy ones, then the "heaviest".

Device settings live in the `[lifecycle]` section of the config: `honor_keep` (whether to respect `idle = "keep"`),
`max_busy_s` (busy state longer than this is ignored, protecting against a lock held forever). For a specific
app they are overridden in `[policy.app."<id>"]` via `priority` and `idle`. The dashboard and
`shellctl list` show declared and effective values, busy locks and the position in the eviction
queue. Every eviction is written to the audit log.

## Cold start

A stopped application (manually, due to idleness or memory pressure) starts on its own when
it is needed:

- a **bridge call** (`shell.call`, `shellctl call`) waits for the start and then runs;
- **opening the UI or returning to the tab**: `shell.js` connects with `?wake=1`. Background
  timer reconnects do not wake the app, so a forgotten tab will not resurrect an app
  that the administrator stopped or that was evicted by memory pressure;
- concurrent requests wait for a single shared start;
- a crashed app is woken no sooner than 2^failures s (at most 5 minutes).

Starting from an AOT image takes tens of milliseconds. Disabled with `[lifecycle] start_on_demand = false`
or, for a single app, `[policy.app."<id>"] on_demand = false`. The "Open UI" button in the dashboard
is also available for stopped apps if cold start is allowed for them.

## Decisions made for the MVP

- Bridge message format: JSON.
- Package: a tar archive or a directory; signing comes after the MVP.
- The `exec` permission has a `name` field (defaults to the binary's file name): the backend uses it to open
  the permission when there are several utilities.
- `exec` privileges (`CAP_*`) are for now only checked against the `exec.allowed_privileges` list, but not
  granted: the privilege helper comes after the MVP. Shelld is expected to run as non-root.
- An app can only be updated while stopped; migrations and rollback come after the MVP.
- In addition to the spec, `shellctl` has `url`, `call`, `uninstall` and `dashboard`.
- On update, previously granted optional permissions are kept by default.

## MVP status

Done: supervisor, one process per app, cgroups v2 + fuel + memory limit, manifest and
static import checking, `storage.private`, `net.http`, `exec` permissions, optional permissions,
UI HTTP server with a subdomain per app, bridge and `shell.js`, a single resource profile,
`shellctl install/list/start/stop/logs`, ping/tracepath example.

Remaining in the MVP scope: `system.services` (systemd over D-Bus) and `system.network`
(NetworkManager) permissions, "network" and "systemd services" examples, seccomp/namespaces for the worker.

## License

Copyright (c) 2026 dkotTech.

- **Shell** (`crates/`, `dashboard/`, `scripts/`): [Elastic License 2.0](LICENSE) (ELv2).
  Free to use, modify and distribute, including commercially and on devices you sell; it may not
  be provided to third parties as a hosted or managed service. Official text:
  https://www.elastic.co/licensing/elastic-license
- **What apps are built with** (`wit/`), **example apps** (`apps/`), **plugins** (`plugins/`) and
  the **Renderer** (`render/`): MIT, see the `LICENSE` file in each directory. Apps and plugins
  written against `wit/` are not bound by ELv2. The Renderer uses only wshell's control protocol
  (JSON lines on a socket), no wshell code.
- Third-party components keep their own licenses (Wasmtime: Apache-2.0 WITH LLVM-exception;
  Servo: MPL-2.0; WPE WebKit: LGPL and BSD, linked dynamically).
