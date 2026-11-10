//! Reading damaged rectangles back from the GPU without waiting on it: each frame copies only
//! its rectangles into one of a few staging buffers, and the CPU takes them once mapped, while
//! later frames render.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::damage::Rect;

/// Frames in flight before `copy` waits for the oldest.
pub(super) const DEPTH: usize = 3;

/// One rectangle's pixels, RGBA rows packed tight.
pub(super) struct Patch {
    pub(super) rect: Rect,
    pub(super) rgba: Vec<u8>,
}

struct Slot {
    buffer: Option<wgpu::Buffer>,
    ready: Arc<AtomicBool>,
}

/// Where a rectangle's rows sit in a staging buffer.
struct Region {
    rect: Rect,
    offset: u64,
    row: u32,
}

struct Pending<T> {
    slot: usize,
    tag: T,
    regions: Vec<Region>,
    bytes: u64,
    mapped: bool,
}

/// Frames' rectangles in flight, oldest first. `T` tags each frame.
pub(super) struct Readback<T> {
    slots: Vec<Slot>,
    next: usize,
    pending: VecDeque<Pending<T>>,
}

impl<T> Readback<T> {
    pub(super) fn new() -> Self {
        let slots = (0..DEPTH).map(|_| Slot { buffer: None, ready: Arc::new(AtomicBool::new(false)) }).collect();
        Readback { slots, next: 0, pending: VecDeque::new() }
    }

    pub(super) fn in_flight(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Frames that must finish before the next `copy` has a buffer (waits for the oldest when
    /// every buffer is in flight).
    pub(super) fn make_room(&mut self, device: &wgpu::Device) -> Vec<(T, Vec<Patch>)> {
        if self.pending.len() < self.slots.len() { return Vec::new(); }
        self.pop(device, true).into_iter().collect()
    }

    /// Records copies of `rects` of `texture` into the next buffer. Call `submitted` after the
    /// encoder is submitted.
    pub(super) fn copy(&mut self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder, texture: &wgpu::Texture, rects: Vec<Rect>, tag: T) {
        assert!(self.pending.len() < self.slots.len(), "Readback::make_room first");
        let slot = self.next;
        self.next = (self.next + 1) % self.slots.len();
        let mut bytes = 0;
        let regions: Vec<Region> = rects.into_iter().map(|rect| {
            let row = (rect.w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
            let region = Region { rect, offset: bytes, row };
            bytes += u64::from(row) * u64::from(rect.h);
            region
        }).collect();
        let buffer = &mut self.slots[slot].buffer;
        if buffer.as_ref().is_none_or(|b| b.size() < bytes) {
            *buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("term readback"),
                size: bytes.next_power_of_two().max(1 << 16),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }));
        }
        let buffer = buffer.as_ref().unwrap();
        for r in &regions {
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: r.rect.x, y: r.rect.y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer,
                    layout: wgpu::TexelCopyBufferLayout { offset: r.offset, bytes_per_row: Some(r.row), rows_per_image: Some(r.rect.h) },
                },
                wgpu::Extent3d { width: r.rect.w, height: r.rect.h, depth_or_array_layers: 1 },
            );
        }
        self.pending.push_back(Pending { slot, tag, regions, bytes, mapped: false });
    }

    /// Starts mapping every copy whose commands were submitted.
    pub(super) fn submitted(&mut self) {
        for p in self.pending.iter_mut().filter(|p| !p.mapped) {
            let slot = &self.slots[p.slot];
            let ready = slot.ready.clone();
            ready.store(false, Ordering::Release);
            slot.buffer.as_ref().unwrap().slice(..p.bytes.max(4)).map_async(wgpu::MapMode::Read, move |r| {
                r.expect("map readback");
                ready.store(true, Ordering::Release);
            });
            p.mapped = true;
        }
    }

    /// Finished frames in order, without waiting.
    pub(super) fn ready(&mut self, device: &wgpu::Device) -> Vec<(T, Vec<Patch>)> {
        let _ = device.poll(wgpu::PollType::Poll);
        std::iter::from_fn(|| self.pop(device, false)).collect()
    }

    /// Every frame in flight, waiting for the GPU.
    pub(super) fn finish(&mut self, device: &wgpu::Device) -> Vec<(T, Vec<Patch>)> {
        std::iter::from_fn(|| self.pop(device, true)).collect()
    }

    fn pop(&mut self, device: &wgpu::Device, wait: bool) -> Option<(T, Vec<Patch>)> {
        let front = self.pending.front()?;
        assert!(front.mapped, "Readback::submitted after submitting the copy");
        let ready = &self.slots[front.slot].ready;
        if !ready.load(Ordering::Acquire) {
            if !wait { return None; }
            while !ready.load(Ordering::Acquire) {
                device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
            }
        }
        let p = self.pending.pop_front()?;
        let buffer = self.slots[p.slot].buffer.as_ref().unwrap();
        let patches = {
            let data = buffer.slice(..p.bytes.max(4)).get_mapped_range().expect("mapped range");
            p.regions.iter().map(|r| {
                let line = r.rect.w as usize * 4;
                let mut rgba = Vec::with_capacity(line * r.rect.h as usize);
                for y in 0..r.rect.h as usize {
                    let at = r.offset as usize + y * r.row as usize;
                    rgba.extend_from_slice(&data[at..at + line]);
                }
                Patch { rect: r.rect, rgba }
            }).collect()
        };
        buffer.unmap();
        Some((p.tag, patches))
    }
}
