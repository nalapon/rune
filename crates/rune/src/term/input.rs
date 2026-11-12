//! Terminal input: the bytes on stdin decoded into keys, pastes and pointer events, one byte at
//! a time. Keys come in the legacy encoding every terminal sends (F1 to F4 as `ESC O P` to `S`),
//! or as `CSI … u` where the terminal speaks the Kitty keyboard protocol (Tab and Ctrl-I, Esc and
//! Alt sequences tell apart); a paste arrives bracketed (mode 2004); the pointer as SGR reports
//! in pixels (mode 1016), or in cells; focus as `CSI I` and `CSI O` (mode 1004); and the
//! terminal's answers to queries (its background colour, its device attributes) as OSC and CSI
//! replies. Kitty graphics replies (APC, `ESC _ … ESC \`) are read and dropped.

use std::time::Duration;

use crate::edit::{Key, KeyEvent, Mods};
use crate::machine::Effects;
use crate::time::Instant;

/// How long a prefix that is also a key (ESC, `ESC O`, `ESC ]`, `ESC _`) waits for the rest of
/// its sequence. A terminal writes a sequence at once, but a read, or SSH, can split it; what
/// still has nothing after it then was the key. Terminals with the Kitty keyboard protocol send
/// Escape and Alt keys as `CSI … u`, so they never wait.
pub(super) const WAIT: Duration = Duration::from_millis(30);

/// A decoded input. Positions are in pixels.
#[derive(Debug, PartialEq)]
pub(super) enum Input {
    Key(KeyEvent),
    Move(f32, f32),
    Down(f32, f32),
    Up(f32, f32),
    /// Position, then notches right and down (negative: left and up).
    Wheel(f32, f32, f32, f32),
    /// Bracketed paste: the text, as the terminal pasted it.
    Paste(String),
    /// Ctrl-\: the run ends, whatever the app does with keys. Ctrl-C goes to the app.
    Quit,
    /// The terminal's window gained (true) or lost focus.
    Focus(bool),
    /// The answer to `OSC 11 ; ?`: the terminal's background, sRGB.
    Background([u8; 3]),
    /// The answer to a device attributes query (`CSI c`), which every terminal answers: the
    /// replies to the queries sent before it have all arrived.
    Answered,
}

/// Where the decoder is. A prefix that is also a key keeps when it came (`at`): with nothing
/// after it for [`WAIT`], it was the key.
#[derive(Debug, Default)]
pub(super) enum Decode {
    #[default]
    Ground,
    /// After ESC: a sequence, Alt with a key, or the Escape key.
    Esc { at: Instant },
    /// Inside `ESC [`, with the bytes so far.
    Csi(Vec<u8>),
    /// After `ESC O`: F1 to F4, an arrow in the cursor keys' application mode, or Alt+O.
    Ss3 { at: Instant },
    /// After `ESC ]` or `ESC _` (its `opener`): a reply from the terminal, or Alt with that key.
    Opened { opener: u8, at: Instant },
    /// Inside a UTF-8 character that needs `need` bytes in all.
    Utf8 { bytes: Vec<u8>, need: usize },
    /// Inside a bracketed paste, until `ESC [ 201 ~`.
    Paste(Vec<u8>),
    /// Inside a string the terminal sends: an OSC reply (`ESC ]`, its `opener`) or a Kitty
    /// graphics reply (`ESC _`, APC), until BEL or `ESC \\`; `esc` after an ESC in it.
    Str { opener: u8, bytes: Vec<u8>, esc: bool },
}

/// Ends a bracketed paste.
const PASTE_END: &[u8] = b"\x1b[201~";

/// Feeds one byte, which came `at`.
pub(super) fn step(state: &mut Decode, b: u8, at: Instant) -> Effects<Input> {
    match std::mem::take(state) {
        Decode::Ground => ground(state, b, Mods::default(), at),
        Decode::Esc { at: since } => match b {
            b'[' => { *state = Decode::Csi(Vec::new()); Effects::none() }
            b']' | b'_' => { *state = Decode::Opened { opener: b, at }; Effects::none() }
            b'O' => { *state = Decode::Ss3 { at }; Effects::none() }
            0x1b => { *state = Decode::Esc { at }; Effects::one(key(Key::Escape, Mods::default())) }
            // A key read after the wait was its own: Escape, then it. (A late `[`, `O`, `]` or
            // `_` above still joins: a sequence split by a busy loop is likelier than a key.)
            _ if at >= since + WAIT => {
                let mut fx = Effects::one(key(Key::Escape, Mods::default()));
                fx.extend(ground(state, b, Mods::default(), at));
                fx
            }
            _ => ground(state, b, Mods { alt: true, ..Mods::default() }, at),
        },
        Decode::Csi(mut params) => {
            if !(0x40..=0x7e).contains(&b) {
                params.push(b);
                *state = Decode::Csi(params);
                return Effects::none();
            }
            if (params.as_slice(), b) == (b"200".as_slice(), b'~') {
                *state = Decode::Paste(Vec::new());
                return Effects::none();
            }
            csi(&params, b).map_or(Effects::none(), Effects::one)
        }
        Decode::Ss3 { .. } => match ss3(b) {
            Some(k) => Effects::one(k),
            // Not a key's sequence: Alt+O, then the byte on its own.
            None => {
                let mut fx = Effects::one(alt(b'O'));
                fx.extend(ground(state, b, Mods::default(), at));
                fx
            }
        },
        Decode::Opened { opener, .. } => {
            *state = Decode::Str { opener, bytes: Vec::new(), esc: false };
            step(state, b, at)
        }
        Decode::Str { opener, mut bytes, esc } => {
            if b == 0x07 || esc && b == b'\\' {
                return if opener == b']' { osc(&bytes).map_or(Effects::none(), Effects::one) } else { Effects::none() };
            }
            if b != 0x1b { bytes.push(b); }
            *state = Decode::Str { opener, bytes, esc: b == 0x1b };
            Effects::none()
        }
        Decode::Paste(mut bytes) => {
            bytes.push(b);
            if bytes.ends_with(PASTE_END) {
                bytes.truncate(bytes.len() - PASTE_END.len());
                return Effects::one(Input::Paste(String::from_utf8_lossy(&bytes).into_owned()));
            }
            *state = Decode::Paste(bytes);
            Effects::none()
        }
        Decode::Utf8 { mut bytes, need } => {
            if b & 0xc0 != 0x80 { return ground(state, b, Mods::default(), at); }
            bytes.push(b);
            if bytes.len() < need {
                *state = Decode::Utf8 { bytes, need };
                return Effects::none();
            }
            std::str::from_utf8(&bytes).map_or(Effects::none(), |s| Effects::one(key(Key::Text(s.to_owned()), Mods::default())))
        }
    }
}

/// What one read brought, beyond the inputs `feed` handed on.
#[derive(Default)]
pub(super) struct Fed {
    /// An input ended the run (Ctrl-\\).
    pub(super) quit: bool,
    /// A key, press, release, wheel, paste or focus change: the next frame shows it at once.
    pub(super) acted: bool,
    /// The pointer moved.
    pub(super) moved: bool,
}

/// Decodes the bytes one read brought `at` (none after a timeout) into inputs for `apply`, in
/// order; a prefix that waited [`WAIT`] with nothing after it becomes its key. `apply` returns
/// true for an input that ends the run.
pub(super) fn feed(decode: &mut Decode, bytes: &[u8], at: Instant, mut apply: impl FnMut(Input) -> bool) -> Fed {
    let mut inputs: Vec<Input> = bytes.iter().flat_map(|&b| step(decode, b, at)).collect();
    if decode.deadline().is_some_and(|d| d <= at) { inputs.extend(expire(decode)); }
    let mut fed = Fed::default();
    for input in inputs {
        let moving = matches!(input, Input::Move(..));
        fed.moved |= moving;
        fed.acted |= !moving && !matches!(input, Input::Background(_) | Input::Answered);
        fed.quit |= apply(input);
    }
    fed
}

impl Decode {
    /// When a prefix that is also a key stops waiting for the rest of its sequence; the caller
    /// feeds the decoder then, with no bytes if none came.
    pub(super) fn deadline(&self) -> Option<Instant> {
        match self {
            Decode::Esc { at } | Decode::Ss3 { at } | Decode::Opened { at, .. } => Some(*at + WAIT),
            _ => None,
        }
    }
}

/// Nothing followed a prefix within [`WAIT`]: a lone ESC was the Escape key, and `ESC O`,
/// `ESC ]` or `ESC _` Alt with that key.
fn expire(state: &mut Decode) -> Effects<Input> {
    match std::mem::take(state) {
        Decode::Esc { .. } => Effects::one(key(Key::Escape, Mods::default())),
        Decode::Ss3 { .. } => Effects::one(alt(b'O')),
        Decode::Opened { opener, .. } => Effects::one(alt(opener)),
        other => { *state = other; Effects::none() }
    }
}

/// Alt with an ASCII key.
fn alt(c: u8) -> Input {
    key(Key::Text(char::from(c).to_string()), Mods { alt: true, ..Mods::default() })
}

fn key(key: Key, mods: Mods) -> Input {
    Input::Key(KeyEvent { key, mods })
}

/// Ctrl with a letter. Off macOS, Ctrl is also the shortcut modifier (`Mods::cmd`).
fn ctrl(mods: Mods) -> Mods {
    Mods { ctrl: true, cmd: cfg!(not(target_os = "macos")), ..mods }
}

fn ground(state: &mut Decode, b: u8, mods: Mods, at: Instant) -> Effects<Input> {
    let input = match b {
        0x1b => { *state = Decode::Esc { at }; return Effects::none(); }
        0x1c => Input::Quit,
        b'\r' | b'\n' => key(Key::Enter, mods),
        b'\t' => key(Key::Tab, mods),
        0x7f | 0x08 => key(Key::Backspace, mods),
        0x01..=0x1a => key(Key::Text(char::from(b'a' + b - 1).to_string()), ctrl(mods)),
        0x20..=0x7e => key(Key::Text(char::from(b).to_string()), mods),
        0xc0..=0xf7 => {
            let need = if b >= 0xf0 { 4 } else if b >= 0xe0 { 3 } else { 2 };
            *state = Decode::Utf8 { bytes: vec![b], need };
            return Effects::none();
        }
        _ => return Effects::none(),
    };
    Effects::one(input)
}

/// `1 + bits` in a CSI's modifier parameter: shift 1, alt 2, ctrl 4.
fn csi_mods(param: Option<u32>) -> Mods {
    let bits = param.unwrap_or(1).saturating_sub(1);
    let mods = Mods { shift: bits & 1 != 0, alt: bits & 2 != 0, ..Mods::default() };
    if bits & 4 != 0 { ctrl(mods) } else { mods }
}

/// An OSC reply: only `11 ; rgb:R/G/B`, each 1 to 4 hex digits, is read.
fn osc(bytes: &[u8]) -> Option<Input> {
    let rgb = std::str::from_utf8(bytes).ok()?.strip_prefix("11;rgb:")?;
    let mut parts = rgb.split('/').map(|h| {
        let v = u32::from_str_radix(h, 16).ok()?;
        let max = (1u32 << (4 * h.len().clamp(1, 4))) - 1;
        Some((v * 255 / max) as u8)
    });
    Some(Input::Background([parts.next()??, parts.next()??, parts.next()??]))
}

/// The key after `ESC O`.
fn ss3(fin: u8) -> Option<Input> {
    let k = match fin {
        b'P'..=b'S' => Key::Function(fin - b'P' + 1),
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        _ => return None,
    };
    Some(key(k, Mods::default()))
}

/// F1 to F12 by their `CSI n ~` number.
fn function_tilde(n: u32) -> Option<u8> {
    let f = match n {
        11..=15 => n - 10,
        17..=21 => n - 11,
        23 | 24 => n - 12,
        _ => return None,
    };
    u8::try_from(f).ok()
}

fn csi(params: &[u8], fin: u8) -> Option<Input> {
    if let [b'<', rest @ ..] = params {
        return pointer(rest, fin);
    }
    match (params, fin) {
        ([b'?', ..], b'c') => return Some(Input::Answered),
        ([], b'I') => return Some(Input::Focus(true)),
        ([], b'O') => return Some(Input::Focus(false)),
        _ => {}
    }
    if fin == b'u' {
        return kitty_key(params);
    }
    let mut nums = std::str::from_utf8(params).ok()?.split(';').map(|p| p.parse::<u32>().ok());
    let (first, mods) = (nums.next().flatten(), csi_mods(nums.next().flatten()));
    if fin == b'~' && let Some(f) = first.and_then(function_tilde) { return Some(key(Key::Function(f), mods)); }
    let k = match (fin, first) {
        (b'A', _) => Key::Up,
        (b'B', _) => Key::Down,
        (b'C', _) => Key::Right,
        (b'D', _) => Key::Left,
        (b'H', _) | (b'~', Some(1 | 7)) => Key::Home,
        (b'F', _) | (b'~', Some(4 | 8)) => Key::End,
        (b'Z', _) => return Some(key(Key::Tab, Mods { shift: true, ..Mods::default() })),
        // F1, F2 and F4 as the Kitty keyboard protocol sends them (F3 is `CSI 13 ~`).
        (b'P', _) => Key::Function(1),
        (b'Q', _) => Key::Function(2),
        (b'S', _) => Key::Function(4),
        (b'~', Some(2)) => Key::Insert,
        (b'~', Some(3)) => Key::Delete,
        (b'~', Some(5)) => Key::PageUp,
        (b'~', Some(6)) => Key::PageDown,
        _ => return None,
    };
    Some(key(k, mods))
}

/// A Kitty keyboard protocol key, `ESC [ code[:alternates] ; mods[:event] u`: a code point,
/// with Escape, Enter, Tab and Backspace by their C0 codes, and keys without one in the
/// Private Use Area.
fn kitty_key(params: &[u8]) -> Option<Input> {
    let text = std::str::from_utf8(params).ok()?;
    let mut fields = text.split(';');
    let code: u32 = fields.next()?.split(':').next()?.parse().ok()?;
    let mods = csi_mods(fields.next().and_then(|m| m.split(':').next()?.parse().ok()));
    let k = match code {
        27 => Key::Escape,
        13 => Key::Enter,
        9 => Key::Tab,
        127 | 8 => Key::Backspace,
        57363 => Key::ContextMenu,
        57376..=57387 => Key::Function(u8::try_from(code - 57376 + 13).ok()?),
        // The rest of the Private Use Area: keys Rune has no name for (keypad, media, locks).
        0xe000..=0xf8ff => return None,
        c => {
            let ch = char::from_u32(c).filter(|ch| !ch.is_control())?;
            if mods.ctrl && ch == '\\' { return Some(Input::Quit); }
            Key::Text(ch.to_string())
        }
    };
    Some(key(k, mods))
}

/// An SGR report, `ESC [ < b ; x ; y M|m`: `b` 64 to 67 is the wheel (up, down, left, right),
/// bit 32 a move, 128 and up buttons 8 to 11.
fn pointer(params: &[u8], fin: u8) -> Option<Input> {
    let mut n = std::str::from_utf8(params).ok()?.split(';').map(|p| p.parse::<u32>().ok());
    let (b, x, y) = (n.next()??, n.next()?? as f32, n.next()?? as f32);
    Some(if b & 64 != 0 {
        let notch = if b & 1 == 0 { -1.0 } else { 1.0 };
        if b & 2 == 0 { Input::Wheel(x, y, 0.0, notch) } else { Input::Wheel(x, y, notch, 0.0) }
    } else if b & 32 != 0 {
        Input::Move(x, y)
    } else if b & (3 | 128) != 0 {
        return None; // Only the primary button presses.
    } else if fin == b'M' {
        Input::Down(x, y)
    } else {
        Input::Up(x, y)
    })
}

#[cfg(test)]
#[path = "input_tests.rs"]
mod tests;
