# wshell roadmap

wshell as a general-purpose orchestrator for WASM workloads, independent of any particular
device. This is a direction, not a commitment: phases are ordered, items within a phase are not.

## Positioning

Not a head-on Kubernetes replacement: wasmCloud (CNCF, distributed, NATS + wRPC), Spin/SpinKube
and Wasmer Edge already cover large distributed setups. The niche is **"k3s for WASM, simpler
than systemd"**:

- **One binary, one node first**: lighter than k3s, nothing else to run.
- **Permissions instead of containers**: per-call checks of rights declared in the manifest,
  finer than namespaces and Pod Security.
- **Scale to zero built in**: idle stop, cold start in tens of milliseconds, eviction under
  memory pressure (Kubernetes needs Knative for this).
- **An app is a service plus a UI**: a first-class web UI per app.

Target users: edge gateways, on-prem appliances, homelabs, small fleets (1–50 nodes),
running third-party plugins safely.

The Elastic License 2.0 forbids offering wshell as a managed service. That suits self-hosting
but may put off contributors and integrators; decide on it before a public launch.

## Where wshell stands, in Kubernetes terms

| Kubernetes | wshell today | Status |
| --- | --- | --- |
| Pod / Deployment | `app.toml` + `install` | ✅ imperative |
| kubectl / Dashboard | `shellctl` / dashboard | ✅ |
| requests / limits | cgroups v2 + fuel + memory limit | ✅ |
| Pod Security, workload RBAC | manifest permissions, import checks | ✅ stronger |
| HPA to zero (Knative) | idle stop + cold start | ✅ |
| restartPolicy / backoff | restart with 2^n backoff | ✅ |
| Prometheus | `/metrics`, apps' own metrics | ✅ |
| PersistentVolume | `storage.private` | ⚠️ local only |
| ConfigMap / Secret | — | ❌ |
| Service / Ingress | `*.localhost` subdomains, `net.listen` | ⚠️ loopback, no TLS |
| Desired state / reconcile | — | ❌ |
| Registry / image signing | tar package | ❌ |
| Rolling update / rollback | update only while stopped | ❌ |
| Readiness / liveness | — | ❌ |
| Job / CronJob | — | ❌ |
| Service-to-service calls | forbidden (v1 non-goal) | ❌ |
| Multiple nodes | — | ❌ |
| Operator users / RBAC | one-time tokens, local socket | ❌ |

## Phase 0: finish the MVP

- seccomp and namespaces for the worker: required before claiming "safer than containers".
- `system.services` (systemd over D-Bus) and `system.network` (NetworkManager) permissions.
- Close the spec's open questions: bridge format, minimum kernel.

## Phase 1: a dependable single node

Goal: one machine can run production instead of docker-compose or k3s.

1. **Declarative state.** A desired-state file; shelld reconciles the node to it. The base for
   GitOps and everything after.
   - ✅ `shellctl apply [--prune] [-n] apps.toml`: client-side plan and apply (see README).
   - ✅ `[desired] file`: shelld applies it on start, on change and on `shellctl reconcile`.
   - Next: policy (`[policy.app]`) in the same file; reconcile status in the dashboard.
2. **Distribution.** Packages as OCI artifacts (the wasm OCI format): `shellctl install
   ghcr.io/…`. Signing with cosign/sigstore and trusted keys (from the spec).
3. **Updates.** Start the new version next to the old one, wait for readiness, switch over;
   rollback and a `storage` migration hook (from the spec).
4. **Health checks.** Readiness and liveness as a WIT export (`health.check`) with timeouts.
5. **Config and secrets.** Standard `wasi:config` rather than an own interface; secrets
   encrypted at rest and handed out only by permission.
6. **Jobs and cron.** `kind = "job"` and `schedule = "…"`; Shell coalesces timers, which also
   serves the spec's power goals.
7. **Backup and export** of `storage.private`.

## Phase 2: networking and services

1. **Ingress.** UI and `net.listen` reachable from outside: TLS (ACME), routing by domain, user
   authentication (OIDC). A v1 non-goal today, but a Kubernetes replacement needs it.
2. **HTTP handlers (`wasi:http/proxy`).** As in Spin: an instance per request, autoscaling by
   load, several instances of one app. The most common server workload.
3. **Typed calls between apps.** An app imports another app's WIT interface; the supervisor
   checks an `app.call:<id>/<interface>` permission. A service mesh with types and permissions
   and no sidecars. Possibly the main differentiator. Cross-node transport later (wRPC).
4. **Observability.** Logs and traces over OTLP (`wasi:otel`); tracing of bridge and `exec` calls.
5. **Standard storage.** `wasi:keyvalue` (later `wasi:blobstore`) with pluggable backends: local
   file, Redis, S3. Lets stateful apps outlive a single node in phase 3.

## Phase 3: fleets

No own consensus up front; in steps:

1. **Pull-based fleet.** Every shelld pulls its desired state from Git or OCI by node labels,
   like Flux or Fleet. Suits edge links that come and go; no central server needed.
2. **Central console.** A dashboard over many nodes: aggregated metrics, logs and status;
   operator RBAC and the existing audit log.
3. **Simple scheduler.** Bin-packing by memory, labels and affinity. Stateless apps move freely;
   stateful apps are pinned to a node or use the external key-value store from phase 2.
4. Only on demand: an HA control plane.

## Phase 4: ecosystem

- Templates and SDKs: Rust, Python, JS (`jco`), Go once `wasip3` lands.
- WASIp3 (async): removes today's tokio and TinyGo workarounds.
- More plugin points (authentication, scheduling, storage backends) under the same permission
  model.
- Multi-tenancy: namespaces, quotas per group of apps, users.
- A bridge to Kubernetes rather than a replacement: importing Spin/wasmCloud apps, possibly a
  Virtual Kubelet provider so Kubernetes can place workloads on wshell nodes.

## Not doing

- Copying the Kubernetes API or CRDs: the simplicity of `app.toml` is the point.
- Containers: WASM plus permission-checked `exec` is the difference.
- Clustering before a single node is boringly reliable (phase 1).

## Risks

- **WASI ecosystem.** actix does not build, Go has no networking on wasip2, CPython is 18 MB;
  some popular stacks wait for WASIp3.
- **Competitors are ahead on distribution.** Compete on security, footprint and UI, not node count.
- **License** (see Positioning).

## Next

OCI distribution with signing (phase 1, item 2): the desired-state file then names packages
in a registry instead of local paths.
