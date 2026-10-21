//! Answering a meeting invitation: `accept-invite`, `tentative-invite` and
//! `decline-invite`, in the pager. For a meeting that repeats, the answer
//! is for the occurrence the message is about or for the series; then
//! comes a comment, and whether the organizer is told.

use mett_core::{Answer, Invitation, MessageId, Rsvp, Scope};

use super::composing::{Input, Question};
use super::{App, Effect, Screen, effect_id};
use crate::action::Action;
use crate::worker::{Request, RequestId};

/// An answer being given: asked about, then on its way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Answering {
    message: MessageId,
    invitation: Invitation,
    rsvp: Rsvp,
    scope: Scope,
    comment: Option<String>,
    /// The request sending it, once it is sent.
    request: Option<RequestId>,
}

/// What the question about an answer starts with.
fn verb(rsvp: Rsvp) -> &'static str {
    match rsvp {
        Rsvp::Accept => "Accept",
        Rsvp::Tentative => "Tentatively accept",
        Rsvp::Decline => "Decline",
    }
}

/// What is said once an answer was sent.
fn done(rsvp: Rsvp) -> &'static str {
    match rsvp {
        Rsvp::Accept => "Accepted.",
        Rsvp::Tentative => "Tentatively accepted.",
        Rsvp::Decline => "Declined.",
    }
}

impl App {
    /// Handles the actions that answer an invitation. Returns `None` for
    /// other actions.
    pub(super) fn on_invite_action(&mut self, action: Action) -> Option<Vec<Effect>> {
        let rsvp = match action {
            Action::AcceptInvite => Rsvp::Accept,
            Action::TentativeInvite => Rsvp::Tentative,
            Action::DeclineInvite => Rsvp::Decline,
            _ => return None,
        };
        self.begin_answer(rsvp);
        Some(Vec::new())
    }

    fn begin_answer(&mut self, rsvp: Rsvp) {
        if self.answering.as_ref().is_some_and(|a| a.request.is_some()) {
            self.fail("Please wait: an answer is still being sent.");
            return;
        }
        let Some(Screen::Pager(pager)) = self.screens.last() else {
            self.fail("Open the message to answer its invitation.");
            return;
        };
        let Some(message) = &pager.message else {
            self.fail("The message is still loading.");
            return;
        };
        let Some(invitation) = message.invitation.clone() else {
            self.fail("This message is not an invitation.");
            return;
        };
        if let Some(why) = invitation.cannot_answer() {
            self.fail(format!("Nothing to answer: {why}."));
            return;
        }
        let repeats = invitation.repeats();
        self.answering = Some(Answering {
            message: pager.id.clone(),
            invitation,
            rsvp,
            scope: Scope::Series,
            comment: None,
            request: None,
        });
        if repeats {
            self.ask_scope();
        } else {
            self.ask_comment();
        }
    }

    fn ask_scope(&mut self) {
        let Some(answering) = &self.answering else {
            return;
        };
        // A meeting of whole days is by its date, whatever the time zone.
        let when = answering
            .invitation
            .start
            .map(|start| {
                if answering.invitation.all_day {
                    format!(" {}", start.format("%d %b"))
                } else {
                    format!(" {}", start.with_timezone(&chrono::Local).format("%d %b"))
                }
            })
            .unwrap_or_default();
        let series = answering
            .invitation
            .recurrence
            .as_ref()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default();
        self.input = Some(Input::Choice {
            prompt: format!(
                "{}: [o]ccurrence{when}, [s]eries{series}, [c]ancel? ",
                verb(answering.rsvp)
            ),
            keys: vec!['o', 's'],
            question: Question::InviteScope,
        });
    }

    fn ask_comment(&mut self) {
        self.ask(Question::InviteComment, "Comment (optional): ", "");
    }

    /// The user chose what the answer covers.
    pub(super) fn answer_scope(&mut self, key: char) -> Vec<Effect> {
        if let Some(answering) = &mut self.answering {
            answering.scope = if key == 'o' {
                Scope::Occurrence
            } else {
                Scope::Series
            };
            self.ask_comment();
        }
        Vec::new()
    }

    /// The user typed the comment: now to whether the organizer is told.
    pub(super) fn answer_comment(&mut self, text: String) -> Vec<Effect> {
        let Some(answering) = &mut self.answering else {
            return Vec::new();
        };
        let text = text.trim();
        answering.comment = (!text.is_empty()).then(|| text.to_owned());
        // Nobody who didn't ask for an answer is told one.
        if !answering.invitation.response_requested {
            return self.send_answer(false);
        }
        let who = answering
            .invitation
            .organizer
            .as_ref()
            .map_or("the organizer".to_owned(), ToString::to_string);
        self.input = Some(Input::YesNo {
            prompt: format!("Send a response to {who}?"),
            default: true,
            question: Question::InviteNotify,
        });
        Vec::new()
    }

    /// The user said whether the organizer is told.
    pub(super) fn answer_notify(&mut self, notify: bool) -> Vec<Effect> {
        self.send_answer(notify)
    }

    fn send_answer(&mut self, notify: bool) -> Vec<Effect> {
        let Some(answering) = self.answering.take() else {
            return Vec::new();
        };
        let answer = Answer {
            rsvp: answering.rsvp,
            scope: answering.scope,
            comment: answering.comment.clone(),
            notify,
        };
        let request = self.request(Request::AnswerInvitation {
            id: answering.message.clone(),
            invitation: answering.invitation.clone(),
            answer,
        });
        self.answering = Some(Answering {
            request: Some(effect_id(&request)),
            ..answering
        });
        self.status = "Sending answer...".into();
        vec![request]
    }

    /// The user gave up on the questions.
    pub(super) fn cancel_answer(&mut self) {
        self.answering = None;
    }

    /// The answer was sent, or failed.
    pub(super) fn on_invitation_answered(
        &mut self,
        id: RequestId,
        result: mett_core::Result<()>,
    ) -> Vec<Effect> {
        let Some(answering) = self.answering.take_if(|a| a.request == Some(id)) else {
            return Vec::new();
        };
        if let Err(e) = result {
            self.report(e);
            return Vec::new();
        }
        // An answer for one occurrence leaves the series' as it was.
        let own = answering.scope == Scope::Series || !answering.invitation.repeats();
        self.status = if own {
            done(answering.rsvp).into()
        } else {
            format!(
                "{} (this occurrence only).",
                done(answering.rsvp).trim_end_matches('.')
            )
        };
        // The message shows the new answer, if it is still open.
        let width = self.wrap_width();
        for screen in &mut self.screens {
            let Screen::Pager(pager) = screen else {
                continue;
            };
            if pager.id != answering.message {
                continue;
            }
            if let Some(mut message) = pager.message.take() {
                if let Some(invitation) = message.invitation.as_mut().filter(|_| own) {
                    invitation.reply = answering.rsvp.reply();
                }
                pager.show(&message, width);
                pager.message = Some(message);
            }
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
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
        act(&mut app, Action::TentativeInvite);
        let (_, answer) = answer_request(&key(&mut app, KeyCode::Enter));
        assert_eq!((answer.rsvp, answer.notify), (Rsvp::Tentative, false));
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
}
