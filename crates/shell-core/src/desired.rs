//! Desired state of the device (`shellctl apply`): which apps are installed, from which packages,
//! with which optional permissions, and whether they run. The plan is the difference between it
//! and what is installed; applying the same file twice does nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::control::{AppState, AppStatus};
use crate::manifest::Manifest;
use crate::source::{OciRef, Source};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredState {
    #[serde(default, rename = "app")]
    pub apps: BTreeMap<String, DesiredApp>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredApp {
    /// A `.pkg` archive or a package directory, relative to the state file, or a registry
    /// reference (`oci://ghcr.io/acme/ping:1.0`).
    pub package: Source,
    /// Optional permissions to grant; the rest stay denied. Required ones come with the install.
    #[serde(default)]
    pub grant: Vec<String>,
    /// Absent: the run state is not managed (a new app stays stopped until a cold start).
    #[serde(default)]
    pub state: Option<RunState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Running,
    Stopped,
}

impl DesiredState {
    pub fn parse(text: &str, base: &Path) -> Result<Self> {
        let mut s: DesiredState = toml::from_str(text)?;
        for (id, app) in &mut s.apps {
            crate::manifest::validate_app_id(id)?;
            if let Source::Path(p) = &app.package
                && p.is_relative()
            {
                app.package = Source::Path(base.join(p));
            }
        }
        Ok(s)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("{}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        Self::parse(&text, base).with_context(|| format!("{}", path.display()))
    }

    /// Reads what every package declares, each app on its own: one package that cannot be
    /// read (a registry out of reach) does not stop the others. Package paths become absolute
    /// (shelld opens them from its own working directory); `oci` reads a registry package and
    /// returns its reference pinned to the digest it read, so the install gets the same artifact.
    pub fn read_packages(
        &mut self,
        mut oci: impl FnMut(&OciRef) -> Result<(Package, OciRef)>,
    ) -> BTreeMap<String, Result<Package>> {
        let mut packages = BTreeMap::new();
        for (id, app) in &mut self.apps {
            let pkg = match &app.package {
                Source::Path(p) => std::fs::canonicalize(p)
                    .with_context(|| format!("{id}: package {}", p.display()))
                    .and_then(|p| {
                        let pkg = Package::read(&p).with_context(|| format!("{id}: {}", p.display()))?;
                        app.package = Source::Path(p);
                        Ok(pkg)
                    }),
                Source::Oci(r) => oci(r).with_context(|| format!("{id}: {r}")).map(|(pkg, pinned)| {
                    app.package = Source::Oci(pinned);
                    pkg
                }),
            };
            packages.insert(id.clone(), pkg);
        }
        packages
    }
}

/// What a package declares, read from its `app.toml`.
#[derive(Debug, Clone)]
pub struct Package {
    pub id: String,
    pub version: String,
    /// Permission keys and whether each one is optional.
    pub permissions: Vec<(String, bool)>,
}

impl Package {
    /// From `app.toml` of a package directory or a `.pkg` archive, without unpacking the rest.
    pub fn read(path: &Path) -> Result<Self> {
        let text = if path.is_dir() {
            std::fs::read_to_string(path.join("app.toml"))?
        } else {
            use std::io::Read;
            let mut ar = tar::Archive::new(std::fs::File::open(path)?);
            let mut found = None;
            // Every entry: unpacking lets a later one replace an earlier, so a second app.toml
            // would make this one not the manifest that gets installed.
            for entry in ar.entries()? {
                let mut entry = entry?;
                if archive_path(&entry.path()?) == Path::new("app.toml") {
                    ensure!(found.is_none(), "more than one app.toml in the package");
                    let mut text = String::new();
                    entry.read_to_string(&mut text)?;
                    found = Some(text);
                }
            }
            found.context("no app.toml in the package")?
        };
        let m = Manifest::parse(&text)?;
        let permissions = m.permissions.iter().map(|p| (p.key(), p.optional)).collect();
        Ok(Package { id: m.app.id, version: m.app.version, permissions })
    }
}

/// Where an archive entry lands when unpacked: `./app.toml` and `/app.toml` are `app.toml`.
/// `..` is kept (the unpacker refuses it).
pub fn archive_path(path: &Path) -> PathBuf {
    path.components().filter(|c| !matches!(c, Component::CurDir | Component::RootDir | Component::Prefix(_))).collect()
}

/// An installed app as far as the plan is concerned.
#[derive(Debug, Clone)]
pub struct Current {
    pub id: String,
    pub version: String,
    pub running: bool,
    pub granted: BTreeSet<String>,
}

impl From<&AppStatus> for Current {
    fn from(a: &AppStatus) -> Self {
        Current {
            id: a.id.clone(),
            version: a.version.clone(),
            running: matches!(a.state, AppState::Starting | AppState::Running | AppState::Suspended),
            granted: a.permissions.iter().filter(|p| p.granted).map(|p| p.key.clone()).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Stop { id: String },
    Install { id: String, package: Source, grant: Vec<String>, from: Option<String>, version: String },
    Start { id: String },
    Uninstall { id: String },
}

impl Step {
    pub fn id(&self) -> &str {
        match self {
            Step::Stop { id } | Step::Install { id, .. } | Step::Start { id } | Step::Uninstall { id } => id,
        }
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Stop { id } => write!(f, "stop {id}"),
            Step::Start { id } => write!(f, "start {id}"),
            Step::Uninstall { id } => write!(f, "uninstall {id} (with its data)"),
            Step::Install { id, from: None, version, .. } => write!(f, "install {id} {version}"),
            Step::Install { id, from: Some(old), version, .. } if old == version => {
                write!(f, "reinstall {id} {version} (permissions changed)")
            }
            Step::Install { id, from: Some(old), version, .. } => write!(f, "update {id} {old} → {version}"),
        }
    }
}

#[derive(Debug, Default)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// Installed but absent from the state, left alone without `prune`.
    pub unlisted: Vec<String>,
    /// In the state, but their package could not be read: left as they are.
    pub held: Vec<String>,
}

/// Compares the desired state with what is installed. An app missing from `packages` (its
/// package could not be read) is held: neither changed nor removed.
pub fn plan(
    desired: &DesiredState,
    packages: &BTreeMap<String, Package>,
    current: &[Current],
    prune: bool,
) -> Result<Plan> {
    let mut plan = Plan::default();
    // An empty file is more likely a truncated one than an order to remove every app with
    // its data.
    ensure!(
        !prune || !desired.apps.is_empty() || current.is_empty(),
        "the state lists no apps: refusing to remove all {} installed (uninstall them one by one)",
        current.len()
    );
    // Removals first: they free the ports that new apps may take.
    for cur in current.iter().filter(|c| !desired.apps.contains_key(&c.id)) {
        if !prune {
            plan.unlisted.push(cur.id.clone());
            continue;
        }
        if cur.running {
            plan.steps.push(Step::Stop { id: cur.id.clone() });
        }
        plan.steps.push(Step::Uninstall { id: cur.id.clone() });
    }

    for (id, app) in &desired.apps {
        let Some(pkg) = packages.get(id) else {
            plan.held.push(id.clone());
            continue;
        };
        ensure!(&pkg.id == id, "{id}: {} contains app {}", app.package, pkg.id);
        for key in &app.grant {
            let optional = pkg.permissions.iter().find(|(k, _)| k == key).map(|(_, o)| *o);
            ensure!(optional.is_some(), "{id}: permission {key} is not declared in the package");
            ensure!(optional == Some(true), "{id}: permission {key} is required; it comes with the install");
        }
        let want: BTreeSet<String> = pkg
            .permissions
            .iter()
            .filter(|(key, optional)| !optional || app.grant.contains(key))
            .map(|(key, _)| key.clone())
            .collect();

        let cur = current.iter().find(|c| &c.id == id);
        let running = cur.is_some_and(|c| c.running);
        let want_running = match app.state {
            Some(RunState::Running) => true,
            Some(RunState::Stopped) => false,
            None => running,
        };
        let install = cur.is_none_or(|c| c.version != pkg.version || c.granted != want);
        if install {
            if running {
                plan.steps.push(Step::Stop { id: id.clone() });
            }
            plan.steps.push(Step::Install {
                id: id.clone(),
                package: app.package.clone(),
                grant: app.grant.clone(),
                from: cur.map(|c| c.version.clone()),
                version: pkg.version.clone(),
            });
            if want_running {
                plan.steps.push(Step::Start { id: id.clone() });
            }
        } else if want_running && !running {
            plan.steps.push(Step::Start { id: id.clone() });
        } else if !want_running && running {
            plan.steps.push(Step::Stop { id: id.clone() });
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATE: &str = r#"
        [app."org.example.ping"]
        package = "ping.pkg"
        grant = ["exec:tracepath"]
        state = "running"

        [app."org.example.todo"]
        package = "/abs/todo.pkg"
    "#;

    fn packages() -> BTreeMap<String, Package> {
        let ping = Package {
            id: "org.example.ping".into(),
            version: "0.2.0".into(),
            permissions: vec![("exec:ping".into(), false), ("exec:tracepath".into(), true)],
        };
        let todo = Package { id: "org.example.todo".into(), version: "1.0.0".into(), permissions: vec![] };
        [ping, todo].into_iter().map(|p| (p.id.clone(), p)).collect()
    }

    fn current(id: &str, version: &str, running: bool, granted: &[&str]) -> Current {
        Current {
            id: id.into(),
            version: version.into(),
            running,
            granted: granted.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn names(plan: &Plan) -> Vec<String> {
        plan.steps.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_and_resolves_paths() {
        let s = DesiredState::parse(STATE, Path::new("/etc/wshell")).unwrap();
        assert_eq!(s.apps["org.example.ping"].package, Source::Path("/etc/wshell/ping.pkg".into()));
        assert_eq!(s.apps["org.example.todo"].package, Source::Path("/abs/todo.pkg".into()));
        assert_eq!(s.apps["org.example.ping"].state, Some(RunState::Running));
        assert_eq!(s.apps["org.example.todo"].state, None);
        assert!(DesiredState::parse("[app.Bad]\npackage = \"x\"", Path::new("/")).is_err());
        assert!(DesiredState::parse("[app.\"a.b\"]\npackage = \"x\"\nstat = \"running\"", Path::new("/")).is_err());
    }

    #[test]
    fn fresh_install() {
        let s = DesiredState::parse(STATE, Path::new("/")).unwrap();
        let plan = plan(&s, &packages(), &[], false).unwrap();
        assert_eq!(
            names(&plan),
            ["install org.example.ping 0.2.0", "start org.example.ping", "install org.example.todo 1.0.0"]
        );
    }

    #[test]
    fn up_to_date_is_a_no_op() {
        let s = DesiredState::parse(STATE, Path::new("/")).unwrap();
        let cur = [
            current("org.example.ping", "0.2.0", true, &["exec:ping", "exec:tracepath"]),
            // The run state of todo is not managed: running or not, nothing to do.
            current("org.example.todo", "1.0.0", true, &[]),
        ];
        assert!(plan(&s, &packages(), &cur, false).unwrap().steps.is_empty());
    }

    #[test]
    fn update_stops_and_restarts() {
        let s = DesiredState::parse(STATE, Path::new("/")).unwrap();
        let cur = [
            current("org.example.ping", "0.1.0", true, &["exec:ping", "exec:tracepath"]),
            current("org.example.todo", "1.0.0", false, &[]),
        ];
        assert_eq!(
            names(&plan(&s, &packages(), &cur, false).unwrap()),
            ["stop org.example.ping", "update org.example.ping 0.1.0 → 0.2.0", "start org.example.ping"]
        );
    }

    #[test]
    fn grant_change_reinstalls() {
        let s = DesiredState::parse(STATE, Path::new("/")).unwrap();
        let cur = [
            current("org.example.ping", "0.2.0", false, &["exec:ping"]),
            current("org.example.todo", "1.0.0", false, &[]),
        ];
        assert_eq!(
            names(&plan(&s, &packages(), &cur, false).unwrap()),
            ["reinstall org.example.ping 0.2.0 (permissions changed)", "start org.example.ping"]
        );
    }

    #[test]
    fn unlisted_apps_and_prune() {
        let s = DesiredState::parse(STATE, Path::new("/")).unwrap();
        let cur = [
            current("org.example.ping", "0.2.0", true, &["exec:ping", "exec:tracepath"]),
            current("org.example.todo", "1.0.0", false, &[]),
            current("org.example.old", "1.0.0", true, &[]),
        ];
        // An empty state does not remove everything.
        let empty = DesiredState::parse("", Path::new("/")).unwrap();
        assert!(plan(&empty, &packages(), &cur, true).is_err());
        assert!(plan(&empty, &packages(), &[], true).unwrap().steps.is_empty());
        let kept = plan(&s, &packages(), &cur, false).unwrap();
        assert!(kept.steps.is_empty());
        assert_eq!(kept.unlisted, ["org.example.old"]);
        assert_eq!(
            names(&plan(&s, &packages(), &cur, true).unwrap()),
            ["stop org.example.old", "uninstall org.example.old (with its data)"]
        );
    }

    #[test]
    fn unread_packages_are_held() {
        let s = DesiredState::parse(STATE, Path::new("/")).unwrap();
        let mut pkgs = packages();
        pkgs.remove("org.example.todo");
        let cur = [current("org.example.todo", "0.9.0", true, &[])];
        let plan = plan(&s, &pkgs, &cur, true).unwrap();
        assert_eq!(names(&plan), ["install org.example.ping 0.2.0", "start org.example.ping"]);
        assert_eq!(plan.held, ["org.example.todo"]);
    }

    #[test]
    fn archive_entries() {
        use std::io::Write;
        assert_eq!(archive_path(Path::new("./app.toml")), Path::new("app.toml"));
        assert_eq!(archive_path(Path::new("/ui/./x.js")), Path::new("ui/x.js"));
        assert_eq!(archive_path(Path::new("../app.toml")), Path::new("../app.toml"));
        let manifest = |id: &str| {
            format!("[app]\nid = \"{id}\"\nname = \"T\"\nversion = \"1.0.0\"\nshell_api = \"0.1\"\n[backend]\nworld = \"shell:app/app@0.1.0\"\n")
        };
        let pkg = |entries: &[(&str, String)]| {
            let path = std::env::temp_dir().join(format!("wshell-test-{}-{}.pkg", std::process::id(), entries.len()));
            let mut ar = tar::Builder::new(std::fs::File::create(&path).unwrap());
            for (name, text) in entries {
                let mut h = tar::Header::new_gnu();
                h.set_size(text.len() as u64);
                h.set_mode(0o644);
                // `append_data` refuses `./`; the raw name field takes it as written.
                h.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
                h.set_cksum();
                ar.append(&h, text.as_bytes()).unwrap();
            }
            ar.into_inner().unwrap().flush().unwrap();
            let read = Package::read(&path);
            std::fs::remove_file(&path).unwrap();
            read
        };
        assert_eq!(pkg(&[("./app.toml", manifest("org.example.todo"))]).unwrap().id, "org.example.todo");
        let two = [("app.toml", manifest("org.example.todo")), ("./app.toml", manifest("org.example.bank"))];
        assert!(pkg(&two).unwrap_err().to_string().contains("more than one"));
    }

    #[test]
    fn stopped_state() {
        let s = DesiredState::parse(
            "[app.\"org.example.todo\"]\npackage = \"t.pkg\"\nstate = \"stopped\"",
            Path::new("/"),
        )
        .unwrap();
        let cur = [current("org.example.todo", "1.0.0", true, &[])];
        assert_eq!(names(&plan(&s, &packages(), &cur, false).unwrap()), ["stop org.example.todo"]);
    }

    #[test]
    fn rejects_bad_grants_and_ids() {
        let pkgs = packages();
        let bad = |text: &str| plan(&DesiredState::parse(text, Path::new("/")).unwrap(), &pkgs, &[], false);
        let e = bad("[app.\"org.example.ping\"]\npackage = \"p\"\ngrant = [\"exec:ping\"]").unwrap_err();
        assert!(e.to_string().contains("required"), "{e}");
        let e = bad("[app.\"org.example.ping\"]\npackage = \"p\"\ngrant = [\"net.http\"]").unwrap_err();
        assert!(e.to_string().contains("not declared"), "{e}");
        // The key names one app, the package holds another.
        let mut pkgs = packages();
        pkgs.get_mut("org.example.todo").unwrap().id = "org.example.other".into();
        let s = DesiredState::parse("[app.\"org.example.todo\"]\npackage = \"t\"", Path::new("/")).unwrap();
        assert!(plan(&s, &pkgs, &[], false).is_err());
    }
}
