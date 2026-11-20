//! `RichText` runs → GPUI `TextRun`s for `StyledText::with_runs` (plan
//! section 6).
//!
//! `with_runs` panics unless the runs cover the text exactly on UTF-8
//! boundaries. Core already guarantees that for `RichText`, but this module
//! does not rely on it: the display string is built here, piece by piece, and
//! every `TextRun` gets exactly the byte length of the piece it styles. A
//! malformed partition degrades to plain text instead of reaching GPUI.
//!
//! Links are never opened (Stage 1): link text is styled, a hidden target is
//! shown inline after the text it hides behind, and a click copies the target.
//! Under a hidden spoiler a link is only spoiler: no target is shown and a
//! click only reveals.

use std::ops::Range;

use abyss_core::rich_text::{LinkTarget, RichText, Style};
use abyss_core::sanitize::clean;
use gpui::{
    AnyElement, App, ElementId, Font, FontStyle, FontWeight, Hsla, Rgba, SharedString,
    StrikethroughStyle, TextRun, UnderlineStyle, Window, prelude::*, px,
};

mod accessible;

/// Bytes of a hidden link target shown inline; the click copies all of it.
const MAX_SHOWN_TARGET: usize = 256;

#[derive(Clone, Debug)]
pub(crate) struct Palette {
    pub(crate) text: Hsla,
    pub(crate) muted: Hsla,
    pub(crate) link: Hsla,
    pub(crate) mention: Hsla,
    pub(crate) code_bg: Hsla,
    /// Foreground and background of a hidden spoiler (the same color).
    pub(crate) spoiler: Hsla,
}

#[derive(Clone, Debug)]
pub(crate) struct Fonts {
    pub(crate) body: Font,
    pub(crate) mono: Font,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Spoilers {
    Hidden,
    Revealed,
}

/// A message body ready for `StyledText::with_runs` and `InteractiveText`.
#[derive(Debug)]
pub(crate) struct Body {
    pub(crate) text: SharedString,
    pub(crate) runs: Vec<TextRun>,
    /// Clickable ranges of the display text and the link target each copies.
    pub(crate) links: Vec<(Range<usize>, SharedString)>,
    /// Display ranges of hidden spoilers (a click reveals them).
    pub(crate) spoilers: Vec<Range<usize>>,
}

/// How a run is drawn beyond its `Style`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Look {
    Text,
    Link,
    /// The target shown after a link's text.
    Target,
    HiddenSpoiler,
}

struct Builder<'a> {
    fonts: &'a Fonts,
    palette: &'a Palette,
    spoilers: Spoilers,
    out: String,
    runs: Vec<TextRun>,
    links: Vec<(Range<usize>, SharedString)>,
    hidden: Vec<Range<usize>>,
    /// The open link has text under a hidden spoiler.
    link_hidden: Hiding,
}

/// Whether a link's text is (partly) under a hidden spoiler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hiding {
    No,
    Yes,
}

impl Builder<'_> {
    /// Appends `piece` with `style`; the run length is the piece's own length.
    fn push(&mut self, piece: &str, style: Style, link: Option<u16>) {
        if piece.is_empty() {
            return;
        }
        let start = self.out.len();
        self.out.push_str(piece);
        let look = if self.spoilers == Spoilers::Hidden && style.contains(Style::SPOILER) {
            self.hidden.push(start..self.out.len());
            if link.is_some() {
                self.link_hidden = Hiding::Yes;
            }
            Look::HiddenSpoiler
        } else if link.is_some() {
            Look::Link
        } else {
            Look::Text
        };
        let run = text_run(piece.len(), style, look, self.fonts, self.palette);
        self.runs.push(run);
    }

    /// Ends a link that started at display offset `start`. A link under a
    /// hidden spoiler shows no target and copies nothing: its whole text
    /// becomes one reveal range, so a click reveals and does nothing else.
    fn close_link(&mut self, target: Option<&LinkTarget>, start: usize) {
        if std::mem::replace(&mut self.link_hidden, Hiding::No) == Hiding::Yes {
            self.hidden.retain(|r| r.start < start);
            self.hidden.push(start..self.out.len());
            return;
        }
        let url = match target {
            Some(LinkTarget::Visible(url)) => url,
            Some(LinkTarget::Hidden(url)) => {
                let full = clean(url, url.len());
                let shown = full
                    .get(..full.floor_char_boundary(MAX_SHOWN_TARGET))
                    .unwrap_or_default();
                let ellipsis = if shown.len() < full.len() { "…" } else { "" };
                let suffix = format!(" ({shown}{ellipsis})");
                self.out.push_str(&suffix);
                let run = text_run(
                    suffix.len(),
                    Style::default(),
                    Look::Target,
                    self.fonts,
                    self.palette,
                );
                self.runs.push(run);
                url
            }
            None => return,
        };
        self.links.push((start..self.out.len(), url.clone().into()));
    }

    fn finish(self) -> Body {
        Body {
            text: self.out.into(),
            runs: self.runs,
            links: self.links,
            spoilers: self.hidden,
        }
    }
}

fn text_run(len: usize, style: Style, look: Look, fonts: &Fonts, palette: &Palette) -> TextRun {
    let mono = style.contains(Style::CODE) || style.contains(Style::PRE);
    let mut font = if mono {
        fonts.mono.clone()
    } else {
        fonts.body.clone()
    };
    if style.contains(Style::BOLD) {
        font.weight = FontWeight::BOLD;
    } else if look == Look::Text && style.contains(Style::MENTION) {
        // The theme has no accent color, so weight marks a mention.
        font.weight = FontWeight::SEMIBOLD;
    }
    if style.contains(Style::ITALIC) {
        font.style = FontStyle::Italic;
    }
    let color = match look {
        Look::HiddenSpoiler => palette.spoiler,
        Look::Link => palette.link,
        Look::Target => palette.muted,
        Look::Text if style.contains(Style::MENTION) => palette.mention,
        Look::Text => palette.text,
    };
    let background_color = match look {
        Look::HiddenSpoiler => Some(palette.spoiler),
        _ if mono => Some(palette.code_bg),
        _ => None,
    };
    let line = |on: bool| on.then_some(color);
    TextRun {
        len,
        font,
        color,
        background_color,
        underline: line(look == Look::Link || style.contains(Style::UNDERLINE)).map(|c| {
            UnderlineStyle {
                thickness: px(1.),
                color: Some(c),
                wavy: false,
            }
        }),
        strikethrough: line(style.contains(Style::STRIKE)).map(|c| StrikethroughStyle {
            thickness: px(1.),
            color: Some(c),
        }),
    }
}

/// Builds the display text and runs for `rt`.
pub(crate) fn style(rt: &RichText, fonts: &Fonts, palette: &Palette, spoilers: Spoilers) -> Body {
    let mut b = Builder {
        fonts,
        palette,
        spoilers,
        out: String::with_capacity(rt.text().len()),
        runs: Vec::with_capacity(rt.runs().len() + 1),
        links: Vec::new(),
        hidden: Vec::new(),
        link_hidden: Hiding::No,
    };
    let text = rt.text();
    let mut pos = 0usize;
    // The link being emitted: (index into `rt.links()`, display start).
    let mut open: Option<(u16, usize)> = None;
    for run in rt.runs() {
        let Some(piece) = pos.checked_add(run.len).and_then(|end| text.get(pos..end)) else {
            break; // a malformed partition: the rest is appended plainly below
        };
        if let Some((ix, start)) = open.filter(|(ix, _)| run.link != Some(*ix)) {
            b.close_link(rt.links().get(usize::from(ix)), start);
            open = None;
        }
        if let (Some(ix), None) = (run.link, open) {
            open = Some((ix, b.out.len()));
        }
        b.push(piece, run.style, run.link);
        pos += piece.len();
    }
    if let Some((ix, start)) = open {
        b.close_link(rt.links().get(usize::from(ix)), start);
    }
    b.push(text.get(pos..).unwrap_or_default(), Style::default(), None);
    b.finish()
}

/// What a click on a message body asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Click {
    /// Copy a link's exact target (links never open).
    Copy(SharedString),
    /// Reveal the message's spoilers.
    Reveal,
}

impl Body {
    /// The rendered body shares activation across mouse, keyboard and `AppKit`.
    pub(crate) fn interactive(
        self,
        id: impl Into<ElementId>,
        ink: Rgba,
        activate: impl Fn(&Click, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        accessible::AccessibleText::new(id.into(), self, ink, activate).into_any_element()
    }
}

/// Plain text in the body style (echoes of own sends, service lines).
pub(crate) fn plain(text: &str, fonts: &Fonts, palette: &Palette) -> Body {
    style(&RichText::plain(text), fonts, palette, Spoilers::Revealed)
}

#[cfg(test)]
mod tests;
