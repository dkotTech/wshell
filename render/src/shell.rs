//! wshell, as seen by the Renderer: its home screen is a launcher plugin, opened through
//! one-time links to the plugin's UI. The links come from an extra control socket of shelld
//! (`[control.sockets.render]` with `allow = ["url:<launcher id>"]`), which allows nothing else.
//!
//! Only wshell's control protocol is used here, not its code: one JSON object per line,
//! `{"cmd":"url","id":…}` → `{"type":"url","url":…}` or `{"type":"error","message":…}`.

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// shelld's default socket for the Renderer: `<runtime dir>/render.sock`, where the runtime
/// dir is `/run/wshell` for root and `$XDG_RUNTIME_DIR/wshell` otherwise (as in shelld).
pub fn default_socket() -> PathBuf {
    let root = std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0);
    let dir = if root {
        PathBuf::from("/run/wshell")
    } else {
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("wshell")
    };
    dir.join("render.sock")
}

pub struct Launcher {
    /// shelld's socket for the Renderer.
    pub socket: PathBuf,
    /// App id of the launcher plugin; `None`: no home screen.
    pub id: Option<String>,
}

impl Launcher {
    /// A fresh one-time link to the launcher's UI.
    pub async fn url(&self) -> Result<String, String> {
        let id = self.id.clone().ok_or("no launcher: [shell] launcher is empty")?;
        let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", self.socket.display());
        let stream = UnixStream::connect(&self.socket).await.map_err(|e| fail(&e))?;
        let (rd, mut wr) = stream.into_split();
        let mut line = json!({ "cmd": "url", "id": id }).to_string().into_bytes();
        line.push(b'\n');
        wr.write_all(&line).await.map_err(|e| fail(&e))?;
        let reply = BufReader::new(rd).lines().next_line().await.map_err(|e| fail(&e))?;
        let reply: Value = serde_json::from_str(reply.as_deref().unwrap_or_default()).map_err(|e| fail(&e))?;
        match (reply["type"].as_str(), reply["url"].as_str(), reply["message"].as_str()) {
            (Some("url"), Some(url), _) => Ok(url.to_string()),
            (Some("error"), _, Some(message)) => Err(fail(&message)),
            _ => Err(fail(&format!("unexpected reply {reply}"))),
        }
    }
}
