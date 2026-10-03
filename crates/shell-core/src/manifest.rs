//! App manifest `app.toml`.

use std::collections::BTreeSet;
use std::fmt;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::exec_template::Template;

/// Shell API version implemented by this build.
pub const SHELL_API: (u32, u32) = (0, 1);
/// WIT world of an `app` kind application.
pub const APP_WORLD: &str = "shell:app/app@0.1.0";
/// World of a `command` kind application: an ordinary program with `main` (`wasi:cli/run`).
pub const COMMAND_WORLD: &str = "wasi:cli/command";
/// WIT world of a plugin: the app world plus access to Shell (`shell.*` permissions).
pub const PLUGIN_WORLD: &str = "shell:app/plugin@0.1.0";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub app: AppInfo,
    pub backend: Backend,
    #[serde(default)]
    pub ui: Option<Ui>,
    #[serde(default)]
    pub permissions: Vec<Permission>,
    #[serde(default)]
    pub resources: Resources,
    #[serde(default)]
    pub lifecycle: LifecycleHints,
}

/// The app's hints about its lifecycle. Purely informational:
/// Shell makes the decision, and the device policy may override them.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleHints {
    /// Whether the app may be stopped when idle.
    #[serde(default)]
    pub idle: IdleMode,
    /// Stop order under resource pressure: `low` goes first.
    #[serde(default)]
    pub priority: Priority,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IdleMode {
    /// An idle app may be stopped.
    #[default]
    Stop,
    /// The app asks not to be stopped when idle (background work).
    Keep,
}

/// Eviction priority. Variant order is the stop order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppInfo {
    pub id: String,
    pub version: String,
    pub name: String,
    pub shell_api: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    #[serde(default)]
    pub kind: BackendKind,
    #[serde(default = "default_component")]
    pub component: String,
    pub world: String,
}

/// How the backend is structured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// A component of the `shell:app/app` world: lifecycle hooks, UI bridge, events.
    #[default]
    App,
    /// An ordinary program with `main` (`wasi:cli/command`), e.g. its own
    /// HTTP server on ports from the `net.listen` permission. No bridge or hooks.
    Command,
    /// A Shell plugin: like `app` (hooks, bridge, UI), plus `shell.*` permissions that
    /// give access to Shell itself, e.g. the list of apps and links to their UI.
    Plugin,
}

impl BackendKind {
    /// Lifecycle hooks and the UI bridge (`app` and `plugin`), as opposed to a plain `main`.
    pub fn has_bridge(self) -> bool {
        self != BackendKind::Command
    }
}

fn default_component() -> String {
    "backend.wasm".into()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ui {
    #[serde(default = "default_entry")]
    pub entry: String,
}

fn default_entry() -> String {
    "ui/index.html".into()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    /// Memory request; the policy may reduce it.
    pub memory_mb: Option<u32>,
    #[serde(default)]
    pub background: Background,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Background {
    #[default]
    None,
    Periodic,
    Persistent,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Permission {
    #[serde(flatten)]
    pub kind: PermissionKind,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "id")]
pub enum PermissionKind {
    #[serde(rename = "storage.private")]
    StoragePrivate { quota_mb: u32 },
    #[serde(rename = "net.http")]
    NetHttp {
        hosts: Vec<String>,
        #[serde(default = "default_http_methods")]
        methods: Vec<String>,
    },
    #[serde(rename = "exec")]
    Exec(ExecSpec),
    #[serde(rename = "system.services")]
    SystemServices { units: Vec<String> },
    #[serde(rename = "system.network")]
    SystemNetwork { mode: NetworkMode },
    /// Incoming TCP connections on device ports. Shell only opens the port;
    /// it does not control access to the app itself.
    #[serde(rename = "net.listen")]
    NetListen { ports: Vec<u16> },
    /// Plugins only: the installed apps and one-time links to their UI.
    #[serde(rename = "shell.apps")]
    ShellApps {},
}

fn default_http_methods() -> Vec<String> {
    vec!["GET".into(), "HEAD".into()]
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExecSpec {
    /// Name by which the backend opens the permission; defaults to the binary's file name.
    pub name: Option<String>,
    pub binary: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub privileges: Vec<String>,
    #[serde(default = "default_exec_timeout")]
    pub timeout_s: u64,
}

fn default_exec_timeout() -> u64 {
    30
}

impl ExecSpec {
    pub fn name(&self) -> &str {
        self.name
            .as_deref()
            .unwrap_or_else(|| self.binary.rsplit('/').next().unwrap_or(&self.binary))
    }

    pub fn template(&self) -> Result<Template> {
        Template::parse(&self.args)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    Read,
    Manage,
}

impl Permission {
    /// Unique permission key within the app: `exec:<name>` or the permission id.
    pub fn key(&self) -> String {
        match &self.kind {
            PermissionKind::Exec(e) => format!("exec:{}", e.name()),
            k => k.id().to_string(),
        }
    }
}

impl PermissionKind {
    pub fn id(&self) -> &'static str {
        match self {
            PermissionKind::StoragePrivate { .. } => "storage.private",
            PermissionKind::NetHttp { .. } => "net.http",
            PermissionKind::Exec(_) => "exec",
            PermissionKind::SystemServices { .. } => "system.services",
            PermissionKind::SystemNetwork { .. } => "system.network",
            PermissionKind::NetListen { .. } => "net.listen",
            PermissionKind::ShellApps {} => "shell.apps",
        }
    }
}

/// Interfaces (without version) whose import requires a permission. The single source of truth.
/// `net.http` is standard `wasi:http`; requests are still performed by the supervisor.
const PERMISSION_INTERFACES: &[(&str, &str)] = &[
    ("storage.private", "shell:app/storage"),
    ("net.http", "wasi:http/outgoing-handler"),
    ("net.http", "wasi:http/types"),
    ("exec", "shell:app/exec"),
    ("system.services", "shell:app/services"),
    ("system.network", "shell:app/network"),
    ("shell.apps", "shell:app/apps"),
];

/// Permission required to import an interface (`package/interface`, without version).
pub fn permission_for_interface(iface: &str) -> Option<&'static str> {
    PERMISSION_INTERFACES.iter().find(|(_, i)| *i == iface).map(|(p, _)| *p)
}

impl fmt::Display for PermissionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PermissionKind::StoragePrivate { quota_mb } => {
                write!(f, "private storage up to {quota_mb} MB")
            }
            PermissionKind::NetHttp { hosts, methods } => {
                write!(f, "HTTP requests ({}) to {}", methods.join(", "), hosts.join(", "))
            }
            PermissionKind::Exec(e) => {
                write!(f, "run {} {}", e.binary, e.args.join(" "))?;
                if !e.privileges.is_empty() {
                    write!(f, " with privileges {}", e.privileges.join(", "))?;
                }
                Ok(())
            }
            PermissionKind::SystemServices { units } => {
                write!(f, "manage systemd services: {}", units.join(", "))
            }
            PermissionKind::SystemNetwork { mode: NetworkMode::Read } => {
                write!(f, "read network settings")
            }
            PermissionKind::NetListen { ports } => {
                let ports: Vec<String> = ports.iter().map(u16::to_string).collect();
                write!(f, "incoming TCP connections on ports {}", ports.join(", "))
            }
            PermissionKind::SystemNetwork { mode: NetworkMode::Manage } => {
                write!(f, "change network settings")
            }
            PermissionKind::ShellApps {} => {
                write!(f, "see the installed apps and open their UI")
            }
        }
    }
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self> {
        let m: Manifest = toml::from_str(text).context("parsing app.toml")?;
        m.validate()?;
        Ok(m)
    }

    pub fn validate(&self) -> Result<()> {
        validate_app_id(&self.app.id)?;
        ensure!(
            !self.app.version.is_empty()
                && self.app.version.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
            "version {:?}: expected the form 1.2.3",
            self.app.version
        );
        ensure!(!self.app.name.trim().is_empty(), "empty app name");
        check_shell_api(&self.app.shell_api)?;
        match self.backend.kind {
            BackendKind::App => ensure!(
                self.backend.world == APP_WORLD,
                "world {:?} is not supported, expected {APP_WORLD:?}",
                self.backend.world
            ),
            BackendKind::Command => {
                ensure!(
                    self.backend.world.split('@').next() == Some(COMMAND_WORLD),
                    "kind = \"command\" expects world {COMMAND_WORLD:?}, got {:?}",
                    self.backend.world
                );
                // The UI of such a program is its own server on the net.listen ports.
                ensure!(self.ui.is_none(), "kind = \"command\" cannot have [ui]: the program serves its own UI");
            }
            BackendKind::Plugin => ensure!(
                self.backend.world == PLUGIN_WORLD,
                "kind = \"plugin\" expects world {PLUGIN_WORLD:?}, got {:?}",
                self.backend.world
            ),
        }
        // Access to Shell itself is for plugins: an app stays isolated from other apps.
        if self.backend.kind != BackendKind::Plugin
            && let Some(p) = self.permissions.iter().find(|p| p.kind.id().starts_with("shell."))
        {
            bail!("permission {} is for plugins only (kind = \"plugin\")", p.key());
        }
        check_rel_path(&self.backend.component)?;
        if let Some(ui) = &self.ui {
            check_rel_path(&ui.entry)?;
            ensure!(ui.entry.starts_with("ui/"), "ui.entry must be inside ui/");
        }

        let mut keys = BTreeSet::new();
        for p in &self.permissions {
            let key = p.key();
            ensure!(keys.insert(key.clone()), "permission {key} is declared twice");
            p.validate().with_context(|| format!("permission {key}"))?;
        }
        Ok(())
    }

    pub fn permission(&self, key: &str) -> Option<&Permission> {
        self.permissions.iter().find(|p| p.key() == key)
    }
}

impl Permission {
    fn validate(&self) -> Result<()> {
        match &self.kind {
            PermissionKind::StoragePrivate { quota_mb } => {
                ensure!(*quota_mb > 0, "quota_mb must be greater than 0")
            }
            PermissionKind::NetHttp { hosts, methods } => {
                ensure!(!hosts.is_empty(), "empty hosts list");
                for h in hosts {
                    let bare = h.strip_prefix("*.").unwrap_or(h);
                    ensure!(crate::exec_template::is_hostname(bare), "invalid host {h:?}");
                }
                for m in methods {
                    ensure!(
                        ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"].contains(&m.as_str()),
                        "invalid method {m:?}"
                    );
                }
            }
            PermissionKind::Exec(e) => {
                ensure!(e.binary.starts_with('/'), "binary must be an absolute path");
                ensure!(!e.binary.contains("/../"), "binary must not contain '..'");
                let name = e.name();
                ensure!(
                    !name.is_empty()
                        && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)),
                    "invalid name {name:?}"
                );
                e.template()?;
                ensure!(e.timeout_s > 0 && e.timeout_s <= 3600, "timeout_s outside 1..3600");
                for p in &e.privileges {
                    ensure!(
                        p.starts_with("CAP_") && p[4..].chars().all(|c| c.is_ascii_uppercase() || c == '_'),
                        "invalid privilege {p:?}"
                    );
                }
            }
            PermissionKind::SystemServices { units } => {
                ensure!(!units.is_empty(), "empty units list");
                for u in units {
                    ensure!(
                        u.contains('.') && u.chars().all(|c| c.is_ascii_alphanumeric() || "-_.@:\\*".contains(c)),
                        "invalid unit name {u:?}"
                    );
                }
            }
            PermissionKind::SystemNetwork { .. } => {}
            PermissionKind::NetListen { ports } => {
                ensure!(!ports.is_empty(), "empty ports list");
                ensure!(!ports.contains(&0), "port 0 is not allowed");
            }
            PermissionKind::ShellApps {} => {}
        }
        Ok(())
    }
}

/// App id: a reverse domain name, usable as a `<id>.localhost` subdomain.
pub fn validate_app_id(id: &str) -> Result<()> {
    let labels: Vec<&str> = id.split('.').collect();
    ensure!(labels.len() >= 2, "id {id:?}: expected a reverse domain name (com.example.app)");
    ensure!(id.len() <= 200, "id {id:?} is too long");
    for l in labels {
        ensure!(
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "id {id:?}: invalid segment {l:?} (only a-z, 0-9, '-')"
        );
    }
    Ok(())
}

fn check_shell_api(req: &str) -> Result<()> {
    let (maj, min) = req
        .split_once('.')
        .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)))
        .with_context(|| format!("shell_api {req:?}: expected the form 0.1"))?;
    let (our_maj, our_min) = SHELL_API;
    // Before 1.0 a minor version is as incompatible as a major one.
    let compatible = maj == our_maj && if maj == 0 { min == our_min } else { min <= our_min };
    if !compatible {
        bail!("app requires shell_api {req}, the shell supports {our_maj}.{our_min}");
    }
    Ok(())
}

fn check_rel_path(p: &str) -> Result<()> {
    ensure!(
        !p.is_empty() && !p.starts_with('/') && p.split('/').all(|s| !s.is_empty() && s != "." && s != ".."),
        "invalid path {p:?}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PING: &str = r#"
[app]
id = "org.wshell.ping"
version = "0.1.0"
name = "Ping"
shell_api = "0.1"

[backend]
world = "shell:app/app@0.1.0"

[ui]
entry = "ui/index.html"

[[permissions]]
id = "exec"
binary = "/usr/bin/ping"
args = ["-c", "{count:int:1..20}", "{host:host}"]
reason = "Checking host reachability"

[[permissions]]
id = "storage.private"
quota_mb = 1
optional = true
"#;

    #[test]
    fn parses_example() {
        let m = Manifest::parse(PING).unwrap();
        assert_eq!(m.backend.component, "backend.wasm");
        let keys: Vec<_> = m.permissions.iter().map(Permission::key).collect();
        assert_eq!(keys, ["exec:ping", "storage.private"]);
        assert!(m.permissions[1].optional);
    }

    #[test]
    fn rejects_unknown_permission() {
        let bad = PING.replace("id = \"storage.private\"", "id = \"root.everything\"");
        assert!(Manifest::parse(&bad).is_err());
    }

    #[test]
    fn rejects_bad_ids_and_paths() {
        assert!(validate_app_id("Com.Example").is_err());
        assert!(validate_app_id("single").is_err());
        assert!(validate_app_id("com..x").is_err());
        assert!(validate_app_id("com.example.ping").is_ok());
        assert!(check_rel_path("../x").is_err());
        assert!(check_rel_path("/etc/passwd").is_err());
    }

    const SERVER: &str = r#"
[app]
id = "org.wshell.hello-server"
version = "0.1.0"
name = "Hello"
shell_api = "0.1"

[backend]
kind = "command"
world = "wasi:cli/command"

[[permissions]]
id = "net.listen"
ports = [8480]
"#;

    #[test]
    fn command_kind() {
        let m = Manifest::parse(SERVER).unwrap();
        assert_eq!(m.backend.kind, BackendKind::Command);
        assert!(matches!(&m.permissions[0].kind, PermissionKind::NetListen { ports } if ports == &[8480]));
        // shell:app world for a program, Shell UI and empty ports are errors.
        assert!(Manifest::parse(&SERVER.replace("wasi:cli/command", "shell:app/app@0.1.0")).is_err());
        assert!(Manifest::parse(&format!("{SERVER}\n[ui]\nentry = \"ui/index.html\"")).is_err());
        assert!(Manifest::parse(&SERVER.replace("ports = [8480]", "ports = []")).is_err());
        // Sockets require net.listen, standard HTTP requires net.http.
        assert_eq!(permission_for_interface("wasi:sockets/tcp"), None);
        assert_eq!(permission_for_interface("wasi:http/outgoing-handler"), Some("net.http"));
    }

    const PLUGIN: &str = r#"
[app]
id = "org.wshell.launcher"
version = "0.1.0"
name = "Launcher"
shell_api = "0.1"

[backend]
kind = "plugin"
world = "shell:app/plugin@0.1.0"

[ui]

[[permissions]]
id = "shell.apps"
reason = "The list of apps to open"
"#;

    #[test]
    fn plugin_kind() {
        let m = Manifest::parse(PLUGIN).unwrap();
        assert_eq!(m.backend.kind, BackendKind::Plugin);
        assert!(m.backend.kind.has_bridge());
        assert!(matches!(m.permissions[0].kind, PermissionKind::ShellApps {}));
        assert_eq!(permission_for_interface("shell:app/apps"), Some("shell.apps"));
        // A plugin needs the plugin world; an app cannot declare shell.* permissions.
        assert!(Manifest::parse(&PLUGIN.replace("shell:app/plugin@0.1.0", "shell:app/app@0.1.0")).is_err());
        let as_app = PLUGIN.replace("kind = \"plugin\"", "kind = \"app\"").replace("plugin@", "app@");
        let err = Manifest::parse(&as_app).unwrap_err();
        assert!(format!("{err:#}").contains("plugins only"), "{err:#}");
    }

    #[test]
    fn shell_api_compat() {
        assert!(check_shell_api("0.1").is_ok());
        assert!(check_shell_api("0.2").is_err());
        assert!(check_shell_api("1.0").is_err());
    }
}
