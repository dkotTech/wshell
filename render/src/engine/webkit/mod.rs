//! WPE WebKit engine: fully headless rendering via the FDO SHM backend.
//! All webviews share one GLib main loop on the calling (main) thread;
//! commands from the API arrive over an mpsc channel polled every 5 ms.

mod ffi;

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::mpsc::{Receiver, SyncSender};
use std::time::Duration;

use crate::encode::{spawn_encoder, PixelBuf, PixelFormat};
use crate::types::{Button, ButtonAction, EngineCmd, KeyAction, RemoteEvent, RenderSpec};
use ffi::*;

pub const NAME: &str = "webkit";

// ── Frame export callback ────────────────────────────────────────────────────

/// Heap state owned by one render; the exportable's user-data pointer.
struct CallbackState {
    exportable: *mut WpeViewBackendExportableFdo,
    pixel_tx: SyncSender<PixelBuf>,
}

unsafe extern "C" fn on_shm_buffer(
    data: *mut std::os::raw::c_void,
    buf: *mut WpeFdoShmExportedBuffer,
) {
    let s = &mut *(data as *mut CallbackState);

    // Forward every exported frame — no rate throttling. Dropping frames here
    // risks discarding the final, fully-rendered frame and leaving the viewer
    // stuck on a partial paint. Backpressure is handled by the encoder's
    // sync_channel(1), which always keeps the latest frame.
    let shm = wpe_fdo_shm_exported_buffer_get_shm_buffer(buf);
    wl_shm_buffer_begin_access(shm);
    let w = wl_shm_buffer_get_width(shm) as u32;
    let h = wl_shm_buffer_get_height(shm) as u32;
    let stride = wl_shm_buffer_get_stride(shm) as u32;
    let src = wl_shm_buffer_get_data(shm) as *const u8;
    let size = (h * stride) as usize;

    // Fast pixel copy only — encoding happens on the encoder thread.
    let mut pixels = vec![0u8; size];
    std::ptr::copy_nonoverlapping(src, pixels.as_mut_ptr(), size);
    wl_shm_buffer_end_access(shm);

    let _ = s.pixel_tx.try_send(PixelBuf {
        data: pixels,
        width: w,
        height: h,
        format: PixelFormat::BgraStride(stride),
    });

    // ACK immediately — unblocks WPE to render the next frame.
    wpe_view_backend_exportable_fdo_dispatch_frame_complete(s.exportable);
    wpe_view_backend_exportable_fdo_dispatch_release_shm_exported_buffer(s.exportable, buf);
}

static CLIENT: WpeViewBackendExportableFdoClient = WpeViewBackendExportableFdoClient {
    export_buffer_resource: None,
    export_dmabuf_resource: None,
    export_shm_buffer: Some(on_shm_buffer),
    _reserved0: None,
    _reserved1: None,
};

// ── Input mapping ─────────────────────────────────────────────────────────────

/// JS KeyboardEvent.key → XKB keysym.
fn key_to_keysym(key: &str) -> Option<u32> {
    let named = match key {
        "Enter" => XKB_KEY_RETURN,
        "Backspace" => XKB_KEY_BACKSPACE,
        "Tab" => XKB_KEY_TAB,
        "Escape" => XKB_KEY_ESCAPE,
        "ArrowUp" => XKB_KEY_UP,
        "ArrowDown" => XKB_KEY_DOWN,
        "ArrowLeft" => XKB_KEY_LEFT,
        "ArrowRight" => XKB_KEY_RIGHT,
        "Delete" => XKB_KEY_DELETE,
        "Home" => XKB_KEY_HOME,
        "End" => XKB_KEY_END,
        "PageUp" => XKB_KEY_PAGE_UP,
        "PageDown" => XKB_KEY_PAGE_DOWN,
        "Shift" => XKB_KEY_SHIFT_L,
        "Control" => XKB_KEY_CONTROL_L,
        "Alt" => XKB_KEY_ALT_L,
        _ => 0,
    };
    if named != 0 {
        return Some(named);
    }
    // Single character → unicode keysym (ASCII maps directly).
    let mut chars = key.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None; // unknown multi-char named key
    }
    let cp = c as u32;
    Some(if (0x20..=0x7e).contains(&cp) { cp } else { 0x0100_0000 + cp })
}

unsafe fn dispatch_key(backend: *mut WpeViewBackend, keysym: u32, pressed: bool) {
    let ev = WpeInputKeyboardEvent {
        time: 0,
        key_code: keysym,
        hardware_key_code: 0,
        pressed,
        modifiers: 0,
    };
    wpe_view_backend_dispatch_keyboard_event(backend, &ev);
}

unsafe fn dispatch_button(backend: *mut WpeViewBackend, btn: Button, pressed: bool, x: i32, y: i32) {
    let (button, modifier) = match btn {
        Button::Left => (1, 1u32 << 20),
        Button::Right => (2, 1 << 21),
        Button::Middle => (3, 1 << 22),
    };
    let ev = WpeInputPointerEvent {
        type_: WPE_POINTER_BUTTON,
        time: 0,
        x,
        y,
        button,
        state: pressed as u32,
        modifiers: if pressed { modifier } else { 0 },
    };
    wpe_view_backend_dispatch_pointer_event(backend, &ev);
}

unsafe fn dispatch_input(backend: *mut WpeViewBackend, event: &RemoteEvent) {
    match event {
        RemoteEvent::Key { key, state } => {
            let Some(keysym) = key_to_keysym(key) else {
                eprintln!("webkit: unmapped key {key:?}");
                return;
            };
            match state {
                KeyAction::Press => {
                    dispatch_key(backend, keysym, true);
                    dispatch_key(backend, keysym, false);
                }
                KeyAction::Down => dispatch_key(backend, keysym, true),
                KeyAction::Up => dispatch_key(backend, keysym, false),
            }
        }
        RemoteEvent::MouseMove { x, y } => {
            let ev = WpeInputPointerEvent {
                type_: WPE_POINTER_MOTION,
                time: 0,
                x: *x as i32,
                y: *y as i32,
                button: 0,
                state: 0,
                modifiers: 0,
            };
            wpe_view_backend_dispatch_pointer_event(backend, &ev);
        }
        RemoteEvent::MouseButton { button, state, x, y } => {
            let (x, y) = (*x as i32, *y as i32);
            match state {
                ButtonAction::Click => {
                    dispatch_button(backend, *button, true, x, y);
                    dispatch_button(backend, *button, false, x, y);
                }
                ButtonAction::Down => dispatch_button(backend, *button, true, x, y),
                ButtonAction::Up => dispatch_button(backend, *button, false, x, y),
            }
        }
        RemoteEvent::Wheel { x, y, dx, dy } => {
            let ev = WpeInputAxis2dEvent {
                base: WpeInputAxisEvent {
                    type_: WPE_AXIS_MASK_2D | WPE_AXIS_MOTION_SMOOTH,
                    time: 0,
                    x: *x as i32,
                    y: *y as i32,
                    axis: 0,
                    value: 0,
                    modifiers: 0,
                },
                // Browser wheel deltas are positive-down; WPE axis is positive-up.
                x_axis: -dx,
                y_axis: -dy,
            };
            wpe_view_backend_dispatch_axis_event(backend, &ev.base);
        }
    }
}

// ── Render lifecycle ──────────────────────────────────────────────────────────

struct WpeRender {
    webview: *mut WebKitWebView,
    backend: *mut WpeViewBackend,
    state: *mut CallbackState,
}

unsafe fn create_render(spec: RenderSpec) -> WpeRender {
    let pixel_tx = spawn_encoder(spec.last_frame);
    let state = Box::into_raw(Box::new(CallbackState {
        exportable: std::ptr::null_mut(),
        pixel_tx,
    }));

    let exportable =
        wpe_view_backend_exportable_fdo_create(&CLIENT, state as *mut _, spec.width, spec.height);
    assert!(!exportable.is_null(), "exportable_fdo_create failed");
    (*state).exportable = exportable;

    let backend = wpe_view_backend_exportable_fdo_get_view_backend(exportable);
    assert!(!backend.is_null());

    // The WebKitWebViewBackend takes ownership: destroying the webview later
    // runs this notify, which tears the exportable (and view backend) down.
    let wk_backend = webkit_web_view_backend_new(
        backend,
        Some(destroy_exportable_notify),
        exportable as *mut _,
    );
    let webview = webkit_web_view_new(wk_backend);
    assert!(
        !webview.is_null(),
        "webkit_web_view_new failed — try WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1"
    );

    // Surface page console messages (incl. JS errors) on our stdout.
    let settings = webkit_web_view_get_settings(webview);
    webkit_settings_set_enable_write_console_messages_to_stdout(settings, true);

    let uri = CString::new(spec.url.as_str()).expect("url with NUL byte");
    webkit_web_view_load_uri(webview, uri.as_ptr());

    eprintln!("render {} → {} ({}x{} @{}fps)", spec.id, spec.url, spec.width, spec.height, spec.fps);
    WpeRender { webview, backend, state }
}

unsafe extern "C" fn destroy_exportable_notify(data: *mut std::os::raw::c_void) {
    wpe_view_backend_exportable_fdo_destroy(data as *mut WpeViewBackendExportableFdo);
}

unsafe fn destroy_render(r: WpeRender) {
    // Dropping the last webview ref runs destroy_exportable_notify for us.
    g_object_unref(r.webview as *mut _);
    // In-flight frame exports may still hit the callback state right after the
    // unref, so free it a beat later from the same GLib thread.
    let state = r.state as usize;
    glib::timeout_add_local_once(Duration::from_secs(2), move || unsafe {
        drop(Box::from_raw(state as *mut CallbackState));
    });
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run(cmd_rx: Receiver<EngineCmd>) -> ! {
    // WPE's web-process sandbox needs bwrap+dbus plumbing that doesn't exist on
    // a bare server; this renderer only loads trusted local UI bundles.
    if std::env::var_os("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS").is_none() {
        std::env::set_var("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS", "1");
    }

    unsafe {
        let lib = CString::new("libWPEBackend-fdo-1.0.so").unwrap();
        assert!(wpe_loader_init(lib.as_ptr()), "wpe_loader_init failed");
        assert!(wpe_fdo_initialize_shm(), "wpe_fdo_initialize_shm failed");
    }

    let mut renders: HashMap<u64, WpeRender> = HashMap::new();

    // Poll the command channel from inside the GLib loop.
    glib::timeout_add_local(Duration::from_millis(5), move || {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                EngineCmd::Create(spec) => {
                    let id = spec.id;
                    let render = unsafe { create_render(spec) };
                    renders.insert(id, render);
                }
                EngineCmd::Destroy { id } => {
                    if let Some(render) = renders.remove(&id) {
                        unsafe { destroy_render(render) };
                        eprintln!("render {id} destroyed");
                    }
                }
                EngineCmd::Input { id, event } => {
                    if let Some(render) = renders.get(&id) {
                        unsafe { dispatch_input(render.backend, &event) };
                    }
                }
                EngineCmd::Navigate { id, url } => {
                    if let Some(render) = renders.get(&id) {
                        if let Ok(uri) = CString::new(url) {
                            unsafe { webkit_web_view_load_uri(render.webview, uri.as_ptr()) };
                        }
                    }
                }
                // Frames carry their own size (read from each SHM buffer).
                EngineCmd::Resize { id, width, height } => {
                    if let Some(render) = renders.get(&id) {
                        unsafe { wpe_view_backend_dispatch_set_size(render.backend, width, height) };
                    }
                }
            }
        }
        glib::ControlFlow::Continue
    });

    eprintln!("wpe webkit engine started");
    glib::MainLoop::new(None, false).run();
    unreachable!()
}
