//! wshell, as seen by the Renderer: its home screen is a launcher plugin, opened through
//! one-time links to the plugin's UI. The links come from an extra control socket of shelld
//! (`[control.sockets.render]` with `allow = ["url:<launcher id>"]`), which allows nothing else.

use std::path::PathBuf;

use shell_core::control::{Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

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
        let mut line = serde_json::to_vec(&Request::Url { id }).map_err(|e| fail(&e))?;
        line.push(b'\n');
        wr.write_all(&line).await.map_err(|e| fail(&e))?;
        let reply = BufReader::new(rd).lines().next_line().await.map_err(|e| fail(&e))?;
        match serde_json::from_str(reply.as_deref().unwrap_or_default()) {
            Ok(Response::Url { url }) => Ok(url),
            Ok(Response::Error { message }) => Err(fail(&message)),
            Ok(other) => Err(fail(&format!("unexpected reply {other:?}"))),
            Err(e) => Err(fail(&e)),
        }
    }
}
