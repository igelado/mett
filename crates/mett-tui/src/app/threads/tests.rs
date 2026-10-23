use chrono::{DateTime, TimeDelta};
use mett_core::{ThreadPosition, mock};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use crate::app::testing::*;
use crate::app::{Msg, Settings};
use crate::worker::{Event, Request};
use mett_core::FlagsUpdate;

fn message(id: &str, minutes: i64, conversation: &str, path: &[u8]) -> MessageSummary {
    let mut m = mock::message(
        id,
        "in",
        &format!("Re: {conversation}"),
        DateTime::UNIX_EPOCH + TimeDelta::minutes(minutes),
    )
    .summary;
    m.thread = Some(ThreadPosition {
        conversation: conversation.into(),
        path: path.iter().map(|step| vec![*step]).collect(),
    });
    // Read: collapsing every thread leaves those with unread mail open.
    m.flags.read = true;
    m
}

/// An Inbox sorted by threads: a thread "a" (a0, with replies a1 and
/// a2) and a thread "b" (b0 and b1), listed newest first.
fn app() -> App {
    let messages = vec![
        message("b1", 5, "b", &[0, 1]),
        message("a2", 4, "a", &[0, 2]),
        message("b0", 3, "b", &[0]),
        message("a1", 2, "a", &[0, 1]),
        message("a0", 1, "a", &[0]),
    ];
    opened(sorted("threads"), 24, messages, None).0
}

fn shown(app: &App) -> Vec<String> {
    app.index
        .messages
        .iter()
        .map(|m| {
            let thread = app.thread_of(&m.id).unwrap();
            let folded = if thread.folded > 0 {
                format!(" ({})", thread.folded)
            } else {
                String::new()
            };
            format!("{}{}{folded}", thread.tree, m.id)
        })
        .collect()
}

#[test]
fn shows_threads_with_their_trees() {
    let app = app();
    assert_eq!(shown(&app), ["a0", "├─>a1", "└─>a2", "b0", "└─>b1"]);
    assert_eq!(app.index.selected, 0);
    assert!(app.thread_of(&MessageId::new("a1")).unwrap().same_subject);
}

#[test]
fn moves_between_threads() {
    let mut app = app();
    act(&mut app, Action::NextEntry);
    act(&mut app, Action::NextThread);
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "b0");
    act(&mut app, Action::NextThread);
    assert_eq!(app.status, "You are on the last thread.");
    act(&mut app, Action::NextEntry);
    act(&mut app, Action::PreviousThread);
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "a0");
    act(&mut app, Action::PreviousThread);
    assert_eq!(app.status, "You are on the first thread.");
}

#[test]
fn collapses_threads() {
    let mut app = app();
    act(&mut app, Action::NextEntry);
    act(&mut app, Action::CollapseThread);
    assert_eq!(shown(&app), ["a0 (2)", "b0", "└─>b1"]);
    assert_eq!(app.index.selected, 0);
    assert_eq!(app.total_messages(), 5);
    // The key: Escape then v.
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('v'),
        KeyModifiers::ALT,
    )));
    assert_eq!(shown(&app), ["a0", "├─>a1", "└─>a2", "b0", "└─>b1"]);
    act(&mut app, Action::CollapseAll);
    assert_eq!(shown(&app), ["a0 (2)", "b0 (1)"]);
    act(&mut app, Action::CollapseAll);
    assert_eq!(shown(&app).len(), 5);
}

/// [`app`], with `unread` unread.
fn app_with_unread(unread: &[&str]) -> App {
    let mut app = app();
    for m in app.index.messages.iter_mut() {
        m.flags.read = !unread.contains(&m.id.as_str());
    }
    app.arrange();
    app
}

/// Selects the entry of `id`.
fn select(app: &mut App, id: &str) {
    app.index.selected = app
        .index
        .messages
        .iter()
        .position(|m| m.id.as_str() == id)
        .unwrap();
}

#[test]
fn collapsing_every_thread_leaves_those_with_unread_mail_open() {
    let mut app = app_with_unread(&["a1"]);
    act(&mut app, Action::CollapseAll);
    assert_eq!(shown(&app), ["a0", "├─>a1", "└─>a2", "b0 (1)"]);
    // Again: every thread opens.
    act(&mut app, Action::CollapseAll);
    assert_eq!(shown(&app).len(), 5);
    // One the user collapses stays collapsed, unread or not.
    act(&mut app, Action::CollapseAll);
    select(&mut app, "a0");
    act(&mut app, Action::CollapseThread);
    assert_eq!(shown(&app), ["a0 (2)", "b0 (1)"]);
}

#[test]
fn toggle_new_reads_a_collapsed_thread_whole() {
    let mut app = app_with_unread(&["a1", "a2"]);
    select(&mut app, "a0");
    act(&mut app, Action::CollapseThread);
    assert_eq!(shown(&app)[0], "a0 (2)");
    // Both are read at once; the changes go to the mailbox one by one.
    let (first, request) = sent(&act(&mut app, Action::ToggleNew));
    assert_eq!(
        request,
        Request::UpdateFlags("a1".into(), FlagsUpdate::read(true))
    );
    assert!(
        app.roots_with_folded_unread().is_empty(),
        "no longer unread"
    );
    let (second, request) = sent(&respond(&mut app, first, Event::FlagsUpdated(Ok(()))));
    assert_eq!(
        request,
        Request::UpdateFlags("a2".into(), FlagsUpdate::read(true))
    );
    respond(&mut app, second, Event::FlagsUpdated(Ok(())));
    // Read now, the thread's first message is marked unread, as usual.
    let (_, request) = sent(&act(&mut app, Action::ToggleNew));
    assert_eq!(
        request,
        Request::UpdateFlags("a0".into(), FlagsUpdate::read(false))
    );
}

#[test]
fn opening_a_collapsed_thread_opens_its_first_unread_message() {
    let mut app = app_with_unread(&["a2"]);
    select(&mut app, "a0");
    act(&mut app, Action::CollapseThread);
    let (_, request) = sent(&act(&mut app, Action::DisplayMessage));
    assert_eq!(request, Request::FetchMessage("a2".into()));
    assert_eq!(shown(&app), ["a0", "├─>a1", "└─>a2", "b0", "└─>b1"]);
    // Its first message unread, that is its first unread message.
    let mut app = app_with_unread(&["a0", "a2"]);
    select(&mut app, "a0");
    act(&mut app, Action::CollapseThread);
    let (_, request) = sent(&act(&mut app, Action::DisplayMessage));
    assert_eq!(request, Request::FetchMessage("a0".into()));
    // Without unread mail, it opens at its first message, collapsed.
    let mut app = app_with_unread(&[]);
    select(&mut app, "a0");
    act(&mut app, Action::CollapseThread);
    let (_, request) = sent(&act(&mut app, Action::DisplayMessage));
    assert_eq!(request, Request::FetchMessage("a0".into()));
}

#[test]
fn new_mail_opens_its_collapsed_thread() {
    let mut app = app();
    select(&mut app, "a0");
    act(&mut app, Action::CollapseThread);
    assert_eq!(shown(&app)[0], "a0 (2)");
    // Read mail (as from mett itself) leaves it as it is.
    let mut read = message("a3", 6, "a", &[0, 3]);
    app.open_for_new_mail(std::slice::from_ref(&read));
    app.arrange();
    assert_eq!(shown(&app)[0], "a0 (2)");
    read.flags.read = false;
    app.open_for_new_mail(std::slice::from_ref(&read));
    app.arrange();
    assert_eq!(shown(&app)[..3], ["a0", "├─>a1", "└─>a2"]);
    // It stays open once the mail is read.
    for m in app.index.messages.iter_mut() {
        m.flags.read = true;
    }
    app.arrange();
    assert_eq!(shown(&app)[..3], ["a0", "├─>a1", "└─>a2"]);
    // So does one collapsing every thread had collapsed.
    let mut app = app_with_unread(&[]);
    act(&mut app, Action::CollapseAll);
    assert!(app.open_for_new_mail(&[read]));
    app.arrange();
    assert_eq!(shown(&app)[..3], ["a0", "├─>a1", "└─>a2"]);
}

#[test]
fn collapsing_a_thread_with_nothing_to_fold_can_be_undone() {
    // A thread of one message (its replies not listed yet, say).
    let messages = vec![message("c0", 1, "c", &[0])];
    let mut app = opened(sorted("threads"), 24, messages, None).0;
    let key = "conversation c";
    act(&mut app, Action::CollapseThread);
    assert_eq!(app.index.chosen.get(key), Some(&true));
    act(&mut app, Action::CollapseThread);
    assert_eq!(app.index.chosen.get(key), Some(&false), "undone");
}

#[test]
fn a_search_opens_the_collapsed_thread_it_finds() {
    let mut app = app();
    let a2 = app
        .index
        .messages
        .iter_mut()
        .find(|m| m.id.as_str() == "a2")
        .unwrap();
    a2.subject = "Re: a, special".into();
    act(&mut app, Action::CollapseAll);
    // a2 is folded into a0's thread.
    app.index
        .messages
        .iter()
        .for_each(|m| assert_ne!(m.id.as_str(), "a2"));
    app.index.selected = 1;
    app.new_search("~s special");
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "a2");
    // When the start of the thread matches too, the thread opens, and
    // `n` goes on to its other matches.
    act(&mut app, Action::CollapseAll);
    app.index.selected = 1;
    app.new_search("~s a");
    assert_eq!(shown(&app)[0], "a0");
    assert!(shown(&app).len() > 2);
    assert_eq!(shown(&app), ["a0", "├─>a1", "└─>a2", "b0 (1)"]);
}

#[test]
fn escape_then_v_collapses_too() {
    let mut app = app();
    key(&mut app, KeyCode::Esc);
    key(&mut app, KeyCode::Char('v'));
    assert_eq!(shown(&app)[0], "a0 (2)");
}

#[test]
fn an_escape_long_ago_leaves_the_next_key_alone() {
    let mut app = app();
    key(&mut app, KeyCode::Esc);
    if let Some((_, when)) = &mut app.escape
        && let Some(earlier) = when.checked_sub(std::time::Duration::from_secs(5))
    {
        *when = earlier;
    }
    key(&mut app, KeyCode::Char('v'));
    // `v`, not `Esc v`: the thread isn't collapsed.
    assert_eq!(shown(&app)[0], "a0");
}

#[test]
fn escape_then_control_n_is_control_n() {
    let mut app = app();
    key(&mut app, KeyCode::Esc);
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('n'),
        KeyModifiers::CONTROL,
    )));
    // The next thread, not Alt-n.
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "b0");
}

#[test]
fn an_escape_is_forgotten_when_another_menu_takes_over() {
    let mut app = app();
    key(&mut app, KeyCode::Esc);
    // The help screen opens before the next key (as the sign-in window
    // may): its `q` still leaves it.
    act(&mut app, Action::Help);
    key(&mut app, KeyCode::Char('q'));
    assert_eq!(app.screen(), &Screen::Index);
}

#[test]
fn a_collapsed_thread_stays_selected_when_its_first_message_goes() {
    let mut app = app();
    act(&mut app, Action::CollapseThread);
    assert_eq!(shown(&app)[0], "a0 (2)");
    app.remove_messages(&std::collections::HashSet::from([MessageId::new("a0")]));
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "a1");
}

#[test]
fn a_pending_page_down_counts_from_where_it_started() {
    let (mut app, list) = opening(sorted("subject"), 5);
    let next = page(&mut app, list, vec![named("a"), named("z")], "in next");
    // Two rows a page, from a.
    act(&mut app, Action::NextPage);
    listed(&mut app, next, vec![named("b"), named("zz")], None);
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "z");
}

#[test]
fn a_pending_move_waits_for_mail_from_the_same_time() {
    let settings = Settings {
        ordering: Ordering {
            sort: Sort::parse("reverse-date", true).unwrap(),
            aux: Sort::parse("from", false).unwrap(),
            ..Ordering::default()
        },
        ..Settings::default()
    };
    let (mut app, list) = opening(settings, 24);
    let from = |id: &str, minutes: i64| {
        let mut m = mock::message(
            id,
            "in",
            id,
            DateTime::UNIX_EPOCH + TimeDelta::minutes(minutes),
        )
        .summary;
        m.from = Some(mett_core::Address::new(format!("{id}@x.org")));
        m
    };
    let next = page(&mut app, list, vec![from("c", 5)], "in next");
    act(&mut app, Action::NextEntry);
    // e may not come right after c: d, from the same time, may follow.
    let next = page(&mut app, next, vec![from("e", 3)], "in next");
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "c");
    listed(&mut app, next, vec![from("d", 3)], None);
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "d");
}

#[test]
fn a_pending_move_waits_among_drafts() {
    let (mut app, list) = opening(sorted("reverse-date"), 24);
    let at = |id: &str, minutes: i64, draft: bool| {
        let mut m = mock::message(
            id,
            "in",
            id,
            DateTime::UNIX_EPOCH + TimeDelta::minutes(minutes),
        )
        .summary;
        m.draft = draft;
        m
    };
    // Drafts first, then the rest, each by its own date.
    let next = page(&mut app, list, vec![at("d9", 9, true)], "in next");
    act(&mut app, Action::NextEntry);
    let next = page(&mut app, next, vec![at("d2", 2, true)], "in next");
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "d9");
    listed(&mut app, next, vec![at("s5", 5, false)], None);
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "s5");
}

#[test]
fn a_collapsed_thread_tells_of_its_unread_mail() {
    let mut app = app();
    for m in &mut app.index.messages {
        m.flags.read = m.id.as_str() != "a2";
    }
    act(&mut app, Action::CollapseThread);
    let a0 = MessageId::new("a0");
    assert_eq!(app.roots_with_folded_unread(), HashSet::from([&a0]));
    // As the flags are now.
    let a2 = app
        .index
        .folded
        .iter_mut()
        .find(|m| m.id.as_str() == "a2")
        .unwrap();
    a2.flags.read = true;
    assert!(app.roots_with_folded_unread().is_empty());
}

#[test]
fn a_search_looks_under_the_selected_collapsed_thread_first() {
    let mut app = app();
    let a1 = app
        .index
        .messages
        .iter_mut()
        .find(|m| m.id.as_str() == "a1")
        .unwrap();
    a1.subject = "Re: a, found".into();
    let b1 = app
        .index
        .messages
        .iter_mut()
        .find(|m| m.id.as_str() == "b1")
        .unwrap();
    b1.subject = "Re: b, found".into();
    act(&mut app, Action::CollapseAll);
    app.index.selected = 0;
    app.new_search("~s found");
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "a1");
}

#[test]
fn a_limit_works_on_collapsed_threads() {
    let mut app = app();
    act(&mut app, Action::CollapseAll);
    app.set_limit("~s b");
    assert_eq!(shown(&app), ["b0 (1)"]);
    // The limit shows both, one folded.
    assert_eq!(app.shown_count(), 2);
    app.set_limit("all");
    assert_eq!(shown(&app), ["a0 (2)", "b0 (1)"]);
}

/// An app sorted by threads whose first page is `first`, with more to
/// come; returns the request for the next page.
fn paged(first: Vec<MessageSummary>) -> (App, crate::worker::RequestId) {
    let (app, next) = opened(sorted("threads"), 24, first, Some("in next"));
    (app, next.unwrap())
}

#[test]
fn pages_are_merged_into_a_sorted_index() {
    let (mut app, next) = paged(vec![
        message("x4", 4, "x", &[0]),
        message("x2", 2, "x", &[0, 1]),
    ]);
    app.apply_ordering(Ordering {
        sort: Sort::parse("date", true).unwrap(),
        ..Ordering::default()
    });
    app.index.selected = 1;
    listed(
        &mut app,
        next,
        vec![
            message("x5", 5, "x", &[0, 2]),
            message("x1", 1, "x", &[0, 3]),
            message("x3", 3, "x", &[0, 4]),
        ],
        None,
    );
    let ids: Vec<&str> = app.index.messages.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["x1", "x2", "x3", "x4", "x5"]);
    // Still on x4.
    assert_eq!(app.index.selected, 3);
}

#[test]
fn threads_are_built_as_the_index_doubles_and_once_it_is_listed() {
    let (mut app, next) = paged(vec![
        message("a2", 4, "a", &[0, 2]),
        message("a1", 2, "a", &[0, 1]),
    ]);
    let more = page(&mut app, next, vec![message("b0", 3, "b", &[0])], "in more");
    // Three of the two threaded so far: listed, not threaded yet…
    assert!(app.thread_of(&MessageId::new("b0")).is_none());
    // …until a thread action needs them.
    act(&mut app, Action::NextThread);
    assert!(app.thread_of(&MessageId::new("b0")).is_some());
    listed(&mut app, more, vec![message("a0", 1, "a", &[0])], None);
    assert_eq!(shown(&app), ["a0", "├─>a1", "└─>a2", "b0"]);
}

#[test]
fn replies_listed_later_fold_into_their_collapsed_thread() {
    let (mut app, next) = paged(vec![
        message("a2", 4, "a", &[0, 2]),
        message("a1", 3, "a", &[0, 1]),
        message("a0", 2, "a", &[0]),
        message("b0", 1, "b", &[0]),
    ]);
    act(&mut app, Action::CollapseAll);
    // One more message: not enough to thread the index again.
    let effects = listed(
        &mut app,
        next,
        vec![message("b1", 0, "b", &[0, 1])],
        Some("in more"),
    );
    assert!(!effects.is_empty());
    assert!(!app.index.messages.iter().any(|m| m.id.as_str() == "b1"));
}

#[test]
fn collapsing_all_takes_in_threads_listed_later() {
    let (mut app, next) = paged(vec![message("a1", 2, "a", &[0])]);
    act(&mut app, Action::CollapseAll);
    listed(
        &mut app,
        next,
        vec![message("a2", 1, "a", &[0, 1]), message("b0", 0, "b", &[0])],
        None,
    );
    assert_eq!(shown(&app), ["b0", "a1 (1)"]);
    // And all open again.
    act(&mut app, Action::CollapseAll);
    assert_eq!(shown(&app).len(), 3);
}

#[test]
fn a_collapsed_thread_stays_collapsed_when_its_start_is_listed() {
    let (mut app, next) = paged(vec![
        message("a2", 4, "a", &[0, 2]),
        message("a1", 2, "a", &[0, 1]),
    ]);
    act(&mut app, Action::CollapseThread);
    assert_eq!(shown(&app), ["a1 (1)"]);
    listed(&mut app, next, vec![message("a0", 1, "a", &[0])], None);
    assert_eq!(shown(&app), ["a0 (2)"]);
}

#[test]
fn the_next_thread_may_be_on_a_page_to_come() {
    let (mut app, next) = paged(vec![
        message("a1", 4, "a", &[0, 1]),
        message("a0", 3, "a", &[0]),
    ]);
    act(&mut app, Action::NextThread);
    assert_ne!(app.status, "You are on the last thread.");
    // (A later page normally has older mail; this thread sorts after.)
    listed(&mut app, next, vec![message("b0", 9, "b", &[0])], None);
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "b0");
}

#[test]
fn a_pending_next_entry_goes_after_its_message_whatever_comes_before() {
    let (mut app, list) = opening(sorted("subject"), 24);
    let next = page(&mut app, list, vec![named("b")], "in next");
    act(&mut app, Action::NextEntry);
    // d comes first, but the last page may hold what goes between.
    let more = page(&mut app, next, vec![named("d")], "in more");
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "b");
    listed(&mut app, more, vec![named("a"), named("c")], None);
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "c");
}

#[test]
fn a_pending_next_entry_survives_its_message_going() {
    let (mut app, list) = opening(sorted("subject"), 24);
    let next = page(&mut app, list, vec![named("a"), named("b")], "in next");
    act(&mut app, Action::LastEntry);
    act(&mut app, Action::NextEntry);
    // b goes (moved elsewhere, say) before the next page comes.
    app.remove_messages(&std::collections::HashSet::from([MessageId::new("b")]));
    listed(&mut app, next, vec![named("c"), named("d")], None);
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "c");
}

#[test]
fn a_pending_move_from_a_folded_message_goes_after_its_thread() {
    let mut app = app();
    act(&mut app, Action::CollapseAll);
    app.index.pending = Some(super::super::Target::After(MessageId::new("a1"), 1));
    app.apply_pending();
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "b0");
}

#[test]
fn a_limit_keeps_a_visible_reply_selected() {
    let mut app = app();
    act(&mut app, Action::NextEntry);
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "a1");
    app.set_limit("~s a");
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "a1");
}

/// What the index shows and the folded messages, how each is
/// threaded, and the entry selected.
type State = (
    Vec<MessageId>,
    Vec<MessageId>,
    Vec<(MessageId, Threaded)>,
    usize,
);

fn state(app: &App) -> State {
    let ids = |list: &[MessageSummary]| list.iter().map(|m| m.id.clone()).collect();
    let mut threads: Vec<_> = app.index.threads.clone().into_iter().collect();
    threads.sort_by(|a, b| a.0.cmp(&b.0));
    (
        ids(&app.index.messages),
        ids(&app.index.folded),
        threads,
        app.index.selected,
    )
}

#[test]
fn collapsing_and_opening_one_thread_is_arranging_the_index_again() {
    // A small random generator, so that the test is the same each run.
    let mut seed = 7_u64;
    let mut below = |n: usize| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) as usize % n.max(1)
    };
    for round in 0..60 {
        // Threads of up to six messages, a few deep; listed newest
        // first, in pages.
        let mut messages = Vec::new();
        for t in 0..1 + below(8) {
            let size = 1 + below(6);
            for k in 0..size {
                let path: Vec<u8> = match k {
                    0 => vec![0],
                    1..=3 => vec![0, k as u8],
                    _ => vec![0, 1, k as u8],
                };
                let minutes = (below(50) * 10 + messages.len()) as i64;
                messages.push(message(
                    &format!("t{t}m{k}"),
                    minutes,
                    &format!("c{t}"),
                    &path,
                ));
            }
        }
        messages.sort_by_key(|m| std::cmp::Reverse(m.received));
        let mut pages: Vec<Vec<MessageSummary>> = Vec::new();
        while !messages.is_empty() {
            let size = (1 + below(5)).min(messages.len());
            pages.push(messages.drain(..size).collect());
        }
        let last = pages.len() == 1;
        let (mut app, mut next) = opened(
            sorted(["threads", "reverse-threads", "last-threads"][below(3)]),
            24,
            pages.remove(0),
            (!last).then_some("in next"),
        );
        for step in 0..30 {
            match below(5) {
                0 | 1 if !app.index.messages.is_empty() => {
                    app.index.selected = below(app.index.messages.len());
                    act(&mut app, Action::CollapseThread);
                }
                2 if !app.index.folded.is_empty() => {
                    let id = app.index.folded[below(app.index.folded.len())].id.clone();
                    app.uncollapse_to(&id);
                }
                3 if below(4) == 0 => {
                    act(&mut app, Action::CollapseAll);
                }
                _ => {
                    if let Some(id) = next.take()
                        && !pages.is_empty()
                    {
                        let page = pages.remove(0);
                        let more = (!pages.is_empty()).then_some("in next");
                        let effects = listed(&mut app, id, page, more);
                        next = more.map(|_| first_request(&effects));
                    }
                }
            }
            // Threaded, it is as arranging it all again makes it.
            if app.shown_count() == app.index.threaded_count {
                assert!(app.index.settled);
                let before = state(&app);
                app.arrange();
                assert_eq!(state(&app), before, "round {round}, step {step}");
            }
        }
    }
}

#[test]
fn without_threads_thread_keys_say_so() {
    let mut app = app();
    app.apply_ordering(Ordering::default());
    assert_eq!(
        app.index
            .messages
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["b1", "a2", "b0", "a1", "a0"]
    );
    act(&mut app, Action::CollapseThread);
    assert_eq!(app.status, "Threading is not enabled.");
}
