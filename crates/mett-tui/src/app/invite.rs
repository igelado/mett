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
        Some(self.begin_answer(rsvp))
    }

    fn begin_answer(&mut self, rsvp: Rsvp) -> Vec<Effect> {
        if self.answering.as_ref().is_some_and(|a| a.request.is_some()) {
            self.fail("Please wait: an answer is still being sent.");
            return Vec::new();
        }
        let Some(Screen::Pager(pager)) = self.screens.last() else {
            self.fail("Open the message to answer its invitation.");
            return Vec::new();
        };
        let Some(message) = &pager.message else {
            self.fail("The message is still loading.");
            return Vec::new();
        };
        let Some(invitation) = message.invitation.clone() else {
            self.fail("This message is not an invitation.");
            return Vec::new();
        };
        if let Some(why) = invitation.cannot_answer() {
            self.fail(format!("Nothing to answer: {why}."));
            return Vec::new();
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
            Vec::new()
        } else {
            self.ask_comment()
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

    /// Asks for what is told with the answer, unless nobody is: a meeting
    /// that asked for no answer gets none, so a comment would go nowhere.
    fn ask_comment(&mut self) -> Vec<Effect> {
        let tells = self
            .answering
            .as_ref()
            .is_some_and(|a| a.invitation.response_requested);
        if !tells {
            return self.send_answer(false);
        }
        self.ask(Question::InviteComment, "Comment (optional): ", "");
        Vec::new()
    }

    /// The user chose what the answer covers.
    pub(super) fn answer_scope(&mut self, key: char) -> Vec<Effect> {
        let Some(answering) = &mut self.answering else {
            return Vec::new();
        };
        answering.scope = if key == 'o' {
            Scope::Occurrence
        } else {
            Scope::Series
        };
        self.ask_comment()
    }

    /// The user typed the comment: now to whether the organizer is told.
    pub(super) fn answer_comment(&mut self, text: String) -> Vec<Effect> {
        let Some(answering) = &mut self.answering else {
            return Vec::new();
        };
        let text = text.trim();
        answering.comment = (!text.is_empty()).then(|| text.to_owned());
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
        let own = answering.invitation.answer_changes_reply(answering.scope);
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
mod tests;
