use chrono::{DateTime, TimeDelta};
use mett_core::mock;
use mett_core::{PageCursor, WellKnownFolder};

use super::*;
use crate::app::testing::*;

#[test]
fn loading_says_how_far_it_has_got() {
    let (mut app, effects) = App::new(80, 23);
    let [Effect::Request(open, Request::OpenFolder(_))] = effects.as_slice() else {
        panic!("expected OpenFolder, got {effects:?}");
    };
    let folder = mock::folder("in", "Inbox", Some(WellKnownFolder::Inbox));
    let effects = respond(&mut app, *open, Event::FolderOpened(Ok(folder)));
    let list = effects
        .iter()
        .find_map(|e| match e {
            Effect::Request(id, Request::ListMessages { .. }) => Some(*id),
            _ => None,
        })
        .unwrap();
    assert_eq!(app.status, "Loading Inbox...");
    // A first sync of the folder, in the cache.
    let syncing = |taken| Msg::Syncing {
        folder: mett_core::FolderId::new("in"),
        taken,
    };
    app.update(syncing(512));
    assert_eq!(app.status, "Loading Inbox: 512 messages synced...");
    // Another folder's sync (a mailbox checked) doesn't say.
    app.update(Msg::Syncing {
        folder: mett_core::FolderId::new("other"),
        taken: 9,
    });
    assert_eq!(app.status, "Loading Inbox: 512 messages synced...");
    // Then the pages: how many are listed so far.
    let page = mett_core::MessagePage {
        messages: summaries(3),
        next: Some(PageCursor::new("p2")),
    };
    respond(&mut app, list, Event::MessagesListed(Ok(page)));
    assert_eq!(app.status, "Loading Inbox: 3 messages...");
    // The sync's news, come late, doesn't take it back.
    app.update(syncing(600));
    assert_eq!(app.status, "Loading Inbox: 3 messages...");
    // Not once loaded.
    app.status.info("Mail sent.");
    app.update(syncing(3));
    assert_eq!(app.status, "Mail sent.");
}

#[test]
fn counts_down_the_wait_for_a_throttling_server() {
    let (mut app, _) = App::new(80, 23);
    let throttled = |wait: u64, attempt| Event::Throttled {
        wait: Duration::from_millis(wait),
        attempt,
    };
    let tick = |timer| [Effect::Later(SECOND, Timer::Throttled(timer))];
    assert_eq!(
        respond(&mut app, RequestId(90), throttled(2500, 1)),
        tick(1)
    );
    assert_eq!(
        app.throttled_notice().as_deref(),
        Some("Throttled by the server: retry 1/5 in 3s")
    );
    // Another request throttled meanwhile, for less: one countdown, to
    // the end of the longest wait.
    assert_eq!(respond(&mut app, RequestId(91), throttled(1000, 2)), []);
    assert_eq!(
        app.throttled_notice().as_deref(),
        Some("Throttled by the server: retry 2/5 in 3s")
    );
    assert_eq!(app.update(Msg::Timer(Timer::Throttled(1))), tick(1));
    assert_eq!(
        app.throttled_notice().as_deref(),
        Some("Throttled by the server: retry 1/5 in 2s"),
        "the other one's wait is over"
    );
    assert_eq!(app.update(Msg::Timer(Timer::Throttled(1))), tick(1));
    assert_eq!(app.update(Msg::Timer(Timer::Throttled(1))), []);
    assert_eq!(app.throttled_notice(), None);
    // The status line is left alone.
    assert_eq!(app.status, "Opening folder...");
}

#[test]
fn answered_requests_wait_no_more() {
    let (mut app, _) = App::new(80, 23);
    let throttled = Event::Throttled {
        wait: Duration::from_secs(30),
        attempt: 1,
    };
    respond(&mut app, RequestId(90), throttled);
    // Its answer, however late or stale, ends its wait.
    respond(&mut app, RequestId(90), Event::FlagsUpdated(Ok(())));
    assert_eq!(app.throttled_notice(), None);
    // The countdown running then stops, and a new one starts afresh.
    assert_eq!(app.update(Msg::Timer(Timer::Throttled(1))), []);
    let throttled = Event::Throttled {
        wait: Duration::from_secs(30),
        attempt: 1,
    };
    assert_eq!(
        respond(&mut app, RequestId(91), throttled),
        [Effect::Later(SECOND, Timer::Throttled(2))]
    );
    assert_eq!(app.update(Msg::Timer(Timer::Throttled(1))), [], "stale");
    // Giving it up ends it too, even if a wait told of before comes
    // after.
    app.request(Request::Cancel(RequestId(91)));
    assert_eq!(app.throttled_notice(), None);
    let late = Event::Throttled {
        wait: Duration::from_secs(30),
        attempt: 2,
    };
    assert_eq!(respond(&mut app, RequestId(91), late), []);
    assert_eq!(app.throttled_notice(), None);
    // Only the last few are remembered.
    for query in 0..100 {
        app.request(Request::Cancel(RequestId(1000 + query)));
    }
    assert_eq!(app.given_up.len(), GIVEN_UP_KEPT);
    // Calls of one request waiting at once: the longest wait counts.
    for wait in [20, 5] {
        let throttled = Event::Throttled {
            wait: Duration::from_secs(wait),
            attempt: 1,
        };
        respond(&mut app, RequestId(92), throttled);
    }
    assert_eq!(
        app.throttled_notice().as_deref(),
        Some("Throttled by the server: retry 1/5 in 20s")
    );
    // None of them saves anything.
    assert_eq!(app.save_wait(), None);
}

/// `n` messages, newest first, as Graph lists them.
fn summaries(n: usize) -> Vec<MessageSummary> {
    (0..n)
        .map(|i| {
            let received = DateTime::UNIX_EPOCH + TimeDelta::minutes((n - i) as i64);
            mock::message(&format!("m{i}"), "in", &format!("subject {i}"), received).summary
        })
        .collect()
}

/// An app whose Inbox has been opened and listed with `n` messages.
fn loaded(n: usize) -> App {
    let (mut app, effects) = App::new(80, 23);
    let [Effect::Request(open, Request::OpenFolder(_))] = effects.as_slice() else {
        panic!("expected OpenFolder, got {effects:?}");
    };
    let folder = mock::folder("in", "Inbox", Some(WellKnownFolder::Inbox));
    let effects = respond(&mut app, *open, Event::FolderOpened(Ok(folder)));
    // The user's own addresses are asked for alongside.
    let [
        Effect::Request(list, Request::ListMessages { .. }),
        Effect::Request(_, Request::OwnAddresses),
    ] = effects.as_slice()
    else {
        panic!("expected ListMessages, got {effects:?}");
    };
    listed(&mut app, *list, summaries(n), None);
    app
}

fn message(id: &str, body_lines: usize) -> Message {
    let mut message = mock::message(id, "in", id, DateTime::UNIX_EPOCH);
    message.body.content = (0..body_lines).map(|i| format!("line {i}\n")).collect();
    message
}

/// Opens the selected message and delivers it with `body_lines` lines;
/// returns the effects of the delivery.
fn open(app: &mut App, body_lines: usize) -> Vec<Effect> {
    let effects = act(app, Action::DisplayMessage);
    let [Effect::Request(fetch, Request::FetchMessage(id))] = effects.as_slice() else {
        panic!("expected FetchMessage, got {effects:?}");
    };
    let message = message(id.as_str(), body_lines);
    respond(app, *fetch, Event::MessageFetched(Ok(message)))
}

fn denied() -> Error {
    Error::Auth("denied".into())
}

#[test]
fn loads_the_inbox_on_start() {
    let app = loaded(3);
    assert_eq!(app.messages.len(), 3);
    assert_eq!(app.folder.as_ref().unwrap().name, "Inbox");
    assert!(app.status.is_empty());
}

#[test]
fn keeps_listing_until_the_last_page() {
    let (mut app, effects) = App::new(80, 23);
    let open = first_request(&effects);
    let folder = mock::folder("in", "Inbox", None);
    let first = first_request(&respond(&mut app, open, Event::FolderOpened(Ok(folder))));
    let effects = listed(&mut app, first, summaries(2), Some("c"));
    let [Effect::Request(_, Request::ListMessages { page, .. })] = effects.as_slice() else {
        panic!("expected another page request, got {effects:?}");
    };
    assert_eq!(page.cursor(), Some(&PageCursor::new("c")));
    assert_eq!(app.messages.len(), 2);
}

#[test]
fn ignores_a_stale_listing() {
    let mut app = loaded(1);
    listed(&mut app, RequestId(999), summaries(5), None);
    assert_eq!(app.messages.len(), 1);
}

#[test]
fn moves_through_the_index_within_bounds() {
    let mut app = loaded(3);
    act(&mut app, Action::PreviousEntry);
    assert_eq!(app.selected, 0);
    assert_eq!(app.status, "You are on the first message.");
    act(&mut app, Action::NextEntry);
    act(&mut app, Action::NextEntry);
    act(&mut app, Action::NextEntry);
    assert_eq!(app.selected, 2);
    assert_eq!(app.status, "You are on the last message.");
    act(&mut app, Action::FirstEntry);
    assert_eq!(app.selected, 0);
    act(&mut app, Action::LastEntry);
    assert_eq!(app.selected, 2);
}

#[test]
fn pages_through_the_index() {
    let mut app = loaded(50);
    assert_eq!(app.body_height, 20);
    act(&mut app, Action::NextPage);
    assert_eq!(app.selected, 20);
    act(&mut app, Action::NextPage);
    act(&mut app, Action::NextPage);
    assert_eq!(app.selected, 49);
    act(&mut app, Action::PreviousPage);
    assert_eq!(app.selected, 29);
}

#[test]
fn marks_a_message_read_once_it_is_shown() {
    let mut app = loaded(2);
    let effects = act(&mut app, Action::DisplayMessage);
    assert_eq!(app.menu(), Menu::Pager);
    assert!(matches!(
        effects.as_slice(),
        [Effect::Request(_, Request::FetchMessage(id))] if id.as_str() == "m0"
    ));
    assert!(!app.messages[0].flags.read, "not read before it is shown");

    act(&mut app, Action::Exit);
    let effects = open(&mut app, 1);
    assert!(matches!(
        effects.as_slice(),
        [Effect::Request(_, Request::UpdateFlags(id, FlagsUpdate { read: Some(true), .. }))]
            if id.as_str() == "m0"
    ));
    assert!(app.messages[0].flags.read);

    // Reopening a read message doesn't update its flags again.
    act(&mut app, Action::Exit);
    assert!(open(&mut app, 1).is_empty());
}

#[test]
fn finishing_the_listing_keeps_an_error_reported_meanwhile() {
    let (mut app, first) = opening(Settings::default(), 23);
    let second = page(&mut app, first, summaries(1), "c");

    // An error is reported while the second page is loading…
    let update = first_request(&open(&mut app, 1));
    respond(&mut app, update, Event::FlagsUpdated(Err(offline())));
    // …and survives the end of the listing.
    listed(&mut app, second, Vec::new(), None);
    assert_eq!(app.status, "network error: offline");
}

#[test]
fn a_failed_page_leaves_the_index_incomplete_until_retried() {
    let (mut app, first) = opening(Settings::default(), 23);
    let second = page(&mut app, first, summaries(2), "c");
    respond(&mut app, second, Event::MessagesListed(Err(offline())));
    assert!(app.incomplete());

    // Reaching the end of the index retries the failed page.
    act(&mut app, Action::NextEntry);
    let effects = act(&mut app, Action::NextEntry);
    let [Effect::Request(retry, Request::ListMessages { page, .. })] = effects.as_slice() else {
        panic!("expected a retry, got {effects:?}");
    };
    assert_eq!(page.cursor(), Some(&PageCursor::new("c")));
    assert!(!app.incomplete());
    listed(&mut app, *retry, summaries(1), None);
    assert_eq!(app.messages.len(), 3);
    assert!(!app.incomplete());
}

#[test]
fn the_pager_also_retries_a_failed_page() {
    let (mut app, first) = opening(Settings::default(), 23);
    let second = page(&mut app, first, summaries(1), "c");
    respond(&mut app, second, Event::MessagesListed(Err(offline())));

    open(&mut app, 1);
    let effects = act(&mut app, Action::NextEntry);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Request(_, Request::ListMessages { .. })]
        ),
        "{effects:?}"
    );
    assert_eq!(app.status, "Loading more messages...");
}

#[test]
fn records_every_failed_save() {
    let mut app = loaded(2);
    let first = open(&mut app, 1);
    act(&mut app, Action::Exit);
    act(&mut app, Action::NextEntry);
    // One change at a time: the second waits for the first.
    assert!(open(&mut app, 1).is_empty());
    let (first, _) = sent(&first);
    let (second, _) = sent(&respond(
        &mut app,
        first,
        Event::FlagsUpdated(Err(offline())),
    ));
    // A later success doesn't hide the earlier failure.
    respond(&mut app, second, Event::FlagsUpdated(Ok(())));
    assert_eq!(app.save_failures(), ["network error: offline"]);
    assert!(!app.saving());

    // Saving the same message again successfully clears its failure.
    act(&mut app, Action::Exit);
    act(&mut app, Action::FirstEntry);
    let (retry, _) = sent(&open(&mut app, 1));
    respond(&mut app, retry, Event::FlagsUpdated(Ok(())));
    assert!(app.save_failures().is_empty());
}

/// An app whose first page of `n` messages has loaded, with another page
/// still loading; returns the app and that request.
fn loading_more(n: usize) -> (App, RequestId) {
    let (mut app, first) = opening(Settings::default(), 23);
    let second = page(&mut app, first, summaries(n), "c");
    (app, second)
}

#[test]
fn next_entry_waits_for_the_next_page_in_the_index() {
    let (mut app, second) = loading_more(1);
    act(&mut app, Action::NextEntry);
    assert_eq!(app.selected, 0);
    assert_eq!(app.status, "Loading more messages...");
    let mut more = summaries(2);
    more.remove(0);
    listed(&mut app, second, more, None);
    assert_eq!(app.selected, 1, "moved on once the page arrived");
}

#[test]
fn next_entry_waits_for_the_next_page_in_the_pager() {
    let (mut app, second) = loading_more(1);
    open(&mut app, 1);
    act(&mut app, Action::NextEntry);
    let mut more = summaries(2);
    more.remove(0);
    let effects = listed(&mut app, second, more, None);
    assert_eq!(app.selected, 1);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Request(_, Request::FetchMessage(id))] if id.as_str() == "m1"
        ),
        "{effects:?}"
    );
}

#[test]
fn last_entry_follows_the_folder_while_it_loads() {
    let (mut app, second) = loading_more(2);
    act(&mut app, Action::LastEntry);
    assert_eq!(app.selected, 1);
    listed(&mut app, second, summaries(5), None);
    assert_eq!(app.selected, 6, "the real last message");
}

#[test]
fn next_page_completes_once_the_page_arrives() {
    let (mut app, second) = loading_more(5);
    // 20 rows per page: the target, entry 20, isn't loaded yet.
    act(&mut app, Action::NextPage);
    assert_eq!(app.selected, 4);
    listed(&mut app, second, summaries(30), None);
    assert_eq!(app.selected, 20);
}

#[test]
fn another_action_cancels_a_pending_next_entry() {
    let (mut app, second) = loading_more(2);
    act(&mut app, Action::NextEntry);
    act(&mut app, Action::NextEntry);
    act(&mut app, Action::PreviousEntry);
    listed(&mut app, second, Vec::new(), None);
    assert_eq!(app.selected, 0);
}

#[test]
fn tracks_unsaved_flag_changes() {
    let mut app = loaded(1);
    assert!(!app.saving());
    let effects = open(&mut app, 1);
    assert!(app.saving());
    let update = first_request(&effects);
    respond(&mut app, update, Event::FlagsUpdated(Ok(())));
    assert!(!app.saving());
}

#[test]
fn a_failed_flag_update_marks_the_message_unread_again() {
    let mut app = loaded(1);
    let update = first_request(&open(&mut app, 1));
    respond(&mut app, update, Event::FlagsUpdated(Err(offline())));
    assert!(!app.messages[0].flags.read);
    assert_eq!(app.status, "network error: offline");
}

#[test]
fn shows_the_fetched_message_and_ignores_stale_ones() {
    let mut app = loaded(2);
    let first = first_request(&act(&mut app, Action::DisplayMessage));
    // Move on to the next message before the first one arrives.
    let second = first_request(&act(&mut app, Action::NextEntry));

    respond(&mut app, first, Event::MessageFetched(Ok(message("m0", 1))));
    assert_eq!(pager(&app).message, None, "stale response ignored");
    assert!(!app.messages[0].flags.read);

    respond(
        &mut app,
        second,
        Event::MessageFetched(Ok(message("m1", 1))),
    );
    let shown = pager(&app).message.as_ref().unwrap();
    assert_eq!(shown.summary.id.as_str(), "m1");
    assert_eq!(app.selected, 1);
}

#[test]
fn a_failed_fetch_returns_to_the_index_and_leaves_it_unread() {
    let mut app = loaded(1);
    let fetch = first_request(&act(&mut app, Action::DisplayMessage));
    let effects = respond(&mut app, fetch, Event::MessageFetched(Err(offline())));
    assert!(effects.is_empty());
    assert_eq!(app.menu(), Menu::Index);
    assert_eq!(app.status, "network error: offline");
    assert!(!app.messages[0].flags.read);
}

#[test]
fn pager_scrolls_within_the_message() {
    let mut app = loaded(1);
    // 4 header lines and a blank line, 40 body lines; 20 visible rows.
    open(&mut app, 40);
    let rows = pager(&app).row_count();
    assert_eq!(rows, text::wrap_all(&pager(&app).lines, 80).len());
    act(&mut app, Action::NextLine);
    act(&mut app, Action::NextPage);
    act(&mut app, Action::PreviousLine);
    assert_eq!(pager(&app).scroll, 20);

    for _ in 0..5 {
        act(&mut app, Action::NextPage);
    }
    assert_eq!(pager(&app).scroll, rows - 20, "stops at the last full page");
    act(&mut app, Action::PreviousPage);
    assert_eq!(
        pager(&app).scroll,
        rows - 40,
        "responds at once after the end"
    );

    act(&mut app, Action::Exit);
    assert_eq!(app.screen(), &Screen::Index);
    act(&mut app, Action::Quit);
    assert!(app.quit);
}

#[test]
fn links_show_their_urls_only_when_asked() {
    let mut app = loaded(1);
    let fetch = first_request(&act(&mut app, Action::DisplayMessage));
    let mut message = message("m0", 30);
    message
        .body
        .content
        .push_str("the docs<https://example.com>\n");
    respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
    let last = pager(&app).lines.len() - 1;
    assert_eq!(pager(&app).lines[last], "the docs");
    let link = text::Link {
        line: last,
        range: 4..8,
    };
    assert_eq!(pager(&app).links, [link]);

    act(&mut app, Action::NextPage);
    let top = pager(&app).scroll;
    act(&mut app, Action::ToggleLinks);
    assert_eq!(pager(&app).lines[last], "the docs<https://example.com>");
    assert_eq!(pager(&app).links[0].range, 9..28);
    assert_eq!(pager(&app).scroll, top, "the same line stays at the top");
    assert_eq!(
        pager(&app).row_count(),
        text::wrap_all(&pager(&app).lines, 80).len()
    );

    act(&mut app, Action::ToggleLinks);
    assert_eq!(pager(&app).lines[last], "the docs");
    // The next message shows its links collapsed again.
    act(&mut app, Action::ToggleLinks);
    act(&mut app, Action::Exit);
    act(&mut app, Action::DisplayMessage);
    assert!(pager(&app).collapse_links);
}

#[test]
fn the_open_message_follows_collapse_links() {
    let mut app = loaded(1);
    let fetch = first_request(&act(&mut app, Action::DisplayMessage));
    let mut message = message("m0", 0);
    message.body.content = "the docs<https://example.com>".into();
    respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
    let last = pager(&app).lines.len() - 1;
    assert_eq!(pager(&app).lines[last], "the docs");

    let mut settings = Settings::default();
    settings.display.collapse_links = false;
    app.apply_settings(settings.clone());
    assert_eq!(pager(&app).lines[last], "the docs<https://example.com>");
    // Shown as toggle-links chose, until the setting changes again.
    act(&mut app, Action::ToggleLinks);
    app.apply_settings(settings.clone());
    assert_eq!(pager(&app).lines[last], "the docs");
    settings.display.collapse_links = true;
    app.apply_settings(settings);
    assert_eq!(pager(&app).lines[last], "the docs");
}

#[test]
fn long_lines_wrap_and_can_be_scrolled_to() {
    let mut app = loaded(1);
    app.update(Msg::Resize {
        width: 10,
        height: 8,
    });
    let fetch = first_request(&act(&mut app, Action::DisplayMessage));
    let mut message = message("m0", 0);
    message.body.content = "x".repeat(100);
    respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
    let rows = pager(&app).row_count();
    assert_eq!(rows, text::wrap_all(&pager(&app).lines, 10).len());
    assert!(rows > 10);
    for _ in 0..rows {
        act(&mut app, Action::NextLine);
    }
    assert_eq!(pager(&app).scroll, rows - 5, "every row is reachable");
}

#[test]
fn the_sign_in_window_opens_the_browser_and_closes_once_signed_in() {
    let (mut app, _) = App::new(80, 23);
    let effects = app.update(Msg::SignIn(browser_prompt()));
    assert_eq!(
        effects,
        [Effect::OpenUrl("https://login.example/authorize".into())]
    );
    assert_eq!(app.menu(), Menu::SignIn);
    // o opens it again.
    assert_eq!(
        act(&mut app, Action::OpenBrowser),
        [Effect::OpenUrl("https://login.example/authorize".into())]
    );
    assert_eq!(
        act(&mut app, Action::CopyLink),
        [Effect::Copy("https://login.example/authorize".into())]
    );
    app.update(Msg::SignIn(finished(Ok(()))));
    assert_eq!(app.sign_in, None);
    assert_eq!(app.menu(), Menu::Index);
}

#[test]
fn the_device_code_window_opens_the_page_only_when_asked() {
    let (mut app, _) = App::new(80, 23);
    let effects = app.update(Msg::SignIn(SignInEvent::Prompt {
        account: AccountId::new("alice@contoso.com"),
        prompt: SignInPrompt::EnterCode {
            verification_uri: "https://microsoft.com/devicelogin".into(),
            user_code: "ABCD".into(),
            message: "Enter ABCD".into(),
        },
    }));
    assert!(effects.is_empty());
    assert_eq!(
        act(&mut app, Action::OpenBrowser),
        [Effect::OpenUrl("https://microsoft.com/devicelogin".into())]
    );
}

#[test]
fn cancelling_closes_the_window_without_reporting_a_failure() {
    let (mut app, _) = App::new(80, 23);
    app.update(Msg::SignIn(browser_prompt()));
    app.retry_after.insert(RequestId(1));
    assert_eq!(act(&mut app, Action::Exit), [Effect::CancelSignIn]);
    assert_eq!(app.sign_in, None);
    assert!(app.retry_after.is_empty());
    app.update(Msg::SignIn(finished(Err("sign-in cancelled".into()))));
    assert_eq!(app.sign_in, None);
    assert_eq!(app.status, "Sign-in cancelled.");
    // The request it held up says the same, in the same words.
    let opening = app.opening.unwrap();
    respond(
        &mut app,
        opening,
        Event::FolderOpened(Err(Error::Auth("sign-in cancelled".into()))),
    );
    assert_eq!(app.status, "Sign-in cancelled.");
}

#[test]
fn a_failed_sign_in_can_be_retried() {
    let (mut app, effects) = App::new(80, 23);
    let [Effect::Request(first, Request::OpenFolder(_))] = effects.as_slice() else {
        panic!("expected OpenFolder, got {effects:?}");
    };
    // The sign-in fails before any prompt, and so does the request.
    app.update(Msg::SignIn(finished(Err("bad app".into()))));
    respond(
        &mut app,
        *first,
        Event::FolderOpened(Err(Error::Auth("bad app".into()))),
    );
    let window = app.sign_in.as_ref().expect("the failure is shown");
    assert_eq!(window.failure.as_deref(), Some("bad app"));
    assert_eq!(window.prompt, None);

    // Retrying closes the window and opens the folder again.
    let effects = act(&mut app, Action::Retry);
    let [Effect::Request(second, Request::OpenFolder(_))] = effects.as_slice() else {
        panic!("expected OpenFolder, got {effects:?}");
    };
    assert_eq!(app.sign_in, None);
    assert_eq!(app.status, "Opening folder...");
    respond(
        &mut app,
        *second,
        Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
    );
    assert_eq!(app.folder.as_ref().unwrap().name, "Inbox");
}

#[test]
fn enter_in_an_empty_index_retries_opening_the_folder() {
    let (mut app, effects) = App::new(80, 23);
    let open_inbox = first_request(&effects);
    // Pressing Enter while the folder is still opening does nothing.
    assert!(act(&mut app, Action::DisplayMessage).is_empty());
    assert_eq!(app.status, "Opening folder...");
    respond(&mut app, open_inbox, Event::FolderOpened(Err(offline())));
    assert!(matches!(
        act(&mut app, Action::DisplayMessage).as_slice(),
        [Effect::Request(_, Request::OpenFolder(_))]
    ));
}

#[test]
fn retrying_after_a_sign_in_resends_what_failed() {
    let mut app = loaded(2);
    // Opening the first message works, but saving it as read fails…
    let effects = open(&mut app, 1);
    let [Effect::Request(update, Request::UpdateFlags(..))] = effects.as_slice() else {
        panic!("expected a flag update, got {effects:?}");
    };
    respond(&mut app, *update, Event::FlagsUpdated(Err(expired())));
    // …and opening the second one fails too.
    act(&mut app, Action::Exit);
    act(&mut app, Action::NextEntry);
    let effects = act(&mut app, Action::DisplayMessage);
    let [Effect::Request(fetch, Request::FetchMessage(_))] = effects.as_slice() else {
        panic!("expected a fetch, got {effects:?}");
    };
    respond(&mut app, *fetch, Event::MessageFetched(Err(expired())));
    assert_eq!(app.menu(), Menu::Index);
    signed_out(&mut app);

    let effects = act(&mut app, Action::Retry);
    assert!(
        matches!(
            effects.as_slice(),
            [
                Effect::Request(_, Request::UpdateFlags(first, _)),
                Effect::Request(_, Request::FetchMessage(second)),
            ] if first.as_str() == "m0" && second.as_str() == "m1"
        ),
        "{effects:?}"
    );
    assert_eq!(app.menu(), Menu::Pager);
}

#[test]
fn retry_waits_for_the_failed_request_to_report_back() {
    let (mut app, effects) = App::new(80, 23);
    let [Effect::Request(first, Request::OpenFolder(_))] = effects.as_slice() else {
        panic!("expected OpenFolder, got {effects:?}");
    };
    // The sign-in failure arrives before the request's own failure, and
    // the user retries straight away.
    signed_out_as(&mut app, "denied");
    assert!(act(&mut app, Action::Retry).is_empty());
    let effects = respond(&mut app, *first, Event::FolderOpened(Err(denied())));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Request(_, Request::OpenFolder(_))]
        ),
        "{effects:?}"
    );
}

#[test]
fn a_successful_sign_in_keeps_an_error_already_reported() {
    let (mut app, effects) = App::new(80, 23);
    let open_inbox = first_request(&effects);
    app.update(Msg::SignIn(browser_prompt()));
    respond(
        &mut app,
        open_inbox,
        Event::FolderOpened(Err(Error::not_found("folder", "x"))),
    );
    app.update(Msg::SignIn(finished(Ok(()))));
    assert_eq!(app.status, "folder not found: x");
}

#[test]
fn retry_waits_for_every_pending_request() {
    let (mut app, list) = loading_more(1);
    // Opening a message fails while the next page is still loading.
    let (fetch, _) = sent(&act(&mut app, Action::DisplayMessage));
    signed_out_as(&mut app, "denied");
    assert!(act(&mut app, Action::Retry).is_empty());
    let effects = respond(&mut app, fetch, Event::MessageFetched(Err(denied())));
    assert!(
        effects.is_empty(),
        "still waiting for the page: {effects:?}"
    );
    let effects = respond(&mut app, list, Event::MessagesListed(Err(denied())));
    assert!(
        matches!(
            effects.as_slice(),
            [
                Effect::Request(_, Request::ListMessages { .. }),
                Effect::Request(_, Request::FetchMessage(_)),
            ]
        ),
        "{effects:?}"
    );
}

#[test]
fn retry_resends_failures_at_once_and_waits_for_the_rest() {
    let (mut app, list) = loading_more(1);
    let (fetch, _) = sent(&act(&mut app, Action::DisplayMessage));
    // The fetch has failed; the page hasn't reported back yet.
    respond(&mut app, fetch, Event::MessageFetched(Err(denied())));
    signed_out_as(&mut app, "denied");
    let effects = act(&mut app, Action::Retry);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Request(_, Request::FetchMessage(_))]
        ),
        "{effects:?}"
    );
    // The retried fetch asks the user to sign in again meanwhile.
    app.update(Msg::SignIn(browser_prompt()));
    let effects = respond(&mut app, list, Event::MessagesListed(Err(denied())));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Request(_, Request::ListMessages { .. })]
        ),
        "{effects:?}"
    );
}

#[test]
fn the_sign_in_window_keeps_a_pending_move() {
    let (mut app, second) = loading_more(1);
    act(&mut app, Action::NextEntry);
    app.update(Msg::SignIn(browser_prompt()));
    act(&mut app, Action::OpenBrowser);
    app.update(Msg::SignIn(finished(Ok(()))));
    let mut more = summaries(2);
    more.remove(0);
    listed(&mut app, second, more, None);
    assert_eq!(app.selected, 1, "moved on once the page arrived");
}

#[test]
fn closing_a_failed_sign_in_drops_waiting_retries() {
    let (mut app, _) = App::new(80, 23);
    signed_out_as(&mut app, "denied");
    app.retry_after.insert(RequestId(1));
    act(&mut app, Action::Exit);
    assert!(app.retry_after.is_empty());
}

#[test]
fn a_closed_failure_window_stays_closed_until_the_user_acts() {
    let (mut app, _) = App::new(80, 23);
    app.update(Msg::SignIn(finished(Err("unavailable".into()))));
    act(&mut app, Action::Exit);
    // A late failure from a request that was already waiting.
    app.update(Msg::SignIn(finished(Err("unavailable".into()))));
    assert_eq!(app.sign_in, None);
    // Once the user does something else, failures show again.
    act(&mut app, Action::NextEntry);
    app.update(Msg::SignIn(finished(Err("unavailable".into()))));
    assert!(app.sign_in.is_some());
}

#[test]
fn a_cancelled_sign_in_stays_quiet_after_other_keys() {
    let (mut app, _) = App::new(80, 23);
    app.update(Msg::SignIn(browser_prompt()));
    act(&mut app, Action::Exit);
    // A key before the cancellation is reported.
    act(&mut app, Action::NextEntry);
    app.update(Msg::SignIn(finished(Err("sign-in cancelled".into()))));
    assert_eq!(app.sign_in, None);
    // A later failure is shown.
    signed_out_as(&mut app, "denied");
    assert!(app.sign_in.is_some());
}

#[test]
fn retrying_while_quitting_waits_only_for_saves() {
    let (mut app, list) = loading_more(1);
    let (update, _) = sent(&open(&mut app, 1));
    act(&mut app, Action::Exit);
    act(&mut app, Action::Quit);
    assert!(app.quit);
    signed_out_as(&mut app, "denied");
    act(&mut app, Action::Retry);
    assert!(!app.retry_after.contains(&list), "listing isn't waited for");
    let effects = respond(&mut app, update, Event::FlagsUpdated(Err(denied())));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Request(_, Request::UpdateFlags(..))]
        ),
        "{effects:?}"
    );
}

#[test]
fn a_failed_fetch_is_retried_under_a_new_sign_in_window() {
    let mut app = loaded(1);
    let (fetch, _) = sent(&act(&mut app, Action::DisplayMessage));
    signed_out_as(&mut app, "denied");
    act(&mut app, Action::Retry);
    // Another sign-in starts before the fetch reports its failure.
    app.update(Msg::SignIn(browser_prompt()));
    let effects = respond(&mut app, fetch, Event::MessageFetched(Err(denied())));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Request(_, Request::FetchMessage(_))]
        ),
        "{effects:?}"
    );
}

#[test]
fn a_failed_sign_in_offers_no_stale_link() {
    let (mut app, _) = App::new(80, 23);
    app.update(Msg::SignIn(browser_prompt()));
    signed_out_as(&mut app, "denied");
    assert!(act(&mut app, Action::OpenBrowser).is_empty());
    assert!(act(&mut app, Action::CopyLink).is_empty());
    let rows = app.sign_in.as_ref().unwrap().rows(80);
    assert!(rows.iter().all(|(_, kind)| *kind != Row::Link), "{rows:?}");
}

#[test]
fn a_cancellation_too_late_is_forgotten() {
    let (mut app, _) = App::new(80, 23);
    app.update(Msg::SignIn(browser_prompt()));
    act(&mut app, Action::Exit);
    assert_eq!(app.status, "Sign-in cancelled.");
    app.update(Msg::SignIn(finished(Ok(()))));
    assert_eq!(app.status, "");
}

/// A 40×6 app whose sign-in window, with a long link, is scrolled down.
fn scrolled_sign_in() -> App {
    let (mut app, _) = App::new(40, 6);
    let url = format!("https://login.example/authorize?{}", "x".repeat(400));
    app.update(Msg::SignIn(SignInEvent::Prompt {
        account: AccountId::new("alice@contoso.com"),
        prompt: SignInPrompt::OpenBrowser {
            url: url.parse().unwrap(),
        },
    }));
    for _ in 0..30 {
        act(&mut app, Action::NextLine);
    }
    app
}

#[test]
fn the_sign_in_window_scroll_follows_resizes() {
    let mut app = scrolled_sign_in();
    let deep = app.sign_in.as_ref().unwrap().scroll;
    assert!(deep > 0);
    app.update(Msg::Resize {
        width: 40,
        height: 60,
    });
    assert_eq!(app.sign_in.as_ref().unwrap().scroll, 0, "it all fits now");
}

#[test]
fn a_failure_shows_in_a_scrolled_sign_in_window() {
    let mut app = scrolled_sign_in();
    assert!(app.sign_in.as_ref().unwrap().scroll > 0);
    signed_out(&mut app);
    let window = app.sign_in.as_ref().unwrap();
    let room = (app.body_height + CHROME_ROWS).saturating_sub(3);
    assert!(window.scroll <= window.rows(app.width).len().saturating_sub(room));
}

#[test]
fn the_mailbox_tells_the_users_own_addresses_once() {
    let (mut app, effects) = App::new(80, 23);
    let open = first_request(&effects);
    // Asked as soon as the folder opens, before any reply can be made.
    let effects = respond(
        &mut app,
        open,
        Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
    );
    let Some(Effect::Request(id, _)) = effects
        .iter()
        .find(|e| matches!(e, Effect::Request(_, Request::OwnAddresses)))
    else {
        panic!("expected OwnAddresses, got {effects:?}");
    };
    respond(
        &mut app,
        *id,
        Event::OwnAddresses(Ok(vec!["Alice@Contoso.com".into()])),
    );
    assert!(app.settings.me.contains(&"alice@contoso.com".to_owned()));
    // Not asked again when the folder is listed again.
    let (list, _) = sent(&app.relist());
    let effects = listed(&mut app, list, Vec::new(), None);
    assert!(effects.is_empty(), "{effects:?}");
}

/// The folder's counts, counted afresh.
fn counted(app: &App) -> (usize, usize) {
    (
        app.folder_messages().filter(|m| !m.flags.read).count(),
        app.folder_messages().filter(|m| m.flags.flagged).count(),
    )
}

#[test]
fn the_folder_counts_follow_the_messages() {
    let (mut app, more) = opened(Settings::default(), 24, summaries(3), Some("p2"));
    assert_eq!(app.folder_counts(), counted(&app));
    // Asked again, the same.
    let before = app.folder_counts();
    assert_eq!(app.folder_counts(), before);
    // Flags change.
    act(&mut app, Action::ToggleNew);
    assert_eq!(app.folder_counts(), counted(&app));
    assert_ne!(app.folder_counts(), before);
    act(&mut app, Action::FlagMessage);
    act(&mut app, Action::NextEntry);
    act(&mut app, Action::FlagMessage);
    assert_eq!(app.folder_counts().1, 2);
    assert_eq!(app.folder_counts(), counted(&app));
    act(&mut app, Action::ToggleNew);
    assert_eq!(app.folder_counts(), counted(&app));
    // Messages arrive: the next page.
    let mut next = summaries(2);
    for (i, m) in next.iter_mut().enumerate() {
        m.id = mett_core::MessageId::new(format!("p{i}"));
        m.flags.read = i == 0;
    }
    let (unread, flagged) = counted(&app);
    let more = more.expect("a page is asked for");
    let page = mett_core::MessagePage {
        messages: next,
        next: None,
    };
    respond(&mut app, more, Event::MessagesListed(Ok(page)));
    assert_eq!(app.total_messages(), 5);
    assert_eq!(app.folder_counts(), counted(&app));
    assert_eq!(app.folder_counts(), (unread + 1, flagged));
    // Drawing again (as the screen does) starts from the folder as it is.
    app.before_draw();
    assert_eq!(app.folder_counts(), counted(&app));
}

#[test]
fn addresses_are_the_user_s_whatever_their_case() {
    let mut app = empty_app();
    app.settings.me = vec!["me@contoso.com".to_owned()];
    let address = |email: &str| mett_core::Address {
        name: None,
        email: email.to_owned(),
    };
    assert!(app.is_me(&address("me@contoso.com")));
    assert!(app.is_me(&address("Me@Contoso.COM")));
    assert!(!app.is_me(&address("me@contoso.co")));
    assert!(!app.is_me(&address("you@contoso.com")));
    // Not ASCII: lowercased as before.
    app.settings.me.push("ünï@contoso.com".to_owned());
    assert!(app.is_me(&address("ÜNÏ@Contoso.com")));
}
