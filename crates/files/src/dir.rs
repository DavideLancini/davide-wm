//! Directory listing, sorting and human-readable sizes and dates.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Dir,
    File,
    Other,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: OsString,
    /// With symlinks followed; a broken link is `Other`.
    pub kind: Kind,
    pub link: bool,
    pub size: u64,
    /// Unix seconds.
    pub mtime: i64,
    pub mode: u32,
    pub hidden: bool,
}

impl Entry {
    pub fn display(&self) -> Cow<'_, str> {
        self.name.to_string_lossy()
    }

    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }
}

fn kind_of(m: &fs::Metadata) -> Kind {
    if m.is_dir() {
        Kind::Dir
    } else if m.is_file() {
        Kind::File
    } else {
        Kind::Other
    }
}

/// All entries of `path`, hidden ones included, unsorted. Entries that vanish
/// while listing are skipped.
pub fn list(path: &Path) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for e in fs::read_dir(path)? {
        let Ok(e) = e else { continue };
        let Ok(lmeta) = e.metadata() else { continue };
        let link = lmeta.file_type().is_symlink();
        let target = if link { fs::metadata(e.path()).ok() } else { None };
        let (kind, m) = match &target {
            Some(t) => (kind_of(t), t),
            None if link => (Kind::Other, &lmeta),
            None => (kind_of(&lmeta), &lmeta),
        };
        let name = e.file_name();
        let hidden = name.as_encoded_bytes().first() == Some(&b'.');
        out.push(Entry { kind, link, size: m.size(), mtime: m.mtime(), mode: m.mode(), hidden, name });
    }
    Ok(out)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Sort {
    #[default]
    Name,
    Size,
    Modified,
    /// By extension.
    Type,
}

fn extension(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[i + 1..],
        _ => "",
    }
}

/// Sorts with directories always first; `reverse` flips the order within
/// each group. Ties fall back to the natural name order.
pub fn sort(entries: &mut [Entry], by: Sort, reverse: bool) {
    entries.sort_by(|a, b| {
        let (na, nb) = (a.display(), b.display());
        let o = match by {
            Sort::Name => Ordering::Equal,
            Sort::Size => a.size.cmp(&b.size),
            Sort::Modified => a.mtime.cmp(&b.mtime),
            Sort::Type => natural_cmp(extension(&na), extension(&nb)),
        }
        .then_with(|| natural_cmp(&na, &nb));
        b.is_dir().cmp(&a.is_dir()).then(if reverse { o.reverse() } else { o })
    });
}

fn digits(it: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut s = String::new();
    while let Some(c) = it.next_if(char::is_ascii_digit) {
        s.push(c);
    }
    s
}

/// Case-insensitive comparison with digit runs compared by value, so
/// "file2" < "file10".
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        let o = match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let (n, m) = (digits(&mut x), digits(&mut y));
                let (n, m) = (n.trim_start_matches('0'), m.trim_start_matches('0'));
                n.len().cmp(&m.len()).then_with(|| n.cmp(m))
            }
            (Some(c), Some(d)) => {
                x.next();
                y.next();
                c.to_lowercase().cmp(d.to_lowercase())
            }
        };
        if o != Ordering::Equal {
            return o;
        }
    }
}

/// "512 B", "1.2 KB", "34 MB": powers of 1000, one decimal under 10.
pub fn human_size(bytes: u64) -> String {
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    for unit in ["KB", "MB", "GB", "TB", "PB"] {
        v /= 1000.0;
        if v < 9.95 {
            return format!("{v:.1} {unit}");
        }
        if v < 999.5 || unit == "PB" {
            return format!("{v:.0} {unit}");
        }
    }
    unreachable!()
}

const MONTHS: [&str; 12] = ["gen", "feb", "mar", "apr", "mag", "giu", "lug", "ago", "set", "ott", "nov", "dic"];

/// "14:03" today, "ieri 14:03", "12 nov" this year, else "12 nov 2024".
pub fn human_time(mtime: i64, now: i64) -> String {
    let t = crate::local_time(mtime);
    let same_day = |n: &libc::tm| n.tm_year == t.tm_year && n.tm_yday == t.tm_yday;
    let today = crate::local_time(now);
    let month = MONTHS[t.tm_mon.clamp(0, 11) as usize];
    if same_day(&today) {
        format!("{:02}:{:02}", t.tm_hour, t.tm_min)
    } else if same_day(&crate::local_time(now - 86400)) {
        format!("ieri {:02}:{:02}", t.tm_hour, t.tm_min)
    } else if today.tm_year == t.tm_year {
        format!("{} {month}", t.tm_mday)
    } else {
        format!("{} {month} {}", t.tm_mday, t.tm_year + 1900)
    }
}

/// "12 nov 2024, 14:03".
pub fn full_time(mtime: i64) -> String {
    let t = crate::local_time(mtime);
    format!("{} {} {}, {:02}:{:02}", t.tm_mday, MONTHS[t.tm_mon.clamp(0, 11) as usize], t.tm_year + 1900, t.tm_hour, t.tm_min)
}

/// "rwxr-xr-x".
pub fn permissions(mode: u32) -> String {
    (0..9).map(|k| if mode & (0o400 >> k) != 0 { ['r', 'w', 'x'][k % 3] } else { '-' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural() {
        assert_eq!(natural_cmp("file2", "file10"), Ordering::Less);
        assert_eq!(natural_cmp("File2", "file1"), Ordering::Greater);
        assert_eq!(natural_cmp("a", "B"), Ordering::Less);
        assert_eq!(natural_cmp("x007", "x7"), natural_cmp("x007", "x7"));
        assert_ne!(natural_cmp("x007", "x7"), Ordering::Equal);
        assert_eq!(natural_cmp("abc", "abcd"), Ordering::Less);
        let mut v = ["b10", "b9", "a", "B1"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["a", "B1", "b9", "b10"]);
    }

    #[test]
    fn perms() {
        assert_eq!(permissions(0o100755), "rwxr-xr-x");
        assert_eq!(permissions(0o640), "rw-r-----");
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1234), "1.2 KB");
        assert_eq!(human_size(34_000_000), "34 MB");
        assert_eq!(human_size(1_500_000_000), "1.5 GB");
        assert_eq!(human_size(999_999), "1.0 MB");
    }

    #[test]
    fn times() {
        let now = crate::now();
        let t = crate::local_time(now);
        assert_eq!(human_time(now, now), format!("{:02}:{:02}", t.tm_hour, t.tm_min));
        assert!(human_time(now - 86400, now).starts_with("ieri "));
        assert!(human_time(now - 3 * 365 * 86400, now).ends_with(&(t.tm_year + 1900 - 3).to_string()));
        assert_eq!(human_time(2 * 86400 + 43200, now), "3 gen 1970");
    }

    #[test]
    fn list_and_sort() {
        let d = std::env::temp_dir().join(format!("files-dir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("zdir")).unwrap();
        fs::write(d.join("b10.txt"), "123456789012345").unwrap();
        fs::write(d.join("b9.md"), "1").unwrap();
        fs::write(d.join(".hidden"), "").unwrap();
        std::os::unix::fs::symlink("missing", d.join("broken")).unwrap();
        std::os::unix::fs::symlink("zdir", d.join("adir")).unwrap();
        let mut v = list(&d).unwrap();
        assert_eq!(v.len(), 6);
        sort(&mut v, Sort::Name, false);
        let names: Vec<_> = v.iter().map(|e| e.display().into_owned()).collect();
        assert_eq!(names, ["adir", "zdir", ".hidden", "b9.md", "b10.txt", "broken"]);
        assert!(v[0].link && v[0].is_dir());
        let broken = v.iter().find(|e| e.name == "broken").unwrap();
        assert_eq!((broken.kind, broken.link), (Kind::Other, true));
        assert!(v.iter().find(|e| e.name == ".hidden").unwrap().hidden);
        sort(&mut v, Sort::Size, true);
        assert!(v[0].is_dir() && v[1].is_dir());
        assert_eq!(v[2].name, "b10.txt");
        sort(&mut v, Sort::Type, false);
        let files: Vec<_> = v[2..].iter().map(|e| e.display().into_owned()).collect();
        assert_eq!(files, [".hidden", "broken", "b9.md", "b10.txt"]);
        fs::remove_dir_all(&d).unwrap();
    }
}
