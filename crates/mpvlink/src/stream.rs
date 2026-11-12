//! The `mpvlink://ROOM` stream protocol: how received video gets into mpv.
//!
//! The WebRTC session pushes whole H.264 frames into a [`FrameQueue`]; mpv's
//! demuxer pulls bytes out through the stream callbacks below, as if reading
//! a raw .h264 file that never ends.
//!
//! Latency matters more than completeness here: the queue holds at most a few
//! frames. If mpv falls behind (or isn't reading, e.g. paused), the backlog is
//! thrown away and the queue waits for the next keyframe, so playback resumes
//! at "now" instead of drifting further behind.

use crate::ffi::{MPV_ERROR_LOADING_FAILED, mpv_stream_cb_info};
use bytes::Bytes;
use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

pub const PROTOCOL: &str = "mpvlink";

/// About a quarter second at 30 fps. Beyond this, the reader is behind.
const MAX_QUEUED_FRAMES: usize = 8;

#[derive(Default)]
struct Inner {
    frames: VecDeque<Bytes>,
    /// How much of the front frame the reader has consumed.
    offset: usize,
    /// No more data until `reset`; readers get EOF once drained.
    ended: bool,
    /// Discard frames until a keyframe (after dropping backlog).
    need_keyframe: bool,
}

#[derive(Default)]
pub struct FrameQueue {
    inner: Mutex<Inner>,
    readable: Condvar,
}

impl FrameQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(Inner { need_keyframe: true, ..Default::default() }), ..Default::default() })
    }

    /// Queue a complete frame. Returns true if the queue is waiting for a
    /// keyframe, in which case the caller should ask the sender for one.
    pub fn push(&self, frame: Bytes, keyframe: bool) -> bool {
        let mut q = self.inner.lock().unwrap();
        if q.frames.len() >= MAX_QUEUED_FRAMES {
            // Keep a partially-read front frame so the byte stream stays intact.
            let keep = usize::from(q.offset > 0);
            q.frames.truncate(keep);
            q.need_keyframe = true;
        }
        if q.need_keyframe && !keyframe {
            return true;
        }
        q.need_keyframe = false;
        q.frames.push_back(frame);
        self.readable.notify_all();
        false
    }

    /// Signal end of stream: readers get EOF after the remaining frames.
    pub fn end(&self) {
        self.inner.lock().unwrap().ended = true;
        self.readable.notify_all();
    }

    /// Start a fresh stream (e.g. a new publisher): clear everything.
    pub fn reset(&self) {
        *self.inner.lock().unwrap() = Inner { need_keyframe: true, ..Default::default() };
    }

    /// A new reader starts at "now": drop the backlog, resume at a keyframe.
    fn attach_reader(&self) {
        let mut q = self.inner.lock().unwrap();
        q.frames.clear();
        q.offset = 0;
        q.need_keyframe = true;
    }

    /// Blocking read for mpv. Returns bytes read, 0 at EOF, -1 if cancelled.
    fn read(&self, buf: &mut [u8], cancelled: &AtomicBool) -> i64 {
        let mut q = self.inner.lock().unwrap();
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return -1;
            }
            let offset = q.offset;
            if let Some(front) = q.frames.front() {
                let n = buf.len().min(front.len() - offset);
                buf[..n].copy_from_slice(&front[offset..offset + n]);
                if offset + n == front.len() {
                    q.frames.pop_front();
                    q.offset = 0;
                } else {
                    q.offset += n;
                }
                return n as i64;
            }
            if q.ended {
                return 0;
            }
            // The timeout is only a backstop; push/end/cancel all notify.
            q = self.readable.wait_timeout(q, Duration::from_millis(250)).unwrap().0;
        }
    }

    fn wake(&self) {
        self.readable.notify_all();
    }
}

// ---- Room -> queue routing -------------------------------------------------

fn routes() -> &'static Mutex<HashMap<String, Arc<FrameQueue>>> {
    static ROUTES: OnceLock<Mutex<HashMap<String, Arc<FrameQueue>>>> = OnceLock::new();
    ROUTES.get_or_init(Default::default)
}

/// Make `mpvlink://ROOM` read from `queue`.
pub fn publish(room: &str, queue: Arc<FrameQueue>) {
    routes().lock().unwrap().insert(room.to_string(), queue);
}

pub fn unpublish(room: &str) {
    routes().lock().unwrap().remove(room);
}

pub fn url(room: &str) -> String {
    format!("{PROTOCOL}://{room}")
}

// ---- mpv stream callbacks ---------------------------------------------------
//
// mpv calls these from its demuxer thread (cancel from another thread). The
// protocol stays registered for the life of the mpv core, so the callbacks
// must not depend on plugin state that could go away first.

struct Reader {
    queue: Arc<FrameQueue>,
    cancelled: AtomicBool,
}

/// `mpv_stream_cb_open_ro_fn` for [`PROTOCOL`].
pub unsafe extern "C" fn open(_user_data: *mut c_void, uri: *mut c_char, info: *mut mpv_stream_cb_info) -> c_int {
    let uri = unsafe { CStr::from_ptr(uri) }.to_string_lossy();
    let room = uri.trim_start_matches(PROTOCOL).trim_start_matches("://").trim_end_matches('/');
    let Some(queue) = routes().lock().unwrap().get(room).cloned() else {
        return MPV_ERROR_LOADING_FAILED;
    };
    queue.attach_reader();
    let reader = Box::new(Reader { queue, cancelled: AtomicBool::new(false) });
    let info = unsafe { &mut *info };
    info.cookie = Box::into_raw(reader).cast();
    info.read_fn = Some(read);
    info.close_fn = Some(close);
    info.cancel_fn = Some(cancel);
    // No seek/size: it's a live stream.
    0
}

unsafe extern "C" fn read(cookie: *mut c_void, buf: *mut c_char, nbytes: u64) -> i64 {
    let reader = unsafe { &*(cookie as *const Reader) };
    let buf = unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), nbytes as usize) };
    reader.queue.read(buf, &reader.cancelled)
}

unsafe extern "C" fn close(cookie: *mut c_void) {
    drop(unsafe { Box::from_raw(cookie as *mut Reader) });
}

unsafe extern "C" fn cancel(cookie: *mut c_void) {
    let reader = unsafe { &*(cookie as *const Reader) };
    reader.cancelled.store(true, Ordering::Relaxed);
    reader.queue.wake();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(q: &FrameQueue) -> Vec<u8> {
        let never = AtomicBool::new(false);
        let mut out = Vec::new();
        let mut buf = [0u8; 3];
        q.end();
        loop {
            match q.read(&mut buf, &never) {
                0 => return out,
                n => out.extend_from_slice(&buf[..n as usize]),
            }
        }
    }

    #[test]
    fn starts_at_a_keyframe_and_reads_in_order() {
        let q = FrameQueue::new();
        assert!(q.push(Bytes::from_static(b"p0"), false), "needs a keyframe first");
        assert!(!q.push(Bytes::from_static(b"KEY1"), true));
        assert!(!q.push(Bytes::from_static(b"p2"), false));
        assert_eq!(read_all(&q), b"KEY1p2");
    }

    #[test]
    fn overflow_drops_backlog_and_waits_for_keyframe() {
        let q = FrameQueue::new();
        q.push(Bytes::from_static(b"K"), true);
        for _ in 1..MAX_QUEUED_FRAMES {
            q.push(Bytes::from_static(b"p"), false);
        }
        assert!(q.push(Bytes::from_static(b"x"), false), "overflow -> keyframe wanted");
        assert!(q.push(Bytes::from_static(b"y"), false));
        assert!(!q.push(Bytes::from_static(b"K2"), true));
        assert_eq!(read_all(&q), b"K2");
    }

    #[test]
    fn overflow_keeps_partially_read_frame() {
        let q = FrameQueue::new();
        q.push(Bytes::from_static(b"KEYFRAME"), true);
        let never = AtomicBool::new(false);
        let mut buf = [0u8; 3];
        assert_eq!(q.read(&mut buf, &never), 3);
        for _ in 0..MAX_QUEUED_FRAMES {
            q.push(Bytes::from_static(b"p"), false);
        }
        q.push(Bytes::from_static(b"K2"), true);
        assert_eq!(read_all(&q), b"FRAMEK2");
    }

    #[test]
    fn cancel_unblocks_reader() {
        let q = FrameQueue::new();
        let reader = Arc::new(Reader { queue: q.clone(), cancelled: AtomicBool::new(false) });
        let r = reader.clone();
        let t = std::thread::spawn(move || r.queue.read(&mut [0u8; 4], &r.cancelled));
        std::thread::sleep(Duration::from_millis(50));
        unsafe { cancel(Arc::as_ptr(&reader) as *mut c_void) };
        assert_eq!(t.join().unwrap(), -1);
    }
}
