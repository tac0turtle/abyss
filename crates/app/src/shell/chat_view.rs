//! Rendering of the signed-in shell: the chat list (plan section 7) and the
//! open chat (section 8) with its composer and pending sends (section 9).
//! State and decisions live in `crate::chat`; this only draws them.

use std::borrow::Cow;
use std::ops::Range;
use std::time::Instant;

use abyss_core::model::{
    Body, ChatId, ChatKind, ChatSummary, LocalId, MediaKind, Message, MsgId, PeerKind, Topic,
};
use abyss_core::rich_text::RichText;
use gpui::{
    AnyElement, App, ClipboardItem, Context, Div, ElementId, FocusHandle, FontWeight, Hsla,
    KeyDownEvent, ListOffset, ListState, MouseButton, Rgba, Role, SharedString, Stateful,
    StyledText, WeakEntity, Window, canvas, div, font, list, prelude::*, px, transparent_black,
    uniform_list,
};
use jiff::tz::TimeZone;

use super::nav::RowKey;
use super::session::Session;
use super::topic_view::{RowFocus, topic_row};
use super::view::{centered, muted, muted_in};
use super::{ReturnToRow, Root, State};
use crate::chat::inline::Inline;
use crate::chat::list::{ArchiveEntry, Shown, SidebarRow, Tab, View};
use crate::chat::messages::MessageWindow;
use crate::chat::rich::{self, Click, Fonts, Palette, Spoilers};
use crate::chat::sends::Echo;
use crate::chat::{ChatShell, Composer, Pane, Row, copy, format, list as chat_list, topics};
use crate::ui::a11y;
use crate::ui::mark::mark;
use crate::ui::theme::{self, Depth, Side, Theme, Themes};

/// The chat shell's key context: Escape here returns to the opened row.
pub(super) const KEY_CONTEXT: &str = "ChatShell";

const CHAT_ROW_HEIGHT: f32 = 56.;
/// A chat row's avatar disc.
const AVATAR_SIZE: f32 = 36.;
/// A chat row's side padding (`px_3`).
const ROW_PADDING: f32 = 12.;
const SIDEBAR_WIDTH: f32 = 300.;
const BUBBLE_MAX_WIDTH: f32 = 560.;

fn fonts() -> Fonts {
    Fonts {
        body: font(theme::BODY_FONT),
        mono: font(theme::MONO_FONT),
    }
}

/// Rich-text colors inside a message from `side`. All one color: a link is marked
/// by its underline and a mention by its weight. A hidden spoiler is an
/// opaque block, so the text under it cannot show through.
fn palette(side: Side, theme: &Theme) -> Palette {
    let ink = theme.bubble_ink(side);
    Palette {
        text: ink.into(),
        muted: theme::faded(ink, theme.quiet_in(side)).into(),
        link: ink.into(),
        mention: ink.into(),
        code_bg: theme::faded(ink, theme::CODE_WASH).into(),
        spoiler: theme.spoiler(side).into(),
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
        .key_context(KEY_CONTEXT)
        .on_action(cx.listener(|root, _: &ReturnToRow, _, cx| {
            root.with_session(Session::focus_opened_row, cx);
        }))
        .size_full()
        .flex()
        .flex_row()
        .child(super::tabs::rail(session, theme, cx))
        .child(sidebar(session, theme, cx))
        .child(chat_pane(session, theme, cx))
        .into_any_element()
}

fn sidebar(session: &Session, theme: &Theme, cx: &mut Context<'_, Root>) -> Div {
    let list = &session.shell.list;
    let rows = list.rows();
    let shown = list.shown();
    let body = sidebar_list(session, theme, cx);
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
                        .child(quiet("connection", copy::conn_label(session.conn), theme).text_xs())
                        .child(appearance_button(theme, cx)),
                ),
        )
        .child(body)
        // Last, so it paints over the rows and takes their clicks.
        .children(retry)
}

fn sidebar_list(session: &Session, theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
    let list = &session.shell.list;
    let rows = list.rows();
    let shown = list.shown();
    if rows.is_empty() {
        let empty = match shown {
            _ if list.view().unread_only && list.partly_loaded() => copy::NO_UNREAD_LOADED,
            _ if list.view().unread_only => copy::NO_UNREAD,
            Shown::Tab(Tab::All) => copy::NO_CHATS,
            Shown::Tab(Tab::Private) => copy::NO_PRIVATE_CHATS,
            Shown::Tab(Tab::Groups) => copy::NO_GROUPS,
            Shown::Tab(Tab::Forums) => copy::NO_FORUMS,
            Shown::Tab(Tab::Folder(_)) => copy::NO_FOLDER_CHATS,
            Shown::Archive => copy::NO_ARCHIVED,
        };
        centered()
            .child(quiet("empty-chats", empty, theme))
            .into_any_element()
    } else {
        let name = super::tabs::view_title(shown, list);
        let rows = uniform_list(list_id(list.view()), rows.len(), cx.processor(chat_rows))
            .track_scroll(&session.nav.scroll(list.view()))
            .flex_1();
        let fallback = session.nav.sidebar_list.get_or_init(|| cx.focus_handle());
        a11y::list("chat-list", copy::chat_list_label(&name))
            .track_focus(fallback)
            .tab_index(0)
            .tab_stop(false)
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(rows)
            .into_any_element()
    }
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

/// The sidebar's heading: the wordmark, or in the archive the way back to
/// the main list.
fn sidebar_title(shown: Shown, theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
    match shown {
        Shown::Tab(_) => a11y::heading_for("sidebar-heading", copy::CHATS_HEADING, 2)
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
        Shown::Archive => back_button(theme, cx).into_any_element(),
    }
}

/// Leaves the archive for All.
fn back_button(theme: &Theme, cx: &mut Context<'_, Root>) -> Stateful<Div> {
    let button = a11y::control("archive-back", Role::Button, copy::ARCHIVE_BACK_LABEL)
        .font_family(theme::NAME_FONT)
        .text_lg()
        .cursor_pointer()
        .child(copy::ARCHIVE_BACK);
    let button = a11y::on_activate_in(button, cx, |root, window, cx| {
        root.with_session(|s, cx| s.show_chats(Shown::default(), window, cx), cx);
    });
    a11y::focus_ring(button, theme.text)
}

/// Opens settings; also in the app menu and on Cmd+comma.
fn appearance_button(theme: &Theme, cx: &mut Context<'_, Root>) -> Stateful<Div> {
    let button = a11y::control("appearance", Role::Button, crate::ui::copy::SETTINGS)
        .text_xs()
        .cursor_pointer()
        .child(crate::ui::copy::SETTINGS);
    let button = a11y::on_activate_in(button, cx, |root, window, cx| {
        root.toggle_appearance(window, cx);
    });
    a11y::focus_ring(button, theme.text)
}

/// Quiet text a screen reader reads as it is, under a stable identity.
pub(super) fn quiet(id: impl Into<ElementId>, text: impl Into<SharedString>, theme: &Theme) -> Div {
    div()
        .opacity(theme.quiet_opacity())
        .child(a11y::text(id, text.into()))
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
    let nav = &session.nav;
    let marked = shell.marked();
    let rows = shell.list.rows();
    let visible = rows.get(range.clone()).unwrap_or_default();
    let keys: Vec<_> = visible.iter().filter_map(|&r| RowKey::of(r)).collect();
    nav.sidebar.keep(&keys);
    visible
        .iter()
        .zip(range)
        .map(|(&row, ix)| {
            let selected = marked == Some(row);
            let Some(key) = RowKey::of(row) else {
                return pending_row(shell, row, (&tz, now), theme, cx);
            };
            let handle = nav.sidebar.get(key, cx);
            if nav.take_pending(key) {
                let wanted = handle.clone();
                window.defer(cx, move |window, cx| wanted.focus(window, cx));
            }
            match row {
                SidebarRow::Archive(entry) => archive_row(entry, &handle, window, theme, cx),
                SidebarRow::Chat(id) => {
                    let Some(chat) = shell.list.get(id) else {
                        return empty_row(ix);
                    };
                    let row = ChatRow {
                        chat,
                        selected,
                        inline: shell.inline_of(id),
                        time: format::list_time(chat.last_date, now, &tz),
                        now,
                        depth: depth_of(chat.last_date, now, &tz),
                        handle,
                    };
                    row.render(window, theme, cx).when(selected, selected_row)
                }
                SidebarRow::Topic { topic, .. } => {
                    let Some(topic) = shell.topics().and_then(|l| l.get(topic)) else {
                        return empty_row(ix);
                    };
                    let focus = RowFocus {
                        handle: &handle,
                        window,
                        selected,
                    };
                    inline_topic(topic, (&tz, now), &focus, theme, cx)
                }
                SidebarRow::MoreTopics { forum } => {
                    let depth = forum_depth(shell, forum, now, &tz);
                    more_row(depth, &handle, window, theme, cx)
                }
                SidebarRow::TopicsPending { .. } => pending_row(shell, row, (&tz, now), theme, cx),
            }
        })
        .collect()
}

/// A topic of the pane's forum under the forum's row.
fn inline_topic(
    topic: &Topic,
    clock: (&TimeZone, i64),
    focus: &RowFocus<'_>,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> Stateful<Div> {
    let (tz, now) = clock;
    let id = ("inline-topic", u64::from(topic.id.get().cast_unsigned()));
    let depth = depth_of(topics::activity(topic), now, tz);
    let mark = if focus.selected {
        theme.row_mark(depth).into()
    } else {
        transparent_black()
    };
    let row = topic_row(topic, id.into(), clock, focus, theme, cx);
    inline_band(row, theme.text_on(depth), mark).when(focus.selected, selected_row)
}

/// Marks the selected row for tests, which look it up by this name.
fn selected_row(row: Stateful<Div>) -> Stateful<Div> {
    row.debug_selector(|| "selected-row".to_owned())
}

/// A forum's rows under it while its topics load: not a control, only what
/// is happening.
fn pending_row(
    shell: &ChatShell,
    row: SidebarRow,
    (tz, now): (&TimeZone, i64),
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> Stateful<Div> {
    let SidebarRow::TopicsPending { forum } = row else {
        return empty_row(0);
    };
    let depth = forum_depth(shell, forum, now, tz);
    let countdown = shell
        .topics()
        .and_then(|l| l.countdown(cx.background_executor().now()));
    let text = super::topic_view::placeholder(shell, countdown);
    let band = div()
        .id("topics-pending")
        .child(quiet("topics-status", text, theme).text_xs());
    inline_band(band, theme.text_on(depth), transparent_black()).bg(theme.surface(depth))
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
/// width, with a chat row's selection rule (`mark`: the theme's `row_mark`, or nothing).
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
fn more_row(
    depth: Depth,
    handle: &FocusHandle,
    window: &Window,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> Stateful<Div> {
    let ink = theme.text_on(depth);
    let band = a11y::tracked("more-topics", Role::Button, copy::MORE_TOPICS, handle)
        .cursor_pointer()
        .child(quiet("more-label", copy::MORE_TOPICS, theme).text_xs())
        .children(a11y::ring(handle, window, ink));
    let band = a11y::on_activate_in(band, cx, |root, window, cx| {
        root.with_session(|s, cx| s.more_topics(window, cx), cx);
    });
    inline_band(band, ink, transparent_black()).bg(theme.surface(depth))
}

/// The main list's entry for the archive: opens it in the sidebar.
fn archive_row(
    entry: ArchiveEntry,
    handle: &FocusHandle,
    window: &Window,
    theme: &Theme,
    cx: &mut Context<'_, Root>,
) -> Stateful<Div> {
    let (surface, ink) = (theme.surface(Depth::NOW), theme.text_on(Depth::NOW));
    let label = copy::archive_row_label(entry.chats, entry.unread);
    let row = a11y::row("archive-row", label, handle)
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
        .cursor_pointer();
    a11y::on_activate_in(row, cx, |root, window, cx| {
        root.with_session(|s, cx| s.show_chats(Shown::Archive, window, cx), cx);
    })
    .child(
        div()
            .size(px(36.))
            .flex_none()
            .rounded_full()
            .border_1()
            .border_color(theme.rule(ink, 0.4)),
    )
    .child(
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(div().font_family(theme::NAME_FONT).child(copy::ARCHIVE))
            .child(
                muted(copy::archive_summary(entry.chats, entry.unread), theme)
                    .text_xs()
                    .truncate(),
            ),
    )
    .children(a11y::ring(handle, window, theme.focus_on(Depth::NOW)))
}

struct ChatRow<'a> {
    chat: &'a ChatSummary,
    selected: bool,
    /// The pane's forum's expansion, shown by its disclosure glyph.
    inline: Option<Inline>,
    time: SharedString,
    now: i64,
    /// How far the chat has sunk: its surface darkens with its age.
    depth: Depth,
    handle: FocusHandle,
}

/// A chat's row id, stable while the list moves under it.
fn chat_row_id(id: ChatId) -> ElementId {
    let name = match id.kind {
        PeerKind::User => "chat-user",
        PeerKind::Chat => "chat-group",
        PeerKind::Channel => "chat-channel",
    };
    ElementId::NamedInteger(name.into(), id.id.cast_unsigned())
}

impl ChatRow<'_> {
    fn render(self, window: &Window, theme: &Theme, cx: &mut Context<'_, Root>) -> Stateful<Div> {
        let chat = self.chat;
        let id = chat.id;
        let (surface, ink) = (theme.surface(self.depth), theme.text_on(self.depth));
        let mark: Hsla = if self.selected {
            theme.row_mark(self.depth).into()
        } else {
            transparent_black()
        };
        let preview_text = chat_list::preview_text(chat.last.as_ref());
        let label = copy::chat_row_label(chat, &preview_text, &self.time, self.now);
        let tag = copy::kind_label(chat.kind).map(|t| muted(t, theme).text_xs().flex_none());
        let title = div()
            .flex()
            .gap_2()
            .children(self.inline.map(|inline| disclosure(inline, ink, cx)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(theme::NAME_FONT)
                    .child(chat.title.clone()),
            )
            .children(tag)
            .child(muted(self.time, theme).text_xs().flex_none());
        let preview = div()
            .flex()
            .gap_2()
            .child(
                muted(preview_text, theme)
                    .text_xs()
                    .flex_1()
                    .min_w_0()
                    .truncate(),
            )
            .children(unread_badge(chat, theme.badge(self.depth)));
        let row = a11y::row(chat_row_id(id), label, &self.handle)
            .aria_selected(self.selected)
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
            .cursor_pointer();
        let row = a11y::on_activate_in(row, cx, move |root, window, cx| {
            root.with_session(|s, cx| s.select_chat(id, window, cx), cx);
        })
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
        .children(a11y::ring(&self.handle, window, theme.focus_on(self.depth)));
        let root = cx.weak_entity();
        super::folder_menu::trigger(row, &chat.title, &self.handle, move |at, window, cx| {
            root.update(cx, |root, cx| {
                root.with_session(|s, cx| s.open_chat_menu(id, at, window, cx), cx);
            })
            .ok();
        })
    }
}

/// The pane's forum's disclosure glyph: "▾" while its topics show under
/// it, "▸" while collapsed (no state by hue alone, brand rule 2). A click
/// toggles them and stops there, so the row's own click doesn't select.
fn disclosure(inline: Inline, ink: Rgba, cx: &mut Context<'_, Root>) -> Stateful<Div> {
    let (glyph, label) = match inline {
        Inline::Expanded => (copy::EXPANDED, copy::COLLAPSE_TOPICS),
        Inline::Collapsed => (copy::COLLAPSED, copy::EXPAND_TOPICS),
    };
    let button = a11y::control("forum-disclosure", Role::Button, label)
        .aria_expanded(inline == Inline::Expanded)
        .flex_none()
        .cursor_pointer()
        .child(glyph)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
    let button = a11y::on_activate_in(button, cx, move |root, _, cx| {
        cx.stop_propagation();
        root.with_session(|s, cx| s.set_inline(inline.toggled(), cx), cx);
    });
    a11y::focus_ring(button, ink)
}

/// A chat row's unread count, as displayed (core's read overlay applied).
fn unread_badge(chat: &ChatSummary, badge: (Rgba, Rgba)) -> Option<Div> {
    let unread = chat.displayed_unread();
    (unread > 0).then(|| count_badge(unread, chat.unread_mentions, badge))
}

/// An unread count, marked when it holds mentions, in a `(fill, ink)` pair
/// from [`Theme::badge`].
pub(super) fn count_badge(unread: u32, mentions: u32, (fill, ink): (Rgba, Rgba)) -> Div {
    let mention = if mentions > 0 { "@ " } else { "" };
    div()
        .px_2()
        .flex_none()
        .rounded_full()
        .bg(fill)
        .text_color(ink)
        .text_xs()
        .child(format!("{mention}{}", copy::unread_badge(unread)))
}

/// Initials on a faint disc of the row's text color (no images in Stage 1).
fn avatar(chat: &ChatSummary, ink: Rgba) -> Div {
    div()
        .size(px(AVATAR_SIZE))
        .flex_none()
        .rounded_full()
        .bg(theme::faded(ink, theme::DISC))
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
            .child(quiet("no-chat", copy::SELECT_CHAT, theme))
            .into_any_element();
    };
    let header = match session.shell.pane() {
        Pane::Forum { open: None, .. } => {
            return super::topic_view::topic_pane(session, chat, theme, cx);
        }
        Pane::Forum {
            open: Some(topic), ..
        } => super::topic_view::topic_header(session, chat, topic, theme, cx).into_any_element(),
        Pane::Empty | Pane::Thread(_) => pane_header(chat, theme).into_any_element(),
    };
    let window = session.shell.window();
    // Above the oldest loaded message the pane keeps that message's tone.
    let oldest = session.shell.row(0).map_or(Depth::NOW, |r| {
        depth_of(r.date(), format::now(), &format::local_tz())
    });
    let body = match placeholder(&session.shell) {
        None => history(session, theme.text_on(oldest), cx),
        Some(text) => centered()
            .child(quiet("history-status", text, theme))
            .into_any_element(),
    };
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
        .child(composer_area(session, theme, cx))
        // After the pane's content, outside anything that clips, so a screen
        // reader finds it.
        .children(
            session
                .shell
                .announcement()
                .map(|a| a11y::polite(("new-message", a.seq), a.text.clone())),
        )
        .into_any_element()
}

/// How far a key scrolls the history, in points.
const SCROLL_LINE: f32 = 48.;

/// The messages: one list and one tab stop, scrolled by Up, Down, Page Up,
/// Page Down, Home and End, so a keyboard reaches all of the loaded history.
fn history(session: &Session, ink: Rgba, cx: &mut Context<'_, Root>) -> AnyElement {
    let focus = session.nav.history(cx);
    let thread = thread_name(session);
    let rows = list(session.shell.rows().clone(), cx.processor(message_row)).size_full();
    let history = a11y::list("history", copy::history_label(&thread))
        .track_focus(&focus)
        .size_full()
        .on_key_down(cx.listener(|root, event: &KeyDownEvent, _, cx| {
            let scrolled = root.with_session(
                |s, _| scroll_history(&s.shell.rows().clone(), &event.keystroke.key),
                cx,
            );
            if scrolled == Some(true) {
                cx.stop_propagation();
                cx.notify();
            }
        }))
        .child(rows);
    a11y::focus_ring(history, ink)
        .size_full()
        .into_any_element()
}

/// Scrolls the message list for `key`; whether the key was one of its own.
fn scroll_history(rows: &ListState, key: &str) -> bool {
    let page = (rows.viewport_bounds().size.height - px(SCROLL_LINE)).max(px(SCROLL_LINE));
    match key {
        "up" => rows.scroll_by(px(-SCROLL_LINE)),
        "down" => rows.scroll_by(px(SCROLL_LINE)),
        "pageup" => rows.scroll_by(-page),
        "pagedown" => rows.scroll_by(page),
        "home" => rows.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        }),
        "end" => rows.scroll_to_end(),
        _ => return false,
    }
    true
}

/// The open thread's name: the chat's, and the topic's inside a forum.
fn thread_name(session: &Session) -> String {
    let Some(chat) = session.shell.selected_chat() else {
        return String::new();
    };
    match session.shell.pane() {
        Pane::Forum {
            open: Some(topic), ..
        } => {
            let shown = session.shell.shown_topic();
            let title = shown.map_or_else(|| topics::fallback_title(topic), topics::title);
            format!("{}, {title}", chat.title)
        }
        Pane::Empty | Pane::Thread(_) | Pane::Forum { open: None, .. } => chat.title.clone(),
    }
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

/// The chat's title and kind label, as a heading.
pub(super) fn pane_header(chat: &ChatSummary, theme: &Theme) -> Stateful<Div> {
    let kind = copy::kind_label(chat.kind);
    let label = a11y::compose([chat.title.as_str(), kind.unwrap_or_default()]);
    a11y::heading_for("chat-heading", label, 1)
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
        .children(kind.map(|t| muted(t, theme).text_xs()))
}

fn jump_button(theme: &Theme, cx: &mut Context<'_, Root>) -> Stateful<Div> {
    let button = floating(
        pill(
            "jump-to-latest",
            copy::JUMP_TO_LATEST,
            theme.accent,
            theme.on_accent,
        ),
        theme,
    )
    .absolute()
    .bottom_3()
    .right_3();
    a11y::on_activate_in(button, cx, |root, window, cx| {
        root.with_session(|s, cx| s.jump_to_latest(window, cx), cx);
    })
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
    let button = floating(pill(id, label, theme.accent, theme.on_accent), theme)
        .debug_selector(|| id.to_owned())
        .absolute()
        .left_3()
        .occlude();
    a11y::on_activate_in(button, cx, |root, window, cx| {
        cx.stop_propagation();
        root.with_session(|s, cx| s.retry_loads(window, cx), cx);
    })
}

/// A control that floats over panes of any depth and over messages: a ring
/// in `on_accent` keeps its edge showing over whatever is beneath, where the
/// accent alone might match it (colourways plan, section 2).
fn floating(control: Stateful<Div>, theme: &Theme) -> Stateful<Div> {
    control.border_2().border_color(theme.on_accent)
}

/// A small action in `ink` on `fill`, a button. Focused, it swaps the two:
/// the ring a floating pill already wears at its edge leaves no room for
/// another, and the swap is no colour change alone but a filled shape.
pub(super) fn pill(
    id: impl Into<ElementId>,
    label: &'static str,
    fill: Rgba,
    ink: Rgba,
) -> Stateful<Div> {
    a11y::control(id, Role::Button, label)
        .px_3()
        .py_1()
        .rounded_full()
        .border_2()
        .border_color(transparent_black())
        .bg(fill)
        .text_color(ink)
        .text_sm()
        .cursor_pointer()
        .focus(move |style| style.bg(ink).text_color(fill).border_color(fill))
        .child(label)
}

/// The message box, or why there is none: a read-only chat, a closed
/// topic. Echoes above it still show and can be discarded.
fn composer_area(session: &Session, theme: &Theme, cx: &mut Context<'_, Root>) -> Div {
    let area = div().p_3();
    match session.shell.composer() {
        Composer::Open => {
            let root = cx.weak_entity();
            let send = a11y::hidden_action("send", copy::SEND_MESSAGE, move |window, cx| {
                root.update(cx, |root, cx| {
                    root.with_session(|s, cx| s.submit_message(window, cx), cx);
                })
                .ok();
            });
            area.child(session.composer.clone()).child(send)
        }
        Composer::ReadOnly => area.child(blocked(copy::READ_ONLY, theme)),
        Composer::TopicClosed => area.child(blocked(copy::TOPIC_CLOSED, theme)),
    }
}

/// Why there is no message box, as text a screen reader finds in its place.
fn blocked(text: &'static str, theme: &Theme) -> Div {
    div()
        .opacity(theme.quiet_opacity())
        .text_sm()
        .child(a11y::text("composer-blocked", text))
}

/// The identities of a message row's parts, which must not change while a
/// message does not: a screen reader follows its place by them.
struct Ids {
    day: ElementId,
    item: ElementId,
}

impl Ids {
    fn of(row: &Row<'_>) -> Self {
        let (kind, n) = match row {
            Row::Message(m) => ("msg", u64::try_from(m.id.0).unwrap_or_default()),
            Row::Echo(e) => ("echo", e.local_id.0),
        };
        Self {
            day: ElementId::NamedInteger(format!("day-{kind}").into(), n),
            item: ElementId::NamedInteger(format!("item-{kind}").into(), n),
        }
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
    let chat = shell.selected_chat();
    let ctx = RowCtx {
        this: cx.weak_entity(),
        theme,
        tz: format::local_tz(),
        groupish: chat.is_some_and(|c| !matches!(c.kind, ChatKind::Private { .. } | ChatKind::Bot)),
        title: chat.map(|c| c.title.clone()).unwrap_or_default(),
    };
    let ids = Ids::of(&row);
    let prev_date = ix
        .checked_sub(1)
        .and_then(|p| shell.row(p))
        .map(|r| r.date());
    let separator = ctx.separator(&ids, row.date(), prev_date);
    let start = (ix == 0
        && shell
            .window()
            .is_some_and(|w| w.is_loaded() && !w.has_older()))
    .then(|| {
        day_separator(
            "start-of-history".into(),
            copy::START_OF_HISTORY.into(),
            &theme,
        )
    });
    // Each row carries the tone of its own age, so history darkens upward.
    let depth = depth_of(row.date(), format::now(), &ctx.tz);
    let divider = shell
        .divider_before(ix)
        .then(|| unread_divider(theme.text_on(depth), &theme));
    let content = match row {
        Row::Message(m) => {
            let reply = m
                .reply_to
                .and_then(|r| shell.window().and_then(|w| w.by_id(r)));
            ctx.message(ids.item, m, shell.spoilers(m.id), reply)
        }
        Row::Echo(e) => echo_row(ids.item, e, &theme, cx),
    };
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
        .children(divider)
        .child(content);
    // Painted bounds feed the after-paint check (mark read, live tail).
    shell.painted().track(ix, row).w_full().into_any_element()
}

fn day_separator(id: ElementId, label: SharedString, theme: &Theme) -> Div {
    div()
        .w_full()
        .py_2()
        .flex()
        .justify_center()
        .child(quiet(id, label, theme).text_xs())
}

/// The read position (plan section 17.5, decision D10): a full-width band
/// like the day separator, its label in quiet ink between hairline rules
/// of the row's ink. No colour (brand rule 2). A heading, so a screen reader's
/// heading navigation jumps to it.
fn unread_divider(ink: Rgba, theme: &Theme) -> Div {
    let rule = || div().flex_1().h(px(1.)).bg(theme.line(ink));
    let label = a11y::heading_for("unread-divider", copy::NEW_SINCE_READ, 3)
        .opacity(theme.quiet_opacity())
        .text_xs()
        .flex_none()
        .child(copy::NEW_SINCE_READ);
    div()
        .w_full()
        .py_2()
        .flex()
        .items_center()
        .gap_3()
        .child(rule())
        .child(label)
        .child(rule())
}

/// A message bubble, right-aligned for outgoing messages. A `header` (the
/// sender, in groups) sits on its own tinted band above the body, set off by
/// a hairline, so who spoke never blends into what was said. `item` is the
/// bubble's row in the list, named for a screen reader.
fn in_bubble(
    item: Stateful<Div>,
    side: Side,
    theme: &Theme,
    header: Option<AnyElement>,
    content: Vec<AnyElement>,
) -> AnyElement {
    let ink = theme.bubble_ink(side);
    let band = header.map(|h| {
        div()
            .w_full()
            .px_3()
            .pt_2()
            .pb_1p5()
            // GPUI clips children to a rectangle, not the bubble's rounded
            // corners, so the band rounds its own top to match.
            .rounded_t(px(theme::RADIUS))
            .bg(theme::faded(ink, theme::HEADER_WASH))
            .border_b_1()
            .border_color(theme.line(ink))
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
        .bg(theme.bubble(side))
        .text_color(ink)
        .flex()
        .flex_col()
        .children(band)
        .child(body);
    let row = item.w_full().flex();
    let row = if side == Side::Outgoing {
        row.justify_end()
    } else {
        row.justify_start()
    };
    row.child(inner).into_any_element()
}

/// What a media message is, shown as `[Photo]` and read as "Photo, not
/// shown": nothing of it is downloaded or drawn.
fn media_chip(kind: &MediaKind, ink: Rgba, theme: &Theme) -> Stateful<Div> {
    div()
        .id("media")
        .role(Role::Label)
        .aria_value(copy::media_not_shown(kind))
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(theme.line(ink))
        .text_sm()
        .child(format!("[{}]", copy::media_label(kind)))
}

/// A reply's quoted message, as the quote shows it.
fn quote_text(target: &Message) -> SharedString {
    match &target.body {
        Body::Text(_) => chat_list::preview_text(Some(&abyss_core::model::Preview::of(target))),
        Body::Media { kind, .. } => format!("[{}]", copy::media_label(kind)).into(),
        Body::Service(_) | Body::Unsupported => copy::SERVICE_FALLBACK.into(),
    }
}

fn reply_quote(text: SharedString, side: Side, theme: &Theme) -> Stateful<Div> {
    div()
        .id("reply")
        .role(Role::Label)
        .aria_value(format!("In reply to: {text}"))
        .pl_2()
        .border_l_2()
        .border_color(theme.bubble_ink(side))
        .text_xs()
        .opacity(theme.quiet_in(side))
        .truncate()
        .child(text)
}

/// A service message ("Alice joined"): centred text, with no bubble.
fn service_row(id: ElementId, text: &str, theme: &Theme) -> AnyElement {
    a11y::list_row(id, [text])
        .w_full()
        .flex()
        .justify_center()
        .child(muted(text.to_owned(), theme).text_sm().italic())
        .into_any_element()
}

/// Per-row rendering context.
struct RowCtx {
    this: WeakEntity<Root>,
    theme: Theme,
    tz: TimeZone,
    /// Groups show sender names.
    groupish: bool,
    /// The open chat's title, who an incoming message of a private chat is from.
    title: String,
}

impl RowCtx {
    /// A day separator when this row starts a new local day.
    fn separator(&self, ids: &Ids, date: i64, prev: Option<i64>) -> Option<Div> {
        let day = format::day(date, &self.tz)?;
        let today = format::day(format::now(), &self.tz)?;
        let prev = prev.and_then(|p| format::day(p, &self.tz));
        (prev != Some(day))
            .then(|| day_separator(ids.day.clone(), format::day_label(day, today), &self.theme))
    }

    /// Who a message is from, as it is read.
    fn sender<'a>(&'a self, m: &'a Message) -> &'a str {
        match (&m.sender, m.outgoing) {
            (_, true) => copy::YOU,
            (Some(sender), false) => &sender.name,
            (None, false) => &self.title,
        }
    }

    fn message(
        &self,
        id: ElementId,
        m: &Message,
        spoilers: Spoilers,
        reply: Option<&Message>,
    ) -> AnyElement {
        if let Body::Service(text) = &m.body {
            return service_row(id, text, &self.theme);
        }
        let side = if m.outgoing {
            Side::Outgoing
        } else {
            Side::Incoming
        };
        let time = format::time_of_day(m.date, &self.tz);
        let quote = reply.map(quote_text);
        let (spoken, body) = self.body(m, spoilers, side);
        let media = match &m.body {
            Body::Media { kind, .. } => Some(copy::media_not_shown(kind)),
            _ => None,
        };
        let parts: Vec<&str> = media
            .as_deref()
            .into_iter()
            .chain(spoken.iter().map(AsRef::as_ref))
            .collect();
        let sender = self.sender(m);
        let edited = if m.edited() { copy::EDITED } else { "" };
        let label = copy::message_label(sender, quote.as_deref(), &parts, [&time, edited]);
        let item = a11y::list_row(id, [label.as_str()]);
        let header = m
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
        let footer = if m.edited() {
            format!("{} · {time}", copy::EDITED)
        } else {
            time.to_string()
        };
        let mut content: Vec<AnyElement> = Vec::with_capacity(3);
        content.extend(quote.map(|q| reply_quote(q, side, &self.theme).into_any_element()));
        content.push(body);
        content.push(
            muted_in(footer, side, &self.theme)
                .text_xs()
                .into_any_element(),
        );
        in_bubble(item, side, &self.theme, header, content)
    }

    /// What a message says, as words for the screen reader and as it is drawn.
    fn body<'m>(
        &self,
        m: &'m Message,
        spoilers: Spoilers,
        side: Side,
    ) -> (Option<Cow<'m, str>>, AnyElement) {
        match &m.body {
            // Told apart before: a service message has no bubble.
            Body::Service(_) => (None, div().into_any_element()),
            Body::Text(rt) => {
                let (spoken, body) = self.rich(m.id, rt, spoilers, side);
                (Some(spoken), body)
            }
            Body::Media { kind, caption } => {
                let chip = media_chip(kind, self.theme.bubble_ink(side), &self.theme);
                let (spoken, caption) = if caption.text().is_empty() {
                    (None, None)
                } else {
                    let (spoken, caption) = self.rich(m.id, caption, spoilers, side);
                    (Some(spoken), Some(caption))
                };
                let body = div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(chip)
                    .children(caption);
                (spoken, body.into_any_element())
            }
            Body::Unsupported => (
                Some(Cow::Borrowed(copy::UNSUPPORTED)),
                div()
                    .opacity(self.theme.quiet_in(side))
                    .italic()
                    .child(a11y::text("unsupported", copy::UNSUPPORTED))
                    .into_any_element(),
            ),
        }
    }

    /// Formatted text: a link click copies its target (links never open), a
    /// spoiler click reveals the message's spoilers.
    fn rich<'m>(
        &self,
        id: MsgId,
        rt: &'m RichText,
        spoilers: Spoilers,
        side: Side,
    ) -> (Cow<'m, str>, AnyElement) {
        let this = self.this.clone();
        let element_id =
            ElementId::NamedInteger("msg".into(), u64::try_from(id.0).unwrap_or_default());
        let body = rich::style(rt, &fonts(), &palette(side, &self.theme), spoilers);
        let spoken = body
            .redacted_text()
            .map_or(Cow::Borrowed(rt.text()), Cow::Owned);
        let element = body.interactive(
            element_id,
            self.theme.bubble_ink(side),
            move |click, window, cx| {
                on_text_click(&this, id, click, window, cx);
            },
        );
        (spoken, element)
    }
}

/// An own send without a server copy yet: its state, and Retry and Discard
/// where core accepts them; text core refused offers Put back and Dismiss.
fn echo_row(id: ElementId, e: &Echo, theme: &Theme, cx: &mut Context<'_, Root>) -> AnyElement {
    let state = copy::send_label(e, Instant::now());
    let status = div()
        .flex()
        .gap_2()
        .items_center()
        .justify_end()
        .child(muted_in(state.clone(), Side::Outgoing, theme).text_xs())
        .children(echo_actions(e, theme, cx));
    let text = rich::plain(&e.text, &fonts(), &palette(Side::Outgoing, theme));
    let body = StyledText::new(text.text).with_runs(text.runs);
    let label = a11y::compose([copy::YOU, &e.text, &state]);
    in_bubble(
        a11y::list_row(id, [label.as_str()]),
        Side::Outgoing,
        theme,
        None,
        vec![body.into_any_element(), status.into_any_element()],
    )
}

/// The buttons of a send that core accepts a move for. They sit inside the
/// user's own bubble, so its colors are swapped. The message list scrolls, so
/// each registers its own AccessKit activation ([`a11y::on_activate_in`]).
fn echo_actions(e: &Echo, theme: &Theme, cx: &mut Context<'_, Root>) -> Vec<Stateful<Div>> {
    type Move = fn(&mut Session, LocalId, &mut Window, &mut Context<'_, Root>);
    let local_id = e.local_id;
    let (fill, ink) = (theme.on_own, theme.own);
    let mut actions = Vec::new();
    let mut push = |id: &'static str, label: &'static str, spoken: &'static str, run: Move| {
        let pill = pill((id, local_id.0), label, fill, ink).aria_label(spoken);
        actions.push(a11y::on_activate_in(pill, cx, move |root, window, cx| {
            root.with_session(|s, cx| run(s, local_id, window, cx), cx);
        }));
    };
    let dismiss: Move = |s, id, _, cx| s.dismiss_returned(id, cx);
    if e.can_retry() {
        push("retry", copy::RETRY, copy::RETRY_SEND, Session::retry_send);
    }
    if e.can_discard() {
        push(
            "discard",
            copy::DISCARD,
            copy::DISCARD_SEND,
            Session::discard_send,
        );
    }
    if e.is_returned() {
        push(
            "put-back",
            copy::PUT_BACK,
            copy::PUT_BACK_LABEL,
            Session::put_back,
        );
        push("dismiss", copy::DISMISS, copy::DISMISS_RETURNED, dismiss);
    } else if e.is_stranded() {
        push(
            "copy-text",
            copy::COPY_TEXT,
            copy::COPY_UNSENT,
            Session::copy_stranded,
        );
        push("dismiss", copy::DISMISS, copy::DISMISS_STRANDED, dismiss);
    }
    actions
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
