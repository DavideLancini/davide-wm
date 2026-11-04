//! The launcher, ported from super (`~/0Projects/super`): a box in the
//! middle of the screen listing desktop entries and `~/2Bookmarks`, filtered
//! as you type; a query starting with `=` is evaluated as arithmetic.
//!
//! The compositor owns the window and the keyboard: it feeds keys in and
//! draws the pixels this crate renders.

mod apps;
mod calc;
mod draw;
mod settings;

pub use settings::{WINDOW_HEIGHT as HEIGHT, WINDOW_WIDTH as WIDTH};

pub enum Outcome {
    Nothing,
    Redraw,
    Close,
    /// Close and run this shell command.
    Launch(String),
}

pub mod keys {
    pub const ESCAPE: u32 = 0xff1b;
    pub const RETURN: u32 = 0xff0d;
    pub const KP_ENTER: u32 = 0xff8d;
    pub const BACKSPACE: u32 = 0xff08;
    pub const UP: u32 = 0xff52;
    pub const DOWN: u32 = 0xff54;
}

pub struct Launcher {
    font: fontdue::Font,
    apps: Vec<apps::App>,
    query: String,
    filtered: Vec<usize>,
    selected: usize,
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
        let filtered = (0..apps.len()).collect();
        Ok(Launcher { font: draw::load_font()?, apps, query: String::new(), filtered, selected: 0 })
    }

    /// A query starting with "=" is a calculation: nothing to filter or launch.
    fn is_calc_mode(&self) -> bool {
        self.query.starts_with('=')
    }

    fn refresh_filter(&mut self) {
        self.selected = 0;
        if self.is_calc_mode() {
            self.filtered.clear();
            return;
        }
        let needle = self.query.to_lowercase();
        self.filtered = self
            .apps
            .iter()
            .enumerate()
            .filter(|(_, a)| needle.is_empty() || a.name.to_lowercase().contains(&needle))
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

    fn command(app: &apps::App) -> Option<String> {
        if app.is_exit {
            None
        } else if let Some(url) = &app.url {
            Some(format!("{} {}", settings::BROWSER_CMD, quote(url)))
        } else if app.terminal {
            Some(format!("{} -e sh -c {}", settings::TERMINAL_CMD, quote(&app.exec)))
        } else {
            Some(app.exec.clone())
        }
    }

    /// A key press: its keysym and the character it types, if any.
    pub fn key(&mut self, keysym: u32, text: Option<char>) -> Outcome {
        match keysym {
            keys::ESCAPE => Outcome::Close,
            keys::RETURN | keys::KP_ENTER => {
                if self.is_calc_mode() {
                    return Outcome::Nothing;
                }
                match self.filtered.get(self.selected).and_then(|i| Self::command(&self.apps[*i])) {
                    Some(cmd) => Outcome::Launch(cmd),
                    None => Outcome::Close,
                }
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
        if self.is_calc_mode() {
            let label = match calc::eval(&self.query[1..]) {
                Ok(v) => calc::format_result(v),
                Err(_) => "error".to_string(),
            };
            let rows = [draw::ResultRow { label: &label, selected: false }];
            draw::draw_window(&mut buf, w, h, &area, &self.font, &self.query, &rows);
            return buf;
        }
        let offset = self.scroll_offset();
        let rows: Vec<draw::ResultRow> = self
            .filtered
            .iter()
            .skip(offset)
            .take(settings::VISIBLE_RESULTS as usize)
            .enumerate()
            .map(|(i, &idx)| draw::ResultRow { label: &self.apps[idx].name, selected: offset + i == self.selected })
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
        assert!(matches!(l.key(keys::RETURN, None), Outcome::Nothing));
        assert_eq!(l.render().len(), (WIDTH * HEIGHT * 4) as usize);
        assert_eq!(quote("it's"), r"'it'\''s'");
    }
}
