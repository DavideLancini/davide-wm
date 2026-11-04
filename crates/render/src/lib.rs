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
    viewport: (i32, i32),
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
    let vs = compile(gl::VERTEX_SHADER, VERTEX)?;
    let fs = compile(gl::FRAGMENT_SHADER, fragment)?;
    let p = gl::glCreateProgram();
    gl::glAttachShader(p, vs);
    gl::glAttachShader(p, fs);
    gl::glBindAttribLocation(p, 0, c"pos".as_ptr());
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
                viewport: (1, 1),
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
        self.viewport = (t.width, t.height);
        unsafe {
            gl::glBindFramebuffer(gl::FRAMEBUFFER, t.fbo);
            gl::glViewport(0, 0, t.width, t.height);
            gl::glDisable(gl::SCISSOR_TEST);
            gl::glClearColor(clear[0], clear[1], clear[2], clear[3]);
            gl::glClear(gl::COLOR_BUFFER_BIT);
        }
    }

    /// Waits for the GPU so the buffer can be handed out.
    pub fn finish(&mut self) {
        unsafe { gl::glFinish() };
    }

    /// Restricts drawing to `clip` until the next call; `None` removes it.
    pub fn clip(&mut self, clip: Option<Quad>) {
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
