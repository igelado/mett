//! [`EwsMailStore`]: the [`MailStore`] port for Exchange Web Services.
//!
//! It works on the signed-in user's mailbox, with the EWS SOAP API. Every
//! request asks the [`Authenticator`] for a token, so refreshing and
//! signing in happen transparently.

mod drafts;
mod folders;
mod invitations;
#[cfg(test)]
mod tests;
mod wire;

pub use drafts::needs_server_draft;
pub(crate) use wire::address;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::{DateTime, SecondsFormat, Utc};
use mett_core::{
    AccessToken, AccountId, Address, Answer, Attachment, AttachmentId, AttachmentKind,
    Authenticator, Body, BodyKind, Draft, DraftOrigin, DraftSession, Error, Flags, FlagsUpdate,
    Folder, FolderChanges, FolderId, Invitation, MAX_PAGE_SIZE, MailSender, MailStore, Message,
    MessageId, MessagePage, MessageSummary, OutgoingAttachment, Page, PageCursor, RECENT_UNREAD,
    ROOT, Result, Roles, Scope, SyncState, ThreadPosition, WellKnownFolder, error_for_status,
    first_then, join_bounded_until, parse_time, retry_after, unsent, with_fresh_token,
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
use folders::{distinguished, mail_folders, sort_by_name, tree_order};
use wire::{
    EwsFolder, FoundItem, MESSAGE_FIELDS, Resolved, SUMMARY_FIELDS, attachment, create_item,
    created_id, cursor, decode_base64, field, file_attachment_xml, first_item, folder_id,
    folder_shape, full_message, item_id, item_id_xml, item_shape, mailboxes, message_xml,
    modified_since, next_offset, only_message_or_none, parse_cursor, set_field, summary,
    unexpected,
};

/// Upper bound on each EWS request with a small body. Larger ones (with
/// attachments) get a second more for each [`BYTES_PER_SECOND`].
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The slowest upload a request with attachments is given time for.
const BYTES_PER_SECOND: usize = 64 * 1024;

/// Upper bound on a request whose response may be large (an attachment's
/// content), which its own size doesn't tell.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The most items looked up in one `GetItem`: Exchange's limit (its
/// `EWSMaxBatchSize`, by default) is often 100.
const ITEMS_PER_REQUEST: usize = 100;

/// The most `GetItem` requests in flight at once for one listing: a few
/// shorten a big batch without feeding the server's throttling.
const CHUNKS_AT_ONCE: usize = 3;

/// The most changes one `SyncFolderItems` returns (EWS takes up to 512).
const SYNC_BATCH: u32 = 512;

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
    /// What is known of the server drafts made for messages being sent or
    /// postponed.
    drafts: DraftSession,
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
            drafts: DraftSession::new(),
            watching: tokio::sync::Mutex::new(()),
            subscriptions: Arc::new(tokio::sync::Mutex::new(Subscriptions::default())),
            unreported: Mutex::new(Unreported::default()),
        }
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
        // EWS may reject a token that hadn't expired (it may have been
        // revoked): `with_fresh_token` gets a new one and tries once more.
        with_fresh_token(
            &*self.auth,
            before,
            true,
            |response: &reqwest::Response| response.status() == StatusCode::UNAUTHORIZED,
            |token| async move { self.attempt(xml, &token, timeout).await },
        )
        .await
        .map(|(response, _)| response)
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
    let retry_after = retry_after(
        response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
    );
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
    Err(error_for_status(status.as_u16(), retry_after, detail, None))
}

impl<A: Authenticator + 'static> EwsMailStore<A> {
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
        // Exchange takes only so many items per request (often 100): the
        // requests go a few at a time, not one after the other. The error
        // a walk one request at a time would stop at comes back once it is
        // known that none comes before it, without waiting for the other
        // requests (a throttled one included: the caller backs off).
        let chunks = join_bounded_until(
            ids.chunks(ITEMS_PER_REQUEST)
                .map(|ids| self.chunk_summaries(ids, about))
                .collect(),
            CHUNKS_AT_ONCE,
            |chunks| chunks.iter().map_while(Option::as_ref).any(Result::is_err),
        )
        .await;
        let mut summaries = Vec::new();
        for chunk in chunks {
            // Only a chunk after the first error is left unfinished.
            summaries.extend(chunk.expect("every chunk before the first error finished")?);
        }
        Ok(summaries)
    }

    /// The summaries of the messages `ids` (up to [`ITEMS_PER_REQUEST`]),
    /// in that order, from one `GetItem`.
    async fn chunk_summaries(
        &self,
        ids: &[String],
        about: About<'_>,
    ) -> Result<Vec<MessageSummary>> {
        let xml = self.get_items(ids, SUMMARY_FIELDS, false, about).await?;
        let doc = parse(&xml)?;
        let mut summaries = Vec::new();
        for message in response_messages(&doc, about)? {
            if response_code(message) == "ErrorItemNotFound" {
                continue;
            }
            check(message, about)?;
            if let Some(item) = first_item(message) {
                summaries.push(summary(item)?);
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

    fn capabilities(&self) -> mett_core::Capabilities {
        mett_core::Capabilities {
            can_sync: true,
            can_watch: true,
            ..mett_core::Capabilities::default()
        }
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
        if needs_server_draft(draft) || self.drafts.has_kept(draft) {
            // Postponed: what mett attached is the draft's own now.
            let saved = self
                .drafts
                .server_draft(self, draft)
                .await
                .map(|(id, _)| id);
            if saved.is_ok() {
                self.drafts.forget_uploads(draft);
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
        use crate::stream::{Streamed, check_pending, read_envelope, take_envelopes};

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
            for envelope in take_envelopes(&mut buffer, &mut scanned) {
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
            // What came complete was used: a rest that is too large ends it.
            match check_pending(&buffer) {
                Ok(()) => {}
                Err(_) if !changed.is_empty() => return Ok(Some((changed, true))),
                Err(e) => return Err(e),
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
        self.drafts.discard_kept(self, draft).await
    }

    fn keeps_draft(&self, draft: &Draft) -> bool {
        self.drafts.has_kept(draft)
    }

    async fn send(&self, draft: &Draft) -> Result<()> {
        // A server draft kept from an earlier attempt is sent, not left
        // behind.
        if !needs_server_draft(draft) && !self.drafts.has_kept(draft) {
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
        let (id, base) = self
            .drafts
            .server_draft(self, draft)
            .await
            .map_err(unsent)?;
        // Sending takes the draft as it is now, which its change key says.
        let change_key = match self.change_key(&id).await {
            Ok(change_key) => change_key,
            Err(e) => {
                return Err(unsent(self.drafts.give_up(self, draft, id, base, e).await));
            }
        };
        let sent = self.send_draft(&id, &change_key, draft.save_copy).await;
        self.drafts.after_send(self, draft, id, base, sent).await
    }
}

/// How long a request of `size` bytes may take: time for it to go up, and
/// `download` (at least [`REQUEST_TIMEOUT`]) for the rest.
fn timeout_for(size: usize, download: Duration) -> Duration {
    let upload = u64::try_from(size / BYTES_PER_SECOND).unwrap_or(u64::MAX);
    download.max(REQUEST_TIMEOUT) + Duration::from_secs(upload)
}
