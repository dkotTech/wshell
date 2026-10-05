//! render-rs — headless HTML renderer for wshell (a separate process).
//!
//! Renders web pages without a display and streams them as video (PNG frames)
//! over WebSocket. The engine is chosen at build time: WPE WebKit (default,
//! C FFI) or Servo (pure Rust) — see Cargo features.
//!
//! - By default it opens wshell's launcher plugin (one-time links to its UI from an
//!   extra control socket of shelld, `render.sock`); apps are opened from there, and
//!   the `AppSwitch` key brings the launcher back. `--url` renders arbitrary endpoints instead.
//! - Every render can be remote-controlled through the API: keyboard, mouse and
//!   wheel events; keys are remapped per `[keys]` of the config.
//! - The API listens on loopback and requires a token (`?token=`, cookie or
//!   `Authorization: Bearer`): it acts in apps on behalf of the user.
//! - `GET /` serves a management UI, `GET /view/{id}` an interactive viewer.

mod api;
mod config;
mod encode;
mod engine;
mod shell;
mod tui;
mod types;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Parser, Subcommand};

use api::{ApiState, CreateRender};
use config::Config;

#[derive(Parser)]
#[command(name = "render-rs", about = "Headless HTML renderer: WebSocket video + remote control")]
struct Args {
    /// Config file (see render.example.toml); flags below override it
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Control API address (default 127.0.0.1:8090)
    #[arg(long)]
    listen: Option<SocketAddr>,

    /// API access token (default: random per start, printed to stderr)
    #[arg(long, env = "RENDER_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// Endpoint(s) to render at startup instead of wshell's launcher
    /// (repeat for multiple renders)
    #[arg(long)]
    url: Vec<String>,

    /// Default viewport width
    #[arg(long)]
    width: Option<u32>,

    /// Default viewport height
    #[arg(long)]
    height: Option<u32>,

    /// Frame rate cap per render
    #[arg(long)]
    fps: Option<u32>,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show a render in this terminal and control it (kitty graphics + keyboard
    /// protocols: kitty, WezTerm, Ghostty). Connects to the API of `--listen` / `[api]`.
    Tui {
        /// Attach to an existing render (e.g. 1, the device screen) and show it 1:1,
        /// instead of a terminal-sized render of its own
        #[arg(long)]
        render: Option<u64>,
    },
}

/// Random 128-bit token, hex.
fn random_token() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("reading /dev/urandom");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn main() {
    let args = Args::parse();
    let mut cfg = Config::load(args.config.as_deref()).unwrap_or_else(|e| {
        eprintln!("config: {e}");
        std::process::exit(2);
    });
    if let Some(listen) = args.listen {
        cfg.api.listen = listen;
    }
    if let Some(token) = args.token {
        cfg.api.token = token;
    }
    if let Some(Cmd::Tui { render }) = args.command {
        if let Err(e) = tui::run(&cfg, render) {
            eprintln!("tui: {e}");
            std::process::exit(1);
        }
        return;
    }
    if cfg.api.token.is_empty() {
        cfg.api.token = random_token();
    }
    let screen = &mut cfg.screen;
    screen.width = args.width.unwrap_or(screen.width);
    screen.height = args.height.unwrap_or(screen.height);
    screen.fps = args.fps.unwrap_or(screen.fps);

    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let state = ApiState {
        registry: Arc::new(Mutex::new(HashMap::new())),
        cmd_tx: Arc::new(Mutex::new(cmd_tx)),
        engine: engine::NAME,
        token: cfg.api.token.as_str().into(),
        keys: Arc::new(std::mem::take(&mut cfg.keys)),
        launcher: Arc::new(cfg.shell.launcher()),
        viewer_page: api::viewer_page_html(&cfg.web.keys).into(),
        screen: (cfg.screen.width, cfg.screen.height, cfg.screen.fps),
    };

    // API server on a background thread. Startup renders are created *inside* the
    // runtime (spawn_render starts a tokio ticker task), before serving begins.
    {
        let state = state.clone();
        let listen = cfg.api.listen;
        let urls = args.url;
        let launcher = urls.is_empty() && state.launcher.id.is_some();
        std::thread::spawn(move || {
            tokio::runtime::Runtime::new().unwrap().block_on(async move {
                let new = |url| CreateRender { url, launcher: false, width: None, height: None, fps: None, ephemeral: false };
                for url in urls {
                    api::spawn_render(&state, new(url));
                }
                if launcher {
                    let state = state.clone();
                    tokio::spawn(async move {
                        // shelld may start after us: keep asking until it answers.
                        let url = loop {
                            match state.launcher.url().await {
                                Ok(url) => break url,
                                Err(e) => eprintln!("launcher: {e}; retrying"),
                            }
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        };
                        api::spawn_render(&state, new(url));
                    });
                }
                api::serve(listen, state).await;
            });
        });
    }

    eprintln!("engine: {}", engine::NAME);
    // …the engine owns the main thread (GLib loop / servo event loop).
    engine::run(cmd_rx);
}
