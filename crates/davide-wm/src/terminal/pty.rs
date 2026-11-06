//! Pseudo-terminals: the shell runs on the slave side, in its own session,
//! and the master side is read by the compositor.

use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

pub struct Pty {
    pub master: OwnedFd,
    pub pid: i32,
}

fn check(r: i32) -> io::Result<i32> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// Starts the user's shell, or a command through it, on a new
/// pseudo-terminal of `cols`×`rows`.
pub fn spawn(cols: u16, rows: u16, cell: (i32, i32), o: &super::Open) -> io::Result<Pty> {
    let master = unsafe { check(libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_NONBLOCK))? };
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let mut name = [0 as libc::c_char; 128];
    unsafe {
        check(libc::grantpt(master.as_raw_fd()))?;
        check(libc::unlockpt(master.as_raw_fd()))?;
        check(libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()))?;
    }
    let path = unsafe { CStr::from_ptr(name.as_ptr()) }.to_owned();
    let slave = unsafe { check(libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC))? };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    resize(master.as_raw_fd(), cols, rows, cell)?;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let mut cmd = Command::new(&shell);
    match (&o.cmd, o.hold) {
        (Some(c), true) => cmd.arg("-c").arg(format!("{c}\nexec \"$SHELL\"")),
        (Some(c), false) => cmd.arg("-c").arg(c),
        (None, _) => &mut cmd,
    };
    // A login-style argv[0] would read the profile again; foot does not either.
    cmd.stdin(Stdio::from(slave.try_clone()?)).stdout(Stdio::from(slave.try_clone()?)).stderr(Stdio::from(slave));
    for (k, v) in config::TERM_ENV {
        cmd.env(k, v);
    }
    cmd.env_remove("WAYLAND_SOCKET");
    if let Some(dir) = o.cwd.as_deref().filter(|d| std::path::Path::new(d).is_dir()) {
        cmd.current_dir(dir);
    } else if let Some(home) = std::env::var_os("HOME") {
        cmd.current_dir(home);
    }
    unsafe {
        cmd.pre_exec(|| {
            crate::event_loop::reset_signals_in_child();
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let pid = cmd.spawn()?.id() as i32;
    Ok(Pty { master, pid })
}

pub fn resize(fd: RawFd, cols: u16, rows: u16, cell: (i32, i32)) -> io::Result<()> {
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: (cols as i32 * cell.0) as u16, ws_ypixel: (rows as i32 * cell.1) as u16 };
    unsafe { check(libc::ioctl(fd, libc::TIOCSWINSZ, &ws)) }.map(|_| ())
}

/// The working directory of the shell's foreground process, for a new
/// terminal opened next to it.
pub fn cwd(pid: i32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| p.to_string_lossy().into_owned())
}

/// Ends the session: the shell and what runs in it get SIGHUP.
pub fn hang_up(pid: i32) {
    unsafe { libc::kill(-pid, libc::SIGHUP) };
}
