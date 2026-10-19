//! The cache database: its schema, and how summaries and messages are
//! stored in it.

use std::path::Path;

use chrono::{DateTime, Utc};
use mett_core::{
    Address, Body, BodyKind, Error, Flags, FlagsUpdate, Folder, FolderChanges, FolderId, Message,
    MessageId, MessageSummary, SyncState, ThreadPosition, WellKnownFolder,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// The schema's version. A cache of another version is dropped and filled
/// again: it holds nothing the server doesn't.
const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = "
    CREATE TABLE folders (
        id TEXT PRIMARY KEY,
        -- Where the folder's sync stands, once it has started.
        state TEXT,
        -- Whether the sync reached the end: the folder is all here.
        complete INTEGER NOT NULL DEFAULT 0,
        -- Whether the sync started over: what it doesn't report again is
        -- gone, once it is complete.
        scratch INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE messages (
        id TEXT PRIMARY KEY,
        folder TEXT NOT NULL,
        -- The summary's dates, in milliseconds, to sort and filter by.
        received INTEGER NOT NULL,
        modified INTEGER,
        unread INTEGER NOT NULL,
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
    CREATE INDEX messages_by_date ON messages (folder, received DESC, id);
    CREATE INDEX messages_by_change ON messages (folder, modified DESC);
";

/// Where a folder's sync stands.
pub(crate) struct Synced {
    pub state: Option<SyncState>,
    pub complete: bool,
}

/// The cache database.
pub(crate) struct Db {
    conn: Connection,
}

/// A cache failure, as a port error.
fn failed(e: impl std::fmt::Display) -> Error {
    Error::Backend(format!("cache: {e}"))
}

impl Db {
    /// Opens (creating it if needed) the cache at `path`.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::ready(Connection::open(path)?)
    }

    /// A cache in memory, for tests.
    pub fn in_memory() -> rusqlite::Result<Self> {
        Self::ready(Connection::open_in_memory()?)
    }

    fn ready(conn: Connection) -> rusqlite::Result<Self> {
        // Another mett may use the same cache: wait for it rather than fail.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            conn.execute_batch(
                "DROP INDEX IF EXISTS messages_by_date;
                 DROP INDEX IF EXISTS messages_by_change;
                 DROP TABLE IF EXISTS messages;
                 DROP TABLE IF EXISTS folders;
                 DROP TABLE IF EXISTS folder_info;
                 DROP TABLE IF EXISTS well_known;",
            )?;
            conn.execute_batch(SCHEMA)?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(Self { conn })
    }

    /// Where `folder`'s sync stands, if it has started.
    pub fn synced(&self, folder: &FolderId) -> Result<Option<Synced>, Error> {
        self.conn
            .query_row(
                "SELECT state, complete FROM folders WHERE id = ?1",
                [folder.as_str()],
                |r| {
                    Ok(Synced {
                        state: r.get::<_, Option<String>>(0)?.map(SyncState::new),
                        complete: r.get(1)?,
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

    /// Takes in a batch of `folder`'s changes, and where its sync stands
    /// after them.
    pub fn apply(&mut self, folder: &FolderId, changes: &FolderChanges) -> Result<(), Error> {
        let tx = self.conn.transaction().map_err(failed)?;
        for summary in &changes.changed {
            let stored = serde_json::to_string(&StoredSummary::from(summary)).map_err(failed)?;
            // Only a draft's body can change (Graph and EWS let only
            // drafts' be edited): a draft's is fetched again.
            tx.execute(
                "INSERT INTO messages (id, folder, received, modified, unread, summary, seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)
                 ON CONFLICT (id) DO UPDATE SET
                    folder = excluded.folder, received = excluded.received,
                    modified = excluded.modified, unread = excluded.unread,
                    summary = excluded.summary, seen = 1,
                    extras = CASE WHEN ?7 THEN NULL ELSE extras END",
                params![
                    summary.id.as_str(),
                    folder.as_str(),
                    summary.received.timestamp_millis(),
                    summary.modified.map(|m| m.timestamp_millis()),
                    !summary.flags.read,
                    stored,
                    summary.draft,
                ],
            )
            .map_err(failed)?;
        }
        for id in &changes.removed {
            tx.execute(
                "DELETE FROM messages WHERE id = ?1 AND folder = ?2",
                [id.as_str(), folder.as_str()],
            )
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
                "UPDATE folders SET scratch = 0 WHERE id = ?1",
                [folder.as_str()],
            )
            .map_err(failed)?;
        }
        tx.commit().map_err(failed)
    }

    /// Up to `limit` of `folder`'s summaries from `offset`, newest first.
    pub fn page(
        &self,
        folder: &FolderId,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<MessageSummary>, Error> {
        self.summaries(
            "SELECT summary FROM messages WHERE folder = ?1
             ORDER BY received DESC, id LIMIT ?2 OFFSET ?3",
            params![folder.as_str(), sql(limit), sql(offset)],
        )
    }

    /// Up to `limit` of `folder`'s summaries changed at or after `since`,
    /// from `offset`, most recently changed first.
    pub fn changed(
        &self,
        folder: &FolderId,
        since: DateTime<Utc>,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<MessageSummary>, Error> {
        self.summaries(
            "SELECT summary FROM messages WHERE folder = ?1 AND modified >= ?2
             ORDER BY modified DESC, id LIMIT ?3 OFFSET ?4",
            params![
                folder.as_str(),
                since.timestamp_millis(),
                sql(limit),
                sql(offset)
            ],
        )
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
            .prepare(
                "SELECT id, modified FROM messages
                 WHERE folder = ?1 AND unread AND modified >= ?2
                 ORDER BY modified DESC, id LIMIT ?3",
            )
            .map_err(failed)?;
        let rows = statement
            .query_map(
                params![folder.as_str(), since.timestamp_millis(), sql(limit)],
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
        let mut statement = self.conn.prepare(query).map_err(failed)?;
        let rows = statement
            .query_map(params, |r| r.get::<_, String>(0))
            .map_err(failed)?;
        rows.map(|row| {
            let stored: StoredSummary =
                serde_json::from_str(&row.map_err(failed)?).map_err(failed)?;
            stored.into_summary()
        })
        .collect()
    }

    /// The full message `id`, if it is here with its body.
    pub fn message(&self, id: &MessageId) -> Result<Option<Message>, Error> {
        let row: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT summary, extras FROM messages WHERE id = ?1",
                [id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(failed)?;
        let Some((summary, Some(extras))) = row else {
            return Ok(None);
        };
        let summary: StoredSummary = serde_json::from_str(&summary).map_err(failed)?;
        let extras: StoredExtras = serde_json::from_str(&extras).map_err(failed)?;
        Ok(Some(extras.into_message(summary.into_summary()?)))
    }

    /// Keeps the body of `message`, fetched just now, if its summary is
    /// here (in a folder being cached) and it isn't a draft, which may
    /// change.
    pub fn keep_body(&self, message: &Message) -> Result<(), Error> {
        if message.summary.draft {
            return Ok(());
        }
        let extras = serde_json::to_string(&StoredExtras::from(message)).map_err(failed)?;
        self.conn
            .execute(
                "UPDATE messages SET extras = ?2 WHERE id = ?1",
                [message.summary.id.as_str(), extras.as_str()],
            )
            .map_err(failed)?;
        Ok(())
    }

    /// Changes the flags of message `id`, if it is here.
    pub fn update_flags(&self, id: &MessageId, update: FlagsUpdate) -> Result<(), Error> {
        let summary: Option<String> = self
            .conn
            .query_row(
                "SELECT summary FROM messages WHERE id = ?1",
                [id.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(failed)?;
        let Some(summary) = summary else {
            return Ok(());
        };
        let mut stored: StoredSummary = serde_json::from_str(&summary).map_err(failed)?;
        let mut flags = Flags {
            read: stored.read,
            flagged: stored.flagged,
        };
        update.apply(&mut flags);
        stored.read = flags.read;
        stored.flagged = flags.flagged;
        let summary = serde_json::to_string(&stored).map_err(failed)?;
        self.conn
            .execute(
                "UPDATE messages SET summary = ?2, unread = ?3 WHERE id = ?1",
                params![id.as_str(), summary, !flags.read],
            )
            .map_err(failed)?;
        Ok(())
    }

    /// Keeps `folders`, as a listing gave them, in its order.
    pub fn keep_folders(&mut self, folders: &[Folder]) -> Result<(), Error> {
        let tx = self.conn.transaction().map_err(failed)?;
        for (ord, folder) in folders.iter().enumerate() {
            let stored = serde_json::to_string(&StoredFolder::from(folder)).map_err(failed)?;
            tx.execute(
                "INSERT INTO folder_info (id, parent, ord, folder) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (id) DO UPDATE SET
                    parent = excluded.parent, ord = excluded.ord, folder = excluded.folder",
                params![
                    folder.id.as_str(),
                    folder.parent.as_ref().map(FolderId::as_str),
                    sql(ord as u64),
                    stored
                ],
            )
            .map_err(failed)?;
            if let Some(which) = folder.well_known {
                tx.execute(
                    "INSERT INTO well_known (name, id) VALUES (?1, ?2)
                     ON CONFLICT (name) DO UPDATE SET id = excluded.id",
                    [which.as_str(), folder.id.as_str()],
                )
                .map_err(failed)?;
            }
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

    /// The folders directly below `parent` (without one, the top-level
    /// ones), or every folder with `all`, as last seen.
    pub fn folders_below(
        &self,
        parent: Option<&FolderId>,
        all: bool,
    ) -> Result<Vec<Folder>, Error> {
        match (all, parent) {
            (true, _) => self.folders("SELECT folder FROM folder_info ORDER BY ord", []),
            (false, None) => self.folders(
                "SELECT folder FROM folder_info WHERE parent IS NULL ORDER BY ord",
                [],
            ),
            (false, Some(parent)) => self.folders(
                "SELECT folder FROM folder_info WHERE parent = ?1 ORDER BY ord",
                [parent.as_str()],
            ),
        }
    }

    fn folders(&self, query: &str, params: impl rusqlite::Params) -> Result<Vec<Folder>, Error> {
        let mut statement = self.conn.prepare(query).map_err(failed)?;
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
            .execute("DELETE FROM messages WHERE id = ?1", [id.as_str()])
            .map_err(failed)?;
        Ok(())
    }
}

/// A count, as SQLite takes it (which is signed).
fn sql(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// A time in milliseconds, as stored.
fn time(millis: i64) -> Result<DateTime<Utc>, Error> {
    DateTime::from_timestamp_millis(millis).ok_or_else(|| failed("a date out of range"))
}

/// An optional time, as stored.
fn maybe_time(millis: Option<i64>) -> Result<Option<DateTime<Utc>>, Error> {
    millis.map(time).transpose()
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

/// A [`MessageSummary`], as stored.
#[derive(Serialize, Deserialize)]
struct StoredSummary {
    id: String,
    folder: String,
    subject: String,
    from: Option<StoredAddress>,
    to: Vec<StoredAddress>,
    received: i64,
    read: bool,
    flagged: bool,
    has_attachments: bool,
    draft: bool,
    delivered: Option<i64>,
    modified: Option<i64>,
    conversation: Option<String>,
    thread_path: Vec<Vec<u8>>,
}

impl From<&MessageSummary> for StoredSummary {
    fn from(s: &MessageSummary) -> Self {
        Self {
            id: s.id.to_string(),
            folder: s.folder.to_string(),
            subject: s.subject.clone(),
            from: s.from.as_ref().map(StoredAddress::from),
            to: s.to.iter().map(StoredAddress::from).collect(),
            received: s.received.timestamp_millis(),
            read: s.flags.read,
            flagged: s.flags.flagged,
            has_attachments: s.has_attachments,
            draft: s.draft,
            delivered: s.delivered.map(|d| d.timestamp_millis()),
            modified: s.modified.map(|m| m.timestamp_millis()),
            conversation: s.thread.as_ref().map(|t| t.conversation.clone()),
            thread_path: s
                .thread
                .as_ref()
                .map(|t| t.path.clone())
                .unwrap_or_default(),
        }
    }
}

impl StoredSummary {
    fn into_summary(self) -> Result<MessageSummary, Error> {
        Ok(MessageSummary {
            id: MessageId::new(self.id),
            folder: FolderId::new(self.folder),
            subject: self.subject,
            from: self.from.map(Address::from),
            to: self.to.into_iter().map(Address::from).collect(),
            received: time(self.received)?,
            flags: Flags {
                read: self.read,
                flagged: self.flagged,
            },
            has_attachments: self.has_attachments,
            draft: self.draft,
            delivered: maybe_time(self.delivered)?,
            modified: maybe_time(self.modified)?,
            thread: self.conversation.map(|conversation| ThreadPosition {
                conversation,
                path: self.thread_path,
            }),
        })
    }
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
        }
    }
}
