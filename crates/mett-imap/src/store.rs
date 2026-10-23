//! The mailbox, over one IMAP connection.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use async_imap::Session;
use async_imap::imap_proto::{MailboxDatum, NameAttribute, Response, Status, StatusAttribute};
use async_imap::types::{Fetch, Flag};
use chrono::{DateTime, Utc};
use futures_util::TryStreamExt;
use mett_core::{
    AccountId, Address, Attachment, AttachmentId, Authenticator, Draft, DraftOrigin, Error, Flags,
    FlagsUpdate, Folder, FolderChanges, FolderId, Login, MailSender, MailStore, Message, MessageId,
    MessagePage, MessageSummary, NoTokens, Page, PageCursor, RECENT_UNREAD, Result, SyncState,
    WellKnownFolder,
};
use mett_smtp::{SmtpSender, Threading};
use tokio::sync::{Mutex, MutexGuard};
use utf7_imap::{decode_utf7_imap, encode_utf7_imap};

use crate::conn::{self, Conn, Credentials, LoginFailure, within};
use crate::ids::Ref;
use crate::marks::{Floor, Marks, Watch};
use crate::parse::{self, Fetched};
use crate::settings::ImapSettings;
use crate::uidset::UidSet;

/// How long a listing of the folders is taken for what they are, where the
/// listing is not what was asked for (a folder's counts, or the folder that
/// is the Sent one): a poll asks about every mailbox, and each would list
/// them all again.
const LISTING_TTL: Duration = Duration::from_secs(30);

/// What a connection that sat idle this long is asked before it is used:
/// servers drop idle ones, and the first command on a dropped one would
/// fail.
const STALE: Duration = Duration::from_secs(60);

/// What to ask of the server about each message of a listing: its
/// header, to make the index line, and its parts, to tell whether it has
/// attachments.
const SUMMARY: &str = "(UID FLAGS INTERNALDATE BODY.PEEK[HEADER] BODYSTRUCTURE)";

/// Whether the server can be synced is not known yet, is so, is not.
const SYNCS_UNKNOWN: u8 = 0;
const SYNCS_YES: u8 = 1;
const SYNCS_NO: u8 = 2;

/// Whether messages are dated by when they came in: where the server can't
/// sort them by the dates they say, they are listed by arrival, and a cache
/// lists them in the same order only if it has the same dates.
fn by_arrival(caps: &Caps) -> bool {
    !caps.sort
}

/// How many messages a sync takes in at a time (the summaries of a round of
/// `sync_folder`), whatever the number of `FETCH` commands that takes.
const SYNC_BATCH: usize = 100;

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
    /// `SORT`, which orders messages by their dates.
    sort: bool,
    /// `CONDSTORE`, which tells what changed since a modification sequence.
    condstore: bool,
    /// `SPECIAL-USE`, which says which folder is Sent, Drafts and so on, if
    /// it is asked to (`RETURN (SPECIAL-USE)`).
    special_use: bool,
    /// `LIST-STATUS`, which says how many messages folders hold in the
    /// listing of them (`RETURN (STATUS (...))`).
    list_status: bool,
}

/// A connection that is logged in.
struct Live {
    session: Session<Conn>,
    caps: Caps,
    used: Instant,
    /// The folders as last listed, and when.
    listing: Option<(Instant, Vec<Listed>)>,
    /// The folder that is selected, with its `UIDVALIDITY`, as the last
    /// `SELECT` left it: what works on a message by its `UID` needs not
    /// select it again. Not known (`None`) after a command failed.
    open: Option<(String, u32)>,
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
    /// How each folder stood when it was last asked about new mail.
    marks: Marks,
    /// Whether the server can be synced (see [`MailStore::can_sync`]): not
    /// known until connected, when `SYNCS_UNKNOWN`.
    syncs: std::sync::atomic::AtomicU8,
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
            marks: Marks::default(),
            syncs: std::sync::atomic::AtomicU8::new(SYNCS_UNKNOWN),
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
        account(&self.settings.server.host, self.login.user())
    }

    async fn connect(&self) -> Result<Live> {
        let host = &self.settings.server.host;
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
        let caps = within(self.settings.server.timeout, session.capabilities())
            .await?
            .map_err(|e| conn::lost(host, &e.to_string()))?;
        let condstore = caps.has_str("CONDSTORE") || caps.has_str("QRESYNC");
        self.syncs.store(
            if condstore { SYNCS_YES } else { SYNCS_NO },
            std::sync::atomic::Ordering::SeqCst,
        );
        Ok(Live {
            session,
            caps: Caps {
                moves: caps.has_str("MOVE"),
                uidplus: caps.has_str("UIDPLUS"),
                sort: caps.has_str("SORT"),
                condstore: caps.has_str("CONDSTORE") || caps.has_str("QRESYNC"),
                special_use: caps.has_str("SPECIAL-USE"),
                list_status: caps.has_str("LIST-STATUS"),
            },
            used: Instant::now(),
            listing: None,
            open: None,
        })
    }

    /// The connection, logged in (again, if it was lost or has gone
    /// stale).
    async fn lock(&self) -> Result<Guard<'_>> {
        let mut slot = self.live.lock().await;
        if let Some(live) = slot.as_mut()
            && live.used.elapsed() > STALE
        {
            let alive = within(self.settings.server.timeout, live.session.noop()).await;
            if !matches!(alive, Ok(Ok(()))) {
                *slot = None;
            }
        }
        if slot.is_none() {
            *slot = Some(self.connect().await?);
        }
        Ok(Guard {
            slot,
            host: &self.settings.server.host,
            timeout: self.settings.server.timeout,
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
        if result.is_err()
            && let Some(live) = self.slot.as_mut()
        {
            // Where the server stands after a command that failed (or was
            // given up on) is not to be taken for granted.
            live.open = None;
        }
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
        attachments: fetch.bodystructure().map(parse::has_attachment),
        data: if whole { fetch.body() } else { fetch.header() }
            .unwrap_or_default()
            .to_vec(),
    })
}

/// A set of UIDs as a command takes them: ranges where they run on, so that
/// a long one doesn't make a command line too long for the server.
fn set(uids: impl IntoIterator<Item = u32>) -> String {
    UidSet::of(uids).to_string()
}

/// How many messages one `FETCH` command asks about at most, so that a command
/// on thousands of them is not more than a server takes: a limit of the
/// command, not of a sync round.
const FETCH_BATCH: usize = 200;

/// The role a folder has, by what the server says of it (and then it is
/// declared), or else by its name.
fn role(
    name: &str,
    delimiter: Option<&str>,
    attributes: &[NameAttribute<'_>],
) -> Option<(WellKnownFolder, bool)> {
    if name.eq_ignore_ascii_case("INBOX") {
        return Some((WellKnownFolder::Inbox, true));
    }
    for attribute in attributes {
        let declared = match attribute {
            NameAttribute::Sent => WellKnownFolder::SentItems,
            NameAttribute::Drafts => WellKnownFolder::Drafts,
            NameAttribute::Trash => WellKnownFolder::DeletedItems,
            NameAttribute::Junk => WellKnownFolder::JunkEmail,
            NameAttribute::Archive => WellKnownFolder::Archive,
            _ => continue,
        };
        return Some((declared, true));
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
    Some((
        match last.as_str() {
            "sent" | "sent items" | "sent messages" | "sent mail" => WellKnownFolder::SentItems,
            "drafts" => WellKnownFolder::Drafts,
            "trash" | "deleted items" | "deleted messages" | "bin" => WellKnownFolder::DeletedItems,
            "junk" | "spam" | "junk email" | "junk e-mail" => WellKnownFolder::JunkEmail,
            "archive" | "archives" => WellKnownFolder::Archive,
            "outbox" => WellKnownFolder::Outbox,
            _ => return None,
        },
        false,
    ))
}

/// A folder from a listing, without counts.
#[derive(Clone)]
struct Listed {
    name: String,
    delimiter: Option<String>,
    selectable: bool,
    role: Option<WellKnownFolder>,
    /// Whether the server says so (`SPECIAL-USE`), rather than the name.
    declared: bool,
}

fn listed(name: &str, delimiter: Option<&str>, attributes: &[NameAttribute<'_>]) -> Listed {
    let decoded = decode_utf7_imap(name.to_owned());
    let found = role(&decoded, delimiter, attributes);
    Listed {
        role: found.map(|(role, _)| role),
        declared: found.is_some_and(|(_, declared)| declared),
        selectable: !attributes.iter().any(|a| match a {
            NameAttribute::NoSelect => true,
            NameAttribute::Extension(x) => x.eq_ignore_ascii_case("\\NonExistent"),
            _ => false,
        }),
        delimiter: delimiter.map(str::to_owned),
        name: decoded,
    }
}

/// The folder above `name`, if the server has hierarchy.
fn parent_name<'a>(name: &'a str, delimiter: Option<&str>) -> Option<&'a str> {
    let delimiter = delimiter?;
    name.rsplit_once(delimiter).map(|(parent, _)| parent)
}

/// How many messages folders hold, by name: how many are unread, and the
/// total.
type Counts = HashMap<String, (u32, u32)>;

/// What a batch of commands got.
struct Batch {
    /// What `LIST` said.
    listed: Vec<Listed>,
    /// What `STATUS` said (also in the listing, with `LIST-STATUS`).
    counts: Counts,
    /// How each command ended, in the order they were sent.
    ended: Vec<Res<()>>,
}

/// A name, as a command takes it.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Runs `commands` one after the other without waiting for the server in
/// between: they are written, and then the answers read. That is a round
/// trip for all, not for each.
async fn batch(live: &mut Live, commands: &[String]) -> Res<Batch> {
    let mut ids = Vec::with_capacity(commands.len());
    for command in commands {
        ids.push(live.session.run_command(command).await?);
    }
    let mut got = Batch {
        listed: Vec::new(),
        counts: Counts::new(),
        ended: Vec::new(),
    };
    let mut ended: Vec<Option<Res<()>>> = commands.iter().map(|_| None).collect();
    let mut pending = commands.len();
    while pending > 0 {
        let response = live
            .session
            .read_response()
            .await
            .map_err(async_imap::error::Error::Io)?;
        let Some(response) = response else {
            return Err(async_imap::error::Error::ConnectionLost.into());
        };
        match response.parsed() {
            Response::MailboxData(MailboxDatum::List(data)) => got.listed.push(listed(
                &data.name,
                data.delimiter.as_deref(),
                &data.name_attributes,
            )),
            Response::MailboxData(MailboxDatum::Status { mailbox, status }) => {
                let (mut unseen, mut total) = (0, 0);
                for attribute in status {
                    match attribute {
                        StatusAttribute::Unseen(n) => unseen = *n,
                        StatusAttribute::Messages(n) => total = *n,
                        _ => {}
                    }
                }
                got.counts
                    .insert(decode_utf7_imap(mailbox.to_string()), (unseen, total));
            }
            Response::Done {
                tag,
                status,
                outcome,
            } => {
                if let Some(at) = ids.iter().position(|id| id.0 == tag.0)
                    && ended[at].is_none()
                {
                    let why = outcome
                        .information
                        .as_deref()
                        .unwrap_or_default()
                        .to_owned();
                    ended[at] = Some(match status {
                        Status::Ok => Ok(()),
                        Status::No => Err(async_imap::error::Error::No(why).into()),
                        _ => Err(async_imap::error::Error::Bad(why).into()),
                    });
                    pending -= 1;
                }
            }
            _ => {}
        }
    }
    got.ended = ended.into_iter().flatten().collect();
    Ok(got)
}

/// How many `STATUS` commands go out before the answers are read: no
/// more than a server (or the connection, in both directions) takes
/// unanswered.
const PIPELINE: usize = 50;

/// How many messages `names` hold, from the server, asked of without
/// waiting for the answer to each. A folder that can't be asked about fails
/// the whole, as it would one at a time.
async fn counted(live: &mut Live, names: &[&str], timeout: Duration) -> Res<Counts> {
    let mut counts = Counts::new();
    for round in names.chunks(PIPELINE) {
        let commands: Vec<String> = round
            .iter()
            .map(|name| format!("STATUS {} (MESSAGES UNSEEN)", quoted(&wire(name))))
            .collect();
        // The time allowed is for each of them: a server may take each
        // that long.
        let got = bounded(timeout * round.len() as u32, batch(live, &commands)).await?;
        for (name, ended) in round.iter().zip(got.ended) {
            ended?;
            // A server that says nothing of a folder says it is empty.
            let found = got.counts.get(*name).copied().unwrap_or((0, 0));
            counts.insert((*name).to_owned(), found);
        }
    }
    Ok(counts)
}

/// What a listing is asked for, besides the folders.
#[derive(Clone, Copy, Default)]
struct Want<'a> {
    /// How many messages every folder holds (where the server says so in
    /// the listing).
    all_counts: bool,
    /// A folder to ask of at the same time, whatever the listing: it
    /// costs no round trip of its own.
    count_of: Option<&'a str>,
}

/// The folders, listed again if `fresh` or if the listing before is old.
async fn all_folders(live: &mut Live, fresh: bool) -> Res<Vec<Listed>> {
    Ok(listing(live, fresh, Want::default()).await?.0)
}

/// As `all_folders`, with what `want` says of how many messages: that is
/// asked only when the folders are listed (those cached are of no use).
async fn listing(live: &mut Live, fresh: bool, want: Want<'_>) -> Res<(Vec<Listed>, Counts)> {
    if !fresh
        && let Some((at, listing)) = &live.listing
        && at.elapsed() < LISTING_TTL
    {
        return Ok((listing.clone(), Counts::new()));
    }
    let (listing, counts) = list(live, "*", want).await?;
    live.listing = Some((Instant::now(), listing.clone()));
    Ok((listing, counts))
}

async fn list(live: &mut Live, pattern: &str, want: Want<'_>) -> Res<(Vec<Listed>, Counts)> {
    // Some servers say which folders are Sent, Drafts and so on only when
    // asked, and some how many messages the folders hold: those that can
    // are, and one that refuses is listed as every server can be.
    let mut options = Vec::new();
    if live.caps.special_use {
        options.push("SPECIAL-USE");
    }
    let counting = want.all_counts && live.caps.list_status;
    if counting {
        options.push("STATUS (MESSAGES UNSEEN)");
    }
    let mut attempts = vec![options.join(" ")];
    if counting && live.caps.special_use {
        attempts.push("SPECIAL-USE".to_owned());
    }
    attempts.push(String::new());
    attempts.dedup();
    let mut counts = Counts::new();
    let mut extra: Vec<String> = want
        .count_of
        .iter()
        .map(|name| format!("STATUS {} (MESSAGES UNSEEN)", quoted(&wire(name))))
        .collect();
    let last = attempts.len() - 1;
    for (n, options) in attempts.iter().enumerate() {
        let mut commands = vec![if options.is_empty() {
            format!("LIST \"\" {}", quoted(pattern))
        } else {
            format!("LIST \"\" {} RETURN ({options})", quoted(pattern))
        }];
        commands.append(&mut extra);
        let got = batch(live, &commands).await?;
        counts.extend(got.counts);
        match got.ended.into_iter().next() {
            Some(Err(Fail::Imap(
                async_imap::error::Error::Bad(_) | async_imap::error::Error::No(_),
            ))) if n < last => continue,
            Some(Err(e)) => return Err(e),
            _ => {}
        }
        let mut all = got.listed;
        // A folder the server says has a role has it alone: another that
        // only has the usual name for it is an ordinary folder.
        let declared: HashSet<WellKnownFolder> = all
            .iter()
            .filter(|l| l.declared)
            .filter_map(|l| l.role)
            .collect();
        for item in &mut all {
            if !item.declared && item.role.is_some_and(|r| declared.contains(&r)) {
                item.role = None;
            }
        }
        return Ok((all, counts));
    }
    unreachable!("the last attempt returns")
}

/// `listed` as a folder, with its counts from the server.
async fn folder_of(live: &mut Live, listed: &Listed, parent: Option<FolderId>) -> Res<Folder> {
    let counts = if listed.selectable {
        let status = live
            .session
            .status(wire(&listed.name), "(MESSAGES UNSEEN)")
            .await?;
        (status.unseen.unwrap_or(0), status.exists)
    } else {
        (0, 0)
    };
    Ok(folder_with(listed, parent, counts))
}

/// `listed` as a folder, that holds `counts` (unread, total).
fn folder_with(listed: &Listed, parent: Option<FolderId>, counts: (u32, u32)) -> Folder {
    let display = match (&parent, &listed.delimiter) {
        (Some(_), Some(d)) => listed
            .name
            .rsplit(d.as_str())
            .next()
            .unwrap_or(&listed.name),
        _ => &listed.name,
    };
    Folder {
        id: FolderId::new(&listed.name),
        parent,
        name: display.to_owned(),
        well_known: listed.role,
        unread_count: counts.0,
        total_count: counts.1,
    }
}

/// The folders of the mailbox, or, with `under`, those directly below the
/// folder it holds (the top ones if it holds none): the counts of messages
/// come with the listing where the server can, and else are asked for
/// together, not one at a time.
async fn folders(
    live: &mut Live,
    under: Option<Option<&FolderId>>,
    timeout: Duration,
) -> Res<Vec<Folder>> {
    // With `under`, those that are asked about are few: not all of them.
    let want = Want {
        all_counts: under.is_none(),
        count_of: None,
    };
    let (all, mut counts) = bounded(timeout, listing(live, true, want)).await?;
    let names: HashSet<String> = all.iter().map(|l| l.name.clone()).collect();
    let mut chosen = Vec::with_capacity(all.len());
    for item in &all {
        let parent = parent_name(&item.name, item.delimiter.as_deref())
            .filter(|p| names.contains(*p))
            .map(FolderId::new);
        if under.is_some_and(|under| under != parent.as_ref()) {
            continue;
        }
        chosen.push((item, parent));
    }
    let missing: Vec<&str> = chosen
        .iter()
        .filter(|(item, _)| item.selectable && !counts.contains_key(&item.name))
        .map(|(item, _)| item.name.as_str())
        .collect();
    counts.extend(counted(live, &missing, timeout).await?);
    Ok(chosen
        .into_iter()
        .map(|(item, parent)| {
            let found = counts.get(&item.name).copied().unwrap_or((0, 0));
            folder_with(item, parent, found)
        })
        .collect())
}

async fn folder(live: &mut Live, id: &FolderId) -> Res<Folder> {
    // The whole listing, which says what roles the others have: if it is
    // listed again, the folder is asked of in the same round trip.
    let want = Want {
        all_counts: false,
        count_of: Some(id.as_str()),
    };
    let (all, mut counts) = listing(live, false, want).await?;
    let item = all
        .iter()
        .find(|l| l.name == id.as_str())
        .ok_or_else(|| Error::not_found("folder", id))?;
    let parent = parent_name(&item.name, item.delimiter.as_deref())
        .filter(|p| all.iter().any(|l| l.name == *p))
        .map(FolderId::new);
    match counts.remove(&item.name) {
        Some(found) if item.selectable => Ok(folder_with(item, parent, found)),
        _ => folder_of(live, item, parent).await,
    }
}

/// A folder that is selected.
struct Selected {
    /// Its `UIDVALIDITY`.
    validity: u32,
    /// The `UID` the next message to come in will have.
    next: u32,
    /// How many messages it holds.
    exists: u32,
    /// Its highest modification sequence, if the server has them.
    modseq: Option<u64>,
}

impl Selected {
    /// How the folder stands.
    fn floor(&self) -> Floor {
        Floor {
            next: self.next,
            modseq: self.modseq,
        }
    }
}

/// Selects `name`.
async fn select(live: &mut Live, name: &str) -> Res<Selected> {
    // A `SELECT` that fails leaves nothing selected.
    live.open = None;
    let selected = if live.caps.condstore {
        live.session.select_condstore(wire(name)).await
    } else {
        live.session.select(wire(name)).await
    };
    let mailbox = selected.map_err(|e| match e {
        async_imap::error::Error::No(_) => Fail::Core(Error::not_found("folder", name)),
        e => Fail::Imap(e),
    })?;
    live.open = Some((name.to_owned(), mailbox.uid_validity.unwrap_or(0)));
    Ok(Selected {
        validity: mailbox.uid_validity.unwrap_or(0),
        next: mailbox.uid_next.unwrap_or(0),
        exists: mailbox.exists,
        modseq: mailbox.highest_modseq,
    })
}

/// Selects the folder of `at`, as long as it is the one `at` was made in.
/// A folder that is selected already is not selected again: that is a round
/// trip, and what is done to a message by its `UID` doesn't need what a
/// `SELECT` says anew (how many messages the folder holds, and so on); the
/// listings and syncs, which do, select the folder themselves.
async fn select_ref(live: &mut Live, at: &Ref) -> Res<()> {
    if live
        .open
        .as_ref()
        .is_some_and(|(folder, validity)| *folder == at.folder && *validity == at.validity)
    {
        return Ok(());
    }
    if select(live, &at.folder).await?.validity != at.validity {
        return Err(Error::not_found("message", at.id()).into());
    }
    Ok(())
}

/// The messages of the selected folder with `uids`, as `fetch` asks.
async fn fetch(live: &mut Live, uids: &[u32], what: &str, whole: bool) -> Res<Vec<Fetched>> {
    let mut found = Vec::with_capacity(uids.len());
    // In batches: a command for thousands of messages is more than some
    // servers take.
    for batch in uids.chunks(FETCH_BATCH) {
        let fetches: Vec<Fetch> = live
            .session
            .uid_fetch(set(batch.iter().copied()), what)
            .await?
            .try_collect()
            .await?;
        found.extend(fetches.iter().filter_map(|f| fetched(f, whole)));
    }
    Ok(found)
}

/// The summaries of `uids` in the selected folder, in that order.
async fn summaries(
    live: &mut Live,
    folder: &str,
    validity: u32,
    uids: &[u32],
) -> Res<Vec<MessageSummary>> {
    let found = fetch(live, uids, SUMMARY, false).await?;
    let by_arrival = by_arrival(&live.caps);
    let by_uid: HashMap<u32, &Fetched> = found.iter().map(|f| (f.uid, f)).collect();
    // A message that is gone by now is left out.
    uids.iter()
        .filter_map(|uid| by_uid.get(uid))
        .map(|f| Ok(parse::summary(folder, validity, f, by_arrival)?))
        .collect()
}

/// The UIDs a search finds in the selected folder, newest first.
async fn search(live: &mut Live, query: &str) -> Res<Vec<u32>> {
    let mut uids: Vec<u32> = live.session.uid_search(query).await?.into_iter().collect();
    uids.sort_unstable_by(|a, b| b.cmp(a));
    Ok(uids)
}

/// The UIDs a search finds in the selected folder, newest first by the
/// dates the messages say, where the server can sort, and else by UID.
async fn ordered(live: &mut Live, criteria: &str) -> Res<Vec<u32>> {
    if !live.caps.sort {
        return search(live, criteria).await;
    }
    let id = live
        .session
        .run_command(format!("UID SORT (REVERSE DATE) US-ASCII {criteria}"))
        .await?;
    let mut found = Vec::new();
    loop {
        let response = live
            .session
            .read_response()
            .await
            .map_err(async_imap::error::Error::Io)?;
        let Some(response) = response else {
            return Err(async_imap::error::Error::ConnectionLost.into());
        };
        match response.parsed() {
            Response::MailboxData(MailboxDatum::Sort(uids)) => found.clone_from(uids),
            Response::Done {
                tag,
                status,
                outcome,
            } if tag.0 == id.0 => {
                let why = outcome
                    .information
                    .as_deref()
                    .unwrap_or_default()
                    .to_owned();
                return match status {
                    Status::Ok => Ok(found),
                    Status::No => Err(async_imap::error::Error::No(why).into()),
                    _ => Err(async_imap::error::Error::Bad(why).into()),
                };
            }
            _ => {}
        }
    }
}

/// How many of a page's last messages its cursor names: if the last was
/// deleted before the next page was asked for, the one before it will do.
const ANCHORS: usize = 16;

/// The cursor after the page `taken` of a folder whose `UIDVALIDITY` is
/// `validity`: the page's last messages (which are only the same messages
/// while the folder is the same).
fn cursor_after(validity: u32, taken: &[u32]) -> PageCursor {
    let tail = &taken[taken.len().saturating_sub(ANCHORS)..];
    let tail: Vec<String> = tail.iter().map(u32::to_string).collect();
    PageCursor::new(format!("{validity}:{}", tail.join(",")))
}

/// Where a page starts: after the last of the messages its cursor names that
/// is still there. If every one is gone, the place is lost: the folder
/// changed too much while it was being listed, and has to be listed again.
fn start_of(cursor: Option<&PageCursor>, validity: u32, all: &[u32]) -> Res<usize> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let (cursor_validity, tail) = cursor.as_str().split_once(':').unwrap_or(("", ""));
    if cursor_validity.parse() != Ok(validity) {
        // The folder was made anew: its messages are other ones.
        return Err(Error::Backend(
            "the folder changed while it was being listed: list it again".into(),
        )
        .into());
    }
    let tail: Vec<u32> = tail.split(',').filter_map(|u| u.parse().ok()).collect();
    tail.iter()
        .rev()
        .find_map(|last| all.iter().position(|uid| uid == last))
        .map(|at| at + 1)
        .ok_or_else(|| {
            Error::Backend("the folder changed while it was being listed: list it again".into())
                .into()
        })
}

/// A page of the newest messages of `folder`: of all of them, or of `only`.
async fn page_of(
    live: &mut Live,
    folder: &str,
    only: Option<BTreeSet<u32>>,
    page: &Page,
) -> Res<(MessagePage, Selected)> {
    let selected = select(live, folder).await?;
    let validity = selected.validity;
    let all = match only {
        Some(only) if only.is_empty() => Vec::new(),
        Some(only) => ordered(live, &format!("UID {}", set(only))).await?,
        None => ordered(live, "ALL").await?,
    };
    let start = start_of(page.cursor(), validity, &all)?;
    let size = page.size() as usize;
    let taken = &all[start..(start + size).min(all.len())];
    let next = (start + size < all.len()).then(|| cursor_after(validity, taken));
    let messages = summaries(live, folder, validity, taken).await?;
    Ok((MessagePage { messages, next }, selected))
}

/// The messages that came in at or after `since`, with when (the server
/// dates them by the day, in its own zone, so this asks for the day before
/// too, and keeps those that did by the minute). Of the unread ones
/// (`unseen`), only the newest `RECENT_UNREAD * 4` are looked at: no more
/// than `RECENT_UNREAD` are told.
async fn arrived_since(
    live: &mut Live,
    since: DateTime<Utc>,
    unseen: bool,
) -> Res<HashMap<u32, DateTime<Utc>>> {
    let day = day(since - chrono::TimeDelta::days(1));
    let query = if unseen {
        format!("UNSEEN SINCE {day}")
    } else {
        format!("SINCE {day}")
    };
    let mut uids: Vec<u32> = search(live, &query).await?;
    if unseen {
        uids.truncate(RECENT_UNREAD * 4);
    }
    Ok(fetch(live, &uids, "(UID FLAGS INTERNALDATE)", false)
        .await?
        .into_iter()
        .filter_map(|f| Some((f.uid, f.internal?)))
        .filter(|(_, at)| *at >= since)
        .collect())
}

/// What is new in the selected folder since `floor`: the messages from its
/// `UID` on, which came in (even if they are as old as the mail that was
/// copied in), and, where the server has modification sequences, those that
/// changed since it (a flag another client changed too).
async fn changed_since(live: &mut Live, selected: &Selected, floor: Floor) -> Res<BTreeSet<u32>> {
    let mut changed = BTreeSet::new();
    match floor.modseq.filter(|_| live.caps.condstore) {
        Some(modseq) if selected.exists > 0 => {
            let fetches: Vec<Fetch> = live
                .session
                .uid_fetch("1:*", format!("(UID FLAGS) (CHANGEDSINCE {modseq})"))
                .await?
                .try_collect()
                .await?;
            changed.extend(fetches.iter().filter_map(|f| f.uid));
        }
        Some(_) => {}
        None => {
            let fresh = search(live, &format!("UID {}:*", floor.next)).await?;
            changed.extend(fresh.into_iter().filter(|uid| *uid >= floor.next));
        }
    }
    Ok(changed)
}

/// `IMAP`'s date for a search: the day, which is as exact as it gets.
fn day(at: DateTime<Utc>) -> String {
    at.format("%d-%b-%Y").to_string()
}

impl<A: Authenticator> MailStore for ImapMailStore<A> {
    fn folder_names_ignore_case(&self) -> bool {
        false
    }

    /// A message's body is the one it came with: only its flags change.
    fn bodies_change(&self) -> bool {
        false
    }

    /// Whether the server tells what changed since a modification sequence
    /// (`CONDSTORE`), which a sync needs to find flags that changed. That is
    /// known once connected (the first request connects); until then it is
    /// taken to, so that what an earlier run cached can be read offline.
    fn can_sync(&self) -> bool {
        self.syncs.load(std::sync::atomic::Ordering::SeqCst) != SYNCS_NO
    }

    async fn sync_folder(
        &self,
        folder: &FolderId,
        state: Option<&SyncState>,
    ) -> Result<FolderChanges> {
        let mut g = self.lock().await?;
        let t = g.timeout;
        let r = bounded(t, sync::sync_folder(g.live(), folder.as_str(), state)).await;
        g.done(r)
    }

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
        let r = folders(g.live(), None, t).await;
        g.done(r)
    }

    async fn child_folders(&self, parent: Option<&FolderId>) -> Result<Vec<Folder>> {
        let mut g = self.lock().await?;
        let t = g.timeout;
        let r = folders(g.live(), Some(parent), t).await;
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
            let selected = select(live, folder.as_str()).await?;
            let floor = self
                .marks
                .floor(Watch::Unread, folder.as_str(), selected.validity, true);
            // What came in at or after `since`, and what is new since the
            // last time this asked, whatever its date says (it may have
            // been copied in, or marked unread by another client).
            let mut found: HashMap<u32, DateTime<Utc>> = HashMap::new();
            let unseen: BTreeSet<u32> = search(live, "UNSEEN").await?.into_iter().collect();
            if let Some(since) = since {
                found.extend(arrived_since(live, since, true).await?);
            } else {
                // The newest are enough: no more than that are told.
                let newest: Vec<u32> = unseen
                    .iter()
                    .rev()
                    .take(RECENT_UNREAD * 4)
                    .copied()
                    .collect();
                let dates = fetch(live, &newest, "(UID FLAGS INTERNALDATE)", false).await?;
                found.extend(dates.into_iter().filter_map(|f| Some((f.uid, f.internal?))));
            }
            let mut fresh = BTreeSet::new();
            if let Some(floor) = floor {
                for uid in changed_since(live, &selected, floor).await? {
                    if unseen.contains(&uid) {
                        fresh.insert(uid);
                    }
                }
            }
            // What was found new stays reported until it is read: the user
            // may not have visited the folder when the next check comes.
            for (uid, at) in self.marks.new_unread(
                folder.as_str(),
                selected.validity,
                fresh,
                Utc::now(),
                &unseen,
            ) {
                found.insert(uid, at);
            }
            self.marks.commit(
                Watch::Unread,
                folder.as_str(),
                selected.validity,
                selected.floor(),
                floor,
            );
            // The newest first, by the date and, where dates tie (they are to
            // the second), by UID: what is cut off is the oldest.
            let mut found: Vec<(u32, DateTime<Utc>)> = found.into_iter().collect();
            found.sort_by_key(|(uid, at)| std::cmp::Reverse((*at, *uid)));
            found.truncate(RECENT_UNREAD);
            Ok(found
                .into_iter()
                .map(|(uid, at)| {
                    let id = Ref {
                        folder: folder.as_str().to_owned(),
                        validity: selected.validity,
                        uid,
                    }
                    .id();
                    (id, at)
                })
                .collect())
        })
        .await;
        g.done(r)
    }

    async fn well_known_folder(&self, which: WellKnownFolder) -> Result<Folder> {
        let mut g = self.lock().await?;
        let t = g.timeout;
        let r = bounded(t, async {
            let all = all_folders(g.live(), false).await?;
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
        let first = page.cursor().is_none();
        let r = bounded(t, async {
            let (listed, selected) = page_of(g.live(), folder.as_str(), None, &page).await?;
            if first {
                // What comes in from now on is new.
                self.marks
                    .start(folder.as_str(), selected.validity, selected.floor());
            }
            Ok(listed)
        })
        .await;
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
        let first = page.cursor().is_none();
        let r = bounded(t, async {
            let live = g.live();
            let selected = select(live, folder.as_str()).await?;
            // The pages of one scan compare with how the folder stood when
            // its first began, so that the mark can move on at once.
            let floor = self
                .marks
                .floor(Watch::Changes, folder.as_str(), selected.validity, first);
            let uids = match floor {
                Some(floor) => changed_since(live, &selected, floor).await?,
                None => BTreeSet::new(),
            };
            // Without a mark to compare with, or beyond it, what came in
            // after `since`, by the dates the server gives.
            let mut uids = uids;
            if floor.is_none_or(|f| f.modseq.is_none() || !live.caps.condstore) {
                uids.extend(arrived_since(live, since, false).await?.into_keys());
            }
            let (mut page, _) = page_of(live, folder.as_str(), Some(uids), &page).await?;
            // What this finds changed now, whatever date it came in with:
            // the new-mail check reads that to tell a change it hasn't seen
            // from one the listing had.
            let now = Utc::now();
            for message in &mut page.messages {
                message.modified = Some(now);
            }
            if first {
                self.marks.commit(
                    Watch::Changes,
                    folder.as_str(),
                    selected.validity,
                    selected.floor(),
                    floor,
                );
            }
            Ok(page)
        })
        .await;
        // A later page that fails leaves the scan unfinished: the mark its
        // first page moved on is given back, for the next scan to cover it.
        if r.is_err() && !first {
            self.marks.rollback(Watch::Changes, folder.as_str());
        }
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
            Ok(parse::message(
                &at.folder,
                at.validity,
                found,
                by_arrival(&live.caps),
            )?)
        })
        .await;
        g.done(r)
    }

    async fn message_folder(&self, id: &MessageId) -> Result<FolderId> {
        let at = Ref::parse(id)?;
        let mut g = self.lock().await?;
        let t = g.timeout;
        let r = bounded(t, async {
            let live = g.live();
            select_ref(live, &at).await?;
            // Not just the folder the identifier names: the message has to
            // be there still.
            if search(live, &format!("UID {}", at.uid)).await?.is_empty() {
                return Err(Error::not_found("message", id).into());
            }
            Ok(at.folder_id())
        })
        .await;
        g.done(r)
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
            for (name, on) in [("\\Seen", update.read), ("\\Flagged", update.flagged)] {
                if let Some(on) = on {
                    flag(live, &at, if on { '+' } else { '-' }, name).await?;
                }
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
            expunge(live, &at).await
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
        // A recalled draft that is saved again replaces the one recalled. The
        // new one is saved by now: if the old one can't be deleted, that is
        // the user's to tidy up, not a reason to save it all again.
        if let Some(DraftOrigin::Recalled(old)) = &draft.origin {
            let _ = self.delete_message(old).await;
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
            let Some(header) = header.first() else {
                return Err(Error::not_found("message", id).into());
            };
            let wanted = parse::threading(&header.data)?.0;
            let before = live
                .session
                .status(wire(to.as_str()), "(UIDNEXT)")
                .await?
                .uid_next;
            let uids = at.uid.to_string();
            if moving && live.caps.moves {
                live.session.uid_mv(&uids, wire(to.as_str())).await?;
            } else {
                if moving {
                    // Before the copy: a message that is copied and can't
                    // be deleted is in two folders.
                    expungeable(live)?;
                }
                live.session.uid_copy(&uids, wire(to.as_str())).await?;
                if moving {
                    expunge(live, &at).await?;
                }
            }
            locate(live, to.as_str(), wanted.as_deref(), before, &header.data).await
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
            locate(live, folder.as_str(), Some(message_id), before, &[]).await
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

/// Fails unless the server can delete one message of a folder: without
/// `UIDPLUS`, expunging takes every message marked deleted, including those
/// another client marked since, which no check beforehand can make safe.
fn expungeable(live: &Live) -> Res<()> {
    if live.caps.uidplus {
        return Ok(());
    }
    Err(Error::Backend(
        "this server can't delete just one message (it has no UIDPLUS): nothing was deleted".into(),
    )
    .into())
}

/// Changes the flags of the message `at` in the selected folder: `sign` is
/// `+` or `-`. A message that is no longer there is [`Error::NotFound`].
async fn flag(live: &mut Live, at: &Ref, sign: char, flag: &str) -> Res<()> {
    let changed: Vec<Fetch> = live
        .session
        .uid_store(at.uid.to_string(), format!("{sign}FLAGS ({flag})"))
        .await?
        .try_collect()
        .await?;
    if changed.is_empty() {
        return Err(Error::not_found("message", at.id()).into());
    }
    Ok(())
}

/// Deletes the message `at` from the selected folder.
async fn expunge(live: &mut Live, at: &Ref) -> Res<()> {
    expungeable(live)?;
    flag(live, at, '+', "\\Deleted").await?;
    let _: Vec<u32> = live
        .session
        .uid_expunge(at.uid.to_string())
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// What the message that went into `folder` is called there: the newest
/// from `before` on (the folder's `UIDNEXT` before it went in, so that
/// what came in earlier is not it) that has the `Message-ID:` `wanted`
/// exactly (a search finds those it is part of too), or, without one, whose
/// header is `header`.
async fn locate(
    live: &mut Live,
    folder: &str,
    wanted: Option<&str>,
    before: Option<u32>,
    header: &[u8],
) -> Res<MessageId> {
    /// The newest this many candidates are looked at.
    const LOOKED_AT: usize = 50;
    let validity = select(live, folder).await?.validity;
    let from = before.map_or_else(String::new, |before| format!("UID {before}:* "));
    let query = match wanted {
        Some(id) => format!(
            "{from}HEADER Message-ID \"{}\"",
            id.replace('\\', "\\\\").replace('"', "\\\"")
        ),
        None if before.is_some() => from.trim_end().to_owned(),
        // Nothing says where to look: the newest in the folder.
        None => "ALL".to_owned(),
    };
    let mut candidates: Vec<u32> = search(live, &query).await?;
    candidates.retain(|uid| before.is_none_or(|before| *uid >= before));
    candidates.truncate(LOOKED_AT);
    let headers = fetch(live, &candidates, SUMMARY, false).await?;
    let found = headers
        .iter()
        .filter(|f| match wanted {
            Some(id) => parse::threading(&f.data)
                .ok()
                .and_then(|(own, _)| own)
                .is_some_and(|own| own == id),
            None => f.data == header,
        })
        .map(|f| f.uid)
        .max();
    let uid = found.ok_or_else(|| {
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
        // Once the message is out, what is left to do can't take that back:
        // a draft that can't be deleted is the user's to tidy up. A copy
        // that can't be kept is said, as a sent message, not a failed one.
        let copied = if draft.save_copy {
            match self.well_known_folder(WellKnownFolder::SentItems).await {
                Ok(sent) => self
                    .store(&sent.id, &kept, "(\\Seen)", &message_id)
                    .await
                    .map(drop),
                Err(e) => Err(e),
            }
        } else {
            Ok(())
        };
        if let Some(DraftOrigin::Recalled(old)) = &draft.origin {
            let _ = self.delete_message(old).await;
        }
        copied.map_err(|e| Error::SentNotKept(e.to_string()))
    }
}

/// How the lists of messages a page holds are cut: here for the tests.
mod sync;

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
        let named = |role: Option<(WellKnownFolder, bool)>| role.map(|(r, _)| r);
        assert_eq!(
            role("inbox", Some("/"), &[]),
            Some((WellKnownFolder::Inbox, true))
        );
        assert_eq!(
            role("Mail/Out", Some("/"), &[NameAttribute::Sent]),
            Some((WellKnownFolder::SentItems, true))
        );
        // A name that looks right is only what it looks like.
        assert_eq!(
            role("Sent Items", Some("/"), &[]),
            Some((WellKnownFolder::SentItems, false))
        );
        assert_eq!(
            named(role("INBOX.Drafts", Some("."), &[])),
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
