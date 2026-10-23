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
