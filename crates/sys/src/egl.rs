use std::ffi::{c_char, c_void};

pub type EGLDisplay = *mut c_void;
pub type EGLContext = *mut c_void;
pub type EGLConfig = *mut c_void;
pub type EGLSurface = *mut c_void;
pub type EGLImage = *mut c_void;
pub type EGLint = i32;
pub type EGLenum = u32;
pub type EGLBoolean = u32;
pub type EGLAttrib = isize;

pub const NONE: EGLint = 0x3038;
pub const TRUE: EGLBoolean = 1;
pub const EXTENSIONS: EGLint = 0x3055;
pub const WIDTH: EGLint = 0x3057;
pub const HEIGHT: EGLint = 0x3056;
pub const OPENGL_ES_API: EGLenum = 0x30A0;
pub const CONTEXT_MAJOR_VERSION: EGLint = 0x3098;
pub const CONTEXT_MINOR_VERSION: EGLint = 0x30FB;
pub const PLATFORM_GBM_KHR: EGLenum = 0x31D7;
pub const LINUX_DMA_BUF_EXT: EGLenum = 0x3270;
pub const LINUX_DRM_FOURCC_EXT: EGLint = 0x3271;
pub const DMA_BUF_PLANE0_FD_EXT: EGLint = 0x3272;
pub const DMA_BUF_PLANE0_OFFSET_EXT: EGLint = 0x3273;
pub const DMA_BUF_PLANE0_PITCH_EXT: EGLint = 0x3274;
pub const DMA_BUF_PLANE0_MODIFIER_LO_EXT: EGLint = 0x3443;
pub const DMA_BUF_PLANE0_MODIFIER_HI_EXT: EGLint = 0x3444;
/// Per-plane attributes: (fd, offset, pitch, modifier lo, modifier hi).
pub const DMA_BUF_PLANE_ATTRIBS: [[EGLint; 5]; 4] = [
    [0x3272, 0x3273, 0x3274, 0x3443, 0x3444],
    [0x3275, 0x3276, 0x3277, 0x3445, 0x3446],
    [0x3278, 0x3279, 0x327A, 0x3447, 0x3448],
    [0x3440, 0x3441, 0x3442, 0x3449, 0x344A],
];

#[link(name = "EGL")]
extern "C" {
    pub fn eglGetProcAddress(name: *const c_char) -> *mut c_void;
    pub fn eglGetPlatformDisplay(platform: EGLenum, native: *mut c_void, attribs: *const EGLAttrib) -> EGLDisplay;
    pub fn eglInitialize(dpy: EGLDisplay, major: *mut EGLint, minor: *mut EGLint) -> EGLBoolean;
    pub fn eglTerminate(dpy: EGLDisplay) -> EGLBoolean;
    pub fn eglQueryString(dpy: EGLDisplay, name: EGLint) -> *const c_char;
    pub fn eglBindAPI(api: EGLenum) -> EGLBoolean;
    pub fn eglCreateContext(dpy: EGLDisplay, config: EGLConfig, share: EGLContext, attribs: *const EGLint) -> EGLContext;
    pub fn eglDestroyContext(dpy: EGLDisplay, ctx: EGLContext) -> EGLBoolean;
    pub fn eglMakeCurrent(dpy: EGLDisplay, draw: EGLSurface, read: EGLSurface, ctx: EGLContext) -> EGLBoolean;
    pub fn eglGetError() -> EGLint;
}

pub type PFNEGLCREATEIMAGEKHR =
    unsafe extern "C" fn(dpy: EGLDisplay, ctx: EGLContext, target: EGLenum, buffer: *mut c_void, attribs: *const EGLint) -> EGLImage;
pub type PFNEGLDESTROYIMAGEKHR = unsafe extern "C" fn(dpy: EGLDisplay, image: EGLImage) -> EGLBoolean;
pub type PFNGLEGLIMAGETARGETRENDERBUFFERSTORAGEOES = unsafe extern "C" fn(target: u32, image: EGLImage);
pub type PFNGLEGLIMAGETARGETTEXTURE2DOES = unsafe extern "C" fn(target: u32, image: EGLImage);
pub type PFNEGLQUERYDMABUFFORMATSEXT =
    unsafe extern "C" fn(dpy: EGLDisplay, max: EGLint, formats: *mut EGLint, num: *mut EGLint) -> EGLBoolean;
pub type PFNEGLQUERYDMABUFMODIFIERSEXT = unsafe extern "C" fn(
    dpy: EGLDisplay,
    format: EGLint,
    max: EGLint,
    modifiers: *mut u64,
    external_only: *mut EGLBoolean,
    num: *mut EGLint,
) -> EGLBoolean;

/// Looks up an extension function. `name` must be NUL-terminated.
pub unsafe fn proc_address<T: Copy>(name: &[u8]) -> Option<T> {
    debug_assert_eq!(name.last(), Some(&0));
    let p = eglGetProcAddress(name.as_ptr().cast());
    if p.is_null() {
        None
    } else {
        Some(std::mem::transmute_copy(&p))
    }
}
