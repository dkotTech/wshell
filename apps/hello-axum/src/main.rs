//! Example program (kind = "command") on a web framework: axum on single-threaded tokio.
//! Shell only opens the port from `net.listen`; routes, JSON and state are ordinary.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{Query, State};
use axum::http::Uri;
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

const PORT: u16 = 8481;

#[derive(Default)]
struct Stats {
    requests: AtomicU64,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let stats = Arc::new(Stats::default());
    let app = Router::new()
        .route("/", get(index))
        .route("/api/hello", get(hello))
        .route("/api/echo", post(echo))
        .route("/api/stats", get(stats_handler))
        .fallback(not_found)
        .with_state(stats);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", PORT)).await.expect("bind");
    println!("hello-axum: listening on port {PORT}");
    axum::serve(listener, app).await.expect("serve");
}

async fn index(State(stats): State<Arc<Stats>>) -> Html<&'static str> {
    stats.requests.fetch_add(1, Ordering::Relaxed);
    Html(
        "<!doctype html><meta charset=utf-8><title>hello-axum</title>\
         <h1>axum in wshell</h1>\
         <ul><li><a href=/api/hello?name=wshell>GET /api/hello?name=…</a></li>\
         <li>POST /api/echo (JSON)</li><li><a href=/api/stats>GET /api/stats</a></li></ul>",
    )
}

#[derive(Deserialize)]
struct HelloQuery {
    name: Option<String>,
}

async fn hello(State(stats): State<Arc<Stats>>, Query(q): Query<HelloQuery>) -> Json<Value> {
    stats.requests.fetch_add(1, Ordering::Relaxed);
    let name = q.name.unwrap_or_else(|| "world".into());
    println!("GET /api/hello name={name}");
    Json(json!({ "hello": name, "from": "axum" }))
}

async fn echo(State(stats): State<Arc<Stats>>, Json(body): Json<Value>) -> Json<Value> {
    stats.requests.fetch_add(1, Ordering::Relaxed);
    println!("POST /api/echo {body}");
    Json(json!({ "echo": body }))
}

async fn stats_handler(State(stats): State<Arc<Stats>>) -> Json<Value> {
    let n = stats.requests.fetch_add(1, Ordering::Relaxed) + 1;
    Json(json!({ "requests": n }))
}

async fn not_found(uri: Uri) -> (axum::http::StatusCode, Json<Value>) {
    (axum::http::StatusCode::NOT_FOUND, Json(json!({ "error": "not found", "path": uri.path() })))
}
