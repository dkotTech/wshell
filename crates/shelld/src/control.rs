//! Control Unix socket for `shellctl`: JSON, one message per line.
//! Plus extra sockets from `[control.sockets]`: the same protocol, only the allowed requests.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use shell_core::control::{Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::supervisor::Supervisor;

/// Rules of an extra socket; `None` for `control.sock` (everything is allowed).
type Allow = Option<Arc<[String]>>;

pub async fn serve(sup: Arc<Supervisor>) -> Result<()> {
    let dir = sup.cfg.runtime_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // 0711: a component running as another user (in Shell's group) has to reach its
    // extra socket; listing is still denied, and control.sock itself is 0600.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o711))?;
    let control = bind(&shell_core::config::control_socket(&dir), 0o600).await?;
    for (name, socket) in &sup.cfg.control.sockets {
        let listener = bind(&shell_core::config::extra_socket(&dir, name), socket.mode).await?;
        tokio::spawn(accept(sup.clone(), listener, Some(socket.allow.clone().into())));
    }
    accept(sup, control, None).await
}

async fn bind(path: &Path, mode: u32) -> Result<UnixListener> {
    prepare_socket(path).await?;
    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    tracing::info!("control socket: {}", path.display());
    Ok(listener)
}

async fn accept(sup: Arc<Supervisor>, listener: UnixListener, allow: Allow) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let (sup, allow) = (sup.clone(), allow.clone());
        tokio::spawn(async move {
            if let Err(e) = client(sup, stream, allow).await {
                tracing::debug!("control: {e:#}");
            }
        });
    }
}

async fn prepare_socket(path: &Path) -> Result<()> {
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            bail!("shelld is already running ({})", path.display());
        }
        std::fs::remove_file(path)?;
    }
    Ok(())
}

async fn client(sup: Arc<Supervisor>, stream: UnixStream, allow: Allow) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    while let Some(line) = lines.next_line().await? {
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                send(&mut wr, &Response::Error { message: format!("invalid request: {e}") }).await?;
                continue;
            }
        };
        if let Some(rules) = &allow
            && !rules.iter().any(|r| req.allowed_by(r))
        {
            let message = format!("{} is not allowed on this socket", req.name());
            send(&mut wr, &Response::Error { message }).await?;
            continue;
        }
        if let Request::Logs { id, lines: n, follow } = req {
            let Some(log) = sup.app_log(&id) else {
                send(&mut wr, &Response::Error { message: format!("app {id} is not installed") }).await?;
                continue;
            };
            let (tail, mut rx) = log.tail_and_subscribe(n);
            for line in tail {
                send(&mut wr, &Response::Log { line }).await?;
            }
            if !follow {
                send(&mut wr, &Response::Ok).await?;
                continue;
            }
            loop {
                match rx.recv().await {
                    Ok(line) => send(&mut wr, &Response::Log { line }).await?,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        send(&mut wr, &Response::Log { line: format!("… lines skipped: {n}") }).await?
                    }
                    Err(_) => return Ok(()),
                }
            }
        }
        let resp = handle(&sup, req).await.unwrap_or_else(|e| Response::Error { message: format!("{e:#}") });
        send(&mut wr, &resp).await?;
    }
    Ok(())
}

async fn handle(sup: &Arc<Supervisor>, req: Request) -> Result<Response> {
    Ok(match req {
        Request::Inspect { path } => Response::Package(sup.inspect(path).await?),
        Request::Install { path, grant_optional } => {
            let (id, version) = sup.install(path, grant_optional).await?;
            Response::Installed { id, version }
        }
        Request::Uninstall { id } => {
            sup.uninstall(&id).await?;
            Response::Ok
        }
        Request::List => Response::Apps { apps: sup.list() },
        Request::Start { id } => {
            sup.start(&id).await?;
            match sup.issue_url(&id) {
                Ok(url) => Response::Url { url },
                Err(_) => Response::Ok,
            }
        }
        Request::Stop { id, force } => {
            sup.stop(&id, shell_core::control::StopReason::User, force).await?;
            Response::Ok
        }
        Request::Url { id } => Response::Url { url: sup.issue_url(&id)? },
        Request::Call { id, method, payload } => {
            serde_json::from_str::<serde_json::Value>(&payload).context("payload must be JSON")?;
            let result = sup.call(&id, method, payload).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            Response::CallResult { result }
        }
        Request::DashboardUrl => Response::Url { url: sup.issue_dashboard_url()? },
        Request::Logs { .. } => unreachable!("handled in the client"),
    })
}

async fn send(wr: &mut (impl AsyncWriteExt + Unpin), resp: &Response) -> Result<()> {
    let mut line = serde_json::to_vec(resp)?;
    line.push(b'\n');
    wr.write_all(&line).await?;
    Ok(())
}
