//! Software rendering: black background plus the widget row, into an
//! ARGB8888 buffer (bytes B, G, R, A).

use std::path::Path;

use fontdue::{Font, FontSettings};

use crate::settings;

/// The bar's font, compiled into the binary: a subset of FiraCode Nerd Font
/// holding only the glyphs the bar draws (see `tools/build-font.sh`). Embedding
/// it removes the runtime dependency on a system font — previously a hard
/// requirement that stopped the bar from starting — and keeps text and icons in
/// one file at 16 KB.
const FONT_DATA: &[u8] = include_bytes!("../assets/topbar-font.ttf");

/// Loads the embedded font, falling back to the configured system path only if
/// the embedded data somehow fails to parse.
pub fn load_font() -> Result<Font, String> {
    match Font::from_bytes(FONT_DATA, FontSettings::default()) {
        Ok(font) => Ok(font),
        Err(embedded_err) => {
            let bytes = std::fs::read(Path::new(settings::FONT_PATH))
                .map_err(|e| format!("embedded font: {}; fallback {}: {}", embedded_err, settings::FONT_PATH, e))?;
            Font::from_bytes(bytes, FontSettings::default()).map_err(|e| format!("font: {}", e))
        }
    }
}

/// Characters that bound the drawn area: the tallest, the deepest descenders
/// and the accents. Measuring these is what lets the bar be exactly as tall as
/// the text it draws, rather than as tall as the font's nominal line box —
/// which includes leading the bar has no use for.
const EXTENT_SAMPLE: &str = "|[]{}()gyqpjÀÈÌÒÙàèìòù°ABCXYZ0123456789%:/";

/// Vertical extent of the glyphs the bar can draw, relative to the baseline:
/// `(rise above baseline, total height)`.
pub fn glyph_extent(font: &Font) -> (i32, i32) {
    let mut top = i32::MAX;
    let mut bottom = i32::MIN;
    for c in EXTENT_SAMPLE.chars().chain(crate::icons::ALL.iter().copied()) {
        let m = font.metrics(c, settings::FONT_SIZE);
        if m.height == 0 {
            continue;
        }
        // ymin is the bitmap's bottom edge relative to the baseline.
        top = top.min(-(m.ymin + m.height as i32));
        bottom = bottom.max(-m.ymin);
    }
    if top == i32::MAX {
        let fallback = settings::FONT_SIZE.ceil() as i32;
        return (fallback, fallback);
    }
    // `top` is negative above the baseline; the rise is its magnitude.
    (-top, bottom - top)
}

/// Baseline offset from the top of the bar: the glyphs' rise plus the padding,
/// so the tallest glyph sits exactly `VPAD` pixels below the top edge.
fn baseline(font: &Font, height: u32) -> i32 {
    let (rise, _) = glyph_extent(font);
    (rise + settings::VPAD).min(height as i32 - 1)
}

/// Width of a string as drawn. The widget separator is a control character the
/// renderer consumes rather than draws, so it contributes nothing — the font
/// would otherwise report a full advance for its missing glyph.
pub fn text_width(font: &Font, s: &str) -> i32 {
    s.chars()
        .filter(|c| !c.is_control())
        .map(|c| font.metrics(c, settings::FONT_SIZE).advance_width.ceil() as i32)
        .sum()
}

/// Horizontal extent of a clickable item, in surface coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HitBox {
    pub x0: i32,
    pub x1: i32,
}

impl HitBox {
    pub fn contains(&self, x: i32) -> bool {
        x >= self.x0 && x < self.x1
    }
}

/// Hit boxes for space-separated items of a string starting at `start_x`.
/// Superseded by `segment_hitboxes` now that every widget — workspaces
/// included — is a segment of its own; kept for the tests that measure how
/// words within one string line up.
#[cfg(test)]
pub fn item_hitboxes(font: &Font, start_x: i32, s: &str) -> Vec<HitBox> {
    let space = text_width(font, " ");
    let mut boxes = Vec::new();
    let mut x = start_x;
    for item in s.split(' ') {
        let w = text_width(font, item);
        if !item.is_empty() {
            boxes.push(HitBox { x0: x, x1: x + w });
        }
        x += w + space;
    }
    boxes
}

/// Hit boxes for the widgets of the bar, at the positions they are drawn at.
/// A widget that renders several words gets one box spanning all of them,
/// unlike `item_hitboxes`.
pub fn segment_hitboxes(font: &Font, start_x: i32, s: &str) -> Vec<HitBox> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(crate::modules::layout::SEP)
        .zip(segment_positions(font, start_x, s))
        .map(|(segment, x)| HitBox { x0: x, x1: x + text_width(font, segment) })
        .collect()
}

/// Fills the background, then draws the two halves: the left one from the left
/// padding, the right one packed against the right edge.
#[allow(clippy::too_many_arguments)]
pub fn draw_bar(
    buf: &mut [u8],
    width: u32,
    height: u32,
    font: &Font,
    left: &str,
    left_colors: &[u32],
    right: &str,
    right_colors: &[u32],
) {
    let stride = (width * 4) as usize;
    if buf.len() < stride * (height as usize) {
        return;
    }

    let [_, bg_r, bg_g, bg_b] = settings::BG_COLOR.to_be_bytes();
    for px in buf[..stride * height as usize].as_chunks_mut::<4>().0 {
        px.copy_from_slice(&[bg_b, bg_g, bg_r, 0xFF]);
    }

    let base = baseline(font, height);
    draw_zone(buf, stride, width, height, font, settings::PAD, base, left, left_colors);
    let anchor = right_anchor(font, width, right);
    draw_zone(buf, stride, width, height, font, anchor, base, right, right_colors);
}

/// Draws one zone from `start_x`, each widget in its own colour.
#[allow(clippy::too_many_arguments)]
fn draw_zone(
    buf: &mut [u8],
    stride: usize,
    width: u32,
    height: u32,
    font: &Font,
    start_x: i32,
    base: i32,
    text: &str,
    colors: &[u32],
) {
    if text.is_empty() {
        return;
    }
    for (i, (segment, x)) in text
        .split(crate::modules::layout::SEP)
        .zip(segment_positions(font, start_x, text))
        .enumerate()
    {
        let color = colors.get(i).copied().unwrap_or(settings::FG_COLOR);
        draw_line(buf, stride, width as i32, height, font, x, base, segment, color);
    }
}

/// Trims the elastic segment so the whole bar fits the width.
///
/// The track line has no meaningful length of its own: rather than
/// cutting it at a fixed character count, it takes whatever room the other
/// widgets leave, shrinking to nothing when they need it all. Returns the text
/// with that segment resized.
pub fn fit_elastic(
    font: &Font,
    available_width: i32,
    text: &str,
    elastic: Option<usize>,
) -> String {
    let sep = crate::modules::layout::SEP;
    let Some(index) = elastic else {
        return text.to_string();
    };
    let mut segments: Vec<String> = text.split(sep).map(String::from).collect();
    if index >= segments.len() {
        return text.to_string();
    }

    // Room the other widgets and the gaps between them already claim.
    let gaps = segments.len().saturating_sub(1) as i32 * settings::WIDGET_GAP;
    let others: i32 = segments
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != index)
        .map(|(_, seg)| text_width(font, seg))
        .sum();
    let available = available_width - gaps - others;

    // Drop the segment entirely when there is no room worth using.
    if available < text_width(font, "…") {
        segments.remove(index);
        return segments.join(sep);
    }
    if text_width(font, &segments[index]) <= available {
        return text.to_string(); // already fits
    }

    // Trim character by character, leaving room for the ellipsis.
    let ellipsis = text_width(font, "…");
    let mut kept = String::new();
    let mut used = 0;
    for c in segments[index].chars() {
        let w = font.metrics(c, settings::FONT_SIZE).advance_width.ceil() as i32;
        if used + w + ellipsis > available {
            break;
        }
        kept.push(c);
        used += w;
    }
    segments[index] = format!("{}…", kept.trim_end());
    segments.join(sep)
}

/// Where each widget of a zone starts, packed from `start_x` with a fixed gap.
///
/// Widgets keep their natural width and a constant distance: nothing stretches
/// to fill the row, so a value changing length moves only what follows it in
/// its own half of the bar (ADR-032).
pub fn segment_positions(font: &Font, start_x: i32, text: &str) -> Vec<i32> {
    let mut positions = Vec::new();
    let mut x = start_x;
    if text.is_empty() {
        return positions;
    }
    for segment in text.split(crate::modules::layout::SEP) {
        positions.push(x);
        x += text_width(font, segment) + settings::WIDGET_GAP;
    }
    positions
}

/// Total width a zone occupies, gaps included.
pub fn zone_width(font: &Font, text: &str) -> i32 {
    if text.is_empty() {
        return 0;
    }
    let count = text.split(crate::modules::layout::SEP).count() as i32;
    text_width(font, text) + (count - 1).max(0) * settings::WIDGET_GAP
}

/// Leftmost x of a zone anchored to the right edge.
pub fn right_anchor(font: &Font, width: u32, text: &str) -> i32 {
    (width as i32 - settings::PAD - zone_width(font, text)).max(settings::PAD)
}

/// Draws one line of text with its glyph bottoms placed relative to `base`.
#[allow(clippy::too_many_arguments)]
fn draw_line(
    buf: &mut [u8],
    stride: usize,
    width: i32,
    height: u32,
    font: &Font,
    start_x: i32,
    base: i32,
    s: &str,
    color: u32,
) {
    let h = height as i32;
    let [_, fg_r, fg_g, fg_b] = color.to_be_bytes();
    let [_, bg_r, bg_g, bg_b] = settings::BG_COLOR.to_be_bytes();
    let mut x = start_x;

    for c in s.chars() {
        if c.is_control() {
            continue; // separators carry structure, not pixels
        }
        let (metrics, bitmap) = font.rasterize(c, settings::FONT_SIZE);
        // ymin is the bitmap's bottom edge relative to the baseline (negative
        // for descenders), so the bitmap's top row sits here:
        let top = base - metrics.ymin - metrics.height as i32;

        if metrics.width > 0 {
            for (i, &coverage) in bitmap.iter().enumerate() {
                if coverage == 0 {
                    continue;
                }
                let px_x = x + metrics.xmin + (i % metrics.width) as i32;
                let px_y = top + (i / metrics.width) as i32;
                if px_x < 0 || px_x >= width || px_y < 0 || px_y >= h {
                    continue;
                }
                let idx = (px_y as usize * stride) + (px_x as usize) * 4;
                let a = coverage as u32;
                let blend = |fg: u8, bg: u8| ((fg as u32 * a + bg as u32 * (255 - a)) / 255) as u8;
                buf[idx] = blend(fg_b, bg_b);
                buf[idx + 1] = blend(fg_g, bg_g);
                buf[idx + 2] = blend(fg_r, bg_r);
                buf[idx + 3] = 0xFF;
            }
        }

        x += metrics.advance_width.ceil() as i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit_columns(buf: &[u8], width: u32, height: u32) -> Vec<usize> {
        let stride = (width * 4) as usize;
        (0..width as usize)
            .filter(|&x| {
                (0..height as usize).any(|y| {
                    let i = y * stride + x * 4;
                    buf[i] != 0 || buf[i + 1] != 0 || buf[i + 2] != 0
                })
            })
            .collect()
    }

    /// Glyphs must advance horizontally: a multi-character string has to light
    /// up columns spanning at least its full advance width.
    #[test]
    fn glyphs_do_not_overlap() {
        let font = load_font().unwrap();
        let (w, h) = (400, settings::bar_height(&font));
        let mut buf = vec![0u8; (w * h * 4) as usize];
        draw_bar(&mut buf, w, h, &font, "iiii", &[], "", &[]);

        let cols = lit_columns(&buf, w, h);
        let advance = font.metrics('i', settings::FONT_SIZE).advance_width.ceil() as usize;
        let span = cols.last().unwrap() - cols.first().unwrap() + 1;
        assert!(
            span > advance * 3,
            "span {span} too narrow for 4 glyphs of advance {advance}: glyphs overlap"
        );
    }

    /// Ascenders and descenders must both stay inside the bar.
    #[test]
    fn tall_and_descending_glyphs_fit() {
        let font = load_font().unwrap();
        let (w, h) = (400, settings::bar_height(&font));
        let stride = (w * 4) as usize;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        draw_bar(&mut buf, w, h, &font, "|[gyà°", &[], "", &[]);

        for c in "|[gyà°".chars() {
            let m = font.metrics(c, settings::FONT_SIZE);
            let top = baseline(&font, h) - m.ymin - m.height as i32;
            assert!(top >= 0, "{c:?} clipped at top (top={top})");
            assert!(top + m.height as i32 <= h as i32, "{c:?} clipped at bottom");
        }
        // Background stays black where nothing is drawn.
        let last_row = (h as usize - 1) * stride;
        assert_eq!(&buf[last_row..last_row + 4], &[0, 0, 0, 0xFF], "opaque black");
    }

    /// The bar is exactly as tall as the glyphs plus one pixel above and below,
    /// and nothing is drawn in those padding rows.
    #[test]
    fn bar_height_is_glyphs_plus_padding() {
        let font = load_font().unwrap();
        let (rise, extent) = glyph_extent(&font);
        let h = settings::bar_height(&font);
        assert_eq!(h as i32, extent + 2 * settings::VPAD, "height must be glyphs + padding");
        assert_eq!(baseline(&font, h), rise + settings::VPAD);

        // Render the tallest and deepest glyphs: the padding rows stay empty.
        let w = 400;
        let stride = (w * 4) as usize;
        let mut buf = vec![0u8; stride * h as usize];
        draw_bar(&mut buf, w, h, &font, "|[]gyà°ABC", &[], "", &[]);

        let row_is_blank = |y: usize| {
            (0..w as usize).all(|x| {
                let i = y * stride + x * 4;
                buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 0
            })
        };
        for y in 0..settings::VPAD as usize {
            assert!(row_is_blank(y), "top padding row {y} must be empty");
            assert!(
                row_is_blank(h as usize - 1 - y),
                "bottom padding row must be empty"
            );
        }
    }

    /// Hit boxes must line up with where the glyphs actually land, so a click
    /// on an item's pixels maps back to that same item.
    #[test]
    fn hitboxes_match_rendered_glyphs() {
        let font = load_font().unwrap();
        let (w, h) = (400, settings::bar_height(&font));
        let text = "chat resize 31 [32]";
        let mut buf = vec![0u8; (w * h * 4) as usize];
        draw_bar(&mut buf, w, h, &font, text, &[], "", &[]);

        let boxes = item_hitboxes(&font, settings::PAD, text);
        assert_eq!(boxes.len(), 4, "one box per item: {boxes:?}");

        // Boxes are ordered, disjoint and inside the drawn span.
        for pair in boxes.windows(2) {
            assert!(pair[0].x1 <= pair[1].x0, "overlapping boxes: {pair:?}");
        }
        let cols = lit_columns(&buf, w, h);
        assert!(*cols.first().unwrap() as i32 >= boxes[0].x0);
        assert!((*cols.last().unwrap() as i32) < boxes[3].x1);

        // Every item's box contains lit pixels, and no box is empty.
        for (i, b) in boxes.iter().enumerate() {
            assert!(
                cols.iter().any(|&c| b.contains(c as i32)),
                "item {i} box {b:?} covers no glyph"
            );
        }
    }

    /// The elastic segment takes the room the others leave, instead of being
    /// cut at a fixed character count.
    #[test]
    fn elastic_segment_uses_the_leftover_room() {
        let font = load_font().unwrap();
        let sep = crate::modules::layout::SEP;
        let long: String = "abcdefghij".repeat(20);
        let text = format!("fixed{sep}{long}{sep}tail");

        // Plenty of room: nothing is trimmed.
        let wide = fit_elastic(&font, 4000, &text, Some(1));
        assert_eq!(wide, text, "no trimming when it already fits");

        // Tight: the elastic segment shrinks and the whole thing fits.
        let narrow = fit_elastic(&font, 600, &text, Some(1));
        let parts: Vec<&str> = narrow.split(sep).collect();
        assert_eq!(parts.len(), 3, "the other widgets survive");
        assert_eq!(parts[0], "fixed");
        assert_eq!(parts[2], "tail");
        assert!(parts[1].ends_with('…'), "trimmed text is marked: {:?}", parts[1]);
        assert!(parts[1].chars().count() < long.chars().count());
        assert!(
            zone_width(&font, &narrow) <= 600,
            "the zone must fit the room it was given"
        );
    }

    /// When the other widgets need every pixel, the elastic segment disappears
    /// rather than pushing them off the edge.
    #[test]
    fn elastic_segment_drops_when_there_is_no_room() {
        let font = load_font().unwrap();
        let sep = crate::modules::layout::SEP;
        let text = format!("a very wide fixed widget{sep}clipboard preview{sep}another wide one");

        let squeezed = fit_elastic(&font, 200, &text, Some(1));
        let parts: Vec<&str> = squeezed.split(sep).collect();
        assert_eq!(parts.len(), 2, "the elastic segment is gone: {parts:?}");
        assert_eq!(parts[0], "a very wide fixed widget");
        assert_eq!(parts[1], "another wide one");
    }

    /// Without an elastic segment the text is returned untouched, and an index
    /// past the end is ignored rather than panicking.
    #[test]
    fn fit_elastic_handles_missing_and_bad_indices() {
        let font = load_font().unwrap();
        let sep = crate::modules::layout::SEP;
        let text = format!("one{sep}two");
        assert_eq!(fit_elastic(&font, 100, &text, None), text);
        assert_eq!(fit_elastic(&font, 100, &text, Some(9)), text);
    }

    /// Widgets are packed from their anchor with a constant gap: nothing
    /// stretches, so a value changing width moves only what follows it.
    #[test]
    fn widgets_pack_with_a_fixed_gap() {
        let font = load_font().unwrap();
        let sep = crate::modules::layout::SEP;
        let text = format!("AB{sep}CD{sep}EF");

        let positions = segment_positions(&font, settings::PAD, &text);
        assert_eq!(positions.len(), 3);
        assert_eq!(positions[0], settings::PAD, "first widget starts at the padding");

        let widths: Vec<i32> = text.split(sep).map(|seg| text_width(&font, seg)).collect();
        for (i, pair) in positions.windows(2).enumerate() {
            assert_eq!(
                pair[1] - pair[0] - widths[i],
                settings::WIDGET_GAP,
                "gap after widget {i}"
            );
        }

        // A widget growing shifts only the ones after it.
        let wider = format!("ABCD{sep}CD{sep}EF");
        let shifted = segment_positions(&font, settings::PAD, &wider);
        assert_eq!(shifted[0], positions[0], "the first widget does not move");
        assert!(shifted[1] > positions[1], "the ones after it do");
    }

    /// A zone anchored right ends flush with the right padding.
    #[test]
    fn right_zone_is_flush_with_the_edge() {
        let font = load_font().unwrap();
        let sep = crate::modules::layout::SEP;
        let text = format!("AB{sep}CD");
        let w = 800u32;

        let anchor = right_anchor(&font, w, &text);
        let positions = segment_positions(&font, anchor, &text);
        let last_width = text_width(&font, text.split(sep).last().unwrap());
        assert_eq!(
            positions.last().unwrap() + last_width,
            w as i32 - settings::PAD,
            "the last widget must end at the right padding"
        );

        // Too wide to fit: clamp to the left padding rather than going negative.
        let huge = "x".repeat(500);
        assert_eq!(right_anchor(&font, 100, &huge), settings::PAD);
    }

    /// Hit boxes must sit exactly where the widgets are drawn, in both halves.
    #[test]
    fn hitboxes_follow_the_drawn_positions() {
        let font = load_font().unwrap();
        let sep = crate::modules::layout::SEP;
        let text = format!("AB{sep}CD{sep}EF");

        for start in [settings::PAD, right_anchor(&font, 800, &text)] {
            let positions = segment_positions(&font, start, &text);
            let boxes = segment_hitboxes(&font, start, &text);
            assert_eq!(boxes.len(), positions.len());
            for (b, x) in boxes.iter().zip(&positions) {
                assert_eq!(b.x0, *x, "hit box must start where the widget is drawn");
                assert!(b.x1 > b.x0);
            }
            for pair in boxes.windows(2) {
                assert!(pair[0].x1 <= pair[1].x0, "overlapping boxes: {pair:?}");
            }
        }
    }

}
