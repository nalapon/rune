//! What changed between two frames, from the instances Rune drew rather than their pixels.
//!
//! Each instance gets a key: its bytes, the bytes of the instance before it (so a reorder or an
//! insertion changes keys around it) and the layers it sits in. Keys in one frame and not the
//! other mark their instance's bounds as damaged, in the frame each belongs to. The bounds come
//! out conservative: an instance inside a blurred or turned layer damages that whole layer, and
//! a backdrop blur over any damage is damaged itself. The rectangles are then snapped to a
//! tile grid and merged so none overlap.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::render::{
    Instance, KIND_BACKDROP_BLUR, KIND_DASHED, KIND_EFFECT, KIND_GLYPH, KIND_GRADIENT, KIND_LAYER, KIND_LAYER_END,
    KIND_RADIAL, KIND_RECT, KIND_SHADOW,
};

/// A rectangle in device pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Rect {
    pub(super) x: u32,
    pub(super) y: u32,
    pub(super) w: u32,
    pub(super) h: u32,
}

impl Rect {
    pub(super) fn whole((w, h): (u32, u32)) -> Rect {
        Rect { x: 0, y: 0, w, h }
    }
}

/// `[x0, y0, x1, y1]` in device pixels.
type Bounds = [f32; 4];

/// Damage snaps to squares this many pixels wide.
const TILE: u32 = 32;
/// Past this share of the window, one whole-window rectangle is cheaper than many.
const WHOLE: f32 = 0.6;

/// The rectangles of `size` that differ between frames drawn from `old` and `new`, merged and
/// non-overlapping; empty when nothing changed.
pub(super) fn between(old: &[Instance], new_instances: &[Instance], size: (u32, u32)) -> Vec<Rect> {
    let full = [0.0, 0.0, size.0 as f32, size.1 as f32];
    let (old, new) = (entries(old, full), entries(new_instances, full));
    let mut unmatched: HashMap<u64, Vec<Bounds>> = HashMap::with_capacity(old.len());
    for (key, b) in &old { unmatched.entry(*key).or_default().push(*b); }
    let mut damage = Vec::new();
    for (key, b) in &new {
        match unmatched.get_mut(key).and_then(Vec::pop) {
            Some(_) => {}
            None => damage.push(*b),
        }
    }
    damage.extend(unmatched.into_values().flatten());
    damage.retain(|b| b[2] > b[0] && b[3] > b[1]);
    if !damage.is_empty() { spread_backdrops(new_instances, &mut damage); }
    tiles(&damage, size)
}

/// `rects` as `between` gives them, from several frames, merged the same way: what those
/// frames changed together.
pub(super) fn merge(rects: &[Rect], size: (u32, u32)) -> Vec<Rect> {
    let bounds: Vec<Bounds> = rects.iter().map(|r| [r.x as f32, r.y as f32, (r.x + r.w) as f32, (r.y + r.h) as f32]).collect();
    tiles(&bounds, size)
}

/// A backdrop blur shows what's under it, so damage within its reach damages the blur.
fn spread_backdrops(instances: &[Instance], damage: &mut Vec<Bounds>) {
    for inst in instances.iter().filter(|i| i.params[1] == KIND_BACKDROP_BLUR) {
        let reach = inst.params[0] * 3.0 + 1.0;
        let own = clip(grow(inst.rect, 1.0), inst.clip);
        let near = [own[0] - reach, own[1] - reach, own[2] + reach, own[3] + reach];
        if damage.iter().any(|b| overlaps(*b, near)) { damage.push(own); }
    }
}

/// Each instance's key and the pixels it may touch.
fn entries(instances: &[Instance], full: Bounds) -> Vec<(u64, Bounds)> {
    // Open layers: their bounds, whether they spread their contents (blur, a turn) and a hash of
    // the path to them.
    let mut layers: Vec<(Bounds, bool, u64)> = Vec::new();
    let mut prev: &[u8] = &[];
    let mut out = Vec::with_capacity(instances.len());
    for inst in instances {
        let bytes = bytemuck::bytes_of(inst);
        let kind = inst.params[1];
        let path = layers.last().map_or(0, |l| l.2);
        let spread = layers.iter().find(|l| l.1).map(|l| l.0);
        let bounds = if kind == KIND_LAYER {
            let blur = crate::render::layer_blur::sigma(inst.params[2]);
            let turned = inst.border[1] != 0.0;
            let own = if turned { full } else { clip(grow(inst.rect, (3.0 * blur).ceil() + 1.0), inst.clip) };
            let own = spread.unwrap_or(own);
            let mut h = std::hash::DefaultHasher::new();
            (path, bytes).hash(&mut h);
            layers.push((own, blur > 0.0 || turned, h.finish()));
            own
        } else if kind == KIND_LAYER_END {
            layers.pop().map_or(full, |l| l.0)
        } else {
            spread.unwrap_or_else(|| ink(inst))
        };
        let mut h = std::hash::DefaultHasher::new();
        (bytes, prev, path).hash(&mut h);
        out.push((h.finish(), bounds));
        prev = bytes;
    }
    out
}

/// The pixels one instance may draw, outside any layer that spreads them.
fn ink(inst: &Instance) -> Bounds {
    let mut rect = inst.rect;
    let kind = inst.params[1];
    if kind == KIND_GLYPH && inst.params[0] != 0.0 {
        let (sin, cos) = inst.params[0].sin_cos();
        let (w, h) = (cos.abs() * rect[2] + sin.abs() * rect[3], sin.abs() * rect[2] + cos.abs() * rect[3]);
        rect = [rect[0] + (rect[2] - w) * 0.5, rect[1] + (rect[3] - h) * 0.5, w, h];
    }
    let by = match kind {
        k if k == KIND_GLYPH => 1.0,
        k if k == KIND_SHADOW => inst.params[2] * 3.0 + 1.0,
        k if k == KIND_EFFECT => inst.params[2] + 1.0,
        k if k == KIND_RECT || k == KIND_GRADIENT || k == KIND_DASHED || k == KIND_RADIAL => 1.0 + inst.params[2] * 2.0,
        _ => 1.0,
    };
    clip(grow(rect, by), inst.clip)
}

/// `[x, y, w, h]` grown by `by` on every side, as bounds.
fn grow(r: [f32; 4], by: f32) -> Bounds {
    [r[0] - by, r[1] - by, r[0] + r[2] + by, r[1] + r[3] + by]
}

fn clip(b: Bounds, c: [f32; 4]) -> Bounds {
    [b[0].max(c[0]), b[1].max(c[1]), b[2].min(c[2]), b[3].min(c[3])]
}

fn overlaps(a: Bounds, b: Bounds) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

/// `damage` as tiles, merged into rectangles: runs along each tile row, then runs that line up
/// with the row above grow down into one. Clipped to `size`.
fn tiles(damage: &[Bounds], size: (u32, u32)) -> Vec<Rect> {
    if damage.is_empty() || size.0 == 0 || size.1 == 0 { return Vec::new(); }
    let (nx, ny) = (size.0.div_ceil(TILE), size.1.div_ceil(TILE));
    let mut hit = vec![false; (nx * ny) as usize];
    for b in damage {
        let cell = |v: f32, n: u32| (v.max(0.0) as u32 / TILE).min(n);
        let (x0, y0) = (cell(b[0], nx), cell(b[1], ny));
        let (x1, y1) = (cell(b[2].ceil() + (TILE - 1) as f32, nx), cell(b[3].ceil() + (TILE - 1) as f32, ny));
        for y in y0..y1 { for x in x0..x1 { hit[(y * nx + x) as usize] = true; } }
    }
    let count = hit.iter().filter(|h| **h).count();
    if count as f32 > WHOLE * hit.len() as f32 { return vec![Rect::whole(size)]; }
    // Rectangles in tiles; `open` reached the row above.
    let (mut done, mut open) = (Vec::new(), Vec::<Rect>::new());
    for y in 0..ny {
        let mut row = Vec::new();
        let mut x = 0;
        while x < nx {
            if !hit[(y * nx + x) as usize] { x += 1; continue; }
            let x0 = x;
            while x < nx && hit[(y * nx + x) as usize] { x += 1; }
            row.push(match open.iter().position(|r| r.x == x0 && r.w == x - x0) {
                Some(i) => { let r = open.swap_remove(i); Rect { h: r.h + 1, ..r } }
                None => Rect { x: x0, y, w: x - x0, h: 1 },
            });
        }
        done.append(&mut open);
        open = row;
    }
    done.append(&mut open);
    done.into_iter().map(|r| {
        let (x, y) = (r.x * TILE, r.y * TILE);
        Rect { x, y, w: (r.w * TILE).min(size.0 - x), h: (r.h * TILE).min(size.1 - y) }
    }).collect()
}
