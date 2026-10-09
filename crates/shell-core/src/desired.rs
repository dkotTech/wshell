//! Desired state of the device (`shellctl apply`): which apps are installed, from which packages,
//! with which optional permissions, and whether they run. The plan is the difference between it
//! and what is installed; applying the same file twice does nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredState {
    #[serde(default, rename = "app")]
    pub apps: BTreeMap<String, DesiredApp>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredApp {
    /// A `.pkg` archive or a package directory; relative to the state file.
    pub package: PathBuf,
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
            if app.package.is_relative() {
                app.package = base.join(&app.package);
            }
        }
        Ok(s)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("{}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        Self::parse(&text, base).with_context(|| format!("{}", path.display()))
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

/// An installed app as far as the plan is concerned.
#[derive(Debug, Clone)]
pub struct Current {
    pub id: String,
    pub version: String,
    pub running: bool,
    pub granted: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Stop { id: String },
    Install { id: String, package: PathBuf, grant: Vec<String>, from: Option<String>, version: String },
    Start { id: String },
    Uninstall { id: String },
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
}

/// Compares the desired state with what is installed. `packages` holds every desired app's package.
pub fn plan(
    desired: &DesiredState,
    packages: &BTreeMap<String, Package>,
    current: &[Current],
    prune: bool,
) -> Result<Plan> {
    let mut plan = Plan::default();
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
        let pkg = packages.get(id).with_context(|| format!("{id}: package not read"))?;
        ensure!(&pkg.id == id, "{id}: {} contains app {}", app.package.display(), pkg.id);
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
        assert_eq!(s.apps["org.example.ping"].package, Path::new("/etc/wshell/ping.pkg"));
        assert_eq!(s.apps["org.example.todo"].package, Path::new("/abs/todo.pkg"));
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
        let kept = plan(&s, &packages(), &cur, false).unwrap();
        assert!(kept.steps.is_empty());
        assert_eq!(kept.unlisted, ["org.example.old"]);
        assert_eq!(
            names(&plan(&s, &packages(), &cur, true).unwrap()),
            ["stop org.example.old", "uninstall org.example.old (with its data)"]
        );
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
