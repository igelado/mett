//! Writing mail, as mutt does it: the questions asked on the message line
//! before the editor starts, the editor, then the compose menu, from which
//! the message is sent or postponed.

use std::collections::VecDeque;

use mett_core::{Address, Draft, DraftOrigin, Message, WellKnownFolder};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::mailbox::Choose;
use super::{App, Effect, Screen, effect_id};
use crate::action::Action;
use crate::alias;
use crate::compose::{self, Kind};
use crate::config::Quad;
use crate::lineedit::{LineEditor, Outcome};
use crate::worker::{Request, RequestId};

/// How long after sending Sent Items is listed again, to catch a copy
/// that Graph filed late.
const SENT_COPY_DELAY: std::time::Duration = std::time::Duration::from_secs(5);

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
    /// Whether to go back to the compose menu rather than send a message
    /// without a subject (`abort_nosubject`, asked again when sending).
    AbortSendNoSubject,
    /// Whether to give up on a message that wasn't edited
    /// (`abort_unmodified`).
    AbortUnmodified,
    /// Whether to postpone the message when leaving the compose menu
    /// (`postpone`).
    Postpone,
    /// Whether to keep a copy of the sent message (`copy`).
    Copy,
    /// The folder to open (`c`).
    OpenMailbox,
    /// The folder to move the message to (`s`).
    SaveTo,
    /// The folder to copy the message to (`C`).
    CopyTo,
    /// Whether to purge deleted messages (`delete`).
    Purge,
    /// What to search for (`/`).
    Search,
    /// What to limit the index to (`l`).
    Limit,
    /// A mettrc command (`:`).
    Command,
    /// A file to attach (`a` in the compose menu).
    AttachFile,
    /// The file to save an attachment to (`s` in the attachment menu).
    SaveAttachment,
    /// Whether to replace a file that is there.
    Overwrite,
    /// What to query for (`Q`).
    Query,
    /// The name of an alias being made (`a`).
    AliasName,
    /// The address of an alias being made.
    AliasAddress,
}

impl Question {
    /// Whether the answer is a mailbox, so that the folder browser can
    /// help.
    fn is_mailbox(self) -> bool {
        matches!(
            self,
            Question::OpenMailbox | Question::SaveTo | Question::CopyTo
        )
    }
}

/// A message being set up, before the compose menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Composing {
    kind: Kind,
    /// The message replied to or forwarded.
    original: Option<Message>,
    pub(super) draft: Draft,
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
    /// A failed attempt to send or postpone it may have left a server
    /// draft behind, to delete if the user gives up on it.
    pub(super) left_draft: bool,
    /// The backend kept a draft from the last attempt, which the next one
    /// goes on from.
    pub(super) kept_draft: bool,
    /// The selected part: its text, the forwarded attachments if there
    /// are any, then the attached files.
    pub selected: usize,
}

impl Compose {
    /// The compose menu for `draft`, which carries a forwarded message's
    /// attachments if `forwarded_attachments`.
    pub fn new(draft: Draft, forwarded_attachments: bool) -> Self {
        Self {
            draft,
            delivery: None,
            unauthorized: None,
            forwarded_attachments,
            left_draft: false,
            kept_draft: false,
            selected: 0,
        }
    }

    /// How many parts the message has.
    pub fn parts(&self) -> usize {
        1 + usize::from(self.forwarded_attachments) + self.draft.attachments.len()
    }
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
            return self.reply_when_ready(kind, message);
        }
        let Some(summary) = self.messages.get(self.selected) else {
            self.fail("No messages.");
            return Vec::new();
        };
        let id = summary.id.clone();
        let fetch = self.request(Request::FetchMessage(id.clone()));
        self.compose_fetch = Some((effect_id(&fetch), kind, id));
        self.status = "Fetching message...".into();
        vec![fetch]
    }

    /// Starts a reply or forward to `message`. Whom a reply goes to depends
    /// on which addresses are the user's: it waits for the mailbox to tell
    /// (asking again if it couldn't before).
    fn reply_when_ready(&mut self, kind: Kind, message: Message) -> Vec<Effect> {
        if !matches!(kind, Kind::Reply | Kind::GroupReply) {
            return self.begin(kind, Some(message));
        }
        let mut effects = Vec::new();
        if self.addresses_request.is_none() && !self.asked_addresses {
            self.asked_addresses = true;
            let request = self.request(Request::OwnAddresses);
            self.addresses_request = Some(effect_id(&request));
            effects.push(request);
        }
        if self.addresses_request.is_some() {
            self.awaiting_addresses = Some((kind, message));
            return effects;
        }
        self.begin(kind, Some(message))
    }

    /// Continues a reply or forward once `message` has been fetched.
    pub(super) fn on_compose_fetched(
        &mut self,
        kind: Kind,
        message: mett_core::MessageId,
        result: mett_core::Result<Message>,
    ) -> Vec<Effect> {
        match result {
            Ok(message) => self.reply_when_ready(kind, message),
            Err(e) => {
                // Without a sign-in, the sign-in window's Retry starts the
                // reply or forward again.
                if matches!(e, mett_core::Error::Auth(_)) {
                    self.compose_unfetched = Some((message, kind));
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
        if selected != Some(&id) {
            return Vec::new();
        }
        match self.screen() {
            Screen::Index | Screen::Pager(_) => self.start_compose(kind),
            // Back on the index or pager, the user can start it again.
            _ => Vec::new(),
        }
    }

    pub(super) fn begin(&mut self, kind: Kind, original: Option<Message>) -> Vec<Effect> {
        self.last_draft += 1;
        let settings = &self.settings;
        let mut draft = Draft {
            from: settings.from.clone(),
            attempt: self.last_draft,
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
                draft.subject =
                    compose::expand(&settings.forward_format, message, &settings.date_format);
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
                    // A reply to one's own message goes to its recipients.
                    if !compose::has_distinct_reply_to(original)
                        || compose::from_me(original, &self.settings)
                    {
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
                // Tab, or `?` on an empty line, shows the folders to choose
                // from, as in mutt.
                let browse = key.code == KeyCode::Tab
                    || (key.code == KeyCode::Char('?') && editor.text().is_empty());
                if question.is_mailbox() && browse {
                    let typed = editor.text();
                    self.input = None;
                    return self.open_browser(Choose::Answer(question, typed));
                }
                // In an address question, Tab completes an alias and ^T
                // queries, as in mutt.
                if App::takes_addresses(question) {
                    let control = key.modifiers.contains(KeyModifiers::CONTROL);
                    // What is before the cursor is completed (the rest of
                    // its entry replaced, once one is chosen).
                    if key.code == KeyCode::Tab {
                        let (typed, rest) = editor.split_at_cursor();
                        return self.complete_alias(question, typed, rest);
                    }
                    if control && key.code == KeyCode::Char('t') {
                        let (typed, rest) = editor.split_at_cursor();
                        return self.query_for(question, typed, rest);
                    }
                }
                // Space on an empty line, or on a mailbox offered, offers
                // the next one with new mail, as in mutt.
                let typed = editor.text();
                let offered = typed.is_empty() || self.mailboxes.iter().any(|m| m.text == typed);
                if question == Question::OpenMailbox
                    && key.code == KeyCode::Char(' ')
                    && key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
                    && offered
                    && let Some(next) = self.next_new_mailbox(Some(&typed))
                {
                    let next = next.text.clone();
                    if let Some(Input::Line { editor, .. }) = &mut self.input {
                        *editor = LineEditor::new(&next);
                    }
                    return Vec::new();
                }
                let Some(Input::Line { editor, .. }) = &mut self.input else {
                    return Vec::new();
                };
                match editor.key(key) {
                    Outcome::Editing => Vec::new(),
                    Outcome::Done(text) => {
                        self.input = None;
                        self.answer_line(question, text)
                    }
                    Outcome::Cancelled => {
                        self.input = None;
                        self.cancel_question(question)
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
                        return self.cancel_question(question);
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
    /// up, or leave things as they were.
    fn cancel_question(&mut self, question: Question) -> Vec<Effect> {
        match question {
            Question::Purge => self.cancel_purge(),
            // A folder chosen in the browser goes with the question.
            // So does a browser to open again after a sign-in.
            Question::OpenMailbox | Question::SaveTo | Question::CopyTo => {
                self.browsed = None;
                self.unbrowsed = None;
                self.question_message = None;
            }
            Question::Search | Question::Limit | Question::Command | Question::AttachFile => {}
            Question::SaveAttachment | Question::Query => {}
            Question::AliasName | Question::AliasAddress => {
                self.new_alias = None;
                self.alias_key = None;
            }
            Question::Overwrite => {
                self.unsaved = None;
            }
            _ => self.composing = None,
        }
        Vec::new()
    }

    fn answer_line(&mut self, question: Question, text: String) -> Vec<Effect> {
        if question.is_mailbox() {
            return self.answer_mailbox(question, text);
        }
        if matches!(question, Question::Search | Question::Limit) {
            return self.answer_search(question, text);
        }
        if question == Question::Command {
            return self.answer_command(text);
        }
        if question == Question::AttachFile {
            return self.answer_attach_file(text);
        }
        if question == Question::SaveAttachment {
            return self.answer_save_attachment(text);
        }
        match question {
            Question::Query => return self.start_query(&text, super::query::Purpose::Mail),
            Question::AliasName => return self.answer_alias_name(text),
            Question::AliasAddress => return self.answer_alias_address(text),
            _ => {}
        }
        // Aliases stand for their addresses.
        let parsed = alias::expand(&text, &self.aliases)
            .and_then(|expanded| Address::parse_list(&expanded).map_err(|e| e.to_string()));
        let setting_up = self.composing.is_some();
        let Some(draft) = self.draft_mut() else {
            return Vec::new();
        };
        match question {
            Question::To | Question::Cc | Question::Bcc => {
                let list = match parsed {
                    Ok(list) => list,
                    Err(e) => {
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
            Question::AbortSendNoSubject if answer => {
                self.fail("No subject specified.");
                return Vec::new();
            }
            Question::AbortSendNoSubject => return self.send_or_ask_copy(),
            Question::AbortUnmodified if answer => {
                return self.abort("Aborted unmodified message.");
            }
            Question::AbortUnmodified => return self.open_compose_menu(),
            Question::Postpone if answer => return self.deliver(Delivery::Postpone),
            Question::Postpone => {
                self.status = "Mail not sent.".into();
                // Given up on: so is a draft a failed attempt left.
                return match self.screens.pop() {
                    Some(Screen::Compose(compose)) if compose.left_draft => {
                        self.discard_kept(compose.draft)
                    }
                    _ => Vec::new(),
                };
            }
            Question::Copy => {
                if let Some(draft) = self.draft_mut() {
                    draft.save_copy = answer;
                }
                return self.deliver(Delivery::Send);
            }
            Question::Purge => return self.answer_purge(answer),
            Question::Overwrite => return self.answer_overwrite(answer),
            Question::AbortNoSubject
            | Question::To
            | Question::Cc
            | Question::Bcc
            | Question::Subject
            | Question::OpenMailbox
            | Question::SaveTo
            | Question::CopyTo
            | Question::Search
            | Question::Limit
            | Question::Command
            | Question::AttachFile
            | Question::SaveAttachment
            | Question::Query
            | Question::AliasName
            | Question::AliasAddress => {}
        }
        self.advance()
    }

    /// Sends the compose menu's message, asking first whether to keep a
    /// copy (`copy`). Graph always keeps a copy of what it sends through a
    /// server draft (replies, forwards, messages with large attachments),
    /// so `copy` only applies to other mail.
    fn send_or_ask_copy(&mut self) -> Vec<Effect> {
        let Some(Screen::Compose(compose)) = self.screens.last() else {
            return Vec::new();
        };
        // A server draft kept from an earlier attempt is what is sent,
        // even if the message would now go without one.
        if mett_msgraph::needs_server_draft(&compose.draft) || compose.kept_draft {
            return self.answer(Question::Copy, true);
        }
        let prompt = "Save a copy of this message?".to_owned();
        match self.decide(self.settings.copy, prompt, Question::Copy) {
            Some(copy) => self.answer(Question::Copy, copy),
            None => Vec::new(),
        }
    }

    /// Deletes what failed attempts to send or postpone `draft` left
    /// behind.
    pub(super) fn discard_kept(&mut self, draft: Draft) -> Vec<Effect> {
        let request = self.request(Request::DiscardKept(draft.clone()));
        if let Effect::Request(id, _) = &request {
            self.discarding.insert(*id, draft);
        }
        vec![request]
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
                self.fail(e);
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
            self.screens.push(Screen::Compose(Compose::new(
                composing.draft,
                forwarded_attachments,
            )));
        }
        Vec::new()
    }

    pub(super) fn on_compose_action(&mut self, action: Action) -> Vec<Effect> {
        let Some(Screen::Compose(compose)) = self.screens.last() else {
            return Vec::new();
        };
        if compose.delivery.is_some() {
            self.fail("Please wait: the message is still being sent or postponed.");
            return Vec::new();
        }
        // What the actions need of the message, without copying its
        // attachments.
        let draft = &compose.draft;
        let (recipients, subject) = (draft.has_recipients(), !draft.subject.is_empty());
        match action {
            Action::SendMessage => {
                if !recipients {
                    self.fail("No recipients are specified!");
                    return Vec::new();
                }
                // As in mutt, a subject left out is checked again when
                // sending (it may have been cleared in the menu).
                if !subject {
                    let prompt = "No subject, abort sending?".to_owned();
                    let quad = self.settings.abort_nosubject;
                    return match self.decide(quad, prompt, Question::AbortSendNoSubject) {
                        Some(answer) => self.answer(Question::AbortSendNoSubject, answer),
                        None => Vec::new(),
                    };
                }
                return self.send_or_ask_copy();
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
            Action::EditMessage => return vec![Effect::Edit(draft.body.clone())],
            Action::EditTo | Action::EditCc | Action::EditBcc | Action::EditSubject => {
                let question = match action {
                    Action::EditTo => Question::To,
                    Action::EditCc => Question::Cc,
                    Action::EditBcc => Question::Bcc,
                    _ => Question::Subject,
                };
                let current = field(draft, question);
                self.ask_line(question, &current);
            }
            Action::AttachFile => self.ask_attach_file(),
            Action::DetachFile => self.detach_file(),
            Action::NextEntry | Action::PreviousEntry => {
                if let Some(Screen::Compose(compose)) = self.screens.last_mut() {
                    compose.selected = if action == Action::NextEntry {
                        (compose.selected + 1).min(compose.parts() - 1)
                    } else {
                        compose.selected.saturating_sub(1)
                    };
                }
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
        kept: bool,
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
        compose.kept_draft = kept;
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
                    // Graph files the sent copy a moment after accepting
                    // the message: look again a little later too.
                    let mut effects = Vec::new();
                    if landed_in == Some(WellKnownFolder::SentItems) {
                        effects.push(Effect::Later(
                            SENT_COPY_DELAY,
                            super::Timer::RelistSentItems,
                        ));
                    }
                    // Not under a pager, whose message and position would
                    // no longer match the index: once back in the index.
                    // On the same message as before.
                    if self.screen() == &Screen::Index {
                        effects.extend(self.refresh());
                    } else {
                        self.relist_on_return = true;
                    }
                    return effects;
                }
            }
            Err(e) => {
                if matches!(e, mett_core::Error::Auth(_)) {
                    compose.unauthorized = delivery;
                }
                // Replies, forwards and messages with large attachments go
                // through a server draft, as does one the store kept.
                compose.left_draft |= kept || mett_msgraph::needs_server_draft(&compose.draft);
                // A lost response doesn't mean nothing happened.
                let failure = match (delivery, &e) {
                    (Some(Delivery::Send), mett_core::Error::Network(_)) if saved_copy => format!(
                        "The message may have been sent ({e}). Check Sent Items before sending it again."
                    ),
                    // Without a copy, Sent Items can't tell.
                    (Some(Delivery::Send), mett_core::Error::Network(_)) => format!(
                        "The message may have been sent ({e}), without a copy in Sent Items. \
                         Check with its recipients before sending it again."
                    ),
                    // It failed before being handed over.
                    (Some(Delivery::Send), mett_core::Error::NotSent(why)) => {
                        format!("The message wasn't sent: {why}.")
                    }
                    (Some(Delivery::Postpone), mett_core::Error::NotSent(why)) => {
                        format!("The message wasn't postponed: {why}.")
                    }
                    (Some(Delivery::Postpone), mett_core::Error::Network(_)) => format!(
                        "The message may have been saved ({e}). Check Drafts before postponing it again."
                    ),
                    _ => e.to_string(),
                };
                self.fail(failure);
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
        // The mailbox knows of no other addresses of the user's.
        let Effect::Request(addresses, _) = effects[1].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        app.update(Msg::Response(Response {
            id: addresses,
            event: Event::OwnAddresses(Ok(Vec::new())),
        }));
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
            event: Event::Sent(Ok(()), false),
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
            event: Event::Sent(Err(Error::Backend("bad recipient".into())), false),
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
            event: Event::Sent(Err(Error::Network("offline".into())), false),
        }));

        let (id, request) = request(&act(&mut app, Action::PostponeMessage));
        assert!(matches!(request, Request::SaveDraft(d) if d.body == "Sure.\n"));
        app.update(Msg::Response(Response {
            id,
            event: Event::DraftSaved(Ok("d1".into()), false),
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
    fn sending_without_a_subject_asks_again() {
        let mut app = app(settings());
        act(&mut app, Action::Mail);
        answer(&mut app, "bob@x.org");
        answer(&mut app, "Hi");
        app.update(Msg::Edited(Ok("text".into())));
        // Cleared in the compose menu.
        act(&mut app, Action::EditSubject);
        key(&mut app, KeyCode::Backspace);
        key(&mut app, KeyCode::Backspace);
        key(&mut app, KeyCode::Enter);
        assert_eq!(compose(&app).draft.subject, "");
        assert!(act(&mut app, Action::SendMessage).is_empty());
        assert_eq!(prompt(&app), "No subject, abort sending?");
        key(&mut app, KeyCode::Char('y'));
        assert_eq!(app.status, "No subject specified.");
        assert!(compose(&app).delivery.is_none(), "still in the menu");
        // Or it is sent anyway.
        act(&mut app, Action::SendMessage);
        let effects = key(&mut app, KeyCode::Char('n'));
        assert!(matches!(request(&effects).1, Request::Send(d) if d.subject.is_empty()));
    }

    #[test]
    fn giving_up_on_a_reply_deletes_the_draft_a_failure_left() {
        let mut app = app(settings());
        act(&mut app, Action::Mail);
        answer(&mut app, "bob@x.org");
        answer(&mut app, "Hi");
        app.update(Msg::Edited(Ok("text".into())));
        // As for a reply, which goes through a server draft.
        if let Some(Screen::Compose(compose)) = app.screens.last_mut() {
            compose.draft.origin = Some(DraftOrigin::Reply("m1".into()));
        }
        let (id, _) = request(&act(&mut app, Action::SendMessage));
        app.update(Msg::Response(Response {
            id,
            event: Event::Sent(Err(Error::Network("timed out".into())), false),
        }));
        act(&mut app, Action::Exit);
        let effects = key(&mut app, KeyCode::Char('n'));
        let (id, request) = request(&effects);
        assert!(matches!(request, Request::DiscardKept(d) if d.body == "text"));
        assert_eq!(app.status, "Mail not sent.");
        assert!(app.saving(), "quitting waits for it");
        app.update(Msg::Response(Response {
            id,
            event: Event::KeptDiscarded(Err(Error::Network("offline".into()))),
        }));
        assert!(!app.saving());
        assert!(
            app.status
                .starts_with("A draft of the message is left in Drafts")
        ); // Reported on exit too: the status line goes with the terminal.
        assert_eq!(app.save_failures().len(), 1);
    }

    #[test]
    fn a_draft_left_for_want_of_a_sign_in_is_deleted_on_retry() {
        let mut app = app(settings());
        act(&mut app, Action::Mail);
        answer(&mut app, "bob@x.org");
        answer(&mut app, "Hi");
        app.update(Msg::Edited(Ok("text".into())));
        if let Some(Screen::Compose(compose)) = app.screens.last_mut() {
            compose.draft.origin = Some(DraftOrigin::Reply("m1".into()));
        }
        let (id, _) = request(&act(&mut app, Action::SendMessage));
        app.update(Msg::Response(Response {
            id,
            event: Event::Sent(Err(Error::Network("timed out".into())), false),
        }));
        act(&mut app, Action::Exit);
        let (id, _) = request(&key(&mut app, KeyCode::Char('n')));
        app.update(Msg::SignIn(crate::signin::SignInEvent::Finished {
            account: mett_core::AccountId::new("me@contoso.com"),
            outcome: Err("expired".into()),
        }));
        app.update(Msg::Response(Response {
            id,
            event: Event::KeptDiscarded(Err(Error::Auth("expired".into()))),
        }));
        assert_eq!(app.save_failures().len(), 1);
        let effects = act(&mut app, Action::Retry);
        let (id, request) = request(&effects);
        assert!(matches!(request, Request::DiscardKept(d) if d.body == "text"));
        app.update(Msg::Response(Response {
            id,
            event: Event::KeptDiscarded(Ok(())),
        }));
        assert!(app.save_failures().is_empty());
    }

    #[test]
    fn retrying_while_quitting_waits_for_a_draft_being_deleted() {
        let mut app = app(settings());
        act(&mut app, Action::Mail);
        answer(&mut app, "bob@x.org");
        answer(&mut app, "Hi");
        app.update(Msg::Edited(Ok("text".into())));
        if let Some(Screen::Compose(compose)) = app.screens.last_mut() {
            compose.draft.origin = Some(DraftOrigin::Reply("m1".into()));
        }
        let (id, _) = request(&act(&mut app, Action::SendMessage));
        app.update(Msg::Response(Response {
            id,
            event: Event::Sent(Err(Error::Network("timed out".into())), false),
        }));
        act(&mut app, Action::Exit);
        let (discard, _) = request(&key(&mut app, KeyCode::Char('n')));
        app.quit = true;
        app.update(Msg::SignIn(crate::signin::SignInEvent::Finished {
            account: mett_core::AccountId::new("me@contoso.com"),
            outcome: Err("expired".into()),
        }));
        // Retry before the delete reports back: it is retried once it has.
        act(&mut app, Action::Retry);
        let effects = app.update(Msg::Response(Response {
            id: discard,
            event: Event::KeptDiscarded(Err(Error::Auth("expired".into()))),
        }));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Request(_, Request::DiscardKept(_)))),
            "{effects:?}"
        );
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
            event: Event::Sent(Ok(()), false),
        }));
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::Later(_, crate::app::Timer::RelistSentItems),
                    Effect::Request(_, Request::ListMessages { .. }),
                ]
            ),
            "{effects:?}"
        );
        // And again when the timer goes off.
        let effects = app.update(Msg::Timer(crate::app::Timer::RelistSentItems));
        assert!(matches!(request(&effects).1, Request::ListMessages { .. }));
        assert!(app.messages.is_empty());
    }

    #[test]
    fn the_later_sent_items_relist_keeps_the_selection() {
        let mut app = app(settings());
        app.folder = Some(mock::folder(
            "sent",
            "Sent Items",
            Some(WellKnownFolder::SentItems),
        ));
        let listed = |ids: &[&str]| {
            ids.iter()
                .map(|id| mock::message(id, "sent", "s", DateTime::UNIX_EPOCH).summary)
                .collect::<Vec<_>>()
        };
        app.messages = listed(&["a", "b", "c"]);
        app.selected = 1;
        let relisted = |app: &mut App, ids: &[&str]| {
            let effects = app.update(Msg::Timer(crate::app::Timer::RelistSentItems));
            let (id, _) = request(&effects);
            app.update(Msg::Response(Response {
                id,
                event: Event::MessagesListed(Ok(MessagePage {
                    messages: listed(ids),
                    next: None,
                })),
            }));
        };
        // The new copy comes first: still on b.
        relisted(&mut app, &["new", "a", "b", "c"]);
        assert_eq!(app.messages[app.selected].id.as_str(), "b");
        // A second refresh before the first is listed keeps the place.
        let first = app.update(Msg::Timer(crate::app::Timer::RelistSentItems));
        assert!(!first.is_empty());
        relisted(&mut app, &["new", "a", "b", "c"]);
        assert_eq!(app.messages[app.selected].id.as_str(), "b");
        // b is gone: the message now where it was.
        relisted(&mut app, &["new", "a", "c"]);
        assert_eq!(app.messages[app.selected].id.as_str(), "c");
    }

    #[test]
    fn sending_from_sent_items_keeps_the_selection() {
        let mut app = app(settings());
        app.folder = Some(mock::folder(
            "sent",
            "Sent Items",
            Some(WellKnownFolder::SentItems),
        ));
        let listed = |ids: &[&str]| {
            ids.iter()
                .map(|id| mock::message(id, "sent", "s", DateTime::UNIX_EPOCH).summary)
                .collect::<Vec<_>>()
        };
        app.messages = listed(&["a", "b", "c"]);
        app.selected = 1;
        act(&mut app, Action::Mail);
        answer(&mut app, "bob@x.org");
        answer(&mut app, "Hi");
        app.update(Msg::Edited(Ok("text".into())));
        let (id, _) = request(&act(&mut app, Action::SendMessage));
        let effects = app.update(Msg::Response(Response {
            id,
            event: Event::Sent(Ok(()), false),
        }));
        let list = effects
            .iter()
            .find_map(|e| match e {
                Effect::Request(id, Request::ListMessages { .. }) => Some(*id),
                _ => None,
            })
            .expect("listed again");
        app.update(Msg::Response(Response {
            id: list,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: listed(&["new", "a", "b", "c"]),
                next: None,
            })),
        }));
        assert_eq!(app.messages[app.selected].id.as_str(), "b");
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
            event: Event::Sent(Err(Error::Auth("expired".into())), false),
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
            event: Event::Sent(Ok(()), false),
        }));
        assert!(
            matches!(effects.as_slice(), [Effect::Later(..)]),
            "no listing under the pager: {effects:?}"
        );
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
    fn a_reply_retried_after_the_index_moved_is_not_to_another_message() {
        let mut app = app(settings());
        let (fetch, _) = request(&act(&mut app, Action::Reply));
        // A background listing puts another message where m1 was.
        let other = mock::message("m0", "in", "Newer", DateTime::UNIX_EPOCH);
        app.messages.insert(0, other.summary);
        app.update(Msg::SignIn(crate::signin::SignInEvent::Finished {
            account: mett_core::AccountId::new("me@contoso.com"),
            outcome: Err("expired".into()),
        }));
        app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Err(Error::Auth("expired".into()))),
        }));
        let effects = act(&mut app, Action::Retry);
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Request(_, Request::FetchMessage(id)) if id.as_str() == "m0")),
            "{effects:?}"
        );
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
            event: Event::Sent(Err(Error::Network("timed out".into())), false),
        }));
        assert!(
            app.status.starts_with("The message may have been sent"),
            "{}",
            app.status
        );
    }

    #[test]
    fn a_message_that_failed_before_sending_was_not_sent() {
        let mut app = app(settings());
        act(&mut app, Action::Mail);
        answer(&mut app, "bob@x.org");
        answer(&mut app, "Hi");
        app.update(Msg::Edited(Ok("text".into())));
        let (id, _) = request(&act(&mut app, Action::SendMessage));
        app.update(Msg::Response(Response {
            id,
            event: Event::Sent(
                Err(Error::NotSent("network error: timed out".into())),
                false,
            ),
        }));
        assert_eq!(
            app.status,
            "The message wasn't sent: network error: timed out."
        );
    }

    #[test]
    fn each_composed_message_is_its_own_attempt() {
        let mut app = app(settings());
        let mut attempts = Vec::new();
        for _ in 0..2 {
            act(&mut app, Action::Mail);
            answer(&mut app, "bob@x.org");
            answer(&mut app, "Hi");
            app.update(Msg::Edited(Ok("text".into())));
            let (id, Request::Send(sent)) = request(&act(&mut app, Action::SendMessage)) else {
                panic!("expected Send");
            };
            app.update(Msg::Response(Response {
                id,
                event: Event::Sent(Err(Error::Network("timed out".into())), false),
            }));
            // Sending the same message again is the same attempt.
            let (id, Request::Send(again)) = request(&act(&mut app, Action::SendMessage)) else {
                panic!("expected Send");
            };
            assert_eq!(again.attempt, sent.attempt);
            app.update(Msg::Response(Response {
                id,
                event: Event::Sent(Ok(()), false),
            }));
            attempts.push(sent.attempt);
        }
        assert_ne!(attempts[0], attempts[1]);
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
            event: Event::Sent(Ok(()), false),
        }));
        assert!(effects.is_empty());
        assert_eq!(app.messages.len(), 1);
    }

    #[test]
    fn a_left_server_draft_is_sent_with_its_copy() {
        let mut app = app(compose::Settings {
            copy: Quad::No,
            ..settings()
        });
        act(&mut app, Action::Mail);
        answer(&mut app, "bob@x.org");
        answer(&mut app, "Hi");
        app.update(Msg::Edited(Ok("text".into())));
        // A failed attempt that kept nothing leaves `copy` as it is…
        let (id, sent) = request(&act(&mut app, Action::SendMessage));
        assert!(matches!(sent, Request::Send(d) if !d.save_copy));
        app.update(Msg::Response(Response {
            id,
            event: Event::Sent(Err(Error::Auth("expired".into())), false),
        }));
        let (id, sent) = request(&act(&mut app, Action::SendMessage));
        assert!(matches!(sent, Request::Send(d) if !d.save_copy));
        // …but one that kept a server draft sends that, which Graph keeps
        // a copy of.
        app.update(Msg::Response(Response {
            id,
            event: Event::Sent(Err(Error::Network("timed out".into())), true),
        }));
        let (_, sent) = request(&act(&mut app, Action::SendMessage));
        assert!(matches!(sent, Request::Send(d) if d.save_copy));
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
            event: Event::Sent(Err(Error::Auth("expired".into())), false),
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
            event: Event::DraftSaved(Err(Error::Network("timed out".into())), false),
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
            event: Event::Sent(Err(Error::Network("timed out".into())), false),
        }));
        assert!(app.status.contains("without a copy"), "{}", app.status);
    }

    #[test]
    fn a_reply_that_needed_a_sign_in_is_retried_from_the_pager() {
        let mut app = app(settings());
        let (pager_fetch, _) = request(&act(&mut app, Action::DisplayMessage));
        let (reply_fetch, _) = request(&act(&mut app, Action::Reply));
        app.update(Msg::Response(Response {
            id: pager_fetch,
            event: Event::MessageFetched(Ok(original())),
        }));
        app.update(Msg::SignIn(crate::signin::SignInEvent::Finished {
            account: mett_core::AccountId::new("me@contoso.com"),
            outcome: Err("expired".into()),
        }));
        app.update(Msg::Response(Response {
            id: reply_fetch,
            event: Event::MessageFetched(Err(Error::Auth("expired".into()))),
        }));
        act(&mut app, Action::Retry);
        // The pager has the message now: the reply starts from it.
        assert_eq!(prompt(&app), "Include message in reply?");
    }

    #[test]
    fn a_relist_due_while_composing_happens_on_leaving() {
        let mut app = app(compose::Settings {
            postpone: Quad::No,
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
        // The timer from an earlier send goes off in the compose menu.
        assert!(
            app.update(Msg::Timer(crate::app::Timer::RelistSentItems))
                .is_empty()
        );
        let effects = act(&mut app, Action::Exit);
        assert!(matches!(request(&effects).1, Request::ListMessages { .. }));
    }

    #[test]
    fn a_reply_waits_for_the_users_own_addresses() {
        let inbox = crate::config::mailbox::FolderPath::Named(vec!["Inbox".into()]);
        let (mut app, effects) = App::open(80, 24, inbox, Settings::default());
        let (open, _) = request(&effects);
        let effects = app.update(Msg::Response(Response {
            id: open,
            event: Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
        }));
        let Effect::Request(list, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let Effect::Request(addresses, _) = effects[1].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        // A message the user sent from an alias, to Bob.
        let mut mine = original();
        mine.summary.from = Some(Address::new("alias@contoso.com"));
        mine.summary.to = vec![Address::new("bob@x.org")];
        app.update(Msg::Response(Response {
            id: list,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: vec![mine.summary.clone()],
                next: None,
            })),
        }));
        let (fetch, _) = request(&act(&mut app, Action::Reply));
        app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Ok(mine)),
        }));
        assert!(app.input.is_none(), "no recipients chosen yet");
        app.update(Msg::Response(Response {
            id: addresses,
            event: Event::OwnAddresses(Ok(vec!["alias@contoso.com".into()])),
        }));
        assert!(app.input.is_some(), "the reply goes on");
        // Include the message (the default), then To.
        key(&mut app, KeyCode::Enter);
        let Some(Input::Line { editor, .. }) = &app.input else {
            panic!("expected the To question, got {:?}", app.input);
        };
        assert_eq!(editor.text(), "bob@x.org");
    }

    #[test]
    fn a_failed_address_lookup_is_tried_again_by_a_reply() {
        let inbox = crate::config::mailbox::FolderPath::Named(vec!["Inbox".into()]);
        let (mut app, effects) = App::open(80, 24, inbox, Settings::default());
        let (open, _) = request(&effects);
        let effects = app.update(Msg::Response(Response {
            id: open,
            event: Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
        }));
        let Effect::Request(list, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let Effect::Request(addresses, _) = effects[1].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        app.update(Msg::Response(Response {
            id: addresses,
            event: Event::OwnAddresses(Err(Error::Network("busy".into()))),
        }));
        let mut mine = original();
        mine.summary.from = Some(Address::new("alias@contoso.com"));
        mine.summary.to = vec![Address::new("bob@x.org")];
        app.update(Msg::Response(Response {
            id: list,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: vec![mine.summary.clone()],
                next: None,
            })),
        }));
        let (fetch, _) = request(&act(&mut app, Action::Reply));
        let effects = app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Ok(mine)),
        }));
        let (again, asked) = request(&effects);
        assert!(matches!(asked, Request::OwnAddresses), "asked again");
        assert!(app.input.is_none(), "the reply waits");
        app.update(Msg::Response(Response {
            id: again,
            event: Event::OwnAddresses(Ok(vec!["alias@contoso.com".into()])),
        }));
        key(&mut app, KeyCode::Enter);
        let Some(Input::Line { editor, .. }) = &app.input else {
            panic!("expected the To question, got {:?}", app.input);
        };
        assert_eq!(editor.text(), "bob@x.org");
    }

    #[test]
    fn a_reply_given_up_with_its_sign_in_is_not_restarted_later() {
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
        // The user closes the failure window.
        act(&mut app, Action::Exit);
        // Another sign-in fails later, and the user retries that one.
        app.update(Msg::SignIn(crate::signin::SignInEvent::Finished {
            account: mett_core::AccountId::new("me@contoso.com"),
            outcome: Err("expired".into()),
        }));
        let effects = act(&mut app, Action::Retry);
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Request(_, Request::FetchMessage(_)))),
            "{effects:?}"
        );
    }

    #[test]
    fn writing_during_a_sent_items_refresh_keeps_the_selection() {
        let mut app = app(settings());
        app.folder = Some(mock::folder(
            "sent",
            "Sent Items",
            Some(WellKnownFolder::SentItems),
        ));
        let listed = |ids: &[&str]| {
            ids.iter()
                .map(|id| mock::message(id, "sent", "s", DateTime::UNIX_EPOCH).summary)
                .collect::<Vec<_>>()
        };
        app.messages = listed(&["a", "b", "c"]);
        app.selected = 1;
        let effects = app.update(Msg::Timer(crate::app::Timer::RelistSentItems));
        let (list, _) = request(&effects);
        // The user starts a new message before the listing is back.
        act(&mut app, Action::Mail);
        app.update(Msg::Response(Response {
            id: list,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: listed(&["new", "a", "b", "c"]),
                next: None,
            })),
        }));
        assert_eq!(app.messages[app.selected].id.as_str(), "b");
    }
}
