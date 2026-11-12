//! `--term`: run the app inside a terminal that speaks the Kitty graphics protocol (Kitty,
//! Ghostty), as one image filling the window, with the terminal's mouse and keys as input.
//! `--term` alone asks the terminal first and draws on its character grid (`cells`) when it
//! has no Kitty graphics.
//!
//! Each frame renders offscreen as a window's would. `damage` finds what changed from the
//! instances drawn, `readback` copies only those rectangles off the GPU while the next frame
//! renders, and `kitty` encodes them into the terminal's image, on `writer`'s thread. An idle app draws and sends
//! nothing, and a frame that builds the same instances as the last (the pointer moving over
//! nothing that reacts) renders nothing. Keys and presses draw at once; pointer moves at the
//! frame rate, as a window's vsync paces them. `--scale` defaults to 2 when the terminal's
//! cells are at least 30 pixels tall.
//!
//! The pointer takes the shape the element under it asks for (OSC 22). Copies reach the system
//! clipboard and the terminal's (OSC 52), and a mouse selection is copied as the drag ends.
//! Ctrl-C reaches the app as a key; Ctrl-\ always ends the run. Single-window apps only for
//! now; native menus, file dialogs and pop-up menus aren't there.

mod cells;
mod check;
mod damage;
mod input;
mod kitty;
mod readback;
mod tty;
mod writer;

use std::time::Duration;

pub(crate) use cells::run as run_cells;
pub(crate) use kitty::Transport;

use crate::app::App;
use crate::compose::Frame;
use crate::el::Cursor;
use crate::frame::clock::FrameClock;
use crate::render::{Gpu, Instance, OFFSCREEN_FORMAT, Renderer};
use crate::runtime::{Env, Runtime, Waker};
use crate::time::Instant;
use crate::tooling::{Args, Length, runtime_for};

use check::Check;
use damage::Rect;
use input::{Decode, Input};
use readback::Readback;
use tty::{Size, Tty};
use writer::{Unsent, Writer, Written};

/// The fastest the loop draws while something moves.
const FRAME_GAP: Duration = Duration::from_micros(8_333);
/// Points one wheel notch scrolls.
const WHEEL_POINTS: f32 = 40.0;

/// One frame's tag through the readback: its size, and when its CPU work started.
struct Tag {
    size: (u32, u32),
    started: Instant,
}

/// Where a frame's time goes, for `--term-stats`.
#[derive(Default)]
struct Stats {
    /// Frames input or updates asked for, and frames motion asked for.
    asked: u32,
    motion: u32,
    motion_secs: f64,
    /// Frame step, damage and GPU encode, on the CPU.
    cpu: Duration,
    /// Waiting for a free readback buffer, or for the last frames at rest.
    waits: Duration,
}

/// Where frames go once rendered: what they changed waits in `unsent` until the writer has
/// nothing waiting, then is read back off the GPU and handed to it (and to the check when on).
struct Sink {
    writer: Writer,
    unsent: Unsent,
    transport: Transport,
    stats: Stats,
    check: Option<Check>,
}

impl Sink {
    /// Hands frames read back to the writer.
    fn show(&mut self, frames: Vec<(Tag, Vec<readback::Patch>)>) -> Result<(), String> {
        for (tag, patches) in frames {
            if let Some(c) = self.check.as_mut() { c.sent(tag.size, &patches); }
            self.writer.push(tag, patches)?;
        }
        Ok(())
    }

    /// Starts reading back what frames changed since the writer was last handed any, from
    /// `target` (the latest frame, `size` pixels big), once the writer has nothing waiting. The
    /// check takes every frame.
    fn read_back(&mut self, gpu: &Gpu, target: &wgpu::Texture, readback: &mut Readback<Tag>, size: (u32, u32)) -> Result<(), String> {
        let ready = self.check.is_some() || self.writer.idle();
        let Some((rects, tag)) = self.unsent.take(ready, size, self.transport) else { return Ok(()) };
        let wait = Instant::now();
        let done = readback.make_room(&gpu.device);
        self.stats.waits += wait.elapsed();
        self.show(done)?;
        let mut enc = gpu.device.create_command_encoder(&Default::default());
        readback.copy(&gpu.device, &mut enc, target, rects, tag);
        gpu.queue.submit([enc.finish()]);
        readback.submitted();
        Ok(())
    }
}

/// Where the loop's own small writes go: straight to the terminal, or behind the frames the
/// writer has queued, so they never wait for one to finish.
trait Out {
    fn out(&self, bytes: &[u8]);
}

impl Out for Tty {
    fn out(&self, bytes: &[u8]) {
        self.write(bytes);
    }
}

impl Out for Writer {
    fn out(&self, bytes: &[u8]) {
        self.bytes(bytes);
    }
}

/// The OSC 22 name of a pointer shape (the CSS cursor names terminals take).
pub(super) fn cursor_name(c: &Cursor) -> &'static str {
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

/// The pointer shape the terminal shows (OSC 22): what the element under the pointer asks for.
struct Pointer(&'static str);

impl Default for Pointer {
    fn default() -> Self {
        Pointer("default")
    }
}

impl Pointer {
    /// Sends the shape when it changed. Terminals without OSC 22 keep their own.
    fn show<S: 'static>(&mut self, rt: &Runtime<S>, out: &impl Out) {
        let shape = cursor_name(&rt.cursor());
        if shape != self.0 {
            self.0 = shape;
            out.out(format!("\x1b]22;{shape}\x1b\\").as_bytes());
        }
    }
}

/// The app's clipboard in a terminal: the system's, through Rune's clipboard worker, and for a
/// copy the terminal's too (OSC 52), so it reaches your own machine over SSH. A secret stays
/// out of OSC 52: the terminal has no way to clear it after.
struct Clipboard {
    client: crate::clipboard::Client,
    /// The text the last mouse selection copied, so a click that keeps it copies nothing new.
    copied: Option<String>,
}

impl Clipboard {
    fn new(wake: Waker) -> Clipboard {
        let mut client = crate::clipboard::Client::default();
        client.connect(wake);
        Clipboard { client, copied: None }
    }

    /// The pointer let go: a new mouse selection is copied, as a terminal copies its own (its
    /// Cmd+C stays the terminal's).
    fn released<S: 'static>(&mut self, rt: &mut Runtime<S>) {
        if rt.copy_request.is_some() { return; }
        let text = rt.selected_text();
        if let Some(t) = &text && text != self.copied { rt.copy_request = Some(crate::ClipboardWrite::new(t.clone().into())); }
        self.copied = text;
    }

    /// Hands on what the app asked of the clipboard, and gives it the answers that came back;
    /// true when one did, for a frame to show it.
    fn step<S: 'static>(&self, rt: &mut Runtime<S>, out: &impl Out) -> bool {
        if let Some(copy) = &rt.copy_request && !copy.is_secret() {
            let mut osc = b"\x1b]52;c;".to_vec();
            kitty::base64(copy.content().text().as_bytes(), &mut osc);
            osc.push(0x07);
            out.out(&osc);
        }
        self.client.flush(rt);
        self.client.receive(rt)
    }
}

/// The render target, sized to the window in pixels.
struct Target {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

impl Target {
    fn new(device: &wgpu::Device, (w, h): (u32, u32), format: wgpu::TextureFormat) -> Target {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("term target"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        Target { texture, view }
    }
}

/// Whether `--term` without a transport draws pixels: the terminal reports its pixel size and
/// answers a Kitty graphics query. Otherwise it draws on the character grid (`--term cells`).
pub(crate) fn shows_pixels() -> bool {
    Size::now().is_some() && tty::kitty_graphics()
}

pub(crate) fn run<S: 'static>(mut app: App<S>, args: Args, transport: Transport) -> Result<(), String> {
    let mut size = Size::now().ok_or("--term: the terminal reports no pixel size; run it in Kitty, Ghostty or another terminal with the Kitty graphics protocol")?;
    let scale = args.scale.unwrap_or(if size.cell.1 >= 30 { 2.0 } else { 1.0 });
    let logical = |s: Size| { let (w, h) = s.px(); (w as f32 / scale, h as f32 / scale) };
    let dark = args.theme.as_deref() != Some("light");
    let quit_after = match args.duration { Some(Length::Seconds(s)) => Some(Duration::from_secs_f64(s)), _ => None };

    let mut comp = app.compositor.take();
    let mut on_ready = app.on_ready.take();
    let (srgb_blend, font_smoothing) = (app.srgb_blend, app.font_smoothing);
    let (spec, state, shared) = app.into_parts(args.scene.as_deref());
    let features = comp.as_ref().map_or(wgpu::Features::empty(), |c| c.features());
    let gpu = Gpu::try_with_features(Gpu::instance_for(features), None, features)?;
    let format = if srgb_blend { OFFSCREEN_FORMAT.remove_srgb_suffix() } else { OFFSCREEN_FORMAT };
    let mut renderer = Renderer::new(&gpu.device, format);
    renderer.set_srgb_blend(srgb_blend);
    let mut target = Target::new(&gpu.device, size.px(), format);

    let tty = Tty::open(false)?;
    // The window's title, as a window would show it.
    tty.write(format!("\x1b]2;{}\x07", spec.title).as_bytes());
    let waker = tty.waker();
    let mut clipboard = Clipboard::new(waker.clone());
    let writer = Writer::start(transport, waker.clone());
    let mut sink = Sink { writer, unsent: Unsent::default(), transport, stats: Stats::default(), check: Check::from_env() };
    shared.connect(waker.clone());
    let mut rt: Runtime<S> = runtime_for(&spec, state, logical(size), Env::new(dark, scale), waker);
    renderer.attach_text(&mut rt.text.atlas);
    rt.set_srgb_blend(srgb_blend);
    rt.set_font_smoothing(font_smoothing);
    rt.set_os_drag(false);
    rt.track_marks = comp.is_some();
    rt.record_events = false;
    let started = Instant::now();
    rt.wall_start = Some(started);

    let mut clock = FrameClock::new(started);
    let mut readback = Readback::<Tag>::new();
    let mut decode = Decode::default();
    let mut buf = [0u8; 4096];
    // The instances and clear colour the last frame drew; None until a frame draws, and after
    // a resize, so the next frame sends everything.
    let mut last: Option<(Vec<Instance>, [f32; 4])> = None;
    // `asked`: input or an update a frame shows at once. `moved`: the pointer moved, drawn at
    // the frame rate.
    let (mut asked, mut moved, mut animating, mut last_frame) = (true, false, false, started);
    let mut pointer = Pointer::default();

    loop {
        if quit_after.is_some_and(|d| started.elapsed() >= d) || crate::quit::quitting() { break; }
        // Wait for input, a wake, the next motion frame, a timer, or a readback in flight.
        let now = Instant::now();
        let mut timeout = if asked { Some(Duration::ZERO) }
            else if animating || moved { Some(FRAME_GAP.saturating_sub(now - last_frame)) }
            else { rt.wake_deadline().map(|d| Duration::from_secs_f64((d - (now - started).as_secs_f64()).max(0.0))) };
        let soonest = |t: Option<Duration>, limit: Duration| Some(t.map_or(limit, |t| t.min(limit)));
        if readback.in_flight() { timeout = soonest(timeout, Duration::from_millis(1)); }
        if let Some(d) = quit_after { timeout = soonest(timeout, d.saturating_sub(started.elapsed())); }
        if let Some(d) = decode.deadline() { timeout = soonest(timeout, d.saturating_duration_since(now)); }
        let n = tty.wait(timeout, &mut buf);
        let mut released = false;
        let fed = input::feed(&mut decode, &buf[..n], Instant::now(), |i| {
            released |= matches!(i, Input::Up(..));
            apply(&mut rt, i, scale)
        });
        if fed.quit { break; }
        if released { clipboard.released(&mut rt); }
        (asked, moved) = (asked || fed.acted, moved || fed.moved);
        let updates = shared.take().updates;
        asked |= !updates.is_empty();
        for f in updates { rt.update_if(f); }
        asked |= clipboard.step(&mut rt, &sink.writer);
        pointer.show(&rt, &sink.writer);
        if let Some(now) = Size::now() && now != size {
            size = now;
            target = Target::new(&gpu.device, size.px(), format);
            rt.set_viewport(logical(size), scale);
            last = None;
            sink.unsent.clear();
            asked = true;
        }
        sink.writer.failed()?;
        sink.show(readback.ready(&gpu.device))?;
        // The writer may have taken what waited: hand it what frames changed meanwhile.
        sink.read_back(&gpu, &target.texture, &mut readback, size.px())?;

        let now = Instant::now();
        let due = rt.wake_deadline().is_some_and(|d| d <= (now - started).as_secs_f64());
        if !(asked || due || (animating || moved) && now - last_frame >= FRAME_GAP) { continue; }
        let stats = &mut sink.stats;
        if asked || due || moved { stats.asked += 1 } else { stats.motion += 1; stats.motion_secs += (now - last_frame).as_secs_f64() }
        (asked, moved) = (false, false);
        last_frame = now;

        if rt.popup_menu().is_some() { rt.cancel_popups(); }
        rt.refuse_file_dialog("native file dialogs are unsupported in a terminal");
        let comp_animating = comp.as_ref().is_some_and(|c| c.animating());
        let (moving, dt) = clock.advance(&mut rt, now, comp_animating);
        if rt.shell_request.take().is_some() { break; }

        // What to send: everything after a resize or with things damage can't see (a
        // compositor's passes, native surfaces, image pixels changing in place).
        let px = size.px();
        let images_changed = rt.images.pages.iter().any(|p| p.dirty.is_some());
        let atlas = &rt.text.atlas;
        let glyphs_changed = atlas.dirty.is_some() || atlas.pages.iter().any(|p| p.dirty.is_some());
        let clear = rt.clear_color();
        let seen = comp.is_none() && rt.surfaces.is_empty() && !images_changed;
        // The same instances into the same target, with no upload waiting, draw the pixels the
        // terminal already shows: nothing to render, read back or send.
        let same = seen && !glyphs_changed && last.as_ref().is_some_and(|(old, old_clear)| {
            *old_clear == clear && bytemuck::cast_slice::<_, u8>(old) == bytemuck::cast_slice::<_, u8>(&rt.instances)
        });
        if !same {
            let rects = match &last {
                Some((old, old_clear)) if seen && *old_clear == clear => damage::between(old, &rt.instances, px),
                _ => vec![Rect::whole(px)],
            };
            sink.unsent.add(rects, px, now);
            match &mut last {
                Some((old, old_clear)) => { old.clone_from(&rt.instances); *old_clear = clear; }
                None => last = Some((rt.instances.clone(), clear)),
            }

            renderer.upload_images(&gpu.device, &gpu.queue, &mut rt.images);
            renderer.prepare_surfaces(&gpu.device, &gpu.queue, &rt.surfaces, px);
            let mut enc = gpu.device.create_command_encoder(&Default::default());
            if let Some(c) = comp.as_mut() {
                let mut f = Frame { device: &gpu.device, queue: &gpu.queue, encoder: &mut enc, target: &target.view, format, size: px, dt, update_ms: 0.0, rt: &mut rt, ui: &mut renderer };
                c.render(&mut f);
            } else {
                renderer.prepare(&gpu.device, &gpu.queue, &mut rt.text.atlas, &rt.instances, px);
                renderer.draw(&mut enc, &target.view, clear);
            }
            if let Some(c) = sink.check.as_mut() { c.copy(&gpu.device, &mut enc, &target.texture, px); }
            renderer.submit(&gpu.queue, enc);
            if let Some(c) = sink.check.as_mut() { c.submitted(); }
            sink.read_back(&gpu, &target.texture, &mut readback, px)?;
        }
        clock.presented(&mut rt, Instant::now());
        sink.stats.cpu += now.elapsed();
        if !same && sink.check.is_some() {
            let done = readback.finish(&gpu.device);
            sink.show(done)?;
            sink.check.as_mut().unwrap().compare(&gpu.device);
        }
        if let Some(mut ready) = on_ready.take() { ready(&crate::WindowHandle::new("main")); }
        asked |= clipboard.step(&mut rt, &sink.writer);
        pointer.show(&rt, &sink.writer);

        animating = moving || comp.as_ref().is_some_and(|c| c.animating()) || rt.has_pending_input() || rt.live_pending();
        // At rest nothing renders next, so finish the frames in flight now.
        if !animating && readback.in_flight() {
            let wait = Instant::now();
            let done = readback.finish(&gpu.device);
            sink.stats.waits += wait.elapsed();
            sink.show(done)?;
        }
    }
    let done = readback.finish(&gpu.device);
    sink.show(done)?;
    let Sink { writer, stats, check, .. } = sink;
    let written = writer.finish()?;
    drop(tty);
    if args.term_stats { report(&stats, &written, size.px(), scale, transport, started.elapsed()); }
    if let Some(c) = check {
        eprintln!("rune --term check: {} frames compared, {} wrong (worst {} pixels)", c.frames, c.wrong, c.worst);
    }
    Ok(())
}

/// Hands one input to the runtime; true for Ctrl-\\, which ends the run.
fn apply<S: 'static>(rt: &mut Runtime<S>, input: Input, scale: f32) -> bool {
    let p = |v: f32| v / scale;
    match input {
        Input::Key(k) => rt.key(k),
        Input::Move(x, y) => rt.pointer_move(p(x), p(y)),
        Input::Down(x, y) => { rt.pointer_move(p(x), p(y)); rt.pointer_down(p(x), p(y)) }
        Input::Up(x, y) => rt.pointer_up(p(x), p(y)),
        // As a window's wheel: `on_wheel` handlers hear it before a scroll view moves. A notch
        // down moves the content up (negative `dy`), one right moves it left.
        Input::Wheel(x, y, right, down) => rt.wheel_event(crate::WheelEvent::new(-right * WHEEL_POINTS, -down * WHEEL_POINTS).at(p(x), p(y))),
        Input::Paste(text) => rt.paste(text),
        Input::Quit => return true,
        Input::Focus(focused) => rt.window_focus(focused),
        Input::Background(_) | Input::Answered => {}
    }
    false
}

fn report(s: &Stats, written: &Written, (w, h): (u32, u32), scale: f32, transport: Transport, elapsed: Duration) {
    let sent = &written.sent;
    let frames = s.asked + s.motion;
    let n = f64::from(frames.max(1));
    let shown = sent.frames.max(1) as f64;
    let ms = |d: Duration, by: f64| d.as_secs_f64() * 1000.0 / by;
    eprintln!(
        "rune --term ({transport:?}, {w}x{h} px @{scale}x): {frames} frames in {:.1} s ({} asked, {} motion at {:.0} fps), {} sent; \
         per frame sent: {:.1} rects, {:.0} KB pixels, {:.1} KB pty; per frame: cpu {:.2} ms, readback wait {:.2} ms; \
         per frame sent: encode {:.2} ms, write {:.2} ms, start to pty {:.2} ms",
        elapsed.as_secs_f64(), s.asked, s.motion, f64::from(s.motion) / s.motion_secs.max(1e-9), sent.frames,
        sent.rects as f64 / shown, sent.pixels as f64 / 1024.0 / shown, sent.pty as f64 / 1024.0 / shown,
        ms(s.cpu, n), ms(s.waits, n), ms(written.encode, shown), ms(written.write, shown), ms(written.latency, shown),
    );
}
