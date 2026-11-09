//! File manager back end, free of UI: directory listing and sorting, copy,
//! move, trash and delete with progress, URIs and clipboard formats, MIME
//! types and desktop applications, thumbnails (PNG/JPEG) and UDisks2
//! volumes. Everything here may block and is meant for worker threads.

pub mod dir;
mod inflate;
pub mod md5;
pub mod mime;
pub mod ops;
pub mod places;
pub mod png;
pub mod thumb;
pub mod udisks;
pub mod uri;

use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// An absolute path from the XDG variable `var`, else `$HOME/default`.
fn xdg_home(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(default))
}

/// The absolute entries of the colon-separated XDG variable `var`, else `default`.
fn xdg_dirs(var: &str, default: &str) -> Vec<PathBuf> {
    let v = std::env::var(var).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.into());
    v.split(':').map(PathBuf::from).filter(|p| p.is_absolute()).collect()
}

fn data_home() -> PathBuf {
    xdg_home("XDG_DATA_HOME", ".local/share")
}

fn config_home() -> PathBuf {
    xdg_home("XDG_CONFIG_HOME", ".config")
}

/// Broken-down local time of `t`.
fn local_time(t: i64) -> libc::tm {
    // SAFETY: tm is plain data; localtime_r only writes it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = t as libc::time_t;
    unsafe { libc::localtime_r(&t, &mut tm) };
    tm
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}
