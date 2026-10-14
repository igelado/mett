//! [`GraphMailStore`]: the [`MailStore`] port for Microsoft Graph.
//!
//! It works on the signed-in user's mailbox (`/me`), using the Graph REST
//! API v1.0. Every request asks the [`Authenticator`] for a token, so
//! refreshing and signing in happen transparently.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use mett_core::{
    Address, Authenticator, Body, BodyKind, Error, Flags, FlagsUpdate, Folder, FolderId, MailStore,
    Message, MessageId, MessagePage, MessageSummary, Page, PageCursor, Result, WellKnownFolder,
};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;

use crate::settings::AuthSettings;

/// Upper bound on each Graph request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The message fields the index needs.
const SUMMARY_FIELDS: &str = "id,parentFolderId,subject,from,toRecipients,sentDateTime,createdDateTime,isDraft,isRead,flag,hasAttachments";

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
    auth: Arc<A>,
    /// The mailbox's folder roles, once looked up.
    roles: Mutex<Option<Arc<Roles>>>,
    /// The hidden root folder's ID, once looked up on its own.
    root: Mutex<Option<Option<FolderId>>>,
}

/// Which folders play special roles. Folder IDs don't change, so this is
/// looked up once.
#[derive(Debug, Default)]
struct Roles {
    /// The hidden root that top-level folders hang from.
    root: Option<FolderId>,
    /// Well-known folders by ID.
    well_known: HashMap<FolderId, WellKnownFolder>,
}

impl Roles {
    fn folder(&self, folder: GraphFolder) -> Folder {
        let id = FolderId::new(folder.id);
        Folder {
            well_known: self.well_known.get(&id).copied(),
            // Top-level folders hang from the hidden root; the port says
            // they have no parent.
            parent: folder
                .parent_folder_id
                .map(FolderId::new)
                .filter(|p| Some(p) != self.root.as_ref()),
            id,
            name: folder.display_name,
            unread_count: folder.unread_item_count,
            total_count: folder.total_item_count,
        }
    }
}

impl<A: Authenticator> GraphMailStore<A> {
    /// A store for the account in `settings`, using its cloud's Graph
    /// endpoint and `auth` for tokens.
    pub fn new(http: reqwest::Client, settings: &AuthSettings, auth: Arc<A>) -> Self {
        Self {
            http,
            base: settings.graph_api(""),
            canonical: {
                let mut url = settings.cloud.graph_url();
                url.path_segments_mut()
                    .expect("Graph URL can be a base")
                    .pop_if_empty()
                    .extend(["v1.0", ""]);
                url
            },
            auth,
            roles: Mutex::new(None),
            root: Mutex::new(None),
        }
    }

    /// The URL of `segments` below the Graph root. Segments are escaped, so
    /// IDs containing `/` or `+` are safe.
    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("Graph URL can be a base")
            .pop_if_empty()
            .extend(segments);
        url
    }

    /// Sends `request` for `about` (the object it concerns, named in a
    /// "not found" error).
    async fn send(&self, request: RequestBuilder, about: About<'_>) -> Result<reqwest::Response> {
        let token = self.auth.access_token().await?;
        let response = request
            .bearer_auth(token.secret())
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(error_from(response, about).await)
        }
    }

    async fn json<T: DeserializeOwned>(
        &self,
        request: RequestBuilder,
        about: About<'_>,
    ) -> Result<T> {
        let body = self
            .send(request, about)
            .await?
            .bytes()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;
        serde_json::from_slice(&body)
            .map_err(|e| Error::Backend(format!("unexpected response from Graph: {e}")))
    }

    fn request(&self, method: Method, url: Url) -> RequestBuilder {
        self.http.request(method, url)
    }

    /// The hidden root folder's ID: from the roles if they are known,
    /// otherwise looked up on its own (and cached).
    async fn root(&self) -> Result<Option<FolderId>> {
        if let Some(roles) = self
            .roles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return Ok(roles.root.clone());
        }
        if let Some(root) = self.root.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Ok(root);
        }
        let mut url = self.url(&["me", "mailFolders", ROOT]);
        url.query_pairs_mut().append_pair("$select", "id");
        let root: IdOnly = self
            .json(self.request(Method::GET, url), About::folder(ROOT))
            .await?;
        let root = Some(FolderId::new(root.id));
        *self.root.lock().unwrap_or_else(|e| e.into_inner()) = Some(root.clone());
        Ok(root)
    }

    /// The mailbox's folder roles, looked up on first use with a single
    /// `$batch` request for the root and every well-known folder.
    async fn roles(&self) -> Result<Arc<Roles>> {
        if let Some(roles) = self.roles.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Ok(roles);
        }
        let names: Vec<&str> = std::iter::once(ROOT)
            .chain(WellKnownFolder::ALL.iter().map(|w| w.as_str()))
            .collect();
        let requests: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                json!({
                    "id": i.to_string(),
                    "method": "GET",
                    "url": format!("/me/mailFolders/{name}?$select=id"),
                })
            })
            .collect();
        let batch: BatchResponse = self
            .json(
                self.request(Method::POST, self.url(&["$batch"]))
                    .json(&json!({ "requests": requests })),
                About::MAILBOX,
            )
            .await?;

        let mut roles = Roles::default();
        for response in batch.responses {
            // A mailbox may lack some well-known folders (404): skip them.
            // Any other failure is reported rather than cached, so a
            // passing problem can't leave the roles wrong for good.
            match response.status {
                200 => {}
                404 => continue,
                429 => {
                    return Err(Error::Throttled {
                        retry_after: response.retry_after(),
                    });
                }
                401 | 403 => {
                    return Err(Error::Auth(format!(
                        "Graph refused a folder lookup: HTTP {}",
                        response.status
                    )));
                }
                s if s >= 500 => {
                    return Err(Error::Network(format!(
                        "Graph failed to look up a folder: HTTP {s}"
                    )));
                }
                s => {
                    return Err(Error::Backend(format!(
                        "Graph refused to look up a folder: HTTP {s}"
                    )));
                }
            }
            let Ok(index) = response.id.parse::<usize>() else {
                continue;
            };
            let Some(id) = response.body.and_then(|b| b.id).map(FolderId::new) else {
                continue;
            };
            match index.checked_sub(1) {
                None => roles.root = Some(id),
                Some(i) => {
                    if let Some(which) = WellKnownFolder::ALL.get(i) {
                        roles.well_known.insert(id, *which);
                    }
                }
            }
        }
        let roles = Arc::new(roles);
        *self.roles.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&roles));
        Ok(roles)
    }
}

/// Graph's well-known name for the hidden root of the folder tree.
const ROOT: &str = "msgfolderroot";

impl<A: Authenticator> MailStore for GraphMailStore<A> {
    async fn list_folders(&self) -> Result<Vec<Folder>> {
        let roles = self.roles().await?;
        let top = |mut url: Url| {
            url.query_pairs_mut()
                .append_pair("$top", "100")
                .append_pair("includeHiddenFolders", "true");
            url
        };
        // `/me/mailFolders` lists only top-level folders; walk down through
        // `childFolders` for the rest.
        let mut pending = vec![(top(self.url(&["me", "mailFolders"])), None)];
        let mut folders = Vec::new();
        while let Some((mut url, parent)) = pending.pop() {
            loop {
                let about = parent.as_deref().map_or(About::MAILBOX, About::folder);
                let page: Collection<GraphFolder> =
                    self.json(self.request(Method::GET, url), about).await?;
                for folder in page.value {
                    if folder.child_folder_count > 0 {
                        let children =
                            top(self.url(&["me", "mailFolders", &folder.id, "childFolders"]));
                        pending.push((children, Some(folder.id.clone())));
                    }
                    folders.push(roles.folder(folder));
                }
                match page.next_link {
                    Some(next) => url = self.next_page_url(&next)?,
                    None => break,
                }
            }
        }
        Ok(folders)
    }

    async fn well_known_folder(&self, which: WellKnownFolder) -> Result<Folder> {
        // Only the root is needed here, so other well-known folders failing
        // to resolve can't get in the way.
        let roles = Roles {
            root: self.root().await?,
            well_known: HashMap::new(),
        };
        let url = self.url(&["me", "mailFolders", which.as_str()]);
        let folder: GraphFolder = self
            .json(
                self.request(Method::GET, url),
                About::folder(which.as_str()),
            )
            .await?;
        Ok(Folder {
            well_known: Some(which),
            ..roles.folder(folder)
        })
    }

    async fn list_messages(&self, folder: &FolderId, page: Page) -> Result<MessagePage> {
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
                url.query_pairs_mut()
                    .append_pair("$top", &top_for(&page))
                    .append_pair("$select", SUMMARY_FIELDS)
                    // Sort by the date the summaries show (see `split`).
                    // Graph can't sort by "this field, else that one", so
                    // the date is the sent date, which received and sent
                    // mail both have; only unsent drafts fall back to
                    // their creation date.
                    .append_pair("$orderby", "sentDateTime desc,createdDateTime desc");
                url
            }
        };
        let page: Collection<GraphMessage> = self
            .json(
                self.request(Method::GET, url),
                About::folder(folder.as_str()),
            )
            .await?;
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

    async fn fetch_message(&self, id: &MessageId) -> Result<Message> {
        let mut url = self.url(&["me", "messages", id.as_str()]);
        url.query_pairs_mut().append_pair(
            "$select",
            &format!("{SUMMARY_FIELDS},ccRecipients,internetMessageId,body"),
        );
        let request = self
            .request(Method::GET, url)
            // Graph converts HTML bodies to text, better than mett could.
            .header("Prefer", "outlook.body-content-type=\"text\"");
        let message: GraphMessage = self.json(request, About::message(id.as_str())).await?;
        message.into_message()
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
        let url = self.url(&["me", "messages", id.as_str(), "move"]);
        let body = MoveRequest {
            destination_id: to.as_str(),
        };
        let moved: IdOnly = self
            .json(
                self.request(Method::POST, url).json(&body),
                About::message(id.as_str()),
            )
            .await?;
        Ok(MessageId::new(moved.id))
    }

    async fn delete_message(&self, id: &MessageId) -> Result<()> {
        // DELETE would only move it to Deleted Items; the port promises a
        // permanent delete.
        let url = self.url(&["me", "messages", id.as_str(), "permanentDelete"]);
        self.send(self.request(Method::POST, url), About::message(id.as_str()))
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

/// The most messages Graph returns per request.
const MAX_PAGE_SIZE: u32 = 1000;

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

/// Maps an unsuccessful Graph response to a port error.
async fn error_from(response: reqwest::Response, about: About<'_>) -> Error {
    let status = response.status();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let body = response.bytes().await.unwrap_or_default();
    let detail = serde_json::from_slice::<ErrorResponse>(&body)
        .map(|e| format!("{}: {}", e.error.code, e.error.message))
        .unwrap_or_else(|_| format!("HTTP {status}"));
    match status {
        StatusCode::TOO_MANY_REQUESTS => Error::Throttled { retry_after },
        StatusCode::NOT_FOUND => Error::NotFound {
            kind: about.kind,
            id: about.id.to_owned(),
        },
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Error::Auth(detail),
        StatusCode::REQUEST_TIMEOUT => Error::Network(detail),
        s if s.is_server_error() => Error::Network(detail),
        _ => Error::Backend(detail),
    }
}

#[derive(Deserialize)]
struct Collection<T> {
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
}

#[derive(Deserialize)]
struct IdOnly {
    id: String,
}

#[derive(Deserialize)]
struct BatchResponse {
    responses: Vec<BatchItem>,
}

#[derive(Deserialize)]
struct BatchItem {
    id: String,
    status: u16,
    #[serde(default)]
    headers: HashMap<String, String>,
    body: Option<MaybeId>,
}

impl BatchItem {
    /// The item's `Retry-After`, in seconds, if it has one.
    fn retry_after(&self) -> Option<Duration> {
        self.headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
            .and_then(|(_, value)| value.trim().parse().ok())
            .map(Duration::from_secs)
    }
}

#[derive(Deserialize)]
struct MaybeId {
    id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MoveRequest<'a> {
    destination_id: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphFolder {
    id: String,
    display_name: String,
    parent_folder_id: Option<String>,
    #[serde(default)]
    child_folder_count: u32,
    #[serde(default)]
    unread_item_count: u32,
    #[serde(default)]
    total_item_count: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphMessage {
    id: String,
    parent_folder_id: String,
    subject: Option<String>,
    from: Option<Recipient>,
    #[serde(default)]
    to_recipients: Vec<Recipient>,
    #[serde(default)]
    cc_recipients: Vec<Recipient>,
    /// Missing, or a placeholder, for unsent drafts.
    sent_date_time: Option<String>,
    #[serde(default)]
    is_draft: bool,
    created_date_time: Option<String>,
    #[serde(default)]
    is_read: bool,
    flag: Option<FollowupFlag>,
    #[serde(default)]
    has_attachments: bool,
    internet_message_id: Option<String>,
    body: Option<ItemBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Recipient {
    email_address: EmailAddress,
}

#[derive(Deserialize)]
struct EmailAddress {
    name: Option<String>,
    address: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FollowupFlag {
    flag_status: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemBody {
    content_type: String,
    content: String,
}

impl Recipient {
    fn into_address(self) -> Address {
        Address {
            name: self.email_address.name.filter(|n| !n.is_empty()),
            email: self.email_address.address.unwrap_or_default(),
        }
    }
}

impl GraphMessage {
    fn into_summary(self) -> Result<MessageSummary> {
        self.split().map(|(summary, _)| summary)
    }

    fn into_message(self) -> Result<Message> {
        let (summary, rest) = self.split()?;
        Ok(Message {
            summary,
            cc: rest.cc.into_iter().map(Recipient::into_address).collect(),
            internet_message_id: rest.internet_message_id,
            body: rest.body.map_or(
                Body {
                    kind: BodyKind::Text,
                    content: String::new(),
                },
                |b| Body {
                    kind: if b.content_type.eq_ignore_ascii_case("html") {
                        BodyKind::Html
                    } else {
                        BodyKind::Text
                    },
                    content: b.content,
                },
            ),
        })
    }

    fn split(self) -> Result<(MessageSummary, Rest)> {
        // The date is when the message was sent, as in mutt's index, or for
        // an unsent draft, when it was created. It matches the listing's
        // sort order.
        // Graph may give drafts a placeholder sent date (such as year 1),
        // so drafts always use their creation date.
        let sent = self.sent_date_time.filter(|_| !self.is_draft);
        let timestamp = sent
            .or(self.created_date_time)
            .ok_or_else(|| Error::Backend("message has no timestamp".into()))?;
        let received = DateTime::parse_from_rfc3339(&timestamp)
            .map_err(|e| Error::Backend(format!("invalid message timestamp {timestamp:?}: {e}")))?
            .with_timezone(&Utc);
        let summary = MessageSummary {
            id: MessageId::new(self.id),
            folder: FolderId::new(self.parent_folder_id),
            subject: self.subject.unwrap_or_default(),
            from: self.from.map(Recipient::into_address),
            to: self
                .to_recipients
                .into_iter()
                .map(Recipient::into_address)
                .collect(),
            received,
            flags: Flags {
                read: self.is_read,
                flagged: self.flag.is_some_and(|f| f.flag_status == "flagged"),
            },
            has_attachments: self.has_attachments,
        };
        let rest = Rest {
            cc: self.cc_recipients,
            internet_message_id: self.internet_message_id,
            body: self.body,
        };
        Ok((summary, rest))
    }
}

/// The parts of a [`GraphMessage`] that only a full [`Message`] needs.
struct Rest {
    cc: Vec<Recipient>,
    internet_message_id: Option<String>,
    body: Option<ItemBody>,
}
