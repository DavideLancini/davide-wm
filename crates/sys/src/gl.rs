use std::ffi::{c_char, c_void};

pub type GLenum = u32;
pub type GLuint = u32;
pub type GLint = i32;
pub type GLsizei = i32;
pub type GLfloat = f32;
pub type GLbitfield = u32;

pub const TRIANGLES: GLenum = 0x0004;
pub const TRIANGLE_STRIP: GLenum = 0x0005;
pub const RED: GLenum = 0x1903;
pub const R8: GLenum = 0x8229;
pub const NEAREST: GLint = 0x2600;
pub const ONE: GLenum = 1;
pub const ONE_MINUS_SRC_ALPHA: GLenum = 0x0303;
pub const BLEND: GLenum = 0x0BE2;
pub const SCISSOR_TEST: GLenum = 0x0C11;
pub const UNPACK_ALIGNMENT: GLenum = 0x0CF5;
pub const UNPACK_ROW_LENGTH: GLenum = 0x0CF2;
pub const UNPACK_SKIP_ROWS: GLenum = 0x0CF3;
pub const UNPACK_SKIP_PIXELS: GLenum = 0x0CF4;
pub const TEXTURE_2D: GLenum = 0x0DE1;
pub const UNSIGNED_BYTE: GLenum = 0x1401;
pub const FLOAT: GLenum = 0x1406;
pub const LINEAR: GLint = 0x2601;
pub const LINEAR_MIPMAP_LINEAR: GLint = 0x2703;
pub const RGBA: GLenum = 0x1908;
pub const TEXTURE_MAG_FILTER: GLenum = 0x2800;
pub const TEXTURE_MIN_FILTER: GLenum = 0x2801;
pub const TEXTURE_WRAP_S: GLenum = 0x2802;
pub const TEXTURE_WRAP_T: GLenum = 0x2803;
pub const CLAMP_TO_EDGE: GLint = 0x812F;
pub const TEXTURE0: GLenum = 0x84C0;
pub const FRAGMENT_SHADER: GLenum = 0x8B30;
pub const VERTEX_SHADER: GLenum = 0x8B31;
pub const COMPILE_STATUS: GLenum = 0x8B81;
pub const LINK_STATUS: GLenum = 0x8B82;
pub const FRAMEBUFFER: GLenum = 0x8D40;
pub const READ_FRAMEBUFFER: GLenum = 0x8CA8;
pub const DRAW_FRAMEBUFFER: GLenum = 0x8CA9;
pub const RENDERBUFFER: GLenum = 0x8D41;
pub const COLOR_ATTACHMENT0: GLenum = 0x8CE0;
pub const FRAMEBUFFER_COMPLETE: GLenum = 0x8CD5;
pub const COLOR_BUFFER_BIT: GLbitfield = 0x4000;
pub const BGRA_EXT: GLenum = 0x80E1;

#[link(name = "GLESv2")]
extern "C" {
    pub fn glGetString(name: GLenum) -> *const c_char;
    pub fn glGetError() -> GLenum;
    pub fn glFinish();
    pub fn glViewport(x: GLint, y: GLint, w: GLsizei, h: GLsizei);
    pub fn glScissor(x: GLint, y: GLint, w: GLsizei, h: GLsizei);
    pub fn glEnable(cap: GLenum);
    pub fn glDisable(cap: GLenum);
    pub fn glBlendFunc(s: GLenum, d: GLenum);
    pub fn glClearColor(r: GLfloat, g: GLfloat, b: GLfloat, a: GLfloat);
    pub fn glClear(mask: GLbitfield);

    pub fn glGenFramebuffers(n: GLsizei, ids: *mut GLuint);
    pub fn glDeleteFramebuffers(n: GLsizei, ids: *const GLuint);
    pub fn glBindFramebuffer(target: GLenum, id: GLuint);
    pub fn glFramebufferRenderbuffer(target: GLenum, attachment: GLenum, rbtarget: GLenum, rb: GLuint);
    pub fn glFramebufferTexture2D(target: GLenum, attachment: GLenum, textarget: GLenum, tex: GLuint, level: GLint);
    pub fn glReadPixels(x: GLint, y: GLint, w: GLsizei, h: GLsizei, format: GLenum, kind: GLenum, data: *mut c_void);
    #[allow(clippy::too_many_arguments)]
    pub fn glBlitFramebuffer(sx0: GLint, sy0: GLint, sx1: GLint, sy1: GLint, dx0: GLint, dy0: GLint, dx1: GLint, dy1: GLint, mask: GLbitfield, filter: GLenum);
    pub fn glCheckFramebufferStatus(target: GLenum) -> GLenum;
    pub fn glGenRenderbuffers(n: GLsizei, ids: *mut GLuint);
    pub fn glDeleteRenderbuffers(n: GLsizei, ids: *const GLuint);
    pub fn glBindRenderbuffer(target: GLenum, id: GLuint);

    pub fn glCreateShader(kind: GLenum) -> GLuint;
    pub fn glDeleteShader(id: GLuint);
    pub fn glShaderSource(id: GLuint, count: GLsizei, src: *const *const c_char, len: *const GLint);
    pub fn glCompileShader(id: GLuint);
    pub fn glGetShaderiv(id: GLuint, pname: GLenum, out: *mut GLint);
    pub fn glGetShaderInfoLog(id: GLuint, max: GLsizei, len: *mut GLsizei, log: *mut c_char);
    pub fn glCreateProgram() -> GLuint;
    pub fn glAttachShader(p: GLuint, s: GLuint);
    pub fn glBindAttribLocation(p: GLuint, index: GLuint, name: *const c_char);
    pub fn glLinkProgram(p: GLuint);
    pub fn glGetProgramiv(p: GLuint, pname: GLenum, out: *mut GLint);
    pub fn glGetProgramInfoLog(p: GLuint, max: GLsizei, len: *mut GLsizei, log: *mut c_char);
    pub fn glUseProgram(p: GLuint);
    pub fn glGetUniformLocation(p: GLuint, name: *const c_char) -> GLint;
    pub fn glUniform1i(loc: GLint, v: GLint);
    pub fn glUniform1f(loc: GLint, v: GLfloat);
    pub fn glUniform4f(loc: GLint, a: GLfloat, b: GLfloat, c: GLfloat, d: GLfloat);
    pub fn glVertexAttribPointer(index: GLuint, size: GLint, kind: GLenum, norm: u8, stride: GLsizei, ptr: *const c_void);
    pub fn glEnableVertexAttribArray(index: GLuint);
    pub fn glDisableVertexAttribArray(index: GLuint);
    pub fn glDrawArrays(mode: GLenum, first: GLint, count: GLsizei);

    pub fn glGenTextures(n: GLsizei, ids: *mut GLuint);
    pub fn glDeleteTextures(n: GLsizei, ids: *const GLuint);
    pub fn glBindTexture(target: GLenum, id: GLuint);
    pub fn glActiveTexture(unit: GLenum);
    pub fn glTexParameteri(target: GLenum, pname: GLenum, v: GLint);
    pub fn glPixelStorei(pname: GLenum, v: GLint);
    pub fn glGenerateMipmap(target: GLenum);
    pub fn glTexImage2D(
        target: GLenum,
        level: GLint,
        internal: GLint,
        w: GLsizei,
        h: GLsizei,
        border: GLint,
        format: GLenum,
        kind: GLenum,
        data: *const c_void,
    );
    pub fn glTexSubImage2D(
        target: GLenum,
        level: GLint,
        x: GLint,
        y: GLint,
        w: GLsizei,
        h: GLsizei,
        format: GLenum,
        kind: GLenum,
        data: *const c_void,
    );
}
