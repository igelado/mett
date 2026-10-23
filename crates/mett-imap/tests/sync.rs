//! Syncing folders of an IMAP server in this process, alone and with the
//! cache in front.

mod support;

use mett_core::{Address, Error, FlagsUpdate, FolderChanges, FolderId, MailStore, Page, SyncState};
use mett_imap::{ImapMailStore, ImapSettings, Login, Security};
use mett_sqlite::{Cache, CachedStore};
use support::{Accept, Folder, Mode, Server, server};

fn simple(subject: &str) -> String {
    format!(
        "From: alice@example.org\nTo: me@example.com\nSubject: {subject}\nDate: Tue, 20 Oct 2026 09:00:00 +0000\nMessage-ID: <{subject}@example.org>\n\nbody\n"
    )
}

fn folders() -> Vec<Folder> {
    vec![
        Folder::new("INBOX", &[]),
        Folder::new("Archive", &[]),
        Folder::new("Sent", &["\\Sent"]),
    ]
}

fn store_for(server: &Server) -> ImapMailStore {
    let mut settings = ImapSettings::new("127.0.0.1", Security::None);
    settings.server.port = server.port;
    let login = Login::Password {
        user: "me".into(),
        password: "secret".into(),
    };
    ImapMailStore::new(settings, login, Address::new("me@example.com"))
}

async fn plain() -> (Server, ImapMailStore) {
    let server = server(Mode::Plain, Accept::Password("me", "secret"), folders()).await;
    let store = store_for(&server);
    // Connecting says what the server can do.
    store.list_folders().await.unwrap();
    (server, store)
}

fn deliver(server: &Server, folder: &str, subject: &str, flags: &[&str]) -> u32 {
    server
        .state
        .lock()
        .unwrap()
        .deliver(folder, &simple(subject), flags)
}

fn inbox() -> FolderId {
    FolderId::new("INBOX")
}

/// Syncs `folder` to the end, from `state`, and returns what came.
async fn synced(store: &ImapMailStore, state: Option<SyncState>) -> (FolderChanges, usize) {
    let mut state = state;
    let mut all = FolderChanges {
        changed: Vec::new(),
        removed: Vec::new(),
        state: SyncState::new(""),
        more: true,
    };
    let mut batches = 0;
    while all.more {
        let batch = store.sync_folder(&inbox(), state.as_ref()).await.unwrap();
        batches += 1;
        state = Some(batch.state.clone());
        all.changed.extend(batch.changed);
        all.removed.extend(batch.removed);
        all.state = batch.state;
        all.more = batch.more;
    }
    (all, batches)
}

fn subjects(changes: &FolderChanges) -> Vec<String> {
    let mut subjects: Vec<String> = changes.changed.iter().map(|m| m.subject.clone()).collect();
    subjects.sort();
    subjects
}

#[tokio::test]
async fn syncs_only_where_the_server_tells_what_changed() {
    let server = server(Mode::Plain, Accept::Password("me", "secret"), folders()).await;
    let store = store_for(&server);
    // Before it has connected, it doesn't know, and an earlier run's cache
    // can be read.
    assert!(store.capabilities().can_sync);
    store.list_folders().await.unwrap();
    assert!(store.capabilities().can_sync);

    let server = server_without_condstore().await;
    let store = store_for(&server);
    store.list_folders().await.unwrap();
    assert!(!store.capabilities().can_sync);
}

async fn server_without_condstore() -> Server {
    let server = server(Mode::Plain, Accept::Password("me", "secret"), folders()).await;
    server.state.lock().unwrap().no_condstore = true;
    server
}

#[tokio::test]
async fn the_first_sync_brings_every_message_in_batches() {
    let (server, store) = plain().await;
    for n in 0..250 {
        deliver(&server, "INBOX", &format!("m{n}"), &[]);
    }
    let (changes, batches) = synced(&store, None).await;
    assert_eq!(changes.changed.len(), 250);
    assert_eq!(batches, 3, "100 at a time");
    assert!(changes.removed.is_empty());
    // The newest came first.
    let first = store.sync_folder(&inbox(), None).await.unwrap();
    assert!(first.more);
    let names = subjects(&first);
    assert!(names.contains(&"m249".to_owned()) && !names.contains(&"m0".to_owned()));
}

#[tokio::test]
async fn a_synced_folder_with_nothing_new_costs_nothing() {
    let (server, store) = plain().await;
    deliver(&server, "INBOX", "one", &[]);
    let (done, _) = synced(&store, None).await;
    let fetches = server.state.lock().unwrap().fetches;
    let again = store
        .sync_folder(&inbox(), Some(&done.state))
        .await
        .unwrap();
    assert!(again.changed.is_empty() && again.removed.is_empty() && !again.more);
    assert_eq!(again.state, done.state);
    assert_eq!(
        server.state.lock().unwrap().fetches,
        fetches,
        "nothing fetched"
    );
}

#[tokio::test]
async fn new_mail_and_changed_flags_come_as_changes() {
    let (server, store) = plain().await;
    let read = deliver(&server, "INBOX", "old", &["\\Seen"]);
    let (done, _) = synced(&store, None).await;
    deliver(&server, "INBOX", "new", &[]);
    server.state.lock().unwrap().set_flags("INBOX", read, &[]);
    let (changes, _) = synced(&store, Some(done.state)).await;
    assert_eq!(subjects(&changes), ["new", "old"]);
    let old = changes.changed.iter().find(|m| m.subject == "old").unwrap();
    assert!(!old.flags.read, "as it is now");
    // What changed did so by now, whatever its date says.
    let age = chrono::Utc::now() - old.modified.unwrap();
    assert!(age < chrono::TimeDelta::minutes(1));
}

#[tokio::test]
async fn messages_that_went_are_removed_even_with_others_coming() {
    let (server, store) = plain().await;
    let first = deliver(&server, "INBOX", "one", &[]);
    deliver(&server, "INBOX", "two", &[]);
    let (done, _) = synced(&store, None).await;
    // One goes, one comes: the count alone doesn't say.
    server
        .state
        .lock()
        .unwrap()
        .folder("INBOX")
        .messages
        .retain(|m| m.uid != first);
    deliver(&server, "INBOX", "three", &[]);
    let (changes, _) = synced(&store, Some(done.state)).await;
    assert_eq!(subjects(&changes), ["three"]);
    assert_eq!(changes.removed.len(), 1);
    assert!(changes.removed[0].as_str().contains(&format!(".{first}.")));
}

#[tokio::test]
async fn a_folder_made_anew_or_a_state_not_understood_says_to_start_over() {
    let (server, store) = plain().await;
    deliver(&server, "INBOX", "one", &[]);
    let (done, _) = synced(&store, None).await;
    for state in [
        SyncState::new("rubbish"),
        SyncState::new("imap1/9/9/9/1//0"),
    ] {
        let error = store.sync_folder(&inbox(), Some(&state)).await.unwrap_err();
        assert!(matches!(error, Error::StaleSync), "{error}");
    }
    server.state.lock().unwrap().folder("INBOX").validity += 1;
    let error = store
        .sync_folder(&inbox(), Some(&done.state))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::StaleSync), "{error}");
}

#[tokio::test]
async fn a_sync_cut_short_goes_on_from_its_state() {
    let (server, store) = plain().await;
    for n in 0..150 {
        deliver(&server, "INBOX", &format!("m{n}"), &[]);
    }
    let first = store.sync_folder(&inbox(), None).await.unwrap();
    assert!(first.more);
    // Mail comes meanwhile; the sync goes on where it was, and the next one
    // takes the new mail in.
    deliver(&server, "INBOX", "late", &[]);
    let (rest, _) = synced(&store, Some(first.state)).await;
    assert_eq!(rest.changed.len(), 50);
    let (late, _) = synced(&store, Some(rest.state)).await;
    assert_eq!(subjects(&late), ["late"]);
}

fn account() -> mett_core::AccountId {
    mett_core::AccountId::new("me@127.0.0.1")
}

fn cached(store: ImapMailStore) -> CachedStore<ImapMailStore> {
    let cache = Cache::in_memory(&account()).unwrap();
    CachedStore::new(store, Some(cache))
}

/// The subjects of the folder's messages, sorted: they are dated alike.
async fn listed(store: &impl MailStore) -> Vec<String> {
    let mut subjects: Vec<String> = store
        .list_messages(&inbox(), Page::first(500))
        .await
        .unwrap()
        .messages
        .into_iter()
        .map(|m| m.subject)
        .collect();
    subjects.sort();
    subjects
}

/// A folder synced this recently isn't synced again.
async fn after_fresh() {
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
}

#[tokio::test]
async fn the_cache_in_front_lists_from_what_it_has_and_follows_the_server() {
    let server = server(Mode::Plain, Accept::Password("me", "secret"), folders()).await;
    let store = cached(store_for(&server));
    // Connecting first, as opening the folder does (it finds it by name).
    store.child_folders(None).await.unwrap();
    let one = deliver(&server, "INBOX", "one", &[]);
    let two = deliver(&server, "INBOX", "two", &["\\Seen"]);
    assert_eq!(listed(&store).await, ["one", "two"]);

    // Listed again with nothing new, the server isn't asked for messages.
    after_fresh().await;
    let fetches = server.state.lock().unwrap().fetches;
    assert_eq!(listed(&store).await, ["one", "two"]);
    assert_eq!(server.state.lock().unwrap().fetches, fetches);

    // Mail comes, a message is read by another client, another goes.
    let three = deliver(&server, "INBOX", "three", &[]);
    {
        let mut state = server.state.lock().unwrap();
        state.set_flags("INBOX", one, &["\\Seen"]);
        state.folder("INBOX").messages.retain(|m| m.uid != two);
    }
    after_fresh().await;
    let page = store
        .list_messages(&inbox(), Page::first(10))
        .await
        .unwrap();
    let mut seen: Vec<(String, bool)> = page
        .messages
        .iter()
        .map(|m| (m.subject.clone(), m.flags.read))
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        [("one".to_owned(), true), ("three".to_owned(), false)]
    );
    let _ = three;
}

#[tokio::test]
async fn what_mett_does_is_in_the_cache_at_once_and_agrees_with_the_server() {
    let server = server(Mode::Plain, Accept::Password("me", "secret"), folders()).await;
    let store = cached(store_for(&server));
    store.child_folders(None).await.unwrap();
    deliver(&server, "INBOX", "one", &[]);
    deliver(&server, "INBOX", "two", &[]);
    let page = store
        .list_messages(&inbox(), Page::first(10))
        .await
        .unwrap();
    let (newer, older) = (page.messages[0].id.clone(), page.messages[1].id.clone());

    store
        .update_flags(&older, FlagsUpdate::read(true))
        .await
        .unwrap();
    store.delete_message(&newer).await.unwrap();
    let page = store
        .list_messages(&inbox(), Page::first(10))
        .await
        .unwrap();
    assert_eq!(page.messages.len(), 1);
    assert!(page.messages[0].flags.read);
    // The server says the same.
    let state = server.state.lock().unwrap();
    assert_eq!(state.folders[0].messages.len(), 1);
}

#[tokio::test]
async fn an_opened_message_keeps_its_body_in_the_cache_when_its_flags_change() {
    let server = server(Mode::Plain, Accept::Password("me", "secret"), folders()).await;
    let store = cached(store_for(&server));
    store.child_folders(None).await.unwrap();
    let uid = deliver(&server, "INBOX", "one", &[]);
    let page = store
        .list_messages(&inbox(), Page::first(10))
        .await
        .unwrap();
    let id = page.messages[0].id.clone();
    store.fetch_message(&id).await.unwrap();

    // Another client reads it: the sync brings the new flags, not a new body.
    server
        .state
        .lock()
        .unwrap()
        .set_flags("INBOX", uid, &["\\Seen"]);
    after_fresh().await;
    let page = store
        .list_messages(&inbox(), Page::first(10))
        .await
        .unwrap();
    assert!(page.messages[0].flags.read);
    let fetches = server.state.lock().unwrap().fetches;
    let message = store.fetch_message(&id).await.unwrap();
    assert_eq!(message.summary.subject, "one");
    assert_eq!(server.state.lock().unwrap().fetches, fetches);
}

#[tokio::test]
async fn every_batch_of_the_first_sync_keeps_the_dates_the_messages_came_in() {
    let (server, store) = plain().await;
    for n in 0..150 {
        deliver(&server, "INBOX", &format!("m{n}"), &[]);
    }
    let (changes, batches) = synced(&store, None).await;
    assert_eq!(batches, 2);
    let now = chrono::Utc::now();
    for message in &changes.changed {
        // Delivered long before: not changed just now.
        assert!(now - message.modified.unwrap() > chrono::TimeDelta::hours(1));
    }
}

#[tokio::test]
async fn a_folder_without_modification_sequences_is_not_synced() {
    let (server, store) = plain().await;
    server.state.lock().unwrap().nomodseq = true;
    deliver(&server, "INBOX", "one", &[]);
    let error = store.sync_folder(&inbox(), None).await.unwrap_err();
    assert!(matches!(error, Error::CannotSync), "{error}");

    // The cache then leaves the folder to the ordinary listing, which works.
    let cached = cached(store_for(&server));
    cached.child_folders(None).await.unwrap();
    assert_eq!(listed(&cached).await, ["one"]);
    // So does a server that learns it can't sync only when asked to.
    let server = server_without_condstore().await;
    deliver(&server, "INBOX", "two", &[]);
    let cached = self::cached(store_for(&server));
    assert_eq!(listed(&cached).await, ["two"]);
}
