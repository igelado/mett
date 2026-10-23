//! [`EwsMailStore`]: the [`MailStore`] port for Exchange Web Services.
//!
//! It works on the signed-in user's mailbox, with the EWS SOAP API. Every
//! request asks the [`Authenticator`] for a token, so refreshing and
//! signing in happen transparently.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::{DateTime, SecondsFormat, Utc};
use mett_core::{
    AccessToken, AccountId, Address, Answer, Attachment, AttachmentId, AttachmentKind,
    Authenticator, Body, BodyKind, Draft, DraftOrigin, Error, Flags, FlagsUpdate, Folder,
    FolderChanges, FolderId, Invitation, MailSender, MailStore, Message, MessageId, MessagePage,
    MessageSummary, OutgoingAttachment, Page, PageCursor, RECENT_UNREAD, Result, Scope, SyncState,
    ThreadPosition, WellKnownFolder,
};
use reqwest::StatusCode;
use roxmltree::Node;
use url::Url;

use crate::invitation::{
    INVITATION_FIELDS, answer_xml, find_occurrence, invitation, occurrence_view,
};
use crate::soap::{
    About, check, child, child_text, children, coded_fault, descendant, envelope, escape,
    only_message, parse, response_code, response_messages, text,
};

/// Upper bound on each EWS request with a small body. Larger ones (with
/// attachments) get a second more for each [`BYTES_PER_SECOND`].
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The slowest upload a request with attachments is given time for.
const BYTES_PER_SECOND: usize = 64 * 1024;

/// Upper bound on a request whose response may be large (an attachment's
/// content), which its own size doesn't tell.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How long the roles a folder listing looked up serve other calls, which
/// would otherwise look them up each time.
const ROLES_TTL: Duration = Duration::from_secs(15 * 60);

/// The most items or folders EWS returns per request.
const MAX_PAGE_SIZE: u32 = 1000;

/// The folder fields mett reads.
const FOLDER_FIELDS: &[&str] = &[
    "folder:DisplayName",
    "folder:ParentFolderId",
    "folder:TotalCount",
    "folder:UnreadCount",
    "folder:ChildFolderCount",
    "folder:FolderClass",
];

/// The message fields the index needs.
const SUMMARY_FIELDS: &[&str] = &[
    "item:ParentFolderId",
    "item:Subject",
    "message:From",
    "message:ToRecipients",
    "item:DateTimeSent",
    "item:DateTimeCreated",
    "item:DateTimeReceived",
    "item:IsDraft",
    "message:IsRead",
    "item:Flag",
    "item:HasAttachments",
    "item:LastModifiedTime",
    "item:ConversationId",
    "message:ConversationIndex",
];

/// The fields only a full message needs, besides its body.
const MESSAGE_FIELDS: &[&str] = &[
    "message:CcRecipients",
    "message:BccRecipients",
    "message:ReplyTo",
    "message:InternetMessageId",
    "item:Body",
];

/// The most items looked up in one `GetItem`: Exchange's limit (its
/// `EWSMaxBatchSize`, by default) is often 100.
const ITEMS_PER_REQUEST: usize = 100;

/// The most changes one `SyncFolderItems` returns (EWS takes up to 512).
const SYNC_BATCH: u32 = 512;

/// The distinguished folder at the root of the mail folders.
const ROOT: &str = "msgfolderroot";

/// The streaming subscriptions mett made.
#[derive(Debug, Default)]
struct Subscriptions {
    /// The one watching folders (its ID), and which.
    current: Option<(String, Vec<FolderId>)>,
    /// Those still to end: ending them failed, and is tried again before
    /// another is made (up to [`STALE_KEPT`]).
    stale: Vec<String>,
}

/// How many subscriptions that couldn't be ended are kept to try again:
/// older ones are left to expire.
const STALE_KEPT: usize = 8;

/// A [`MailStore`] backed by Exchange Web Services.
///
/// Messages are dated and sorted by when they were sent; unsent drafts use
/// their creation date, as with Microsoft Graph. As with Graph, a listing
/// sorts by one date, then the other, so in a folder that mixes unsent
/// drafts with other mail, the drafts are grouped together rather than
/// interleaved by date. Drafts normally live only in the Drafts folder,
/// where they all sort by creation date. Listings are paged by offset.
/// Folders below one are listed in name order.
pub struct EwsMailStore<A> {
    conn: Connection<A>,
    /// The mailbox's folder roles, once looked up, and when.
    roles: Mutex<Option<(Arc<Roles>, Instant)>>,
    /// Server drafts kept after a send or update whose outcome is unknown,
    /// by what they answer and the composed message they hold
    /// ([`Draft::attempt`]): sending or postponing that message again
    /// updates the draft rather than making another.
    kept: Mutex<HashMap<DraftKey, Kept>>,
    /// The files mett attached to recalled drafts, by the same key, until
    /// they are known to be gone or the message is done with: a retry
    /// takes them off (to put on again what is still to go), and leaves
    /// alone what was added elsewhere.
    uploads: Mutex<HashMap<DraftKey, Vec<Upload>>>,
    /// One watch at a time.
    watching: tokio::sync::Mutex<()>,
    /// The streaming subscriptions. Shared with the task that changes
    /// them, which a canceled watch doesn't stop.
    subscriptions: Arc<tokio::sync::Mutex<Subscriptions>>,
    /// What a watch read, until it tells: a watch canceled first leaves
    /// it to the next (EWS doesn't tell again).
    unreported: Mutex<Unreported>,
}

/// What a watch read and hasn't told yet.
#[derive(Debug, Default)]
struct Unreported {
    /// The folders it read had changed.
    changed: Vec<FolderId>,
    /// The folders it watched, while it has read part of an envelope: any
    /// of them may have changed.
    partly: Vec<FolderId>,
}

impl Unreported {
    /// The folders that changed, or may have.
    fn folders(self) -> Vec<FolderId> {
        let mut folders = self.changed;
        for folder in self.partly {
            if !folders.contains(&folder) {
                folders.push(folder);
            }
        }
        folders
    }
}

/// How long a stream of notifications stays open, in minutes (EWS takes 1
/// to 30): then [`MailStore::watch`] returns, to be called again.
const WATCH_MINUTES: u64 = 5;

/// Whether `buffer` holds part of an envelope (more than white space).
fn unread(buffer: &[u8]) -> bool {
    buffer.iter().any(|b| !b.is_ascii_whitespace())
}

/// How long after a change the stream is read for more, before what
/// changed is told.
const BURST: Duration = Duration::from_secs(1);

/// A stream closed this soon with nothing to tell was cut short: watched
/// again at once, it could be cut again and again.
const CUT_SHORT: Duration = Duration::from_secs(60);

/// A composed message, as backends tell its attempts apart: what it answers
/// or recalls, and its [`Draft::attempt`].
type DraftKey = (Option<DraftOrigin>, u64);

/// `draft`'s [`DraftKey`].
fn key(draft: &Draft) -> DraftKey {
    (draft.origin.clone(), draft.attempt)
}

/// A file mett attached (or may have: the answer can be lost).
#[derive(Clone, Debug)]
struct Upload {
    /// Its attachment's ID, once EWS said.
    id: Option<String>,
    /// Without an ID, it is known by its name and type.
    name: String,
    content_type: String,
    /// What it held, to tell a file put in its place since.
    digest: [u8; 32],
    /// The attachments the draft had before it was made: one whose answer
    /// was lost may be any other of its name and type, however many
    /// attempts later.
    prior: Vec<String>,
}

/// A file's contents, to tell two apart.
fn digest(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).into()
}

impl Upload {
    /// Whether `attachment` is this upload. One whose answer was lost is
    /// matched by name and type, among attachments the draft didn't have
    /// before it.
    fn is(&self, attachment: &Attachment) -> bool {
        match &self.id {
            Some(uploaded) => *uploaded == attachment.id.as_str(),
            None => {
                !self.prior.contains(&attachment.id.to_string())
                    && self.name == attachment.name
                    && self
                        .content_type
                        .eq_ignore_ascii_case(&attachment.content_type)
            }
        }
    }
}

/// A server draft kept after an attempt whose outcome is unknown.
#[derive(Clone, Debug)]
struct Kept {
    id: MessageId,
    /// The attachments the draft had before mett attached any (what a
    /// forward carries along), once known: the others come off before
    /// attaching again, even those whose answer was lost.
    base: Option<Vec<String>>,
}

/// Which folders play special roles. Folder IDs don't change, so this is
/// looked up once.
#[derive(Debug, Default)]
struct Roles {
    /// The root that top-level folders hang from.
    root: Option<FolderId>,
    /// Well-known folders by ID.
    well_known: HashMap<FolderId, WellKnownFolder>,
}

impl Roles {
    fn folder(&self, folder: EwsFolder) -> Folder {
        let id = FolderId::new(folder.id);
        Folder {
            well_known: self.well_known.get(&id).copied(),
            // Top-level folders hang from the root; the port says they
            // have no parent.
            parent: folder
                .parent
                .map(FolderId::new)
                .filter(|p| Some(p) != self.root.as_ref()),
            id,
            name: folder.name,
            unread_count: folder.unread,
            total_count: folder.total,
        }
    }
}

/// EWS's distinguished folder name for a well-known folder. EWS has none
/// for Outlook's Archive folder.
fn distinguished(which: WellKnownFolder) -> Option<&'static str> {
    match which {
        WellKnownFolder::Archive => None,
        which => Some(which.as_str()),
    }
}

/// The well-known folders EWS can name.
fn nameable() -> impl Iterator<Item = (WellKnownFolder, &'static str)> {
    WellKnownFolder::ALL
        .into_iter()
        .filter_map(|which| distinguished(which).map(|name| (which, name)))
}

impl<A: Authenticator> EwsMailStore<A> {
    /// A store for `account`'s mailbox at the EWS endpoint `url`, using
    /// `auth` for tokens.
    pub fn new(http: reqwest::Client, url: Url, account: AccountId, auth: Arc<A>) -> Self {
        Self {
            conn: Connection {
                http,
                url,
                account,
                auth,
            },
            roles: Mutex::new(None),
            kept: Mutex::new(HashMap::new()),
            uploads: Mutex::new(HashMap::new()),
            watching: tokio::sync::Mutex::new(()),
            subscriptions: Arc::new(tokio::sync::Mutex::new(Subscriptions::default())),
            unreported: Mutex::new(Unreported::default()),
        }
    }

    /// The uploads of each attempt.
    fn uploads(&self) -> MutexGuard<'_, HashMap<DraftKey, Vec<Upload>>> {
        self.uploads.lock().expect("lock")
    }

    /// The server drafts kept.
    fn kept(&self) -> MutexGuard<'_, HashMap<DraftKey, Kept>> {
        self.kept.lock().expect("lock")
    }

    /// Calls the EWS operation in `body` for `about` (the object it
    /// concerns, named in a "not found" error) and returns the response.
    async fn call(&self, body: &str, about: About<'_>) -> Result<String> {
        self.call_with(body, about, |e| e, Duration::ZERO).await
    }

    /// Like [`Self::call`], for an operation whose response may be large:
    /// it is given [`DOWNLOAD_TIMEOUT`].
    async fn download(&self, body: &str, about: About<'_>) -> Result<String> {
        self.call_with(body, about, |e| e, DOWNLOAD_TIMEOUT).await
    }

    /// Calls an operation that hands a message over to be sent. Failing
    /// before the request is made (say, getting a token) leaves the message
    /// unsent, which [`Error::NotSent`] says; only a lost response to the
    /// request itself leaves it in doubt.
    async fn hand_over(&self, body: &str, about: About<'_>) -> Result<String> {
        self.call_with(body, about, unsent, Duration::ZERO).await
    }

    /// Like [`Self::call`], passing errors from before the request is made
    /// through `before`. The request is given time to go up, and `download`
    /// (at least [`REQUEST_TIMEOUT`]) for the rest.
    async fn call_with(
        &self,
        body: &str,
        about: About<'_>,
        before: fn(Error) -> Error,
        download: Duration,
    ) -> Result<String> {
        self.conn.call_with(body, about, before, download).await
    }
}

/// What requests to the mailbox are made with; cheap to clone, for a task
/// of its own.
struct Connection<A> {
    http: reqwest::Client,
    /// The EWS endpoint, for example
    /// `https://outlook.office365.com/EWS/Exchange.asmx`.
    url: Url,
    /// The mailbox's owner, which Exchange Online routes requests by.
    account: AccountId,
    auth: Arc<A>,
}

impl<A> Clone for Connection<A> {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            url: self.url.clone(),
            account: self.account.clone(),
            auth: Arc::clone(&self.auth),
        }
    }
}

impl<A: Authenticator> Connection<A> {
    /// See [`EwsMailStore::call_with`].
    async fn call_with(
        &self,
        body: &str,
        about: About<'_>,
        before: fn(Error) -> Error,
        download: Duration,
    ) -> Result<String> {
        let xml = Bytes::from(envelope(body));
        let timeout = timeout_for(xml.len(), download);
        let response = self.send_soap(&xml, before, timeout).await?;
        checked(response, about).await
    }

    /// Sends the envelope `xml` (given `timeout`), with a token, and
    /// returns the response, whatever its status. Errors from before the
    /// request is made go through `before`.
    async fn send_soap(
        &self,
        xml: &Bytes,
        before: fn(Error) -> Error,
        timeout: Duration,
    ) -> Result<reqwest::Response> {
        let token = self.auth.access_token().await.map_err(before)?;
        let response = self.attempt(xml, &token, timeout).await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        // EWS rejected a token that hadn't expired (it may have been
        // revoked): get a new one and try once more.
        self.auth.token_rejected(&token).await;
        let fresh = self.auth.access_token().await.map_err(before)?;
        if fresh == token {
            return Ok(response);
        }
        let response = self.attempt(xml, &fresh, timeout).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            // Rejected too: don't let the next request reuse it.
            self.auth.token_rejected(&fresh).await;
        }
        Ok(response)
    }

    async fn attempt(
        &self,
        xml: &Bytes,
        token: &AccessToken,
        timeout: Duration,
    ) -> Result<reqwest::Response> {
        self.http
            .post(self.url.clone())
            .bearer_auth(token.secret())
            .header(reqwest::header::CONTENT_TYPE, "text/xml; charset=utf-8")
            // Exchange Online routes the request to the mailbox's server.
            .header("X-AnchorMailbox", self.account.as_str())
            .timeout(timeout)
            .body(xml.clone())
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))
    }
}

/// The body of a successful response, or the error an unsuccessful one
/// reports.
async fn checked(response: reqwest::Response, about: About<'_>) -> Result<String> {
    let status = response.status();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let body = response
        .text()
        .await
        .map_err(|e| Error::Network(e.to_string()))?;
    if status.is_success() {
        return Ok(body);
    }
    // Exchange reports most failures as a SOAP fault (HTTP 500): one
    // that says what failed is that error (a refused send wasn't sent);
    // without a code, the status tells.
    if let Ok(doc) = parse(&body)
        && let Some(e) = coded_fault(&doc, about)
    {
        return Err(e);
    }
    let detail = format!("EWS returned HTTP {status}");
    Err(match status {
        StatusCode::TOO_MANY_REQUESTS => Error::Throttled { retry_after },
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Error::Auth(detail),
        StatusCode::REQUEST_TIMEOUT => Error::Network(detail),
        s if s.is_server_error() => Error::Network(detail),
        _ => Error::Backend(detail),
    })
}

impl<A: Authenticator + 'static> EwsMailStore<A> {
    /// The mailbox's folder roles, looked up with one `GetFolder` for the
    /// root and every well-known folder EWS can name. Looked up afresh on
    /// every call, so a well-known folder created since is recognised: a
    /// listing of every folder does so, and other calls use what it found
    /// ([`Self::cached_roles`]). The result is cached.
    async fn roles(&self) -> Result<Arc<Roles>> {
        let names: Vec<&str> = std::iter::once(ROOT)
            .chain(nameable().map(|(_, name)| name))
            .collect();
        let ids: String = names
            .iter()
            .map(|name| format!("<t:DistinguishedFolderId Id=\"{name}\"/>"))
            .collect();
        let body = format!(
            "<m:GetFolder><m:FolderShape><t:BaseShape>IdOnly</t:BaseShape></m:FolderShape>\
             <m:FolderIds>{ids}</m:FolderIds></m:GetFolder>"
        );
        let xml = self.call(&body, About::MAILBOX).await?;
        let doc = parse(&xml)?;
        let mut roles = Roles::default();
        let roots = std::iter::once(None).chain(nameable().map(|(which, _)| Some(which)));
        for (message, which) in response_messages(&doc, About::MAILBOX)?
            .into_iter()
            .zip(roots)
        {
            // A mailbox may lack some well-known folders: skip them. Any
            // other failure is reported rather than cached, so a passing
            // problem can't leave the roles wrong for good.
            if response_code(message) == "ErrorFolderNotFound" {
                continue;
            }
            check(
                message,
                About::folder(which.map_or(ROOT, WellKnownFolder::as_str)),
            )?;
            let Some(id) = descendant(message, "FolderId").and_then(|n| n.attribute("Id")) else {
                continue;
            };
            let id = FolderId::new(id);
            match which {
                None => roles.root = Some(id),
                Some(which) => {
                    roles.well_known.insert(id, which);
                }
            }
        }
        let roles = Arc::new(roles);
        *self.roles.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((Arc::clone(&roles), Instant::now()));
        Ok(roles)
    }

    /// The roles, as last looked up if that was less than [`ROLES_TTL`]
    /// ago, otherwise looked up now.
    async fn cached_roles(&self) -> Result<Arc<Roles>> {
        let cached = self
            .roles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|(_, at)| at.elapsed() < ROLES_TTL)
            .map(|(roles, _)| Arc::clone(roles));
        match cached {
            Some(roles) => Ok(roles),
            None => self.roles().await,
        }
    }

    /// The folders below `parent` (without one, below the root): those
    /// directly below it, or with `deep`, all of them, every page of them.
    async fn find_folders(&self, parent: Option<&FolderId>, deep: bool) -> Result<Vec<EwsFolder>> {
        let (parent_xml, about) = match parent {
            Some(id) => (folder_id(id), About::folder(id.as_str())),
            None => (
                format!("<t:DistinguishedFolderId Id=\"{ROOT}\"/>"),
                About::folder(ROOT),
            ),
        };
        let traversal = if deep { "Deep" } else { "Shallow" };
        let shape = folder_shape();
        let mut folders = Vec::new();
        let mut offset = 0;
        loop {
            let body = format!(
                "<m:FindFolder Traversal=\"{traversal}\">{shape}\
                 <m:IndexedPageFolderView MaxEntriesReturned=\"{MAX_PAGE_SIZE}\" \
                 Offset=\"{offset}\" BasePoint=\"Beginning\"/>\
                 <m:ParentFolderIds>{parent_xml}</m:ParentFolderIds></m:FindFolder>"
            );
            let xml = self.call(&body, about).await?;
            let doc = parse(&xml)?;
            let message = only_message(&doc, about)?;
            let root = descendant(message, "RootFolder")
                .ok_or_else(|| unexpected("FindFolder", "no RootFolder"))?;
            let page: Vec<EwsFolder> = child(root, "Folders")
                .into_iter()
                .flat_map(|list| list.children().filter(Node::is_element))
                .filter_map(EwsFolder::parse)
                .collect();
            let found = page.len();
            folders.extend(page);
            match next_offset(root, offset, found) {
                Some(next) => offset = next,
                None => return Ok(folders),
            }
        }
    }

    /// One folder, by its ID or distinguished name (`folder_xml`).
    async fn get_folder(&self, folder_xml: &str, about: About<'_>) -> Result<EwsFolder> {
        let body = format!(
            "<m:GetFolder>{}<m:FolderIds>{folder_xml}</m:FolderIds></m:GetFolder>",
            folder_shape()
        );
        let xml = self.call(&body, about).await?;
        let doc = parse(&xml)?;
        let message = only_message(&doc, about)?;
        child(message, "Folders")
            .and_then(|list| list.children().find(Node::is_element))
            .and_then(EwsFolder::parse)
            .ok_or_else(|| unexpected("GetFolder", "no folder"))
    }

    /// A page of the IDs of the items in `folder` that match `restriction`,
    /// sorted by `order`, with the `fields` asked for besides, and the
    /// offset of the next page, if there is one.
    async fn find_items(
        &self,
        folder: &FolderId,
        offset: u32,
        size: u32,
        restriction: Option<&str>,
        order: &[&str],
        fields: &[&str],
    ) -> Result<(Vec<FoundItem>, Option<u32>)> {
        let about = About::folder(folder.as_str());
        let restriction = restriction
            .map(|r| format!("<m:Restriction>{r}</m:Restriction>"))
            .unwrap_or_default();
        let order: String = order
            .iter()
            .map(|uri| {
                format!(
                    "<t:FieldOrder Order=\"Descending\">{}</t:FieldOrder>",
                    field(uri)
                )
            })
            .collect();
        let body = format!(
            "<m:FindItem Traversal=\"Shallow\">{}\
             <m:IndexedPageItemView MaxEntriesReturned=\"{}\" Offset=\"{offset}\" \
             BasePoint=\"Beginning\"/>{restriction}<m:SortOrder>{order}</m:SortOrder>\
             <m:ParentFolderIds>{}</m:ParentFolderIds></m:FindItem>",
            item_shape(fields, None),
            size.clamp(1, MAX_PAGE_SIZE),
            folder_id(folder),
        );
        let xml = self.call(&body, about).await?;
        let doc = parse(&xml)?;
        let message = only_message(&doc, about)?;
        let root = descendant(message, "RootFolder")
            .ok_or_else(|| unexpected("FindItem", "no RootFolder"))?;
        let items: Vec<FoundItem> = child(root, "Items")
            .into_iter()
            .flat_map(|list| list.children().filter(Node::is_element))
            .filter_map(|item| {
                Some(FoundItem {
                    id: item_id(item)?,
                    modified: child_text(item, "LastModifiedTime"),
                })
            })
            .collect();
        let next = next_offset(root, offset, items.len());
        Ok((items, next))
    }

    /// The items `ids`, with `fields` (and, with `body`, their body as
    /// text), in that order. Those gone meanwhile are left out.
    async fn get_items(
        &self,
        ids: &[String],
        fields: &[&str],
        body: bool,
        about: About<'_>,
    ) -> Result<String> {
        let ids: String = ids.iter().map(|id| item_id_xml(id)).collect();
        let request = format!(
            "<m:GetItem>{}<m:ItemIds>{ids}</m:ItemIds></m:GetItem>",
            item_shape(fields, body.then_some("Text"))
        );
        self.call(&request, about).await
    }

    /// The summaries of the messages `ids`, in that order. Those gone
    /// meanwhile are left out.
    async fn summaries(&self, ids: &[String], about: About<'_>) -> Result<Vec<MessageSummary>> {
        let mut summaries = Vec::new();
        // Exchange takes only so many items per request (often 100).
        for ids in ids.chunks(ITEMS_PER_REQUEST) {
            let xml = self.get_items(ids, SUMMARY_FIELDS, false, about).await?;
            let doc = parse(&xml)?;
            for message in response_messages(&doc, about)? {
                if response_code(message) == "ErrorItemNotFound" {
                    continue;
                }
                check(message, about)?;
                if let Some(item) = first_item(message) {
                    summaries.push(summary(item)?);
                }
            }
        }
        Ok(summaries)
    }

    /// A page of a folder's message summaries, matching `restriction` and
    /// sorted by `order`. `query` tells this listing's cursors apart from
    /// another's.
    async fn messages_page(
        &self,
        folder: &FolderId,
        page: Page,
        query: &str,
        restriction: Option<&str>,
        order: &[&str],
    ) -> Result<MessagePage> {
        let offset = match page.cursor() {
            None => 0,
            Some(cursor) => parse_cursor(cursor, folder, query)?,
        };
        let (found, next) = self
            .find_items(folder, offset, page.size(), restriction, order, &[])
            .await?;
        let ids: Vec<String> = found.into_iter().map(|f| f.id).collect();
        Ok(MessagePage {
            messages: self.summaries(&ids, About::folder(folder.as_str())).await?,
            next: next.map(|offset| cursor(offset, query, folder)),
        })
    }

    /// Moves or copies (`operation`) message `id` to folder `to`, returning
    /// the ID it has there.
    async fn relocate(&self, id: &MessageId, to: &FolderId, operation: &str) -> Result<MessageId> {
        let about = About::message(id.as_str());
        let body = format!(
            "<m:{operation} ReturnNewItemIds=\"true\"><m:ToFolderId>{}</m:ToFolderId>\
             <m:ItemIds>{}</m:ItemIds></m:{operation}>",
            folder_id(to),
            item_id_xml(id.as_str())
        );
        let xml = self.call(&body, about).await?;
        let doc = parse(&xml)?;
        let message = only_message(&doc, about)?;
        first_item(message)
            .and_then(item_id)
            .map(MessageId::new)
            .ok_or_else(|| unexpected(operation, "no new item ID"))
    }

    /// The calendar item a meeting message is about. It can be made again
    /// with another ID when a meeting is answered, so the message says
    /// which it is now.
    async fn associated_item(
        &self,
        id: &MessageId,
        invitation: &Invitation,
        about: About<'_>,
    ) -> Result<String> {
        let xml = self
            .get_items(
                &[id.to_string()],
                &["meeting:AssociatedCalendarItemId"],
                false,
                about,
            )
            .await?;
        let doc = parse(&xml)?;
        let associated = first_item(only_message(&doc, about)?)
            .and_then(|item| child(item, "AssociatedCalendarItemId"))
            .and_then(|item| item.attribute("Id"))
            .map(str::to_owned);
        associated
            .or_else(|| invitation.event.clone())
            .ok_or_else(|| Error::Backend("the meeting isn't in your calendar yet".into()))
    }

    /// The calendar item of the series that the occurrence a message is
    /// about belongs to.
    async fn series_item(
        &self,
        id: &MessageId,
        invitation: &Invitation,
        about: About<'_>,
    ) -> Result<String> {
        let occurrence = self.associated_item(id, invitation, about).await?;
        let body = format!(
            "<m:GetItem>{}<m:ItemIds><t:RecurringMasterItemId OccurrenceId=\"{}\"/>\
             </m:ItemIds></m:GetItem>",
            item_shape(&[], None),
            escape(&occurrence)
        );
        let xml = self.call(&body, about).await?;
        let doc = parse(&xml)?;
        first_item(only_message(&doc, about)?)
            .and_then(item_id)
            .ok_or_else(|| Error::Backend("the meeting's series isn't in your calendar".into()))
    }

    /// The calendar item of the occurrence an invitation to a series is
    /// about: the one of the series that starts when the invitation says.
    async fn occurrence_item(
        &self,
        id: &MessageId,
        invitation: &Invitation,
        about: About<'_>,
    ) -> Result<String> {
        let Some(start) = invitation.start else {
            return Err(Error::Backend("the meeting has no start".into()));
        };
        let event = self.associated_item(id, invitation, about).await?;
        // The series' occurrences share its UID, and are in the calendar it
        // is in (which may not be the default one).
        let fields = ["calendar:UID", "item:ParentFolderId"];
        let xml = self.get_items(&[event], &fields, false, about).await?;
        let doc = parse(&xml)?;
        let item = first_item(only_message(&doc, about)?)
            .ok_or_else(|| unexpected("GetItem", "no item"))?;
        let uid = child_text(item, "UID").ok_or_else(|| unexpected("GetItem", "no UID"))?;
        let calendar = child(item, "ParentFolderId")
            .and_then(|f| f.attribute("Id"))
            .map_or_else(
                || "<t:DistinguishedFolderId Id=\"calendar\"/>".to_owned(),
                |id| folder_id(&FolderId::new(id)),
            );
        let body = format!(
            "<m:FindItem Traversal=\"Shallow\">{}<m:CalendarView {}/>\
             <m:ParentFolderIds>{calendar}</m:ParentFolderIds></m:FindItem>",
            item_shape(
                &["calendar:UID", "calendar:Start", "calendar:StartWallClock"],
                None
            ),
            occurrence_view(start, invitation.end),
        );
        let xml = self.call(&body, about).await?;
        let doc = parse(&xml)?;
        let message = only_message(&doc, about)?;
        let items = descendant(message, "RootFolder")
            .and_then(|root| child(root, "Items"))
            .ok_or_else(|| unexpected("FindItem", "no Items"))?;
        find_occurrence(items, &uid, start, invitation.all_day)
            .ok_or_else(|| Error::Backend("that occurrence isn't in your calendar".into()))
    }

    /// Deletes item `id` for good.
    async fn delete_item(&self, id: &MessageId) -> Result<()> {
        let about = About::message(id.as_str());
        let body = format!(
            "<m:DeleteItem DeleteType=\"HardDelete\"><m:ItemIds>{}</m:ItemIds></m:DeleteItem>",
            item_id_xml(id.as_str())
        );
        let xml = self.call(&body, about).await?;
        only_message(&parse(&xml)?, about).map(drop)
    }

    /// The people `ResolveNames` finds for `text` in `scope`, with their
    /// contact data if `full`.
    async fn resolve_names(&self, text: &str, scope: &str, full: bool) -> Result<Vec<Resolved>> {
        let body = format!(
            "<m:ResolveNames ReturnFullContactData=\"{full}\" SearchScope=\"{scope}\">\
             <m:UnresolvedEntry>{}</m:UnresolvedEntry></m:ResolveNames>",
            escape(text)
        );
        let xml = self.call(&body, About::MAILBOX).await?;
        let doc = parse(&xml)?;
        let message = only_message_or_none(&doc)?;
        let Some(message) = message else {
            return Ok(Vec::new());
        };
        Ok(descendant(message, "ResolutionSet")
            .into_iter()
            .flat_map(|set| children(set, "Resolution"))
            .map(Resolved::parse)
            .collect())
    }
}

/// The response message of a `ResolveNames`, or `None` if it found no one.
fn only_message_or_none<'a, 'i>(doc: &'a roxmltree::Document<'i>) -> Result<Option<Node<'a, 'i>>> {
    let message = response_messages(doc, About::MAILBOX)?
        .into_iter()
        .next()
        .ok_or_else(|| unexpected("ResolveNames", "no response"))?;
    if response_code(message) == "ErrorNameResolutionNoResults" {
        return Ok(None);
    }
    check(message, About::MAILBOX)?;
    Ok(Some(message))
}

/// `first` and `second` at once, as if `first` came first: its error is
/// the answer as soon as it is known, without waiting for `second`.
async fn first_then<T, U>(
    first: impl Future<Output = Result<T>>,
    second: impl Future<Output = U>,
) -> Result<(T, U)> {
    tokio::pin!(first, second);
    tokio::select! {
        biased;
        first = &mut first => Ok((first?, second.await)),
        second = &mut second => Ok((first.await?, second)),
    }
}

/// `folders` in tree order: each folder below `parent`, by name, followed
/// by the folders below it. Folders whose parent isn't among them (nor
/// `parent`) are left out.
fn tree_order(folders: Vec<EwsFolder>, parent: Option<&str>) -> Vec<EwsFolder> {
    let mut below: HashMap<Option<String>, Vec<EwsFolder>> = HashMap::new();
    for folder in folders {
        let parent = folder.parent.clone().filter(|p| Some(p.as_str()) != parent);
        below.entry(parent).or_default().push(folder);
    }
    for siblings in below.values_mut() {
        sort_by_name(siblings);
    }
    let mut ordered = Vec::new();
    let mut pending: Vec<EwsFolder> = below.remove(&None).unwrap_or_default();
    pending.reverse();
    while let Some(folder) = pending.pop() {
        if let Some(mut children) = below.remove(&Some(folder.id.clone())) {
            children.reverse();
            pending.extend(children);
        }
        ordered.push(folder);
    }
    ordered
}

/// Sorts folders by name, ignoring case (then by ID, so the order is
/// always the same).
fn sort_by_name(folders: &mut [EwsFolder]) {
    folders.sort_by_cached_key(|f| (f.name.to_lowercase(), f.id.clone()));
}

/// The mail folders among `folders`, and none below a folder of another
/// kind (such as a calendar).
fn mail_folders(folders: Vec<EwsFolder>) -> Vec<EwsFolder> {
    let other: HashSet<String> = folders
        .iter()
        .filter(|f| !f.is_mail())
        .map(|f| f.id.clone())
        .collect();
    let parents: HashMap<String, Option<String>> = folders
        .iter()
        .map(|f| (f.id.clone(), f.parent.clone()))
        .collect();
    let below_other = |folder: &EwsFolder| {
        let mut parent = folder.parent.clone();
        let mut steps = 0;
        while let Some(id) = parent {
            if other.contains(&id) {
                return true;
            }
            steps += 1;
            if steps > parents.len() {
                return true;
            }
            parent = parents.get(&id).cloned().flatten();
        }
        false
    };
    let keep: Vec<bool> = folders
        .iter()
        .map(|f| f.is_mail() && !below_other(f))
        .collect();
    folders
        .into_iter()
        .zip(keep)
        .filter_map(|(f, keep)| keep.then_some(f))
        .collect()
}

impl<A: Authenticator + 'static> MailStore for EwsMailStore<A> {
    async fn own_addresses(&self) -> Result<Vec<String>> {
        let resolved = self
            .resolve_names(self.conn.account.as_str(), "ActiveDirectory", true)
            .await?;
        let mut addresses = vec![self.conn.account.to_string()];
        let mut add = |address: &str| {
            let address = address.trim();
            if address.contains('@') && !addresses.iter().any(|a| a.eq_ignore_ascii_case(address)) {
                addresses.push(address.to_owned());
            }
        };
        // The account's own entry: the one with its name among its
        // addresses, or the only one found (the account's name, its UPN,
        // need not be an address of its own). A name can resolve to others
        // too.
        let own = |r: &&Resolved| {
            r.addresses
                .iter()
                .any(|a| a.eq_ignore_ascii_case(self.conn.account.as_str()))
        };
        let mine: Vec<&Resolved> = match resolved.iter().filter(own).collect::<Vec<_>>() {
            mine if mine.is_empty() && resolved.len() == 1 => resolved.iter().collect(),
            mine => mine,
        };
        for person in mine {
            for address in &person.addresses {
                add(address);
            }
        }
        Ok(addresses)
    }

    async fn search_people(&self, text: &str) -> Result<Vec<Address>> {
        let resolved = self
            .resolve_names(text, "ActiveDirectoryContacts", false)
            .await?;
        let mut found: Vec<Address> = Vec::new();
        for person in resolved {
            let Some(email) = person.email.filter(|e| e.contains('@')) else {
                continue;
            };
            if found.iter().any(|f| f.email.eq_ignore_ascii_case(&email)) {
                continue;
            }
            let address = Address::new(email);
            found.push(match person.name.filter(|n| !n.is_empty()) {
                Some(name) => address.with_name(name),
                None => address,
            });
        }
        Ok(found)
    }

    async fn list_folders(&self) -> Result<Vec<Folder>> {
        // One deep listing gives the whole tree. The roles are looked up
        // meanwhile; failing, they are what is reported, as if looked up
        // first.
        let (roles, folders) = first_then(self.roles(), self.find_folders(None, true)).await?;
        let root = roles.root.as_ref().map(FolderId::as_str);
        Ok(tree_order(mail_folders(folders?), root)
            .into_iter()
            .map(|f| roles.folder(f))
            .collect())
    }

    async fn child_folders(&self, parent: Option<&FolderId>) -> Result<Vec<Folder>> {
        // The roles label the folders as in a listing of them all: those
        // the last such listing found, if recent, otherwise looked up.
        let (roles, listing) =
            first_then(self.cached_roles(), self.find_folders(parent, false)).await?;
        let mut folders = mail_folders(listing?);
        sort_by_name(&mut folders);
        Ok(folders.into_iter().map(|f| roles.folder(f)).collect())
    }

    async fn folder(&self, id: &FolderId) -> Result<Folder> {
        let (roles, folder) = first_then(
            self.cached_roles(),
            self.get_folder(&folder_id(id), About::folder(id.as_str())),
        )
        .await?;
        Ok(roles.folder(folder?))
    }

    async fn recent_unread(
        &self,
        folder: &FolderId,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<(MessageId, DateTime<Utc>)>> {
        // A message moved in keeps when it was received; when it was last
        // modified tells. Mail that changed when `since` did may not have
        // been seen: which was is told by its ID.
        let after = since.unwrap_or(DateTime::UNIX_EPOCH);
        let restriction = format!(
            "<t:And>{}<t:IsEqualTo>{}<t:FieldURIOrConstant><t:Constant Value=\"false\"/>\
             </t:FieldURIOrConstant></t:IsEqualTo></t:And>",
            modified_since(after),
            field("message:IsRead"),
        );
        // Without `since`, the last few tell; with it, all of them, up to a
        // limit.
        let size = match since {
            Some(_) => RECENT_UNREAD,
            None => 50,
        };
        let (found, _) = self
            .find_items(
                folder,
                0,
                u32::try_from(size).unwrap_or(MAX_PAGE_SIZE),
                Some(&restriction),
                &["item:LastModifiedTime"],
                &["item:LastModifiedTime"],
            )
            .await?;
        let mut recent = Vec::new();
        for item in found {
            let at = item
                .modified
                .as_deref()
                .and_then(parse_time)
                .ok_or_else(|| unexpected("FindItem", "an item without LastModifiedTime"))?;
            recent.push((MessageId::new(item.id), at));
        }
        recent.truncate(RECENT_UNREAD);
        Ok(recent)
    }

    async fn well_known_folder(&self, which: WellKnownFolder) -> Result<Folder> {
        let name = distinguished(which).ok_or_else(|| Error::not_found("folder", which))?;
        let (roles, folder) = first_then(
            self.cached_roles(),
            self.get_folder(
                &format!("<t:DistinguishedFolderId Id=\"{name}\"/>"),
                About::folder(name),
            ),
        )
        .await?;
        Ok(Folder {
            well_known: Some(which),
            ..roles.folder(folder?)
        })
    }

    async fn list_messages(&self, folder: &FolderId, page: Page) -> Result<MessagePage> {
        // Sort by the date the summaries show (see `summary`): the sent
        // date, which received and sent mail both have; only unsent drafts
        // fall back to their creation date.
        self.messages_page(
            folder,
            page,
            "all",
            None,
            &["item:DateTimeSent", "item:DateTimeCreated"],
        )
        .await
    }

    async fn changed_messages(
        &self,
        folder: &FolderId,
        since: DateTime<Utc>,
        page: Page,
    ) -> Result<MessagePage> {
        let query = since.to_rfc3339_opts(SecondsFormat::Secs, true);
        self.messages_page(
            folder,
            page,
            &query,
            Some(&modified_since(since)),
            &["item:LastModifiedTime"],
        )
        .await
    }

    async fn fetch_message(&self, id: &MessageId) -> Result<Message> {
        let about = About::message(id.as_str());
        let fields: Vec<&str> = SUMMARY_FIELDS
            .iter()
            .chain(MESSAGE_FIELDS)
            .chain(INVITATION_FIELDS)
            .copied()
            .collect();
        // EWS converts HTML bodies to text, better than mett could.
        let xml = self
            .get_items(&[id.to_string()], &fields, true, about)
            .await?;
        let doc = parse(&xml)?;
        let message = only_message(&doc, about)?;
        let item = first_item(message).ok_or_else(|| unexpected("GetItem", "no item"))?;
        full_message(item)
    }

    async fn message_folder(&self, id: &MessageId) -> Result<FolderId> {
        let about = About::message(id.as_str());
        let xml = self
            .get_items(&[id.to_string()], &["item:ParentFolderId"], false, about)
            .await?;
        let doc = parse(&xml)?;
        let message = only_message(&doc, about)?;
        first_item(message)
            .and_then(|item| child(item, "ParentFolderId"))
            .and_then(|f| f.attribute("Id"))
            .map(FolderId::new)
            .ok_or_else(|| unexpected("GetItem", "no ParentFolderId"))
    }

    async fn list_attachments(&self, message: &MessageId) -> Result<Vec<Attachment>> {
        let about = About::message(message.as_str());
        let xml = self
            .get_items(&[message.to_string()], &["item:Attachments"], false, about)
            .await?;
        let doc = parse(&xml)?;
        let response = only_message(&doc, about)?;
        Ok(first_item(response)
            .and_then(|item| child(item, "Attachments"))
            .into_iter()
            .flat_map(|list| list.children().filter(Node::is_element))
            .filter_map(attachment)
            .collect())
    }

    async fn fetch_attachment(
        &self,
        message: &MessageId,
        attachment: &AttachmentId,
    ) -> Result<Vec<u8>> {
        // A file's bytes, or an attached item as MIME.
        let about = About::attachment(attachment.as_str());
        let body = format!(
            "<m:GetAttachment><m:AttachmentShape><t:IncludeMimeContent>true</t:IncludeMimeContent>\
             </m:AttachmentShape><m:AttachmentIds><t:AttachmentId Id=\"{}\"/></m:AttachmentIds>\
             </m:GetAttachment>",
            escape(attachment.as_str())
        );
        let xml = self.download(&body, about).await?;
        let doc = parse(&xml)?;
        let response = only_message(&doc, about)?;
        let found = descendant(response, "Attachments")
            .and_then(|list| list.children().find(Node::is_element))
            .ok_or_else(|| Error::not_found("attachment", attachment))?;
        // Attachment IDs are the mailbox's: make sure it is this message's.
        if let Some(root) = child(found, "AttachmentId").and_then(|a| a.attribute("RootItemId"))
            && root != message.as_str()
        {
            return Err(Error::not_found("attachment", attachment));
        }
        // Decoded from the text in the document, without copies of it.
        let content = match found.tag_name().name() {
            "FileAttachment" => child(found, "Content"),
            "ItemAttachment" => found
                .children()
                .filter(Node::is_element)
                .find_map(|item| child(item, "MimeContent")),
            _ => {
                return Err(Error::Backend(format!(
                    "{} is a link, with no content of its own",
                    child_text(found, "Name").unwrap_or_default()
                )));
            }
        };
        decode_base64(content.and_then(|n| n.text()).unwrap_or_default())
    }

    async fn update_flags(&self, id: &MessageId, update: FlagsUpdate) -> Result<()> {
        let mut updates = String::new();
        if let Some(read) = update.read {
            updates.push_str(&set_field(
                "message:IsRead",
                &format!("<t:IsRead>{read}</t:IsRead>"),
            ));
        }
        if let Some(flagged) = update.flagged {
            let status = if flagged { "Flagged" } else { "NotFlagged" };
            updates.push_str(&set_field(
                "item:Flag",
                &format!("<t:Flag><t:FlagStatus>{status}</t:FlagStatus></t:Flag>"),
            ));
        }
        if updates.is_empty() {
            return Ok(());
        }
        self.update_item(id, &updates).await
    }

    async fn move_message(&self, id: &MessageId, to: &FolderId) -> Result<MessageId> {
        self.relocate(id, to, "MoveItem").await
    }

    async fn copy_message(&self, id: &MessageId, to: &FolderId) -> Result<MessageId> {
        self.relocate(id, to, "CopyItem").await
    }

    async fn delete_message(&self, id: &MessageId) -> Result<()> {
        self.delete_item(id).await
    }

    async fn answer_invitation(
        &self,
        id: &MessageId,
        invitation: &Invitation,
        answer: &Answer,
    ) -> Result<()> {
        let about = About::message(id.as_str());
        // The message answers the meeting, or all its series. One
        // occurrence of a series is answered through its calendar item.
        let reference = if answer.scope == Scope::Occurrence && invitation.repeats() {
            self.occurrence_item(id, invitation, about).await?
        } else if answer.scope == Scope::Series && invitation.in_series && !invitation.series_master
        {
            // The message is about one occurrence: the series is another
            // item, which is answered instead.
            self.series_item(id, invitation, about).await?
        } else {
            id.to_string()
        };
        let items = answer_xml(answer.rsvp, &reference, answer.comment.as_deref());
        // Without telling the organizer, the response is only saved (as a
        // draft, which is then not wanted).
        let disposition = if answer.notify {
            "SendAndSaveCopy"
        } else {
            "SaveOnly"
        };
        let xml = self
            .call(&create_item(disposition, None, &items), about)
            .await?;
        let doc = parse(&xml)?;
        let message = only_message(&doc, about)?;
        if !answer.notify
            && let Some(saved) = first_item(message)
            // Only the response saved is unwanted: the calendar item the
            // answer updated, which Exchange may give back, is the answer.
            && matches!(saved.tag_name().name(), "Message" | "MeetingResponse")
            && let Some(saved) = item_id(saved)
        {
            // Failing to remove it leaves a draft, not a wrong answer.
            let _ = self.delete_item(&MessageId::new(saved)).await;
        }
        Ok(())
    }

    fn can_sync(&self) -> bool {
        true
    }

    async fn sync_folder(
        &self,
        folder: &FolderId,
        state: Option<&SyncState>,
    ) -> Result<FolderChanges> {
        let about = About::folder(folder.as_str());
        let state_xml = state
            .map(|s| format!("<m:SyncState>{}</m:SyncState>", escape(s.as_str())))
            .unwrap_or_default();
        let body = format!(
            "<m:SyncFolderItems>{}<m:SyncFolderId>{}</m:SyncFolderId>{state_xml}\
             <m:MaxChangesReturned>{SYNC_BATCH}</m:MaxChangesReturned>\
             <m:SyncScope>NormalItems</m:SyncScope></m:SyncFolderItems>",
            item_shape(&[], None),
            folder_id(folder)
        );
        let xml = self.call(&body, about).await?;
        let doc = parse(&xml)?;
        let message = response_messages(&doc, about)?
            .into_iter()
            .next()
            .ok_or_else(|| unexpected("SyncFolderItems", "no response"))?;
        if matches!(
            response_code(message).as_str(),
            "ErrorInvalidSyncStateData" | "ErrorSyncStateNotFound"
        ) {
            return Err(Error::StaleSync);
        }
        check(message, about)?;
        let new_state = child_text(message, "SyncState")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| unexpected("SyncFolderItems", "no SyncState"))?;
        let last = child_text(message, "IncludesLastItemInRange").as_deref() != Some("false");
        // What arrived or changed is looked up as the index needs it; what
        // went is gone.
        let mut touched: Vec<String> = Vec::new();
        let mut removed: Vec<MessageId> = Vec::new();
        for change in child(message, "Changes")
            .into_iter()
            .flat_map(|changes| changes.children().filter(Node::is_element))
        {
            let id = match change.tag_name().name() {
                "Create" | "Update" => change.children().find(Node::is_element).and_then(item_id),
                _ => child(change, "ItemId")
                    .and_then(|id| id.attribute("Id"))
                    .map(str::to_owned),
            };
            let Some(id) = id else {
                continue;
            };
            if change.tag_name().name() == "Delete" {
                touched.retain(|t| *t != id);
                removed.push(MessageId::new(id));
            } else if !touched.contains(&id) {
                removed.retain(|r| r.as_str() != id);
                touched.push(id);
            }
        }
        let summaries = self.summaries(&touched, about).await?;
        // Those gone (or moved away) since are gone.
        let here: HashSet<&str> = summaries
            .iter()
            .filter(|s| &s.folder == folder)
            .map(|s| s.id.as_str())
            .collect();
        for id in &touched {
            if !here.contains(id.as_str()) {
                removed.push(MessageId::new(id.as_str()));
            }
        }
        drop(here);
        Ok(FolderChanges {
            changed: summaries
                .into_iter()
                .filter(|s| &s.folder == folder)
                .collect(),
            removed,
            state: SyncState::new(new_state),
            more: !last,
        })
    }

    fn can_watch(&self) -> bool {
        true
    }

    /// With a streaming subscription to `folders` (made once, and again if
    /// it expires), whose events come on a response kept open for up to
    /// [`WATCH_MINUTES`].
    async fn watch(&self, folders: &[FolderId]) -> Result<Vec<FolderId>> {
        let _watching = self.watching.lock().await;
        if folders.is_empty() {
            self.unsubscribed().await?;
            return Ok(Vec::new());
        }
        let unreported = std::mem::take(&mut *self.unreported.lock().expect("lock")).folders();
        if !unreported.is_empty() {
            return Ok(unreported);
        }
        for _ in 0..2 {
            let id = self.subscribed(folders).await?;
            let started = tokio::time::Instant::now();
            let streamed = self.stream_events(&id, folders).await;
            // Told now, whatever comes of it. After an error (which the
            // caller waits a while after), the next watch tells.
            if streamed.is_ok() {
                *self.unreported.lock().expect("lock") = Unreported::default();
            }
            match streamed? {
                // Part of an envelope was left unread: any folder may have
                // changed.
                Some((_, true)) => return Ok(folders.to_vec()),
                Some((changed, false)) if changed.is_empty() && started.elapsed() < CUT_SHORT => {
                    return Err(Error::Network(
                        "EWS closed the stream of changes at once".into(),
                    ));
                }
                Some((changed, false)) => return Ok(changed),
                // Gone (expired, say): subscribe again.
                None => {
                    let mut subscriptions = self.subscriptions.lock().await;
                    if subscriptions
                        .current
                        .as_ref()
                        .is_some_and(|(gone, _)| *gone == id)
                    {
                        subscriptions.current = None;
                    }
                }
            }
        }
        // Gone again at once: something is wrong, and is tried again later.
        Err(Error::Backend(
            "EWS keeps dropping the subscription to the folders' changes".into(),
        ))
    }

    async fn save_draft(&self, draft: &Draft) -> Result<MessageId> {
        if needs_server_draft(draft) || self.has_kept(draft) {
            // Postponed: what mett attached is the draft's own now.
            let saved = self.server_draft(draft).await.map(|(id, _)| id);
            if saved.is_ok() {
                self.forget_uploads(draft);
            }
            return saved;
        }
        // A plain draft is created with its content in one request.
        let body = create_item("SaveOnly", Some("drafts"), &message_xml(draft));
        let xml = self.call(&body, About::MAILBOX).await?;
        created_id(&xml)
    }
}

impl<A: Authenticator> Connection<A> {
    /// Subscribes to `folders`' events, streamed; returns the
    /// subscription's ID.
    async fn subscribe(&self, folders: &[FolderId]) -> Result<String> {
        let ids: String = folders.iter().map(folder_id).collect();
        let events: String = [
            "NewMailEvent",
            "CreatedEvent",
            "DeletedEvent",
            "ModifiedEvent",
            "MovedEvent",
            "CopiedEvent",
        ]
        .iter()
        .map(|event| format!("<t:EventType>{event}</t:EventType>"))
        .collect();
        let body = format!(
            "<m:Subscribe><m:StreamingSubscriptionRequest><t:FolderIds>{ids}</t:FolderIds>\
             <t:EventTypes>{events}</t:EventTypes></m:StreamingSubscriptionRequest></m:Subscribe>"
        );
        let xml = self
            .call_with(&body, About::MAILBOX, |e| e, Duration::ZERO)
            .await?;
        let doc = parse(&xml)?;
        let message = only_message(&doc, About::MAILBOX)?;
        child_text(message, "SubscriptionId")
            .filter(|id| !id.is_empty())
            .ok_or_else(|| unexpected("Subscribe", "no SubscriptionId"))
    }

    /// Ends subscription `id`.
    async fn unsubscribe(&self, id: &str) -> Result<()> {
        let body = format!(
            "<m:Unsubscribe><m:SubscriptionId>{}</m:SubscriptionId></m:Unsubscribe>",
            escape(id)
        );
        let xml = self
            .call_with(&body, About::MAILBOX, |e| e, Duration::ZERO)
            .await?;
        let doc = parse(&xml)?;
        match response_messages(&doc, About::MAILBOX)?.first() {
            // Gone already (expired, say): as good as ended.
            Some(message) if crate::stream::subscription_gone(&response_code(*message)) => Ok(()),
            _ => only_message(&doc, About::MAILBOX).map(drop),
        }
    }

    /// Ends the subscriptions in `stale`, keeping those it couldn't (up
    /// to [`STALE_KEPT`], the newest) to try again later.
    async fn end_stale(&self, stale: &mut Vec<String>) -> Result<()> {
        let mut result = Ok(());
        let mut kept = Vec::new();
        for id in std::mem::take(stale) {
            if let Err(e) = self.unsubscribe(&id).await {
                kept.push(id);
                result = Err(e);
            }
        }
        kept.drain(..kept.len().saturating_sub(STALE_KEPT));
        *stale = kept;
        result
    }
}

impl<A: Authenticator + 'static> EwsMailStore<A> {
    /// The ID of the subscription to `folders`' events: the one kept if it
    /// watches them, else a new one, which is kept instead. That is done
    /// in a task of its own: were the watch canceled half way, a
    /// subscription made would be lost, and another made each time, up to
    /// Exchange's limit.
    async fn subscribed(&self, folders: &[FolderId]) -> Result<String> {
        let conn = self.conn.clone();
        // Locked before the task starts, so whatever is asked of the
        // subscriptions next (say, to end them) comes after.
        let mut subscriptions = Arc::clone(&self.subscriptions).lock_owned().await;
        let folders = folders.to_vec();
        tokio::spawn(async move {
            if let Some((id, watched)) = &subscriptions.current
                && *watched == folders
            {
                return Ok(id.clone());
            }
            // Other folders now: the old subscription goes, with those
            // that couldn't be ended before (if not now, later).
            if let Some((old, _)) = subscriptions.current.take() {
                subscriptions.stale.push(old);
            }
            let _ = conn.end_stale(&mut subscriptions.stale).await;
            let id = conn.subscribe(&folders).await?;
            subscriptions.current = Some((id.clone(), folders));
            Ok(id)
        })
        .await
        .map_err(|e| Error::Backend(format!("subscribing to the folders' changes failed: {e}")))?
    }

    /// Ends the subscription kept, if any, in a task of its own like
    /// [`Self::subscribed`]'s.
    async fn unsubscribed(&self) -> Result<()> {
        let conn = self.conn.clone();
        let mut subscriptions = Arc::clone(&self.subscriptions).lock_owned().await;
        tokio::spawn(async move {
            if let Some((id, _)) = subscriptions.current.take() {
                subscriptions.stale.push(id);
            }
            conn.end_stale(&mut subscriptions.stale).await
        })
        .await
        .map_err(|e| {
            Error::Backend(format!(
                "unsubscribing from the folders' changes failed: {e}"
            ))
        })?
    }

    /// Reads the events of subscription `id` until some say folders
    /// changed (those), or the server closes the stream (none), and
    /// whether part of an envelope was left unread. `None` if the
    /// subscription is gone. It watches `folders`.
    async fn stream_events(
        &self,
        id: &str,
        folders: &[FolderId],
    ) -> Result<Option<(Vec<FolderId>, bool)>> {
        use crate::stream::{Streamed, read_envelope, take_envelopes};

        let body = format!(
            "<m:GetStreamingEvents><m:SubscriptionIds><t:SubscriptionId>{}</t:SubscriptionId>\
             </m:SubscriptionIds><m:ConnectionTimeout>{WATCH_MINUTES}</m:ConnectionTimeout>\
             </m:GetStreamingEvents>",
            escape(id)
        );
        let xml = Bytes::from(envelope(&body));
        // The response stays open as long as asked, and a little more.
        let timeout = Duration::from_secs(WATCH_MINUTES * 60 + 60);
        let mut response = self.conn.send_soap(&xml, |e| e, timeout).await?;
        if !response.status().is_success() {
            return checked(response, About::MAILBOX)
                .await
                .map(|_| Some((Vec::new(), false)));
        }
        let mut buffer = Vec::new();
        let mut scanned = 0;
        let mut changed: Vec<FolderId> = Vec::new();
        // Once something changed, what follows at once (the rest of a
        // burst) is read too, for a moment: events sent are not sent again.
        let mut until: Option<tokio::time::Instant> = None;
        // A stream that ends before telling anything ended early.
        let mut told = false;
        loop {
            let next = match until {
                None => response.chunk().await,
                Some(until) => match tokio::time::timeout_at(until, response.chunk()).await {
                    Ok(next) => next,
                    Err(_) => return Ok(Some((changed, unread(&buffer)))),
                },
            };
            let chunk = match next {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                // What changed, or part of an envelope, was sent already:
                // it is not sent again.
                Err(_) if !changed.is_empty() || unread(&buffer) => {
                    return Ok(Some((changed, true)));
                }
                Err(e) => return Err(Error::Network(e.to_string())),
            };
            buffer.extend_from_slice(&chunk);
            let envelopes = match take_envelopes(&mut buffer, &mut scanned) {
                Ok(envelopes) => envelopes,
                Err(_) if !changed.is_empty() => return Ok(Some((changed, true))),
                Err(e) => return Err(e),
            };
            for envelope in envelopes {
                told = true;
                // What changed was told already: an error after it, or
                // the subscription's end, doesn't lose it.
                let streamed = match read_envelope(&envelope) {
                    Ok(streamed) => streamed,
                    Err(_) if !changed.is_empty() => return Ok(Some((changed, true))),
                    Err(e) => return Err(e),
                };
                match streamed {
                    Streamed::Changed(folders) => {
                        let mut unreported = self.unreported.lock().expect("lock");
                        for folder in folders {
                            if !unreported.changed.contains(&folder) {
                                unreported.changed.push(folder.clone());
                            }
                            if !changed.contains(&folder) {
                                changed.push(folder);
                            }
                        }
                    }
                    Streamed::Closed => return Ok(Some((changed, false))),
                    Streamed::Unsubscribed if changed.is_empty() => return Ok(None),
                    Streamed::Unsubscribed => return Ok(Some((changed, false))),
                }
            }
            self.unreported.lock().expect("lock").partly = if unread(&buffer) {
                folders.to_vec()
            } else {
                Vec::new()
            };
            if !changed.is_empty() && until.is_none() {
                until = Some(tokio::time::Instant::now() + BURST);
            }
        }
        if !told {
            return Err(Error::Network(
                "EWS ended the stream of changes before telling anything".into(),
            ));
        }
        Ok(Some((changed, unread(&buffer))))
    }
}

impl<A: Authenticator + 'static> MailSender for EwsMailStore<A> {
    /// Deletes the server draft kept for `draft`, if any. It is kept still
    /// if that fails.
    async fn discard_kept(&self, draft: &Draft) -> Result<()> {
        let Some(Kept { id, base }) = self.kept().remove(&key(draft)) else {
            return Ok(());
        };
        // A recalled postponed message stays in Drafts (with what the
        // attempts put on it).
        if is_recalled(draft, &id) {
            self.forget_uploads(draft);
            return Ok(());
        }
        let deleted = self.delete_draft(&id).await;
        if deleted.is_err() {
            self.keep(draft, id, base);
        }
        deleted
    }

    fn keeps_draft(&self, draft: &Draft) -> bool {
        self.has_kept(draft)
    }

    async fn send(&self, draft: &Draft) -> Result<()> {
        // A server draft kept from an earlier attempt is sent, not left
        // behind.
        if !needs_server_draft(draft) && !self.has_kept(draft) {
            let body = if draft.save_copy {
                create_item("SendAndSaveCopy", Some("sentitems"), &message_xml(draft))
            } else {
                create_item("SendOnly", None, &message_xml(draft))
            };
            let xml = self.hand_over(&body, About::MAILBOX).await?;
            return only_message(&parse(&xml)?, About::MAILBOX).map(drop);
        }
        // Replies and forwards go through a server draft, so that Exchange
        // threads them under the original and forwards keep its
        // attachments, and so do messages with attachments, which Exchange
        // takes only on a saved message. Failing before the send request
        // leaves nothing sent, whatever became of the draft.
        let (id, base) = self.server_draft(draft).await.map_err(unsent)?;
        // Sending takes the draft as it is now, which its change key says.
        let change_key = match self.change_key(&id).await {
            Ok(change_key) => change_key,
            Err(e) => return Err(unsent(self.give_up(draft, id, base, e).await)),
        };
        let sent = self.send_draft(&id, &change_key, draft.save_copy).await;
        match &sent {
            // The response was lost or EWS failed: the message may have
            // gone out anyway. Keep the draft, so nothing is lost if it
            // didn't, and say so; sending again reuses it.
            Err(Error::Network(e)) => {
                self.keep(draft, id, base);
                return Err(Error::Network(format!(
                    "{e}; if it wasn't sent, it is in Drafts"
                )));
            }
            // EWS refused it: it wasn't sent. A draft that can't be
            // deleted either is kept, for the next attempt.
            Err(_) => {
                if !self.discard(draft, &id).await {
                    self.keep(draft, id, base);
                }
            }
            Ok(()) => self.forget_uploads(draft),
        }
        sent
    }
}

impl<A: Authenticator + 'static> EwsMailStore<A> {
    /// Creates a server draft with `draft`'s content and attachments, which
    /// replies to or forwards the original if it does, and returns its ID,
    /// with what to keep of it should sending it fail.
    async fn server_draft(&self, draft: &Draft) -> Result<(MessageId, Option<Vec<String>>)> {
        // A draft kept after an earlier attempt gets the new content (and
        // its attachments again); if it is gone (it was sent after all, or
        // deleted), a new one is made.
        let kept = self.kept().remove(&key(draft));
        if let Some(Kept { id, base }) = kept {
            match self.update_draft(&id, draft).await {
                Ok(()) => return self.attach_all(draft, id, base, false).await,
                Err(Error::NotFound { .. }) => {}
                Err(e) => {
                    self.keep(draft, id, base);
                    return Err(e);
                }
            }
        }
        // A recalled postponed message is the draft: it gets the new
        // content, and keeps its attachments (they are the base). One gone
        // from Drafts was sent or deleted elsewhere; sending it again from
        // a new draft could send it twice.
        if let Some(DraftOrigin::Recalled(id)) = &draft.origin {
            return match self.update_draft(id, draft).await {
                // Those the user took off come off.
                Ok(()) => self.attach_all(draft, id.clone(), None, false).await,
                Err(Error::NotFound { .. }) => Err(Error::Backend(
                    "the postponed message is no longer in Drafts (it was sent or deleted \
                     elsewhere)"
                        .into(),
                )),
                Err(e) => Err(e),
            };
        }
        // A lost response may still have made a draft, which mett can't
        // tell apart from others: say so. Failing before the request is
        // made leaves nothing.
        let (item, about) = match &draft.origin {
            Some(origin) => {
                let (original, element) = match origin {
                    DraftOrigin::Reply(id) => (id, "ReplyToItem"),
                    DraftOrigin::Forward(id) => (id, "ForwardItem"),
                    DraftOrigin::Recalled(_) => unreachable!("updated above"),
                };
                // Exchange Online answers to a version of the original:
                // without its change key, it refuses
                // (`ErrorChangeKeyRequiredForWriteOperations`).
                let change_key = self.change_key(original).await?;
                (
                    format!(
                        "<t:{element}><t:ReferenceItemId Id=\"{}\" ChangeKey=\"{}\"/>\
                         </t:{element}>",
                        escape(original.as_str()),
                        escape(&change_key)
                    ),
                    About::message(original.as_str()),
                )
            }
            None => (message_xml(draft), About::MAILBOX),
        };
        let body = create_item("SaveOnly", Some("drafts"), &item);
        let created = match self.call_with(&body, about, unsent, Duration::ZERO).await {
            Ok(xml) => created_id(&xml),
            Err(Error::Network(e)) => Err(Error::Network(format!(
                "{e}; a draft of it may be left in Drafts"
            ))),
            Err(e) => Err(e),
        }?;
        // Replace what Exchange filled in (recipients, the quoted original)
        // with what the user wrote.
        let updated = match draft.origin {
            Some(_) => self.update_draft(&created, draft).await,
            None => Ok(()),
        };
        match updated {
            Ok(()) => self.attach_all(draft, created, None, true).await,
            Err(e) => Err(self.give_up(draft, created, None, e).await),
        }
    }

    /// Gives the server draft `id` `draft`'s attachments: what it has
    /// beyond `base` (an earlier attempt's) comes off, then `draft`'s go
    /// on. Without `base`, mett has attached nothing to it yet, and what it
    /// has now is its base (a `fresh` new message has none).
    async fn attach_all(
        &self,
        draft: &Draft,
        id: MessageId,
        base: Option<Vec<String>>,
        fresh: bool,
    ) -> Result<(MessageId, Option<Vec<String>>)> {
        let recalled = is_recalled(draft, &id);
        let key = key(draft);
        // A recalled draft has what the user took off, and mett's uploads
        // of earlier attempts, to take off.
        let to_take_off = recalled
            && (!draft.drop_attachments.is_empty()
                || self
                    .uploads()
                    .get(&key)
                    .is_some_and(|uploads| !uploads.is_empty()));
        // Nothing of mett's there, and nothing to put on: done.
        if base.is_none() && draft.attachments.is_empty() && !to_take_off {
            return Ok((id, base));
        }
        let present = match &base {
            None if fresh && draft.origin.is_none() => Vec::new(),
            _ => match self.list_attachments(&id).await {
                Ok(present) => present,
                Err(e) => return Err(self.give_up(draft, id, base, e).await),
            },
        };
        // A recalled draft's base is what it has (from this one listing,
        // so that nothing added elsewhere meanwhile comes off), but what
        // is to come off.
        let base = if recalled {
            match self.recalled_base(draft, &present) {
                Ok(recalled) => recalled,
                Err(e) => return Err(self.give_up(draft, id, base, e).await),
            }
        } else {
            base.unwrap_or_else(|| present.iter().map(|a| a.id.to_string()).collect())
        };
        for attachment in present.iter().filter(|a| !base.contains(&a.id.to_string())) {
            if let Err(e) = self.remove_attachment(attachment.id.as_str()).await {
                return Err(self.give_up(draft, id, Some(base), e).await);
            }
            // Gone: no more an upload of mett's to take off.
            if recalled
                && let Some(uploads) = self.uploads().get_mut(&key)
                && let Some(at) = uploads
                    .iter()
                    .position(|u| u.id.as_deref() == Some(attachment.id.as_str()))
            {
                uploads.remove(at);
            }
        }
        for file in &draft.attachments {
            // Remembered before it is sent: the answer may be lost.
            if recalled {
                self.uploads().entry(key.clone()).or_default().push(Upload {
                    id: None,
                    name: file.name.clone(),
                    content_type: file.content_type.clone(),
                    digest: digest(&file.data),
                    prior: present.iter().map(|a| a.id.to_string()).collect(),
                });
            }
            match self.attach(&id, file).await {
                Ok(uploaded) => {
                    if recalled
                        && let Some(upload) = self
                            .uploads()
                            .get_mut(&key)
                            .and_then(|uploads| uploads.last_mut())
                    {
                        upload.id = uploaded;
                    }
                }
                Err(e) => {
                    // Refused: nothing was attached. Only a lost answer
                    // leaves it unknown.
                    if recalled
                        && !matches!(e, Error::Network(_))
                        && let Some(uploads) = self.uploads().get_mut(&key)
                    {
                        uploads.pop();
                    }
                    return Err(self.give_up(draft, id, Some(base), e).await);
                }
            }
        }
        Ok((id, Some(base)))
    }

    /// The base for [`Self::attach_all`] of the recalled `draft`, which has
    /// the attachments `present`: all of them, but those of its own the
    /// user took off and mett's uploads of earlier attempts, which come
    /// off. Others, even ones added elsewhere since, stay.
    ///
    /// An upload whose answer was lost has no known ID, and an attachment
    /// of its name and type may be it, or one added elsewhere: it stays, as
    /// mett can't tell. If the file is still to go, it is attached again
    /// (it may then go twice). If the user took it off (or put another file
    /// in its place), sending it could send what they didn't want: that
    /// fails, for them to look in Outlook.
    fn recalled_base(&self, draft: &Draft, present: &[Attachment]) -> Result<Vec<String>> {
        let uploads = self.uploads().get(&key(draft)).cloned().unwrap_or_default();
        let known = |a: &Attachment| {
            uploads
                .iter()
                .any(|u| u.id.as_deref() == Some(a.id.as_str()))
        };
        let digests: Vec<[u8; 32]> = if uploads.iter().any(|u| u.id.is_none()) {
            draft.attachments.iter().map(|f| digest(&f.data)).collect()
        } else {
            Vec::new()
        };
        for upload in uploads.iter().filter(|u| u.id.is_none()) {
            let same = |name: &str, content_type: &str, digest: &[u8; 32]| {
                name == upload.name
                    && content_type == upload.content_type
                    && *digest == upload.digest
            };
            let wanted = draft
                .attachments
                .iter()
                .zip(&digests)
                .filter(|(f, digest)| same(&f.name, &f.content_type, digest))
                .count();
            let lost = uploads
                .iter()
                .filter(|u| u.id.is_none() && same(&u.name, &u.content_type, &u.digest))
                .count();
            let still_wanted = wanted >= lost;
            let unsure = present.iter().any(|a| !known(a) && upload.is(a));
            if !still_wanted && unsure {
                return Err(Error::Backend(format!(
                    "can't tell whether {} (taken off) is still on the message: \
                     check it in Drafts in Outlook",
                    upload.name
                )));
            }
        }
        Ok(present
            .iter()
            .filter(|a| !draft.drop_attachments.contains(&a.id) && !known(a))
            .map(|a| a.id.to_string())
            .collect())
    }

    /// Gives up on the server draft `id` after `e`: kept for the next
    /// attempt if `e` leaves its state unknown (or it can't be deleted),
    /// deleted otherwise.
    async fn give_up(
        &self,
        draft: &Draft,
        id: MessageId,
        base: Option<Vec<String>>,
        e: Error,
    ) -> Error {
        match e {
            Error::Network(_) => self.keep(draft, id, base),
            _ => {
                if !self.discard(draft, &id).await {
                    self.keep(draft, id, base);
                }
            }
        }
        e
    }

    /// Attaches `file` to the server draft `message`. Returns the
    /// attachment's ID, if EWS said.
    async fn attach(
        &self,
        message: &MessageId,
        file: &OutgoingAttachment,
    ) -> Result<Option<String>> {
        let about = About::message(message.as_str());
        let body = format!(
            "<m:CreateAttachment><m:ParentItemId Id=\"{}\"/><m:Attachments>{}</m:Attachments>\
             </m:CreateAttachment>",
            escape(message.as_str()),
            file_attachment_xml(file)
        );
        let xml = self.call(&body, about).await?;
        let doc = parse(&xml)?;
        let response = only_message(&doc, about)?;
        Ok(descendant(response, "AttachmentId")
            .and_then(|a| a.attribute("Id"))
            .map(str::to_owned))
    }

    /// Takes an attachment off a server draft. One already gone is as good
    /// as taken off.
    async fn remove_attachment(&self, attachment: &str) -> Result<()> {
        let about = About::attachment(attachment);
        let body = format!(
            "<m:DeleteAttachment><m:AttachmentIds><t:AttachmentId Id=\"{}\"/></m:AttachmentIds>\
             </m:DeleteAttachment>",
            escape(attachment)
        );
        let outcome = match self.call(&body, about).await {
            Ok(xml) => only_message(&parse(&xml)?, about).map(drop),
            Err(e) => Err(e),
        };
        match outcome {
            Ok(()) | Err(Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Changes item `id` with `updates` (`SetItemField`s and
    /// `DeleteItemField`s).
    async fn update_item(&self, id: &MessageId, updates: &str) -> Result<()> {
        let about = About::message(id.as_str());
        let body = format!(
            "<m:UpdateItem MessageDisposition=\"SaveOnly\" ConflictResolution=\"AlwaysOverwrite\" \
             SuppressReadReceipts=\"true\"><m:ItemChanges><t:ItemChange>{}\
             <t:Updates>{updates}</t:Updates></t:ItemChange></m:ItemChanges></m:UpdateItem>",
            item_id_xml(id.as_str())
        );
        let xml = self.call(&body, about).await?;
        only_message(&parse(&xml)?, about).map(drop)
    }

    /// Gives a server draft `draft`'s content.
    async fn update_draft(&self, id: &MessageId, draft: &Draft) -> Result<()> {
        let mut updates = set_field(
            "item:Subject",
            &format!("<t:Subject>{}</t:Subject>", escape(&draft.subject)),
        );
        updates.push_str(&set_field(
            "item:Body",
            &format!("<t:Body BodyType=\"Text\">{}</t:Body>", escape(&draft.body)),
        ));
        for (uri, element, list) in [
            ("message:ToRecipients", "ToRecipients", &draft.to),
            ("message:CcRecipients", "CcRecipients", &draft.cc),
            ("message:BccRecipients", "BccRecipients", &draft.bcc),
        ] {
            if list.is_empty() {
                updates.push_str(&format!(
                    "<t:DeleteItemField>{}</t:DeleteItemField>",
                    field(uri)
                ));
            } else {
                updates.push_str(&set_field(
                    uri,
                    &format!("<t:{element}>{}</t:{element}>", mailboxes(list)),
                ));
            }
        }
        if let Some(from) = &draft.from {
            updates.push_str(&set_field(
                "message:From",
                &format!("<t:From>{}</t:From>", mailboxes(std::slice::from_ref(from))),
            ));
        }
        self.update_item(id, &updates).await
    }

    /// The current change key of item `id`, which sending it, and
    /// replying to or forwarding it, take.
    async fn change_key(&self, id: &MessageId) -> Result<String> {
        let about = About::message(id.as_str());
        let xml = self.get_items(&[id.to_string()], &[], false, about).await?;
        let doc = parse(&xml)?;
        let message = only_message(&doc, about)?;
        first_item(message)
            .and_then(|item| child(item, "ItemId"))
            .and_then(|item| item.attribute("ChangeKey"))
            .map(str::to_owned)
            .ok_or_else(|| unexpected("GetItem", "no ChangeKey"))
    }

    /// Sends the server draft `id`, as of `change_key`, keeping a copy in
    /// Sent Items if `save_copy`.
    async fn send_draft(&self, id: &MessageId, change_key: &str, save_copy: bool) -> Result<()> {
        let about = About::message(id.as_str());
        let saved = if save_copy {
            "<m:SavedItemFolderId><t:DistinguishedFolderId Id=\"sentitems\"/></m:SavedItemFolderId>"
        } else {
            ""
        };
        let body = format!(
            "<m:SendItem SaveItemToFolder=\"{save_copy}\"><m:ItemIds>\
             <t:ItemId Id=\"{}\" ChangeKey=\"{}\"/></m:ItemIds>{saved}</m:SendItem>",
            escape(id.as_str()),
            escape(change_key)
        );
        let xml = self.hand_over(&body, about).await?;
        only_message(&parse(&xml)?, about).map(drop)
    }

    /// Forgets the uploads of `draft`, which is done with: sent, postponed,
    /// or given up.
    fn forget_uploads(&self, draft: &Draft) {
        self.uploads().remove(&key(draft));
    }

    /// Whether a server draft is kept for `draft`.
    fn has_kept(&self, draft: &Draft) -> bool {
        self.kept().contains_key(&key(draft))
    }

    /// Remembers a server draft kept after an attempt whose outcome is
    /// unknown, with the attachments it added.
    fn keep(&self, draft: &Draft, id: MessageId, base: Option<Vec<String>>) {
        self.kept().insert(key(draft), Kept { id, base });
    }

    /// Deletes a server draft of `draft` that could not be completed, so it
    /// doesn't linger in Drafts, and returns whether it is gone. A recalled
    /// postponed message is never deleted: it is the user's.
    async fn discard(&self, draft: &Draft, id: &MessageId) -> bool {
        !is_recalled(draft, id) && self.delete_draft(id).await.is_ok()
    }

    /// Deletes a server draft for good. One that is gone already is as
    /// good as deleted.
    async fn delete_draft(&self, id: &MessageId) -> Result<()> {
        match self.delete_item(id).await {
            Ok(()) | Err(Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// How long a request of `size` bytes may take: time for it to go up, and
/// `download` (at least [`REQUEST_TIMEOUT`]) for the rest.
fn timeout_for(size: usize, download: Duration) -> Duration {
    let upload = u64::try_from(size / BYTES_PER_SECOND).unwrap_or(u64::MAX);
    download.max(REQUEST_TIMEOUT) + Duration::from_secs(upload)
}

/// Whether `id` is the postponed message `draft` recalls.
fn is_recalled(draft: &Draft, id: &MessageId) -> bool {
    matches!(&draft.origin, Some(DraftOrigin::Recalled(recalled)) if recalled == id)
}

/// A network failure before a message was handed over: it wasn't sent.
fn unsent(e: Error) -> Error {
    match e {
        Error::Network(e) => Error::NotSent(format!("network error: {e}")),
        e => e,
    }
}

/// Whether `draft` goes through a server draft: it answers a message, has
/// attachments, which Exchange takes only on a saved message, or is sent
/// from a given address without a copy, which Exchange doesn't send in one
/// request for a delegate. A failed attempt to send such a message may leave
/// its draft behind.
pub fn needs_server_draft(draft: &Draft) -> bool {
    draft.origin.is_some()
        || !draft.attachments.is_empty()
        || (draft.from.is_some() && !draft.save_copy)
}

/// An unexpected response to `operation`.
fn unexpected(operation: &str, what: &str) -> Error {
    Error::Backend(format!(
        "unexpected response from EWS to {operation}: {what}"
    ))
}

/// A `FieldURI` for `uri`, such as `item:Subject`.
fn field(uri: &str) -> String {
    format!("<t:FieldURI FieldURI=\"{uri}\"/>")
}

/// A `SetItemField` setting `uri` to the message `content` holds.
fn set_field(uri: &str, content: &str) -> String {
    format!(
        "<t:SetItemField>{}<t:Message>{content}</t:Message></t:SetItemField>",
        field(uri)
    )
}

/// An `ItemShape` with `fields`, and the body as `body_type` if given.
fn item_shape(fields: &[&str], body_type: Option<&str>) -> String {
    let fields: String = fields.iter().map(|uri| field(uri)).collect();
    let body_type = body_type
        .map(|t| format!("<t:BodyType>{t}</t:BodyType>"))
        .unwrap_or_default();
    let fields = if fields.is_empty() {
        String::new()
    } else {
        format!("<t:AdditionalProperties>{fields}</t:AdditionalProperties>")
    };
    format!("<m:ItemShape><t:BaseShape>IdOnly</t:BaseShape>{body_type}{fields}</m:ItemShape>")
}

/// The `FolderShape` with the folder fields mett reads.
fn folder_shape() -> String {
    let fields: String = FOLDER_FIELDS.iter().map(|uri| field(uri)).collect();
    format!(
        "<m:FolderShape><t:BaseShape>IdOnly</t:BaseShape>\
         <t:AdditionalProperties>{fields}</t:AdditionalProperties></m:FolderShape>"
    )
}

/// A `FolderId` element.
fn folder_id(id: &FolderId) -> String {
    format!("<t:FolderId Id=\"{}\"/>", escape(id.as_str()))
}

/// An `ItemId` element.
fn item_id_xml(id: &str) -> String {
    format!("<t:ItemId Id=\"{}\"/>", escape(id))
}

/// A restriction to items modified at or after `since`.
fn modified_since(since: DateTime<Utc>) -> String {
    format!(
        "<t:IsGreaterThanOrEqualTo>{}<t:FieldURIOrConstant><t:Constant Value=\"{}\"/>\
         </t:FieldURIOrConstant></t:IsGreaterThanOrEqualTo>",
        field("item:LastModifiedTime"),
        since.to_rfc3339_opts(SecondsFormat::Secs, true)
    )
}

/// A `CreateItem` of `items` with `disposition`, saved in the
/// distinguished folder `saved` if given.
fn create_item(disposition: &str, saved: Option<&str>, items: &str) -> String {
    let saved = saved
        .map(|name| {
            format!(
                "<m:SavedItemFolderId><t:DistinguishedFolderId Id=\"{name}\"/></m:SavedItemFolderId>"
            )
        })
        .unwrap_or_default();
    format!(
        "<m:CreateItem MessageDisposition=\"{disposition}\">{saved}<m:Items>{items}</m:Items>\
         </m:CreateItem>"
    )
}

/// The ID of the item a `CreateItem` made.
fn created_id(xml: &str) -> Result<MessageId> {
    let doc = parse(xml)?;
    let message = only_message(&doc, About::MAILBOX)?;
    first_item(message)
        .and_then(item_id)
        .map(MessageId::new)
        .ok_or_else(|| unexpected("CreateItem", "no item ID"))
}

/// `Mailbox` elements for `addresses`.
fn mailboxes(addresses: &[Address]) -> String {
    addresses
        .iter()
        .map(|a| {
            let name = a
                .name
                .as_deref()
                .filter(|n| !n.is_empty())
                .map(|n| format!("<t:Name>{}</t:Name>", escape(n)))
                .unwrap_or_default();
            format!(
                "<t:Mailbox>{name}<t:EmailAddress>{}</t:EmailAddress></t:Mailbox>",
                escape(&a.email)
            )
        })
        .collect()
}

/// A `FileAttachment` element for `file`.
fn file_attachment_xml(file: &OutgoingAttachment) -> String {
    use base64::Engine;

    format!(
        "<t:FileAttachment><t:Name>{}</t:Name><t:ContentType>{}</t:ContentType>\
         <t:Content>{}</t:Content></t:FileAttachment>",
        escape(&file.name),
        escape(&file.content_type),
        base64::engine::general_purpose::STANDARD.encode(&file.data)
    )
}

/// A `Message` element with `draft`'s content, but its attachments (which
/// Exchange takes only on a saved message). The elements follow the order
/// the schema requires.
fn message_xml(draft: &Draft) -> String {
    let mut xml = format!(
        "<t:Message><t:Subject>{}</t:Subject><t:Body BodyType=\"Text\">{}</t:Body>",
        escape(&draft.subject),
        escape(&draft.body)
    );
    for (element, list) in [
        ("ToRecipients", &draft.to),
        ("CcRecipients", &draft.cc),
        ("BccRecipients", &draft.bcc),
    ] {
        if !list.is_empty() {
            xml.push_str(&format!("<t:{element}>{}</t:{element}>", mailboxes(list)));
        }
    }
    if let Some(from) = &draft.from {
        xml.push_str(&format!(
            "<t:From>{}</t:From>",
            mailboxes(std::slice::from_ref(from))
        ));
    }
    xml.push_str("</t:Message>");
    xml
}

/// The offset of the page after one at `offset` with `found` entries, in
/// a `RootFolder` that says whether it has more.
fn next_offset(root: Node<'_, '_>, offset: u32, found: usize) -> Option<u32> {
    if root.attribute("IncludesLastItemInRange") != Some("false") || found == 0 {
        return None;
    }
    let after = offset.saturating_add(u32::try_from(found).unwrap_or(u32::MAX));
    let next = root
        .attribute("IndexedPagingOffset")
        .and_then(|o| o.parse().ok())
        .unwrap_or(after);
    // Always make progress.
    Some(next.max(after))
}

/// A page cursor: where the next page starts, in which listing (`query`)
/// of which folder.
fn cursor(offset: u32, query: &str, folder: &FolderId) -> PageCursor {
    PageCursor::new(format!("{offset} {query} {folder}"))
}

/// The offset in `cursor`, which must be for `query` of `folder`.
fn parse_cursor(cursor: &PageCursor, folder: &FolderId, query: &str) -> Result<u32> {
    let bad = || {
        Error::Backend(format!(
            "page cursor is not for this listing of folder {folder}"
        ))
    };
    let (offset, rest) = cursor.as_str().split_once(' ').ok_or_else(bad)?;
    let (cursor_query, cursor_folder) = rest.split_once(' ').ok_or_else(bad)?;
    if cursor_query != query || cursor_folder != folder.as_str() {
        return Err(bad());
    }
    offset.parse().map_err(|_| bad())
}

/// The first item of a response message's `Items`.
fn first_item<'a, 'i>(message: Node<'a, 'i>) -> Option<Node<'a, 'i>> {
    child(message, "Items").and_then(|items| items.children().find(Node::is_element))
}

/// An item's ID.
fn item_id(item: Node<'_, '_>) -> Option<String> {
    child(item, "ItemId")
        .and_then(|id| id.attribute("Id"))
        .map(str::to_owned)
}

/// An item found by `FindItem`.
struct FoundItem {
    id: String,
    /// Its `LastModifiedTime`, if asked for.
    modified: Option<String>,
}

/// A folder, as EWS gives it.
#[derive(Debug)]
struct EwsFolder {
    id: String,
    parent: Option<String>,
    name: String,
    /// The kind of folder: `Folder` for mail folders (and generic ones),
    /// or `CalendarFolder`, `SearchFolder` and so on.
    element: String,
    class: Option<String>,
    unread: u32,
    total: u32,
}

impl EwsFolder {
    fn parse(node: Node<'_, '_>) -> Option<Self> {
        let number = |name: &str| {
            child_text(node, name)
                .and_then(|n| n.trim().parse().ok())
                .unwrap_or(0)
        };
        Some(Self {
            id: child(node, "FolderId")?.attribute("Id")?.to_owned(),
            parent: child(node, "ParentFolderId")
                .and_then(|p| p.attribute("Id"))
                .map(str::to_owned),
            name: child_text(node, "DisplayName").unwrap_or_default(),
            element: node.tag_name().name().to_owned(),
            class: child_text(node, "FolderClass"),
            unread: number("UnreadCount"),
            total: number("TotalCount"),
        })
    }

    /// Whether it holds mail: a plain folder of mail (`IPF.Note`) or of no
    /// class.
    fn is_mail(&self) -> bool {
        self.element == "Folder"
            && self.class.as_deref().is_none_or(|class| {
                class.is_empty() || class == "IPF.Note" || class.starts_with("IPF.Note.")
            })
    }
}

/// A person `ResolveNames` found.
struct Resolved {
    name: Option<String>,
    /// Their address, as the mailbox gives it.
    email: Option<String>,
    /// Every SMTP address known for them: the mailbox's, and with full
    /// contact data, the contact's.
    addresses: Vec<String>,
}

impl Resolved {
    fn parse(resolution: Node<'_, '_>) -> Self {
        let mailbox = child(resolution, "Mailbox");
        let routing = mailbox.and_then(|m| child_text(m, "RoutingType"));
        let email = mailbox
            .and_then(|m| child_text(m, "EmailAddress"))
            .filter(|_| {
                routing
                    .as_deref()
                    .is_none_or(|r| r.eq_ignore_ascii_case("SMTP"))
            });
        let name = mailbox.and_then(|m| child_text(m, "Name"));
        let mut addresses: Vec<String> = email.iter().cloned().collect();
        if let Some(entries) = child(resolution, "Contact").and_then(|c| child(c, "EmailAddresses"))
        {
            for entry in children(entries, "Entry") {
                let value = entry.text().unwrap_or_default().trim();
                let address = match value.split_once(':') {
                    Some((kind, address)) if kind.eq_ignore_ascii_case("smtp") => address,
                    Some(_) => continue,
                    None => value,
                };
                addresses.push(address.to_owned());
            }
        }
        Self {
            name,
            email,
            addresses,
        }
    }
}

/// An attachment, from an item's `Attachments`.
fn attachment(node: Node<'_, '_>) -> Option<Attachment> {
    let kind = match node.tag_name().name() {
        "FileAttachment" => AttachmentKind::File,
        "ItemAttachment" => AttachmentKind::Item,
        "ReferenceAttachment" => AttachmentKind::Link,
        _ => return None,
    };
    let id = child(node, "AttachmentId")?.attribute("Id")?.to_owned();
    Some(Attachment {
        id: AttachmentId::new(id),
        name: child_text(node, "Name").unwrap_or_default(),
        // An attached item comes as MIME.
        content_type: child_text(node, "ContentType")
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| match kind {
                AttachmentKind::Item => "message/rfc822".into(),
                _ => "application/octet-stream".into(),
            }),
        size: child_text(node, "Size")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0),
        inline: child_text(node, "IsInline").as_deref() == Some("true"),
        kind,
    })
}

/// `text` decoded from base64, ignoring whitespace.
fn decode_base64(text: &str) -> Result<Vec<u8>> {
    use base64::Engine;

    /// Characters decoded at a time (a multiple of 4).
    const BLOCK: usize = 4096;

    let invalid = |e: &dyn std::fmt::Display| {
        Error::Backend(format!("invalid attachment content from EWS: {e}"))
    };
    let engine = base64::engine::general_purpose::STANDARD;
    let mut decoded = Vec::with_capacity(text.len() / 4 * 3);
    let mut block = [0u8; BLOCK];
    let mut filled = 0;
    for c in text.chars().filter(|c| !c.is_whitespace()) {
        block[filled] = u8::try_from(c).map_err(|_| invalid(&"not base64"))?;
        filled += 1;
        if filled == BLOCK {
            engine
                .decode_vec(&block[..], &mut decoded)
                .map_err(|e| invalid(&e))?;
            filled = 0;
        }
    }
    engine
        .decode_vec(&block[..filled], &mut decoded)
        .map_err(|e| invalid(&e))?;
    Ok(decoded)
}

/// The addresses of a recipients element (such as `ToRecipients`) of
/// `item`.
fn recipients(item: Node<'_, '_>, element: &str) -> Vec<Address> {
    child(item, element)
        .into_iter()
        .flat_map(|list| children(list, "Mailbox"))
        .map(address)
        .collect()
}

/// A `Mailbox` element as an address.
pub(crate) fn address(mailbox: Node<'_, '_>) -> Address {
    Address {
        name: child_text(mailbox, "Name").filter(|n| !n.is_empty()),
        email: child_text(mailbox, "EmailAddress").unwrap_or_default(),
    }
}

/// An EWS timestamp, if it parses.
fn parse_time(time: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(time.trim())
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// An item's summary.
fn summary(item: Node<'_, '_>) -> Result<MessageSummary> {
    let id = item_id(item).ok_or_else(|| unexpected("GetItem", "an item without an ID"))?;
    let time = |name: &str| child_text(item, name).as_deref().and_then(parse_time);
    let is_draft = child_text(item, "IsDraft").as_deref() == Some("true");
    // The date is when the message was sent, as in mutt's index, or for an
    // unsent draft, when it was created. It matches the listing's sort
    // order.
    let sent = time("DateTimeSent").filter(|_| !is_draft);
    let received = sent
        .or_else(|| time("DateTimeCreated"))
        .ok_or_else(|| Error::Backend("message has no timestamp".into()))?;
    let conversation = child(item, "ConversationId")
        .and_then(|c| c.attribute("Id"))
        .map(str::to_owned);
    Ok(MessageSummary {
        id: MessageId::new(id),
        folder: FolderId::new(
            child(item, "ParentFolderId")
                .and_then(|f| f.attribute("Id"))
                .unwrap_or_default(),
        ),
        subject: child_text(item, "Subject").unwrap_or_default(),
        from: child(item, "From")
            .and_then(|f| child(f, "Mailbox"))
            .map(address),
        to: recipients(item, "ToRecipients"),
        received,
        flags: Flags {
            // Items that aren't messages have no read state: they count as
            // read.
            read: child_text(item, "IsRead").is_none_or(|r| r == "true"),
            flagged: text(item, "FlagStatus").as_deref() == Some("Flagged"),
        },
        has_attachments: child_text(item, "HasAttachments").as_deref() == Some("true"),
        // Without a sent date, it is listed with the drafts.
        draft: sent.is_none(),
        delivered: time("DateTimeReceived"),
        modified: time("LastModifiedTime"),
        thread: conversation.map(|conversation| ThreadPosition {
            conversation,
            path: child_text(item, "ConversationIndex")
                .as_deref()
                .map(thread_path)
                .unwrap_or_default(),
        }),
    })
}

/// A full message.
fn full_message(item: Node<'_, '_>) -> Result<Message> {
    let body = child(item, "Body");
    Ok(Message {
        summary: summary(item)?,
        cc: recipients(item, "CcRecipients"),
        bcc: recipients(item, "BccRecipients"),
        reply_to: recipients(item, "ReplyTo"),
        internet_message_id: child_text(item, "InternetMessageId"),
        body: Body {
            kind: match body.and_then(|b| b.attribute("BodyType")) {
                Some(t) if t.eq_ignore_ascii_case("HTML") => BodyKind::Html,
                _ => BodyKind::Text,
            },
            content: body.and_then(|b| b.text()).unwrap_or_default().to_owned(),
        },
        invitation: invitation(item),
    })
}

/// A message's path in its conversation, from Outlook's conversation
/// index: the 22-byte header (the conversation's first message), then a
/// 5-byte block for each reply. An index that doesn't decode, or doesn't
/// have that shape, gives none.
fn thread_path(index: &str) -> Vec<Vec<u8>> {
    use base64::Engine;

    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(index.trim()) else {
        return Vec::new();
    };
    if bytes.len() < 22 || (bytes.len() - 22) % 5 != 0 {
        return Vec::new();
    }
    let (header, replies) = bytes.split_at(22);
    std::iter::once(header.to_vec())
        .chain(replies.chunks(5).map(<[u8]>::to_vec))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(id: &str, parent: &str, name: &str, element: &str, class: Option<&str>) -> EwsFolder {
        EwsFolder {
            id: id.into(),
            parent: Some(parent.into()),
            name: name.into(),
            element: element.into(),
            class: class.map(str::to_owned),
            unread: 0,
            total: 0,
        }
    }

    #[test]
    fn folders_come_in_tree_order_by_name() {
        let folders = vec![
            folder("b", "root", "beta", "Folder", Some("IPF.Note")),
            folder("a2", "a", "Two", "Folder", None),
            folder("a", "root", "Alpha", "Folder", Some("IPF.Note")),
            folder("a1", "a", "one", "Folder", Some("IPF.Note")),
        ];
        let ids: Vec<String> = tree_order(folders, Some("root"))
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(ids, ["a", "a1", "a2", "b"]);
    }

    #[test]
    fn only_mail_folders_are_listed() {
        let folders = vec![
            folder("inbox", "root", "Inbox", "Folder", Some("IPF.Note")),
            folder(
                "cal",
                "root",
                "Calendar",
                "CalendarFolder",
                Some("IPF.Appointment"),
            ),
            folder("below-cal", "cal", "Mail?", "Folder", Some("IPF.Note")),
            folder("search", "root", "Finder", "SearchFolder", Some("IPF.Note")),
            folder("tasks", "root", "Tasks", "Folder", Some("IPF.Task")),
            folder("plain", "inbox", "Plain", "Folder", None),
        ];
        let ids: Vec<String> = mail_folders(folders).into_iter().map(|f| f.id).collect();
        assert_eq!(ids, ["inbox", "plain"]);
    }

    #[test]
    fn large_requests_and_downloads_get_more_time() {
        assert_eq!(timeout_for(100, Duration::ZERO), REQUEST_TIMEOUT);
        assert_eq!(
            timeout_for(10 * BYTES_PER_SECOND, Duration::ZERO),
            REQUEST_TIMEOUT + Duration::from_secs(10)
        );
        assert_eq!(timeout_for(100, DOWNLOAD_TIMEOUT), DOWNLOAD_TIMEOUT);
    }

    #[test]
    fn decodes_base64_wrapped_in_white_space() {
        use base64::Engine;

        let data: Vec<u8> = (0..20_000u32).map(|i| (i * 31 % 251) as u8).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
        let wrapped: String = encoded
            .as_bytes()
            .chunks(76)
            .flat_map(|line| [std::str::from_utf8(line).unwrap(), "\r\n"])
            .collect();
        assert_eq!(decode_base64(&wrapped).unwrap(), data);
        assert_eq!(decode_base64("").unwrap(), b"");
        assert_eq!(decode_base64(" TW\nE= ").unwrap(), b"Ma");
        assert!(decode_base64("TW=E").is_err());
        assert!(decode_base64("TWFu\u{e9}").is_err());
    }

    #[test]
    fn folders_sort_by_name_without_regard_to_case() {
        let folder = |name: &str, id: &str| EwsFolder {
            name: name.into(),
            id: id.into(),
            parent: None,
            element: "Folder".into(),
            class: None,
            unread: 0,
            total: 0,
        };
        let mut folders = vec![folder("b", "2"), folder("B", "1"), folder("A", "3")];
        sort_by_name(&mut folders);
        let ids: Vec<&str> = folders.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["3", "1", "2"]);
    }

    #[test]
    fn cursors_belong_to_their_listing() {
        let folder = FolderId::new("AQMk/a+b=");
        let c = cursor(50, "all", &folder);
        assert_eq!(parse_cursor(&c, &folder, "all").unwrap(), 50);
        assert!(parse_cursor(&c, &FolderId::new("other"), "all").is_err());
        assert!(parse_cursor(&c, &folder, "2026-01-01T00:00:00Z").is_err());
    }

    #[test]
    fn messages_follow_the_schema_order() {
        let draft = Draft {
            from: Some(Address::new("me@contoso.com")),
            to: vec![Address::new("a@x.org").with_name("A & B")],
            bcc: vec![Address::new("b@x.org")],
            subject: "<hi>".into(),
            body: "text".into(),
            ..Draft::default()
        };
        let xml = message_xml(&draft);
        let order: Vec<usize> = [
            "<t:Subject>",
            "<t:Body",
            "<t:ToRecipients>",
            "<t:BccRecipients>",
            "<t:From>",
        ]
        .iter()
        .map(|tag| xml.find(tag).unwrap())
        .collect();
        assert!(order.is_sorted(), "{xml}");
        assert!(xml.contains("<t:Subject>&lt;hi&gt;</t:Subject>"));
        assert!(xml.contains("<t:Name>A &amp; B</t:Name>"));
        assert!(!xml.contains("CcRecipients"));
    }
}
