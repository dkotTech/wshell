//! Example app: systemd services (`system.services`). Shows the units the manifest allows
//! and starts, stops and restarts them; Shell checks every unit and action.

use std::cell::RefCell;

use serde::Deserialize;
use serde_json::json;

wit_bindgen::generate!({
    world: "app",
    path: "../../wit",
});

use exports::shell::app::bridge::Guest as Bridge;
use exports::shell::app::lifecycle::{Guest as Lifecycle, HostEvent};
use shell::app::services;

thread_local! {
    static MANAGER: RefCell<Option<services::Manager>> = const { RefCell::new(None) };
}

struct App;

impl Lifecycle for App {
    fn on_start() -> Result<(), String> {
        let manager = services::open().ok_or("system.services is not granted")?;
        MANAGER.set(Some(manager));
        Ok(())
    }
    fn on_stop() {}
    fn on_event(_: HostEvent) {}
}

#[derive(Deserialize)]
struct ControlArgs {
    unit: String,
    action: String,
}

impl Bridge for App {
    fn handle(method: String, payload: String) -> Result<String, String> {
        MANAGER.with_borrow(|m| {
            let m = m.as_ref().ok_or("not started")?;
            match method.as_str() {
                "list" => {
                    let units: Vec<_> = m.list()?.iter().map(to_json).collect();
                    Ok(serde_json::Value::from(units).to_string())
                }
                "control" => {
                    let args: ControlArgs =
                        serde_json::from_str(&payload).map_err(|e| format!("invalid parameters: {e}"))?;
                    let action = match args.action.as_str() {
                        "start" => services::Action::Start,
                        "stop" => services::Action::Stop,
                        "restart" => services::Action::Restart,
                        "reload" => services::Action::Reload,
                        a => return Err(format!("unknown action {a}")),
                    };
                    m.control(&args.unit, action)?;
                    // The job is queued; the UI polls the state.
                    Ok(to_json(&m.status(&args.unit)?).to_string())
                }
                _ => Err(format!("unknown method: {method}")),
            }
        })
    }
}

fn to_json(u: &services::UnitStatus) -> serde_json::Value {
    json!({
        "name": u.name,
        "description": u.description,
        "load": u.load_state,
        "active": u.active_state,
        "sub": u.sub_state,
    })
}

export!(App);
