use super::*;

impl<A: Authenticator> GraphMailStore<A> {
    /// The hidden root folder's ID: from the roles if they are known,
    /// otherwise looked up on its own (and cached).
    pub(super) async fn root(&self) -> Result<Option<FolderId>> {
        if let Some((roles, _)) = self
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
        let root: IdOnly = self.get(url, About::folder(ROOT)).await?;
        let root = Some(FolderId::new(root.id));
        *self.root.lock().unwrap_or_else(|e| e.into_inner()) = Some(root.clone());
        Ok(root)
    }

    /// The roles looked up less than [`ROLES_TTL`] ago, if any.
    pub(super) fn known_roles(&self) -> Option<Arc<Roles>> {
        let cached = self.roles.lock().unwrap_or_else(|e| e.into_inner());
        cached
            .as_ref()
            .filter(|(_, at)| at.elapsed() < ROLES_TTL)
            .map(|(roles, _)| Arc::clone(roles))
    }

    /// The mailbox's folder roles, looked up with a single `$batch` request
    /// for the root and every well-known folder. Looked up afresh on every
    /// call, so a well-known folder created since is recognised: a listing
    /// of every folder does so, and a listing of the folders below one
    /// uses what that found ([`Self::known_roles`]). The result is cached.
    pub(super) async fn roles(&self) -> Result<Arc<Roles>> {
        let nameable: Vec<_> = nameable(|which| Some(which.as_str())).collect();
        let names: Vec<&str> = std::iter::once(ROOT)
            .chain(nameable.iter().map(|(_, name)| *name))
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
                    if let Some((which, _)) = nameable.get(i) {
                        roles.well_known.insert(id, *which);
                    }
                }
            }
        }
        let roles = Arc::new(roles);
        *self.roles.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((Arc::clone(&roles), Instant::now()));
        Ok(roles)
    }
}

impl<A: Authenticator> GraphMailStore<A> {
    /// Sends a `$batch` and returns its responses with the token used.
    async fn batch(&self, requests: &[serde_json::Value]) -> Result<(BatchResponse, AccessToken)> {
        let request = self
            .request(Method::POST, self.url(&["$batch"]))
            .json(&json!({ "requests": requests }));
        let (response, token) = self
            .send_with(request, About::MAILBOX, |e| e, REQUEST_TIMEOUT)
            .await?;
        Ok((decode(response).await?, token))
    }
}

/// How long the roles a folder listing looked up serve the listings of the
/// folders below one, which would otherwise look them up each time.
const ROLES_TTL: Duration = Duration::from_secs(15 * 60);

/// How many folder listings [`MailStore::list_folders`] makes at once: a
/// large tree is listed sooner, without flooding Graph.
pub(super) const LISTINGS_AT_ONCE: usize = 4;

impl<A: Authenticator> GraphMailStore<A> {
    /// The folders directly below `parent` (without one, the top-level
    /// folders), every page of them, in Graph's order.
    pub(super) async fn subfolders(&self, parent: Option<&str>) -> Result<Vec<GraphFolder>> {
        let (mut url, about) = match parent {
            None => (self.url(&["me", "mailFolders"]), About::MAILBOX),
            Some(id) => (
                self.url(&["me", "mailFolders", id, "childFolders"]),
                About::folder(id),
            ),
        };
        url.query_pairs_mut()
            .append_pair("$top", "100")
            .append_pair("includeHiddenFolders", "true");
        let mut folders = Vec::new();
        loop {
            let page: Collection<GraphFolder> = self.get(url, about).await?;
            folders.extend(page.value);
            match page.next_link {
                Some(next) => url = self.next_page_url(&next)?,
                None => return Ok(folders),
            }
        }
    }
}

/// The folders of a successful `listing` that have folders below them.
pub(super) fn with_children(listing: Option<&Result<Vec<GraphFolder>>>) -> Vec<String> {
    match listing {
        Some(Ok(folders)) => folders
            .iter()
            .filter(|f| f.child_folder_count > 0)
            .map(|f| f.id.clone())
            .collect(),
        _ => Vec::new(),
    }
}

/// The folder listings made so far, by the folder they list (`None` for
/// the top level).
pub(super) type Listings = HashMap<Option<String>, Result<Vec<GraphFolder>>>;

/// How a walk down the folder tree, one listing at a time, would go with
/// the listings made so far.
pub(super) enum Walk<'a> {
    /// It lists every folder, from these listings, in this order.
    Listed(Vec<Option<String>>),
    /// It stops at this error.
    Failed(&'a Error),
    /// It needs a listing not made yet.
    Unlisted,
}

/// How a depth-first walk down the tree, one listing at a time, goes with
/// `listings`: it lists the top level, then each time the folders below the
/// last folder found (and not listed yet) that has any. This is the order
/// [`MailStore::list_folders`] gives the folders in.
/// `listed` gives each listing made so far.
pub(super) fn walk<'a>(
    listed: impl Fn(&Option<String>) -> Option<&'a Result<Vec<GraphFolder>>>,
) -> Walk<'a> {
    let mut pending = vec![None];
    let mut order = Vec::new();
    while let Some(parent) = pending.pop() {
        match listed(&parent) {
            None => return Walk::Unlisted,
            Some(Err(e)) => return Walk::Failed(e),
            Some(listing) => pending.extend(with_children(Some(listing)).into_iter().map(Some)),
        }
        order.push(parent);
    }
    Walk::Listed(order)
}
