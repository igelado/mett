use chrono::DateTime;
use mett_core::{AttachmentId, mock};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use crate::app::testing::*;
use crate::app::{Msg, Settings};
use crate::worker::Event;

#[test]
fn text_is_decoded_in_its_charset() {
    assert_eq!(
        decode("text/plain; charset=windows-1252", b"caf\xe9").as_deref(),
        Some("caf\u{e9}")
    );
    assert_eq!(
        decode("text/plain; charset=\"UTF-8\"", "café".as_bytes()).as_deref(),
        Some("café")
    );
    assert_eq!(
        decode("text/plain", b"caf\xe9").as_deref(),
        Some("caf\u{e9}")
    );
    assert_eq!(decode("text/plain; charset=x-unknown", b"x"), None);
}

fn app() -> App {
    let settings = Settings {
        home: Some("/home/alice".into()),
        ..Settings::default()
    };
    let mut message = mock::message("m0", "in", "Files", DateTime::UNIX_EPOCH).summary;
    message.has_attachments = true;
    opened(settings, 24, vec![message], None).0
}

fn attachment(id: &str, name: &str, content_type: &str, kind: AttachmentKind) -> Attachment {
    Attachment {
        id: AttachmentId::new(id),
        name: name.into(),
        content_type: content_type.into(),
        size: 5,
        inline: false,
        kind,
    }
}

fn notes() -> Attachment {
    attachment("a0", "notes.txt", "text/plain", AttachmentKind::File)
}

/// The attachment menu of m0, with a text file, a PDF and a link.
fn menu() -> App {
    let mut app = app();
    let (id, request) = sent(&act(&mut app, Action::ViewAttachments));
    assert_eq!(request, Request::ListAttachments(MessageId::new("m0")));
    respond(
        &mut app,
        id,
        Event::AttachmentsListed(Ok(vec![
            notes(),
            attachment("a1", "report.pdf", "application/pdf", AttachmentKind::File),
            attachment(
                "a2",
                "shared",
                "application/octet-stream",
                AttachmentKind::Link,
            ),
        ])),
    );
    app
}

#[test]
fn attachments_are_listed_and_fetched_again_after_a_sign_in() {
    let mut app = app();
    let (id, _) = sent(&act(&mut app, Action::ViewAttachments));
    respond(&mut app, id, Event::AttachmentsListed(Err(expired())));
    assert!(matches!(app.screen(), Screen::Attachments(_)));
    let failed = |app: &mut App| {
        app.update(Msg::SignIn(crate::signin::SignInEvent::Finished {
            account: mett_core::AccountId::new("me@contoso.com"),
            outcome: Err("expired".into()),
        }));
    };
    failed(&mut app);
    let (id, request) = sent(&act(&mut app, Action::Retry));
    assert_eq!(request, Request::ListAttachments(MessageId::new("m0")));
    respond(&mut app, id, Event::AttachmentsListed(Ok(vec![notes()])));
    let (id, _) = sent(&act(&mut app, Action::DisplayMessage));
    respond(&mut app, id, Event::AttachmentFetched(Err(expired())));
    failed(&mut app);
    let (_, request) = sent(&act(&mut app, Action::Retry));
    assert!(
        matches!(request, Request::FetchAttachment(m, a) if m.as_str() == "m0" && a.as_str() == "a0")
    );
}

#[test]
fn a_text_attachment_wraps_where_wrap_says() {
    let mut app = menu();
    app.screens.push(Screen::Text(TextView::new(
        "notes".into(),
        vec!["word ".repeat(30)],
        80,
    )));
    let mut settings = crate::app::Settings::default();
    settings.display.wrap = 20;
    app.update(configured(settings));
    let Screen::Text(view) = app.screen() else {
        panic!("expected the text");
    };
    assert_eq!(view.rows_width, Some(20));
    assert!(
        view.rows.iter().all(|row| row.len() <= 20),
        "{:?}",
        view.rows
    );
}

#[test]
fn the_pager_keys_scroll_a_text_attachment() {
    let mut app = menu();
    app.update(Msg::Resize {
        width: 80,
        height: 5,
    });
    app.screens.push(Screen::Text(TextView::new(
        "notes".into(),
        (0..10).map(|i| i.to_string()).collect(),
        80,
    )));
    let scroll = |app: &App| match app.screen() {
        Screen::Text(view) => view.scroll,
        _ => panic!("expected the text"),
    };
    act(&mut app, Action::NextEntry);
    assert_eq!(scroll(&app), 1);
    act(&mut app, Action::LastEntry);
    assert_eq!(scroll(&app), 8);
    act(&mut app, Action::PreviousEntry);
    assert_eq!(scroll(&app), 7);
    act(&mut app, Action::FirstEntry);
    assert_eq!(scroll(&app), 0);
    // Search isn't silently ignored.
    act(&mut app, Action::Search);
    assert!(app.status.contains("isn't supported"), "{}", app.status);
    assert!(app.input.is_none());
}

#[test]
fn attached_items_are_named_by_their_kind() {
    let item = |name: &str, content_type: &str| {
        file_name(&attachment("i", name, content_type, AttachmentKind::Item))
    };
    assert_eq!(item("Budget v1.2", "message/rfc822"), "Budget v1.2.eml");
    assert_eq!(item("Lunch.ICS", "text/calendar"), "Lunch.ICS");
    assert_eq!(item("", "text/vcard"), "item.vcf");
    let file = |name: &str| {
        file_name(&attachment(
            "f",
            name,
            "application/pdf",
            AttachmentKind::File,
        ))
    };
    assert_eq!(file("C:report.pdf"), "report.pdf");
    assert_eq!(file("../a\\b/c.pdf"), "c.pdf");
    assert_eq!(item("Lunch", "Text/Calendar; charset=utf-8"), "Lunch.ics");
}

#[test]
fn views_text_attachments_in_mett() {
    let mut app = menu();
    let (id, request) = sent(&act(&mut app, Action::ViewAttach));
    assert_eq!(
        request,
        Request::FetchAttachment(MessageId::new("m0"), AttachmentId::new("a0"))
    );
    respond(
        &mut app,
        id,
        Event::AttachmentFetched(Ok(b"hello\nworld".to_vec())),
    );
    let Screen::Text(view) = app.screen() else {
        panic!("expected text, got {:?}", app.screen());
    };
    assert_eq!(view.lines, ["hello", "world"]);
    act(&mut app, Action::Exit);
    assert!(matches!(app.screen(), Screen::Attachments(_)));
    act(&mut app, Action::Exit);
    assert_eq!(app.screen(), &Screen::Index);
}

#[test]
fn opens_other_attachments_with_the_system() {
    let mut app = menu();
    act(&mut app, Action::NextEntry);
    let (id, _) = sent(&act(&mut app, Action::ViewAttach));
    let effects = respond(&mut app, id, Event::AttachmentFetched(Ok(b"%PDF".to_vec())));
    assert_eq!(
        effects,
        [Effect::OpenFile {
            name: "report.pdf".into(),
            data: b"%PDF".to_vec(),
        }]
    );
    // A link has nothing to fetch.
    act(&mut app, Action::NextEntry);
    assert!(act(&mut app, Action::ViewAttach).is_empty());
    assert!(app.status.contains("link"), "{}", app.status);
}

#[test]
fn saves_attachments_asking_before_replacing_a_file() {
    let mut app = menu();
    act(&mut app, Action::SaveEntry);
    let Some(Input::Line { editor, .. }) = &app.input else {
        panic!("expected a question");
    };
    assert_eq!(editor.text(), "notes.txt");
    // `~` is the home directory.
    app.input = None;
    let (id, _) = sent(&app.answer_save_attachment("~/notes.txt".into()));
    let effects = respond(
        &mut app,
        id,
        Event::AttachmentFetched(Ok(b"hello".to_vec())),
    );
    let path = PathBuf::from("/home/alice/notes.txt");
    assert_eq!(
        effects,
        [Effect::SaveFile {
            op: FileOp(1),
            path: path.clone(),
            data: b"hello".to_vec(),
            overwrite: false,
        }]
    );
    app.update(Msg::FileSaved(
        FileOp(1),
        FileOutcome::Exists(path.clone(), b"hello".to_vec()),
    ));
    let effects = key(&mut app, KeyCode::Char('y'));
    assert_eq!(
        effects,
        [Effect::SaveFile {
            op: FileOp(2),
            path: path.clone(),
            data: b"hello".to_vec(),
            overwrite: true,
        }]
    );
    app.update(Msg::FileSaved(FileOp(2), FileOutcome::Done(path)));
    assert_eq!(app.status, "Saved to /home/alice/notes.txt.");
}

#[test]
fn attachments_listed_under_the_help_screen_still_show() {
    let mut app = app();
    let (id, _) = sent(&act(&mut app, Action::ViewAttachments));
    // Keys wait for the listing.
    assert!(app.waiting());
    app.screens.push(Screen::Help(crate::app::Help {
        menu: crate::keymap::Menu::Attach,
        scroll: 0,
    }));
    respond(&mut app, id, Event::AttachmentsListed(Ok(vec![notes()])));
    assert!(!app.waiting());
    app.screens.pop();
    let Screen::Attachments(menu) = app.screen() else {
        panic!("expected the menu");
    };
    assert_eq!(menu.entries.len(), 1);
}

#[test]
fn a_message_without_attachments_says_so() {
    let mut app = app();
    let (id, _) = sent(&act(&mut app, Action::ViewAttachments));
    respond(&mut app, id, Event::AttachmentsListed(Ok(Vec::new())));
    assert_eq!(app.screen(), &Screen::Index);
    assert_eq!(app.status, "This message has no attachments.");
}

#[test]
fn attaches_and_detaches_files_in_the_compose_menu() {
    let mut app = app();
    app.screens.push(Screen::Compose(super::super::Compose::new(
        mett_core::Draft::default(),
        false,
    )));
    act(&mut app, Action::AttachFile);
    app.input = None;
    let effects = app.answer_attach_file("~/report.pdf".into());
    let path = PathBuf::from("/home/alice/report.pdf");
    assert_eq!(effects, [Effect::ReadFile(FileOp(1), path.clone())]);
    // Keys wait for it.
    assert!(key(&mut app, KeyCode::Char('q')).is_empty());
    assert!(matches!(app.screen(), Screen::Compose(_)));
    assert!(app.status.starts_with("Waiting for /home/alice/report.pdf"));
    app.update(Msg::FileRead(FileOp(1), path, Ok(b"%PDF".to_vec())));
    let Screen::Compose(compose) = app.screen() else {
        panic!("expected the compose menu");
    };
    assert_eq!(compose.draft.attachments[0].name, "report.pdf");
    assert_eq!(compose.draft.attachments[0].content_type, "application/pdf");
    assert_eq!(compose.selected, 1);
    // The text can't be detached; the file can.
    act(&mut app, Action::PreviousEntry);
    act(&mut app, Action::DetachFile);
    assert_eq!(app.status, "Only attached files can be detached.");
    act(&mut app, Action::NextEntry);
    act(&mut app, Action::DetachFile);
    let Screen::Compose(compose) = app.screen() else {
        panic!("expected the compose menu");
    };
    assert!(compose.draft.attachments.is_empty());
    // A file that can't be read is said.
    app.answer_attach_file("/nope".into());
    app.update(Msg::FileRead(
        FileOp(2),
        PathBuf::from("/nope"),
        Err("not found".into()),
    ));
    assert_eq!(app.status, "/nope: not found");
    // One given up on (^G) is left out, when it comes after all.
    app.answer_attach_file("/slow".into());
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('g'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(app.status, "Not waiting for /slow any more.");
    // Asked for again: only the new reading counts.
    app.answer_attach_file("/slow".into());
    app.update(Msg::FileRead(
        FileOp(3),
        PathBuf::from("/slow"),
        Ok(b"old".to_vec()),
    ));
    let Screen::Compose(compose) = app.screen() else {
        panic!("expected the compose menu");
    };
    assert!(compose.draft.attachments.is_empty());
    app.update(Msg::FileRead(
        FileOp(4),
        PathBuf::from("/slow"),
        Ok(b"new".to_vec()),
    ));
    let Screen::Compose(compose) = app.screen() else {
        panic!("expected the compose menu");
    };
    assert_eq!(compose.draft.attachments[0].data, b"new");
}
