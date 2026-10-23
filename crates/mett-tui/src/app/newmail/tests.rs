use chrono::{DateTime, TimeDelta};
use mett_core::{AccountId, FlagsUpdate, FolderId, mock};
use ratatui::crossterm::event::{KeyCode, KeyEvent};

use super::*;
use crate::action::Action;
use crate::app::testing::*;
use crate::app::{Input, Msg, Settings};
use crate::worker::Event;
use mett_core::MessagePage;

const MINUTE: Duration = Duration::from_secs(60);

/// A message dated (and last changed) at `minutes`.
fn summary(id: &str, minutes: i64) -> MessageSummary {
    let mut summary = summary_at(id, id, minutes);
    summary.modified = Some(DateTime::UNIX_EPOCH + TimeDelta::minutes(minutes));
    summary
}

fn work() -> FolderPath {
    FolderPath::Named(vec!["Work".into()])
}

fn settings() -> Settings {
    let entry = |name: &str, path| MailboxEntry {
        name: name.into(),
        label: None,
        text: name.into(),
        path,
        notify: true,
        poll: true,
    };
    Settings {
        account: AccountId::new("alice@contoso.com"),
        folder: "msgraph://alice@contoso.com/".into(),
        sync_interval: Some(MINUTE),
        mailboxes: vec![entry("=@inbox", inbox()), entry("=Work", work())],
        ..Settings::default()
    }
}

fn folder(id: &str, name: &str, unread: u32) -> Folder {
    Folder {
        unread_count: unread,
        ..mock::folder(id, name, None)
    }
}

/// A mailbox checked, whose newest unread message arrived at
/// `minutes`, if it has any.
fn checked(id: &str, name: &str, unread: u32, newest: Option<i64>) -> mett_core::Result<Checked> {
    Ok(Checked {
        folder: folder(id, name, unread),
        recent: newest
            .map(|m| {
                let at = DateTime::UNIX_EPOCH + TimeDelta::minutes(m);
                // As the test folders name their messages.
                vec![(MessageId::new(format!("m{m}")), at)]
            })
            .unwrap_or_default(),
    })
}

/// An Inbox with m2, m1 and m0 (newest first), and the mailboxes check
/// it started, not yet answered.
fn app() -> (App, RequestId) {
    let (mut app, effects) = App::open(80, 24, inbox(), settings());
    let (open, _) = requests(&effects)[0].clone();
    let effects = respond(
        &mut app,
        open,
        Event::FolderOpened(Ok(folder("in", "Inbox", 1))),
    );
    let sent = requests(&effects);
    assert!(matches!(sent[0].1, Request::ListMessages { .. }));
    assert!(matches!(&sent[1].1, Request::CheckMailboxes(m) if m.len() == 2));
    assert!(effects.contains(&Effect::Later(MINUTE, Timer::Poll(0))));
    let messages = vec![summary("m2", 2), summary("m1", 1), summary("m0", 0)];
    listed(&mut app, sent[0].0, messages, None);
    (app, sent[1].0)
}

fn unsorted() -> crate::app::Ordering {
    crate::app::Ordering {
        sort: crate::sort::Sort::parse("unsorted", true).unwrap(),
        ..crate::app::Ordering::default()
    }
}

/// Fires the poll timer and returns the folder's listing request.
fn poll(app: &mut App) -> RequestId {
    let effects = app.update(Msg::Timer(Timer::Poll(0)));
    assert!(effects.contains(&Effect::Later(MINUTE, Timer::Poll(0))));
    requests(&effects)
        .into_iter()
        .find(|(_, r)| matches!(r, Request::PollFolder { .. }))
        .expect("the folder is checked")
        .0
}

fn page(messages: Vec<MessageSummary>) -> MessagePage {
    MessagePage {
        messages,
        next: None,
    }
}

/// A page with more to come after it, at `next`.
fn more(messages: Vec<MessageSummary>, next: &str) -> MessagePage {
    MessagePage {
        messages,
        next: Some(mett_core::PageCursor::new(next)),
    }
}

/// Answers the check of the open folder with its `newest` messages
/// and those `changed` since the last check.
fn polled_with(
    app: &mut App,
    id: RequestId,
    newest: MessagePage,
    changed: MessagePage,
) -> Vec<Effect> {
    respond(app, id, Event::FolderPolled(Ok(Polled { newest, changed })))
}

/// Answers the check of the open folder with its newest `messages`,
/// and no changes.
fn polled(app: &mut App, id: RequestId, messages: Vec<MessageSummary>) -> Vec<Effect> {
    polled_with(app, id, page(messages), page(Vec::new()))
}

/// Fires the poll timer; returns the check of the mailboxes it starts.
fn checking(app: &mut App) -> (RequestId, Request) {
    let effects = app.update(Msg::Timer(Timer::Poll(0)));
    requests(&effects)
        .into_iter()
        .find(|(_, r)| matches!(r, Request::CheckMailboxes(_)))
        .unwrap()
}

fn next_check(app: &mut App) -> RequestId {
    checking(app).0
}

/// Answers a check of the mailboxes with what it found in the Inbox
/// and in Work.
fn check_both(
    app: &mut App,
    id: RequestId,
    inbox_found: mett_core::Result<Checked>,
    work_found: mett_core::Result<Checked>,
) -> Vec<Effect> {
    let found = vec![(inbox(), inbox_found), (work(), work_found)];
    respond(app, id, Event::MailboxesChecked(found))
}

/// Unread mail found by a check: each message and its minute.
fn recent(messages: &[(&str, i64)]) -> Vec<(MessageId, DateTime<Utc>)> {
    messages
        .iter()
        .map(|(id, m)| {
            (
                MessageId::new(*id),
                DateTime::UNIX_EPOCH + TimeDelta::minutes(*m),
            )
        })
        .collect()
}

/// Opens the folder at `path`, found as `found`.
fn switch_to(app: &mut App, path: FolderPath, found: Folder) -> Vec<Effect> {
    app.change_folder(path);
    let (switch, _) = app.switching.clone().unwrap();
    respond(app, switch, Event::FolderOpened(Ok(found)))
}

/// A draft dated (and last changed) at `minutes`.
fn draft(id: &str, minutes: i64) -> MessageSummary {
    let mut draft = summary(id, minutes);
    draft.draft = true;
    draft
}

#[test]
fn a_watch_whose_sign_in_failed_waits_for_the_user() {
    let (mut app, _) = app();
    let watch = app.watching.expect("watched");
    let denied = || Event::Watched(Some(Err(Error::Auth("cancelled".into()))));
    // Not tried again unbidden. Checks go on: the mailbox may only
    // refuse to be watched.
    assert!(respond(&mut app, watch, denied()).is_empty());
    assert!(app.watch_paused && !app.poll_paused);
    assert!(app.watch().is_empty());
    // The user signs in again: watched again.
    let effects = app.retry();
    assert!(
        requests(&effects)
            .iter()
            .any(|(_, r)| matches!(r, Request::Watch(_))),
        "{effects:?}"
    );
    // A failure from before then doesn't hold it off again.
    let watch = app.watching.expect("watched");
    app.poll_epoch += 1;
    let effects = respond(&mut app, watch, denied());
    assert_eq!(effects, [Effect::Later(WATCH_RETRY, Timer::Watch)]);
    assert!(!app.watch_paused);
}

#[test]
fn folders_are_checked_as_soon_as_they_change() {
    let (mut app, _) = app();
    let inbox = mett_core::FolderId::new("in");
    // Opened, the Inbox is watched.
    let watch = app.watching.expect("the folder is watched");
    // A change: it is checked at once, and watched again.
    let effects = respond(
        &mut app,
        watch,
        Event::Watched(Some(Ok(vec![inbox.clone()]))),
    );
    let sent = requests(&effects);
    assert!(
        sent.iter()
            .any(|(_, r)| matches!(r, Request::PollFolder { .. })),
        "{sent:?}"
    );
    assert!(
        sent.iter()
            .any(|(_, r)| matches!(r, Request::Watch(f) if f == std::slice::from_ref(&inbox))),
        "{sent:?}"
    );
    // Its wait ran out: watched again, nothing checked.
    let watch = app.watching.expect("watched again");
    let effects = respond(&mut app, watch, Event::Watched(Some(Ok(Vec::new()))));
    assert!(matches!(requests(&effects)[..], [(_, Request::Watch(_))]));
    // It failed: tried again later (the timer's checks go on).
    let watch = app.watching.expect("watched again");
    let failed = Event::Watched(Some(Err(mett_core::Error::Network("down".into()))));
    let effects = respond(&mut app, watch, failed);
    assert_eq!(effects, [Effect::Later(WATCH_RETRY, Timer::Watch)]);
    assert!(
        app.update(Msg::Timer(Timer::Poll(0)))
            .iter()
            .all(|e| { !matches!(e, Effect::Request(_, Request::Watch(_))) })
    );
    let effects = app.update(Msg::Timer(Timer::Watch));
    assert!(matches!(requests(&effects)[..], [(_, Request::Watch(_))]));
    // The mailbox can't watch: never again.
    let watch = app.watching.expect("watched again");
    assert!(respond(&mut app, watch, Event::Watched(None)).is_empty());
    assert!(
        app.update(Msg::Timer(Timer::Poll(0)))
            .iter()
            .all(|e| { !matches!(e, Effect::Request(_, Request::Watch(_))) })
    );
}

#[test]
fn the_watch_follows_the_folders_and_checks_only_those_changed() {
    let (mut app, check) = app();
    let inbox = mett_core::FolderId::new("in");
    let work = mett_core::FolderId::new("wk");
    let first = app.watching.expect("the folder is watched");
    // The mailboxes found, they are watched too: the watch starts
    // again.
    let effects = check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, None),
        checked("wk", "Work", 0, None),
    );
    let sent = requests(&effects);
    assert!(
        sent.iter()
            .any(|(_, r)| matches!(r, Request::Cancel(id) if *id == first)),
        "{sent:?}"
    );
    assert!(
        sent.iter()
            .any(|(_, r)| matches!(r, Request::Watch(f) if f == &[inbox.clone(), work.clone()])),
        "{sent:?}"
    );
    // The same folders: not again.
    assert!(app.watch().is_empty());
    // The Inbox changed: its index is checked, and its counts (it is
    // one of the mailboxes too).
    let watch = app.watching.expect("watched");
    let effects = respond(
        &mut app,
        watch,
        Event::Watched(Some(Ok(vec![inbox.clone()]))),
    );
    let sent = requests(&effects);
    assert!(
        sent.iter()
            .any(|(_, r)| matches!(r, Request::PollFolder { .. })),
        "{sent:?}"
    );
    let check = sent
        .iter()
        .find_map(|(id, r)| match r {
            Request::CheckMailboxes(m) if m.len() == 1 && m[0].0 == super::tests::inbox() => {
                Some(*id)
            }
            _ => None,
        })
        .expect("the Inbox's counts are checked");
    respond(&mut app, check, Event::MailboxesChecked(Vec::new()));
    // Work changed: only it is checked.
    let watch = app.watching.expect("watched");
    let effects = respond(
        &mut app,
        watch,
        Event::Watched(Some(Ok(vec![work.clone()]))),
    );
    let sent = requests(&effects);
    assert!(
        !sent
            .iter()
            .any(|(_, r)| matches!(r, Request::PollFolder { .. })),
        "the Inbox didn't change: {sent:?}"
    );
    assert!(
        sent.iter().any(|(_, r)| matches!(
            r,
            Request::CheckMailboxes(m) if m.len() == 1 && m[0].0 == super::tests::work()
        )),
        "{sent:?}"
    );
}

#[test]
fn a_change_during_a_check_is_checked_after_it() {
    let (mut app, _) = app();
    let inbox = mett_core::FolderId::new("in");
    // A check is under way when the watch says the Inbox changed.
    let check = poll(&mut app);
    let watch = app.watching.expect("watched");
    let effects = respond(&mut app, watch, Event::Watched(Some(Ok(vec![inbox]))));
    assert!(
        !requests(&effects)
            .iter()
            .any(|(_, r)| matches!(r, Request::PollFolder { .. })),
        "one at a time"
    );
    // It may predate the change: once it ends, the Inbox is checked
    // again.
    let effects = polled(&mut app, check, Vec::new());
    assert!(
        requests(&effects)
            .iter()
            .any(|(_, r)| matches!(r, Request::PollFolder { .. })),
        "{effects:?}"
    );
}

#[test]
fn turning_checks_off_ends_the_watch() {
    let (mut app, _) = app();
    let watch = app.watching.expect("watched");
    app.sync_interval = None;
    // The watch ends, and so does what the mailbox keeps for it.
    let effects = app.watch();
    let sent = requests(&effects);
    assert!(
        matches!(&sent[..], [(_, Request::Cancel(id)), (_, Request::Watch(f))]
            if *id == watch && f.is_empty()),
        "{sent:?}"
    );
    assert!(app.watching.is_none());
    assert!(app.unwatching());
    // Once.
    assert!(app.watch().is_empty());
    // It failed: tried again later.
    let unwatch = effect_id(&effects[1]);
    let failed = Event::Watched(Some(Err(Error::Network("down".into()))));
    let effects = respond(&mut app, unwatch, failed);
    assert_eq!(effects, [Effect::Later(WATCH_RETRY, Timer::Watch)]);
    assert!(!app.unwatching());
    let effects = app.update(Msg::Timer(Timer::Watch));
    let sent = requests(&effects);
    assert!(
        matches!(&sent[..], [(_, Request::Watch(f))] if f.is_empty()),
        "{sent:?}"
    );
    let unwatch = effect_id(&effects[0]);
    assert!(respond(&mut app, unwatch, Event::Watched(Some(Ok(Vec::new())))).is_empty());
    assert!(!app.unwatching());
    assert!(app.watch().is_empty());
}

#[test]
fn changes_told_by_a_replaced_watch_are_checked() {
    let (mut app, check) = app();
    let inbox = mett_core::FolderId::new("in");
    let first = app.watching.expect("the folder is watched");
    // The mailboxes found, the watch starts again; the first had ended
    // already, telling of a change.
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, None),
        checked("wk", "Work", 0, None),
    );
    let second = app.watching.expect("watched again");
    assert_ne!(first, second);
    let effects = respond(&mut app, first, Event::Watched(Some(Ok(vec![inbox]))));
    let sent = requests(&effects);
    assert!(
        sent.iter()
            .any(|(_, r)| matches!(r, Request::PollFolder { .. })),
        "{sent:?}"
    );
    // Not watched a third time.
    assert_eq!(app.watching, Some(second));
    assert!(
        !sent.iter().any(|(_, r)| matches!(r, Request::Watch(_))),
        "{sent:?}"
    );
}

#[test]
fn ending_a_watch_whose_sign_in_failed_waits_for_the_user() {
    let (mut app, _) = app();
    app.sync_interval = None;
    let effects = app.watch();
    let unwatch = effect_id(&effects[1]);
    let denied = Event::Watched(Some(Err(Error::Auth("cancelled".into()))));
    assert!(respond(&mut app, unwatch, denied).is_empty());
    // Not on the timer, nor when the sign-in window closes.
    assert!(app.update(Msg::Timer(Timer::Watch)).is_empty());
    let account = mett_core::AccountId::from("alice@contoso.com");
    let cancelled = crate::SignInEvent::Finished {
        account: account.clone(),
        outcome: Err("cancelled".into()),
    };
    assert!(requests(&app.update(Msg::SignIn(cancelled))).is_empty());
    // Signed in: tried again.
    let signed_in = crate::SignInEvent::Finished {
        account,
        outcome: Ok(()),
    };
    let effects = app.update(Msg::SignIn(signed_in));
    let sent = requests(&effects);
    assert!(
        matches!(&sent[..], [(_, Request::Watch(f))] if f.is_empty()),
        "{sent:?}"
    );
}

#[test]
fn a_watch_paused_is_ended_once_signed_in() {
    let (mut app, _) = app();
    let watch = app.watching.expect("watched");
    let denied = Event::Watched(Some(Err(Error::Auth("cancelled".into()))));
    respond(&mut app, watch, denied);
    // Checks turned off: not ended yet, not to ask for a sign-in.
    app.sync_interval = None;
    assert!(app.watch().is_empty());
    let signed_in = crate::SignInEvent::Finished {
        account: "alice@contoso.com".into(),
        outcome: Ok(()),
    };
    let effects = app.update(Msg::SignIn(signed_in));
    assert!(
        matches!(&requests(&effects)[..], [(_, Request::Watch(f))] if f.is_empty()),
        "{effects:?}"
    );
}

#[test]
fn a_watch_paused_goes_on_once_signed_in() {
    let (mut app, _) = app();
    let watch = app.watching.expect("watched");
    let denied = Event::Watched(Some(Err(Error::Auth("cancelled".into()))));
    respond(&mut app, watch, denied);
    let signed_in = crate::SignInEvent::Finished {
        account: "alice@contoso.com".into(),
        outcome: Ok(()),
    };
    let effects = app.update(Msg::SignIn(signed_in));
    assert!(
        requests(&effects)
            .iter()
            .any(|(_, r)| matches!(r, Request::Watch(f) if !f.is_empty())),
        "{effects:?}"
    );
}

#[test]
fn checks_turned_on_again_watch_once_the_watch_has_ended() {
    let (mut app, _) = app();
    let interval = app.sync_interval;
    app.sync_interval = None;
    let effects = app.watch();
    let unwatch = effect_id(&effects[1]);
    app.sync_interval = interval;
    assert!(app.watch().is_empty());
    let effects = respond(&mut app, unwatch, Event::Watched(Some(Ok(Vec::new()))));
    assert!(
        matches!(&requests(&effects)[..], [(_, Request::Watch(f))] if !f.is_empty()),
        "{effects:?}"
    );
}

#[test]
fn nothing_is_watched_nor_checked_after_quitting() {
    let (mut app, _) = app();
    app.quit = true;
    let effects = app.unwatch();
    assert!(app.unwatching(), "{effects:?}");
    // Say, a sign-in a save needed ends: no watch starts again.
    app.recheck_folder = true;
    assert!(app.watch().is_empty());
    assert!(app.recheck().is_empty());
}

#[test]
fn a_message_missing_at_the_page_s_end_is_asked_about() {
    let (mut app, _) = app();
    // m0 is as old as m1, the page's last: on the next page, or gone.
    app.messages.iter_mut().for_each(|m| {
        if m.id.as_str() == "m0" {
            m.received = DateTime::UNIX_EPOCH + TimeDelta::minutes(1);
        }
    });
    let partial = |app: &mut App| {
        let id = poll(app);
        let newest = more(vec![summary("m2", 2), summary("m1", 1)], "in more");
        polled_with(app, id, newest, page(Vec::new()))
    };
    let still_there = |effects: &[Effect]| {
        requests(effects)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::StillThere { ids, .. } if ids == &[MessageId::new("m0")]))
            .map(|(id, _)| id)
    };
    // There still: kept, and asked about again at the next check.
    let effects = partial(&mut app);
    let check = still_there(&effects).expect("m0 is asked about");
    respond(&mut app, check, Event::StillThere(Ok(Vec::new())));
    assert_eq!(ids(&app), ["m2", "m1", "m0"]);
    // Gone since: taken out.
    let effects = partial(&mut app);
    let check = still_there(&effects).expect("m0 is asked about again");
    respond(
        &mut app,
        check,
        Event::StillThere(Ok(vec![MessageId::new("m0")])),
    );
    assert_eq!(ids(&app), ["m2", "m1"]);
}

#[test]
fn new_mail_joins_the_index_where_its_date_puts_it() {
    let (mut app, _) = app();
    act(&mut app, Action::NextEntry);
    let id = poll(&mut app);
    let mut read = summary("m1", 1);
    read.flags.read = true;
    polled(
        &mut app,
        id,
        vec![summary("m3", 3), summary("m2", 2), read, summary("m0", 0)],
    );
    assert_eq!(ids(&app), ["m3", "m2", "m1", "m0"]);
    // Still on m1, which was read elsewhere.
    assert_eq!(app.selected, 2);
    assert!(app.messages[2].flags.read);
    assert_eq!(app.status, NEW_MAIL);
}

#[test]
fn mail_that_arrives_with_an_old_date_joins_the_index() {
    let (mut app, _) = app();
    let id = poll(&mut app);
    let Some(Polling { started, .. }) = app.polling.clone() else {
        panic!("expected a check");
    };
    polled_with(
        &mut app,
        id,
        page(vec![summary("m2", 2), summary("m1", 1), summary("m0", 0)]),
        page(vec![summary("late", -5)]),
    );
    assert_eq!(ids(&app), ["m2", "m1", "m0", "late"]);
    assert_eq!(app.status, NEW_MAIL);
    // The next check asks for changes since this one began.
    let effects = app.update(Msg::Timer(Timer::Poll(0)));
    let since = requests(&effects).into_iter().find_map(|(_, r)| match r {
        Request::PollFolder { since, .. } => Some(since),
        _ => None,
    });
    assert_eq!(since, Some(started - OVERLAP));
}

#[test]
fn a_new_interval_replaces_the_timer() {
    let (mut app, _) = app();
    let mut settings = settings();
    settings.sync_interval = Some(Duration::from_secs(10));
    app.apply_settings(settings);
    let effects = app.arm_poll();
    assert_eq!(
        effects,
        [Effect::Later(Duration::from_secs(10), Timer::Poll(1))]
    );
    // The old timer does nothing.
    assert_eq!(app.update(Msg::Timer(Timer::Poll(0))), []);
}

#[test]
fn mail_gone_elsewhere_leaves_the_index() {
    let (mut app, _) = app();
    let id = poll(&mut app);
    // A full page that ends at m1: m0 may be on the next.
    polled_with(
        &mut app,
        id,
        more(vec![summary("m1", 1)], "in next"),
        page(Vec::new()),
    );
    assert_eq!(ids(&app), ["m1", "m0"]);
    assert!(app.status.is_empty());
}

#[test]
fn a_draft_beyond_the_newest_page_stays() {
    let (mut app, _) = app();
    // In the order the folder is listed in.
    app.apply_ordering(unsorted());
    let id = poll(&mut app);
    // A draft created after m1 was sent, but listed after the sent
    // mail, as Graph sorts unsent drafts.
    let draft = draft("d", 5);
    app.messages.push(draft);
    polled_with(
        &mut app,
        id,
        more(vec![summary("m2", 2), summary("m1", 1)], "in next"),
        page(Vec::new()),
    );
    assert_eq!(ids(&app), ["m2", "m1", "m0", "d"]);
}

#[test]
fn drafts_are_placed_and_removed_among_drafts() {
    let (mut app, _) = app();
    // In the order the folder is listed in.
    app.apply_ordering(unsorted());
    // The drafts were listed after the sent mail.
    app.messages.push(draft("d2", 9));
    app.messages.push(draft("d1", 8));
    app.positions.insert(MessageId::new("d2"), 3);
    app.positions.insert(MessageId::new("d1"), 4);
    let id = poll(&mut app);
    // d3 is new, and d2 went; d1 may be on the next page.
    polled_with(
        &mut app,
        id,
        more(
            vec![
                summary("m2", 2),
                summary("m1", 1),
                summary("m0", 0),
                draft("d3", 10),
                draft("d1", 8),
            ],
            "in next",
        ),
        page(Vec::new()),
    );
    assert_eq!(ids(&app), ["m2", "m1", "m0", "d3", "d1"]);
}

#[test]
fn changes_older_than_the_listing_need_no_listing_again() {
    let (mut app, _) = app();
    let id = poll(&mut app);
    let effects = polled_with(
        &mut app,
        id,
        page(vec![summary("m2", 2), summary("m1", 1), summary("m0", 0)]),
        more(vec![summary("m1", 1)], "in more"),
    );
    assert!(requests(&effects).is_empty(), "{effects:?}");
    // And the next check still allows for the clocks.
    let effects = app.update(Msg::Timer(Timer::Poll(0)));
    let Some(Polling { started, .. }) = app.polling.clone() else {
        panic!("expected a check");
    };
    let since = requests(&effects).into_iter().find_map(|(_, r)| match r {
        Request::PollFolder { since, .. } => Some(since),
        _ => None,
    });
    assert!(since.is_some_and(|since| since <= started - OVERLAP + TimeDelta::seconds(5)));
}

#[test]
fn a_late_failure_from_before_a_retry_does_not_stop_checks() {
    let (mut app, check) = app();
    app.retry();
    respond(
        &mut app,
        check,
        Event::MailboxesChecked(vec![(inbox(), Err(expired()))]),
    );
    poll(&mut app);
}

#[test]
fn mail_that_changed_with_what_was_seen_may_be_new() {
    let seen = Seen {
        until: Some(DateTime::UNIX_EPOCH),
        messages: HashSet::from([MessageId::new("m1")]),
    };
    assert!(!seen.is_new(&MessageId::new("m1"), DateTime::UNIX_EPOCH));
    assert!(seen.is_new(&MessageId::new("m2"), DateTime::UNIX_EPOCH));
}

#[test]
fn mail_seen_before_the_first_check_is_not_new() {
    let (mut app, check) = app();
    switch_to(&mut app, work(), folder("work", "Work", 0));
    // The Inbox index had m2 (dated 2) unread when the user left.
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 3, Some(2)),
        checked("work", "Work", 0, None),
    );
    assert_eq!(app.mailboxes_with_new_mail(), 0);
}

#[test]
fn another_folder_at_a_mailbox_s_path_starts_afresh() {
    let (mut app, check) = app();
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, Some(1)),
        checked("work", "Work", 0, None),
    );
    app.seen.insert(
        FolderId::new("work"),
        Seen {
            until: Some(DateTime::UNIX_EPOCH + TimeDelta::minutes(50)),
            messages: HashSet::new(),
        },
    );
    let check = next_check(&mut app);
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, Some(1)),
        checked("work2", "Work", 1, Some(3)),
    );
    assert_eq!(app.mailboxes_with_new_mail(), 1);
}

#[test]
fn mail_found_by_listing_again_is_announced() {
    let (mut app, _) = app();
    let id = poll(&mut app);
    // More changes than were read, since the folder was listed.
    let mut recent = summary("m1", 1);
    recent.modified = Some(Utc::now());
    let effects = polled_with(
        &mut app,
        id,
        page(vec![summary("m2", 2), summary("m1", 1), summary("m0", 0)]),
        more(vec![recent], "in more"),
    );
    assert!(app.status.is_empty(), "nothing new seen yet");
    let (list, _) = requests(&effects)[0].clone();
    respond(
        &mut app,
        list,
        Event::MessagesListed(Ok(page(vec![
            summary("m2", 2),
            summary("m1", 1),
            summary("m0", 0),
            summary("old", -9),
        ]))),
    );
    assert_eq!(app.status, NEW_MAIL);
}

#[test]
fn lifting_a_limit_keeps_new_mail_in_place() {
    let (mut app, _) = app();
    // In the order the folder is listed in.
    app.apply_ordering(unsorted());
    // Listed out of date order, as drafts can be.
    let draft = draft("d", 9);
    app.messages.push(draft.clone());
    app.positions.insert(draft.id.clone(), 3);
    act(&mut app, Action::Limit);
    typed(&mut app, "~s m2");
    key(&mut app, KeyCode::Enter);
    assert_eq!(ids(&app), ["m2"]);
    let id = poll(&mut app);
    polled_with(
        &mut app,
        id,
        more(
            vec![summary("m3", 3), summary("m2", 2), summary("m1", 1)],
            "in next",
        ),
        page(Vec::new()),
    );
    act(&mut app, Action::Limit);
    // The question starts with the limit in force.
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('u'),
        ratatui::crossterm::event::KeyModifiers::CONTROL,
    )));
    typed(&mut app, "all");
    key(&mut app, KeyCode::Enter);
    assert_eq!(ids(&app), ["m3", "m2", "m1", "m0", "d"]);
}

#[test]
fn a_full_page_of_new_mail_lists_the_folder_again() {
    let (mut app, _) = app();
    let id = poll(&mut app);
    let effects = polled_with(
        &mut app,
        id,
        more(vec![summary("m4", 4), summary("m3", 3)], "in next"),
        page(Vec::new()),
    );
    assert!(matches!(
        requests(&effects)[..],
        [(_, Request::ListMessages { .. })]
    ));
    assert_eq!(app.status, NEW_MAIL);
}

#[test]
fn a_message_moved_away_meanwhile_does_not_come_back() {
    let (mut app, _) = app();
    let id = poll(&mut app);
    let gone: HashSet<MessageId> = [MessageId::new("m2")].into();
    app.remove_messages(&gone);
    polled(
        &mut app,
        id,
        vec![summary("m2", 2), summary("m1", 1), summary("m0", 0)],
    );
    assert_eq!(ids(&app), ["m1", "m0"]);
}

#[test]
fn flags_saved_meanwhile_are_kept() {
    let (mut app, _) = app();
    let id = poll(&mut app);
    let effects = app.update_flags(MessageId::new("m2"), FlagsUpdate::flagged(true));
    let (write, _) = requests(&effects)[0].clone();
    respond(&mut app, write, Event::FlagsUpdated(Ok(())));
    polled(
        &mut app,
        id,
        vec![summary("m2", 2), summary("m1", 1), summary("m0", 0)],
    );
    assert!(app.messages[0].flags.flagged);
}

#[test]
fn a_listing_started_meanwhile_wins() {
    let (mut app, _) = app();
    let id = poll(&mut app);
    app.refresh();
    polled(&mut app, id, vec![summary("m9", 9)]);
    assert!(app.messages.is_empty(), "the relist hasn't answered yet");
}

#[test]
fn checks_wait_for_a_sign_in_after_one_failed() {
    let (mut app, _) = app();
    let id = poll(&mut app);
    respond(&mut app, id, Event::FolderPolled(Err(expired())));
    let effects = app.update(Msg::Timer(Timer::Poll(0)));
    assert_eq!(effects, [Effect::Later(MINUTE, Timer::Poll(0))]);
    app.retry();
    poll(&mut app);
}

#[test]
fn no_timer_without_an_interval() {
    let settings = Settings {
        sync_interval: None,
        ..settings()
    };
    let (mut app, effects) = App::open(80, 24, inbox(), settings);
    let (open, _) = requests(&effects)[0].clone();
    let effects = respond(
        &mut app,
        open,
        Event::FolderOpened(Ok(folder("in", "Inbox", 0))),
    );
    assert!(!effects.iter().any(|e| matches!(e, Effect::Later(..))));
    // Nor a check of the mailboxes.
    assert!(
        !requests(&effects)
            .iter()
            .any(|(_, r)| matches!(r, Request::CheckMailboxes(_)))
    );
}

#[test]
fn says_which_mailboxes_have_new_mail() {
    let (mut app, check) = app();
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, Some(1)),
        checked("work", "Work", 0, None),
    );
    // The open folder doesn't count.
    assert_eq!(app.mailboxes_with_new_mail(), 0);
    let (check, request) = checking(&mut app);
    // Checked again by ID.
    assert_eq!(
        request,
        Request::CheckMailboxes(vec![
            (
                inbox(),
                Some(FolderId::new("in")),
                Some(DateTime::UNIX_EPOCH + TimeDelta::minutes(2))
            ),
            (work(), Some(FolderId::new("work")), None),
        ])
    );
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 2, Some(2)),
        checked("work", "Work", 3, Some(3)),
    );
    assert_eq!(app.status, "New mail in =Work.");
    assert_eq!(app.mailboxes_with_new_mail(), 1);

    // `c` offers it.
    act(&mut app, Action::ChangeFolder);
    let Some(Input::Line { editor, .. }) = &app.input else {
        panic!("expected a question");
    };
    assert_eq!(editor.text(), "=Work");
    key(&mut app, KeyCode::Enter);
    let (switch, _) = app.switching.clone().expect("the folder is being opened");
    respond(
        &mut app,
        switch,
        Event::FolderOpened(Ok(folder("work", "Work", 3))),
    );
    assert_eq!(app.mailboxes_with_new_mail(), 0);
}

#[test]
fn mail_that_arrives_after_leaving_is_new_whatever_was_read() {
    let (mut app, check) = app();
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, Some(1)),
        checked("work", "Work", 0, None),
    );
    // m0 is read in the Inbox; then the user leaves for Work, before
    // the change is saved.
    let effects = app.update_flags(MessageId::new("m0"), FlagsUpdate::read(true));
    let (write, _) = requests(&effects)[0].clone();
    let effects = switch_to(&mut app, work(), folder("work", "Work", 0));
    respond(&mut app, write, Event::FlagsUpdated(Ok(())));
    assert!(
        requests(&effects)
            .iter()
            .all(|(_, r)| !matches!(r, Request::CheckMailboxes(_)))
    );
    // Old unread mail isn't new.
    let check_again = |app: &mut App, newest: i64| {
        let check = next_check(app);
        check_both(
            app,
            check,
            checked("in", "Inbox", 1, Some(newest)),
            checked("work", "Work", 0, None),
        );
    };
    check_again(&mut app, 1);
    assert_eq!(app.mailboxes_with_new_mail(), 0);
    assert!(!app.status.starts_with("New mail"), "{}", app.status);
    // Mail that arrives now is, though the count is as before.
    check_again(&mut app, 5);
    assert_eq!(app.status, "New mail in =@inbox.");
    assert_eq!(app.mailboxes_with_new_mail(), 1);
}

#[test]
fn mail_seen_on_a_visit_is_not_new_after_it() {
    let (mut app, check) = app();
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, Some(1)),
        checked("work", "Work", 0, None),
    );
    // Mail arrives in Work; the user opens it before the next check,
    // and leaves before that check answers.
    let effects = switch_to(&mut app, work(), folder("work", "Work", 1));
    let sent = requests(&effects);
    // The new mail is listed: the user sees it.
    let (list, _) = sent
        .iter()
        .find(|(_, r)| matches!(r, Request::ListMessages { .. }))
        .cloned()
        .unwrap();
    let mut arrived = summary("m3", 3);
    arrived.modified = Some(DateTime::UNIX_EPOCH + TimeDelta::minutes(3));
    respond(
        &mut app,
        list,
        Event::MessagesListed(Ok(page(vec![arrived]))),
    );
    switch_to(&mut app, inbox(), folder("in", "Inbox", 1));
    let check = next_check(&mut app);
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, Some(1)),
        checked("work", "Work", 1, Some(3)),
    );
    assert_eq!(app.mailboxes_with_new_mail(), 0);
    assert!(!app.status.starts_with("New mail"), "{}", app.status);
}

#[test]
fn mail_arriving_after_leaving_is_new_even_if_a_check_straddles_it() {
    let (mut app, first) = app();
    check_both(
        &mut app,
        first,
        checked("in", "Inbox", 1, Some(1)),
        checked("work", "Work", 0, None),
    );
    // A check starts while the Inbox is open; the user leaves; mail
    // arrives in the Inbox before the check reads it.
    let check = next_check(&mut app);
    switch_to(&mut app, work(), folder("work", "Work", 0));
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 2, Some(9)),
        checked("work", "Work", 0, None),
    );
    assert_eq!(app.mailboxes_with_new_mail(), 1);
}

#[test]
fn mail_a_check_finds_before_it_is_listed_is_not_seen() {
    let (mut app, check) = app();
    // The check finds w9 in Work while it is open, before the listing
    // shows it; the user leaves before it does.
    switch_to(&mut app, work(), folder("work", "Work", 1));
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, Some(1)),
        checked("work", "Work", 1, Some(9)),
    );
    switch_to(&mut app, inbox(), folder("in", "Inbox", 1));
    let check = next_check(&mut app);
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, Some(1)),
        checked("work", "Work", 1, Some(9)),
    );
    assert_eq!(app.mailboxes_with_new_mail(), 1);
}

#[test]
fn a_failure_hidden_by_another_message_is_said_later() {
    let (mut app, check) = app();
    app.status.info("Loading Inbox...");
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, Some(1)),
        Err(Error::Network("down".into())),
    );
    assert_eq!(app.status, "Loading Inbox...");
    app.status.clear();
    let check = next_check(&mut app);
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, Some(1)),
        Err(Error::Network("down".into())),
    );
    assert!(
        app.status.starts_with("Couldn't check =Work"),
        "{}",
        app.status
    );
}

#[test]
fn an_unchecked_mailbox_has_no_new_mail() {
    let (mut app, check) = app();
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, Some(1)),
        checked("work", "Work", 2, Some(2)),
    );
    assert_eq!(app.mailboxes_with_new_mail(), 1);
    let mut settings = settings();
    settings.mailboxes[1].poll = false;
    app.apply_settings(settings);
    assert_eq!(app.mailboxes_with_new_mail(), 0);
}

#[test]
fn mail_found_while_open_but_unseen_is_announced_after_leaving() {
    let (mut app, check) = app();
    // Found while the Inbox is open, before its index shows it.
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 4, Some(9)),
        checked("work", "Work", 0, None),
    );
    switch_to(&mut app, work(), folder("work", "Work", 0));
    let check = next_check(&mut app);
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 4, Some(9)),
        checked("work", "Work", 0, None),
    );
    assert_eq!(app.status, "New mail in =@inbox.");
}

#[test]
fn a_refused_mailbox_is_said_and_checks_go_on() {
    let (mut app, check) = app();
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, Some(1)),
        Err(Error::Auth("access denied".into())),
    );
    assert!(
        app.status.starts_with("Couldn't check =Work"),
        "{}",
        app.status
    );
    poll(&mut app);
}

#[test]
fn a_check_lists_again_what_failed_to_list() {
    let (mut app, _) = app();
    app.unlisted = Some(Page::first(10));
    let effects = app.update(Msg::Timer(Timer::Poll(0)));
    let requests = requests(&effects);
    assert!(
        !requests
            .iter()
            .any(|(_, r)| matches!(r, Request::PollFolder { .. }))
    );
    assert!(
        requests
            .iter()
            .any(|(_, r)| matches!(r, Request::ListMessages { .. }))
    );
    assert!(app.unlisted.is_none());
}

/// Leaves the Inbox for Work, then has a check find the Inbox's
/// `recent` unread mail.
fn leave_and_check(app: &mut App, found: Vec<(&str, i64)>) {
    switch_to(app, work(), folder("work", "Work", 0));
    let check = next_check(app);
    check_both(
        app,
        check,
        Ok(Checked {
            folder: folder("in", "Inbox", 3),
            recent: recent(&found),
        }),
        checked("work", "Work", 0, None),
    );
}

#[test]
fn mail_the_user_changed_is_not_new() {
    let (mut app, check) = app();
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 3, Some(2)),
        checked("work", "Work", 0, None),
    );
    // m1 is flagged: Graph says it changed (at 30).
    let effects = app.update_flags(MessageId::new("m1"), FlagsUpdate::flagged(true));
    let (write, _) = requests(&effects)[0].clone();
    respond(&mut app, write, Event::FlagsUpdated(Ok(())));
    leave_and_check(&mut app, vec![("m1", 30), ("m2", 2)]);
    assert_eq!(app.mailboxes_with_new_mail(), 0);
    // Mail that really is new still counts.
    switch_to(&mut app, inbox(), folder("in", "Inbox", 3));
    leave_and_check(&mut app, vec![("m9", 40), ("m1", 30)]);
    assert_eq!(app.mailboxes_with_new_mail(), 1);
}

#[test]
fn mail_arriving_after_a_visit_is_announced_at_the_first_check() {
    // The user leaves the Inbox before the first check answers; mail
    // arrives after what they saw.
    let (mut app, check) = app();
    switch_to(&mut app, work(), folder("work", "Work", 0));
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 4, Some(50)),
        checked("work", "Work", 0, None),
    );
    assert_eq!(app.mailboxes_with_new_mail(), 1);
    assert_eq!(app.status, "New mail in =@inbox.");
}

#[test]
fn what_was_seen_counts_for_any_path_to_the_folder() {
    // The Inbox is open as @inbox; the mailbox names it Inbox. The
    // user leaves before the first check answers.
    let (mut app, check) = app();
    app.mailboxes[0].path = FolderPath::Named(vec!["Inbox".into()]);
    switch_to(&mut app, work(), folder("work", "Work", 0));
    respond(
        &mut app,
        check,
        Event::MailboxesChecked(vec![
            (
                FolderPath::Named(vec!["Inbox".into()]),
                checked("in", "Inbox", 3, Some(2)),
            ),
            (work(), checked("work", "Work", 0, None)),
        ]),
    );
    assert_eq!(app.mailboxes_with_new_mail(), 0);
}

#[test]
fn a_message_in_both_lists_comes_as_last_changed() {
    let (mut app, _) = app();
    let id = poll(&mut app);
    let unread = summary("m3", 3);
    let mut read = unread.clone();
    read.flags.read = true;
    read.modified = Some(DateTime::UNIX_EPOCH + TimeDelta::minutes(9));
    polled_with(
        &mut app,
        id,
        page(vec![
            unread,
            summary("m2", 2),
            summary("m1", 1),
            summary("m0", 0),
        ]),
        page(vec![read]),
    );
    assert!(app.messages[0].flags.read);
    assert_ne!(app.status, NEW_MAIL);
}

#[test]
fn a_known_message_in_both_lists_comes_as_last_changed() {
    let (mut app, _) = app();
    let id = poll(&mut app);
    let mut read = summary("m1", 1);
    read.flags.read = true;
    read.modified = Some(DateTime::UNIX_EPOCH + TimeDelta::minutes(9));
    // The changes saw it before it was read.
    polled_with(
        &mut app,
        id,
        page(vec![summary("m2", 2), read, summary("m0", 0)]),
        page(vec![summary("m1", 1)]),
    );
    assert!(app.messages[1].flags.read);
}

#[test]
fn too_many_changes_to_tell_count_as_new_mail() {
    let (mut app, check) = app();
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 3, Some(2)),
        checked("work", "Work", 0, None),
    );
    let recent: Vec<(&str, i64)> = vec![("m2", 30); mett_core::RECENT_UNREAD];
    leave_and_check(&mut app, recent);
    assert_eq!(app.mailboxes_with_new_mail(), 1);
}

/// Has a check find Work's `recent` unread mail.
fn check_work(app: &mut App, check: RequestId, found: &[(&str, i64)]) {
    check_both(
        app,
        check,
        checked("in", "Inbox", 1, Some(1)),
        Ok(Checked {
            folder: folder("work", "Work", 2),
            recent: recent(found),
        }),
    );
}

#[test]
fn older_mail_coming_into_the_sample_did_not_arrive() {
    let (mut app, check) = app();
    check_work(&mut app, check, &[("m10", 10), ("m9", 9)]);
    app.status.clear();
    // m10 is read elsewhere: m8, older, now makes the sample.
    let check = next_check(&mut app);
    check_work(&mut app, check, &[("m9", 9), ("m8", 8)]);
    assert!(app.status.is_empty(), "{}", app.status);
    // m11 arrives.
    let check = next_check(&mut app);
    check_work(&mut app, check, &[("m11", 11), ("m9", 9)]);
    assert_eq!(app.status, "New mail in =Work.");
}

#[test]
fn new_mail_leaves_an_error_on_the_message_line() {
    let (mut app, check) = app();
    check_work(&mut app, check, &[("m9", 9)]);
    app.fail("The message may have been sent. Check Sent Items before sending it again.");
    let check = next_check(&mut app);
    check_work(&mut app, check, &[("m11", 11), ("m9", 9)]);
    assert!(app.status.starts_with("The message may"), "{}", app.status);
    assert_eq!(app.mailboxes_with_new_mail(), 1);
}

#[test]
fn a_change_to_unread_mail_already_there_is_not_announced() {
    let (mut app, check) = app();
    // Work had unread m5 before the user ever opened it.
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, Some(1)),
        checked("work", "Work", 1, Some(5)),
    );
    assert_eq!(app.mailboxes_with_new_mail(), 1);
    app.status.clear();
    // m5 is flagged in Outlook: it changed, but didn't arrive.
    let check = next_check(&mut app);
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, Some(1)),
        Ok(Checked {
            folder: folder("work", "Work", 1),
            recent: recent(&[("m5", 30)]),
        }),
    );
    assert!(app.status.is_empty(), "{}", app.status);
}

#[test]
fn a_failed_first_check_is_no_baseline() {
    let (mut app, check) = app();
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, Some(1)),
        Err(Error::Network("down".into())),
    );
    app.status.clear();
    let check = next_check(&mut app);
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, Some(1)),
        checked("work", "Work", 2, Some(2)),
    );
    // Unread mail it had all along: shown, not announced.
    assert_eq!(app.mailboxes_with_new_mail(), 1);
    assert!(app.status.is_empty(), "{}", app.status);
}

#[test]
fn sent_mail_goes_before_the_drafts() {
    let (mut app, _) = app();
    // In the order the folder is listed in.
    app.apply_ordering(unsorted());
    app.messages.clear();
    app.positions.clear();
    let draft = draft("d", 9);
    app.messages.push(draft.clone());
    app.positions.insert(draft.id.clone(), 0);
    let id = poll(&mut app);
    polled(&mut app, id, vec![summary("moved", -50), draft]);
    assert_eq!(ids(&app), ["moved", "d"]);
}

#[test]
fn a_check_of_mailboxes_since_changed_is_left_aside() {
    let (mut app, check) = app();
    let mut fewer = app.mailboxes.clone();
    fewer.truncate(1);
    app.apply_mailboxes(fewer, app.sync_interval);
    check_work(&mut app, check, &[("m9", 9)]);
    assert_eq!(app.mailboxes_with_new_mail(), 0);
    // The next check is of the mailboxes as they are.
    let effects = app.update(Msg::Timer(Timer::Poll(0)));
    assert!(
        requests(&effects)
            .iter()
            .any(|(_, r)| matches!(r, Request::CheckMailboxes(m) if m.len() == 1))
    );
}

#[test]
fn nopoll_counts_for_its_own_entry() {
    let (mut app, check) = app();
    // Work again, by another name, not checked.
    let mut again = app.mailboxes[1].clone();
    again.text = "Work".into();
    again.poll = false;
    app.mailboxes.push(again);
    check_work(&mut app, check, &[("m9", 9)]);
    assert!(app.has_new_mail(&app.mailboxes[1]));
    assert!(!app.has_new_mail(&app.mailboxes[2]));
    assert_eq!(app.mailboxes_with_new_mail(), 1);
}

#[test]
fn the_browser_starts_on_the_mailboxes() {
    let (mut app, check) = app();
    check_both(
        &mut app,
        check,
        checked("in", "Inbox", 1, Some(1)),
        Err(Error::not_found("folder", "Work")),
    );
    assert!(app.status.starts_with("Couldn't check =Work"));
    assert!(!app.has_new_mail(&app.mailboxes[1]));
    // New mail elsewhere leaves the failure on the message line.
    app.announce("New mail in =Elsewhere.");
    assert!(app.status.starts_with("Couldn't check =Work"));
    let effects = act(&mut app, Action::BrowseMailboxes);
    // With fresh counts.
    assert!(matches!(
        requests(&effects)[..],
        [(_, Request::CheckMailboxes(_))]
    ));
    let Screen::Browser(browser) = app.screen() else {
        panic!("expected the browser");
    };
    let names: Vec<_> = browser
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e.counts))
        .collect();
    assert_eq!(names, [("=@inbox", Some((1, 0))), ("=Work", None)]);
    let effects = act(&mut app, Action::ToggleMailboxes);
    assert!(matches!(
        requests(&effects)[..],
        [(_, Request::ListFolders)]
    ));
    let Screen::Browser(browser) = app.screen() else {
        panic!("expected the browser");
    };
    assert!(!browser.mailboxes);
}

#[test]
fn space_cycles_through_mailboxes_with_new_mail() {
    let (mut app, check) = app();
    let mut lists = folder("lists", "Lists", 2);
    lists.id = FolderId::new("lists");
    app.mailboxes.push(MailboxEntry {
        name: "=Lists".into(),
        label: None,
        text: "=Lists".into(),
        path: FolderPath::Named(vec!["Lists".into()]),
        notify: true,
        poll: true,
    });
    respond(
        &mut app,
        check,
        Event::MailboxesChecked(vec![
            (work(), checked("work", "Work", 1, Some(1))),
            (
                FolderPath::Named(vec!["Lists".into()]),
                Ok(Checked {
                    folder: lists,
                    recent: vec![(MessageId::new("l"), DateTime::UNIX_EPOCH)],
                }),
            ),
        ]),
    );
    act(&mut app, Action::ChangeFolder);
    let text = |app: &App| match &app.input {
        Some(Input::Line { editor, .. }) => editor.text(),
        _ => panic!("expected a question"),
    };
    assert_eq!(text(&app), "=Work");
    key(&mut app, KeyCode::Char(' '));
    assert_eq!(text(&app), "=Lists");
    key(&mut app, KeyCode::Char(' '));
    assert_eq!(text(&app), "=Work");
}

#[test]
fn an_announcement_of_new_mail_marks_the_account_for_its_tab() {
    let mut app = opened(Settings::default(), 24, Vec::new(), None).0;
    assert!(!app.arrived);
    app.announce("New mail in the Inbox.");
    assert!(app.arrived);
}
