//! Rendering of a forum (plan section 8): its topic list, with paging, the
//! flood-wait countdown and Retry, and the header of an open topic, which
//! leads back to the list. State and decisions live in `crate::chat::topics`;
//! this only draws them.

use std::ops::Range;

use abyss_core::model::{ChatSummary, Topic, TopicId};
use gpui::{
    AnyElement, ClickEvent, Context, Div, ElementId, Rgba, SharedString, Stateful, Window, div,
    prelude::*, px, uniform_list,
};
use jiff::tz::TimeZone;

use super::chat_view::{after_paint, count_badge, depth_of, pane_header, retry_button};
use super::session::Session;
use super::view::{centered, muted};
use super::{Root, State};
use crate::chat::topics::{self, UnreadMark};
use crate::chat::{ChatShell, copy, format, list as chat_list};
use crate::ui::theme::{self, Depth, Theme, Themes};

const TOPIC_ROW_HEIGHT: f32 = 56.;
/// The "new since the count" dot.
const DOT_SIZE: f32 = 8.;

/// What the topic list shows instead of rows: a flood wait's countdown
/// replaces "Loading topics…" while there is no copy.
pub(super) fn placeholder(shell: &ChatShell, countdown: Option<u64>) -> SharedString {
    match (shell.topics().filter(|l| l.is_loaded()), countdown) {
        (Some(_), _) => copy::NO_TOPICS.into(),
        (None, Some(left)) => copy::topics_flood(left),
        (None, None) if shell.is_stopped() => copy::TOPICS_UNAVAILABLE.into(),
        (None, None) => copy::LOADING_TOPICS.into(),
    }
}

/// A forum's pane: its header and topic list. Under a retained list, a
/// flood wait counts down in a footer.
pub(super) fn topic_pane(
    session: &Session,
    chat: &ChatSummary,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let shell = &session.shell;
    let list = shell.topics();
    let countdown = list.and_then(|l| l.countdown(cx.background_executor().now()));
    let rows = list.map_or(0, |l| l.topics().len());
    let body = if rows == 0 {
        let text = placeholder(shell, countdown);
        centered().child(muted(text)).into_any_element()
    } else {
        // Each forum keeps its own scroll position.
        let id = ElementId::NamedInteger("topics".into(), chat.id.id.cast_unsigned());
        uniform_list(id, rows, cx.processor(topic_rows))
            .flex_1()
            .into_any_element()
    };
    let footer = countdown.filter(|_| rows > 0).map(|left| {
        div()
            .px_3()
            .py_2()
            .child(muted(copy::topics_flood(left)).text_xs())
    });
    // Below the last topic the column keeps that topic's tone.
    let floor = list
        .and_then(|l| l.topics().last())
        .map_or(Depth::NOW, |t| {
            depth_of(topics::activity(t), format::now(), &format::local_tz())
        });
    // After a core stop nothing can load: no Retry (round 37).
    let retry = (shell.topics_gave_up() && !shell.is_stopped())
        .then(|| retry_button("retry-topics", copy::RETRY_TOPICS, theme, cx).bottom_3());
    div()
        .flex_1()
        .h_full()
        .min_w_0()
        .flex()
        .flex_col()
        .child(pane_header(chat))
        .child(
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .flex()
                .flex_col()
                .bg(theme.surface(floor))
                .text_color(theme.text_on(floor))
                .child(body)
                .children(footer)
                .children(retry)
                .child(after_paint(cx)),
        )
        .into_any_element()
}

/// An open topic's header: the way back to the list, then the chat's and
/// the topic's titles, and a mark while the topic is closed. The title
/// falls back on "General" or "Topic" while the retained list lacks it.
pub(super) fn topic_header(
    session: &Session,
    chat: &ChatSummary,
    topic: TopicId,
    cx: &mut Context<'_, Root>,
) -> Div {
    let shown = session.shell.shown_topic();
    let title = shown.map_or_else(|| topics::fallback_title(topic), topics::title);
    let closed = shown.is_some_and(|t| t.closed);
    div()
        .p_3()
        .flex()
        .gap_3()
        .items_baseline()
        .child(
            div()
                .id("topics-back")
                .flex_none()
                .cursor_pointer()
                .child(copy::TOPICS_BACK)
                .on_click(cx.listener(|root, _: &ClickEvent, window, cx| {
                    root.with_session(|s, cx| s.back_from_forum(window, cx), cx);
                })),
        )
        .child(
            div()
                .flex_none()
                .max_w(px(240.))
                .truncate()
                .font_family(theme::NAME_FONT)
                .text_lg()
                .child(chat.title.clone()),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .font_family(theme::NAME_FONT)
                .child(title),
        )
        .children(closed.then(|| muted(copy::CLOSED_TOPIC).text_xs().flex_none()))
}

fn topic_rows(
    root: &mut Root,
    range: Range<usize>,
    window: &mut Window,
    cx: &mut Context<'_, Root>,
) -> Vec<Stateful<Div>> {
    let State::Running(session) = &mut root.state else {
        return Vec::new();
    };
    session.topics_seen(range.end.saturating_sub(1), window, cx);
    let theme = &Themes::of(window, cx);
    let (tz, now) = (format::local_tz(), format::now());
    let Some(list) = session.shell.topics() else {
        return Vec::new();
    };
    list.topics()
        .get(range.clone())
        .unwrap_or_default()
        .iter()
        .zip(range)
        .map(|(topic, ix)| topic_row(topic, ix.into(), (&tz, now), theme, cx))
        .collect()
}

/// A topic: the neutral glyph (owner decision D3), its title with closed
/// and pinned marks and its time, then its last message and unread mark.
/// Its tone sinks with its age, like a chat row. The sidebar's inline rows
/// are these too.
pub(super) fn topic_row(
    topic: &Topic,
    id: ElementId,
    (tz, now): (&TimeZone, i64),
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> Stateful<Div> {
    let topic_id = topic.id;
    let depth = depth_of(topics::activity(topic), now, tz);
    let (surface, ink) = (theme.surface(depth), theme.text_on(depth));
    let tag = |text: &'static str| muted(text).text_xs().flex_none();
    let title = div()
        .flex()
        .gap_2()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(theme::NAME_FONT)
                .child(topics::title(topic)),
        )
        .children(topic.closed.then(|| tag(copy::CLOSED_TOPIC)))
        .children(topic.pinned.then(|| tag(copy::PINNED_TOPIC)))
        .child(
            muted(format::list_time(topic.last_date, now, tz))
                .text_xs()
                .flex_none(),
        );
    let preview = div()
        .flex()
        .gap_2()
        .items_center()
        .child(
            muted(chat_list::preview_text(topic.last.as_ref()))
                .text_xs()
                .flex_1()
                .min_w_0()
                .truncate(),
        )
        .children(unread_mark(UnreadMark::of(topic), theme.badge(depth)));
    div()
        .id(id)
        .w_full()
        .h(px(TOPIC_ROW_HEIGHT))
        .px_3()
        .flex()
        .items_center()
        .gap_3()
        .bg(surface)
        .text_color(ink)
        .cursor_pointer()
        .on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
            root.with_session(|s, cx| s.select_topic(topic_id, window, cx), cx);
        }))
        .child(glyph(ink))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(title)
                .child(preview),
        )
}

/// The topic glyph on a faint disc of the row's text color, like a chat
/// row's initials.
fn glyph(ink: Rgba) -> Div {
    div()
        .size(px(36.))
        .flex_none()
        .rounded_full()
        .bg(theme::faded(ink, 0.14))
        .flex()
        .justify_center()
        .items_center()
        .child(copy::TOPIC_GLYPH)
}

fn unread_mark(mark: UnreadMark, badge: (Rgba, Rgba)) -> Option<Div> {
    match mark {
        UnreadMark::None => None,
        UnreadMark::Count { unread, mentions } => Some(count_badge(unread, mentions, badge)),
        UnreadMark::Dot => Some(
            div()
                .size(px(DOT_SIZE))
                .flex_none()
                .rounded_full()
                .bg(badge.0),
        ),
    }
}
