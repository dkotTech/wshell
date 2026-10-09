//! systemd over D-Bus for the `system.services` permission. Only the supervisor talks to
//! systemd: the worker asks over IPC, and `hostsvc` checks the unit and the action first.

use anyhow::{Context, Result};
use shell_core::config::Bus;
use shell_core::manifest::ServiceAction;
use zbus::zvariant::OwnedObjectPath;

use crate::ipc::UnitStatus;

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
trait Manager {
    fn load_unit(&self, name: &str) -> zbus::Result<OwnedObjectPath>;
    fn start_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn restart_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn reload_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    /// Loaded units: name, description, load, active and sub state, followed unit, object
    /// path, job id, job type, job path.
    #[allow(clippy::type_complexity)]
    fn list_units_by_patterns(
        &self,
        states: &[&str],
        patterns: &[&str],
    ) -> zbus::Result<Vec<(String, String, String, String, String, String, OwnedObjectPath, u32, String, OwnedObjectPath)>>;
    /// Installed unit files: path and enablement state.
    fn list_unit_files_by_patterns(&self, states: &[&str], patterns: &[&str]) -> zbus::Result<Vec<(String, String)>>;
}

#[zbus::proxy(interface = "org.freedesktop.systemd1.Unit", default_service = "org.freedesktop.systemd1")]
trait Unit {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn description(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn load_state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn active_state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn sub_state(&self) -> zbus::Result<String>;
}

pub async fn connect(bus: Bus) -> Result<zbus::Connection> {
    match bus {
        Bus::System => zbus::Connection::system().await.context("D-Bus system bus"),
        Bus::User => zbus::Connection::session().await.context("D-Bus user (session) bus"),
    }
}

/// Also for a unit that is not loaded or does not exist (`load-state` says so).
pub async fn status(conn: &zbus::Connection, unit: &str) -> Result<UnitStatus> {
    let path = ManagerProxy::new(conn).await?.load_unit(unit).await.map_err(explain)?;
    let u = UnitProxy::builder(conn).path(path)?.build().await?;
    Ok(UnitStatus {
        name: u.id().await?,
        description: u.description().await?,
        load_state: u.load_state().await?,
        active_state: u.active_state().await?,
        sub_state: u.sub_state().await?,
    })
}

/// Units matching systemd glob patterns: the loaded ones and those with a unit file (a stopped
/// unit nothing refers to is not loaded). Templates (`name@.service`) are skipped.
pub async fn list(conn: &zbus::Connection, patterns: &[&str]) -> Result<Vec<UnitStatus>> {
    let m = ManagerProxy::new(conn).await?;
    let mut units: Vec<UnitStatus> = m
        .list_units_by_patterns(&[], patterns)
        .await
        .map_err(explain)?
        .into_iter()
        .map(|(name, description, load_state, active_state, sub_state, ..)| UnitStatus {
            name,
            description,
            load_state,
            active_state,
            sub_state,
        })
        .collect();
    for (path, _) in m.list_unit_files_by_patterns(&[], patterns).await.map_err(explain)? {
        let Some(name) = path.rsplit('/').next() else { continue };
        if name.contains("@.") || units.iter().any(|u| u.name == name) {
            continue;
        }
        units.push(status(conn, name).await?);
    }
    Ok(units)
}

/// Queues the job (`replace` mode, like `systemctl`) without waiting for it to finish.
pub async fn control(conn: &zbus::Connection, unit: &str, action: ServiceAction) -> Result<()> {
    let m = ManagerProxy::new(conn).await?;
    let job = match action {
        ServiceAction::Start => m.start_unit(unit, "replace").await,
        ServiceAction::Stop => m.stop_unit(unit, "replace").await,
        ServiceAction::Restart => m.restart_unit(unit, "replace").await,
        ServiceAction::Reload => m.reload_unit(unit, "replace").await,
    };
    job.map(drop).map_err(explain)
}

/// systemd's errors as they are, with a hint for the usual one on the system bus.
fn explain(e: zbus::Error) -> anyhow::Error {
    match &e {
        zbus::Error::MethodError(name, msg, _) if name.as_str().ends_with("AccessDenied") => anyhow::anyhow!(
            "systemd denied it: {} (on the system bus, a polkit rule must allow the shelld user)",
            msg.as_deref().unwrap_or("access denied")
        ),
        zbus::Error::MethodError(_, Some(msg), _) => anyhow::anyhow!("systemd: {msg}"),
        _ => anyhow::Error::new(e),
    }
}
