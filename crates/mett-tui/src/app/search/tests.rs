use chrono::{DateTime, TimeDelta};
use mett_core::{Address, MessageSummary, mock};
use ratatui::crossterm::event::KeyCode;

use super::*;
use crate::app::composing::Input;
use crate::app::testing::*;
use crate::app::{Msg, Settings};
use crate::lineedit::LineEditor;
use crate::worker::{Event, Request, RequestId};

/// Messages from alternating senders, newest first: m0 from Alice
/// ("Lunch 0"), m1 from Bob ("Report 1"), and so on; m0 and m1 unread.
fn app() -> App {
    let (mut app, list) = opening(Settings::default(), 24);
    let messages = (0..6)
        .map(|i| {
            let date = DateTime::UNIX_EPOCH + TimeDelta::minutes(100 - i);
            let (from, subject) = if i % 2 == 0 {
                ("alice@contoso.com", "Lunch")
            } else {
                ("bob@fabrikam.com", "Report")
            };
            let mut m =
                mock::message(&format!("m{i}"), "in", &format!("{subject} {i}"), date).summary;
            m.from = Some(Address::new(from));
            m.flags.read = i >= 2;
            m
        })
        .collect();
    listed(&mut app, list, messages, None);
    app
}

/// Types `text` over what the question holds, and presses Enter.
fn answer(app: &mut App, text: &str) -> Vec<Effect> {
    if let Some(Input::Line { editor, .. }) = &mut app.input {
        *editor = LineEditor::new("");
    }
    typed(app, text);
    key(app, KeyCode::Enter)
}

/// Answers the fetch of m0 ("Lunch 0") with `body`.
fn fetched(app: &mut App, fetch: RequestId, body: String) {
    let mut message = mock::message("m0", "in", "Lunch 0", DateTime::UNIX_EPOCH);
    message.body.content = body;
    respond(app, fetch, Event::MessageFetched(Ok(message)));
}

/// Shows the selected message as m0 ("Lunch 0"), with `body`.
fn show(app: &mut App, body: String) {
    let fetch = first_request(&act(app, Action::DisplayMessage));
    fetched(app, fetch, body);
}

#[test]
fn searches_the_index_and_wraps() {
    let mut app = app();
    act(&mut app, Action::Search);
    answer(&mut app, "~f bob");
    assert_eq!(app.index.selected, 1);
    act(&mut app, Action::SearchNext);
    act(&mut app, Action::SearchNext);
    assert_eq!(app.index.selected, 5);
    act(&mut app, Action::SearchNext);
    assert_eq!(app.index.selected, 1);
    assert_eq!(app.status, "Search wrapped to top.");
    // Plain text searches the sender and the subject.
    act(&mut app, Action::Search);
    answer(&mut app, "lunch 4");
    assert_eq!(app.index.selected, 4);
    act(&mut app, Action::Search);
    answer(&mut app, "nobody");
    assert_eq!(app.status, "Not found.");
    assert_eq!(app.index.selected, 4);
    act(&mut app, Action::Search);
    answer(&mut app, "~q");
    assert!(app.status.contains("not supported"), "{}", app.status);
}

#[test]
fn limits_the_index_and_lifts_the_limit() {
    let mut app = app();
    act(&mut app, Action::NextEntry);
    act(&mut app, Action::Limit);
    answer(&mut app, "~f bob");
    assert_eq!(ids(&app), ["m1", "m3", "m5"]);
    assert_eq!(app.index.selected, 0, "still on m1");
    assert_eq!(app.total_messages(), 6);
    assert_eq!(app.index.limit.as_ref().unwrap().text, "~f bob");

    act(&mut app, Action::Limit);
    answer(&mut app, "~N");
    assert_eq!(ids(&app), ["m0", "m1"]);
    assert_eq!(app.index.selected, 1, "still on m1");

    // A bad pattern changes nothing.
    act(&mut app, Action::Limit);
    answer(&mut app, "~N (");
    assert_eq!(ids(&app), ["m0", "m1"]);

    act(&mut app, Action::Limit);
    answer(&mut app, "all");
    assert_eq!(ids(&app), ["m0", "m1", "m2", "m3", "m4", "m5"]);
    assert!(app.index.limit.is_none());

    act(&mut app, Action::Limit);
    answer(&mut app, "~F");
    assert!(app.index.messages.is_empty());
    assert_eq!(app.status, "No messages matched the criteria.");
}

#[test]
fn searches_the_text_in_the_pager() {
    let mut app = app();
    show(&mut app, (0..60).map(|i| format!("line {i}\n")).collect());
    act(&mut app, Action::Search);
    answer(&mut app, "line 1[0-9]");
    let view = pager(&app);
    assert_eq!(view.row(view.scroll), "line 10");
    act(&mut app, Action::SearchNext);
    let view = pager(&app);
    assert_eq!(view.row(view.scroll), "line 11");
}

#[test]
fn pager_search_moves_on_from_a_match_near_the_end() {
    let mut app = app();
    show(&mut app, (0..60).map(|i| format!("line {i}\n")).collect());
    // Both matches are on the last screen.
    act(&mut app, Action::Search);
    answer(&mut app, "line 5[89]");
    act(&mut app, Action::SearchNext);
    let view = pager(&app);
    assert_eq!(view.lines[view.search_match.unwrap().0], "line 59");
    act(&mut app, Action::SearchNext);
    let view = pager(&app);
    assert_eq!(view.lines[view.search_match.unwrap().0], "line 58");
    assert_eq!(app.status, "Search wrapped to top.");
}

#[test]
fn a_failed_flag_change_is_undone_for_a_hidden_message() {
    let mut app = app();
    let effects = act(&mut app, Action::FlagMessage);
    let Effect::Request(flag, _) = effects[0].clone() else {
        panic!("expected a flag update, got {effects:?}");
    };
    // Limit to Bob's messages: m0, being flagged, is hidden.
    act(&mut app, Action::Limit);
    answer(&mut app, "~f bob");
    respond(&mut app, flag, Event::FlagsUpdated(Err(offline())));
    act(&mut app, Action::Limit);
    answer(&mut app, "all");
    assert!(
        !app.index.messages[0].flags.flagged,
        "back as in the mailbox"
    );
}

#[test]
fn lifting_a_limit_keeps_the_listing_order() {
    let mut app = app();
    // Same dates: the order comes from the listing, not the date.
    for m in &mut app.index.messages {
        m.received = DateTime::UNIX_EPOCH;
    }
    act(&mut app, Action::Limit);
    answer(&mut app, "~f alice");
    assert_eq!(ids(&app), ["m0", "m2", "m4"]);
    act(&mut app, Action::Limit);
    answer(&mut app, "all");
    assert_eq!(ids(&app), ["m0", "m1", "m2", "m3", "m4", "m5"]);
}

#[test]
fn pager_search_finds_text_across_a_wrap() {
    let mut app = app();
    app.update(Msg::Resize {
        width: 20,
        height: 24,
    });
    show(
        &mut app,
        "first line\nsome words before the important phrase\n".into(),
    );
    act(&mut app, Action::Search);
    answer(&mut app, "important phrase");
    assert_ne!(app.status, "Not found.");
    let view = pager(&app);
    assert!(view.lines[view.search_match.unwrap().0].contains("important phrase"));
}

#[test]
fn retrying_a_save_reaches_a_hidden_message() {
    let mut app = app();
    let effects = act(&mut app, Action::FlagMessage);
    let Effect::Request(flag, _) = effects[0].clone() else {
        panic!("expected a flag update, got {effects:?}");
    };
    act(&mut app, Action::Limit);
    answer(&mut app, "~f bob");
    respond(&mut app, flag, Event::FlagsUpdated(Err(expired())));
    app.update(Msg::SignIn(crate::signin::SignInEvent::Finished {
        account: mett_core::AccountId::new("demo@example.com"),
        outcome: Err("expired".into()),
    }));
    let effects = act(&mut app, Action::Retry);
    assert!(
        effects.iter().any(
            |e| matches!(e, Effect::Request(_, Request::UpdateFlags(id, _)) if id.as_str() == "m0")
        ),
        "{effects:?}"
    );
}

#[test]
fn index_and_pager_searches_are_kept_apart() {
    let mut app = app();
    act(&mut app, Action::Search);
    answer(&mut app, "nobody");
    assert_eq!(app.status, "Not found.");
    act(&mut app, Action::Search);
    answer(&mut app, "~f bob");
    assert_eq!(app.index.selected, 1);
    assert_eq!(app.status, "", "the earlier Not found. is gone");

    let fetch = first_request(&act(&mut app, Action::DisplayMessage));
    respond(
        &mut app,
        fetch,
        Event::MessageFetched(Ok(mock::message("m1", "in", "s", DateTime::UNIX_EPOCH))),
    );
    act(&mut app, Action::SearchNext);
    assert_eq!(app.status, "No search pattern.", "not the index's ~f bob");
    act(&mut app, Action::Search);
    let Some(Input::Line { editor, .. }) = &app.input else {
        panic!("no question");
    };
    assert_eq!(editor.text(), "");
}

#[test]
fn pager_search_shows_matches_deep_in_a_long_line() {
    let mut app = app();
    app.update(Msg::Resize {
        width: 20,
        height: 8,
    });
    // One line, many rows, with two matches far down it.
    show(
        &mut app,
        format!("{} needle one needle two\nend\n", "word ".repeat(60)),
    );
    act(&mut app, Action::Search);
    answer(&mut app, "needle");
    let shown = |app: &App| {
        let view = pager(app);
        (view.scroll..view.row_count())
            .take(app.body_height)
            .map(|row| view.row(row))
            .collect::<Vec<_>>()
            .join("|")
    };
    assert!(shown(&app).contains("needle"), "{}", shown(&app));
    // The second match in the same line, not the first again.
    act(&mut app, Action::SearchNext);
    let view = pager(&app);
    let (line, end) = view.search_match.unwrap();
    assert!(view.lines[line][..end].ends_with("needle one needle"));
    assert!(shown(&app).contains("two"), "{}", shown(&app));
    assert_eq!(app.status, "");
    act(&mut app, Action::SearchNext);
    assert_eq!(app.status, "Search wrapped to top.");
}

#[test]
fn a_working_limit_clears_an_earlier_complaint() {
    let mut app = app();
    act(&mut app, Action::Limit);
    answer(&mut app, "~F");
    assert_eq!(app.status, "No messages matched the criteria.");
    act(&mut app, Action::Limit);
    answer(&mut app, "all");
    assert_eq!(app.status, "");
}

/// An app whose first page (m0 from Alice) is listed, with more to
/// come; returns the app and the request for the next page.
fn loading() -> (App, RequestId) {
    let (mut app, list) = opening(Settings::default(), 24);
    let mut first = mock::message("m0", "in", "Lunch", DateTime::UNIX_EPOCH).summary;
    first.from = Some(Address::new("alice@contoso.com"));
    let next = page(&mut app, list, vec![first], "c");
    (app, next)
}

fn bob() -> MessageSummary {
    let mut m = mock::message("m1", "in", "Report", DateTime::UNIX_EPOCH).summary;
    m.from = Some(Address::new("bob@fabrikam.com"));
    m
}

#[test]
fn a_search_goes_on_as_pages_arrive() {
    let (mut app, next) = loading();
    act(&mut app, Action::Search);
    answer(&mut app, "~f bob");
    assert_eq!(app.status, "Searching...");
    listed(&mut app, next, vec![bob()], None);
    assert_eq!(app.index.selected, 1);
    assert_eq!(app.status, "");
}

#[test]
fn a_limit_that_matched_nothing_yet_stops_saying_so() {
    let (mut app, next) = loading();
    act(&mut app, Action::Limit);
    answer(&mut app, "~f bob");
    assert_eq!(app.status, "No messages matched the criteria.");
    listed(&mut app, next, vec![bob()], None);
    assert_eq!(ids(&app), ["m1"]);
    assert_eq!(app.status, "");
}

#[test]
fn empty_matches_at_line_ends_move_on() {
    let mut app = app();
    show(&mut app, "one\ntwo\n".into());
    act(&mut app, Action::Search);
    answer(&mut app, "$");
    let line = |app: &App| {
        let view = pager(app);
        view.search_match.unwrap().0
    };
    let first = line(&app);
    act(&mut app, Action::SearchNext);
    assert_ne!(line(&app), first, "moved to another line");
}

#[test]
fn a_new_pager_query_starts_on_screen() {
    let mut app = app();
    show(&mut app, "apple banana apple banana\n".into());
    act(&mut app, Action::Search);
    answer(&mut app, "banana");
    act(&mut app, Action::Search);
    answer(&mut app, "apple");
    let view = pager(&app);
    let (line, end) = view.search_match.unwrap();
    // The first apple on the line, not the one after the banana.
    assert!(
        view.lines[line][..end].ends_with("apple") && end < 20,
        "{end}"
    );
}

#[test]
fn a_search_reads_the_whole_folder_before_wrapping() {
    // m0 (alice) is selected and matches, but so may later pages.
    let (mut app, next) = loading();
    act(&mut app, Action::Search);
    answer(&mut app, "~f alice|~f bob");
    assert_eq!(app.status, "Searching...");
    assert_eq!(app.index.selected, 0);
    listed(&mut app, next, vec![bob()], None);
    assert_eq!(app.index.selected, 1, "the next match, not the wrap");
    act(&mut app, Action::SearchNext);
    assert_eq!(app.index.selected, 0);
    assert_eq!(app.status, "Search wrapped to top.");
}

#[test]
fn a_pager_search_waits_for_the_message() {
    let mut app = app();
    let fetch = first_request(&act(&mut app, Action::DisplayMessage));
    act(&mut app, Action::Search);
    answer(&mut app, "line 1[0-9]");
    assert_eq!(app.status, "Searching...");
    fetched(
        &mut app,
        fetch,
        (0..60).map(|i| format!("line {i}\n")).collect(),
    );
    let view = pager(&app);
    assert_eq!(view.row(view.scroll), "line 10");
    assert_eq!(app.status, "");
}

#[test]
fn the_pager_highlights_what_it_found() {
    let mut app = app();
    show(&mut app, "one two one\n".into());
    act(&mut app, Action::Search);
    answer(&mut app, "one");
    let view = pager(&app);
    let found = highlighted(view);
    assert_eq!(found.len(), 1);
    let (row, range) = &found[0];
    assert_eq!(&view.row(*row)[range.clone()], "one");
    let first = range.start;
    act(&mut app, Action::SearchNext);
    let view = pager(&app);
    let (_, range) = &highlighted(view)[0];
    assert!(range.start > first, "the second one");
}

#[test]
fn a_resized_pager_is_searched_as_shown() {
    let mut app = app();
    show(
        &mut app,
        format!("{} needle\n{}", "word ".repeat(80), "more\n".repeat(40)),
    );
    // Resized while something else covered the pager.
    app.width = 40;
    act(&mut app, Action::Search);
    answer(&mut app, "needle");
    let view = pager(&app);
    assert_eq!(view.rows_width, Some(40));
    assert!(view.row(view.scroll).contains("needle"));
}

#[test]
fn a_search_waits_for_the_folder_to_open() {
    let (mut app, effects) = App::open(80, 24, inbox(), Settings::default());
    let open = first_request(&effects);
    act(&mut app, Action::Search);
    answer(&mut app, "~f bob");
    assert_eq!(app.status, "Searching...");
    let list = folder_opened(&mut app, open, mock::folder("in", "Inbox", None));
    let mut alice = mock::message("m0", "in", "Lunch", DateTime::UNIX_EPOCH).summary;
    alice.from = Some(Address::new("alice@contoso.com"));
    listed(&mut app, list, vec![alice, bob()], None);
    assert_eq!(app.index.selected, 1);
}

#[test]
fn a_search_loads_a_page_that_failed() {
    let (mut app, next) = loading();
    respond(&mut app, next, Event::MessagesListed(Err(offline())));
    act(&mut app, Action::Search);
    let effects = answer(&mut app, "~f bob");
    let again = first_request(&effects);
    assert_eq!(app.status, "Searching...");
    listed(&mut app, again, vec![bob()], None);
    assert_eq!(app.index.selected, 1);
}

#[test]
fn a_search_stays_with_its_folder() {
    let (mut app, _) = loading();
    act(&mut app, Action::ChangeFolder);
    let effects = answer(&mut app, "=Archive");
    let open = first_request(&effects);
    // Searched while the other folder is being opened, and this one's
    // pages are still on their way.
    act(&mut app, Action::Search);
    answer(&mut app, "~f bob");
    assert_eq!(app.status, "Searching...");
    let effects = respond(
        &mut app,
        open,
        Event::FolderOpened(Ok(mock::folder("ar", "Archive", None))),
    );
    let list = first_request(&effects);
    let mut other = mock::message("m9", "ar", "Old", DateTime::UNIX_EPOCH).summary;
    other.from = Some(Address::new("carol@contoso.com"));
    listed(&mut app, list, vec![other, bob()], None);
    assert_eq!(app.index.selected, 0, "the old search doesn't run here");
}

#[test]
fn moving_on_stops_saying_searching() {
    let (mut app, _) = loading();
    act(&mut app, Action::Search);
    answer(&mut app, "~f bob");
    assert_eq!(app.status, "Searching...");
    act(&mut app, Action::DisplayMessage);
    assert_ne!(app.status, "Searching...");
}

#[test]
fn a_widened_pager_is_searched_from_what_it_shows() {
    let mut app = app();
    show(
        &mut app,
        (0..60)
            .map(|i| format!("row {i:02} {}\n", "x".repeat(140)))
            .collect(),
    );
    let Some(Screen::Pager(pager)) = app.screens.last_mut() else {
        panic!("not in the pager");
    };
    // At the end, then widened while another screen covered it.
    pager.scroll = pager.row_count() - app.body_height;
    app.width = 200;
    act(&mut app, Action::Search);
    answer(&mut app, "row 4[0-9]");
    assert_eq!(app.status, "", "found below, without wrapping");
}

/// Sets `wrap`, as `:set wrap=…` does.
fn wrap_at(app: &mut App, wrap: i64) {
    let mut settings = Settings::default();
    settings.display.wrap = wrap;
    app.update(configured(settings));
}

#[test]
fn the_pager_wraps_where_wrap_says() {
    use unicode_width::UnicodeWidthStr;
    let mut app = app();
    show(&mut app, format!("{}\n", "word ".repeat(40)));
    assert_eq!(pager(&app).rows_width, Some(80), "the terminal's width");
    // At most the terminal's width; a negative one leaves room on the
    // right, unless that leaves nothing.
    for (wrap, width) in [
        (30, 30),
        (200, 80),
        (-20, 60),
        (-80, 80),
        (-100, 80),
        (0, 80),
    ] {
        wrap_at(&mut app, wrap);
        let view = pager(&app);
        assert_eq!(view.rows_width, Some(width), "wrap={wrap}");
        assert!(
            (0..view.row_count()).all(|row| view.row(row).width() <= width),
            "wrap={wrap}"
        );
    }
}

#[test]
fn setting_wrap_wraps_a_covered_pager_again() {
    let mut app = app();
    show(&mut app, format!("{} needle\n", "word ".repeat(40)));
    act(&mut app, Action::Search);
    answer(&mut app, "needle");
    act(&mut app, Action::BrowseMailboxes);
    wrap_at(&mut app, 25);
    let pager = app
        .screens
        .iter()
        .find_map(|s| match s {
            Screen::Pager(p) => Some(p),
            _ => None,
        })
        .unwrap();
    assert_eq!(pager.rows_width, Some(25));
    let found = highlighted(pager);
    assert_eq!(&pager.row(found[0].0)[found[0].1.clone()], "needle");
}

#[test]
fn a_narrower_wrap_is_searched_as_shown() {
    let mut app = app();
    wrap_at(&mut app, 20);
    show(
        &mut app,
        format!("{} needle\n{}", "word ".repeat(80), "more\n".repeat(40)),
    );
    assert_eq!(pager(&app).rows_width, Some(20));
    act(&mut app, Action::Search);
    answer(&mut app, "needle");
    let view = pager(&app);
    assert!(view.row(view.scroll).contains("needle"));
    let found = highlighted(view);
    assert_eq!(found.len(), 1);
    assert_eq!(&view.row(found[0].0)[found[0].1.clone()], "needle");
}

#[test]
fn the_pager_pages_and_searches_in_the_rows_the_mini_index_leaves() {
    let mut app = app();
    let mut settings = Settings::default();
    settings.display.pager_index_lines = 6;
    app.update(configured(settings));
    show(&mut app, (0..60).map(|i| format!("line {i}\n")).collect());
    let height = app.body_height - 6;
    assert_eq!(app.pager_height(), height);
    act(&mut app, Action::NextPage);
    assert_eq!(pager(&app).scroll, height);
    // The last page is full, in the rows left.
    let last = pager(&app).row_count() - height;
    act(&mut app, Action::Search);
    answer(&mut app, "line 59");
    assert_eq!(pager(&app).scroll, last);
    for _ in 0..10 {
        act(&mut app, Action::NextPage);
    }
    assert_eq!(pager(&app).scroll, last);
    // Without it, the pager has the rows back.
    app.update(configured(Settings::default()));
    assert_eq!(app.pager_height(), app.body_height);
    assert_eq!(pager(&app).scroll, last - 6);
}

#[test]
fn a_search_before_any_message_includes_the_first() {
    let (mut app, effects) = App::open(80, 24, inbox(), Settings::default());
    let open = first_request(&effects);
    act(&mut app, Action::Search);
    answer(&mut app, "~f bob");
    let list = folder_opened(&mut app, open, mock::folder("in", "Inbox", None));
    let mut other = bob();
    other.id = "m2".into();
    listed(&mut app, list, vec![bob(), other], None);
    assert_eq!(app.index.selected, 0, "the first match");
    assert_eq!(app.status, "");
}

#[test]
fn leaving_a_loading_message_stops_searching() {
    let mut app = app();
    act(&mut app, Action::DisplayMessage);
    act(&mut app, Action::Search);
    answer(&mut app, "anything");
    assert_eq!(app.status, "Searching...");
    act(&mut app, Action::Exit);
    assert_eq!(app.screen(), &Screen::Index);
    assert_ne!(app.status, "Searching...");
}

#[test]
fn a_search_waits_for_a_page_through_a_sign_in() {
    let (mut app, next) = loading();
    act(&mut app, Action::Search);
    answer(&mut app, "~f bob");
    signed_out(&mut app);
    respond(&mut app, next, Event::MessagesListed(Err(expired())));
    let effects = app.retry();
    let again = request_for(&effects, |r| matches!(r, Request::ListMessages { .. }))
        .expect("the page again");
    listed(&mut app, again, vec![bob()], None);
    assert_eq!(app.index.selected, 1);
}

#[test]
fn a_pager_search_waits_for_its_message_through_a_sign_in() {
    let mut app = app();
    let fetch = first_request(&act(&mut app, Action::DisplayMessage));
    act(&mut app, Action::Search);
    answer(&mut app, "line 1[0-9]");
    signed_out(&mut app);
    respond(&mut app, fetch, Event::MessageFetched(Err(expired())));
    let effects = app.retry();
    let again =
        request_for(&effects, |r| matches!(r, Request::FetchMessage(_))).expect("fetched again");
    fetched(
        &mut app,
        again,
        (0..60).map(|i| format!("line {i}\n")).collect(),
    );
    let view = pager(&app);
    assert_eq!(view.row(view.scroll), "line 10");
}

#[test]
fn next_in_an_empty_limited_index_takes_the_first_match() {
    let (mut app, next) = loading();
    act(&mut app, Action::Limit);
    answer(&mut app, "~f bob");
    assert!(app.index.messages.is_empty(), "alice is hidden");
    act(&mut app, Action::NextEntry);
    let mut second = bob();
    second.id = "m2".into();
    listed(&mut app, next, vec![bob(), second], None);
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "m1");
}

#[test]
fn a_covered_pager_keeps_its_highlight_through_a_resize() {
    let mut app = app();
    show(&mut app, format!("{} needle\n", "word ".repeat(30)));
    act(&mut app, Action::Search);
    answer(&mut app, "needle");
    act(&mut app, Action::BrowseMailboxes);
    app.update(Msg::Resize {
        width: 50,
        height: 24,
    });
    let pager = app
        .screens
        .iter()
        .find_map(|s| match s {
            Screen::Pager(p) => Some(p),
            _ => None,
        })
        .unwrap();
    assert_eq!(pager.rows_width, Some(50));
    let found = highlighted(pager);
    assert_eq!(&pager.row(found[0].0)[found[0].1.clone()], "needle");
}

#[test]
fn a_space_shown_after_a_wrap_is_highlighted_where_it_is() {
    let mut app = app();
    show(&mut app, format!("{}  y\n", "x".repeat(80)));
    act(&mut app, Action::Search);
    answer(&mut app, "x  y");
    act(&mut app, Action::Search);
    answer(&mut app, r"\s y");
    let view = pager(&app);
    let found = highlighted(view);
    assert_eq!(found.len(), 1, "{found:?}");
    let (row, range) = &found[0];
    assert_eq!(&view.row(*row)[range.clone()], " y");
}

#[test]
fn a_search_waits_for_the_folder_through_a_sign_in() {
    let (mut app, effects) = App::open(80, 24, inbox(), Settings::default());
    let open = first_request(&effects);
    act(&mut app, Action::Search);
    answer(&mut app, "~f bob");
    signed_out(&mut app);
    respond(&mut app, open, Event::FolderOpened(Err(expired())));
    let (again, _) = requests(&app.retry())[0].clone();
    let effects = respond(
        &mut app,
        again,
        Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
    );
    let (list, _) = requests(&effects)[0].clone();
    let mut alice = mock::message("m0", "in", "Lunch", DateTime::UNIX_EPOCH).summary;
    alice.from = Some(Address::new("alice@contoso.com"));
    listed(&mut app, list, vec![alice, bob()], None);
    assert_eq!(app.index.selected, 1);
}

#[test]
fn next_in_an_empty_limited_index_keeps_waiting_through_empty_pages() {
    let (mut app, next) = loading();
    act(&mut app, Action::Limit);
    answer(&mut app, "~f bob");
    act(&mut app, Action::NextEntry);
    // A page without a match: still nothing to move to.
    let mut alice = mock::message("m5", "in", "Lunch", DateTime::UNIX_EPOCH).summary;
    alice.from = Some(Address::new("alice@contoso.com"));
    let effects = listed(&mut app, next, vec![alice], Some("d"));
    assert!(app.index.pending.is_some(), "still waiting");
    let last = first_request(&effects);
    listed(&mut app, last, vec![bob()], None);
    assert_eq!(app.index.messages[app.index.selected].id.as_str(), "m1");
    assert!(app.index.pending.is_none());
}

#[test]
fn a_relist_keeps_the_limit_s_messages() {
    let mut app = app();
    act(&mut app, Action::Limit);
    answer(&mut app, "~N");
    let shown = ids(&app)
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>();
    assert_eq!(shown, ["m0", "m1"]);
    // m0 is read now; a relist (say, new mail) brings it back read.
    let effects = app.relist();
    let list = first_request(&effects);
    let messages = (0..6)
        .map(|i| {
            let mut m = mock::message(&format!("m{i}"), "in", "s", DateTime::UNIX_EPOCH).summary;
            m.flags.read = i != 1;
            m
        })
        .collect();
    listed(&mut app, list, messages, None);
    assert_eq!(ids(&app), ["m0", "m1"], "still shown, as in mutt");
}
