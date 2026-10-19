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
    Ok(CachedStore::new(inner, Some(Cache::in_memory()?)))
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
    store
        .move_message(&MessageId::new("m1"), &FolderId::new("arc"))
        .await
        .unwrap();
    // Without syncing again.
    let page = store
        .list_messages(&inbox(), Page::first(10))
        .await
        .unwrap();
    assert_eq!(ids(&page.messages), ["m2", "m0"]);
    assert!(page.messages[0].flags.flagged);
    assert_eq!(store.inner().syncs(), 1);
}

#[tokio::test(start_paused = true)]
async fn keeps_the_messages_read() {
    let store = cached(mailbox(2)).unwrap();
    listing(&store, 10).await;
    let id = MessageId::new("m1");
    let fetched = store.fetch_message(&id).await.unwrap();
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

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn the_cache_is_private_and_lasts() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("mett-sqlite-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("cache").join("alice@contoso.com.sqlite");
    {
        let store = CachedStore::new(mailbox(2), Some(Cache::open(&path).unwrap()));
        listing(&store, 10).await;
    }
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    // Opened again, the folder goes on from where its sync stood: the
    // mock store's state still tells nothing changed.
    let inner = mailbox(2);
    let store = CachedStore::new(inner, Some(Cache::open(&path).unwrap()));
    let page = store
        .list_messages(&inbox(), Page::first(10))
        .await
        .unwrap();
    assert_eq!(ids(&page.messages), ["m1", "m0"]);
    std::fs::remove_dir_all(&dir).unwrap();
}
