//! Example app: the network (`system.network`). Shows the devices with their addresses and
//! the Wi-Fi networks in range; connects and disconnects them through NetworkManager.

use std::cell::RefCell;

use serde::Deserialize;
use serde_json::json;

wit_bindgen::generate!({
    world: "app",
    path: "../../wit",
});

use exports::shell::app::bridge::Guest as Bridge;
use exports::shell::app::lifecycle::{Guest as Lifecycle, HostEvent};
use shell::app::network;

thread_local! {
    static MANAGER: RefCell<Option<network::Manager>> = const { RefCell::new(None) };
}

struct App;

impl Lifecycle for App {
    fn on_start() -> Result<(), String> {
        let manager = network::open().ok_or("system.network is not granted")?;
        MANAGER.set(Some(manager));
        Ok(())
    }
    fn on_stop() {}
    fn on_event(_: HostEvent) {}
}

#[derive(Deserialize)]
struct DeviceArgs {
    device: String,
}

#[derive(Deserialize)]
struct WifiArgs {
    device: String,
    ssid: String,
    password: Option<String>,
}

fn args<T: for<'a> Deserialize<'a>>(payload: &str) -> Result<T, String> {
    serde_json::from_str(payload).map_err(|e| format!("invalid parameters: {e}"))
}

impl Bridge for App {
    fn handle(method: String, payload: String) -> Result<String, String> {
        MANAGER.with_borrow(|m| {
            let m = m.as_ref().ok_or("not started")?;
            let reply = match method.as_str() {
                "devices" => m.devices()?.iter().map(device_json).collect(),
                "access-points" => {
                    let a: DeviceArgs = args(&payload)?;
                    m.access_points(&a.device)?.iter().map(access_point_json).collect()
                }
                "scan" => m.scan(&args::<DeviceArgs>(&payload)?.device).map(|()| json!(null))?,
                "connect" => m.connect(&args::<DeviceArgs>(&payload)?.device).map(|()| json!(null))?,
                "disconnect" => m.disconnect(&args::<DeviceArgs>(&payload)?.device).map(|()| json!(null))?,
                "connect-wifi" => {
                    let a: WifiArgs = args(&payload)?;
                    // An empty field in the form means no password.
                    let password = a.password.filter(|p| !p.is_empty());
                    m.connect_wifi(&a.device, &a.ssid, password.as_deref()).map(|()| json!(null))?
                }
                _ => return Err(format!("unknown method: {method}")),
            };
            Ok(reply.to_string())
        })
    }
}

fn device_json(d: &network::Device) -> serde_json::Value {
    json!({
        "name": d.name,
        "kind": d.kind,
        "state": d.state,
        "connection": d.connection,
        "hw_address": d.hw_address,
        "ipv4": d.ipv4,
        "ipv6": d.ipv6,
        "gateway": d.gateway,
        "dns": d.dns,
    })
}

fn access_point_json(a: &network::AccessPoint) -> serde_json::Value {
    json!({
        "ssid": a.ssid,
        "strength": a.strength,
        "secured": a.secured,
        "active": a.active,
        "saved": a.saved,
    })
}

export!(App);
