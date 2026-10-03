//! `render-rs tui`: a render in the terminal. Frames go out through the kitty graphics
//! protocol, input comes in through the kitty keyboard protocol (press / repeat / release)
//! and SGR mouse reporting in pixels. Supported by kitty, WezTerm and Ghostty.
//!
//! By default it creates a render of its own, sized to the terminal window in pixels
//! and resized with it, and opens wshell's launcher there: the page lays itself out for
//! a computer screen. The render is ephemeral, gone with its last viewer (also when the
//! SSH session drops). `--render N` attaches to an existing render (e.g. the device
//! screen) and shows it 1:1.
//!
//! It is a client of the render API (`[api]` of the config), like any viewer: run it
//! next to the Renderer, e.g. over SSH on the device.

use std::io::{self, Write};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use base64::Engine as _;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    MouseButton as TermButton, MouseEvent, MouseEventKind,
};
use crossterm::{cursor, execute, terminal};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

use crate::config::Config;
use crate::types::{Button, ButtonAction, KeyAction, RemoteEvent};

/// Browser wheel step, in pixels.
const WHEEL_STEP: f64 = 40.0;

pub fn run(cfg: &Config, attach: Option<u64>) -> Result<(), String> {
    if cfg.api.token.is_empty() {
        return Err("the render API token is needed: [api] token in the config, --token or RENDER_TOKEN".into());
    }
    let api = Api { listen: cfg.api.listen, token: cfg.api.token.clone() };
    let keys = cfg.tui.keys.clone();

    let term = Terminal::enter()?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(session(&api, attach, keys, term.enhanced_keys))
}

// ── Terminal state ───────────────────────────────────────────────────────────

/// Raw mode, alternate screen, keyboard and mouse protocols; restored on drop,
/// including on errors and panics.
struct Terminal {
    enhanced_keys: bool,
}

impl Terminal {
    fn enter() -> Result<Terminal, String> {
        terminal::enable_raw_mode().map_err(|e| format!("terminal: {e}"))?;
        if !kitty_graphics_supported() {
            let _ = terminal::disable_raw_mode();
            return Err("the terminal does not support the kitty graphics protocol (kitty, WezTerm, Ghostty do)".into());
        }
        let enhanced_keys = terminal::supports_keyboard_enhancement().unwrap_or(false);
        let mut out = io::stdout();
        let _ = execute!(out, terminal::EnterAlternateScreen, cursor::Hide, crossterm::event::EnableMouseCapture);
        // Mouse coordinates in pixels (SGR-Pixels), not cells.
        let _ = write!(out, "\x1b[?1016h");
        if enhanced_keys {
            let _ = execute!(
                out,
                crossterm::event::PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                        | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                )
            );
        }
        let _ = out.flush();
        restore_on_panic(enhanced_keys);
        Ok(Terminal { enhanced_keys })
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        restore(self.enhanced_keys);
    }
}

fn restore(enhanced_keys: bool) {
    let mut out = io::stdout();
    if enhanced_keys {
        let _ = execute!(out, crossterm::event::PopKeyboardEnhancementFlags);
    }
    // Delete our images, then undo the rest.
    let _ = write!(out, "\x1b_Ga=d,d=A,q=2\x1b\\\x1b[?1016l");
    let _ = execute!(out, crossterm::event::DisableMouseCapture, cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}

/// A panic message printed on the alternate screen vanishes with it: restore the
/// terminal first, so the reason stays visible. Then exit: a panic in crossterm's input
/// thread would otherwise leave the TUI running without input.
fn restore_on_panic(enhanced_keys: bool) {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore(enhanced_keys);
        default(info);
        std::process::exit(101);
    }));
}

/// Asks the terminal (a 1×1 query image), with a device-attributes request behind it:
/// every terminal answers the latter, so no answer to the former before it means "no".
fn kitty_graphics_supported() -> bool {
    let mut out = io::stdout();
    if write!(out, "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c").and_then(|_| out.flush()).is_err() {
        return false;
    }
    let stdin = io::stdin();
    let fd = stdin.as_fd();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut reply = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let timeout = rustix::event::Timespec { tv_sec: left.as_secs() as _, tv_nsec: left.subsec_nanos() as _ };
        let mut fds = [rustix::event::PollFd::new(&fd, rustix::event::PollFlags::IN)];
        if left.is_zero() || !matches!(rustix::event::poll(&mut fds, Some(&timeout)), Ok(n) if n > 0) {
            break;
        }
        let mut buf = [0u8; 256];
        match rustix::io::read(fd, &mut buf) {
            Ok(n) if n > 0 => reply.extend_from_slice(&buf[..n]),
            _ => break,
        }
        // The device attributes answer, `ESC [ ? … c`, comes last.
        if let Some(i) = reply.windows(3).position(|w| w == b"\x1b[?") {
            if reply[i..].contains(&b'c') {
                break;
            }
        }
    }
    String::from_utf8_lossy(&reply).contains("_Gi=31;OK")
}

// ── Session ──────────────────────────────────────────────────────────────────

/// The terminal window: cells for the status line, pixels for the render.
#[derive(Clone, Copy)]
struct Screen {
    rows: u16,
    columns: u16,
    /// Pixels above the status line (0×0 if the terminal does not report pixels).
    width: u32,
    height: u32,
}

fn screen() -> Screen {
    match terminal::window_size() {
        Ok(s) if s.rows > 1 => {
            let cell_h = s.height as u32 / s.rows as u32;
            Screen {
                rows: s.rows,
                columns: s.columns,
                width: s.width as u32,
                height: (s.height as u32).saturating_sub(cell_h),
            }
        }
        _ => Screen { rows: 24, columns: 80, width: 0, height: 0 },
    }
}

async fn session(
    api: &Api,
    attach: Option<u64>,
    keys: std::collections::BTreeMap<String, String>,
    enhanced_keys: bool,
) -> Result<(), String> {
    let mut scr = screen();
    let own = attach.is_none();
    let render = match attach {
        Some(id) => id,
        None => {
            if scr.width == 0 || scr.height == 0 {
                return Err("the terminal does not report its size in pixels; use --render N to attach".into());
            }
            let body = json!({
                "launcher": true, "width": scr.width, "height": scr.height, "ephemeral": true,
            });
            let created = api.request("POST", "/api/renders", Some(body)).await?;
            created["id"].as_u64().ok_or("render: no id in the reply")?
        }
    };

    let url = format!("ws://{}/api/renders/{render}/ws?token={}", api.listen, api.token);
    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| format!("render {render}: {e}"))?;
    let (mut ws_tx, mut ws_rx) = ws.split();
    let mut events = EventStream::new();
    let mut size = (scr.width, scr.height);
    let mut last: Option<Vec<u8>> = None;
    let mut out = io::stdout();
    status(&mut out, scr, render, own);

    loop {
        tokio::select! {
            msg = ws_rx.next() => match msg {
                // Metadata `{"w","h","url"}`, sent once before the frames.
                Some(Ok(Message::Text(text))) => {
                    if let Ok(meta) = serde_json::from_str::<Value>(&text) {
                        size = (meta["w"].as_u64().unwrap_or(0) as u32, meta["h"].as_u64().unwrap_or(0) as u32);
                    }
                }
                Some(Ok(Message::Binary(png))) => {
                    // The render re-sends its last frame at a steady rate: draw only changes.
                    if last.as_deref() == Some(&png[..]) {
                        continue;
                    }
                    draw(&mut out, &png);
                    last = Some(png.to_vec());
                }
                Some(Ok(Message::Close(_))) | None => return Err(format!("render {render}: connection closed")),
                Some(Err(e)) => return Err(format!("render {render}: {e}")),
                Some(Ok(_)) => {}
            },
            ev = events.next() => {
                let ev = match ev {
                    Some(Ok(ev)) => ev,
                    Some(Err(e)) => return Err(format!("terminal input: {e}")),
                    None => return Err("terminal input closed".into()),
                };
                let input = match ev {
                    Event::Key(k) if is_quit(&k) => return Ok(()),
                    Event::Key(k) => key_event(&k, &keys, enhanced_keys),
                    Event::Mouse(m) => mouse_event(&m, size),
                    Event::Resize(..) => {
                        scr = screen();
                        let _ = write!(out, "\x1b_Ga=d,d=A,q=2\x1b\\");
                        let _ = execute!(out, terminal::Clear(terminal::ClearType::All));
                        status(&mut out, scr, render, own);
                        if own && scr.width > 0 && scr.height > 0 {
                            size = (scr.width, scr.height);
                            let body = json!({ "width": scr.width, "height": scr.height });
                            api.request("POST", &format!("/api/renders/{render}/resize"), Some(body)).await?;
                            last = None;
                        } else if let Some(png) = &last {
                            draw(&mut out, png);
                        }
                        None
                    }
                    _ => None,
                };
                if let Some(input) = input {
                    let text = serde_json::to_string(&input).expect("serializable event");
                    ws_tx.send(Message::Text(text.into())).await.map_err(|e| format!("render {render}: {e}"))?;
                }
            }
        }
    }
}

/// The render API over plain HTTP/1.1 (loopback, one request per connection).
struct Api {
    listen: std::net::SocketAddr,
    token: String,
}

impl Api {
    async fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        let fail = |e: &dyn std::fmt::Display| format!("{method} {path}: {e}");
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.listen,
            self.token,
            body.len()
        );
        let mut conn = tokio::net::TcpStream::connect(self.listen).await.map_err(|e| fail(&e))?;
        conn.write_all(head.as_bytes()).await.map_err(|e| fail(&e))?;
        conn.write_all(body.as_bytes()).await.map_err(|e| fail(&e))?;
        let mut reply = Vec::new();
        conn.read_to_end(&mut reply).await.map_err(|e| fail(&e))?;
        let reply = String::from_utf8_lossy(&reply);
        let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
        let code: u16 = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        if !(200..300).contains(&code) {
            return Err(fail(&format!("HTTP {code} {}", body.trim())));
        }
        Ok(serde_json::from_str(body).unwrap_or(Value::Null))
    }
}

fn status(out: &mut impl Write, scr: Screen, render: u64, own: bool) {
    let what = if own { "own render" } else { "attached, 1:1" };
    let text = format!(" render {render} ({what}) · F12 apps · Ctrl+Q quit");
    let text: String = text.chars().take(scr.columns as usize).collect();
    let _ = write!(out, "\x1b[{};1H\x1b[2K\x1b[7m{text}\x1b[0m", scr.rows);
    let _ = out.flush();
}

/// One image with a fixed id and placement at the top left: the terminal replaces it in place.
/// The PNG goes as is, pixel for pixel.
fn draw(out: &mut impl Write, png: &[u8]) {
    let data = base64::engine::general_purpose::STANDARD.encode(png);
    let mut buf = String::with_capacity(data.len() + data.len() / 4096 * 16 + 64);
    buf.push_str("\x1b[H");
    let chunks: Vec<&str> = data
        .as_bytes()
        .chunks(4096)
        .map(|c| std::str::from_utf8(c).expect("base64 is ASCII"))
        .collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            buf.push_str(&format!("\x1b_Ga=T,f=100,i=1,p=1,q=2,C=1,m={more};{chunk}\x1b\\"));
        } else {
            buf.push_str(&format!("\x1b_Gm={more};{chunk}\x1b\\"));
        }
    }
    let _ = out.write_all(buf.as_bytes());
    let _ = out.flush();
}

// ── Input ────────────────────────────────────────────────────────────────────

fn is_quit(k: &KeyEvent) -> bool {
    k.kind == KeyEventKind::Press
        && k.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(k.code, KeyCode::Char('q' | 'c'))
}

/// Terminal key → `KeyboardEvent.key` name, then `[tui.keys]` (e.g. F12 → AppSwitch).
fn key_event(k: &KeyEvent, keys: &std::collections::BTreeMap<String, String>, enhanced: bool) -> Option<RemoteEvent> {
    // Shortcuts with Ctrl/Alt are the terminal's business; the render has no modifiers.
    if k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) {
        return None;
    }
    let name = match k.code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Esc => "Escape".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Tab | KeyCode::BackTab => "Tab".into(),
        KeyCode::Up => "ArrowUp".into(),
        KeyCode::Down => "ArrowDown".into(),
        KeyCode::Left => "ArrowLeft".into(),
        KeyCode::Right => "ArrowRight".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PageUp".into(),
        KeyCode::PageDown => "PageDown".into(),
        KeyCode::Delete => "Delete".into(),
        KeyCode::Insert => "Insert".into(),
        KeyCode::F(n) => format!("F{n}"),
        _ => return None,
    };
    let key = keys.get(&name).cloned().unwrap_or(name);
    // Without the keyboard protocol there are no release events: send whole presses.
    let state = match k.kind {
        KeyEventKind::Press if !enhanced => KeyAction::Press,
        KeyEventKind::Press | KeyEventKind::Repeat => KeyAction::Down,
        KeyEventKind::Release => KeyAction::Up,
    };
    Some(RemoteEvent::Key { key, state })
}

/// SGR-Pixels coordinates are render pixels (the image is shown 1:1); outside it, dropped.
fn mouse_event(m: &MouseEvent, size: (u32, u32)) -> Option<RemoteEvent> {
    let (x, y) = (m.column as u32, m.row as u32);
    if x >= size.0 || y >= size.1 {
        return None;
    }
    let (x, y) = (x as f32, y as f32);
    let button = |b: TermButton| match b {
        TermButton::Left => Button::Left,
        TermButton::Right => Button::Right,
        TermButton::Middle => Button::Middle,
    };
    Some(match m.kind {
        MouseEventKind::Down(b) => RemoteEvent::MouseButton { button: button(b), state: ButtonAction::Down, x, y },
        MouseEventKind::Up(b) => RemoteEvent::MouseButton { button: button(b), state: ButtonAction::Up, x, y },
        MouseEventKind::Moved | MouseEventKind::Drag(_) => RemoteEvent::MouseMove { x, y },
        MouseEventKind::ScrollDown => RemoteEvent::Wheel { x, y, dx: 0.0, dy: WHEEL_STEP },
        MouseEventKind::ScrollUp => RemoteEvent::Wheel { x, y, dx: 0.0, dy: -WHEEL_STEP },
        MouseEventKind::ScrollRight => RemoteEvent::Wheel { x, y, dx: WHEEL_STEP, dy: 0.0 },
        MouseEventKind::ScrollLeft => RemoteEvent::Wheel { x, y, dx: -WHEEL_STEP, dy: 0.0 },
    })
}
