//! Servo engine: pure-Rust headless rendering via SoftwareRenderingContext.
//! One Servo instance on the calling (main) thread hosts every webview;
//! commands from the API are drained between event-loop spins.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, SyncSender};
use std::time::Duration;

use dpi::PhysicalSize;
use servo::{
    DeviceIntPoint, DeviceIntRect, DevicePoint, InputEvent, Key, KeyState, KeyboardEvent,
    LoadStatus, MouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent, Preferences,
    RenderingContext, Servo, ServoBuilder, SoftwareRenderingContext, UserContentManager,
    UserScript, WebView, WebViewBuilder, WebViewDelegate, WebViewPoint, WheelDelta, WheelEvent,
    WheelMode,
};
use url::Url;

use crate::encode::{spawn_encoder, PixelBuf, PixelFormat};
use crate::types::{Button, ButtonAction, EngineCmd, KeyAction, RemoteEvent, RenderSpec};

pub const NAME: &str = "servo";

/// Servo leaves `document.visibilityState` at "hidden" after a normal load (it only
/// turns "visible" on a bfcache reactivation), so pages that pause work while hidden
/// never wake up; wshell's shell.js, for one, reports the UI as hidden and Shell stops
/// sending it events. A render is on screen by definition: report the page as visible.
/// User scripts run as a delayed task, possibly after the page's own scripts, so the
/// change is announced with a `visibilitychange` event.
const VISIBLE_SCRIPT: &str = r#"
Object.defineProperty(Document.prototype, "visibilityState", { configurable: true, get: () => "visible" });
Object.defineProperty(Document.prototype, "hidden", { configurable: true, get: () => false });
document.dispatchEvent(new Event("visibilitychange"));
"#;

// ── Frame delegate ────────────────────────────────────────────────────────────

struct StreamDelegate {
    rendering_context: Rc<dyn RenderingContext>,
    pixel_tx: SyncSender<PixelBuf>,
    size: Cell<PhysicalSize<u32>>,
    loaded: Cell<bool>,
}

impl WebViewDelegate for StreamDelegate {
    fn notify_load_status_changed(&self, webview: WebView, status: LoadStatus) {
        if matches!(status, LoadStatus::Complete) && !self.loaded.replace(true) {
            eprintln!("loaded: {}", webview.url().map(|u| u.to_string()).unwrap_or_default());
        }
    }

    fn notify_new_frame_ready(&self, webview: WebView) {
        // Multiple webviews → multiple software contexts; make ours current
        // before painting into it.
        let _ = self.rendering_context.make_current();
        webview.paint();

        // Send every painted frame — no rate throttling. Dropping frames here
        // risks discarding the final, fully-painted frame of a static page and
        // leaving the viewer stuck on a partial paint. Backpressure is handled
        // by the encoder's sync_channel(1), which always keeps the latest frame.
        let size = self.size.get();
        let rect = DeviceIntRect::new(
            DeviceIntPoint::new(0, 0),
            DeviceIntPoint::new(size.width as i32, size.height as i32),
        );
        let Some(rgba) = self.rendering_context.read_to_image(rect) else {
            return;
        };
        let (w, h) = (rgba.width(), rgba.height());
        let _ = self.pixel_tx.try_send(PixelBuf {
            data: rgba.into_raw(),
            width: w,
            height: h,
            format: PixelFormat::Rgba,
        });
    }
}

// ── Input mapping ─────────────────────────────────────────────────────────────

fn point(x: f32, y: f32) -> WebViewPoint {
    WebViewPoint::Device(DevicePoint::new(x, y))
}

fn button(btn: Button) -> MouseButton {
    match btn {
        Button::Left => MouseButton::Primary,
        Button::Middle => MouseButton::Auxiliary,
        Button::Right => MouseButton::Secondary,
    }
}

fn dispatch_input(webview: &WebView, event: &RemoteEvent) {
    match event {
        RemoteEvent::Key { key, state } => {
            let Ok(servo_key) = key.parse::<Key>() else {
                eprintln!("servo: unmapped key {key:?}");
                return;
            };
            let states: &[KeyState] = match state {
                KeyAction::Press => &[KeyState::Down, KeyState::Up],
                KeyAction::Down => &[KeyState::Down],
                KeyAction::Up => &[KeyState::Up],
            };
            webview.focus();
            for s in states {
                let ev = KeyboardEvent::from_state_and_key(*s, servo_key.clone());
                webview.notify_input_event(InputEvent::Keyboard(ev));
            }
        }
        RemoteEvent::MouseMove { x, y } => {
            webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(point(*x, *y))));
        }
        RemoteEvent::MouseButton { button: btn, state, x, y } => {
            let actions: &[MouseButtonAction] = match state {
                ButtonAction::Click => &[MouseButtonAction::Down, MouseButtonAction::Up],
                ButtonAction::Down => &[MouseButtonAction::Down],
                ButtonAction::Up => &[MouseButtonAction::Up],
            };
            // Make sure the page knows where the cursor is before the click.
            webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(point(*x, *y))));
            for a in actions {
                let ev = MouseButtonEvent::new(*a, button(*btn), point(*x, *y));
                webview.notify_input_event(InputEvent::MouseButton(ev));
            }
        }
        RemoteEvent::Wheel { x, y, dx, dy } => {
            // Browser wheel deltas are positive-down; servo's are positive-up.
            let delta = WheelDelta { x: -dx, y: -dy, z: 0.0, mode: WheelMode::DeltaPixel };
            webview.notify_input_event(InputEvent::Wheel(WheelEvent::new(delta, point(*x, *y))));
        }
    }
}

// ── Render lifecycle ──────────────────────────────────────────────────────────

struct SrvRender {
    webview: WebView,
    delegate: Rc<StreamDelegate>,
}

fn create_render(servo: &Servo, content: &Rc<UserContentManager>, spec: RenderSpec) -> Option<SrvRender> {
    let url = match Url::parse(&spec.url) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("render {}: bad url {}: {e}", spec.id, spec.url);
            return None;
        }
    };

    let size = PhysicalSize::new(spec.width, spec.height);
    let rendering_context: Rc<dyn RenderingContext> = Rc::new(
        SoftwareRenderingContext::new(size).expect("SoftwareRenderingContext::new failed"),
    );
    let _ = rendering_context.make_current();

    let pixel_tx = spawn_encoder(spec.last_frame);
    let delegate = Rc::new(StreamDelegate {
        rendering_context: rendering_context.clone(),
        pixel_tx,
        size: Cell::new(size),
        loaded: Cell::new(false),
    });

    let webview = WebViewBuilder::new(servo, rendering_context)
        .url(url)
        .user_content_manager(content.clone())
        .delegate(delegate.clone() as Rc<dyn WebViewDelegate>)
        .build();
    // Without an explicit resize servo keeps its default viewport for CSS media
    // queries (the framebuffer size alone doesn't set it) — @media by width or
    // height would never match the actual render size.
    webview.resize(size);
    webview.focus();

    eprintln!("render {} → {} ({}x{} @{}fps)", spec.id, spec.url, spec.width, spec.height, spec.fps);
    Some(SrvRender { webview, delegate })
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run(cmd_rx: Receiver<EngineCmd>) -> ! {
    // Pixel-UI: mono text rasterisation (a pixel is either on or off). Servo's
    // grayscale/subpixel AA is what blurs small vector fonts and paints the
    // colored fringes. Set RENDER_SERVO_TEXT_AA=1 to keep AA for comparison.
    let text_aa = std::env::var("RENDER_SERVO_TEXT_AA").is_ok_and(|v| v == "1");
    let preferences = Preferences {
        gfx_text_antialiasing_enabled: text_aa,
        gfx_subpixel_text_antialiasing_enabled: false,
        ..Preferences::default()
    };
    let servo = ServoBuilder::default().preferences(preferences).build();
    let content = Rc::new(UserContentManager::new(&servo));
    content.add_script(Rc::new(UserScript::new(VISIBLE_SCRIPT.to_string(), None)));
    let mut renders: HashMap<u64, SrvRender> = HashMap::new();

    eprintln!("servo engine started");
    loop {
        let mut had_input = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                EngineCmd::Create(spec) => {
                    let id = spec.id;
                    if let Some(render) = create_render(&servo, &content, spec) {
                        renders.insert(id, render);
                    }
                }
                EngineCmd::Destroy { id } => {
                    // Dropping the WebView (and its delegate → pixel_tx) tears
                    // the render down; the encoder thread exits with it.
                    if renders.remove(&id).is_some() {
                        eprintln!("render {id} destroyed");
                    }
                }
                EngineCmd::Input { id, event } => {
                    if let Some(render) = renders.get(&id) {
                        dispatch_input(&render.webview, &event);
                        had_input = true;
                    }
                }
                EngineCmd::Resize { id, width, height } => {
                    if let Some(render) = renders.get(&id) {
                        // Resizes the rendering context and the page viewport; resizing the
                        // context first would make servo skip the viewport (sizes match).
                        let size = PhysicalSize::new(width, height);
                        render.delegate.size.set(size);
                        render.webview.resize(size);
                    }
                }
                EngineCmd::Navigate { id, url } => {
                    if let Some(render) = renders.get(&id) {
                        match Url::parse(&url) {
                            Ok(u) => render.webview.load(u),
                            Err(e) => eprintln!("render {id}: bad url {url}: {e}"),
                        }
                    }
                }
            }
        }

        servo.spin_event_loop();

        // After input, spin aggressively to flush servo's internal pipeline —
        // this cuts input→frame latency from ~100ms to ~25ms.
        if had_input {
            for _ in 0..10 {
                servo.spin_event_loop();
            }
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
