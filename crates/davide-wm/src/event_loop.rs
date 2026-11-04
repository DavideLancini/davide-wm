//! epoll and signalfd wrappers.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

pub struct Epoll {
    fd: OwnedFd,
}

pub const READABLE: u32 = libc::EPOLLIN as u32;
const WRITABLE: u32 = libc::EPOLLOUT as u32;
pub const HANGUP: u32 = (libc::EPOLLHUP | libc::EPOLLERR) as u32;

fn check(r: i32) -> io::Result<i32> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

impl Epoll {
    pub fn new() -> io::Result<Epoll> {
        let fd = check(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })?;
        Ok(Epoll { fd: unsafe { OwnedFd::from_raw_fd(fd) } })
    }

    fn ctl(&self, op: i32, fd: RawFd, token: u64, writable: bool) -> io::Result<()> {
        let events = READABLE | if writable { WRITABLE } else { 0 };
        let mut ev = libc::epoll_event { events, u64: token };
        check(unsafe { libc::epoll_ctl(self.fd.as_raw_fd(), op, fd, &mut ev) }).map(|_| ())
    }

    pub fn add(&self, fd: RawFd, token: u64) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_ADD, fd, token, false)
    }

    pub fn modify(&self, fd: RawFd, token: u64, writable: bool) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_MOD, fd, token, writable)
    }

    pub fn delete(&self, fd: RawFd) {
        unsafe { libc::epoll_ctl(self.fd.as_raw_fd(), libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut()) };
    }

    /// Waits for events; `timeout_ms < 0` waits forever.
    pub fn wait(&self, timeout_ms: i32) -> io::Result<Vec<(u64, u32)>> {
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 64];
        let n = unsafe { libc::epoll_wait(self.fd.as_raw_fd(), events.as_mut_ptr(), events.len() as i32, timeout_ms) };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted { Ok(Vec::new()) } else { Err(e) };
        }
        Ok(events[..n as usize].iter().map(|e| (e.u64, e.events)).collect())
    }
}

/// Blocks SIGINT and SIGTERM and returns a signalfd that reports them.
pub fn signal_fd() -> io::Result<OwnedFd> {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        check(libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()))?;
        let fd = check(libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK))?;
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::signal(libc::SIGCHLD, libc::SIG_IGN);
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

/// Undoes `signal_fd` in a child process before exec.
pub fn reset_signals_in_child() {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        libc::signal(libc::SIGCHLD, libc::SIG_DFL);
    }
}
