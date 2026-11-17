//! Rendering of the signed-in shell: the chat list (plan section 7) and the
//! open chat (section 8) with its composer and pending sends (section 9).
//! State and decisions live in `crate::chat`; this only draws them.

use std::ops::Range;
use std::time::Instant;

use abyss_core::model::{Body, ChatId, ChatKind, ChatSummary, Message, MsgId};
use abyss_core::rich_text::RichText;
use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, Div, ElementId, FontWeight, Hsla,
    MouseButton, MouseDownEvent, Rgba, SharedString, Stateful, StyledText, WeakEntity, Window,
    canvas, div, font, list, prelude::*, px, transparent_black, uniform_list,
};
use jiff::tz::TimeZone;

use super::session::Session;
use super::topic_view::topic_row;
use super::view::{centered, field, muted};
use super::{Root, State};
use crate::chat::inline::Inline;
use crate::chat::list::{ArchiveEntry, ChatList, Shown, SidebarRow, Tab, View};
use crate::chat::messages::MessageWindow;
use crate::chat::rich::{self, Click, Fonts, Palette, Spoilers};
use crate::chat::sends::Echo;
use crate::chat::{ChatShell, Composer, Pane, Row, copy, format, list as chat_list, topics};
use crate::ui::mark::mark;
use crate::ui::theme::{self, Depth, Theme, Themes};

const CHAT_ROW_HEIGHT: f32 = 56.;
/// A chat row's avatar disc.
const AVATAR_SIZE: f32 = 36.;
/// A chat row's side padding (`px_3`).
const ROW_PADDING: f32 = 12.;
const SIDEBAR_WIDTH: f32 = 300.;
const BUBBLE_MAX_WIDTH: f32 = 560.;
/// Strength of the ink wash behind a bubble's sender band.
const HEADER_WASH: f32 = 0.06;

fn fonts() -> Fonts {
    Fonts {
        body: font(theme::BODY_FONT),
        mono: font(theme::MONO_FONT),
    }
}

/// Rich-text colors for text set in `ink`. All one color: a link is marked
/// by its underline and a mention by its weight. A hidden spoiler is an
/// opaque block, so the text under it cannot show through.
fn palette(ink: Rgba, theme: &Theme) -> Palette {
    Palette {
        text: ink.into(),
        muted: theme::quiet(ink).into(),
        link: ink.into(),
        mention: ink.into(),
        code_bg: theme::faded(ink, 0.1).into(),
        spoiler: theme.hidden.into(),
    }
}

/// Tone tracks age: the depth of something dated `unix`.
pub(super) fn depth_of(unix: i64, now: i64, tz: &TimeZone) -> Depth {
    Depth::from_days(format::days_ago(unix, now, tz))
}

pub(super) fn chat_shell(
    session: &Session,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    div()
        .track_focus(&session.pane_focus)
        .size_full()
        .flex()
        .flex_row()
        .child(sidebar(session, theme, cx))
        .child(chat_pane(session, theme, cx))
        .into_any_element()
}

fn sidebar(session: &Session, theme: &Theme, cx: &mut Context<'_, Root>) -> Div {
    let list = &session.shell.list;
    let rows = list.rows();
    let shown = list.shown();
    let body = if rows.is_empty() {
        let empty = match shown {
            Shown::Tab(Tab::All) => copy::NO_CHATS,
            Shown::Tab(Tab::Private) => copy::NO_PRIVATE_CHATS,
            Shown::Tab(Tab::Groups) => copy::NO_GROUPS,
            Shown::Tab(Tab::Forums) => copy::NO_FORUMS,
            Shown::Tab(Tab::Folder(_)) => copy::NO_FOLDER_CHATS,
            Shown::Archive => copy::NO_ARCHIVED,
        };
        centered().child(muted(empty)).into_any_element()
    } else {
        uniform_list(list_id(list.view()), rows.len(), cx.processor(chat_rows))
            .flex_1()
            .into_any_element()
    };
    // Below the last chat the column keeps that chat's tone, so the list
    // reads as one descent with no lighter gap under it.
    let floor = rows
        .last()
        .and_then(|&row| row_date(&session.shell, row))
        .map_or(Depth::NOW, |date| {
            depth_of(date, format::now(), &format::local_tz())
        });
    let title = sidebar_title(shown, theme, cx);
    // After a core stop nothing can load: no Retry (round 37).
    let retry = (session.shell.list.gave_up() && !session.shell.is_stopped())
        .then(|| retry_button("retry-chats", copy::RETRY_CHATS, theme, cx).bottom_3());
    div()
        .w(px(SIDEBAR_WIDTH))
        .flex_none()
        .h_full()
        .relative()
        .flex()
        .flex_col()
        .bg(theme.surface(floor))
        .text_color(theme.text_on(floor))
        .child(
            div()
                .p_3()
                .flex()
                .justify_between()
                .items_baseline()
                .bg(theme.surface(Depth::NOW))
                .text_color(theme.text)
                .child(title)
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap_3()
                        .child(muted(copy::conn_label(session.conn)).text_xs())
                        .child(appearance_button(cx)),
                ),
        )
        .children(tab_strip(session, theme, cx))
        .child(body)
        // Last, so it paints over the rows and takes their clicks.
        .children(retry)
}

/// The sidebar list's id: each view, with the unread filter on or off,
/// keeps its own scroll position.
fn list_id(view: View) -> ElementId {
    let (name, folder) = match view.shown {
        Shown::Tab(Tab::All) => ("chats-all", None),
        Shown::Tab(Tab::Private) => ("chats-private", None),
        Shown::Tab(Tab::Groups) => ("chats-groups", None),
        Shown::Tab(Tab::Forums) => ("chats-forums", None),
        Shown::Tab(Tab::Folder(id)) => ("chats-folder", Some(u64::from(id.cast_unsigned()))),
        Shown::Archive => ("chats-archive", None),
    };
    let name = if view.unread_only {
        SharedString::from(format!("{name}-unread"))
    } else {
        name.into()
    };
    match folder {
        Some(folder) => ElementId::NamedInteger(name, folder),
        None => name.into(),
    }
}

/// The sidebar's heading: the wordmark, or the archive's title, which also
/// leads back.
fn sidebar_title(shown: Shown, theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
    match shown {
        Shown::Tab(_) => div()
            .flex()
            .items_center()
            .gap_2()
            .child(mark(theme, 18.))
            .child(
                div()
                    .font_family(theme::NAME_FONT)
                    .text_lg()
                    .child(copy::WORDMARK),
            )
            .into_any_element(),
        Shown::Archive => back_button(cx).into_any_element(),
    }
}

/// The strip under the heading: All, Private, Groups and Forums, then each
/// chat folder in server order (plan section 17.2). The archive has no tabs.
/// The tabs scroll sideways; the unread toggle (task 15) goes at the
/// strip's right end, outside the scroll.
fn tab_strip(session: &Session, theme: &Theme, cx: &mut Context<'_, Root>) -> Option<Div> {
    let list = &session.shell.list;
    let Shown::Tab(shown) = list.shown() else {
        return None;
    };
    let tabs = list.tabs().map(|tab| {
        let selected = tab == shown;
        let mark: Hsla = if selected {
            theme.text.into()
        } else {
            transparent_black()
        };
        div()
            .id(tab_id(tab))
            .flex_none()
            .px_2()
            .pb_1()
            .border_b_2()
            .border_color(mark)
            .text_sm()
            .cursor_pointer()
            .child(tab_title(tab, list))
            .when(!selected, |d| d.text_color(theme::quiet(theme.text)))
            .on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
                root.with_session(|s, cx| s.show_chats(Shown::Tab(tab), window, cx), cx);
            }))
            // Folder tabs keep their menu; built-in tabs have none.
            .when_some(folder_id(tab), |d, folder| {
                d.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |root, ev: &MouseDownEvent, window, cx| {
                        root.with_session(
                            |s, cx| s.open_folder_menu(folder, ev.position, window, cx),
                            cx,
                        );
                    }),
                )
            })
    });
    let scroll = div()
        .id("tabs")
        .flex_1()
        .min_w_0()
        .flex()
        .gap_1()
        .px_1()
        .pt_1()
        .overflow_x_scroll()
        .children(tabs);
    Some(
        div()
            .flex()
            .flex_none()
            .bg(theme.surface(Depth::NOW))
            .text_color(theme.text)
            .child(scroll),
    )
}

/// A tab's stable element id: built-in tabs by name, folders by id.
fn tab_id(tab: Tab) -> ElementId {
    match tab {
        Tab::All => "tab-all".into(),
        Tab::Private => "tab-private".into(),
        Tab::Groups => "tab-groups".into(),
        Tab::Forums => "tab-forums".into(),
        Tab::Folder(id) => ("tab-folder", u64::from(id.cast_unsigned())).into(),
    }
}

fn tab_title(tab: Tab, list: &ChatList) -> SharedString {
    match tab {
        Tab::All => copy::ALL_CHATS.into(),
        Tab::Private => copy::PRIVATE_TAB.into(),
        Tab::Groups => copy::GROUPS_TAB.into(),
        Tab::Forums => copy::FORUMS_TAB.into(),
        Tab::Folder(id) => list
            .folders()
            .iter()
            .find(|f| f.id == id)
            .map(|f| SharedString::from(f.title.clone()))
            .unwrap_or_default(),
    }
}

/// The chat folder a tab shows, if it is one.
fn folder_id(tab: Tab) -> Option<i32> {
    match tab {
        Tab::Folder(id) => Some(id),
        Tab::All | Tab::Private | Tab::Groups | Tab::Forums => None,
    }
}

/// Leaves the archive for All.
fn back_button(cx: &mut Context<'_, Root>) -> Stateful<Div> {
    div()
        .id("archive-back")
        .font_family(theme::NAME_FONT)
        .text_lg()
        .cursor_pointer()
        .child(copy::ARCHIVE_BACK)
        .on_click(cx.listener(|root, _: &ClickEvent, window, cx| {
            root.with_session(|s, cx| s.show_chats(Shown::default(), window, cx), cx);
        }))
}

/// Opens the appearance panel; also in the app menu and on Cmd+comma.
fn appearance_button(cx: &mut Context<'_, Root>) -> Stateful<Div> {
    div()
        .id("appearance")
        .text_xs()
        .cursor_pointer()
        .child(crate::ui::copy::APPEARANCE)
        .on_click(cx.listener(|root, _: &ClickEvent, _, cx| {
            root.toggle_appearance(cx);
        }))
}

fn chat_rows(
    root: &mut Root,
    range: Range<usize>,
    window: &mut Window,
    cx: &mut Context<'_, Root>,
) -> Vec<Stateful<Div>> {
    let State::Running(session) = &mut root.state else {
        return Vec::new();
    };
    session.dialogs_seen(range.end.saturating_sub(1), window, cx);
    let theme = &Themes::of(window, cx);
    let (tz, now) = (format::local_tz(), format::now());
    let shell = &session.shell;
    let marked = shell.marked();
    let rows = shell.list.rows();
    rows.get(range.clone())
        .unwrap_or_default()
        .iter()
        .zip(range)
        .map(|(&row, ix)| {
            let selected = marked == Some(row);
            match row {
                SidebarRow::Archive(entry) => archive_row(entry, ix, theme, cx),
                SidebarRow::Chat(id) => {
                    let Some(chat) = shell.list.get(id) else {
                        return empty_row(ix);
                    };
                    let row = ChatRow {
                        chat,
                        selected,
                        inline: shell.inline_of(id),
                        time: format::list_time(chat.last_date, now, &tz),
                        depth: depth_of(chat.last_date, now, &tz),
                    };
                    row.render(ix, theme, cx)
                }
                SidebarRow::Topic { topic, .. } => {
                    let Some(topic) = shell.topics().and_then(|l| l.get(topic)) else {
                        return empty_row(ix);
                    };
                    let id = ("inline-topic", u64::from(topic.id.get().cast_unsigned()));
                    let ink = theme.text_on(depth_of(topics::activity(topic), now, &tz));
                    let mark = if selected {
                        ink.into()
                    } else {
                        transparent_black()
                    };
                    let row = topic_row(topic, id.into(), (&tz, now), theme, cx);
                    inline_band(row, ink, mark)
                }
                SidebarRow::MoreTopics { forum } => {
                    let depth = forum_depth(shell, forum, now, &tz);
                    more_row(depth, theme, cx)
                }
                SidebarRow::TopicsPending { forum } => {
                    let depth = forum_depth(shell, forum, now, &tz);
                    let countdown = shell
                        .topics()
                        .and_then(|l| l.countdown(cx.background_executor().now()));
                    let text = super::topic_view::placeholder(shell, countdown);
                    let band = div().id("topics-pending").child(muted(text).text_xs());
                    inline_band(band, theme.text_on(depth), transparent_black())
                        .bg(theme.surface(depth))
                }
            }
        })
        .collect()
}

/// When a sidebar row's thing last changed, for its tone: a chat's last
/// message, a topic's activity. Rows without one sit at the surface.
fn row_date(shell: &ChatShell, row: SidebarRow) -> Option<i64> {
    match row {
        SidebarRow::Chat(id)
        | SidebarRow::MoreTopics { forum: id }
        | SidebarRow::TopicsPending { forum: id } => shell.list.get(id).map(|c| c.last_date),
        SidebarRow::Topic { topic, .. } => shell
            .topics()
            .and_then(|l| l.get(topic))
            .map(topics::activity),
        SidebarRow::Archive(_) => None,
    }
}

/// The tone of a forum's More and pending rows: the forum row's own.
fn forum_depth(shell: &ChatShell, forum: ChatId, now: i64, tz: &TimeZone) -> Depth {
    shell
        .list
        .get(forum)
        .map_or(Depth::NOW, |c| depth_of(c.last_date, now, tz))
}

/// A row whose chat or topic no longer resolves: blank for the one frame
/// until the rows are recomputed (plan section 17.3).
fn empty_row(ix: usize) -> Stateful<Div> {
    div().id(ix).w_full().h(px(CHAT_ROW_HEIGHT))
}

/// An inline row under the pane's forum (plan section 17.3): as tall as a
/// chat row, so the sidebar stays a uniform list, indented by the avatar's
/// width, with a chat row's selection rule (`mark`: the ink or nothing).
fn inline_band(row: Stateful<Div>, ink: Rgba, mark: Hsla) -> Stateful<Div> {
    row.w_full()
        .h(px(CHAT_ROW_HEIGHT))
        .pl(px(ROW_PADDING + AVATAR_SIZE))
        .pr(px(ROW_PADDING))
        .flex()
        .items_center()
        .border_l_2()
        .border_color(mark)
        .text_color(ink)
}

/// "More topics": the full list shows in the pane. From a topic, it goes
/// back to the list; on the list itself, it does nothing more.
fn more_row(depth: Depth, theme: &Theme, cx: &mut Context<'_, Root>) -> Stateful<Div> {
    let band = div()
        .id("more-topics")
        .cursor_pointer()
        .child(muted(copy::MORE_TOPICS).text_xs())
        .on_click(cx.listener(|root, _: &ClickEvent, window, cx| {
            root.with_session(|s, cx| s.more_topics(window, cx), cx);
        }));
    inline_band(band, theme.text_on(depth), transparent_black()).bg(theme.surface(depth))
}

/// The main list's entry for the archive: opens it in the sidebar.
fn archive_row(
    entry: ArchiveEntry,
    ix: usize,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> Stateful<Div> {
    let (surface, ink) = (theme.surface(Depth::NOW), theme.text_on(Depth::NOW));
    div()
        .id(ix)
        .w_full()
        .h(px(CHAT_ROW_HEIGHT))
        .px_3()
        .flex()
        .items_center()
        .gap_3()
        .border_l_2()
        .border_color(transparent_black())
        .bg(surface)
        .text_color(ink)
        .cursor_pointer()
        .on_click(cx.listener(|root, _: &ClickEvent, window, cx| {
            root.with_session(|s, cx| s.show_chats(Shown::Archive, window, cx), cx);
        }))
        .child(
            div()
                .size(px(36.))
                .flex_none()
                .rounded_full()
                .border_1()
                .border_color(theme::faded(ink, 0.4)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().font_family(theme::NAME_FONT).child(copy::ARCHIVE))
                .child(
                    muted(copy::archive_summary(entry.chats, entry.unread))
                        .text_xs()
                        .truncate(),
                ),
        )
}

struct ChatRow<'a> {
    chat: &'a ChatSummary,
    selected: bool,
    /// The pane's forum's expansion, shown by its disclosure glyph.
    inline: Option<Inline>,
    time: SharedString,
    /// How far the chat has sunk: its surface darkens with its age.
    depth: Depth,
}

impl ChatRow<'_> {
    fn render(self, ix: usize, theme: &Theme, cx: &mut Context<'_, Root>) -> Stateful<Div> {
        let chat = self.chat;
        let id = chat.id;
        let (surface, ink) = (theme.surface(self.depth), theme.text_on(self.depth));
        // The selected chat is marked by a rule in its own text color.
        let mark: Hsla = if self.selected {
            ink.into()
        } else {
            transparent_black()
        };
        let tag = copy::kind_label(chat.kind).map(|t| muted(t).text_xs().flex_none());
        let title = div()
            .flex()
            .gap_2()
            .children(self.inline.map(|inline| disclosure(inline, cx)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(theme::NAME_FONT)
                    .child(chat.title.clone()),
            )
            .children(tag)
            .child(muted(self.time).text_xs().flex_none());
        let preview = div()
            .flex()
            .gap_2()
            .child(
                muted(chat_list::preview_text(chat.last.as_ref()))
                    .text_xs()
                    .flex_1()
                    .min_w_0()
                    .truncate(),
            )
            .children(unread_badge(chat, surface, ink));
        div()
            .id(ix)
            .w_full()
            .h(px(CHAT_ROW_HEIGHT))
            .px_3()
            .flex()
            .items_center()
            .gap_3()
            .border_l_2()
            .border_color(mark)
            .bg(surface)
            .text_color(ink)
            .cursor_pointer()
            .on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
                root.with_session(|s, cx| s.select_chat(id, window, cx), cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |root, ev: &MouseDownEvent, window, cx| {
                    root.with_session(|s, cx| s.open_chat_menu(id, ev.position, window, cx), cx);
                }),
            )
            .child(avatar(chat, ink))
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
}

/// The pane's forum's disclosure glyph: "▾" while its topics show under
/// it, "▸" while collapsed (no state by hue alone, brand rule 2). A click
/// toggles them and stops there, so the row's own click doesn't select.
fn disclosure(inline: Inline, cx: &mut Context<'_, Root>) -> Stateful<Div> {
    let glyph = match inline {
        Inline::Expanded => copy::EXPANDED,
        Inline::Collapsed => copy::COLLAPSED,
    };
    div()
        .id("forum-disclosure")
        .flex_none()
        .cursor_pointer()
        .child(glyph)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(move |root, _: &ClickEvent, _, cx| {
            cx.stop_propagation();
            root.with_session(|s, cx| s.set_inline(inline.toggled(), cx), cx);
        }))
}

/// A chat row's unread count.
fn unread_badge(chat: &ChatSummary, surface: Rgba, ink: Rgba) -> Option<Div> {
    (chat.unread > 0).then(|| count_badge(chat.unread, chat.unread_mentions, surface, ink))
}

/// An unread count, marked when it holds mentions: the row's own two
/// colors, swapped.
pub(super) fn count_badge(unread: u32, mentions: u32, surface: Rgba, ink: Rgba) -> Div {
    let mention = if mentions > 0 { "@ " } else { "" };
    div()
        .px_2()
        .flex_none()
        .rounded_full()
        .bg(ink)
        .text_color(surface)
        .text_xs()
        .child(format!("{mention}{}", copy::unread_badge(unread)))
}

/// Initials on a faint disc of the row's text color (no images in Stage 1).
fn avatar(chat: &ChatSummary, ink: Rgba) -> Div {
    div()
        .size(px(AVATAR_SIZE))
        .flex_none()
        .rounded_full()
        .bg(theme::faded(ink, 0.14))
        .text_sm()
        .flex()
        .justify_center()
        .items_center()
        .child(chat_list::initials(&chat.title))
}

/// What the open chat's pane shows instead of rows (`None`: its rows).
/// After a core stop, history never loaded is unavailable, never
/// "Loading…" (task 6m).
pub(super) fn placeholder(shell: &ChatShell) -> Option<&'static str> {
    if shell.row_count() > 0 {
        None
    } else if shell.window().is_some_and(MessageWindow::is_loaded) {
        Some(copy::EMPTY_CHAT)
    } else if shell.is_stopped() {
        Some(copy::HISTORY_UNAVAILABLE)
    } else {
        Some(copy::LOADING_HISTORY)
    }
}

fn chat_pane(session: &Session, theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
    let Some(chat) = session.shell.selected_chat() else {
        return centered()
            .child(muted(copy::SELECT_CHAT))
            .into_any_element();
    };
    let header = match session.shell.pane() {
        Pane::Forum { open: None, .. } => {
            return super::topic_view::topic_pane(session, chat, theme, cx);
        }
        Pane::Forum {
            open: Some(topic), ..
        } => super::topic_view::topic_header(session, chat, topic, cx),
        Pane::Empty | Pane::Thread(_) => pane_header(chat),
    };
    let window = session.shell.window();
    let body = match placeholder(&session.shell) {
        None => list(session.shell.rows().clone(), cx.processor(message_row))
            .size_full()
            .into_any_element(),
        Some(text) => centered().child(muted(text)).into_any_element(),
    };
    // Above the oldest loaded message the pane keeps that message's tone.
    let oldest = session.shell.row(0).map_or(Depth::NOW, |r| {
        depth_of(r.date(), format::now(), &format::local_tz())
    });
    // After a core stop nothing can load: no Jump or Retry (round 37).
    let live = !session.shell.is_stopped();
    let detached = live && window.is_some_and(MessageWindow::has_newer);
    let gave_up = live && session.shell.history_gave_up();
    div()
        .flex_1()
        .h_full()
        .min_w_0()
        .flex()
        .flex_col()
        .child(header)
        .child(
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .bg(theme.surface(oldest))
                .text_color(theme.text_on(oldest))
                .child(body)
                .children(detached.then(|| jump_button(theme, cx)))
                .children(
                    gave_up.then(|| {
                        retry_button("retry-history", copy::RETRY_HISTORY, theme, cx).top_3()
                    }),
                )
                .child(after_paint(cx)),
        )
        .child(composer_area(session, theme))
        .into_any_element()
}

/// Once the pane is painted, checks what the message list shows (mark
/// read, load more, live tail): the layout is then the one on screen.
pub(super) fn after_paint(cx: &mut Context<'_, Root>) -> impl IntoElement {
    let this = cx.weak_entity();
    canvas(
        |_, _, _| {},
        move |_, (), window, cx| super::chats::after_paint(this, window, cx),
    )
    .absolute()
    .size_0()
}

/// The chat's title and kind label.
pub(super) fn pane_header(chat: &ChatSummary) -> Div {
    div()
        .p_3()
        .flex()
        .gap_2()
        .items_baseline()
        .child(
            div()
                .font_family(theme::NAME_FONT)
                .text_lg()
                .truncate()
                .child(chat.title.clone()),
        )
        .children(copy::kind_label(chat.kind).map(|t| muted(t).text_xs()))
}

fn jump_button(theme: &Theme, cx: &mut Context<'_, Root>) -> Stateful<Div> {
    pill(
        "jump-to-latest",
        copy::JUMP_TO_LATEST,
        theme.text,
        theme.on_text,
    )
    .absolute()
    .bottom_3()
    .right_3()
    .on_click(cx.listener(|root, _: &ClickEvent, window, cx| {
        root.with_session(|s, cx| s.jump_to_latest(window, cx), cx);
    }))
}

/// Offered once automatic retries gave up: retries everything now. An
/// overlay: rendered after what it covers, it blocks the mouse for the
/// rows below and stops the click there (task 10e).
pub(super) fn retry_button(
    id: &'static str,
    label: &'static str,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> Stateful<Div> {
    pill(id, label, theme.text, theme.on_text)
        .debug_selector(|| id.to_owned())
        .absolute()
        .left_3()
        .occlude()
        .on_click(cx.listener(|root, _: &ClickEvent, window, cx| {
            cx.stop_propagation();
            root.with_session(|s, cx| s.retry_loads(window, cx), cx);
        }))
}

/// A small action in `ink` on `fill`.
pub(super) fn pill(
    id: impl Into<ElementId>,
    label: &'static str,
    fill: Rgba,
    ink: Rgba,
) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_full()
        .bg(fill)
        .text_color(ink)
        .text_sm()
        .cursor_pointer()
        .child(label)
}

/// The message box, or why there is none: a read-only chat, a closed
/// topic. Echoes above it still show and can be discarded.
fn composer_area(session: &Session, theme: &Theme) -> Div {
    let area = div().p_3();
    match session.shell.composer() {
        Composer::Open => area.child(field(session.composer.clone().into_any_element(), theme)),
        Composer::ReadOnly => area.child(muted(copy::READ_ONLY).text_sm()),
        Composer::TopicClosed => area.child(muted(copy::TOPIC_CLOSED).text_sm()),
    }
}

/// Renders row `ix` of the open chat's message list.
fn message_row(
    root: &mut Root,
    ix: usize,
    window: &mut Window,
    cx: &mut Context<'_, Root>,
) -> AnyElement {
    let theme = Themes::of(window, cx);
    let State::Running(session) = &root.state else {
        return div().into_any_element();
    };
    let shell = &session.shell;
    let Some(row) = shell.row(ix) else {
        return div().into_any_element();
    };
    let ctx = RowCtx {
        this: cx.weak_entity(),
        theme,
        tz: format::local_tz(),
        groupish: shell
            .selected_chat()
            .is_some_and(|c| !matches!(c.kind, ChatKind::Private { .. } | ChatKind::Bot)),
    };
    let prev_date = ix
        .checked_sub(1)
        .and_then(|p| shell.row(p))
        .map(|r| r.date());
    let separator = ctx.separator(row.date(), prev_date);
    let start = (ix == 0
        && shell
            .window()
            .is_some_and(|w| w.is_loaded() && !w.has_older()))
    .then(|| day_separator(copy::START_OF_HISTORY.into()));
    let content = match row {
        Row::Message(m) => {
            let reply = m
                .reply_to
                .and_then(|r| shell.window().and_then(|w| w.by_id(r)));
            ctx.message(m, shell.spoilers(m.id), reply)
        }
        Row::Echo(e) => echo_row(e, &theme, cx),
    };
    // Each row carries the tone of its own age, so history darkens upward.
    let depth = depth_of(row.date(), format::now(), &ctx.tz);
    let row = div()
        .w_full()
        .px_4()
        .py_1()
        .flex()
        .flex_col()
        .bg(theme.surface(depth))
        .text_color(theme.text_on(depth))
        .children(start)
        .children(separator)
        .child(content);
    // Painted bounds feed the after-paint check (mark read, live tail).
    shell.painted().track(ix, row).w_full().into_any_element()
}

fn day_separator(label: SharedString) -> Div {
    div()
        .w_full()
        .py_2()
        .flex()
        .justify_center()
        .child(muted(label).text_xs())
}

/// Which side of the pane a bubble sits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Incoming,
    Outgoing,
}

impl Side {
    /// The user's own words carry the most contrast on screen; whatever
    /// arrives sits on a quiet wash.
    fn fill(self, theme: &Theme) -> Rgba {
        match self {
            Self::Incoming => theme.incoming,
            Self::Outgoing => theme.text,
        }
    }

    /// The text color inside a bubble on this side.
    fn ink(self, theme: &Theme) -> Rgba {
        match self {
            Self::Incoming => theme.text,
            Self::Outgoing => theme.on_text,
        }
    }
}

/// A message bubble, right-aligned for outgoing messages. A `header` (the
/// sender, in groups) sits on its own tinted band above the body, set off by
/// a hairline, so who spoke never blends into what was said.
fn in_bubble(
    side: Side,
    theme: &Theme,
    header: Option<AnyElement>,
    content: Vec<AnyElement>,
) -> AnyElement {
    let ink = side.ink(theme);
    let band = header.map(|h| {
        div()
            .w_full()
            .px_3()
            .pt_2()
            .pb_1p5()
            // GPUI clips children to a rectangle, not the bubble's rounded
            // corners, so the band rounds its own top to match.
            .rounded_t(px(theme::RADIUS))
            .bg(theme::faded(ink, HEADER_WASH))
            .border_b_1()
            .border_color(theme::line(ink))
            .child(h)
    });
    let body = div()
        .px_3()
        .py_2()
        .flex()
        .flex_col()
        .gap_1()
        .children(content);
    let inner = div()
        .max_w(px(BUBBLE_MAX_WIDTH))
        .rounded(px(theme::RADIUS))
        .bg(side.fill(theme))
        .text_color(ink)
        .flex()
        .flex_col()
        .children(band)
        .child(body);
    let row = div().w_full().flex();
    let row = if side == Side::Outgoing {
        row.justify_end()
    } else {
        row.justify_start()
    };
    row.child(inner).into_any_element()
}

fn media_chip(label: &str, ink: Rgba) -> Div {
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(theme::line(ink))
        .text_sm()
        .child(format!("[{label}]"))
}

fn reply_quote(target: &Message, ink: Rgba) -> Div {
    let text: SharedString = match &target.body {
        Body::Text(rt) => chat_list::preview_text(Some(&abyss_core::model::Preview::Text(
            abyss_core::sanitize::clean(rt.text(), abyss_core::sanitize::MAX_PREVIEW_BYTES),
        ))),
        Body::Media { kind, .. } => format!("[{}]", copy::media_label(kind)).into(),
        Body::Service(_) | Body::Unsupported => copy::SERVICE_FALLBACK.into(),
    };
    div()
        .pl_2()
        .border_l_2()
        .border_color(ink)
        .text_xs()
        .opacity(theme::QUIET)
        .truncate()
        .child(text)
}

/// Per-row rendering context.
struct RowCtx {
    this: WeakEntity<Root>,
    theme: Theme,
    tz: TimeZone,
    /// Groups show sender names.
    groupish: bool,
}

impl RowCtx {
    /// A day separator when this row starts a new local day.
    fn separator(&self, date: i64, prev: Option<i64>) -> Option<Div> {
        let day = format::day(date, &self.tz)?;
        let today = format::day(format::now(), &self.tz)?;
        let prev = prev.and_then(|p| format::day(p, &self.tz));
        (prev != Some(day)).then(|| day_separator(format::day_label(day, today)))
    }

    fn message(&self, m: &Message, spoilers: Spoilers, reply: Option<&Message>) -> AnyElement {
        let side = if m.outgoing {
            Side::Outgoing
        } else {
            Side::Incoming
        };
        let ink = side.ink(&self.theme);
        let body = match &m.body {
            Body::Service(text) => {
                return div()
                    .w_full()
                    .flex()
                    .justify_center()
                    .child(muted(text.clone()).text_sm().italic())
                    .into_any_element();
            }
            Body::Text(rt) => self.rich(m.id, rt, spoilers, ink),
            Body::Media { kind, caption } => div()
                .flex()
                .flex_col()
                .gap_1()
                .child(media_chip(&copy::media_label(kind), ink))
                .children(
                    (!caption.text().is_empty()).then(|| self.rich(m.id, caption, spoilers, ink)),
                )
                .into_any_element(),
            Body::Unsupported => muted(copy::UNSUPPORTED).italic().into_any_element(),
        };
        let sender = m
            .sender
            .as_ref()
            .filter(|_| self.groupish && !m.outgoing)
            .map(|s| {
                div()
                    .text_sm()
                    .font_family(theme::NAME_FONT)
                    .font_weight(FontWeight::SEMIBOLD)
                    .truncate()
                    .child(s.name.clone())
                    .into_any_element()
            });
        let time = format::time_of_day(m.date, &self.tz);
        let footer = if m.edited() {
            format!("{} · {time}", copy::EDITED)
        } else {
            time.to_string()
        };
        let mut content: Vec<AnyElement> = Vec::with_capacity(3);
        content.extend(reply.map(|r| reply_quote(r, ink).into_any_element()));
        content.push(body);
        content.push(muted(footer).text_xs().into_any_element());
        in_bubble(side, &self.theme, sender, content)
    }

    /// Formatted text: a link click copies its target (links never open), a
    /// spoiler click reveals the message's spoilers.
    fn rich(&self, id: MsgId, rt: &RichText, spoilers: Spoilers, ink: Rgba) -> AnyElement {
        let this = self.this.clone();
        let element_id =
            ElementId::NamedInteger("msg".into(), u64::try_from(id.0).unwrap_or_default());
        rich::style(rt, &fonts(), &palette(ink, &self.theme), spoilers)
            .interactive(element_id, move |click, window, cx| {
                on_text_click(&this, id, &click, window, cx);
            })
            .into_any_element()
    }
}

/// An own send without a server copy yet: its state, and Retry and Discard
/// where core accepts them; text core refused offers Put back and Dismiss.
fn echo_row(e: &Echo, theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
    let local_id = e.local_id;
    // Actions sit inside the user's own bubble, so their colors are swapped.
    let (fill, ink) = (theme.on_text, theme.text);
    let status = div()
        .flex()
        .gap_2()
        .items_center()
        .justify_end()
        .child(muted(copy::send_label(e, Instant::now())).text_xs());
    let retry = e.can_retry().then(|| {
        let retry = cx.listener(move |root, _: &ClickEvent, window, cx| {
            root.with_session(|s, cx| s.retry_send(local_id, window, cx), cx);
        });
        pill(("retry", local_id.0), copy::RETRY, fill, ink).on_click(retry)
    });
    let discard = e.can_discard().then(|| {
        let discard = cx.listener(move |root, _: &ClickEvent, window, cx| {
            root.with_session(|s, cx| s.discard_send(local_id, window, cx), cx);
        });
        pill(("discard", local_id.0), copy::DISCARD, fill, ink).on_click(discard)
    });
    let returned = e.is_returned().then(|| {
        let put_back = cx.listener(move |root, _: &ClickEvent, window, cx| {
            root.with_session(|s, cx| s.put_back(local_id, window, cx), cx);
        });
        let dismiss = cx.listener(move |root, _: &ClickEvent, _, cx| {
            root.with_session(|s, cx| s.dismiss_returned(local_id, cx), cx);
        });
        [
            pill(("put-back", local_id.0), copy::PUT_BACK, fill, ink).on_click(put_back),
            pill(("dismiss", local_id.0), copy::DISMISS, fill, ink).on_click(dismiss),
        ]
    });
    let stranded = e.is_stranded().then(|| {
        let copy_text = cx.listener(move |root, _: &ClickEvent, window, cx| {
            root.with_session(|s, cx| s.copy_stranded(local_id, window, cx), cx);
        });
        let dismiss = cx.listener(move |root, _: &ClickEvent, _, cx| {
            root.with_session(|s, cx| s.dismiss_returned(local_id, cx), cx);
        });
        [
            pill(("copy-text", local_id.0), copy::COPY_TEXT, fill, ink).on_click(copy_text),
            pill(("dismiss", local_id.0), copy::DISMISS, fill, ink).on_click(dismiss),
        ]
    });
    let status = status
        .children(retry)
        .children(discard)
        .children(returned.into_iter().flatten())
        .children(stranded.into_iter().flatten());
    let text = rich::plain(
        &e.text,
        &fonts(),
        &palette(Side::Outgoing.ink(theme), theme),
    );
    let body = StyledText::new(text.text).with_runs(text.runs);
    in_bubble(
        Side::Outgoing,
        theme,
        None,
        vec![body.into_any_element(), status.into_any_element()],
    )
}

fn on_text_click(
    this: &WeakEntity<Root>,
    id: MsgId,
    click: &Click,
    window: &mut Window,
    cx: &mut App,
) {
    if let Click::Copy(target) = click {
        cx.write_to_clipboard(ClipboardItem::new_string(target.to_string()));
    }
    this.update(cx, |root, cx| {
        root.with_session(
            |s, cx| match click {
                Click::Copy(_) => s.show_notice(copy::LINK_COPIED.into(), window, cx),
                Click::Reveal => s.shell.reveal(id),
            },
            cx,
        );
        cx.notify();
    })
    .ok();
}
