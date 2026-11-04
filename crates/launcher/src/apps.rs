//! Discover launchable applications from XDG .desktop files, plus bookmark
//! links from ~/2Bookmarks (also .desktop files, Type=Link).

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

pub struct App {
    pub name: String,
    /// Shell command to run (Type=Application entries). Empty for bookmarks
    /// and the synthetic exit entry.
    pub exec: String,
    pub terminal: bool,
    /// Set for Type=Link entries (bookmarks): the URL to open in a new
    /// browser window instead of running `exec`.
    pub url: Option<String>,
    /// If true, selecting this entry closes the launcher instead of launching anything.
    /// Exists so the launcher is always closable even if ESC/pointer both fail.
    pub is_exit: bool,
}

const EXIT_ENTRY_NAME: &str = "exit";

fn exit_entry() -> App {
    App {
        name: EXIT_ENTRY_NAME.to_string(),
        exec: String::new(),
        terminal: false,
        url: None,
        is_exit: true,
    }
}

fn xdg_dirs() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/share/applications"),
        dirs_local_share(),
    ]
}

fn dirs_local_share() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".local/share/applications")
}

fn bookmarks_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("2Bookmarks")
}

/// Strips desktop-entry field codes (%f, %F, %u, %U, %i, %c, %k) from an Exec value.
fn clean_exec(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            if let Some(&next) = chars.peek() {
                if matches!(next, 'f' | 'F' | 'u' | 'U' | 'i' | 'c' | 'k') {
                    chars.next();
                    continue;
                }
                if next == '%' {
                    out.push('%');
                    chars.next();
                    continue;
                }
            }
        }
        out.push(c);
    }
    out.trim().to_string()
}

fn parse_desktop_file(contents: &str) -> Option<App> {
    let mut in_desktop_entry = false;
    let mut entry_type = String::from("Application");
    let mut name = None;
    let mut exec = None;
    let mut url = None;
    let mut no_display = false;
    let mut hidden = false;
    let mut terminal = false;

    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_desktop_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_desktop_entry {
            continue;
        }
        if let Some(v) = line.strip_prefix("Type=") {
            entry_type = v.to_string();
        } else if let Some(v) = line.strip_prefix("Name=") {
            if name.is_none() {
                name = Some(v.to_string());
            }
        } else if let Some(v) = line.strip_prefix("Exec=") {
            exec = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("URL=") {
            url = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("NoDisplay=") {
            no_display = v.eq_ignore_ascii_case("true");
        } else if let Some(v) = line.strip_prefix("Hidden=") {
            hidden = v.eq_ignore_ascii_case("true");
        } else if let Some(v) = line.strip_prefix("Terminal=") {
            terminal = v.eq_ignore_ascii_case("true");
        }
    }

    if no_display || hidden {
        return None;
    }
    let name = name?;

    if entry_type == "Link" {
        let url = url?;
        if url.is_empty() {
            return None;
        }
        return Some(App { name, exec: String::new(), terminal: false, url: Some(url), is_exit: false });
    }

    let exec = clean_exec(&exec?);
    if exec.is_empty() {
        return None;
    }
    Some(App { name, exec, terminal, url: None, is_exit: false })
}

fn scan_desktop_dir(dir: &PathBuf, by_id: &mut HashMap<String, App>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
            continue;
        }
        let Some(id) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
            continue;
        };
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        // A user override that parses to None (e.g. NoDisplay=true) must
        // still remove whatever the system .desktop with the same filename
        // already inserted — otherwise "hide this app" silently does
        // nothing when a visible system entry was scanned first.
        match parse_desktop_file(&contents) {
            Some(app) => {
                by_id.insert(id, app);
            }
            None => {
                by_id.remove(&id);
            }
        }
    }
}

/// Scans XDG application directories (user entries in ~/.local/share
/// override system ones in /usr/share with the same filename) plus
/// ~/2Bookmarks for Type=Link bookmark entries (a separate namespace, so a
/// bookmark never collides with an app's .desktop filename).
pub fn scan() -> Vec<App> {
    let mut by_id: HashMap<String, App> = HashMap::new();
    for dir in xdg_dirs() {
        scan_desktop_dir(&dir, &mut by_id);
    }
    let mut apps: Vec<App> = by_id.into_values().collect();

    let mut bookmarks_by_id: HashMap<String, App> = HashMap::new();
    scan_desktop_dir(&bookmarks_dir(), &mut bookmarks_by_id);
    apps.extend(bookmarks_by_id.into_values());

    apps.sort_by_key(|a| a.name.to_lowercase());
    apps.push(exit_entry());
    apps
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Regression test: a user-level NoDisplay=true override must hide an
    /// app that a same-filename system .desktop would otherwise show, even
    /// though the system dir is scanned first. Caught in practice hiding
    /// "About Xfce" — scan_desktop_dir used to only `insert` on a Some from
    /// parse_desktop_file, never removing what an earlier dir had already
    /// inserted for the same id, so a filtered-out override was silently a
    /// no-op whenever the system entry was visible.
    #[test]
    fn override_with_no_display_hides_system_entry() {
        let path = std::env::temp_dir().join(format!("davide-wm-launcher-test-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        struct Dir(std::path::PathBuf);
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = Dir(path);
        let mut f = std::fs::File::create(dir.0.as_path().join("example.desktop")).unwrap();
        writeln!(f, "[Desktop Entry]\nType=Application\nName=Example\nExec=example").unwrap();
        drop(f);

        let mut by_id = HashMap::new();
        scan_desktop_dir(&dir.0.as_path().to_path_buf(), &mut by_id);
        assert!(by_id.contains_key("example.desktop"));

        let mut f = std::fs::File::create(dir.0.as_path().join("example.desktop")).unwrap();
        writeln!(f, "[Desktop Entry]\nType=Application\nName=Example\nExec=example\nNoDisplay=true").unwrap();
        drop(f);

        scan_desktop_dir(&dir.0.as_path().to_path_buf(), &mut by_id);
        assert!(!by_id.contains_key("example.desktop"), "NoDisplay override should remove the earlier entry");
    }
}
