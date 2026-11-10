//! Frames as Kitty graphics: the window is one image (id 1) placed at the top-left cell. Through
//! the pty (zlib, base64) a new size transmits it whole (`a=T`) and after that each damaged
//! rectangle is written into it in place (`a=f` on frame 1, the image itself), so few bytes cross.
//! Through shared memory nothing but a name crosses, so every frame replaces the whole image as
//! RGBA (`a=T`): Ghostty spends far less on that than on `a=f` edits, which cost it about the same
//! whatever their size, and than on many small images, which cost it per image. Each frame is one
//! synchronized update.

use std::ffi::CString;
use std::io::Write;

use super::readback::Patch;

/// How pixels reach the terminal: `--term zlib|base64|shm`, or [`Transport::auto`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Transport {
    /// RGB deflated (`o=z`), base64 in the pty. Works over SSH.
    Zlib,
    /// RGB, base64 in the pty.
    Base64,
    /// A POSIX shared-memory object the terminal maps and unlinks; only its name goes through
    /// the pty. Local only; whole RGBA frames.
    Shm,
}

impl Transport {
    pub(crate) fn parse(v: &str) -> Option<Transport> {
        match v {
            "zlib" => Some(Transport::Zlib),
            "base64" => Some(Transport::Base64),
            "shm" => Some(Transport::Shm),
            _ => None,
        }
    }

    /// Without a choice: shared memory in a local Ghostty or Kitty, the fastest by far, and zlib
    /// anywhere else (SSH, other terminals), which sends only what changed, compressed.
    pub(crate) fn auto() -> Transport {
        let set = |k: &str| std::env::var_os(k).is_some();
        let is = |k: &str, v: &str| std::env::var(k).is_ok_and(|x| x == v);
        let ssh = set("SSH_CONNECTION") || set("SSH_CLIENT") || set("SSH_TTY");
        let local = is("TERM_PROGRAM", "ghostty") || is("TERM", "xterm-kitty") || set("KITTY_WINDOW_ID");
        if local && !ssh { Transport::Shm } else { Transport::Zlib }
    }

    /// Whether every frame goes whole ([`Transport::Shm`]) rather than as damaged rectangles.
    pub(crate) fn whole_frames(self) -> bool {
        self == Transport::Shm
    }
}

/// Totals, for `--term-stats`.
#[derive(Default)]
pub(super) struct Sent {
    pub(super) frames: u64,
    pub(super) rects: u64,
    /// Pixel bytes the terminal decodes, before compression.
    pub(super) pixels: u64,
    /// Bytes written to the pty.
    pub(super) pty: u64,
}

/// Shared-memory names it cycles through; the terminal unlinks each after reading it.
const SHM_NAMES: u64 = 256;

pub(super) struct Kitty {
    transport: Transport,
    /// The image's size, once transmitted.
    shown: Option<(u32, u32)>,
    out: Vec<u8>,
    rgb: Vec<u8>,
    b64: Vec<u8>,
    shm_seq: u64,
    pub(super) sent: Sent,
}

impl Kitty {
    pub(super) fn new(transport: Transport) -> Kitty {
        Kitty { transport, shown: None, out: Vec::new(), rgb: Vec::new(), b64: Vec::new(), shm_seq: 0, sent: Sent::default() }
    }

    /// The commands that draw `patches` of a frame `size` pixels big. A frame at a new size, and
    /// every shared-memory frame, must be one whole-window patch; one that isn't is dropped (a
    /// later full frame replaces it).
    pub(super) fn frame(&mut self, size: (u32, u32), patches: &[Patch]) -> std::io::Result<&[u8]> {
        self.out.clear();
        let fresh = self.shown != Some(size);
        let whole = matches!(patches, [p] if (p.rect.w, p.rect.h) == size);
        if patches.is_empty() || (fresh || self.transport.whole_frames()) && !whole { return Ok(&self.out); }
        self.out.extend_from_slice(b"\x1b[?2026h");
        if fresh { self.out.extend_from_slice(b"\x1b_Ga=d,d=A,q=2\x1b\\\x1b[H"); }
        if self.transport.whole_frames() {
            let keys = format!("a=T,f=32,s={},v={},i=1,p=1,q=2,C=1", size.0, size.1);
            self.transmit_shm(&keys, &patches[0].rgba)?;
            self.sent.rects += 1;
            self.sent.pixels += patches[0].rgba.len() as u64;
        }
        let rects = if self.transport.whole_frames() { &[][..] } else { patches };
        for p in rects {
            self.rgb.clear();
            self.rgb.extend(p.rgba.chunks_exact(4).flat_map(|px| [px[0], px[1], px[2]]));
            let keys = if fresh {
                format!("a=T,f=24,s={},v={},i=1,q=2,C=1", size.0, size.1)
            } else {
                format!("a=f,r=1,i=1,f=24,x={},y={},s={},v={},q=2", p.rect.x, p.rect.y, p.rect.w, p.rect.h)
            };
            self.transmit(&keys)?;
            self.sent.rects += 1;
            self.sent.pixels += self.rgb.len() as u64;
        }
        self.out.extend_from_slice(b"\x1b[?2026l");
        self.shown = Some(size);
        self.sent.frames += 1;
        self.sent.pty += self.out.len() as u64;
        Ok(&self.out)
    }

    /// Appends `data` sent through a fresh shared-memory object with the command's `keys`.
    fn transmit_shm(&mut self, keys: &str, data: &[u8]) -> std::io::Result<()> {
        let name = shm_name(self.shm_seq % SHM_NAMES);
        self.shm_seq += 1;
        write_shm(&name, data)?;
        self.b64.clear();
        base64(name.as_bytes(), &mut self.b64);
        write!(self.out, "\x1b_G{keys},t=s,S={};", data.len())?;
        self.out.extend_from_slice(&self.b64);
        self.out.extend_from_slice(b"\x1b\\");
        Ok(())
    }

    /// Appends `self.rgb` sent through the pty with the command's `keys`.
    fn transmit(&mut self, keys: &str) -> std::io::Result<()> {
        self.b64.clear();
        let out = &mut self.out;
        match self.transport {
            Transport::Shm => unreachable!("shared memory sends whole frames"),
            Transport::Base64 => base64(&self.rgb, &mut self.b64),
            Transport::Zlib => base64(&miniz_oxide::deflate::compress_to_vec_zlib(&self.rgb, 1), &mut self.b64),
        }
        let z = if self.transport == Transport::Zlib { ",o=z" } else { "" };
        // Chunks of at most 4096 base64 bytes; `m=1` on all but the last.
        let mut chunks = self.b64.chunks(4096).peekable();
        let mut first = true;
        while let Some(chunk) = chunks.next() {
            let more = u8::from(chunks.peek().is_some());
            if first { write!(out, "\x1b_G{keys}{z},m={more};")? } else { write!(out, "\x1b_Gm={more};")? }
            out.extend_from_slice(chunk);
            out.extend_from_slice(b"\x1b\\");
            first = false;
        }
        Ok(())
    }
}

impl Drop for Kitty {
    /// Unlinks names the terminal never read (one without `t=s` support reads none).
    fn drop(&mut self) {
        for seq in 0..self.shm_seq.min(SHM_NAMES) {
            unsafe { libc::shm_unlink(shm_name(seq).as_ptr()) };
        }
    }
}

fn shm_name(seq: u64) -> CString {
    CString::new(format!("/rune-term-{}-{seq}", std::process::id())).expect("no NUL")
}

/// Writes `data` into a fresh shared-memory object named `name`.
fn write_shm(name: &CString, data: &[u8]) -> std::io::Result<()> {
    let err = std::io::Error::last_os_error;
    unsafe {
        libc::shm_unlink(name.as_ptr());
        let fd = libc::shm_open(name.as_ptr(), libc::O_CREAT | libc::O_RDWR | libc::O_EXCL, 0o600 as libc::c_uint);
        if fd < 0 { return Err(err()); }
        if libc::ftruncate(fd, data.len() as libc::off_t) != 0 { libc::close(fd); return Err(err()); }
        let p = libc::mmap(std::ptr::null_mut(), data.len(), libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
        libc::close(fd);
        if p == libc::MAP_FAILED { return Err(err()); }
        std::ptr::copy_nonoverlapping(data.as_ptr(), p.cast::<u8>(), data.len());
        libc::munmap(p, data.len());
    }
    Ok(())
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(data: &[u8], out: &mut Vec<u8>) {
    out.reserve(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        out.extend([B64[(n >> 18) as usize & 63], B64[(n >> 12) as usize & 63]]);
        out.push(if c.len() > 1 { B64[(n >> 6) as usize & 63] } else { b'=' });
        out.push(if c.len() > 2 { B64[n as usize & 63] } else { b'=' });
    }
}
