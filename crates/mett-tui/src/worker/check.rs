//! Checking mailboxes for new mail.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use mett_core::{
    Error, Folder, FolderId, MailStore, MessagePage, Page, Result, WellKnownFolder, join_bounded,
};

use super::{Checked, at_path, resolve_with};
use crate::config::mailbox::FolderPath;

/// How many mailboxes are checked at once: a slow one doesn't hold up the
/// others, nor do many at once flood Graph.
pub(super) const CHECKS_AT_ONCE: usize = 4;

/// Checks `mailboxes` (see [`Request::CheckMailboxes`]), a few at a time.
pub(super) async fn check_mailboxes<S: MailStore>(
    store: &S,
    mailboxes: Vec<(FolderPath, Option<FolderId>, Option<DateTime<Utc>>)>,
) -> Vec<(FolderPath, Result<Checked>)> {
    // Parents, well-known folders and the folders below each are looked up
    // once for all.
    let lookups = &Lookups::default();
    // The folders, by the IDs known, each checked once found. A folder
    // deleted (or moved away) since is looked for by its path again; so is
    // one renamed or moved since (another may be at its path now).
    let known = join_bounded(
        mailboxes
            .iter()
            .map(|(path, id, since)| async move {
                let id = id.as_ref()?;
                let found = match lookups.folder(store, id).await {
                    Err(Error::NotFound { .. }) => return None,
                    Ok(folder) => match at_path(store, lookups, path, &folder).await {
                        Ok(true) => Ok(folder),
                        Ok(false) => return None,
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(e),
                };
                Some(check(store, found, Some(id), *since).await)
            })
            .collect(),
        CHECKS_AT_ONCE,
    )
    .await;
    // The others, by their paths.
    let by_path = join_bounded(
        mailboxes
            .iter()
            .zip(&known)
            .filter(|(_, known)| known.is_none())
            .map(|((path, id, since), _)| async move {
                let found = resolve_with(store, lookups, path).await;
                check(store, found, id.as_ref(), *since).await
            })
            .collect(),
        CHECKS_AT_ONCE,
    )
    .await;
    let mut by_path = by_path.into_iter();
    mailboxes
        .into_iter()
        .zip(known)
        .map(|((path, _, _), known)| {
            let checked = known.unwrap_or_else(|| by_path.next().expect("checked by its path"));
            (path, checked)
        })
        .collect()
}

/// Checks the folder `found` for a mailbox last seen with `id` (if it was
/// found): reads its unread mail that changed `since`.
async fn check<S: MailStore>(
    store: &S,
    found: Result<Folder>,
    id: Option<&FolderId>,
    since: Option<DateTime<Utc>>,
) -> Result<Checked> {
    let folder = found?;
    // Changes since what was seen of the folder asked about, not of
    // another now at its path.
    let since = since.filter(|_| id == Some(&folder.id));
    store
        .recent_unread(&folder.id, since)
        .await
        .map(|recent| Checked { folder, recent })
}

/// What the lookups for one request found, so that each is made once:
/// checking many mailboxes looks up their common parents, and lists the
/// folders below each, once for all. A lookup that fails isn't kept: the
/// next one is made again.
#[derive(Default)]
pub(super) struct Lookups {
    /// Folders by ID.
    folders: Memo<FolderId, Folder>,
    /// Well-known folders.
    well_known: Memo<WellKnownFolder, Folder>,
    /// The folders below each folder (`None`: the top-level ones).
    children: Memo<Option<FolderId>, Arc<Vec<Folder>>>,
}

impl Lookups {
    /// The folder `id` (see [`MailStore::folder`]).
    pub(super) async fn folder<S: MailStore>(&self, store: &S, id: &FolderId) -> Result<Folder> {
        self.folders.get(id, || store.folder(id)).await
    }

    /// The well-known folder `which` (see [`MailStore::well_known_folder`]).
    pub(super) async fn well_known<S: MailStore>(
        &self,
        store: &S,
        which: WellKnownFolder,
    ) -> Result<Folder> {
        self.well_known
            .get(&which, || store.well_known_folder(which))
            .await
    }

    /// The folders below `parent` (see [`MailStore::child_folders`]).
    pub(super) async fn children<S: MailStore>(
        &self,
        store: &S,
        parent: Option<&FolderId>,
    ) -> Result<Arc<Vec<Folder>>> {
        self.children
            .get(&parent.cloned(), || async move {
                store.child_folders(parent).await.map(Arc::new)
            })
            .await
    }
}

/// Values looked up by key, each once (while one lookup runs, others for
/// the same key wait for it), unless it fails.
struct Memo<K, V>(std::sync::Mutex<HashMap<K, Arc<tokio::sync::OnceCell<V>>>>);

impl<K, V> Default for Memo<K, V> {
    fn default() -> Self {
        Self(std::sync::Mutex::default())
    }
}

impl<K: Clone + Eq + Hash, V: Clone> Memo<K, V> {
    /// The value for `key`, looked up with `look_up` if it isn't known.
    async fn get<F: Future<Output = Result<V>>>(
        &self,
        key: &K,
        look_up: impl FnOnce() -> F,
    ) -> Result<V> {
        let cell = Arc::clone(
            crate::platform::lock(&self.0)
                .entry(key.clone())
                .or_default(),
        );
        cell.get_or_try_init(look_up).await.cloned()
    }
}

/// How many pages of changes a check reads before listing the whole
/// folder again is cheaper.
const CHANGE_PAGES: usize = 10;

/// The messages of `folder` changed since `since`: up to
/// [`CHANGE_PAGES`] pages, with a cursor if there are more.
pub(super) async fn changes<S: MailStore>(
    store: &S,
    folder: &FolderId,
    since: DateTime<Utc>,
    page: Page,
) -> Result<MessagePage> {
    let size = page.size();
    let mut changed = store.changed_messages(folder, since, page).await?;
    for _ in 1..CHANGE_PAGES {
        let Some(cursor) = changed.next.take() else {
            break;
        };
        let more = store
            .changed_messages(folder, since, Page::after(cursor, size))
            .await?;
        changed.messages.extend(more.messages);
        changed.next = more.next;
    }
    Ok(changed)
}
