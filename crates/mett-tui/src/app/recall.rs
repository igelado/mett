//! Recalling a postponed message, as mutt's `R` (`<recall-message>`) does:
//! the messages in Drafts, where mett postpones them, are listed (one is
//! taken at once), and the one chosen opens in the compose menu. It is
//! then sent, or postponed again, as itself.

use mett_core::{
    Attachment, BodyKind, Draft, DraftOrigin, Error, Folder, FolderId, Message, MessagePage,
    MessageSummary, Page, WellKnownFolder,
};

use super::{App, Compose, Effect, Screen, effect_id};
use crate::action::Action;
use crate::config::mailbox::FolderPath;
use crate::text;
use crate::worker::{Request, RequestId};

/// The postponed messages to choose from (mutt's postpone menu).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostponedMenu {
    /// The messages, newest first.
    pub entries: Vec<MessageSummary>,
    /// The selected entry.
    pub selected: usize,
}

/// A recall on its way: the request, and what it does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Recall {
    /// Opening the Drafts folder.
    Opening(RequestId),
    /// Listing it: the request, the folder, and the drafts found so far.
    Listing(RequestId, FolderId, Vec<MessageSummary>),
    /// Fetching the message chosen.
    Fetching(RequestId),
    /// Listing the attachments of the message fetched.
    Attachments(RequestId, Box<Message>),
}

impl Recall {
    fn request(&self) -> RequestId {
        match self {
            Recall::Opening(id)
            | Recall::Listing(id, ..)
            | Recall::Fetching(id)
            | Recall::Attachments(id, _) => *id,
        }
    }
}

impl App {
    /// Whether a recall may still open what it found: nothing else has
    /// taken over (a message being written, another screen, a question).
    fn recall_wanted(&mut self) -> bool {
        let free = matches!(self.screen(), Screen::Index | Screen::Pager(_))
            && self.composing.is_none()
            && self.input.is_none();
        if !free {
            self.recall = None;
            self.clear_recall_status();
        }
        free
    }

    /// The request of the recall on its way, if any.
    pub(super) fn recall_request(&self) -> Option<RequestId> {
        self.recall.as_ref().map(Recall::request)
    }

    /// `R`: looks for the postponed messages in Drafts.
    pub(super) fn recall_message(&mut self) -> Vec<Effect> {
        if self.recall.is_some() {
            return Vec::new();
        }
        self.unrecalled = false;
        let path = FolderPath::WellKnown(WellKnownFolder::Drafts, Vec::new());
        let request = self.request(Request::OpenFolder(path));
        self.recall = Some(Recall::Opening(effect_id(&request)));
        self.status = "Looking for postponed messages...".into();
        vec![request]
    }

    /// The Drafts folder was opened: lists it.
    pub(super) fn on_recall_opened(&mut self, result: mett_core::Result<Folder>) -> Vec<Effect> {
        if !self.recall_wanted() {
            return Vec::new();
        }
        match result {
            Ok(folder) => self.list_postponed(folder.id, Page::first(self.page_size), Vec::new()),
            Err(e) => self.recall_failed(e),
        }
    }

    /// Lists a page of Drafts, after the drafts `found` before it.
    fn list_postponed(
        &mut self,
        folder: FolderId,
        page: Page,
        found: Vec<MessageSummary>,
    ) -> Vec<Effect> {
        let request = self.request(Request::ListMessages {
            folder: folder.clone(),
            page,
        });
        self.recall = Some(Recall::Listing(effect_id(&request), folder, found));
        vec![request]
    }

    /// A page of Drafts was listed: the next one is, until all are. Then
    /// there is none to recall, the one there is, or a menu.
    pub(super) fn on_recall_listed(
        &mut self,
        result: mett_core::Result<MessagePage>,
    ) -> Vec<Effect> {
        if !self.recall_wanted() {
            return Vec::new();
        }
        let Some(Recall::Listing(_, folder, mut entries)) = self.recall.take() else {
            return Vec::new();
        };
        match result {
            // Drafts only: a message moved there isn't one to send.
            Ok(page) => {
                entries.extend(page.messages.into_iter().filter(|m| m.draft));
                if let Some(cursor) = page.next {
                    return self.list_postponed(
                        folder,
                        Page::after(cursor, self.page_size),
                        entries,
                    );
                }
            }
            Err(e) => return self.recall_failed(e),
        }
        self.clear_recall_status();
        match entries.len() {
            0 => {
                self.fail("No postponed messages.");
                Vec::new()
            }
            1 => {
                let message = entries.remove(0);
                self.fetch_postponed(message)
            }
            _ => {
                self.screens.push(Screen::Postponed(PostponedMenu {
                    entries,
                    selected: 0,
                }));
                Vec::new()
            }
        }
    }

    /// The postpone menu's actions.
    pub(super) fn on_postponed_action(&mut self, action: Action) -> Vec<Effect> {
        let page = self.body_height.max(1);
        let Some(Screen::Postponed(menu)) = self.screens.last_mut() else {
            return Vec::new();
        };
        if super::navigate(&mut menu.selected, action, menu.entries.len(), page) {
            return Vec::new();
        }
        match action {
            Action::Exit | Action::Quit => {
                self.screens.pop();
            }
            Action::DisplayMessage => {
                let Some(message) = menu.entries.get(menu.selected).cloned() else {
                    return Vec::new();
                };
                self.screens.pop();
                return self.fetch_postponed(message);
            }
            _ => {}
        }
        Vec::new()
    }

    /// Fetches the postponed `message`, to recall it.
    fn fetch_postponed(&mut self, message: MessageSummary) -> Vec<Effect> {
        let request = self.request(Request::FetchMessage(message.id));
        self.recall = Some(Recall::Fetching(effect_id(&request)));
        self.status = "Fetching postponed message...".into();
        vec![request]
    }

    /// The postponed message was fetched: it opens in the compose menu, as
    /// it was written, to be sent or postponed again as itself.
    pub(super) fn on_recall_fetched(&mut self, result: mett_core::Result<Message>) -> Vec<Effect> {
        if !self.recall_wanted() {
            return Vec::new();
        }
        self.recall = None;
        let message = match result {
            Ok(message) => message,
            Err(e) => return self.recall_failed(e),
        };
        if !message.summary.draft {
            self.clear_recall_status();
            self.fail("That message is no longer a draft.");
            return Vec::new();
        }
        // Its attachments are listed, each to keep or take off (even
        // inline ones only, which Graph doesn't count as attachments).
        let request = self.request(Request::ListAttachments(message.summary.id.clone()));
        self.recall = Some(Recall::Attachments(effect_id(&request), Box::new(message)));
        vec![request]
    }

    /// The recalled message's attachments were listed.
    pub(super) fn on_recall_attachments(
        &mut self,
        result: mett_core::Result<Vec<Attachment>>,
    ) -> Vec<Effect> {
        if !self.recall_wanted() {
            return Vec::new();
        }
        let Some(Recall::Attachments(_, message)) = self.recall.take() else {
            return Vec::new();
        };
        match result {
            Ok(attachments) => self.open_recalled(*message, attachments),
            Err(e) => self.recall_failed(e),
        }
    }

    /// Opens the recalled `message`, with its `attachments`, in the compose
    /// menu.
    fn open_recalled(&mut self, message: Message, attachments: Vec<Attachment>) -> Vec<Effect> {
        self.clear_recall_status();
        self.last_draft += 1;
        let body = match message.body.kind {
            BodyKind::Text => message.body.content.replace("\r\n", "\n"),
            // As the pager shows it: mett writes plain text.
            BodyKind::Html => text::body_lines(&message.body).join("\n"),
        };
        let draft = Draft {
            // From whom it was written.
            from: message.summary.from.or_else(|| self.settings.from.clone()),
            to: message.summary.to,
            cc: message.cc,
            bcc: message.bcc,
            subject: message.summary.subject,
            body,
            attachments: Vec::new(),
            drop_attachments: Vec::new(),
            origin: Some(DraftOrigin::Recalled(message.summary.id)),
            save_copy: true,
            attempt: self.last_draft,
        };
        // Its attachments stay with it, unless taken off.
        let mut compose = Compose::new(draft, false);
        compose.kept = attachments;
        self.screens.push(Screen::Compose(compose));
        Vec::new()
    }

    /// A step of the recall failed. Without a sign-in, the sign-in window's
    /// Retry starts it again.
    fn recall_failed(&mut self, e: Error) -> Vec<Effect> {
        self.recall = None;
        if matches!(e, Error::Auth(_)) {
            self.unrecalled = true;
        }
        self.report(e);
        Vec::new()
    }

    /// Starts again a recall that failed for want of a sign-in, from the
    /// index or the pager.
    pub(super) fn retry_recall(&mut self) -> Vec<Effect> {
        if !std::mem::take(&mut self.unrecalled) {
            return Vec::new();
        }
        match self.screen() {
            Screen::Index | Screen::Pager(_) => self.recall_message(),
            _ => Vec::new(),
        }
    }

    fn clear_recall_status(&mut self) {
        self.clear_status_if(&[
            "Looking for postponed messages...",
            "Fetching postponed message...",
        ]);
    }
}

#[cfg(test)]
mod tests;
