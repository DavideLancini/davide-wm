//! The launcher box: search row + result rows below, drawn into a buffer
//! the size of the box.

use fontdue::{Font, FontSettings};

use crate::settings as config;

/// The launcher's font, compiled into the binary: a subset of Noto Sans Mono
/// ExtraCondensed holding only the glyphs the launcher draws (see
/// tools/build-font.sh, same approach as the bar).
/// Embedding it removes the runtime dependency on a system font and keeps
/// text in one ~12 KB file.
const FONT_DATA: &[u8] = include_bytes!("../assets/launcher-font.ttf");

pub fn load_font() -> Result<Font, String> {
    Font::from_bytes(FONT_DATA, FontSettings::default()).map_err(|e| format!("launcher font: {e}"))
}

const PAD: i32 = 8;

/// Characters sampled to compute a single, fixed baseline for every row
/// (see `glyph_rise`) — includes ascenders, descenders and accents so the
/// baseline is correct regardless of which of these any given label uses.
const EXTENT_SAMPLE: &str = "|[]{}()gyqpjÀÈÌÒÙàèìòù°ABCXYZ0123456789%:/";

/// How far above the baseline the tallest sampled glyph rises, in pixels.
/// Computed once from a fixed character sample (not per-string): using
/// each label's own glyph heights, as an earlier version did, gave every
/// row a different baseline depending on its text — misaligned rows, and
/// descenders (like 'g') clipped because `ymin` wasn't accounted for.
fn glyph_rise(font: &Font) -> i32 {
    let mut top = i32::MAX;
    for c in EXTENT_SAMPLE.chars() {
        let m = font.metrics(c, config::FONT_SIZE);
        if m.height == 0 {
            continue;
        }
        // ymin is the bitmap's bottom edge relative to the baseline, so
        // -(ymin + height) is how far the glyph's top extends above it.
        top = top.min(-(m.ymin + m.height as i32));
    }
    if top == i32::MAX {
        config::FONT_SIZE.ceil() as i32
    } else {
        -top
    }
}

/// One row of the results list, already filtered — just the label to draw.
pub struct ResultRow<'a> {
    pub label: &'a str,
    pub selected: bool,
}

/// Where the opaque box sits within the fullscreen buffer.
pub struct Box2D {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// 1) Fully transparent fullscreen buffer. 2) Opaque black box at `box2d`:
/// search row (query text + cursor) + result rows.
pub fn draw_window(
    buf: &mut [u8],
    screen_w: u32,
    screen_h: u32,
    box2d: &Box2D,
    font: &Font,
    query: &str,
    results: &[ResultRow],
) {
    let stride = (screen_w * 4) as usize;
    if buf.len() < stride * (screen_h as usize) {
        return;
    }

    buf.fill(0); // fully transparent (alpha 0 everywhere)

    fill_rect_opaque(buf, stride, box2d.x, box2d.y, box2d.w, box2d.h, config::BG_COLOR);

    let row_h = config::ROW_HEIGHT;
    let rise = glyph_rise(font);
    draw_search_row(buf, stride, box2d, row_h, font, query, rise);

    for (i, row) in results.iter().enumerate() {
        let y0 = box2d.y + row_h * (1 + i as u32);
        if y0 >= box2d.y + box2d.h {
            break;
        }
        if row.selected {
            fill_rect_opaque(buf, stride, box2d.x, y0, box2d.w, row_h, config::SELECTED_BG_COLOR);
        }
        let target_bottom = row_target_bottom(row_h, rise);
        draw_line(
            buf,
            stride,
            screen_w as i32,
            screen_h,
            font,
            box2d.x as i32 + PAD,
            y0 as i32 + target_bottom,
            row.label,
            config::FG_COLOR,
        );
    }

}

fn draw_search_row(
    buf: &mut [u8],
    stride: usize,
    box2d: &Box2D,
    row_h: u32,
    font: &Font,
    query: &str,
    rise: i32,
) {
    let target_bottom = row_target_bottom(row_h, rise);
    if !query.is_empty() {
        draw_line(
            buf,
            stride,
            (box2d.x + box2d.w) as i32,
            box2d.y + box2d.h,
            font,
            box2d.x as i32 + PAD,
            box2d.y as i32 + target_bottom,
            query,
            config::FG_COLOR,
        );
    }

    let cursor_x = box2d.x as i32 + PAD + text_width(font, query);
    let cursor_h = config::FONT_SIZE as u32;
    let cursor_y0 = box2d.y + (row_h - cursor_h) / 2 + 2;
    fill_rect_opaque(buf, stride, cursor_x.max(0) as u32, cursor_y0, 2, cursor_h, config::FG_COLOR);

    let sep_y = box2d.y + row_h - 1;
    fill_rect_opaque(buf, stride, box2d.x, sep_y, box2d.w, 1, config::BORDER_COLOR);
}

/// Baseline position (as an offset from the row's top) that vertically
/// centers a row of text `rise` pixels tall — the same for every row,
/// computed from the fixed glyph sample rather than each row's own text.
fn row_target_bottom(row_h: u32, rise: i32) -> i32 {
    ((row_h as i32) + rise) / 2 - 1
}

fn text_width(font: &Font, s: &str) -> i32 {
    s.chars()
        .map(|c| font.metrics(c, config::FONT_SIZE).advance_width.ceil() as i32)
        .sum()
}

fn argb_bytes(color: u32) -> [u8; 4] {
    [
        (color & 0xFF) as u8,
        ((color >> 8) & 0xFF) as u8,
        ((color >> 16) & 0xFF) as u8,
        0xFF,
    ]
}

fn fill_rect_opaque(buf: &mut [u8], stride: usize, x0: u32, y0: u32, w: u32, h: u32, color: u32) {
    let [b, g, r, a] = argb_bytes(color);
    let width = (stride / 4) as u32;
    let x1 = (x0 + w).min(width);
    for y in y0..y0 + h {
        let row_start = y as usize * stride;
        if row_start >= buf.len() {
            break;
        }
        for x in x0..x1 {
            let idx = row_start + (x as usize) * 4;
            buf[idx] = b;
            buf[idx + 1] = g;
            buf[idx + 2] = r;
            buf[idx + 3] = a;
        }
    }
}

fn draw_line(
    buf: &mut [u8],
    stride: usize,
    width: i32,
    height: u32,
    font: &Font,
    start_x: i32,
    target_bottom: i32,
    s: &str,
    color: u32,
) {
    let [fg_b, fg_g, fg_r, _] = argb_bytes(color);
    let h = height as i32;
    let mut x = start_x;
    for c in s.chars() {
        let (metrics, bitmap) = font.rasterize(c, config::FONT_SIZE);
        let glyph_h = metrics.height as i32;
        let origin_y = target_bottom - metrics.ymin - (glyph_h - 1);
        for (i, &alpha) in bitmap.iter().enumerate() {
            if alpha == 0 {
                continue;
            }
            let bx = i % metrics.width;
            let by = i / metrics.width;
            let px_x = x + metrics.xmin + bx as i32;
            let px_y = origin_y + metrics.ymin + by as i32;
            if px_x >= 0 && px_x < width && px_y >= 0 && px_y < h {
                let idx = (px_y as usize * stride) + (px_x as usize) * 4;
                // Anti-aliasing: blend the glyph color into whatever is
                // already in the buffer (background or highlighted row),
                // weighted by the rasterizer's coverage (alpha) for this
                // pixel, instead of a hard on/off stamp — that's what was
                // making text look jagged.
                let a = alpha as u32;
                let blend = |fg: u8, bg: u8| ((fg as u32 * a + bg as u32 * (255 - a)) / 255) as u8;
                buf[idx] = blend(fg_b, buf[idx]);
                buf[idx + 1] = blend(fg_g, buf[idx + 1]);
                buf[idx + 2] = blend(fg_r, buf[idx + 2]);
                buf[idx + 3] = 0xFF;
            }
        }
        x += metrics.advance_width.ceil() as i32;
    }
}

#[cfg(test)]
mod render_debug {
    use super::*;

    /// Not run by default (`cargo test -- --ignored` to run it). Renders a
    /// sample window to a PPM for visual inspection — e.g. `convert
    /// /tmp/launcher_render_debug.ppm /tmp/launcher_render_debug.png`. Useful
    /// whenever a rendering bug (baseline, glyph shape, alignment) is
    /// suspected and needs eyeballing without launching the real Wayland
    /// window.
    #[test]
    #[ignore]
    fn dump_ppm() {
        let font = load_font().unwrap();
        let width = 400u32;
        let height = 200u32;
        let mut buf = vec![0u8; (width * height * 4) as usize];
        let box2d = Box2D { x: 0, y: 0, w: width, h: height };
        let rows = [
            ResultRow { label: "gpqjy Applications gnome", selected: false },
            ResultRow { label: "Firefox Telegram Signal", selected: true },
        ];
        draw_window(&mut buf, width, height, &box2d, &font, "search gxyz", &rows);

        use std::io::Write;
        let mut f = std::fs::File::create("/tmp/launcher_render_debug.ppm").unwrap();
        f.write_all(format!("P6\n{} {}\n255\n", width, height).as_bytes()).unwrap();
        for chunk in buf.chunks(4) {
            let (b, g, r, _a) = (chunk[0], chunk[1], chunk[2], chunk[3]);
            f.write_all(&[r, g, b]).unwrap();
        }
    }
}
