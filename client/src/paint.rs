//! Small drawing kit on top of tiny-skia: rounded shapes and cached text.

use std::collections::HashMap;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};

const FONT_DIR: &str = "/usr/share/fonts/noto-cjk";
/// Face index of Simplified Chinese inside the Noto CJK collections.
const SC_INDEX: u32 = 2;

pub fn rgba(hex: u32, a: f32) -> Color {
    Color::from_rgba8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8, (a.clamp(0.0, 1.0) * 255.0) as u8)
}

pub fn rrect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Path {
    let r = r.min(w / 2.0).min(h / 2.0);
    let k = 0.552_284_8 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish().unwrap()
}

pub fn circle(cx: f32, cy: f32, r: f32) -> Path {
    PathBuilder::from_circle(cx, cy, r.max(0.1)).unwrap()
}

pub struct Canvas {
    pub pm: Pixmap,
    /// Design-space (1920×1080) to pixels.
    pub ts: Transform,
    pub scale: f32,
    pub ox: f32,
    pub oy: f32,
}

impl Canvas {
    pub fn fill(&mut self, path: &Path, color: Color) {
        let paint = Paint { anti_alias: true, ..paint(color) };
        self.pm.fill_path(path, &paint, FillRule::Winding, self.ts, None);
    }
    pub fn stroke(&mut self, path: &Path, color: Color, width: f32) {
        let st = Stroke { width, line_cap: tiny_skia::LineCap::Round, ..Stroke::default() };
        self.pm.stroke_path(path, &paint(color), &st, self.ts, None);
    }
}

fn paint(color: Color) -> Paint<'static> {
    let mut p = Paint { anti_alias: true, ..Paint::default() };
    p.set_color(color);
    p
}

#[derive(Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum Align {
    Left,
    Center,
    Right,
}

struct Glyph {
    left: i32,
    top: i32,
    w: usize,
    h: usize,
    cov: Vec<u8>,
}

pub struct Text {
    fonts: [FontVec; 2],
    cache: HashMap<(usize, char, u32), Option<Glyph>>,
}

impl Text {
    pub fn load() -> Result<Self, String> {
        let load = |p: &str| -> Result<FontVec, String> {
            let data = std::fs::read(p).map_err(|e| format!("{p}: {e}"))?;
            FontVec::try_from_vec_and_index(data, SC_INDEX).map_err(|e| format!("{p}: {e}"))
        };
        let dir = std::env::var("FLIPAD_FONT_DIR").unwrap_or_else(|_| FONT_DIR.into());
        let (regular, bold) = (format!("{dir}/NotoSansCJK-Regular.ttc"), format!("{dir}/NotoSansCJK-Bold.ttc"));
        Ok(Self { fonts: [load(&regular)?, load(&bold)?], cache: HashMap::new() })
    }

    /// Width in design units.
    pub fn measure(&self, s: &str, size: f32, bold: bool) -> f32 {
        let f = self.fonts[bold as usize].as_scaled(PxScale::from(size));
        s.chars().map(|c| f.h_advance(f.glyph_id(c))).sum()
    }

    /// Draws `s` with its baseline at (x, y) in design units.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(&mut self, cv: &mut Canvas, s: &str, x: f32, y: f32, size: f32, color: Color, bold: bool, align: Align) {
        let width = self.measure(s, size, bold);
        let x = match align {
            Align::Left => x,
            Align::Center => x - width / 2.0,
            Align::Right => x - width,
        };
        let px = (size * cv.scale).round().max(1.0) as u32;
        let fi = bold as usize;
        let mut pen = x * cv.scale + cv.ox;
        let base = (y * cv.scale + cv.oy).round() as i32;
        for c in s.chars() {
            let font = &self.fonts[fi];
            let scaled = font.as_scaled(PxScale::from(px as f32));
            let advance = scaled.h_advance(scaled.glyph_id(c));
            let g = self.cache.entry((fi, c, px)).or_insert_with(|| raster(font, c, px));
            if let Some(g) = g {
                blend(&mut cv.pm, pen.round() as i32 + g.left, base + g.top, g, color);
            }
            pen += advance;
        }
    }
}

fn raster(font: &FontVec, c: char, px: u32) -> Option<Glyph> {
    let glyph = font.glyph_id(c).with_scale_and_position(px as f32, ab_glyph::point(0.0, 0.0));
    let outline = font.outline_glyph(glyph)?;
    let b = outline.px_bounds();
    let (w, h) = (b.width() as usize, b.height() as usize);
    let mut cov = vec![0u8; w * h];
    outline.draw(|x, y, v| {
        if let Some(p) = cov.get_mut(y as usize * w + x as usize) {
            *p = (v.clamp(0.0, 1.0) * 255.0) as u8;
        }
    });
    Some(Glyph { left: b.min.x as i32, top: b.min.y as i32, w, h, cov })
}

/// Source-over blend of a coverage mask into the premultiplied pixmap.
fn blend(pm: &mut Pixmap, x0: i32, y0: i32, g: &Glyph, color: Color) {
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let c = color.premultiply().to_color_u8();
    let data = pm.data_mut();
    for gy in 0..g.h as i32 {
        let y = y0 + gy;
        if y < 0 || y >= ph {
            continue;
        }
        for gx in 0..g.w as i32 {
            let x = x0 + gx;
            if x < 0 || x >= pw {
                continue;
            }
            let cov = g.cov[(gy as usize) * g.w + gx as usize] as u32;
            if cov == 0 {
                continue;
            }
            let i = ((y * pw + x) * 4) as usize;
            let a = c.alpha() as u32 * cov / 255;
            let inv = 255 - a;
            let src = [c.red() as u32, c.green() as u32, c.blue() as u32, c.alpha() as u32];
            for k in 0..4 {
                let s = src[k] * cov / 255;
                data[i + k] = (s + data[i + k] as u32 * inv / 255) as u8;
            }
        }
    }
}
