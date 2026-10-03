//! Launcher plugin: the home screen of a device. Lists the apps with a UI and opens one:
//! Shell gives a one-time link to the app's UI (`shell.apps`), the page navigates there.
//! The Renderer brings the launcher back (App switch) with its own one-time link.

use std::cell::RefCell;

use serde::Deserialize;
use serde_json::json;

wit_bindgen::generate!({
    world: "plugin",
    path: "../../wit",
});

use exports::shell::app::bridge::Guest as Bridge;
use exports::shell::app::lifecycle::{Guest as Lifecycle, HostEvent};
use shell::app::apps;

thread_local! {
    static REGISTRY: RefCell<Option<apps::Registry>> = const { RefCell::new(None) };
}

#[derive(Deserialize)]
struct Open {
    id: String,
}

struct Launcher;

impl Lifecycle for Launcher {
    fn on_start() -> Result<(), String> {
        let registry = apps::open().ok_or("permission shell.apps is not granted")?;
        REGISTRY.set(Some(registry));
        Ok(())
    }

    fn on_stop() {}

    fn on_event(_event: HostEvent) {}
}

impl Bridge for Launcher {
    fn handle(method: String, payload: String) -> Result<String, String> {
        REGISTRY.with_borrow(|registry| {
            let registry = registry.as_ref().ok_or("not started")?;
            match method.as_str() {
                "apps" => Ok(list(registry)),
                "open" => {
                    let Open { id } = serde_json::from_str(&payload).map_err(|e| format!("open: {e}"))?;
                    let url = registry.ui_link(&id)?;
                    Ok(json!({ "url": url }).to_string())
                }
                other => Err(format!("unknown method {other}")),
            }
        })
    }
}

/// Apps with a UI, by name; plugins (including this one) are not apps to launch.
fn list(registry: &apps::Registry) -> String {
    let mut apps: Vec<_> = registry.list().into_iter().filter(|a| a.has_ui && !a.plugin).collect();
    apps.sort_by_key(|a| a.name.to_lowercase());
    let apps: Vec<_> = apps
        .iter()
        .map(|a| json!({ "id": a.id, "name": a.name, "state": state(a.state) }))
        .collect();
    json!(apps).to_string()
}

fn state(s: apps::State) -> &'static str {
    match s {
        apps::State::Stopped => "stopped",
        apps::State::Starting => "starting",
        apps::State::Running => "running",
        apps::State::Suspended => "suspended",
        apps::State::Stopping => "stopping",
        apps::State::Failed => "failed",
    }
}

export!(Launcher);
