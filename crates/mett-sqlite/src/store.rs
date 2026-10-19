//! [`CachedStore`]: a [`MailStore`] that keeps folders' indexes and
//! messages in SQLite, up to date by syncing them.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use mett_core::{
    AccountId, Address, Attachment, AttachmentId, Draft, Error, FlagsUpdate, Folder, FolderChanges,
    FolderId, MailSender, MailStore, Message, MessageId, MessagePage, MessageSummary, Page,
    PageCursor, RECENT_UNREAD, Result, SyncState, WellKnownFolder,
};
use tokio::time::Instant;

use crate::db::{Db, Key, Listing, comes_after, modified_key, received_key};

/// A folder synced this recently isn't synced again: the calls of one
/// request (a check for new mail lists a folder and its changes) share a
/// sync.
const FRESH: Duration = Duration::from_secs(2);

/// How many times a sync goes on after another mett sharing the cache
/// took in changes first.
const LOST_RACES: u32 = 3;

/// Why the cache could not be opened.
#[derive(Debug, thiserror::Error)]
#[error("cache {path}: {source}")]
pub struct CacheError {
    path: String,
    #[source]
    source: Box<dyn std::error::Error + Send + Sync>,
}

/// A [`MailStore`] and [`MailSender`] in front of `S`, keeping the indexes
/// of the folders listed (and the messages fetched) in SQLite.
///
/// Listing a folder first syncs it ([`MailStore::sync_folder`]): only what
/// changed since the last time comes from the server, and the listing comes
/// from the cache. The server stays the source of truth: changes go to it
/// first, and the cache follows. Without a cache, or with a backend that
/// can't sync, everything goes to `S`.
pub struct CachedStore<S> {
    inner: S,
    db: Option<Mutex<Db>>,
    /// Set when the cache fails (a full disk, say): from then on,
    /// everything goes to `S`.
    broken: AtomicBool,
    /// How many changes mett made, which a sync brings in (with the dates
    /// the server gives them) or elsewhere (a copy, a draft): any folder
    /// synced before one is synced again.
    writes: AtomicU64,
    /// When each folder was last synced, and how many changes mett had
    /// made then. Its lock also keeps two syncs of one folder from running
    /// at once.
    synced: Mutex<HashMap<FolderId, Arc<tokio::sync::Mutex<LastSync>>>>,
}

/// When a folder was last synced, and how many changes mett had made then.
type LastSync = Option<(Instant, u64)>;

/// A cache database, for a [`CachedStore`].
pub struct Cache(Db);

impl Cache {
    /// The cache of `account`'s mailbox in the file `path`, which is
    /// created (only its owner may read it, as it holds mail) if needed.
    /// One that held another account's is emptied.
    pub fn open(path: &Path, account: &AccountId) -> std::result::Result<Self, CacheError> {
        let error = |source: Box<dyn std::error::Error + Send + Sync>| CacheError {
            path: path.display().to_string(),
            source,
        };
        private_file(path).map_err(|e| error(Box::new(e)))?;
        Db::open(path, account)
            .map(Self)
            .map_err(|e| error(Box::new(e)))
    }

    /// A cache of `account`'s mailbox in memory, gone when it is dropped.
    pub fn in_memory(account: &AccountId) -> std::result::Result<Self, CacheError> {
        Db::in_memory(account).map(Self).map_err(|e| CacheError {
            path: ":memory:".into(),
            source: Box::new(e),
        })
    }
}

impl<S> CachedStore<S> {
    /// `inner` with `cache`, or without one.
    pub fn new(inner: S, cache: Option<Cache>) -> Self {
        Self {
            inner,
            db: cache.map(|Cache(db)| Mutex::new(db)),
            broken: AtomicBool::new(false),
            writes: AtomicU64::new(0),
            synced: Mutex::new(HashMap::new()),
        }
    }

    /// The store behind the cache.
    pub fn inner(&self) -> &S {
        &self.inner
    }
}

/// Makes `path` readable only by its owner, creating it (and its
/// directory, likewise) if it doesn't exist: it holds mail. (SQLite gives
/// its journal files the database's permissions.)
fn private_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        // A file that was there already may be readable by others.
        let mut permissions = file.metadata()?.permissions();
        if permissions.mode() & 0o077 != 0 {
            permissions.set_mode(permissions.mode() & 0o700);
            file.set_permissions(permissions)?;
        }
    }
    #[cfg(not(unix))]
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

impl<S: MailStore> CachedStore<S> {
    /// Whether folders are cached: there is a cache, it works, and the
    /// backend can keep it up to date.
    fn caching(&self) -> bool {
        self.db.is_some() && !self.broken() && self.inner.can_sync()
    }

    /// Whether the cache failed, and is no longer used.
    fn broken(&self) -> bool {
        self.broken.load(Ordering::SeqCst)
    }

    /// Runs `f` on the cache. If it fails, the cache is no longer used.
    fn with_cache<T>(&self, f: impl FnOnce(&mut Db) -> Result<T>) -> Result<T> {
        let db = self
            .db
            .as_ref()
            .filter(|_| self.caching())
            .ok_or_else(|| Error::Backend("cache: not caching".into()))?;
        let mut db = db.lock().unwrap_or_else(|e| e.into_inner());
        let done = f(&mut db);
        if done.is_err() {
            self.broken.store(true, Ordering::SeqCst);
        }
        done
    }

    /// Notes a change mett made that a sync may bring in elsewhere.
    fn wrote(&self) {
        self.writes.fetch_add(1, Ordering::SeqCst);
    }

    /// Brings `folder`'s cache up to date, unless it just was. Each batch
    /// of changes is kept as it comes, so a sync cut short carries on
    /// where it stopped.
    async fn sync(&self, folder: &FolderId) -> Result<()> {
        let lock = Arc::clone(
            self.synced
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(folder.clone())
                .or_default(),
        );
        let mut last = lock.lock().await;
        let writes = self.writes.load(Ordering::SeqCst);
        if last.is_some_and(|(at, then)| at.elapsed() < FRESH && then == writes) {
            return Ok(());
        }
        let mut state = match self.with_cache(|db| db.synced(folder))? {
            Some(synced) if synced.state.is_some() => synced.state,
            _ => {
                self.with_cache(|db| db.start_over(folder))?;
                None
            }
        };
        let mut lost = 0;
        loop {
            let changes = match self.inner.sync_folder(folder, state.as_ref()).await {
                // The server forgot where the sync stood: start over,
                // keeping what is here until the sync says it's gone.
                Err(Error::StaleSync) if state.is_some() => {
                    self.with_cache(|db| db.start_over(folder))?;
                    state = None;
                    continue;
                }
                changes => changes?,
            };
            if !self.with_cache(|db| db.apply(folder, state.as_ref(), &changes))? {
                // Another mett sharing the cache synced the folder
                // meanwhile: go on from where it left it (what came since
                // may be in neither's changes). Should it keep winning, its
                // syncs keep the folder up to date.
                lost += 1;
                if lost > LOST_RACES {
                    break;
                }
                match self.with_cache(|db| db.synced(folder))? {
                    Some(synced) if synced.state.is_some() => state = synced.state,
                    _ => {
                        self.with_cache(|db| db.start_over(folder))?;
                        state = None;
                    }
                }
                continue;
            }
            let FolderChanges {
                state: next, more, ..
            } = changes;
            state = Some(next);
            if !more {
                break;
            }
        }
        *last = Some((Instant::now(), writes));
        Ok(())
    }

    /// Whether `folder` was ever all in the cache: offline, it can be
    /// listed as it was then (or since).
    fn has_snapshot(&self, folder: &FolderId) -> bool {
        self.with_cache(|db| db.synced(folder))
            .ok()
            .flatten()
            .is_some_and(|synced| synced.snapshot)
    }

    /// A page of `folder`'s summaries from the cache, from `query` (`what`
    /// the listing is, which its cursors record), which `key` orders.
    fn cached_page(
        &self,
        folder: &FolderId,
        page: &Page,
        what: &str,
        key: fn(&MessageSummary) -> Key,
        query: impl FnOnce(&Db, Option<&Key>, u64) -> Result<Vec<MessageSummary>>,
    ) -> Result<MessagePage> {
        let after = match page.cursor() {
            None => None,
            Some(cursor) => Some(parse_cursor(cursor, folder, what)?),
        };
        let size = u64::from(page.size());
        // One more than asked for tells whether there are more.
        let mut messages = self.with_cache(|db| query(db, after.as_ref(), size + 1))?;
        let more = messages.len() as u64 > size;
        messages.truncate(page.size() as usize);
        // The next page starts after the last message of this one, however
        // the folder changes meanwhile.
        let next = messages
            .last()
            .filter(|_| more)
            .map(|last| cursor(&key(last), what, folder));
        Ok(MessagePage { messages, next })
    }

    /// Goes on, from the server, with a listing (`what` of `folder`) the
    /// cache began before it failed: from `page` of the server's (its first,
    /// then the next), without what came up to `after` in the cache's order
    /// (`key`), on every page.
    async fn resume<F: Future<Output = Result<MessagePage>>>(
        &self,
        folder: &FolderId,
        what: &str,
        mut page: Page,
        after: &Key,
        key: fn(&MessageSummary) -> Key,
        list: impl Fn(Page) -> F,
    ) -> Result<MessagePage> {
        let size = page.size();
        loop {
            let listed = list(page).await?;
            let rest: Vec<MessageSummary> = listed
                .messages
                .into_iter()
                .filter(|m| comes_after(&key(m), after))
                .collect();
            match listed.next {
                Some(next) if rest.is_empty() => page = Page::after(next, size),
                next => {
                    return Ok(MessagePage {
                        messages: rest,
                        next: next.map(|next| resumed(after, what, folder, &next)),
                    });
                }
            }
        }
    }

    /// Goes on with a listing the cache began, from the server, if `page`
    /// is of one: `None` otherwise.
    async fn resume_listing<F: Future<Output = Result<MessagePage>>>(
        &self,
        folder: &FolderId,
        what: &str,
        page: &Page,
        key: fn(&MessageSummary) -> Key,
        list: impl Fn(Page) -> F,
    ) -> Option<Result<MessagePage>> {
        let cursor = page.cursor()?;
        let size = page.size();
        let (after, from) = if is_cached(cursor) {
            match parse_cursor(cursor, folder, what) {
                Ok(after) => (after, Page::first(size)),
                Err(e) => return Some(Err(e)),
            }
        } else if is_resumed(cursor) {
            match parse_resumed(cursor, folder, what) {
                Ok((after, next)) => (after, Page::after(next, size)),
                Err(e) => return Some(Err(e)),
            }
        } else {
            return None;
        };
        Some(self.resume(folder, what, from, &after, key, list).await)
    }

    async fn list_cached(&self, folder: &FolderId, page: &Page) -> Result<MessagePage> {
        if page.cursor().is_none() {
            match self.sync(folder).await {
                // Offline, a folder that was all here is listed as it was.
                Err(Error::Network(_)) if self.has_snapshot(folder) => {}
                synced => synced?,
            }
        }
        self.cached_page(folder, page, "all", received_key, |db, after, limit| {
            db.page(folder, after, limit)
        })
    }

    async fn changed_cached(
        &self,
        folder: &FolderId,
        since: DateTime<Utc>,
        page: &Page,
    ) -> Result<MessagePage> {
        if page.cursor().is_none() {
            self.sync(folder).await?;
        }
        let what = since.timestamp_millis().to_string();
        self.cached_page(folder, page, &what, modified_key, |db, after, limit| {
            db.changed(folder, since, after, limit)
        })
    }

    async fn recent_unread_cached(
        &self,
        folder: &FolderId,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<(MessageId, DateTime<Utc>)>> {
        self.sync(folder).await?;
        // Without `since`, the last few tell.
        let limit = if since.is_some() { RECENT_UNREAD } else { 50 };
        let since = since.unwrap_or(DateTime::UNIX_EPOCH);
        self.with_cache(|db| db.recent_unread(folder, since, limit as u64))
    }

    /// Keeps what the server said of folders (as `listing`), to find them
    /// offline.
    fn note_folders(&self, folders: &Result<Vec<Folder>>, listing: Listing<'_>) {
        if let Ok(folders) = folders
            && self.caching()
        {
            let _ = self.with_cache(|db| db.keep_folders(folders, listing));
        }
    }

    /// `online`, or offline (a network failure), what `recall` finds in the
    /// cache, as last seen.
    fn or_as_seen<T>(
        &self,
        online: Result<T>,
        recall: impl FnOnce(&Db) -> Result<Option<T>>,
    ) -> Result<T> {
        match online {
            Err(Error::Network(e)) => {
                match self.caching().then(|| self.with_cache(|db| recall(db))) {
                    Some(Ok(Some(seen))) => Ok(seen),
                    _ => Err(Error::Network(e)),
                }
            }
            online => online,
        }
    }
}

/// A page cursor into the cache: the key of the last message listed, in
/// which listing (`what`) of which folder.
fn cursor((at, id): &Key, what: &str, folder: &FolderId) -> PageCursor {
    PageCursor::new(format!("cache\t{what}\t{at}\t{id}\t{folder}"))
}

/// Whether `cursor` is the cache's.
fn is_cached(cursor: &PageCursor) -> bool {
    cursor.as_str().starts_with("cache\t")
}

/// A cursor of a listing the cache began, resumed from the server: the
/// key of the last message the cache listed, and the server's cursor.
fn resumed((at, id): &Key, what: &str, folder: &FolderId, next: &PageCursor) -> PageCursor {
    PageCursor::new(format!("resumed\t{what}\t{at}\t{id}\t{folder}\t{next}"))
}

/// Whether `cursor` is of a listing resumed from the server ([`resumed`]).
fn is_resumed(cursor: &PageCursor) -> bool {
    cursor.as_str().starts_with("resumed\t")
}

/// The key and the server's cursor in `cursor` ([`resumed`]), which must be
/// for `what` of `folder`.
fn parse_resumed(cursor: &PageCursor, folder: &FolderId, what: &str) -> Result<(Key, PageCursor)> {
    let bad = || {
        Error::Backend(format!(
            "page cursor is not for this listing of folder {folder}"
        ))
    };
    let rest = cursor.as_str().strip_prefix("resumed\t").ok_or_else(bad)?;
    let (key, next) = rest
        .match_indices('\t')
        .nth(3)
        .map(|(at, _)| (&rest[..at], &rest[at + 1..]))
        .ok_or_else(bad)?;
    let after = parse_cursor(&PageCursor::new(format!("cache\t{key}")), folder, what)?;
    Ok((after, PageCursor::new(next)))
}

/// The key in `cursor`, which must be for `what` of `folder`.
fn parse_cursor(cursor: &PageCursor, folder: &FolderId, what: &str) -> Result<Key> {
    let bad = || {
        Error::Backend(format!(
            "page cursor is not for this listing of folder {folder}"
        ))
    };
    let mut parts = cursor.as_str().splitn(5, '\t');
    let (Some("cache"), Some(cursor_what), Some(at), Some(id), Some(cursor_folder)) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(bad());
    };
    if cursor_what != what || cursor_folder != folder.as_str() {
        return Err(bad());
    }
    Ok((at.parse().map_err(|_| bad())?, id.to_owned()))
}

// Reads go to the cache while it works; if it fails, the cache is no
// longer used, and they go to the server (`broken`).
impl<S: MailStore> MailStore for CachedStore<S> {
    async fn own_addresses(&self) -> Result<Vec<String>> {
        self.inner.own_addresses().await
    }

    async fn search_people(&self, text: &str) -> Result<Vec<Address>> {
        self.inner.search_people(text).await
    }

    async fn list_folders(&self) -> Result<Vec<Folder>> {
        let listed = self.inner.list_folders().await;
        self.note_folders(&listed, Listing::All);
        self.or_as_seen(listed, |db| db.folders_listed(Listing::All))
    }

    async fn child_folders(&self, parent: Option<&FolderId>) -> Result<Vec<Folder>> {
        let listed = self.inner.child_folders(parent).await;
        self.note_folders(&listed, Listing::Below(parent));
        self.or_as_seen(listed, |db| db.folders_listed(Listing::Below(parent)))
    }

    async fn folder(&self, id: &FolderId) -> Result<Folder> {
        let found = self.inner.folder(id).await;
        self.note_folders(
            &found
                .as_ref()
                .map(|f| vec![f.clone()])
                .map_err(Clone::clone),
            Listing::Some,
        );
        self.or_as_seen(found, |db| db.folder_info(id))
    }

    async fn recent_unread(
        &self,
        folder: &FolderId,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<(MessageId, DateTime<Utc>)>> {
        if self.caching() {
            match self.recent_unread_cached(folder, since).await {
                Err(_) if self.broken() => {}
                found => return found,
            }
        }
        self.inner.recent_unread(folder, since).await
    }

    async fn well_known_folder(&self, which: WellKnownFolder) -> Result<Folder> {
        let found = self.inner.well_known_folder(which).await;
        self.note_folders(
            &found
                .as_ref()
                .map(|f| vec![f.clone()])
                .map_err(Clone::clone),
            Listing::Some,
        );
        self.or_as_seen(found, |db| db.well_known(which))
    }

    async fn list_messages(&self, folder: &FolderId, page: Page) -> Result<MessagePage> {
        if self.caching() {
            match self.list_cached(folder, &page).await {
                Err(_) if self.broken() => {}
                listed => return listed,
            }
        }
        let resumed = self
            .resume_listing(folder, "all", &page, received_key, |page| {
                self.inner.list_messages(folder, page)
            })
            .await;
        match resumed {
            Some(listed) => listed,
            None => self.inner.list_messages(folder, page).await,
        }
    }

    async fn changed_messages(
        &self,
        folder: &FolderId,
        since: DateTime<Utc>,
        page: Page,
    ) -> Result<MessagePage> {
        if self.caching() {
            match self.changed_cached(folder, since, &page).await {
                Err(_) if self.broken() => {}
                listed => return listed,
            }
        }
        let what = since.timestamp_millis().to_string();
        let resumed = self
            .resume_listing(folder, &what, &page, modified_key, |page| {
                self.inner.changed_messages(folder, since, page)
            })
            .await;
        match resumed {
            Some(listed) => listed,
            None => self.inner.changed_messages(folder, since, page).await,
        }
    }

    async fn fetch_message(&self, id: &MessageId) -> Result<Message> {
        if self.caching()
            && let Ok(Some(message)) = self.with_cache(|db| db.message(id))
        {
            return Ok(message);
        }
        let message = self.inner.fetch_message(id).await?;
        if self.caching() {
            // Failing to keep it costs only fetching it again.
            let _ = self.with_cache(|db| db.keep_body(&message));
        }
        Ok(message)
    }

    async fn message_folder(&self, id: &MessageId) -> Result<FolderId> {
        self.inner.message_folder(id).await
    }

    async fn list_attachments(&self, message: &MessageId) -> Result<Vec<Attachment>> {
        self.inner.list_attachments(message).await
    }

    async fn fetch_attachment(
        &self,
        message: &MessageId,
        attachment: &AttachmentId,
    ) -> Result<Vec<u8>> {
        self.inner.fetch_attachment(message, attachment).await
    }

    // Once a change is made on the server, it is: failing to note it in
    // the cache (a full disk, say) isn't failing to make it, and the next
    // sync notes it anyway.

    async fn update_flags(&self, id: &MessageId, update: FlagsUpdate) -> Result<()> {
        self.inner.update_flags(id, update).await?;
        // The server dates the change: the next sync brings its date in.
        self.wrote();
        if self.caching() {
            let _ = self.with_cache(|db| db.update_flags(id, update));
        }
        Ok(())
    }

    async fn move_message(&self, id: &MessageId, to: &FolderId) -> Result<MessageId> {
        let moved = self.inner.move_message(id, to).await;
        self.wrote();
        let moved = moved?;
        // Its new folder's sync brings it in there.
        if self.caching() {
            let _ = self.with_cache(|db| db.remove(id));
        }
        Ok(moved)
    }

    async fn copy_message(&self, id: &MessageId, to: &FolderId) -> Result<MessageId> {
        let copied = self.inner.copy_message(id, to).await;
        self.wrote();
        copied
    }

    async fn delete_message(&self, id: &MessageId) -> Result<()> {
        self.inner.delete_message(id).await?;
        self.wrote();
        if self.caching() {
            let _ = self.with_cache(|db| db.remove(id));
        }
        Ok(())
    }

    async fn save_draft(&self, draft: &Draft) -> Result<MessageId> {
        let saved = self.inner.save_draft(draft).await;
        self.wrote();
        saved
    }

    fn can_sync(&self) -> bool {
        self.inner.can_sync()
    }

    async fn sync_folder(
        &self,
        folder: &FolderId,
        state: Option<&SyncState>,
    ) -> Result<FolderChanges> {
        self.inner.sync_folder(folder, state).await
    }
}

// Sending puts mail in Sent Items, and may leave a draft: folders are
// synced again.
impl<S: MailSender + MailStore> MailSender for CachedStore<S> {
    async fn send(&self, draft: &Draft) -> Result<()> {
        let sent = self.inner.send(draft).await;
        self.wrote();
        sent
    }

    async fn discard_kept(&self, draft: &Draft) -> Result<()> {
        let discarded = self.inner.discard_kept(draft).await;
        self.wrote();
        discarded
    }

    fn keeps_draft(&self, draft: &Draft) -> bool {
        self.inner.keeps_draft(draft)
    }
}
