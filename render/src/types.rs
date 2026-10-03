use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// One encoded PNG frame, shared between all subscribers.
pub type Frame = Arc<Vec<u8>>;

/// Last encoded frame of a render — sent to new WebSocket clients immediately
/// so they don't have to wait for the page to repaint.
pub type LastFrame = Arc<Mutex<Option<Frame>>>;

// ── Remote input ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyAction {
    /// Down + up in one event (default).
    #[default]
    Press,
    Down,
    Up,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    #[default]
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ButtonAction {
    /// Down + up in one event (default).
    #[default]
    Click,
    Down,
    Up,
}

/// An input event sent by a remote controller, JS-flavoured:
/// `key` uses KeyboardEvent.key values ("a", "Enter", "ArrowUp", " ").
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteEvent {
    Key {
        key: String,
        #[serde(default)]
        state: KeyAction,
    },
    MouseMove {
        x: f32,
        y: f32,
    },
    MouseButton {
        #[serde(default)]
        button: Button,
        #[serde(default)]
        state: ButtonAction,
        x: f32,
        y: f32,
    },
    Wheel {
        x: f32,
        y: f32,
        #[serde(default)]
        dx: f64,
        #[serde(default)]
        dy: f64,
    },
}

// ── Engine commands ──────────────────────────────────────────────────────────

/// Everything the engine thread needs to start one render.
/// The engine only keeps `last_frame` fresh; broadcasting at a steady fps is the
/// API-side ticker's job (see `spawn_render`).
pub struct RenderSpec {
    pub id: u64,
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub last_frame: LastFrame,
}

/// Commands flowing API → engine thread.
pub enum EngineCmd {
    Create(RenderSpec),
    Destroy { id: u64 },
    Input { id: u64, event: RemoteEvent },
    Navigate { id: u64, url: String },
    Resize { id: u64, width: u32, height: u32 },
}

// ── Shared registry (API side) ───────────────────────────────────────────────

pub struct RenderEntry {
    pub id: u64,
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub frame_tx: broadcast::Sender<Frame>,
    pub last_frame: LastFrame,
    /// Steady-fps broadcast ticker; aborted when the render is destroyed.
    pub ticker: tokio::task::AbortHandle,
    /// Destroyed when its last WebSocket viewer leaves (a terminal's own render).
    pub ephemeral: bool,
    /// Open WebSocket sessions.
    pub viewers: usize,
}

pub type Registry = Arc<Mutex<HashMap<u64, RenderEntry>>>;
