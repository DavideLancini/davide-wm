//! Glyphs for the terminal: fonts mapped from disk (shared with every other
//! program using them, nothing parsed up front), rasterised on first use
//! into one alpha atlas that all terminal windows share.

use std::collections::HashMap;
use std::os::fd::AsRawFd;

use ab_glyph::{Font, FontRef, GlyphId, PxScale, ScaleFont};
use render::{Quad, Renderer, Texture};

/// A font file mapped read-only.
struct Face {
    font: FontRef<'static>,
    map: (*mut libc::c_void, usize),
}

impl Face {
    fn open(path: &str) -> Option<Face> {
        let file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len() as usize;
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0) };
        if ptr == libc::MAP_FAILED {
            return None;
        }
        // The mapping outlives the font: both go in `Drop` below.
        let bytes: &'static [u8] = unsafe { std::slice::from_raw_parts(ptr.cast(), len) };
        match FontRef::try_from_slice(bytes) {
            Ok(font) => Some(Face { font, map: (ptr, len) }),
            Err(_) => {
                unsafe { libc::munmap(ptr, len) };
                None
            }
        }
    }
}

impl Drop for Face {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.map.0, self.map.1) };
    }
}

/// Where a glyph is in the atlas and where it goes relative to its cell's
/// top-left corner.
#[derive(Clone, Copy)]
pub struct Slot {
    pub src: Quad,
    pub dx: f32,
    pub dy: f32,
}

const ATLAS: i32 = 2048;

pub struct Glyphs {
    faces: Vec<Face>,
    bold: Option<Face>,
    scale: PxScale,
    /// Cell width and height in pixels.
    pub cell: (i32, i32),
    baseline: f32,
    atlas: Option<Texture>,
    /// Shelf packing: current x, shelf top, shelf height.
    shelf: (i32, i32, i32),
    cache: HashMap<(char, bool, bool), Option<Slot>>,
}

impl Glyphs {
    pub fn new() -> Option<Glyphs> {
        let faces: Vec<Face> = config::TERM_FONTS.iter().filter_map(|p| Face::open(p)).collect();
        let main = faces.first()?;
        let scale = main.font.pt_to_px_scale(config::TERM_FONT_PT)?;
        let scaled = main.font.as_scaled(scale);
        let advance = scaled.h_advance(main.font.glyph_id('M'));
        let height = scaled.ascent() - scaled.descent() + scaled.line_gap();
        Some(Glyphs {
            bold: Face::open(config::TERM_FONT_BOLD),
            scale,
            cell: (advance.round().max(1.0) as i32, height.ceil().max(1.0) as i32),
            baseline: (scaled.ascent() + scaled.line_gap() / 2.0).round(),
            faces,
            atlas: None,
            shelf: (0, 0, 0),
            cache: HashMap::new(),
        })
    }

    pub fn atlas(&self) -> Option<&Texture> {
        self.atlas.as_ref()
    }

    /// The atlas went with the GL context (live upgrade): start again.
    pub fn forget_atlas(&mut self) {
        self.atlas = None;
        self.cache.clear();
        self.shelf = (0, 0, 0);
    }

    /// The glyph of `c`, rasterised into the atlas on first use; `wide`
    /// glyphs get two cells.
    pub fn get(&mut self, r: &mut Renderer, c: char, bold: bool, wide: bool) -> Option<Slot> {
        if let Some(s) = self.cache.get(&(c, bold, wide)) {
            return *s;
        }
        let slot = self.rasterize(r, c, bold, wide);
        self.cache.insert((c, bold, wide), slot);
        slot
    }

    fn face_for(&self, c: char, bold: bool) -> Option<(&Face, GlyphId)> {
        let bold = self.bold.as_ref().filter(|_| bold).map(|f| (f, f.font.glyph_id(c))).filter(|(_, g)| g.0 != 0);
        bold.or_else(|| self.faces.iter().map(|f| (f, f.font.glyph_id(c))).find(|(_, g)| g.0 != 0))
    }

    fn rasterize(&mut self, r: &mut Renderer, c: char, bold: bool, wide: bool) -> Option<Slot> {
        if c == ' ' {
            return None;
        }
        let (face, id) = self.face_for(c, bold)?;
        let room = self.cell.0 as f32 * if wide { 2.0 } else { 1.0 };
        // Glyphs wider than their cells (fallback fonts) are scaled down.
        let advance = face.font.as_scaled(self.scale).h_advance(id);
        let scale = if advance > room * 1.05 { PxScale { x: self.scale.x * room / advance, y: self.scale.y * room / advance } } else { self.scale };
        let outline = face.font.outline_glyph(id.with_scale_and_position(scale, ab_glyph::point(0.0, self.baseline)))?;
        let b = outline.px_bounds();
        let (w, h) = (b.width().ceil() as i32, b.height().ceil() as i32);
        if w <= 0 || h <= 0 {
            return None;
        }
        let mut px = vec![0u8; (w * h) as usize];
        outline.draw(|x, y, v| {
            if let Some(p) = px.get_mut((y as i32 * w + x as i32) as usize) {
                *p = (v.clamp(0.0, 1.0) * 255.0) as u8;
            }
        });
        let (x, y) = self.place(r, w, h)?;
        let atlas = self.atlas.as_ref()?;
        r.atlas_put(atlas, x, y, w, h, &px);
        Some(Slot { src: Quad { x: x as f32, y: y as f32, w: w as f32, h: h as f32 }, dx: b.min.x.round(), dy: b.min.y.round() })
    }

    /// Room for a `w`×`h` glyph; a full atlas starts over.
    fn place(&mut self, r: &mut Renderer, w: i32, h: i32) -> Option<(i32, i32)> {
        if self.atlas.is_none() {
            self.atlas = Some(r.create_atlas(ATLAS, ATLAS));
        }
        if w > ATLAS || h > ATLAS {
            return None;
        }
        let (mut x, mut top, mut height) = self.shelf;
        if x + w > ATLAS {
            (x, top, height) = (0, top + height + 1, 0);
        }
        if top + h > ATLAS {
            if let Some(t) = self.atlas.take() {
                r.destroy_texture(t);
            }
            self.forget_atlas();
            self.atlas = Some(r.create_atlas(ATLAS, ATLAS));
            (x, top, height) = (0, 0, 0);
        }
        self.shelf = (x + w + 1, top, height.max(h));
        Some((x, top))
    }
}
