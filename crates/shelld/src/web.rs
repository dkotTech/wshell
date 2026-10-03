//! Shared code for Shell's HTTP servers (app UI and dashboard).

use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::{Json, Router};
use axum::response::{IntoResponse, Response};

use crate::supervisor::{Scope, Supervisor};

const CSP: &str = "default-src 'self'; connect-src 'self'; frame-ancestors 'none'; form-action 'self'; base-uri 'none'; object-src 'none'";

/// Security headers on every response.
pub async fn security_headers(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    h.insert("cross-origin-opener-policy", HeaderValue::from_static("same-origin"));
    h.insert("cross-origin-resource-policy", HeaderValue::from_static("same-origin"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    resp
}

/// API error as `{"error": "..."}`.
pub fn json_error(status: StatusCode, message: impl std::fmt::Display) -> Response {
    (status, Json(serde_json::json!({ "error": message.to_string() }))).into_response()
}

/// The request's `Content-Type` starts with `mime` (parameters like `charset` are allowed).
pub fn content_type_is(headers: &HeaderMap, mime: &str) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(';').next().is_some_and(|t| t.trim().eq_ignore_ascii_case(mime)))
}

/// Whether the request has a `name` cookie with a valid session of scope `scope`.
pub fn session_ok(sup: &Supervisor, scope: &Scope, headers: &HeaderMap, name: &str) -> bool {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .any(|(k, v)| k == name && sup.session_valid(scope, v))
}

pub fn session_cookie(name: &str, session: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("{name}={session}; Path=/; HttpOnly; SameSite=Strict")).expect("hex session")
}

/// Response to a successful token exchange: sets the session cookie and navigates to `/`.
///
/// Not an HTTP redirect: if the login was started from another site (e.g. "Open UI" in the dashboard),
/// the whole redirect chain is considered cross-site and the browser will not send the
/// just-received `SameSite=Strict` cookie to `/`. Navigation via `<meta refresh>`
/// is initiated by the page of this origin itself, so the cookie is sent.
pub fn auth_success(cookie_name: &str, session: &str) -> Response {
    let body = "<!doctype html><meta charset=utf-8><meta http-equiv=refresh content=\"0;url=/\">\
                <title>wshell</title><p><a href=\"/\">Continue</a></p>";
    let mut resp = ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response();
    let h = resp.headers_mut();
    h.insert(header::SET_COOKIE, session_cookie(cookie_name, session));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// Listens on loopback only: 127.0.0.1 and, if available, ::1.
pub async fn serve_loopback(name: &str, port: u16, app: Router) -> Result<()> {
    let v4 = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .with_context(|| format!("{name}: bind 127.0.0.1:{port}"))?;
    // Browsers may resolve *.localhost to ::1.
    if let Ok(v6) = tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port))).await {
        let app = app.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            if let Err(e) = axum::serve(v6, app).await {
                tracing::error!("{name} [::1]: {e}");
            }
        });
    }
    axum::serve(v4, app).await?;
    Ok(())
}
