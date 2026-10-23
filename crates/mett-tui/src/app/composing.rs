//! Writing mail, as mutt does it: the questions asked on the message line
//! before the editor starts, the editor, then the compose menu, from which
//! the message is sent or postponed.

use std::collections::VecDeque;

use mett_core::{Address, Draft, DraftOrigin, Message, WellKnownFolder};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::mailbox::Choose;
use super::retry::{Retry, RetryKind};
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
    /// One of a few keys.
    Choice {
        /// The question, with the keys it takes.
        prompt: String,
        /// The keys that choose (the answer is the one pressed, in lower
        /// case); any other key waits, but for `^G` and Esc, which give
        /// up and 'c', which also does.
        keys: Vec<char>,
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
    /// What an answer to an invitation covers.
    InviteScope,
    /// What is told with an answer to an invitation.
    InviteComment,
    /// Whether the organizer is told the answer.
    InviteNotify,
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
    /// Ask for the recipients or the subject.
    Ask(Question),
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
    /// A recalled postponed message's attachments that stay with it.
    pub kept: Vec<mett_core::Attachment>,
    /// The selected part: its text, the forwarded attachments if there
    /// are any, the recalled message's, then the attached files.
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
            kept: Vec::new(),
            selected: 0,
        }
    }

    /// How many parts the message has.
    pub fn parts(&self) -> usize {
        1 + usize::from(self.forwarded_attachments) + self.kept.len() + self.draft.attachments.len()
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
        let Some(summary) = self.index.messages.get(self.index.selected) else {
            self.fail("No messages.");
            return Vec::new();
        };
        let id = summary.id.clone();
        let fetch = self.request(Request::FetchMessage(id.clone()));
        self.compose_fetch = Some((effect_id(&fetch), kind, id));
        self.status.info("Fetching message...");
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
                    self.retries.push(Retry::ComposeFetch { id: message, kind });
                }
                self.report(e);
                Vec::new()
            }
        }
    }

    /// Starts again a reply or forward whose message couldn't be fetched
    /// for want of a sign-in, if its message is still the one selected.
    pub(super) fn retry_compose_fetch(&mut self) -> Vec<Effect> {
        let Some(Retry::ComposeFetch { id, kind }) = self.retries.take(RetryKind::ComposeFetch)
        else {
            return Vec::new();
        };
        let selected = self.index.messages.get(self.index.selected).map(|m| &m.id);
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
            steps.push_back(Step::Ask(Question::To));
            if settings.askcc {
                steps.push_back(Step::Ask(Question::Cc));
            }
            if settings.askbcc {
                steps.push_back(Step::Ask(Question::Bcc));
            }
            steps.push_back(Step::Ask(Question::Subject));
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
                    return self.settle(self.settings.reply_to, prompt, Question::ReplyTo);
                }
                Step::Include => {
                    let prompt = "Include message in reply?";
                    return self.settle(self.settings.include, prompt, Question::Include);
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
                Step::Ask(Question::Subject) | Step::Edit if !composing.draft.has_recipients() => {
                    return self.abort("No recipients were specified.");
                }
                Step::Ask(question) => {
                    let current = field(&composing.draft, question);
                    self.ask_line(question, &current, "");
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
                    return self.edit(body);
                }
            }
        }
    }

    fn composing_mut(&mut self) -> &mut Composing {
        self.composing.as_mut().expect("a message is being set up")
    }

    /// Settles a quad-option: answers `question` as it says, or asks the
    /// user.
    fn settle(&mut self, quad: Quad, prompt: impl Into<String>, question: Question) -> Vec<Effect> {
        match quad {
            Quad::Yes => self.answer(question, true),
            Quad::No => self.answer(question, false),
            Quad::AskYes | Quad::AskNo => {
                self.input = Some(Input::YesNo {
                    prompt: prompt.into(),
                    default: quad == Quad::AskYes,
                    question,
                });
                Vec::new()
            }
        }
    }

    /// Asks `question` on the message line, with `current` typed.
    pub(super) fn ask(&mut self, question: Question, prompt: &str, current: &str) {
        self.input = Some(Input::Line {
            prompt: prompt.to_owned(),
            editor: LineEditor::new(current),
            question,
        });
    }

    /// Asks for the recipients or the subject, with `text` typed before
    /// the cursor and `rest` after it.
    pub(super) fn ask_line(&mut self, question: Question, text: &str, rest: &str) {
        let prompt = match question {
            Question::To => "To: ",
            Question::Cc => "Cc: ",
            Question::Bcc => "Bcc: ",
            _ => "Subject: ",
        };
        self.input = Some(Input::Line {
            prompt: prompt.to_owned(),
            editor: LineEditor::with_cursor(text, rest),
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
            Input::Choice { keys, question, .. } => {
                let question = *question;
                let KeyCode::Char(typed) = key.code else {
                    if cancel {
                        self.input = None;
                        return self.cancel_question(question);
                    }
                    return Vec::new();
                };
                let typed = typed.to_ascii_lowercase();
                let control = key.modifiers.contains(KeyModifiers::CONTROL);
                if (control && typed == 'g') || (!control && typed == 'c') {
                    self.input = None;
                    return self.cancel_question(question);
                }
                if control || !keys.contains(&typed) {
                    return Vec::new();
                }
                self.input = None;
                self.answer_scope(typed)
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
            Question::InviteScope | Question::InviteComment | Question::InviteNotify => {
                self.cancel_answer();
            }
            // A folder chosen in the browser goes with the question.
            // So does a browser to open again after a sign-in.
            Question::OpenMailbox | Question::SaveTo | Question::CopyTo => {
                self.browsed = None;
                self.retries.forget(RetryKind::Browse);
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
        match question {
            Question::OpenMailbox | Question::SaveTo | Question::CopyTo => {
                return self.answer_mailbox(question, text);
            }
            Question::Search | Question::Limit => return self.answer_search(question, text),
            Question::Command => return self.answer_command(text),
            Question::AttachFile => return self.answer_attach_file(text),
            Question::SaveAttachment => return self.answer_save_attachment(text),
            Question::InviteComment => return self.answer_comment(text),
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
                        self.ask_line(question, &text, "");
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
                    let quad = self.settings.abort_nosubject;
                    return self.settle(quad, "No subject, abort?", Question::AbortNoSubject);
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
                self.status.info("Mail not sent.");
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
            Question::InviteNotify => return self.answer_notify(answer),
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
            | Question::InviteScope
            | Question::InviteComment
            | Question::Query
            | Question::AliasName
            | Question::AliasAddress => {}
        }
        self.advance()
    }

    /// Sends the compose menu's message, asking first whether to keep a
    /// copy (`copy`). Graph always keeps a copy of what it sends through a
    /// server draft (replies, forwards, messages with large attachments),
    /// so there `copy` only applies to other mail.
    fn send_or_ask_copy(&mut self) -> Vec<Effect> {
        let Some(Screen::Compose(compose)) = self.screens.last() else {
            return Vec::new();
        };
        // A server draft kept from an earlier attempt is what is sent,
        // even if the message would now go without one.
        if self.api.always_copies(&compose.draft)
            || (compose.kept_draft && self.api == crate::account::Api::Graph)
        {
            return self.answer(Question::Copy, true);
        }
        let prompt = "Save a copy of this message?";
        self.settle(self.settings.copy, prompt, Question::Copy)
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
        self.status.info(why);
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

    /// The window was closed: mett quits as `q` in the index does (asking
    /// to purge deleted messages, as set), unless what is going on would
    /// be lost: a message being written, a file being saved, a question.
    pub(super) fn close(&mut self) -> Vec<Effect> {
        let writing = self.editing.is_some()
            || self.composing.is_some()
            || self.screens.iter().any(|s| matches!(s, Screen::Compose(_)));
        if writing {
            let why = "A message is being written: send or postpone it first.";
            match &mut self.editing {
                // Said where the editor shows it.
                Some(editing) => editing.get_mut().say(why),
                None => self.fail(why),
            }
            return Vec::new();
        }
        // What a file being saved, or a question, is waiting for would be
        // lost.
        if let Some((_, path)) = &self.file_wait {
            let why = format!("Waiting for {} (^G gives up) first.", path.display());
            self.fail(why);
            return Vec::new();
        }
        if self.input.is_some() {
            self.input_note = Some("Answer the question (or ^G) first.".into());
            return Vec::new();
        }
        self.leave_folder(super::mailbox::AfterPurge::Quit)
    }

    /// Puts pasted `text` where the user is typing: in the built-in editor,
    /// or in the answer to the question on the message line (on one line).
    pub(super) fn paste(&mut self, text: &str) {
        if self.sign_in.is_some() {
            return;
        }
        if let Some(editing) = &mut self.editing {
            editing.get_mut().paste(text);
        } else if let Some(Input::Line { editor, .. }) = &mut self.input {
            editor.insert_text(text);
            // The answer shows again, as when a key is typed.
            self.input_note = None;
        }
    }

    /// Pastes the clipboard's text where the user is typing: the system's,
    /// or, in the built-in editor, its own if the system's can't be read.
    pub(super) fn paste_clipboard(&mut self) {
        if self.sign_in.is_some() {
            return;
        }
        if let Some(editing) = &mut self.editing {
            editing.get_mut().paste_from_clipboard();
        } else if let Some(text) = crate::platform::clipboard_text() {
            self.paste(&text);
        }
    }

    /// Lets the user edit `text`, in the built-in editor or (through the
    /// main loop) in their own; [`Msg::Edited`] brings it back.
    pub(super) fn edit(&mut self, text: String) -> Vec<Effect> {
        match self.settings.builtin_editor {
            Some(keys) => {
                let mut editor = crate::editor::builtin::Builtin::new(&text, keys);
                let (speller, effects) = self.speller();
                editor.set_spelling(speller, self.settings.spell.check, self.unchecked());
                self.editing = Some(std::cell::RefCell::new(editor));
                effects
            }
            None => vec![Effect::Edit(text)],
        }
    }

    /// The dictionaries for the spelling settings, if spelling is checked
    /// and they are loaded; if not loaded, the effect that loads them
    /// (once).
    fn speller(
        &mut self,
    ) -> (
        Option<std::sync::Arc<crate::editor::spell::Speller>>,
        Vec<Effect>,
    ) {
        if !self.settings.spell.check {
            return (None, Vec::new());
        }
        // Turning checking off and on again needs no other dictionaries.
        let wanted = crate::editor::spell::Settings {
            check: true,
            ..self.settings.spell.clone()
        };
        if let Some((loaded, speller)) = &self.speller
            && *loaded == wanted
        {
            return (Some(speller.clone()), Vec::new());
        }
        if self.speller_loading.as_ref() == Some(&wanted) {
            return (None, Vec::new());
        }
        self.speller_loading = Some(wanted.clone());
        (None, vec![Effect::LoadSpeller(wanted)])
    }

    /// Gives the built-in editor, if open, the spelling settings and their
    /// dictionaries (loading them, if need be).
    pub(super) fn respell(&mut self) -> Vec<Effect> {
        if self.editing.is_none() {
            return Vec::new();
        }
        let (speller, effects) = self.speller();
        let check = self.settings.spell.check;
        let unchecked = self.unchecked();
        if let Some(editing) = &mut self.editing {
            editing.get_mut().set_spelling(speller, check, unchecked);
        }
        effects
    }

    /// What spelling isn't checked in: quotes and the signature, as the
    /// settings write them.
    fn unchecked(&self) -> crate::editor::spell::Unchecked {
        crate::editor::spell::Unchecked::new(
            &self.settings.indent_string,
            self.settings.signature.as_deref(),
        )
    }

    /// Takes in dictionaries loaded for `settings`, or why they couldn't
    /// be.
    pub(super) fn on_speller_loaded(
        &mut self,
        settings: crate::editor::spell::Settings,
        result: Result<std::sync::Arc<crate::editor::spell::Speller>, String>,
    ) -> Vec<Effect> {
        if self.speller_loading.as_ref() == Some(&settings) {
            self.speller_loading = None;
        }
        // For settings changed since: of no use.
        let wanted = crate::editor::spell::Settings {
            check: true,
            ..self.settings.spell.clone()
        };
        if settings != wanted {
            return Vec::new();
        }
        match result {
            Ok(speller) => {
                for (file, word) in &self.added_words {
                    if *file == settings.words {
                        speller.add(word);
                    }
                }
                self.speller = Some((settings, speller));
                self.respell()
            }
            // Not said once checking is off: nothing needs them.
            Err(_) if !self.settings.spell.check => Vec::new(),
            Err(error) => {
                self.fail(error);
                Vec::new()
            }
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
            let quad = self.settings.abort_unmodified;
            return self.settle(quad, "Abort unmodified message?", Question::AbortUnmodified);
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
                    let prompt = "No subject, abort sending?";
                    let quad = self.settings.abort_nosubject;
                    return self.settle(quad, prompt, Question::AbortSendNoSubject);
                }
                return self.send_or_ask_copy();
            }
            Action::Exit => {
                let prompt = "Postpone this message?";
                return self.settle(self.settings.postpone, prompt, Question::Postpone);
            }
            Action::PostponeMessage => return self.deliver(Delivery::Postpone),
            Action::EditMessage => {
                let body = draft.body.clone();
                return self.edit(body);
            }
            Action::EditTo | Action::EditCc | Action::EditBcc | Action::EditSubject => {
                let question = match action {
                    Action::EditTo => Question::To,
                    Action::EditCc => Question::Cc,
                    Action::EditBcc => Question::Bcc,
                    _ => Question::Subject,
                };
                let current = field(draft, question);
                self.ask_line(question, &current, "");
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
        self.status.info(status);
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
        // A message that was sent, though its copy wasn't kept, was sent.
        let (result, not_kept) = match result {
            Err(mett_core::Error::SentNotKept(why)) => (Ok(()), Some(why)),
            other => (other, None),
        };
        let delivery = compose.delivery.take().map(|(_, d)| d);
        compose.unauthorized = None;
        compose.kept_draft = kept;
        let saved_copy = compose.draft.save_copy;
        let recalled = match &compose.draft.origin {
            Some(DraftOrigin::Recalled(id)) => Some(id.clone()),
            _ => None,
        };
        match result {
            Ok(()) => {
                self.screens.remove(position);
                self.status.info(match delivery {
                    Some(Delivery::Postpone) => "Message postponed.",
                    _ => "Mail sent.",
                });
                if let Some(why) = &not_kept {
                    self.fail(format!("Mail sent, but no copy was kept: {why}"));
                }
                // The message landed in the open folder: list it again.
                // (A sent message lands in Sent Items only if a copy was
                // kept.)
                let landed_in = match delivery {
                    Some(Delivery::Postpone) => Some(WellKnownFolder::Drafts),
                    _ if saved_copy => Some(WellKnownFolder::SentItems),
                    _ => None,
                };
                // A pager showing a recalled message closes: once sent, it
                // has left Drafts; postponed again, it no longer reads as
                // the pager shows it.
                if let Some(id) = &recalled {
                    self.screens.retain(
                        |screen| !matches!(screen, Screen::Pager(pager) if &pager.id == id),
                    );
                }
                // Once sent, it has left Drafts.
                let left = recalled
                    .filter(|_| delivery != Some(Delivery::Postpone))
                    .map(|_| WellKnownFolder::Drafts);
                let open = self.folder.as_ref().and_then(|f| f.well_known);
                if open.is_some() && (open == landed_in || open == left) {
                    // Graph files the sent copy (and takes the sent draft
                    // out of Drafts) a moment after accepting the message:
                    // look again a little later too.
                    let mut effects = Vec::new();
                    if open == Some(WellKnownFolder::SentItems) {
                        effects.push(Effect::Later(
                            SENT_COPY_DELAY,
                            super::Timer::RelistSentItems,
                        ));
                    } else if open == left {
                        effects.push(Effect::Later(SENT_COPY_DELAY, super::Timer::RelistDrafts));
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
                compose.left_draft |= kept || self.api.needs_server_draft(&compose.draft);
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
        self.delivery_request().is_some()
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
mod tests;
