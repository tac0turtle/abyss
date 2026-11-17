//! The editing model behind [`super::TextInput`], free of GPUI so it can be
//! unit-tested directly.
//!
//! Content, selection and the IME marked range are kept in UTF-8 byte offsets,
//! always on char boundaries. The platform input APIs speak UTF-16, so every
//! entry point that takes a platform range converts it here.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

/// The composer's length cap, in UTF-16 units (Telegram's message limit).
pub(crate) const COMPOSER_MAX_UTF16: usize = 4096;

/// Which characters a field accepts. Rejected characters are dropped silently,
/// so pasting "+1 (555) 010-0000" into a phone field yields "+15550100000".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CharFilter {
    Any,
    /// ASCII digits and `+`.
    Phone,
    /// ASCII digits.
    Digits,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lines {
    /// Line breaks become spaces.
    Single,
    /// Line breaks are kept (normalized to `\n`).
    Multi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) filter: CharFilter,
    pub(crate) lines: Lines,
    pub(crate) max_utf16: usize,
}

impl Limits {
    pub(crate) const PHONE: Limits = Limits {
        filter: CharFilter::Phone,
        lines: Lines::Single,
        max_utf16: 20,
    };
    /// A login code: up to core's bound for any delivery; the code screen
    /// narrows it to the delivery's (`EditBuffer::set_max_utf16`).
    pub(crate) const CODE: Limits = Limits {
        filter: CharFilter::Digits,
        lines: Lines::Single,
        max_utf16: abyss_core::api::MAX_CODE_DIGITS,
    };
    /// `api_id`: at most 10 digits (a positive 32-bit number).
    pub(crate) const API_ID: Limits = Limits {
        filter: CharFilter::Digits,
        lines: Lines::Single,
        max_utf16: 10,
    };

    pub(crate) const COMPOSER: Limits = Limits {
        filter: CharFilter::Any,
        lines: Lines::Multi,
        max_utf16: COMPOSER_MAX_UTF16,
    };
}

/// Whether an edit inserted all of its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fit {
    Whole,
    /// Text was dropped by the length cap.
    Clipped,
}

/// UTF-16 offset to UTF-8 offset. An offset inside a surrogate pair rounds up
/// to the end of that char; offsets past the end clamp to the end.
pub(crate) fn utf16_to_utf8(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (i, ch) in text.char_indices() {
        if units >= utf16 {
            return i;
        }
        units += ch.len_utf16();
    }
    text.len()
}

/// UTF-8 offset to UTF-16 offset. Offsets past the end clamp to the end.
pub(crate) fn utf8_to_utf16(text: &str, utf8: usize) -> usize {
    let mut units = 0;
    for (i, ch) in text.char_indices() {
        if i >= utf8 {
            break;
        }
        units += ch.len_utf16();
    }
    units
}

pub(crate) fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// The content (a phone number, a code, a message draft) never reaches
/// `Debug` output (task 6c).
pub(crate) struct EditBuffer {
    content: String,
    selected: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    limits: Limits,
    /// Bumps on every content change, so layouts from an older revision are
    /// never used to map offsets.
    revision: u64,
}

impl std::fmt::Debug for EditBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditBuffer")
            .field("len", &self.content.len())
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl EditBuffer {
    pub(crate) fn new(limits: Limits) -> Self {
        Self {
            content: String::new(),
            selected: 0..0,
            reversed: false,
            marked: None,
            limits,
            revision: 0,
        }
    }

    pub(crate) fn text(&self) -> &str {
        &self.content
    }

    pub(crate) fn limits(&self) -> Limits {
        self.limits
    }

    pub(crate) fn selected(&self) -> Range<usize> {
        self.selected.clone()
    }

    pub(crate) fn reversed(&self) -> bool {
        self.reversed
    }

    pub(crate) fn marked(&self) -> Option<Range<usize>> {
        self.marked.clone()
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn utf16_len(&self) -> usize {
        utf16_len(&self.content)
    }

    pub(crate) fn cursor(&self) -> usize {
        if self.reversed {
            self.selected.start
        } else {
            self.selected.end
        }
    }

    pub(crate) fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        utf8_to_utf16(&self.content, range.start)..utf8_to_utf16(&self.content, range.end)
    }

    /// Converts a platform range, ordering and clamping it.
    pub(crate) fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        let a = utf16_to_utf8(&self.content, range.start);
        let b = utf16_to_utf8(&self.content, range.end);
        a.min(b)..a.max(b)
    }

    /// Clamps to the content and moves down to a char boundary, so offsets
    /// from a stale layout can never split a char.
    pub(crate) fn snap(&self, offset: usize) -> usize {
        let mut at = offset.min(self.content.len());
        while !self.content.is_char_boundary(at) {
            at = at.saturating_sub(1);
        }
        at
    }

    pub(crate) fn move_to(&mut self, offset: usize) {
        let at = self.snap(offset);
        self.selected = at..at;
        self.reversed = false;
    }

    pub(crate) fn select_to(&mut self, offset: usize) {
        let at = self.snap(offset);
        if self.reversed {
            self.selected.start = at;
        } else {
            self.selected.end = at;
        }
        if self.selected.end < self.selected.start {
            self.reversed = !self.reversed;
            self.selected = self.selected.end..self.selected.start;
        }
    }

    pub(crate) fn select_all(&mut self) {
        self.selected = 0..self.content.len();
        self.reversed = false;
    }

    pub(crate) fn set_selected_utf16(&mut self, range: &Range<usize>) {
        self.selected = self.range_from_utf16(range);
        self.reversed = false;
    }

    pub(crate) fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    pub(crate) fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    /// The text in a platform range and the range actually returned.
    pub(crate) fn text_for_utf16(&self, range: &Range<usize>) -> (String, Range<usize>) {
        let bytes = self.range_from_utf16(range);
        let text = self.content.get(bytes.clone()).unwrap_or_default();
        (text.to_owned(), self.range_to_utf16(&bytes))
    }

    /// The text of the current selection.
    pub(crate) fn selected_text(&self) -> &str {
        self.content.get(self.selected.clone()).unwrap_or_default()
    }

    /// Replaces `range` (or the marked text, or the selection) with `text`,
    /// filtered and clipped to the field's limits, and ends any composition.
    pub(crate) fn replace(&mut self, range_utf16: Option<&Range<usize>>, text: &str) -> Fit {
        let range = self.target_range(range_utf16);
        let (admitted, fit) = self.admit(text, &range);
        self.content.replace_range(range.clone(), &admitted);
        let end = range.start + admitted.len();
        self.selected = end..end;
        self.reversed = false;
        self.marked = None;
        self.revision = self.revision.wrapping_add(1);
        fit
    }

    /// Like [`Self::replace`] but marks the inserted text as an in-progress
    /// composition (IME or dead key). `new_selected_utf16` is relative to the
    /// inserted text.
    pub(crate) fn replace_and_mark(
        &mut self,
        range_utf16: Option<&Range<usize>>,
        text: &str,
        new_selected_utf16: Option<&Range<usize>>,
    ) -> Fit {
        let range = self.target_range(range_utf16);
        let (admitted, fit) = self.admit(text, &range);
        self.content.replace_range(range.clone(), &admitted);
        let end = range.start + admitted.len();
        self.marked = (!admitted.is_empty()).then_some(range.start..end);
        let start_utf16 = utf8_to_utf16(&self.content, range.start);
        let inserted_utf16 = utf16_len(&admitted);
        self.selected = match new_selected_utf16 {
            Some(sel) => self.range_from_utf16(
                &(start_utf16 + sel.start.min(inserted_utf16)
                    ..start_utf16 + sel.end.min(inserted_utf16)),
            ),
            None => end..end,
        };
        self.reversed = false;
        self.revision = self.revision.wrapping_add(1);
        fit
    }

    pub(crate) fn unmark(&mut self) {
        self.marked = None;
    }

    /// Takes the content, leaving the field empty.
    /// Changes the length bound (a code field follows the delivery). Text
    /// over the new bound is cleared rather than cut.
    pub(crate) fn set_max_utf16(&mut self, max: usize) {
        self.limits.max_utf16 = max;
        if self.utf16_len() > max {
            drop(self.take());
        }
    }

    pub(crate) fn take(&mut self) -> String {
        self.selected = 0..0;
        self.reversed = false;
        self.marked = None;
        self.revision = self.revision.wrapping_add(1);
        std::mem::take(&mut self.content)
    }

    fn target_range(&self, range_utf16: Option<&Range<usize>>) -> Range<usize> {
        let range = range_utf16
            .map(|r| self.range_from_utf16(r))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selected.clone());
        let (a, b) = (self.snap(range.start), self.snap(range.end));
        a.min(b)..a.max(b)
    }

    /// Filters `text` and clips it to the room left once `replaced` is removed.
    fn admit(&self, text: &str, replaced: &Range<usize>) -> (String, Fit) {
        let removed = self.content.get(replaced.clone()).map_or(0, utf16_len);
        let room = self
            .limits
            .max_utf16
            .saturating_sub(self.utf16_len().saturating_sub(removed));
        let mut cleaned = clean(text, self.limits);
        let keep = fit_graphemes(&cleaned, room);
        if keep < cleaned.len() {
            cleaned.truncate(keep);
            return (cleaned, Fit::Clipped);
        }
        (cleaned, Fit::Whole)
    }
}

/// Applies the field's character filter and line policy.
fn clean(text: &str, limits: Limits) -> String {
    let line_break = match limits.lines {
        Lines::Single => ' ',
        Lines::Multi => '\n',
    };
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let keep = match limits.filter {
            CharFilter::Phone => (ch.is_ascii_digit() || ch == '+').then_some(ch),
            CharFilter::Digits => ch.is_ascii_digit().then_some(ch),
            CharFilter::Any => match ch {
                '\r' => {
                    chars.next_if_eq(&'\n');
                    Some(line_break)
                }
                '\n' => Some(line_break),
                '\t' => Some(' '),
                c if c.is_control() => None,
                c => Some(c),
            },
        };
        out.extend(keep);
    }
    out
}

/// The longest prefix of whole grapheme clusters within `room` UTF-16 units,
/// in bytes. Clipping never splits a cluster (an emoji ZWJ sequence stays whole
/// or is dropped whole).
fn fit_graphemes(text: &str, room: usize) -> usize {
    let mut used = 0;
    for (i, g) in text.grapheme_indices(true) {
        let n = utf16_len(g);
        if used + n > room {
            return i;
        }
        used += n;
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";

    fn composer(text: &str) -> EditBuffer {
        let mut b = EditBuffer::new(Limits::COMPOSER);
        b.replace(None, text);
        b
    }

    /// Task 6c: no field's content (phone, code, draft) reaches `Debug`.
    #[test]
    fn debug_never_prints_the_content() {
        for (limits, text) in [
            (Limits::PHONE, "+15550100"),
            (Limits::CODE, "24680"),
            (Limits::API_ID, "97531"),
            (Limits::COMPOSER, "secret plans"),
        ] {
            let mut b = EditBuffer::new(limits);
            b.replace(None, text);
            assert_eq!(b.text(), text);
            let shown = format!("{b:?}");
            assert!(!shown.contains(text), "{shown}");
        }
    }

    #[test]
    fn utf16_mapping_non_ascii_emoji_zwj() {
        let text = format!("é🙂{FAMILY}x");
        // é = 1 unit/2 bytes, 🙂 = 2/4, family = 3 emoji (2/4 each) + 2 ZWJ (1/3 each).
        assert_eq!(utf16_len(&text), 1 + 2 + 8 + 1);
        assert_eq!(utf8_to_utf16(&text, 2), 1);
        assert_eq!(utf8_to_utf16(&text, 6), 3);
        assert_eq!(utf16_to_utf8(&text, 3), 6);
        assert_eq!(utf16_to_utf8(&text, 11), 6 + FAMILY.len());
        // Inside a surrogate pair rounds up to the char end.
        assert_eq!(utf16_to_utf8(&text, 2), 6);
        // Past the end clamps.
        assert_eq!(utf16_to_utf8(&text, 999), text.len());
        assert_eq!(utf8_to_utf16(&text, 999), utf16_len(&text));
        for (i, _) in text.char_indices() {
            assert_eq!(utf16_to_utf8(&text, utf8_to_utf16(&text, i)), i);
        }
    }

    #[test]
    fn grapheme_boundaries_keep_zwj_sequences_whole() {
        let b = composer(&format!("a{FAMILY}b"));
        assert_eq!(b.next_boundary(1), 1 + FAMILY.len());
        assert_eq!(b.previous_boundary(1 + FAMILY.len()), 1);
        assert_eq!(b.previous_boundary(0), 0);
        assert_eq!(b.next_boundary(b.text().len()), b.text().len());
    }

    #[test]
    fn platform_ranges_are_ordered_and_clamped() {
        let mut b = composer("e🙂漢");
        assert_eq!(b.utf16_len(), 4);
        b.set_selected_utf16(&(1..3));
        assert_eq!(b.selected(), 1..5);
        b.set_selected_utf16(&(3..99));
        assert_eq!(b.selected(), 5..8);
        b.set_selected_utf16(&Range { start: 3, end: 1 });
        assert_eq!(b.selected(), 1..5);
        let (text, actual) = b.text_for_utf16(&(1..99));
        assert_eq!(text, "🙂漢");
        assert_eq!(actual, 1..4);
    }

    #[test]
    fn snap_never_splits_a_char() {
        let b = composer("🙂");
        assert_eq!(b.snap(2), 0);
        assert_eq!(b.snap(99), 4);
    }

    #[test]
    fn composer_cap_counts_utf16_and_keeps_graphemes_whole() {
        let mut b = composer(&"a".repeat(COMPOSER_MAX_UTF16 - 1));
        assert_eq!(b.replace(None, "🙂"), Fit::Clipped);
        assert_eq!(b.utf16_len(), COMPOSER_MAX_UTF16 - 1);
        assert_eq!(b.replace(None, "é"), Fit::Whole);
        assert_eq!(b.utf16_len(), COMPOSER_MAX_UTF16);
        assert_eq!(b.replace(None, "x"), Fit::Clipped);

        let mut b = composer(&"a".repeat(COMPOSER_MAX_UTF16 - 9));
        assert_eq!(b.replace(None, &format!("{FAMILY}{FAMILY}")), Fit::Clipped);
        assert!(b.text().ends_with(FAMILY));
        assert_eq!(b.utf16_len(), COMPOSER_MAX_UTF16 - 1);
    }

    #[test]
    fn replacing_a_selection_frees_room() {
        let mut b = composer(&"a".repeat(COMPOSER_MAX_UTF16));
        b.set_selected_utf16(&(0..2));
        assert_eq!(b.replace(None, "🙂"), Fit::Whole);
        assert_eq!(b.utf16_len(), COMPOSER_MAX_UTF16);
        assert!(b.text().starts_with("🙂a"));
    }

    #[test]
    fn phone_filter_and_cap() {
        let mut b = EditBuffer::new(Limits::PHONE);
        assert_eq!(b.replace(None, "+1 (555) 010-0000"), Fit::Whole);
        assert_eq!(b.text(), "+15550100000");
        assert_eq!(b.replace(None, "abc١٢"), Fit::Whole);
        assert_eq!(b.text(), "+15550100000");
        assert_eq!(b.replace(None, &"9".repeat(30)), Fit::Clipped);
        assert_eq!(b.utf16_len(), Limits::PHONE.max_utf16);
    }

    #[test]
    fn code_filter_digits_only() {
        let mut b = EditBuffer::new(Limits::CODE);
        b.replace(None, "12-34 5+6\n");
        assert_eq!(b.text(), "123456");
    }

    #[test]
    fn line_policy() {
        let mut single = EditBuffer::new(Limits {
            filter: CharFilter::Any,
            lines: Lines::Single,
            max_utf16: 100,
        });
        single.replace(None, "a\r\nb\nc\td\u{7}");
        assert_eq!(single.text(), "a b c d");
        let multi = composer("a\r\nb\rc\nd\u{0}");
        assert_eq!(multi.text(), "a\nb\nc\nd");
    }

    #[test]
    fn dead_key_composes() {
        let mut b = composer("x");
        b.replace_and_mark(None, "´", Some(&(1..1)));
        assert_eq!(b.text(), "x´");
        assert_eq!(b.marked(), Some(1..3));
        b.replace(None, "é");
        assert_eq!(b.text(), "xé");
        assert_eq!(b.marked(), None);
        assert_eq!(b.selected(), 3..3);
    }

    #[test]
    fn ime_composition_and_cancel() {
        let mut b = composer("A🙂Z");
        b.replace_and_mark(Some(&(1..3)), "é🙂", Some(&(1..3)));
        assert_eq!(b.text(), "Aé🙂Z");
        assert_eq!(b.marked(), Some(1..7));
        assert_eq!(b.selected(), 3..7);
        b.replace_and_mark(None, "漢", Some(&(1..1)));
        assert_eq!(b.text(), "A漢Z");
        assert_eq!(b.marked(), Some(1..4));
        assert_eq!(b.selected(), 4..4);
        // Cancelling the composition removes the marked text.
        b.replace_and_mark(None, "", Some(&(0..0)));
        assert_eq!(b.text(), "AZ");
        assert_eq!(b.marked(), None);
        assert_eq!(b.selected(), 1..1);
    }

    #[test]
    fn take_empties_and_bumps_revision() {
        let mut b = composer("hello");
        let rev = b.revision();
        assert_eq!(b.take(), "hello");
        assert_eq!(b.text(), "");
        assert_eq!(b.selected(), 0..0);
        assert_ne!(b.revision(), rev);
    }

    #[test]
    fn select_to_flips_direction() {
        let mut b = composer("abcd");
        b.move_to(2);
        b.select_to(0);
        assert_eq!(b.selected(), 0..2);
        assert!(b.reversed());
        assert_eq!(b.cursor(), 0);
        b.select_to(4);
        assert_eq!(b.selected(), 2..4);
        assert!(!b.reversed());
        assert_eq!(b.selected_text(), "cd");
    }
}
