use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use std::net::SocketAddr;

use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, Request, State, WebSocketUpgrade,
    },
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::config::{Action, Keys};
use crate::types::{
    EngineCmd, Frame, KeyAction, LastFrame, Registry, RemoteEvent, RenderEntry, RenderSpec,
};

/// Cookie set after `?token=`, so the viewer pages work in a browser.
const COOKIE: &str = "render_token";

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn alloc_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone)]
pub struct ApiState {
    pub registry: Registry,
    // std mpsc Sender is Send but not Sync — the mutex makes it shareable.
    pub cmd_tx: Arc<Mutex<std::sync::mpsc::Sender<EngineCmd>>>,
    pub engine: &'static str,
    pub token: Arc<str>,
    pub keys: Arc<Keys>,
    /// The home screen: wshell's launcher plugin.
    pub launcher: Arc<crate::shell::Launcher>,
    /// Default width, height and fps of a new render (`[screen]`).
    pub screen: (u32, u32, u32),
}

impl ApiState {
    fn send(&self, cmd: EngineCmd) {
        let _ = self.cmd_tx.lock().unwrap().send(cmd);
    }

    /// Input from any source. Keys are renamed first (`keys.map`); the Renderer's own
    /// keys (`keys.actions`) never reach the page.
    pub fn input(&self, id: u64, mut event: RemoteEvent) {
        if let RemoteEvent::Key { key, state } = &mut event {
            if let Some(to) = self.keys.map.get(key.as_str()) {
                *key = to.clone();
            }
            if let Some(action) = self.keys.actions.get(key.as_str()) {
                // Once per press: on down (or a whole press), not again on up.
                if !matches!(state, KeyAction::Up) {
                    self.action(id, *action);
                }
                return;
            }
        }
        self.send(EngineCmd::Input { id, event });
    }

    fn action(&self, id: u64, action: Action) {
        match action {
            Action::Launcher => {
                let state = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = state.open_launcher(id).await {
                        eprintln!("render {id}: launcher: {e}");
                    }
                });
            }
        }
    }

    /// A fresh one-time link every time: the launcher session may be gone (shelld restarted).
    pub async fn open_launcher(&self, id: u64) -> Result<(), String> {
        let url = self.launcher.url().await?;
        if self.navigate(id, url) {
            Ok(())
        } else {
            Err("no such render".into())
        }
    }

    fn destroy(&self, id: u64) -> bool {
        let Some(entry) = self.registry.lock().unwrap().remove(&id) else { return false };
        entry.ticker.abort();
        self.send(EngineCmd::Destroy { id });
        true
    }

    /// A WebSocket viewer came (`+1`) or left (`-1`); an ephemeral render goes with its last viewer.
    fn viewer(&self, id: u64, joined: bool) {
        let mut reg = self.registry.lock().unwrap();
        let Some(entry) = reg.get_mut(&id) else { return };
        entry.viewers = if joined { entry.viewers + 1 } else { entry.viewers.saturating_sub(1) };
        let gone = entry.ephemeral && entry.viewers == 0;
        drop(reg);
        if gone && self.destroy(id) {
            eprintln!("render {id}: last viewer left, destroyed");
        }
    }

    fn navigate(&self, id: u64, url: String) -> bool {
        let mut reg = self.registry.lock().unwrap();
        let Some(entry) = reg.get_mut(&id) else { return false };
        entry.url = url.clone();
        drop(reg);
        self.send(EngineCmd::Navigate { id, url });
        true
    }
}

/// Constant-time comparison: the token must not leak through response timing.
fn token_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find_map(|(k, v)| (k == COOKIE).then_some(v))
}

/// Every request needs the token: `Authorization: Bearer`, the cookie, or `?token=`
/// (which also sets the cookie, for opening the viewer in a browser).
async fn require_token(State(state): State<ApiState>, req: Request, next: Next) -> Response {
    let ok = |t: &str| token_eq(t, &state.token);
    let headers = req.headers();
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if bearer.is_some_and(ok) || cookie_token(headers).is_some_and(ok) {
        return next.run(req).await;
    }
    let query = req.uri().query().unwrap_or_default().split('&').find_map(|p| p.strip_prefix("token="));
    if query.is_some_and(ok) {
        let mut resp = next.run(req).await;
        let cookie = format!("{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict", state.token);
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
        return resp;
    }
    (StatusCode::UNAUTHORIZED, "token required: ?token=, cookie or Authorization: Bearer\n").into_response()
}

pub async fn serve(listen: SocketAddr, state: ApiState) {
    let token = state.token.clone();
    let app = Router::new()
        .route("/", get(index_page))
        .route("/view/{id}", get(viewer_page))
        .route("/api/renders", get(list_renders).post(create_render))
        .route("/api/renders/{id}", axum::routing::delete(delete_render))
        .route("/api/renders/{id}/input", post(input_render))
        .route("/api/renders/{id}/navigate", post(navigate_render))
        .route("/api/renders/{id}/launcher", post(launcher_render))
        .route("/api/renders/{id}/resize", post(resize_render))
        .route("/api/renders/{id}/ws", get(ws_render))
        .route("/api/renders/{id}/frame", get(frame_render))
        .route("/api/renders/{id}/stream", get(stream_render))
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .unwrap_or_else(|e| panic!("bind {listen}: {e}"));
    eprintln!("control api → http://{listen}/?token={token}");
    axum::serve(listener, app).await.unwrap();
}

// ── Render management ────────────────────────────────────────────────────────

fn entry_json(e: &RenderEntry, engine: &str) -> Value {
    json!({
        "id": e.id,
        "url": e.url,
        "width": e.width,
        "height": e.height,
        "fps": e.fps,
        "engine": engine,
        "clients": e.frame_tx.receiver_count(),
        "ws": format!("/api/renders/{}/ws", e.id),
        "viewer": format!("/view/{}", e.id),
    })
}

async fn list_renders(State(state): State<ApiState>) -> Json<Value> {
    let reg = state.registry.lock().unwrap();
    let mut list: Vec<Value> = reg.values().map(|e| entry_json(e, state.engine)).collect();
    list.sort_by_key(|v| v["id"].as_u64());
    Json(json!(list))
}

#[derive(Deserialize)]
pub struct CreateRender {
    /// Endpoint the render loads its HTML from, e.g. http://localhost:5173/
    #[serde(default)]
    pub url: String,
    /// Open wshell's launcher instead of `url`.
    #[serde(default)]
    pub launcher: bool,
    /// Defaults: `[screen]` of the config.
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
    /// Destroy the render when its last WebSocket viewer leaves.
    #[serde(default)]
    pub ephemeral: bool,
}

/// Largest render side: bounds the memory of a frame.
const MAX_SIDE: u32 = 8192;

/// Registers a render in the registry and tells the engine to start it.
/// Shared by the HTTP handler and `--url` CLI startup.
pub fn spawn_render(state: &ApiState, req: CreateRender) -> u64 {
    let id = alloc_id();
    let (frame_tx, _) = broadcast::channel::<Frame>(4);
    let last_frame: LastFrame = Arc::new(Mutex::new(None));
    let (width, height, fps) = state.screen;
    let (width, height) = (req.width.unwrap_or(width).clamp(1, MAX_SIDE), req.height.unwrap_or(height).clamp(1, MAX_SIDE));
    let fps = req.fps.unwrap_or(fps).clamp(1, 60);

    // Steady-fps output: broadcast the latest encoded frame every 1/fps, whether
    // or not the page repainted. This keeps the video flowing at a constant rate
    // even for a completely static page.
    let ticker = {
        let frame_tx = frame_tx.clone();
        let last_frame = last_frame.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_secs_f64(1.0 / fps as f64));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                // No subscribers yet? Nothing to do — avoids buffering.
                if frame_tx.receiver_count() == 0 {
                    continue;
                }
                let frame = last_frame.lock().unwrap().clone();
                if let Some(frame) = frame {
                    let _ = frame_tx.send(frame);
                }
            }
        })
        .abort_handle()
    };

    state.registry.lock().unwrap().insert(
        id,
        RenderEntry {
            id,
            url: req.url.clone(),
            width,
            height,
            fps,
            frame_tx,
            last_frame: last_frame.clone(),
            ticker,
            ephemeral: req.ephemeral,
            viewers: 0,
        },
    );

    state.send(EngineCmd::Create(RenderSpec {
        id,
        url: req.url,
        width,
        height,
        fps,
        last_frame,
    }));
    id
}

async fn create_render(
    State(state): State<ApiState>,
    Json(mut req): Json<CreateRender>,
) -> Response {
    if req.launcher {
        match state.launcher.url().await {
            Ok(url) => req.url = url,
            Err(e) => return (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
        }
    }
    if req.url.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "empty url"}))).into_response();
    }
    let id = spawn_render(&state, req);
    let reg = state.registry.lock().unwrap();
    Json(entry_json(&reg[&id], state.engine)).into_response()
}

async fn delete_render(State(state): State<ApiState>, Path(id): Path<u64>) -> Response {
    if state.destroy(id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, Json(json!({"error": "no such render"}))).into_response()
    }
}

#[derive(Deserialize)]
struct ResizeBody {
    width: u32,
    height: u32,
}

/// New viewport size: the page lays itself out again (a terminal window was resized).
async fn resize_render(State(state): State<ApiState>, Path(id): Path<u64>, Json(body): Json<ResizeBody>) -> Response {
    let (width, height) = (body.width.clamp(1, MAX_SIDE), body.height.clamp(1, MAX_SIDE));
    let mut reg = state.registry.lock().unwrap();
    let Some(entry) = reg.get_mut(&id) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "no such render"}))).into_response();
    };
    (entry.width, entry.height) = (width, height);
    drop(reg);
    state.send(EngineCmd::Resize { id, width, height });
    StatusCode::OK.into_response()
}

async fn input_render(
    State(state): State<ApiState>,
    Path(id): Path<u64>,
    Json(event): Json<RemoteEvent>,
) -> Response {
    if !state.registry.lock().unwrap().contains_key(&id) {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "no such render"}))).into_response();
    }
    state.input(id, event);
    StatusCode::OK.into_response()
}

#[derive(Deserialize)]
struct NavigateBody {
    url: String,
}

async fn navigate_render(
    State(state): State<ApiState>,
    Path(id): Path<u64>,
    Json(body): Json<NavigateBody>,
) -> Response {
    if state.navigate(id, body.url) {
        StatusCode::OK.into_response()
    } else {
        (StatusCode::NOT_FOUND, Json(json!({"error": "no such render"}))).into_response()
    }
}

/// Back to wshell's launcher (what the `AppSwitch` key does).
async fn launcher_render(State(state): State<ApiState>, Path(id): Path<u64>) -> Response {
    match state.open_launcher(id).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

// ── Video: WebSocket (PNG frames) + multipart fallback ──────────────────────

/// Per-subscription options, e.g. for a small USB display:
/// `/api/renders/1/ws?format=rgb565&w=256&h=144&fps=10`.
/// Without params the socket streams PNG at the render's fps (browser viewer).
#[derive(Deserialize)]
struct WsParams {
    /// png (default) | rgb565 | rgb888
    format: Option<String>,
    w: Option<u32>,
    h: Option<u32>,
    fps: Option<u32>,
}

#[derive(Clone, Copy, PartialEq)]
enum RawFormat {
    Rgb565,
    Rgb888,
}

impl RawFormat {
    fn code(self) -> u8 {
        match self {
            RawFormat::Rgb565 => 0,
            RawFormat::Rgb888 => 1,
        }
    }
    fn name(self) -> &'static str {
        match self {
            RawFormat::Rgb565 => "rgb565",
            RawFormat::Rgb888 => "rgb888",
        }
    }
}

async fn ws_render(
    ws: WebSocketUpgrade,
    State(state): State<ApiState>,
    Path(id): Path<u64>,
    axum::extract::Query(params): axum::extract::Query<WsParams>,
) -> Response {
    let raw_format = match params.format.as_deref() {
        None | Some("png") => None,
        Some("rgb565") => Some(RawFormat::Rgb565),
        Some("rgb888") => Some(RawFormat::Rgb888),
        Some(other) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("unknown format {other:?}") })),
            )
                .into_response()
        }
    };

    let (rx, last_frame, first, entry_info) = {
        let reg = state.registry.lock().unwrap();
        let Some(entry) = reg.get(&id) else {
            return (StatusCode::NOT_FOUND, "no such render").into_response();
        };
        let out = (
            entry.frame_tx.subscribe(),
            entry.last_frame.clone(),
            entry.last_frame.lock().unwrap().clone(),
            (entry.width, entry.height, entry.fps, entry.url.clone()),
        );
        out
    };
    let (rw, rh, rfps, url) = entry_info;

    match raw_format {
        // Raw pixel session: server-side decode/scale/convert at its own pace.
        Some(format) => {
            let out_w = params.w.unwrap_or(rw);
            let out_h = params.h.unwrap_or(rh);
            let fps = params.fps.unwrap_or(rfps).clamp(1, 60);
            let meta = json!({
                "w": out_w, "h": out_h, "fps": fps, "format": format.name(), "url": url,
            })
            .to_string();
            ws.on_upgrade(move |socket| async move {
                state.viewer(id, true);
                ws_raw_session(socket, state.clone(), id, last_frame, meta, format, out_w, out_h, fps).await;
                state.viewer(id, false);
            })
        }
        // PNG session: existing broadcast path (custom fps not supported here).
        None => {
            let meta = json!({ "w": rw, "h": rh, "url": url }).to_string();
            ws.on_upgrade(move |socket| async move {
                state.viewer(id, true);
                ws_session(socket, state.clone(), id, rx, first, meta).await;
                state.viewer(id, false);
            })
        }
    }
}

/// Frames flow out; JSON input events flow in over the same socket.
async fn ws_session(
    socket: WebSocket,
    state: ApiState,
    id: u64,
    mut rx: broadcast::Receiver<Frame>,
    first: Option<Frame>,
    meta: String,
) {
    use futures_util::{SinkExt, StreamExt};
    let (mut tx, mut rx_ws) = socket.split();

    if tx.send(Message::Text(meta.into())).await.is_err() {
        return;
    }
    if let Some(frame) = first {
        let _ = tx
            .send(Message::Binary(frame.as_ref().clone().into()))
            .await;
    }

    // inbound: text messages are RemoteEvent JSON
    let input_state = state.clone();
    let inbound = tokio::spawn(async move {
        while let Some(Ok(msg)) = rx_ws.next().await {
            if let Message::Text(text) = msg {
                match serde_json::from_str::<RemoteEvent>(&text) {
                    Ok(event) => input_state.input(id, event),
                    Err(e) => eprintln!("render {id}: bad ws input: {e}"),
                }
            }
        }
    });

    // outbound: broadcast frames
    loop {
        match rx.recv().await {
            Ok(frame) => {
                if tx
                    .send(Message::Binary(frame.as_ref().clone().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    inbound.abort();
}

/// Decodes the cached PNG, optionally rescales (Nearest — it's a pixel UI) and
/// converts to the requested raw pixel format. Returns (w, h, pixels).
fn convert_frame(png: &[u8], format: RawFormat, out_w: u32, out_h: u32) -> Option<(u16, u16, Vec<u8>)> {
    let img = image::load_from_memory(png).ok()?.to_rgba8();
    let img = if img.width() != out_w || img.height() != out_h {
        image::imageops::resize(&img, out_w, out_h, image::imageops::FilterType::Nearest)
    } else {
        img
    };
    let (w, h) = (img.width() as u16, img.height() as u16);
    let data = match format {
        RawFormat::Rgb565 => {
            let mut v = Vec::with_capacity(img.pixels().len() * 2);
            for p in img.pixels() {
                let [r, g, b, _] = p.0;
                let c = ((r as u16 >> 3) << 11) | ((g as u16 >> 2) << 5) | (b as u16 >> 3);
                v.extend_from_slice(&c.to_le_bytes());
            }
            v
        }
        RawFormat::Rgb888 => {
            let mut v = Vec::with_capacity(img.pixels().len() * 3);
            for p in img.pixels() {
                v.extend_from_slice(&p.0[..3]);
            }
            v
        }
    };
    Some((w, h, data))
}

/// Builds one binary WS frame: 12-byte header + raw pixels.
/// Header: "FR" | format u8 | flags u8 (0) | w u16 le | h u16 le | seq u32 le.
fn raw_ws_frame(format: RawFormat, w: u16, h: u16, seq: u32, pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + pixels.len());
    out.extend_from_slice(b"FR");
    out.push(format.code());
    out.push(0);
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(pixels);
    out
}

/// Raw-format session: paced by its own per-subscriber ticker, converting the
/// latest frame on change. Same inbound input-event protocol as ws_session.
#[allow(clippy::too_many_arguments)]
async fn ws_raw_session(
    socket: WebSocket,
    state: ApiState,
    id: u64,
    last_frame: crate::types::LastFrame,
    meta: String,
    format: RawFormat,
    out_w: u32,
    out_h: u32,
    fps: u32,
) {
    use futures_util::{SinkExt, StreamExt};
    let (mut tx, mut rx_ws) = socket.split();

    if tx.send(Message::Text(meta.into())).await.is_err() {
        return;
    }

    let input_state = state.clone();
    let inbound = tokio::spawn(async move {
        while let Some(Ok(msg)) = rx_ws.next().await {
            if let Message::Text(text) = msg {
                match serde_json::from_str::<RemoteEvent>(&text) {
                    Ok(event) => input_state.input(id, event),
                    Err(e) => eprintln!("render {id}: bad ws input: {e}"),
                }
            }
        }
    });

    let mut interval = tokio::time::interval(Duration::from_secs_f64(1.0 / fps as f64));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut converted_from: Option<Frame> = None; // PNG the cache was built from
    let mut cached: Option<(u16, u16, Vec<u8>)> = None;
    let mut seq: u32 = 0;

    loop {
        interval.tick().await;
        let Some(png) = last_frame.lock().unwrap().clone() else {
            continue; // nothing rendered yet
        };

        let stale = converted_from
            .as_ref()
            .is_none_or(|prev| !Arc::ptr_eq(prev, &png));
        if stale {
            let src = png.clone();
            let result = tokio::task::spawn_blocking(move || {
                convert_frame(&src, format, out_w, out_h)
            })
            .await
            .ok()
            .flatten();
            match result {
                Some(frame) => {
                    cached = Some(frame);
                    converted_from = Some(png);
                }
                None => continue,
            }
        }

        let Some((w, h, ref pixels)) = cached else { continue };
        seq = seq.wrapping_add(1);
        let msg = raw_ws_frame(format, w, h, seq, pixels);
        if tx.send(Message::Binary(msg.into())).await.is_err() {
            break;
        }
    }
    inbound.abort();
}

/// Last rendered frame as a single PNG — a poor man's screenshot API.
async fn frame_render(State(state): State<ApiState>, Path(id): Path<u64>) -> Response {
    use axum::http::header;
    let frame = {
        let reg = state.registry.lock().unwrap();
        let Some(entry) = reg.get(&id) else {
            return (StatusCode::NOT_FOUND, "no such render").into_response();
        };
        let out = entry.last_frame.lock().unwrap().clone();
        out
    };
    match frame {
        Some(png) => (
            [(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "no-cache")],
            png.as_ref().clone(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no frame rendered yet").into_response(),
    }
}

/// Multipart PNG stream — fallback for clients without WebSocket.
async fn stream_render(State(state): State<ApiState>, Path(id): Path<u64>) -> Response {
    use axum::body::Body;
    use axum::http::header;

    let mut rx = {
        let reg = state.registry.lock().unwrap();
        let Some(entry) = reg.get(&id) else {
            return (StatusCode::NOT_FOUND, "no such render").into_response();
        };
        entry.frame_tx.subscribe()
    };

    let boundary = "render_frame";
    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    let hdr = format!(
                        "--{boundary}\r\nContent-Type: image/png\r\nContent-Length: {}\r\n\r\n",
                        frame.len()
                    );
                    yield Ok::<_, std::convert::Infallible>(bytes::Bytes::from(hdr));
                    yield Ok(bytes::Bytes::from(frame.as_ref().clone()));
                    yield Ok(bytes::Bytes::from_static(b"\r\n"));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Response::builder()
        .header(
            header::CONTENT_TYPE,
            format!("multipart/x-mixed-replace;boundary={boundary}"),
        )
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .unwrap()
}

// ── Built-in viewer pages ────────────────────────────────────────────────────

async fn index_page() -> Html<&'static str> {
    Html(include_str!("viewer/index.html"))
}

async fn viewer_page(Path(_id): Path<u64>) -> Html<&'static str> {
    Html(include_str!("viewer/view.html"))
}
