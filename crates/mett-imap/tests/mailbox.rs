//! `ImapMailStore` against an IMAP server in this process.

mod support;

use std::sync::Mutex;

use mett_core::{
    AccessToken, Address, Authenticator, Draft, DraftOrigin, Error, FlagsUpdate, FolderId,
    MailSender, MailStore, MessageId, OutgoingAttachment, Page, WellKnownFolder,
};
use mett_imap::{ImapMailStore, ImapSettings, Login, NoTokens, Security};
use mett_smtp::{Login as SmtpLogin, Security as SmtpSecurity, SmtpSender, SmtpSettings};
use support::smtp::smtp;
use support::{Accept, Folder, Mode, Server, server};

const ME: &str = "me@example.com";

fn folders() -> Vec<Folder> {
    vec![
        Folder::new("INBOX", &["\\HasChildren"]),
        Folder::new("INBOX/Work", &["\\HasNoChildren"]),
        Folder::new("Sent", &["\\Sent"]),
        Folder::new("Drafts", &["\\Drafts"]),
        Folder::new("Trash", &[]),
        Folder::new("Archive", &["\\Archive"]),
        Folder::new("Entw&APw-rfe", &[]),
    ]
}

fn password() -> Accept {
    Accept::Password("me", "secret")
}

fn settings(server: &Server, security: Security) -> ImapSettings {
    let mut settings = ImapSettings::new("127.0.0.1", security);
    settings.port = server.port;
    settings.ca_certificates = vec![server.ca.clone()];
    settings
}

fn login() -> Login {
    Login::Password {
        user: "me".into(),
        password: "secret".into(),
    }
}

async fn connected(mode: Mode, security: Security) -> (Server, ImapMailStore) {
    let server = server(mode, password(), folders()).await;
    let store = ImapMailStore::new(settings(&server, security), login(), Address::new(ME));
    (server, store)
}

async fn plain() -> (Server, ImapMailStore) {
    connected(Mode::Plain, Security::None).await
}

fn mail(subject: &str, from: &str, to: &str, extra: &str, body: &str) -> String {
    format!(
        "From: {from}\nTo: {to}\nSubject: {subject}\nDate: Tue, 20 Oct 2026 09:00:00 +0000\n{extra}\n{body}\n"
    )
}

fn simple(subject: &str) -> String {
    mail(
        subject,
        "Alice <alice@example.org>",
        "me@example.com",
        "Message-ID: <m-1@example.org>\n",
        "Hello\n",
    )
}

fn deliver(server: &Server, folder: &str, raw: &str, flags: &[&str]) -> u32 {
    server.state.lock().unwrap().deliver(folder, raw, flags)
}

#[tokio::test]
async fn lists_folders_with_their_roles_counts_and_nesting() {
    let (server, store) = plain().await;
    deliver(&server, "INBOX", &simple("one"), &[]);
    deliver(&server, "INBOX", &simple("two"), &["\\Seen"]);
    let folders = store.list_folders().await.unwrap();
    let by = |name: &str| {
        folders
            .iter()
            .find(|f| f.id.as_str() == name)
            .unwrap()
            .clone()
    };

    let inbox = by("INBOX");
    assert_eq!(
        (inbox.total_count, inbox.unread_count, inbox.well_known),
        (2, 1, Some(WellKnownFolder::Inbox))
    );
    let work = by("INBOX/Work");
    assert_eq!(work.parent, Some(FolderId::new("INBOX")));
    assert_eq!(work.name, "Work");
    assert_eq!(by("Sent").well_known, Some(WellKnownFolder::SentItems));
    assert_eq!(by("Drafts").well_known, Some(WellKnownFolder::Drafts));
    // No attribute, but the usual name.
    assert_eq!(by("Trash").well_known, Some(WellKnownFolder::DeletedItems));
    assert_eq!(by("Archive").well_known, Some(WellKnownFolder::Archive));
    // Names with accents travel as modified UTF-7.
    assert_eq!(by("Entwürfe").name, "Entwürfe");
}

#[tokio::test]
async fn lists_the_folders_below_one() {
    let (_server, store) = plain().await;
    let top = store.child_folders(None).await.unwrap();
    let names: Vec<_> = top.iter().map(|f| f.id.as_str()).collect();
    assert!(names.contains(&"INBOX") && names.contains(&"Sent"));
    assert!(!names.contains(&"INBOX/Work"));
    let below = store
        .child_folders(Some(&FolderId::new("INBOX")))
        .await
        .unwrap();
    assert_eq!(below.len(), 1);
    assert_eq!(below[0].name, "Work");
}

#[tokio::test]
async fn finds_a_folder_by_role_and_by_id() {
    let (_server, store) = plain().await;
    let sent = store
        .well_known_folder(WellKnownFolder::SentItems)
        .await
        .unwrap();
    assert_eq!(sent.id.as_str(), "Sent");
    let trash = store
        .well_known_folder(WellKnownFolder::DeletedItems)
        .await
        .unwrap();
    assert_eq!(trash.id.as_str(), "Trash");
    let work = store.folder(&FolderId::new("INBOX/Work")).await.unwrap();
    assert_eq!(work.parent, Some(FolderId::new("INBOX")));
    assert!(matches!(
        store.well_known_folder(WellKnownFolder::Outbox).await,
        Err(Error::NotFound { .. })
    ));
    assert!(matches!(
        store.folder(&FolderId::new("Nowhere")).await,
        Err(Error::NotFound { .. })
    ));
}

#[tokio::test]
async fn lists_messages_newest_first_a_page_at_a_time() {
    let (server, store) = plain().await;
    for n in 1..=5 {
        deliver(&server, "INBOX", &simple(&format!("message {n}")), &[]);
    }
    let inbox = FolderId::new("INBOX");
    let first = store.list_messages(&inbox, Page::first(2)).await.unwrap();
    let subjects = |page: &mett_core::MessagePage| {
        page.messages
            .iter()
            .map(|m| m.subject.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(subjects(&first), ["message 5", "message 4"]);
    let second = store
        .list_messages(&inbox, Page::after(first.next.clone().unwrap(), 2))
        .await
        .unwrap();
    assert_eq!(subjects(&second), ["message 3", "message 2"]);
    let last = store
        .list_messages(&inbox, Page::after(second.next.clone().unwrap(), 2))
        .await
        .unwrap();
    assert_eq!(subjects(&last), ["message 1"]);
    assert!(last.next.is_none());
}

#[tokio::test]
async fn summarises_a_message() {
    let (server, store) = plain().await;
    let raw = mail(
        "Plans",
        "Alice <alice@example.org>",
        "me@example.com, Bob <bob@example.org>",
        "Message-ID: <b@example.org>\nReferences: <a@example.org>\nIn-Reply-To: <a@example.org>\nContent-Type: multipart/mixed; boundary=x\n",
        "--x\nContent-Type: text/plain\n\nhi\n--x--",
    );
    deliver(&server, "INBOX", &raw, &["\\Seen", "\\Flagged"]);
    let page = store
        .list_messages(&FolderId::new("INBOX"), Page::first(10))
        .await
        .unwrap();
    let m = &page.messages[0];
    assert_eq!(m.subject, "Plans");
    assert_eq!(m.from.as_ref().unwrap().email, "alice@example.org");
    assert_eq!(m.from.as_ref().unwrap().name.as_deref(), Some("Alice"));
    assert_eq!(m.to.len(), 2);
    assert!(m.flags.read && m.flags.flagged);
    assert!(m.has_attachments);
    assert_eq!(m.received.to_rfc3339(), "2026-10-20T09:00:00+00:00");
    assert_eq!(
        m.delivered.unwrap().to_rfc3339(),
        "2026-10-20T10:00:00+00:00"
    );
    let thread = m.thread.as_ref().unwrap();
    assert_eq!(thread.conversation, "<a@example.org>");
    assert_eq!(
        thread.path,
        [b"<a@example.org>".to_vec(), b"<b@example.org>".to_vec()]
    );
}

#[tokio::test]
async fn fetches_a_message_with_its_body_and_attachments() {
    let (server, store) = plain().await;
    let raw = mail(
        "Report",
        "Alice <alice@example.org>",
        "me@example.com",
        "Cc: Carol <carol@example.org>\nReply-To: team@example.org\nMessage-ID: <r@example.org>\nContent-Type: multipart/mixed; boundary=b\n",
        "--b\nContent-Type: text/plain; charset=utf-8\n\nSee attached.\n--b\nContent-Type: text/csv; name=\"data.csv\"\nContent-Disposition: attachment; filename=\"data.csv\"\nContent-Transfer-Encoding: base64\n\nYSxiCjEsMgo=\n--b--",
    );
    deliver(&server, "INBOX", &raw, &[]);
    let page = store
        .list_messages(&FolderId::new("INBOX"), Page::first(10))
        .await
        .unwrap();
    let id = page.messages[0].id.clone();
    let message = store.fetch_message(&id).await.unwrap();
    assert!(message.body.content.contains("See attached."));
    assert_eq!(message.cc[0].email, "carol@example.org");
    assert_eq!(message.reply_to[0].email, "team@example.org");
    assert_eq!(
        message.internet_message_id.as_deref(),
        Some("<r@example.org>")
    );
    assert!(message.summary.has_attachments);

    let attachments = store.list_attachments(&id).await.unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].name, "data.csv");
    assert_eq!(attachments[0].content_type, "text/csv");
    let data = store
        .fetch_attachment(&id, &attachments[0].id)
        .await
        .unwrap();
    assert_eq!(data, b"a,b\n1,2\n");
}

#[tokio::test]
async fn reads_a_message_that_is_only_html_as_text() {
    let (server, store) = plain().await;
    let raw = mail(
        "Newsletter",
        "news@example.org",
        "me@example.com",
        "Content-Type: text/html; charset=utf-8\n",
        "<html><body><p>Hello <b>there</b></p></body></html>",
    );
    deliver(&server, "INBOX", &raw, &[]);
    let page = store
        .list_messages(&FolderId::new("INBOX"), Page::first(1))
        .await
        .unwrap();
    let message = store.fetch_message(&page.messages[0].id).await.unwrap();
    assert!(message.body.content.contains("Hello"));
    assert!(!message.body.content.contains("<b>"));
}

#[tokio::test]
async fn changes_flags() {
    let (server, store) = plain().await;
    deliver(&server, "INBOX", &simple("flags"), &[]);
    let inbox = FolderId::new("INBOX");
    let id = store
        .list_messages(&inbox, Page::first(1))
        .await
        .unwrap()
        .messages[0]
        .id
        .clone();
    store
        .update_flags(&id, FlagsUpdate::read(true))
        .await
        .unwrap();
    store
        .update_flags(&id, FlagsUpdate::flagged(true))
        .await
        .unwrap();
    let m = &store
        .list_messages(&inbox, Page::first(1))
        .await
        .unwrap()
        .messages[0];
    assert!(m.flags.read && m.flags.flagged);
    store
        .update_flags(&id, FlagsUpdate::read(false))
        .await
        .unwrap();
    let m = &store
        .list_messages(&inbox, Page::first(1))
        .await
        .unwrap()
        .messages[0];
    assert!(!m.flags.read && m.flags.flagged);
}

#[tokio::test]
async fn moves_copies_and_deletes() {
    let (server, store) = plain().await;
    deliver(&server, "INBOX", &simple("travelling"), &[]);
    let inbox = FolderId::new("INBOX");
    let id = store
        .list_messages(&inbox, Page::first(1))
        .await
        .unwrap()
        .messages[0]
        .id
        .clone();

    let copy = store
        .copy_message(&id, &FolderId::new("Archive"))
        .await
        .unwrap();
    assert!(copy.as_str().ends_with(".Archive"));
    assert_eq!(
        store.fetch_message(&copy).await.unwrap().summary.subject,
        "travelling"
    );
    // The original is still there.
    store.fetch_message(&id).await.unwrap();

    let moved = store
        .move_message(&id, &FolderId::new("Trash"))
        .await
        .unwrap();
    assert!(moved.as_str().ends_with(".Trash"));
    assert!(matches!(
        store.fetch_message(&id).await,
        Err(Error::NotFound { .. })
    ));
    assert_eq!(
        store
            .fetch_message(&moved)
            .await
            .unwrap()
            .summary
            .folder
            .as_str(),
        "Trash"
    );

    store.delete_message(&moved).await.unwrap();
    assert!(
        server
            .state
            .lock()
            .unwrap()
            .folder("Trash")
            .messages
            .is_empty()
    );
    // Deleting the one copy left the other alone.
    assert_eq!(
        server
            .state
            .lock()
            .unwrap()
            .folder("Archive")
            .messages
            .len(),
        1
    );
}

#[tokio::test]
async fn a_message_of_a_folder_made_anew_is_gone() {
    let (server, store) = plain().await;
    deliver(&server, "INBOX", &simple("old"), &[]);
    let inbox = FolderId::new("INBOX");
    let id = store
        .list_messages(&inbox, Page::first(1))
        .await
        .unwrap()
        .messages[0]
        .id
        .clone();
    server.state.lock().unwrap().folder("INBOX").validity += 1;
    assert!(matches!(
        store.fetch_message(&id).await,
        Err(Error::NotFound { .. })
    ));
    assert!(matches!(
        store.fetch_message(&MessageId::new("not an id")).await,
        Err(Error::NotFound { .. })
    ));
}

#[tokio::test]
async fn finds_new_unread_mail_and_what_came_in_since() {
    let (server, store) = plain().await;
    deliver(&server, "INBOX", &simple("seen"), &["\\Seen"]);
    deliver(&server, "INBOX", &simple("unread"), &[]);
    let inbox = FolderId::new("INBOX");
    let unread = store.recent_unread(&inbox, None).await.unwrap();
    assert_eq!(unread.len(), 1);
    let at = unread[0].1;
    assert_eq!(at.to_rfc3339(), "2026-10-20T10:00:00+00:00");

    // Mail that came in before `since` isn't new.
    let later = at + chrono::TimeDelta::hours(1);
    assert!(
        store
            .recent_unread(&inbox, Some(later))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.recent_unread(&inbox, Some(at)).await.unwrap().len(),
        1
    );

    let changed = store
        .changed_messages(&inbox, at, Page::first(10))
        .await
        .unwrap();
    assert_eq!(changed.messages.len(), 2);
    let changed = store
        .changed_messages(&inbox, later, Page::first(10))
        .await
        .unwrap();
    assert!(changed.messages.is_empty());
}

#[tokio::test]
async fn saves_a_draft_and_replaces_one_recalled() {
    let (server, store) = plain().await;
    let draft = Draft {
        to: vec![Address::new("alice@example.org")],
        subject: "Later".into(),
        body: "First try\n".into(),
        ..Draft::default()
    };
    let id = store.save_draft(&draft).await.unwrap();
    {
        let mut state = server.state.lock().unwrap();
        let drafts = state.folder("Drafts");
        assert_eq!(drafts.messages.len(), 1);
        assert!(drafts.messages[0].flags.contains("\\Draft"));
        let raw = String::from_utf8_lossy(&drafts.messages[0].raw).to_string();
        assert!(raw.contains("Subject: Later"));
    }
    assert_eq!(
        store.fetch_message(&id).await.unwrap().summary.subject,
        "Later"
    );

    let again = Draft {
        body: "Second try\n".into(),
        origin: Some(DraftOrigin::Recalled(id.clone())),
        ..draft
    };
    let id2 = store.save_draft(&again).await.unwrap();
    assert_eq!(
        server.state.lock().unwrap().folder("Drafts").messages.len(),
        1
    );
    assert_ne!(id, id2);
    assert!(
        store
            .fetch_message(&id2)
            .await
            .unwrap()
            .body
            .content
            .contains("Second try")
    );
}

async fn mailer(server: &Server) -> (ImapMailStore, support::smtp::Smtp) {
    let out = smtp().await;
    let mut settings = SmtpSettings::new("127.0.0.1", SmtpSecurity::None, Address::new(ME));
    settings.port = out.port;
    let sender = SmtpSender::new(settings, SmtpLogin::None);
    let store = ImapMailStore::new(
        self::settings(server, Security::None),
        login(),
        Address::new(ME),
    )
    .with_smtp(sender);
    (store, out)
}

#[tokio::test]
async fn sends_mail_and_keeps_a_copy_in_sent() {
    let server = server(Mode::Plain, password(), folders()).await;
    let (store, out) = mailer(&server).await;
    let draft = Draft {
        to: vec![Address::new("alice@example.org")],
        bcc: vec![Address::new("secret@example.org")],
        subject: "Hi".into(),
        body: "Hello\n".into(),
        save_copy: true,
        ..Draft::default()
    };
    store.send(&draft).await.unwrap();

    let taken = out.taken.lock().unwrap();
    assert_eq!(taken[0].to, ["alice@example.org", "secret@example.org"]);
    assert!(!taken[0].data.contains("secret@example.org"));
    drop(taken);
    let mut state = server.state.lock().unwrap();
    let sent = state.folder("Sent");
    assert_eq!(sent.messages.len(), 1);
    assert!(sent.messages[0].flags.contains("\\Seen"));
    // The copy has the blind copy, as it should.
    assert!(String::from_utf8_lossy(&sent.messages[0].raw).contains("Bcc: secret@example.org"));
}

#[tokio::test]
async fn sends_without_a_copy_when_not_asked_for_one() {
    let server = server(Mode::Plain, password(), folders()).await;
    let (store, _out) = mailer(&server).await;
    let draft = Draft {
        to: vec![Address::new("alice@example.org")],
        subject: "Hi".into(),
        body: "Hello\n".into(),
        ..Draft::default()
    };
    store.send(&draft).await.unwrap();
    assert!(
        server
            .state
            .lock()
            .unwrap()
            .folder("Sent")
            .messages
            .is_empty()
    );
}

#[tokio::test]
async fn threads_a_reply_and_forwards_the_attachments() {
    let server = server(Mode::Plain, password(), folders()).await;
    let (store, out) = mailer(&server).await;
    let raw = mail(
        "Plans",
        "Alice <alice@example.org>",
        "me@example.com",
        "Message-ID: <b@example.org>\nReferences: <a@example.org>\nContent-Type: multipart/mixed; boundary=b\n",
        "--b\nContent-Type: text/plain\n\nhi\n--b\nContent-Type: text/plain; name=\"notes.txt\"\nContent-Disposition: attachment; filename=\"notes.txt\"\n\nthe notes\n--b--",
    );
    deliver(&server, "INBOX", &raw, &[]);
    let original = store
        .list_messages(&FolderId::new("INBOX"), Page::first(1))
        .await
        .unwrap()
        .messages[0]
        .id
        .clone();

    let reply = Draft {
        to: vec![Address::new("alice@example.org")],
        subject: "Re: Plans".into(),
        body: "Sure\n".into(),
        origin: Some(DraftOrigin::Reply(original.clone())),
        ..Draft::default()
    };
    store.send(&reply).await.unwrap();
    let forward = Draft {
        to: vec![Address::new("carol@example.org")],
        subject: "Fwd: Plans".into(),
        body: "FYI\n".into(),
        origin: Some(DraftOrigin::Forward(original)),
        attachments: vec![OutgoingAttachment {
            name: "own.txt".into(),
            content_type: "text/plain".into(),
            data: b"mine".to_vec(),
        }],
        ..Draft::default()
    };
    store.send(&forward).await.unwrap();

    let taken = out.taken.lock().unwrap();
    assert!(taken[0].data.contains("In-Reply-To: <b@example.org>"));
    assert!(
        taken[0]
            .data
            .contains("References: <a@example.org> <b@example.org>")
    );
    assert!(taken[1].data.contains("notes.txt"));
    assert!(taken[1].data.contains("own.txt"));
}

#[tokio::test]
async fn sending_without_an_smtp_server_is_not_sent() {
    let (_server, store) = plain().await;
    let draft = Draft {
        to: vec![Address::new("alice@example.org")],
        ..Draft::default()
    };
    assert!(matches!(store.send(&draft).await, Err(Error::NotSent(_))));
}

#[tokio::test]
async fn connects_with_tls_from_the_start_and_with_starttls() {
    for (mode, security) in [
        (Mode::Tls, Security::Tls),
        (Mode::StartTls, Security::StartTls),
    ] {
        let (server, store) = connected(mode, security).await;
        store.list_folders().await.unwrap();
        assert_eq!(server.state.lock().unwrap().encrypted, [true]);
    }
}

#[tokio::test]
async fn upgrades_if_offered_and_goes_on_in_plain_text_if_not() {
    let (server, store) = connected(Mode::StartTls, Security::StartTlsIfOffered).await;
    store.list_folders().await.unwrap();
    assert_eq!(server.state.lock().unwrap().encrypted, [true]);

    let (server, store) = connected(Mode::Plain, Security::StartTlsIfOffered).await;
    store.list_folders().await.unwrap();
    assert_eq!(server.state.lock().unwrap().encrypted, [false]);

    // A server that can't, when STARTTLS is required.
    let (_server, store) = connected(Mode::Plain, Security::StartTls).await;
    let error = store.list_folders().await.unwrap_err();
    assert!(error.to_string().contains("STARTTLS"), "{error}");
}

#[tokio::test]
async fn does_not_trust_a_certificate_it_is_not_given() {
    let server = server(Mode::Tls, password(), folders()).await;
    let mut settings = settings(&server, Security::Tls);
    settings.ca_certificates.clear();
    let store = ImapMailStore::new(settings, login(), Address::new(ME));
    assert!(store.list_folders().await.is_err());
    assert!(server.state.lock().unwrap().logins.is_empty());

    let mut settings = self::settings(&server, Security::Tls);
    settings.ca_certificates.clear();
    settings.verify = false;
    let store = ImapMailStore::new(settings, login(), Address::new(ME));
    store.list_folders().await.unwrap();
}

#[tokio::test]
async fn a_wrong_password_is_an_authentication_error() {
    let server = server(Mode::Plain, password(), folders()).await;
    let login = Login::Password {
        user: "me".into(),
        password: "wrong".into(),
    };
    let store: ImapMailStore<NoTokens> =
        ImapMailStore::new(settings(&server, Security::None), login, Address::new(ME));
    let error = store.list_folders().await.unwrap_err();
    assert!(matches!(error, Error::Auth(_)), "{error}");
}

/// Tokens that change: what the server turns down, then what it takes.
struct Tokens(Mutex<Vec<&'static str>>, Mutex<Vec<String>>);

impl Authenticator for Tokens {
    async fn access_token(&self) -> mett_core::Result<AccessToken> {
        let mut tokens = self.0.lock().unwrap();
        let token = if tokens.len() > 1 {
            tokens.remove(0)
        } else {
            tokens[0]
        };
        Ok(AccessToken::new(
            token,
            chrono::DateTime::<chrono::Utc>::MAX_UTC,
        ))
    }

    async fn token_rejected(&self, token: &AccessToken) {
        self.1.lock().unwrap().push(token.secret().to_owned());
    }
}

#[tokio::test]
async fn logs_in_with_a_token_and_asks_for_another_when_refused() {
    let server = server(Mode::Plain, Accept::Token("good"), folders()).await;
    let tokens = Tokens(Mutex::new(vec!["stale", "good"]), Mutex::default());
    let login = Login::OAuth {
        user: "me@example.com".into(),
        tokens,
    };
    let store = ImapMailStore::new(settings(&server, Security::None), login, Address::new(ME));
    store.list_folders().await.unwrap();
    let state = server.state.lock().unwrap();
    assert_eq!(state.refused, 1);
    assert!(state.logins[1].contains("auth=Bearer good"));
}

#[tokio::test]
async fn the_account_is_the_user_at_the_server() {
    let (_server, store) = plain().await;
    assert_eq!(store.account().as_str(), "me@127.0.0.1");
    assert_eq!(store.own_addresses().await.unwrap(), [ME]);
}

#[tokio::test]
async fn a_saved_draft_is_listed_as_a_draft_so_it_can_be_recalled() {
    let (_server, store) = plain().await;
    let draft = Draft {
        to: vec![Address::new("alice@example.org")],
        subject: "Unfinished".into(),
        ..Draft::default()
    };
    store.save_draft(&draft).await.unwrap();
    let page = store
        .list_messages(&FolderId::new("Drafts"), Page::first(10))
        .await
        .unwrap();
    assert!(page.messages[0].draft);
}

#[tokio::test]
async fn a_draft_without_recipients_can_be_postponed() {
    let (server, store) = plain().await;
    let draft = Draft {
        subject: "Whom to?".into(),
        body: "Notes\n".into(),
        ..Draft::default()
    };
    store.save_draft(&draft).await.unwrap();
    assert_eq!(
        server.state.lock().unwrap().folder("Drafts").messages.len(),
        1
    );
}

#[tokio::test]
async fn will_not_delete_one_message_without_uidplus() {
    let (server, store) = plain().await;
    server.state.lock().unwrap().no_uidplus = true;
    let ours = deliver(&server, "INBOX", &simple("ours"), &[]);
    let theirs = deliver(&server, "INBOX", &simple("theirs"), &["\\Deleted"]);
    let inbox = FolderId::new("INBOX");
    let id = store
        .list_messages(&inbox, Page::first(5))
        .await
        .unwrap()
        .messages[1]
        .id
        .clone();
    // Expunging would take the other client's message as well.
    let error = store.delete_message(&id).await.unwrap_err();
    assert!(error.to_string().contains("UIDPLUS"), "{error}");
    let mut state = server.state.lock().unwrap();
    let kept: Vec<u32> = state
        .folder("INBOX")
        .messages
        .iter()
        .map(|m| m.uid)
        .collect();
    assert_eq!(kept, [ours, theirs]);
    assert!(
        !state.folder("INBOX").messages[0]
            .flags
            .contains("\\Deleted")
    );
}

#[tokio::test]
async fn moves_without_move_or_uidplus_fail_before_copying() {
    let (server, store) = plain().await;
    {
        let mut state = server.state.lock().unwrap();
        state.no_uidplus = true;
        state.no_move = true;
    }
    deliver(&server, "INBOX", &simple("stays"), &[]);
    let inbox = FolderId::new("INBOX");
    let id = store
        .list_messages(&inbox, Page::first(1))
        .await
        .unwrap()
        .messages[0]
        .id
        .clone();
    assert!(
        store
            .move_message(&id, &FolderId::new("Trash"))
            .await
            .is_err()
    );
    assert!(
        server
            .state
            .lock()
            .unwrap()
            .folder("Trash")
            .messages
            .is_empty()
    );
    // A copy leaves the original, so it works.
    store
        .copy_message(&id, &FolderId::new("Trash"))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_message_without_a_message_id_is_found_where_it_was_moved() {
    let (server, store) = plain().await;
    let raw = "From: a@example.org\nTo: me@example.com\nSubject: anonymous\nDate: Tue, 20 Oct 2026 09:00:00 +0000\n\nbody\n";
    deliver(&server, "INBOX", raw, &[]);
    // Something else is already in the destination.
    deliver(&server, "Archive", &simple("resident"), &[]);
    let inbox = FolderId::new("INBOX");
    let id = store
        .list_messages(&inbox, Page::first(1))
        .await
        .unwrap()
        .messages[0]
        .id
        .clone();
    let copy = store
        .copy_message(&id, &FolderId::new("Archive"))
        .await
        .unwrap();
    assert_eq!(
        store.fetch_message(&copy).await.unwrap().summary.subject,
        "anonymous"
    );
}

#[tokio::test]
async fn all_mail_is_not_the_archive() {
    let server = server(
        Mode::Plain,
        password(),
        vec![
            Folder::new("INBOX", &[]),
            Folder::new("All Mail", &["\\All"]),
            Folder::new("Filed", &["\\Archive"]),
        ],
    )
    .await;
    let store = ImapMailStore::new(settings(&server, Security::None), login(), Address::new(ME));
    let archive = store
        .well_known_folder(WellKnownFolder::Archive)
        .await
        .unwrap();
    assert_eq!(archive.id.as_str(), "Filed");
}

#[tokio::test]
async fn mail_that_arrives_with_an_old_date_is_still_new() {
    let (server, store) = plain().await;
    deliver(&server, "INBOX", &simple("old"), &[]);
    let inbox = FolderId::new("INBOX");
    // The user looked at the folder after everything in it.
    let since = chrono::DateTime::parse_from_rfc3339("2026-10-22T00:00:00Z")
        .unwrap()
        .to_utc();
    assert!(
        store
            .changed_messages(&inbox, since, Page::first(10))
            .await
            .unwrap()
            .messages
            .is_empty()
    );
    assert!(
        store
            .recent_unread(&inbox, Some(since))
            .await
            .unwrap()
            .is_empty()
    );

    // A message that was copied in keeps the date it was first delivered.
    deliver(&server, "INBOX", &simple("copied in"), &[]);
    let changed = store
        .changed_messages(&inbox, since, Page::first(10))
        .await
        .unwrap();
    assert_eq!(changed.messages.len(), 1);
    assert_eq!(changed.messages[0].subject, "copied in");
    assert_eq!(
        store
            .recent_unread(&inbox, Some(since))
            .await
            .unwrap()
            .len(),
        1
    );

    // Once seen, it isn't new again.
    let changed = store
        .changed_messages(&inbox, since, Page::first(10))
        .await
        .unwrap();
    assert!(changed.messages.is_empty());
    assert!(changed.next.is_none());
}

#[tokio::test]
async fn nothing_changed_is_an_empty_page_not_the_whole_folder() {
    let (server, store) = plain().await;
    for n in 0..30 {
        deliver(&server, "INBOX", &simple(&format!("old {n}")), &[]);
    }
    let since = chrono::DateTime::parse_from_rfc3339("2026-10-22T00:00:00Z")
        .unwrap()
        .to_utc();
    let page = store
        .changed_messages(&FolderId::new("INBOX"), since, Page::first(5))
        .await
        .unwrap();
    assert!(page.messages.is_empty() && page.next.is_none());
}

#[tokio::test]
async fn the_folder_the_server_calls_sent_wins_over_one_called_sent() {
    let server = server(
        Mode::Plain,
        password(),
        vec![
            Folder::new("INBOX", &[]),
            Folder::new("Sent", &[]),
            Folder::new("Mail Out", &["\\Sent"]),
        ],
    )
    .await;
    let store = ImapMailStore::new(settings(&server, Security::None), login(), Address::new(ME));
    let sent = store
        .well_known_folder(WellKnownFolder::SentItems)
        .await
        .unwrap();
    assert_eq!(sent.id.as_str(), "Mail Out");
    let folders = store.list_folders().await.unwrap();
    let ordinary = folders.iter().find(|f| f.id.as_str() == "Sent").unwrap();
    assert_eq!(ordinary.well_known, None);
    let by_id = store.folder(&FolderId::new("Sent")).await.unwrap();
    assert_eq!(by_id.well_known, None);
}

#[tokio::test]
async fn a_message_that_has_gone_is_not_found() {
    let (server, store) = plain().await;
    deliver(&server, "INBOX", &simple("going"), &[]);
    let inbox = FolderId::new("INBOX");
    let id = store
        .list_messages(&inbox, Page::first(1))
        .await
        .unwrap()
        .messages[0]
        .id
        .clone();
    server
        .state
        .lock()
        .unwrap()
        .folder("INBOX")
        .messages
        .clear();
    for result in [
        store
            .update_flags(&id, FlagsUpdate::read(true))
            .await
            .map(drop),
        store.delete_message(&id).await,
        store
            .move_message(&id, &FolderId::new("Trash"))
            .await
            .map(drop),
        store
            .copy_message(&id, &FolderId::new("Trash"))
            .await
            .map(drop),
    ] {
        assert!(matches!(result, Err(Error::NotFound { .. })), "{result:?}");
    }
    assert!(
        server
            .state
            .lock()
            .unwrap()
            .folder("Trash")
            .messages
            .is_empty()
    );
}

fn dated(subject: &str, date: &str) -> String {
    format!(
        "From: a@example.org\nTo: me@example.com\nSubject: {subject}\nDate: {date}\nMessage-ID: <{subject}@example.org>\n\nbody\n"
    )
}

#[tokio::test]
async fn lists_messages_by_the_dates_they_say_where_the_server_can_sort() {
    let (server, store) = plain().await;
    deliver(
        &server,
        "INBOX",
        &dated("middle", "Tue, 20 Oct 2026 09:00:00 +0000"),
        &[],
    );
    deliver(
        &server,
        "INBOX",
        &dated("newest", "Wed, 21 Oct 2026 09:00:00 +0000"),
        &[],
    );
    // Arrived last, with an old date: the UID is the highest, the date the lowest.
    deliver(
        &server,
        "INBOX",
        &dated("oldest", "Thu, 01 Oct 2026 09:00:00 +0000"),
        &[],
    );
    let inbox = FolderId::new("INBOX");
    let subjects = |page: &mett_core::MessagePage| {
        page.messages
            .iter()
            .map(|m| m.subject.clone())
            .collect::<Vec<_>>()
    };
    let first = store.list_messages(&inbox, Page::first(2)).await.unwrap();
    assert_eq!(subjects(&first), ["newest", "middle"]);
    let second = store
        .list_messages(&inbox, Page::after(first.next.clone().unwrap(), 2))
        .await
        .unwrap();
    assert_eq!(subjects(&second), ["oldest"]);
    assert!(second.next.is_none());

    // The order within a page is the dates', not the UIDs'.
    let all = store.list_messages(&inbox, Page::first(3)).await.unwrap();
    assert_eq!(subjects(&all), ["newest", "middle", "oldest"]);

    // Where it can't, by UID.
    server.state.lock().unwrap().no_sort = true;
    let (_, unsorted) = {
        let settings = settings(&server, Security::None);
        ((), ImapMailStore::new(settings, login(), Address::new(ME)))
    };
    let page = unsorted
        .list_messages(&inbox, Page::first(3))
        .await
        .unwrap();
    assert_eq!(subjects(&page), ["oldest", "newest", "middle"]);
}

#[tokio::test]
async fn sees_flags_another_client_changed_where_the_server_tracks_changes() {
    let (server, store) = plain().await;
    let read = deliver(&server, "INBOX", &simple("read"), &["\\Seen"]);
    let other = deliver(&server, "INBOX", &simple("other"), &["\\Seen"]);
    let inbox = FolderId::new("INBOX");
    let since = chrono::DateTime::parse_from_rfc3339("2026-10-22T00:00:00Z")
        .unwrap()
        .to_utc();
    // The first look only marks where the folder stands.
    assert!(
        store
            .changed_messages(&inbox, since, Page::first(10))
            .await
            .unwrap()
            .messages
            .is_empty()
    );
    assert!(
        store
            .recent_unread(&inbox, Some(since))
            .await
            .unwrap()
            .is_empty()
    );

    // Another client marks an old message unread and another flagged.
    {
        let mut state = server.state.lock().unwrap();
        state.set_flags("INBOX", read, &[]);
        state.set_flags("INBOX", other, &["\\Seen", "\\Flagged"]);
    }
    let changed = store
        .changed_messages(&inbox, since, Page::first(10))
        .await
        .unwrap();
    let mut subjects: Vec<_> = changed.messages.iter().map(|m| m.subject.clone()).collect();
    subjects.sort();
    assert_eq!(subjects, ["other", "read"]);
    let unread = store.recent_unread(&inbox, Some(since)).await.unwrap();
    assert_eq!(unread.len(), 1);
    assert!(unread[0].0.as_str().contains(&format!(".{read}.")));

    // Seen once, they are not changed again.
    assert!(
        store
            .changed_messages(&inbox, since, Page::first(10))
            .await
            .unwrap()
            .messages
            .is_empty()
    );
}

#[tokio::test]
async fn pages_of_changes_keep_the_mark_of_the_first() {
    let (server, store) = plain().await;
    deliver(&server, "INBOX", &simple("first"), &[]);
    let inbox = FolderId::new("INBOX");
    let since = chrono::DateTime::parse_from_rfc3339("2026-10-22T00:00:00Z")
        .unwrap()
        .to_utc();
    store
        .changed_messages(&inbox, since, Page::first(2))
        .await
        .unwrap();
    for n in 0..5 {
        deliver(&server, "INBOX", &simple(&format!("copied {n}")), &[]);
    }
    let mut page = store
        .changed_messages(&inbox, since, Page::first(2))
        .await
        .unwrap();
    let mut all = page.messages.clone();
    while let Some(next) = page.next.clone() {
        page = store
            .changed_messages(&inbox, since, Page::after(next, 2))
            .await
            .unwrap();
        all.extend(page.messages.clone());
    }
    assert_eq!(all.len(), 5);
}

#[tokio::test]
async fn finds_new_mail_without_condstore_by_uid_and_date() {
    let (server, store) = plain().await;
    server.state.lock().unwrap().no_condstore = true;
    let store = {
        let settings = settings(&server, Security::None);
        drop(store);
        ImapMailStore::new(settings, login(), Address::new(ME))
    };
    deliver(&server, "INBOX", &simple("old"), &[]);
    let inbox = FolderId::new("INBOX");
    let since = chrono::DateTime::parse_from_rfc3339("2026-10-22T00:00:00Z")
        .unwrap()
        .to_utc();
    assert!(
        store
            .changed_messages(&inbox, since, Page::first(10))
            .await
            .unwrap()
            .messages
            .is_empty()
    );
    deliver(&server, "INBOX", &simple("copied in"), &[]);
    let changed = store
        .changed_messages(&inbox, since, Page::first(10))
        .await
        .unwrap();
    assert_eq!(changed.messages.len(), 1);
}

#[tokio::test]
async fn the_day_before_is_searched_for_the_zone_the_server_dates_in() {
    let (server, store) = plain().await;
    {
        let mut state = server.state.lock().unwrap();
        // 22 Oct 01:00 UTC, as the server wrote it, a day earlier.
        state.deliver_dated(
            "INBOX",
            &simple("late evening"),
            "21-Oct-2026 18:00:00 -0700",
        );
    }
    let inbox = FolderId::new("INBOX");
    let since = chrono::DateTime::parse_from_rfc3339("2026-10-22T00:30:00Z")
        .unwrap()
        .to_utc();
    let changed = store
        .changed_messages(&inbox, since, Page::first(10))
        .await
        .unwrap();
    assert_eq!(changed.messages.len(), 1);
    let before = since + chrono::TimeDelta::hours(1);
    let changed = store
        .changed_messages(&inbox, before, Page::first(10))
        .await
        .unwrap();
    assert!(changed.messages.is_empty());
}

#[tokio::test]
async fn reads_html_only_mail_in_parts_as_html() {
    let (server, store) = plain().await;
    let raw = mail(
        "Rich",
        "news@example.org",
        "me@example.com",
        "Content-Type: multipart/mixed; boundary=b\n",
        "--b\nContent-Type: text/html; charset=utf-8\n\n<p>Hello <b>there</b></p>\n--b--",
    );
    deliver(&server, "INBOX", &raw, &[]);
    let page = store
        .list_messages(&FolderId::new("INBOX"), Page::first(1))
        .await
        .unwrap();
    let message = store.fetch_message(&page.messages[0].id).await.unwrap();
    assert!(message.body.content.contains("Hello"), "{:?}", message.body);
}

#[tokio::test]
async fn the_folder_of_a_message_is_known_only_while_it_is_there() {
    let (server, store) = plain().await;
    deliver(&server, "INBOX", &simple("here"), &[]);
    let inbox = FolderId::new("INBOX");
    let id = store
        .list_messages(&inbox, Page::first(1))
        .await
        .unwrap()
        .messages[0]
        .id
        .clone();
    assert_eq!(store.message_folder(&id).await.unwrap(), inbox);
    server
        .state
        .lock()
        .unwrap()
        .folder("INBOX")
        .messages
        .clear();
    assert!(matches!(
        store.message_folder(&id).await,
        Err(Error::NotFound { .. })
    ));
}

#[tokio::test]
async fn folder_names_are_exact() {
    let (_server, store) = plain().await;
    assert!(!store.folder_names_ignore_case());
}

#[tokio::test]
async fn a_replaced_draft_is_saved_even_if_the_old_one_cannot_be_deleted() {
    let (server, store) = plain().await;
    server.state.lock().unwrap().no_uidplus = true;
    let draft = Draft {
        to: vec![Address::new("alice@example.org")],
        subject: "Later".into(),
        body: "First\n".into(),
        ..Draft::default()
    };
    let id = store.save_draft(&draft).await.unwrap();
    // Another client marked a draft deleted: the old one can't be expunged alone.
    deliver(&server, "Drafts", &simple("theirs"), &["\\Deleted"]);
    let again = Draft {
        body: "Second\n".into(),
        origin: Some(DraftOrigin::Recalled(id)),
        ..draft
    };
    let saved = store.save_draft(&again).await.unwrap();
    assert!(
        store
            .fetch_message(&saved)
            .await
            .unwrap()
            .body
            .content
            .contains("Second")
    );
}

#[tokio::test]
async fn a_page_goes_on_where_it_left_off_if_the_message_it_ended_at_is_gone() {
    let (server, store) = plain().await;
    for n in 1..=6 {
        deliver(&server, "INBOX", &simple(&format!("message {n}")), &[]);
    }
    let inbox = FolderId::new("INBOX");
    let first = store.list_messages(&inbox, Page::first(2)).await.unwrap();
    assert_eq!(first.messages[1].subject, "message 5");
    // Another client deletes the message the first page ended at.
    server
        .state
        .lock()
        .unwrap()
        .folder("INBOX")
        .messages
        .retain(|m| m.uid != 5);
    let second = store
        .list_messages(&inbox, Page::after(first.next.unwrap(), 2))
        .await
        .unwrap();
    let subjects: Vec<_> = second.messages.iter().map(|m| m.subject.as_str()).collect();
    assert_eq!(subjects, ["message 4", "message 3"]);
}

#[tokio::test]
async fn what_comes_in_between_listing_a_folder_and_the_first_poll_is_new() {
    let (server, store) = plain().await;
    deliver(&server, "INBOX", &simple("listed"), &[]);
    let inbox = FolderId::new("INBOX");
    // The user opens the folder.
    store.list_messages(&inbox, Page::first(10)).await.unwrap();
    // Mail with an old date is copied in, then the first poll comes.
    deliver(&server, "INBOX", &simple("copied in"), &[]);
    let since = chrono::DateTime::parse_from_rfc3339("2026-10-22T00:00:00Z")
        .unwrap()
        .to_utc();
    let changed = store
        .changed_messages(&inbox, since, Page::first(10))
        .await
        .unwrap();
    assert_eq!(changed.messages.len(), 1);
    assert_eq!(changed.messages[0].subject, "copied in");
    // The polls list the newest page again, which doesn't forget what they found.
    store.list_messages(&inbox, Page::first(10)).await.unwrap();
    deliver(&server, "INBOX", &simple("copied in too"), &[]);
    let changed = store
        .changed_messages(&inbox, since, Page::first(10))
        .await
        .unwrap();
    assert_eq!(changed.messages.len(), 1);
    assert_eq!(changed.messages[0].subject, "copied in too");
}
