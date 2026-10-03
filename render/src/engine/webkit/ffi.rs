//! Raw C bindings: libwpe, WPEBackend-fdo, wpe-webkit, wayland-server.
//! Only what the renderer needs — no bindgen.

#![allow(dead_code)]

use std::os::raw::{c_char, c_int, c_uint, c_void};

pub enum WpeViewBackend {}
pub enum WpeViewBackendExportableFdo {}
pub enum WpeFdoShmExportedBuffer {}
pub enum WlShmBuffer {}
pub enum WlResource {}
pub enum WebKitWebView {}
pub enum WebKitWebViewBackend {}
pub enum WebKitSettings {}

#[repr(C)]
pub struct WpeDmabufResource {
    pub buffer_resource: *mut WlResource,
    pub width: c_uint,
    pub height: c_uint,
    pub format: c_uint,
    pub n_planes: u8,
    pub fds: [c_int; 4],
    pub strides: [c_uint; 4],
    pub offsets: [c_uint; 4],
    pub modifiers: [u64; 4],
}

#[repr(C)]
pub struct WpeViewBackendExportableFdoClient {
    pub export_buffer_resource: Option<unsafe extern "C" fn(*mut c_void, *mut WlResource)>,
    pub export_dmabuf_resource: Option<unsafe extern "C" fn(*mut c_void, *mut WpeDmabufResource)>,
    pub export_shm_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut WpeFdoShmExportedBuffer)>,
    pub _reserved0: Option<unsafe extern "C" fn()>,
    pub _reserved1: Option<unsafe extern "C" fn()>,
}

// The client struct only holds function pointers — safe to share.
unsafe impl Sync for WpeViewBackendExportableFdoClient {}

// ── input structs (wpe-1.0/wpe/input.h) ──────────────────────────────────────

#[repr(C)]
pub struct WpeInputKeyboardEvent {
    pub time: u32,
    pub key_code: u32, // XKB keysym
    pub hardware_key_code: u32,
    pub pressed: bool,
    pub modifiers: u32,
}

pub const WPE_POINTER_MOTION: u32 = 1; // wpe_input_pointer_event_type_motion
pub const WPE_POINTER_BUTTON: u32 = 2; // wpe_input_pointer_event_type_button

#[repr(C)]
pub struct WpeInputPointerEvent {
    pub type_: u32,
    pub time: u32,
    pub x: c_int,
    pub y: c_int,
    pub button: u32, // 1 = left, 2 = right, 3 = middle
    pub state: u32,  // 1 = pressed, 0 = released
    pub modifiers: u32,
}

pub const WPE_AXIS_MOTION_SMOOTH: u32 = 2; // wpe_input_axis_event_type_motion_smooth
pub const WPE_AXIS_MASK_2D: u32 = 1 << 16;

#[repr(C)]
pub struct WpeInputAxisEvent {
    pub type_: u32,
    pub time: u32,
    pub x: c_int,
    pub y: c_int,
    pub axis: u32,
    pub value: i32,
    pub modifiers: u32,
}

#[repr(C)]
pub struct WpeInputAxis2dEvent {
    pub base: WpeInputAxisEvent,
    pub x_axis: f64,
    pub y_axis: f64,
}

// XKB keysyms
pub const XKB_KEY_BACKSPACE: u32 = 0xFF08;
pub const XKB_KEY_TAB: u32 = 0xFF09;
pub const XKB_KEY_RETURN: u32 = 0xFF0D;
pub const XKB_KEY_ESCAPE: u32 = 0xFF1B;
pub const XKB_KEY_HOME: u32 = 0xFF50;
pub const XKB_KEY_LEFT: u32 = 0xFF51;
pub const XKB_KEY_UP: u32 = 0xFF52;
pub const XKB_KEY_RIGHT: u32 = 0xFF53;
pub const XKB_KEY_DOWN: u32 = 0xFF54;
pub const XKB_KEY_PAGE_UP: u32 = 0xFF55;
pub const XKB_KEY_PAGE_DOWN: u32 = 0xFF56;
pub const XKB_KEY_END: u32 = 0xFF57;
pub const XKB_KEY_DELETE: u32 = 0xFFFF;
pub const XKB_KEY_SHIFT_L: u32 = 0xFFE1;
pub const XKB_KEY_CONTROL_L: u32 = 0xFFE3;
pub const XKB_KEY_ALT_L: u32 = 0xFFE9;

extern "C" {
    pub fn wpe_loader_init(impl_library_name: *const c_char) -> bool;
    pub fn wpe_fdo_initialize_shm() -> bool;

    pub fn wpe_view_backend_exportable_fdo_create(
        client: *const WpeViewBackendExportableFdoClient,
        data: *mut c_void,
        width: c_uint,
        height: c_uint,
    ) -> *mut WpeViewBackendExportableFdo;

    pub fn wpe_view_backend_exportable_fdo_destroy(exportable: *mut WpeViewBackendExportableFdo);

    pub fn wpe_view_backend_exportable_fdo_get_view_backend(
        exportable: *mut WpeViewBackendExportableFdo,
    ) -> *mut WpeViewBackend;

    pub fn wpe_view_backend_exportable_fdo_dispatch_frame_complete(
        exportable: *mut WpeViewBackendExportableFdo,
    );

    pub fn wpe_view_backend_exportable_fdo_dispatch_release_shm_exported_buffer(
        exportable: *mut WpeViewBackendExportableFdo,
        buffer: *mut WpeFdoShmExportedBuffer,
    );

    pub fn wpe_fdo_shm_exported_buffer_get_shm_buffer(
        buffer: *mut WpeFdoShmExportedBuffer,
    ) -> *mut WlShmBuffer;

    pub fn wpe_view_backend_dispatch_keyboard_event(
        backend: *mut WpeViewBackend,
        event: *const WpeInputKeyboardEvent,
    );

    pub fn wpe_view_backend_dispatch_pointer_event(
        backend: *mut WpeViewBackend,
        event: *const WpeInputPointerEvent,
    );

    pub fn wpe_view_backend_dispatch_axis_event(
        backend: *mut WpeViewBackend,
        event: *const WpeInputAxisEvent,
    );

    pub fn wpe_view_backend_dispatch_set_size(backend: *mut WpeViewBackend, width: u32, height: u32);
}

extern "C" {
    pub fn wl_shm_buffer_begin_access(buffer: *mut WlShmBuffer);
    pub fn wl_shm_buffer_end_access(buffer: *mut WlShmBuffer);
    pub fn wl_shm_buffer_get_data(buffer: *mut WlShmBuffer) -> *mut c_void;
    pub fn wl_shm_buffer_get_stride(buffer: *const WlShmBuffer) -> i32;
    pub fn wl_shm_buffer_get_width(buffer: *const WlShmBuffer) -> i32;
    pub fn wl_shm_buffer_get_height(buffer: *const WlShmBuffer) -> i32;
}

extern "C" {
    pub fn webkit_web_view_backend_new(
        backend: *mut WpeViewBackend,
        notify: Option<unsafe extern "C" fn(*mut c_void)>,
        user_data: *mut c_void,
    ) -> *mut WebKitWebViewBackend;

    pub fn webkit_web_view_new(backend: *mut WebKitWebViewBackend) -> *mut WebKitWebView;

    pub fn webkit_web_view_load_uri(view: *mut WebKitWebView, uri: *const c_char);

    pub fn webkit_web_view_get_settings(view: *mut WebKitWebView) -> *mut WebKitSettings;

    pub fn webkit_settings_set_enable_write_console_messages_to_stdout(
        settings: *mut WebKitSettings,
        enabled: bool,
    );

    pub fn g_object_unref(obj: *mut c_void);
}
