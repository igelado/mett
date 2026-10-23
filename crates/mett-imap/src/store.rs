//! The mailbox, over one IMAP connection.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use async_imap::Session;
use async_imap::imap_proto::NameAttribute;
use async_imap::types::{Fetch, Flag, Name};
use chrono::{DateTime, Utc};
use futures_util::TryStreamExt;
use mett_core::{
    AccountId, Address, Attachment, AttachmentId, Authenticator, Draft, DraftOrigin, Error, Flags,
    FlagsUpdate, Folder, FolderId, MailSender, MailStore, Message, MessageId, MessagePage,
    MessageSummary, Page, PageCursor, RECENT_UNREAD, Result, WellKnownFolder,
};
use mett_smtp::{SmtpSender, Threading};
use tokio::sync::{Mutex, MutexGuard};
use utf7_imap::{decode_utf7_imap, encode_utf7_imap};

use crate::conn::{self, Conn, Credentials, LoginFailure, within};
use crate::ids::Ref;
use crate::parse::{self, Fetched};
use crate::settings::{ImapSettings, Login, NoTokens};

/// What a connection that sat idle this long is asked before it is used:
/// servers drop idle ones, and the first command on a dropped one would
/// fail.
const STALE: Duration = Duration::from_secs(60);

/// What to ask of the server about each message of a listing: its
/// header, to make the index line.
const SUMMARY: &str = "(UID FLAGS INTERNALDATE BODY.PEEK[HEADER])";

/// The whole message.
const WHOLE: &str = "(UID FLAGS INTERNALDATE BODY.PEEK[])";

/// A failure in a command: the server's, or ours.
enum Fail {
    Imap(async_imap::error::Error),
    Core(Error),
}

impl From<async_imap::error::Error> for Fail {
    fn from(e: async_imap::error::Error) -> Self {
        Self::Imap(e)
    }
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        Self::Core(e)
    }
}

type Res<T> = std::result::Result<T, Fail>;

/// What the server can do.
#[derive(Clone, Copy, Debug, Default)]
struct Caps {
    /// `MOVE`.
    moves: bool,
    /// `UIDPLUS`, which expunges just the messages asked for.
    uidplus: bool,
}

/// A connection that is logged in.
struct Live {
    session: Session<Conn>,
    caps: Caps,
    used: Instant,
}

/// A mailbox on an IMAP server.
///
/// It keeps one connection, logs in when first used and again if the
/// connection is lost, so a request after a drop fails once at most. With
/// an SMTP server ([`ImapMailStore::with_smtp`]) it also sends mail, and
/// keeps its own copy of what it sends: IMAP servers don't.
pub struct ImapMailStore<A = NoTokens> {
    settings: ImapSettings,
    login: Login<A>,
    /// The address the mailbox's owner writes from.
    from: Address,
    smtp: Option<SmtpSender<A>>,
    live: Mutex<Option<Live>>,
}

impl<A: Authenticator> ImapMailStore<A> {
    /// A mailbox on the server `settings` describe, logged in as `login`,
    /// whose owner writes from `from`.
    pub fn new(settings: ImapSettings, login: Login<A>, from: Address) -> Self {
        Self {
            settings,
            login,
            from,
            smtp: None,
            live: Mutex::new(None),
        }
    }

    /// Sends mail through `smtp`.
    #[must_use]
    pub fn with_smtp(mut self, smtp: SmtpSender<A>) -> Self {
        self.smtp = Some(smtp);
        self
    }

    /// The account: the user at the server.
    pub fn account(&self) -> AccountId {
        account(&self.settings.host, self.login.user())
    }

    async fn connect(&self) -> Result<Live> {
        let host = &self.settings.host;
        let session = match &self.login {
            Login::Password { user, password } => {
                conn::connect(&self.settings, Credentials::Password(user, password)).await
            }
            Login::OAuth { user, tokens } => {
                let token = tokens.access_token().await?;
                match conn::connect(&self.settings, Credentials::OAuth(user, &token)).await {
                    // The server turned down a token that hadn't expired
                    // (it may have been revoked): get a new one.
                    Err(LoginFailure::Refused(refused)) => {
                        tokens.token_rejected(&token).await;
                        let fresh = tokens.access_token().await?;
                        if fresh == token {
                            Err(LoginFailure::Refused(refused))
                        } else {
                            conn::connect(&self.settings, Credentials::OAuth(user, &fresh)).await
                        }
                    }
                    other => other,
                }
            }
        };
        let mut session = session.map_err(|failure| match failure {
            LoginFailure::Refused(e) | LoginFailure::Other(e) => e,
        })?;
        let caps = within(self.settings.timeout, session.capabilities())
            .await?
            .map_err(|e| conn::lost(host, &e.to_string()))?;
        Ok(Live {
            session,
            caps: Caps {
                moves: caps.has_str("MOVE"),
                uidplus: caps.has_str("UIDPLUS"),
            },
            used: Instant::now(),
        })
    }

    /// The connection, logged in (again, if it was lost or has gone
    /// stale).
    async fn lock(&self) -> Result<Guard<'_>> {
        let mut slot = self.live.lock().await;
        if let Some(live) = slot.as_mut()
            && live.used.elapsed() > STALE
        {
            let alive = within(self.settings.timeout, live.session.noop()).await;
            if !matches!(alive, Ok(Ok(()))) {
                *slot = None;
            }
        }
        if slot.is_none() {
            *slot = Some(self.connect().await?);
        }
        Ok(Guard {
            slot,
            host: &self.settings.host,
            timeout: self.settings.timeout,
        })
    }
}

/// The user at the server, as the account is called.
pub fn account(host: &str, user: &str) -> AccountId {
    if user.contains('@') {
        AccountId::new(user)
    } else {
        AccountId::new(format!("{user}@{host}"))
    }
}

/// The connection, held while a request uses it.
struct Guard<'a> {
    slot: MutexGuard<'a, Option<Live>>,
    host: &'a str,
    timeout: Duration,
}

impl Guard<'_> {
    fn live(&mut self) -> &mut Live {
        self.slot
            .as_mut()
            .expect("the connection is there while the guard is")
    }

    /// `result` for the caller: a connection that failed is let go, so
    /// that the next request makes a new one.
    fn done<T>(&mut self, result: Res<T>) -> Result<T> {
        match result {
            Ok(value) => {
                self.live().used = Instant::now();
                Ok(value)
            }
            Err(Fail::Core(e)) => {
                // A timeout leaves the connection mid-command.
                if matches!(e, Error::Network(_)) {
                    *self.slot = None;
                }
                Err(e)
            }
            Err(Fail::Imap(e)) => {
                use async_imap::error::Error as E;
                let host = self.host;
                match e {
                    E::No(text) => Err(Error::Backend(format!("{host}: {text}"))),
                    E::Bad(text) => Err(Error::Backend(format!("{host}: {text}"))),
                    E::Validate(e) => Err(Error::Backend(format!("{host}: {e}"))),
                    e => {
                        *self.slot = None;
                        Err(conn::lost(host, &e.to_string()))
                    }
                }
            }
        }
    }
}

/// `future`, within the time the server is given: a connection that is still
/// in the middle of a command after that is let go.
async fn bounded<T>(
    timeout: Duration,
    future: impl std::future::Future<Output = Res<T>>,
) -> Res<T> {
    within(timeout, future).await.map_err(Fail::Core)?
}

/// The name of a folder, as the server writes it.
fn wire(name: &str) -> String {
    encode_utf7_imap(name.to_owned())
}

fn flags_of(fetch: &Fetch) -> (Flags, bool) {
    let mut flags = Flags::default();
    let mut draft = false;
    for flag in fetch.flags() {
        match flag {
            Flag::Seen => flags.read = true,
            Flag::Flagged => flags.flagged = true,
            Flag::Draft => draft = true,
            _ => {}
        }
    }
    (flags, draft)
}

fn fetched(fetch: &Fetch, whole: bool) -> Option<Fetched> {
    let (flags, draft) = flags_of(fetch);
    Some(Fetched {
        uid: fetch.uid?,
        flags,
        draft,
        internal: fetch.internal_date().map(|d| d.with_timezone(&Utc)),
        data: if whole { fetch.body() } else { fetch.header() }
            .unwrap_or_default()
            .to_vec(),
    })
}

/// A set of UIDs as a command takes them.
fn set(uids: impl IntoIterator<Item = u32>) -> String {
    uids.into_iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// The role a folder has, by what the server says of it, or else by its
/// name.
fn role(
    name: &str,
    delimiter: Option<&str>,
    attributes: &[NameAttribute<'_>],
) -> Option<WellKnownFolder> {
    if name.eq_ignore_ascii_case("INBOX") {
        return Some(WellKnownFolder::Inbox);
    }
    for attribute in attributes {
        match attribute {
            NameAttribute::Sent => return Some(WellKnownFolder::SentItems),
            NameAttribute::Drafts => return Some(WellKnownFolder::Drafts),
            NameAttribute::Trash => return Some(WellKnownFolder::DeletedItems),
            NameAttribute::Junk => return Some(WellKnownFolder::JunkEmail),
            NameAttribute::Archive | NameAttribute::All => return Some(WellKnownFolder::Archive),
            _ => {}
        }
    }
    // Servers that say nothing of the kind use the usual names, at the top
    // or just below `INBOX` (where some keep every folder).
    let last = delimiter
        .and_then(|d| name.rsplit(d).next())
        .unwrap_or(name)
        .to_lowercase();
    let depth = delimiter.map_or(1, |d| name.split(d).count());
    let top = depth == 1
        || (depth == 2
            && delimiter
                .and_then(|d| name.split(d).next())
                .is_some_and(|first| first.eq_ignore_ascii_case("INBOX")));
    if !top {
        return None;
    }
    Some(match last.as_str() {
        "sent" | "sent items" | "sent messages" | "sent mail" => WellKnownFolder::SentItems,
        "drafts" => WellKnownFolder::Drafts,
        "trash" | "deleted items" | "deleted messages" | "bin" => WellKnownFolder::DeletedItems,
        "junk" | "spam" | "junk email" | "junk e-mail" => WellKnownFolder::JunkEmail,
        "archive" | "archives" => WellKnownFolder::Archive,
        "outbox" => WellKnownFolder::Outbox,
        _ => return None,
    })
}

/// A folder from a listing, without counts.
struct Listed {
    name: String,
    delimiter: Option<String>,
    selectable: bool,
    role: Option<WellKnownFolder>,
}

fn listed(name: &Name) -> Listed {
    let decoded = decode_utf7_imap(name.name().to_owned());
    Listed {
        role: role(&decoded, name.delimiter(), name.attributes()),
        selectable: !name.attributes().iter().any(|a| match a {
            NameAttribute::NoSelect => true,
            NameAttribute::Extension(x) => x.eq_ignore_ascii_case("\\NonExistent"),
            _ => false,
        }),
        delimiter: name.delimiter().map(str::to_owned),
        name: decoded,
    }
}

/// The folder above `name`, if the server has hierarchy.
fn parent_name<'a>(name: &'a str, delimiter: Option<&str>) -> Option<&'a str> {
    let delimiter = delimiter?;
    name.rsplit_once(delimiter).map(|(parent, _)| parent)
}

async fn list(live: &mut Live, pattern: &str) -> Res<Vec<Listed>> {
    let names: Vec<Name> = live
        .session
        .list(Some(""), Some(&format!("\"{pattern}\"")))
        .await?
        .try_collect()
        .await?;
    Ok(names.iter().map(listed).collect())
}

/// `listed` as a folder, with its counts from the server.
async fn folder_of(live: &mut Live, listed: &Listed, parent: Option<FolderId>) -> Res<Folder> {
    let (unread, total) = if listed.selectable {
        let status = live
            .session
            .status(wire(&listed.name), "(MESSAGES UNSEEN)")
            .await?;
        (status.unseen.unwrap_or(0), status.exists)
    } else {
        (0, 0)
    };
    let display = match (&parent, &listed.delimiter) {
        (Some(_), Some(d)) => listed
            .name
            .rsplit(d.as_str())
            .next()
            .unwrap_or(&listed.name),
        _ => &listed.name,
    };
    Ok(Folder {
        id: FolderId::new(&listed.name),
        parent,
        name: display.to_owned(),
        well_known: listed.role,
        unread_count: unread,
        total_count: total,
    })
}

async fn folders(live: &mut Live) -> Res<Vec<Folder>> {
    let all = list(live, "*").await?;
    let names: HashSet<String> = all.iter().map(|l| l.name.clone()).collect();
    let mut out = Vec::with_capacity(all.len());
    for item in &all {
        let parent = parent_name(&item.name, item.delimiter.as_deref())
            .filter(|p| names.contains(*p))
            .map(FolderId::new);
        out.push(folder_of(live, item, parent).await?);
    }
    Ok(out)
}

async fn folder(live: &mut Live, id: &FolderId) -> Res<Folder> {
    let found = list(live, &wire(id.as_str()).replace('"', "\\\"")).await?;
    let item = found
        .into_iter()
        .find(|l| l.name == id.as_str())
        .ok_or_else(|| Error::not_found("folder", id))?;
    // Whether the parent is a folder of the listing: it is, if the server
    // lists it.
    let parent = match parent_name(&item.name, item.delimiter.as_deref()) {
        Some(parent) => {
            let up = list(live, &wire(parent).replace('"', "\\\"")).await?;
            up.iter()
                .any(|l| l.name == parent)
                .then(|| FolderId::new(parent))
        }
        None => None,
    };
    folder_of(live, &item, parent).await
}

/// Selects `name` and returns its `UIDVALIDITY`.
async fn select(live: &mut Live, name: &str) -> Res<u32> {
    let mailbox = live.session.select(wire(name)).await.map_err(|e| match e {
        async_imap::error::Error::No(_) => Fail::Core(Error::not_found("folder", name)),
        e => Fail::Imap(e),
    })?;
    Ok(mailbox.uid_validity.unwrap_or(0))
}

/// Selects the folder of `at`, as long as it is the one `at` was made in.
async fn select_ref(live: &mut Live, at: &Ref) -> Res<()> {
    let validity = select(live, &at.folder).await?;
    if validity != at.validity {
        return Err(Error::not_found("message", at.id()).into());
    }
    Ok(())
}

/// The messages of the selected folder with `uids`, as `fetch` asks.
async fn fetch(live: &mut Live, uids: &[u32], what: &str, whole: bool) -> Res<Vec<Fetched>> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let fetches: Vec<Fetch> = live
        .session
        .uid_fetch(set(uids.iter().copied()), what)
        .await?
        .try_collect()
        .await?;
    Ok(fetches.iter().filter_map(|f| fetched(f, whole)).collect())
}

/// The summaries of `uids` in the selected folder, newest UID first.
async fn summaries(
    live: &mut Live,
    folder: &str,
    validity: u32,
    uids: &[u32],
) -> Res<Vec<MessageSummary>> {
    let mut found = fetch(live, uids, SUMMARY, false).await?;
    found.sort_by_key(|f| std::cmp::Reverse(f.uid));
    found
        .iter()
        .map(|f| Ok(parse::summary(folder, validity, f)?))
        .collect()
}

/// The UIDs a search finds in the selected folder, newest first.
async fn search(live: &mut Live, query: &str) -> Res<Vec<u32>> {
    let mut uids: Vec<u32> = live.session.uid_search(query).await?.into_iter().collect();
    uids.sort_unstable_by(|a, b| b.cmp(a));
    Ok(uids)
}

/// A page of the newest messages of `folder`, from `ids` (newest first).
async fn page_of(live: &mut Live, folder: &str, ids: Vec<u32>, page: &Page) -> Res<MessagePage> {
    let validity = select(live, folder).await?;
    let all = if ids.is_empty() {
        search(live, "ALL").await?
    } else {
        ids
    };
    let after = page.cursor().and_then(|c| c.as_str().parse::<u32>().ok());
    let rest: Vec<u32> = all
        .into_iter()
        .filter(|u| after.is_none_or(|after| *u < after))
        .collect();
    let size = page.size() as usize;
    let taken = &rest[..rest.len().min(size)];
    let next = (rest.len() > size)
        .then(|| taken.last().map(|u| PageCursor::new(u.to_string())))
        .flatten();
    let messages = summaries(live, folder, validity, taken).await?;
    Ok(MessagePage { messages, next })
}

/// `IMAP`'s date for a search: the day, which is as exact as it gets.
fn day(at: DateTime<Utc>) -> String {
    at.format("%d-%b-%Y").to_string()
}

impl<A: Authenticator> MailStore for ImapMailStore<A> {
    async fn own_addresses(&self) -> Result<Vec<String>> {
        let mut own = vec![self.from.email.clone()];
        if self.login.user().contains('@') && self.login.user() != self.from.email {
            own.push(self.login.user().to_owned());
        }
        Ok(own)
    }

    async fn list_folders(&self) -> Result<Vec<Folder>> {
        let mut g = self.lock().await?;
        let t = g.timeout;
        let r = bounded(t, folders(g.live())).await;
        g.done(r)
    }

    async fn folder(&self, id: &FolderId) -> Result<Folder> {
        let mut g = self.lock().await?;
        let t = g.timeout;
        let r = bounded(t, folder(g.live(), id)).await;
        g.done(r)
    }

    async fn recent_unread(
        &self,
        folder: &FolderId,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<(MessageId, DateTime<Utc>)>> {
        let mut g = self.lock().await?;
        let t = g.timeout;
        let r = bounded(t, async {
            let live = g.live();
            let validity = select(live, folder.as_str()).await?;
            let query = match since {
                Some(since) => format!("UNSEEN SINCE {}", day(since)),
                None => "UNSEEN".to_owned(),
            };
            let uids = search(live, &query).await?;
            let mut found = fetch(live, &uids, "(UID FLAGS INTERNALDATE)", false).await;
            if let Err(Fail::Core(_)) = &found {
                found = Ok(Vec::new());
            }
            let mut out: Vec<(MessageId, DateTime<Utc>)> = found?
                .into_iter()
                .filter_map(|f| {
                    let at = f.internal?;
                    since.is_none_or(|s| at >= s).then(|| {
                        let id = Ref {
                            folder: folder.as_str().to_owned(),
                            validity,
                            uid: f.uid,
                        }
                        .id();
                        (id, at)
                    })
                })
                .collect();
            out.sort_by_key(|(_, at)| std::cmp::Reverse(*at));
            out.truncate(RECENT_UNREAD);
            Ok(out)
        })
        .await;
        g.done(r)
    }

    async fn well_known_folder(&self, which: WellKnownFolder) -> Result<Folder> {
        let mut g = self.lock().await?;
        let t = g.timeout;
        let r = bounded(t, async {
            let all = list(g.live(), "*").await?;
            // The server's own say (an attribute) outranks a name that
            // looks right: the first one found goes by the order listed.
            let item = all
                .iter()
                .find(|l| l.role == Some(which))
                .ok_or_else(|| Error::not_found("folder", which))?;
            let parent = parent_name(&item.name, item.delimiter.as_deref())
                .filter(|p| all.iter().any(|l| l.name == *p))
                .map(FolderId::new);
            folder_of(g.live(), item, parent).await
        })
        .await;
        g.done(r)
    }

    async fn list_messages(&self, folder: &FolderId, page: Page) -> Result<MessagePage> {
        let mut g = self.lock().await?;
        let t = g.timeout;
        let r = bounded(t, page_of(g.live(), folder.as_str(), Vec::new(), &page)).await;
        g.done(r)
    }

    async fn changed_messages(
        &self,
        folder: &FolderId,
        since: DateTime<Utc>,
        page: Page,
    ) -> Result<MessagePage> {
        let mut g = self.lock().await?;
        let t = g.timeout;
        let r = bounded(t, async {
            let live = g.live();
            select(live, folder.as_str()).await?;
            // The server dates what it took in by the day, so this asks
            // for the days since and keeps what came in after `since`.
            let uids = search(live, &format!("SINCE {}", day(since))).await?;
            let mut page = page_of(live, folder.as_str(), uids, &page).await?;
            page.messages
                .retain(|m| m.delivered.is_none_or(|at| at >= since));
            Ok(page)
        })
        .await;
        g.done(r)
    }

    async fn fetch_message(&self, id: &MessageId) -> Result<Message> {
        let at = Ref::parse(id)?;
        let mut g = self.lock().await?;
        let t = g.timeout;
        let r = bounded(t, async {
            let live = g.live();
            select_ref(live, &at).await?;
            let found = fetch(live, &[at.uid], WHOLE, true).await?;
            let found = found
                .first()
                .ok_or_else(|| Error::not_found("message", id))?;
            Ok(parse::message(&at.folder, at.validity, found)?)
        })
        .await;
        g.done(r)
    }

    async fn message_folder(&self, id: &MessageId) -> Result<FolderId> {
        Ok(Ref::parse(id)?.folder_id())
    }

    async fn list_attachments(&self, message: &MessageId) -> Result<Vec<Attachment>> {
        let data = self.raw(message).await?;
        parse::attachments(&data)
    }

    async fn fetch_attachment(
        &self,
        message: &MessageId,
        attachment: &AttachmentId,
    ) -> Result<Vec<u8>> {
        let data = self.raw(message).await?;
        parse::attachment(&data, attachment)
    }

    async fn update_flags(&self, id: &MessageId, update: FlagsUpdate) -> Result<()> {
        let at = Ref::parse(id)?;
        let mut g = self.lock().await?;
        let t = g.timeout;
        let r = bounded(t, async {
            let live = g.live();
            select_ref(live, &at).await?;
            for (flag, on) in [("\\Seen", update.read), ("\\Flagged", update.flagged)] {
                let Some(on) = on else { continue };
                let sign = if on { '+' } else { '-' };
                let _: Vec<Fetch> = live
                    .session
                    .uid_store(at.uid.to_string(), format!("{sign}FLAGS.SILENT ({flag})"))
                    .await?
                    .try_collect()
                    .await?;
            }
            Ok(())
        })
        .await;
        g.done(r)
    }

    async fn move_message(&self, id: &MessageId, to: &FolderId) -> Result<MessageId> {
        self.relocate(id, to, true).await
    }

    async fn copy_message(&self, id: &MessageId, to: &FolderId) -> Result<MessageId> {
        self.relocate(id, to, false).await
    }

    async fn delete_message(&self, id: &MessageId) -> Result<()> {
        let at = Ref::parse(id)?;
        let mut g = self.lock().await?;
        let t = g.timeout;
        let r = bounded(t, async {
            let live = g.live();
            select_ref(live, &at).await?;
            expunge(live, at.uid).await
        })
        .await;
        g.done(r)
    }

    async fn save_draft(&self, draft: &Draft) -> Result<MessageId> {
        let (draft, threading) = self.prepared(draft).await?;
        let composed = mett_smtp::compose(&draft, &self.from, &threading)?;
        let drafts = self.well_known_folder(WellKnownFolder::Drafts).await?;
        let id = self
            .store(
                &drafts.id,
                &composed.kept,
                "(\\Draft \\Seen)",
                &composed.message_id,
            )
            .await?;
        // A recalled draft that is saved again replaces the one recalled.
        if let Some(DraftOrigin::Recalled(old)) = &draft.origin {
            self.delete_message(old).await?;
        }
        Ok(id)
    }
}

impl<A: Authenticator> ImapMailStore<A> {
    /// A message as the server has it.
    async fn raw(&self, id: &MessageId) -> Result<Vec<u8>> {
        let at = Ref::parse(id)?;
        let mut g = self.lock().await?;
        let t = g.timeout;
        let r = bounded(t, async {
            let live = g.live();
            select_ref(live, &at).await?;
            let found = fetch(live, &[at.uid], WHOLE, true).await?;
            found
                .into_iter()
                .next()
                .map(|f| f.data)
                .ok_or_else(|| Fail::Core(Error::not_found("message", id)))
        })
        .await;
        g.done(r)
    }

    /// Moves or copies a message, and says where it went.
    async fn relocate(&self, id: &MessageId, to: &FolderId, moving: bool) -> Result<MessageId> {
        let at = Ref::parse(id)?;
        let mut g = self.lock().await?;
        let t = g.timeout;
        let r = bounded(t, async {
            let live = g.live();
            select_ref(live, &at).await?;
            // Its `Message-ID:` finds it again where it goes: the server
            // gives it a new UID, and says which only to some clients.
            let header = fetch(live, &[at.uid], SUMMARY, false).await?;
            let wanted = match header.first() {
                Some(h) => parse::threading(&h.data)?.0,
                None => None,
            };
            let before = live
                .session
                .status(wire(to.as_str()), "(UIDNEXT)")
                .await?
                .uid_next;
            let uids = at.uid.to_string();
            if moving && live.caps.moves {
                live.session.uid_mv(&uids, wire(to.as_str())).await?;
            } else {
                live.session.uid_copy(&uids, wire(to.as_str())).await?;
                if moving {
                    expunge(live, at.uid).await?;
                }
            }
            locate(live, to.as_str(), wanted.as_deref(), before).await
        })
        .await;
        g.done(r)
    }

    /// Appends `message` to `folder` and says what it is called there.
    async fn store(
        &self,
        folder: &FolderId,
        message: &[u8],
        flags: &str,
        message_id: &str,
    ) -> Result<MessageId> {
        let mut g = self.lock().await?;
        let t = g.timeout;
        let r = bounded(t, async {
            let live = g.live();
            let before = live
                .session
                .status(wire(folder.as_str()), "(UIDNEXT)")
                .await?
                .uid_next;
            live.session
                .append(wire(folder.as_str()), Some(flags), None, message)
                .await?;
            locate(live, folder.as_str(), Some(message_id), before).await
        })
        .await;
        g.done(r)
    }

    /// What `draft` takes from the message it comes from, and what
    /// threads it.
    async fn prepared(&self, draft: &Draft) -> Result<(Draft, Threading)> {
        let mut draft = draft.clone();
        let mut threading = Threading::default();
        match draft.origin.clone() {
            None => {}
            Some(DraftOrigin::Reply(original)) => {
                let (id, refs) = parse::threading(&self.raw(&original).await?)?;
                threading.in_reply_to.clone_from(&id);
                threading.references = refs.into_iter().chain(id).collect();
            }
            Some(DraftOrigin::Forward(original)) => {
                let data = self.raw(&original).await?;
                let mut files = parse::files(&data, &draft.drop_attachments)?;
                files.append(&mut draft.attachments);
                draft.attachments = files;
            }
            Some(DraftOrigin::Recalled(old)) => {
                let data = self.raw(&old).await?;
                let mut files = parse::files(&data, &draft.drop_attachments)?;
                files.append(&mut draft.attachments);
                draft.attachments = files;
                // It goes on answering what it did.
                let (_, refs) = parse::threading(&data)?;
                threading.in_reply_to = refs.last().cloned();
                threading.references = refs;
            }
        }
        Ok((draft, threading))
    }
}

/// Deletes the message `uid` of the selected folder.
async fn expunge(live: &mut Live, uid: u32) -> Res<()> {
    let _: Vec<Fetch> = live
        .session
        .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")
        .await?
        .try_collect()
        .await?;
    if live.caps.uidplus {
        let _: Vec<u32> = live
            .session
            .uid_expunge(uid.to_string())
            .await?
            .try_collect()
            .await?;
    } else {
        let _: Vec<u32> = live.session.expunge().await?.try_collect().await?;
    }
    Ok(())
}

/// What the message that went into `folder` is called there: the newest
/// with the `Message-ID:` `wanted`, or, without one, the one that would
/// have `before` (the folder's `UIDNEXT` then).
async fn locate(
    live: &mut Live,
    folder: &str,
    wanted: Option<&str>,
    before: Option<u32>,
) -> Res<MessageId> {
    let validity = select(live, folder).await?;
    let found = match wanted {
        Some(id) => {
            let query = format!("HEADER Message-ID \"{}\"", id.replace(['\\', '"'], ""));
            search(live, &query).await?.first().copied()
        }
        None => None,
    };
    let uid = found.or(before).ok_or_else(|| {
        Error::Backend(format!(
            "{folder}: the message went there, but where it is isn't known"
        ))
    })?;
    Ok(Ref {
        folder: folder.to_owned(),
        validity,
        uid,
    }
    .id())
}

impl<A: Authenticator> MailSender for ImapMailStore<A> {
    async fn send(&self, draft: &Draft) -> Result<()> {
        let smtp = self
            .smtp
            .as_ref()
            .ok_or_else(|| Error::NotSent("no SMTP server is set (smtp_url)".into()))?;
        let (draft, threading) = self.prepared(draft).await.map_err(|e| match e {
            Error::Network(e) => Error::NotSent(e),
            e => e,
        })?;
        let (message_id, kept) = smtp.send(&draft, &threading).await?;
        // Once the message is out, what is left to do can't take that
        // back: a copy that can't be kept, or a draft that can't be
        // deleted, is the user's to tidy up.
        if draft.save_copy
            && let Ok(sent) = self.well_known_folder(WellKnownFolder::SentItems).await
        {
            let _ = self.store(&sent.id, &kept, "(\\Seen)", &message_id).await;
        }
        if let Some(DraftOrigin::Recalled(old)) = &draft.origin {
            let _ = self.delete_message(old).await;
        }
        Ok(())
    }
}

/// How the lists of messages a page holds are cut: here for the tests.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_account_is_the_user_at_the_server() {
        assert_eq!(
            account("mail.example.com", "me").as_str(),
            "me@mail.example.com"
        );
        assert_eq!(
            account("mail.example.com", "me@example.org").as_str(),
            "me@example.org"
        );
    }

    #[test]
    fn roles_come_from_attributes_then_names() {
        assert_eq!(role("inbox", Some("/"), &[]), Some(WellKnownFolder::Inbox));
        assert_eq!(
            role("Mail/Out", Some("/"), &[NameAttribute::Sent]),
            Some(WellKnownFolder::SentItems)
        );
        assert_eq!(
            role("Sent Items", Some("/"), &[]),
            Some(WellKnownFolder::SentItems)
        );
        assert_eq!(
            role("INBOX.Drafts", Some("."), &[]),
            Some(WellKnownFolder::Drafts)
        );
        // Deeper down, a name that looks right is only a name.
        assert_eq!(role("Work/Sent", Some("/"), &[]), None);
        assert_eq!(role("Projects", Some("/"), &[]), None);
    }

    #[test]
    fn a_folder_is_below_the_one_its_name_starts_with() {
        assert_eq!(
            parent_name("INBOX.Work.2026", Some(".")),
            Some("INBOX.Work")
        );
        assert_eq!(parent_name("INBOX", Some(".")), None);
        assert_eq!(parent_name("Flat", None), None);
    }
}
