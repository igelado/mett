//! The background worker: the actor that owns all I/O (`docs/DESIGN.md`
//! §5.4).
//!
//! The UI sends [`Request`]s tagged with a [`RequestId`] and receives a
//! [`Response`] for each, so it can match results and drop stale ones. The
//! worker is generic over the [`MailStore`] and [`MailSender`] ports, so
//! the UI is tested against the in-memory store.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use mett_core::{
    Attachment, AttachmentId, Draft, Error, FlagsUpdate, Folder, FolderId, MailSender, MailStore,
    Message, MessageId, MessagePage, Page, Result, WellKnownFolder, join_bounded,
    join_bounded_until,
};
use tokio::sync::mpsc;

use crate::config::mailbox::FolderPath;

/// Identifies a request, so its response can be matched to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(pub u64);

/// Work for the worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Find a folder by its path in the mailbox.
    OpenFolder(FolderPath),
    /// List a page of messages in a folder.
    ListMessages {
        /// The folder.
        folder: FolderId,
        /// Which page.
        page: Page,
    },
    /// Check a folder for new mail: list its newest messages, and those
    /// that changed or arrived since a time.
    PollFolder {
        /// The folder.
        folder: FolderId,
        /// How many of each to list.
        page: Page,
        /// Changes since when.
        since: DateTime<Utc>,
    },
    /// Fetch a full message.
    FetchMessage(MessageId),
    /// Change a message's flags.
    UpdateFlags(MessageId, FlagsUpdate),
    /// List a message's attachments.
    ListAttachments(MessageId),
    /// Fetch an attachment's content.
    FetchAttachment(MessageId, AttachmentId),
    /// List every folder in the mailbox.
    ListFolders,
    /// Move or copy messages to the folder at a path.
    Transfer {
        /// The messages.
        ids: Vec<MessageId>,
        /// Where they go.
        to: FolderPath,
        /// Copy them rather than move them.
        copy: bool,
    },
    /// Purge messages marked for deletion from the folder `from`: move
    /// them to the trash folder, or delete them for good without one, or
    /// in the trash itself (as mutt does).
    Purge {
        /// The messages.
        ids: Vec<MessageId>,
        /// The folder they are in.
        from: FolderId,
        /// The trash folder, if any.
        trash: Option<FolderPath>,
    },
    /// Send a message.
    Send(Draft),
    /// Save a message in the drafts folder.
    SaveDraft(Draft),
    /// Delete what failed attempts to send or save a message the user gave
    /// up on left behind.
    DiscardKept(Draft),
    /// Find the mailbox owner's own addresses.
    OwnAddresses,
    /// Find addresses matching a text.
    Query(QuerySource, String),
    /// Give up on a [`Request::Query`] still running (its command is
    /// killed). It has no answer.
    CancelQuery(RequestId),
    /// Read the counts of the folders at these paths, and their unread
    /// mail that changed since a time (if one is given), to tell which have
    /// new mail: by ID, where the folder was found before, or else by
    /// path.
    CheckMailboxes(Vec<(FolderPath, Option<FolderId>, Option<DateTime<Utc>>)>),
    /// Find which of `ids` are no longer in `folder` (deleted, or moved
    /// away).
    StillThere {
        /// The folder.
        folder: FolderId,
        /// The messages.
        ids: Vec<MessageId>,
    },
}

/// The outcome of a [`Request`].
// Events are handled one at a time, so their size doesn't matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Event {
    /// Outcome of [`Request::OpenFolder`].
    FolderOpened(Result<Folder>),
    /// Outcome of [`Request::ListMessages`].
    MessagesListed(Result<MessagePage>),
    /// Outcome of [`Request::PollFolder`].
    FolderPolled(Result<Polled>),
    /// Outcome of [`Request::FetchMessage`].
    MessageFetched(Result<Message>),
    /// Outcome of [`Request::UpdateFlags`].
    FlagsUpdated(Result<()>),
    /// Outcome of [`Request::ListAttachments`].
    AttachmentsListed(Result<Vec<Attachment>>),
    /// Outcome of [`Request::FetchAttachment`].
    AttachmentFetched(Result<Vec<u8>>),
    /// Outcome of [`Request::ListFolders`].
    FoldersListed(Result<Vec<Folder>>),
    /// Outcome of [`Request::Transfer`]: the folder could not be found, or
    /// the folder and, for each message, its new ID (or the copy's) or why
    /// it failed.
    Transferred(Result<(FolderId, Moved)>),
    /// Outcome of [`Request::Purge`]: the trash folder could not be found,
    /// or for each message, whether it was purged.
    Purged(Result<Vec<(MessageId, Result<()>)>>),
    /// Outcome of [`Request::Send`], and whether a draft is kept for the
    /// next attempt (see [`MailSender::keeps_draft`]).
    Sent(Result<()>, bool),
    /// Outcome of [`Request::SaveDraft`], and whether a draft is kept.
    DraftSaved(Result<MessageId>, bool),
    /// Outcome of [`Request::DiscardKept`].
    KeptDiscarded(Result<()>),
    /// Outcome of [`Request::OwnAddresses`].
    OwnAddresses(Result<Vec<String>>),
    /// Outcome of [`Request::Query`]: the addresses found, or why none
    /// could be.
    Queried(std::result::Result<Vec<mett_core::Address>, String>),
    /// Outcome of [`Request::CheckMailboxes`]: each folder, or why it
    /// couldn't be read.
    MailboxesChecked(Vec<(FolderPath, Result<Checked>)>),
    /// Outcome of [`Request::StillThere`]: the messages gone.
    StillThere(Result<Vec<MessageId>>),
}

/// A mailbox, as checked for new mail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    /// The folder, with its counts.
    pub folder: Folder,
    /// Its unread messages that arrived or changed last, and when.
    pub recent: Vec<(MessageId, DateTime<Utc>)>,
}

/// Where to find addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuerySource {
    /// mutt's `query_command`: a shell command, with `%s` for the text.
    Command(String),
    /// The people the user works with and the organisation's directory.
    Directory,
}

/// What a check of a folder for new mail found.
#[derive(Debug)]
pub struct Polled {
    /// The newest messages.
    pub newest: MessagePage,
    /// The messages that changed or arrived, most recently first.
    pub changed: MessagePage,
}

/// How each message of a move or copy fared: its new ID (or the copy's),
/// or why it failed.
pub type Moved = Vec<(MessageId, Result<MessageId>)>;

/// An [`Event`] for the request with `id`.
#[derive(Debug)]
pub struct Response {
    /// The request this answers.
    pub id: RequestId,
    /// What happened.
    pub event: Event,
}

/// Serves requests until the request channel closes. Each request runs as
/// its own task, so a slow fetch doesn't hold up the others.
pub async fn run<S: MailStore + MailSender + 'static>(
    store: Arc<S>,
    mut requests: mpsc::UnboundedReceiver<(RequestId, Request)>,
    responses: mpsc::UnboundedSender<Response>,
) {
    // Queries still running, which the user may give up on.
    let queries: Arc<
        std::sync::Mutex<std::collections::HashMap<RequestId, tokio::task::AbortHandle>>,
    > = Arc::default();
    while let Some((id, request)) = requests.recv().await {
        if let Request::CancelQuery(query) = request {
            if let Some(task) = queries.lock().expect("lock").remove(&query) {
                task.abort();
            }
            continue;
        }
        let query = matches!(request, Request::Query(..));
        let store = Arc::clone(&store);
        let responses = responses.clone();
        let running = Arc::clone(&queries);
        // Held until the query is registered: it can't take itself out
        // before then.
        let mut registered = queries.lock().expect("lock");
        let task = tokio::spawn(async move {
            let event = serve(&*store, request).await;
            if query {
                running.lock().expect("lock").remove(&id);
            }
            // The UI may have quit; nothing to do then.
            let _ = responses.send(Response { id, event });
        });
        if query {
            registered.insert(id, task.abort_handle());
        }
        drop(registered);
    }
}

async fn serve<S: MailStore + MailSender>(store: &S, request: Request) -> Event {
    match request {
        Request::OpenFolder(path) => Event::FolderOpened(resolve(store, &path).await),
        Request::ListMessages { folder, page } => {
            Event::MessagesListed(store.list_messages(&folder, page).await)
        }
        Request::PollFolder {
            folder,
            page,
            since,
        } => {
            let (newest, changed) = tokio::join!(
                store.list_messages(&folder, page.clone()),
                changes(store, &folder, since, page),
            );
            Event::FolderPolled(newest.and_then(|newest| {
                Ok(Polled {
                    newest,
                    changed: changed?,
                })
            }))
        }
        Request::FetchMessage(id) => Event::MessageFetched(store.fetch_message(&id).await),
        Request::ListAttachments(id) => Event::AttachmentsListed(store.list_attachments(&id).await),
        Request::FetchAttachment(id, attachment) => {
            Event::AttachmentFetched(store.fetch_attachment(&id, &attachment).await)
        }
        Request::UpdateFlags(id, update) => {
            Event::FlagsUpdated(store.update_flags(&id, update).await)
        }
        Request::ListFolders => Event::FoldersListed(store.list_folders().await),
        Request::Transfer { ids, to, copy } => {
            Event::Transferred(transfer(store, &ids, &to, copy).await)
        }
        Request::Purge { ids, from, trash } => {
            Event::Purged(purge(store, &ids, &from, trash.as_ref()).await)
        }
        Request::Send(draft) => {
            let sent = store.send(&draft).await;
            Event::Sent(sent, store.keeps_draft(&draft))
        }
        Request::SaveDraft(draft) => {
            let saved = store.save_draft(&draft).await;
            Event::DraftSaved(saved, store.keeps_draft(&draft))
        }
        Request::DiscardKept(draft) => Event::KeptDiscarded(store.discard_kept(&draft).await),
        Request::OwnAddresses => Event::OwnAddresses(store.own_addresses().await),
        Request::Query(QuerySource::Directory, text) => {
            Event::Queried(store.search_people(&text).await.map_err(|e| e.to_string()))
        }
        Request::CancelQuery(_) => unreachable!("handled by run"),
        Request::Query(QuerySource::Command(command), text) => {
            Event::Queried(run_query(&command, &text).await)
        }
        Request::StillThere { folder, ids } => {
            // Where each is, a few at a time; the first failure (in the
            // order of `ids`) is the answer, as soon as those before it are
            // known.
            let failed = |found: &Option<Result<FolderId>>| matches!(found, Some(Err(e)) if !matches!(e, Error::NotFound { .. }));
            let found = join_bounded_until(
                ids.iter().map(|id| store.message_folder(id)).collect(),
                CHECKS_AT_ONCE,
                |found| found.iter().take_while(|f| f.is_some()).any(failed),
            )
            .await;
            let mut gone = Vec::new();
            for (id, found) in ids.into_iter().zip(found) {
                let Some(found) = found else {
                    unreachable!("a failure before it is the answer");
                };
                match found {
                    Err(Error::NotFound { .. }) => gone.push(id),
                    Ok(at) if at != folder => gone.push(id),
                    Ok(_) => {}
                    Err(e) => return Event::StillThere(Err(e)),
                }
            }
            Event::StillThere(Ok(gone))
        }
        Request::CheckMailboxes(mailboxes) => {
            Event::MailboxesChecked(check_mailboxes(store, mailboxes).await)
        }
    }
}

/// How many mailboxes are checked at once: a slow one doesn't hold up the
/// others, nor do many at once flood Graph.
const CHECKS_AT_ONCE: usize = 4;

/// Checks `mailboxes` (see [`Request::CheckMailboxes`]), a few at a time.
async fn check_mailboxes<S: MailStore>(
    store: &S,
    mailboxes: Vec<(FolderPath, Option<FolderId>, Option<DateTime<Utc>>)>,
) -> Vec<(FolderPath, Result<Checked>)> {
    // Parents, well-known folders and the folders below each are looked up
    // once for all.
    let lookups = &Lookups::default();
    // The folders, by the IDs known, each checked once found. A folder
    // deleted (or moved away) since is looked for by its path again; so is
    // one renamed or moved since (another may be at its path now).
    let known = join_bounded(
        mailboxes
            .iter()
            .map(|(path, id, since)| async move {
                let id = id.as_ref()?;
                let found = match lookups.folder(store, id).await {
                    Err(Error::NotFound { .. }) => return None,
                    Ok(folder) => match at_path(store, lookups, path, &folder).await {
                        Ok(true) => Ok(folder),
                        Ok(false) => return None,
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(e),
                };
                Some(check(store, found, Some(id), *since).await)
            })
            .collect(),
        CHECKS_AT_ONCE,
    )
    .await;
    // The others, by their paths.
    let by_path = join_bounded(
        mailboxes
            .iter()
            .zip(&known)
            .filter(|(_, known)| known.is_none())
            .map(|((path, id, since), _)| async move {
                let found = resolve_with(store, lookups, path).await;
                check(store, found, id.as_ref(), *since).await
            })
            .collect(),
        CHECKS_AT_ONCE,
    )
    .await;
    let mut by_path = by_path.into_iter();
    mailboxes
        .into_iter()
        .zip(known)
        .map(|((path, _, _), known)| {
            let checked = known.unwrap_or_else(|| by_path.next().expect("checked by its path"));
            (path, checked)
        })
        .collect()
}

/// Checks the folder `found` for a mailbox last seen with `id` (if it was
/// found): reads its unread mail that changed `since`.
async fn check<S: MailStore>(
    store: &S,
    found: Result<Folder>,
    id: Option<&FolderId>,
    since: Option<DateTime<Utc>>,
) -> Result<Checked> {
    let folder = found?;
    // Changes since what was seen of the folder asked about, not of
    // another now at its path.
    let since = since.filter(|_| id == Some(&folder.id));
    store
        .recent_unread(&folder.id, since)
        .await
        .map(|recent| Checked { folder, recent })
}

/// What the lookups for one request found, so that each is made once:
/// checking many mailboxes looks up their common parents, and lists the
/// folders below each, once for all. A lookup that fails isn't kept: the
/// next one is made again.
#[derive(Default)]
struct Lookups {
    /// Folders by ID.
    folders: Memo<FolderId, Folder>,
    /// Well-known folders.
    well_known: Memo<WellKnownFolder, Folder>,
    /// The folders below each folder (`None`: the top-level ones).
    children: Memo<Option<FolderId>, Arc<Vec<Folder>>>,
}

impl Lookups {
    /// The folder `id` (see [`MailStore::folder`]).
    async fn folder<S: MailStore>(&self, store: &S, id: &FolderId) -> Result<Folder> {
        self.folders.get(id, || store.folder(id)).await
    }

    /// The well-known folder `which` (see [`MailStore::well_known_folder`]).
    async fn well_known<S: MailStore>(&self, store: &S, which: WellKnownFolder) -> Result<Folder> {
        self.well_known
            .get(&which, || store.well_known_folder(which))
            .await
    }

    /// The folders below `parent` (see [`MailStore::child_folders`]).
    async fn children<S: MailStore>(
        &self,
        store: &S,
        parent: Option<&FolderId>,
    ) -> Result<Arc<Vec<Folder>>> {
        self.children
            .get(&parent.cloned(), || async move {
                store.child_folders(parent).await.map(Arc::new)
            })
            .await
    }
}

/// Values looked up by key, each once (while one lookup runs, others for
/// the same key wait for it), unless it fails.
struct Memo<K, V>(std::sync::Mutex<HashMap<K, Arc<tokio::sync::OnceCell<V>>>>);

impl<K, V> Default for Memo<K, V> {
    fn default() -> Self {
        Self(std::sync::Mutex::default())
    }
}

impl<K: Clone + Eq + Hash, V: Clone> Memo<K, V> {
    /// The value for `key`, looked up with `look_up` if it isn't known.
    async fn get<F: Future<Output = Result<V>>>(
        &self,
        key: &K,
        look_up: impl FnOnce() -> F,
    ) -> Result<V> {
        let cell = Arc::clone(self.0.lock().expect("lock").entry(key.clone()).or_default());
        cell.get_or_try_init(look_up).await.cloned()
    }
}

/// How many pages of changes a check reads before listing the whole
/// folder again is cheaper.
const CHANGE_PAGES: usize = 10;

/// The messages of `folder` changed since `since`: up to
/// [`CHANGE_PAGES`] pages, with a cursor if there are more.
async fn changes<S: MailStore>(
    store: &S,
    folder: &FolderId,
    since: DateTime<Utc>,
    page: Page,
) -> Result<MessagePage> {
    let size = page.size();
    let mut changed = store.changed_messages(folder, since, page).await?;
    for _ in 1..CHANGE_PAGES {
        let Some(cursor) = changed.next.take() else {
            break;
        };
        let more = store
            .changed_messages(folder, since, Page::after(cursor, size))
            .await?;
        changed.messages.extend(more.messages);
        changed.next = more.next;
    }
    Ok(changed)
}

/// Runs `query_command` for `text`, as mutt does: `%s` is the text,
/// quoted for the shell; the first line of output is a message, and each
/// line after it an address, a tab, then a name.
async fn run_query(
    command: &str,
    text: &str,
) -> std::result::Result<Vec<mett_core::Address>, String> {
    // Killed, with whatever it started, if the lookup is given up or mett
    // quits first: a command that hangs doesn't keep it from exiting.
    let mut shell = tokio::process::Command::new("sh");
    shell
        .arg("-c")
        .arg(query_script(command))
        .env("METT_QUERY", text)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    shell.process_group(0);
    let child = shell.spawn().map_err(|e| format!("query_command: {e}"))?;
    let mut group = KillGroup(child.id());
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("query_command: {e}"))?;
    group.0 = None;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let first = lines.next().unwrap_or_default();
    if !output.status.success() {
        return Err(if first.is_empty() {
            format!("query_command failed: {}", output.status)
        } else {
            first.to_owned()
        });
    }
    Ok(lines
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let email = fields.next()?.trim();
            // One mailbox, and nothing else (not a list, nor a name).
            let parsed = mett_core::Address::parse_list(email).ok()?;
            let [address] = parsed.as_slice() else {
                return None;
            };
            if !email.contains('@') || address.name.is_some() || address.email != email {
                return None;
            }
            let address = mett_core::Address::new(email);
            Some(
                match fields.next().map(str::trim).filter(|n| !n.is_empty()) {
                    Some(name) => address.with_name(name),
                    None => address,
                },
            )
        })
        .collect())
}

/// Kills the process group of a query command when dropped while it runs
/// (the lookup was given up): `kill_on_drop` alone leaves what the shell
/// started.
struct KillGroup(Option<u32>);

impl Drop for KillGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", "--", &format!("-{pid}")])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

/// `command` with each `%s` standing for the text looked up, which the
/// shell gets in the environment (`$METT_QUERY`, which shell functions
/// see too): whatever it holds, it is never read
/// as shell syntax, inside quotes or out.
fn query_script(command: &str) -> String {
    /// Where in the shell's syntax the command is.
    #[derive(Clone, Copy, PartialEq)]
    enum Context {
        /// Unquoted, in the command or a `(...)`.
        Plain,
        /// In `'...'`.
        Single,
        /// In `"..."`.
        Double,
        /// In `$(...)` or `` `...` ``, quoted afresh whatever is around.
        Substitution,
        Backtick,
    }
    use Context::{Backtick, Double, Plain, Single, Substitution};
    let mut script = String::new();
    let mut stack = vec![Plain];
    // Without `%s`, the text goes at the end, as in mutt.
    let placed = command.replace("%%", "").contains("%s");
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        let context = *stack.last().expect("the command is there");
        match c {
            // `%%` is a `%`, as in mutt.
            '%' if chars.peek() == Some(&'%') => {
                chars.next();
                script.push('%');
                continue;
            }
            '%' if chars.peek() == Some(&'s') => {
                chars.next();
                script.push_str(match context {
                    Double => "${METT_QUERY}",
                    // Out of the quotes and back.
                    Single => "'\"${METT_QUERY}\"'",
                    Plain | Substitution | Backtick => "\"${METT_QUERY}\"",
                });
                continue;
            }
            '\\' if context != Single => {
                script.push(c);
                if let Some(next) = chars.next() {
                    script.push(next);
                }
                continue;
            }
            '\'' if context == Single => {
                stack.pop();
            }
            '\'' if context != Double => stack.push(Single),
            '"' if context == Double => {
                stack.pop();
            }
            '"' if context != Single => stack.push(Double),
            '$' if context != Single && chars.peek() == Some(&'(') => {
                script.push(c);
                script.extend(chars.next());
                stack.push(Substitution);
                continue;
            }
            '(' if matches!(context, Plain | Substitution | Backtick) => stack.push(Plain),
            ')' if matches!(context, Plain | Substitution) && stack.len() > 1 => {
                stack.pop();
            }
            '`' if context == Backtick => {
                stack.pop();
            }
            '`' if context != Single => stack.push(Backtick),
            _ => {}
        }
        script.push(c);
    }
    if !placed {
        script.push_str(" \"${METT_QUERY}\"");
    }
    script
}

/// How many messages are moved, copied or deleted at once.
const MOVES_AT_ONCE: usize = 4;

/// Purges `ids` from the folder `from` (see [`Request::Purge`]), a few at
/// a time, each on its own, so that a failure only affects its own
/// message.
async fn purge<S: MailStore>(
    store: &S,
    ids: &[MessageId],
    from: &FolderId,
    trash: Option<&FolderPath>,
) -> Result<Vec<(MessageId, Result<()>)>> {
    // The trash may be named differently from how the open folder was
    // opened (`=Deleted Items` and `@deleteditems`): compare the folders.
    let trash = match trash {
        Some(path) => Some(resolve(store, path).await?).filter(|t| &t.id != from),
        None => None,
    };
    let trash = trash.as_ref();
    let results = join_bounded(
        ids.iter()
            .map(|id| async move {
                let result = match trash {
                    Some(trash) => store.move_message(id, &trash.id).await.map(drop),
                    None => store.delete_message(id).await,
                };
                (id.clone(), result)
            })
            .collect(),
        MOVES_AT_ONCE,
    )
    .await;
    Ok(results)
}

/// Moves or copies `ids` to the folder at `to`, a few at a time, each on
/// its own, so that a failure only affects its own message.
async fn transfer<S: MailStore>(
    store: &S,
    ids: &[MessageId],
    to: &FolderPath,
    copy: bool,
) -> Result<(FolderId, Moved)> {
    let folder = resolve(store, to).await?;
    let to = &folder.id;
    let results = join_bounded(
        ids.iter()
            .map(|id| async move {
                let result = if copy {
                    store.copy_message(id, to).await
                } else {
                    store.move_message(id, to).await
                };
                (id.clone(), result)
            })
            .collect(),
        MOVES_AT_ONCE,
    )
    .await;
    Ok((folder.id, results))
}

/// Whether `folder` is still at `path`: its name and its parents' match
/// the path's, up to the top of the mailbox or the well-known folder the
/// path starts from.
async fn at_path<S: MailStore>(
    store: &S,
    lookups: &Lookups,
    path: &FolderPath,
    folder: &Folder,
) -> Result<bool> {
    let (names, start) = match path {
        FolderPath::Root => return Ok(true),
        FolderPath::WellKnown(which, rest) => (rest.as_slice(), Some(*which)),
        FolderPath::Named(names) => (names.as_slice(), None),
    };
    let mut current = folder.clone();
    for (i, name) in names.iter().enumerate().rev() {
        if !crate::config::mailbox::same_name(name, &current.name) {
            return Ok(false);
        }
        if i > 0 {
            let Some(parent) = current.parent.clone() else {
                return Ok(false);
            };
            current = lookups.folder(store, &parent).await?;
        }
    }
    match start {
        // A well-known folder keeps its ID.
        Some(_) if names.is_empty() => Ok(true),
        Some(which) => {
            let start = lookups.well_known(store, which).await?;
            Ok(current.parent.as_ref() == Some(&start.id))
        }
        None => Ok(current.parent.is_none()),
    }
}

/// Finds the folder at `path`: a well-known folder, possibly followed by
/// names of folders below it, or names from the top of the mailbox.
async fn resolve<S: MailStore>(store: &S, path: &FolderPath) -> Result<Folder> {
    resolve_with(store, &Lookups::default(), path).await
}

/// [`resolve`], with the `lookups` made for other paths: checking several
/// mailboxes lists the folders below each parent once.
async fn resolve_with<S: MailStore>(
    store: &S,
    lookups: &Lookups,
    path: &FolderPath,
) -> Result<Folder> {
    match path {
        FolderPath::Root => Err(Error::Backend(
            "the mailbox itself isn't a folder; choose one, such as @inbox".into(),
        )),
        FolderPath::WellKnown(which, rest) => {
            let start = lookups.well_known(store, *which).await?;
            if rest.is_empty() {
                return Ok(start);
            }
            descend(store, lookups, Some(start), rest).await
        }
        FolderPath::Named(names) => descend(store, lookups, None, names).await,
    }
}

/// Walks down `names` from `start` (or the top of the mailbox), listing
/// the folders below each level only. Names match exactly, or else
/// ignoring case (in any script), as Office365 folder names do.
async fn descend<S: MailStore>(
    store: &S,
    lookups: &Lookups,
    start: Option<Folder>,
    names: &[String],
) -> Result<Folder> {
    let mut current = start;
    for name in names {
        let parent = current.as_ref().map(|f| &f.id);
        let folders = lookups.children(store, parent).await?;
        let children = || folders.iter().filter(|f| f.parent.as_ref() == parent);
        let found = children()
            .find(|f| &f.name == name)
            .or_else(|| {
                let name = name.to_lowercase();
                children().find(|f| f.name.to_lowercase() == name)
            })
            .ok_or_else(|| Error::NotFound {
                kind: "folder",
                id: names.join("/"),
            })?;
        current = Some(found.clone());
    }
    current.ok_or_else(|| Error::not_found("folder", names.join("/")))
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use mett_core::WellKnownFolder;
    use mett_core::mock::{self, MemoryStore};

    use super::*;

    fn tree() -> MemoryStore {
        let child = |id: &str, name: &str, parent: &str| {
            let mut folder = mock::folder(id, name, None);
            folder.parent = Some(FolderId::new(parent));
            folder
        };
        MemoryStore::new()
            .with_folder(mock::folder("in", "Inbox", Some(WellKnownFolder::Inbox)))
            .with_folder(child("proj", "Projects", "in"))
            .with_folder(child("mett", "mett", "proj"))
            .with_folder(child("team", "équipe", "in"))
            .with_folder(mock::folder("arc", "Archive", None))
    }

    #[tokio::test]
    async fn tells_which_messages_left_a_folder() {
        let store = MemoryStore::new()
            .with_folder(mock::folder("in", "Inbox", None))
            .with_message(mock::message("m1", "in", "Hi", DateTime::UNIX_EPOCH));
        let event = serve(
            &store,
            Request::StillThere {
                folder: FolderId::new("in"),
                ids: vec![MessageId::new("m1"), MessageId::new("m2")],
            },
        )
        .await;
        let Event::StillThere(Ok(gone)) = event else {
            panic!("expected the messages gone, got {event:?}");
        };
        assert_eq!(gone, [MessageId::new("m2")]);
    }
    #[tokio::test]
    async fn a_query_given_up_has_no_answer() {
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let (response_tx, mut response_rx) = mpsc::unbounded_channel();
        tokio::spawn(run(Arc::new(MemoryStore::new()), request_rx, response_tx));
        let slow = Request::Query(QuerySource::Command("sleep 30".into()), "x".into());
        request_tx.send((RequestId(1), slow)).unwrap();
        request_tx
            .send((RequestId(2), Request::CancelQuery(RequestId(1))))
            .unwrap();
        request_tx
            .send((RequestId(3), Request::OwnAddresses))
            .unwrap();
        let first = response_rx.recv().await.unwrap();
        assert_eq!(first.id, RequestId(3));
        let more =
            tokio::time::timeout(std::time::Duration::from_millis(200), response_rx.recv()).await;
        assert!(more.is_err(), "the query answered: {more:?}");
    }

    #[tokio::test]
    async fn runs_query_commands_like_mutt() {
        let found = run_query(
            "echo Searching for %s; printf 'bob@x.org\\tBob Smith\\tx\\nnot an address\\na@x.org, e@x.org\\n'",
            "it's",
        )
        .await
        .unwrap();
        assert_eq!(
            found,
            [mett_core::Address::new("bob@x.org").with_name("Bob Smith")]
        );
        assert_eq!(
            run_query("echo 'No such person'; exit 1", "x").await,
            Err("No such person".into())
        );
        // Without `%s`, the text comes last.
        let found = run_query("echo; echo", "c@d.org").await.unwrap();
        assert_eq!(found, [mett_core::Address::new("c@d.org")]);
        // In a command substitution, the text is still one word.
        let found = run_query("echo; echo \"$(set -- %s; echo n$#@x.org)\"", "a b")
            .await
            .unwrap();
        assert_eq!(found, [mett_core::Address::new("n1@x.org")]);
        // In a shell function too.
        let found = run_query("f() { echo; echo %s; }; f", "g@x.org")
            .await
            .unwrap();
        assert_eq!(found, [mett_core::Address::new("g@x.org")]);
        // `%%` is a `%`.
        let found = run_query("echo; printf '%%s\\n' %s", "a@b.org")
            .await
            .unwrap();
        assert_eq!(found, [mett_core::Address::new("a@b.org")]);
        // The text is never shell syntax, whatever quotes `%s` is in.
        for command in [
            "echo; echo %s",
            "echo; echo \"%s\"",
            "echo; echo '%s'",
            "echo; echo \"$(echo %s)\"",
            "echo; echo \"`echo %s`\"",
        ] {
            let found = run_query(command, "a$HOME`x`'q@y.org").await.unwrap();
            assert_eq!(found, [mett_core::Address::new("a$HOME`x`'q@y.org")]);
        }
    }

    #[tokio::test]
    async fn resolves_folder_paths() {
        let store = tree();
        let inbox = || FolderPath::WellKnown(WellKnownFolder::Inbox, vec![]);
        assert_eq!(
            resolve(&store, &inbox()).await.unwrap().id,
            FolderId::new("in")
        );
        let nested = FolderPath::WellKnown(
            WellKnownFolder::Inbox,
            vec!["projects".into(), "mett".into()],
        );
        assert_eq!(
            resolve(&store, &nested).await.unwrap().id,
            FolderId::new("mett")
        );
        let accented = FolderPath::WellKnown(WellKnownFolder::Inbox, vec!["ÉQUIPE".into()]);
        assert_eq!(
            resolve(&store, &accented).await.unwrap().id,
            FolderId::new("team")
        );
        let named = FolderPath::Named(vec!["Archive".into()]);
        assert_eq!(
            resolve(&store, &named).await.unwrap().id,
            FolderId::new("arc")
        );
        // A name is looked up only below its parent.
        let wrong = FolderPath::Named(vec!["Projects".into()]);
        assert!(matches!(
            resolve(&store, &wrong).await,
            Err(Error::NotFound { kind: "folder", .. })
        ));
        assert!(resolve(&store, &FolderPath::Root).await.is_err());
    }

    #[tokio::test]
    async fn checks_mailboxes_by_id_or_path() {
        let store = tree().with_message(mock::message("m1", "arc", "one", DateTime::UNIX_EPOCH));
        let archive = FolderPath::Named(vec!["Archive".into()]);
        let inbox = FolderPath::WellKnown(WellKnownFolder::Inbox, vec![]);
        let missing = FolderPath::Named(vec!["nowhere".into()]);
        let Event::MailboxesChecked(checked) = serve(
            &store,
            Request::CheckMailboxes(vec![
                (archive.clone(), Some(FolderId::new("arc")), None),
                // A folder that is gone is looked for by path again.
                (inbox.clone(), Some(FolderId::new("gone")), None),
                (missing.clone(), None, None),
            ]),
        )
        .await
        else {
            panic!("expected MailboxesChecked");
        };
        let found: Vec<_> = checked
            .iter()
            .map(|(path, checked)| {
                let folder = checked.as_ref().ok().map(|c| &c.folder);
                (path, folder.map(|f| (f.id.as_str(), f.unread_count)))
            })
            .collect();
        assert_eq!(found[0], (&archive, Some(("arc", 1))));
        assert_eq!(found[1], (&inbox, Some(("in", 0))));
        assert!(matches!(checked[2].1, Err(Error::NotFound { .. })));
        // Renamed: the folder now called Archive is the one checked.
        let renamed = MemoryStore::new()
            .with_folder(mock::folder("old", "Elsewhere", None))
            .with_folder(mock::folder("arc", "Archive", None));
        let Event::MailboxesChecked(checked) = serve(
            &renamed,
            Request::CheckMailboxes(vec![(archive, Some(FolderId::new("old")), None)]),
        )
        .await
        else {
            panic!("expected MailboxesChecked");
        };
        assert_eq!(
            checked[0].1.as_ref().unwrap().folder.id,
            FolderId::new("arc")
        );
        // Moved to another parent: the folder now at the path is checked.
        let child = |id: &str, name: &str, parent: &str| {
            let mut folder = mock::folder(id, name, None);
            folder.parent = Some(FolderId::new(parent));
            folder
        };
        let moved = tree()
            .with_folder(child("shared-old", "Shared", "arc"))
            .with_folder(child("shared-new", "Shared", "proj"));
        let path = FolderPath::WellKnown(
            WellKnownFolder::Inbox,
            vec!["Projects".into(), "Shared".into()],
        );
        let Event::MailboxesChecked(checked) = serve(
            &moved,
            Request::CheckMailboxes(vec![
                (path.clone(), Some(FolderId::new("shared-old")), None),
                (path, Some(FolderId::new("shared-new")), None),
            ]),
        )
        .await
        else {
            panic!("expected MailboxesChecked");
        };
        assert_eq!(
            checked[0].1.as_ref().unwrap().folder.id,
            FolderId::new("shared-new")
        );
        assert_eq!(
            checked[1].1.as_ref().unwrap().folder.id,
            FolderId::new("shared-new")
        );
    }

    #[tokio::test]
    async fn moves_and_copies_messages_and_lists_folders() {
        let store = tree()
            .with_message(mock::message("m1", "in", "one", DateTime::UNIX_EPOCH))
            .with_message(mock::message("m2", "in", "two", DateTime::UNIX_EPOCH));
        let archive = FolderPath::Named(vec!["archive".into()]);
        let ids = vec![MessageId::new("m1"), MessageId::new("nope")];
        let Event::Transferred(Ok((_, results))) = serve(
            &store,
            Request::Transfer {
                ids,
                to: archive.clone(),
                copy: false,
            },
        )
        .await
        else {
            panic!("expected Transferred");
        };
        assert!(results[0].1.is_ok());
        assert!(matches!(results[1].1, Err(Error::NotFound { .. })));
        let missing = FolderPath::Named(vec!["nowhere".into()]);
        let event = serve(
            &store,
            Request::Transfer {
                ids: vec![MessageId::new("m2")],
                to: missing,
                copy: true,
            },
        )
        .await;
        assert!(matches!(event, Event::Transferred(Err(_))));

        let Event::FoldersListed(Ok(folders)) = serve(&store, Request::ListFolders).await else {
            panic!("expected FoldersListed");
        };
        assert_eq!(folders.len(), 5);
    }

    #[tokio::test]
    async fn purges_to_the_trash_or_for_good() {
        let store = tree()
            .with_message(mock::message("m1", "in", "one", DateTime::UNIX_EPOCH))
            .with_message(mock::message("a1", "arc", "old", DateTime::UNIX_EPOCH));
        let count = |folder: &'static str| {
            let store = &store;
            async move {
                store
                    .list_messages(&FolderId::new(folder), Page::first(10))
                    .await
                    .unwrap()
                    .messages
                    .len()
            }
        };
        let trash = Some(FolderPath::Named(vec!["Archive".into()]));
        let purge = |id: &str, from: &str| Request::Purge {
            ids: vec![MessageId::new(id)],
            from: FolderId::new(from),
            trash: trash.clone(),
        };
        let Event::Purged(Ok(results)) = serve(&store, purge("m1", "in")).await else {
            panic!("expected Purged");
        };
        assert!(results[0].1.is_ok());
        assert_eq!((count("in").await, count("arc").await), (0, 2), "moved");
        // In the trash itself, however it is named, purging deletes.
        let Event::Purged(Ok(results)) = serve(&store, purge("a1", "arc")).await else {
            panic!("expected Purged");
        };
        assert!(results[0].1.is_ok());
        assert_eq!(count("arc").await, 1, "deleted, not moved");
        let missing = Request::Purge {
            ids: vec![],
            from: FolderId::new("in"),
            trash: Some(FolderPath::Named(vec!["nowhere".into()])),
        };
        assert!(matches!(
            serve(&store, missing).await,
            Event::Purged(Err(_))
        ));
    }

    #[tokio::test]
    async fn answers_each_request_with_its_id() {
        let store = MemoryStore::new()
            .with_folder(mock::folder("in", "Inbox", Some(WellKnownFolder::Inbox)))
            .with_message(mock::message("m1", "in", "hello", DateTime::UNIX_EPOCH));
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let (response_tx, mut response_rx) = mpsc::unbounded_channel();
        tokio::spawn(run(Arc::new(store), request_rx, response_tx));

        let inbox = FolderPath::WellKnown(WellKnownFolder::Inbox, vec![]);
        request_tx
            .send((RequestId(1), Request::OpenFolder(inbox)))
            .unwrap();
        let response = response_rx.recv().await.unwrap();
        assert_eq!(response.id, RequestId(1));
        assert!(matches!(response.event, Event::FolderOpened(Ok(f)) if f.name == "Inbox"));

        request_tx
            .send((RequestId(2), Request::FetchMessage("m1".into())))
            .unwrap();
        let response = response_rx.recv().await.unwrap();
        assert_eq!(response.id, RequestId(2));
        assert!(
            matches!(response.event, Event::MessageFetched(Ok(m)) if m.summary.subject == "hello")
        );
    }

    /// The worker against a mock Microsoft Graph, counting the requests
    /// it makes.
    mod graph {
        use std::collections::HashMap;
        use std::time::{Duration, Instant};

        use chrono::TimeDelta;
        use mett_core::{AccessToken, Authenticator};
        use mett_msgraph::{AuthSettings, GraphMailStore};
        use serde_json::{Value, json};
        use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

        use super::*;

        /// Hands out a fixed token.
        struct StaticAuth;

        impl Authenticator for StaticAuth {
            async fn access_token(&self) -> Result<AccessToken> {
                Ok(AccessToken::new("tok", Utc::now() + TimeDelta::hours(1)))
            }
        }

        /// Answers each request by its path (whatever its method or
        /// query), or else with a 404.
        struct Paths(HashMap<String, ResponseTemplate>);

        impl Respond for Paths {
            fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
                let at = request.url.path().strip_prefix("/v1.0").unwrap_or_default();
                self.0
                    .get(at)
                    .cloned()
                    .unwrap_or_else(|| ResponseTemplate::new(404))
            }
        }

        fn ok(body: Value) -> ResponseTemplate {
            ResponseTemplate::new(200).set_body_json(body)
        }

        fn folder(id: &str, name: &str, parent: &str, children: u32) -> Value {
            json!({
                "id": id,
                "displayName": name,
                "parentFolderId": parent,
                "childFolderCount": children,
                "unreadItemCount": 1,
                "totalItemCount": 3,
            })
        }

        /// A mailbox whose root is `root-id` and Inbox `inbox-id`, with no
        /// other well-known folder, and nothing else yet.
        fn mailbox() -> HashMap<String, ResponseTemplate> {
            let roles: Vec<Value> = (0..8)
                .map(|i| match i {
                    0 => json!({ "id": "0", "status": 200, "body": { "id": "root-id" } }),
                    1 => json!({ "id": "1", "status": 200, "body": { "id": "inbox-id" } }),
                    _ => json!({ "id": i.to_string(), "status": 404 }),
                })
                .collect();
            HashMap::from([
                ("/$batch".into(), ok(json!({ "responses": roles }))),
                (
                    "/me/mailFolders/msgfolderroot".into(),
                    ok(json!({ "id": "root-id" })),
                ),
                (
                    "/me/mailFolders/inbox".into(),
                    ok(folder("inbox-id", "Inbox", "root-id", 0)),
                ),
            ])
        }

        /// [`mailbox`], with 300 folders, 50 of them with folders below
        /// them: Archive (`arc`) holds `2020` to `2024` (`arc-2020`, ...).
        fn large_tree() -> HashMap<String, ResponseTemplate> {
            let mut paths = mailbox();
            let mut top = vec![folder("arc", "Archive", "root-id", 5)];
            top.extend(
                (1..50).map(|i| folder(&format!("t{i}"), &format!("Folder {i}"), "root-id", 5)),
            );
            paths.insert("/me/mailFolders".into(), ok(json!({ "value": top })));
            paths.insert(
                "/me/mailFolders/arc/childFolders".into(),
                ok(json!({
                    "value": (2020..2025)
                        .map(|y| folder(&format!("arc-{y}"), &y.to_string(), "arc", 0))
                        .collect::<Vec<_>>(),
                })),
            );
            for i in 1..50 {
                let parent = format!("t{i}");
                paths.insert(
                    format!("/me/mailFolders/{parent}/childFolders"),
                    ok(json!({
                        "value": (0..5)
                            .map(|j| folder(&format!("{parent}-{j}"), &format!("Sub {j}"), &parent, 0))
                            .collect::<Vec<_>>(),
                    })),
                );
            }
            paths
        }

        async fn serve_paths(paths: HashMap<String, ResponseTemplate>) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(wiremock::matchers::any())
                .respond_with(Paths(paths))
                .mount(&server)
                .await;
            server
        }

        fn store(server: &MockServer) -> GraphMailStore<StaticAuth> {
            let settings = AuthSettings {
                graph_url: Some(url::Url::parse(&server.uri()).unwrap()),
                ..AuthSettings::new("alice@contoso.com".into(), "client")
            };
            GraphMailStore::new(reqwest::Client::new(), &settings, Arc::new(StaticAuth))
        }

        /// The paths of the requests `server` received, below the Graph
        /// root, and how many there were.
        async fn requests(server: &MockServer) -> Vec<String> {
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .map(|r| r.url.path().trim_start_matches("/v1.0").to_owned())
                .collect()
        }

        /// The folder listings among `requests`.
        fn listings(requests: &[String]) -> Vec<&str> {
            requests
                .iter()
                .map(String::as_str)
                .filter(|p| *p == "/me/mailFolders" || p.ends_with("/childFolders"))
                .collect()
        }

        fn archive_2024() -> FolderPath {
            FolderPath::Named(vec!["archive".into(), "2024".into()])
        }

        #[tokio::test]
        async fn opening_a_path_lists_only_its_levels() {
            let server = serve_paths(large_tree()).await;
            let event = serve(&store(&server), Request::OpenFolder(archive_2024())).await;
            let Event::FolderOpened(Ok(folder)) = event else {
                panic!("expected the folder, got {event:?}");
            };
            assert_eq!(folder.id, FolderId::new("arc-2024"));
            assert_eq!(folder.parent, Some(FolderId::new("arc")));
            let requests = requests(&server).await;
            assert_eq!(
                listings(&requests),
                ["/me/mailFolders", "/me/mailFolders/arc/childFolders"]
            );
            // The roles, which label well-known folders as a listing of
            // them all does.
            assert_eq!(requests.len(), 4, "{requests:?}");

            // A name missing at any level, as before.
            let missing = FolderPath::Named(vec!["Archive".into(), "1999".into()]);
            let event = serve(&store(&server), Request::OpenFolder(missing)).await;
            assert!(
                matches!(&event, Event::FolderOpened(Err(Error::NotFound { kind: "folder", id })) if id == "Archive/1999"),
                "{event:?}"
            );
        }

        #[tokio::test]
        async fn a_failed_listing_on_the_way_is_the_answer() {
            let mut paths = large_tree();
            paths.insert(
                "/me/mailFolders/arc/childFolders".into(),
                ResponseTemplate::new(503),
            );
            let server = serve_paths(paths).await;
            let event = serve(&store(&server), Request::OpenFolder(archive_2024())).await;
            assert!(
                matches!(event, Event::FolderOpened(Err(Error::Network(_)))),
                "{event:?}"
            );
            // Elsewhere in the tree, it doesn't get in the way.
            let mut paths = large_tree();
            paths.insert(
                "/me/mailFolders/t7/childFolders".into(),
                ResponseTemplate::new(503),
            );
            let server = serve_paths(paths).await;
            let event = serve(&store(&server), Request::OpenFolder(archive_2024())).await;
            assert!(matches!(event, Event::FolderOpened(Ok(_))), "{event:?}");
        }

        #[tokio::test]
        async fn moves_a_few_at_once_in_order() {
            let mut paths = large_tree();
            let delay = Duration::from_millis(300);
            for i in 0..8 {
                // The fourth is gone.
                if i != 3 {
                    paths.insert(
                        format!("/me/messages/m{i}/move"),
                        ok(json!({ "id": format!("n{i}") })).set_delay(delay),
                    );
                }
            }
            let server = serve_paths(paths).await;
            let ids: Vec<_> = (0..8).map(|i| MessageId::new(format!("m{i}"))).collect();
            let started = Instant::now();
            let event = serve(
                &store(&server),
                Request::Transfer {
                    ids: ids.clone(),
                    to: archive_2024(),
                    copy: false,
                },
            )
            .await;
            // One after the other, they would take 8 times the delay.
            assert!(started.elapsed() < delay * 6, "{:?}", started.elapsed());
            let Event::Transferred(Ok((to, moved))) = event else {
                panic!("expected the messages moved, got {event:?}");
            };
            assert_eq!(to, FolderId::new("arc-2024"));
            let got: Vec<_> = moved.iter().map(|(id, _)| id.clone()).collect();
            assert_eq!(got, ids);
            for (i, (_, result)) in moved.into_iter().enumerate() {
                match result {
                    Ok(new) => assert_eq!(new, MessageId::new(format!("n{i}"))),
                    Err(e) => assert!(i == 3 && matches!(e, Error::NotFound { .. }), "{e}"),
                }
            }
        }

        #[tokio::test]
        async fn tells_which_messages_left_by_their_folder_alone() {
            let mut paths = mailbox();
            let at = |folder: &str| ok(json!({ "id": "x", "parentFolderId": folder }));
            paths.insert("/me/messages/m1".into(), at("inbox-id"));
            paths.insert("/me/messages/m2".into(), at("elsewhere"));
            paths.insert("/me/messages/m4".into(), ResponseTemplate::new(503));
            paths.insert("/me/messages/m5".into(), ResponseTemplate::new(403));
            let server = serve_paths(paths).await;
            let ids = |n: &[u8]| n.iter().map(|i| MessageId::new(format!("m{i}"))).collect();
            let still_there = |ids| Request::StillThere {
                folder: FolderId::new("inbox-id"),
                ids,
            };
            let event = serve(&store(&server), still_there(ids(&[1, 2, 3]))).await;
            let Event::StillThere(Ok(gone)) = event else {
                panic!("expected the messages gone, got {event:?}");
            };
            assert_eq!(gone, [MessageId::new("m2"), MessageId::new("m3")]);
            for request in server.received_requests().await.unwrap() {
                let query = request.url.query().unwrap_or_default();
                assert_eq!(query, "%24select=id%2CparentFolderId");
            }
            // The first failure, in order, is the answer.
            let event = serve(&store(&server), still_there(ids(&[1, 4, 5]))).await;
            assert!(
                matches!(event, Event::StillThere(Err(Error::Network(_)))),
                "{event:?}"
            );
            let event = serve(&store(&server), still_there(ids(&[5, 4]))).await;
            assert!(
                matches!(event, Event::StillThere(Err(Error::Auth(_)))),
                "{event:?}"
            );
        }

        #[tokio::test]
        async fn the_first_failure_is_not_held_up_by_later_messages() {
            let stalled = Duration::from_secs(5);
            let mut paths = mailbox();
            let at = ok(json!({ "id": "x", "parentFolderId": "inbox-id" }));
            paths.insert("/me/messages/m1".into(), at.clone());
            paths.insert("/me/messages/m2".into(), ResponseTemplate::new(503));
            paths.insert("/me/messages/m3".into(), at.set_delay(stalled));
            let server = serve_paths(paths).await;
            let started = Instant::now();
            let event = serve(
                &store(&server),
                Request::StillThere {
                    folder: FolderId::new("inbox-id"),
                    ids: ["m1", "m2", "m3"].map(MessageId::new).to_vec(),
                },
            )
            .await;
            assert!(
                matches!(event, Event::StillThere(Err(Error::Network(_)))),
                "{event:?}"
            );
            assert!(started.elapsed() < stalled / 2, "{:?}", started.elapsed());
        }

        /// [`mailbox`], with `List 0` to `List 19` (`l0`, ...) in the
        /// Inbox, each with no unread mail.
        fn lists() -> HashMap<String, ResponseTemplate> {
            let mut paths = mailbox();
            for i in 0..20 {
                let id = format!("l{i}");
                paths.insert(
                    format!("/me/mailFolders/{id}"),
                    ok(folder(&id, &format!("List {i}"), "inbox-id", 0)),
                );
                paths.insert(
                    format!("/me/mailFolders/{id}/messages"),
                    ok(json!({ "value": [] })),
                );
            }
            paths
        }

        fn list_path(i: usize) -> FolderPath {
            FolderPath::WellKnown(WellKnownFolder::Inbox, vec![format!("List {i}")])
        }

        #[tokio::test]
        async fn checking_mailboxes_looks_up_their_parents_once() {
            let server = serve_paths(lists()).await;
            let since = Some(DateTime::UNIX_EPOCH);
            let mailboxes = (0..20)
                .map(|i| (list_path(i), Some(FolderId::new(format!("l{i}"))), since))
                .collect();
            let event = serve(&store(&server), Request::CheckMailboxes(mailboxes)).await;
            let Event::MailboxesChecked(checked) = event else {
                panic!("expected the mailboxes checked, got {event:?}");
            };
            for (i, (path, checked)) in checked.into_iter().enumerate() {
                assert_eq!(path, list_path(i));
                assert_eq!(checked.unwrap().folder.id, FolderId::new(format!("l{i}")));
            }
            let requests = requests(&server).await;
            let count = |at: &str| requests.iter().filter(|p| *p == at).count();
            assert_eq!(count("/me/mailFolders/inbox"), 1, "{requests:?}");
            for i in 0..20 {
                assert_eq!(count(&format!("/me/mailFolders/l{i}")), 1);
                assert_eq!(count(&format!("/me/mailFolders/l{i}/messages")), 1);
            }
            assert!(listings(&requests).is_empty());
        }

        #[tokio::test]
        async fn a_mailbox_gone_is_looked_for_level_by_level() {
            let mut paths = large_tree();
            paths.extend(lists());
            paths.insert(
                "/me/mailFolders/arc-2024/messages".into(),
                ok(json!({ "value": [] })),
            );
            let server = serve_paths(paths).await;
            let mailboxes = vec![
                (list_path(0), Some(FolderId::new("l0")), None),
                // Deleted: looked for by its path.
                (archive_2024(), Some(FolderId::new("gone")), None),
                (
                    FolderPath::Named(vec!["Archive".into(), "1999".into()]),
                    Some(FolderId::new("gone too")),
                    None,
                ),
            ];
            let event = serve(&store(&server), Request::CheckMailboxes(mailboxes)).await;
            let Event::MailboxesChecked(checked) = event else {
                panic!("expected the mailboxes checked, got {event:?}");
            };
            assert_eq!(
                checked[0].1.as_ref().unwrap().folder.id,
                FolderId::new("l0")
            );
            assert_eq!(
                checked[1].1.as_ref().unwrap().folder.id,
                FolderId::new("arc-2024")
            );
            assert!(matches!(checked[2].1, Err(Error::NotFound { .. })));
            // Each level listed once for both.
            let requests = requests(&server).await;
            assert_eq!(
                listings(&requests),
                ["/me/mailFolders", "/me/mailFolders/arc/childFolders"]
            );
        }
    }
}
