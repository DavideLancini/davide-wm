//! GLES renderer. Draws into GBM buffers that can be scanned out or handed
//! to a host compositor as dmabufs.

use std::ffi::CStr;
use std::fmt;
use std::fs::OpenOptions;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr::null_mut;

use sys::{egl, gbm, gl};

#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn err<T>(msg: impl Into<String>) -> Result<T, Error> {
    Err(Error(msg.into()))
}

/// A GBM buffer bound to a framebuffer object.
pub struct Target {
    bo: *mut gbm::gbm_bo,
    image: egl::EGLImage,
    rb: gl::GLuint,
    fbo: gl::GLuint,
    pub width: i32,
    pub height: i32,
    pub format: u32,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
}

impl Target {
    /// GEM handle of the buffer on the device the renderer was created on.
    pub fn handle(&self) -> u32 {
        unsafe { gbm::gbm_bo_get_handle(self.bo) as u32 }
    }

    /// A new dmabuf fd for the buffer.
    pub fn export_fd(&self) -> Result<OwnedFd, Error> {
        let fd = unsafe { gbm::gbm_bo_get_fd(self.bo) };
        if fd < 0 {
            return err("gbm_bo_get_fd failed");
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

/// One plane of a client dmabuf.
pub struct Plane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

/// `DRM_FORMAT_MOD_INVALID`: the driver picks the layout implicitly.
pub const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

pub struct Texture {
    id: gl::GLuint,
    pub width: i32,
    pub height: i32,
}

/// Rectangle in output pixels, y pointing down.
#[derive(Clone, Copy, Debug)]
pub struct Quad {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// One vertex of a batch: position in output pixels, texture coordinate in
/// the atlas (negative: solid) and premultiplied colour.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct BatchVertex {
    pos: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
}

/// Rectangles drawn in one call: solid fills and glyphs from an atlas
/// (alpha-only texture) tinted with their colour.
#[derive(Default)]
pub struct Batch {
    vertices: Vec<BatchVertex>,
}

impl Batch {
    pub fn clear(&mut self) {
        self.vertices.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.vertices.is_empty()
    }

    fn quad(&mut self, q: Quad, uv: [f32; 4], color: [f32; 4]) {
        let (x0, y0, x1, y1) = (q.x, q.y, q.x + q.w, q.y + q.h);
        let (u0, v0, u1, v1) = (uv[0], uv[1], uv[2], uv[3]);
        let v = |x, y, u, w| BatchVertex { pos: [x, y], uv: [u, w], color };
        self.vertices.extend_from_slice(&[v(x0, y0, u0, v0), v(x1, y0, u1, v0), v(x0, y1, u0, v1), v(x1, y0, u1, v0), v(x1, y1, u1, v1), v(x0, y1, u0, v1)]);
    }

    /// `color` is premultiplied RGBA.
    pub fn fill(&mut self, q: Quad, color: [f32; 4]) {
        self.quad(q, [-1.0; 4], color);
    }

    /// Draws the atlas area `src` (texels) at `q`, tinted with `color`.
    pub fn glyph(&mut self, atlas: &Texture, src: Quad, q: Quad, color: [f32; 4]) {
        let (w, h) = (atlas.width as f32, atlas.height as f32);
        self.quad(q, [src.x / w, src.y / h, (src.x + src.w) / w, (src.y + src.h) / h], color);
    }
}

struct Program {
    id: gl::GLuint,
    rect: gl::GLint,
    viewport: gl::GLint,
    color: gl::GLint,
    tex: gl::GLint,
    opaque: gl::GLint,
}

pub struct Renderer {
    _node: OwnedFd,
    gbm: *mut gbm::gbm_device,
    display: egl::EGLDisplay,
    context: egl::EGLContext,
    create_image: egl::PFNEGLCREATEIMAGEKHR,
    destroy_image: egl::PFNEGLDESTROYIMAGEKHR,
    image_target_rb: egl::PFNGLEGLIMAGETARGETRENDERBUFFERSTORAGEOES,
    image_target_tex: egl::PFNGLEGLIMAGETARGETTEXTURE2DOES,
    query_formats: Option<egl::PFNEGLQUERYDMABUFFORMATSEXT>,
    query_modifiers: Option<egl::PFNEGLQUERYDMABUFMODIFIERSEXT>,
    solid: Program,
    textured: Program,
    batch: Program,
    viewport: (i32, i32),
    /// The damaged area of this frame: drawing never leaves it.
    region: Option<Quad>,
}

const VERTEX: &str = "#version 100
attribute vec2 pos;
uniform vec4 rect;
uniform vec4 viewport;
varying vec2 uv;
void main() {
    uv = pos;
    vec2 p = (rect.xy + pos * rect.zw) / viewport.xy * 2.0 - 1.0;
    gl_Position = vec4(p, 0.0, 1.0);
}
";

const SOLID: &str = "#version 100
precision mediump float;
uniform vec4 color;
void main() { gl_FragColor = color; }
";

// Textures come from BGRA uploads; `opaque` forces alpha for XRGB buffers.
const TEXTURED: &str = "#version 100
precision mediump float;
uniform sampler2D tex;
uniform float opaque;
varying vec2 uv;
void main() {
    vec4 c = texture2D(tex, uv);
    gl_FragColor = vec4(c.rgb, max(c.a, opaque));
}
";

const BATCH_VERTEX: &str = "#version 100
attribute vec2 bpos;
attribute vec2 buv;
attribute vec4 bcolor;
uniform vec4 viewport;
varying vec2 uv;
varying vec4 color;
void main() {
    uv = buv;
    color = bcolor;
    gl_Position = vec4(bpos / viewport.xy * 2.0 - 1.0, 0.0, 1.0);
}
";

const BATCH: &str = "#version 100
precision mediump float;
uniform sampler2D tex;
varying vec2 uv;
varying vec4 color;
void main() {
    float a = uv.x < 0.0 ? 1.0 : texture2D(tex, uv).r;
    gl_FragColor = color * a;
}
";

const PACK_ALIGNMENT: gl::GLenum = 0x0D05;

const UNIT_SQUARE: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0];

unsafe fn compile(kind: gl::GLenum, src: &str) -> Result<gl::GLuint, Error> {
    let s = gl::glCreateShader(kind);
    let ptr = src.as_ptr().cast();
    let len = src.len() as gl::GLint;
    gl::glShaderSource(s, 1, &ptr, &len);
    gl::glCompileShader(s);
    let mut ok = 0;
    gl::glGetShaderiv(s, gl::COMPILE_STATUS, &mut ok);
    if ok == 0 {
        let mut log = [0u8; 1024];
        gl::glGetShaderInfoLog(s, log.len() as i32, null_mut(), log.as_mut_ptr().cast());
        return err(format!("shader compile failed: {}", CStr::from_bytes_until_nul(&log).unwrap_or_default().to_string_lossy()));
    }
    Ok(s)
}

unsafe fn program(fragment: &str) -> Result<Program, Error> {
    program_with(VERTEX, fragment)
}

unsafe fn program_with(vertex: &str, fragment: &str) -> Result<Program, Error> {
    let vs = compile(gl::VERTEX_SHADER, vertex)?;
    let fs = compile(gl::FRAGMENT_SHADER, fragment)?;
    let p = gl::glCreateProgram();
    gl::glAttachShader(p, vs);
    gl::glAttachShader(p, fs);
    gl::glBindAttribLocation(p, 0, c"pos".as_ptr());
    gl::glBindAttribLocation(p, 1, c"bpos".as_ptr());
    gl::glBindAttribLocation(p, 2, c"buv".as_ptr());
    gl::glBindAttribLocation(p, 3, c"bcolor".as_ptr());
    gl::glLinkProgram(p);
    gl::glDeleteShader(vs);
    gl::glDeleteShader(fs);
    let mut ok = 0;
    gl::glGetProgramiv(p, gl::LINK_STATUS, &mut ok);
    if ok == 0 {
        let mut log = [0u8; 1024];
        gl::glGetProgramInfoLog(p, log.len() as i32, null_mut(), log.as_mut_ptr().cast());
        return err(format!("program link failed: {}", CStr::from_bytes_until_nul(&log).unwrap_or_default().to_string_lossy()));
    }
    Ok(Program {
        id: p,
        rect: gl::glGetUniformLocation(p, c"rect".as_ptr()),
        viewport: gl::glGetUniformLocation(p, c"viewport".as_ptr()),
        color: gl::glGetUniformLocation(p, c"color".as_ptr()),
        tex: gl::glGetUniformLocation(p, c"tex".as_ptr()),
        opaque: gl::glGetUniformLocation(p, c"opaque".as_ptr()),
    })
}

impl Renderer {
    /// Opens `node` (a DRM render or primary node) and creates a surfaceless
    /// GLES 3 context on it.
    pub fn new(node: &str) -> Result<Renderer, Error> {
        let file = OpenOptions::new().read(true).write(true).open(node).map_err(|e| Error(format!("open {node}: {e}")))?;
        Renderer::from_fd(OwnedFd::from(file))
    }

    /// Uses an already open DRM device (e.g. the card fd from logind).
    pub fn from_fd(file: OwnedFd) -> Result<Renderer, Error> {
        unsafe {
            let dev = gbm::gbm_create_device(file.as_raw_fd());
            if dev.is_null() {
                return err("gbm_create_device failed");
            }
            let display = egl::eglGetPlatformDisplay(egl::PLATFORM_GBM_KHR, dev, std::ptr::null());
            if display.is_null() || egl::eglInitialize(display, null_mut(), null_mut()) != egl::TRUE {
                return err("EGL initialisation failed");
            }
            let exts = CStr::from_ptr(egl::eglQueryString(display, egl::EXTENSIONS)).to_string_lossy().into_owned();
            for needed in ["EGL_KHR_surfaceless_context", "EGL_KHR_no_config_context", "EGL_EXT_image_dma_buf_import"] {
                if !exts.split(' ').any(|e| e == needed) {
                    return err(format!("missing {needed}"));
                }
            }
            egl::eglBindAPI(egl::OPENGL_ES_API);
            let attribs = [egl::CONTEXT_MAJOR_VERSION, 3, egl::CONTEXT_MINOR_VERSION, 0, egl::NONE];
            let context = egl::eglCreateContext(display, null_mut(), null_mut(), attribs.as_ptr());
            if context.is_null() {
                return err(format!("eglCreateContext failed: {:#x}", egl::eglGetError()));
            }
            if egl::eglMakeCurrent(display, null_mut(), null_mut(), context) != egl::TRUE {
                return err("eglMakeCurrent failed");
            }
            let create_image = egl::proc_address(b"eglCreateImageKHR\0").ok_or(Error("no eglCreateImageKHR".into()))?;
            let destroy_image = egl::proc_address(b"eglDestroyImageKHR\0").ok_or(Error("no eglDestroyImageKHR".into()))?;
            let image_target_rb = egl::proc_address(b"glEGLImageTargetRenderbufferStorageOES\0")
                .ok_or(Error("no glEGLImageTargetRenderbufferStorageOES".into()))?;
            let image_target_tex = egl::proc_address(b"glEGLImageTargetTexture2DOES\0")
                .ok_or(Error("no glEGLImageTargetTexture2DOES".into()))?;
            let query_formats = egl::proc_address(b"eglQueryDmaBufFormatsEXT\0");
            let query_modifiers = egl::proc_address(b"eglQueryDmaBufModifiersEXT\0");
            let solid = program(SOLID)?;
            let textured = program(TEXTURED)?;
            let batch = program_with(BATCH_VERTEX, BATCH)?;
            gl::glEnableVertexAttribArray(0);
            gl::glVertexAttribPointer(0, 2, gl::FLOAT, 0, 0, UNIT_SQUARE.as_ptr().cast());
            gl::glEnable(gl::BLEND);
            gl::glBlendFunc(gl::ONE, gl::ONE_MINUS_SRC_ALPHA);
            Ok(Renderer {
                _node: file,
                gbm: dev,
                display,
                context,
                create_image,
                destroy_image,
                image_target_rb,
                image_target_tex,
                query_formats,
                query_modifiers,
                solid,
                textured,
                batch,
                viewport: (1, 1),
                region: None,
            })
        }
    }

    pub fn create_target(&mut self, width: i32, height: i32, flags: u32) -> Result<Target, Error> {
        let format = gbm::FORMAT_XRGB8888;
        unsafe {
            let bo = gbm::gbm_bo_create(self.gbm, width as u32, height as u32, format, gbm::BO_USE_RENDERING | flags);
            if bo.is_null() {
                return err("gbm_bo_create failed");
            }
            let mut t = Target {
                bo,
                image: null_mut(),
                rb: 0,
                fbo: 0,
                width,
                height,
                format,
                stride: gbm::gbm_bo_get_stride(bo),
                offset: gbm::gbm_bo_get_offset(bo, 0),
                modifier: gbm::gbm_bo_get_modifier(bo),
            };
            let fd = t.export_fd()?;
            let attribs = [
                egl::WIDTH,
                width,
                egl::HEIGHT,
                height,
                egl::LINUX_DRM_FOURCC_EXT,
                format as i32,
                egl::DMA_BUF_PLANE0_FD_EXT,
                fd.as_raw_fd(),
                egl::DMA_BUF_PLANE0_OFFSET_EXT,
                t.offset as i32,
                egl::DMA_BUF_PLANE0_PITCH_EXT,
                t.stride as i32,
                egl::DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                t.modifier as u32 as i32,
                egl::DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                (t.modifier >> 32) as u32 as i32,
                egl::NONE,
            ];
            t.image = (self.create_image)(self.display, null_mut(), egl::LINUX_DMA_BUF_EXT, null_mut(), attribs.as_ptr());
            if t.image.is_null() {
                let e = egl::eglGetError();
                self.destroy_target(t);
                return err(format!("eglCreateImageKHR failed: {e:#x}"));
            }
            gl::glGenRenderbuffers(1, &mut t.rb);
            gl::glBindRenderbuffer(gl::RENDERBUFFER, t.rb);
            (self.image_target_rb)(gl::RENDERBUFFER, t.image);
            gl::glGenFramebuffers(1, &mut t.fbo);
            gl::glBindFramebuffer(gl::FRAMEBUFFER, t.fbo);
            gl::glFramebufferRenderbuffer(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::RENDERBUFFER, t.rb);
            if gl::glCheckFramebufferStatus(gl::FRAMEBUFFER) != gl::FRAMEBUFFER_COMPLETE {
                self.destroy_target(t);
                return err("framebuffer incomplete");
            }
            Ok(t)
        }
    }

    pub fn destroy_target(&mut self, t: Target) {
        unsafe {
            if t.fbo != 0 {
                gl::glDeleteFramebuffers(1, &t.fbo);
            }
            if t.rb != 0 {
                gl::glDeleteRenderbuffers(1, &t.rb);
            }
            if !t.image.is_null() {
                (self.destroy_image)(self.display, t.image);
            }
            gbm::gbm_bo_destroy(t.bo);
        }
    }

    pub fn begin(&mut self, t: &Target, clear: [f32; 4]) {
        self.begin_region(t, clear, None);
    }

    /// Starts a frame that only redraws `region` (the rest of the buffer
    /// keeps what it showed); None redraws everything.
    pub fn begin_region(&mut self, t: &Target, clear: [f32; 4], region: Option<Quad>) {
        self.viewport = (t.width, t.height);
        self.region = region;
        if region.is_some() {
            unsafe {
                gl::glBindFramebuffer(gl::FRAMEBUFFER, t.fbo);
                gl::glViewport(0, 0, t.width, t.height);
                gl::glClearColor(clear[0], clear[1], clear[2], clear[3]);
            }
            self.clip(None);
            unsafe { gl::glClear(gl::COLOR_BUFFER_BIT) };
            return;
        }
        unsafe {
            gl::glBindFramebuffer(gl::FRAMEBUFFER, t.fbo);
            gl::glViewport(0, 0, t.width, t.height);
            gl::glDisable(gl::SCISSOR_TEST);
            gl::glClearColor(clear[0], clear[1], clear[2], clear[3]);
            gl::glClear(gl::COLOR_BUFFER_BIT);
        }
    }

    /// Copies `src` into `dst` scaled to fit, centred on black (mirroring).
    pub fn blit(&mut self, src: &Target, dst: &Target) {
        let scale = (dst.width as f32 / src.width as f32).min(dst.height as f32 / src.height as f32);
        let (w, h) = ((src.width as f32 * scale) as i32, (src.height as f32 * scale) as i32);
        let (x, y) = ((dst.width - w) / 2, (dst.height - h) / 2);
        self.begin(dst, [0.0, 0.0, 0.0, 1.0]);
        unsafe {
            gl::glBindFramebuffer(gl::READ_FRAMEBUFFER, src.fbo);
            gl::glBindFramebuffer(gl::DRAW_FRAMEBUFFER, dst.fbo);
            gl::glBlitFramebuffer(0, 0, src.width, src.height, x, y, x + w, y + h, gl::COLOR_BUFFER_BIT, gl::LINEAR as gl::GLenum);
            gl::glBindFramebuffer(gl::FRAMEBUFFER, dst.fbo);
        }
    }

    /// Waits for the GPU so the buffer can be handed out.
    pub fn finish(&mut self) {
        unsafe { gl::glFinish() };
    }

    /// The area this frame redraws; None: all of it.
    pub fn region(&self) -> Option<Quad> {
        self.region
    }

    /// Restricts drawing to `clip` until the next call; `None` removes it.
    pub fn clip(&mut self, clip: Option<Quad>) {
        let clip = match (clip, self.region) {
            (Some(a), Some(b)) => {
                let (x0, y0) = (a.x.max(b.x), a.y.max(b.y));
                let (x1, y1) = ((a.x + a.w).min(b.x + b.w), (a.y + a.h).min(b.y + b.h));
                Some(Quad { x: x0, y: y0, w: (x1 - x0).max(0.0), h: (y1 - y0).max(0.0) })
            }
            (a, b) => a.or(b),
        };
        unsafe {
            match clip {
                None => gl::glDisable(gl::SCISSOR_TEST),
                Some(q) => {
                    gl::glEnable(gl::SCISSOR_TEST);
                    gl::glScissor(q.x as i32, q.y as i32, q.w.max(0.0) as i32, q.h.max(0.0) as i32);
                }
            }
        }
    }

    fn use_program(&self, p: &Program, q: Quad) {
        unsafe {
            gl::glUseProgram(p.id);
            gl::glUniform4f(p.rect, q.x, q.y, q.w, q.h);
            gl::glUniform4f(p.viewport, self.viewport.0 as f32, self.viewport.1 as f32, 0.0, 0.0);
        }
    }

    /// `color` is premultiplied RGBA.
    pub fn fill(&mut self, q: Quad, color: [f32; 4]) {
        self.use_program(&self.solid, q);
        unsafe {
            gl::glUniform4f(self.solid.color, color[0], color[1], color[2], color[3]);
            gl::glDrawArrays(gl::TRIANGLE_STRIP, 0, 4);
        }
    }

    pub fn draw(&mut self, tex: &Texture, q: Quad, opaque: bool) {
        self.use_program(&self.textured, q);
        unsafe {
            gl::glActiveTexture(gl::TEXTURE0);
            gl::glBindTexture(gl::TEXTURE_2D, tex.id);
            gl::glUniform1i(self.textured.tex, 0);
            gl::glUniform1f(self.textured.opaque, if opaque { 1.0 } else { 0.0 });
            gl::glDrawArrays(gl::TRIANGLE_STRIP, 0, 4);
        }
    }

    /// Uploads 32-bit little-endian ARGB/XRGB pixels (`stride` in bytes).
    /// Reuses `tex` when given.
    pub fn upload(&mut self, tex: Option<Texture>, width: i32, height: i32, stride: i32, pixels: &[u8]) -> Texture {
        debug_assert!(pixels.len() >= (stride * (height - 1) + width * 4) as usize);
        unsafe {
            let id = match tex {
                Some(t) => t.id,
                None => {
                    let mut id = 0;
                    gl::glGenTextures(1, &mut id);
                    gl::glBindTexture(gl::TEXTURE_2D, id);
                    gl::glTexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR);
                    gl::glTexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR);
                    gl::glTexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE);
                    gl::glTexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE);
                    id
                }
            };
            gl::glBindTexture(gl::TEXTURE_2D, id);
            gl::glPixelStorei(gl::UNPACK_ALIGNMENT, 4);
            gl::glPixelStorei(gl::UNPACK_ROW_LENGTH, stride / 4);
            gl::glTexImage2D(
                gl::TEXTURE_2D,
                0,
                gl::BGRA_EXT as i32,
                width,
                height,
                0,
                gl::BGRA_EXT,
                gl::UNSIGNED_BYTE,
                pixels.as_ptr().cast(),
            );
            gl::glPixelStorei(gl::UNPACK_ROW_LENGTH, 0);
            Texture { id, width, height }
        }
    }

    /// Replaces the given areas `(x, y, w, h)` of a texture made by
    /// `upload` with the same pixels' layout.
    pub fn update(&mut self, tex: &Texture, areas: &[(i32, i32, i32, i32)], stride: i32, pixels: &[u8]) {
        unsafe {
            gl::glBindTexture(gl::TEXTURE_2D, tex.id);
            gl::glPixelStorei(gl::UNPACK_ALIGNMENT, 4);
            gl::glPixelStorei(gl::UNPACK_ROW_LENGTH, stride / 4);
            for &(x, y, w, h) in areas {
                debug_assert!(x >= 0 && y >= 0 && x + w <= tex.width && y + h <= tex.height);
                gl::glPixelStorei(gl::UNPACK_SKIP_PIXELS, x);
                gl::glPixelStorei(gl::UNPACK_SKIP_ROWS, y);
                gl::glTexSubImage2D(gl::TEXTURE_2D, 0, x, y, w, h, gl::BGRA_EXT, gl::UNSIGNED_BYTE, pixels.as_ptr().cast());
            }
            gl::glPixelStorei(gl::UNPACK_SKIP_PIXELS, 0);
            gl::glPixelStorei(gl::UNPACK_SKIP_ROWS, 0);
            gl::glPixelStorei(gl::UNPACK_ROW_LENGTH, 0);
        }
    }

    /// Uploads RGBA pixels with mipmaps, for images drawn scaled down.
    pub fn upload_mipmapped(&mut self, width: i32, height: i32, rgba: &[u8]) -> Texture {
        debug_assert_eq!(rgba.len(), (width * height * 4) as usize);
        unsafe {
            let mut id = 0;
            gl::glGenTextures(1, &mut id);
            gl::glBindTexture(gl::TEXTURE_2D, id);
            gl::glTexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR_MIPMAP_LINEAR);
            gl::glTexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR);
            gl::glTexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE);
            gl::glTexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE);
            gl::glPixelStorei(gl::UNPACK_ALIGNMENT, 4);
            gl::glTexImage2D(gl::TEXTURE_2D, 0, gl::RGBA as i32, width, height, 0, gl::RGBA, gl::UNSIGNED_BYTE, rgba.as_ptr().cast());
            gl::glGenerateMipmap(gl::TEXTURE_2D);
            Texture { id, width, height }
        }
    }

    /// Draws `tex` cropped to `src` (texture pixels) into `dst`.
    pub fn draw_cropped(&mut self, tex: &Texture, src: Quad, dst: Quad) {
        // The unit square is mapped through `rect`; express the crop by
        // drawing the whole texture larger and clipping to `dst`.
        let sx = dst.w / src.w;
        let sy = dst.h / src.h;
        let full = Quad { x: dst.x - src.x * sx, y: dst.y - src.y * sy, w: tex.width as f32 * sx, h: tex.height as f32 * sy };
        self.clip(Some(dst));
        self.draw(tex, full, true);
        self.clip(None);
    }

    /// Formats and modifiers that client dmabufs may use.
    /// The device number of the DRM node rendered on.
    pub fn device(&self) -> u64 {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        unsafe { libc::fstat(self._node.as_raw_fd(), &mut st) };
        st.st_rdev
    }

    pub fn dmabuf_formats(&self) -> Vec<(u32, u64)> {
        let (Some(qf), Some(qm)) = (self.query_formats, self.query_modifiers) else {
            return vec![(gbm::FORMAT_XRGB8888, MOD_INVALID), (gbm::FORMAT_ARGB8888, MOD_INVALID)];
        };
        let mut out = Vec::new();
        unsafe {
            let mut n = 0;
            qf(self.display, 0, null_mut(), &mut n);
            let mut formats = vec![0; n as usize];
            qf(self.display, n, formats.as_mut_ptr(), &mut n);
            for f in formats {
                let mut m = 0;
                qm(self.display, f, 0, null_mut(), null_mut(), &mut m);
                let mut mods = vec![0u64; m as usize];
                let mut ext = vec![0u32; m as usize];
                qm(self.display, f, m, mods.as_mut_ptr(), ext.as_mut_ptr(), &mut m);
                for (md, e) in mods.iter().zip(&ext) {
                    if *e == 0 {
                        out.push((f as u32, *md));
                    }
                }
                // Implicit modifier, for clients that do not pass one.
                out.push((f as u32, MOD_INVALID));
            }
        }
        out
    }

    pub fn import_dmabuf(&mut self, width: i32, height: i32, format: u32, modifier: u64, planes: &[Plane]) -> Result<Texture, Error> {
        if planes.is_empty() || planes.len() > 4 {
            return err("bad plane count");
        }
        let mut attribs = vec![egl::WIDTH, width, egl::HEIGHT, height, egl::LINUX_DRM_FOURCC_EXT, format as i32];
        for (p, a) in planes.iter().zip(egl::DMA_BUF_PLANE_ATTRIBS) {
            attribs.extend([a[0], p.fd.as_raw_fd(), a[1], p.offset as i32, a[2], p.stride as i32]);
            if modifier != MOD_INVALID {
                attribs.extend([a[3], modifier as u32 as i32, a[4], (modifier >> 32) as u32 as i32]);
            }
        }
        attribs.push(egl::NONE);
        unsafe {
            let image = (self.create_image)(self.display, null_mut(), egl::LINUX_DMA_BUF_EXT, null_mut(), attribs.as_ptr());
            if image.is_null() {
                return err(format!("dmabuf import failed: {:#x}", egl::eglGetError()));
            }
            let mut id = 0;
            gl::glGenTextures(1, &mut id);
            gl::glBindTexture(gl::TEXTURE_2D, id);
            gl::glTexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR);
            gl::glTexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR);
            gl::glTexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE);
            gl::glTexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE);
            (self.image_target_tex)(gl::TEXTURE_2D, image);
            // The texture keeps the buffer alive on its own.
            (self.destroy_image)(self.display, image);
            Ok(Texture { id, width, height })
        }
    }

    /// Reads a texture back as RGBA (for the live upgrade).
    pub fn read_texture(&mut self, tex: &Texture) -> Vec<u8> {
        let mut out = vec![0u8; (tex.width * tex.height * 4) as usize];
        unsafe {
            let mut fbo = 0;
            gl::glGenFramebuffers(1, &mut fbo);
            gl::glBindFramebuffer(gl::FRAMEBUFFER, fbo);
            gl::glFramebufferTexture2D(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_2D, tex.id, 0);
            if gl::glCheckFramebufferStatus(gl::FRAMEBUFFER) == gl::FRAMEBUFFER_COMPLETE {
                gl::glPixelStorei(PACK_ALIGNMENT, 4);
                gl::glReadPixels(0, 0, tex.width, tex.height, gl::RGBA, gl::UNSIGNED_BYTE, out.as_mut_ptr().cast());
            }
            gl::glBindFramebuffer(gl::FRAMEBUFFER, 0);
            gl::glDeleteFramebuffers(1, &fbo);
        }
        out
    }

    /// RGBA pixels of a render target, rows top to bottom (targets are
    /// drawn upside down for scanout).
    pub fn read_target(&mut self, t: &Target) -> Vec<u8> {
        let mut out = vec![0u8; (t.width * t.height * 4) as usize];
        unsafe {
            gl::glBindFramebuffer(gl::FRAMEBUFFER, t.fbo);
            gl::glPixelStorei(PACK_ALIGNMENT, 4);
            gl::glReadPixels(0, 0, t.width, t.height, gl::RGBA, gl::UNSIGNED_BYTE, out.as_mut_ptr().cast());
            gl::glBindFramebuffer(gl::FRAMEBUFFER, 0);
        }
        out
    }

    /// Uploads RGBA pixels without mipmaps.
    pub fn upload_rgba(&mut self, width: i32, height: i32, rgba: &[u8]) -> Texture {
        unsafe {
            let mut id = 0;
            gl::glGenTextures(1, &mut id);
            gl::glBindTexture(gl::TEXTURE_2D, id);
            gl::glTexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR);
            gl::glTexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR);
            gl::glTexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE);
            gl::glTexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE);
            gl::glPixelStorei(gl::UNPACK_ALIGNMENT, 4);
            gl::glTexImage2D(gl::TEXTURE_2D, 0, gl::RGBA as i32, width, height, 0, gl::RGBA, gl::UNSIGNED_BYTE, rgba.as_ptr().cast());
            Texture { id, width, height }
        }
    }

    /// An alpha-only texture for glyphs, cleared.
    pub fn create_atlas(&mut self, width: i32, height: i32) -> Texture {
        let zero = vec![0u8; (width * height) as usize];
        unsafe {
            let mut id = 0;
            gl::glGenTextures(1, &mut id);
            gl::glBindTexture(gl::TEXTURE_2D, id);
            for (p, v) in [(gl::TEXTURE_MIN_FILTER, gl::NEAREST), (gl::TEXTURE_MAG_FILTER, gl::NEAREST), (gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE), (gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE)] {
                gl::glTexParameteri(gl::TEXTURE_2D, p, v);
            }
            gl::glPixelStorei(gl::UNPACK_ALIGNMENT, 1);
            gl::glTexImage2D(gl::TEXTURE_2D, 0, gl::R8 as i32, width, height, 0, gl::RED, gl::UNSIGNED_BYTE, zero.as_ptr().cast());
            gl::glPixelStorei(gl::UNPACK_ALIGNMENT, 4);
            Texture { id, width, height }
        }
    }

    /// Writes coverage values (one byte per texel) into an atlas.
    pub fn atlas_put(&mut self, atlas: &Texture, x: i32, y: i32, w: i32, h: i32, alpha: &[u8]) {
        if w == 0 || h == 0 {
            return;
        }
        unsafe {
            gl::glBindTexture(gl::TEXTURE_2D, atlas.id);
            gl::glPixelStorei(gl::UNPACK_ALIGNMENT, 1);
            gl::glTexSubImage2D(gl::TEXTURE_2D, 0, x, y, w, h, gl::RED, gl::UNSIGNED_BYTE, alpha.as_ptr().cast());
            gl::glPixelStorei(gl::UNPACK_ALIGNMENT, 4);
        }
    }

    /// Draws a batch, its glyphs from `atlas`.
    pub fn draw_batch(&mut self, atlas: &Texture, b: &Batch) {
        if b.vertices.is_empty() {
            return;
        }
        let stride = std::mem::size_of::<BatchVertex>() as i32;
        let base = b.vertices.as_ptr() as *const u8;
        unsafe {
            gl::glUseProgram(self.batch.id);
            gl::glUniform4f(self.batch.viewport, self.viewport.0 as f32, self.viewport.1 as f32, 0.0, 0.0);
            gl::glActiveTexture(gl::TEXTURE0);
            gl::glBindTexture(gl::TEXTURE_2D, atlas.id);
            gl::glUniform1i(self.batch.tex, 0);
            for (loc, size, offset) in [(1, 2, 0), (2, 2, 8), (3, 4, 16)] {
                gl::glEnableVertexAttribArray(loc);
                gl::glVertexAttribPointer(loc, size, gl::FLOAT, 0, stride, base.add(offset).cast());
            }
            gl::glDrawArrays(gl::TRIANGLES, 0, b.vertices.len() as i32);
            for loc in 1..=3 {
                gl::glDisableVertexAttribArray(loc);
            }
        }
    }

    pub fn destroy_texture(&mut self, tex: Texture) {
        unsafe { gl::glDeleteTextures(1, &tex.id) };
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            egl::eglMakeCurrent(self.display, null_mut(), null_mut(), null_mut());
            egl::eglDestroyContext(self.display, self.context);
            egl::eglTerminate(self.display);
            gbm::gbm_device_destroy(self.gbm);
        }
    }
}

state::persist_struct!(Plane { fd, offset, stride });
