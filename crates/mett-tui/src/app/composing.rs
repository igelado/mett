//! Writing mail, as mutt does it: the questions asked on the message line
//! before the editor starts, the editor, then the compose menu, from which
//! the message is sent or postponed.

use std::collections::VecDeque;

use mett_core::{Address, Draft, DraftOrigin, Message, WellKnownFolder};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, Effect, Screen, effect_id};
use crate::action::Action;
use crate::compose::{self, Kind};
use crate::config::Quad;
use crate::lineedit::{LineEditor, Outcome};
use crate::worker::{Request, RequestId};

/// A question on the message line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// A line of text, such as the recipients.
    Line {
        /// The prompt, such as `To: `.
        prompt: String,
        /// The answer being typed.
        editor: LineEditor,
        /// What the answer is for.
        question: Question,
    },
    /// Yes or no.
    YesNo {
        /// The question, such as `Postpone this message?`.
        prompt: String,
        /// The answer Enter gives.
        default: bool,
        /// What the answer is for.
        question: Question,
    },
}

/// What a question on the message line is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Question {
    /// The `To:` recipients.
    To,
    /// The `Cc:` recipients.
    Cc,
    /// The `Bcc:` recipients.
    Bcc,
    /// The subject.
    Subject,
    /// Whether to reply to the `Reply-To:` address (`reply_to`).
    ReplyTo,
    /// Whether to quote the message in the reply (`include`).
    Include,
    /// Whether to give up on a message without a subject
    /// (`abort_nosubject`).
    AbortNoSubject,
    /// Whether to give up on a message that wasn't edited
    /// (`abort_unmodified`).
    AbortUnmodified,
    /// Whether to postpone the message when leaving the compose menu
    /// (`postpone`).
    Postpone,
    /// Whether to keep a copy of the sent message (`copy`).
    Copy,
}

/// A message being set up, before the compose menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Composing {
    kind: Kind,
    /// The message replied to or forwarded.
    original: Option<Message>,
    draft: Draft,
    use_reply_to: bool,
    include: bool,
    /// What is left to do before the compose menu.
    steps: VecDeque<Step>,
    /// The text first given to the editor, to tell whether the user
    /// changed it.
    template: Option<String>,
}

/// One step of setting up a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    ReplyTo,
    Include,
    /// Fill in a reply's recipients.
    Recipients,
    To,
    Cc,
    Bcc,
    Subject,
    Edit,
}

/// The compose menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compose {
    /// The message.
    pub draft: Draft,
    /// The request sending or postponing it, while it is pending.
    pub delivery: Option<(RequestId, Delivery)>,
    /// Sending or postponing it failed for want of a sign-in: the sign-in
    /// window's Retry tries again.
    pub(super) unauthorized: Option<Delivery>,
    /// It forwards a message with attachments, which go with it.
    pub forwarded_attachments: bool,
}

/// What happens to a finished message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// It is sent.
    Send,
    /// It is saved in the drafts folder.
    Postpone,
}

impl App {
    /// Starts writing a message of `kind`. Replies and forwards need the
    /// whole selected message, which is fetched first unless the pager
    /// already shows it.
    pub(super) fn start_compose(&mut self, kind: Kind) -> Vec<Effect> {
        if kind == Kind::New {
            return self.begin(kind, None);
        }
        if let Screen::Pager(pager) = self.screen()
            && let Some(message) = &pager.message
        {
            let message = message.clone();
            return self.begin(kind, Some(message));
        }
        let Some(summary) = self.messages.get(self.selected) else {
            self.status = "No messages.".into();
            return Vec::new();
        };
        let fetch = self.request(Request::FetchMessage(summary.id.clone()));
        self.compose_fetch = Some((effect_id(&fetch), kind));
        self.status = "Fetching message...".into();
        vec![fetch]
    }

    /// Continues a reply or forward once its message has been fetched.
    pub(super) fn on_compose_fetched(
        &mut self,
        kind: Kind,
        result: mett_core::Result<Message>,
    ) -> Vec<Effect> {
        match result {
            Ok(message) => self.begin(kind, Some(message)),
            Err(e) => {
                // Without a sign-in, the sign-in window's Retry starts the
                // reply or forward again.
                if matches!(e, mett_core::Error::Auth(_)) {
                    self.compose_unfetched = self
                        .messages
                        .get(self.selected)
                        .map(|m| (m.id.clone(), kind));
                }
                self.report(e);
                Vec::new()
            }
        }
    }

    /// Starts again a reply or forward whose message couldn't be fetched
    /// for want of a sign-in, if its message is still the one selected.
    pub(super) fn retry_compose_fetch(&mut self) -> Vec<Effect> {
        let Some((id, kind)) = self.compose_unfetched.take() else {
            return Vec::new();
        };
        let selected = self.messages.get(self.selected).map(|m| &m.id);
        if selected == Some(&id) && self.screen() == &Screen::Index {
            return self.start_compose(kind);
        }
        Vec::new()
    }

    fn begin(&mut self, kind: Kind, original: Option<Message>) -> Vec<Effect> {
        let settings = &self.settings;
        let mut draft = Draft {
            from: settings.from.clone(),
            ..Draft::default()
        };
        let mut steps = VecDeque::new();
        let mut ask_recipients = true;
        match (&kind, &original) {
            (Kind::Reply | Kind::GroupReply, Some(message)) => {
                draft.origin = Some(DraftOrigin::Reply(message.summary.id.clone()));
                draft.subject = compose::reply_subject(&message.summary.subject);
                steps.extend([Step::ReplyTo, Step::Include, Step::Recipients]);
                ask_recipients = !settings.fast_reply;
            }
            (Kind::Forward, Some(message)) => {
                draft.origin = Some(DraftOrigin::Forward(message.summary.id.clone()));
                draft.subject = compose::expand(&settings.forward_format, message);
            }
            _ => {}
        }
        if ask_recipients {
            steps.push_back(Step::To);
            if settings.askcc {
                steps.push_back(Step::Cc);
            }
            if settings.askbcc {
                steps.push_back(Step::Bcc);
            }
            steps.push_back(Step::Subject);
        }
        steps.push_back(Step::Edit);
        self.composing = Some(Composing {
            kind,
            original,
            draft,
            use_reply_to: true,
            include: true,
            steps,
            template: None,
        });
        self.advance()
    }

    /// Carries out the next steps of setting up the message, until one
    /// needs the user.
    fn advance(&mut self) -> Vec<Effect> {
        loop {
            let Some(composing) = &mut self.composing else {
                return Vec::new();
            };
            let Some(step) = composing.steps.pop_front() else {
                return Vec::new();
            };
            match step {
                Step::ReplyTo => {
                    let Some(original) = &composing.original else {
                        continue;
                    };
                    if !compose::has_distinct_reply_to(original) {
                        continue;
                    }
                    let prompt = format!("Reply to {}?", Address::format_list(&original.reply_to));
                    match self.decide(self.settings.reply_to, prompt, Question::ReplyTo) {
                        Some(answer) => self.composing_mut().use_reply_to = answer,
                        None => return Vec::new(),
                    }
                }
                Step::Include => {
                    let prompt = "Include message in reply?".to_owned();
                    match self.decide(self.settings.include, prompt, Question::Include) {
                        Some(answer) => self.composing_mut().include = answer,
                        None => return Vec::new(),
                    }
                }
                Step::Recipients => {
                    if let Some(original) = &composing.original {
                        let group = composing.kind == Kind::GroupReply;
                        let (to, cc) = compose::reply_recipients(
                            original,
                            composing.use_reply_to,
                            group,
                            &self.settings,
                        );
                        composing.draft.to = to;
                        composing.draft.cc = cc;
                    }
                }
                // Once the address questions are done, give up on a
                // message without recipients, as mutt does.
                Step::Subject | Step::Edit if !composing.draft.has_recipients() => {
                    return self.abort("No recipients were specified.");
                }
                Step::To | Step::Cc | Step::Bcc | Step::Subject => {
                    let question = match step {
                        Step::To => Question::To,
                        Step::Cc => Question::Cc,
                        Step::Bcc => Question::Bcc,
                        _ => Question::Subject,
                    };
                    let current = field(&composing.draft, question);
                    self.ask_line(question, &current);
                    return Vec::new();
                }
                Step::Edit => {
                    let mut body = match (&composing.kind, &composing.original) {
                        (Kind::Reply | Kind::GroupReply, Some(original)) if composing.include => {
                            compose::quoted(original, &self.settings)
                        }
                        (Kind::Forward, Some(original)) => compose::forwarded(original),
                        _ => String::new(),
                    };
                    body = compose::signed(body, &self.settings);
                    composing.template = Some(body.clone());
                    return vec![Effect::Edit(body)];
                }
            }
        }
    }

    fn composing_mut(&mut self) -> &mut Composing {
        self.composing.as_mut().expect("a message is being set up")
    }

    /// Settles a quad-option: its answer, or `None` if the user is asked.
    fn decide(&mut self, quad: Quad, prompt: String, question: Question) -> Option<bool> {
        match quad {
            Quad::Yes => Some(true),
            Quad::No => Some(false),
            Quad::AskYes | Quad::AskNo => {
                self.input = Some(Input::YesNo {
                    prompt,
                    default: quad == Quad::AskYes,
                    question,
                });
                None
            }
        }
    }

    fn ask_line(&mut self, question: Question, current: &str) {
        let prompt = match question {
            Question::To => "To: ",
            Question::Cc => "Cc: ",
            Question::Bcc => "Bcc: ",
            _ => "Subject: ",
        };
        self.input = Some(Input::Line {
            prompt: prompt.to_owned(),
            editor: LineEditor::new(current),
            question,
        });
    }

    /// Handles a key press while a question is on the message line.
    pub(super) fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(input) = &mut self.input else {
            return Vec::new();
        };
        let cancel = key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL));
        match input {
            Input::Line {
                editor, question, ..
            } => {
                let question = *question;
                match editor.key(key) {
                    Outcome::Editing => Vec::new(),
                    Outcome::Done(text) => {
                        self.input = None;
                        self.answer_line(question, text)
                    }
                    Outcome::Cancelled => {
                        self.input = None;
                        self.cancel_question()
                    }
                }
            }
            Input::YesNo {
                default, question, ..
            } => {
                let question = *question;
                let answer = match key.code {
                    _ if cancel => {
                        self.input = None;
                        return self.cancel_question();
                    }
                    KeyCode::Enter => *default,
                    KeyCode::Char('y' | 'Y') => true,
                    KeyCode::Char('n' | 'N') => false,
                    _ => return Vec::new(),
                };
                self.input = None;
                self.answer(question, answer)
            }
        }
    }

    /// The user gave up on a question: give up on the message being set
    /// up, or leave the compose menu as it was.
    fn cancel_question(&mut self) -> Vec<Effect> {
        self.composing = None;
        Vec::new()
    }

    fn answer_line(&mut self, question: Question, text: String) -> Vec<Effect> {
        let setting_up = self.composing.is_some();
        let Some(draft) = self.draft_mut() else {
            return Vec::new();
        };
        match question {
            Question::To | Question::Cc | Question::Bcc => {
                let list = match Address::parse_list(&text) {
                    Ok(list) => list,
                    Err(e) => {
                        // Ask again, keeping what was typed.
                        // Ask again, keeping what was typed, and say why on
                        // the line itself, where the question hides the
                        // status.
                        self.ask_line(question, &text);
                        if let Some(Input::Line { prompt, .. }) = &mut self.input {
                            *prompt = format!("{e}. {prompt}");
                        }
                        return Vec::new();
                    }
                };
                match question {
                    Question::To => draft.to = list,
                    Question::Cc => draft.cc = list,
                    _ => draft.bcc = list,
                }
            }
            _ => {
                draft.subject = text;
                if setting_up && draft.subject.is_empty() {
                    let prompt = "No subject, abort?".to_owned();
                    let quad = self.settings.abort_nosubject;
                    match self.decide(quad, prompt, Question::AbortNoSubject) {
                        Some(true) => return self.abort("No subject, aborting."),
                        Some(false) => {}
                        None => return Vec::new(),
                    }
                }
            }
        }
        self.advance()
    }

    fn answer(&mut self, question: Question, answer: bool) -> Vec<Effect> {
        match question {
            Question::ReplyTo => self.composing_mut().use_reply_to = answer,
            Question::Include => self.composing_mut().include = answer,
            Question::AbortNoSubject if answer => return self.abort("No subject, aborting."),
            Question::AbortUnmodified if answer => {
                return self.abort("Aborted unmodified message.");
            }
            Question::AbortUnmodified => return self.open_compose_menu(),
            Question::Postpone if answer => return self.deliver(Delivery::Postpone),
            Question::Postpone => {
                self.screens.pop();
                self.status = "Mail not sent.".into();
                return Vec::new();
            }
            Question::Copy => {
                if let Some(draft) = self.draft_mut() {
                    draft.save_copy = answer;
                }
                return self.deliver(Delivery::Send);
            }
            Question::AbortNoSubject
            | Question::To
            | Question::Cc
            | Question::Bcc
            | Question::Subject => {}
        }
        self.advance()
    }

    /// Gives up on the message being set up.
    fn abort(&mut self, why: &str) -> Vec<Effect> {
        self.composing = None;
        self.status = why.into();
        Vec::new()
    }

    /// The message being set up, or the one in the compose menu.
    fn draft_mut(&mut self) -> Option<&mut Draft> {
        if let Some(composing) = &mut self.composing {
            return Some(&mut composing.draft);
        }
        match self.screens.last_mut() {
            Some(Screen::Compose(compose)) => Some(&mut compose.draft),
            _ => None,
        }
    }

    /// Takes the text back from the editor.
    pub(super) fn on_edited(&mut self, result: Result<String, String>) -> Vec<Effect> {
        let text = match result {
            Ok(text) => text,
            Err(e) => {
                // The message being set up is lost without its text; one
                // in the compose menu keeps the text it had.
                self.composing = None;
                self.status = e;
                return Vec::new();
            }
        };
        let Some(composing) = &mut self.composing else {
            if let Some(draft) = self.draft_mut() {
                draft.body = text;
            }
            return Vec::new();
        };
        let unmodified = composing.template.as_ref() == Some(&text);
        composing.draft.body = text;
        if unmodified {
            let prompt = "Abort unmodified message?".to_owned();
            let quad = self.settings.abort_unmodified;
            match self.decide(quad, prompt, Question::AbortUnmodified) {
                Some(true) => return self.abort("Aborted unmodified message."),
                Some(false) => {}
                None => return Vec::new(),
            }
        }
        self.open_compose_menu()
    }

    fn open_compose_menu(&mut self) -> Vec<Effect> {
        if let Some(composing) = self.composing.take() {
            let forwarded_attachments = composing.kind == Kind::Forward
                && composing
                    .original
                    .as_ref()
                    .is_some_and(|m| m.summary.has_attachments);
            self.screens.push(Screen::Compose(Compose {
                draft: composing.draft,
                delivery: None,
                unauthorized: None,
                forwarded_attachments,
            }));
        }
        Vec::new()
    }

    pub(super) fn on_compose_action(&mut self, action: Action) -> Vec<Effect> {
        let Some(Screen::Compose(compose)) = self.screens.last() else {
            return Vec::new();
        };
        if compose.delivery.is_some() {
            self.status = "Please wait: the message is still being sent or postponed.".into();
            return Vec::new();
        }
        let draft = compose.draft.clone();
        match action {
            Action::SendMessage => {
                if !draft.has_recipients() {
                    self.status = "No recipients are specified!".into();
                    return Vec::new();
                }
                // Graph always keeps a copy of replies and forwards, so
                // `copy` only applies to new mail.
                if draft.origin.is_some() {
                    return self.answer(Question::Copy, true);
                }
                let prompt = "Save a copy of this message?".to_owned();
                match self.decide(self.settings.copy, prompt, Question::Copy) {
                    Some(copy) => return self.answer(Question::Copy, copy),
                    None => return Vec::new(),
                }
            }
            Action::Exit => {
                let prompt = "Postpone this message?".to_owned();
                if let Some(postpone) =
                    self.decide(self.settings.postpone, prompt, Question::Postpone)
                {
                    return self.answer(Question::Postpone, postpone);
                }
            }
            Action::PostponeMessage => return self.deliver(Delivery::Postpone),
            Action::EditMessage => return vec![Effect::Edit(draft.body)],
            Action::EditTo | Action::EditCc | Action::EditBcc | Action::EditSubject => {
                let question = match action {
                    Action::EditTo => Question::To,
                    Action::EditCc => Question::Cc,
                    Action::EditBcc => Question::Bcc,
                    _ => Question::Subject,
                };
                let current = field(&draft, question);
                self.ask_line(question, &current);
            }
            _ => {}
        }
        Vec::new()
    }

    /// Sends or postpones the message in the compose menu.
    fn deliver(&mut self, delivery: Delivery) -> Vec<Effect> {
        let Some(Screen::Compose(compose)) = self.screens.last() else {
            return Vec::new();
        };
        let draft = compose.draft.clone();
        let (request, status) = match delivery {
            Delivery::Send => (Request::Send(draft), "Sending message..."),
            Delivery::Postpone => (Request::SaveDraft(draft), "Postponing message..."),
        };
        let effect = self.request(request);
        if let Some(Screen::Compose(compose)) = self.screens.last_mut() {
            compose.delivery = Some((effect_id(&effect), delivery));
        }
        self.status = status.into();
        vec![effect]
    }

    /// Handles the outcome of sending or postponing a message.
    pub(super) fn on_delivered(
        &mut self,
        id: RequestId,
        result: mett_core::Result<()>,
    ) -> Vec<Effect> {
        let Some(position) = self.screens.iter().position(|screen| {
            matches!(screen, Screen::Compose(Compose { delivery: Some((request, _)), .. }) if *request == id)
        }) else {
            return Vec::new();
        };
        let Screen::Compose(compose) = &mut self.screens[position] else {
            return Vec::new();
        };
        let delivery = compose.delivery.take().map(|(_, d)| d);
        compose.unauthorized = None;
        let saved_copy = compose.draft.save_copy;
        match result {
            Ok(()) => {
                self.screens.remove(position);
                self.status = match delivery {
                    Some(Delivery::Postpone) => "Message postponed.",
                    _ => "Mail sent.",
                }
                .into();
                // The message landed in the open folder: list it again.
                // (A sent message lands in Sent Items only if a copy was
                // kept.)
                let landed_in = match delivery {
                    Some(Delivery::Postpone) => Some(WellKnownFolder::Drafts),
                    _ if saved_copy => Some(WellKnownFolder::SentItems),
                    _ => None,
                };
                let open = self.folder.as_ref().and_then(|f| f.well_known);
                if landed_in.is_some() && open == landed_in {
                    // Not under a pager, whose message and position would
                    // no longer match the index: once back in the index.
                    if self.screen() == &Screen::Index {
                        return self.relist();
                    }
                    self.relist_on_return = true;
                }
            }
            Err(e) => {
                if matches!(e, mett_core::Error::Auth(_)) {
                    compose.unauthorized = delivery;
                }
                // A lost response doesn't mean nothing happened.
                self.status = match (delivery, &e) {
                    (Some(Delivery::Send), mett_core::Error::Network(_)) if saved_copy => format!(
                        "The message may have been sent ({e}). Check Sent Items before sending it again."
                    ),
                    // Without a copy, Sent Items can't tell.
                    (Some(Delivery::Send), mett_core::Error::Network(_)) => format!(
                        "The message may have been sent ({e}), without a copy in Sent Items. \
                         Check with its recipients before sending it again."
                    ),
                    (Some(Delivery::Postpone), mett_core::Error::Network(_)) => format!(
                        "The message may have been saved ({e}). Check Drafts before postponing it again."
                    ),
                    _ => e.to_string(),
                };
            }
        }
        Vec::new()
    }

    /// Sends or postpones again the message whose delivery failed for want
    /// of a sign-in.
    pub(super) fn retry_delivery(&mut self) -> Vec<Effect> {
        let Some(Screen::Compose(compose)) = self.screens.last_mut() else {
            return Vec::new();
        };
        match compose.unauthorized.take() {
            Some(delivery) => self.deliver(delivery),
            None => Vec::new(),
        }
    }

    /// The request sending or postponing a message, while it is pending.
    pub(super) fn delivery_request(&self) -> Option<RequestId> {
        self.screens.iter().find_map(|s| match s {
            Screen::Compose(Compose {
                delivery: Some((id, _)),
                ..
            }) => Some(*id),
            _ => None,
        })
    }

    /// Whether a message is being sent or postponed.
    pub(super) fn delivering(&self) -> bool {
        self.screens.iter().any(|s| {
            matches!(
                s,
                Screen::Compose(Compose {
                    delivery: Some(_),
                    ..
                })
            )
        })
    }
}

/// A draft's field, as the user edits it.
fn field(draft: &Draft, question: Question) -> String {
    match question {
        Question::To => Address::format_list(&draft.to),
        Question::Cc => Address::format_list(&draft.cc),
        Question::Bcc => Address::format_list(&draft.bcc),
        _ => draft.subject.clone(),
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use mett_core::{Error, MessagePage, mock};

    use super::*;
    use crate::app::{Msg, Settings};
    use crate::keymap::Menu;
    use crate::worker::{Event, Response};

    /// An app with one message, from Bob, in its index.
    fn app(compose: compose::Settings) -> App {
        let inbox = crate::config::mailbox::FolderPath::Named(vec!["Inbox".into()]);
        let settings = Settings {
            compose,
            ..Settings::default()
        };
        let (mut app, effects) = App::open(80, 24, inbox, settings);
        let Effect::Request(open, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let effects = app.update(Msg::Response(Response {
            id: open,
            event: Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
        }));
        let Effect::Request(list, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        app.update(Msg::Response(Response {
            id: list,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: vec![original().summary],
                next: None,
            })),
        }));
        app
    }

    fn original() -> Message {
        let mut message = mock::message("m1", "in", "Lunch", DateTime::UNIX_EPOCH);
        message.summary.from = Some(Address::new("bob@x.org").with_name("Bob"));
        message.summary.to = vec![Address::new("me@contoso.com")];
        message.body.content = "Noon?".into();
        message
    }

    fn settings() -> compose::Settings {
        compose::Settings {
            from: Some(Address::new("me@contoso.com").with_name("Me")),
            me: vec!["me@contoso.com".into()],
            signature: Some("Me".into()),
            attribution: "%n wrote:".into(),
            ..compose::Settings::default()
        }
    }

    fn act(app: &mut App, action: Action) -> Vec<Effect> {
        app.update(Msg::Action(action))
    }

    fn key(app: &mut App, code: KeyCode) -> Vec<Effect> {
        app.update(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    /// Types `text` and presses Enter.
    fn answer(app: &mut App, text: &str) -> Vec<Effect> {
        for c in text.chars() {
            key(app, KeyCode::Char(c));
        }
        key(app, KeyCode::Enter)
    }

    fn prompt(app: &App) -> String {
        match &app.input {
            Some(Input::Line { prompt, editor, .. }) => format!("{prompt}{}", editor.text()),
            Some(Input::YesNo { prompt, .. }) => prompt.clone(),
            None => String::new(),
        }
    }

    fn compose(app: &App) -> &Compose {
        match app.screen() {
            Screen::Compose(compose) => compose,
            other => panic!("not in the compose menu: {other:?}"),
        }
    }

    /// The single request among `effects`.
    fn request(effects: &[Effect]) -> (RequestId, Request) {
        match effects {
            [Effect::Request(id, request)] => (*id, request.clone()),
            _ => panic!("expected one request, got {effects:?}"),
        }
    }

    #[test]
    fn writes_and_sends_new_mail() {
        let mut app = app(settings());
        act(&mut app, Action::Mail);
        assert_eq!(prompt(&app), "To: ");
        answer(&mut app, "Alice <alice@x.org>, carol@x.org");
        assert_eq!(prompt(&app), "Subject: ");
        let effects = answer(&mut app, "Hello");
        assert_eq!(effects, [Effect::Edit("\n-- \nMe\n".into())]);
        assert_eq!(app.input, None);

        app.update(Msg::Edited(Ok("Hi!\n\n-- \nMe\n".into())));
        assert_eq!(app.menu(), Menu::Compose);
        let draft = &compose(&app).draft;
        assert_eq!(draft.to.len(), 2);
        assert_eq!(draft.subject, "Hello");

        let (id, request) = request(&act(&mut app, Action::SendMessage));
        let Request::Send(sent) = request else {
            panic!("expected Send, got {request:?}");
        };
        assert_eq!(sent.body, "Hi!\n\n-- \nMe\n");
        assert_eq!(
            sent.from,
            Some(Address::new("me@contoso.com").with_name("Me"))
        );
        assert!(sent.save_copy);
        assert_eq!(sent.origin, None);
        assert!(app.saving(), "waits for the message to be sent");
        // Nothing else can be done meanwhile.
        assert!(act(&mut app, Action::SendMessage).is_empty());

        app.update(Msg::Response(Response {
            id,
            event: Event::Sent(Ok(())),
        }));
        assert_eq!(app.screen(), &Screen::Index);
        assert_eq!(app.status, "Mail sent.");
        assert!(!app.saving());
    }

    #[test]
    fn a_failed_send_stays_in_the_compose_menu() {
        let mut app = app(settings());
        act(&mut app, Action::Mail);
        answer(&mut app, "bob@x.org");
        answer(&mut app, "Hi");
        app.update(Msg::Edited(Ok("text".into())));
        let (id, _) = request(&act(&mut app, Action::SendMessage));
        app.update(Msg::Response(Response {
            id,
            event: Event::Sent(Err(Error::Backend("bad recipient".into()))),
        }));
        assert_eq!(compose(&app).delivery, None);
        assert_eq!(app.status, "backend error: bad recipient");
    }

    #[test]
    fn replies_quote_the_message_shown_in_the_pager() {
        let mut app = app(settings());
        let (fetch, _) = request(&act(&mut app, Action::DisplayMessage));
        app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Ok(original())),
        }));
        // The pager has the message: no need to fetch it again.
        act(&mut app, Action::Reply);
        assert_eq!(prompt(&app), "Include message in reply?");
        key(&mut app, KeyCode::Enter);
        assert_eq!(prompt(&app), "To: Bob <bob@x.org>");
        key(&mut app, KeyCode::Enter);
        assert_eq!(prompt(&app), "Subject: Re: Lunch");
        let effects = key(&mut app, KeyCode::Enter);
        assert_eq!(
            effects,
            [Effect::Edit("Bob wrote:\n> Noon?\n\n-- \nMe\n".into())]
        );
        app.update(Msg::Edited(Ok("Sure.\n".into())));
        let draft = &compose(&app).draft;
        assert_eq!(draft.origin, Some(DraftOrigin::Reply("m1".into())));

        // Replies are always copied to Sent Items: no question about it.
        app.settings.copy = Quad::AskNo;
        let effects = act(&mut app, Action::SendMessage);
        assert!(matches!(request(&effects).1, Request::Send(d) if d.save_copy));
        app.update(Msg::Response(Response {
            id: request(&effects).0,
            event: Event::Sent(Err(Error::Network("offline".into()))),
        }));

        let (id, request) = request(&act(&mut app, Action::PostponeMessage));
        assert!(matches!(request, Request::SaveDraft(d) if d.body == "Sure.\n"));
        app.update(Msg::Response(Response {
            id,
            event: Event::DraftSaved(Ok("d1".into())),
        }));
        assert_eq!(app.status, "Message postponed.");
        assert!(matches!(app.screen(), Screen::Pager(_)));
    }

    #[test]
    fn replies_from_the_index_fetch_the_message_first() {
        let mut app = app(compose::Settings {
            include: Quad::No,
            fast_reply: true,
            ..settings()
        });
        let (fetch, _) = request(&act(&mut app, Action::GroupReply));
        let effects = app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Ok(original())),
        }));
        // fast_reply: straight to the editor, without quoting.
        assert_eq!(effects, [Effect::Edit("\n-- \nMe\n".into())]);
        assert_eq!(app.screen(), &Screen::Index, "the pager isn't opened");
    }

    #[test]
    fn a_late_fetch_does_not_replace_a_newer_message() {
        let mut app = app(settings());
        let (fetch, _) = request(&act(&mut app, Action::Reply));
        act(&mut app, Action::Mail);
        let effects = app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Ok(original())),
        }));
        assert!(effects.is_empty());
        assert_eq!(prompt(&app), "To: ");
    }

    #[test]
    fn moving_on_forgets_a_reply_still_being_fetched() {
        let mut app = app(settings());
        let (fetch, _) = request(&act(&mut app, Action::Reply));
        act(&mut app, Action::NextEntry);
        let effects = app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Ok(original())),
        }));
        assert!(effects.is_empty());
        assert!(app.input.is_none(), "no reply was started");
    }

    #[test]
    fn asks_whether_to_reply_to_the_reply_to_address() {
        let mut app = app(settings());
        let mut message = original();
        message.reply_to = vec![Address::new("list@x.org")];
        app.begin(Kind::Reply, Some(message));
        assert_eq!(prompt(&app), "Reply to list@x.org?");
        key(&mut app, KeyCode::Char('n'));
        key(&mut app, KeyCode::Char('n'));
        assert_eq!(prompt(&app), "To: Bob <bob@x.org>");
    }

    #[test]
    fn forwards_inline_with_mutts_subject() {
        let mut app = app(settings());
        app.begin(Kind::Forward, Some(original()));
        answer(&mut app, "carol@x.org");
        assert_eq!(prompt(&app), "Subject: [bob@x.org: Lunch]");
        let effects = key(&mut app, KeyCode::Enter);
        let [Effect::Edit(text)] = effects.as_slice() else {
            panic!("expected the editor, got {effects:?}");
        };
        assert!(text.starts_with("----- Forwarded message from Bob <bob@x.org> -----"));
        app.update(Msg::Edited(Ok(text.clone())));
        // Unchanged, but abort_unmodified gives up on it.
        assert_eq!(app.status, "Aborted unmodified message.");
        assert_eq!(app.screen(), &Screen::Index);
    }

    #[test]
    fn bad_addresses_are_asked_again() {
        let mut app = app(settings());
        act(&mut app, Action::Mail);
        answer(&mut app, "bob");
        assert_eq!(prompt(&app), "bad address \"bob\". To: bob");
    }

    #[test]
    fn giving_up_on_a_question_abandons_the_message() {
        let mut app = app(settings());
        act(&mut app, Action::Mail);
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.input, None);
        assert_eq!(app.screen(), &Screen::Index);
        // No recipients at all gives up too, as in mutt.
        act(&mut app, Action::Mail);
        answer(&mut app, "");
        assert_eq!(app.status, "No recipients were specified.");
        assert_eq!(app.input, None);
        // But a Cc is enough.
        app.settings.askcc = true;
        act(&mut app, Action::Mail);
        answer(&mut app, "");
        assert_eq!(prompt(&app), "Cc: ");
        answer(&mut app, "carol@x.org");
        assert_eq!(prompt(&app), "Subject: ");
    }

    #[test]
    fn an_empty_subject_asks_whether_to_abort() {
        let mut app = app(settings());
        act(&mut app, Action::Mail);
        answer(&mut app, "bob@x.org");
        answer(&mut app, "");
        assert_eq!(prompt(&app), "No subject, abort?");
        key(&mut app, KeyCode::Char('n'));
        assert!(app.input.is_none());
    }

    #[test]
    fn leaving_the_compose_menu_asks_whether_to_postpone() {
        let mut app = app(settings());
        act(&mut app, Action::Mail);
        answer(&mut app, "bob@x.org");
        answer(&mut app, "Hi");
        app.update(Msg::Edited(Ok("text".into())));
        act(&mut app, Action::EditSubject);
        key(&mut app, KeyCode::Char('!'));
        key(&mut app, KeyCode::Enter);
        assert_eq!(compose(&app).draft.subject, "Hi!");

        act(&mut app, Action::Exit);
        assert_eq!(prompt(&app), "Postpone this message?");
        key(&mut app, KeyCode::Char('n'));
        assert_eq!(app.status, "Mail not sent.");
        assert_eq!(app.screen(), &Screen::Index);
    }

    #[test]
    fn editing_again_keeps_the_text_if_the_editor_fails() {
        let mut app = app(settings());
        act(&mut app, Action::Mail);
        answer(&mut app, "bob@x.org");
        answer(&mut app, "Hi");
        app.update(Msg::Edited(Ok("first".into())));
        assert_eq!(
            act(&mut app, Action::EditMessage),
            [Effect::Edit("first".into())]
        );
        app.update(Msg::Edited(Err("Error running \"vi\"".into())));
        assert_eq!(compose(&app).draft.body, "first");
        act(&mut app, Action::EditMessage);
        app.update(Msg::Edited(Ok("second".into())));
        assert_eq!(compose(&app).draft.body, "second");
    }

    #[test]
    fn a_pager_still_loading_under_the_compose_menu_finishes() {
        let mut app = app(compose::Settings {
            fast_reply: true,
            include: Quad::No,
            ..settings()
        });
        let (fetch, _) = request(&act(&mut app, Action::DisplayMessage));
        // Reply before the pager's message arrives: it is fetched again.
        let (reply_fetch, _) = request(&act(&mut app, Action::Reply));
        app.update(Msg::Response(Response {
            id: reply_fetch,
            event: Event::MessageFetched(Ok(original())),
        }));
        app.update(Msg::Edited(Ok("text".into())));
        assert_eq!(app.menu(), Menu::Compose);
        app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Ok(original())),
        }));
        let Screen::Pager(pager) = &app.screens[1] else {
            panic!("the pager is under the compose menu");
        };
        assert!(pager.message.is_some(), "and it has its message");
    }

    #[test]
    fn sending_from_sent_items_lists_it_again() {
        let mut app = app(settings());
        app.folder = Some(mock::folder(
            "sent",
            "Sent Items",
            Some(WellKnownFolder::SentItems),
        ));
        act(&mut app, Action::Mail);
        answer(&mut app, "bob@x.org");
        answer(&mut app, "Hi");
        app.update(Msg::Edited(Ok("text".into())));
        let (id, _) = request(&act(&mut app, Action::SendMessage));
        let effects = app.update(Msg::Response(Response {
            id,
            event: Event::Sent(Ok(())),
        }));
        assert!(
            matches!(request(&effects).1, Request::ListMessages { .. }),
            "{effects:?}"
        );
        assert!(app.messages.is_empty());
    }

    #[test]
    fn a_send_that_needed_a_sign_in_is_retried() {
        let mut app = app(settings());
        act(&mut app, Action::Mail);
        answer(&mut app, "bob@x.org");
        answer(&mut app, "Hi");
        app.update(Msg::Edited(Ok("text".into())));
        let (id, sent) = request(&act(&mut app, Action::SendMessage));
        app.update(Msg::SignIn(crate::signin::SignInEvent::Finished {
            account: mett_core::AccountId::new("me@contoso.com"),
            outcome: Err("expired".into()),
        }));
        app.update(Msg::Response(Response {
            id,
            event: Event::Sent(Err(Error::Auth("expired".into()))),
        }));
        let (_, again) = request(&act(&mut app, Action::Retry));
        assert_eq!(again, sent);
    }

    #[test]
    fn sent_items_is_listed_again_once_the_pager_closes() {
        let mut app = app(settings());
        app.folder = Some(mock::folder(
            "sent",
            "Sent Items",
            Some(WellKnownFolder::SentItems),
        ));
        let (fetch, _) = request(&act(&mut app, Action::DisplayMessage));
        app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Ok(original())),
        }));
        app.settings.include = Quad::No;
        app.settings.fast_reply = true;
        act(&mut app, Action::Reply);
        app.update(Msg::Edited(Ok("text".into())));
        let (id, _) = request(&act(&mut app, Action::SendMessage));
        let effects = app.update(Msg::Response(Response {
            id,
            event: Event::Sent(Ok(())),
        }));
        assert!(effects.is_empty(), "not under the pager");
        assert_eq!(app.messages.len(), 1);
        let effects = act(&mut app, Action::Exit);
        assert!(matches!(request(&effects).1, Request::ListMessages { .. }));
    }

    #[test]
    fn a_reply_whose_message_needed_a_sign_in_starts_again_on_retry() {
        let mut app = app(settings());
        let (fetch, _) = request(&act(&mut app, Action::Reply));
        app.update(Msg::SignIn(crate::signin::SignInEvent::Finished {
            account: mett_core::AccountId::new("me@contoso.com"),
            outcome: Err("expired".into()),
        }));
        app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Err(Error::Auth("expired".into()))),
        }));
        let (_, again) = request(&act(&mut app, Action::Retry));
        assert_eq!(again, Request::FetchMessage("m1".into()));
    }

    #[test]
    fn a_lost_send_response_may_have_sent_the_message() {
        let mut app = app(settings());
        act(&mut app, Action::Mail);
        answer(&mut app, "bob@x.org");
        answer(&mut app, "Hi");
        app.update(Msg::Edited(Ok("text".into())));
        let (id, _) = request(&act(&mut app, Action::SendMessage));
        app.update(Msg::Response(Response {
            id,
            event: Event::Sent(Err(Error::Network("timed out".into()))),
        }));
        assert!(
            app.status.starts_with("The message may have been sent"),
            "{}",
            app.status
        );
    }

    #[test]
    fn sending_without_a_copy_leaves_sent_items_alone() {
        let mut app = app(compose::Settings {
            copy: Quad::No,
            ..settings()
        });
        app.folder = Some(mock::folder(
            "sent",
            "Sent Items",
            Some(WellKnownFolder::SentItems),
        ));
        act(&mut app, Action::Mail);
        answer(&mut app, "bob@x.org");
        answer(&mut app, "Hi");
        app.update(Msg::Edited(Ok("text".into())));
        let (id, _) = request(&act(&mut app, Action::SendMessage));
        let effects = app.update(Msg::Response(Response {
            id,
            event: Event::Sent(Ok(())),
        }));
        assert!(effects.is_empty());
        assert_eq!(app.messages.len(), 1);
    }

    #[test]
    fn retry_resends_a_delivery_and_failed_saves_together() {
        let mut app = app(settings());
        // Reading the message fails to mark it read…
        let (fetch, _) = request(&act(&mut app, Action::DisplayMessage));
        let effects = app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Ok(original())),
        }));
        let (flags, _) = request(&effects);
        app.update(Msg::Response(Response {
            id: flags,
            event: Event::FlagsUpdated(Err(Error::Auth("expired".into()))),
        }));
        // …and replying fails to send, for the same reason.
        app.settings.include = Quad::No;
        app.settings.fast_reply = true;
        act(&mut app, Action::Reply);
        app.update(Msg::Edited(Ok("text".into())));
        let (send, _) = request(&act(&mut app, Action::SendMessage));
        app.update(Msg::SignIn(crate::signin::SignInEvent::Finished {
            account: mett_core::AccountId::new("me@contoso.com"),
            outcome: Err("expired".into()),
        }));
        app.update(Msg::Response(Response {
            id: send,
            event: Event::Sent(Err(Error::Auth("expired".into()))),
        }));
        let effects = act(&mut app, Action::Retry);
        let requests: Vec<_> = effects
            .iter()
            .map(|e| match e {
                Effect::Request(_, Request::Send(_)) => "send",
                Effect::Request(_, Request::UpdateFlags(..)) => "flags",
                _ => "other",
            })
            .collect();
        assert_eq!(requests, ["send", "flags"]);
    }

    #[test]
    fn a_lost_postpone_response_may_have_saved_the_draft() {
        let mut app = app(settings());
        act(&mut app, Action::Mail);
        answer(&mut app, "bob@x.org");
        answer(&mut app, "Hi");
        app.update(Msg::Edited(Ok("text".into())));
        let (id, _) = request(&act(&mut app, Action::PostponeMessage));
        app.update(Msg::Response(Response {
            id,
            event: Event::DraftSaved(Err(Error::Network("timed out".into()))),
        }));
        assert!(
            app.status.starts_with("The message may have been saved"),
            "{}",
            app.status
        );
    }

    #[test]
    fn a_lost_send_without_a_copy_says_sent_items_cannot_tell() {
        let mut app = app(compose::Settings {
            copy: Quad::No,
            ..settings()
        });
        act(&mut app, Action::Mail);
        answer(&mut app, "bob@x.org");
        answer(&mut app, "Hi");
        app.update(Msg::Edited(Ok("text".into())));
        let (id, _) = request(&act(&mut app, Action::SendMessage));
        app.update(Msg::Response(Response {
            id,
            event: Event::Sent(Err(Error::Network("timed out".into()))),
        }));
        assert!(app.status.contains("without a copy"), "{}", app.status);
    }
}
