//! Example: HTTP requests via standard `wasi:http` (`net.http` permission).
//! The backend uses the ordinary `wstd` client; Shell checks the hosts and performs the request.

use serde::Deserialize;
use serde_json::json;
use wstd::http::{Body, Client, Request};

wit_bindgen::generate!({
    world: "app",
    path: "../../wit",
});

use exports::shell::app::bridge::Guest as Bridge;
use exports::shell::app::lifecycle::{Guest as Lifecycle, HostEvent};

struct App;

impl Lifecycle for App {
    fn on_start() -> Result<(), String> {
        Ok(())
    }
    fn on_stop() {}
    fn on_event(_: HostEvent) {}
}

#[derive(Deserialize)]
struct GetArgs {
    url: String,
}

impl Bridge for App {
    fn handle(method: String, payload: String) -> Result<String, String> {
        match method.as_str() {
            "get" => {
                let args: GetArgs = serde_json::from_str(&payload).map_err(|e| e.to_string())?;
                wstd::runtime::block_on(get(&args.url)).map_err(|e| format!("{e:#}"))
            }
            _ => Err(format!("unknown method: {method}")),
        }
    }
}

async fn get(url: &str) -> wstd::http::Result<String> {
    let request = Request::get(url).body(Body::empty())?;
    let mut response = Client::new().send(request).await?;
    let status = response.status().as_u16();
    let body = response.body_mut().contents().await?;
    let preview: String = String::from_utf8_lossy(body).chars().take(120).collect();
    Ok(json!({ "status": status, "bytes": body.len(), "preview": preview }).to_string())
}

export!(App);
