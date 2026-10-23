//! The cache database: its schema, and how summaries and messages are
//! stored in it.

use std::path::Path;

use chrono::{DateTime, Utc};
use mett_core::{
    AccountId, Address, Body, BodyKind, Error, Flags, FlagsUpdate, Folder, FolderChanges, FolderId,
    Invitation, Message, MessageId, MessageSummary, SyncState, ThreadPosition, WellKnownFolder,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::invitation::StoredInvitation;

/// The schema's version. A cache of another version is dropped and filled
/// again: it holds nothing the server doesn't.
const SCHEMA_VERSION: i64 = 8;

/// Marks mett's caches (SQLite's `application_id`): "mett".
const APPLICATION_ID: i64 = 0x6D65_7474;

const SCHEMA: &str = "
    -- Whose mailbox this is.
    CREATE TABLE account (id TEXT NOT NULL);
    CREATE TABLE folders (
        id TEXT PRIMARY KEY,
        -- Where the folder's sync stands, once it has started.
        state TEXT,
        -- Whether the sync reached the end: the folder is all here.
        complete INTEGER NOT NULL DEFAULT 0,
        -- Whether a sync ever reached the end: the folder is here as it
        -- was then, at least, to list offline.
        snapshot INTEGER NOT NULL DEFAULT 0,
        -- Whether the sync started over: what it doesn't report again is
        -- gone, once it is complete.
        scratch INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE messages (
        id TEXT PRIMARY KEY,
        folder TEXT NOT NULL,
        -- The summary's dates, in microseconds, to sort and filter by.
        received INTEGER NOT NULL,
        modified INTEGER,
        unread INTEGER NOT NULL,
        flagged INTEGER NOT NULL,
        -- The rest of the summary, as JSON.
        summary TEXT NOT NULL,
        -- The rest of the message, once it was fetched.
        extras TEXT,
        -- Whether the sync that started over reported it.
        seen INTEGER NOT NULL DEFAULT 1
    );
    -- The folders as last seen, to find them offline.
    CREATE TABLE folder_info (
        id TEXT PRIMARY KEY,
        parent TEXT,
        -- Where it was in the last listing that had it.
        ord INTEGER NOT NULL,
        folder TEXT NOT NULL
    );
    CREATE TABLE well_known (
        name TEXT PRIMARY KEY,
        id TEXT NOT NULL
    );
    -- The folder listings seen: `*` for all folders, `below:<id>` for
    -- those below a folder (`below:` for the top-level ones).
    CREATE TABLE listed (what TEXT PRIMARY KEY);
    CREATE INDEX messages_by_date ON messages (folder, received DESC, id);
    CREATE INDEX messages_by_change ON messages (folder, modified DESC, id);
    -- To find what is unread among the recent changes.
    CREATE INDEX messages_unread ON messages (folder, modified DESC, id)
        WHERE unread;
";

/// Where a message is in a listing: its date there, in microseconds, and
/// its ID (listings go by date, newest first, then by ID).
pub(crate) type Key = (i64, String);

/// `summary`'s key in a listing by date ([`Db::page`]).
pub(crate) fn received_key(summary: &MessageSummary) -> Key {
    (summary.received.timestamp_micros(), summary.id.to_string())
}

/// `summary`'s key in a listing by change ([`Db::changed`]).
pub(crate) fn modified_key(summary: &MessageSummary) -> Key {
    (
        summary.modified.map_or(i64::MIN, |m| m.timestamp_micros()),
        summary.id.to_string(),
    )
}

/// Whether the message of key `key` comes after the one of `after` in a
/// listing.
pub(crate) fn comes_after(key: &Key, after: &Key) -> bool {
    key.0 < after.0 || (key.0 == after.0 && key.1 > after.1)
}

/// Checks that `conn` is `account`'s cache, or a new database (`true`): a
/// database of something else, or another account's cache (whatever its
/// schema), is never touched.
fn identify(conn: &Connection, account: &AccountId) -> Result<bool, OpenError> {
    let id: i64 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
    if id != APPLICATION_ID {
        let tables: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))?;
        if tables > 0 {
            return Err(OpenError::Foreign);
        }
        return Ok(true);
    }
    let owner: Option<String> = conn
        .query_row("SELECT id FROM account", [], |r| r.get(0))
        .optional()
        .ok()
        .flatten();
    if let Some(owner) = owner.filter(|owner| owner != account.as_str()) {
        return Err(OpenError::Account(owner));
    }
    Ok(false)
}

/// Why the cache could not be opened.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OpenError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// It holds another account's mailbox.
    #[error("it holds the mailbox of {0}; give each account its own header_cache")]
    Account(String),
    /// It is another program's database.
    #[error("it is a database, but not mett's cache")]
    Foreign,
}

/// Which folders a listing gave.
#[derive(Clone, Copy)]
pub(crate) enum Listing<'a> {
    /// All of them.
    All,
    /// Those directly below a folder (without one, the top-level ones).
    Below(Option<&'a FolderId>),
    /// Some, such as one looked up.
    Some,
}

impl Listing<'_> {
    /// How the `listed` table names this listing.
    fn name(self) -> Option<String> {
        match self {
            Self::All => Some("*".into()),
            Self::Below(parent) => Some(format!(
                "below:{}",
                parent.map(FolderId::as_str).unwrap_or_default()
            )),
            Self::Some => None,
        }
    }
}

/// Where a folder's sync stands.
pub(crate) struct Synced {
    pub state: Option<SyncState>,
    pub complete: bool,
    /// Whether a sync of it ever completed: it can be listed offline.
    pub snapshot: bool,
}

/// The cache database.
pub(crate) struct Db {
    conn: Connection,
    /// Whether a message's body is good whatever happens to the message
    /// (the backend says its bodies don't change): it is kept when the
    /// message changes, and kept whatever version of the message it was
    /// fetched with.
    bodies_stay: bool,
}

/// How a failure because the database was busy begins.
const BUSY: &str = "cache busy: ";

/// A cache failure, as a port error. A database that is busy or locked
/// (another mett holds it) is told from the rest by [`is_busy`].
fn failed<E: std::fmt::Display + 'static>(e: E) -> Error {
    let busy = (&e as &dyn std::any::Any)
        .downcast_ref::<rusqlite::Error>()
        .and_then(rusqlite::Error::sqlite_error_code)
        .is_some_and(|code| {
            matches!(
                code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
        });
    Error::Backend(format!("{}{e}", if busy { BUSY } else { "cache: " }))
}

/// Whether `e` is a cache failure because the database was busy or locked,
/// which passes, unlike a full disk or a corrupt file.
pub(crate) fn is_busy(e: &Error) -> bool {
    matches!(e, Error::Backend(m) if m.starts_with(BUSY))
}

impl Db {
    /// Checks, writing nothing, that the database at `path` is `account`'s
    /// cache, or new.
    pub fn check(path: &Path, account: &AccountId) -> Result<(), OpenError> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        identify(&conn, account).map(drop)
    }

    /// Opens (creating it if needed) the cache of `account`'s mailbox at
    /// `path`.
    pub fn open(path: &Path, account: &AccountId) -> Result<Self, OpenError> {
        Self::ready(Connection::open(path)?, account)
    }

    /// A cache in memory, for tests.
    pub fn in_memory(account: &AccountId) -> Result<Self, OpenError> {
        Self::ready(Connection::open_in_memory()?, account)
    }

    /// Readies the cache for `account`: one of another schema version is
    /// made afresh. One of another account isn't used: that account's mett
    /// may have it open. Another mett may open it at once: this is one
    /// transaction, which waits for the other.
    fn ready(mut conn: Connection, account: &AccountId) -> Result<Self, OpenError> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // Nothing is changed before it is known to be this account's cache
        // (or new); and again, as another mett may have changed it since.
        identify(&conn, account)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // The cache can be made again from the server: a crash may lose
        // the last changes, but not corrupt it.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if identify(&tx, account)? {
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
        }
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            tx.execute_batch(
                "DROP INDEX IF EXISTS messages_by_date;
                 DROP INDEX IF EXISTS messages_by_change;
                 DROP INDEX IF EXISTS messages_unread;
                 DROP TABLE IF EXISTS messages;
                 DROP TABLE IF EXISTS folders;
                 DROP TABLE IF EXISTS folder_info;
                 DROP TABLE IF EXISTS well_known;
                 DROP TABLE IF EXISTS listed;
                 DROP TABLE IF EXISTS account;",
            )?;
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        let owner: Option<String> = tx
            .query_row("SELECT id FROM account", [], |r| r.get(0))
            .optional()?;
        if owner.is_none() {
            tx.execute("INSERT INTO account (id) VALUES (?1)", [account.as_str()])?;
        }
        tx.commit()?;
        Ok(Self {
            conn,
            bodies_stay: false,
        })
    }

    /// Says whether bodies stay good (see the field).
    pub fn set_bodies_stay(&mut self, stay: bool) {
        self.bodies_stay = stay;
    }

    /// Where `folder`'s sync stands, if it has started.
    pub fn synced(&self, folder: &FolderId) -> Result<Option<Synced>, Error> {
        self.conn
            .query_row(
                "SELECT state, complete, snapshot FROM folders WHERE id = ?1",
                [folder.as_str()],
                |r| {
                    Ok(Synced {
                        state: r.get::<_, Option<String>>(0)?.map(SyncState::new),
                        complete: r.get(1)?,
                        snapshot: r.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(failed)
    }

    /// Starts syncing `folder` over: what the sync doesn't report again is
    /// gone once it is complete.
    pub fn start_over(&mut self, folder: &FolderId) -> Result<(), Error> {
        let tx = self.conn.transaction().map_err(failed)?;
        tx.execute(
            "UPDATE messages SET seen = 0 WHERE folder = ?1",
            [folder.as_str()],
        )
        .map_err(failed)?;
        tx.execute(
            "INSERT INTO folders (id, state, complete, scratch) VALUES (?1, NULL, 0, 1)
             ON CONFLICT (id) DO UPDATE SET state = NULL, complete = 0, scratch = 1",
            [folder.as_str()],
        )
        .map_err(failed)?;
        tx.commit().map_err(failed)
    }

    /// Takes in a batch of `folder`'s changes, synced from the state
    /// `from`, and where its sync stands after them, and returns how many
    /// more of its messages are here (as [`Db::count`] counts them) now, if
    /// asked to `count` (else 0); unless another mett (sharing the cache)
    /// took in changes since `from`: then nothing is done, and this is
    /// `None`.
    pub fn apply(
        &mut self,
        folder: &FolderId,
        from: Option<&SyncState>,
        changes: &FolderChanges,
        count: bool,
    ) -> Result<Option<i64>, Error> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(failed)?;
        // A change the backend doesn't date happened by now.
        let synced_at = Utc::now();
        let now: Option<String> = tx
            .query_row(
                "SELECT state FROM folders WHERE id = ?1",
                [folder.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(failed)?
            .flatten();
        if now.as_deref() != from.map(SyncState::as_str) {
            return Ok(None);
        }
        // Whether a message counts already, as one of the folder's seen.
        let counted = |id: &str| -> Result<bool, Error> {
            if !count {
                return Ok(false);
            }
            Ok(tx
                .prepare_cached("SELECT folder = ?2 AND seen FROM messages WHERE id = ?1")
                .and_then(|mut q| {
                    q.query_row([id, folder.as_str()], |r| r.get::<_, bool>(0))
                        .optional()
                })
                .map_err(failed)?
                .unwrap_or(false))
        };
        let mut more = 0;
        for summary in &changes.changed {
            if count && !counted(summary.id.as_str())? {
                more += 1;
            }
            let mut summary = summary.clone();
            summary.modified.get_or_insert(synced_at);
            let stored = serde_json::to_string(&StoredSummary::from(&summary)).map_err(failed)?;
            // A message whose change date moved may have a new body (EWS
            // lets bodies be edited), and a draft's may change without
            // one: either is fetched again.
            tx.prepare_cached(
                "INSERT INTO messages
                    (id, folder, received, modified, unread, flagged, summary, seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)
                 ON CONFLICT (id) DO UPDATE SET
                    folder = excluded.folder, received = excluded.received,
                    modified = excluded.modified, unread = excluded.unread,
                    flagged = excluded.flagged, summary = excluded.summary, seen = 1,
                    extras = CASE WHEN ?8 OR (NOT ?9 AND modified IS NOT excluded.modified)
                        THEN NULL ELSE extras END",
            )
            .and_then(|mut upsert| {
                upsert.execute(params![
                    summary.id.as_str(),
                    folder.as_str(),
                    summary.received.timestamp_micros(),
                    summary.modified.map(|m| m.timestamp_micros()),
                    !summary.flags.read,
                    summary.flags.flagged,
                    stored,
                    summary.draft,
                    self.bodies_stay,
                ])
            })
            .map_err(failed)?;
        }
        for id in &changes.removed {
            if counted(id.as_str())? {
                more -= 1;
            }
            tx.prepare_cached("DELETE FROM messages WHERE id = ?1 AND folder = ?2")
                .and_then(|mut delete| delete.execute([id.as_str(), folder.as_str()]))
                .map_err(failed)?;
        }
        let complete = !changes.more;
        tx.execute(
            "INSERT INTO folders (id, state, complete) VALUES (?1, ?2, ?3)
             ON CONFLICT (id) DO UPDATE SET state = excluded.state, complete = excluded.complete",
            params![folder.as_str(), changes.state.as_str(), complete],
        )
        .map_err(failed)?;
        if complete {
            tx.execute(
                "DELETE FROM messages WHERE folder = ?1 AND seen = 0
                 AND (SELECT scratch FROM folders WHERE id = ?1)",
                [folder.as_str()],
            )
            .map_err(failed)?;
            tx.execute(
                "UPDATE folders SET scratch = 0, snapshot = 1 WHERE id = ?1",
                [folder.as_str()],
            )
            .map_err(failed)?;
        }
        tx.commit().map_err(failed)?;
        Ok(Some(more))
    }

    /// How many of `folder`'s messages are here (and, while it syncs
    /// from the start, seen again).
    pub fn count(&self, folder: &FolderId) -> Result<u64, Error> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE folder = ?1 AND seen",
                [folder.as_str()],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n.unsigned_abs())
            .map_err(failed)
    }

    /// Up to `limit` of `folder`'s summaries, newest first (by
    /// [`received_key`], then ID), after the one `after` is the key of.
    ///
    /// The next page is a range on the index (`received <= at`), which
    /// SQLite seeks to, not an `OR` it scans through from the start.
    pub fn page(
        &self,
        folder: &FolderId,
        after: Option<&Key>,
        limit: u64,
    ) -> Result<Vec<MessageSummary>, Error> {
        match after {
            None => self.summaries(
                concat!(
                    "SELECT ",
                    summary_columns!(),
                    " FROM messages WHERE folder = ?1
                     ORDER BY received DESC, id LIMIT ?2"
                ),
                params![folder.as_str(), sql(limit)],
            ),
            Some((at, id)) => self.summaries(
                concat!(
                    "SELECT ",
                    summary_columns!(),
                    " FROM messages WHERE folder = ?1
                     AND received <= ?2 AND (received < ?2 OR id > ?3)
                     ORDER BY received DESC, id LIMIT ?4"
                ),
                params![folder.as_str(), at, id, sql(limit)],
            ),
        }
    }

    /// Up to `limit` of `folder`'s summaries changed at or after `since`,
    /// most recently changed first (by [`modified_key`], then ID), after
    /// the one `after` is the key of.
    pub fn changed(
        &self,
        folder: &FolderId,
        since: DateTime<Utc>,
        after: Option<&Key>,
        limit: u64,
    ) -> Result<Vec<MessageSummary>, Error> {
        let since = since.timestamp_micros();
        match after {
            None => self.summaries(
                concat!(
                    "SELECT ",
                    summary_columns!(),
                    " FROM messages
                     WHERE folder = ?1 AND modified >= ?2
                     ORDER BY modified DESC, id LIMIT ?3"
                ),
                params![folder.as_str(), since, sql(limit)],
            ),
            Some((at, id)) => self.summaries(
                concat!(
                    "SELECT ",
                    summary_columns!(),
                    " FROM messages WHERE folder = ?1
                     AND modified >= ?2 AND modified <= ?3 AND (modified < ?3 OR id > ?4)
                     ORDER BY modified DESC, id LIMIT ?5"
                ),
                params![folder.as_str(), since, at, id, sql(limit)],
            ),
        }
    }

    /// Up to `limit` of `folder`'s unread messages changed at or after
    /// `since`, most recently changed first, with when.
    pub fn recent_unread(
        &self,
        folder: &FolderId,
        since: DateTime<Utc>,
        limit: u64,
    ) -> Result<Vec<(MessageId, DateTime<Utc>)>, Error> {
        let mut statement = self
            .conn
            .prepare_cached(
                "SELECT id, modified FROM messages
                 WHERE folder = ?1 AND unread AND modified >= ?2
                 ORDER BY modified DESC, id LIMIT ?3",
            )
            .map_err(failed)?;
        let rows = statement
            .query_map(
                params![folder.as_str(), since.timestamp_micros(), sql(limit)],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .map_err(failed)?;
        rows.map(|row| {
            let (id, modified) = row.map_err(failed)?;
            Ok((MessageId::new(id), time(modified)?))
        })
        .collect()
    }

    fn summaries(
        &self,
        query: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<MessageSummary>, Error> {
        let mut statement = self.conn.prepare_cached(query).map_err(failed)?;
        let rows = statement
            .query_map(params, StoredRow::read)
            .map_err(failed)?;
        rows.map(|row| row.map_err(failed)?.into_summary())
            .collect()
    }

    /// The full message `id`, if it is here with its body.
    pub fn message(&self, id: &MessageId) -> Result<Option<Message>, Error> {
        let row = self
            .conn
            .prepare_cached(concat!(
                "SELECT ",
                summary_columns!(),
                ", extras FROM messages WHERE id = ?1"
            ))
            .and_then(|mut q| {
                q.query_row([id.as_str()], |r| {
                    Ok((StoredRow::read(r)?, r.get::<_, Option<String>>(7)?))
                })
                .optional()
            })
            .map_err(failed)?;
        let Some((row, Some(extras))) = row else {
            return Ok(None);
        };
        let extras: StoredExtras = serde_json::from_str(&extras).map_err(failed)?;
        Ok(Some(extras.into_message(row.into_summary()?)))
    }

    /// Keeps the body of `message`, fetched just now, if its summary is
    /// here (in a folder being cached) and it isn't a draft, which may
    /// change.
    pub fn keep_body(&self, message: &Message) -> Result<(), Error> {
        // Only as the summary here is: a sync may have brought a newer one
        // (whose body this isn't) while it was fetched.
        if message.summary.draft {
            return Ok(());
        }
        let Some(modified) = message
            .summary
            .modified
            .or(self.bodies_stay.then_some(chrono::DateTime::UNIX_EPOCH))
        else {
            return Ok(());
        };
        let extras = serde_json::to_string(&StoredExtras::from(message)).map_err(failed)?;
        self.conn
            .prepare_cached(
                "UPDATE messages SET extras = ?2 WHERE id = ?1 AND (?4 OR modified = ?3)",
            )
            .and_then(|mut update| {
                update.execute(params![
                    message.summary.id.as_str(),
                    extras,
                    modified.timestamp_micros(),
                    self.bodies_stay
                ])
            })
            .map_err(failed)?;
        Ok(())
    }

    /// Changes the flags of message `id`, if it is here.
    pub fn update_flags(&self, id: &MessageId, update: FlagsUpdate) -> Result<(), Error> {
        let flags = self
            .conn
            .prepare_cached("SELECT unread, flagged FROM messages WHERE id = ?1")
            .and_then(|mut q| {
                q.query_row([id.as_str()], |r| {
                    Ok(Flags {
                        read: !r.get::<_, bool>(0)?,
                        flagged: r.get(1)?,
                    })
                })
                .optional()
            })
            .map_err(failed)?;
        let Some(mut flags) = flags else {
            return Ok(());
        };
        update.apply(&mut flags);
        self.conn
            .prepare_cached("UPDATE messages SET unread = ?2, flagged = ?3 WHERE id = ?1")
            .and_then(|mut u| u.execute(params![id.as_str(), !flags.read, flags.flagged]))
            .map_err(failed)?;
        Ok(())
    }

    /// Keeps `folders`, as `listing` gave them, in its order. A listing of
    /// them all replaces what was kept.
    pub fn keep_folders(&mut self, folders: &[Folder], listing: Listing<'_>) -> Result<(), Error> {
        let tx = self.conn.transaction().map_err(failed)?;
        if matches!(listing, Listing::All) {
            tx.execute_batch(
                "DELETE FROM folder_info; DELETE FROM well_known; DELETE FROM listed;",
            )
            .map_err(failed)?;
        }
        // The folders found gone, whose mail goes too.
        let mut purged: Vec<String> = Vec::new();
        if let Some(name) = listing.name() {
            tx.execute("INSERT OR IGNORE INTO listed (what) VALUES (?1)", [name])
                .map_err(failed)?;
        }
        // A listing of the folders below one replaces those kept there:
        // those it leaves out are gone, with the folders below them.
        if let Listing::Below(parent) = listing {
            let mut gone: Vec<String> = {
                let mut statement = tx
                    .prepare("SELECT id FROM folder_info WHERE parent IS ?1")
                    .map_err(failed)?;
                let rows = statement
                    .query_map([parent.map(FolderId::as_str)], |r| r.get::<_, String>(0))
                    .map_err(failed)?;
                rows.collect::<rusqlite::Result<_>>().map_err(failed)?
            };
            gone.retain(|id| !folders.iter().any(|f| f.id.as_str() == id));
            while let Some(id) = gone.pop() {
                let mut statement = tx
                    .prepare_cached("SELECT id FROM folder_info WHERE parent = ?1")
                    .map_err(failed)?;
                let below = statement
                    .query_map([&id], |r| r.get::<_, String>(0))
                    .map_err(failed)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(failed)?;
                gone.extend(below);
                tx.prepare_cached("DELETE FROM folder_info WHERE id = ?1")
                    .and_then(|mut q| q.execute([&id]))
                    .map_err(failed)?;
                tx.prepare_cached("DELETE FROM well_known WHERE id = ?1")
                    .and_then(|mut q| q.execute([&id]))
                    .map_err(failed)?;
                purged.push(id);
            }
        }
        for (ord, folder) in folders.iter().enumerate() {
            // A lookup that doesn't tell a folder's role (Graph's, before it
            // knows them all) leaves the role known.
            let mut folder = folder.clone();
            if folder.well_known.is_none() && !matches!(listing, Listing::All) {
                folder.well_known = tx
                    .prepare_cached("SELECT name FROM well_known WHERE id = ?1")
                    .and_then(|mut q| {
                        q.query_row([folder.id.as_str()], |r| r.get::<_, String>(0))
                            .optional()
                    })
                    .map_err(failed)?
                    .and_then(|name| name.parse().ok());
            }
            let folder = &folder;
            let stored = serde_json::to_string(&StoredFolder::from(folder)).map_err(failed)?;
            // A folder looked up alone keeps its place among the others
            // (a new one goes last).
            let ord = match listing {
                Listing::Some => None,
                _ => Some(sql(ord as u64)),
            };
            tx.prepare_cached(
                "INSERT INTO folder_info (id, parent, ord, folder) VALUES (?1, ?2,
                    COALESCE(?3, (SELECT COALESCE(MAX(ord) + 1, 0) FROM folder_info)), ?4)
                 ON CONFLICT (id) DO UPDATE SET
                    parent = excluded.parent, folder = excluded.folder,
                    ord = COALESCE(?3, ord)",
            )
            .and_then(|mut q| {
                q.execute(params![
                    folder.id.as_str(),
                    folder.parent.as_ref().map(FolderId::as_str),
                    ord,
                    stored
                ])
            })
            .map_err(failed)?;
            if let Some(which) = folder.well_known {
                tx.prepare_cached(
                    "INSERT INTO well_known (name, id) VALUES (?1, ?2)
                     ON CONFLICT (name) DO UPDATE SET id = excluded.id",
                )
                .and_then(|mut q| q.execute([which.as_str(), folder.id.as_str()]))
                .map_err(failed)?;
            }
        }
        // A listing of them all leaves out every folder gone.
        if matches!(listing, Listing::All) {
            tx.execute_batch(
                "DELETE FROM messages WHERE folder NOT IN (SELECT id FROM folder_info);
                 DELETE FROM folders WHERE id NOT IN (SELECT id FROM folder_info);",
            )
            .map_err(failed)?;
        }
        for id in &purged {
            tx.prepare_cached("DELETE FROM messages WHERE folder = ?1")
                .and_then(|mut q| q.execute([id]))
                .map_err(failed)?;
            tx.prepare_cached("DELETE FROM folders WHERE id = ?1")
                .and_then(|mut q| q.execute([id]))
                .map_err(failed)?;
        }
        tx.commit().map_err(failed)
    }

    /// Folder `id`, as last seen.
    pub fn folder_info(&self, id: &FolderId) -> Result<Option<Folder>, Error> {
        Ok(self
            .folders(
                "SELECT folder FROM folder_info WHERE id = ?1",
                [id.as_str()],
            )?
            .pop())
    }

    /// The folder with the role `which`, as last seen.
    pub fn well_known(&self, which: WellKnownFolder) -> Result<Option<Folder>, Error> {
        Ok(self
            .folders(
                "SELECT folder FROM folder_info
                 WHERE id = (SELECT id FROM well_known WHERE name = ?1)",
                [which.as_str()],
            )?
            .pop())
    }

    /// The folders `listing` gives (but [`Listing::Some`]), as last seen,
    /// if such a listing was seen (one of them all tells those below any
    /// folder).
    pub fn folders_listed(&self, listing: Listing<'_>) -> Result<Option<Vec<Folder>>, Error> {
        let Some(name) = listing.name() else {
            return Ok(None);
        };
        let seen: bool = self
            .conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM listed WHERE what IN (?1, '*'))",
                [name],
                |r| r.get(0),
            )
            .map_err(failed)?;
        if !seen {
            return Ok(None);
        }
        let folders = match listing {
            Listing::Below(None) => self.folders(
                "SELECT folder FROM folder_info WHERE parent IS NULL ORDER BY ord",
                [],
            ),
            Listing::Below(Some(parent)) => self.folders(
                "SELECT folder FROM folder_info WHERE parent = ?1 ORDER BY ord",
                [parent.as_str()],
            ),
            _ => self.folders("SELECT folder FROM folder_info ORDER BY ord", []),
        }?;
        Ok(Some(folders))
    }

    fn folders(&self, query: &str, params: impl rusqlite::Params) -> Result<Vec<Folder>, Error> {
        let mut statement = self.conn.prepare_cached(query).map_err(failed)?;
        let rows = statement
            .query_map(params, |r| r.get::<_, String>(0))
            .map_err(failed)?;
        rows.map(|row| {
            let stored: StoredFolder =
                serde_json::from_str(&row.map_err(failed)?).map_err(failed)?;
            Ok(stored.into())
        })
        .collect()
    }

    /// Forgets message `id`.
    pub fn remove(&self, id: &MessageId) -> Result<(), Error> {
        self.conn
            .prepare_cached("DELETE FROM messages WHERE id = ?1")
            .and_then(|mut q| q.execute([id.as_str()]))
            .map_err(failed)?;
        Ok(())
    }
}

/// A count, as SQLite takes it (which is signed).
fn sql(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// A time in microseconds, as stored.
fn time(micros: i64) -> Result<DateTime<Utc>, Error> {
    DateTime::from_timestamp_micros(micros).ok_or_else(|| failed("a date out of range"))
}

/// An optional time, as stored.
fn maybe_time(micros: Option<i64>) -> Result<Option<DateTime<Utc>>, Error> {
    micros.map(time).transpose()
}

/// A [`Folder`], as stored.
#[derive(Serialize, Deserialize)]
struct StoredFolder {
    id: String,
    parent: Option<String>,
    name: String,
    well_known: Option<String>,
    unread: u32,
    total: u32,
}

impl From<&Folder> for StoredFolder {
    fn from(f: &Folder) -> Self {
        Self {
            id: f.id.to_string(),
            parent: f.parent.as_ref().map(ToString::to_string),
            name: f.name.clone(),
            well_known: f.well_known.map(|w| w.as_str().to_owned()),
            unread: f.unread_count,
            total: f.total_count,
        }
    }
}

impl From<StoredFolder> for Folder {
    fn from(f: StoredFolder) -> Self {
        Self {
            id: FolderId::new(f.id),
            parent: f.parent.map(FolderId::new),
            name: f.name,
            well_known: f.well_known.and_then(|w| w.parse().ok()),
            unread_count: f.unread,
            total_count: f.total,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredAddress {
    name: Option<String>,
    email: String,
}

impl From<&Address> for StoredAddress {
    fn from(a: &Address) -> Self {
        Self {
            name: a.name.clone(),
            email: a.email.clone(),
        }
    }
}

impl From<StoredAddress> for Address {
    fn from(a: StoredAddress) -> Self {
        Self {
            name: a.name,
            email: a.email,
        }
    }
}

/// The columns of a message's row that make its summary (as a macro, to
/// make the queries' text at compile time).
macro_rules! summary_columns {
    () => {
        "id, folder, received, modified, unread, flagged, summary"
    };
}
use summary_columns;

/// A [`MessageSummary`], as its row stores it: the columns that are
/// sorted or filtered by (or changed alone, as the flags are), and the rest
/// as JSON.
struct StoredRow {
    id: String,
    folder: String,
    received: i64,
    modified: Option<i64>,
    unread: bool,
    flagged: bool,
    rest: String,
}

impl StoredRow {
    /// The row of the columns of `summary_columns!`.
    fn read(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            folder: r.get(1)?,
            received: r.get(2)?,
            modified: r.get(3)?,
            unread: r.get(4)?,
            flagged: r.get(5)?,
            rest: r.get(6)?,
        })
    }

    fn into_summary(self) -> Result<MessageSummary, Error> {
        let rest: StoredSummary = serde_json::from_str(&self.rest).map_err(failed)?;
        Ok(MessageSummary {
            id: MessageId::new(self.id),
            folder: FolderId::new(self.folder),
            subject: rest.subject,
            from: rest.from.map(Address::from),
            to: rest.to.into_iter().map(Address::from).collect(),
            received: time(self.received)?,
            flags: Flags {
                read: !self.unread,
                flagged: self.flagged,
            },
            has_attachments: rest.has_attachments,
            draft: rest.draft,
            delivered: maybe_time(rest.delivered)?,
            modified: maybe_time(self.modified)?,
            thread: rest
                .conversation
                .map(|conversation| {
                    Ok::<_, Error>(ThreadPosition {
                        conversation,
                        path: rest
                            .thread_path
                            .iter()
                            .map(|p| unhex(p))
                            .collect::<Result<_, _>>()?,
                    })
                })
                .transpose()?,
        })
    }
}

/// What of a [`MessageSummary`] isn't in columns, as stored.
#[derive(Serialize, Deserialize)]
struct StoredSummary {
    subject: String,
    from: Option<StoredAddress>,
    to: Vec<StoredAddress>,
    has_attachments: bool,
    draft: bool,
    delivered: Option<i64>,
    conversation: Option<String>,
    /// The thread path's elements, in hex (as numbers, JSON is about four
    /// times the size).
    thread_path: Vec<String>,
}

impl From<&MessageSummary> for StoredSummary {
    fn from(s: &MessageSummary) -> Self {
        Self {
            subject: s.subject.clone(),
            from: s.from.as_ref().map(StoredAddress::from),
            to: s.to.iter().map(StoredAddress::from).collect(),
            has_attachments: s.has_attachments,
            draft: s.draft,
            delivered: s.delivered.map(|d| d.timestamp_micros()),
            conversation: s.thread.as_ref().map(|t| t.conversation.clone()),
            thread_path: s
                .thread
                .as_ref()
                .map(|t| t.path.iter().map(|p| hex(p)).collect())
                .unwrap_or_default(),
        }
    }
}

/// `bytes` in lower-case hex.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// The bytes `hex` is of.
fn unhex(hex: &str) -> Result<Vec<u8>, Error> {
    let bad = || failed("a thread path that isn't hex");
    if !hex.is_ascii() || !hex.len().is_multiple_of(2) {
        return Err(bad());
    }
    hex.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|p| u8::from_str_radix(p, 16).ok())
                .ok_or_else(bad)
        })
        .collect()
}

/// The parts of a [`Message`] besides its summary, as stored.
#[derive(Serialize, Deserialize)]
struct StoredExtras {
    cc: Vec<StoredAddress>,
    bcc: Vec<StoredAddress>,
    reply_to: Vec<StoredAddress>,
    internet_message_id: Option<String>,
    html: bool,
    content: String,
    invitation: Option<StoredInvitation>,
}

impl From<&Message> for StoredExtras {
    fn from(m: &Message) -> Self {
        let addresses = |list: &[Address]| list.iter().map(StoredAddress::from).collect();
        Self {
            cc: addresses(&m.cc),
            bcc: addresses(&m.bcc),
            reply_to: addresses(&m.reply_to),
            internet_message_id: m.internet_message_id.clone(),
            html: m.body.kind == BodyKind::Html,
            content: m.body.content.clone(),
            invitation: m.invitation.as_ref().map(StoredInvitation::from),
        }
    }
}

impl StoredExtras {
    fn into_message(self, summary: MessageSummary) -> Message {
        let addresses = |list: Vec<StoredAddress>| list.into_iter().map(Address::from).collect();
        Message {
            summary,
            cc: addresses(self.cc),
            bcc: addresses(self.bcc),
            reply_to: addresses(self.reply_to),
            internet_message_id: self.internet_message_id,
            body: Body {
                kind: if self.html {
                    BodyKind::Html
                } else {
                    BodyKind::Text
                },
                content: self.content,
            },
            invitation: self.invitation.map(Invitation::from),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(id: &str) -> Folder {
        Folder {
            id: FolderId::new(id),
            parent: None,
            name: id.to_owned(),
            well_known: None,
            unread_count: 0,
            total_count: 0,
        }
    }

    fn summary(id: &str, received: i64, modified: i64) -> MessageSummary {
        let mut summary = mett_core::mock::message(
            id,
            "in",
            "hi",
            DateTime::from_timestamp_micros(received).unwrap(),
        )
        .summary;
        summary.modified = DateTime::from_timestamp_micros(modified);
        summary
    }

    fn changes(summaries: Vec<MessageSummary>) -> FolderChanges {
        FolderChanges {
            changed: summaries,
            removed: Vec::new(),
            state: SyncState::new("s"),
            more: false,
        }
    }

    /// A cache whose Inbox holds `summaries`.
    fn filled(summaries: Vec<MessageSummary>) -> Db {
        let mut db = Db::in_memory(&AccountId::new("a@b.c")).unwrap();
        let inbox = FolderId::new("in");
        db.start_over(&inbox).unwrap();
        db.apply(&inbox, None, &changes(summaries), false).unwrap();
        db
    }

    #[test]
    fn pages_go_across_messages_of_one_date() {
        // Five messages of one date, and one older and one newer.
        let mut all = vec![summary("old", 1_000, 1_000), summary("new", 3_000, 3_000)];
        all.extend(["c", "a", "e", "b", "d"].map(|id| summary(id, 2_000, 2_000)));
        let db = filled(all);
        let inbox = FolderId::new("in");
        let since = DateTime::from_timestamp_micros(0).unwrap();
        type Lister = Box<dyn Fn(&Db, Option<&Key>, u64) -> Vec<MessageSummary>>;
        let by_date: Lister = {
            let inbox = inbox.clone();
            Box::new(move |db, after, n| db.page(&inbox, after, n).unwrap())
        };
        let by_change: Lister =
            Box::new(move |db, after, n| db.changed(&inbox, since, after, n).unwrap());
        for (list, key) in [
            (by_date, received_key as fn(&MessageSummary) -> Key),
            (by_change, modified_key),
        ] {
            for size in 1..=4 {
                let mut got: Vec<String> = Vec::new();
                let mut after: Option<Key> = None;
                loop {
                    let page = list(&db, after.as_ref(), size);
                    if page.is_empty() {
                        break;
                    }
                    got.extend(page.iter().map(|s| s.id.to_string()));
                    after = page.last().map(key);
                }
                assert_eq!(got, ["new", "a", "b", "c", "d", "e", "old"], "by {size}");
            }
        }
    }

    #[test]
    fn flags_change_in_place() {
        let mut thread = summary("m", 1_000, 1_000);
        thread.thread = Some(ThreadPosition {
            conversation: "conv".into(),
            path: vec![vec![0, 255, 16], vec![], vec![7]],
        });
        let other = summary("n", 2_000, 2_000);
        let db = filled(vec![thread.clone(), other]);
        let inbox = FolderId::new("in");
        let since = DateTime::from_timestamp_micros(0).unwrap();
        let unread = |db: &Db| {
            db.recent_unread(&inbox, since, 10)
                .unwrap()
                .into_iter()
                .map(|(id, _)| id.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(unread(&db), ["n", "m"]);
        db.update_flags(&thread.id, FlagsUpdate::read(true))
            .unwrap();
        db.update_flags(&thread.id, FlagsUpdate::flagged(true))
            .unwrap();
        assert_eq!(unread(&db), ["n"]);
        let page = db.page(&inbox, None, 10).unwrap();
        let m = page.iter().find(|s| s.id.as_str() == "m").unwrap();
        assert_eq!(
            m.flags,
            Flags {
                read: true,
                flagged: true
            }
        );
        // All else as it was, the thread's path too.
        let mut expected = thread;
        expected.flags = m.flags;
        assert_eq!(*m, expected);
        db.update_flags(&expected.id, FlagsUpdate::read(false))
            .unwrap();
        assert_eq!(unread(&db), ["n", "m"]);
        // A message that isn't here is left alone.
        db.update_flags(&MessageId::new("gone"), FlagsUpdate::read(true))
            .unwrap();
    }

    #[test]
    fn a_busy_database_is_told_from_a_broken_one() {
        let dir = std::env::temp_dir().join(format!("mett-sqlite-busy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cache.sqlite");
        let account = AccountId::new("a@b.c");
        let mut db = Db::open(&path, &account).unwrap();
        db.conn.busy_timeout(std::time::Duration::ZERO).unwrap();
        let inbox = FolderId::new("in");
        db.start_over(&inbox).unwrap();
        let other = Connection::open(&path).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        let busy = db
            .apply(&inbox, None, &changes(Vec::new()), false)
            .err()
            .unwrap();
        assert!(is_busy(&busy), "{busy}");
        other.execute_batch("ROLLBACK").unwrap();
        db.apply(&inbox, None, &changes(Vec::new()), false).unwrap();
        // A missing table isn't a busy database.
        other.execute_batch("DROP TABLE messages").unwrap();
        let broken = db.count(&inbox).unwrap_err();
        assert!(!is_busy(&broken), "{broken}");
        // Windows can't remove files that are open.
        drop((db, other));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn changes_from_a_state_another_mett_moved_on_from_are_not_taken() {
        let mut db = Db::in_memory(&AccountId::new("a@b.c")).unwrap();
        let inbox = FolderId::new("in");
        let changes = |state: &str| FolderChanges {
            changed: Vec::new(),
            removed: Vec::new(),
            state: SyncState::new(state),
            more: false,
        };
        db.start_over(&inbox).unwrap();
        assert!(
            db.apply(&inbox, None, &changes("s1"), true)
                .unwrap()
                .is_some()
        );
        // Another mett took in s1 → s2; this one's s1 → s2' comes late.
        let s1 = SyncState::new("s1");
        assert!(
            db.apply(&inbox, Some(&s1), &changes("s2"), true)
                .unwrap()
                .is_some()
        );
        assert!(
            db.apply(&inbox, Some(&s1), &changes("s2'"), true)
                .unwrap()
                .is_none()
        );
        let synced = db.synced(&inbox).unwrap().unwrap();
        assert_eq!(synced.state, Some(SyncState::new("s2")));
        // Once all here, it can be listed offline, even while a later sync
        // is cut short.
        assert!(synced.complete && synced.snapshot);
        let unfinished = FolderChanges {
            more: true,
            ..changes("s3")
        };
        let s2 = SyncState::new("s2");
        assert!(
            db.apply(&inbox, Some(&s2), &unfinished, true)
                .unwrap()
                .is_some()
        );
        let synced = db.synced(&inbox).unwrap().unwrap();
        assert!(!synced.complete && synced.snapshot);
    }

    #[test]
    fn undated_changes_are_dated_when_synced() {
        let mut db = Db::in_memory(&AccountId::new("a@b.c")).unwrap();
        let inbox = FolderId::new("in");
        let mut summary = mett_core::mock::message("m1", "in", "hi", DateTime::UNIX_EPOCH).summary;
        summary.modified = None;
        let before = Utc::now();
        db.start_over(&inbox).unwrap();
        let changes = FolderChanges {
            changed: vec![summary],
            removed: Vec::new(),
            state: SyncState::new("s1"),
            more: false,
        };
        assert!(db.apply(&inbox, None, &changes, true).unwrap().is_some());
        let changed = db.changed(&inbox, before, None, 10).unwrap();
        assert_eq!(changed.len(), 1);
        assert!(
            changed[0]
                .modified
                .is_some_and(|m| m.timestamp_micros() >= before.timestamp_micros())
        );
    }

    #[test]
    fn a_body_is_kept_only_with_its_summary() {
        let mut db = Db::in_memory(&AccountId::new("a@b.c")).unwrap();
        let inbox = FolderId::new("in");
        let at = |minutes| DateTime::UNIX_EPOCH + chrono::TimeDelta::minutes(minutes);
        let mut message = mett_core::mock::message("m1", "in", "hi", at(0));
        message.summary.modified = Some(at(1));
        db.start_over(&inbox).unwrap();
        let mut newer = message.summary.clone();
        newer.modified = Some(at(2));
        let changes = FolderChanges {
            changed: vec![newer],
            removed: Vec::new(),
            state: SyncState::new("s1"),
            more: false,
        };
        assert!(db.apply(&inbox, None, &changes, true).unwrap().is_some());
        // Fetched before the sync brought the newer summary: not kept.
        db.keep_body(&message).unwrap();
        assert_eq!(db.message(&message.summary.id).unwrap(), None);
        message.summary.modified = Some(at(2));
        db.keep_body(&message).unwrap();
        assert!(db.message(&message.summary.id).unwrap().is_some());
    }

    #[test]
    fn a_sync_from_the_start_counts_only_what_it_saw() {
        let mut db = Db::in_memory(&AccountId::new("a@b.c")).unwrap();
        let inbox = FolderId::new("in");
        let summary =
            |id: &str| mett_core::mock::message(id, "in", "hi", DateTime::UNIX_EPOCH).summary;
        let changes = |ids: &[&str], state: &str, more: bool| FolderChanges {
            changed: ids.iter().map(|id| summary(id)).collect(),
            removed: Vec::new(),
            state: SyncState::new(state),
            more,
        };
        db.start_over(&inbox).unwrap();
        assert_eq!(
            db.apply(&inbox, None, &changes(&["a", "b", "c"], "s1", false), true)
                .unwrap(),
            Some(3),
            "each batch says how many more are here"
        );
        assert_eq!(db.count(&inbox).unwrap(), 3);
        // Changed again, they are no more.
        let s1 = SyncState::new("s1");
        assert_eq!(
            db.apply(&inbox, Some(&s1), &changes(&["a"], "s2", false), true)
                .unwrap(),
            Some(0)
        );
        // Started over: what it hasn't seen again doesn't count.
        db.start_over(&inbox).unwrap();
        assert_eq!(db.count(&inbox).unwrap(), 0);
        // Seen again, it counts again.
        assert_eq!(
            db.apply(&inbox, None, &changes(&["a"], "t1", true), true)
                .unwrap(),
            Some(1)
        );
        assert_eq!(db.count(&inbox).unwrap(), 1);
    }

    #[test]
    fn dates_keep_their_microseconds() {
        let mut db = Db::in_memory(&AccountId::new("a@b.c")).unwrap();
        let inbox = FolderId::new("in");
        let at = DateTime::UNIX_EPOCH + chrono::TimeDelta::milliseconds(1);
        let later = at + chrono::TimeDelta::microseconds(500);
        // "b" before "a" by ID, but after it by date.
        let a = mett_core::mock::message("a", "in", "later", later).summary;
        let b = mett_core::mock::message("b", "in", "sooner", at).summary;
        db.start_over(&inbox).unwrap();
        let changes = FolderChanges {
            changed: vec![b, a],
            removed: Vec::new(),
            state: SyncState::new("s1"),
            more: false,
        };
        assert!(db.apply(&inbox, None, &changes, true).unwrap().is_some());
        let page = db.page(&inbox, None, 10).unwrap();
        let ids: Vec<&str> = page.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(page[0].received, later);
    }

    #[test]
    fn a_lookup_without_a_role_keeps_the_one_known() {
        let mut db = Db::in_memory(&AccountId::new("a@b.c")).unwrap();
        let mut inbox = folder("in");
        inbox.well_known = Some(WellKnownFolder::Inbox);
        db.keep_folders(std::slice::from_ref(&inbox), Listing::Some)
            .unwrap();
        db.keep_folders(&[folder("in")], Listing::Some).unwrap();
        assert_eq!(db.well_known(WellKnownFolder::Inbox).unwrap(), Some(inbox));
    }

    #[test]
    fn a_listing_of_all_folders_replaces_those_kept() {
        let mut db = Db::in_memory(&AccountId::new("a@b.c")).unwrap();
        let all = |db: &Db| -> Vec<String> {
            db.folders_listed(Listing::All)
                .unwrap()
                .unwrap()
                .into_iter()
                .map(|f| f.id.to_string())
                .collect()
        };
        assert_eq!(
            db.folders_listed(Listing::All).unwrap(),
            None,
            "never listed"
        );
        db.keep_folders(&[folder("a"), folder("b")], Listing::All)
            .unwrap();
        // One folder alone (opened) is added to the others.
        db.keep_folders(&[folder("c")], Listing::Some).unwrap();
        assert_eq!(all(&db), ["a", "b", "c"]);
        // A listing of them all, without b (deleted), leaves it out.
        let b = FolderId::new("b");
        db.start_over(&b).unwrap();
        let mail = FolderChanges {
            changed: vec![mett_core::mock::message("m1", "b", "hi", DateTime::UNIX_EPOCH).summary],
            removed: Vec::new(),
            state: SyncState::new("s1"),
            more: false,
        };
        assert!(db.apply(&b, None, &mail, true).unwrap().is_some());
        db.keep_folders(&[folder("a"), folder("c")], Listing::All)
            .unwrap();
        assert_eq!(all(&db), ["a", "c"]);
        // Its mail went with it.
        assert!(db.page(&b, None, 10).unwrap().is_empty());
        assert!(db.synced(&b).unwrap().is_none());
        // A listing below a folder replaces those kept there, and those
        // below them.
        let mut below_a = folder("d");
        below_a.parent = Some(FolderId::new("a"));
        let mut below_d = folder("e");
        below_d.parent = Some(FolderId::new("d"));
        db.keep_folders(&[below_a, below_d], Listing::Some).unwrap();
        // An empty listing below a folder is a listing too.
        let a = FolderId::new("a");
        db.keep_folders(&[], Listing::Below(Some(&a))).unwrap();
        assert_eq!(
            db.folders_listed(Listing::Below(Some(&a))).unwrap(),
            Some(vec![])
        );
        assert_eq!(all(&db), ["a", "c"], "e went with d");
    }
}
