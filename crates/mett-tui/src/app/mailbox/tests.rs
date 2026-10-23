use chrono::{DateTime, TimeDelta};
use mett_core::{AccountId, Error, Message, WellKnownFolder, mock};
use ratatui::crossterm::event::KeyCode;

use super::*;
use crate::app::Settings;
use crate::app::testing::*;
use crate::keymap::Menu;
use crate::worker::Event;

/// An app with Inbox messages m0, m1 and m2, newest first.
fn app(settings: Settings) -> App {
    let inbox = FolderPath::WellKnown(WellKnownFolder::Inbox, vec![]);
    let (mut app, effects) = App::open(80, 24, inbox, settings);
    let open = first_request(&effects);
    let folder = mett_core::mock::folder("in", "Inbox", Some(WellKnownFolder::Inbox));
    let list = first_request(&respond(&mut app, open, Event::FolderOpened(Ok(folder))));
    let messages = (0..3)
        .map(|i| summary_at(&format!("m{i}"), "s", 10 - i))
        .collect();
    listed(&mut app, list, messages, None);
    app
}

/// An app that purges without asking.
fn yes() -> App {
    app(Settings {
        delete: Quad::Yes,
        ..settings()
    })
}

/// Answers the transfer of `message` to the folder `to`: it is
/// `new` there, or failed.
fn transferred(
    app: &mut App,
    id: RequestId,
    to: &str,
    message: &str,
    result: mett_core::Result<&str>,
) -> Vec<Effect> {
    let moved = vec![(message.into(), result.map(MessageId::new))];
    respond(app, id, Event::Transferred(Ok((FolderId::new(to), moved))))
}

/// Answers the purge of `message`: done.
fn purged(app: &mut App, id: RequestId, message: &str) -> Vec<Effect> {
    respond(app, id, Event::Purged(Ok(vec![(message.into(), Ok(()))])))
}

fn m0() -> Message {
    mock::message("m0", "in", "s", DateTime::UNIX_EPOCH)
}

/// Shows m0 in the pager; returns the effects of its delivery.
fn show_m0(app: &mut App) -> Vec<Effect> {
    let (fetch, _) = sent(&act(app, Action::DisplayMessage));
    respond(app, fetch, Event::MessageFetched(Ok(m0())))
}

fn settings() -> Settings {
    Settings {
        account: AccountId::new("alice@contoso.com"),
        folder: "msgraph://alice@contoso.com/".into(),
        ..Settings::default()
    }
}

#[test]
fn marks_and_purges_deleted_messages() {
    let mut app = app(settings());
    act(&mut app, Action::DeleteMessage);
    assert!(app.is_deleted(&"m0".into()));
    assert_eq!(app.selected, 1, "moved on");
    act(&mut app, Action::DeleteMessage);
    act(&mut app, Action::PreviousEntry);
    act(&mut app, Action::UndeleteMessage);
    assert_eq!(app.deleted_count(), 1);

    act(&mut app, Action::SyncMailbox);
    assert_eq!(prompt(&app), "Purge 1 deleted message?");
    let (id, request) = sent(&key(&mut app, KeyCode::Char('y')));
    // Without `trash`, purged messages are deleted for good.
    assert_eq!(
        request,
        Request::Purge {
            ids: vec!["m0".into()],
            from: FolderId::new("in"),
            trash: None,
        }
    );
    assert!(app.saving());
    // Only one purge at a time.
    act(&mut app, Action::SyncMailbox);
    assert!(app.input.is_none());
    assert_eq!(app.status, "Deleted messages are already being purged.");
    purged(&mut app, id, "m0");
    assert_eq!(ids(&app), ["m1", "m2"]);
    assert_eq!(app.status, "1 message purged.");
    // `u` moved on to m2, which stays selected.
    assert_eq!(ids(&app)[app.selected], "m2");
}

#[test]
fn purging_moves_messages_to_the_trash() {
    let mut app = app(Settings {
        trash: Some(FolderPath::WellKnown(WellKnownFolder::DeletedItems, vec![])),
        delete: Quad::Yes,
        ..settings()
    });
    act(&mut app, Action::DeleteMessage);
    let (id, request) = sent(&act(&mut app, Action::SyncMailbox));
    assert!(matches!(request, Request::Purge { trash: Some(ref to), .. }
        if *to == FolderPath::WellKnown(WellKnownFolder::DeletedItems, vec![])));
    purged(&mut app, id, "m0");
    assert_eq!(ids(&app), ["m1", "m2"]);
}

#[test]
fn quitting_offers_to_purge_first() {
    let mut app = app(settings());
    act(&mut app, Action::DeleteMessage);
    act(&mut app, Action::Quit);
    assert!(!app.quit);
    let (id, _) = sent(&key(&mut app, KeyCode::Enter));
    assert!(!app.quit, "not before the purge is done");
    respond(
        &mut app,
        id,
        Event::Purged(Ok(vec![("m0".into(), Err(offline()))])),
    );
    assert!(!app.quit, "the purge failed: stay");
    assert_eq!(app.status, "1 of 1 message failed: network error: offline");
    assert!(app.is_deleted(&"m0".into()));

    // Declining to purge quits straight away.
    act(&mut app, Action::Quit);
    key(&mut app, KeyCode::Char('n'));
    assert!(app.quit);
}

#[test]
fn flags_and_toggles_new() {
    let mut app = app(settings());
    let (id, request) = sent(&act(&mut app, Action::FlagMessage));
    assert!(matches!(
        request,
        Request::UpdateFlags(_, update) if update == FlagsUpdate::flagged(true)
    ));
    assert!(app.messages[0].flags.flagged);
    respond(&mut app, id, Event::FlagsUpdated(Err(offline())));
    assert!(!app.messages[0].flags.flagged, "undone");

    let (_, request) = sent(&act(&mut app, Action::ToggleNew));
    assert!(matches!(
        request,
        Request::UpdateFlags(_, update) if update == FlagsUpdate::read(true)
    ));
    assert!(app.messages[0].flags.read);
}

#[test]
fn saves_and_copies_to_other_folders() {
    let mut app = app(settings());
    act(&mut app, Action::SaveMessage);
    assert_eq!(prompt(&app), "Save to mailbox ('?' for list): ");
    let (id, request) = sent(&answer(&mut app, "=Archive/2026"));
    assert_eq!(
        request,
        Request::Transfer {
            ids: vec!["m0".into()],
            to: FolderPath::Named(vec!["Archive".into(), "2026".into()]),
            copy: false,
        }
    );
    transferred(&mut app, id, "arc", "m0", Ok("a0"));
    assert_eq!(ids(&app), ["m1", "m2"]);
    assert_eq!(app.status, "Message saved.");

    act(&mut app, Action::CopyMessage);
    let (id, request) = sent(&answer(&mut app, "=@archive"));
    assert!(matches!(request, Request::Transfer { copy: true, .. }));
    transferred(&mut app, id, "arc", "m1", Ok("c1"));
    assert_eq!(ids(&app), ["m1", "m2"], "copies stay");

    act(&mut app, Action::SaveMessage);
    assert!(answer(&mut app, "msgraph://bob@contoso.com/@inbox").is_empty());
    assert!(
        app.status.contains("can only use folders of"),
        "{}",
        app.status
    );
}

#[test]
fn changes_folder() {
    let mut app = app(settings());
    act(&mut app, Action::ChangeFolder);
    let (id, request) = sent(&answer(&mut app, "=@sentitems"));
    assert_eq!(
        request,
        Request::OpenFolder(FolderPath::WellKnown(WellKnownFolder::SentItems, vec![]))
    );
    // The Inbox stays until Sent Items is found.
    assert_eq!(ids(&app), ["m0", "m1", "m2"]);
    let effects = respond(
        &mut app,
        id,
        Event::FolderOpened(Ok(mock::folder(
            "sent",
            "Sent Items",
            Some(WellKnownFolder::SentItems),
        ))),
    );
    assert!(matches!(sent(&effects).1, Request::ListMessages { .. }));
    assert!(app.messages.is_empty());
    assert_eq!(app.folder.as_ref().unwrap().name, "Sent Items");

    // A folder that isn't there leaves the user where they were.
    act(&mut app, Action::ChangeFolder);
    let (id, _) = sent(&answer(&mut app, "=Nowhere"));
    respond(
        &mut app,
        id,
        Event::FolderOpened(Err(Error::not_found("folder", "Nowhere"))),
    );
    assert_eq!(app.folder.as_ref().unwrap().name, "Sent Items");
    assert_eq!(app.status, "folder not found: Nowhere");
}

fn folders() -> Vec<Folder> {
    let child = |id: &str, name: &str, parent: &str| Folder {
        parent: Some(FolderId::new(parent)),
        ..mock::folder(id, name, None)
    };
    vec![
        mock::folder("in", "Inbox", Some(WellKnownFolder::Inbox)),
        child("p", "Projects", "in"),
        child("m", "mett", "p"),
        mock::folder("arc", "Archive", None),
    ]
}

#[test]
fn browses_folders_as_a_tree() {
    let mut app = app(settings());
    let (id, request) = sent(&act(&mut app, Action::BrowseMailboxes));
    assert_eq!(request, Request::ListFolders);
    assert_eq!(app.menu(), Menu::Browser);
    respond(&mut app, id, Event::FoldersListed(Ok(folders())));
    let Screen::Browser(browser) = app.screen() else {
        panic!("not in the browser");
    };
    let paths: Vec<_> = browser
        .entries
        .iter()
        .map(|e| (e.depth, e.path.as_str()))
        .collect();
    assert_eq!(
        paths,
        [
            (0, "Archive"),
            (0, "Inbox"),
            (1, "Inbox/Projects"),
            (2, "Inbox/Projects/mett"),
        ]
    );
    assert_eq!(browser.selected, 1, "starts on the open folder");
    act(&mut app, Action::NextEntry);
    let (_, request) = sent(&act(&mut app, Action::DisplayMessage));
    assert_eq!(
        request,
        Request::OpenFolder(FolderPath::Named(vec!["Inbox".into(), "Projects".into()]))
    );
    assert_eq!(app.menu(), Menu::Index);
}

#[test]
fn the_browser_answers_a_mailbox_question() {
    let mut app = app(settings());
    act(&mut app, Action::SaveMessage);
    let (id, _) = sent(&key(&mut app, KeyCode::Char('?')));
    respond(&mut app, id, Event::FoldersListed(Ok(folders())));
    act(&mut app, Action::FirstEntry);
    act(&mut app, Action::DisplayMessage);
    assert_eq!(prompt(&app), "Save to mailbox ('?' for list): Archive");

    // Leaving the browser keeps what was typed.
    key(&mut app, KeyCode::Esc);
    act(&mut app, Action::CopyMessage);
    key(&mut app, KeyCode::Char('='));
    let (id, _) = sent(&key(&mut app, KeyCode::Tab));
    respond(&mut app, id, Event::FoldersListed(Ok(folders())));
    act(&mut app, Action::Exit);
    assert_eq!(prompt(&app), "Copy to mailbox ('?' for list): =");
}

#[test]
fn browser_paths_work_whatever_folder_is() {
    // `folder` points below the mailbox root (or at ~/Mail): the
    // browser's paths still name the right folders.
    let mut app = app(Settings {
        folder: "msgraph://alice@contoso.com/Inbox".into(),
        ..settings()
    });
    let (id, _) = sent(&act(&mut app, Action::BrowseMailboxes));
    respond(&mut app, id, Event::FoldersListed(Ok(folders())));
    act(&mut app, Action::FirstEntry);
    let (_, request) = sent(&act(&mut app, Action::DisplayMessage));
    assert_eq!(
        request,
        Request::OpenFolder(FolderPath::Named(vec!["Archive".into()]))
    );
}

#[test]
fn flag_changes_to_a_message_go_out_one_at_a_time() {
    let mut app = app(settings());
    let (first, _) = sent(&act(&mut app, Action::FlagMessage));
    // Toggled back before the first change is saved: it waits.
    assert!(act(&mut app, Action::FlagMessage).is_empty());
    assert!(!app.messages[0].flags.flagged);
    let effects = respond(&mut app, first, Event::FlagsUpdated(Ok(())));
    let (second, request) = sent(&effects);
    assert!(matches!(
        request,
        Request::UpdateFlags(
            _,
            FlagsUpdate {
                flagged: Some(false),
                ..
            }
        )
    ));
    // Flagged again while that goes out, and it fails: the mailbox
    // still has the flag from the first update, which is what the index
    // shows, so nothing more is sent.
    act(&mut app, Action::FlagMessage);
    let effects = respond(&mut app, second, Event::FlagsUpdated(Err(offline())));
    assert!(effects.is_empty(), "{effects:?}");
    assert!(app.messages[0].flags.flagged);
    assert!(!app.saving());

    // Toggled three times while the first is on its way, and the
    // first fails: what differs from the mailbox goes out.
    let (first, _) = sent(&act(&mut app, Action::ToggleNew));
    act(&mut app, Action::ToggleNew);
    act(&mut app, Action::ToggleNew);
    let effects = respond(&mut app, first, Event::FlagsUpdated(Err(offline())));
    assert!(matches!(
        sent(&effects).1,
        Request::UpdateFlags(_, update) if update == FlagsUpdate::read(true)
    ));
}

#[test]
fn transfers_that_needed_a_sign_in_are_retried() {
    let mut app = app(settings());
    act(&mut app, Action::SaveMessage);
    let (id, request) = sent(&answer(&mut app, "=Archive"));
    signed_out(&mut app);
    respond(&mut app, id, Event::Transferred(Err(expired())));
    let effects = act(&mut app, Action::Retry);
    assert_eq!(sent(&effects).1, request);
}

#[test]
fn a_move_waits_for_the_message_s_flag_update() {
    let mut app = app(settings());
    let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
    act(&mut app, Action::SaveMessage);
    assert!(answer(&mut app, "=Archive").is_empty(), "waits");
    assert!(app.saving());
    let effects = respond(&mut app, flag, Event::FlagsUpdated(Ok(())));
    assert!(matches!(sent(&effects).1, Request::Transfer { .. }));
}

#[test]
fn failed_moves_are_reported_on_exit() {
    let mut app = app(settings());
    act(&mut app, Action::SaveMessage);
    let (id, _) = sent(&answer(&mut app, "=Archive"));
    transferred(&mut app, id, "arc", "m0", Err(offline()));
    assert_eq!(app.save_failures(), ["network error: offline"]);
}

#[test]
fn a_failed_flag_change_goes_out_again_with_the_next() {
    let mut app = app(settings());
    let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
    act(&mut app, Action::ToggleNew);
    let effects = respond(&mut app, flag, Event::FlagsUpdated(Err(offline())));
    assert!(matches!(
        sent(&effects).1,
        Request::UpdateFlags(
            _,
            FlagsUpdate {
                flagged: Some(true),
                read: Some(true)
            }
        )
    ));
}

#[test]
fn transfers_into_the_open_folder_list_it_again() {
    let mut app = app(settings());
    act(&mut app, Action::CopyMessage);
    let (id, _) = sent(&answer(&mut app, "=@inbox"));
    let effects = transferred(&mut app, id, "in", "m0", Ok("c0"));
    assert!(matches!(sent(&effects).1, Request::ListMessages { .. }));
}

#[test]
fn failures_follow_messages_moved_on_again() {
    let mut app = app(settings());
    let archive = Some(FolderPath::Named(vec!["Archive".into()]));
    let other = Some(FolderPath::Named(vec!["Other".into()]));
    let failure = |id: &str, transfer, to: &Option<FolderPath>| {
        (
            (MessageId::new(id), transfer, to.clone()),
            format!("{id} failed"),
        )
    };
    app.transfer_failures = HashMap::from([
        failure("a", Transfer::Save, &archive),
        failure("a", Transfer::Purge, &None),
        failure("b", Transfer::Copy, &other),
        failure("b", Transfer::Purge, &None),
        failure("z", Transfer::Save, &archive),
    ]);
    let unsaved = ("offline".to_owned(), FlagsUpdate::read(true));
    app.save_failures.insert("a".into(), unsaved.clone());
    // a became b, and b (another message) became c: what failed for
    // a goes along to c, as what failed for b does.
    app.follow_moves(&[("a".into(), "b".into()), ("b".into(), "c".into())]);
    let mut failures: Vec<_> = app
        .transfer_failures
        .iter()
        .map(|((id, transfer, to), e)| format!("{id:?} {transfer:?} {to:?}: {e}"))
        .collect();
    failures.sort();
    let expected = [
        (failure("c", Transfer::Copy, &other).0, "b failed".into()),
        (failure("c", Transfer::Save, &archive).0, "a failed".into()),
        failure("z", Transfer::Save, &archive),
    ]
    .map(|((id, transfer, to), e)| format!("{id:?} {transfer:?} {to:?}: {e}"));
    assert_eq!(failures, expected);
    assert_eq!(
        app.save_failures,
        HashMap::from([(MessageId::new("c"), unsaved)])
    );
}

#[test]
fn a_failed_flag_change_follows_the_move() {
    let mut app = app(settings());
    let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
    act(&mut app, Action::SaveMessage);
    assert!(answer(&mut app, "=Archive").is_empty(), "waits its turn");
    let effects = respond(&mut app, flag, Event::FlagsUpdated(Err(offline())));
    let (moving, _) = sent(&effects);
    transferred(&mut app, moving, "archive", "m0", Ok("a0"));
    assert!(!app.saving());
    // Trying again changes the moved message.
    assert!(app.save_failures.contains_key(&MessageId::new("a0")));
    assert!(!app.save_failures.contains_key(&MessageId::new("m0")));
}

#[test]
fn a_purge_waiting_for_flags_counts_as_in_progress() {
    let mut app = yes();
    sent(&act(&mut app, Action::FlagMessage));
    act(&mut app, Action::DeleteMessage);
    assert!(
        act(&mut app, Action::SyncMailbox).is_empty(),
        "waits for the flags"
    );
    act(&mut app, Action::SyncMailbox);
    assert_eq!(app.status, "Deleted messages are already being purged.");
}

#[test]
fn messages_already_gone_count_as_purged() {
    let mut app = yes();
    act(&mut app, Action::DeleteMessage);
    let (id, _) = sent(&act(&mut app, Action::SyncMailbox));
    respond(
        &mut app,
        id,
        Event::Purged(Ok(vec![(
            "m0".into(),
            Err(Error::not_found("message", "m0")),
        )])),
    );
    assert_eq!(app.deleted_count(), 0);
    assert_eq!(ids(&app), ["m1", "m2"]);
}

#[test]
fn a_copy_does_not_make_up_for_a_failed_move() {
    let mut app = app(settings());
    act(&mut app, Action::SaveMessage);
    let (id, _) = sent(&answer(&mut app, "=Archive"));
    transferred(&mut app, id, "arc", "m0", Err(offline()));
    act(&mut app, Action::CopyMessage);
    let (id, _) = sent(&answer(&mut app, "=Archive"));
    transferred(&mut app, id, "arc", "m0", Ok("c0"));
    assert_eq!(app.save_failures(), ["network error: offline"]);
}

#[test]
fn quitting_retries_transfers_too() {
    let mut app = app(settings());
    act(&mut app, Action::SaveMessage);
    let (id, request) = sent(&answer(&mut app, "=Archive"));
    act(&mut app, Action::Quit);
    assert!(app.quit);
    signed_out(&mut app);
    respond(&mut app, id, Event::Transferred(Err(expired())));
    let effects = act(&mut app, Action::Retry);
    assert_eq!(sent(&effects).1, request);
}

#[test]
fn a_copy_into_the_open_folder_waits_for_the_pager_to_close() {
    let mut app = app(settings());
    let (read, _) = sent(&show_m0(&mut app));
    respond(&mut app, read, Event::FlagsUpdated(Ok(())));
    act(&mut app, Action::CopyMessage);
    let (id, _) = sent(&answer(&mut app, "=@inbox"));
    let effects = transferred(&mut app, id, "in", "m0", Ok("c0"));
    assert!(effects.is_empty(), "not under the pager: {effects:?}");
    assert_eq!(ids(&app), ["m0", "m1", "m2"]);
    let effects = act(&mut app, Action::Exit);
    assert!(matches!(sent(&effects).1, Request::ListMessages { .. }));
}

#[test]
fn a_failed_save_is_retried_after_changing_folder() {
    let mut app = app(settings());
    let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
    act(&mut app, Action::ChangeFolder);
    answer(&mut app, "=Archive");
    signed_out(&mut app);
    respond(&mut app, flag, Event::FlagsUpdated(Err(expired())));
    let effects = act(&mut app, Action::Retry);
    assert!(
        effects.iter().any(|e| matches!(e,
            Effect::Request(_, Request::UpdateFlags(id, _)) if id.as_str() == "m0")),
        "{effects:?}"
    );
}

#[test]
fn leaving_waits_if_the_purge_left_a_message_behind() {
    let mut app = yes();
    act(&mut app, Action::DeleteMessage);
    act(&mut app, Action::DeleteMessage);
    let (purge, _) = sent(&act(&mut app, Action::Quit));
    respond(
        &mut app,
        purge,
        Event::Purged(Ok(vec![
            ("m0".into(), Err(offline())),
            ("m1".into(), Ok(())),
        ])),
    );
    assert!(!app.quit, "m0 is still marked: stay");
    assert!(app.is_deleted(&"m0".into()));
    assert!(!app.leaving());
}

#[test]
fn a_purge_after_a_failed_move_still_goes_ahead() {
    let mut app = yes();
    let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
    act(&mut app, Action::DeleteMessage);
    act(&mut app, Action::PreviousEntry);
    act(&mut app, Action::SaveMessage);
    answer(&mut app, "=Archive");
    assert!(act(&mut app, Action::Quit).is_empty(), "waits its turn");
    assert!(app.leaving());
    let (moving, _) = sent(&respond(
        &mut app,
        flag,
        Event::FlagsUpdated(Err(offline())),
    ));
    // The move fails: m0 stays, marked, and is purged.
    let (purge, request) = sent(&respond(
        &mut app,
        moving,
        Event::Transferred(Err(offline())),
    ));
    assert!(matches!(request, Request::Purge { ref ids, .. } if ids == &[MessageId::new("m0")]));
    purged(&mut app, purge, "m0");
    assert!(app.quit);
}

#[test]
fn a_purge_leaves_out_messages_moved_away_before_it() {
    let mut app = yes();
    act(&mut app, Action::DeleteMessage);
    act(&mut app, Action::PreviousEntry);
    act(&mut app, Action::SaveMessage);
    let (moving, _) = sent(&answer(&mut app, "=Archive"));
    assert!(act(&mut app, Action::Quit).is_empty(), "waits its turn");
    let effects = transferred(&mut app, moving, "archive", "m0", Ok("a0"));
    assert!(effects.is_empty(), "nothing left to purge: {effects:?}");
    assert_eq!(app.status, "Mailbox is unchanged.");
    assert!(app.quit);
}

#[test]
fn a_purge_waits_for_a_copy() {
    let mut app = yes();
    act(&mut app, Action::DeleteMessage);
    act(&mut app, Action::PreviousEntry);
    act(&mut app, Action::CopyMessage);
    let (copy, _) = sent(&answer(&mut app, "=Archive"));
    assert!(
        act(&mut app, Action::SyncMailbox).is_empty(),
        "waits its turn"
    );
    let (_, request) = sent(&transferred(&mut app, copy, "archive", "m0", Ok("c0")));
    assert!(matches!(request, Request::Purge { ref ids, .. } if ids == &[MessageId::new("m0")]));
}

#[test]
fn resolve_waits_for_the_next_page() {
    let inbox = FolderPath::WellKnown(WellKnownFolder::Inbox, vec![]);
    let (mut app, effects) = App::open(80, 24, inbox, settings());
    let (open, _) = sent(&effects);
    let effects = respond(
        &mut app,
        open,
        Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
    );
    // The listing, and the user's own addresses alongside.
    let Effect::Request(first, _) = effects[0] else {
        panic!("expected a request, got {effects:?}");
    };
    let message = |id: &str, minutes| {
        mock::message(
            id,
            "in",
            "s",
            DateTime::UNIX_EPOCH + TimeDelta::minutes(minutes),
        )
        .summary
    };
    let effects = listed(&mut app, first, vec![message("m0", 9)], Some("c"));
    let (second, _) = sent(&effects);
    act(&mut app, Action::DeleteMessage);
    assert_eq!(app.selected, 0);
    listed(&mut app, second, vec![message("m1", 8)], None);
    assert_eq!(app.selected, 1, "moved on once it arrived");
}

#[test]
fn the_browser_opens_again_after_a_sign_in() {
    let mut app = app(settings());
    let (id, _) = sent(&act(&mut app, Action::BrowseMailboxes));
    signed_out(&mut app);
    respond(&mut app, id, Event::FoldersListed(Err(expired())));
    assert_eq!(app.screen(), &Screen::Index);
    let (_, request) = sent(&act(&mut app, Action::Retry));
    assert_eq!(request, Request::ListFolders);
    assert_eq!(app.menu(), Menu::Browser);
}

#[test]
fn a_message_being_moved_is_left_alone() {
    let mut app = app(settings());
    act(&mut app, Action::DeleteMessage);
    act(&mut app, Action::PreviousEntry);
    act(&mut app, Action::SaveMessage);
    let (moving, _) = sent(&answer(&mut app, "=Archive"));
    // Marked until the move succeeds, but not purged meanwhile.
    assert!(app.is_deleted(&"m0".into()));
    for action in [
        Action::DeleteMessage,
        Action::FlagMessage,
        Action::SaveMessage,
        Action::CopyMessage,
    ] {
        assert!(act(&mut app, action).is_empty());
        assert_eq!(
            app.status,
            "The message is still being moved, copied or purged."
        );
    }
    act(&mut app, Action::SyncMailbox);
    assert!(
        key(&mut app, KeyCode::Char('y')).is_empty(),
        "waits for the move"
    );
    // A move that fails keeps the mark, and the purge takes it.
    let effects = respond(&mut app, moving, Event::Transferred(Err(offline())));
    assert!(app.is_deleted(&"m0".into()));
    assert!(matches!(sent(&effects).1, Request::Purge { .. }));
}

#[test]
fn a_folder_named_like_a_well_known_one_is_itself() {
    let mut app = app(settings());
    // Browse from the question.
    act(&mut app, Action::SaveMessage);
    let (id, _) = sent(&key(&mut app, KeyCode::Tab));
    respond(
        &mut app,
        id,
        Event::FoldersListed(Ok(vec![mock::folder("odd", "@archive", None)])),
    );
    act(&mut app, Action::DisplayMessage);
    let (_, request) = sent(&key(&mut app, KeyCode::Enter));
    assert!(matches!(request, Request::Transfer { ref to, .. }
        if *to == FolderPath::Named(vec!["@archive".into()])));
}

#[test]
fn a_listing_error_shows_on_the_question() {
    let mut app = app(settings());
    act(&mut app, Action::CopyMessage);
    let (id, _) = sent(&key(&mut app, KeyCode::Tab));
    respond(&mut app, id, Event::FoldersListed(Err(offline())));
    assert_eq!(
        prompt(&app),
        "network error: offline. Copy to mailbox ('?' for list): "
    );
}

#[test]
fn a_copy_into_the_open_folder_shows_once_the_browser_closes() {
    let mut app = app(settings());
    act(&mut app, Action::CopyMessage);
    let (copy, _) = sent(&answer(&mut app, "=@inbox"));
    sent(&act(&mut app, Action::BrowseMailboxes));
    transferred(&mut app, copy, "in", "m0", Ok("c0"));
    let effects = act(&mut app, Action::Exit);
    assert!(matches!(sent(&effects).1, Request::ListMessages { .. }));
}

#[test]
fn the_last_flag_choice_survives_a_folder_change() {
    let mut app = app(settings());
    let (first, _) = sent(&act(&mut app, Action::FlagMessage));
    act(&mut app, Action::FlagMessage);
    act(&mut app, Action::ChangeFolder);
    let (open, _) = sent(&answer(&mut app, "=Archive"));
    respond(
        &mut app,
        open,
        Event::FolderOpened(Ok(mock::folder("arc", "Archive", None))),
    );
    // The first update lands; the user's last choice (unflagged) goes
    // out though m0 is no longer in the index.
    let effects = respond(&mut app, first, Event::FlagsUpdated(Ok(())));
    assert!(
        matches!(sent(&effects).1, Request::UpdateFlags(ref id, FlagsUpdate { flagged: Some(false), .. }) if id.as_str() == "m0"),
        "{effects:?}"
    );
}

#[test]
fn an_unrelated_save_does_not_hide_a_failed_one() {
    let mut app = app(settings());
    let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
    respond(&mut app, flag, Event::FlagsUpdated(Err(offline())));
    let (read, _) = sent(&act(&mut app, Action::ToggleNew));
    respond(&mut app, read, Event::FlagsUpdated(Ok(())));
    assert_eq!(app.save_failures(), ["network error: offline"]);
}

#[test]
fn a_message_being_copied_is_not_moved() {
    let mut app = app(settings());
    act(&mut app, Action::CopyMessage);
    sent(&answer(&mut app, "=Archive"));
    assert!(act(&mut app, Action::SaveMessage).is_empty());
    assert_eq!(
        app.status,
        "The message is still being moved, copied or purged."
    );
    // Flags can change while it is only copied (once it is).
    assert!(act(&mut app, Action::FlagMessage).is_empty());
    assert!(app.messages[0].flags.flagged);
}

#[test]
fn a_folder_change_that_needed_a_sign_in_is_retried() {
    let mut app = app(settings());
    act(&mut app, Action::ChangeFolder);
    let (open, _) = sent(&answer(&mut app, "=Archive"));
    signed_out(&mut app);
    respond(&mut app, open, Event::FolderOpened(Err(expired())));
    let (_, request) = sent(&act(&mut app, Action::Retry));
    assert_eq!(
        request,
        Request::OpenFolder(FolderPath::Named(vec!["Archive".into()]))
    );
}

#[test]
fn a_browser_reopened_on_retry_takes_the_keys() {
    let mut app = app(settings());
    act(&mut app, Action::SaveMessage);
    let (list, _) = sent(&key(&mut app, KeyCode::Tab));
    signed_out(&mut app);
    respond(&mut app, list, Event::FoldersListed(Err(expired())));
    act(&mut app, Action::Retry);
    assert_eq!(app.menu(), Menu::Browser);
    assert!(app.input.is_none(), "the question waits for the browser");
}

#[test]
fn a_purge_on_its_way_cannot_be_undeleted() {
    let mut app = yes();
    act(&mut app, Action::DeleteMessage);
    act(&mut app, Action::PreviousEntry);
    sent(&act(&mut app, Action::SyncMailbox));
    act(&mut app, Action::UndeleteMessage);
    assert_eq!(app.status, "The message is already being purged.");
    assert!(app.is_deleted(&"m0".into()));
}

#[test]
fn a_failed_flag_change_follows_its_message_when_moved() {
    let mut app = app(settings());
    let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
    respond(&mut app, flag, Event::FlagsUpdated(Err(expired())));
    act(&mut app, Action::SaveMessage);
    let (save, _) = sent(&answer(&mut app, "=Archive"));
    transferred(&mut app, save, "arc", "m0", Ok("a0"));
    signed_out(&mut app);
    let effects = act(&mut app, Action::Retry);
    assert!(
        effects.iter().any(|e| matches!(e,
            Effect::Request(_, Request::UpdateFlags(id, _)) if id.as_str() == "a0")),
        "{effects:?}"
    );
}

#[test]
fn a_cancelled_question_forgets_the_browsed_folder() {
    let mut app = app(settings());
    act(&mut app, Action::SaveMessage);
    let (id, _) = sent(&key(&mut app, KeyCode::Tab));
    respond(
        &mut app,
        id,
        Event::FoldersListed(Ok(vec![mock::folder("odd", "@archive", None)])),
    );
    act(&mut app, Action::DisplayMessage);
    key(&mut app, KeyCode::Esc);
    act(&mut app, Action::SaveMessage);
    let (_, request) = sent(&answer(&mut app, "@archive"));
    assert!(matches!(request, Request::Transfer { ref to, .. }
        if *to == FolderPath::WellKnown(WellKnownFolder::Archive, vec![])));
}

#[test]
fn a_newer_folder_change_drops_an_older_retry() {
    let mut app = app(settings());
    act(&mut app, Action::ChangeFolder);
    let (a, _) = sent(&answer(&mut app, "=A"));
    respond(&mut app, a, Event::FolderOpened(Err(expired())));
    act(&mut app, Action::ChangeFolder);
    let (b, _) = sent(&answer(&mut app, "=B"));
    respond(
        &mut app,
        b,
        Event::FolderOpened(Ok(mock::folder("b", "B", None))),
    );
    signed_out(&mut app);
    let effects = act(&mut app, Action::Retry);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::Request(_, Request::OpenFolder(_)))),
        "{effects:?}"
    );
}

#[test]
fn no_marks_while_opening_another_folder() {
    let mut app = app(settings());
    act(&mut app, Action::ChangeFolder);
    sent(&answer(&mut app, "=Archive"));
    act(&mut app, Action::DeleteMessage);
    assert_eq!(app.status, "Leaving the folder; wait a moment.");
    assert_eq!(app.deleted_count(), 0);
}

#[test]
fn leaving_waits_for_moves_of_marked_messages() {
    let mut app = yes();
    act(&mut app, Action::DeleteMessage);
    act(&mut app, Action::PreviousEntry);
    act(&mut app, Action::SaveMessage);
    let (save, _) = sent(&answer(&mut app, "=Archive"));
    // m0 is marked and being moved: quitting waits for the move.
    assert!(act(&mut app, Action::Quit).is_empty());
    assert!(!app.quit);
    act(&mut app, Action::NextEntry);
    act(&mut app, Action::DeleteMessage);
    assert_eq!(app.status, "Leaving the folder; wait a moment.");
    // The move fails: m0 stays, still marked, and is purged.
    let effects = respond(&mut app, save, Event::Transferred(Err(offline())));
    let (purge, request) = sent(&effects);
    assert!(matches!(request, Request::Purge { ref ids, .. } if ids == &[MessageId::new("m0")]));
    purged(&mut app, purge, "m0");
    assert!(app.quit);
}

#[test]
fn a_message_being_purged_is_left_alone() {
    let mut app = yes();
    act(&mut app, Action::DeleteMessage);
    act(&mut app, Action::PreviousEntry);
    sent(&act(&mut app, Action::SyncMailbox));
    for action in [Action::SaveMessage, Action::FlagMessage] {
        assert!(act(&mut app, action).is_empty());
        assert_eq!(
            app.status,
            "The message is still being moved, copied or purged."
        );
    }
}

#[test]
fn a_save_question_keeps_its_message() {
    let mut app = app(settings());
    act(&mut app, Action::SaveMessage);
    // The index moves on meanwhile (say, a relist).
    app.selected = 2;
    let (_, request) = sent(&answer(&mut app, "=Archive"));
    assert!(matches!(request, Request::Transfer { ref ids, .. } if ids == &[MessageId::new("m0")]));
}

#[test]
fn a_relist_keeps_flag_changes_on_their_way() {
    let mut app = app(settings());
    let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
    // Listed again before the flag is saved; the listing still has m0
    // unflagged.
    let (list, _) = sent(&app.relist());
    listed(&mut app, list, vec![m0().summary], None);
    assert!(app.messages[0].flags.flagged, "still the user's choice");
    respond(&mut app, flag, Event::FlagsUpdated(Ok(())));
    assert!(app.messages[0].flags.flagged);
}

#[test]
fn a_listing_older_than_a_saved_flag_keeps_it() {
    let mut app = app(settings());
    let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
    let (list, _) = sent(&app.relist());
    // Saved before the listing, which was taken earlier, comes back.
    respond(&mut app, flag, Event::FlagsUpdated(Ok(())));
    listed(&mut app, list, vec![m0().summary], None);
    assert!(app.messages[0].flags.flagged);
    // The next listing is newer: it is taken as it is.
    let (list, _) = sent(&app.relist());
    listed(&mut app, list, vec![m0().summary], None);
    assert!(!app.messages[0].flags.flagged);
}

#[test]
fn failed_flag_changes_are_kept_together() {
    let mut app = app(settings());
    let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
    respond(&mut app, flag, Event::FlagsUpdated(Err(offline())));
    let (read, _) = sent(&act(&mut app, Action::ToggleNew));
    respond(&mut app, read, Event::FlagsUpdated(Err(offline())));
    let (_, failed) = &app.save_failures[&MessageId::new("m0")];
    assert_eq!(
        *failed,
        FlagsUpdate {
            read: Some(true),
            flagged: Some(true),
        }
    );
    // Saving one of them leaves the other to try again.
    let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
    respond(&mut app, flag, Event::FlagsUpdated(Ok(())));
    let (_, failed) = &app.save_failures[&MessageId::new("m0")];
    assert_eq!(*failed, FlagsUpdate::read(true));
}

#[test]
fn a_message_read_while_being_moved_is_marked_where_it_went() {
    let mut app = app(settings());
    let (fetch, _) = sent(&act(&mut app, Action::DisplayMessage));
    // Saved from the pager before the message arrives.
    act(&mut app, Action::SaveMessage);
    let (moving, _) = sent(&answer(&mut app, "=Archive"));
    let effects = respond(&mut app, fetch, Event::MessageFetched(Ok(m0())));
    assert!(effects.is_empty(), "the read mark waits its turn");
    let (_, request) = sent(&transferred(&mut app, moving, "archive", "m0", Ok("a0")));
    assert!(
        matches!(request, Request::UpdateFlags(ref id, _) if id.as_str() == "a0"),
        "{request:?}"
    );
}

#[test]
fn a_pager_under_the_browser_closes_when_its_message_moves() {
    let mut app = app(settings());
    let (read, _) = sent(&show_m0(&mut app));
    respond(&mut app, read, Event::FlagsUpdated(Ok(())));
    act(&mut app, Action::SaveMessage);
    let (moving, _) = sent(&answer(&mut app, "=Archive"));
    sent(&act(&mut app, Action::BrowseMailboxes));
    transferred(&mut app, moving, "archive", "m0", Ok("a0"));
    assert_eq!(app.menu(), Menu::Browser);
    act(&mut app, Action::Exit);
    assert_eq!(app.screen(), &Screen::Index);
    assert_eq!(ids(&app), ["m1", "m2"]);
}

#[test]
fn a_blank_mailbox_answer_forgets_the_browsed_folder() {
    let mut app = app(settings());
    act(&mut app, Action::SaveMessage);
    app.browsed = Some(("Archive".into(), FolderPath::Named(vec!["@archive".into()])));
    assert!(answer(&mut app, "").is_empty());
    assert!(app.browsed.is_none());
    assert!(app.question_message.is_none());
}

#[test]
fn a_change_undone_meanwhile_is_not_a_failure() {
    let mut app = app(settings());
    let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
    assert!(act(&mut app, Action::FlagMessage).is_empty(), "queued");
    let effects = respond(&mut app, flag, Event::FlagsUpdated(Err(offline())));
    assert!(effects.is_empty(), "nothing left to change");
    assert!(app.save_failures().is_empty());
    assert!(!app.saving());
}

#[test]
fn a_purged_message_has_nothing_left_to_save() {
    let mut app = yes();
    let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
    respond(&mut app, flag, Event::FlagsUpdated(Err(offline())));
    app.selected = 0;
    act(&mut app, Action::DeleteMessage);
    let (purge, _) = sent(&act(&mut app, Action::SyncMailbox));
    purged(&mut app, purge, "m0");
    assert!(app.save_failures().is_empty());
}

#[test]
fn copying_again_replaces_the_retry() {
    let mut app = app(settings());
    act(&mut app, Action::CopyMessage);
    let (id, _) = sent(&answer(&mut app, "=Archive"));
    signed_out(&mut app);
    respond(&mut app, id, Event::Transferred(Err(expired())));
    app.sign_in = None;
    act(&mut app, Action::CopyMessage);
    let (id, _) = sent(&answer(&mut app, "=Archive"));
    transferred(&mut app, id, "archive", "m0", Ok("c0"));
    let effects = act(&mut app, Action::Retry);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::Request(_, Request::Transfer { .. }))),
        "{effects:?}"
    );
}

#[test]
fn moving_elsewhere_replaces_the_retry() {
    let mut app = app(settings());
    act(&mut app, Action::SaveMessage);
    let (id, _) = sent(&answer(&mut app, "=Archive"));
    signed_out(&mut app);
    respond(&mut app, id, Event::Transferred(Err(expired())));
    app.sign_in = None;
    act(&mut app, Action::SaveMessage);
    let (id, _) = sent(&answer(&mut app, "=Projects"));
    transferred(&mut app, id, "projects", "m0", Ok("p0"));
    let effects = act(&mut app, Action::Retry);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::Request(_, Request::Transfer { .. }))),
        "{effects:?}"
    );
}

#[test]
fn no_replies_to_a_message_being_moved() {
    let mut app = app(settings());
    act(&mut app, Action::SaveMessage);
    sent(&answer(&mut app, "=Archive"));
    app.selected = 0;
    for action in [Action::Reply, Action::GroupReply, Action::Forward] {
        assert!(act(&mut app, action).is_empty());
        assert_eq!(
            app.status,
            "The message is still being moved, copied or purged."
        );
    }
}

#[test]
fn a_save_that_works_the_second_time_is_not_a_failure() {
    let mut app = app(settings());
    act(&mut app, Action::SaveMessage);
    let (id, _) = sent(&answer(&mut app, "=Archive"));
    transferred(&mut app, id, "archive", "m0", Err(offline()));
    assert!(!app.save_failures().is_empty());
    app.selected = 0;
    act(&mut app, Action::SaveMessage);
    let (id, _) = sent(&answer(&mut app, "=Archive"));
    transferred(&mut app, id, "archive", "m0", Ok("a0"));
    assert!(app.save_failures().is_empty(), "{:?}", app.save_failures());
}

#[test]
fn a_message_moved_into_the_open_folder_leaves_its_old_id() {
    let mut app = app(settings());
    act(&mut app, Action::SaveMessage);
    let (id, _) = sent(&answer(&mut app, "=@inbox"));
    let effects = transferred(&mut app, id, "in", "m0", Ok("n0"));
    assert!(!app.messages.iter().any(|m| m.id.as_str() == "m0"));
    assert!(
        matches!(sent(&effects).1, Request::ListMessages { .. }),
        "listed again"
    );
}

#[test]
fn quitting_during_a_purge_quits_after_it() {
    let mut app = yes();
    act(&mut app, Action::DeleteMessage);
    let (purge, _) = sent(&act(&mut app, Action::SyncMailbox));
    assert!(act(&mut app, Action::Quit).is_empty(), "no second purge");
    assert!(!app.quit);
    purged(&mut app, purge, "m0");
    assert!(app.quit);
}

#[test]
fn a_purge_retried_after_a_sign_in_still_quits() {
    let mut app = yes();
    act(&mut app, Action::DeleteMessage);
    let (purge, _) = sent(&act(&mut app, Action::Quit));
    signed_out(&mut app);
    respond(&mut app, purge, Event::Purged(Err(expired())));
    assert!(!app.quit);
    let (purge, _) = sent(&act(&mut app, Action::Retry));
    purged(&mut app, purge, "m0");
    assert!(app.quit);
}

#[test]
fn an_undeleted_message_is_not_purged_on_retry() {
    let mut app = yes();
    act(&mut app, Action::DeleteMessage);
    act(&mut app, Action::PreviousEntry);
    let (purge, _) = sent(&act(&mut app, Action::SyncMailbox));
    signed_out(&mut app);
    respond(&mut app, purge, Event::Purged(Err(expired())));
    app.sign_in = None;
    act(&mut app, Action::UndeleteMessage);
    assert!(!app.is_deleted(&"m0".into()));
    let effects = act(&mut app, Action::Retry);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::Request(_, Request::Purge { .. }))),
        "{effects:?}"
    );
    assert!(app.save_failures().is_empty());
}

#[test]
fn a_cancelled_question_forgets_its_browser() {
    let mut app = app(settings());
    act(&mut app, Action::SaveMessage);
    let (list, _) = sent(&key(&mut app, KeyCode::Tab));
    signed_out(&mut app);
    respond(&mut app, list, Event::FoldersListed(Err(expired())));
    app.sign_in = None;
    assert!(app.input.is_some(), "the question is back");
    key(&mut app, KeyCode::Esc);
    assert!(app.input.is_none());
    act(&mut app, Action::Retry);
    assert_ne!(app.menu(), Menu::Browser);
    assert!(app.input.is_none());
}

#[test]
fn a_partly_undone_change_keeps_only_what_still_failed() {
    let mut app = app(settings());
    let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
    act(&mut app, Action::FlagMessage);
    act(&mut app, Action::ToggleNew);
    let effects = respond(&mut app, flag, Event::FlagsUpdated(Err(offline())));
    let (read, request) = sent(&effects);
    assert!(matches!(
        request,
        Request::UpdateFlags(_, FlagsUpdate { flagged: None, .. })
    ));
    respond(&mut app, read, Event::FlagsUpdated(Ok(())));
    assert!(app.save_failures().is_empty(), "{:?}", app.save_failures());
}

#[test]
fn a_listing_older_than_a_purge_does_not_bring_it_back() {
    let mut app = yes();
    act(&mut app, Action::DeleteMessage);
    app.selected = 0;
    act(&mut app, Action::CopyMessage);
    let (copy, _) = sent(&answer(&mut app, "=@inbox"));
    assert!(
        act(&mut app, Action::SyncMailbox).is_empty(),
        "behind the copy"
    );
    let effects = transferred(&mut app, copy, "in", "m0", Ok("c0"));
    let list =
        request_for(&effects, |r| matches!(r, Request::ListMessages { .. })).expect("listed again");
    let purge =
        request_for(&effects, |r| matches!(r, Request::Purge { .. })).expect("the purge goes next");
    purged(&mut app, purge, "m0");
    let messages = plain(&["m0", "c0", "m1"]);
    listed(&mut app, list, messages, None);
    assert!(!app.messages.iter().any(|m| m.id.as_str() == "m0"));
}

fn lists_folders(effects: &[Effect]) -> bool {
    effects
        .iter()
        .any(|e| matches!(e, Effect::Request(_, Request::ListFolders)))
}

#[test]
fn an_off_screen_flag_retry_goes_out_once() {
    let mut app = app(settings());
    let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
    signed_out(&mut app);
    respond(&mut app, flag, Event::FlagsUpdated(Err(expired())));
    // Its folder isn't open any more.
    app.messages.clear();
    let first = app.retry();
    assert!(matches!(sent(&first).1, Request::UpdateFlags(..)));
    assert!(
        !app.retry()
            .iter()
            .any(|e| matches!(e, Effect::Request(_, Request::UpdateFlags(..)))),
        "already on its way"
    );
}

#[test]
fn a_browser_asked_for_before_the_folder_opened_comes_back() {
    let (mut app, effects) = App::new(80, 23);
    let (open, _) = sent(&effects);
    let (list, _) = sent(&act(&mut app, Action::BrowseMailboxes));
    signed_out(&mut app);
    respond(&mut app, open, Event::FolderOpened(Err(expired())));
    respond(&mut app, list, Event::FoldersListed(Err(expired())));
    let effects = app.retry();
    assert!(lists_folders(&effects), "{effects:?}");
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Request(_, Request::OpenFolder(_))))
    );
}

#[test]
fn an_answered_question_forgets_its_browser() {
    let mut app = app(settings());
    act(&mut app, Action::SaveMessage);
    let (list, _) = sent(&key(&mut app, KeyCode::Tab));
    signed_out(&mut app);
    respond(&mut app, list, Event::FoldersListed(Err(expired())));
    app.sign_in = None;
    sent(&answer(&mut app, "=Archive"));
    assert!(!lists_folders(&app.retry()));
}

fn purge_failed_for_sign_in(app: &mut App) {
    act(app, Action::DeleteMessage);
    app.selected = 0;
    let (purge, _) = sent(&act(app, Action::SyncMailbox));
    signed_out(app);
    respond(app, purge, Event::Purged(Err(expired())));
    app.sign_in = None;
}

#[test]
fn declining_a_purge_gives_up_its_retry() {
    let mut app = yes();
    purge_failed_for_sign_in(&mut app);
    app.delete = Quad::No;
    act(&mut app, Action::SyncMailbox);
    assert!(app.save_failures().is_empty());
    assert!(
        !app.retry()
            .iter()
            .any(|e| matches!(e, Effect::Request(_, Request::Purge { .. })))
    );
}

#[test]
fn moving_a_message_away_ends_its_purge_failure() {
    let mut app = yes();
    purge_failed_for_sign_in(&mut app);
    app.selected = 0;
    act(&mut app, Action::SaveMessage);
    let (id, _) = sent(&answer(&mut app, "=Archive"));
    transferred(&mut app, id, "archive", "m0", Ok("a0"));
    assert!(app.save_failures().is_empty(), "{:?}", app.save_failures());
}

#[test]
fn cancelling_a_purge_gives_up_its_retry() {
    let mut app = yes();
    purge_failed_for_sign_in(&mut app);
    app.delete = Quad::AskYes;
    act(&mut app, Action::SyncMailbox);
    key(&mut app, KeyCode::Esc);
    assert!(app.save_failures().is_empty());
    assert!(
        !app.retry()
            .iter()
            .any(|e| matches!(e, Effect::Request(_, Request::Purge { .. })))
    );
}

#[test]
fn browsing_again_without_choosing_keeps_the_choice() {
    let mut app = app(settings());
    act(&mut app, Action::SaveMessage);
    let target = FolderPath::Named(vec!["@archive".into()]);
    app.browsed = Some(("=@archive".into(), target.clone()));
    // Browsing again, then leaving without a choice.
    app.input = None;
    app.open_browser(Choose::Answer(Question::SaveTo, "=@archive".into()));
    app.leave_browser(None);
    let (_, request) = sent(&key(&mut app, KeyCode::Enter));
    assert!(matches!(request, Request::Transfer { to, .. } if to == target));
}

fn flag_failed(app: &mut App) {
    let (flag, _) = sent(&act(app, Action::FlagMessage));
    respond(app, flag, Event::FlagsUpdated(Err(offline())));
}

#[test]
fn an_off_screen_retry_leaves_other_flags_alone() {
    let mut app = app(settings());
    flag_failed(&mut app);
    let listed = app.messages.clone();
    app.messages.clear();
    let (retry, _) = sent(&app.retry());
    // Back in the folder before the retry is done; m0 is read there.
    app.messages = listed;
    app.messages[0].flags.read = true;
    app.messages[0].flags.flagged = false;
    respond(&mut app, retry, Event::FlagsUpdated(Ok(())));
    assert!(app.messages[0].flags.flagged);
    assert!(app.messages[0].flags.read, "untouched");
    assert!(app.save_failures().is_empty());
}

#[test]
fn a_listing_showing_a_failed_change_made_settles_it() {
    let mut app = app(settings());
    flag_failed(&mut app);
    // The mailbox had it after all, as a new listing shows.
    app.messages[0].flags.flagged = true;
    let effects = app.retry();
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::Request(_, Request::UpdateFlags(..)))),
        "{effects:?}"
    );
    assert!(app.save_failures().is_empty());
}

#[test]
fn a_save_before_a_failure_survives_an_older_listing() {
    let mut app = app(settings());
    let list = request_for(&app.relist(), |r| matches!(r, Request::ListMessages { .. }))
        .expect("listed again");
    app.messages = plain(&["m0", "m1", "m2"]);
    let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
    act(&mut app, Action::ToggleNew);
    let effects = respond(&mut app, flag, Event::FlagsUpdated(Ok(())));
    let (read, _) = sent(&effects);
    respond(&mut app, read, Event::FlagsUpdated(Err(offline())));
    // The listing taken before both changes arrives now, into an
    // index it fills afresh.
    app.messages.clear();
    listed(&mut app, list, vec![m0().summary], None);
    assert!(app.messages[0].flags.flagged, "the saved flag stays");
    assert!(
        !app.save_failures().is_empty(),
        "the read change still failed"
    );
}

fn switch_to(app: &mut App, text: &str, id: &str) {
    act(app, Action::ChangeFolder);
    let (open, _) = sent(&answer(app, text));
    respond(
        app,
        open,
        Event::FolderOpened(Ok(mock::folder(id, id, None))),
    );
}

#[test]
fn no_writing_while_another_folder_opens() {
    let mut app = app(settings());
    act(&mut app, Action::ChangeFolder);
    sent(&answer(&mut app, "=Archive"));
    for action in [Action::Mail, Action::Reply, Action::Forward] {
        assert!(act(&mut app, action).is_empty());
        assert_eq!(app.status, "Opening folder; wait a moment.");
    }
}

#[test]
fn mailbox_shortcuts_name_folders() {
    let mut app = app(Settings {
        shortcuts: crate::app::Shortcuts {
            spoolfile: "msgraph://alice@contoso.com/@inbox".into(),
            mbox: String::new(),
            record: "=@sentitems".into(),
        },
        ..settings()
    });
    let opened = app.path.clone();
    switch_to(&mut app, "=Archive", "arc");
    act(&mut app, Action::ChangeFolder);
    let (_, request) = sent(&answer(&mut app, "-"));
    assert_eq!(request, Request::OpenFolder(opened));
    app.switching = None;
    act(&mut app, Action::ChangeFolder);
    let (_, request) = sent(&answer(&mut app, "^"));
    assert_eq!(request, Request::OpenFolder(app.path.clone()));
    app.switching = None;
    act(&mut app, Action::ChangeFolder);
    let (_, request) = sent(&answer(&mut app, "!"));
    assert!(matches!(
        request,
        Request::OpenFolder(FolderPath::WellKnown(..))
    ));
    app.switching = None;
    act(&mut app, Action::ChangeFolder);
    let (_, request) = sent(&answer(&mut app, "<"));
    assert!(matches!(
        request,
        Request::OpenFolder(FolderPath::WellKnown(..))
    ));
    app.switching = None;
    act(&mut app, Action::ChangeFolder);
    assert!(answer(&mut app, ">").is_empty());
    assert_eq!(app.status, "mbox is not set");
}

#[test]
fn a_change_during_an_off_screen_retry_keeps_the_retry() {
    let mut app = app(settings());
    flag_failed(&mut app);
    let listed = app.messages.clone();
    app.messages.clear();
    let (retry, _) = sent(&app.retry());
    // Back in the folder, which still shows m0 unflagged; the user
    // marks it read meanwhile.
    app.messages = listed;
    app.messages[0].flags.flagged = false;
    app.messages[0].flags.read = false;
    assert!(
        act(&mut app, Action::ToggleNew).is_empty(),
        "waits its turn"
    );
    assert!(app.messages[0].flags.flagged, "the retry shows");
    let effects = respond(&mut app, retry, Event::FlagsUpdated(Ok(())));
    let (_, request) = sent(&effects);
    let Request::UpdateFlags(_, update) = request else {
        panic!("expected a flag update, got {request:?}");
    };
    assert_eq!(update.flagged, None, "the retry isn't undone");
    assert_eq!(update.read, Some(true));
}

#[test]
fn a_copy_elsewhere_doesnt_make_up_for_a_failed_one() {
    let mut app = app(settings());
    act(&mut app, Action::CopyMessage);
    let (id, _) = sent(&answer(&mut app, "=Archive"));
    transferred(&mut app, id, "archive", "m0", Err(offline()));
    app.selected = 0;
    act(&mut app, Action::CopyMessage);
    let (id, _) = sent(&answer(&mut app, "=Projects"));
    transferred(&mut app, id, "projects", "m0", Ok("p0"));
    assert_eq!(
        app.save_failures().len(),
        1,
        "the copy to Archive still failed"
    );
    // Copying to Archive again settles it.
    app.selected = 0;
    act(&mut app, Action::CopyMessage);
    let (id, _) = sent(&answer(&mut app, "=Archive"));
    transferred(&mut app, id, "archive", "m0", Ok("c0"));
    assert!(app.save_failures().is_empty());
}

#[test]
fn a_copy_into_the_open_folder_keeps_the_selection() {
    let mut app = app(settings());
    app.selected = 1;
    act(&mut app, Action::CopyMessage);
    let (copy, _) = sent(&answer(&mut app, "=@inbox"));
    let effects = transferred(&mut app, copy, "in", "m1", Ok("c1"));
    let (list, _) = sent(&effects);
    let messages = plain(&["c1", "m0", "m1", "m2"]);
    listed(&mut app, list, messages, None);
    assert_eq!(app.messages[app.selected].id.as_str(), "m1");
}

#[test]
fn a_failed_copy_into_the_open_folder_lists_nothing() {
    let mut app = app(settings());
    act(&mut app, Action::CopyMessage);
    let (copy, _) = sent(&answer(&mut app, "=@inbox"));
    let effects = transferred(&mut app, copy, "in", "m0", Err(offline()));
    assert!(effects.is_empty(), "{effects:?}");
}

#[test]
fn an_off_screen_retry_survives_an_older_listing() {
    let mut app = app(settings());
    flag_failed(&mut app);
    app.messages.clear();
    let (retry, _) = sent(&app.retry());
    // The folder is listed again before the retry is done.
    let list = request_for(&app.relist(), |r| matches!(r, Request::ListMessages { .. }))
        .expect("listed again");
    respond(&mut app, retry, Event::FlagsUpdated(Ok(())));
    let mut m0 = m0().summary;
    m0.flags.read = true;
    listed(&mut app, list, vec![m0], None);
    assert!(app.messages[0].flags.flagged, "the retry's flag");
    assert!(app.messages[0].flags.read, "the rest as listed");
}

#[test]
fn a_browser_opened_by_hand_replaces_the_one_to_retry() {
    let mut app = app(settings());
    let (list, _) = sent(&act(&mut app, Action::BrowseMailboxes));
    signed_out(&mut app);
    respond(&mut app, list, Event::FoldersListed(Err(expired())));
    app.sign_in = None;
    let (list, _) = sent(&act(&mut app, Action::BrowseMailboxes));
    respond(
        &mut app,
        list,
        Event::FoldersListed(Ok(vec![mock::folder("arc", "Archive", None)])),
    );
    act(&mut app, Action::Exit);
    assert!(!lists_folders(&app.retry()));
}

#[test]
fn a_mark_made_during_a_purge_is_purged_before_quitting() {
    let mut app = yes();
    act(&mut app, Action::DeleteMessage);
    app.selected = 0;
    let (purge, _) = sent(&act(&mut app, Action::SyncMailbox));
    // m1 is marked while m0 is purged, then the user quits.
    app.selected = 1;
    act(&mut app, Action::DeleteMessage);
    act(&mut app, Action::Quit);
    let effects = purged(&mut app, purge, "m0");
    assert!(!app.quit, "m1 first");
    let (second, request) = sent(&effects);
    assert!(matches!(request, Request::Purge { ref ids, .. } if ids == &[MessageId::new("m1")]));
    purged(&mut app, second, "m1");
    assert!(app.quit);
}

#[test]
fn unread_chosen_while_loading_stays_unread() {
    let mut app = app(settings());
    app.messages[0].flags.read = true;
    let (fetch, _) = sent(&act(&mut app, Action::DisplayMessage));
    let (toggle, _) = sent(&act(&mut app, Action::ToggleNew));
    let effects = respond(&mut app, fetch, Event::MessageFetched(Ok(m0())));
    assert!(effects.is_empty(), "not marked read again: {effects:?}");
    respond(&mut app, toggle, Event::FlagsUpdated(Ok(())));
    assert!(!app.messages[0].flags.read);
}

#[test]
fn a_copy_to_the_same_folder_by_another_name_makes_up_for_a_failed_one() {
    let mut app = app(settings());
    act(&mut app, Action::CopyMessage);
    let (id, _) = sent(&answer(&mut app, "=Archive"));
    transferred(&mut app, id, "arc", "m0", Err(offline()));
    assert!(!app.save_failures().is_empty());
    app.selected = 0;
    act(&mut app, Action::CopyMessage);
    let (id, _) = sent(&answer(&mut app, "=@archive"));
    transferred(&mut app, id, "arc", "m0", Ok("c0"));
    assert!(app.save_failures().is_empty(), "{:?}", app.save_failures());
}

#[test]
fn a_copy_to_the_same_folder_replaces_its_retry() {
    let mut app = app(settings());
    act(&mut app, Action::CopyMessage);
    let (id, _) = sent(&answer(&mut app, "=Archive"));
    signed_out(&mut app);
    // The folder was found, but copying needed a sign-in.
    transferred(&mut app, id, "arc", "m0", Err(expired()));
    app.sign_in = None;
    app.selected = 0;
    act(&mut app, Action::CopyMessage);
    let (id, _) = sent(&answer(&mut app, "=@archive"));
    transferred(&mut app, id, "arc", "m0", Ok("c0"));
    assert!(
        !app.retry()
            .iter()
            .any(|e| matches!(e, Effect::Request(_, Request::Transfer { .. })))
    );
}

#[test]
fn copies_to_one_folder_by_two_names_are_retried_once() {
    let mut app = app(settings());
    signed_out(&mut app);
    for name in ["=Archive", "=@archive"] {
        app.sign_in = None;
        app.selected = 0;
        act(&mut app, Action::CopyMessage);
        let (id, _) = sent(&answer(&mut app, name));
        transferred(&mut app, id, "arc", "m0", Err(expired()));
    }
    // One retry is kept (changes go out one at a time, so look at what
    // is kept, not just what is sent first).
    let kept: Vec<_> = app
        .failed_transfers
        .iter()
        .flat_map(|(_, retry)| transfer_ids(retry).to_vec())
        .collect();
    assert_eq!(kept, [MessageId::new("m0")]);
    app.retry();
    assert_eq!(app.writes.len() + usize::from(app.writing.is_some()), 1);
}

#[test]
fn the_pager_s_message_is_the_one_acted_on() {
    let mut app = app(Settings {
        resolve: false,
        ..settings()
    });
    show_m0(&mut app);
    // A background listing moves the index's selection meanwhile.
    app.selected = 1;
    act(&mut app, Action::DeleteMessage);
    assert!(app.is_deleted(&"m0".into()));
    assert!(!app.is_deleted(&"m1".into()));
}

#[test]
fn marking_in_the_pager_goes_on_from_its_message() {
    let mut app = app(settings());
    show_m0(&mut app);
    // A background listing moves the index's selection meanwhile.
    app.selected = 2;
    let (_, request) = sent(&act(&mut app, Action::DeleteMessage));
    assert_eq!(
        request,
        Request::FetchMessage("m1".into()),
        "the one after m0"
    );
}
