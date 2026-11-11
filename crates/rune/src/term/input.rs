//! Terminal input: the bytes on stdin decoded into keys, pastes and pointer events, one byte at
//! a time. Keys come in the legacy encoding every terminal sends, or as `CSI … u` where the
//! terminal speaks the Kitty keyboard protocol (Tab and Ctrl-I, Esc and Alt sequences tell apart);
//! a paste arrives bracketed (mode 2004); the pointer as SGR reports in pixels (mode 1016), or
//! in cells; focus as `CSI I` and `CSI O` (mode 1004); and the terminal's answers to queries
//! (its background colour, its device attributes) as OSC and CSI replies.

use crate::edit::{Key, KeyEvent, Mods};
use crate::machine::Effects;

/// A decoded input. Positions are in pixels.
#[derive(Debug, PartialEq)]
pub(super) enum Input {
    Key(KeyEvent),
    Move(f32, f32),
    Down(f32, f32),
    Up(f32, f32),
    /// Position and notches, positive down.
    Wheel(f32, f32, f32),
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

/// Where the decoder is.
#[derive(Debug, Default)]
pub(super) enum Decode {
    #[default]
    Ground,
    /// After ESC: a sequence, Alt with a key, or Escape itself when the read ends here.
    Esc,
    /// Inside `ESC [`, with the bytes so far.
    Csi(Vec<u8>),
    /// Inside a UTF-8 character that needs `need` bytes in all.
    Utf8 { bytes: Vec<u8>, need: usize },
    /// Inside a bracketed paste, until `ESC [ 201 ~`.
    Paste(Vec<u8>),
    /// Inside `ESC ]`, until BEL or `ESC \\`; `esc` after an ESC in it.
    Osc { bytes: Vec<u8>, esc: bool },
}

/// Ends a bracketed paste.
const PASTE_END: &[u8] = b"\x1b[201~";

/// Feeds one byte.
pub(super) fn step(state: &mut Decode, b: u8) -> Effects<Input> {
    match std::mem::take(state) {
        Decode::Ground => ground(state, b, Mods::default()),
        Decode::Esc => match b {
            b'[' => { *state = Decode::Csi(Vec::new()); Effects::none() }
            b']' => { *state = Decode::Osc { bytes: Vec::new(), esc: false }; Effects::none() }
            0x1b => { *state = Decode::Esc; Effects::one(key(Key::Escape, Mods::default())) }
            _ => ground(state, b, Mods { alt: true, ..Mods::default() }),
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
        Decode::Osc { mut bytes, esc } => {
            if b == 0x07 || esc && b == b'\\' { return osc(&bytes).map_or(Effects::none(), Effects::one); }
            if b != 0x1b { bytes.push(b); }
            *state = Decode::Osc { bytes, esc: b == 0x1b };
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
            if b & 0xc0 != 0x80 { return ground(state, b, Mods::default()); }
            bytes.push(b);
            if bytes.len() < need {
                *state = Decode::Utf8 { bytes, need };
                return Effects::none();
            }
            std::str::from_utf8(&bytes).map_or(Effects::none(), |s| Effects::one(key(Key::Text(s.to_owned()), Mods::default())))
        }
    }
}

/// The read ended: an ESC with nothing after it was the Escape key.
pub(super) fn idle(state: &mut Decode) -> Effects<Input> {
    match std::mem::take(state) {
        Decode::Esc => Effects::one(key(Key::Escape, Mods::default())),
        // ESC ] with nothing after it in the read was Alt+], not a reply.
        Decode::Osc { bytes, .. } if bytes.is_empty() => Effects::one(key(Key::Text("]".into()), Mods { alt: true, ..Mods::default() })),
        other => { *state = other; Effects::none() }
    }
}

fn key(key: Key, mods: Mods) -> Input {
    Input::Key(KeyEvent { key, mods })
}

/// Ctrl with a letter. Off macOS, Ctrl is also the shortcut modifier (`Mods::cmd`).
fn ctrl(mods: Mods) -> Mods {
    Mods { ctrl: true, cmd: cfg!(not(target_os = "macos")), ..mods }
}

fn ground(state: &mut Decode, b: u8, mods: Mods) -> Effects<Input> {
    let input = match b {
        0x1b => { *state = Decode::Esc; return Effects::none(); }
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
    let k = match (fin, first) {
        (b'A', _) => Key::Up,
        (b'B', _) => Key::Down,
        (b'C', _) => Key::Right,
        (b'D', _) => Key::Left,
        (b'H', _) | (b'~', Some(1 | 7)) => Key::Home,
        (b'F', _) | (b'~', Some(4 | 8)) => Key::End,
        (b'Z', _) => return Some(key(Key::Tab, Mods { shift: true, ..Mods::default() })),
        (b'~', Some(2)) => Key::Insert,
        (b'~', Some(3)) => Key::Delete,
        (b'~', Some(5)) => Key::PageUp,
        (b'~', Some(6)) => Key::PageDown,
        _ => return None,
    };
    Some(key(k, mods))
}

/// A Kitty keyboard protocol key, `ESC [ code[:alternates] ; mods[:event] u`: a code point,
/// with Escape, Enter, Tab and Backspace by their C0 codes.
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
        c => {
            let ch = char::from_u32(c).filter(|ch| !ch.is_control())?;
            if mods.ctrl && ch == '\\' { return Some(Input::Quit); }
            Key::Text(ch.to_string())
        }
    };
    Some(key(k, mods))
}

/// An SGR report, `ESC [ < b ; x ; y M|m`: `b` 64 and up is the wheel, bit 32 a move.
fn pointer(params: &[u8], fin: u8) -> Option<Input> {
    let mut n = std::str::from_utf8(params).ok()?.split(';').map(|p| p.parse::<u32>().ok());
    let (b, x, y) = (n.next()??, n.next()?? as f32, n.next()?? as f32);
    Some(if b & 64 != 0 {
        Input::Wheel(x, y, if b & 1 == 0 { -1.0 } else { 1.0 })
    } else if b & 32 != 0 {
        Input::Move(x, y)
    } else if b & 3 != 0 {
        return None; // Only the primary button presses.
    } else if fin == b'M' {
        Input::Down(x, y)
    } else {
        Input::Up(x, y)
    })
}
