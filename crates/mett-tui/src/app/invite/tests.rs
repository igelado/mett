use chrono::{DateTime, TimeZone, Utc};
use mett_core::{Address, InvitationKind, Recurrence, RecurrenceUnit, Reply, Scope, mock};
use ratatui::crossterm::event::KeyCode;

use super::*;
use crate::app::Settings;
use crate::app::testing::*;
use crate::worker::Event;

fn invitation(repeats: bool) -> Invitation {
    Invitation {
        kind: InvitationKind::Request,
        title: None,
        start: Some(Utc.with_ymd_and_hms(2026, 11, 3, 14, 0, 0).unwrap()),
        end: Some(Utc.with_ymd_and_hms(2026, 11, 3, 15, 0, 0).unwrap()),
        all_day: false,
        location: None,
        organizer: Some(Address::new("alice@x.com").with_name("Alice")),
        attendees: Vec::new(),
        recurrence: repeats.then(|| Recurrence::new(RecurrenceUnit::Week, 1)),
        reply: Reply::None,
        out_of_date: false,
        response_requested: true,
        in_series: false,
        series_master: false,
        event: Some("ev".into()),
    }
}

/// An app in the index, with one message, m0.
fn index() -> App {
    let message = mock::message("m0", "in", "Planning", DateTime::UNIX_EPOCH).summary;
    opened(Settings::default(), 24, vec![message], None).0
}

/// An app showing m0, which says `invitation`.
fn showing(invitation: Option<Invitation>) -> App {
    let mut app = index();
    let fetch = first_request(&act(&mut app, Action::DisplayMessage));
    let mut message = mock::message("m0", "in", "Planning", DateTime::UNIX_EPOCH);
    message.invitation = invitation;
    respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
    app.status.clear();
    app
}

/// The lines of the message in the pager.
fn lines(app: &App) -> String {
    pager(app).lines.join("\n")
}

/// The request among `effects`, which answers an invitation.
fn answer_request(effects: &[Effect]) -> (RequestId, Answer) {
    let (id, request) = sent(effects);
    let Request::AnswerInvitation { answer, .. } = request else {
        panic!("{request:?}");
    };
    (id, answer)
}

#[test]
fn answers_a_meeting_after_a_comment_and_whether_to_tell() {
    let mut app = showing(Some(invitation(false)));
    assert!(act(&mut app, Action::AcceptInvite).is_empty());
    assert_eq!(prompt(&app), "Comment (optional): ");
    typed(&mut app, "On my way");
    assert!(key(&mut app, KeyCode::Enter).is_empty());
    assert_eq!(prompt(&app), "Send a response to Alice <alice@x.com>?");
    let effects = key(&mut app, KeyCode::Char('y'));
    let (id, request) = sent(&effects);
    assert_eq!(
        request,
        Request::AnswerInvitation {
            id: MessageId::new("m0"),
            invitation: invitation(false),
            answer: Answer {
                rsvp: Rsvp::Accept,
                scope: Scope::Series,
                comment: Some("On my way".into()),
                notify: true,
            },
        }
    );
    assert_eq!(app.status, "Sending answer...");
    assert!(lines(&app).contains("Your reply: not answered"));
    respond(&mut app, id, Event::InvitationAnswered(Ok(())));
    assert_eq!(app.status, "Accepted.");
    assert!(
        lines(&app).contains("Your reply: accepted"),
        "{}",
        lines(&app)
    );
}

#[test]
fn a_series_asks_what_the_answer_covers() {
    let mut app = showing(Some(invitation(true)));
    act(&mut app, Action::DeclineInvite);
    let asked = prompt(&app);
    assert!(asked.starts_with("Decline: [o]ccurrence"), "{asked}");
    assert!(asked.contains("[s]eries (Weekly)"), "{asked}");
    // Other keys wait for a choice.
    assert!(key(&mut app, KeyCode::Char('x')).is_empty());
    assert_eq!(prompt(&app), asked);
    key(&mut app, KeyCode::Char('o'));
    assert_eq!(prompt(&app), "Comment (optional): ");
    key(&mut app, KeyCode::Enter);
    let (_, answer) = answer_request(&key(&mut app, KeyCode::Char('n')));
    assert_eq!(answer.rsvp, Rsvp::Decline);
    assert_eq!(answer.scope, Scope::Occurrence);
    assert_eq!(answer.comment, None);
    assert!(!answer.notify);
}

#[test]
fn an_answer_for_one_occurrence_leaves_the_series_reply() {
    let mut app = showing(Some(invitation(true)));
    act(&mut app, Action::AcceptInvite);
    key(&mut app, KeyCode::Char('o'));
    key(&mut app, KeyCode::Enter);
    let (id, _) = answer_request(&key(&mut app, KeyCode::Enter));
    respond(&mut app, id, Event::InvitationAnswered(Ok(())));
    assert_eq!(app.status, "Accepted (this occurrence only).");
    assert!(
        lines(&app).contains("Your reply: not answered"),
        "{}",
        lines(&app)
    );
    // The series can be answered next.
    act(&mut app, Action::DeclineInvite);
    key(&mut app, KeyCode::Char('s'));
    key(&mut app, KeyCode::Enter);
    let (id, _) = answer_request(&key(&mut app, KeyCode::Enter));
    respond(&mut app, id, Event::InvitationAnswered(Ok(())));
    assert_eq!(app.status, "Declined.");
    assert!(lines(&app).contains("Your reply: declined"));
}

#[test]
fn the_occurrence_of_a_whole_day_meeting_is_named_by_its_date() {
    let mut all_day = invitation(true);
    all_day.all_day = true;
    all_day.start = Some(Utc.with_ymd_and_hms(2026, 11, 3, 0, 0, 0).unwrap());
    all_day.end = Some(Utc.with_ymd_and_hms(2026, 11, 4, 0, 0, 0).unwrap());
    let mut app = showing(Some(all_day));
    act(&mut app, Action::AcceptInvite);
    assert!(
        prompt(&app).contains("[o]ccurrence 03 Nov,"),
        "{}",
        prompt(&app)
    );
}

#[test]
fn a_message_about_one_occurrence_asks_too_and_shows_its_answer() {
    let mut one = invitation(false);
    one.in_series = true;
    let mut app = showing(Some(one));
    act(&mut app, Action::AcceptInvite);
    assert!(prompt(&app).contains("[o]ccurrence"), "{}", prompt(&app));
    key(&mut app, KeyCode::Char('o'));
    key(&mut app, KeyCode::Enter);
    let (id, answer) = answer_request(&key(&mut app, KeyCode::Enter));
    assert_eq!(answer.scope, Scope::Occurrence);
    respond(&mut app, id, Event::InvitationAnswered(Ok(())));
    // What the message says is of that occurrence, so it changes.
    assert_eq!(app.status, "Accepted.");
    assert!(
        lines(&app).contains("Your reply: accepted"),
        "{}",
        lines(&app)
    );
}

#[test]
fn the_whole_series_can_be_answered() {
    let mut app = showing(Some(invitation(true)));
    act(&mut app, Action::AcceptInvite);
    key(&mut app, KeyCode::Char('s'));
    key(&mut app, KeyCode::Enter);
    let (_, answer) = answer_request(&key(&mut app, KeyCode::Enter));
    assert_eq!((answer.rsvp, answer.scope), (Rsvp::Accept, Scope::Series));
    // Enter chose to tell the organizer.
    assert!(answer.notify);
}

#[test]
fn a_meeting_that_asked_for_no_answer_tells_nobody() {
    let mut invitation = invitation(false);
    invitation.response_requested = false;
    let mut app = showing(Some(invitation));
    // Nobody would read a comment, so none is asked for.
    let (_, answer) = answer_request(&act(&mut app, Action::TentativeInvite));
    assert_eq!(
        (answer.rsvp, answer.notify, answer.comment),
        (Rsvp::Tentative, false, None)
    );
    assert_eq!(prompt(&app), "");
}

#[test]
fn giving_up_on_a_question_answers_nothing() {
    let mut app = showing(Some(invitation(true)));
    act(&mut app, Action::AcceptInvite);
    assert!(key(&mut app, KeyCode::Esc).is_empty());
    assert_eq!(prompt(&app), "");
    assert!(app.answering.is_none());
    act(&mut app, Action::AcceptInvite);
    key(&mut app, KeyCode::Char('s'));
    key(&mut app, KeyCode::Esc);
    assert_eq!(prompt(&app), "");
    assert!(app.answering.is_none());
    act(&mut app, Action::AcceptInvite);
    key(&mut app, KeyCode::Char('c'));
    assert_eq!(prompt(&app), "");
}

#[test]
fn says_why_there_is_nothing_to_answer() {
    let mut app = showing(None);
    act(&mut app, Action::AcceptInvite);
    assert_eq!(app.status, "This message is not an invitation.");
    let mut cancelled = invitation(false);
    cancelled.kind = InvitationKind::Cancellation;
    let mut app = showing(Some(cancelled));
    act(&mut app, Action::AcceptInvite);
    assert_eq!(
        app.status,
        "Nothing to answer: it doesn't ask for an answer."
    );
    let mut mine = invitation(false);
    mine.reply = Reply::Organizer;
    let mut app = showing(Some(mine));
    act(&mut app, Action::DeclineInvite);
    assert_eq!(app.status, "Nothing to answer: you organize this meeting.");
    let mut old = invitation(false);
    old.out_of_date = true;
    let mut app = showing(Some(old));
    act(&mut app, Action::DeclineInvite);
    assert_eq!(app.status, "Nothing to answer: it is out of date.");
}

#[test]
fn a_failed_answer_is_reported_and_changes_nothing() {
    let mut app = showing(Some(invitation(false)));
    act(&mut app, Action::AcceptInvite);
    key(&mut app, KeyCode::Enter);
    let (id, _) = answer_request(&key(&mut app, KeyCode::Enter));
    respond(&mut app, id, Event::InvitationAnswered(Err(offline())));
    assert_eq!(app.status, "network error: offline");
    assert!(lines(&app).contains("Your reply: not answered"));
    // It can be tried again.
    act(&mut app, Action::AcceptInvite);
    assert_eq!(prompt(&app), "Comment (optional): ");
}

#[test]
fn another_answer_waits_for_the_one_being_sent() {
    let mut app = showing(Some(invitation(false)));
    act(&mut app, Action::AcceptInvite);
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Enter);
    act(&mut app, Action::DeclineInvite);
    assert_eq!(app.status, "Please wait: an answer is still being sent.");
    assert_eq!(prompt(&app), "");
}

#[test]
fn the_index_asks_to_open_the_message() {
    let mut app = index();
    act(&mut app, Action::AcceptInvite);
    assert_eq!(app.status, "Open the message to answer its invitation.");
}
