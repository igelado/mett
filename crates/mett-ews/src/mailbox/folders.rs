use super::*;

/// How long the roles a folder listing looked up serve other calls, which
/// would otherwise look them up each time.
const ROLES_TTL: Duration = Duration::from_secs(15 * 60);

/// The distinguished folder at the root of the mail folders.
const ROOT: &str = "msgfolderroot";

/// Which folders play special roles. Folder IDs don't change, so this is
/// looked up once.
#[derive(Debug, Default)]
pub(super) struct Roles {
    /// The root that top-level folders hang from.
    pub(super) root: Option<FolderId>,
    /// Well-known folders by ID.
    well_known: HashMap<FolderId, WellKnownFolder>,
}

impl Roles {
    pub(super) fn folder(&self, folder: EwsFolder) -> Folder {
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
pub(super) fn distinguished(which: WellKnownFolder) -> Option<&'static str> {
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

impl<A: Authenticator + 'static> EwsMailStore<A> {
    /// The mailbox's folder roles, looked up with one `GetFolder` for the
    /// root and every well-known folder EWS can name. Looked up afresh on
    /// every call, so a well-known folder created since is recognised: a
    /// listing of every folder does so, and other calls use what it found
    /// ([`Self::cached_roles`]). The result is cached.
    pub(super) async fn roles(&self) -> Result<Arc<Roles>> {
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
    pub(super) async fn cached_roles(&self) -> Result<Arc<Roles>> {
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
    pub(super) async fn find_folders(
        &self,
        parent: Option<&FolderId>,
        deep: bool,
    ) -> Result<Vec<EwsFolder>> {
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
    pub(super) async fn get_folder(&self, folder_xml: &str, about: About<'_>) -> Result<EwsFolder> {
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
}

/// `first` and `second` at once, as if `first` came first: its error is
/// the answer as soon as it is known, without waiting for `second`.
pub(super) async fn first_then<T, U>(
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
pub(super) fn tree_order(folders: Vec<EwsFolder>, parent: Option<&str>) -> Vec<EwsFolder> {
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
pub(super) fn sort_by_name(folders: &mut [EwsFolder]) {
    folders.sort_by_cached_key(|f| (f.name.to_lowercase(), f.id.clone()));
}

/// The mail folders among `folders`, and none below a folder of another
/// kind (such as a calendar).
pub(super) fn mail_folders(folders: Vec<EwsFolder>) -> Vec<EwsFolder> {
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
