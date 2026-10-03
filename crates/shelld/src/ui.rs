//! UI HTTP server and bridge. Loopback only; one origin per app:
//! `http://<app-id>.localhost:<port>/`.

use std::sync::Arc;

use anyhow::Result;
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::supervisor::{CallError, Scope, Supervisor};
use crate::web;

const SESSION_COOKIE: &str = "wshell_session";
const SHELL_JS: &str = include_str!("../assets/shell.js");

/// App id determined from the `Host` header.
#[derive(Clone)]
struct AppId(String);

pub async fn serve(sup: Arc<Supervisor>) -> Result<()> {
    let port = sup.cfg.http.port;
    let app = Router::new()
        .route("/_shell/auth", get(auth))
        .route("/_shell/shell.js", get(shell_js))
        .route("/_shell/call", post(call))
        .route("/_shell/events", get(events))
        .fallback(static_file)
        .layer(middleware::from_fn_with_state(sup.clone(), resolve_app))
        .layer(middleware::map_response(web::security_headers))
        .with_state(sup);

    tracing::info!("UI: http://<app-id>.localhost:{port}/");
    web::serve_loopback("UI", port, app).await
}

/// Strict `Host` check against the list of apps (DNS rebinding protection).
async fn resolve_app(State(sup): State<Arc<Supervisor>>, mut req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let suffix = format!(".localhost:{}", sup.cfg.http.port);
    let id = host.strip_suffix(&suffix).filter(|id| sup.is_installed(id));
    match id {
        Some(id) => {
            let id = id.to_string();
            req.extensions_mut().insert(AppId(id));
            next.run(req).await
        }
        None => (StatusCode::MISDIRECTED_REQUEST, "unknown host").into_response(),
    }
}

fn session_ok(sup: &Supervisor, id: &str, headers: &HeaderMap) -> bool {
    web::session_ok(sup, &Scope::App(id.to_string()), headers, SESSION_COOKIE)
}

/// Bridge requests: session + `Origin` strictly of our own app.
fn bridge_allowed(sup: &Supervisor, id: &str, headers: &HeaderMap) -> bool {
    let origin_ok = headers
        .get(header::ORIGIN)
        .and_then(|o| o.to_str().ok())
        .is_some_and(|o| o == sup.origin(id));
    origin_ok && session_ok(sup, id, headers)
}

fn unauthorized(id: &str) -> Response {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>wshell</title>\
         <p>No session. Open the app via a one-time link: <code>shellctl url {id}</code></p>"
    );
    (StatusCode::UNAUTHORIZED, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response()
}

#[derive(Deserialize)]
struct AuthQuery {
    token: String,
}

async fn auth(State(sup): State<Arc<Supervisor>>, Extension(AppId(id)): Extension<AppId>, Query(q): Query<AuthQuery>) -> Response {
    match sup.redeem_token(&Scope::App(id), &q.token) {
        Some(session) => web::auth_success(SESSION_COOKIE, &session),
        None => (StatusCode::FORBIDDEN, "token is invalid or already used").into_response(),
    }
}

async fn shell_js() -> Response {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], SHELL_JS)
        .into_response()
}

#[derive(Deserialize)]
struct CallBody {
    method: String,
    #[serde(default)]
    params: Value,
}

async fn call(
    State(sup): State<Arc<Supervisor>>,
    Extension(AppId(id)): Extension<AppId>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !bridge_allowed(&sup, &id, &headers) {
        return web::json_error(StatusCode::FORBIDDEN, "forbidden");
    }
    if !web::content_type_is(&headers, "application/json") {
        return web::json_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "expected application/json");
    }
    if body.len() > sup.cfg.http.max_message_kb as usize * 1024 {
        return web::json_error(StatusCode::PAYLOAD_TOO_LARGE, "message too large");
    }
    let Ok(req) = serde_json::from_slice::<CallBody>(&body) else {
        return web::json_error(StatusCode::BAD_REQUEST, "expected {method, params}");
    };
    match sup.call(&id, req.method, req.params.to_string()).await {
        Ok(result) => {
            let value = serde_json::from_str(&result).unwrap_or(Value::String(result));
            Json(json!({ "result": value })).into_response()
        }
        Err(e) => {
            let status = match e {
                CallError::NotRunning | CallError::StartFailed(_) => StatusCode::SERVICE_UNAVAILABLE,
                CallError::Timeout => StatusCode::GATEWAY_TIMEOUT,
                CallError::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
                CallError::App(_) => StatusCode::UNPROCESSABLE_ENTITY,
            };
            web::json_error(status, e)
        }
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    /// The page was opened or became visible again: the app may be woken.
    /// Background reconnects of `shell.js` do not set this flag.
    /// The value does not matter, only its presence (`?wake=1`).
    wake: Option<String>,
}

async fn events(
    State(sup): State<Arc<Supervisor>>,
    Extension(AppId(id)): Extension<AppId>,
    Query(q): Query<EventsQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !bridge_allowed(&sup, &id, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let worker = match sup.worker(&id) {
        Some(w) => w,
        None if q.wake.is_some() => match sup.ensure_running(&id, "UI opened").await {
            Ok(w) => w,
            Err(e) => return web::json_error(StatusCode::SERVICE_UNAVAILABLE, e),
        },
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let limit = sup.cfg.http.max_message_kb as usize * 1024;
    ws.max_message_size(limit).on_upgrade(move |socket| events_loop(socket, worker))
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ClientMsg {
    Visibility { visible: bool },
}

async fn events_loop(mut socket: WebSocket, worker: Arc<crate::supervisor::Worker>) {
    let mut rx = worker.events.subscribe();
    let mut exited = worker.exited.clone();
    let mut visible = false;
    loop {
        tokio::select! {
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(ClientMsg::Visibility { visible: v }) = serde_json::from_str(&text) {
                        worker.set_visible(visible, v);
                        visible = v;
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            ev = rx.recv() => match ev {
                Ok(frame) if visible => {
                    if socket.send(Message::Text((*frame).into())).await.is_err() {
                        break;
                    }
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            },
            _ = async { exited.wait_for(|e| *e).await.map(|_| ()) } => break,
        }
    }
    worker.set_visible(visible, false);
    let _ = socket.send(Message::Close(None)).await;
}

async fn static_file(
    State(sup): State<Arc<Supervisor>>,
    Extension(AppId(id)): Extension<AppId>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if !session_ok(&sup, &id, &headers) {
        return unauthorized(&id);
    }
    let Some(installed) = sup.installed(&id) else { return StatusCode::NOT_FOUND.into_response() };
    let Some(ui) = &installed.manifest.ui else { return StatusCode::NOT_FOUND.into_response() };

    let rel = match uri.path() {
        "/" => ui.entry.clone(),
        p => match sanitize(p) {
            Some(p) => format!("ui/{p}"),
            None => return StatusCode::NOT_FOUND.into_response(),
        },
    };
    let path = installed.pkg_dir.join(&rel);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            let ct = if mime.type_() == "text" || mime.essence_str() == "application/javascript" {
                format!("{}; charset=utf-8", mime.essence_str())
            } else {
                mime.essence_str().to_string()
            };
            let body = if method == Method::HEAD { Body::empty() } else { Body::from(bytes) };
            ([(header::CONTENT_TYPE, ct.as_str()), (header::CACHE_CONTROL, "no-cache")], body).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Request path → relative path inside `ui/` without `..`, hidden files or special characters.
fn sanitize(path: &str) -> Option<String> {
    let decoded = percent_decode(path.strip_prefix('/')?)?;
    let ok = decoded
        .split('/')
        .all(|s| !s.is_empty() && !s.starts_with('.') && !s.contains(['\\', '\0']));
    ok.then_some(decoded)
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn sanitize_paths() {
        assert_eq!(sanitize("/app.js").as_deref(), Some("app.js"));
        assert_eq!(sanitize("/css/a%20b.css").as_deref(), Some("css/a b.css"));
        assert_eq!(sanitize("/../app.toml"), None);
        assert_eq!(sanitize("/%2e%2e/app.toml"), None);
        assert_eq!(sanitize("/a//b"), None);
        assert_eq!(sanitize("/.hidden"), None);
        assert_eq!(sanitize("/a%2"), None);
    }
}
