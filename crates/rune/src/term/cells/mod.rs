//! `--term cells`: the app on the terminal's own character grid, as OpenTUI draws, in any
//! terminal. No GPU: Rune lays the frame out with one layout unit per cell and every text in one
//! monospace size whose characters and lines are one unit (`crate::text::set_cell_font`). Each
//! element then draws as cells (`runtime/cells.rs` says how), and only the cells that changed
//! are written, in 24-bit colour, as one synchronized update.
//!
//! Beside the grid it speaks the terminal's own protocols: the focused field's caret is the
//! terminal's cursor in the text's colour (OSC 12), links are its hyperlinks (OSC 8), the
//! pointer takes the shape the element under it asks for (OSC 22), copies reach the clipboard
//! through the terminal too (OSC 52, so they work over SSH), a selection made with the mouse
//! is copied as the drag ends (the terminal keeps Cmd+C), the window's focus comes in as
//! focus reports (1004), and without `--theme` the terminal's background picks light or dark
//! (OSC 11).
//!
//! Changes land at once, as in a terminal UI: layout, entrances, exits and colours
//! (`cells_mode` makes the whole tree `El::instant_changes` and its colours instant), since text
//! can't slide between cells and a fade would rewrite every cell it covers on each frame. What
//! still moves (a scroll glide, an offset the app animates) has its box edges on eighths of a
//! cell, drawn with the eighth-block characters. While something moves, a cell whose colour
//! changed too little to see is not rewritten, and frames are paced to what the terminal can
//! take in bytes (less over SSH); the frame that comes to rest writes every cell exactly.
//! Changes that come closer together than a frame share one, and a frame with nothing new
//! writes nothing.

mod grid;

use std::collections::HashMap;
use std::time::Duration;

use self::grid::{Caret, Grid, Strings, Texels, rgb};
use super::input::{self, Decode, Input};
use super::tty::{Size, Tty};
use crate::app::App;
use crate::color::Color;
use crate::el::Cursor;
use crate::frame::clock::FrameClock;
use crate::runtime::{Env, Runtime};
use crate::time::Instant;
use crate::tooling::{Args, Length, runtime_for};
use crate::{Family, TextStyle};

/// The fastest the loop draws: 60 frames a second.
const FRAME_GAP: Duration = Duration::from_micros(16_667);
/// Bytes a second motion may write to a local terminal, and over SSH.
const MOTION_BYTES: [f64; 2] = [8e6, 1e6];
/// The biggest step, per sRGB channel (of 255), a cell's colours may lag while things move.
const MOTION_TOLERANCE: u8 = 6;
/// Rows one wheel notch scrolls.
const WHEEL_ROWS: f32 = 3.0;
/// How long the terminal gets to answer the start-up queries.
const PROBE_WAIT: Duration = Duration::from_millis(200);
/// Images whose sampled pixels are kept between frames.
const IMAGES_KEPT: usize = 32;

/// The font size at which the mono family advances exactly one unit per character.
fn cell_font_size<S: 'static>(rt: &mut Runtime<S>) -> f32 {
    let probe = "0".repeat(100);
    let (w, _) = rt.text.measure(&probe, TextStyle { family: Family::SystemMono, ..TextStyle::new(100.0, 400.0) }, 1.0);
    100.0 / (w / 100.0).max(1e-3)
}

/// Asks the terminal for its background (OSC 11), then for its device attributes, which every
/// terminal answers, so the wait ends as soon as the answers are in. True for a dark
/// background; None when it didn't say. Input that arrives meanwhile goes to `early`.
fn dark_background(tty: &Tty, decode: &mut Decode, early: &mut Vec<Input>) -> Option<bool> {
    tty.write(b"\x1b]11;?\x1b\\\x1b[c");
    let (start, mut buf, mut dark) = (Instant::now(), [0u8; 1024], None);
    while let Some(left) = PROBE_WAIT.checked_sub(start.elapsed()) {
        let n = tty.wait(Some(left), &mut buf);
        for &b in &buf[..n] {
            for i in input::step(decode, b) {
                match i {
                    Input::Background(c) => dark = Some(rgb_luminance(c) < 0.5),
                    Input::Answered => return dark,
                    other => early.push(other),
                }
            }
        }
    }
    dark
}

fn rgb_luminance([r, g, b]: [u8; 3]) -> f32 {
    (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) / 255.0
}

/// The OSC 22 name of a pointer shape (the CSS cursor names terminals take).
fn cursor_name(c: &Cursor) -> &'static str {
    match c {
        Cursor::Pointer => "pointer",
        Cursor::Text => "text",
        Cursor::Grab => "grab",
        Cursor::Grabbing => "grabbing",
        Cursor::NotAllowed => "not-allowed",
        Cursor::Move => "move",
        Cursor::ResizeColumn => "col-resize",
        Cursor::ResizeRow => "row-resize",
        Cursor::Crosshair => "crosshair",
        Cursor::ZoomIn => "zoom-in",
        Cursor::ZoomOut => "zoom-out",
        _ => "default",
    }
}

/// Images' pixels at the size their cells sample them, by signature and size.
#[derive(Default)]
struct Images(HashMap<((u64, u64), u32, u32), (u32, u32, Vec<u8>)>);

impl Images {
    fn get(&mut self, img: &crate::runtime::CellImage) -> Option<Texels<'_>> {
        let (w, h) = (img.dest[2].ceil().max(1.0) as u32, (img.dest[3] * 2.0).ceil().max(1.0) as u32);
        let key = (img.pixels.signature(), w, h);
        if !self.0.contains_key(&key) {
            if self.0.len() >= IMAGES_KEPT { self.0.clear(); }
            let read = img.pixels.read_at_least(w, h)?;
            self.0.insert(key, read);
        }
        self.0.get(&key).map(|(w, h, rgba)| Texels { w: *w, h: *h, rgba })
    }
}

pub(crate) fn run<S: 'static>(mut app: App<S>, args: Args) -> Result<(), String> {
    let (mut cols, mut rows) = Size::grid().ok_or("--term cells: stdout is not a terminal")?;
    let quit_after = match args.duration { Some(Length::Seconds(s)) => Some(Duration::from_secs_f64(s)), _ => None };
    let mut on_ready = app.on_ready.take();
    let (spec, state, shared) = app.into_parts(args.scene.as_deref());

    let tty = Tty::open(true)?;
    let mut decode = Decode::default();
    let mut early = Vec::new();
    let dark = match args.theme.as_deref() {
        Some(theme) => theme != "light",
        None => dark_background(&tty, &mut decode, &mut early).unwrap_or(true),
    };
    tty.write(format!("\x1b]2;{}\x07", spec.title).as_bytes());
    let waker = tty.waker();
    shared.connect(waker.clone());
    let mut clipboard = crate::clipboard::Client::default();
    clipboard.connect(waker.clone());
    let mut rt: Runtime<S> = runtime_for(&spec, state, (cols as f32, rows as f32), Env::new(dark, 1.0), waker);
    crate::text::set_cell_font(cell_font_size(&mut rt));
    rt.set_os_drag(false);
    rt.record_events = false;
    rt.open_urls_natively = true;
    // The terminal is in front when it starts the app; focus reports say when that changes.
    rt.window_focus(true);
    rt.invalidate();
    let started = Instant::now();
    rt.wall_start = Some(started);

    let mut clock = FrameClock::new(started);
    let mut buf = [0u8; 4096];
    let mut shown: Option<Grid> = None;
    let (mut strings, mut images) = (Strings::default(), Images::default());
    let mut pointer = "default";
    // The text the last mouse selection copied, so a click that keeps it copies nothing new.
    let mut copied: Option<String> = None;
    let mut out = Vec::new();
    let mut quit = early.into_iter().fold(false, |q, i| apply(&mut rt, i) || q);
    let (mut asked, mut busy, mut last_frame) = (true, false, started);
    // The caret the terminal shows.
    let mut shown_caret: Option<Caret> = None;
    // The wait between frames while busy: longer for motion that writes a lot.
    let mut gap = FRAME_GAP;
    let budget = MOTION_BYTES[usize::from(std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some())];
    let (mut frames, mut bytes, mut cpu, mut drawing) = (0u64, 0u64, Duration::ZERO, Duration::ZERO);

    while !quit {
        if quit_after.is_some_and(|d| started.elapsed() >= d) || crate::quit::quitting() { break; }
        let now = Instant::now();
        let mut timeout = if asked { Some(FRAME_GAP.saturating_sub(now - last_frame)) }
            else if busy { Some(gap.saturating_sub(now - last_frame)) }
            else { rt.wake_deadline().map(|d| Duration::from_secs_f64((d - (now - started).as_secs_f64()).max(0.0))) };
        if let Some(d) = quit_after { timeout = Some(timeout.map_or(d, |t| t.min(d.saturating_sub(started.elapsed())))); }
        let n = tty.wait(timeout, &mut buf);
        let mut released = false;
        for &b in &buf[..n] {
            for i in input::step(&mut decode, b) {
                released |= matches!(i, Input::Up(..));
                quit |= apply(&mut rt, i);
            }
        }
        if n > 0 { for i in input::idle(&mut decode) { quit |= apply(&mut rt, i); } }
        if quit { break; }
        if released && rt.copy_request.is_none() {
            let text = rt.selected_text();
            if text.is_some() && text != copied { rt.copy_request = Some(crate::ClipboardWrite::new(text.clone().unwrap_or_default().into())); }
            copied = text;
        }
        asked |= n > 0 || clipboard.receive(&mut rt);
        let updates = shared.take().updates;
        asked |= !updates.is_empty();
        for f in updates { rt.update_if(f); }
        if let Some((c, r)) = Size::grid() && (c, r) != (cols, rows) {
            (cols, rows) = (c, r);
            rt.set_viewport((cols as f32, rows as f32), 1.0);
            shown = None;
            asked = true;
        }

        let now = Instant::now();
        let due = rt.wake_deadline().is_some_and(|d| d <= (now - started).as_secs_f64());
        let since = now - last_frame;
        // Changes that come within a frame of the last one wait and share the next.
        if !((asked || due) && since >= FRAME_GAP || busy && since >= gap) { continue; }
        asked = false;
        last_frame = now;

        out.clear();
        // A copy reaches the terminal's clipboard as well as the system's, so it works over
        // SSH. A secret stays out: the terminal has no way to clear it after.
        if let Some(copy) = &rt.copy_request && !copy.is_secret() {
            out.extend_from_slice(b"\x1b]52;c;");
            super::kitty::base64(copy.content().text().as_bytes(), &mut out);
            out.push(0x07);
        }
        clipboard.flush(&mut rt);
        if rt.popup_menu().is_some() { rt.cancel_popups(); }
        rt.refuse_file_dialog("native file dialogs are unsupported in a terminal");
        let (moving, _) = clock.advance(&mut rt, now, false);
        if rt.shell_request.take().is_some() { break; }

        let drawn = Instant::now();
        if strings.full() { (strings, shown) = (Strings::default(), None); }
        let [r, g, b, _] = rt.clear_color();
        let mut grid = Grid::new(cols, rows, rgb(Color { r, g, b, a: 1.0 }));
        for p in rt.cell_paints() {
            let texels = p.image.as_ref().and_then(|img| images.get(img));
            grid.draw(&p, &mut strings, texels);
        }
        let caret: Option<Caret> = rt.cell_caret().map(|([x, y], c)| ((x.round() as i32, y.round() as i32), rgb(c)))
            .filter(|&((x, y), _)| x >= 0 && y >= 0 && x < grid.w && y < grid.h);
        let tolerance = if moving { MOTION_TOLERANCE } else { 0 };
        let start = out.len();
        let wrote = grid::encode(shown.as_ref(), &mut grid, &strings, caret, tolerance, &mut out);
        // Nothing new on screen: send nothing rather than an empty update.
        if !wrote && shown.is_some() && caret == shown_caret { out.truncate(start); }
        shown_caret = caret;
        let shape = cursor_name(&rt.cursor());
        if shape != pointer {
            pointer = shape;
            out.extend_from_slice(format!("\x1b]22;{shape}\x1b\\").as_bytes());
        }
        if !out.is_empty() { tty.write(&out); }
        shown = Some(grid);
        clock.presented(&mut rt, Instant::now());
        (frames, bytes, cpu, drawing) = (frames + 1, bytes + out.len() as u64, cpu + now.elapsed(), drawing + drawn.elapsed());
        if let Some(mut ready) = on_ready.take() { ready(&crate::WindowHandle::new("main")); }
        let input = rt.has_pending_input() || rt.live_pending();
        busy = moving || input;
        gap = if input { FRAME_GAP } else { FRAME_GAP.max(Duration::from_secs_f64(out.len() as f64 / budget)) };
    }
    drop(tty);
    if args.term_stats {
        let n = frames.max(1) as f64;
        let ms = |d: Duration| d.as_secs_f64() * 1000.0 / n;
        eprintln!("rune --term cells ({cols}x{rows}): {frames} frames in {:.1} s; per frame: {:.1} KB written, cpu {:.2} ms ({:.2} ms of it drawing cells)",
            started.elapsed().as_secs_f64(), bytes as f64 / 1024.0 / n, ms(cpu), ms(drawing));
    }
    Ok(())
}

/// Hands one input to the runtime; true to end the run. Mouse reports come in 1-based cells,
/// and point at the middle of theirs.
fn apply<S: 'static>(rt: &mut Runtime<S>, input: Input) -> bool {
    let p = |v: f32| v - 0.5;
    match input {
        Input::Key(k) => rt.key(k),
        Input::Move(x, y) => rt.pointer_move(p(x), p(y)),
        Input::Down(x, y) => { rt.pointer_move(p(x), p(y)); rt.pointer_down(p(x), p(y)) }
        Input::Up(x, y) => rt.pointer_up(p(x), p(y)),
        Input::Wheel(x, y, notches) => rt.wheel(p(x), p(y), notches * WHEEL_ROWS),
        Input::Paste(text) => rt.paste(text),
        Input::Focus(focused) => rt.window_focus(focused),
        Input::Background(_) | Input::Answered => {}
        Input::Quit => return true,
    }
    false
}
