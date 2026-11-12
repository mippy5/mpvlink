//! Raw bindings to the subset of <mpv/client.h> this plugin uses.
//!
//! Nothing here links against libmpv; mpv provides the functions when it loads
//! the plugin. On macOS and Linux they're ordinary symbols resolved from the mpv
//! executable at load time. Windows has no such lookup, so there the plugin
//! exports a `pfn_<name>` function pointer per function, which mpv (0.37+) fills
//! in before calling `mpv_open_cplugin` (`MPV_CPLUGIN_DYNAMIC_SYM` in client.h).

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_void};

#[repr(C)]
pub struct mpv_handle {
    _private: [u8; 0],
}

#[repr(C)]
pub struct mpv_event {
    pub event_id: c_int,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct mpv_event_client_message {
    pub num_args: c_int,
    pub args: *const *const c_char,
}

#[repr(C)]
pub union mpv_node_u {
    pub string: *mut c_char,
    pub flag: c_int,
    pub int64: i64,
    pub double_: f64,
    /// mpv_node_list* / mpv_byte_array* (unused here).
    pub ptr: *mut c_void,
}

#[repr(C)]
pub struct mpv_node {
    pub u: mpv_node_u,
    pub format: c_int,
}

pub const MPV_FORMAT_NONE: c_int = 0;
pub const MPV_FORMAT_STRING: c_int = 1;

pub const MPV_EVENT_NONE: c_int = 0;
pub const MPV_EVENT_SHUTDOWN: c_int = 1;
pub const MPV_EVENT_FILE_LOADED: c_int = 8;
pub const MPV_EVENT_CLIENT_MESSAGE: c_int = 16;
pub const MPV_EVENT_PROPERTY_CHANGE: c_int = 22;

pub const MPV_ERROR_LOADING_FAILED: c_int = -13;

// <mpv/stream_cb.h>: custom stream protocols.
pub type mpv_stream_cb_read_fn = unsafe extern "C" fn(cookie: *mut c_void, buf: *mut c_char, nbytes: u64) -> i64;
pub type mpv_stream_cb_seek_fn = unsafe extern "C" fn(cookie: *mut c_void, offset: i64) -> i64;
pub type mpv_stream_cb_size_fn = unsafe extern "C" fn(cookie: *mut c_void) -> i64;
pub type mpv_stream_cb_close_fn = unsafe extern "C" fn(cookie: *mut c_void);
pub type mpv_stream_cb_cancel_fn = unsafe extern "C" fn(cookie: *mut c_void);

#[repr(C)]
pub struct mpv_stream_cb_info {
    pub cookie: *mut c_void,
    pub read_fn: Option<mpv_stream_cb_read_fn>,
    pub seek_fn: Option<mpv_stream_cb_seek_fn>,
    pub size_fn: Option<mpv_stream_cb_size_fn>,
    pub close_fn: Option<mpv_stream_cb_close_fn>,
    pub cancel_fn: Option<mpv_stream_cb_cancel_fn>,
}

pub type mpv_stream_cb_open_ro_fn =
    unsafe extern "C" fn(user_data: *mut c_void, uri: *mut c_char, info: *mut mpv_stream_cb_info) -> c_int;

/// Declares the mpv functions this plugin calls: plain `extern` declarations on
/// macOS/Linux, and on Windows exported `pfn_*` pointers plus same-named
/// wrappers, so callers look the same on every platform.
macro_rules! mpv_functions {
    ($(pub fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)?;)*) => {
        #[cfg(not(windows))]
        unsafe extern "C" {
            $(pub fn $name($($arg: $ty),*) $(-> $ret)?;)*
        }

        /// The pointers mpv fills in, one module per function (modules and
        /// functions live in different namespaces, so the names can match).
        #[cfg(windows)]
        mod slots {
            use super::*;
            $(
                pub mod $name {
                    use super::*;
                    #[unsafe(export_name = concat!("pfn_", stringify!($name)))]
                    pub static mut PTR: Option<unsafe extern "C" fn($($ty),*) $(-> $ret)?> = None;
                }
            )*
        }

        $(
            #[cfg(windows)]
            pub unsafe fn $name($($arg: $ty),*) $(-> $ret)? {
                // SAFETY: mpv writes the pointer once, before mpv_open_cplugin
                // runs; everything after only reads it.
                let f = unsafe { slots::$name::PTR }
                    .expect(concat!("mpv didn't provide ", stringify!($name)));
                unsafe { f($($arg),*) }
            }
        )*

        /// Whether mpv provided every function (always true off Windows, where
        /// a missing symbol fails the load instead). Older mpv on Windows
        /// doesn't fill the pointers in.
        pub fn functions_available() -> bool {
            #[cfg(windows)]
            {
                // SAFETY: see above; a plain read of each pointer.
                true $(&& unsafe { slots::$name::PTR }.is_some())*
            }
            #[cfg(not(windows))]
            {
                true
            }
        }
    };
}

mpv_functions! {
    pub fn mpv_stream_cb_add_ro(
        ctx: *mut mpv_handle,
        protocol: *const c_char,
        user_data: *mut c_void,
        open_fn: Option<mpv_stream_cb_open_ro_fn>,
    ) -> c_int;
    pub fn mpv_client_name(ctx: *mut mpv_handle) -> *const c_char;
    pub fn mpv_wait_event(ctx: *mut mpv_handle, timeout: f64) -> *mut mpv_event;
    pub fn mpv_command(ctx: *mut mpv_handle, args: *mut *const c_char) -> c_int;
    pub fn mpv_command_ret(ctx: *mut mpv_handle, args: *mut *const c_char, result: *mut mpv_node) -> c_int;
    pub fn mpv_free_node_contents(node: *mut mpv_node);
    pub fn mpv_get_property_string(ctx: *mut mpv_handle, name: *const c_char) -> *mut c_char;
    pub fn mpv_set_property_string(
        ctx: *mut mpv_handle,
        name: *const c_char,
        data: *const c_char,
    ) -> c_int;
    pub fn mpv_observe_property(
        ctx: *mut mpv_handle,
        reply_userdata: u64,
        name: *const c_char,
        format: c_int,
    ) -> c_int;
    pub fn mpv_unobserve_property(ctx: *mut mpv_handle, registered_reply_userdata: u64) -> c_int;
    pub fn mpv_error_string(error: c_int) -> *const c_char;
    pub fn mpv_free(data: *mut c_void);
}
