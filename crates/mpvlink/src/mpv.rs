//! A small safe wrapper around the raw client API.

use crate::ffi::*;
use std::ffi::{CStr, CString, c_char, c_int};
use std::path::PathBuf;
use std::ptr;

pub type Result<T> = std::result::Result<T, String>;

/// Events the plugin cares about, copied out of mpv's event struct (which is
/// only valid until the next `wait_event` call).
pub enum Event {
    /// The wait timed out.
    None,
    Shutdown,
    FileLoaded,
    /// Playback (re)started after a seek or loading a file.
    PlaybackRestart,
    /// A `script-message-to` / `script-binding` addressed to this client.
    ClientMessage(Vec<String>),
    /// An observed property changed; `id` is the id passed to `observe`.
    PropertyChange { id: u64 },
    Other,
}

pub struct Mpv {
    ctx: *mut mpv_handle,
    name: String,
}

impl Mpv {
    /// # Safety
    /// `ctx` must be the valid handle mpv passed to `mpv_open_cplugin`, and the
    /// returned value must only be used from that thread.
    pub unsafe fn from_raw(ctx: *mut mpv_handle) -> Self {
        let name = unsafe { cstr_to_string(mpv_client_name(ctx)) };
        Self { ctx, name }
    }

    /// This client's name, i.e. the plugin filename without extension. Used to
    /// address `script-binding <name>/<binding>` commands at ourselves.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn command(&self, args: &[&str]) -> Result<()> {
        self.remote().command(args)
    }

    /// A handle other threads can use to run commands (the client API is
    /// thread-safe; only `wait_event` is reserved for the plugin thread).
    pub fn remote(&self) -> Remote {
        Remote { ctx: self.ctx }
    }

    /// Register a custom `protocol://` handled by `open`.
    pub fn add_stream_protocol(&self, protocol: &str, open: mpv_stream_cb_open_ro_fn) -> Result<()> {
        let protocol = CString::new(protocol).map_err(|e| e.to_string())?;
        check(unsafe { mpv_stream_cb_add_ro(self.ctx, protocol.as_ptr(), ptr::null_mut(), Some(open)) })
    }

    /// Run a command that returns a string, such as `expand-path`.
    pub fn command_string(&self, args: &[&str]) -> Result<String> {
        let args = CArgs::new(args)?;
        let mut node = mpv_node { u: mpv_node_u { ptr: ptr::null_mut() }, format: MPV_FORMAT_NONE };
        check(unsafe { mpv_command_ret(self.ctx, args.ptrs(), &mut node) })?;
        let value = (node.format == MPV_FORMAT_STRING).then(|| unsafe { cstr_to_string(node.u.string) });
        unsafe { mpv_free_node_contents(&mut node) };
        value.ok_or_else(|| "command did not return a string".to_string())
    }

    /// Expand mpv path prefixes like `~~state/` (see "Paths" in the mpv manual).
    ///
    /// Returns None unless the result is absolute: with --no-config there is no
    /// config/state directory, and mpv expands `~~state/x` to the relative `x`,
    /// which would land in whatever directory mpv was started from.
    pub fn expand_path(&self, path: &str) -> Option<PathBuf> {
        let expanded = PathBuf::from(self.command_string(&["expand-path", path]).ok()?);
        expanded.is_absolute().then_some(expanded)
    }

    pub fn get_string(&self, name: &str) -> Option<String> {
        self.remote().get_string(name)
    }

    pub fn get_f64(&self, name: &str) -> Option<f64> {
        self.get_string(name)?.parse().ok()
    }

    pub fn get_bool(&self, name: &str) -> Option<bool> {
        match self.get_string(name)?.as_str() {
            "yes" => Some(true),
            "no" => Some(false),
            _ => None,
        }
    }

    pub fn set_string(&self, name: &str, value: &str) -> Result<()> {
        let name = CString::new(name).map_err(|e| e.to_string())?;
        let value = CString::new(value).map_err(|e| e.to_string())?;
        check(unsafe { mpv_set_property_string(self.ctx, name.as_ptr(), value.as_ptr()) })
    }

    /// Get a `PropertyChange { id }` event whenever `name` changes.
    pub fn observe(&self, id: u64, name: &str) -> Result<()> {
        let name = CString::new(name).map_err(|e| e.to_string())?;
        check(unsafe { mpv_observe_property(self.ctx, id, name.as_ptr(), MPV_FORMAT_NONE) })
    }

    /// Wait for the next event; a negative timeout waits forever.
    pub fn wait_event(&self, timeout: f64) -> Event {
        let ev = unsafe { &*mpv_wait_event(self.ctx, timeout) };
        match ev.event_id {
            MPV_EVENT_NONE => Event::None,
            MPV_EVENT_SHUTDOWN => Event::Shutdown,
            MPV_EVENT_FILE_LOADED => Event::FileLoaded,
            MPV_EVENT_PLAYBACK_RESTART => Event::PlaybackRestart,
            MPV_EVENT_CLIENT_MESSAGE => {
                let msg = unsafe { &*(ev.data as *const mpv_event_client_message) };
                let args = (0..msg.num_args as usize)
                    .map(|i| unsafe { cstr_to_string(*msg.args.add(i)) })
                    .collect();
                Event::ClientMessage(args)
            }
            MPV_EVENT_PROPERTY_CHANGE => Event::PropertyChange { id: ev.reply_userdata },
            _ => Event::Other,
        }
    }
}

/// Read a scalar field from a flat JSON object as mpv formats it, e.g.
/// `json_field(r#"{"x":10,"hover":true}"#, "x") == Some("10")`.
/// Not a general JSON parser; it's only meant for mpv's small fixed-shape maps.
pub fn json_field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":");
    let start = json.find(&needle)? + needle.len();
    let rest = &json[start..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(rest[..end].trim())
}

/// Thread-safe command handle. Must not be used after the plugin returns from
/// `mpv_open_cplugin` (mpv destroys the handle then), so anything holding one
/// has to be shut down before that.
#[derive(Clone, Copy)]
pub struct Remote {
    ctx: *mut mpv_handle,
}

// SAFETY: libmpv's client API functions (other than mpv_wait_event) are
// documented as thread-safe; see the lifetime rule above.
unsafe impl Send for Remote {}
unsafe impl Sync for Remote {}

impl Remote {
    pub fn command(&self, args: &[&str]) -> Result<()> {
        let args = CArgs::new(args)?;
        check(unsafe { mpv_command(self.ctx, args.ptrs()) })
    }

    pub fn get_string(&self, name: &str) -> Option<String> {
        let name = CString::new(name).ok()?;
        unsafe {
            let raw = mpv_get_property_string(self.ctx, name.as_ptr());
            if raw.is_null() {
                return None;
            }
            let value = cstr_to_string(raw);
            mpv_free(raw.cast());
            Some(value)
        }
    }

    pub fn get_f64(&self, name: &str) -> Option<f64> {
        self.get_string(name)?.parse().ok()
    }
}

/// A NULL-terminated `const char*` array that owns its strings.
struct CArgs {
    _owned: Vec<CString>,
    ptrs: Vec<*const c_char>,
}

impl CArgs {
    fn new(args: &[&str]) -> Result<Self> {
        let owned = args
            .iter()
            .map(|a| CString::new(*a))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "command argument contains a NUL byte".to_string())?;
        let mut ptrs: Vec<*const c_char> = owned.iter().map(|s| s.as_ptr()).collect();
        ptrs.push(ptr::null());
        Ok(Self { _owned: owned, ptrs })
    }

    fn ptrs(&self) -> *mut *const c_char {
        // mpv takes `const char **`; it doesn't write through the array.
        self.ptrs.as_ptr().cast_mut()
    }
}

fn check(rc: c_int) -> Result<()> {
    if rc >= 0 {
        Ok(())
    } else {
        Err(unsafe { cstr_to_string(mpv_error_string(rc)) })
    }
}

unsafe fn cstr_to_string(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn json_field_reads_mouse_pos() {
        let j = r#"{"x":10,"y":20,"hover":true}"#;
        assert_eq!(super::json_field(j, "x"), Some("10"));
        assert_eq!(super::json_field(j, "y"), Some("20"));
        assert_eq!(super::json_field(j, "hover"), Some("true"));
        assert_eq!(super::json_field(j, "z"), None);
    }
}
