//! Encoding and writing to the terminal on a thread of its own, so the loop never waits on it: a
//! whole frame through zlib takes about 10 ms to encode, and a write over SSH can block for as
//! long as the link needs. The loop hands it a frame only when nothing waits (`idle`); until
//! then frames add up their damage (`Unsent`), and the next readback takes all of it from the
//! latest frame. A slow terminal gets fewer, bigger updates, never a queue of old ones.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use super::Tag;
use super::damage::{self, Rect};
use super::kitty::{Kitty, Sent, Transport};
use super::readback::Patch;
use crate::runtime::Waker;
use crate::time::Instant;

/// What the writer did, for `--term-stats`.
#[derive(Default)]
pub(super) struct Written {
    pub(super) sent: Sent,
    pub(super) encode: Duration,
    pub(super) write: Duration,
    /// From each frame's start to its bytes reaching the pty.
    pub(super) latency: Duration,
}

/// What waits for the writer, in the order it goes out.
enum Item {
    Frame(Tag, Vec<Patch>),
    /// Bytes of the loop's own (a pointer shape, a copy), behind the frames before them.
    Bytes(Vec<u8>),
}

#[derive(Default)]
struct Queue {
    items: Vec<Item>,
    closed: bool,
    /// Why encoding failed (a shared-memory object that couldn't be made): the run ends.
    failed: Option<String>,
}

pub(super) struct Writer {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    thread: Option<JoinHandle<Written>>,
}

impl Writer {
    /// Starts the thread. It calls `wake` as it takes what waits, so the loop can hand it the
    /// damage gathered meanwhile.
    pub(super) fn start(transport: Transport, wake: Waker) -> Writer {
        let queue = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let thread = std::thread::Builder::new().name("rune-term-writer".into()).spawn({
            let queue = queue.clone();
            move || write_out(&queue, Kitty::new(transport), &wake)
        }).expect("--term: writer thread");
        Writer { queue, thread: Some(thread) }
    }

    /// Nothing waits: the writer took everything (it may still be writing it).
    pub(super) fn idle(&self) -> bool {
        self.queue.0.lock().unwrap().items.is_empty()
    }

    /// Why the writer stopped, if it did: the run ends with it. It wakes the loop when it stops.
    pub(super) fn failed(&self) -> Result<(), String> {
        self.queue.0.lock().unwrap().failed.clone().map_or(Ok(()), Err)
    }

    /// Queues a finished frame; an error when the writer failed.
    pub(super) fn push(&self, tag: Tag, patches: Vec<Patch>) -> Result<(), String> {
        self.queue_item(Item::Frame(tag, patches))
    }

    /// Queues the loop's own bytes behind the frames already queued.
    pub(super) fn bytes(&self, bytes: &[u8]) {
        let _ = self.queue_item(Item::Bytes(bytes.to_vec()));
    }

    fn queue_item(&self, item: Item) -> Result<(), String> {
        let (lock, ready) = &*self.queue;
        let mut q = lock.lock().unwrap();
        if let Some(e) = &q.failed { return Err(e.clone()); }
        q.items.push(item);
        drop(q);
        ready.notify_one();
        Ok(())
    }

    /// Writes what waits, ends the thread and says what it did.
    pub(super) fn finish(mut self) -> Result<Written, String> {
        let written = self.close();
        self.queue.0.lock().unwrap().failed.take().map_or(Ok(written), Err)
    }

    fn close(&mut self) -> Written {
        let (lock, ready) = &*self.queue;
        lock.lock().unwrap().closed = true;
        ready.notify_one();
        self.thread.take().map(|t| t.join().expect("--term: writer thread")).unwrap_or_default()
    }
}

impl Drop for Writer {
    /// A run that ends early writes nothing after the terminal is given back: the writer ends
    /// first.
    fn drop(&mut self) {
        self.close();
    }
}

fn write_out(queue: &(Mutex<Queue>, Condvar), mut kitty: Kitty, wake: &Waker) -> Written {
    let (lock, ready) = queue;
    let mut done = Written::default();
    loop {
        let items = {
            let mut q = lock.lock().unwrap();
            while q.items.is_empty() && !q.closed { q = ready.wait(q).unwrap(); }
            if q.items.is_empty() { break; }
            std::mem::take(&mut q.items)
        };
        wake();
        for item in items {
            match item {
                Item::Bytes(bytes) => super::tty::write(&bytes),
                Item::Frame(tag, patches) => {
                    let t = Instant::now();
                    let bytes = match kitty.frame(tag.size, &patches) {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            lock.lock().unwrap().failed = Some(format!("--term: {e}"));
                            wake();
                            done.sent = std::mem::take(&mut kitty.sent);
                            return done;
                        }
                    };
                    let t1 = Instant::now();
                    if !bytes.is_empty() { super::tty::write(bytes); }
                    done.encode += t1 - t;
                    done.write += t1.elapsed();
                    done.latency += tag.started.elapsed();
                }
            }
        }
    }
    done.sent = std::mem::take(&mut kitty.sent);
    done
}

/// Damage the writer hasn't been handed yet, merged as it comes so it never outgrows the
/// window's tiles, and when the first frame of it started.
#[derive(Default)]
pub(super) struct Unsent(Option<(Vec<Rect>, Instant)>);

impl Unsent {
    /// A frame of a window `size` pixels big that started `at` changed `rects`.
    pub(super) fn add(&mut self, rects: Vec<Rect>, size: (u32, u32), at: Instant) {
        if rects.is_empty() { return; }
        match &mut self.0 {
            Some((all, _)) => {
                all.extend(rects);
                *all = damage::merge(all, size);
            }
            None => self.0 = Some((rects, at)),
        }
    }

    /// Forgets it: the window changed size, and the next frame goes whole.
    pub(super) fn clear(&mut self) {
        self.0 = None;
    }

    /// What to read back of a window `size` pixels big, when `ready` for it: the damage, or
    /// the whole window when every frame goes whole.
    pub(super) fn take(&mut self, ready: bool, size: (u32, u32), transport: Transport) -> Option<(Vec<Rect>, Tag)> {
        if !ready { return None; }
        let (rects, started) = self.0.take()?;
        let rects = if transport.whole_frames() { vec![Rect::whole(size)] } else { rects };
        Some((rects, Tag { size, started }))
    }
}
