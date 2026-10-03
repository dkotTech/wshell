//! Test app for checking resource limits (fuel, memory). No UI.

wit_bindgen::generate!({
    world: "app",
    path: "../../wit",
});

use std::cell::RefCell;

use exports::shell::app::bridge::Guest as Bridge;
use exports::shell::app::lifecycle::{Guest as Lifecycle, HostEvent};
use shell::app::{activity, log};

#[derive(Default)]
struct State {
    busy: Vec<activity::Busy>,
    /// Release locks on stop-requested.
    cooperative: bool,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

struct App;

impl Lifecycle for App {
    fn on_start() -> Result<(), String> {
        Ok(())
    }
    fn on_stop() {}
    fn on_event(event: HostEvent) {
        if let HostEvent::StopRequested(req) = event {
            let cooperative = STATE.with_borrow(|s| s.cooperative);
            log::log(log::Level::Info, &format!("stop-requested {:?}, grace {} ms, cooperative={cooperative}", req.reason, req.grace_ms));
            if cooperative {
                STATE.with_borrow_mut(|s| s.busy.clear());
            }
        }
    }
}

impl Bridge for App {
    fn handle(method: String, payload: String) -> Result<String, String> {
        match method.as_str() {
            "echo" => Ok(payload),
            // Take a busy lock; payload: {"reason": "...", "cooperative": bool}.
            "busy" => {
                let v: serde_json::Value = serde_json::from_str(&payload).map_err(|e| e.to_string())?;
                let reason = v["reason"].as_str().unwrap_or("stress").to_string();
                STATE.with_borrow_mut(|s| {
                    s.cooperative = v["cooperative"].as_bool().unwrap_or(false);
                    s.busy.push(activity::Busy::new(&reason));
                });
                Ok("null".into())
            }
            "release" => {
                STATE.with_borrow_mut(|s| s.busy.clear());
                Ok("null".into())
            }
            "exit" => {
                activity::request_stop();
                Ok("null".into())
            }
            // Infinite loop: must be interrupted by fuel.
            "spin" => {
                let mut x: u64 = 0;
                loop {
                    x = std::hint::black_box(x.wrapping_add(1));
                }
            }
            // Allocate memory 1 MB at a time: must hit the linear memory limit.
            "alloc" => {
                let mut blocks: Vec<Vec<u8>> = Vec::new();
                loop {
                    blocks.push(vec![1u8; 1 << 20]);
                    std::hint::black_box(&blocks);
                }
            }
            _ => Err(format!("unknown method {method}")),
        }
    }
}

export!(App);
