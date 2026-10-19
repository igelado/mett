//! [`CachedStore`]: a [`MailStore`] that keeps folders' indexes and
//! messages in SQLite, up to date by syncing them.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use mett_core::{
    Address, Attachment, AttachmentId, Draft, Error, FlagsUpdate, Folder, FolderChanges, FolderId,
    MailSender, MailStore, Message, MessageId, MessagePage, Page, PageCursor, RECENT_UNREAD,
    Result, SyncState, WellKnownFolder,
};
use tokio::time::Instant;

use crate::db::Db;

/// A folder synced this recently isn't synced again: the calls of one
/// request (a check for new mail lists a folder and its changes) share a
/// sync.
const FRESH: Duration = Duration::from_secs(2);

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
    /// When each folder was last synced, which also keeps two syncs of one
    /// folder from running at once.
    synced: Mutex<HashMap<FolderId, Arc<tokio::sync::Mutex<Option<Instant>>>>>,
}

/// A cache database, for a [`CachedStore`].
pub struct Cache(Db);

impl Cache {
    /// The cache in the file `path`, which is created (only its owner may
    /// read it, as it holds mail) if needed.
    pub fn open(path: &Path) -> std::result::Result<Self, CacheError> {
        let error = |source: Box<dyn std::error::Error + Send + Sync>| CacheError {
            path: path.display().to_string(),
            source,
        };
        private_file(path).map_err(|e| error(Box::new(e)))?;
        Db::open(path).map(Self).map_err(|e| error(Box::new(e)))
    }

    /// A cache in memory, gone when it is dropped.
    pub fn in_memory() -> std::result::Result<Self, CacheError> {
        Db::in_memory().map(Self).map_err(|e| CacheError {
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
            synced: Mutex::new(HashMap::new()),
        }
    }

    /// The store behind the cache.
    pub fn inner(&self) -> &S {
        &self.inner
    }
}

/// Creates `path` (and its directory) readable only by its owner, if it
/// doesn't exist: it holds mail.
fn private_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
    }
    #[cfg(not(unix))]
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

impl<S: MailStore> CachedStore<S> {
    /// The cache, if there is one and the backend can keep it up to date.
    fn cache(&self) -> Option<MutexGuard<'_, Db>> {
        self.db
            .as_ref()
            .filter(|_| self.inner.can_sync())
            .map(|db| db.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Whether folders are cached.
    fn caching(&self) -> bool {
        self.db.is_some() && self.inner.can_sync()
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
        if last.is_some_and(|at| at.elapsed() < FRESH) {
            return Ok(());
        }
        let mut state = match self.with_cache(|db| db.synced(folder))? {
            Some(synced) if synced.state.is_some() => synced.state,
            _ => {
                self.with_cache(|db| db.start_over(folder))?;
                None
            }
        };
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
            self.with_cache(|db| db.apply(folder, &changes))?;
            let FolderChanges {
                state: next, more, ..
            } = changes;
            state = Some(next);
            if !more {
                break;
            }
        }
        *last = Some(Instant::now());
        Ok(())
    }

    /// Runs `f` on the cache.
    fn with_cache<T>(&self, f: impl FnOnce(&mut Db) -> Result<T>) -> Result<T> {
        let mut db = self
            .cache()
            .ok_or_else(|| Error::Backend("cache: not caching".into()))?;
        f(&mut db)
    }

    /// Whether `folder` is all in the cache.
    fn complete(&self, folder: &FolderId) -> bool {
        self.with_cache(|db| db.synced(folder))
            .ok()
            .flatten()
            .is_some_and(|synced| synced.complete)
    }

    /// A page of `folder`'s summaries from the cache, from `query` (`what`
    /// the listing is, which its cursors record).
    fn cached_page(
        &self,
        folder: &FolderId,
        page: &Page,
        what: &str,
        query: impl FnOnce(&Db, u64, u64) -> Result<Vec<mett_core::MessageSummary>>,
    ) -> Result<MessagePage> {
        let offset = match page.cursor() {
            None => 0,
            Some(cursor) => parse_cursor(cursor, folder, what)?,
        };
        let size = u64::from(page.size());
        // One more than asked for tells whether there are more.
        let mut messages = self.with_cache(|db| query(db, offset, size + 1))?;
        let more = messages.len() as u64 > size;
        messages.truncate(page.size() as usize);
        Ok(MessagePage {
            messages,
            next: more.then(|| cursor(offset + size, what, folder)),
        })
    }
}

/// A page cursor into the cache: where the next page starts, in which
/// listing (`what`) of which folder.
fn cursor(offset: u64, what: &str, folder: &FolderId) -> PageCursor {
    PageCursor::new(format!("cache {offset} {what} {folder}"))
}

/// The offset in `cursor`, which must be for `what` of `folder`.
fn parse_cursor(cursor: &PageCursor, folder: &FolderId, what: &str) -> Result<u64> {
    let bad = || {
        Error::Backend(format!(
            "page cursor is not for this listing of folder {folder}"
        ))
    };
    let rest = cursor.as_str().strip_prefix("cache ").ok_or_else(bad)?;
    let (offset, rest) = rest.split_once(' ').ok_or_else(bad)?;
    let (cursor_what, cursor_folder) = rest.split_once(' ').ok_or_else(bad)?;
    if cursor_what != what || cursor_folder != folder.as_str() {
        return Err(bad());
    }
    offset.parse().map_err(|_| bad())
}

impl<S: MailStore> MailStore for CachedStore<S> {
    async fn own_addresses(&self) -> Result<Vec<String>> {
        self.inner.own_addresses().await
    }

    async fn search_people(&self, text: &str) -> Result<Vec<Address>> {
        self.inner.search_people(text).await
    }

    async fn list_folders(&self) -> Result<Vec<Folder>> {
        self.inner.list_folders().await
    }

    async fn child_folders(&self, parent: Option<&FolderId>) -> Result<Vec<Folder>> {
        self.inner.child_folders(parent).await
    }

    async fn folder(&self, id: &FolderId) -> Result<Folder> {
        self.inner.folder(id).await
    }

    async fn recent_unread(
        &self,
        folder: &FolderId,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<(MessageId, DateTime<Utc>)>> {
        if !self.caching() {
            return self.inner.recent_unread(folder, since).await;
        }
        self.sync(folder).await?;
        // Without `since`, the last few tell.
        let limit = if since.is_some() { RECENT_UNREAD } else { 50 };
        let since = since.unwrap_or(DateTime::UNIX_EPOCH);
        self.with_cache(|db| db.recent_unread(folder, since, limit as u64))
    }

    async fn well_known_folder(&self, which: WellKnownFolder) -> Result<Folder> {
        self.inner.well_known_folder(which).await
    }

    async fn list_messages(&self, folder: &FolderId, page: Page) -> Result<MessagePage> {
        if !self.caching() {
            return self.inner.list_messages(folder, page).await;
        }
        if page.cursor().is_none() {
            match self.sync(folder).await {
                // Offline, a folder all here is listed as it was.
                Err(Error::Network(_)) if self.complete(folder) => {}
                synced => synced?,
            }
        }
        self.cached_page(folder, &page, "all", |db, offset, limit| {
            db.page(folder, offset, limit)
        })
    }

    async fn changed_messages(
        &self,
        folder: &FolderId,
        since: DateTime<Utc>,
        page: Page,
    ) -> Result<MessagePage> {
        if !self.caching() {
            return self.inner.changed_messages(folder, since, page).await;
        }
        if page.cursor().is_none() {
            self.sync(folder).await?;
        }
        let what = since.timestamp_millis().to_string();
        self.cached_page(folder, &page, &what, |db, offset, limit| {
            db.changed(folder, since, offset, limit)
        })
    }

    async fn fetch_message(&self, id: &MessageId) -> Result<Message> {
        if !self.caching() {
            return self.inner.fetch_message(id).await;
        }
        if let Some(message) = self.with_cache(|db| db.message(id))? {
            return Ok(message);
        }
        let message = self.inner.fetch_message(id).await?;
        self.with_cache(|db| db.keep_body(&message))?;
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

    async fn update_flags(&self, id: &MessageId, update: FlagsUpdate) -> Result<()> {
        self.inner.update_flags(id, update).await?;
        if self.caching() {
            self.with_cache(|db| db.update_flags(id, update))?;
        }
        Ok(())
    }

    async fn move_message(&self, id: &MessageId, to: &FolderId) -> Result<MessageId> {
        let moved = self.inner.move_message(id, to).await?;
        // Its new folder's sync brings it in there.
        if self.caching() {
            self.with_cache(|db| db.remove(id))?;
        }
        Ok(moved)
    }

    async fn copy_message(&self, id: &MessageId, to: &FolderId) -> Result<MessageId> {
        self.inner.copy_message(id, to).await
    }

    async fn delete_message(&self, id: &MessageId) -> Result<()> {
        self.inner.delete_message(id).await?;
        if self.caching() {
            self.with_cache(|db| db.remove(id))?;
        }
        Ok(())
    }

    async fn save_draft(&self, draft: &Draft) -> Result<MessageId> {
        self.inner.save_draft(draft).await
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

impl<S: MailSender> MailSender for CachedStore<S> {
    async fn send(&self, draft: &Draft) -> Result<()> {
        self.inner.send(draft).await
    }

    async fn discard_kept(&self, draft: &Draft) -> Result<()> {
        self.inner.discard_kept(draft).await
    }

    fn keeps_draft(&self, draft: &Draft) -> bool {
        self.inner.keeps_draft(draft)
    }
}
