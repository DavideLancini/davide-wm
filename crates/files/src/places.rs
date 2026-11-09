//! Sidebar places: home, XDG user dirs, GTK bookmarks, trash and root.

use std::fs;
use std::path::PathBuf;

pub struct Place {
    pub name: String,
    pub path: PathBuf,
    pub icon: char,
}

const USER_DIRS: [(&str, char); 6] = [
    ("DESKTOP", '\u{f108}'),
    ("DOCUMENTS", '\u{f02d}'),
    ("DOWNLOAD", '\u{f019}'),
    ("MUSIC", '\u{f001}'),
    ("PICTURES", '\u{f03e}'),
    ("VIDEOS", '\u{f03d}'),
];

/// `XDG_<KEY>_DIR` from user-dirs.dirs text, `$HOME` expanded.
fn user_dir(text: &str, key: &str, home: &std::path::Path) -> Option<PathBuf> {
    let var = format!("XDG_{key}_DIR");
    text.lines().find_map(|l| {
        let (k, v) = l.trim().split_once('=')?;
        if k.trim() != var {
            return None;
        }
        let v = v.trim().trim_matches('"');
        match v.strip_prefix("$HOME") {
            Some(rest) => Some(home.join(rest.trim_start_matches('/'))),
            None => v.starts_with('/').then(|| PathBuf::from(v)),
        }
    })
}

fn last_name(p: &std::path::Path) -> String {
    p.file_name().map_or_else(|| p.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned())
}

pub fn places() -> Vec<Place> {
    let home = crate::home();
    let mut out = vec![Place { name: "Home".into(), path: home.clone(), icon: '\u{f015}' }];
    let config = crate::config_home();
    let dirs = fs::read_to_string(config.join("user-dirs.dirs")).unwrap_or_default();
    for (key, icon) in USER_DIRS {
        if let Some(p) = user_dir(&dirs, key, &home).filter(|p| *p != home && p.is_dir()) {
            if !out.iter().any(|x| x.path == p) {
                out.push(Place { name: last_name(&p), path: p, icon });
            }
        }
    }
    for line in fs::read_to_string(config.join("gtk-3.0/bookmarks")).unwrap_or_default().lines() {
        let (uri, label) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
        let Some(p) = crate::uri::from_uri(uri).filter(|p| p.is_dir()) else { continue };
        let name = if label.trim().is_empty() { last_name(&p) } else { label.trim().to_string() };
        out.push(Place { name, path: p, icon: crate::mime::ICON_FOLDER });
    }
    out.push(Place { name: "Cestino".into(), path: crate::ops::trash_dir(), icon: '\u{f1f8}' });
    out.push(Place { name: "Sistema".into(), path: "/".into(), icon: '\u{f0a0}' });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn user_dirs_file() {
        let text = "# x\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_MUSIC_DIR=\"$HOME/\"\nXDG_VIDEOS_DIR=\"/srv/v\"\n";
        let home = Path::new("/home/u");
        assert_eq!(user_dir(text, "DESKTOP", home), Some(PathBuf::from("/home/u/Desktop")));
        assert_eq!(user_dir(text, "MUSIC", home).as_deref(), Some(home));
        assert_eq!(user_dir(text, "VIDEOS", home), Some(PathBuf::from("/srv/v")));
        assert_eq!(user_dir(text, "PICTURES", home), None);
    }

    #[test]
    fn always_has_home_trash_root() {
        let p = places();
        assert_eq!(p[0].name, "Home");
        assert_eq!(p[p.len() - 2].name, "Cestino");
        assert_eq!(p[p.len() - 1].path, Path::new("/"));
        for x in &p {
            eprintln!("{} {:?}", x.name, x.path);
        }
    }
}
