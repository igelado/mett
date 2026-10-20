//! CachedStore in front of the in-memory store.

use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use mett_core::mock::{self, MemoryStore};
use mett_core::{
    Error, FlagsUpdate, FolderId, MailStore, MessageId, MessageSummary, Page, WellKnownFolder,
};
use mett_sqlite::{Cache, CachedStore};

/// `inner` with a cache in memory.
fn cached(
    inner: MemoryStore,
) -> std::result::Result<CachedStore<MemoryStore>, mett_sqlite::CacheError> {
    Ok(CachedStore::new(inner, Some(Cache::in_memory(&alice())?)))
}

fn at(minutes: i64) -> DateTime<Utc> {
    DateTime::UNIX_EPOCH + TimeDelta::minutes(minutes)
}

/// A mailbox whose Inbox has `n` messages, m0 the oldest.
fn mailbox(n: i64) -> MemoryStore {
    let mut store = MemoryStore::new()
        .with_sync()
        .with_folder(mock::folder("in", "Inbox", Some(WellKnownFolder::Inbox)))
        .with_folder(mock::folder(
            "arc",
            "Archive",
            Some(WellKnownFolder::Archive),
        ));
    for i in 0..n {
        store = store.with_message(mock::message(&format!("m{i}"), "in", "hi", at(i)));
    }
    store
}

fn alice() -> mett_core::AccountId {
    mett_core::AccountId::new("alice@contoso.com")
}

fn inbox() -> FolderId {
    FolderId::new("in")
}

fn ids(messages: &[MessageSummary]) -> Vec<&str> {
    messages.iter().map(|m| m.id.as_str()).collect()
}

/// Every message of the Inbox, newest first, listed `size` at a time.
async fn listing(store: &CachedStore<MemoryStore>, size: u32) -> Vec<String> {
    let mut page = store
        .list_messages(&inbox(), Page::first(size))
        .await
        .unwrap();
    let mut all: Vec<String> = ids(&page.messages).into_iter().map(str::to_owned).collect();
    while let Some(next) = page.next {
        page = store
            .list_messages(&inbox(), Page::after(next, size))
            .await
            .unwrap();
        all.extend(ids(&page.messages).into_iter().map(str::to_owned));
    }
    all
}

/// Lets the folders be synced again.
async fn later() {
    tokio::time::advance(Duration::from_secs(3)).await;
}

#[tokio::test(start_paused = true)]
async fn lists_folders_from_the_cache_after_one_sync() {
    let store = cached(mailbox(5)).unwrap();
    assert_eq!(listing(&store, 2).await, ["m4", "m3", "m2", "m1", "m0"]);
    assert_eq!(
        store.inner().syncs(),
        1,
        "the later pages come from the cache"
    );
    // Listed again at once (as a check for new mail does), it isn't synced
    // again.
    store.list_messages(&inbox(), Page::first(2)).await.unwrap();
    assert_eq!(store.inner().syncs(), 1);
    later().await;
    store.list_messages(&inbox(), Page::first(2)).await.unwrap();
    assert_eq!(store.inner().syncs(), 2);
}

#[tokio::test(start_paused = true)]
async fn takes_in_changes_made_elsewhere() {
    let store = cached(mailbox(3)).unwrap();
    listing(&store, 10).await;
    // In Outlook, say: one is read, one deleted, one arrives.
    let inner = store.inner();
    inner
        .update_flags(&MessageId::new("m0"), FlagsUpdate::read(true))
        .await
        .unwrap();
    inner.delete_message(&MessageId::new("m1")).await.unwrap();
    inner.deliver(mock::message("m9", "in", "new", at(9)), at(9));
    later().await;
    let page = store
        .list_messages(&inbox(), Page::first(10))
        .await
        .unwrap();
    assert_eq!(ids(&page.messages), ["m9", "m2", "m0"]);
    assert!(page.messages[2].flags.read);
    let changed = store
        .changed_messages(&inbox(), at(9), Page::first(10))
        .await
        .unwrap();
    // m0 was read just now; m9 arrived at 9.
    assert_eq!(ids(&changed.messages), ["m0", "m9"]);
    let unread = store.recent_unread(&inbox(), Some(at(0))).await.unwrap();
    let unread: Vec<&str> = unread.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(unread, ["m9", "m2"]);
}

#[tokio::test(start_paused = true)]
async fn changes_made_here_show_at_once() {
    let store = cached(mailbox(3)).unwrap();
    listing(&store, 10).await;
    store
        .update_flags(&MessageId::new("m2"), FlagsUpdate::flagged(true))
        .await
        .unwrap();
    // Shown as made.
    let page = store
        .list_messages(&inbox(), Page::first(10))
        .await
        .unwrap();
    assert!(page.messages[0].flags.flagged);
    // The next listing syncs, to take in the server's date of the change.
    assert_eq!(store.inner().syncs(), 2);
    let changed = store
        .changed_messages(
            &inbox(),
            Utc::now() - TimeDelta::minutes(1),
            Page::first(10),
        )
        .await
        .unwrap();
    assert_eq!(ids(&changed.messages), ["m2"]);
    // A move brings mail into another folder: folders sync again.
    store
        .move_message(&MessageId::new("m1"), &FolderId::new("arc"))
        .await
        .unwrap();
    let page = store
        .list_messages(&inbox(), Page::first(10))
        .await
        .unwrap();
    assert_eq!(ids(&page.messages), ["m2", "m0"]);
    assert_eq!(store.inner().syncs(), 3);
}

#[tokio::test(start_paused = true)]
async fn keeps_the_messages_read() {
    let store = cached(mailbox(2)).unwrap();
    listing(&store, 10).await;
    let id = MessageId::new("m1");
    let fetched = store.fetch_message(&id).await.unwrap();
    // Unchanged, it is kept across syncs.
    later().await;
    listing(&store, 10).await;
    // Gone from the server, it is still here until the next sync.
    store.inner().delete_message(&id).await.unwrap();
    assert_eq!(store.fetch_message(&id).await.unwrap(), fetched);
    later().await;
    listing(&store, 10).await;
    assert!(matches!(
        store.fetch_message(&id).await,
        Err(Error::NotFound { .. })
    ));
}

#[tokio::test(start_paused = true)]
async fn a_stale_sync_starts_over() {
    let store = cached(mailbox(3)).unwrap();
    listing(&store, 10).await;
    store.inner().forget_syncs();
    store
        .inner()
        .delete_message(&MessageId::new("m0"))
        .await
        .unwrap();
    later().await;
    assert_eq!(listing(&store, 10).await, ["m2", "m1"]);
    // The state was refused, then the folder synced from the start.
    assert_eq!(store.inner().syncs(), 3);
}

#[tokio::test]
async fn without_a_cache_everything_goes_to_the_server() {
    let store = CachedStore::new(mailbox(3), None);
    assert_eq!(listing(&store, 2).await, ["m2", "m1", "m0"]);
    assert_eq!(store.inner().syncs(), 0);
}

#[tokio::test]
async fn backends_that_cannot_sync_are_not_cached() {
    let inner = MemoryStore::new()
        .with_folder(mock::folder("in", "Inbox", Some(WellKnownFolder::Inbox)))
        .with_message(mock::message("m0", "in", "hi", at(0)));
    let store = cached(inner).unwrap();
    assert_eq!(listing(&store, 2).await, ["m0"]);
}

/// On every system (Windows too), a cache opens in a place that isn't
/// there yet, and opens again.
#[tokio::test(start_paused = true)]
async fn a_new_cache_opens_anywhere() {
    let dir = std::env::temp_dir().join(format!("mett-sqlite-new-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("cache").join("alice@contoso.com.sqlite");
    {
        let store = CachedStore::new(mailbox(2), Some(Cache::open(&path, &alice()).unwrap()));
        listing(&store, 10).await;
    }
    assert!(path.is_file());
    Cache::open(&path, &alice()).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn the_cache_is_private_and_lasts() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("mett-sqlite-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("cache").join("alice@contoso.com.sqlite");
    {
        let store = CachedStore::new(mailbox(2), Some(Cache::open(&path, &alice()).unwrap()));
        listing(&store, 10).await;
    }
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    // A directory others can write to isn't used, nor a symbolic link.
    let shared = dir.join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(Cache::open(&shared.join("c.sqlite"), &alice()).is_err());
    let link = dir.join("link.sqlite");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(Cache::open(&link, &alice()).is_err());
    // Nor, by a relative path, a cache in a working directory others can
    // write to.
    let here = std::env::current_dir().unwrap();
    std::env::set_current_dir(&shared).unwrap();
    let relative = Cache::open(std::path::Path::new("c.sqlite"), &alice());
    std::env::set_current_dir(here).unwrap();
    assert!(relative.is_err());
    // Nor another program's database, which is left as it was.
    let other = dir.join("other.sqlite");
    rusqlite::Connection::open(&other)
        .unwrap()
        .execute_batch("CREATE TABLE messages (x); INSERT INTO messages VALUES (1);")
        .unwrap();
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(Cache::open(&other, &alice()).is_err());
    let conn = rusqlite::Connection::open(&other).unwrap();
    let kept: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(kept, 1);
    let journal: String = conn
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(journal, "delete", "its journal mode too");
    assert_eq!(mode(&other), 0o644, "and its permissions");
    // One made readable by others is made private again, and SQLite's
    // files beside it.
    let wal = dir.join("cache").join("alice@contoso.com.sqlite-wal");
    std::fs::write(&wal, b"").unwrap();
    for file in [&path, &wal] {
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    drop(Cache::open(&path, &alice()).unwrap());
    assert_eq!(mode(&path), 0o600);
    if wal.exists() {
        assert_eq!(mode(&wal), 0o600);
    }
    // Opened again, the folder goes on from where its sync stood: the
    // mock store's state still tells nothing changed.
    let inner = mailbox(2);
    let store = CachedStore::new(inner, Some(Cache::open(&path, &alice()).unwrap()));
    let page = store
        .list_messages(&inbox(), Page::first(10))
        .await
        .unwrap();
    assert_eq!(ids(&page.messages), ["m1", "m0"]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(start_paused = true)]
async fn what_mett_copies_shows_at_once() {
    let store = cached(mailbox(2)).unwrap();
    let archive = FolderId::new("arc");
    listing(&store, 10).await;
    let page = store
        .list_messages(&archive, Page::first(10))
        .await
        .unwrap();
    assert!(page.messages.is_empty());
    // Copied just after its folder was synced.
    let copy = store
        .copy_message(&MessageId::new("m1"), &archive)
        .await
        .unwrap();
    let page = store
        .list_messages(&archive, Page::first(10))
        .await
        .unwrap();
    assert_eq!(page.messages[0].id, copy);
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_cache_that_fails_is_left_for_the_server() {
    let dir = std::env::temp_dir().join(format!("mett-sqlite-broken-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("cache.sqlite");
    let store = CachedStore::new(mailbox(2), Some(Cache::open(&path, &alice()).unwrap()));
    listing(&store, 10).await;
    // Something breaks the cache under mett.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("DROP TABLE messages")
        .unwrap();
    later().await;
    assert_eq!(listing(&store, 10).await, ["m1", "m0"], "from the server");
    let syncs = store.inner().syncs();
    later().await;
    listing(&store, 10).await;
    assert_eq!(store.inner().syncs(), syncs, "the cache is left alone");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The in-memory store, which can be taken offline: every call then fails
/// for want of a network.
struct Flaky {
    inner: MemoryStore,
    down: std::sync::atomic::AtomicBool,
}

impl Flaky {
    fn up(&self) -> mett_core::Result<()> {
        if self.down.load(std::sync::atomic::Ordering::SeqCst) {
            Err(Error::Network("offline".into()))
        } else {
            Ok(())
        }
    }
}

impl MailStore for Flaky {
    async fn list_folders(&self) -> mett_core::Result<Vec<mett_core::Folder>> {
        self.up()?;
        self.inner.list_folders().await
    }
    async fn child_folders(
        &self,
        parent: Option<&FolderId>,
    ) -> mett_core::Result<Vec<mett_core::Folder>> {
        self.up()?;
        self.inner.child_folders(parent).await
    }
    async fn folder(&self, id: &FolderId) -> mett_core::Result<mett_core::Folder> {
        self.up()?;
        self.inner.folder(id).await
    }
    async fn recent_unread(
        &self,
        folder: &FolderId,
        since: Option<DateTime<Utc>>,
    ) -> mett_core::Result<Vec<(MessageId, DateTime<Utc>)>> {
        self.up()?;
        self.inner.recent_unread(folder, since).await
    }
    async fn well_known_folder(
        &self,
        which: WellKnownFolder,
    ) -> mett_core::Result<mett_core::Folder> {
        self.up()?;
        self.inner.well_known_folder(which).await
    }
    async fn list_messages(
        &self,
        folder: &FolderId,
        page: Page,
    ) -> mett_core::Result<mett_core::MessagePage> {
        self.up()?;
        self.inner.list_messages(folder, page).await
    }
    async fn changed_messages(
        &self,
        folder: &FolderId,
        since: DateTime<Utc>,
        page: Page,
    ) -> mett_core::Result<mett_core::MessagePage> {
        self.up()?;
        self.inner.changed_messages(folder, since, page).await
    }
    async fn fetch_message(&self, id: &MessageId) -> mett_core::Result<mett_core::Message> {
        self.up()?;
        self.inner.fetch_message(id).await
    }
    async fn list_attachments(
        &self,
        message: &MessageId,
    ) -> mett_core::Result<Vec<mett_core::Attachment>> {
        self.up()?;
        self.inner.list_attachments(message).await
    }
    async fn fetch_attachment(
        &self,
        message: &MessageId,
        attachment: &mett_core::AttachmentId,
    ) -> mett_core::Result<Vec<u8>> {
        self.up()?;
        self.inner.fetch_attachment(message, attachment).await
    }
    async fn update_flags(&self, id: &MessageId, update: FlagsUpdate) -> mett_core::Result<()> {
        self.up()?;
        self.inner.update_flags(id, update).await
    }
    async fn move_message(&self, id: &MessageId, to: &FolderId) -> mett_core::Result<MessageId> {
        self.up()?;
        self.inner.move_message(id, to).await
    }
    async fn copy_message(&self, id: &MessageId, to: &FolderId) -> mett_core::Result<MessageId> {
        self.up()?;
        self.inner.copy_message(id, to).await
    }
    async fn delete_message(&self, id: &MessageId) -> mett_core::Result<()> {
        self.up()?;
        self.inner.delete_message(id).await
    }
    async fn save_draft(&self, draft: &mett_core::Draft) -> mett_core::Result<MessageId> {
        self.up()?;
        self.inner.save_draft(draft).await
    }
    fn can_sync(&self) -> bool {
        true
    }
    async fn sync_folder(
        &self,
        folder: &FolderId,
        state: Option<&mett_core::SyncState>,
    ) -> mett_core::Result<mett_core::FolderChanges> {
        self.up()?;
        self.inner.sync_folder(folder, state).await
    }
}

#[tokio::test(start_paused = true)]
async fn offline_the_folders_seen_are_listed_as_they_were() {
    let flaky = Flaky {
        inner: mailbox(2),
        down: false.into(),
    };
    let store = CachedStore::new(flaky, Some(Cache::in_memory(&alice()).unwrap()));
    let inbox = store
        .well_known_folder(WellKnownFolder::Inbox)
        .await
        .unwrap();
    store.child_folders(None).await.unwrap();
    assert!(
        store
            .child_folders(Some(&inbox.id))
            .await
            .unwrap()
            .is_empty()
    );
    store
        .list_messages(&inbox.id, Page::first(10))
        .await
        .unwrap();
    let id = MessageId::new("m1");
    store.fetch_message(&id).await.unwrap();

    store
        .inner()
        .down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    later().await;
    // Opening the Inbox finds it, and lists it, as it was.
    let found = store
        .well_known_folder(WellKnownFolder::Inbox)
        .await
        .unwrap();
    assert_eq!(found, inbox);
    assert_eq!(store.folder(&inbox.id).await.unwrap(), inbox);
    let below: Vec<String> = store
        .child_folders(None)
        .await
        .unwrap()
        .into_iter()
        .map(|f| f.name)
        .collect();
    assert_eq!(below, ["Inbox", "Archive"]);
    // Known to have none below it.
    assert!(
        store
            .child_folders(Some(&inbox.id))
            .await
            .unwrap()
            .is_empty()
    );
    let page = store
        .list_messages(&inbox.id, Page::first(10))
        .await
        .unwrap();
    assert_eq!(ids(&page.messages), ["m1", "m0"]);
    assert!(store.fetch_message(&id).await.is_ok(), "read before");
    // What was never seen still needs the network.
    assert!(matches!(
        store.folder(&FolderId::new("nowhere")).await,
        Err(Error::Network(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn pages_go_on_after_the_last_message_listed() {
    let store = cached(mailbox(4)).unwrap();
    let first = store.list_messages(&inbox(), Page::first(2)).await.unwrap();
    assert_eq!(ids(&first.messages), ["m3", "m2"]);
    // Mail arrives, and a check for new mail syncs it, before the next
    // page: it doesn't shift what comes next.
    store
        .inner()
        .deliver(mock::message("m9", "in", "new", at(9)), at(9));
    later().await;
    store
        .changed_messages(&inbox(), at(0), Page::first(10))
        .await
        .unwrap();
    let second = store
        .list_messages(&inbox(), Page::after(first.next.unwrap(), 2))
        .await
        .unwrap();
    assert_eq!(ids(&second.messages), ["m1", "m0"]);
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_cache_that_fails_mid_listing_goes_on_from_the_server() {
    let dir = std::env::temp_dir().join(format!("mett-sqlite-resume-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("cache.sqlite");
    let store = CachedStore::new(mailbox(6), Some(Cache::open(&path, &alice()).unwrap()));
    let first = store.list_messages(&inbox(), Page::first(2)).await.unwrap();
    assert_eq!(ids(&first.messages), ["m5", "m4"]);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("DROP TABLE messages")
        .unwrap();
    // The rest comes from the server, page by page, without what the
    // cache listed.
    let mut rest = Vec::new();
    let mut next = first.next;
    while let Some(cursor) = next {
        let page = store
            .list_messages(&inbox(), Page::after(cursor, 2))
            .await
            .unwrap();
        rest.extend(ids(&page.messages).into_iter().map(str::to_owned));
        next = page.next;
    }
    assert_eq!(rest, ["m3", "m2", "m1", "m0"]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_cache_is_one_accounts() {
    let dir = std::env::temp_dir().join(format!("mett-sqlite-accounts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("cache.sqlite");
    {
        let store = CachedStore::new(mailbox(2), Some(Cache::open(&path, &alice()).unwrap()));
        listing(&store, 10).await;
    }
    // Bob's mett doesn't use (nor empty) Alice's cache: hers may have it
    // open.
    let bob = mett_core::AccountId::new("bob@contoso.com");
    let refused = Cache::open(&path, &bob).err().unwrap().to_string();
    assert!(refused.contains("alice@contoso.com"), "{refused}");
    // Even if its schema is out of date.
    rusqlite::Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", 1)
        .unwrap();
    assert!(Cache::open(&path, &bob).is_err());
    let store = CachedStore::new(mailbox(2), Some(Cache::open(&path, &alice()).unwrap()));
    assert_eq!(store.inner().syncs(), 0);
    let page = store
        .list_messages(&inbox(), Page::first(10))
        .await
        .unwrap();
    assert_eq!(ids(&page.messages), ["m1", "m0"], "still Alice's");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_message_changed_on_the_server_is_fetched_again() {
    let store = cached(mailbox(1)).unwrap();
    listing(&store, 10).await;
    let id = MessageId::new("m0");
    store.fetch_message(&id).await.unwrap();
    // Changed elsewhere: its body may be new.
    store
        .inner()
        .update_flags(&id, FlagsUpdate::flagged(true))
        .await
        .unwrap();
    later().await;
    listing(&store, 10).await;
    // Gone from the server now, it can't come from the cache.
    store.inner().delete_message(&id).await.unwrap();
    assert!(store.fetch_message(&id).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn quick_syncs_have_nothing_to_tell() {
    let told = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let progress = {
        let told = std::sync::Arc::clone(&told);
        Box::new(move |folder: &FolderId, taken: u64| {
            told.lock().unwrap().push((folder.to_string(), taken));
        })
    };
    let store = cached(mailbox(3)).unwrap().with_progress(progress);
    listing(&store, 10).await;
    // One batch: done at once. (Longer ones are counted in db.rs's tests.)
    assert!(told.lock().unwrap().is_empty());
}
