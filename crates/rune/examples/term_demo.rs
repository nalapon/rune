//! A small app to try `--term`: cards that lift on hover, a button, a switch and a theme
//! toggle. It's an ordinary Rune app; in Kitty or Ghostty:
//!
//! cargo run --release -p rune --example term_demo -- --term [zlib|base64|shm] [--term-stats]
//!
//! Space toggles the switch, t the theme; Ctrl-C quits.

use rune::tok::{self, type_};
use rune::{AppSpec, Color, Cx, El, Key, KeyEvent, Spring, TextStyle, Tx, col, el, row, text};

#[derive(Default)]
struct State {
    hovered: Option<usize>,
    clicks: u32,
    on: bool,
    light: bool,
}

const CARDS: [(&str, &str); 3] = [
    ("Springs", "Hover to lift"),
    ("Text", "Shaped by Rune"),
    ("Shadows", "Blurred on the GPU"),
];

fn card(i: usize, s: &State, t: &tok::Theme) -> El {
    let hot = s.hovered == Some(i);
    col().size(190.0, 112.0).padding(16.0).gap(6.0).radius(16.0)
        .fill(t.card).border(1.0, t.card_border)
        .shadow(if hot { 14.0 } else { 4.0 }, if hot { 28.0 } else { 10.0 }, Color::BLACK.alpha(if hot { 0.45 } else { 0.25 }))
        .motion(Spring::new(0.3, 0.0))
        .scale(if hot { 1.05 } else { 1.0 })
        .offset(0.0, if hot { -4.0 } else { 0.0 })
        .on_hover_change(move |s: &mut State, on, _: &mut Tx| {
            if on { s.hovered = Some(i) } else if s.hovered == Some(i) { s.hovered = None }
        })
        .child(el().size(10.0, 10.0).radius(5.0).fill(tok::accent(i, !s.light)))
        .child(text(CARDS[i].0).font(TextStyle::new(17.0, 650.0)).color(t.text))
        .child(text(CARDS[i].1).font(type_::BODY).color(t.text_secondary))
}

fn switch(s: &State, t: &tok::Theme) -> El {
    row().size(64.0, 36.0).padding(4.0).radius(18.0).center_y()
        .fill(if s.on { tok::accent(0, !s.light) } else { t.control_off })
        .on_press(|s: &mut State| s.on = !s.on)
        .child(el().size(28.0, 28.0).radius(14.0).fill(t.knob)
            .shadow(1.0, 3.0, Color::BLACK.alpha(0.3))
            .motion(Spring::new(0.32, 0.12))
            .offset(if s.on { 28.0 } else { 0.0 }, 0.0))
}

fn view(s: &State, _: &mut Cx<State>) -> El {
    let t = tok::theme(!s.light);
    let button = row().size(170.0, 44.0).radius(12.0).center()
        .fill(tok::accent(0, !s.light)).hover(tok::accent(1, !s.light))
        .on_press(|s: &mut State| s.clicks += 1)
        .child(text(format!("Clicked {}", s.clicks)).font(TextStyle::new(15.0, 600.0)).color(Color::WHITE));
    col().w_full().h_full().center().gap(32.0).fill(t.bg)
        .child(col().gap(6.0).items_center()
            .child(text("Rune in a terminal").font(TextStyle::new(30.0, 700.0)).color(t.text))
            .child(text("Kitty graphics protocol").font(type_::BODY).color(t.text_secondary)))
        .child(row().gap(20.0).children((0..CARDS.len()).map(|i| card(i, s, &t))))
        .child(row().gap(28.0).items_center().child(button).child(switch(s, &t)))
}

fn on_key(s: &mut State, k: &KeyEvent, _: &mut Tx) -> bool {
    match &k.key {
        Key::Text(c) if c == " " => s.on = !s.on,
        Key::Text(c) if c == "t" => s.light = !s.light,
        _ => return false,
    }
    true
}

fn main() {
    rune::run(AppSpec::<State> {
        on_key: Some(on_key),
        ..AppSpec::new("Term demo", (760.0, 480.0), State::default, view)
    });
}
