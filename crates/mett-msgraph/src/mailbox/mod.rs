//! [`GraphMailStore`]: the [`MailStore`] port for Microsoft Graph.
//!
//! It works on the signed-in user's mailbox (`/me`), using the Graph REST
//! API v1.0. Every request asks the [`Authenticator`] for a token, so
//! refreshing and signing in happen transparently.

mod drafts;
mod folders;
mod invitations;
#[cfg(test)]
mod tests;
mod wire;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use mett_core::{
    AccessToken, Address, Answer, Attachment, AttachmentId, AttachmentKind, Authenticator, Body,
    BodyKind, Draft, DraftOrigin, DraftSession, Error, Flags, FlagsUpdate, Folder, FolderChanges,
    FolderId, Invitation, MAX_PAGE_SIZE, MailSender, MailStore, Message, MessageId, MessagePage,
    MessageSummary, OutgoingAttachment, Page, PageCursor, RECENT_UNREAD, ROOT, Result, Roles,
    Scope, SyncState, ThreadPosition, WellKnownFolder, error_for_status, first_then,
    join_bounded_until, nameable, retry_after, unsent, with_fresh_token,
};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;

use crate::invitation::{
    EVENT_EXPAND, EventKind, Instances, MEETING_FIELDS, MeetingMessage, PREFER_UTC, answer_action,
    answer_json,
};
use crate::settings::{AuthSettings, Cloud, with_segments};
use folders::{LISTINGS_AT_ONCE, Listings, Walk, walk, with_children};
use wire::{
    BatchResponse, Collection, Delta, ErrorResponse, GraphAttachment, GraphFolder, GraphMessage,
    IdOnly, Me, MoveRequest, file_attachment_json, message_json, send_mail_json, send_mail_size,
};

pub use drafts::needs_server_draft;
pub(crate) use wire::EmailAddress;

/// Upper bound on each Graph request, unless it moves a lot of data (an
/// attachment's content: see [`DOWNLOAD_TIMEOUT`] and [`upload_timeout`]).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The slowest upload a request with a large body is given time for.
const BYTES_PER_SECOND: usize = 64 * 1024;

/// Upper bound on a request whose response may be large (an attachment's
/// content), which its own size doesn't tell.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How long a request that uploads `size` bytes may take: the usual
/// [`REQUEST_TIMEOUT`], and a second more for each [`BYTES_PER_SECOND`].
fn upload_timeout(size: usize) -> Duration {
    let seconds = u64::try_from(size / BYTES_PER_SECOND).unwrap_or(u64::MAX);
    REQUEST_TIMEOUT + Duration::from_secs(seconds)
}

/// The message fields the index needs.
const SUMMARY_FIELDS: &str = "id,parentFolderId,subject,from,toRecipients,sentDateTime,createdDateTime,isDraft,isRead,flag,hasAttachments,lastModifiedDateTime,receivedDateTime,conversationId,conversationIndex";

/// A [`MailStore`] backed by Microsoft Graph.
///
/// Messages are dated and sorted by when they were sent; unsent drafts use
/// their creation date. Graph can only sort by one field at a time, so in
/// a folder that mixes unsent drafts with other mail, the drafts are
/// grouped together rather than interleaved by date. Drafts normally live
/// only in the Drafts folder, where they all sort by creation date.
pub struct GraphMailStore<A> {
    http: reqwest::Client,
    /// The Graph v1.0 root, for example `https://graph.microsoft.com/v1.0/`.
    base: Url,
    /// The cloud's own Graph v1.0 root. Graph's page links point there even
    /// when `base` is a proxy.
    canonical: Url,
    cloud: Cloud,
    auth: Arc<A>,
    /// The mailbox's folder roles, once looked up, and when.
    roles: Mutex<Option<(Arc<Roles>, Instant)>>,
    /// The hidden root folder's ID, once looked up on its own.
    root: Mutex<Option<Option<FolderId>>>,
    /// What is known of the server drafts made for messages being sent or
    /// postponed.
    drafts: DraftSession,
}

impl<A: Authenticator> GraphMailStore<A> {
    /// A store for the account in `settings`, using its cloud's Graph
    /// endpoint and `auth` for tokens.
    pub fn new(http: reqwest::Client, settings: &AuthSettings, auth: Arc<A>) -> Self {
        Self {
            http,
            base: settings.graph_api(""),
            cloud: settings.cloud,
            canonical: with_segments(settings.cloud.graph_url(), &["v1.0", ""]),
            auth,
            roles: Mutex::new(None),
            root: Mutex::new(None),
            drafts: DraftSession::new(),
        }
    }

    /// The URL of `segments` below the Graph root. Segments are escaped, so
    /// IDs containing `/` or `+` are safe.
    fn url(&self, segments: &[&str]) -> Url {
        with_segments(self.base.clone(), segments)
    }

    /// Sends `request` for `about` (the object it concerns, named in a
    /// "not found" error).
    async fn send(&self, request: RequestBuilder, about: About<'_>) -> Result<reqwest::Response> {
        self.send_with(request, about, |e| e, REQUEST_TIMEOUT)
            .await
            .map(|(response, _)| response)
    }

    /// Sends a request that hands a message over to be sent. Failing
    /// before the request is made (say, getting a token) leaves the message
    /// unsent, which [`Error::NotSent`] says; only a lost response to the
    /// request itself leaves it in doubt.
    async fn hand_over(&self, request: RequestBuilder, about: About<'_>) -> Result<()> {
        self.send_with(request, about, unsent, REQUEST_TIMEOUT)
            .await
            .map(drop)
    }

    /// Like [`Self::send`], also returning the token the response was
    /// obtained with, and passing errors from before the request is made
    /// (or after Graph refused it) through `before`. Each attempt, response
    /// body included, may take `timeout`.
    async fn send_with(
        &self,
        request: RequestBuilder,
        about: About<'_>,
        before: fn(Error) -> Error,
        timeout: Duration,
    ) -> Result<(reqwest::Response, AccessToken)> {
        // Kept for a retry; JSON bodies can always be cloned.
        let mut retry = request.try_clone();
        let mut request = Some(request);
        // Graph may reject a token that hadn't expired (it may have been
        // revoked): `with_fresh_token` gets a new one and tries once more.
        let (response, token) = with_fresh_token(
            &*self.auth,
            before,
            retry.is_some(),
            |response: &reqwest::Response| response.status() == StatusCode::UNAUTHORIZED,
            |token| {
                let request = request.take().or_else(|| retry.take());
                async move {
                    let request = request.expect("a request to make");
                    Self::attempt(request, &token, timeout).await
                }
            },
        )
        .await?;
        Ok((Self::checked(response, about).await?, token))
    }

    async fn attempt(
        request: RequestBuilder,
        token: &AccessToken,
        timeout: Duration,
    ) -> Result<reqwest::Response> {
        request
            .bearer_auth(token.secret())
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))
    }

    async fn checked(response: reqwest::Response, about: About<'_>) -> Result<reqwest::Response> {
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(error_from(response, about).await)
        }
    }

    /// GETs `url`, for `about`.
    async fn get<T: DeserializeOwned>(&self, url: Url, about: About<'_>) -> Result<T> {
        self.json(self.request(Method::GET, url), about).await
    }

    async fn json<T: DeserializeOwned>(
        &self,
        request: RequestBuilder,
        about: About<'_>,
    ) -> Result<T> {
        self.json_with(request, about, |e| e).await
    }

    /// Like [`Self::json`], passing errors from before the request is made
    /// through `before` (see [`Self::send_with`]).
    async fn json_with<T: DeserializeOwned>(
        &self,
        request: RequestBuilder,
        about: About<'_>,
        before: fn(Error) -> Error,
    ) -> Result<T> {
        decode(
            self.send_with(request, about, before, REQUEST_TIMEOUT)
                .await?
                .0,
        )
        .await
    }

    fn request(&self, method: Method, url: Url) -> RequestBuilder {
        self.http.request(method, url)
    }
}

impl<A: Authenticator> MailStore for GraphMailStore<A> {
    async fn own_addresses(&self) -> Result<Vec<String>> {
        let mut url = self.url(&["me"]);
        url.query_pairs_mut()
            .append_pair("$select", "mail,userPrincipalName,proxyAddresses");
        let me: Me = self.get(url, About::MAILBOX).await?;
        // `proxyAddresses` holds every alias, as `SMTP:` (the primary one)
        // or `smtp:`; other kinds (`x500:`, `sip:`) aren't mail addresses.
        let aliases = me.proxy_addresses.into_iter().filter_map(|proxy| {
            let (kind, address) = proxy.split_once(':')?;
            kind.eq_ignore_ascii_case("smtp")
                .then(|| address.to_owned())
        });
        Ok(me
            .mail
            .into_iter()
            .chain(me.user_principal_name)
            .chain(aliases)
            .collect())
    }

    async fn search_people(&self, text: &str) -> Result<Vec<Address>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Person {
            display_name: Option<String>,
            #[serde(default)]
            scored_email_addresses: Vec<Scored>,
        }
        #[derive(Deserialize)]
        struct Scored {
            address: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct User {
            display_name: Option<String>,
            mail: Option<String>,
        }
        // Graph's `$search` takes the text in double quotes.
        let quoted = format!("\"{}\"", text.replace(['"', '\\'], " "));
        let mut found: Vec<Address> = Vec::new();
        let mut add = |name: Option<String>, email: Option<String>| {
            let Some(email) = email.filter(|e| e.contains('@')) else {
                return;
            };
            if found.iter().any(|f| f.email.eq_ignore_ascii_case(&email)) {
                return;
            }
            let address = Address::new(email);
            found.push(match name.filter(|n| !n.is_empty()) {
                Some(name) => address.with_name(name),
                None => address,
            });
        };
        // The people the user works with (People.Read).
        let mut url = self.url(&["me", "people"]);
        url.query_pairs_mut()
            .append_pair("$search", &quoted)
            .append_pair("$top", "20")
            .append_pair("$select", "displayName,scoredEmailAddresses");
        let people = self.get::<Collection<Person>>(url, About::MAILBOX);
        // The whole directory (User.ReadBasic.All).
        let mut url = self.url(&["users"]);
        let term = text.replace(['"', '\\'], " ");
        url.query_pairs_mut()
            .append_pair(
                "$search",
                &format!("\"displayName:{term}\" OR \"mail:{term}\""),
            )
            .append_pair("$top", "20")
            .append_pair("$select", "displayName,mail");
        let request = self
            .request(Method::GET, url)
            .header("ConsistencyLevel", "eventual");
        let users = self.json::<Collection<User>>(request, About::MAILBOX);
        // Either search may be refused (the app may not have its
        // permission): the other's results are what there is.
        let refused = |e: &Error| matches!(e, Error::Auth(_));
        // Both at once; their results are taken in turn, as if one came
        // after the other: the people's failure (but a refusal) is the
        // answer as soon as it is known.
        let people = async {
            match people.await {
                Err(e) if !refused(&e) => Err(e),
                people => Ok(people),
            }
        };
        let (people, users) = first_then(people, users).await?;
        let people_refused = match people {
            Ok(people) => {
                for person in people.value {
                    let email = person
                        .scored_email_addresses
                        .into_iter()
                        .find_map(|s| s.address);
                    add(person.display_name, email);
                }
                None
            }
            Err(e) if refused(&e) => Some(e),
            Err(e) => return Err(e),
        };
        match users {
            Ok(users) => {
                for user in users.value {
                    add(user.display_name, user.mail);
                }
            }
            // Both refused: say why.
            Err(e) if refused(&e) => {
                if let Some(e) = people_refused {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
        Ok(found)
    }

    async fn list_folders(&self) -> Result<Vec<Folder>> {
        // `/me/mailFolders` lists only top-level folders; walk down through
        // `childFolders` for the rest, a level at a time, a few listings at
        // once. The roles are looked up meanwhile; failing, they are what
        // is reported, as if looked up first.
        let (roles, top) = first_then(self.roles(), self.subfolders(None)).await?;
        let mut listings: Listings = HashMap::from([(None, Ok(top?))]);
        let mut level = with_children(listings.get(&None));
        let mut failed = false;
        while !level.is_empty() {
            // The error a walk one listing at a time would stop at comes
            // back once it is known that none comes before it, without
            // waiting for the other listings.
            let listed = join_bounded_until(
                level
                    .iter()
                    .map(|id| self.subfolders(Some(id.as_str())))
                    .collect(),
                LISTINGS_AT_ONCE,
                |listed| {
                    failed |= listed.iter().flatten().any(Result::is_err);
                    failed
                        && matches!(
                            walk(|parent| match parent {
                                Some(id) => match level.iter().position(|l| l == id) {
                                    Some(at) => listed[at].as_ref(),
                                    None => listings.get(parent),
                                },
                                None => listings.get(parent),
                            }),
                            Walk::Failed(_)
                        )
                },
            )
            .await;
            let mut next = Vec::new();
            for (id, listing) in level.iter().zip(listed) {
                let Some(listing) = listing else {
                    continue;
                };
                let key = Some(id.clone());
                listings.insert(key.clone(), listing);
                next.extend(with_children(listings.get(&key)));
            }
            level = next;
            if failed && let Walk::Failed(e) = walk(|parent| listings.get(parent)) {
                return Err(e.clone());
            }
        }
        let order = match walk(|parent| listings.get(parent)) {
            Walk::Listed(order) => order,
            Walk::Failed(e) => return Err(e.clone()),
            Walk::Unlisted => unreachable!("the folders below every one listed were listed"),
        };
        let mut folders = Vec::new();
        for parent in order {
            if let Some(Ok(listing)) = listings.remove(&parent) {
                folders.extend(listing.into_iter().map(|f| roles.folder(f)));
            }
        }
        Ok(folders)
    }

    async fn child_folders(&self, parent: Option<&FolderId>) -> Result<Vec<Folder>> {
        // The roles label the folders as in a listing of them all: those
        // the last such listing found, if recent, otherwise looked up.
        let id = parent.map(FolderId::as_str);
        let (roles, listing) = match self.known_roles() {
            Some(roles) => (roles, self.subfolders(id).await),
            None => first_then(self.roles(), self.subfolders(id)).await?,
        };
        Ok(listing?.into_iter().map(|f| roles.folder(f)).collect())
    }

    async fn folder(&self, id: &FolderId) -> Result<Folder> {
        // The roles, if a folder listing looked them up, tell a well-known
        // folder; only the root is needed otherwise, to tell a top-level
        // folder.
        let cached = self.roles.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let roles = match cached {
            Some((roles, _)) => roles,
            None => Arc::new(Roles {
                root: self.root().await?,
                ..Default::default()
            }),
        };
        let url = self.url(&["me", "mailFolders", id.as_str()]);
        let folder: GraphFolder = self.get(url, About::folder(id.as_str())).await?;
        Ok(roles.folder(folder))
    }

    async fn recent_unread(
        &self,
        folder: &FolderId,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<(MessageId, DateTime<Utc>)>> {
        // A message moved in keeps when it was received; when it was
        // last modified tells.
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Modified {
            id: String,
            last_modified_date_time: String,
        }
        let after = since.map_or_else(
            || "ge 1900-01-01T00:00:00Z".to_owned(),
            // Mail that changed when `since` did may not have been seen:
            // which was is told by its ID.
            |since| format!("ge {}", since.to_rfc3339_opts(SecondsFormat::Secs, true)),
        );
        // Without `since`, the last few tell; with it, all of them, up to a
        // limit, which one page holds.
        let (top, wanted) = match since {
            Some(_) => (RECENT_UNREAD, RECENT_UNREAD),
            None => (50, 10),
        };
        let mut url = self.url(&["me", "mailFolders", folder.as_str(), "messages"]);
        url.query_pairs_mut()
            .append_pair("$top", &top.to_string())
            .append_pair("$select", "id,lastModifiedDateTime")
            // Graph wants the sorted property filtered too, first.
            .append_pair(
                "$filter",
                &format!("lastModifiedDateTime {after} and isRead eq false"),
            )
            .append_pair("$orderby", "lastModifiedDateTime desc");
        let mut recent = Vec::new();
        loop {
            let page: Collection<Modified> = self.get(url, About::folder(folder.as_str())).await?;
            for m in page.value {
                let at = DateTime::parse_from_rfc3339(&m.last_modified_date_time)
                    .map_err(|e| Error::Backend(format!("invalid lastModifiedDateTime: {e}")))?;
                recent.push((MessageId::new(m.id), at.with_timezone(&Utc)));
            }
            match page.next_link {
                Some(next) if recent.len() < wanted => url = self.next_page_url(&next)?,
                _ => break,
            }
        }
        recent.truncate(RECENT_UNREAD);
        Ok(recent)
    }

    async fn well_known_folder(&self, which: WellKnownFolder) -> Result<Folder> {
        // Only the root is needed here, so other well-known folders failing
        // to resolve can't get in the way.
        let roles = Roles {
            root: self.root().await?,
            ..Default::default()
        };
        let url = self.url(&["me", "mailFolders", which.as_str()]);
        let folder: GraphFolder = self.get(url, About::folder(which.as_str())).await?;
        Ok(Folder {
            well_known: Some(which),
            ..roles.folder(folder)
        })
    }

    async fn list_messages(&self, folder: &FolderId, page: Page) -> Result<MessagePage> {
        // Sort by the date the summaries show (see `split`). Graph can't
        // sort by "this field, else that one", so the date is the sent
        // date, which received and sent mail both have; only unsent drafts
        // fall back to their creation date.
        self.messages_page(folder, page, None, "sentDateTime desc,createdDateTime desc")
            .await
    }

    async fn changed_messages(
        &self,
        folder: &FolderId,
        since: DateTime<Utc>,
        page: Page,
    ) -> Result<MessagePage> {
        // Graph wants the sorted property filtered too, first.
        let since = since.to_rfc3339_opts(SecondsFormat::Secs, true);
        let filter = format!("lastModifiedDateTime ge {since}");
        self.messages_page(folder, page, Some(&filter), "lastModifiedDateTime desc")
            .await
    }

    async fn fetch_message(&self, id: &MessageId) -> Result<Message> {
        let mut url = self.url(&["me", "messages", id.as_str()]);
        url.query_pairs_mut().append_pair(
            "$select",
            &format!(
                "{SUMMARY_FIELDS},ccRecipients,bccRecipients,replyTo,internetMessageId,body,\
                 {MEETING_FIELDS}"
            ),
        );
        url.query_pairs_mut().append_pair("$expand", EVENT_EXPAND);
        let request = self
            .request(Method::GET, url)
            // Graph converts HTML bodies to text, better than mett could.
            .header(
                "Prefer",
                format!("outlook.body-content-type=\"text\", {PREFER_UTC}"),
            );
        let message: GraphMessage = self.json(request, About::message(id.as_str())).await?;
        message.into_message()
    }

    async fn message_folder(&self, id: &MessageId) -> Result<FolderId> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Located {
            parent_folder_id: String,
        }
        let mut url = self.url(&["me", "messages", id.as_str()]);
        url.query_pairs_mut()
            .append_pair("$select", "id,parentFolderId");
        let located: Located = self.get(url, About::message(id.as_str())).await?;
        Ok(FolderId::new(located.parent_folder_id))
    }

    async fn update_flags(&self, id: &MessageId, update: FlagsUpdate) -> Result<()> {
        let mut patch = serde_json::Map::new();
        if let Some(read) = update.read {
            patch.insert("isRead".into(), json!(read));
        }
        if let Some(flagged) = update.flagged {
            let status = if flagged { "flagged" } else { "notFlagged" };
            patch.insert("flag".into(), json!({ "flagStatus": status }));
        }
        if patch.is_empty() {
            return Ok(());
        }
        let url = self.url(&["me", "messages", id.as_str()]);
        self.send(
            self.request(Method::PATCH, url).json(&patch),
            About::message(id.as_str()),
        )
        .await
        .map(drop)
    }

    async fn move_message(&self, id: &MessageId, to: &FolderId) -> Result<MessageId> {
        self.relocate(id, to, "move").await
    }

    async fn copy_message(&self, id: &MessageId, to: &FolderId) -> Result<MessageId> {
        self.relocate(id, to, "copy").await
    }

    async fn delete_message(&self, id: &MessageId) -> Result<()> {
        // DELETE would only move it to Deleted Items; the port promises a
        // permanent delete, which Graph offers only in the global cloud.
        if self.cloud != Cloud::Global {
            return Err(Error::Backend(format!(
                "permanently deleting messages isn't available in the {} cloud; \
                 move them to Deleted Items instead",
                self.cloud
            )));
        }
        let url = self.url(&["me", "messages", id.as_str(), "permanentDelete"]);
        self.send(self.request(Method::POST, url), About::message(id.as_str()))
            .await
            .map(drop)
    }

    async fn answer_invitation(
        &self,
        id: &MessageId,
        invitation: &Invitation,
        answer: &Answer,
    ) -> Result<()> {
        self.answer_event(id, invitation, answer)
            .await
            .map_err(|e| match e {
                // Without the calendar permission, Graph refuses; signing
                // in again wouldn't help.
                Error::Auth(detail)
                    if ["denied", "forbidden"]
                        .iter()
                        .any(|word| detail.to_lowercase().contains(word)) =>
                {
                    Error::Backend(format!(
                        "{detail}: answering invitations needs the Calendars.ReadWrite \
                         permission; add it to msgraph_scopes and sign in again"
                    ))
                }
                e => e,
            })
    }

    fn capabilities(&self) -> mett_core::Capabilities {
        mett_core::Capabilities {
            can_sync: true,
            ..mett_core::Capabilities::default()
        }
    }

    /// With Graph's delta query: the state is the link to go on from (a
    /// page's `@odata.nextLink`, or the last one's `@odata.deltaLink`).
    async fn sync_folder(
        &self,
        folder: &FolderId,
        state: Option<&SyncState>,
    ) -> Result<FolderChanges> {
        let url = match state {
            // The state records its folder, so it can't be used for
            // another one by mistake.
            Some(state) => {
                let link = state
                    .as_str()
                    .strip_prefix(folder.as_str())
                    .and_then(|rest| rest.strip_prefix(' '))
                    .ok_or(Error::StaleSync)?;
                // A link of another endpoint (msgraph_endpoint changed) is
                // never followed: the folder syncs again from the start.
                self.next_page_url(link).map_err(|_| Error::StaleSync)?
            }
            None => {
                let mut url =
                    self.url(&["me", "mailFolders", folder.as_str(), "messages", "delta"]);
                url.query_pairs_mut().append_pair("$select", SUMMARY_FIELDS);
                url
            }
        };
        let request = self
            .request(Method::GET, url)
            .header("Prefer", format!("odata.maxpagesize={DELTA_PAGE_SIZE}"));
        let page: Delta = self.json(request, About::folder(folder.as_str())).await?;
        let mut changes = FolderChanges {
            changed: Vec::new(),
            removed: Vec::new(),
            state: SyncState::new(String::new()),
            more: page.next_link.is_some(),
        };
        // A message may come more than once: its last entry tells. Each
        // message's outcome (its summary, or none if removed) is kept by ID, with the position of the entry
        // that decided it (a repeated removal keeps the first), and
        // handed out in that order.
        let mut outcomes: HashMap<MessageId, (usize, Option<MessageSummary>)> = HashMap::new();
        for (position, mut entry) in page.value.into_iter().enumerate() {
            if entry.get("@removed").is_some() {
                let id = entry
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| Error::Backend("delta entry without an id".into()))?;
                let id = MessageId::new(id);
                outcomes
                    .entry(id)
                    .and_modify(|(at, outcome)| {
                        if outcome.is_some() {
                            *at = position;
                            *outcome = None;
                        }
                    })
                    .or_insert((position, None));
                continue;
            }
            // Delta entries are in the folder synced, whether or not they
            // say so.
            if let Some(fields) = entry.as_object_mut() {
                fields
                    .entry("parentFolderId")
                    .or_insert_with(|| serde_json::Value::from(folder.as_str()));
            }
            let message: GraphMessage = serde_json::from_value(entry)
                .map_err(|e| Error::Backend(format!("unexpected delta entry from Graph: {e}")))?;
            let summary = message.into_summary()?;
            outcomes.insert(summary.id.clone(), (position, Some(summary)));
        }
        let mut outcomes: Vec<_> = outcomes.into_iter().collect();
        outcomes.sort_unstable_by_key(|(_, (position, _))| *position);
        for (id, (_, outcome)) in outcomes {
            match outcome {
                Some(summary) => changes.changed.push(summary),
                None => changes.removed.push(id),
            }
        }
        let link = page
            .next_link
            .or(page.delta_link)
            .ok_or_else(|| Error::Backend("delta page without a next or delta link".into()))?;
        changes.state = SyncState::new(format!("{folder} {link}"));
        Ok(changes)
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
        let url = self.url(&["me", "messages"]);
        let created: IdOnly = self
            .json(
                self.request(Method::POST, url)
                    .json(&message_json(draft, true)),
                About::MAILBOX,
            )
            .await?;
        Ok(MessageId::new(created.id))
    }

    async fn list_attachments(&self, message: &MessageId) -> Result<Vec<Attachment>> {
        let mut url = self.url(&["me", "messages", message.as_str(), "attachments"]);
        url.query_pairs_mut()
            .append_pair("$select", "id,name,contentType,size,isInline")
            // What an attached item is (a message, an event, a contact),
            // when Graph gives no content type for it.
            .append_pair("$expand", "microsoft.graph.itemAttachment/item($select=id)");
        let mut attachments = Vec::new();
        loop {
            let page: Collection<GraphAttachment> =
                self.get(url, About::message(message.as_str())).await?;
            attachments.extend(page.value.into_iter().map(GraphAttachment::into_attachment));
            match page.next_link {
                Some(next) => url = self.next_page_url(&next)?,
                None => return Ok(attachments),
            }
        }
    }

    async fn fetch_attachment(
        &self,
        message: &MessageId,
        attachment: &AttachmentId,
    ) -> Result<Vec<u8>> {
        // A file's bytes, or an attached message as MIME.
        let url = self.url(&[
            "me",
            "messages",
            message.as_str(),
            "attachments",
            attachment.as_str(),
            "$value",
        ]);
        // The content may be large, and how long it takes isn't known.
        let (response, _) = self
            .send_with(
                self.request(Method::GET, url),
                About::message(message.as_str()),
                |e| e,
                DOWNLOAD_TIMEOUT,
            )
            .await?;
        response
            .bytes()
            .await
            .map(Vec::from)
            .map_err(|e| Error::Network(e.to_string()))
    }
}

impl<A: Authenticator> GraphMailStore<A> {
    /// Moves or copies (`action`) message `id` to folder `to`, returning
    /// the ID it has there.
    async fn relocate(&self, id: &MessageId, to: &FolderId, action: &str) -> Result<MessageId> {
        let url = self.url(&["me", "messages", id.as_str(), action]);
        let body = MoveRequest {
            destination_id: to.as_str(),
        };
        let relocated: IdOnly = self
            .json(
                self.request(Method::POST, url).json(&body),
                About::message(id.as_str()),
            )
            .await?;
        Ok(MessageId::new(relocated.id))
    }

    /// A page of a folder's message summaries, sorted by `order` and, on
    /// the first page, filtered by `filter`.
    async fn messages_page(
        &self,
        folder: &FolderId,
        page: Page,
        filter: Option<&str>,
        order: &str,
    ) -> Result<MessagePage> {
        let url = match page.cursor() {
            // The cursor is Graph's `@odata.nextLink`, which already holds
            // the query; only the page size may change.
            Some(cursor) => {
                let link = cursor
                    .as_str()
                    .strip_prefix(folder.as_str())
                    .and_then(|rest| rest.strip_prefix(' '))
                    .ok_or_else(|| {
                        Error::Backend(format!("page cursor is not for folder {folder}"))
                    })?;
                // Graph's continuation links must be replayed unchanged, so
                // the page keeps the size the listing started with.
                self.next_page_url(link)?
            }
            None => {
                let mut url = self.url(&["me", "mailFolders", folder.as_str(), "messages"]);
                {
                    let mut query = url.query_pairs_mut();
                    query
                        .append_pair("$top", &top_for(&page))
                        .append_pair("$select", SUMMARY_FIELDS);
                    if let Some(filter) = filter {
                        query.append_pair("$filter", filter);
                    }
                    query.append_pair("$orderby", order);
                }
                url
            }
        };
        let page: Collection<GraphMessage> = self.get(url, About::folder(folder.as_str())).await?;
        Ok(MessagePage {
            messages: page
                .value
                .into_iter()
                .map(GraphMessage::into_summary)
                .collect::<Result<_>>()?,
            // The cursor records its folder, so it can't be used for
            // another one by mistake.
            next: page
                .next_link
                .map(|link| PageCursor::new(format!("{folder} {link}"))),
        })
    }
}

impl<A: Authenticator> MailSender for GraphMailStore<A> {
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
            let url = self.url(&["me", "sendMail"]);
            let body = send_mail_json(draft);
            return self
                .hand_over(self.request(Method::POST, url).json(&body), About::MAILBOX)
                .await;
        }
        // Replies, forwards and messages with large attachments go through
        // a server draft, so that Graph threads them under the original,
        // keeps a forwarded message's attachments, and takes large files in
        // pieces. Sending a draft always keeps a copy in Sent Items.
        // Failing before the send request leaves nothing sent, whatever
        // became of the draft.
        let (id, base) = self
            .drafts
            .server_draft(self, draft)
            .await
            .map_err(unsent)?;
        let url = self.url(&["me", "messages", id.as_str(), "send"]);
        let sent = self
            .hand_over(self.request(Method::POST, url), About::message(id.as_str()))
            .await;
        self.drafts
            .after_send(self, draft, id, base, sent)
            .await
            .map(drop)
    }
}

impl<A> GraphMailStore<A> {
    /// Turns a `@odata.nextLink` into a URL on the configured endpoint.
    ///
    /// Only links below this endpoint's v1.0 root, or below the cloud's own
    /// Graph root (which Graph uses even behind a proxy), are accepted, and
    /// the latter are routed through the configured endpoint. The access
    /// token is never sent anywhere else.
    fn next_page_url(&self, link: &str) -> Result<Url> {
        let rest = [&self.base, &self.canonical]
            .iter()
            .find_map(|root| link.strip_prefix(root.as_str()))
            .ok_or_else(|| {
                let host = Url::parse(link)
                    .map(|u| u.origin().ascii_serialization())
                    .unwrap_or_else(|_| link.to_owned());
                Error::Backend(format!(
                    "refusing to follow a page link to another host: {host}"
                ))
            })?;
        let url = self
            .base
            .join(rest)
            .map_err(|e| Error::Backend(format!("invalid page link from Graph: {e}")))?;
        // `rest` could itself be scheme-relative (`//elsewhere/...`); make
        // sure the result really is below this endpoint.
        if url.origin() != self.base.origin() || !url.path().starts_with(self.base.path()) {
            return Err(Error::Backend(format!(
                "refusing to follow a page link outside the Graph endpoint: {}",
                url.origin().ascii_serialization()
            )));
        }
        Ok(url)
    }
}

/// `$top` for `page`: its size, within Graph's limit. A larger request
/// still gets a cursor for the rest.
fn top_for(page: &Page) -> String {
    page.size().min(MAX_PAGE_SIZE).to_string()
}

/// The object a request concerns.
#[derive(Clone, Copy)]
struct About<'a> {
    kind: &'static str,
    id: &'a str,
}

impl<'a> About<'a> {
    const MAILBOX: About<'static> = About {
        kind: "mailbox",
        id: "me",
    };

    fn folder(id: &'a str) -> Self {
        Self { kind: "folder", id }
    }

    fn message(id: &'a str) -> Self {
        Self {
            kind: "message",
            id,
        }
    }
}

/// The JSON body of a successful Graph response.
async fn decode<T: DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    let body = response
        .bytes()
        .await
        .map_err(|e| Error::Network(e.to_string()))?;
    serde_json::from_slice(&body)
        .map_err(|e| Error::Backend(format!("unexpected response from Graph: {e}")))
}

/// Maps an unsuccessful Graph response to a port error.
async fn error_from(response: reqwest::Response, about: About<'_>) -> Error {
    let status = response.status();
    let retry_after = retry_after(
        response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
    );
    let body = response.bytes().await.unwrap_or_default();
    let detail = serde_json::from_slice::<ErrorResponse>(&body)
        .map(|e| format!("{}: {}", e.error.code, e.error.message))
        .unwrap_or_else(|_| format!("HTTP {status}"));
    match status {
        // A delta link Graph no longer takes.
        StatusCode::GONE => Error::StaleSync,
        status => error_for_status(
            status.as_u16(),
            retry_after,
            detail,
            Some(Error::not_found(about.kind, about.id)),
        ),
    }
}

/// How many changes Graph is asked for per delta page.
const DELTA_PAGE_SIZE: u32 = 500;
