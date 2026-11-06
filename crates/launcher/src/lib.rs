//! The launcher, ported from super (`~/0Projects/super`): a box in the
//! middle of the screen listing desktop entries and `~/2Bookmarks`, filtered
//! as you type; a query starting with `=` is evaluated as arithmetic.
//!
//! The same box doubles as a picker over a list the compositor supplies
//! (clipboard history, notifications), and `card` draws notification popups.
//!
//! The compositor owns the window and the keyboard: it feeds keys in and
//! draws the pixels this crate renders.

mod apps;
mod calc;
mod emoji;
mod units;
pub mod card;
mod draw;
mod settings;

pub use settings::{WINDOW_HEIGHT as HEIGHT, WINDOW_WIDTH as WIDTH};

pub enum Outcome {
    Nothing,
    Redraw,
    Close,
    /// Close and run this shell command.
    Launch(String),
    /// Close and run this shell command in a terminal; with `hold` the
    /// terminal stays open on a shell afterwards.
    Terminal { cmd: String, hold: bool },
    /// Picker: this item (index in the list given to `picker`) was chosen.
    Pick(usize),
    /// Picker: delete this item; it is already gone from the list.
    Delete(usize),
    /// Close and put this text on the clipboard.
    Copy(String),
}

/// A row of a special mode and what Enter does with it.
enum Action {
    Launch(String),
    Terminal(String),
    Copy(String),
}

pub mod keys {
    pub const ESCAPE: u32 = 0xff1b;
    pub const RETURN: u32 = 0xff0d;
    pub const KP_ENTER: u32 = 0xff8d;
    pub const BACKSPACE: u32 = 0xff08;
    pub const UP: u32 = 0xff52;
    pub const DOWN: u32 = 0xff54;
    pub const DELETE: u32 = 0xffff;
}

pub struct Launcher {
    font: fontdue::Font,
    apps: Vec<apps::App>,
    /// Row labels: application names, or the picker's items.
    labels: Vec<String>,
    /// Picker mode: original index of each label.
    picks: Option<Vec<usize>>,
    query: String,
    filtered: Vec<usize>,
    selected: usize,
    /// Rows of the special modes ("=", ">", ":", "/").
    special: Vec<(String, Action)>,
}

/// Files under the home whose name contains `needle`, from plocate's index.
fn files(needle: &str) -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let out = std::process::Command::new("plocate").args(["-i", "-b", "-l", "200", "--", needle]).stderr(std::process::Stdio::null()).output();
    let Ok(out) = out else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|p| p.starts_with(&home) && !p[home.len()..].split('/').any(|c| c.starts_with('.')))
        .take(50)
        .map(str::to_string)
        .collect()
}

/// Quotes a string for `sh`.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

impl Launcher {
    /// Scans the desktop entries; call each time the launcher opens so new
    /// applications show up.
    pub fn open() -> Result<Launcher, String> {
        let apps = apps::scan();
        let labels = apps.iter().map(|a| a.name.clone()).collect();
        Ok(Launcher { font: draw::load_font()?, apps, labels, picks: None, query: String::new(), filtered: Vec::new(), selected: 0, special: Vec::new() }.filtered())
    }

    /// A picker over `items`, filtered as you type; Enter picks, Delete removes.
    pub fn picker(items: Vec<String>) -> Result<Launcher, String> {
        let picks = Some((0..items.len()).collect());
        Ok(Launcher { font: draw::load_font()?, apps: Vec::new(), labels: items, picks, query: String::new(), filtered: Vec::new(), selected: 0, special: Vec::new() }.filtered())
    }

    fn filtered(mut self) -> Launcher {
        self.refresh_filter();
        self
    }

    /// "=" calculates or converts, ">" runs a command, ":" finds an emoji,
    /// "/" finds a file (through plocate) under the home.
    fn is_special(&self) -> bool {
        self.picks.is_none() && self.query.starts_with(['=', '>', ':', '/'])
    }

    fn special_rows(&self) -> Vec<(String, Action)> {
        let (mode, rest) = self.query.split_at(1);
        let rest = rest.trim();
        match mode {
            "=" => {
                let result = match units::convert(rest) {
                    Some((v, unit)) => Some(format!("{} {unit}", calc::format_result(v))),
                    None => calc::eval(rest).ok().map(calc::format_result),
                };
                match result {
                    Some(r) => vec![(format!("{r}   (Invio copia)"), Action::Copy(r))],
                    None => vec![("errore".into(), Action::Copy(String::new()))],
                }
            }
            // In a terminal that stays open: `top` and friends need one,
            // and the output of a plain command stays readable.
            ">" if !rest.is_empty() => vec![(format!("Esegui nel terminale: {rest}"), Action::Terminal(rest.to_string()))],
            ":" => {
                let needle = rest.to_lowercase();
                emoji::EMOJI
                    .iter()
                    .filter(|(_, names)| needle.split_whitespace().all(|w| names.contains(w)))
                    .map(|(e, names)| (names.to_string(), Action::Copy(e.to_string())))
                    .collect()
            }
            "/" if rest.chars().count() >= 3 => files(rest).into_iter().map(|p| (p.clone(), Action::Launch(format!("xdg-open {}", quote(&p))))).collect(),
            _ => Vec::new(),
        }
    }

    fn refresh_filter(&mut self) {
        self.selected = 0;
        if self.is_special() {
            self.special = self.special_rows();
            self.filtered = (0..self.special.len()).collect();
            return;
        }
        let needle = self.query.to_lowercase();
        self.filtered = self
            .labels
            .iter()
            .enumerate()
            .filter(|(_, l)| needle.is_empty() || l.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
    }

    /// First visible row, so the selection is always on screen.
    fn scroll_offset(&self) -> usize {
        let page = settings::VISIBLE_RESULTS as usize;
        if self.filtered.len() <= page {
            return 0;
        }
        self.selected.saturating_sub(page - 1).min(self.filtered.len() - page)
    }

    fn command(app: &apps::App) -> Outcome {
        if app.is_exit {
            Outcome::Close
        } else if let Some(url) = &app.url {
            Outcome::Launch(format!("{} {}", settings::BROWSER_CMD, quote(url)))
        } else if app.terminal {
            Outcome::Terminal { cmd: app.exec.clone(), hold: false }
        } else {
            Outcome::Launch(app.exec.clone())
        }
    }

    /// A key press: its keysym and the character it types, if any.
    pub fn key(&mut self, keysym: u32, text: Option<char>) -> Outcome {
        match keysym {
            keys::ESCAPE => Outcome::Close,
            keys::RETURN | keys::KP_ENTER => {
                if self.is_special() {
                    return match self.filtered.get(self.selected).and_then(|i| self.special.get(*i)).map(|(_, a)| a) {
                        Some(Action::Launch(cmd)) => Outcome::Launch(cmd.clone()),
                        Some(Action::Terminal(cmd)) => Outcome::Terminal { cmd: cmd.clone(), hold: true },
                        Some(Action::Copy(text)) if !text.is_empty() => Outcome::Copy(text.clone()),
                        _ => Outcome::Nothing,
                    };
                }
                if let Some(picks) = &self.picks {
                    return self.filtered.get(self.selected).map_or(Outcome::Nothing, |i| Outcome::Pick(picks[*i]));
                }
                self.filtered.get(self.selected).map_or(Outcome::Close, |i| Self::command(&self.apps[*i]))
            }
            keys::DELETE if self.picks.is_some() => {
                let Some(&i) = self.filtered.get(self.selected) else { return Outcome::Nothing };
                let original = self.picks.as_mut().unwrap().remove(i);
                self.labels.remove(i);
                let keep = self.selected;
                self.refresh_filter();
                self.selected = keep.min(self.filtered.len().saturating_sub(1));
                Outcome::Delete(original)
            }
            keys::BACKSPACE => {
                self.query.pop();
                self.refresh_filter();
                Outcome::Redraw
            }
            keys::UP => {
                self.selected = self.selected.saturating_sub(1);
                Outcome::Redraw
            }
            keys::DOWN => {
                if self.selected + 1 < self.filtered.len() {
                    self.selected += 1;
                }
                Outcome::Redraw
            }
            _ => match text.filter(|c| !c.is_control()) {
                Some(c) => {
                    self.query.push(c);
                    self.refresh_filter();
                    Outcome::Redraw
                }
                None => Outcome::Nothing,
            },
        }
    }

    /// The box as ARGB8888 little-endian pixels, `WIDTH` x `HEIGHT`.
    pub fn render(&self) -> Vec<u8> {
        let (w, h) = (settings::WINDOW_WIDTH, settings::WINDOW_HEIGHT);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let area = draw::Box2D { x: 0, y: 0, w, h };
        let offset = self.scroll_offset();
        let label = |idx: usize| -> &str {
            match self.is_special() {
                true => &self.special[idx].0,
                false => &self.labels[idx],
            }
        };
        let rows: Vec<draw::ResultRow> = self
            .filtered
            .iter()
            .skip(offset)
            .take(settings::VISIBLE_RESULTS as usize)
            .enumerate()
            .map(|(i, &idx)| draw::ResultRow { label: label(idx), selected: offset + i == self.selected })
            .collect();
        draw::draw_window(&mut buf, w, h, &area, &self.font, &self.query, &rows);
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculator_and_quoting() {
        let mut l = Launcher::open().unwrap();
        for c in "=2*(3+4)".chars() {
            l.key(0, Some(c));
        }
        assert!(matches!(l.key(keys::RETURN, None), Outcome::Copy(r) if r == "14"));
        assert_eq!(l.render().len(), (WIDTH * HEIGHT * 4) as usize);
        let mut e = Launcher::open().unwrap();
        for c in ":pizza".chars() {
            e.key(0, Some(c));
        }
        assert!(matches!(e.key(keys::RETURN, None), Outcome::Copy(r) if r == "🍕"));
        let mut x = Launcher::open().unwrap();
        for c in ">echo ciao".chars() {
            x.key(0, Some(c));
        }
        assert!(matches!(x.key(keys::RETURN, None), Outcome::Terminal { cmd, hold: true } if cmd == "echo ciao"));
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn picker_filters_picks_and_deletes() {
        let mut p = Launcher::picker(vec!["alpha".into(), "beta".into(), "alphabet".into()]).unwrap();
        for c in "alph".chars() {
            p.key(0, Some(c));
        }
        p.key(keys::DOWN, None);
        assert!(matches!(p.key(keys::RETURN, None), Outcome::Pick(2)));
        assert!(matches!(p.key(keys::DELETE, None), Outcome::Delete(2)));
        assert!(matches!(p.key(keys::RETURN, None), Outcome::Pick(0)));
        for c in "=1+1".chars() {
            p.key(0, Some(c));
        }
        assert!(matches!(p.key(keys::RETURN, None), Outcome::Nothing));
    }
}
