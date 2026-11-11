//! The screen as cells, what each element draws into them, and the bytes that bring a
//! terminal from one grid to the next.

use std::collections::HashMap;
use std::io::Write;

use crate::color::{Color, srgb_encode};
use crate::runtime::{CellBorder, CellGlyph, CellLine, CellPaint, CellText};

pub(super) type Rgb = [u8; 3];

pub(super) fn rgb(c: Color) -> Rgb {
    [c.r, c.g, c.b].map(|v| (srgb_encode(v.clamp(0.0, 1.0)) * 255.0).round() as u8)
}

/// `over` laid on `under` at `alpha`, in sRGB as the terminal shows them.
pub(super) fn mix(under: Rgb, over: Rgb, alpha: f32) -> Rgb {
    std::array::from_fn(|i| (f32::from(under[i]) + (f32::from(over[i]) - f32::from(under[i])) * alpha.clamp(0.0, 1.0)).round() as u8)
}

/// What a cell shows: a character, a cluster of several (by its index in `Strings`), or the
/// second half of a wide one before it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ch {
    One(char),
    Many(u32),
    Wide,
}

/// SGR attributes, one bit each.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Attrs(u8);

impl Attrs {
    const BOLD: u8 = 1;
    const ITALIC: u8 = 2;
    const UNDERLINE: u8 = 4;
    const STRIKE: u8 = 8;
    fn has(self, bit: u8) -> bool { self.0 & bit != 0 }
}

/// One cell. `under` is the underline's colour; `link` a hyperlink's index in `Strings`, 0 for
/// none.
#[derive(Clone, Copy, PartialEq)]
struct Cell {
    ch: Ch,
    fg: Rgb,
    bg: Rgb,
    attrs: Attrs,
    under: Rgb,
    link: u32,
}

impl Cell {
    fn blank(bg: Rgb) -> Cell {
        Cell { ch: Ch::One(' '), fg: [255; 3], bg, attrs: Attrs::default(), under: [0; 3], link: 0 }
    }
}

/// Clusters and link URLs the cells point at, kept across frames so equal cells compare equal.
#[derive(Default)]
pub(super) struct Strings {
    list: Vec<Box<str>>,
    index: HashMap<Box<str>, u32>,
}

impl Strings {
    /// Entries kept before starting over (with a full redraw).
    const MAX: usize = 4096;

    fn id(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.index.get(s) { return i; }
        self.list.push(s.into());
        let i = self.list.len() as u32;
        self.index.insert(s.into(), i);
        i
    }

    fn get(&self, i: u32) -> &str {
        &self.list[i as usize - 1]
    }

    /// Too many to keep: the caller clears them and redraws everything.
    pub(super) fn full(&self) -> bool {
        self.list.len() > Self::MAX
    }
}

/// Straight-alpha sRGB pixels an image's cells sample.
pub(super) struct Texels<'a> {
    pub(super) w: u32,
    pub(super) h: u32,
    pub(super) rgba: &'a [u8],
}

impl Texels<'_> {
    /// The mean colour and alpha of the texels under `[u0, v0, u1, v1]` (fractions of the image).
    fn mean(&self, [u0, v0, u1, v1]: [f32; 4]) -> (Rgb, f32) {
        let span = |a: f32, b: f32, n: u32| {
            let lo = ((a * n as f32).floor().max(0.0) as u32).min(n - 1);
            (lo, ((b * n as f32).ceil() as u32).clamp(lo + 1, n))
        };
        let ((x0, x1), (y0, y1)) = (span(u0, u1, self.w), span(v0, v1, self.h));
        let (mut sum, mut alpha) = ([0.0f32; 3], 0.0f32);
        for y in y0..y1 {
            for x in x0..x1 {
                let p = &self.rgba[((y * self.w + x) * 4) as usize..][..4];
                let a = f32::from(p[3]) / 255.0;
                for k in 0..3 { sum[k] += f32::from(p[k]) * a; }
                alpha += a;
            }
        }
        let n = ((x1 - x0) * (y1 - y0)) as f32;
        if alpha <= 0.0 { return ([0; 3], 0.0); }
        (sum.map(|v| (v / alpha).round() as u8), alpha / n)
    }
}

/// The screen as cells.
pub(super) struct Grid {
    pub(super) w: i32,
    pub(super) h: i32,
    cells: Vec<Cell>,
}

impl Grid {
    pub(super) fn new(w: u32, h: u32, bg: Rgb) -> Grid {
        Grid { w: w as i32, h: h as i32, cells: vec![Cell::blank(bg); (w * h) as usize] }
    }

    /// The cells of `[x0, y0, x1, y1]` (units, rounded to cells) inside `clip` and the screen.
    fn span(&self, r: [f32; 4], clip: [f32; 4]) -> [i32; 4] {
        let c = |v: f32| v.round() as i32;
        [c(r[0].max(clip[0])).max(0), c(r[1].max(clip[1])).max(0), c(r[2].min(clip[2])).min(self.w), c(r[3].min(clip[3])).min(self.h)]
    }

    fn at(&mut self, x: i32, y: i32) -> &mut Cell {
        &mut self.cells[(y * self.w + x) as usize]
    }

    /// Clears the halves of wide characters that a write to `x` breaks.
    fn unsplit(&mut self, x: i32, y: i32) {
        if self.at(x, y).ch == Ch::Wide && x > 0 { self.at(x - 1, y).ch = Ch::One(' '); }
        if x + 1 < self.w && self.at(x + 1, y).ch == Ch::Wide { self.at(x + 1, y).ch = Ch::One(' '); }
    }

    /// `color` over the cells of `[x0, y0, x1, y1]`: an opaque one replaces them, a translucent
    /// one (a modal's backdrop) tints them and their text.
    fn fill(&mut self, [x0, y0, x1, y1]: [i32; 4], color: Color) {
        let c = rgb(color);
        for y in y0..y1 {
            for x in x0..x1 {
                if color.a >= 0.99 {
                    *self.at(x, y) = Cell::blank(c);
                } else {
                    let cell = self.at(x, y);
                    cell.bg = mix(cell.bg, c, color.a);
                    cell.fg = mix(cell.fg, c, color.a);
                    cell.under = mix(cell.under, c, color.a);
                }
            }
        }
    }

    pub(super) fn draw(&mut self, p: &CellPaint, strings: &mut Strings, image: Option<Texels>) {
        let cells = self.span(p.rect, p.clip);
        let [x0, y0, x1, y1] = cells;
        for &c in &p.fills { self.fill(cells, c); }
        if let Some([top, bottom]) = p.gradient {
            let h = (p.rect[3] - p.rect[1]).max(1.0);
            for y in y0..y1 {
                let t = ((y as f32 + 0.5 - p.rect[1]) / h).clamp(0.0, 1.0);
                self.fill([x0, y, x1, y + 1], top.mix(bottom, t));
            }
        }
        if let (Some(img), Some(t)) = (&p.image, image) { self.image(img, cells, &t); }
        if let Some(b) = &p.border { self.border(p, b); }
        for g in &p.glyphs { self.glyph(g, cells, strings); }
    }

    /// An image as half blocks: each cell's top half in its foreground, its bottom half in its
    /// background.
    fn image(&mut self, img: &crate::runtime::CellImage, [x0, y0, x1, y1]: [i32; 4], t: &Texels) {
        let [dx, dy, dw, dh] = img.dest;
        let [u0, v0, u1, v1] = img.window;
        if dw <= 0.0 || dh <= 0.0 { return; }
        let (u, v) = (|x: f32| u0 + (x - dx) / dw * (u1 - u0), |y: f32| v0 + (y - dy) / dh * (v1 - v0));
        let cut = |a: f32, b: f32| (a.max(0.0), b.min(1.0));
        for y in y0.max(dy.floor() as i32)..y1.min((dy + dh).ceil() as i32) {
            for x in x0.max(dx.floor() as i32)..x1.min((dx + dw).ceil() as i32) {
                let (ua, ub) = cut(u(x as f32), u(x as f32 + 1.0));
                let half = |k: f32| {
                    let (va, vb) = cut(v(y as f32 + k * 0.5), v(y as f32 + k * 0.5 + 0.5));
                    if ua >= ub || va >= vb { ([0; 3], 0.0) } else { t.mean([ua, va, ub, vb]) }
                };
                let ((top, ta), (bottom, ba)) = (half(0.0), half(1.0));
                self.unsplit(x, y);
                let cell = self.at(x, y);
                let bg = cell.bg;
                *cell = Cell { ch: Ch::One('▀'), fg: mix(bg, top, ta * img.opacity), ..Cell::blank(mix(bg, bottom, ba * img.opacity)) };
            }
        }
    }

    /// The border's sides on the rect's outer cells, rounded at the corners when the box is.
    fn border(&mut self, p: &CellPaint, b: &CellBorder) {
        let c = |v: f32| v.round() as i32;
        let [rx0, ry0, rx1, ry1] = [c(p.rect[0]), c(p.rect[1]), c(p.rect[2]) - 1, c(p.rect[3]) - 1];
        let [x0, y0, x1, y1] = self.span(p.rect, p.clip);
        let [top, right, bottom, left] = b.sides;
        // Corners top-left, top-right, bottom-left, bottom-right; then across and down.
        let (corners, across, down) = match b.line {
            CellLine::Heavy => (['┏', '┓', '┗', '┛'], '━', '┃'),
            CellLine::Dashed => (if b.round { ['╭', '╮', '╰', '╯'] } else { ['┌', '┐', '└', '┘'] }, '┄', '┆'),
            CellLine::Light => (if b.round { ['╭', '╮', '╰', '╯'] } else { ['┌', '┐', '└', '┘'] }, '─', '│'),
        };
        for y in y0..y1 {
            for x in x0..x1 {
                let (t, bo, l, r) = (top && y == ry0, bottom && y == ry1, left && x == rx0, right && x == rx1);
                let ch = match (t, bo, l, r) {
                    (true, _, true, _) => corners[0],
                    (true, _, _, true) => corners[1],
                    (_, true, true, _) => corners[2],
                    (_, true, _, true) => corners[3],
                    (true, ..) | (_, true, ..) => across,
                    (_, _, true, _) | (_, _, _, true) => down,
                    _ => continue,
                };
                self.unsplit(x, y);
                let cell = self.at(x, y);
                let fg = mix(cell.bg, rgb(b.color), b.color.a);
                *cell = Cell { ch: Ch::One(ch), fg, ..Cell::blank(cell.bg) };
            }
        }
    }

    fn glyph(&mut self, g: &CellGlyph, [x0, y0, x1, y1]: [i32; 4], strings: &mut Strings) {
        let (x, y) = (g.at[0].round() as i32, g.at[1].round() as i32);
        let inside = |x: i32| x >= x0 && x < x1 && y >= y0 && y < y1;
        let wide = g.width == 2;
        if !inside(x) || wide && !inside(x + 1) { return; }
        let ch = match &g.text {
            CellText::One(c) => Ch::One(*c),
            CellText::Many(s) => Ch::Many(strings.id(s)),
        };
        let link = g.link.as_deref().map_or(0, |u| strings.id(u));
        let s = g.style;
        let attrs = Attrs([(s.bold, Attrs::BOLD), (s.italic, Attrs::ITALIC), (s.underline.is_some(), Attrs::UNDERLINE), (s.strike, Attrs::STRIKE)]
            .iter().fold(0, |a, &(on, bit)| if on { a | bit } else { a }));
        self.unsplit(x, y);
        if wide { self.unsplit(x + 1, y); }
        for k in 0..1 + i32::from(wide) {
            let cell = self.at(x + k, y);
            let bg = g.bg.map_or(cell.bg, |c| mix(cell.bg, rgb(c), c.a));
            let fg = mix(bg, rgb(g.color), g.color.a);
            let under = s.underline.map_or([0; 3], |c| mix(bg, rgb(c), c.a));
            *cell = Cell { ch: if k == 0 { ch } else { Ch::Wide }, fg, bg, attrs, under, link };
        }
    }
}

/// The caret: its cell and colour.
pub(super) type Caret = ((i32, i32), Rgb);

/// The bytes that turn `prev` (None: whatever is on screen) into `next`: only changed cells,
/// one cursor move per run of them, a style change only where the pen changes, hyperlinks
/// opened and closed around their cells. Then the caret, as the terminal's bar cursor in its
/// colour, or no cursor.
pub(super) fn encode(prev: Option<&Grid>, next: &Grid, strings: &Strings, caret: Option<Caret>, out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[?2026h");
    let mut pen: Option<(Rgb, Rgb, Attrs, Rgb)> = None;
    let mut link = 0;
    let mut cursor: Option<(i32, i32)> = None;
    for y in 0..next.h {
        for x in 0..next.w {
            let i = (y * next.w + x) as usize;
            let cell = &next.cells[i];
            if cell.ch == Ch::Wide || prev.is_some_and(|p| p.cells[i] == *cell) { continue; }
            if cursor != Some((x, y)) { let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1); }
            if cell.link != link {
                link = cell.link;
                let url = if link == 0 { "" } else { strings.get(link) };
                let _ = write!(out, "\x1b]8;;{url}\x1b\\");
            }
            let style = (cell.fg, cell.bg, cell.attrs, cell.under);
            if pen != Some(style) {
                out.extend_from_slice(b"\x1b[0");
                for (bit, code) in [(Attrs::BOLD, ";1"), (Attrs::ITALIC, ";3"), (Attrs::UNDERLINE, ";4"), (Attrs::STRIKE, ";9")] {
                    if cell.attrs.has(bit) { out.extend_from_slice(code.as_bytes()); }
                }
                let ([fr, fg, fb], [br, bg, bb]) = (cell.fg, cell.bg);
                let _ = write!(out, ";38;2;{fr};{fg};{fb};48;2;{br};{bg};{bb}");
                if cell.attrs.has(Attrs::UNDERLINE) && cell.under != cell.fg {
                    let [ur, ug, ub] = cell.under;
                    let _ = write!(out, ";58;2;{ur};{ug};{ub}");
                }
                out.push(b'm');
                pen = Some(style);
            }
            match cell.ch {
                Ch::One(c) => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
                Ch::Many(k) => out.extend_from_slice(strings.get(k).as_bytes()),
                Ch::Wide => {}
            }
            let wide = x + 1 < next.w && next.cells[i + 1].ch == Ch::Wide;
            cursor = Some((x + 1 + i32::from(wide), y));
        }
    }
    if link != 0 { out.extend_from_slice(b"\x1b]8;;\x1b\\"); }
    match caret {
        Some(((x, y), [r, g, b])) => { let _ = write!(out, "\x1b]12;#{r:02x}{g:02x}{b:02x}\x07\x1b[{};{}H\x1b[6 q\x1b[?25h", y + 1, x + 1); }
        None => out.extend_from_slice(b"\x1b[?25l"),
    }
    out.extend_from_slice(b"\x1b[?2026l");
}
