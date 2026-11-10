//! `RUNE_TERM_CHECK=1`: checks damage against pixels. It keeps what the terminal should show,
//! built only from the rectangles sent, reads every frame back whole, and counts the frames
//! where the two differ. Slow (a full readback and compare per frame); for testing damage.

use super::damage::Rect;
use super::readback::{Patch, Readback};

#[derive(Default)]
pub(super) struct Check {
    readback: Option<Readback<()>>,
    /// What the terminal shows, RGBA.
    canvas: Vec<u8>,
    size: (u32, u32),
    pub(super) frames: u32,
    pub(super) wrong: u32,
    /// The most pixels wrong in one frame.
    pub(super) worst: usize,
}

impl Check {
    pub(super) fn from_env() -> Option<Check> {
        std::env::var_os("RUNE_TERM_CHECK").is_some().then(|| Check { readback: Some(Readback::new()), ..Check::default() })
    }

    /// Paints what was sent for a frame onto the canvas.
    pub(super) fn sent(&mut self, size: (u32, u32), patches: &[Patch]) {
        if self.size != size {
            self.size = size;
            self.canvas = vec![0; size.0 as usize * size.1 as usize * 4];
        }
        for p in patches {
            let line = p.rect.w as usize * 4;
            for y in 0..p.rect.h as usize {
                let at = ((p.rect.y as usize + y) * size.0 as usize + p.rect.x as usize) * 4;
                self.canvas[at..at + line].copy_from_slice(&p.rgba[y * line..(y + 1) * line]);
            }
        }
    }

    /// Records a copy of the whole frame; `compare` after the frame's patches were `sent`.
    pub(super) fn copy(&mut self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder, texture: &wgpu::Texture, size: (u32, u32)) {
        let readback = self.readback.as_mut().expect("Check::from_env");
        readback.copy(device, encoder, texture, vec![Rect::whole(size)], ());
    }

    pub(super) fn submitted(&mut self) {
        if let Some(r) = self.readback.as_mut() { r.submitted(); }
    }

    pub(super) fn compare(&mut self, device: &wgpu::Device) {
        let Some(readback) = self.readback.as_mut() else { return };
        for ((), patches) in readback.finish(device) {
            let Some(whole) = patches.first() else { continue };
            self.frames += 1;
            if whole.rgba.len() != self.canvas.len() { continue; }
            let wrong = whole.rgba.chunks_exact(4).zip(self.canvas.chunks_exact(4)).filter(|(a, b)| a[..3] != b[..3]).count();
            if wrong > 0 {
                self.wrong += 1;
                self.worst = self.worst.max(wrong);
            }
        }
    }
}
