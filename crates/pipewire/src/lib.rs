//! Video source streams on PipeWire, for screen casting.
//!
//! The PipeWire loop runs on the compositor's thread: its fd sits in the
//! compositor's epoll and `dispatch` iterates it, so stream callbacks run
//! there too and need no locking. Frames are RGBx in memfd buffers that
//! PipeWire allocates; SPA parameters are encoded by hand (`pod`), the
//! builder of libspa being header-only.

use std::ffi::{c_char, c_int, c_void, CString};
use std::io;
use std::ptr::null_mut;
use std::time::{Duration, Instant};

pub mod pod;

/// Pods are read with 8-byte loads: keep them 8-aligned.
fn aligned(bytes: Vec<u8>) -> Vec<u64> {
    bytes.as_chunks::<8>().0.iter().map(|c| u64::from_ne_bytes(*c)).collect()
}

#[repr(C)]
struct PwBuffer {
    buffer: *mut SpaBuffer,
    user_data: *mut c_void,
    size: u64,
    requested: u64,
}

#[repr(C)]
struct SpaBuffer {
    n_metas: u32,
    n_datas: u32,
    metas: *mut c_void,
    datas: *mut SpaData,
}

#[repr(C)]
struct SpaData {
    ty: u32,
    flags: u32,
    fd: i64,
    mapoffset: u32,
    maxsize: u32,
    data: *mut c_void,
    chunk: *mut SpaChunk,
}

#[repr(C)]
struct SpaChunk {
    offset: u32,
    size: u32,
    stride: i32,
    flags: i32,
}

#[repr(C)]
struct StreamEvents {
    version: u32,
    destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    state_changed: Option<unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_char)>,
    control_info: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    io_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *mut c_void, u32)>,
    param_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    add_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut PwBuffer)>,
    remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut PwBuffer)>,
    process: Option<unsafe extern "C" fn(*mut c_void)>,
    drained: Option<unsafe extern "C" fn(*mut c_void)>,
    command: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
    trigger_done: Option<unsafe extern "C" fn(*mut c_void)>,
}

/// `struct spa_hook`: a list link, callbacks, removed, priv.
type Hook = [usize; 6];

#[link(name = "pipewire-0.3")]
extern "C" {
    fn pw_init(argc: *mut c_int, argv: *mut *mut *mut c_char);
    fn pw_loop_new(props: *const c_void) -> *mut c_void;
    fn pw_loop_destroy(l: *mut c_void);
    fn pw_loop_get_fd(l: *mut c_void) -> c_int;
    fn pw_loop_iterate(l: *mut c_void, timeout: c_int) -> c_int;
    fn pw_loop_enter(l: *mut c_void);
    fn pw_context_new(l: *mut c_void, props: *mut c_void, user_data_size: usize) -> *mut c_void;
    fn pw_context_destroy(c: *mut c_void);
    fn pw_context_connect(c: *mut c_void, props: *mut c_void, user_data_size: usize) -> *mut c_void;
    fn pw_core_disconnect(core: *mut c_void) -> c_int;
    fn pw_properties_new(key: *const c_char, ...) -> *mut c_void;
    fn pw_stream_new(core: *mut c_void, name: *const c_char, props: *mut c_void) -> *mut c_void;
    fn pw_stream_add_listener(s: *mut c_void, hook: *mut Hook, events: *const StreamEvents, data: *mut c_void);
    fn pw_stream_connect(s: *mut c_void, direction: c_int, target: u32, flags: u32, params: *mut *const c_void, n: u32) -> c_int;
    fn pw_stream_update_params(s: *mut c_void, params: *mut *const c_void, n: u32) -> c_int;
    fn pw_stream_get_node_id(s: *mut c_void) -> u32;
    fn pw_stream_dequeue_buffer(s: *mut c_void) -> *mut PwBuffer;
    fn pw_stream_queue_buffer(s: *mut c_void, b: *mut PwBuffer) -> c_int;
    fn pw_stream_trigger_process(s: *mut c_void) -> c_int;
    fn pw_stream_destroy(s: *mut c_void);
}

const DIRECTION_OUTPUT: c_int = 1;
const ID_ANY: u32 = 0xffff_ffff;
const FLAG_MAP_BUFFERS: u32 = 1 << 2;
const FLAG_DRIVER: u32 = 1 << 3;
const PARAM_FORMAT: u32 = 4;

/// The connection to the PipeWire daemon and its loop.
pub struct PipeWire {
    lp: *mut c_void,
    context: *mut c_void,
    core: *mut c_void,
}

impl PipeWire {
    pub fn connect() -> io::Result<PipeWire> {
        unsafe {
            pw_init(null_mut(), null_mut());
            let lp = pw_loop_new(std::ptr::null());
            if lp.is_null() {
                return Err(io::Error::other("pw_loop_new failed"));
            }
            pw_loop_enter(lp);
            let context = pw_context_new(lp, null_mut(), 0);
            if context.is_null() {
                pw_loop_destroy(lp);
                return Err(io::Error::other("pw_context_new failed"));
            }
            let core = pw_context_connect(context, null_mut(), 0);
            if core.is_null() {
                pw_context_destroy(context);
                pw_loop_destroy(lp);
                return Err(io::Error::other("cannot connect to PipeWire"));
            }
            Ok(PipeWire { lp, context, core })
        }
    }

    pub fn raw_fd(&self) -> i32 {
        unsafe { pw_loop_get_fd(self.lp) }
    }

    /// Runs whatever the loop has ready; callbacks of the streams run here.
    pub fn dispatch(&self) {
        unsafe { pw_loop_iterate(self.lp, 0) };
    }
}

impl Drop for PipeWire {
    fn drop(&mut self) {
        unsafe {
            pw_core_disconnect(self.core);
            pw_context_destroy(self.context);
            pw_loop_destroy(self.lp);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    Error,
    Unconnected,
    Connecting,
    Paused,
    Streaming,
}

/// What the callbacks touch; boxed so its address stays put.
struct Inner {
    stream: *mut c_void,
    width: u32,
    height: u32,
    state: State,
    error: Option<String>,
    negotiated: bool,
    /// A frame waiting for a free buffer.
    pending: Option<Vec<u8>>,
}

pub struct Stream {
    inner: Box<Inner>,
    _hook: Box<Hook>,
    _events: Box<StreamEvents>,
    fps: u32,
    last_frame: Option<Instant>,
}

unsafe extern "C" fn on_state(data: *mut c_void, _old: c_int, state: c_int, error: *const c_char) {
    let inner = &mut *(data as *mut Inner);
    inner.state = match state {
        -1 => State::Error,
        1 => State::Connecting,
        2 => State::Paused,
        3 => State::Streaming,
        _ => State::Unconnected,
    };
    if !error.is_null() {
        inner.error = Some(std::ffi::CStr::from_ptr(error).to_string_lossy().into_owned());
    }
}

unsafe extern "C" fn on_param(data: *mut c_void, id: u32, param: *const c_void) {
    let inner = &mut *(data as *mut Inner);
    if id != PARAM_FORMAT || param.is_null() {
        return;
    }
    inner.negotiated = true;
    let stride = inner.width * 4;
    let buffers = aligned(pod::buffers(stride * inner.height, stride));
    let mut params = [buffers.as_ptr() as *const c_void];
    pw_stream_update_params(inner.stream, params.as_mut_ptr(), 1);
}

unsafe extern "C" fn on_process(data: *mut c_void) {
    let inner = &mut *(data as *mut Inner);
    let Some(frame) = inner.pending.take() else { return };
    let b = pw_stream_dequeue_buffer(inner.stream);
    if b.is_null() {
        inner.pending = Some(frame);
        return;
    }
    let buf = &mut *(*b).buffer;
    if buf.n_datas > 0 {
        let d = &mut *buf.datas;
        let len = frame.len().min(d.maxsize as usize);
        if !d.data.is_null() {
            std::ptr::copy_nonoverlapping(frame.as_ptr(), d.data as *mut u8, len);
            let chunk = &mut *d.chunk;
            chunk.offset = 0;
            chunk.size = len as u32;
            chunk.stride = (inner.width * 4) as i32;
            chunk.flags = 0;
        }
    }
    pw_stream_queue_buffer(inner.stream, b);
}

impl Stream {
    /// A video source of `width`x`height` RGBx frames at most `fps` a second.
    pub fn new(pw: &PipeWire, name: &str, width: u32, height: u32, fps: u32) -> io::Result<Stream> {
        let cname = CString::new(name).unwrap_or_default();
        unsafe {
            let props = pw_properties_new(
                c"media.class".as_ptr(),
                c"Video/Source".as_ptr(),
                c"media.type".as_ptr(),
                c"Video".as_ptr(),
                c"media.category".as_ptr(),
                c"Capture".as_ptr(),
                c"media.role".as_ptr(),
                c"Screen".as_ptr(),
                c"node.name".as_ptr(),
                cname.as_ptr(),
                std::ptr::null::<c_char>(),
            );
            let raw = pw_stream_new(pw.core, cname.as_ptr(), props);
            if raw.is_null() {
                return Err(io::Error::other("pw_stream_new failed"));
            }
            let mut inner = Box::new(Inner { stream: raw, width, height, state: State::Unconnected, error: None, negotiated: false, pending: None });
            let events = Box::new(StreamEvents {
                version: 2,
                destroy: None,
                state_changed: Some(on_state),
                control_info: None,
                io_changed: None,
                param_changed: Some(on_param),
                add_buffer: None,
                remove_buffer: None,
                process: Some(on_process),
                drained: None,
                command: None,
                trigger_done: None,
            });
            let mut hook: Box<Hook> = Box::new([0; 6]);
            pw_stream_add_listener(raw, &mut *hook, &*events, &mut *inner as *mut Inner as *mut c_void);
            let format = aligned(pod::video_format(width, height, fps));
            let mut params = [format.as_ptr() as *const c_void];
            let r = pw_stream_connect(raw, DIRECTION_OUTPUT, ID_ANY, FLAG_DRIVER | FLAG_MAP_BUFFERS, params.as_mut_ptr(), 1);
            let stream = Stream { inner, _hook: hook, _events: events, fps: fps.max(1), last_frame: None };
            if r < 0 {
                return Err(io::Error::from_raw_os_error(-r));
            }
            Ok(stream)
        }
    }

    pub fn state(&self) -> State {
        self.inner.state
    }

    pub fn error(&self) -> Option<&str> {
        self.inner.error.as_deref()
    }

    /// The node consumers connect to, once the stream is registered.
    pub fn node_id(&self) -> Option<u32> {
        let id = unsafe { pw_stream_get_node_id(self.inner.stream) };
        (id != ID_ANY && matches!(self.inner.state, State::Paused | State::Streaming)).then_some(id)
    }

    pub fn size(&self) -> (u32, u32) {
        (self.inner.width, self.inner.height)
    }

    /// A consumer is reading and the frame interval has passed.
    pub fn wants_frame(&self) -> bool {
        self.inner.state == State::Streaming
            && self.inner.negotiated
            && self.last_frame.is_none_or(|t| t.elapsed() >= Duration::from_secs(1) / self.fps)
    }

    /// Hands over a frame (RGBx, rows top to bottom, `width * 4` bytes each).
    pub fn push(&mut self, frame: Vec<u8>) {
        self.last_frame = Some(Instant::now());
        self.inner.pending = Some(frame);
        unsafe { pw_stream_trigger_process(self.inner.stream) };
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe { pw_stream_destroy(self.inner.stream) };
    }
}
