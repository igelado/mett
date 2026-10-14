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
const SUMMARY_FIELDS: &str =
    "id,parentFolderId,subject,from,toRecipients,receivedDateTime,isRead,flag,hasAttachments";

/// A [`MailStore`] backed by Microsoft Graph.
pub struct GraphMailStore<A> {
    http: reqwest::Client,
    /// The Graph v1.0 root, for example `https://graph.microsoft.com/v1.0/`.
    base: Url,
    auth: Arc<A>,
    /// Folder IDs of well-known folders seen so far, so that
    /// [`MailStore::list_folders`] can label them.
    well_known: Mutex<HashMap<FolderId, WellKnownFolder>>,
}

impl<A: Authenticator> GraphMailStore<A> {
    /// A store for the account in `settings`, using its cloud's Graph
    /// endpoint and `auth` for tokens.
    pub fn new(http: reqwest::Client, settings: &AuthSettings, auth: Arc<A>) -> Self {
        Self {
            http,
            base: settings.graph_api(""),
            auth,
            well_known: Mutex::new(HashMap::new()),
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

    async fn send(&self, request: RequestBuilder) -> Result<reqwest::Response> {
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
            Err(error_from(response).await)
        }
    }

    async fn json<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T> {
        let body = self
            .send(request)
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

    fn label(&self, folder: GraphFolder) -> Folder {
        let well_known = self
            .well_known
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&FolderId::new(&folder.id))
            .copied();
        folder.into_folder(well_known)
    }
}

impl<A: Authenticator> MailStore for GraphMailStore<A> {
    async fn list_folders(&self) -> Result<Vec<Folder>> {
        let mut url = self.url(&["me", "mailFolders"]);
        url.query_pairs_mut().append_pair("$top", "100");
        let mut folders = Vec::new();
        loop {
            let page: Collection<GraphFolder> = self.json(self.request(Method::GET, url)).await?;
            folders.extend(page.value.into_iter().map(|f| self.label(f)));
            match page.next_link {
                Some(next) => url = self.next_page_url(&next)?,
                None => return Ok(folders),
            }
        }
    }

    async fn well_known_folder(&self, which: WellKnownFolder) -> Result<Folder> {
        let url = self.url(&["me", "mailFolders", which.as_str()]);
        let folder: GraphFolder = self.json(self.request(Method::GET, url)).await?;
        self.well_known
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(FolderId::new(&folder.id), which);
        Ok(folder.into_folder(Some(which)))
    }

    async fn list_messages(&self, folder: &FolderId, page: Page) -> Result<MessagePage> {
        let url = match page.cursor() {
            // The cursor is Graph's `@odata.nextLink`, which already holds
            // the query.
            Some(cursor) => self.next_page_url(cursor.as_str())?,
            None => {
                let mut url = self.url(&["me", "mailFolders", folder.as_str(), "messages"]);
                url.query_pairs_mut()
                    .append_pair("$top", &page.size().to_string())
                    .append_pair("$select", SUMMARY_FIELDS)
                    .append_pair("$orderby", "receivedDateTime desc");
                url
            }
        };
        let page: Collection<GraphMessage> = self.json(self.request(Method::GET, url)).await?;
        Ok(MessagePage {
            messages: page
                .value
                .into_iter()
                .map(GraphMessage::into_summary)
                .collect::<Result<_>>()?,
            next: page.next_link.map(PageCursor::new),
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
        let message: GraphMessage = self.json(request).await?;
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
        self.send(self.request(Method::PATCH, url).json(&patch))
            .await
            .map(drop)
    }

    async fn move_message(&self, id: &MessageId, to: &FolderId) -> Result<MessageId> {
        let url = self.url(&["me", "messages", id.as_str(), "move"]);
        let body = MoveRequest {
            destination_id: to.as_str(),
        };
        let moved: IdOnly = self
            .json(self.request(Method::POST, url).json(&body))
            .await?;
        Ok(MessageId::new(moved.id))
    }

    async fn delete_message(&self, id: &MessageId) -> Result<()> {
        // DELETE would only move it to Deleted Items; the port promises a
        // permanent delete.
        let url = self.url(&["me", "messages", id.as_str(), "permanentDelete"]);
        self.send(self.request(Method::POST, url)).await.map(drop)
    }
}

impl<A> GraphMailStore<A> {
    /// Checks that a `@odata.nextLink` points at this Graph endpoint before
    /// following it, so the access token is never sent anywhere else.
    fn next_page_url(&self, link: &str) -> Result<Url> {
        let url = Url::parse(link)
            .map_err(|e| Error::Backend(format!("invalid page link from Graph: {e}")))?;
        if url.origin() != self.base.origin() {
            return Err(Error::Backend(format!(
                "refusing to follow a page link to another host: {}",
                url.origin().ascii_serialization()
            )));
        }
        Ok(url)
    }
}

/// Maps an unsuccessful Graph response to a port error.
async fn error_from(response: reqwest::Response) -> Error {
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
            kind: "item",
            id: detail,
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
    unread_item_count: u32,
    #[serde(default)]
    total_item_count: u32,
}

impl GraphFolder {
    fn into_folder(self, well_known: Option<WellKnownFolder>) -> Folder {
        Folder {
            id: FolderId::new(self.id),
            parent: self.parent_folder_id.map(FolderId::new),
            name: self.display_name,
            well_known,
            unread_count: self.unread_item_count,
            total_count: self.total_item_count,
        }
    }
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
    received_date_time: String,
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
        let received = DateTime::parse_from_rfc3339(&self.received_date_time)
            .map_err(|e| {
                Error::Backend(format!(
                    "invalid receivedDateTime {:?}: {e}",
                    self.received_date_time
                ))
            })?
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
