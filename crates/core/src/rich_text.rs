//! Conversion of Telegram message entities (untrusted, UTF-16 offsets, possibly
//! overlapping) into a disjoint run partition that a renderer can consume without
//! further validation.
//!
//! Invariants of every [`RichText`] produced by [`RichText::from_entities`]:
//! - `text.len() <= MAX_TEXT_BYTES`;
//! - run lengths are non-zero, sum exactly to `text.len()`, and every run boundary
//!   lies on a UTF-8 char boundary;
//! - every `Run::link` indexes into `links`.
//!
//! Inputs exceeding the processing budget degrade to unformatted text, but
//! never to revealed spoilers: spoiler ranges are kept independently of every
//! other formatting, and when they cannot be established the whole text is
//! hidden as one spoiler instead (see [`RichText::from_entities`]).

/// Maximum rendered text, in bytes. Longer text is truncated at a char boundary.
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
/// Maximum number of entities processed per message.
pub const MAX_ENTITIES: usize = 256;
/// What stands for hidden spoiler text wherever a message is quoted or
/// summarised: a reply's quote, the chat list, and a screen reader. (Banners
/// carry no message text at all, D10.)
pub const SPOILER_EXCERPT: &str = "Hidden spoiler";
/// Maximum number of link entities per message.
pub const MAX_LINKS: usize = 64;
/// Maximum aggregate bytes of hidden link targets (`TextUrl`) per message.
pub const MAX_URL_BYTES: usize = 8 * 1024;

/// An entity as received from the server, before validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawEntity {
    /// Offset in UTF-16 code units.
    pub offset: i32,
    /// Length in UTF-16 code units.
    pub length: i32,
    pub kind: EntityKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntityKind {
    Bold,
    Italic,
    Underline,
    Strike,
    Code,
    Pre,
    Spoiler,
    Mention,
    /// A URL that is literally present in the text.
    Url,
    /// Visible text that points to a hidden target URL.
    TextUrl(String),
    /// Anything we do not style (hashtags, custom emoji, ...).
    Other,
}

/// Bit set of text styles.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Style(u8);

impl Style {
    pub const BOLD: Style = Style(1);
    pub const ITALIC: Style = Style(1 << 1);
    pub const UNDERLINE: Style = Style(1 << 2);
    pub const STRIKE: Style = Style(1 << 3);
    pub const CODE: Style = Style(1 << 4);
    pub const PRE: Style = Style(1 << 5);
    pub const SPOILER: Style = Style(1 << 6);
    pub const MENTION: Style = Style(1 << 7);

    pub const fn contains(self, other: Style) -> bool {
        self.0 & other.0 == other.0
    }

    const fn union(self, other: Style) -> Style {
        Style(self.0 | other.0)
    }
}

/// Where a link points. Renderers must show the target, not just the visible text.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LinkTarget {
    /// The visible text is the URL.
    Visible(String),
    /// The visible text differs from the target.
    Hidden(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Run {
    /// Length in bytes of the UTF-8 text covered by this run.
    pub len: usize,
    pub style: Style,
    /// Index into [`RichText::links`].
    pub link: Option<u16>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "RichTextParts")]
pub struct RichText {
    text: String,
    runs: Vec<Run>,
    links: Vec<LinkTarget>,
}

impl RichText {
    pub fn plain(text: &str) -> Self {
        let text = clamp_to_boundary(text, MAX_TEXT_BYTES).to_owned();
        let runs = if text.is_empty() {
            Vec::new()
        } else {
            vec![Run {
                len: text.len(),
                style: Style::default(),
                link: None,
            }]
        };
        Self {
            text,
            runs,
            links: Vec::new(),
        }
    }

    /// Validates `entities` against `text` and produces a disjoint run partition.
    /// Invalid entities are dropped; over-budget input degrades to plain text.
    ///
    /// Spoilers are the exception to both: a hidden range must never become
    /// readable because something else was dropped. They are resolved apart
    /// from the other entities, so an over-budget text, entity list or link
    /// set keeps them, and a range that does not land on character boundaries
    /// widens to the nearest ones. When the spoilers themselves cannot be
    /// established (more than [`MAX_ENTITIES`] of them, or one with a negative
    /// offset or length) the whole text is one spoiler: redacted, not shown.
    pub fn from_entities(text: &str, entities: &[RawEntity]) -> Self {
        let kept = clamp_to_boundary(text, MAX_TEXT_BYTES);
        let map = Utf16Map::new(kept);
        let Some(mut spans) = spoiler_spans(entities, &map) else {
            return Self::all_hidden(kept);
        };
        let over_budget = text.len() > MAX_TEXT_BYTES || entities.len() > MAX_ENTITIES;
        if over_budget {
            return build(kept, &spans);
        }
        let spoilers = spans.len();
        spans.extend(entities.iter().filter_map(|e| Span::resolve(e, &map)));
        if !links_within_budget(&spans) {
            spans.truncate(spoilers);
        }
        build(text, &spans)
    }

    /// The whole text as one spoiler: what a message becomes when its spoiler
    /// ranges cannot be established. Revealing it is the reader's choice.
    pub(crate) fn all_hidden(text: &str) -> Self {
        let mut rt = Self::plain(text);
        for run in &mut rt.runs {
            run.style = run.style.union(Style::SPOILER);
        }
        rt
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn runs(&self) -> &[Run] {
        &self.runs
    }

    /// The one excerpt every surface that quotes or summarises this text must
    /// use: the text with each spoiler (adjacent spoiler runs are one) replaced
    /// by [`SPOILER_EXCERPT`]. A spoiler stays hidden here even once the
    /// message itself was revealed: an excerpt has nothing to click.
    pub fn excerpt(&self) -> std::borrow::Cow<'_, str> {
        if !self.runs.iter().any(|r| r.style.contains(Style::SPOILER)) {
            return std::borrow::Cow::Borrowed(&self.text);
        }
        let mut out = String::with_capacity(self.text.len());
        let mut pos = 0usize;
        let mut hiding = false;
        for run in &self.runs {
            let end = pos.saturating_add(run.len);
            if run.style.contains(Style::SPOILER) {
                if !hiding {
                    out.push_str(SPOILER_EXCERPT);
                }
                hiding = true;
            } else {
                hiding = false;
                out.push_str(self.text.get(pos..end).unwrap_or_default());
            }
            pos = end;
        }
        std::borrow::Cow::Owned(out)
    }

    pub fn links(&self) -> &[LinkTarget] {
        &self.links
    }

    /// Approximate heap bytes retained, for byte budgets.
    pub fn retained_bytes(&self) -> usize {
        // Capacities, not lengths: this is what the allocator actually holds.
        let link_strings: usize = self
            .links
            .iter()
            .map(|l| match l {
                LinkTarget::Visible(s) | LinkTarget::Hidden(s) => s.capacity(),
            })
            .sum();
        self.text.capacity()
            + self.runs.capacity() * std::mem::size_of::<Run>()
            + self.links.capacity() * std::mem::size_of::<LinkTarget>()
            + link_strings
    }
}

/// Unvalidated wire form, used only to deserialize (e.g. from the disk cache).
/// Conversion re-checks every [`RichText`] invariant, so stored data can never
/// produce runs that would panic a renderer.
#[derive(serde::Deserialize)]
struct RichTextParts {
    text: String,
    runs: Vec<Run>,
    links: Vec<LinkTarget>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct InvalidRichText;

impl std::fmt::Display for InvalidRichText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("rich text violates its invariants")
    }
}

impl TryFrom<RichTextParts> for RichText {
    type Error = InvalidRichText;

    fn try_from(p: RichTextParts) -> Result<Self, InvalidRichText> {
        let url_bytes = p.links.iter().fold(0usize, |acc, l| match l {
            LinkTarget::Visible(s) | LinkTarget::Hidden(s) => acc.saturating_add(s.len()),
        });
        let bounded = p.text.len() <= MAX_TEXT_BYTES
            && p.runs.len() <= MAX_ENTITIES * 2 + 1
            && p.links.len() <= MAX_LINKS
            && url_bytes <= MAX_URL_BYTES;
        if !bounded {
            return Err(InvalidRichText);
        }
        let mut pos = 0usize;
        for run in &p.runs {
            pos = pos.checked_add(run.len).ok_or(InvalidRichText)?;
            let link_ok = run.link.is_none_or(|l| usize::from(l) < p.links.len());
            if run.len == 0 || !p.text.is_char_boundary(pos) || !link_ok {
                return Err(InvalidRichText);
            }
        }
        if pos != p.text.len() {
            return Err(InvalidRichText);
        }
        Ok(RichText {
            text: p.text,
            runs: p.runs,
            links: p.links,
        })
    }
}

/// Checks link count and the aggregate bytes of every link target that would be
/// retained, visible (copied from the text) and hidden alike, before any allocation.
fn links_within_budget(spans: &[Span<'_>]) -> bool {
    let mut links = 0usize;
    let mut url_bytes = 0usize;
    for s in spans {
        let bytes = match &s.link {
            None => continue,
            Some(SpanLink::Visible) => s.end - s.start,
            Some(SpanLink::Hidden(url)) => url.len(),
        };
        links += 1;
        url_bytes = url_bytes.saturating_add(bytes);
    }
    links <= MAX_LINKS && url_bytes <= MAX_URL_BYTES
}

/// Longest prefix of `s` that is at most `max` bytes and ends on a char boundary.
fn clamp_to_boundary(s: &str, max: usize) -> &str {
    s.get(..s.floor_char_boundary(max)).unwrap_or_default()
}

/// Maps UTF-16 code-unit offsets to UTF-8 byte offsets. Offsets that fall inside a
/// surrogate pair map to `None`.
struct Utf16Map {
    byte_at: Vec<Option<usize>>,
}

impl Utf16Map {
    fn new(text: &str) -> Self {
        let mut byte_at = Vec::with_capacity(text.len() + 1);
        for (byte, ch) in text.char_indices() {
            byte_at.push(Some(byte));
            if ch.len_utf16() == 2 {
                byte_at.push(None);
            }
        }
        byte_at.push(Some(text.len()));
        Self { byte_at }
    }

    fn byte(&self, utf16: usize) -> Option<usize> {
        self.byte_at.get(utf16).copied().flatten()
    }

    /// The text's length in UTF-16 code units.
    fn len16(&self) -> usize {
        self.byte_at.len().saturating_sub(1)
    }

    /// The byte offset at `utf16`, or at the character boundary before it when
    /// it falls inside a surrogate pair.
    fn byte_floor(&self, utf16: usize) -> Option<usize> {
        self.byte(utf16)
            .or_else(|| self.byte(utf16.checked_sub(1)?))
    }

    /// The byte offset at `utf16`, or at the character boundary after it when
    /// it falls inside a surrogate pair.
    fn byte_ceil(&self, utf16: usize) -> Option<usize> {
        self.byte(utf16)
            .or_else(|| self.byte(utf16.checked_add(1)?))
    }
}

/// A validated entity in byte coordinates.
struct Span<'a> {
    start: usize,
    end: usize,
    style: Style,
    link: Option<SpanLink<'a>>,
}

enum SpanLink<'a> {
    Visible,
    Hidden(&'a str),
}

impl<'a> Span<'a> {
    fn resolve(e: &'a RawEntity, map: &Utf16Map) -> Option<Span<'a>> {
        let start16 = usize::try_from(e.offset).ok()?;
        let len16 = usize::try_from(e.length).ok()?;
        let end16 = start16.checked_add(len16)?;
        let start = map.byte(start16)?;
        let end = map.byte(end16)?;
        if start >= end {
            return None;
        }
        let (style, link) = match &e.kind {
            EntityKind::Bold => (Style::BOLD, None),
            EntityKind::Italic => (Style::ITALIC, None),
            EntityKind::Underline => (Style::UNDERLINE, None),
            EntityKind::Strike => (Style::STRIKE, None),
            EntityKind::Code => (Style::CODE, None),
            EntityKind::Pre => (Style::PRE, None),
            EntityKind::Mention => (Style::MENTION, None),
            EntityKind::Url => (Style::default(), Some(SpanLink::Visible)),
            EntityKind::TextUrl(url) => (Style::default(), Some(SpanLink::Hidden(url))),
            // Spoilers are resolved apart, by `spoiler_spans`, so that no
            // budget can drop one.
            EntityKind::Spoiler | EntityKind::Other => return None,
        };
        Some(Span {
            start,
            end,
            style,
            link,
        })
    }
}

/// Every spoiler entity as a byte span, widened outward to character
/// boundaries and clamped to the text. `None` when they cannot be established:
/// more than [`MAX_ENTITIES`] of them, or one with a negative offset or
/// length. A spoiler that is empty or starts past the text hides nothing and is
/// skipped. The scan is linear in `entities` and keeps at most `MAX_ENTITIES`.
fn spoiler_spans<'a>(entities: &[RawEntity], map: &Utf16Map) -> Option<Vec<Span<'a>>> {
    let mut spans = Vec::new();
    for e in entities.iter().filter(|e| e.kind == EntityKind::Spoiler) {
        let start16 = usize::try_from(e.offset).ok()?;
        let len16 = usize::try_from(e.length).ok()?;
        if len16 == 0 || start16 >= map.len16() {
            continue;
        }
        if spans.len() >= MAX_ENTITIES {
            return None;
        }
        let end16 = start16.saturating_add(len16).min(map.len16());
        let start = map.byte_floor(start16)?;
        let end = map.byte_ceil(end16)?;
        spans.push(Span {
            start,
            end,
            style: Style::SPOILER,
            link: None,
        });
    }
    Some(spans)
}

/// Builds the run partition. `spans` is bounded by `MAX_ENTITIES`, so the
/// intervals x spans scan is bounded by roughly 513 * 256 steps.
fn build(text: &str, spans: &[Span<'_>]) -> RichText {
    let mut cuts: Vec<usize> = Vec::with_capacity(spans.len() * 2 + 2);
    cuts.push(0);
    cuts.push(text.len());
    for s in spans {
        cuts.push(s.start);
        cuts.push(s.end);
    }
    cuts.sort_unstable();
    cuts.dedup();

    let mut links: Vec<LinkTarget> = Vec::new();
    let mut link_of_span: Vec<Option<u16>> = vec![None; spans.len()];
    let mut runs: Vec<Run> = Vec::with_capacity(cuts.len());
    for pair in cuts.windows(2) {
        let &[a, b] = pair else { continue };
        if a == b {
            continue;
        }
        let mut style = Style::default();
        let mut link = None;
        // Innermost link wins: the shortest covering link span.
        let mut link_len = usize::MAX;
        for (i, s) in spans.iter().enumerate() {
            if s.start <= a && b <= s.end {
                style = style.union(s.style);
                if s.link.is_some() && s.end - s.start < link_len {
                    link_len = s.end - s.start;
                    link = intern_link(text, s, i, &mut link_of_span, &mut links);
                }
            }
        }
        push_run(
            &mut runs,
            Run {
                len: b - a,
                style,
                link,
            },
        );
    }
    // Runs were reserved per cut and then coalesced; release the slack so the
    // retained allocation matches what byte accounting charges.
    runs.shrink_to_fit();
    links.shrink_to_fit();
    RichText {
        text: text.to_owned(),
        runs,
        links,
    }
}

fn intern_link(
    text: &str,
    span: &Span<'_>,
    index: usize,
    link_of_span: &mut [Option<u16>],
    links: &mut Vec<LinkTarget>,
) -> Option<u16> {
    let slot = link_of_span.get_mut(index)?;
    if slot.is_none() {
        let target = match span.link.as_ref()? {
            SpanLink::Visible => LinkTarget::Visible(text.get(span.start..span.end)?.to_owned()),
            SpanLink::Hidden(url) => LinkTarget::Hidden((*url).to_owned()),
        };
        // links.len() <= MAX_LINKS (64), so this conversion cannot fail.
        *slot = Some(u16::try_from(links.len()).ok()?);
        links.push(target);
    }
    *slot
}

fn push_run(runs: &mut Vec<Run>, run: Run) {
    if let Some(last) = runs.last_mut()
        && last.style == run.style
        && last.link == run.link
    {
        last.len += run.len;
        return;
    }
    runs.push(run);
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn ent(offset: i32, length: i32, kind: EntityKind) -> RawEntity {
        RawEntity {
            offset,
            length,
            kind,
        }
    }

    fn assert_invariants(rt: &RichText) {
        assert!(rt.text.len() <= MAX_TEXT_BYTES);
        let mut pos = 0;
        for run in &rt.runs {
            assert!(run.len > 0, "empty run");
            pos += run.len;
            assert!(rt.text.is_char_boundary(pos), "run split a char");
            if let Some(l) = run.link {
                assert!(usize::from(l) < rt.links.len());
            }
        }
        assert_eq!(pos, rt.text.len(), "runs must cover text exactly");
    }

    #[test]
    fn overlapping_entities_compose() {
        // "hello world": bold 0..7, italic 4..11
        let rt = RichText::from_entities(
            "hello world",
            &[ent(0, 7, EntityKind::Bold), ent(4, 7, EntityKind::Italic)],
        );
        assert_invariants(&rt);
        let styles: Vec<(usize, Style)> = rt.runs.iter().map(|r| (r.len, r.style)).collect();
        assert_eq!(
            styles,
            vec![
                (4, Style::BOLD),
                (3, Style::BOLD.union(Style::ITALIC)),
                (4, Style::ITALIC),
            ]
        );
    }

    #[test]
    fn utf16_offsets_with_astral_chars() {
        // U+1F600 is 2 UTF-16 units and 4 UTF-8 bytes.
        let text = "a\u{1F600}b";
        let rt = RichText::from_entities(text, &[ent(1, 2, EntityKind::Bold)]);
        assert_invariants(&rt);
        assert_eq!(
            rt.runs[1],
            Run {
                len: 4,
                style: Style::BOLD,
                link: None
            }
        );
    }

    #[test]
    fn entity_splitting_surrogate_pair_is_dropped() {
        let rt = RichText::from_entities("a\u{1F600}b", &[ent(2, 1, EntityKind::Bold)]);
        assert_invariants(&rt);
        assert_eq!(rt.runs.len(), 1);
        assert_eq!(rt.runs[0].style, Style::default());
    }

    #[test]
    fn negative_and_overflowing_entities_are_dropped() {
        let rt = RichText::from_entities(
            "abc",
            &[
                ent(-1, 2, EntityKind::Bold),
                ent(1, -1, EntityKind::Bold),
                ent(i32::MAX, i32::MAX, EntityKind::Bold),
                ent(0, 4, EntityKind::Bold),
            ],
        );
        assert_invariants(&rt);
        assert_eq!(
            rt.runs,
            vec![Run {
                len: 3,
                style: Style::default(),
                link: None
            }]
        );
    }

    #[test]
    fn hidden_link_target_is_exposed() {
        let rt = RichText::from_entities(
            "click here",
            &[ent(
                6,
                4,
                EntityKind::TextUrl("https://evil.example".into()),
            )],
        );
        assert_invariants(&rt);
        assert_eq!(
            rt.links,
            vec![LinkTarget::Hidden("https://evil.example".into())]
        );
        assert_eq!(rt.runs[1].link, Some(0));
    }

    #[test]
    fn innermost_link_wins() {
        let rt = RichText::from_entities(
            "abcdef",
            &[
                ent(0, 6, EntityKind::TextUrl("https://outer.example".into())),
                ent(2, 2, EntityKind::TextUrl("https://inner.example".into())),
            ],
        );
        assert_invariants(&rt);
        let inner = rt.runs[1].link.map(usize::from);
        assert_eq!(
            inner.and_then(|i| rt.links.get(i)),
            Some(&LinkTarget::Hidden("https://inner.example".into()))
        );
    }

    #[test]
    fn over_budget_degrades_to_plain() {
        let many: Vec<RawEntity> = (0..=i32::try_from(MAX_ENTITIES).unwrap())
            .map(|i| ent(i, 1, EntityKind::Bold))
            .collect();
        let text = "x".repeat(MAX_ENTITIES + 1);
        let rt = RichText::from_entities(&text, &many);
        assert_invariants(&rt);
        assert_eq!(rt.runs.len(), 1);

        let huge = "h".repeat(MAX_URL_BYTES + 1);
        let rt = RichText::from_entities("abc", &[ent(0, 1, EntityKind::TextUrl(huge))]);
        assert!(rt.links.is_empty());
    }

    #[test]
    fn long_text_is_clamped_on_boundary_and_unformatted() {
        // 2 bytes each, odd byte budget forces a boundary walk-back.
        let text = "\u{00E9}".repeat(MAX_TEXT_BYTES);
        let rt = RichText::from_entities(&text, &[ent(0, 4, EntityKind::Bold)]);
        assert_invariants(&rt);
        assert_eq!(rt.text.len(), MAX_TEXT_BYTES);
        assert_eq!(
            rt.runs,
            vec![Run {
                len: MAX_TEXT_BYTES,
                style: Style::default(),
                link: None
            }]
        );

        let odd = format!("a{text}");
        let rt = RichText::plain(&odd);
        assert_invariants(&rt);
        assert_eq!(rt.text.len(), MAX_TEXT_BYTES - 1);
    }

    #[test]
    fn budget_limits_are_exact() {
        let at_limit: Vec<RawEntity> = (0..i32::try_from(MAX_ENTITIES).unwrap())
            .map(|i| ent(i, 1, EntityKind::Bold))
            .collect();
        let text = "x".repeat(MAX_ENTITIES);
        let rt = RichText::from_entities(&text, &at_limit);
        assert_eq!(
            rt.runs[0].style,
            Style::BOLD,
            "at the limit, formatting is kept"
        );

        // Visible URL targets count toward the aggregate URL budget (review r3 #7).
        let long = "u".repeat(MAX_URL_BYTES + 1);
        let len = i32::try_from(long.len()).unwrap();
        let rt = RichText::from_entities(&long, &[ent(0, len, EntityKind::Url)]);
        assert!(
            rt.links.is_empty(),
            "visible URL over byte budget degrades to plain"
        );
        let rt = RichText::from_entities(&long, &[ent(0, len - 1, EntityKind::Url)]);
        assert_eq!(rt.links.len(), 1, "visible URL at byte budget is kept");

        let links: Vec<RawEntity> = (0..=i32::try_from(MAX_LINKS).unwrap())
            .map(|i| ent(i, 1, EntityKind::Url))
            .collect();
        let rt = RichText::from_entities(&text, &links);
        assert!(
            rt.links.is_empty(),
            "one link over the limit degrades to plain"
        );
    }

    fn arb_kind() -> impl Strategy<Value = EntityKind> {
        prop_oneof![
            Just(EntityKind::Bold),
            Just(EntityKind::Italic),
            Just(EntityKind::Code),
            Just(EntityKind::Spoiler),
            Just(EntityKind::Url),
            Just(EntityKind::Other),
            "[a-z:/.]{0,20}".prop_map(EntityKind::TextUrl),
        ]
    }

    proptest! {
        #[test]
        fn never_panics_and_partitions_exactly(
            text in "\\PC{0,64}",
            raw in prop::collection::vec((-4i32..80, -4i32..80, arb_kind()), 0..24),
        ) {
            let entities: Vec<RawEntity> =
                raw.into_iter().map(|(o, l, k)| ent(o, l, k)).collect();
            let rt = RichText::from_entities(&text, &entities);
            assert_invariants(&rt);
            prop_assert_eq!(rt.text(), text.as_str());
        }
    }
}

#[cfg(test)]
mod serde_tests {
    use super::*;

    fn roundtrip(rt: &RichText) -> Result<RichText, postcard::Error> {
        let bytes = postcard::to_allocvec(rt)?;
        postcard::from_bytes(&bytes)
    }

    #[test]
    fn valid_text_roundtrips() {
        let rt = RichText::from_entities(
            "a\u{1F600} link",
            &[RawEntity {
                offset: 4,
                length: 4,
                kind: EntityKind::TextUrl("https://x.example".into()),
            }],
        );
        assert_eq!(roundtrip(&rt).unwrap(), rt);
    }

    fn run(len: usize, link: Option<u16>) -> Run {
        Run {
            len,
            style: Style::default(),
            link,
        }
    }

    #[test]
    fn tampered_runs_are_rejected() {
        let good = RichText::from_entities(
            "a\u{1F600}b",
            &[RawEntity {
                offset: 1,
                length: 2,
                kind: EntityKind::Bold,
            }],
        );
        let cases = [
            vec![run(2, None), run(4, None)],          // splits the 4-byte emoji
            vec![run(1, None)],                        // does not cover the text
            vec![run(6, Some(0))],                     // dangling link index
            vec![run(0, None), run(6, None)],          // zero-length run
            vec![run(usize::MAX, None), run(7, None)], // overflowing lengths
        ];
        for runs in cases {
            let bad = RichText {
                runs,
                ..good.clone()
            };
            assert!(roundtrip(&bad).is_err(), "accepted {bad:?}");
        }
    }
}

#[cfg(test)]
mod accounting_tests {
    use super::*;

    #[test]
    fn coalesced_runs_release_reserved_capacity() {
        let text = "x".repeat(512);
        let entities: Vec<RawEntity> = (0..i32::try_from(MAX_ENTITIES).unwrap())
            .map(|i| RawEntity {
                offset: i,
                length: 512 - i,
                kind: EntityKind::Bold,
            })
            .collect();
        let rt = RichText::from_entities(&text, &entities);
        assert_eq!(rt.runs.capacity(), rt.runs.len());
        assert_eq!(rt.links.capacity(), rt.links.len());
        assert!(
            rt.retained_bytes()
                >= rt.text.capacity() + rt.runs.capacity() * std::mem::size_of::<Run>()
        );
    }
}

#[cfg(test)]
mod excerpt_tests {
    use super::*;

    const SENTINEL: &str = "hunter2-sentinel";

    fn spoiler(text: &str, from: &str, kind: &[EntityKind]) -> RichText {
        let offset = text.find(from).unwrap();
        let entities: Vec<RawEntity> = kind
            .iter()
            .map(|k| RawEntity {
                offset: i32::try_from(text[..offset].encode_utf16().count()).unwrap(),
                length: i32::try_from(from.encode_utf16().count()).unwrap(),
                kind: k.clone(),
            })
            .collect();
        RichText::from_entities(text, &entities)
    }

    #[test]
    fn plain_text_is_borrowed_unchanged() {
        let rt = RichText::plain("hello");
        assert!(matches!(rt.excerpt(), std::borrow::Cow::Borrowed("hello")));
    }

    #[test]
    fn a_spoiler_is_replaced_and_its_text_never_appears() {
        let text = format!("the code is {SENTINEL} ok");
        let rt = spoiler(&text, SENTINEL, &[EntityKind::Spoiler]);
        assert_eq!(rt.excerpt(), format!("the code is {SPOILER_EXCERPT} ok"));
    }

    #[test]
    fn overlapping_styles_and_links_under_a_spoiler_are_one_placeholder() {
        let text = format!("x {SENTINEL} y");
        let rt = spoiler(
            &text,
            SENTINEL,
            &[
                EntityKind::Spoiler,
                EntityKind::Bold,
                EntityKind::TextUrl("https://example.test/".into()),
            ],
        );
        let excerpt = rt.excerpt();
        assert_eq!(excerpt, format!("x {SPOILER_EXCERPT} y"));
        assert!(!excerpt.contains(SENTINEL));
    }

    #[test]
    fn a_wholly_spoilered_text_is_only_the_placeholder() {
        let rt = spoiler(SENTINEL, SENTINEL, &[EntityKind::Spoiler]);
        assert_eq!(rt.excerpt(), SPOILER_EXCERPT);
    }

    fn raw(offset: i32, length: i32, kind: EntityKind) -> RawEntity {
        RawEntity {
            offset,
            length,
            kind,
        }
    }

    /// The text `SENTINEL` hidden in `prefix SENTINEL suffix`, plus `extra`.
    fn hidden_among(extra: &[RawEntity]) -> (String, Vec<RawEntity>) {
        let text = format!("pre {SENTINEL} post{}", " x".repeat(MAX_ENTITIES + 2));
        let start = i32::try_from("pre ".len()).unwrap();
        let len = i32::try_from(SENTINEL.len()).unwrap();
        let mut entities = vec![raw(start, len, EntityKind::Spoiler)];
        entities.extend_from_slice(extra);
        (text, entities)
    }

    fn assert_hidden(rt: &RichText) {
        let excerpt = rt.excerpt();
        assert!(!excerpt.contains(SENTINEL), "revealed: {excerpt}");
        assert!(excerpt.contains(SPOILER_EXCERPT));
    }

    #[test]
    fn a_spoiler_survives_too_many_entities() {
        let bold: Vec<RawEntity> = (0..=MAX_ENTITIES)
            .map(|i| raw(i32::try_from(i + 40).unwrap(), 1, EntityKind::Bold))
            .collect();
        let (text, entities) = hidden_among(&bold);
        assert!(entities.len() > MAX_ENTITIES);
        let rt = RichText::from_entities(&text, &entities);
        assert_hidden(&rt);
        assert!(
            rt.excerpt()
                .starts_with(&format!("pre {SPOILER_EXCERPT} post"))
        );
    }

    #[test]
    fn a_spoiler_survives_too_many_links_and_an_oversized_link_target() {
        let urls: Vec<RawEntity> = (0..=MAX_LINKS)
            .map(|i| raw(i32::try_from(i + 40).unwrap(), 1, EntityKind::Url))
            .collect();
        let (text, entities) = hidden_among(&urls);
        let rt = RichText::from_entities(&text, &entities);
        assert!(rt.links().is_empty(), "the links are over budget");
        assert_hidden(&rt);

        let huge = EntityKind::TextUrl("h".repeat(MAX_URL_BYTES + 1));
        let (text, entities) = hidden_among(&[raw(40, 2, huge)]);
        let rt = RichText::from_entities(&text, &entities);
        assert!(rt.links().is_empty());
        assert_hidden(&rt);
    }

    #[test]
    fn a_spoiler_survives_an_oversized_text_and_is_clamped_at_the_cut() {
        let (text, entities) = hidden_among(&[]);
        let long = format!("{text}{}", "y".repeat(MAX_TEXT_BYTES));
        let rt = RichText::from_entities(&long, &entities);
        assert_eq!(rt.text().len(), MAX_TEXT_BYTES);
        assert_hidden(&rt);

        // A spoiler that starts before the cut and ends after it hides to the cut.
        let secret = format!("{}{SENTINEL}", "a".repeat(MAX_TEXT_BYTES - 4));
        let len = i32::try_from(secret.len()).unwrap();
        let rt = RichText::from_entities(&secret, &[raw(0, len, EntityKind::Spoiler)]);
        assert_eq!(rt.excerpt(), SPOILER_EXCERPT);
    }

    #[test]
    fn spoilers_that_cannot_be_established_hide_the_whole_text() {
        let many: Vec<RawEntity> = (0..=MAX_ENTITIES)
            .map(|i| raw(i32::try_from(i).unwrap(), 1, EntityKind::Spoiler))
            .collect();
        let text = format!("{SENTINEL}{}", "z".repeat(MAX_ENTITIES));
        for entities in [
            many,
            vec![raw(-1, 3, EntityKind::Spoiler)],
            vec![raw(2, -1, EntityKind::Spoiler)],
        ] {
            let rt = RichText::from_entities(&text, &entities);
            assert_eq!(rt.text(), text);
            assert_eq!(rt.excerpt(), SPOILER_EXCERPT, "{entities:?}");
            assert!(rt.runs().iter().all(|r| r.style.contains(Style::SPOILER)));
        }
    }

    #[test]
    fn a_spoiler_inside_a_surrogate_pair_widens_to_the_whole_character() {
        // "a😀b": the pair is units 1..3. A spoiler over unit 2 only must still
        // hide the emoji, and one ending at unit 2 must too.
        for (offset, length) in [(2, 1), (0, 2), (2, 2)] {
            let rt =
                RichText::from_entities("a\u{1F600}b", &[raw(offset, length, EntityKind::Spoiler)]);
            assert!(!rt.excerpt().contains('\u{1F600}'), "{offset}+{length}");
        }
        // An empty spoiler, or one past the end, hides nothing and changes nothing.
        let rt = RichText::from_entities(
            "abc",
            &[
                raw(1, 0, EntityKind::Spoiler),
                raw(9, 2, EntityKind::Spoiler),
            ],
        );
        assert_eq!(rt.excerpt(), "abc");
    }
}
