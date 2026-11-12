//! The terminal: raw mode, the alternate screen and pixel mouse reports while a run lasts, the
//! window's size, and a wait for input that a resize or background work cuts short.

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use crate::runtime::Waker;

/// The window in cells and pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Size {
    pub(super) cols: u32,
    pub(super) rows: u32,
    /// One cell, in pixels.
    pub(super) cell: (u32, u32),
}

impl Size {
    /// The window's size, or None when the terminal reports no pixel size.
    pub(super) fn now() -> Option<Size> {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } != 0 { return None; }
        let (cols, rows) = (u32::from(ws.ws_col), u32::from(ws.ws_row));
        if cols == 0 || rows == 0 || ws.ws_xpixel == 0 || ws.ws_ypixel == 0 { return None; }
        Some(Size { cols, rows, cell: (u32::from(ws.ws_xpixel) / cols, u32::from(ws.ws_ypixel) / rows) })
    }

    /// The grid in cells, for a terminal that may not report pixels (`--term cells`).
    pub(super) fn grid() -> Option<(u32, u32)> {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } != 0 || ws.ws_col == 0 || ws.ws_row == 0 { return None; }
        Some((u32::from(ws.ws_col), u32::from(ws.ws_row)))
    }

    /// Pixels, cut to whole cells so the image is never scaled.
    pub(super) fn px(self) -> (u32, u32) {
        (self.cols * self.cell.0, self.rows * self.cell.1)
    }
}

/// Alternate screen, hidden cursor, every mouse move reported in pixels (SGR, 1016), bracketed
/// paste, and the Kitty keyboard protocol's unambiguous keys (`CSI > 1 u`; terminals without it
/// ignore it and send legacy keys).
const ENTER: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[?1003h\x1b[?1006h\x1b[?1016h\x1b[?2004h\x1b[>1u";
/// Deletes every image, puts the pointer shape back (OSC 22), then undoes `ENTER`.
const LEAVE: &[u8] = b"\x1b_Ga=d,d=A,q=2\x1b\\\x1b]22;default\x1b\\\x1b[<u\x1b[?2004l\x1b[?1016l\x1b[?1006l\x1b[?1003l\x1b[?25h\x1b[?1049l";
/// `ENTER` for a character grid (`--term cells`): mouse reports in cells, not pixels, and
/// focus reports (1004).
const ENTER_CELLS: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?2004h\x1b[>1u";
/// Undoes `ENTER_CELLS`, with the default cursor shape and colour (OSC 112) and pointer shape
/// (OSC 22) back and any hyperlink closed. No image commands: a terminal without Kitty
/// graphics would print them.
const LEAVE_CELLS: &[u8] = b"\x1b]8;;\x1b\\\x1b[0m\x1b[0 q\x1b]112\x07\x1b]22;default\x1b\\\x1b[<u\x1b[?2004l\x1b[?1004l\x1b[?1006l\x1b[?1003l\x1b[?25h\x1b[?1049l";

/// Asks the terminal whether it shows Kitty graphics: a query about a one-pixel image, then a
/// device-attributes request (DA1) that every terminal answers. A terminal with the protocol
/// answers the query before the request; one without answers only the request, and tmux
/// answers for itself. No answer within half a second (a slow link, no terminal) counts as no.
pub(super) fn kitty_graphics() -> bool {
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut saved) } != 0 { return false; }
    let mut raw = saved;
    unsafe { libc::cfmakeraw(&mut raw) };
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 { return false; }
    {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c").and_then(|()| out.flush());
    }
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut got = Vec::new();
    // The DA1 answer, `ESC [ ? … c`, ends what the terminal sends.
    let answered = |got: &[u8]| got.windows(3).position(|w| w == b"\x1b[?").is_some_and(|at| got[at..].contains(&b'c'));
    while !answered(&got) {
        let left = deadline.saturating_duration_since(Instant::now()).as_millis() as i32;
        let mut fd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
        if left == 0 || unsafe { libc::poll(&mut fd, 1, left) } <= 0 { break; }
        let mut buf = [0u8; 256];
        let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 { break; }
        got.extend_from_slice(&buf[..n as usize]);
    }
    unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved) };
    got.windows(9).any(|w| w == b"_Gi=31;OK")
}

/// The wake pipe's write end, for the SIGWINCH handler.
static RESIZED: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_resize(_: libc::c_int) {
    let fd = RESIZED.load(Ordering::Relaxed);
    if fd >= 0 { unsafe { libc::write(fd, [0u8].as_ptr().cast(), 1) }; }
}

/// The terminal in raw mode; restores it on drop, panics included.
pub(super) struct Tty {
    saved: libc::termios,
    /// A character grid (`--term cells`) rather than Kitty graphics.
    cells: bool,
    wake_read: OwnedFd,
    wake_write: Arc<File>,
}

impl Tty {
    /// The terminal for Kitty graphics, or for a character grid when `cells`.
    pub(super) fn open(cells: bool) -> Result<Tty, String> {
        let os = |what: &str| format!("--term: {what}: {}", std::io::Error::last_os_error());
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut saved) } != 0 { return Err(os("stdin is not a terminal")); }
        let mut fds = [0; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 { return Err(os("pipe")); }
        for fd in fds {
            unsafe { libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) };
        }
        let (wake_read, wake_write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
        let mut raw = saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 { return Err(os("raw mode")); }
        RESIZED.store(wake_write.as_raw_fd(), Ordering::Relaxed);
        unsafe { libc::signal(libc::SIGWINCH, on_resize as *const () as libc::sighandler_t) };
        let tty = Tty { saved, cells, wake_read, wake_write: Arc::new(wake_write) };
        tty.write(if cells { ENTER_CELLS } else { ENTER });
        Ok(tty)
    }

    /// Wakes `wait` from another thread.
    pub(super) fn waker(&self) -> Waker {
        let pipe = self.wake_write.clone();
        Arc::new(move || { let _ = (&*pipe).write(&[0]); })
    }

    /// Waits until input arrives, something wakes it, or `timeout` passes (`None`: no limit),
    /// and returns the input bytes read into `buf`.
    pub(super) fn wait(&self, timeout: Option<Duration>, buf: &mut [u8]) -> usize {
        let mut fds = [
            libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: self.wake_read.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        let ms = timeout.map_or(-1, |t| t.as_millis().min(i32::MAX as u128) as i32);
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) } <= 0 { return 0; }
        if fds[1].revents != 0 {
            let mut drain = [0u8; 64];
            while unsafe { libc::read(fds[1].fd, drain.as_mut_ptr().cast(), drain.len()) } > 0 {}
        }
        if fds[0].revents & libc::POLLIN == 0 { return 0; }
        unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) }.max(0) as usize
    }

    pub(super) fn write(&self, bytes: &[u8]) {
        write(bytes);
    }
}

/// Writes `bytes` to the terminal in one piece, from any thread: what another thread writes
/// goes before or after, never inside.
pub(super) fn write(bytes: &[u8]) {
    let mut out = std::io::stdout().lock();
    // A terminal that went away ends the run at the next read; nothing to report here.
    let _ = out.write_all(bytes).and_then(|()| out.flush());
}

impl Drop for Tty {
    fn drop(&mut self) {
        unsafe { libc::signal(libc::SIGWINCH, libc::SIG_DFL) };
        RESIZED.store(-1, Ordering::Relaxed);
        self.write(if self.cells { LEAVE_CELLS } else { LEAVE });
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved) };
    }
}
