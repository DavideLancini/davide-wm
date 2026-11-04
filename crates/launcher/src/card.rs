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
    }
}
