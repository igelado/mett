//! [`GraphMailStore`]: the [`MailStore`] port for Microsoft Graph.
//!
//! It works on the signed-in user's mailbox (`/me`), using the Graph REST
//! API v1.0. Every request asks the [`Authenticator`] for a token, so
//! refreshing and signing in happen transparently.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use mett_core::{
    AccessToken, Address, Authenticator, Body, BodyKind, Draft, DraftOrigin, Error, Flags,
    FlagsUpdate, Folder, FolderId, MailSender, MailStore, Message, MessageId, MessagePage,
    MessageSummary, Page, PageCursor, RECENT_UNREAD, Result, WellKnownFolder,
};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;

use crate::settings::{AuthSettings, Cloud};

/// Upper bound on each Graph request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The message fields the index needs.
const SUMMARY_FIELDS: &str = "id,parentFolderId,subject,from,toRecipients,sentDateTime,createdDateTime,isDraft,isRead,flag,hasAttachments,lastModifiedDateTime";

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
    /// The mailbox's folder roles, once looked up.
    roles: Mutex<Option<Arc<Roles>>>,
    /// The hidden root folder's ID, once looked up on its own.
    root: Mutex<Option<Option<FolderId>>>,
    /// Reply and forward drafts kept on the server after a send or update
    /// whose outcome is unknown, by what they answer and the composed
    /// message they hold ([`Draft::attempt`]): sending or postponing that
    /// message again updates the draft rather than making another.
    kept: Mutex<HashMap<(DraftOrigin, u64), MessageId>>,
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
            cloud: settings.cloud,
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
            kept: Mutex::new(HashMap::new()),
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
        self.send_with_token(request, about)
            .await
            .map(|(response, _)| response)
    }

    /// Like [`Self::send`], also returning the token the response was
    /// obtained with.
    async fn send_with_token(
        &self,
        request: RequestBuilder,
        about: About<'_>,
    ) -> Result<(reqwest::Response, AccessToken)> {
        self.send_with(request, about, |e| e).await
    }

    /// Sends a request that hands a message over to be sent. Failing
    /// before the request is made (say, getting a token) leaves the message
    /// unsent, which [`Error::NotSent`] says; only a lost response to the
    /// request itself leaves it in doubt.
    async fn hand_over(&self, request: RequestBuilder, about: About<'_>) -> Result<()> {
        self.send_with(request, about, unsent).await.map(drop)
    }

    /// Like [`Self::send_with_token`], passing errors from before the
    /// request is made (or after Graph refused it) through `before`.
    async fn send_with(
        &self,
        request: RequestBuilder,
        about: About<'_>,
        before: fn(Error) -> Error,
    ) -> Result<(reqwest::Response, AccessToken)> {
        let token = self.auth.access_token().await.map_err(before)?;
        // Kept for a retry; JSON bodies can always be cloned.
        let retry = request.try_clone();
        let response = Self::attempt(request, &token).await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok((Self::checked(response, about).await?, token));
        }
        // Graph rejected a token that hadn't expired (it may have been
        // revoked): get a new one and try once more.
        self.auth.token_rejected(&token).await;
        let fresh = self.auth.access_token().await.map_err(before)?;
        match retry {
            Some(retry) if fresh != token => {
                let response = Self::attempt(retry, &fresh).await?;
                if response.status() == StatusCode::UNAUTHORIZED {
                    // Rejected too: don't let the next request reuse it.
                    self.auth.token_rejected(&fresh).await;
                }
                Ok((Self::checked(response, about).await?, fresh))
            }
            _ => Ok((Self::checked(response, about).await?, token)),
        }
    }

    async fn attempt(request: RequestBuilder, token: &AccessToken) -> Result<reqwest::Response> {
        request
            .bearer_auth(token.secret())
            .timeout(REQUEST_TIMEOUT)
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
        let body = self
            .send_with(request, about, before)
            .await?
            .0
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

    /// The mailbox's folder roles, looked up with a single `$batch` request
    /// for the root and every well-known folder. Looked up afresh on every
    /// call (once per folder listing), so a well-known folder created since
    /// is recognised; the result is cached for [`Self::root`].
    async fn roles(&self) -> Result<Arc<Roles>> {
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
        // Like other requests, retry once if Graph rejects the token, which
        // it reports inside the batch rather than on the response.
        // The token each batch was sent with is the one to reject: another
        // request may have refreshed the cached token in the meantime.
        let (mut batch, mut token) = self.batch(&requests).await?;
        if batch.responses.iter().any(|r| r.status == 401) {
            self.auth.token_rejected(&token).await;
            (batch, token) = self.batch(&requests).await?;
        }

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
                    if response.status == 401 {
                        // Rejected again: make sure the next request gets a
                        // new token.
                        self.auth.token_rejected(&token).await;
                    }
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

impl<A: Authenticator> GraphMailStore<A> {
    /// Sends a `$batch` and returns its responses with the token used.
    async fn batch(&self, requests: &[serde_json::Value]) -> Result<(BatchResponse, AccessToken)> {
        let request = self
            .request(Method::POST, self.url(&["$batch"]))
            .json(&json!({ "requests": requests }));
        let (response, token) = self.send_with_token(request, About::MAILBOX).await?;
        let body = response
            .bytes()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;
        let batch = serde_json::from_slice(&body)
            .map_err(|e| Error::Backend(format!("unexpected response from Graph: {e}")))?;
        Ok((batch, token))
    }
}

/// Graph's well-known name for the hidden root of the folder tree.
const ROOT: &str = "msgfolderroot";

impl<A: Authenticator> MailStore for GraphMailStore<A> {
    async fn own_addresses(&self) -> Result<Vec<String>> {
        let mut url = self.url(&["me"]);
        url.query_pairs_mut()
            .append_pair("$select", "mail,userPrincipalName,proxyAddresses");
        let me: Me = self
            .json(self.request(Method::GET, url), About::MAILBOX)
            .await?;
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

    async fn folder(&self, id: &FolderId) -> Result<Folder> {
        // The roles, if a folder listing looked them up, tell a well-known
        // folder; only the root is needed otherwise, to tell a top-level
        // folder.
        let cached = self.roles.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let roles = match cached {
            Some(roles) => roles,
            None => Arc::new(Roles {
                root: self.root().await?,
                well_known: HashMap::new(),
            }),
        };
        let url = self.url(&["me", "mailFolders", id.as_str()]);
        let folder: GraphFolder = self
            .json(self.request(Method::GET, url), About::folder(id.as_str()))
            .await?;
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
            |since| format!("gt {}", since.to_rfc3339_opts(SecondsFormat::Secs, true)),
        );
        let mut url = self.url(&["me", "mailFolders", folder.as_str(), "messages"]);
        url.query_pairs_mut()
            .append_pair("$top", "50")
            .append_pair("$select", "id,lastModifiedDateTime")
            // Graph wants the sorted property filtered too, first.
            .append_pair(
                "$filter",
                &format!("lastModifiedDateTime {after} and isRead eq false"),
            )
            .append_pair("$orderby", "lastModifiedDateTime desc");
        let mut recent = Vec::new();
        loop {
            let page: Collection<Modified> = self
                .json(
                    self.request(Method::GET, url),
                    About::folder(folder.as_str()),
                )
                .await?;
            for m in page.value {
                let at = DateTime::parse_from_rfc3339(&m.last_modified_date_time)
                    .map_err(|e| Error::Backend(format!("invalid lastModifiedDateTime: {e}")))?;
                recent.push((MessageId::new(m.id), at.with_timezone(&Utc)));
            }
            // Without `since`, the last few tell; with it, all of them, up
            // to a limit.
            let wanted = if since.is_some() { RECENT_UNREAD } else { 10 };
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
            &format!("{SUMMARY_FIELDS},ccRecipients,bccRecipients,replyTo,internetMessageId,body"),
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

    async fn copy_message(&self, id: &MessageId, to: &FolderId) -> Result<MessageId> {
        let url = self.url(&["me", "messages", id.as_str(), "copy"]);
        let body = MoveRequest {
            destination_id: to.as_str(),
        };
        let copied: IdOnly = self
            .json(
                self.request(Method::POST, url).json(&body),
                About::message(id.as_str()),
            )
            .await?;
        Ok(MessageId::new(copied.id))
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

    async fn save_draft(&self, draft: &Draft) -> Result<MessageId> {
        match &draft.origin {
            // A plain draft is created with its content in one request.
            None => {
                let url = self.url(&["me", "messages"]);
                let created: IdOnly = self
                    .json(
                        self.request(Method::POST, url).json(&message_json(draft)),
                        About::MAILBOX,
                    )
                    .await?;
                Ok(MessageId::new(created.id))
            }
            Some(origin) => self.answer(origin, draft).await,
        }
    }
}

impl<A: Authenticator> GraphMailStore<A> {
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
}

impl<A: Authenticator> MailSender for GraphMailStore<A> {
    async fn discard_kept(&self, draft: &Draft) -> Result<()> {
        self.discard_kept_draft(draft).await
    }

    async fn send(&self, draft: &Draft) -> Result<()> {
        let Some(origin) = &draft.origin else {
            let url = self.url(&["me", "sendMail"]);
            let body = json!({
                "message": message_json(draft),
                "saveToSentItems": draft.save_copy,
            });
            return self
                .hand_over(self.request(Method::POST, url).json(&body), About::MAILBOX)
                .await;
        };
        // Replies and forwards go through a server draft, so that Graph
        // threads them under the original and keeps a forwarded message's
        // attachments. Sending a draft always keeps a copy in Sent Items.
        // Failing before the send request leaves nothing sent, whatever
        // became of the draft.
        let id = self.answer(origin, draft).await.map_err(unsent)?;
        let url = self.url(&["me", "messages", id.as_str(), "send"]);
        let sent = self
            .hand_over(self.request(Method::POST, url), About::message(id.as_str()))
            .await;
        match &sent {
            // The response was lost or Graph failed: the message may have
            // gone out anyway. Keep the draft, so nothing is lost if it
            // didn't, and say so; sending again reuses it.
            Err(Error::Network(e)) => {
                self.keep(origin, draft, id);
                return Err(Error::Network(format!(
                    "{e}; if it wasn't sent, it is in Drafts"
                )));
            }
            // Graph refused it: it wasn't sent. A draft that can't be
            // deleted either is kept, for the next attempt.
            Err(_) => {
                if !self.discard(&id).await {
                    self.keep(origin, draft, id);
                }
            }
            Ok(_) => {}
        }
        sent.map(drop)
    }
}

impl<A: Authenticator> GraphMailStore<A> {
    /// Creates a server draft that replies to or forwards the original, with
    /// `draft`'s content, and returns its ID.
    async fn answer(&self, origin: &DraftOrigin, draft: &Draft) -> Result<MessageId> {
        let (original, action) = match origin {
            DraftOrigin::Reply(id) => (id, "createReply"),
            DraftOrigin::Forward(id) => (id, "createForward"),
        };
        // A draft kept after an earlier attempt gets the new content; if it
        // is gone (it was sent after all, or deleted), a new one is made.
        let key = (origin.clone(), draft.attempt);
        let kept = self.kept.lock().expect("lock").remove(&key);
        if let Some(id) = kept {
            match self.update_draft(&id, draft).await {
                Ok(()) => return Ok(id),
                Err(Error::NotFound { .. }) => {}
                Err(e) => {
                    self.keep(origin, draft, id);
                    return Err(e);
                }
            }
        }
        let url = self.url(&["me", "messages", original.as_str(), action]);
        // A lost response may still have made a draft, which mett can't
        // tell apart from others: say so. Failing before the request is
        // made leaves nothing.
        let created: IdOnly = self
            .json_with(
                self.request(Method::POST, url),
                About::message(original.as_str()),
                unsent,
            )
            .await
            .map_err(|e| match e {
                Error::Network(e) => {
                    Error::Network(format!("{e}; a draft of it may be left in Drafts"))
                }
                e => e,
            })?;
        let id = MessageId::new(created.id);
        // Replace what Graph filled in (recipients, a quoted HTML body) with
        // what the user wrote.
        match self.update_draft(&id, draft).await {
            Ok(()) => Ok(id),
            // The update may have been made: keep the draft, for the next
            // attempt to update.
            Err(e @ Error::Network(_)) => {
                self.keep(origin, draft, id);
                Err(e)
            }
            Err(e) => {
                // One that can't be deleted (access was denied, say) is kept
                // for the next attempt, rather than left behind.
                if !self.discard(&id).await {
                    self.keep(origin, draft, id);
                }
                Err(e)
            }
        }
    }

    /// Gives a server draft `draft`'s content.
    async fn update_draft(&self, id: &MessageId, draft: &Draft) -> Result<()> {
        let url = self.url(&["me", "messages", id.as_str()]);
        self.send(
            self.request(Method::PATCH, url).json(&message_json(draft)),
            About::message(id.as_str()),
        )
        .await
        .map(drop)
    }

    /// Deletes the server draft kept for `draft`, if any. It is kept still
    /// if that fails.
    async fn discard_kept_draft(&self, draft: &Draft) -> Result<()> {
        let Some(origin) = &draft.origin else {
            return Ok(());
        };
        let key = (origin.clone(), draft.attempt);
        let Some(id) = self.kept.lock().expect("lock").remove(&key) else {
            return Ok(());
        };
        let deleted = self.delete_draft(&id).await;
        if deleted.is_err() {
            self.keep(origin, draft, id);
        }
        deleted
    }

    /// Remembers a server draft kept after an attempt whose outcome is
    /// unknown.
    fn keep(&self, origin: &DraftOrigin, draft: &Draft, id: MessageId) {
        self.kept
            .lock()
            .expect("lock")
            .insert((origin.clone(), draft.attempt), id);
    }

    /// Deletes a server draft that could not be completed, so it doesn't
    /// linger in Drafts, and returns whether it is gone. Failing to is not
    /// worth reporting over the error that caused it.
    async fn discard(&self, id: &MessageId) -> bool {
        self.delete_draft(id).await.is_ok()
    }

    /// Deletes a server draft for good where Graph can (DELETE would only
    /// move it to Deleted Items); elsewhere it goes to Deleted Items. One
    /// that is gone already is as good as deleted.
    async fn delete_draft(&self, id: &MessageId) -> Result<()> {
        let request = if self.cloud == Cloud::Global {
            let url = self.url(&["me", "messages", id.as_str(), "permanentDelete"]);
            self.request(Method::POST, url)
        } else {
            self.request(Method::DELETE, self.url(&["me", "messages", id.as_str()]))
        };
        match self.send(request, About::message(id.as_str())).await {
            Ok(_) | Err(Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// A network failure before a message was handed over: it wasn't sent.
fn unsent(e: Error) -> Error {
    match e {
        Error::Network(e) => Error::NotSent(format!("network error: {e}")),
        e => e,
    }
}

/// A Graph `message` resource with `draft`'s content.
fn message_json(draft: &Draft) -> serde_json::Value {
    let recipients = |list: &[Address]| -> Vec<serde_json::Value> {
        list.iter()
            .map(|a| {
                let mut address = json!({ "address": a.email });
                if let Some(name) = a.name.as_deref().filter(|n| !n.is_empty()) {
                    address["name"] = json!(name);
                }
                json!({ "emailAddress": address })
            })
            .collect()
    };
    let mut message = json!({
        "subject": draft.subject,
        "body": { "contentType": "Text", "content": draft.body },
        "toRecipients": recipients(&draft.to),
        "ccRecipients": recipients(&draft.cc),
        "bccRecipients": recipients(&draft.bcc),
    });
    if let Some(from) = &draft.from {
        message["from"] = recipients(std::slice::from_ref(from)).remove(0);
    }
    message
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

/// The signed-in user's addresses.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Me {
    mail: Option<String>,
    user_principal_name: Option<String>,
    #[serde(default)]
    proxy_addresses: Vec<String>,
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
    #[serde(default)]
    bcc_recipients: Vec<Recipient>,
    #[serde(default)]
    reply_to: Vec<Recipient>,
    /// Missing, or a placeholder, for unsent drafts.
    sent_date_time: Option<String>,
    #[serde(default)]
    is_draft: bool,
    created_date_time: Option<String>,
    last_modified_date_time: Option<String>,
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
            bcc: rest.bcc.into_iter().map(Recipient::into_address).collect(),
            reply_to: rest
                .reply_to
                .into_iter()
                .map(Recipient::into_address)
                .collect(),
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
            draft: self.is_draft,
            modified: self
                .last_modified_date_time
                .as_deref()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&Utc)),
        };
        let rest = Rest {
            cc: self.cc_recipients,
            bcc: self.bcc_recipients,
            reply_to: self.reply_to,
            internet_message_id: self.internet_message_id,
            body: self.body,
        };
        Ok((summary, rest))
    }
}

/// The parts of a [`GraphMessage`] that only a full [`Message`] needs.
struct Rest {
    cc: Vec<Recipient>,
    bcc: Vec<Recipient>,
    reply_to: Vec<Recipient>,
    internet_message_id: Option<String>,
    body: Option<ItemBody>,
}
