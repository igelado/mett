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
mod tests {
    use chrono::DateTime;
    use mett_core::{Address, Body, MessageId, WellKnownFolder, mock};

    use super::*;
    use crate::app::Settings;
    use crate::app::testing::*;
    use crate::worker::Event;

    /// An app that has opened its Inbox, not listed yet.
    fn app() -> App {
        opening(Settings::default(), 24).0
    }

    /// An app that has opened Drafts, with d1 in it.
    fn drafts_app() -> App {
        let path = FolderPath::WellKnown(WellKnownFolder::Drafts, vec![]);
        let (mut app, effects) = App::open(80, 24, path, Settings::default());
        let list = folder_opened(&mut app, first_request(&effects), drafts_folder());
        listed(&mut app, list, vec![postponed("d1", "Plans")], None);
        app
    }

    fn drafts_folder() -> Folder {
        mock::folder("drafts", "Drafts", Some(WellKnownFolder::Drafts))
    }

    fn postponed(id: &str, subject: &str) -> MessageSummary {
        let mut m = mock::message(id, "drafts", subject, DateTime::UNIX_EPOCH).summary;
        m.draft = true;
        m.to = vec![Address::new("bob@x.org")];
        m
    }

    /// Presses `R` and answers until Drafts is listed with `drafts`.
    fn recall(app: &mut App, drafts: Vec<MessageSummary>) -> Vec<Effect> {
        let (open, request) = sent(&act(app, Action::RecallMessage));
        assert_eq!(
            request,
            Request::OpenFolder(FolderPath::WellKnown(WellKnownFolder::Drafts, vec![]))
        );
        assert!(app.waiting());
        let (list, _) = sent(&respond(
            app,
            open,
            Event::FolderOpened(Ok(drafts_folder())),
        ));
        listed(app, list, drafts, None)
    }

    /// Answers the fetch of the recalled `message`, and the listing of its
    /// attachments (none).
    fn opened(app: &mut App, fetch: RequestId, message: Message) {
        let (list, request) = sent(&respond(app, fetch, Event::MessageFetched(Ok(message))));
        assert!(matches!(request, Request::ListAttachments(_)));
        respond(app, list, Event::AttachmentsListed(Ok(Vec::new())));
    }

    fn fetched(id: &str) -> Message {
        let mut message = mock::message(id, "drafts", "Plans", DateTime::UNIX_EPOCH);
        message.summary.from = Some(Address::new("me@work.org"));
        message.summary.draft = true;
        message.summary.to = vec![Address::new("bob@x.org")];
        message.cc = vec![Address::new("carol@x.org")];
        message.body = Body {
            kind: BodyKind::Text,
            content: "Dear Bob,\r\nsoon.\r\n".into(),
        };
        message
    }

    #[test]
    fn r_recalls_in_the_index_and_the_pager() {
        use crate::keymap::{Binding, Key, Keymap, Menu};
        let keymap = Keymap::mutt_defaults();
        for menu in [Menu::Index, Menu::Pager] {
            assert_eq!(
                keymap.lookup(menu, Key::char('R')),
                Some(&Binding::Action(Action::RecallMessage))
            );
        }
        assert_eq!(Menu::from_name("postpone"), Some(Menu::Postpone));
        assert_eq!(
            Action::from_name("recall-message"),
            Some(Action::RecallMessage)
        );
    }

    #[test]
    fn says_when_nothing_is_postponed() {
        let mut app = app();
        assert!(recall(&mut app, Vec::new()).is_empty());
        assert_eq!(app.status, "No postponed messages.");
        assert_eq!(app.screen(), &Screen::Index);
    }

    #[test]
    fn recalls_the_one_postponed_message_into_the_compose_menu() {
        let mut app = app();
        let effects = recall(&mut app, vec![postponed("d1", "Plans")]);
        let (fetch, request) = sent(&effects);
        assert_eq!(request, Request::FetchMessage(MessageId::new("d1")));
        opened(&mut app, fetch, fetched("d1"));
        let Screen::Compose(compose) = app.screen() else {
            panic!("expected the compose menu, got {:?}", app.screen());
        };
        let draft = &compose.draft;
        assert_eq!(
            draft.origin,
            Some(DraftOrigin::Recalled(MessageId::new("d1")))
        );
        assert_eq!(draft.to, [Address::new("bob@x.org")]);
        assert_eq!(draft.cc, [Address::new("carol@x.org")]);
        assert_eq!(draft.subject, "Plans");
        assert_eq!(draft.from, Some(Address::new("me@work.org")));
        assert_eq!(draft.body, "Dear Bob,\nsoon.\n");
        // Sent as itself.
        let (_, request) = sent(&act(&mut app, Action::SendMessage));
        let Request::Send(draft) = request else {
            panic!("expected a send, got {request:?}");
        };
        assert_eq!(
            draft.origin,
            Some(DraftOrigin::Recalled(MessageId::new("d1")))
        );
        assert!(draft.save_copy);
    }

    #[test]
    fn a_recalled_message_s_attachments_can_be_taken_off() {
        let mut app = app();
        let (fetch, _) = sent(&recall(&mut app, vec![postponed("d1", "Plans")]));
        let mut message = fetched("d1");
        message.summary.has_attachments = true;
        let (list, request) = sent(&respond(
            &mut app,
            fetch,
            Event::MessageFetched(Ok(message)),
        ));
        assert_eq!(request, Request::ListAttachments(MessageId::new("d1")));
        let attachment = |id: &str| mett_core::Attachment {
            id: mett_core::AttachmentId::new(id),
            name: format!("{id}.pdf"),
            content_type: "application/pdf".into(),
            size: 10,
            inline: false,
            kind: mett_core::AttachmentKind::File,
        };
        respond(
            &mut app,
            list,
            Event::AttachmentsListed(Ok(vec![attachment("a1"), attachment("a2")])),
        );
        let Screen::Compose(compose) = app.screen() else {
            panic!("expected the compose menu, got {:?}", app.screen());
        };
        assert_eq!(compose.parts(), 3);
        assert!(compose.draft.drop_attachments.is_empty());
        act(&mut app, Action::NextEntry);
        act(&mut app, Action::DetachFile);
        let Screen::Compose(compose) = app.screen() else {
            panic!("expected the compose menu");
        };
        assert_eq!(
            compose.draft.drop_attachments,
            [mett_core::AttachmentId::new("a1")]
        );
        assert_eq!(compose.parts(), 2);
    }

    #[test]
    fn only_drafts_are_recalled() {
        let mut app = app();
        let mut moved = postponed("m1", "Moved here");
        moved.draft = false;
        assert!(recall(&mut app, vec![moved]).is_empty());
        assert_eq!(app.status, "No postponed messages.");
    }

    #[test]
    fn a_recalled_message_is_postponed_again_as_itself() {
        let mut app = app();
        let (fetch, _) = sent(&recall(&mut app, vec![postponed("d1", "Plans")]));
        opened(&mut app, fetch, fetched("d1"));
        let (_, request) = sent(&act(&mut app, Action::PostponeMessage));
        let Request::SaveDraft(draft) = request else {
            panic!("expected a draft saved, got {request:?}");
        };
        assert_eq!(
            draft.origin,
            Some(DraftOrigin::Recalled(MessageId::new("d1")))
        );
    }

    #[test]
    fn every_page_of_drafts_is_looked_through() {
        let mut app = app();
        let (open, _) = sent(&act(&mut app, Action::RecallMessage));
        let (list, _) = sent(&respond(
            &mut app,
            open,
            Event::FolderOpened(Ok(drafts_folder())),
        ));
        let (next, request) = sent(&listed(
            &mut app,
            list,
            vec![postponed("d1", "Plans")],
            Some("drafts next"),
        ));
        assert!(matches!(request, Request::ListMessages { .. }));
        listed(&mut app, next, vec![postponed("d2", "Lunch")], None);
        let Screen::Postponed(menu) = app.screen() else {
            panic!("expected the postpone menu, got {:?}", app.screen());
        };
        assert_eq!(menu.entries.len(), 2);
    }

    #[test]
    fn several_postponed_messages_are_chosen_from_a_menu() {
        let mut app = app();
        let effects = recall(
            &mut app,
            vec![postponed("d1", "Plans"), postponed("d2", "Lunch")],
        );
        assert!(effects.is_empty());
        assert_eq!(app.menu(), crate::keymap::Menu::Postpone);
        act(&mut app, Action::NextEntry);
        let (_, request) = sent(&act(&mut app, Action::DisplayMessage));
        assert_eq!(request, Request::FetchMessage(MessageId::new("d2")));
        assert_eq!(app.screen(), &Screen::Index);
        // Or none.
        let mut app = super::tests::app();
        recall(
            &mut app,
            vec![postponed("d1", "Plans"), postponed("d2", "Lunch")],
        );
        act(&mut app, Action::Exit);
        assert_eq!(app.screen(), &Screen::Index);
    }

    #[test]
    fn a_recall_gives_way_to_what_the_user_does_meanwhile() {
        let mut app = app();
        let (open, _) = sent(&act(&mut app, Action::RecallMessage));
        // The user starts a new message before Drafts is opened.
        act(&mut app, Action::Mail);
        assert!(app.input.is_some());
        assert!(respond(&mut app, open, Event::FolderOpened(Ok(drafts_folder()))).is_empty());
        assert_eq!(app.recall, None);
        assert_eq!(app.screen(), &Screen::Index);
    }

    #[test]
    fn a_recall_gives_way_to_another_folder() {
        let mut app = app();
        let (open, _) = sent(&act(&mut app, Action::RecallMessage));
        app.change_folder(FolderPath::WellKnown(WellKnownFolder::SentItems, vec![]));
        respond(&mut app, open, Event::FolderOpened(Ok(drafts_folder())));
        assert_eq!(app.recall, None);
        assert!(!matches!(
            app.screen(),
            Screen::Postponed(_) | Screen::Compose(_)
        ));
    }

    #[test]
    fn a_pager_on_a_recalled_message_postponed_again_closes() {
        let mut app = drafts_app();
        act(&mut app, Action::DisplayMessage);
        let (fetch, _) = sent(&recall(&mut app, vec![postponed("d1", "Plans")]));
        opened(&mut app, fetch, fetched("d1"));
        let (save, _) = sent(&act(&mut app, Action::PostponeMessage));
        let effects = respond(
            &mut app,
            save,
            Event::DraftSaved(Ok(MessageId::new("d1")), false),
        );
        assert_eq!(app.screen(), &Screen::Index);
        // The index shows it as it is now.
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Request(_, Request::ListMessages { .. }))),
            "{effects:?}"
        );
    }

    #[test]
    fn sending_a_recalled_message_lists_drafts_again() {
        drafts_listed_again_after_sending(false);
    }

    #[test]
    fn a_pager_on_a_sent_recalled_message_closes() {
        drafts_listed_again_after_sending(true);
    }

    /// Sends `d1`, recalled while Drafts is open (from its pager if
    /// `from_pager`).
    fn drafts_listed_again_after_sending(from_pager: bool) {
        let mut app = drafts_app();
        if from_pager {
            act(&mut app, Action::DisplayMessage);
            assert!(matches!(app.screen(), Screen::Pager(_)));
        }
        let (fetch, _) = sent(&recall(&mut app, vec![postponed("d1", "Plans")]));
        opened(&mut app, fetch, fetched("d1"));
        let (send, _) = sent(&act(&mut app, Action::SendMessage));
        let effects = respond(&mut app, send, Event::Sent(Ok(()), false));
        assert_eq!(app.screen(), &Screen::Index);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Request(_, Request::ListMessages { .. }))),
            "{effects:?}"
        );
        // And again once Graph has taken it out.
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Later(_, crate::app::Timer::RelistDrafts))),
            "{effects:?}"
        );
    }

    #[test]
    fn a_recall_waiting_for_a_sign_in_starts_again() {
        let mut app = app();
        let (open, _) = sent(&act(&mut app, Action::RecallMessage));
        respond(&mut app, open, Event::FolderOpened(Err(expired())));
        assert_eq!(app.recall, None);
        let (open, request) = sent(&app.retry_recall());
        assert!(matches!(request, Request::OpenFolder(_)));
        // Given up by a change of folder: not started again.
        respond(&mut app, open, Event::FolderOpened(Err(expired())));
        app.change_folder(FolderPath::WellKnown(WellKnownFolder::SentItems, vec![]));
        assert!(app.retry_recall().is_empty());
    }
}
