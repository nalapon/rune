//! The terminal input decoder, fed reads as a terminal and a busy loop deliver them.

use super::*;

/// Feeds `reads` at their offsets in milliseconds, then an empty read past every wait, as the
/// loop does when its poll times out; the inputs in order, and the reads' flags together.
fn decode(reads: &[(u64, &[u8])]) -> (Vec<Input>, Fed) {
    let start = Instant::now();
    let mut state = Decode::default();
    let (mut inputs, mut all) = (Vec::new(), Fed::default());
    let end = reads.last().map_or(0, |r| r.0) + 100;
    for &(ms, bytes) in reads.iter().chain([&(end, &b""[..])]) {
        let fed = feed(&mut state, bytes, start + Duration::from_millis(ms), |i| {
            let quit = i == Input::Quit;
            inputs.push(i);
            quit
        });
        all = Fed { quit: all.quit || fed.quit, acted: all.acted || fed.acted, moved: all.moved || fed.moved };
    }
    (inputs, all)
}

fn inputs(reads: &[(u64, &[u8])]) -> Vec<Input> {
    decode(reads).0
}

fn plain(k: Key) -> Input {
    key(k, Mods::default())
}

fn text(s: &str) -> Input {
    plain(Key::Text(s.into()))
}

fn with_alt(s: &str) -> Input {
    key(Key::Text(s.into()), Mods { alt: true, ..Mods::default() })
}

#[test]
fn the_wheel_turns_on_both_axes() {
    let got = inputs(&[(0, b"\x1b[<64;10;20M\x1b[<65;10;20M\x1b[<66;10;20M\x1b[<67;10;20M")]);
    assert_eq!(got, [
        Input::Wheel(10.0, 20.0, 0.0, -1.0),
        Input::Wheel(10.0, 20.0, 0.0, 1.0),
        Input::Wheel(10.0, 20.0, -1.0, 0.0),
        Input::Wheel(10.0, 20.0, 1.0, 0.0),
    ]);
}

#[test]
fn only_the_primary_button_presses() {
    let got = inputs(&[(0, b"\x1b[<128;1;1M\x1b[<2;1;1M\x1b[<0;3;4M\x1b[<0;3;4m")]);
    assert_eq!(got, [Input::Down(3.0, 4.0), Input::Up(3.0, 4.0)]);
}

#[test]
fn every_pointer_move_reaches_the_app() {
    let (got, fed) = decode(&[(0, b"\x1b[<35;1;1M\x1b[<32;9;9M\x1b[<32;2;2M")]);
    assert_eq!(got, [Input::Move(1.0, 1.0), Input::Move(9.0, 9.0), Input::Move(2.0, 2.0)]);
    assert!(fed.moved && !fed.acted);
}

#[test]
fn a_lone_escape_waits_for_the_rest_of_a_sequence() {
    assert_eq!(inputs(&[(0, b"\x1b")]), [plain(Key::Escape)]);
    assert_eq!(inputs(&[(0, b"\x1bx")]), [with_alt("x")]);
    // A key read after the wait was typed on its own.
    assert_eq!(inputs(&[(0, b"\x1b"), (50, b"x")]), [plain(Key::Escape), text("x")]);
    assert_eq!(inputs(&[(0, b"\x1b\x1b")]), [plain(Key::Escape), plain(Key::Escape)]);
}

#[test]
fn sequences_split_across_reads_decode_whole() {
    assert_eq!(inputs(&[(0, b"ab\x1b"), (2, b"[<35;9;9M")]), [text("a"), text("b"), Input::Move(9.0, 9.0)]);
    // A loop busy past the wait still joins a sequence's own next byte.
    assert_eq!(inputs(&[(0, b"\x1b"), (50, b"[<35;9;9M")]), [Input::Move(9.0, 9.0)]);
    assert_eq!(inputs(&[(0, b"\x1bO"), (3, b"P")]), [plain(Key::Function(1))]);
    assert_eq!(inputs(&[(0, b"\x1b]11;rgb:1e1e/"), (5, b"1e1e/2e2e\x1b\\")]), [Input::Background([30, 30, 46])]);
}

#[test]
fn function_keys_arrive_in_every_encoding() {
    let got = inputs(&[(0, b"\x1bOP\x1bOS\x1b[15~\x1b[24~\x1b[P\x1b[Q\x1b[13~\x1b[S")]);
    let f = |n| plain(Key::Function(n));
    assert_eq!(got, [f(1), f(4), f(5), f(12), f(1), f(2), f(3), f(4)]);
    assert_eq!(inputs(&[(0, b"\x1b[15;5~")]), [key(Key::Function(5), ctrl(Mods::default()))]);
}

#[test]
fn kitty_private_use_keys_have_names_or_are_dropped() {
    let got = inputs(&[(0, b"\x1b[57376u\x1b[57387u\x1b[57363u\x1b[57399u\x1b[57428u\x1b[97;5u")]);
    assert_eq!(got, [plain(Key::Function(13)), plain(Key::Function(24)), plain(Key::ContextMenu), key(Key::Text("a".into()), ctrl(Mods::default()))]);
}

#[test]
fn graphics_replies_are_not_typed() {
    assert_eq!(inputs(&[(0, b"\x1b_Gi=31;OK\x1b\\x")]), [text("x")]);
    assert_eq!(inputs(&[(0, b"\x1b_"), (3, b"Gi=31;OK\x1b\\y")]), [text("y")]);
    // With nothing after it, ESC _ was Alt+_.
    assert_eq!(inputs(&[(0, b"\x1b_")]), [with_alt("_")]);
}

#[test]
fn an_unknown_ss3_key_keeps_both_keystrokes() {
    assert_eq!(inputs(&[(0, b"\x1bOa")]), [with_alt("O"), text("a")]);
    assert_eq!(inputs(&[(0, b"\x1bO")]), [with_alt("O")]);
}

#[test]
fn ctrl_backslash_ends_the_run_and_replies_ask_for_nothing() {
    let (got, fed) = decode(&[(0, b"a\x1cb")]);
    assert_eq!(got, [text("a"), Input::Quit, text("b")]);
    assert!(fed.quit);
    let (got, fed) = decode(&[(0, b"\x1b[?62;22c")]);
    assert_eq!(got, [Input::Answered]);
    assert!(!fed.acted && !fed.moved);
}
