//! Background jobs: anything slow runs on a thread and comes back to the
//! event loop through a channel and an eventfd, so the main loop never blocks.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

pub enum Done {
    Wallpaper(jpeg::Image),
    /// A clipboard selection read from its owner.
    Clip { generation: u32, clip: crate::server::clipboard::Clip },
    Saved,
    /// The kernel log check after lighting an exclusive monitor.
    LinkCheck { name: String, failed: bool },
    /// A Wayland selection read for an X client.
    XSelection { request: crate::server::xwm::selection::Request, data: Result<Vec<u8>, String> },
}

pub struct Worker {
    efd: Arc<OwnedFd>,
    tx: Sender<Result<Done, String>>,
    rx: Receiver<Result<Done, String>>,
}

impl Worker {
    pub fn new() -> io::Result<Worker> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let (tx, rx) = channel();
        Ok(Worker { efd: Arc::new(unsafe { OwnedFd::from_raw_fd(fd) }), tx, rx })
    }

    pub fn raw_fd(&self) -> RawFd {
        self.efd.as_raw_fd()
    }

    pub fn run(&self, job: impl FnOnce() -> Result<Done, String> + Send + 'static) {
        let (tx, efd) = (self.tx.clone(), self.efd.clone());
        std::thread::spawn(move || {
            let _ = tx.send(job());
            let one = 1u64;
            unsafe { libc::write(efd.as_raw_fd(), (&one as *const u64).cast(), 8) };
        });
    }

    /// Finished jobs; call when the eventfd is readable.
    pub fn results(&self) -> Vec<Result<Done, String>> {
        let mut n = 0u64;
        unsafe { libc::read(self.efd.as_raw_fd(), (&mut n as *mut u64).cast(), 8) };
        self.rx.try_iter().collect()
    }
}
