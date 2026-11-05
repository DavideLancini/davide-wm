//! Password checks through PAM, for the lock screen.
//!
//! `authenticate` blocks (pam_unix waits after a wrong password): it runs on
//! the worker.

use std::ffi::{c_char, c_int, c_void, CStr, CString};

#[repr(C)]
struct PamMessage {
    style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    retcode: c_int,
}

#[repr(C)]
struct PamConv {
    conv: unsafe extern "C" fn(c_int, *mut *const PamMessage, *mut *mut PamResponse, *mut c_void) -> c_int,
    appdata: *mut c_void,
}

#[link(name = "pam")]
extern "C" {
    fn pam_start(service: *const c_char, user: *const c_char, conv: *const PamConv, handle: *mut *mut c_void) -> c_int;
    fn pam_authenticate(handle: *mut c_void, flags: c_int) -> c_int;
    fn pam_end(handle: *mut c_void, status: c_int) -> c_int;
}

const PAM_SUCCESS: c_int = 0;
const PAM_BUF_ERR: c_int = 5;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;

/// Answers every prompt with the password; PAM frees the answers.
unsafe extern "C" fn converse(n: c_int, msgs: *mut *const PamMessage, resp: *mut *mut PamResponse, data: *mut c_void) -> c_int {
    let password = &*(data as *const CString);
    let out = libc::calloc(n.max(1) as usize, std::mem::size_of::<PamResponse>()) as *mut PamResponse;
    if out.is_null() {
        return PAM_BUF_ERR;
    }
    for i in 0..n as usize {
        let m = &**msgs.add(i);
        if m.style == PAM_PROMPT_ECHO_OFF || m.style == PAM_PROMPT_ECHO_ON {
            (*out.add(i)).resp = libc::strdup(password.as_ptr());
        }
    }
    *resp = out;
    PAM_SUCCESS
}

/// The user running the compositor.
pub fn user() -> String {
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return std::env::var("USER").unwrap_or_default();
        }
        CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned()
    }
}

/// Checks `password` for the current user with the `config::LOCK_PAM_SERVICE` stack.
pub fn authenticate(password: String) -> bool {
    let mut bytes = password.into_bytes();
    let pw = CString::new(bytes.clone()).unwrap_or_default();
    bytes.fill(0);
    let service = CString::new(config::LOCK_PAM_SERVICE).unwrap_or_default();
    let user = CString::new(user()).unwrap_or_default();
    let conv = PamConv { conv: converse, appdata: &pw as *const CString as *mut c_void };
    let ok = unsafe {
        let mut handle = std::ptr::null_mut();
        if pam_start(service.as_ptr(), user.as_ptr(), &conv, &mut handle) != PAM_SUCCESS {
            return false;
        }
        let r = pam_authenticate(handle, 0);
        pam_end(handle, r);
        r == PAM_SUCCESS
    };
    let mut bytes = pw.into_bytes();
    bytes.fill(0);
    ok
}
