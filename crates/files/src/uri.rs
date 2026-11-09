//! `file://` URIs, `text/uri-list` and GNOME's copied-files clipboard format.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// Percent-encodes everything but unreserved characters and '/'.
pub(crate) fn escape(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

/// Decodes %XX escapes; malformed ones are kept literally.
pub(crate) fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
        match (b[i], hex) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

pub fn to_uri(path: &Path) -> String {
    format!("file://{}", escape(path.as_os_str().as_bytes()))
}

/// Accepts `file:///x` and `file://localhost/x`.
pub fn from_uri(uri: &str) -> Option<PathBuf> {
    let scheme = uri.get(..7)?;
    if !scheme.eq_ignore_ascii_case("file://") {
        return None;
    }
    let rest = &uri[7..];
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    rest.starts_with('/').then(|| PathBuf::from(OsString::from_vec(unescape(rest))))
}

/// `text/uri-list` with CRLF line ends.
pub fn uri_list(paths: &[PathBuf]) -> String {
    paths.iter().map(|p| to_uri(p) + "\r\n").collect()
}

/// Paths of a `text/uri-list`, also accepting plain absolute paths.
pub fn parse_uri_list(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| if l.starts_with('/') { Some(PathBuf::from(l)) } else { from_uri(l) })
        .collect()
}

/// `x-special/gnome-copied-files`: "copy" or "cut", then one URI per line.
pub fn gnome_copied_files(cut: bool, paths: &[PathBuf]) -> String {
    let mut s = String::from(if cut { "cut" } else { "copy" });
    for p in paths {
        s.push('\n');
        s.push_str(&to_uri(p));
    }
    s
}

/// Also accepts Nautilus' text/plain variant with its
/// "x-special/nautilus-clipboard" header line.
pub fn parse_gnome_copied_files(text: &str) -> Option<(bool, Vec<PathBuf>)> {
    let mut lines = text.lines().map(str::trim).skip_while(|l| l.is_empty() || *l == "x-special/nautilus-clipboard");
    let cut = match lines.next()? {
        "copy" => false,
        "cut" => true,
        _ => return None,
    };
    let rest: Vec<&str> = lines.collect();
    Some((cut, parse_uri_list(&rest.join("\n"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let p = PathBuf::from(OsString::from_vec(b"/tmp/a b/\xff%#\xc3\xa8.txt".to_vec()));
        let u = to_uri(&p);
        assert_eq!(u, "file:///tmp/a%20b/%FF%25%23%C3%A8.txt");
        assert_eq!(from_uri(&u), Some(p));
        assert_eq!(from_uri("file://localhost/x%2"), Some(PathBuf::from("/x%2")));
        assert_eq!(from_uri("FILE:///y"), Some(PathBuf::from("/y")));
        assert_eq!(from_uri("http://x/y"), None);
        assert_eq!(from_uri("file://host/y"), None);
        assert_eq!(from_uri("fil"), None);
        assert_eq!(from_uri("file://é"), None);
    }

    #[test]
    fn lists() {
        let v = vec![PathBuf::from("/a"), PathBuf::from("/b c")];
        assert_eq!(uri_list(&v), "file:///a\r\nfile:///b%20c\r\n");
        assert_eq!(parse_uri_list(&uri_list(&v)), v);
        assert_eq!(parse_uri_list("# c\n\nhttp://x\n/plain path\nfile:///q\n"), [PathBuf::from("/plain path"), PathBuf::from("/q")]);
        let g = gnome_copied_files(true, &v);
        assert_eq!(g, "cut\nfile:///a\nfile:///b%20c");
        assert_eq!(parse_gnome_copied_files(&g), Some((true, v.clone())));
        assert_eq!(parse_gnome_copied_files("x-special/nautilus-clipboard\ncopy\nfile:///a\n"), Some((false, vec![PathBuf::from("/a")])));
        assert_eq!(parse_gnome_copied_files("hello"), None);
        assert_eq!(parse_gnome_copied_files(""), None);
    }
}
