//! What `--term cells` draws of a laid-out frame: each element in paint order as a character
//! grid can show it, in layout units, which there are cells. A fill becomes backgrounds (with
//! its hover, press and focus colours over it, and a vertical gradient row by row), a border a
//! line of box-drawing characters (heavy from two units, dashed when dashed), an image half
//! blocks, a scroll viewport that overflows a thumb on its right edge, and text characters with
//! their weight, slant, underline, strike, span and highlight backgrounds, selection and links.
//! Radius shows only as round corners; shadows, blur, vector shapes and icons have no cell form.
//! An element still in motion says so (`moving`), so its fills take fractional edges.
use super::*;
use parley::PositionedLayoutItem;
use std::rc::Rc;

/// One element, as a terminal's cells draw it.
pub(crate) struct CellPaint {
    /// `[x0, y0, x1, y1]`.
    pub(crate) rect: [f32; 4],
    /// What its ancestors let it draw into, `[x0, y0, x1, y1]`.
    pub(crate) clip: [f32; 4],
    /// Its fills, bottom first: its own, then hover, press and focus colours over it. Each has
    /// its opacity (its own and its ancestors') in the alpha.
    pub(crate) fills: Vec<Color>,
    /// A vertical gradient over the fills, top and bottom.
    pub(crate) gradient: Option<[Color; 2]>,
    pub(crate) border: Option<CellBorder>,
    pub(crate) image: Option<CellImage>,
    pub(crate) glyphs: Vec<CellGlyph>,
    /// It is on its way somewhere (a track of its own or an ancestor's is still running): its
    /// rect is fractional on purpose, and its fills draw their edges on eighths of a cell. At
    /// rest a rect is rounded to cells.
    pub(crate) moving: bool,
}

/// A border as a line of box-drawing characters on the rect's outer cells.
pub(crate) struct CellBorder {
    pub(crate) color: Color,
    /// Top, right, bottom, left.
    pub(crate) sides: [bool; 4],
    pub(crate) round: bool,
    pub(crate) line: CellLine,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellLine {
    Light,
    /// Two units wide or more.
    Heavy,
    Dashed,
}

/// An image placed in cells: `dest` is where it draws (`[x, y, w, h]`), `window` which part of
/// it shows (`[u0, v0, u1, v1]`).
pub(crate) struct CellImage {
    pub(crate) pixels: crate::Pixels,
    pub(crate) dest: [f32; 4],
    pub(crate) window: [f32; 4],
    pub(crate) opacity: f32,
}

/// What one cluster's cell shows beyond its character.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct CellStyle {
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) underline: Option<Color>,
    pub(crate) strike: bool,
}

/// One cluster at its cell's top-left corner.
pub(crate) struct CellGlyph {
    pub(crate) at: [f32; 2],
    /// The cluster, a space for a background or a line under no character.
    pub(crate) text: CellText,
    /// Cells it covers: 2 for a wide character, as its advance says.
    pub(crate) width: u8,
    pub(crate) color: Color,
    /// A background under it: a span's, a highlight's or the selection's.
    pub(crate) bg: Option<Color>,
    pub(crate) style: CellStyle,
    /// The URL a link points at, for the terminal's own hyperlinks.
    pub(crate) link: Option<Rc<str>>,
}

/// A cluster: one character, or several (an emoji sequence, a letter with its marks).
pub(crate) enum CellText {
    One(char),
    Many(Box<str>),
}

impl CellText {
    fn of(s: &str) -> CellText {
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => CellText::One(c),
            (None, _) => CellText::One(' '),
            _ => CellText::Many(s.into()),
        }
    }
}

/// Byte ranges of a text's drawn string and what they add to its cells.
enum Deco {
    /// A selection's background, with the ink it recolours the selected characters to.
    Select(Color, Option<Color>),
    Highlight(Color, Option<Color>),
    Strike,
    Link(Rc<str>),
}

/// A link id that is a URL a terminal can open (`scheme:` then more).
fn url_of(id: &str) -> Option<Rc<str>> {
    let (scheme, rest) = id.split_once(':')?;
    (!scheme.is_empty() && !rest.is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))).then(|| id.into())
}

/// One laid-out cluster: its cell (x from the text's start, the line's top), its byte range in
/// the drawn string, the cells it covers, and its style's ink (a span's index + 1, 0 for none).
struct Laid {
    at: [f32; 2],
    range: std::ops::Range<usize>,
    width: u8,
    ink: usize,
}

/// Every cluster of `layout` once, in visual order. A line's styled pieces (`GlyphRun`s) share
/// their shaping run, so each run is walked once and each cluster takes the piece it falls in.
/// Clusters go on whole cells as a terminal puts them: a wide one (advance over 1.25) takes two
/// and pushes what follows, though the font may have laid it narrower.
fn laid_clusters(layout: &parley::Layout<Ink>) -> Vec<Laid> {
    let mut out = Vec::new();
    for line in layout.lines() {
        let top = line.metrics().block_min_coord.round();
        let pieces: Vec<_> = line.items().filter_map(|item| match item {
            PositionedLayoutItem::GlyphRun(g) => Some(g),
            _ => None,
        }).collect();
        let mut col: Option<f32> = None;
        let mut k = 0;
        while k < pieces.len() {
            let run = pieces[k].run().index();
            let group: Vec<_> = pieces[k..].iter().take_while(|g| g.run().index() == run).collect();
            k += group.len();
            let start = group[0].offset();
            let mut cell = col.map_or(start.round(), |c| c.max(start.round()));
            let mut x = start;
            for cluster in group[0].run().visual_clusters() {
                let piece = group.iter().find(|g| x + 1e-3 >= g.offset() && x + 1e-3 < g.offset() + g.advance()).unwrap_or(group.last().unwrap_or(&group[0]));
                let width = if cluster.advance() > 1.25 { 2 } else { 1 };
                out.push(Laid { at: [cell, top], range: cluster.text_range(), width, ink: piece.style().brush.0 as usize });
                x += cluster.advance();
                cell += f32::from(width);
            }
            col = Some(cell);
        }
    }
    out
}

impl<S: 'static> Runtime<S> {
    /// The frame's elements in paint order (what is drawn later goes over what is drawn earlier).
    pub(crate) fn cell_paints(&self) -> Vec<CellPaint> {
        let n = self.nodes.len();
        let order: Vec<usize> = if self.layer_paint_order.len() == n { self.layer_paint_order.clone() } else { (0..n).collect() };
        let sel = self.selection_ranges();
        order.into_iter().filter_map(|i| self.cell_paint(i, &sel)).collect()
    }

    fn cell_paint(&self, i: usize, sel: &[(usize, usize, usize)]) -> Option<CellPaint> {
        let n = &self.nodes[i];
        let p = self.scene.presented(n.id)?;
        let d = self.drawn.get(i).copied().unwrap_or([p.rect.x, p.rect.y, p.rect.w, p.rect.h]);
        let opacity = self.inh_scratch.get(i).map_or(p.opacity, |h| h.opacity);
        if opacity <= 0.01 { return None; }
        let s = self.env.scale;
        let clip = self.screen_clip_of_parent(n.parent).map(|v| v / s);
        let rect = [d[0], d[1], d[0] + d[2], d[1] + d[3]];
        let fade = |c: Color| Color { a: c.a * opacity, ..c };

        // The fill, then what the GPU paint lays over it: focus, hover and press colours.
        let mut fills = vec![fade(self.color(n.id, P_FILL, n.vis.fill))];
        if let Some(c) = n.focus.highlight && self.highlight_visible(i) { fills.push(fade(c)); }
        if let Some(c) = n.focus.fill && self.focus_ring_visible(i) { fills.push(fade(c)); }
        let state = |prop, color: Color, by| (color.a > 0.0).then(|| self.scene.get(n.id, prop).unwrap_or(0.0).clamp(0.0, 1.0))
            .filter(|&v| v > 0.0).map(|v| Color { a: color.a * v * opacity, ..self.color(n.id, by, color) });
        fills.extend(state(P_HOVER, n.vis.hover(), P_HOVER_C));
        fills.extend(state(P_PRESS, n.vis.press(), P_PRESS_C));
        fills.retain(|c| c.a > 0.004);
        let gradient = n.vis.gradient.as_deref().map(|g| g.map(fade));

        let width = n.vis.border_width();
        let border_color = n.vis.border();
        let border = (width > 0.0 && border_color.a > 0.0).then(|| CellBorder {
            color: fade(border_color),
            sides: n.vis.border_sides.unwrap_or([width; 4]).map(|w| w > 0.0),
            round: p.radius.0[0] > 0.0,
            line: if n.vis.border_dash.is_some() { CellLine::Dashed } else if width >= 2.0 { CellLine::Heavy } else { CellLine::Light },
        });
        let image = n.vis.pixels.as_ref().map(|px| {
            let (dest, window) = crate::pixels::fit_in(n.vis.pixels_fit, n.vis.surface_place, d, px.size(), 1.0);
            CellImage { pixels: px.clone(), dest, window, opacity }
        });
        let mut glyphs = if n.vis.icon.is_some() { Vec::new() } else { self.cell_glyphs(i, rect, opacity, sel) };
        self.cell_scroll_thumb(i, rect, opacity, &mut glyphs);
        let moving = self.scene.rest(n.id).is_some();
        Some(CellPaint { rect, clip, fills, gradient, border, image, glyphs, moving })
    }

    /// A vertical scroll viewport whose content overflows shows where it is as a thumb in its
    /// last column, as OpenTUI's scroll box does.
    fn cell_scroll_thumb(&self, i: usize, rect: [f32; 4], opacity: f32, out: &mut Vec<CellGlyph>) {
        let n = &self.nodes[i];
        if n.scroll.is_none() || !n.scroll_axes.y() { return; }
        let Some(st) = self.scrolls.get(&n.id).filter(|st| st.max >= 0.5) else { return };
        let (top, h) = (rect[1].round(), (rect[3] - rect[1]).round());
        if h < 2.0 { return; }
        let len = (h * h / (h + st.max as f32)).round().clamp(1.0, h - 1.0);
        let at = ((st.offset / st.max).clamp(0.0, 1.0) as f32 * (h - len)).round();
        let v = if self.env.system_dark { 1.0 } else { 0.0 };
        let ink = Color { r: v, g: v, b: v, a: 0.35 * opacity };
        for k in 0..len as i32 {
            out.push(CellGlyph { at: [rect[2].round() - 1.0, top + at + k as f32], text: CellText::One('┃'), width: 1, color: ink, bg: None, style: CellStyle::default(), link: None });
        }
    }

    /// Where node `i`'s text starts, in whole cells: a centred text on an odd width starts on
    /// the cell before the half, and a scrolled field's text starts before its box.
    fn cell_text_origin(&self, i: usize, rect: [f32; 4]) -> Option<[f32; 2]> {
        let n = &self.nodes[i];
        let (spec, layout) = (n.text.as_ref()?, n.text_layout.as_ref()?);
        let (x0, x1) = (rect[0] + n.inset[0], rect[2] - n.inset[2]);
        let mut origin = [text_x(spec, x0, x1, layout.full_width().ceil(), crate::el::Flow::of_layout(layout)), rect[1] + n.inset[1]];
        if n.input.as_ref().is_some_and(|i| i.single_line) {
            origin[0] -= self.input_scroll.get(&n.path).copied().unwrap_or(0.0);
        }
        Some(origin.map(f32::floor))
    }

    /// What decorates byte ranges of node `i`'s drawn text: the selection (a field's own or a
    /// selectable text's), highlights, strikes and links.
    fn cell_decos(&self, i: usize, sel: &[(usize, usize, usize)]) -> Vec<(usize, usize, Deco)> {
        let n = &self.nodes[i];
        let Some(spec) = &n.text else { return Vec::new() };
        let laid = |a: usize, b: usize| n.elided.map_or((a, b), |e| e.range(a, b));
        let at = |a: u32, b: u32| laid(a as usize, b as usize);
        let mut out = Vec::new();
        let focused = self.window_focused != Some(false);
        if let Some(inp) = &n.input && inp.edit.focused && let (a, b) = inp.edit.range() && a != b {
            let (a, b) = inp.edit.display_selection_range(a, b);
            let (bg, ink) = inp.selection_style.map_or((inp.selection, None), |st| st.pick(focused));
            out.push((a, b, Deco::Select(bg, ink)));
        }
        if spec.selectable && let Some((a, b)) = selectable::range_in(sel, i) {
            let (a, b) = laid(a, b);
            let (bg, ink) = spec.selection_style.map_or((spec.selection, None), |st| st.pick(focused));
            out.push((a, b, Deco::Select(bg, ink)));
        }
        for h in &spec.highlights {
            let (a, b) = at(h.start, h.end);
            out.push((a, b, Deco::Highlight(h.bg, h.fg)));
        }
        for &(a, b, _) in &spec.strikes {
            let (a, b) = at(a, b);
            out.push((a, b, Deco::Strike));
        }
        for l in &spec.links {
            if let Some(url) = url_of(&l.id) {
                let (a, b) = at(l.start, l.end);
                out.push((a, b, Deco::Link(url)));
            }
        }
        out
    }

    /// A text's clusters where its layout puts them, or a field's placeholder when it's empty.
    fn cell_glyphs(&self, i: usize, rect: [f32; 4], opacity: f32, sel: &[(usize, usize, usize)]) -> Vec<CellGlyph> {
        let n = &self.nodes[i];
        let (Some(spec), Some(layout), Some(origin)) = (&n.text, &n.text_layout, self.cell_text_origin(i, rect)) else { return Vec::new() };
        let fade = |c: Color| Color { a: c.a * opacity, ..c };
        let base = fade(self.color(n.id, P_TEXT, spec.color));
        let mut out = Vec::new();
        if let Some(inp) = &n.input && inp.edit.text.is_empty() && !inp.placeholder.is_empty() {
            let style = CellStyle { italic: spec.style.italic, ..CellStyle::default() };
            for (k, ch) in inp.placeholder.chars().enumerate() {
                out.push(CellGlyph { at: [origin[0] + k as f32, origin[1]], text: CellText::One(ch), width: 1, color: fade(inp.placeholder_color), bg: None, style, link: None });
            }
            return out;
        }
        // What the layout was built from: the content, or its cut with "…" in the middle.
        let drawn: std::borrow::Cow<str> = match n.elided {
            Some(e) => format!("{}{}{}", &spec.content[..e.head], Elision::ELLIPSIS, &spec.content[e.tail..]).into(),
            None => spec.content.as_str().into(),
        };
        let decos = self.cell_decos(i, sel);
        for c in laid_clusters(layout) {
            let span = c.ink.checked_sub(1).and_then(|k| spec.spans.get(k));
            let weight = span.and_then(|s| s.weight).unwrap_or(spec.style.weight);
            let mut style = CellStyle {
                bold: weight >= 600.0,
                italic: spec.style.italic || span.is_some_and(|s| s.italic),
                underline: span.and_then(|s| s.underline).map(fade),
                strike: false,
            };
            let (mut color, mut bg, mut link) = (span.and_then(|s| s.color).map_or(base, fade), span.and_then(|s| s.bg).map(fade), None);
            for (a, b, deco) in &decos {
                if c.range.start < *a || c.range.start >= *b { continue; }
                match deco {
                    Deco::Select(fill, ink) | Deco::Highlight(fill, ink) => {
                        bg = Some(fade(*fill));
                        if let Some(ink) = ink { color = fade(*ink); }
                    }
                    Deco::Strike => style.strike = true,
                    Deco::Link(url) => link = Some(url.clone()),
                }
            }
            let text = drawn.get(c.range.clone()).unwrap_or(" ");
            let blank = text.chars().all(char::is_whitespace);
            if blank && bg.is_none() && style.underline.is_none() && !style.strike { continue; }
            let text = if blank { CellText::One(' ') } else { CellText::of(text) };
            out.push(CellGlyph { at: [origin[0] + c.at[0], origin[1] + c.at[1]], text, width: c.width, color, bg, style, link });
        }
        out
    }

    /// The focused field's caret, as the cell it sits before, from the same origin its
    /// characters are drawn from, with its colour. None while a range is selected.
    pub(crate) fn cell_caret(&self) -> Option<([f32; 2], Color)> {
        let i = (0..self.nodes.len()).find(|&i| self.nodes[i].input.as_ref().is_some_and(|inp| inp.edit.focused))?;
        let n = &self.nodes[i];
        let (inp, spec, layout) = (n.input.as_ref()?, n.text.as_ref()?, n.text_layout.as_ref()?);
        let (a, b) = inp.edit.range();
        if a != b { return None; }
        let d = self.drawn.get(i).copied()?;
        let origin = self.cell_text_origin(i, [d[0], d[1], d[0] + d[2], d[1] + d[3]])?;
        let color = if inp.caret_inked { inp.caret } else { self.color(n.id, P_TEXT, spec.color) };
        // The cell of the character at the caret's byte, or just past the one ending there.
        let at = caret_index(&inp.edit, &spec.content);
        let mut end = None;
        for c in laid_clusters(layout) {
            let cell = [origin[0] + c.at[0], origin[1] + c.at[1]];
            if c.range.start == at { return Some((cell, color)); }
            if c.range.end == at { end = Some([cell[0] + f32::from(c.width), cell[1]]); }
        }
        Some((end.unwrap_or(origin), color))
    }
}
