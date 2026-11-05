//! Notification popups: a bordered black card with the title on the first
//! line and the body wrapped below it.

use fontdue::Font;

use crate::draw::{draw_text, fill_rect_opaque, text_width_at};

pub const CARD_WIDTH: u32 = 420;
const SIZE: f32 = 18.0;
const LINE: u32 = 24;
const PAD: u32 = 10;
const MAX_BODY_LINES: usize = 4;
const TITLE: u32 = 0xFF_FF_FF;
const BODY: u32 = 0xB0_B0_B0;
const BORDER: u32 = 0xFF_FF_FF;
const CRITICAL: u32 = 0xE6_59_59;

pub struct Cards {
    font: Font,
}

/// Greedy word wrap to `max` pixels; words longer than a line are cut.
fn wrap(font: &Font, text: &str, max: i32, lines: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for para in text.lines() {
        let mut cur = String::new();
        for word in para.split_whitespace() {
            let cand = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
            if text_width_at(font, &cand, SIZE) <= max {
                cur = cand;
                continue;
            }
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            cur = fit(font, word, max);
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    if out.len() > lines {
        out.truncate(lines);
        let last = out.last_mut().unwrap();
        *last = fit(font, &format!("{last}…"), max);
        if !last.ends_with('…') {
            last.pop();
            last.push('…');
        }
    }
    out
}

/// The longest prefix of `s` that fits in `max` pixels.
fn fit(font: &Font, s: &str, max: i32) -> String {
    let mut out = String::new();
    for c in s.chars() {
        out.push(c);
        if text_width_at(font, &out, SIZE) > max {
            out.pop();
            break;
        }
    }
    out
}

impl Cards {
    pub fn new() -> Result<Cards, String> {
        Ok(Cards { font: crate::draw::load_font()? })
    }

    /// ARGB8888 little-endian pixels, `CARD_WIDTH` wide; returns the height too.
    pub fn render(&self, title: &str, body: &str, critical: bool) -> (u32, Vec<u8>) {
        let w = CARD_WIDTH;
        let inner = (w - 2 * PAD) as i32;
        let title = fit(&self.font, title.lines().next().unwrap_or(""), inner);
        let body = wrap(&self.font, body, inner, MAX_BODY_LINES);
        let h = 2 * PAD + LINE * (1 + body.len() as u32);
        let stride = (w * 4) as usize;
        let mut buf = vec![0u8; stride * h as usize];
        let border = if critical { CRITICAL } else { BORDER };
        fill_rect_opaque(&mut buf, stride, 0, 0, w, h, border);
        fill_rect_opaque(&mut buf, stride, 1, 1, w - 2, h - 2, 0);
        let baseline = |row: u32| (PAD + LINE * row + LINE - 6) as i32;
        draw_text(&mut buf, stride, w as i32, h, &self.font, SIZE, PAD as i32, baseline(0), &title, TITLE);
        for (i, line) in body.iter().enumerate() {
            draw_text(&mut buf, stride, w as i32, h, &self.font, SIZE, PAD as i32, baseline(1 + i as u32), line, BODY);
        }
        (h, buf)
    }
}

/// Width of the volume and brightness indicator.
pub const OSD_WIDTH: u32 = 360;
/// Width of a password prompt.
pub const PROMPT_WIDTH: u32 = 560;
const TRACK: u32 = 0x40_40_40;
const ERROR: u32 = 0xE6_59_59;

impl Cards {
    /// The volume/brightness indicator: a label and, unless muted, a level
    /// bar (`fraction` 0..=1). Returns the height and ARGB pixels.
    pub fn osd(&self, label: &str, fraction: Option<f32>) -> (u32, Vec<u8>) {
        let (w, h) = (OSD_WIDTH, 2 * PAD + LINE + 14);
        let stride = (w * 4) as usize;
        let mut buf = vec![0u8; stride * h as usize];
        fill_rect_opaque(&mut buf, stride, 0, 0, w, h, BORDER);
        fill_rect_opaque(&mut buf, stride, 1, 1, w - 2, h - 2, 0);
        let inner = w - 2 * PAD;
        draw_text(&mut buf, stride, w as i32, h, &self.font, SIZE, PAD as i32, (PAD + LINE - 6) as i32, &fit(&self.font, label, inner as i32), TITLE);
        if let Some(f) = fraction {
            let y = PAD + LINE + 2;
            fill_rect_opaque(&mut buf, stride, PAD, y, inner, 6, TRACK);
            let filled = (inner as f32 * f.clamp(0.0, 1.0)).round() as u32;
            if filled > 0 {
                fill_rect_opaque(&mut buf, stride, PAD, y, filled, 6, TITLE);
            }
        }
        (h, buf)
    }

    /// A password prompt: a title, an explanation, the typed characters as
    /// dots and an error line. Returns the height and ARGB pixels.
    pub fn prompt(&self, title: &str, message: &str, typed: usize, error: &str) -> (u32, Vec<u8>) {
        let w = PROMPT_WIDTH;
        let inner = (w - 2 * PAD) as i32;
        let title = fit(&self.font, title, inner);
        let body = wrap(&self.font, message, inner, 3);
        let rows = 1 + body.len() as u32 + 2 + u32::from(!error.is_empty());
        let h = 2 * PAD + LINE * rows + 8;
        let stride = (w * 4) as usize;
        let mut buf = vec![0u8; stride * h as usize];
        fill_rect_opaque(&mut buf, stride, 0, 0, w, h, BORDER);
        fill_rect_opaque(&mut buf, stride, 1, 1, w - 2, h - 2, 0);
        let baseline = |row: u32| (PAD + LINE * row + LINE - 6) as i32;
        draw_text(&mut buf, stride, w as i32, h, &self.font, SIZE, PAD as i32, baseline(0), &title, TITLE);
        for (i, line) in body.iter().enumerate() {
            draw_text(&mut buf, stride, w as i32, h, &self.font, SIZE, PAD as i32, baseline(1 + i as u32), line, BODY);
        }
        let field = 1 + body.len() as u32;
        let fy = PAD + LINE * field + 6;
        fill_rect_opaque(&mut buf, stride, PAD, fy, w - 2 * PAD, LINE + 8, TRACK);
        fill_rect_opaque(&mut buf, stride, PAD + 1, fy + 1, w - 2 * PAD - 2, LINE + 6, 0);
        let dots = fit(&self.font, &"\u{2022}".repeat(typed), inner - 12);
        draw_text(&mut buf, stride, w as i32, h, &self.font, SIZE, PAD as i32 + 6, (fy + LINE) as i32, &dots, TITLE);
        if !error.is_empty() {
            draw_text(&mut buf, stride, w as i32, h, &self.font, SIZE, PAD as i32, baseline(field + 2) + 8, &fit(&self.font, error, inner), ERROR);
        }
        (h, buf)
    }
}

/// Width of the calendar.
pub const CALENDAR_WIDTH: u32 = 7 * CELL + 2 * PAD;
const CELL: u32 = 46;
const MONTHS: [&str; 12] = ["Gennaio", "Febbraio", "Marzo", "Aprile", "Maggio", "Giugno", "Luglio", "Agosto", "Settembre", "Ottobre", "Novembre", "Dicembre"];

impl Cards {
    /// A month: `first` is the weekday of day 1 (0 = Monday), `today` the
    /// day to mark. Returns the height and ARGB pixels.
    pub fn calendar(&self, year: i32, month: u32, days: u32, first: u32, today: Option<u32>) -> (u32, Vec<u8>) {
        let rows = (first + days).div_ceil(7);
        let w = CALENDAR_WIDTH;
        let h = 2 * PAD + LINE * (2 + rows) + 8;
        let stride = (w * 4) as usize;
        let mut buf = vec![0u8; stride * h as usize];
        fill_rect_opaque(&mut buf, stride, 0, 0, w, h, BORDER);
        fill_rect_opaque(&mut buf, stride, 1, 1, w - 2, h - 2, 0);
        let baseline = |row: u32| (PAD + LINE * row + LINE - 6) as i32;
        let title = format!("{} {year}", MONTHS[(month as usize).clamp(1, 12) - 1]);
        let tw = text_width_at(&self.font, &title, SIZE);
        draw_text(&mut buf, stride, w as i32, h, &self.font, SIZE, (w as i32 - tw) / 2, baseline(0), &title, TITLE);
        let centred = |buf: &mut Vec<u8>, col: u32, row: u32, text: &str, color: u32| {
            let tw = text_width_at(&self.font, text, SIZE);
            let x = (PAD + col * CELL) as i32 + (CELL as i32 - tw) / 2;
            draw_text(buf, stride, w as i32, h, &self.font, SIZE, x, baseline(row) + 4, text, color);
        };
        for (i, d) in ["Lu", "Ma", "Me", "Gi", "Ve", "Sa", "Do"].iter().enumerate() {
            centred(&mut buf, i as u32, 1, d, BODY);
        }
        for day in 1..=days {
            let cell = first + day - 1;
            let (col, row) = (cell % 7, 2 + cell / 7);
            if today == Some(day) {
                fill_rect_opaque(&mut buf, stride, PAD + col * CELL + 4, PAD + LINE * row + 4, CELL - 8, LINE, TRACK);
            }
            let color = if col >= 5 { CRITICAL } else { TITLE };
            centred(&mut buf, col, row, &day.to_string(), color);
        }
        (h, buf)
    }
}

/// The mixer panel.
pub const MIXER_WIDTH: u32 = 1100;
/// Rows of the mixer shown at once.
pub const MIXER_ROWS: usize = 12;
/// Height of a mixer row (two lines), and of its header row.
pub const MIXER_ROW: u32 = 2 * LINE + 8;
pub const MIXER_TOP: u32 = PAD + LINE + 12;
const BAR_W: u32 = 360;
const SELECTED: u32 = 0x33_33_33;

/// A line of the mixer: a section title, or a device, stream or card.
pub struct MixRow {
    pub title: String,
    pub detail: String,
    /// Per-channel volume in percent, muted, the channel being edited.
    pub volume: Option<(Vec<u32>, bool, Option<usize>)>,
    pub header: bool,
    /// The default device.
    pub marked: bool,
}

impl Cards {
    /// The mixer: tabs, then rows from `scroll` (`selected` highlighted),
    /// then the key help. Returns the height and ARGB pixels.
    pub fn mixer(&self, tabs: [&str; 2], tab: usize, rows: &[MixRow], selected: usize, scroll: usize, help: &str) -> (u32, Vec<u8>) {
        let w = MIXER_WIDTH;
        let h = MIXER_TOP + MIXER_ROW * MIXER_ROWS as u32 + LINE + 2 * PAD;
        let stride = (w * 4) as usize;
        let mut buf = vec![0u8; stride * h as usize];
        fill_rect_opaque(&mut buf, stride, 0, 0, w, h, BORDER);
        fill_rect_opaque(&mut buf, stride, 1, 1, w - 2, h - 2, 0);
        let text = |buf: &mut Vec<u8>, x: i32, y: i32, s: &str, c: u32| draw_text(buf, stride, w as i32, h, &self.font, SIZE, x, y, s, c);
        let mut x = PAD as i32;
        for (i, t) in tabs.iter().enumerate() {
            let tw = text_width_at(&self.font, t, SIZE);
            if i == tab {
                fill_rect_opaque(&mut buf, stride, x as u32 - 6, PAD - 2, tw as u32 + 12, LINE + 4, SELECTED);
            }
            text(&mut buf, x, (PAD + LINE - 6) as i32, t, if i == tab { TITLE } else { BODY });
            x += tw + 40;
        }
        for (n, row) in rows.iter().skip(scroll).take(MIXER_ROWS).enumerate() {
            let y = MIXER_TOP + n as u32 * MIXER_ROW;
            if scroll + n == selected {
                fill_rect_opaque(&mut buf, stride, 2, y, w - 4, MIXER_ROW - 2, SELECTED);
            }
            let base = (y + LINE - 2) as i32;
            if row.header {
                text(&mut buf, PAD as i32, base + (LINE / 2) as i32, &row.title, CRITICAL);
                continue;
            }
            let title = format!("{}{}", if row.marked { "\u{2022} " } else { "" }, row.title);
            let right = (w - PAD - BAR_W) as i32;
            text(&mut buf, PAD as i32, base, &fit(&self.font, &title, right - 20 - PAD as i32), TITLE);
            text(&mut buf, PAD as i32 + 16, base + LINE as i32, &fit(&self.font, &row.detail, right - 36 - PAD as i32), BODY);
            if let Some((vol, muted, channel)) = &row.volume {
                let n = vol.len().max(1) as u32;
                let bh = ((LINE + 4) / n).clamp(3, 10);
                for (c, v) in vol.iter().enumerate() {
                    let by = y + LINE + 4 + c as u32 * (bh + 2);
                    fill_rect_opaque(&mut buf, stride, right as u32, by, BAR_W, bh, TRACK);
                    let fill = (BAR_W as f32 * (*v as f32 / 150.0).min(1.0)) as u32;
                    let color = if *muted { 0x60_60_60 } else if *channel == Some(c) { CRITICAL } else { TITLE };
                    if fill > 0 {
                        fill_rect_opaque(&mut buf, stride, right as u32, by, fill, bh, color);
                    }
                    // 100 % mark.
                    fill_rect_opaque(&mut buf, stride, right as u32 + BAR_W * 2 / 3, by, 1, bh, BODY);
                }
                let label = if *muted { "muto".to_string() } else { format!("{}%", vol.iter().max().copied().unwrap_or(0)) };
                text(&mut buf, right, base, &label, if *muted { CRITICAL } else { TITLE });
            }
        }
        text(&mut buf, PAD as i32, (h - PAD - 6) as i32, &fit(&self.font, help, (w - 2 * PAD) as i32), BODY);
        (h, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_ellipsises() {
        let c = Cards::new().unwrap();
        let long = "parola ".repeat(200);
        let lines = wrap(&c.font, &long, 300, 3);
        assert_eq!(lines.len(), 3);
        assert!(lines[2].ends_with('…'));
        assert!(lines.iter().all(|l| text_width_at(&c.font, l, SIZE) <= 300));
        let (h, px) = c.render("Titolo", "corpo", false);
        assert_eq!(px.len(), (CARD_WIDTH * h * 4) as usize);
        let (h, px) = c.osd("Volume 40%", Some(0.4));
        assert_eq!(px.len(), (OSD_WIDTH * h * 4) as usize);
        let rows = vec![
            MixRow { title: "Dispositivi".into(), detail: String::new(), volume: None, header: true, marked: false },
            MixRow { title: "Altoparlanti".into(), detail: "porta: cuffie".into(), volume: Some((vec![60, 80], false, Some(1))), header: false, marked: true },
        ];
        let (h, px) = c.mixer(["Uscita", "Ingresso"], 0, &rows, 1, 0, "Tab cambia");
        assert_eq!(px.len(), (MIXER_WIDTH * h * 4) as usize);
        let (h, px) = c.calendar(2026, 11, 30, 6, Some(5));
        assert_eq!(px.len(), (CALENDAR_WIDTH * h * 4) as usize);
        let (h, px) = c.prompt("Autenticazione", "Serve la password per montare il disco", 5, "Password errata");
        assert_eq!(px.len(), (PROMPT_WIDTH * h * 4) as usize);
    }
}
