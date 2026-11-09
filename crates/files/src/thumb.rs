//! Thumbnails per the freedesktop thumbnail spec, in
//! `$XDG_CACHE_HOME/thumbnails`; PNG and JPEG are rendered here.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::png::{self, Image};

pub const NORMAL: u32 = 128;
pub const LARGE: u32 = 256;

const DIRS: [(&str, u32); 4] = [("normal", NORMAL), ("large", LARGE), ("x-large", 512), ("xx-large", 1024)];
const MAX_FILE: u64 = 64 << 20;

fn cache_dir() -> PathBuf {
    crate::xdg_home("XDG_CACHE_HOME", ".cache").join("thumbnails")
}

fn uri_of(path: &Path) -> String {
    crate::uri::to_uri(&std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()))
}

/// Shrinks to fit `size`×`size` by area averaging (alpha-weighted); never
/// enlarges.
fn fit(img: Image, size: u32) -> Image {
    let (w, h) = (img.width as u64, img.height as u64);
    let size = size.max(1) as u64;
    if w <= size && h <= size {
        return img;
    }
    let (tw, th) = if w >= h { (size, ((h * size + w / 2) / w).max(1)) } else { (((w * size + h / 2) / h).max(1), size) };
    let mut out = vec![0u8; (tw * th * 4) as usize];
    for ty in 0..th {
        let (y0, y1) = (ty * h / th, ((ty + 1) * h / th).max(ty * h / th + 1));
        for tx in 0..tw {
            let (x0, x1) = (tx * w / tw, ((tx + 1) * w / tw).max(tx * w / tw + 1));
            let mut acc = [0u64; 4];
            for y in y0..y1 {
                for px in img.rgba[((y * w + x0) * 4) as usize..((y * w + x1) * 4) as usize].as_chunks::<4>().0 {
                    let a = px[3] as u64;
                    acc[0] += px[0] as u64 * a;
                    acc[1] += px[1] as u64 * a;
                    acc[2] += px[2] as u64 * a;
                    acc[3] += a;
                }
            }
            let n = (y1 - y0) * (x1 - x0);
            let o = &mut out[((ty * tw + tx) * 4) as usize..][..4];
            for c in 0..3 {
                o[c] = (acc[c] + acc[3] / 2).checked_div(acc[3]).unwrap_or(0) as u8;
            }
            o[3] = ((acc[3] + n / 2) / n) as u8;
        }
    }
    Image { width: tw as u32, height: th as u32, rgba: out }
}

/// A cached thumbnail at least `size` big and fresh for `mtime`, shrunk to
/// fit `size`.
pub fn cached(path: &Path, mtime: i64, size: u32) -> Option<Image> {
    let name = format!("{}.png", crate::md5::hex(uri_of(path).as_bytes()));
    let base = cache_dir();
    DIRS.iter().filter(|(_, px)| *px >= size.min(1024)).find_map(|(dir, _)| {
        let file = base.join(dir).join(&name);
        if fs::metadata(&file).ok()?.len() > MAX_FILE {
            return None;
        }
        let data = fs::read(file).ok()?;
        if png::read_text(&data, "Thumb::MTime")?.trim().parse::<i64>().ok()? != mtime {
            return None;
        }
        Some(fit(png::decode(&data).ok()?, size))
    })
}

fn render(path: &Path, mime: &str, size: u32) -> Option<Image> {
    if !matches!(mime, "image/png" | "image/jpeg") || fs::metadata(path).ok()?.len() > MAX_FILE {
        return None;
    }
    let data = fs::read(path).ok()?;
    if mime == "image/png" {
        return Some(fit(png::decode(&data).ok()?, size));
    }
    let mut img = jpeg::decode(&data).ok()?;
    let s = size.max(1) as usize;
    while img.width.max(img.height) >= 2 * s && img.width.min(img.height) >= 2 {
        img = jpeg::halve(&img);
    }
    let img = Image { width: img.width as u32, height: img.height as u32, rgba: img.pixels };
    Some(fit(img, size))
}

fn save(dir: &Path, name: &str, img: &Image, uri: &str, mtime: i64) -> std::io::Result<()> {
    static N: AtomicU64 = AtomicU64::new(0);
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let data = png::encode(img.width, img.height, &img.rgba, &[("Thumb::URI", uri), ("Thumb::MTime", &mtime.to_string()), ("Software", "davide-wm")]);
    let tmp = dir.join(format!(".{name}.{}.{}", std::process::id(), N.fetch_add(1, Relaxed)));
    let r = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp).and_then(|mut f| f.write_all(&data)).and_then(|()| fs::rename(&tmp, dir.join(name)));
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

/// Renders a PNG or JPEG thumbnail at the cache size class covering
/// `size`, stores it and returns it shrunk to fit `size`.
pub fn make(path: &Path, mtime: i64, mime: &str, size: u32) -> Option<Image> {
    let &(dir, px) = DIRS.iter().find(|(_, px)| *px >= size).unwrap_or(&DIRS[3]);
    let img = render(path, mime, px)?;
    let uri = uri_of(path);
    let _ = save(&cache_dir().join(dir), &format!("{}.png", crate::md5::hex(uri.as_bytes())), &img, &uri, mtime);
    Some(fit(img, size))
}

pub fn get(path: &Path, mtime: i64, mime: &str, size: u32) -> Option<Image> {
    cached(path, mtime, size).or_else(|| make(path, mtime, mime, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect() {
        let img = Image { width: 300, height: 100, rgba: [10, 20, 30, 255].repeat(300 * 100) };
        let t = fit(img, 128);
        assert_eq!((t.width, t.height), (128, 43));
        assert!(t.rgba.as_chunks::<4>().0.iter().all(|p| *p == [10, 20, 30, 255]));
        let small = fit(Image { width: 3, height: 5, rgba: vec![1; 60] }, 128);
        assert_eq!((small.width, small.height), (3, 5));
        let mut rgba = vec![0u8; 8];
        rgba[..4].copy_from_slice(&[200, 100, 0, 255]);
        let half = fit(Image { width: 2, height: 1, rgba }, 1);
        assert_eq!(half.rgba, [200, 100, 0, 128]);
    }

    /// Writes to the real thumbnail cache under an isolated XDG_CACHE_HOME;
    /// env changes need a single-threaded run.
    #[test]
    #[ignore]
    fn make_and_cache() {
        let d = std::env::temp_dir().join(format!("files-thumb-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        std::env::set_var("XDG_CACHE_HOME", d.join("cache"));
        let src = d.join("big.png");
        let img: Vec<u8> = (0..600 * 400).flat_map(|i| [(i % 600 * 255 / 600) as u8, 0, 0, 255]).collect();
        fs::write(&src, png::encode(600, 400, &img, &[])).unwrap();
        assert!(cached(&src, 42, 128).is_none());
        let t = get(&src, 42, "image/png", 100).unwrap();
        assert_eq!((t.width, t.height), (100, 66));
        let c = cached(&src, 42, 128).unwrap();
        assert_eq!((c.width, c.height), (128, 85));
        assert!(cached(&src, 43, 128).is_none());
        assert!(cached(&src, 42, 256).is_none());
        let data = fs::read(d.join("cache/thumbnails/normal").join(format!("{}.png", crate::md5::hex(crate::uri::to_uri(&src).as_bytes())))).unwrap();
        assert_eq!(png::read_text(&data, "Thumb::URI"), Some(crate::uri::to_uri(&src)));
        assert!(make(&src, 42, "text/plain", 128).is_none());
        if let Some(jpg) = std::env::var_os("THUMB_JPEG").map(PathBuf::from) {
            eprintln!("{jpg:?}: {:?}", make(&jpg, 1, "image/jpeg", 256).map(|i| (i.width, i.height)));
        }
        fs::remove_dir_all(&d).unwrap();
    }
}
