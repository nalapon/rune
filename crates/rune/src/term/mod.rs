//! `--term`: run the app inside a terminal that speaks the Kitty graphics protocol (Kitty,
//! Ghostty), as one image filling the window, with the terminal's mouse and keys as input.
//!
//! Each frame renders offscreen as a window's would. `damage` finds what changed from the
//! instances drawn, `readback` copies only those rectangles off the GPU while the next frame
//! renders, and `kitty` writes them into the terminal's image. An idle app draws and sends
//! nothing. `--scale` defaults to 2 when the terminal's cells are at least 30 pixels tall.
//!
//! The pointer takes the shape the element under it asks for (OSC 22). Ctrl-C reaches the app
//! as a key; Ctrl-\ always ends the run. Single-window apps only for
//! now; native menus, file dialogs and pop-up menus aren't there.

mod cells;
mod check;
mod damage;
mod input;
mod kitty;
mod readback;
mod tty;

use std::time::Duration;

pub(crate) use cells::run as run_cells;
pub(crate) use kitty::Transport;

use crate::app::App;
use crate::compose::Frame;
use crate::el::Cursor;
use crate::frame::clock::FrameClock;
use crate::render::{Gpu, Instance, OFFSCREEN_FORMAT, Renderer};
use crate::runtime::{Env, Runtime};
use crate::time::Instant;
use crate::tooling::{Args, Length, runtime_for};

use check::Check;
use damage::Rect;
use input::{Decode, Input};
use kitty::Kitty;
use readback::Readback;
use tty::{Size, Tty};

/// The fastest the loop draws while something moves.
const FRAME_GAP: Duration = Duration::from_micros(8_333);

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
    /// From a frame's start to its bytes reaching the pty.
    latency: Duration,
    encode: Duration,
    write: Duration,
}

/// Where finished frames go: encoded and written to the terminal, and to the check when on.
struct Sink {
    tty: Tty,
    kitty: Kitty,
    stats: Stats,
    check: Option<Check>,
}

impl Sink {
    fn show(&mut self, frames: Vec<(Tag, Vec<readback::Patch>)>) -> Result<(), String> {
        for (tag, patches) in frames {
            let t = Instant::now();
            let bytes = self.kitty.frame(tag.size, &patches).map_err(|e| format!("--term: {e}"))?;
            let t1 = Instant::now();
            if !bytes.is_empty() { self.tty.write(bytes); }
            self.stats.encode += t1 - t;
            self.stats.write += t1.elapsed();
            self.stats.latency += tag.started.elapsed();
            if let Some(c) = self.check.as_mut() { c.sent(tag.size, &patches); }
        }
        Ok(())
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
    let mut sink = Sink { tty, kitty: Kitty::new(transport), stats: Stats::default(), check: Check::from_env() };
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
    let (mut asked, mut animating, mut last_frame) = (true, false, started);
    let mut pointer = "default";

    loop {
        if quit_after.is_some_and(|d| started.elapsed() >= d) || crate::quit::quitting() { break; }
        // Wait for input, a wake, the next motion frame, a timer, or a readback in flight.
        let now = Instant::now();
        let mut timeout = if asked { Some(Duration::ZERO) }
            else if animating { Some(FRAME_GAP.saturating_sub(now - last_frame)) }
            else { rt.wake_deadline().map(|d| Duration::from_secs_f64((d - (now - started).as_secs_f64()).max(0.0))) };
        let soonest = |t: Option<Duration>, limit: Duration| Some(t.map_or(limit, |t| t.min(limit)));
        if readback.in_flight() { timeout = soonest(timeout, Duration::from_millis(1)); }
        if let Some(d) = quit_after { timeout = soonest(timeout, d.saturating_sub(started.elapsed())); }
        let n = sink.tty.wait(timeout, &mut buf);
        let mut interrupted = false;
        for &b in &buf[..n] {
            for i in input::step(&mut decode, b) { interrupted |= apply(&mut rt, i, scale); }
        }
        if n > 0 { for i in input::idle(&mut decode) { interrupted |= apply(&mut rt, i, scale); } }
        if interrupted { break; }
        asked |= n > 0;
        let updates = shared.take().updates;
        asked |= !updates.is_empty();
        for f in updates { rt.update_if(f); }
        if let Some(now) = Size::now() && now != size {
            size = now;
            target = Target::new(&gpu.device, size.px(), format);
            rt.set_viewport(logical(size), scale);
            last = None;
            asked = true;
        }
        sink.show(readback.ready(&gpu.device))?;

        let now = Instant::now();
        let due = rt.wake_deadline().is_some_and(|d| d <= (now - started).as_secs_f64());
        if !(asked || due || animating && now - last_frame >= FRAME_GAP) { continue; }
        let stats = &mut sink.stats;
        if asked || due { stats.asked += 1 } else { stats.motion += 1; stats.motion_secs += (now - last_frame).as_secs_f64() }
        asked = false;
        last_frame = now;

        crate::clipboard::flush_memory(&mut rt);
        if rt.popup_menu().is_some() { rt.cancel_popups(); }
        rt.refuse_file_dialog("native file dialogs are unsupported in a terminal");
        let comp_animating = comp.as_ref().is_some_and(|c| c.animating());
        let (moving, dt) = clock.advance(&mut rt, now, comp_animating);
        if rt.shell_request.take().is_some() { break; }

        // What to send: everything after a resize or with things damage can't see (a
        // compositor's passes, native surfaces, image pixels changing in place).
        let px = size.px();
        let images_changed = rt.images.pages.iter().any(|p| p.dirty.is_some());
        let clear = rt.clear_color();
        let mut rects = match &last {
            Some((old, old_clear)) if comp.is_none() && rt.surfaces.is_empty() && !images_changed && *old_clear == clear => damage::between(old, &rt.instances, px),
            _ => vec![Rect::whole(px)],
        };
        // Damage still says whether anything changed; shared memory then sends the whole frame.
        if transport.whole_frames() && !rects.is_empty() { rects = vec![Rect::whole(px)]; }
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
        let copying = !rects.is_empty();
        if copying {
            let wait = Instant::now();
            let done = readback.make_room(&gpu.device);
            sink.stats.waits += wait.elapsed();
            sink.show(done)?;
            readback.copy(&gpu.device, &mut enc, &target.texture, rects, Tag { size: px, started: now });
        }
        if let Some(c) = sink.check.as_mut() { c.copy(&gpu.device, &mut enc, &target.texture, px); }
        renderer.submit(&gpu.queue, enc);
        if copying { readback.submitted(); }
        if let Some(c) = sink.check.as_mut() { c.submitted(); }
        clock.presented(&mut rt, Instant::now());
        sink.stats.cpu += now.elapsed();
        if sink.check.is_some() {
            let done = readback.finish(&gpu.device);
            sink.show(done)?;
            sink.check.as_mut().unwrap().compare(&gpu.device);
        }
        if let Some(mut ready) = on_ready.take() { ready(&crate::WindowHandle::new("main")); }
        let shape = cursor_name(&rt.cursor());
        if shape != pointer {
            pointer = shape;
            sink.tty.write(format!("\x1b]22;{shape}\x1b\\").as_bytes());
        }

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
    let Sink { tty, kitty, stats, check } = sink;
    let sent = std::mem::take(&mut { kitty }.sent);
    drop(tty);
    if args.term_stats { report(&stats, &sent, size.px(), scale, transport, started.elapsed()); }
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
        // down moves the content up (negative `dy`).
        Input::Wheel(x, y, notches) => rt.wheel_event(crate::WheelEvent::new(0.0, -notches * 40.0).at(p(x), p(y))),
        Input::Paste(text) => rt.paste(text),
        Input::Quit => return true,
        Input::Focus(focused) => rt.window_focus(focused),
        Input::Background(_) | Input::Answered => {}
    }
    false
}

fn report(s: &Stats, sent: &kitty::Sent, (w, h): (u32, u32), scale: f32, transport: Transport, elapsed: Duration) {
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
        ms(s.cpu, n), ms(s.waits, n), ms(s.encode, shown), ms(s.write, shown), ms(s.latency, shown),
    );
}
