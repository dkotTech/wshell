//! Dashboard: app status (like `kubectl get pods`) and app management.
//!
//! Separate port, loopback only. Access via a one-time link from
//! `shellctl dashboard`, then an `HttpOnly; SameSite=Strict` cookie. Mutating
//! requests additionally check `Origin`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use shell_core::control::StopReason;

use crate::supervisor::{Scope, Supervisor};
use crate::util::unix_now;
use crate::web;

const COOKIE: &str = "wshell_dash";
const UPLOAD_TTL: Duration = Duration::from_secs(600);

/// The built dashboard (`dashboard/`, Preact + Vite), embedded in the binary.
#[derive(rust_embed::RustEmbed)]
#[folder = "../../dashboard/dist/"]
struct Dist;

struct Dash {
    sup: Arc<Supervisor>,
    /// Uploaded but not yet installed packages: upload id → file.
    uploads: Mutex<HashMap<String, (PathBuf, Instant)>>,
}

type St = State<Arc<Dash>>;

pub async fn serve(sup: Arc<Supervisor>) -> Result<()> {
    let port = sup.cfg.dashboard.port;
    let upload_limit = sup.cfg.dashboard.max_upload_mb as usize * 1024 * 1024;
    let dash = Arc::new(Dash { sup, uploads: Mutex::default() });

    let api = Router::new()
        .route("/overview", get(overview))
        .route("/apps/{id}/{action}", post(app_action))
        .route("/apps/{id}", delete(uninstall))
        .route("/apps/{id}/logs/stream", get(logs_stream))
        .route("/packages", post(upload).layer(DefaultBodyLimit::max(upload_limit)))
        .route("/packages/{upload}/install", post(install))
        .route("/packages/{upload}", delete(cancel_upload))
        .layer(middleware::from_fn_with_state(dash.clone(), require_session));

    let app = Router::new()
        .route("/_auth", get(auth))
        .route("/", get(page))
        .route("/assets/{*file}", get(page))
        .nest("/api", api)
        .layer(middleware::from_fn_with_state(dash.clone(), check_host))
        .layer(middleware::map_response(web::security_headers))
        .with_state(dash);

    tracing::info!("dashboard: http://localhost:{port}/ (link: shellctl dashboard)");
    web::serve_loopback("dashboard", port, app).await
}

// --- Access ---

fn host_ok(port: u16, host: &str) -> bool {
    [format!("localhost:{port}"), format!("127.0.0.1:{port}"), format!("[::1]:{port}")].iter().any(|h| h == host)
}

/// `Host`: only loopback names of our own port (DNS rebinding protection).
async fn check_host(State(d): St, req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    if !host_ok(d.sup.cfg.dashboard.port, host) {
        return (StatusCode::MISDIRECTED_REQUEST, "unknown host").into_response();
    }
    next.run(req).await
}

/// API: a session is required; `Origin`, if present, must be our own (required for mutating requests).
async fn require_session(State(d): St, req: Request, next: Next) -> Response {
    let headers = req.headers();
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let origin = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok());
    let origin_ok = match origin {
        Some(o) => o == format!("http://{host}"),
        None => req.method() == Method::GET,
    };
    if !origin_ok || !web::session_ok(&d.sup, &Scope::Dashboard, headers, COOKIE) {
        return web::json_error(StatusCode::FORBIDDEN, "no access: open the dashboard via the link from `shellctl dashboard`");
    }
    next.run(req).await
}

#[derive(Deserialize)]
struct AuthQuery {
    token: String,
}

async fn auth(State(d): St, Query(q): Query<AuthQuery>) -> Response {
    match d.sup.redeem_token(&Scope::Dashboard, &q.token) {
        Some(session) => {
            d.sup.audit.record("@dashboard", "session", "dashboard login");
            web::auth_success(COOKIE, &session)
        }
        None => (StatusCode::FORBIDDEN, "token is invalid or already used").into_response(),
    }
}

async fn page(State(d): St, headers: HeaderMap, req: Request) -> Response {
    if !web::session_ok(&d.sup, &Scope::Dashboard, &headers, COOKIE) {
        let body = "<!doctype html><meta charset=utf-8><title>wshell</title>\
                    <p>No session. Get a one-time link: <code>shellctl dashboard</code></p>";
        return (StatusCode::UNAUTHORIZED, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response();
    }
    let path = match req.uri().path() {
        "/" => "index.html",
        p => p.trim_start_matches('/'),
    };
    let Some(file) = Dist::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    // Vite asset names contain a content hash, so they can be cached forever.
    let cache = if path.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    let headers = [(header::CONTENT_TYPE, mime.essence_str().to_string()), (header::CACHE_CONTROL, cache.to_string())];
    (headers, file.data.into_owned()).into_response()
}

fn app_err(e: anyhow::Error) -> Response {
    web::json_error(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}"))
}

// --- API ---

async fn overview(State(d): St) -> Response {
    Json(json!({
        "shell": d.sup.shell_info(),
        "apps": d.sup.list(),
        "now": unix_now(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct ActionQuery {
    /// For `stop`: do not wait for a busy app to become free.
    #[serde(default)]
    force: bool,
}

async fn app_action(State(d): St, Path((id, action)): Path<(String, String)>, Query(q): Query<ActionQuery>) -> Response {
    let sup = &d.sup;
    if !sup.is_installed(&id) {
        return web::json_error(StatusCode::NOT_FOUND, format!("app {id} is not installed"));
    }
    let result = match action.as_str() {
        "start" => sup.start(&id).await,
        "stop" => sup.stop(&id, StopReason::User, q.force).await,
        "restart" => sup.restart(&id).await,
        "open" => {
            return match sup.issue_url(&id) {
                Ok(url) => Json(json!({ "url": url })).into_response(),
                Err(e) => app_err(e),
            };
        }
        _ => return web::json_error(StatusCode::NOT_FOUND, "unknown action"),
    };
    match result {
        Ok(()) => {
            sup.audit.record(&id, &format!("dashboard-{action}"), "-");
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => app_err(e),
    }
}

async fn uninstall(State(d): St, Path(id): Path<String>) -> Response {
    match d.sup.uninstall(&id).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => app_err(e),
    }
}

#[derive(Deserialize)]
struct StreamQuery {
    /// How many recent lines to send before new ones.
    #[serde(default)]
    tail: usize,
}

/// Log stream (SSE): the last `tail` lines, then new ones, with no gap in between.
async fn logs_stream(State(d): St, Path(id): Path<String>, Query(q): Query<StreamQuery>) -> Response {
    let Some(log) = d.sup.app_log(&id) else {
        return web::json_error(StatusCode::NOT_FOUND, format!("app {id} is not installed"));
    };
    let (tail, rx) = log.tail_and_subscribe(q.tail.min(1000));
    let tail = futures_util::stream::iter(tail.into_iter().map(|line| Ok::<_, Infallible>(Event::default().data(line))));
    let live = futures_util::stream::unfold(rx, |mut rx| async move {
        let data = match rx.recv().await {
            Ok(line) => line,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => format!("… lines skipped: {n}"),
            Err(_) => return None,
        };
        Some((Ok::<_, Infallible>(Event::default().data(data)), rx))
    });
    let stream = futures_util::StreamExt::chain(tail, live);
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(30))).into_response()
}

async fn upload(State(d): St, headers: HeaderMap, body: Bytes) -> Response {
    if !web::content_type_is(&headers, "application/octet-stream") {
        return web::json_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "expected application/octet-stream");
    }
    if body.is_empty() {
        return web::json_error(StatusCode::BAD_REQUEST, "empty package");
    }
    let upload = crate::util::token();
    let path = d.sup.paths.staging().join(format!("{upload}.pkg"));
    if let Err(e) = tokio::fs::write(&path, &body).await {
        return web::json_error(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    {
        let mut uploads = d.uploads.lock().unwrap();
        let now = Instant::now();
        uploads.retain(|_, (p, t)| {
            let fresh = now.duration_since(*t) < UPLOAD_TTL;
            if !fresh {
                let _ = std::fs::remove_file(p);
            }
            fresh
        });
        uploads.insert(upload.clone(), (path.clone(), now));
    }
    match d.sup.inspect(path).await {
        Ok(package) => Json(json!({ "upload": upload, "package": package })).into_response(),
        Err(e) => {
            drop_upload(&d, &upload);
            app_err(e)
        }
    }
}

#[derive(Deserialize)]
struct InstallBody {
    #[serde(default)]
    grant_optional: Vec<String>,
}

async fn install(State(d): St, Path(upload): Path<String>, Json(body): Json<InstallBody>) -> Response {
    let Some(path) = d.uploads.lock().unwrap().get(&upload).map(|(p, _)| p.clone()) else {
        return web::json_error(StatusCode::NOT_FOUND, "upload not found or expired");
    };
    let result = d.sup.install(path, body.grant_optional).await;
    drop_upload(&d, &upload);
    match result {
        Ok((id, version)) => Json(json!({ "id": id, "version": version })).into_response(),
        Err(e) => app_err(e),
    }
}

async fn cancel_upload(State(d): St, Path(upload): Path<String>) -> Response {
    drop_upload(&d, &upload);
    Json(json!({ "ok": true })).into_response()
}

fn drop_upload(d: &Dash, upload: &str) {
    if let Some((path, _)) = d.uploads.lock().unwrap().remove(upload) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn host_check() {
        assert!(super::host_ok(8471, "localhost:8471"));
        assert!(super::host_ok(8471, "127.0.0.1:8471"));
        assert!(!super::host_ok(8471, "localhost:8470"));
        assert!(!super::host_ok(8471, "evil.com:8471"));
        assert!(!super::host_ok(8471, "app.localhost:8471"));
    }
}
