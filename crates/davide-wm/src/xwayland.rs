//! Runs the Xwayland server: picks a display number, listens on its X11
//! sockets and starts `Xwayland` with them, with a private Wayland
//! connection and the socket of our window manager.

use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

pub struct Xwayland {
    pub display: u32,
    pid: i32,
    /// Readable when Xwayland exits.
    pub pidfd: OwnedFd,
    /// Until Xwayland is ready: the pipe it writes the display number to,
    /// and our end of the window manager connection.
    pub pending: Option<(OwnedFd, OwnedFd)>,
    started: Instant,
    lock: PathBuf,
    socket: PathBuf,
}

/// What `spawn` hands back besides the process: our end of its Wayland
/// connection, to add as a client.
pub struct Spawned {
    pub xwayland: Xwayland,
    pub wayland: OwnedFd,
}

fn cloexec(fd: i32, on: bool) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        libc::fcntl(fd, libc::F_SETFD, if on { flags | libc::FD_CLOEXEC } else { flags & !libc::FD_CLOEXEC });
    }
}

fn pid_alive(pid: i32) -> bool {
    pid > 0 && (unsafe { libc::kill(pid, 0) } == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// Claims display `n`: its lock file, then its two sockets.
fn claim(n: u32) -> io::Result<(PathBuf, PathBuf, UnixListener, UnixListener)> {
    let lock = PathBuf::from(format!("/tmp/.X{n}-lock"));
    let create = || OpenOptions::new().write(true).create_new(true).mode(0o444).open(&lock);
    let mut file = match create() {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let owner = std::fs::read_to_string(&lock).ok().and_then(|t| t.trim().parse::<i32>().ok());
            if owner.is_some_and(pid_alive) {
                return Err(e);
            }
            std::fs::remove_file(&lock)?;
            create()?
        }
        Err(e) => return Err(e),
    };
    file.write_all(format!("{:>10}\n", std::process::id()).as_bytes())?;
    let socket = PathBuf::from(format!("/tmp/.X11-unix/X{n}"));
    let listeners = (|| {
        let _ = std::fs::create_dir("/tmp/.X11-unix");
        let abstract_ = UnixListener::bind_addr(&SocketAddr::from_abstract_name(socket.as_os_str().as_encoded_bytes())?)?;
        let _ = std::fs::remove_file(&socket);
        Ok::<_, io::Error>((UnixListener::bind(&socket)?, abstract_))
    })();
    match listeners {
        Ok((path, abstract_)) => Ok((lock, socket, path, abstract_)),
        Err(e) => {
            let _ = std::fs::remove_file(&lock);
            Err(e)
        }
    }
}

pub fn spawn() -> io::Result<Spawned> {
    let (display, lock, socket, path, abstract_) = (0..32)
        .find_map(|n| claim(n).ok().map(|(l, s, p, a)| (n, l, s, p, a)))
        .ok_or_else(|| io::Error::other("no free X display"))?;
    let (wayland, wayland_theirs) = UnixStream::pair()?;
    let (wm, wm_theirs) = UnixStream::pair()?;
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (ready, ready_theirs) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let inherited = [path.as_raw_fd(), abstract_.as_raw_fd(), ready_theirs.as_raw_fd(), wm_theirs.as_raw_fd(), wayland_theirs.as_raw_fd()];
    let [l1, l2, rd, wmfd, wl] = inherited;
    let mut cmd = Command::new("Xwayland");
    cmd.arg(format!(":{display}"))
        .args(["-rootless", "-nolisten", "tcp"])
        .args(["-listenfd", &l1.to_string(), "-listenfd", &l2.to_string()])
        .args(["-displayfd", &rd.to_string(), "-wm", &wmfd.to_string()])
        .env("WAYLAND_SOCKET", wl.to_string())
        .env_remove("DISPLAY");
    unsafe {
        cmd.pre_exec(move || {
            crate::event_loop::reset_signals_in_child();
            for fd in inherited {
                cloexec(fd, false);
            }
            Ok(())
        });
    }
    let child = cmd.spawn().inspect_err(|_| {
        let _ = std::fs::remove_file(&lock);
        let _ = std::fs::remove_file(&socket);
    })?;
    let pid = child.id() as i32;
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    if pidfd < 0 {
        return Err(io::Error::last_os_error());
    }
    log!("Xwayland on :{display} (pid {pid})");
    for fd in [ready.as_raw_fd(), wm.as_raw_fd()] {
        cloexec(fd, true);
    }
    wayland.set_nonblocking(true)?;
    let xwayland = Xwayland {
        display,
        pid,
        pidfd: unsafe { OwnedFd::from_raw_fd(pidfd) },
        pending: Some((ready, OwnedFd::from(wm))),
        started: Instant::now(),
        lock,
        socket,
    };
    Ok(Spawned { xwayland, wayland: OwnedFd::from(wayland) })
}

impl Xwayland {
    /// Xwayland wrote its display number: it accepts clients. Returns the
    /// window manager socket.
    pub fn ready(&mut self) -> Option<OwnedFd> {
        let (ready, wm) = self.pending.take()?;
        let mut line = String::new();
        let _ = std::fs::File::from(ready).read_to_string(&mut line);
        Some(wm)
    }

    /// Worth starting again after it exited: it did not die right away.
    pub fn lived(&self) -> bool {
        self.started.elapsed() > Duration::from_secs(10)
    }
}

impl Drop for Xwayland {
    fn drop(&mut self) {
        if pid_alive(self.pid) {
            unsafe { libc::kill(self.pid, libc::SIGTERM) };
        }
        let _ = std::fs::remove_file(&self.lock);
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl state::Persist for Xwayland {
    fn save(&self, e: &mut state::Enc) {
        (self.display, self.pid).save(e);
        self.pidfd.save(e);
        self.pending.save(e);
        (self.started.elapsed().as_millis() as u64).save(e);
        (self.lock.to_string_lossy().into_owned(), self.socket.to_string_lossy().into_owned()).save(e);
    }
    fn load(d: &mut state::Dec) -> state::Result<Self> {
        let (display, pid) = state::Persist::load(d)?;
        let pidfd = state::Persist::load(d)?;
        let pending = state::Persist::load(d)?;
        let age = Duration::from_millis(u64::load(d)?);
        let (lock, socket): (String, String) = state::Persist::load(d)?;
        let started = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
        Ok(Xwayland { display, pid, pidfd, pending, started, lock: lock.into(), socket: socket.into() })
    }
}

