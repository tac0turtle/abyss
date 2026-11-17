use std::cell::RefCell;
use std::rc::Rc;

use abyss_core::rich_text::{EntityKind, RawEntity};
use gpui::{
    AppContext, Context, FontStyle, FontWeight, InteractiveText, IntoElement, Modifiers,
    ParentElement, Render, Styled, StyledText, TestAppContext, Window, black, div, font, point, px,
    size,
};

use super::*;

fn fonts() -> Fonts {
    Fonts {
        body: font(".SystemUIFont"),
        mono: font("Menlo"),
    }
}

fn palette() -> Palette {
    Palette {
        text: black(),
        muted: gpui::hsla(0., 0., 0.5, 1.),
        link: gpui::hsla(0.6, 0.9, 0.4, 1.),
        mention: gpui::hsla(0.7, 0.9, 0.4, 1.),
        code_bg: gpui::hsla(0., 0., 0.95, 1.),
        spoiler: gpui::hsla(0., 0., 0.7, 1.),
    }
}

fn entity(offset: i32, length: i32, kind: EntityKind) -> RawEntity {
    RawEntity {
        offset,
        length,
        kind,
    }
}

fn styled(text: &str, entities: &[RawEntity], spoilers: Spoilers) -> Body {
    style(
        &RichText::from_entities(text, entities),
        &fonts(),
        &palette(),
        spoilers,
    )
}

/// The runs partition the display text exactly, on char boundaries.
fn assert_partition(s: &Body) {
    let mut pos = 0;
    for run in &s.runs {
        assert!(run.len > 0, "empty run");
        pos += run.len;
        assert!(s.text.is_char_boundary(pos), "run ends inside a char");
    }
    assert_eq!(pos, s.text.len());
    for (range, _) in &s.links {
        assert!(s.text.get(range.clone()).is_some());
    }
}

/// The display text of each run.
fn pieces(s: &Body) -> Vec<&str> {
    let mut pos = 0;
    s.runs
        .iter()
        .map(|r| {
            let piece = &s.text[pos..pos + r.len];
            pos += r.len;
            piece
        })
        .collect()
}

#[test]
fn multi_byte_lengths_are_utf8_bytes() {
    // "a😀é" is 1 + 2 + 1 UTF-16 units and 1 + 4 + 2 bytes; bold the emoji.
    let s = styled(
        "a\u{1F600}é",
        &[entity(1, 2, EntityKind::Bold)],
        Spoilers::Hidden,
    );
    assert_partition(&s);
    assert_eq!(pieces(&s), ["a", "\u{1F600}", "é"]);
    assert_eq!(s.runs.iter().map(|r| r.len).collect::<Vec<_>>(), [1, 4, 2]);
    assert_eq!(s.runs[1].font.weight, FontWeight::BOLD);
    assert_eq!(s.runs[0].font.weight, FontWeight::default());
}

#[test]
fn overlapping_styles_combine() {
    let s = styled(
        "bold both italic",
        &[
            entity(0, 9, EntityKind::Bold),
            entity(5, 11, EntityKind::Italic),
            entity(10, 6, EntityKind::Strike),
        ],
        Spoilers::Hidden,
    );
    assert_partition(&s);
    assert_eq!(pieces(&s), ["bold ", "both", " ", "italic"]);
    let both = &s.runs[1];
    assert_eq!(both.font.weight, FontWeight::BOLD);
    assert_eq!(both.font.style, FontStyle::Italic);
    assert!(s.runs[3].strikethrough.is_some());
    assert!(s.runs[0].strikethrough.is_none());
}

#[test]
fn code_is_monospace_with_background() {
    let s = styled(
        "run `ls` now",
        &[
            entity(4, 4, EntityKind::Code),
            entity(9, 3, EntityKind::Underline),
        ],
        Spoilers::Hidden,
    );
    assert_partition(&s);
    assert_eq!(s.runs[1].font.family, "Menlo");
    assert!(s.runs[1].background_color.is_some());
    assert!(s.runs[3].underline.is_some());
}

#[test]
fn hidden_link_target_is_shown_and_copied() {
    // RTL text before the link must not disturb byte offsets.
    let text = "مرحبا click here";
    let s = styled(
        text,
        &[entity(
            6,
            10,
            EntityKind::TextUrl("https://evil.example/\u{202E}gpj.exe".into()),
        )],
        Spoilers::Hidden,
    );
    assert_partition(&s);
    assert_eq!(
        s.text, "مرحبا click here (https://evil.example/gpj.exe)",
        "the real target is shown, with bidi overrides stripped"
    );
    let [(range, target)] = s.links.as_slice() else {
        panic!("one link expected");
    };
    assert_eq!(
        &s.text[range.clone()],
        "click here (https://evil.example/gpj.exe)"
    );
    assert_eq!(
        target, "https://evil.example/\u{202E}gpj.exe",
        "copied verbatim"
    );
    assert!(
        s.runs.last().unwrap().underline.is_none(),
        "target is muted"
    );
}

#[test]
fn visible_link_and_adjacent_links_stay_separate() {
    let text = "https://a.example https://b.example";
    let s = styled(
        text,
        &[
            entity(0, 17, EntityKind::Url),
            entity(18, 17, EntityKind::Url),
        ],
        Spoilers::Hidden,
    );
    assert_partition(&s);
    assert_eq!(s.text, text, "visible links add nothing");
    let targets: Vec<&str> = s.links.iter().map(|(_, t)| t.as_ref()).collect();
    assert_eq!(targets, ["https://a.example", "https://b.example"]);
    assert_eq!(s.links[0].0, 0..17);
    assert_eq!(s.links[1].0, 18..35);
}

#[test]
fn spoilers_hide_until_revealed() {
    let entities = [entity(4, 6, EntityKind::Spoiler)];
    let hidden = styled("the secret word", &entities, Spoilers::Hidden);
    assert_eq!(hidden.spoilers, vec![4..10]);
    assert_eq!(hidden.runs[1].color, palette().spoiler);
    assert_eq!(hidden.runs[1].background_color, Some(palette().spoiler));
    let shown = styled("the secret word", &entities, Spoilers::Revealed);
    assert!(shown.spoilers.is_empty());
    assert_eq!(shown.runs[1].color, palette().text);
}

#[test]
fn mention_is_marked_by_color_and_weight() {
    let s = styled(
        "hi @bob",
        &[entity(3, 4, EntityKind::Mention)],
        Spoilers::Hidden,
    );
    assert_eq!(s.runs[1].color, palette().mention);
    assert_eq!(s.runs[0].font.weight, FontWeight::NORMAL);
    assert_eq!(s.runs[1].font.weight, FontWeight::SEMIBOLD);
}

/// A small deterministic generator (no extra dev-dependency).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from(self.0 >> 33).unwrap() % bound.max(1)
    }
}

const ALPHABET: [&str; 10] = [
    "a",
    " ",
    "é",
    "\u{1F600}",
    "مر",
    "中",
    "\u{1F469}\u{200D}\u{1F4BB}",
    "\n",
    "\u{202E}",
    "ß",
];

fn kind(rng: &mut Lcg) -> EntityKind {
    match rng.next(10) {
        0 => EntityKind::Bold,
        1 => EntityKind::Italic,
        2 => EntityKind::Underline,
        3 => EntityKind::Strike,
        4 => EntityKind::Code,
        5 => EntityKind::Pre,
        6 => EntityKind::Spoiler,
        7 => EntityKind::Mention,
        8 => EntityKind::Url,
        _ => EntityKind::TextUrl("https://t.example/x".into()),
    }
}

/// Random texts and entities, including offsets inside surrogate pairs and
/// out of range.
fn generated(cases: usize) -> Vec<RichText> {
    let mut rng = Lcg(0x5eed);
    (0..cases)
        .map(|_| {
            let text: String = (0..rng.next(40)).map(|_| ALPHABET[rng.next(10)]).collect();
            let units = i32::try_from(text.encode_utf16().count()).unwrap();
            let entities: Vec<RawEntity> = (0..rng.next(12))
                .map(|_| {
                    let offset = i32::try_from(rng.next(48)).unwrap() - 2;
                    let length = i32::try_from(rng.next(48)).unwrap() - 2;
                    entity(offset.min(units + 1), length, kind(&mut rng))
                })
                .collect();
            RichText::from_entities(&text, &entities)
        })
        .collect()
}

#[test]
fn generated_inputs_always_partition() {
    for rt in generated(2_000) {
        for spoilers in [Spoilers::Hidden, Spoilers::Revealed] {
            assert_partition(&style(&rt, &fonts(), &palette(), spoilers));
        }
    }
}

/// The plan's headless `with_runs` test: every generated body is laid out and
/// painted by GPUI, through `StyledText::with_runs` and `InteractiveText`,
/// without panicking.
#[gpui::test]
fn with_runs_renders_headless(cx: &mut TestAppContext) {
    let window = cx.add_empty_window();
    for chunk in generated(300).chunks(30) {
        let bodies: Vec<Body> = chunk
            .iter()
            .flat_map(|rt| {
                [Spoilers::Hidden, Spoilers::Revealed]
                    .map(|spoilers| style(rt, &fonts(), &palette(), spoilers))
            })
            .collect();
        let view = window.update(|_, cx| cx.new(|_| Bodies(bodies)));
        window.draw(
            point(px(0.), px(0.)),
            size(px(240.), px(20_000.)),
            |_, _| view.clone().into_any_element(),
        );
    }
}

/// A view, so `InteractiveText` has a rendering entity.
struct Bodies(Vec<Body>);

impl Render for Bodies {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .children(self.0.iter().enumerate().map(|(ix, s)| {
                let ranges = s
                    .links
                    .iter()
                    .map(|(r, _)| r.clone())
                    .chain(s.spoilers.clone());
                InteractiveText::new(
                    ix,
                    StyledText::new(s.text.clone()).with_runs(s.runs.clone()),
                )
                .on_click(ranges.collect(), |_, _, _| {})
            }))
    }
}

#[test]
fn spoiler_hides_a_link_target_until_revealed() {
    let entities = [
        entity(4, 6, EntityKind::Spoiler),
        entity(4, 6, EntityKind::TextUrl("https://secret.example".into())),
    ];
    let hidden = styled("the secret word", &entities, Spoilers::Hidden);
    assert_partition(&hidden);
    assert_eq!(hidden.text, "the secret word", "no target suffix");
    assert!(hidden.links.is_empty(), "nothing to copy while hidden");
    assert_eq!(hidden.spoilers, vec![4..10]);
    let shown = styled("the secret word", &entities, Spoilers::Revealed);
    assert_eq!(shown.text, "the secret (https://secret.example) word");
    assert_eq!(shown.links.len(), 1);
}

#[test]
fn a_partly_hidden_link_is_one_reveal_range() {
    // The spoiler covers only "ret" of the link "secret".
    let entities = [
        entity(4, 6, EntityKind::TextUrl("https://secret.example".into())),
        entity(7, 3, EntityKind::Spoiler),
    ];
    let hidden = styled("the secret word", &entities, Spoilers::Hidden);
    assert_partition(&hidden);
    assert_eq!(hidden.text, "the secret word");
    assert!(hidden.links.is_empty());
    assert_eq!(hidden.spoilers, vec![4..10]);
}

/// One message body as clickable text, recording what each click asked for.
struct Clickable {
    text: RichText,
    spoilers: Spoilers,
    clicks: Rc<RefCell<Vec<Click>>>,
}

impl Render for Clickable {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        let clicks = Rc::clone(&self.clicks);
        let body = style(&self.text, &fonts(), &palette(), self.spoilers);
        div()
            .size_full()
            .child(body.interactive("body", move |click, _, _| clicks.borrow_mut().push(click)))
    }
}

/// A spoiler and a link over the same text.
fn spoiler_link() -> [RawEntity; 2] {
    [
        entity(0, 6, EntityKind::Spoiler),
        entity(0, 6, EntityKind::TextUrl("https://secret.example".into())),
    ]
}

/// Clicks the start of "secret" laid out at the window's top left.
fn click_on(cx: &mut TestAppContext, spoilers: Spoilers) -> Vec<Click> {
    let clicks: Rc<RefCell<Vec<Click>>> = Rc::default();
    let shared = Rc::clone(&clicks);
    let text = RichText::from_entities("secret", &spoiler_link());
    let (_view, cx) = cx.add_window_view(|_, _| Clickable {
        text,
        spoilers,
        clicks: shared,
    });
    cx.run_until_parked();
    let at = point(px(2.), px(4.));
    cx.simulate_mouse_move(at, None, Modifiers::none());
    cx.simulate_click(at, Modifiers::none());
    clicks.take()
}

/// While hidden, a real click on the laid-out text only reveals.
#[gpui::test]
fn clicking_a_hidden_spoiler_link_only_reveals(cx: &mut TestAppContext) {
    assert_eq!(click_on(cx, Spoilers::Hidden), [Click::Reveal]);
}

#[gpui::test]
fn clicking_a_revealed_spoiler_link_copies(cx: &mut TestAppContext) {
    assert_eq!(
        click_on(cx, Spoilers::Revealed),
        [Click::Copy("https://secret.example".into())]
    );
}
