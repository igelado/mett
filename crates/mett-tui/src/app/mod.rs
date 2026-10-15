//! The application model and its `update` function: the Elm Architecture
//! (`docs/DESIGN.md` §5.2).
//!
//! [`App`] holds all UI state. Only [`App::update`] changes it, in response
//! to a [`Msg`]; side effects come back as [`Effect`]s for the main loop to
//! carry out. This keeps the UI logic testable without a terminal.

mod composing;
mod keys;
mod mailbox;
mod newmail;
mod search;
mod threads;

use std::collections::{HashMap, HashSet, VecDeque};

use mett_core::{AccountId, Error, FlagsUpdate, Folder, Message, MessageId, MessageSummary, Page};
use mett_msgraph::SignInPrompt;
use ratatui::crossterm::event::KeyEvent;

use self::composing::Composing;
pub use self::composing::{Compose, Input};
pub use self::keys::{Help, Reconfigured};
pub use self::mailbox::Browser;
use self::mailbox::{AfterPurge, Transfer, Write};
pub use self::newmail::MailboxEntry;
pub use self::search::{Limit, highlighted};
pub use self::threads::{Ordering, Threaded};
use crate::action::Action;
use crate::compose::Kind;
use crate::config::Quad;
use crate::config::mailbox::FolderPath;
use crate::keymap::{Keymap, Menu, Stroke};
use crate::signin::SignInEvent;
use crate::text;
use crate::worker::{Event, Request, RequestId, Response};

/// How many messages to request per page, unless configured.
pub const PAGE_SIZE: u32 = 100;

/// Where a pending move goes.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    /// The entry at this position.
    Entry(usize),
    /// The entry this many after a message: the index's order may change
    /// as pages come (with `sort`), and the message stays where it goes.
    After(MessageId, usize),
    /// The last message of the folder.
    Last,
    /// The next thread after the selected message.
    NextThread,
}

/// Rows used by the help line, the status bar and the message line.
const CHROME_ROWS: usize = 3;

/// What the app needs from the configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// How many messages to request per page.
    pub page_size: u32,
    /// How to write messages.
    pub compose: crate::compose::Settings,
    /// The account whose folders are used.
    pub account: AccountId,
    /// mutt's `folder`: what `=` in a mailbox path stands for.
    pub folder: String,
    /// mutt's `trash`: where purged messages go, or `None` to delete them
    /// for good.
    pub trash: Option<FolderPath>,
    /// mutt's `delete`: whether to purge deleted messages.
    pub delete: Quad,
    /// mutt's `resolve`: move to the next message after marking one.
    pub resolve: bool,
    /// What the `!`, `>` and `<` mailbox shortcuts stand for.
    pub shortcuts: Shortcuts,
    /// The folders to check for new mail (`mailboxes`).
    pub mailboxes: Vec<MailboxEntry>,
    /// How the screen is drawn.
    pub display: crate::expando::Display,
    /// How the index is ordered.
    pub ordering: Ordering,
    /// How often to check for new mail (`msgraph_sync_interval`), or
    /// `None` not to.
    pub sync_interval: Option<std::time::Duration>,
}

/// mutt's `spoolfile`, `mbox` and `record`, as set: the mailboxes that
/// `!`, `>` and `<` stand for (`docs/MUTTRC.md` §5).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shortcuts {
    /// `!`.
    pub spoolfile: String,
    /// `>`.
    pub mbox: String,
    /// `<`.
    pub record: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            page_size: PAGE_SIZE,
            compose: crate::compose::Settings::default(),
            account: AccountId::new("demo@example.com"),
            folder: String::new(),
            trash: None,
            delete: Quad::AskYes,
            resolve: true,
            shortcuts: Shortcuts::default(),
            mailboxes: Vec::new(),
            display: crate::expando::Display::default(),
            ordering: Ordering::default(),
            sync_interval: None,
        }
    }
}

/// Input to [`App::update`].
// Messages are handled one at a time, so their size doesn't matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Msg {
    /// The user triggered an action.
    Action(Action),
    /// The worker answered a request.
    Response(Response),
    /// The authenticator needs the user, or finished signing in.
    SignIn(SignInEvent),
    /// A key press: typed into the question on the message line, if
    /// there is one, or looked up in the key bindings.
    Key(KeyEvent),
    /// A `:` command ran.
    Configured(Reconfigured),
    /// The editor finished: the edited text, or why it failed.
    Edited(Result<String, String>),
    /// A timer the app set went off.
    Timer(Timer),
    /// The terminal was resized.
    Resize {
        /// Terminal width, in cells.
        width: u16,
        /// Terminal height, in rows.
        height: u16,
    },
}

/// A side effect for the main loop to carry out.
// Effects are handed over as soon as they are made.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Send `Request` to the worker.
    Request(RequestId, Request),
    /// Open a URL in the user's browser.
    OpenUrl(String),
    /// Cancel the sign-in in progress.
    CancelSignIn,
    /// Put the text on the clipboard, through the terminal.
    Copy(String),
    /// Edit the text in the user's editor, then send back
    /// [`Msg::Edited`].
    Edit(String),
    /// Run a mettrc command, then send back [`Msg::Configured`].
    Command(String),
    /// Send back [`Msg::Timer`] after a while.
    Later(std::time::Duration, Timer),
}

/// Something the app does after a while.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timer {
    /// List Sent Items again, if it is open: Graph files the copy of a
    /// sent message a moment after accepting it.
    RelistSentItems,
    /// Check for new mail, if this is still the timer for it (it counts
    /// the changes of interval).
    Poll(u64),
}

/// A screen on the navigation stack (`docs/DESIGN.md` §5.6).
// The stack holds at most a couple of screens.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Screen {
    /// The message index.
    Index,
    /// The pager, showing one message.
    Pager(Pager),
    /// The compose menu, for a message about to be sent.
    Compose(Compose),
    /// The folder browser.
    Browser(Browser),
    /// The key bindings of a menu.
    Help(Help),
}

/// Pager state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pager {
    /// The message being shown.
    pub id: MessageId,
    /// The message, once fetched.
    pub message: Option<Message>,
    /// The message laid out as lines (headers and body), not yet wrapped.
    pub lines: Vec<String>,
    /// `lines` wrapped to [`Pager::rows_width`]. Cached, so redrawing and
    /// scrolling don't re-wrap the whole message.
    pub rows: Vec<String>,
    /// The width `rows` were wrapped to, if they have been.
    pub rows_width: Option<usize>,
    /// The request fetching it.
    pub request: RequestId,
    /// The first visible row, after wrapping.
    pub scroll: usize,
    /// The user set the message read or unread while it was loading:
    /// showing it doesn't mark it read.
    pub read_chosen: bool,
    /// For each of `lines`, its first row in `rows`.
    pub line_rows: Vec<usize>,
    /// For each of `rows`, the bytes of its line it shows.
    pub row_ranges: Vec<std::ops::Range<usize>>,
    /// Where the last search match ended, as a line and a byte offset in
    /// it: the next search starts there. (The match may be shown lower
    /// down when it is near the end.)
    pub search_match: Option<(usize, usize)>,
    /// The text the last search found, as a line and a byte range in it,
    /// shown highlighted.
    pub highlight: Option<(usize, std::ops::Range<usize>)>,
    /// A search made before the message arrived, to run when it does.
    pub pending_search: Option<String>,
    /// The first line of the body, after the headers and the blank line.
    pub body_start: usize,
    /// The line the signature starts at (`-- `), if there is one.
    pub signature: Option<usize>,
}

impl Pager {
    /// Re-wraps the message if the width changed.
    fn layout(&mut self, width: usize) {
        if self.rows_width != Some(width) {
            self.rows.clear();
            self.line_rows.clear();
            self.row_ranges.clear();
            for line in &self.lines {
                self.line_rows.push(self.rows.len());
                for range in text::wrap_ranges(line, width) {
                    self.rows.push(line[range.clone()].to_owned());
                    self.row_ranges.push(range);
                }
            }
            self.rows_width = Some(width);
        }
    }
}

/// The sign-in window, shown over the current screen while the user has
/// to sign in, or after a sign-in failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignIn {
    /// The account being signed in to.
    pub account: AccountId,
    /// What the user has to do, once known.
    pub prompt: Option<SignInPrompt>,
    /// Why the sign-in failed, once it has.
    pub failure: Option<String>,
    /// How far the window's text is scrolled, for terminals too small to
    /// show it all.
    pub scroll: usize,
}

/// What a row of the sign-in window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// Text.
    Text,
    /// The device code, highlighted.
    Code,
    /// Part of the sign-in link.
    Link,
}

impl SignIn {
    /// The window's text wrapped to `width`, with what each row shows. The
    /// keys are on the help line.
    pub fn rows(&self, width: usize) -> Vec<(String, Row)> {
        let mut lines: Vec<(String, Row)> = Vec::new();
        // After a failure, the link or code is spent: don't offer it.
        let prompt = self.prompt.as_ref().filter(|_| self.failure.is_none());
        match prompt {
            Some(SignInPrompt::OpenBrowser { url }) => {
                lines.push((
                    "Sign in in your browser. If it did not open, visit:".into(),
                    Row::Text,
                ));
                lines.push((String::new(), Row::Text));
                lines.push((url.to_string(), Row::Link));
            }
            Some(SignInPrompt::EnterCode {
                verification_uri,
                user_code,
                ..
            }) => {
                lines.push((
                    format!(
                        "To sign in, visit {verification_uri} on any device and enter the code:"
                    ),
                    Row::Text,
                ));
                lines.push((String::new(), Row::Text));
                lines.push((format!("    {user_code}"), Row::Code));
            }
            None => {}
        }
        if !lines.is_empty() {
            lines.push((String::new(), Row::Text));
        }
        lines.push(match &self.failure {
            Some(failure) => (format!("Sign-in failed: {failure}"), Row::Text),
            None => ("Waiting for you to sign in...".into(), Row::Text),
        });
        lines
            .into_iter()
            .flat_map(|(line, kind)| {
                text::wrap(&line, width)
                    .into_iter()
                    .map(move |row| (row, kind))
            })
            .collect()
    }

    /// What `c` copies: the sign-in link, or the device code.
    fn copyable(&self) -> Option<String> {
        if self.failure.is_some() {
            return None;
        }
        match self.prompt.as_ref()? {
            SignInPrompt::OpenBrowser { url } => Some(url.to_string()),
            SignInPrompt::EnterCode { user_code, .. } => Some(user_code.clone()),
        }
    }
}

/// The whole UI state.
#[derive(Debug)]
pub struct App {
    /// The navigation stack; the bottom is always [`Screen::Index`].
    pub screens: Vec<Screen>,
    /// The open folder, once known.
    pub folder: Option<Folder>,
    /// The index, newest first.
    pub messages: Vec<MessageSummary>,
    /// The selected index entry.
    pub selected: usize,
    /// The message line at the bottom of the screen.
    pub status: String,
    /// Set when the user quits.
    pub quit: bool,
    /// The sign-in window, if it is open.
    pub sign_in: Option<SignIn>,
    /// The key bindings, which `:bind` and `:macro` change.
    pub keymap: Keymap,
    /// How the screen is drawn: formats and colors.
    pub display: crate::expando::Display,
    /// The last error reported on the message line, so that the view can
    /// tell it from other messages.
    pub error: Option<String>,
    /// `alternates`, compiled: the index asks for every row.
    alternates: Vec<regex::Regex>,
    /// Keys and functions a macro or `push` queued, played before new
    /// input.
    strokes: VecDeque<Stroke>,
    /// Keys and functions of a macro run in the sign-in window, played
    /// there while `strokes` wait for the sign-in.
    sign_in_strokes: VecDeque<Stroke>,
    /// `:` commands sent to the main loop and not yet answered.
    configuring: usize,
    /// How many queued keys have played in a row.
    played: usize,
    /// Whether the screen needs drawing again before waiting for input.
    redraw: bool,
    /// Keys to play once the folder is listed.
    after_load: Vec<Stroke>,
    /// The user cancelled the sign-in in progress: the outcome it still
    /// reports isn't shown.
    cancelled: bool,
    /// The user closed a sign-in failure window: failures that arrive
    /// late, from requests that were waiting on the same sign-in, aren't
    /// shown either. Cleared by the next prompt or the user's next action.
    dismissed: bool,
    /// The question on the message line, if any.
    pub input: Option<Input>,
    /// A message being set up for the compose menu.
    composing: Option<Composing>,
    /// The request fetching a message to reply to or forward.
    compose_fetch: Option<(RequestId, Kind, MessageId)>,
    /// The reply or forward whose message couldn't be fetched for want of
    /// a sign-in, to start again on Retry.
    compose_unfetched: Option<(MessageId, Kind)>,
    /// How to write messages.
    settings: crate::compose::Settings,
    /// The open folder gained a message while a pager was open: list it
    /// again once back in the index.
    relist_on_return: bool,
    /// The message selected before the folder was listed again in the
    /// background, and where it was: the selection goes back to it (or
    /// near where it was, if it is gone) as the listing comes in.
    reselect: Option<(MessageId, usize)>,
    /// Requests deleting drafts of messages the user gave up on, with the
    /// message; quitting waits for them.
    discarding: HashMap<RequestId, mett_core::Draft>,
    /// Whether the mailbox has been asked for the user's own addresses.
    asked_addresses: bool,
    /// The user's own addresses as the mailbox told them, kept apart from
    /// the configured ones so that reconfiguring keeps them.
    mailbox_addresses: Vec<String>,
    /// That request, while it is on its way.
    addresses_request: Option<RequestId>,
    /// A reply whose message has arrived, waiting for the user's own
    /// addresses to choose its recipients.
    awaiting_addresses: Option<(Kind, Message)>,
    /// Drafts of messages the user gave up on that couldn't be deleted,
    /// reported on exit like changes that couldn't be saved; with the
    /// message, if deleting it again after a sign-in may work.
    left_drafts: Vec<(String, Option<mett_core::Draft>)>,
    /// The pattern the index is limited to (`l`), if any.
    pub limit: Option<Limit>,
    /// The messages that the limit hides, newest first.
    hidden: Vec<MessageSummary>,
    /// The messages folded into collapsed threads.
    folded: Vec<MessageSummary>,
    /// How the index is ordered.
    pub ordering: Ordering,
    /// Where each message is in its thread, when the index shows threads.
    threads: HashMap<MessageId, Threaded>,
    /// The first messages of the collapsed threads.
    collapsed: HashSet<String>,
    /// Whether the limit showed each message it was applied to, kept
    /// across listings so that they show the same messages.
    limited: HashMap<MessageId, bool>,
    /// Where each message was in the folder's listing, to restore the
    /// order when the limit is lifted.
    positions: HashMap<MessageId, usize>,
    /// The last search (`/`), which `n` repeats.
    last_search: Option<String>,
    /// The last search in the pager, which `n` there repeats.
    last_pager_search: Option<String>,
    /// A search that found nothing yet, waiting for more of the folder.
    pending_search: Option<String>,
    /// The search began with nothing in the index: it starts at the
    /// first message, not after the selected one.
    search_from_top: bool,
    /// Messages marked for deletion, purged by `$` or when leaving.
    deleted: HashSet<MessageId>,
    /// Changes to the mailbox waiting to be made, in order. They are
    /// made one at a time, as mutt does: one may depend on another (a
    /// move gives a message a new ID; a purge mustn't overtake a copy).
    writes: VecDeque<Write>,
    /// The change being made, and its request.
    writing: Option<(RequestId, Write)>,
    /// The folder being opened instead of the open one, and the request
    /// finding it.
    switching: Option<(RequestId, FolderPath)>,
    /// The message a save or copy question is about.
    question_message: Option<MessageId>,
    /// The folder change that failed for want of a sign-in, to try again
    /// on Retry.
    unswitched: Option<FolderPath>,
    /// A folder chosen in the browser to answer a question, with its path
    /// as shown there.
    browsed: Option<(String, FolderPath)>,
    /// The browser whose folders couldn't be listed for want of a sign-in,
    /// to open again on Retry.
    unbrowsed: Option<mailbox::Choose>,
    /// Moves, copies and purges that failed for want of a sign-in, to
    /// retry.
    failed_transfers: Vec<(Transfer, Request)>,
    /// What to do once deleted messages are purged.
    then: Option<AfterPurge>,
    /// See [`Settings::account`].
    account: AccountId,
    /// See [`Settings::folder`].
    mailbox_root: String,
    /// See [`Settings::trash`].
    trash: Option<FolderPath>,
    /// See [`Settings::shortcuts`].
    shortcuts: Shortcuts,
    /// The folder open before this one, for the `-` shortcut.
    previous: Option<FolderPath>,
    /// See [`Settings::delete`].
    delete: Quad,
    /// See [`Settings::resolve`].
    resolve: bool,
    /// The folder to open, kept so that opening it can be retried.
    path: FolderPath,
    /// The request opening the folder, while it is pending.
    opening: Option<RequestId>,
    /// Terminal width, in cells.
    pub width: usize,
    /// Rows available for the index or pager body.
    pub body_height: usize,
    /// The request whose messages the index is waiting for, if any.
    listing: Option<RequestId>,
    /// The page that `listing` asked for.
    listing_page: Option<Page>,
    /// A page that failed to load, so the index is incomplete. Loading it
    /// is retried when the user reaches the end of the index.
    unlisted: Option<Page>,
    /// Changes that failed to save, by message, kept apart from the status
    /// line so that later messages can't hide them. A later successful save
    /// of the same message clears its entry.
    save_failures: HashMap<MessageId, (String, FlagsUpdate)>,
    /// Where the user asked to move while more messages were loading; the
    /// selection moves there once they arrive.
    pending: Option<Target>,
    /// The mailbox's flags, as far as mett knows, for messages whose flag
    /// changes are on their way, so a failure can be undone in the index.
    mailbox_flags: HashMap<MessageId, mett_core::Flags>,
    /// The flags the user last asked for, for messages whose updates are
    /// on their way.
    wanted_flags: HashMap<MessageId, mett_core::Flags>,
    /// Flags saved while the folder was being listed, as far as known:
    /// pages asked for before may still have the old ones. Forgotten once
    /// the listing is done.
    saved_flags: HashMap<MessageId, FlagsUpdate>,
    /// Messages moved or purged away while the folder was being listed:
    /// pages asked for before may still have them. Forgotten once the
    /// listing is done.
    removed_while_listing: HashSet<MessageId>,
    /// Moves, copies and purges that failed, by message, reported on exit
    /// like changes that couldn't be saved.
    /// Keyed by where the message was to go (none for a purge): a copy
    /// elsewhere doesn't make up for one that failed.
    transfer_failures: HashMap<(MessageId, Transfer, Option<FolderPath>), String>,
    /// The folders that mailbox paths moved or copied to turned out to be,
    /// to tell when two paths name the same folder.
    folder_ids: HashMap<FolderPath, mett_core::FolderId>,
    /// The message that last failed to open, so that opening it can be
    /// retried.
    unfetched: Option<MessageId>,
    /// The pager search that was waiting for that message.
    unfetched_search: Option<String>,
    /// When the user asks to retry, the requests that had not reported
    /// back yet; what failed among them is retried once they all have.
    retry_after: HashSet<RequestId>,
    next_request: u64,
    /// The last [`mett_core::Draft::attempt`] given to a composed message.
    last_draft: u64,
    /// How many messages to request per page.
    page_size: u32,
    /// See [`Settings::mailboxes`].
    mailboxes: Vec<MailboxEntry>,
    /// What is known of each mailbox's new mail.
    mailbox_state: HashMap<FolderPath, newmail::MailboxState>,
    /// See [`Settings::sync_interval`].
    sync_interval: Option<std::time::Duration>,
    /// Whether the timer for the next check for new mail is set.
    poll_armed: bool,
    /// Checks for new mail wait for the user to sign in again.
    poll_paused: bool,
    /// The open folder's check for new mail, on its way.
    polling: Option<newmail::Polling>,
    /// Since when the next check asks for changes.
    poll_since: Option<chrono::DateTime<chrono::Utc>>,
    /// Which timer for checks counts.
    poll_timer: u64,
    /// The messages in the index before a check had it listed again, to
    /// tell what arrived.
    known_before_refresh: Option<HashSet<MessageId>>,
    /// The request asking whether messages missing from a check's page,
    /// as old as its last, are still in the folder (on the next page).
    tie_check: Option<(RequestId, mett_core::FolderId)>,
    /// The request reading the mailboxes' counts.
    checking: Option<RequestId>,
    /// The unread mail the user has seen, by folder.
    seen: newmail::SeenByFolder,
    /// How many times the user retried after a failed sign-in: background
    /// checks made before the last retry don't hold off checking.
    poll_epoch: u64,
    /// When the folder was last listed from the start.
    listed_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl App {
    /// A new app for a `width`×`height` terminal that opens the Inbox, and
    /// the effects that start loading it.
    #[cfg(test)]
    pub fn new(width: u16, height: u16) -> (Self, Vec<Effect>) {
        let inbox = FolderPath::WellKnown(mett_core::WellKnownFolder::Inbox, Vec::new());
        Self::open(width, height, inbox, Settings::default())
    }

    /// A new app that opens the folder at `path`, and the effects that
    /// start loading it.
    pub fn open(
        width: u16,
        height: u16,
        path: FolderPath,
        settings: Settings,
    ) -> (Self, Vec<Effect>) {
        let mut app = Self {
            screens: vec![Screen::Index],
            folder: None,
            messages: Vec::new(),
            selected: 0,
            status: "Opening folder...".into(),
            quit: false,
            sign_in: None,
            keymap: Keymap::mutt_defaults(),
            display: settings.display,
            error: None,
            alternates: compile_alternates(&settings.compose.alternates),
            strokes: VecDeque::new(),
            sign_in_strokes: VecDeque::new(),
            configuring: 0,
            played: 0,
            redraw: false,
            after_load: Vec::new(),
            cancelled: false,
            dismissed: false,
            input: None,
            composing: None,
            compose_fetch: None,
            compose_unfetched: None,
            settings: settings.compose,
            relist_on_return: false,
            reselect: None,
            discarding: HashMap::new(),
            asked_addresses: false,
            mailbox_addresses: Vec::new(),
            addresses_request: None,
            awaiting_addresses: None,
            left_drafts: Vec::new(),
            limit: None,
            hidden: Vec::new(),
            folded: Vec::new(),
            ordering: settings.ordering,
            threads: HashMap::new(),
            collapsed: HashSet::new(),
            limited: HashMap::new(),
            positions: HashMap::new(),
            last_search: None,
            last_pager_search: None,
            pending_search: None,
            search_from_top: false,
            deleted: HashSet::new(),
            writes: VecDeque::new(),
            writing: None,
            failed_transfers: Vec::new(),
            unbrowsed: None,
            switching: None,
            unswitched: None,
            question_message: None,
            browsed: None,
            then: None,
            account: settings.account,
            mailbox_root: settings.folder,
            trash: settings.trash,
            shortcuts: settings.shortcuts,
            previous: None,
            delete: settings.delete,
            resolve: settings.resolve,
            path,
            opening: None,
            width: usize::from(width),
            body_height: body_height(height),
            listing: None,
            listing_page: None,
            unlisted: None,
            save_failures: HashMap::new(),
            pending: None,
            mailbox_flags: HashMap::new(),
            wanted_flags: HashMap::new(),
            saved_flags: HashMap::new(),
            removed_while_listing: HashSet::new(),
            transfer_failures: HashMap::new(),
            folder_ids: HashMap::new(),
            unfetched: None,
            unfetched_search: None,
            retry_after: HashSet::new(),
            next_request: 0,
            last_draft: 0,
            page_size: settings.page_size,
            mailboxes: settings.mailboxes,
            mailbox_state: HashMap::new(),
            sync_interval: settings.sync_interval,
            poll_armed: false,
            poll_paused: false,
            polling: None,
            poll_since: None,
            poll_timer: 0,
            known_before_refresh: None,
            tie_check: None,
            checking: None,
            seen: HashMap::new(),
            poll_epoch: 0,
            listed_at: None,
        };
        let effects = app.open_folder();
        (app, effects)
    }

    /// Opens (or retries opening) the configured folder.
    fn open_folder(&mut self) -> Vec<Effect> {
        let effect = self.request(Request::OpenFolder(self.path.clone()));
        self.opening = Some(effect_id(&effect));
        self.status = "Opening folder...".into();
        vec![effect]
    }

    /// The menu for the current screen, which selects the key bindings.
    pub fn menu(&self) -> Menu {
        if self.sign_in.is_some() {
            return Menu::SignIn;
        }
        match self.screen() {
            Screen::Index => Menu::Index,
            Screen::Pager(_) => Menu::Pager,
            Screen::Compose(_) => Menu::Compose,
            Screen::Browser(_) => Menu::Browser,
            Screen::Help(_) => Menu::Help,
        }
    }

    /// The current screen.
    pub fn screen(&self) -> &Screen {
        self.screens
            .last()
            .expect("the index is always on the stack")
    }

    /// Applies `msg` and returns the side effects to carry out.
    pub fn update(&mut self, msg: Msg) -> Vec<Effect> {
        let mut effects = self.handle(msg);
        // A search given up on (the user moved on, or left the message it
        // waited for) stops saying it is searching.
        let searching = self.pending_search.is_some()
            || self
                .screens
                .iter()
                .any(|s| matches!(s, Screen::Pager(p) if p.pending_search.is_some()));
        if !searching && self.status == "Searching..." {
            self.status.clear();
        }
        // The open folder gained messages while another screen was up:
        // list it again once the index is back.
        if self.relist_on_return && self.screen() == &Screen::Index {
            self.relist_on_return = false;
            effects.extend(self.refresh());
        }
        effects
    }

    fn handle(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Action(action) => self.on_action(action),
            Msg::Response(response) => self.on_response(response),
            Msg::SignIn(event) => self.on_sign_in(event),
            Msg::Key(key) => self.on_key_press(key),
            Msg::Configured(reconfigured) => self.reconfigure(reconfigured),
            Msg::Edited(result) => self.on_edited(result),
            Msg::Timer(Timer::RelistSentItems) => {
                let open = self.folder.as_ref().and_then(|f| f.well_known);
                if open == Some(mett_core::WellKnownFolder::SentItems) {
                    if self.screen() == &Screen::Index {
                        return self.refresh();
                    }
                    self.relist_on_return = true;
                }
                Vec::new()
            }
            Msg::Timer(Timer::Poll(timer)) => self.on_poll_timer(timer),
            Msg::Resize { width, height } => {
                self.width = usize::from(width);
                self.body_height = body_height(height);
                self.clamp_scroll();
                self.clamp_sign_in_scroll();
                Vec::new()
            }
        }
    }

    fn request(&mut self, request: Request) -> Effect {
        self.next_request += 1;
        Effect::Request(RequestId(self.next_request), request)
    }

    fn on_action(&mut self, action: Action) -> Vec<Effect> {
        // A new action replaces the previous message, as in mutt.
        self.status.clear();
        let menu = self.menu();
        if action == Action::Noop {
            return Vec::new();
        }
        // A question on the message line takes typed keys only: a macro's
        // function can't open another screen or question over it.
        if self.input.is_some() && menu != Menu::SignIn {
            self.fail("Not available in a prompt.");
            return Vec::new();
        }
        // Any new action in a menu replaces a pending move, and a reply or
        // forward still waiting for its message: the user has moved on,
        // perhaps to another message, or to help. The sign-in window's
        // keys don't move anywhere, so leave them.
        if menu != Menu::SignIn {
            self.pending = None;
            // Moving through the index, or opening a message, replaces
            // going back to the message a background refresh left (other
            // actions leave it be).
            let moving = matches!(
                action,
                Action::NextEntry
                    | Action::PreviousEntry
                    | Action::FirstEntry
                    | Action::LastEntry
                    | Action::NextPage
                    | Action::PreviousPage
                    | Action::NextThread
                    | Action::PreviousThread
                    // Opening a message chooses it too.
                    | Action::DisplayMessage
            );
            if moving && matches!(self.menu(), Menu::Index | Menu::Pager) {
                self.reselect = None;
            }
            if self.pending_search.take().is_some() && self.status == "Searching..." {
                self.status.clear();
            }
            self.compose_fetch = None;
            self.awaiting_addresses = None;
            // What the user does next may need a sign-in that fails; show
            // that.
            self.dismissed = false;
        }
        match action {
            // Help and commands work in every menu but the sign-in window.
            Action::Help if menu != Menu::SignIn => return self.open_help(),
            Action::EnterCommand if menu != Menu::SignIn => return self.ask_command(),
            _ => {}
        }
        match self.menu() {
            Menu::Index => self.on_index_action(action),
            Menu::Pager => self.on_pager_action(action),
            Menu::SignIn => self.on_sign_in_action(action),
            Menu::Compose => self.on_compose_action(action),
            Menu::Browser => self.on_browser_action(action),
            Menu::Help => self.on_help_action(action),
            Menu::Generic => Vec::new(),
        }
    }

    fn on_sign_in(&mut self, event: SignInEvent) -> Vec<Effect> {
        match event {
            SignInEvent::Prompt { account, prompt } => {
                // Sign-ins happen one at a time, so a cancelled one has
                // reported back before another prompts.
                self.cancelled = false;
                self.dismissed = false;
                let effects = match &prompt {
                    // The browser flow needs the browser: open it straight
                    // away. The device code flow is for when there is none.
                    SignInPrompt::OpenBrowser { url } => vec![Effect::OpenUrl(url.to_string())],
                    SignInPrompt::EnterCode { .. } => Vec::new(),
                };
                self.sign_in = Some(SignIn {
                    account,
                    prompt: Some(prompt),
                    failure: None,
                    scroll: 0,
                });
                effects
            }
            SignInEvent::Finished {
                outcome: Ok(()), ..
            } => {
                // Just close the window: the status line may already show
                // how the request that needed the sign-in went. But a
                // cancellation that came too late didn't happen.
                self.sign_in = None;
                if std::mem::take(&mut self.cancelled) && self.status == "Sign-in cancelled." {
                    self.status.clear();
                }
                Vec::new()
            }
            SignInEvent::Finished {
                outcome: Err(_), ..
            } if self.cancelled || self.dismissed => {
                self.cancelled = false;
                self.sign_in = None;
                Vec::new()
            }
            SignInEvent::Finished {
                account,
                outcome: Err(e),
            } => {
                // It can fail before showing a prompt.
                let window = self.sign_in.get_or_insert(SignIn {
                    account,
                    prompt: None,
                    failure: None,
                    scroll: 0,
                });
                window.failure = Some(e);
                // The failure replaces the prompt, which may have been
                // scrolled further than the window is long now.
                self.clamp_sign_in_scroll();
                Vec::new()
            }
        }
    }

    fn on_sign_in_action(&mut self, action: Action) -> Vec<Effect> {
        let Some(window) = &self.sign_in else {
            return Vec::new();
        };
        let failed = window.failure.is_some();
        match action {
            Action::Exit if failed => {
                self.sign_in = None;
                self.dismissed = true;
                // Nor restart, on a later Retry, a reply or forward whose
                // message couldn't be fetched.
                self.compose_unfetched = None;
                // The user gave up: don't let a retry that was waiting
                // start another sign-in.
                self.retry_after.clear();
            }
            Action::Exit => {
                self.sign_in = None;
                self.cancelled = true;
                // Don't let a retry that was waiting start another sign-in.
                self.retry_after.clear();
                self.status = "Sign-in cancelled.".into();
                return vec![Effect::CancelSignIn];
            }
            Action::OpenBrowser if failed => {}
            Action::OpenBrowser => {
                let url = match &window.prompt {
                    Some(SignInPrompt::OpenBrowser { url }) => url.to_string(),
                    Some(SignInPrompt::EnterCode {
                        verification_uri, ..
                    }) => verification_uri.clone(),
                    None => return Vec::new(),
                };
                return vec![Effect::OpenUrl(url)];
            }
            Action::CopyLink => {
                let Some(text) = window.copyable() else {
                    return Vec::new();
                };
                // The terminal may not allow it, and can't say whether it
                // did.
                self.status = "Copied to the clipboard, if your terminal allows it.".into();
                return vec![Effect::Copy(text)];
            }
            Action::NextLine | Action::PreviousLine => {
                self.clamp_sign_in_scroll();
                if let Some(window) = &mut self.sign_in {
                    window.scroll = if action == Action::NextLine {
                        window.scroll + 1
                    } else {
                        window.scroll.saturating_sub(1)
                    };
                }
                self.clamp_sign_in_scroll();
            }
            Action::Retry if failed => {
                self.sign_in = None;
                // Requests that needed the sign-in may not have reported
                // back yet: retry them once they have. When quitting, only
                // saving changes and deleting drafts matter, and only their
                // responses are still handled.
                self.retry_after = if self.quit {
                    self.writing
                        .iter()
                        .map(|(id, _)| *id)
                        .chain(self.discarding.keys().copied())
                        .collect()
                } else {
                    self.in_flight()
                };
                return self.retry();
            }
            _ => {}
        }
        Vec::new()
    }

    /// The requests that [`Self::retry`] can repeat and that are still
    /// waiting for their responses.
    fn in_flight(&self) -> HashSet<RequestId> {
        let fetching = match self.screen() {
            Screen::Pager(Pager {
                message: None,
                request,
                ..
            }) => Some(*request),
            _ => None,
        };
        self.opening
            .into_iter()
            .chain(self.listing)
            .chain(fetching)
            .chain(self.writing.as_ref().map(|(id, _)| *id))
            .chain(self.delivery_request())
            .chain(self.compose_fetch.as_ref().map(|(id, _, _)| *id))
            .chain(self.discarding.keys().copied())
            .chain(self.browser_request())
            .chain(self.switching.as_ref().map(|(id, _)| *id))
            .collect()
    }

    /// Retries once the last of the requests that were pending when the
    /// user asked to retry has reported back.
    fn retry_if_requested(&mut self, id: RequestId) -> Vec<Effect> {
        if self.retry_after.remove(&id) && self.retry_after.is_empty() {
            self.retry()
        } else {
            Vec::new()
        }
    }

    /// Keeps the sign-in window's scroll position within its text, which
    /// a taller or wider terminal makes shorter.
    fn clamp_sign_in_scroll(&mut self) {
        // The window covers all but the help line, and has a border above
        // and below its text.
        let room = (self.body_height + CHROME_ROWS).saturating_sub(3);
        let width = self.width;
        if let Some(window) = &mut self.sign_in {
            let last = window.rows(width).len().saturating_sub(room);
            window.scroll = window.scroll.min(last);
        }
    }

    /// Retries what failed for want of a sign-in: opening the folder,
    /// listing the rest of it, opening the selected message and saving
    /// changes.
    fn retry(&mut self) -> Vec<Effect> {
        // Signed in again: checking for new mail can go on.
        self.poll_paused = false;
        self.poll_epoch += 1;
        if self.quit {
            let mut effects = self.retry_saves();
            effects.extend(self.retry_discards());
            effects.extend(self.retry_transfers());
            return effects;
        }
        // Everything that failed is retried, not just the first thing.
        // Saves and moves don't depend on the open folder.
        let mut effects = self.retry_delivery();
        effects.extend(self.retry_discards());
        effects.extend(self.retry_compose_fetch());
        effects.extend(self.retry_saves());
        effects.extend(self.retry_transfers());
        effects.extend(self.retry_switch());
        // The folders are listed whether or not one is open.
        effects.extend(self.retry_browser());
        if self.folder.is_none() {
            if self.opening.is_none() {
                effects.extend(self.open_folder());
            }
            return effects;
        }
        effects.extend(self.retry_listing());
        // Open the message that failed to open, if it is still the one
        // selected. (Opening another message forgets the failure.) The
        // screen, not the menu: another sign-in window may be over it.
        let failed = self.unfetched.take();
        let search = self.unfetched_search.take();
        let selected = self.messages.get(self.selected).map(|s| &s.id);
        if failed.is_some() && failed.as_ref() == selected && self.screen() == &Screen::Index {
            effects.extend(self.open_selected());
            if let Some(Screen::Pager(pager)) = self.screens.last_mut() {
                pager.pending_search = search;
            }
        }
        effects
    }

    /// Whether a flag change of `message` is waiting its turn or on its way.
    fn flags_pending(&self, message: &MessageId) -> bool {
        self.writing
            .iter()
            .map(|(_, write)| write)
            .chain(&self.writes)
            .any(|write| matches!(write, Write::Flags(id, _) if id == message))
    }

    /// Makes again the changes that failed to save, which the index undid.
    /// (Their failures are cleared once that succeeds.)
    fn retry_saves(&mut self) -> Vec<Effect> {
        let mut unsaved: Vec<(MessageId, FlagsUpdate)> = self
            .save_failures
            .iter()
            .map(|(id, (_, update))| (id.clone(), *update))
            .collect();
        unsaved.sort_by(|a, b| a.0.cmp(&b.0));
        unsaved
            .into_iter()
            .flat_map(|(message, update)| {
                // Its folder may not be open any more: save it all the
                // same, once (a retry may come again before it is done).
                if let Some(summary) = self.summary(&message) {
                    // The mailbox may have it after all (the response was
                    // lost): a fresh listing shows it, and that settles it.
                    let mut flags = summary.flags;
                    update.apply(&mut flags);
                    if flags == summary.flags && !self.flags_pending(&message) {
                        self.save_failures.remove(&message);
                        return Vec::new();
                    }
                    self.update_flags(message, update)
                } else if self.flags_pending(&message) {
                    Vec::new()
                } else {
                    self.write(Write::Flags(message, update))
                }
            })
            .collect()
    }

    fn on_index_action(&mut self, action: Action) -> Vec<Effect> {
        if let Some(effects) = self.on_mailbox_action(action) {
            return effects;
        }
        if let Some(effects) = self.on_thread_action(action) {
            return effects;
        }
        if let Some(effects) = self.on_search_action(action) {
            return effects;
        }
        let last = self.messages.len().saturating_sub(1);
        let page = self.body_height.max(1);
        match action {
            Action::Quit => return self.quit(),
            Action::NextEntry if self.selected < last => self.selected += 1,
            Action::NextEntry if self.more_to_list() => {
                return self.wait_for(self.next_entry());
            }
            Action::NextEntry => self.fail("You are on the last message."),
            Action::PreviousEntry if self.selected > 0 => self.selected -= 1,
            Action::PreviousEntry => self.fail("You are on the first message."),
            Action::FirstEntry => self.selected = 0,
            Action::LastEntry => {
                self.selected = last;
                if self.more_to_list() {
                    return self.wait_for(Target::Last);
                }
            }
            Action::NextPage => {
                let target = self.selected + page;
                self.selected = target.min(last);
                if target > last && self.more_to_list() {
                    let target = match self.messages.get(last) {
                        Some(message) => Target::After(message.id.clone(), target - last),
                        None => Target::Entry(target),
                    };
                    return self.wait_for(target);
                }
            }
            Action::PreviousPage => self.selected = self.selected.saturating_sub(page),
            // Opening the folder failed: try again. (While it is still
            // opening, there is nothing to display yet.)
            Action::DisplayMessage if self.folder.is_none() && self.opening.is_none() => {
                return self.open_folder();
            }
            Action::DisplayMessage if self.folder.is_none() => {
                self.status = "Opening folder...".into();
            }
            Action::DisplayMessage => return self.open_selected(),
            Action::Mail => return self.start_compose(Kind::New),
            Action::Reply => return self.start_compose(Kind::Reply),
            Action::GroupReply => return self.start_compose(Kind::GroupReply),
            Action::Forward => return self.start_compose(Kind::Forward),
            _ => {}
        }
        Vec::new()
    }

    fn on_pager_action(&mut self, action: Action) -> Vec<Effect> {
        if let Some(effects) = self.on_mailbox_action(action) {
            return effects;
        }
        if let Some(effects) = self.on_thread_action(action) {
            return effects;
        }
        if let Some(effects) = self.on_search_action(action) {
            return effects;
        }
        let page = self.body_height.max(1);
        match action {
            Action::Quit => return self.quit(),
            Action::Exit => {
                self.screens.pop();
            }
            // As in mutt, j and k in the pager show the next or previous
            // message.
            Action::NextEntry if self.selected + 1 >= self.messages.len() => {
                if self.more_to_list() {
                    return self.wait_for(self.next_entry());
                }
                self.fail("You are on the last message.");
            }
            Action::PreviousEntry if self.selected == 0 => {
                self.fail("You are on the first message.");
            }
            Action::NextEntry | Action::PreviousEntry => {
                if action == Action::NextEntry {
                    self.selected += 1;
                } else {
                    self.selected -= 1;
                }
                self.screens.pop();
                return self.open_selected();
            }
            Action::NextLine => self.scroll_by(1),
            Action::PreviousLine => self.scroll_by(-1),
            Action::NextPage => self.scroll_by(page as isize),
            Action::PreviousPage => self.scroll_by(-(page as isize)),
            Action::Mail => return self.start_compose(Kind::New),
            Action::Reply => return self.start_compose(Kind::Reply),
            Action::GroupReply => return self.start_compose(Kind::GroupReply),
            Action::Forward => return self.start_compose(Kind::Forward),
            _ => {}
        }
        Vec::new()
    }

    /// Scrolls the pager by `rows`, within the message.
    fn scroll_by(&mut self, rows: isize) {
        if let Some(Screen::Pager(pager)) = self.screens.last_mut() {
            pager.scroll = pager.scroll.saturating_add_signed(rows);
        }
        self.clamp_scroll();
    }

    /// Keeps the pager's scroll position within the wrapped message, so that
    /// the last page stays full and scrolling back responds at once.
    fn clamp_scroll(&mut self) {
        let (width, height) = (self.width, self.body_height);
        // Pagers under other screens too: their rows (and search
        // highlights) must match the width they will be drawn at.
        for screen in &mut self.screens {
            if let Screen::Pager(pager) = screen {
                pager.layout(width);
                pager.scroll = pager.scroll.min(pager.rows.len().saturating_sub(height));
            }
        }
        // A taller help screen shows more lines, not empty space.
        if let Some(Screen::Help(help)) = self.screens.last() {
            let last = self.help_lines(help.menu).len().saturating_sub(height);
            if let Some(Screen::Help(help)) = self.screens.last_mut() {
                help.scroll = help.scroll.min(last);
            }
        }
    }

    /// Opens the pager on the selected message.
    fn open_selected(&mut self) -> Vec<Effect> {
        let Some(summary) = self.messages.get(self.selected) else {
            self.fail("No messages.");
            return Vec::new();
        };
        let id = summary.id.clone();
        self.unfetched = None;
        self.unfetched_search = None;
        let fetch = self.request(Request::FetchMessage(id.clone()));
        self.screens.push(Screen::Pager(Pager {
            id,
            message: None,
            lines: Vec::new(),
            rows: Vec::new(),
            rows_width: None,
            request: effect_id(&fetch),
            scroll: 0,
            read_chosen: false,
            line_rows: Vec::new(),
            row_ranges: Vec::new(),
            search_match: None,
            highlight: None,
            pending_search: None,
            body_start: 0,
            signature: None,
        }));
        vec![fetch]
    }

    fn on_response(&mut self, response: Response) -> Vec<Effect> {
        let id = response.id;
        let mut effects = self.handle_response(response);
        effects.extend(self.retry_if_requested(id));
        effects
    }

    fn handle_response(&mut self, Response { id, event }: Response) -> Vec<Effect> {
        match event {
            Event::FolderOpened(result)
                if self
                    .switching
                    .as_ref()
                    .is_some_and(|(request, _)| *request == id) =>
            {
                return self.on_switched(result);
            }
            Event::FolderOpened(_) if self.opening != Some(id) => {}
            Event::FolderOpened(Ok(folder)) => {
                self.opening = None;
                self.poll_paused = false;
                self.status = format!("Loading {}...", folder.name);
                self.folder = Some(folder);
                self.visited();
                let mut effects = self.list(Page::first(self.page_size));
                // Then the mailboxes' new mail, and checking for more,
                // unless checking is off.
                if self.sync_interval.is_some() {
                    effects.extend(self.check_mailboxes());
                }
                effects.extend(self.arm_poll());
                // Before the user can reply: their own addresses, so that a
                // reply to their own mail goes to its recipients (the
                // sign-in name may not be one of them).
                if !self.asked_addresses {
                    self.asked_addresses = true;
                    let request = self.request(Request::OwnAddresses);
                    self.addresses_request = Some(effect_id(&request));
                    effects.push(request);
                }
                return effects;
            }
            Event::FolderPolled(result) => return self.on_polled(id, result),
            Event::MessagesListed(result) if self.listing == Some(id) => match result {
                Ok(page) => {
                    self.poll_paused = false;
                    let mut messages = page.messages;
                    messages.retain(|m| !self.removed_while_listing.contains(&m.id));
                    self.overlay_wanted_flags(&mut messages);
                    self.add_listed(messages);
                    self.see_index();
                    self.reselect(page.next.is_none());
                    let mut effects = Vec::new();
                    if let Some(cursor) = page.next {
                        // Keep loading until the whole folder is listed.
                        effects = self.list(Page::after(cursor, self.page_size));
                    } else {
                        self.listing = None;
                        self.listing_page = None;
                        self.saved_flags.clear();
                        self.removed_while_listing.clear();
                        self.forget_missing_marks();
                        self.loaded();
                        self.relisted();
                        // Clear the "Loading" message, but not an error that
                        // was reported while the folder was loading.
                        if self.status.starts_with("Loading ") {
                            self.status.clear();
                        }
                    }
                    effects.extend(self.apply_pending());
                    // A search waiting for more messages looks again.
                    effects.extend(self.continue_search());
                    return effects;
                }
                Err(e) => {
                    self.pending = None;
                    self.reselect = None;
                    self.listing = None;
                    self.known_before_refresh = None;
                    self.unlisted = self.listing_page.take();
                    // Pages asked for again come after the saves.
                    self.saved_flags.clear();
                    self.removed_while_listing.clear();
                    // A search waiting for the page goes on once a sign-in
                    // lets it load.
                    if !matches!(e, Error::Auth(_)) {
                        self.pending_search = None;
                    }
                    // Keys waiting for the folder play anyway, unless a
                    // sign-in is to load it after all.
                    if !matches!(e, Error::Auth(_)) {
                        self.loaded();
                    }
                    self.report(e);
                }
            },
            Event::MessageFetched(result)
                if self
                    .compose_fetch
                    .as_ref()
                    .is_some_and(|(request, _, _)| *request == id) =>
            {
                let (_, kind, message) = self.compose_fetch.take().expect("checked above");
                return self.on_compose_fetched(kind, message, result);
            }
            Event::MessageFetched(result) => return self.on_message_fetched(id, result),
            Event::FoldersListed(result) => return self.on_folders_listed(id, result),
            Event::MailboxesChecked(checked) => return self.on_mailboxes_checked(id, checked),
            Event::StillThere(result) => return self.on_still_there(id, result),
            Event::Transferred(result) => {
                // The messages leave the index, or it is listed again; the
                // new IDs matter to changes still to make, or that failed.
                let moved = self.writing.as_ref().is_some_and(|(request, write)| {
                    *request == id && matches!(write, Write::Transfer(Transfer::Save, _))
                });
                let result = result.map(|(to, results)| {
                    if moved {
                        // Moved now: an earlier failure to move them is
                        // done with, not to follow them.
                        for (old, new) in &results {
                            if new.is_ok() {
                                self.transfer_failures.retain(|(id, transfer, _), _| {
                                    !(id == old && *transfer == Transfer::Save)
                                });
                            }
                        }
                        let renamed: Vec<_> = results
                            .iter()
                            .filter_map(|(old, new)| Some((old.clone(), new.clone().ok()?)))
                            .collect();
                        self.follow_moves(&renamed);
                    }
                    let results = results
                        .into_iter()
                        .map(|(id, result)| (id, result.map(drop)))
                        .collect();
                    (Some(to), results)
                });
                return self.on_transferred(id, result);
            }
            Event::Purged(result) => {
                return self.on_transferred(id, result.map(|results| (None, results)));
            }
            Event::Sent(result) => return self.on_delivered(id, result),
            Event::DraftSaved(result) => return self.on_delivered(id, result.map(drop)),
            // Without them, the configured ones do.
            Event::OwnAddresses(result) => {
                if self.addresses_request == Some(id) {
                    self.addresses_request = None;
                }
                let failed = match result {
                    Ok(addresses) => {
                        self.mailbox_addresses = addresses
                            .into_iter()
                            .map(|address| address.to_lowercase())
                            .collect();
                        self.add_mailbox_addresses();
                        None
                    }
                    // Asked again by the next reply.
                    Err(e) => {
                        self.asked_addresses = false;
                        Some(e)
                    }
                };
                // A reply that waited for them can choose its recipients,
                // with the addresses the user configured if the mailbox
                // couldn't tell.
                if let Some((kind, message)) = self.awaiting_addresses.take() {
                    let effects = self.begin(kind, Some(message));
                    if let Some(e) = failed {
                        self.fail(format!(
                            "Couldn't learn your own addresses ({e}); a reply to your own mail may need its recipients checked."
                        ));
                    }
                    return effects;
                }
            }
            Event::KeptDiscarded(result) => {
                let draft = self.discarding.remove(&id);
                if let Err(e) = result {
                    let left = format!("A draft of the message is left in Drafts: {e}");
                    // Retry deletes it once signed in.
                    let retry = draft.filter(|_| matches!(e, Error::Auth(_)));
                    self.left_drafts.push((left.clone(), retry));
                    self.fail(left);
                }
            }
            Event::FlagsUpdated(result) => {
                let Some((_, Write::Flags(message, update))) =
                    self.writing.take_if(|(request, _)| *request == id)
                else {
                    return Vec::new();
                };
                let mut effects = self.on_flags_updated(message, update, result);
                effects.extend(self.write_next());
                return effects;
            }
            Event::FolderOpened(Err(e)) => {
                self.opening = None;
                // A search waiting for the folder goes on once a sign-in
                // lets it open.
                if !matches!(e, Error::Auth(_)) {
                    self.pending_search = None;
                }
                // Keys waiting for the folder play anyway, unless a sign-in
                // is to open it after all.
                if !matches!(e, Error::Auth(_)) {
                    self.loaded();
                }
                self.report(e);
            }
            Event::MessagesListed(_) => {}
        }
        Vec::new()
    }

    fn on_message_fetched(
        &mut self,
        id: RequestId,
        result: mett_core::Result<Message>,
    ) -> Vec<Effect> {
        // Only the pager that asked for it cares; anything else is a stale
        // response for a message the user already left. The pager may be
        // under the compose menu, if the user replied while it loaded.
        let Some(position) = self
            .screens
            .iter()
            .rposition(|s| matches!(s, Screen::Pager(p) if p.request == id))
        else {
            return Vec::new();
        };
        let message = match result {
            Ok(message) => message,
            Err(e) => {
                if let Screen::Pager(pager) = self.screens.remove(position) {
                    self.unfetched = Some(pager.id);
                    // A search waiting for it runs when it opens again.
                    self.unfetched_search = pager.pending_search;
                }
                self.report(e);
                return Vec::new();
            }
        };
        let Screen::Pager(pager) = &mut self.screens[position] else {
            unreachable!("found above");
        };
        pager.lines = text::pager_lines(&message);
        pager.body_start = pager
            .lines
            .iter()
            .position(String::is_empty)
            .map_or(pager.lines.len(), |blank| blank + 1);
        pager.signature = pager.lines[pager.body_start..]
            .iter()
            .position(|line| line == "-- ")
            .map(|at| pager.body_start + at);
        pager.rows_width = None;
        pager.layout(self.width);
        let message_id = message.summary.id.clone();
        let chosen = pager.read_chosen;
        pager.message = Some(message);
        if let Some(text) = pager.pending_search.take() {
            self.search_pager(position, &text);
        }
        // Now that it has been shown, mark it read, as mutt does (unless
        // the user said otherwise meanwhile).
        if chosen {
            return Vec::new();
        }
        self.mark_read(message_id)
    }

    /// Marks a message read, updating the index straight away.
    fn mark_read(&mut self, message_id: MessageId) -> Vec<Effect> {
        let update = FlagsUpdate {
            read: Some(true),
            flagged: None,
        };
        self.update_flags(message_id, update)
    }

    /// Changes a message's flags, updating the index straight away. Nothing
    /// is sent if they already are as asked.
    pub(super) fn update_flags(
        &mut self,
        message_id: MessageId,
        update: FlagsUpdate,
    ) -> Vec<Effect> {
        // A change on its way for a message mett had no flags for (sent
        // while its folder was closed) is what the user wants too: the
        // listing shown may predate it.
        let in_flight = match &self.writing {
            Some((_, Write::Flags(id, sent)))
                if *id == message_id && !self.mailbox_flags.contains_key(&message_id) =>
            {
                Some(*sent)
            }
            _ => None,
        };
        // The message may be hidden by the limit, when retrying.
        let Some(summary) = self.summary_mut(&message_id) else {
            return Vec::new();
        };
        let before = summary.flags;
        let mut flags = before;
        if let Some(sent) = in_flight {
            sent.apply(&mut flags);
        }
        update.apply(&mut flags);
        if flags == before {
            return Vec::new();
        }
        summary.flags = flags;
        // What the mailbox has, as far as mett knows, while changes are
        // on their way.
        self.mailbox_flags
            .entry(message_id.clone())
            .or_insert(before);
        self.wanted_flags.insert(message_id.clone(), flags);
        // Changing back what failed to change gives up on it.
        if let Some((_, failed)) = self.save_failures.get_mut(&message_id) {
            if failed
                .read
                .is_some_and(|read| update.read.is_some_and(|new| new != read))
            {
                failed.read = None;
            }
            if failed
                .flagged
                .is_some_and(|flagged| update.flagged.is_some_and(|new| new != flagged))
            {
                failed.flagged = None;
            }
            if *failed == FlagsUpdate::default() {
                self.save_failures.remove(&message_id);
            }
        }
        // A change still waiting its turn takes this one along.
        let queued = self.writes.iter_mut().find_map(|write| match write {
            Write::Flags(id, queued) if *id == message_id => Some(queued),
            _ => None,
        });
        if let Some(queued) = queued {
            *queued = merged(*queued, update);
            return Vec::new();
        }
        self.write(Write::Flags(message_id, update))
    }

    /// What to send of a flag change, now that its turn has come: what the
    /// user wants and the mailbox doesn't have (which may be nothing, or
    /// include an earlier change that failed), or `update` for a message
    /// mett knows nothing more about.
    pub(super) fn flags_to_send(&self, message: &MessageId, update: FlagsUpdate) -> FlagsUpdate {
        match (
            self.wanted_flags.get(message),
            self.mailbox_flags.get(message),
        ) {
            (Some(wanted), Some(mailbox)) => FlagsUpdate {
                read: (wanted.read != mailbox.read).then_some(wanted.read),
                flagged: (wanted.flagged != mailbox.flagged).then_some(wanted.flagged),
            },
            _ => update,
        }
    }

    /// Handles the outcome of a flag change to a message whose other flags
    /// mett doesn't know: the index, if it shows the message now, takes
    /// just the fields that were saved.
    fn on_unknown_flags_updated(
        &mut self,
        message: MessageId,
        update: FlagsUpdate,
        result: mett_core::Result<()>,
    ) -> Vec<Effect> {
        match result {
            Ok(()) => {
                if let Some(summary) = self.summary_mut(&message) {
                    update.apply(&mut summary.flags);
                }
                // Pages of a listing on their way may predate it.
                if self.fetching_index() {
                    let saved = self.saved_flags.get(&message).copied().unwrap_or_default();
                    self.saved_flags
                        .insert(message.clone(), merged(saved, update));
                }
                if let Some((_, failed)) = self.save_failures.get_mut(&message) {
                    if update.read.is_some() {
                        failed.read = None;
                    }
                    if update.flagged.is_some() {
                        failed.flagged = None;
                    }
                    if *failed == FlagsUpdate::default() {
                        self.save_failures.remove(&message);
                    }
                }
            }
            Err(e) => {
                let failed = self
                    .save_failures
                    .get(&message)
                    .map_or(update, |(_, before)| merged(*before, update));
                self.save_failures.insert(message, (e.to_string(), failed));
                self.report(e);
            }
        }
        Vec::new()
    }

    /// Handles the outcome of a flag change.
    fn on_flags_updated(
        &mut self,
        message: MessageId,
        update: FlagsUpdate,
        result: mett_core::Result<()>,
    ) -> Vec<Effect> {
        if result.is_ok() {
            self.changed_here(&message);
        }
        // Sent for a message mett has no flags for (its folder wasn't
        // open): only the fields it changed are known.
        let Some(mut mailbox) = self.mailbox_flags.get(&message).copied() else {
            return self.on_unknown_flags_updated(message, update, result);
        };
        let saved = result.is_ok();
        match result {
            Ok(()) => {
                update.apply(&mut mailbox);
                // Whatever failed before of what was saved now is done.
                if let Some((_, failed)) = self.save_failures.get_mut(&message) {
                    if update.read.is_some() {
                        failed.read = None;
                    }
                    if update.flagged.is_some() {
                        failed.flagged = None;
                    }
                    if *failed == FlagsUpdate::default() {
                        self.save_failures.remove(&message);
                    }
                }
            }
            Err(e) => {
                // What the user has changed back meanwhile isn't wanted any
                // more; the rest is kept with what failed before, to try
                // again together.
                let update = match self.wanted_flags.get(&message) {
                    Some(wanted) => FlagsUpdate {
                        read: update.read.filter(|read| *read == wanted.read),
                        flagged: update.flagged.filter(|flagged| *flagged == wanted.flagged),
                    },
                    None => update,
                };
                let failed = self
                    .save_failures
                    .get(&message)
                    .map_or(update, |(_, before)| merged(*before, update));
                if failed != FlagsUpdate::default() {
                    self.save_failures
                        .insert(message.clone(), (e.to_string(), failed));
                }
                self.report(e);
            }
        }
        let queued = self
            .writes
            .iter()
            .any(|write| matches!(write, Write::Flags(id, _) if *id == message));
        if queued {
            // The next change sends whatever still differs, which
            // includes what just failed. Pages of the listing on its way
            // may predate what was saved.
            if saved && self.fetching_index() {
                self.saved_flags.insert(message.clone(), whole(mailbox));
            }
            self.mailbox_flags.insert(message, mailbox);
        } else {
            self.flags_settled(&message, mailbox, saved);
        }
        Vec::new()
    }

    /// Forgets the flag bookkeeping of a message whose changes are done,
    /// and shows what the mailbox has: after a failure, so the index is
    /// right and doing it again tries again; after a save, in case a
    /// listing taken before it showed the old flags.
    pub(super) fn flags_settled(
        &mut self,
        message: &MessageId,
        mailbox: mett_core::Flags,
        saved: bool,
    ) {
        self.mailbox_flags.remove(message);
        self.wanted_flags.remove(message);
        if let Some(summary) = self.summary_mut(message) {
            summary.flags = mailbox;
        }
        // Pages of the listing on its way may predate the save (or an
        // earlier one, which a failure since doesn't undo).
        if self.fetching_index() && (saved || self.saved_flags.contains_key(message)) {
            self.saved_flags.insert(message.clone(), whole(mailbox));
        }
    }

    /// Shows freshly listed messages with the flags the user asked for,
    /// while those changes are on their way, or that were saved while the
    /// folder was listed: the listing may predate them.
    fn overlay_wanted_flags(&self, messages: &mut [MessageSummary]) {
        for message in messages {
            if let Some(flags) = self.wanted_flags.get(&message.id) {
                message.flags = *flags;
            } else if let Some(saved) = self.saved_flags.get(&message.id) {
                saved.apply(&mut message.flags);
            }
        }
    }

    /// The summary of a message in the folder, shown or hidden by the
    /// limit.
    fn summary(&self, id: &MessageId) -> Option<&MessageSummary> {
        self.messages
            .iter()
            .chain(&self.hidden)
            .chain(&self.folded)
            .find(|s| &s.id == id)
    }

    /// The summary of a message in the folder, to change.
    fn summary_mut(&mut self, id: &MessageId) -> Option<&mut MessageSummary> {
        self.messages
            .iter_mut()
            .chain(&mut self.hidden)
            .chain(&mut self.folded)
            .find(|s| &s.id == id)
    }

    /// Every message of the folder listed so far, shown or not.
    pub fn folder_messages(&self) -> impl Iterator<Item = &MessageSummary> {
        self.messages.iter().chain(&self.hidden).chain(&self.folded)
    }

    /// Whether part of the folder could not be listed. The index then
    /// shows only the messages loaded so far.
    pub fn incomplete(&self) -> bool {
        self.unlisted.is_some()
    }

    /// Takes in the settings after a `:` command changed them. The account
    /// stays the same.
    /// Counts the addresses the mailbox reported as the user's own.
    fn add_mailbox_addresses(&mut self) {
        for address in &self.mailbox_addresses {
            if !self.settings.me.contains(address) {
                self.settings.me.push(address.clone());
            }
        }
    }

    pub(super) fn apply_settings(&mut self, settings: Settings) {
        self.page_size = settings.page_size;
        self.alternates = compile_alternates(&settings.compose.alternates);
        self.settings = settings.compose;
        // The configuration doesn't know what the mailbox told.
        self.add_mailbox_addresses();
        self.mailbox_root = settings.folder;
        self.trash = settings.trash;
        self.delete = settings.delete;
        self.resolve = settings.resolve;
        self.shortcuts = settings.shortcuts;
        self.display = settings.display;
        self.apply_ordering(settings.ordering);
        self.apply_mailboxes(settings.mailboxes, settings.sync_interval);
    }

    /// Lists the open folder again from the start, to show messages that
    /// arrived in it.
    fn relist(&mut self) -> Vec<Effect> {
        self.reselect = None;
        // As in mutt, the limit's choice of messages stays: only new ones
        // are matched against it.
        if self.limit.is_some() {
            let shown = self
                .messages
                .iter()
                .chain(&self.folded)
                .map(|m| (m.id.clone(), true));
            let hidden = self.hidden.iter().map(|m| (m.id.clone(), false));
            self.limited.extend(shown.chain(hidden));
        }
        self.messages.clear();
        self.saved_flags.clear();
        self.removed_while_listing.clear();
        self.hidden.clear();
        self.folded.clear();
        self.threads.clear();
        self.positions.clear();
        self.selected = 0;
        self.pending = None;
        self.unlisted = None;
        self.list(Page::first(self.page_size))
    }

    /// Lists the open folder again in the background, staying on the
    /// selected message.
    fn refresh(&mut self) -> Vec<Effect> {
        // A refresh still on its way (the user hasn't moved since, or it
        // would be forgotten) knows better than the index it cleared.
        let selected = self.reselect.clone().or_else(|| {
            self.messages
                .get(self.selected)
                .map(|m| (m.id.clone(), self.selected))
        });
        let effects = self.relist();
        self.reselect = selected;
        effects
    }

    /// Selects again the message selected before a [`Self::refresh`], once
    /// it is listed; if it is gone by the end of the folder, the one now
    /// where it was.
    fn reselect(&mut self, done: bool) {
        let Some((id, at)) = &self.reselect else {
            return;
        };
        if let Some(found) = self.messages.iter().position(|m| &m.id == id) {
            self.selected = found;
        } else if done {
            self.selected = (*at).min(self.messages.len().saturating_sub(1));
        } else {
            return;
        }
        self.reselect = None;
    }

    /// Lists `page` of the open folder.
    fn list(&mut self, page: Page) -> Vec<Effect> {
        let Some(folder) = &self.folder else {
            return Vec::new();
        };
        let list = self.request(Request::ListMessages {
            folder: folder.id.clone(),
            page: page.clone(),
        });
        if page.cursor().is_none() {
            self.listing_started();
        }
        self.listing = Some(effect_id(&list));
        self.listing_page = Some(page);
        vec![list]
    }

    /// Whether more messages are loading or could be loaded.
    fn more_to_list(&self) -> bool {
        self.listing.is_some() || self.unlisted.is_some()
    }

    /// The entry after the selected one, or the first if the index shows
    /// none yet (a limit may hide all the messages loaded so far).
    fn next_entry(&self) -> Target {
        match self.messages.get(self.selected) {
            Some(message) => Target::After(message.id.clone(), 1),
            None => Target::Entry(0),
        }
    }

    /// Remembers that the user asked for the next message, and makes sure
    /// more messages are on their way.
    fn wait_for(&mut self, target: Target) -> Vec<Effect> {
        self.pending = Some(target);
        self.status = "Loading more messages...".into();
        self.retry_listing()
    }

    /// Moves towards a pending target now that more messages have arrived.
    /// An entry target is reached once it is loaded, or settles on the last
    /// message once the folder is fully loaded; the last-message target
    /// follows the end of the index until loading finishes.
    fn apply_pending(&mut self) -> Vec<Effect> {
        let Some(target) = self.pending.clone() else {
            return Vec::new();
        };
        let last = self.messages.len().saturating_sub(1);
        let loading = self.listing.is_some();
        let selected = match target {
            Target::NextThread => match self.next_thread_root() {
                Some(next) => next,
                None if loading => return Vec::new(),
                None => {
                    self.pending = None;
                    self.fail("You are on the last thread.");
                    return Vec::new();
                }
            },
            Target::After(message, steps) => {
                let from = self.messages.iter().position(|m| m.id == message);
                match from.map(|from| from + steps) {
                    Some(entry) if entry < self.messages.len() => entry,
                    _ if loading => return Vec::new(),
                    _ => last,
                }
            }
            // Loaded (the index may still be empty, with a limit hiding
            // all the messages loaded so far).
            Target::Entry(entry) if entry < self.messages.len() => entry,
            Target::Entry(_) | Target::Last if !loading => last,
            Target::Last => {
                self.selected = last;
                return Vec::new();
            }
            Target::Entry(_) => return Vec::new(),
        };
        self.pending = None;
        if self.status.starts_with("Loading ") {
            self.status.clear();
        }
        if selected == self.selected {
            return Vec::new();
        }
        self.selected = selected;
        // The screen, not the menu: the sign-in window may be over it.
        match self.screen() {
            Screen::Index | Screen::Compose(_) | Screen::Browser(_) | Screen::Help(_) => Vec::new(),
            Screen::Pager(_) => {
                self.screens.pop();
                self.open_selected()
            }
        }
    }

    /// Deletes again the drafts that couldn't be deleted for want of a
    /// sign-in.
    fn retry_discards(&mut self) -> Vec<Effect> {
        let (retry, left): (Vec<_>, Vec<_>) = std::mem::take(&mut self.left_drafts)
            .into_iter()
            .partition(|(_, draft)| draft.is_some());
        self.left_drafts = left;
        retry
            .into_iter()
            .filter_map(|(_, draft)| draft)
            .flat_map(|draft| self.discard_kept(draft))
            .collect()
    }

    /// Changes that could not be saved, one per message.
    pub fn save_failures(&self) -> Vec<String> {
        let mut failures: Vec<_> = self
            .save_failures
            .values()
            .map(|(error, _)| error.clone())
            .chain(self.transfer_failures.values().cloned())
            .chain(self.left_drafts.iter().map(|(left, _)| left.clone()))
            .collect();
        failures.sort();
        failures
    }

    /// Retries loading the rest of an incomplete folder.
    fn retry_listing(&mut self) -> Vec<Effect> {
        if self.listing.is_some() {
            return Vec::new();
        }
        let Some(page) = self.unlisted.take() else {
            return Vec::new();
        };
        self.status = "Loading more messages...".into();
        self.list(page)
    }

    /// Whether changes to the mailbox are still being saved. mett waits
    /// for them before exiting, so they aren't lost.
    pub fn saving(&self) -> bool {
        self.writing.is_some()
            || !self.writes.is_empty()
            || self.delivering()
            || !self.discarding.is_empty()
    }

    fn report(&mut self, error: Error) {
        // A request the user's cancelled sign-in held up: they know.
        if matches!(&error, Error::Auth(m) if m == "sign-in cancelled") {
            self.status = "Sign-in cancelled.".into();
            return;
        }
        self.fail(error.to_string());
    }

    /// Shows an error on the message line, in the `error` color.
    pub(super) fn fail(&mut self, text: impl Into<String>) {
        self.status = text.into();
        self.error = Some(self.status.clone());
    }

    /// Whether the user sent mail from `address`.
    pub fn is_me(&self, address: &mett_core::Address) -> bool {
        self.settings.me.contains(&address.email.to_lowercase())
            || self
                .alternates
                .iter()
                .any(|alternate| alternate.is_match(&address.email))
    }
}

/// `earlier` with what `later` changes too.
/// All of `flags`, as an update.
fn whole(flags: mett_core::Flags) -> FlagsUpdate {
    FlagsUpdate {
        read: Some(flags.read),
        flagged: Some(flags.flagged),
    }
}

fn merged(earlier: FlagsUpdate, later: FlagsUpdate) -> FlagsUpdate {
    FlagsUpdate {
        read: later.read.or(earlier.read),
        flagged: later.flagged.or(earlier.flagged),
    }
}

/// The ID of a request effect.
fn effect_id(effect: &Effect) -> RequestId {
    match effect {
        Effect::Request(id, _) => *id,
        _ => unreachable!("not a request: {effect:?}"),
    }
}

/// `alternates` as regular expressions, ignoring case (those that don't
/// compile were reported when they were set, and match nothing).
fn compile_alternates(alternates: &[String]) -> Vec<regex::Regex> {
    alternates
        .iter()
        .filter_map(|alternate| {
            regex::RegexBuilder::new(alternate)
                .case_insensitive(true)
                .build()
                .ok()
        })
        .collect()
}

fn body_height(height: u16) -> usize {
    usize::from(height).saturating_sub(CHROME_ROWS)
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeDelta};
    use mett_core::mock;
    use mett_core::{MessagePage, PageCursor, WellKnownFolder};

    use super::*;

    /// `n` messages, newest first, as Graph lists them.
    fn summaries(n: usize) -> Vec<MessageSummary> {
        (0..n)
            .map(|i| {
                let received = DateTime::UNIX_EPOCH + TimeDelta::minutes((n - i) as i64);
                mock::message(&format!("m{i}"), "in", &format!("subject {i}"), received).summary
            })
            .collect()
    }

    /// An app whose Inbox has been opened and listed with `n` messages.
    fn loaded(n: usize) -> App {
        let (mut app, effects) = App::new(80, 23);
        let [Effect::Request(open, Request::OpenFolder(_))] = effects.as_slice() else {
            panic!("expected OpenFolder, got {effects:?}");
        };
        let folder = mock::folder("in", "Inbox", Some(WellKnownFolder::Inbox));
        let effects = app.update(Msg::Response(Response {
            id: *open,
            event: Event::FolderOpened(Ok(folder)),
        }));
        // The user's own addresses are asked for alongside.
        let [
            Effect::Request(list, Request::ListMessages { .. }),
            Effect::Request(_, Request::OwnAddresses),
        ] = effects.as_slice()
        else {
            panic!("expected ListMessages, got {effects:?}");
        };
        app.update(Msg::Response(Response {
            id: *list,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: summaries(n),
                next: None,
            })),
        }));
        app
    }

    fn act(app: &mut App, action: Action) -> Vec<Effect> {
        app.update(Msg::Action(action))
    }

    fn message(id: &str, body_lines: usize) -> Message {
        let mut message = mock::message(id, "in", id, DateTime::UNIX_EPOCH);
        message.body.content = (0..body_lines).map(|i| format!("line {i}\n")).collect();
        message
    }

    /// Opens the selected message and delivers it with `body_lines` lines;
    /// returns the effects of the delivery.
    fn open(app: &mut App, body_lines: usize) -> Vec<Effect> {
        let effects = act(app, Action::DisplayMessage);
        let [Effect::Request(fetch, Request::FetchMessage(id))] = effects.as_slice() else {
            panic!("expected FetchMessage, got {effects:?}");
        };
        let message = message(id.as_str(), body_lines);
        app.update(Msg::Response(Response {
            id: *fetch,
            event: Event::MessageFetched(Ok(message)),
        }))
    }

    fn pager(app: &App) -> &Pager {
        let Screen::Pager(pager) = app.screen() else {
            panic!("not in the pager");
        };
        pager
    }

    #[test]
    fn loads_the_inbox_on_start() {
        let app = loaded(3);
        assert_eq!(app.messages.len(), 3);
        assert_eq!(app.folder.as_ref().unwrap().name, "Inbox");
        assert!(app.status.is_empty());
    }

    #[test]
    fn keeps_listing_until_the_last_page() {
        let (mut app, effects) = App::new(80, 23);
        let Effect::Request(open, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let folder = mock::folder("in", "Inbox", None);
        let effects = app.update(Msg::Response(Response {
            id: open,
            event: Event::FolderOpened(Ok(folder)),
        }));
        let Effect::Request(first, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let effects = app.update(Msg::Response(Response {
            id: first,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: summaries(2),
                next: Some(PageCursor::new("c")),
            })),
        }));
        let [Effect::Request(_, Request::ListMessages { page, .. })] = effects.as_slice() else {
            panic!("expected another page request, got {effects:?}");
        };
        assert_eq!(page.cursor(), Some(&PageCursor::new("c")));
        assert_eq!(app.messages.len(), 2);
    }

    #[test]
    fn ignores_a_stale_listing() {
        let mut app = loaded(1);
        app.update(Msg::Response(Response {
            id: RequestId(999),
            event: Event::MessagesListed(Ok(MessagePage {
                messages: summaries(5),
                next: None,
            })),
        }));
        assert_eq!(app.messages.len(), 1);
    }

    #[test]
    fn moves_through_the_index_within_bounds() {
        let mut app = loaded(3);
        act(&mut app, Action::PreviousEntry);
        assert_eq!(app.selected, 0);
        assert_eq!(app.status, "You are on the first message.");
        act(&mut app, Action::NextEntry);
        act(&mut app, Action::NextEntry);
        act(&mut app, Action::NextEntry);
        assert_eq!(app.selected, 2);
        assert_eq!(app.status, "You are on the last message.");
        act(&mut app, Action::FirstEntry);
        assert_eq!(app.selected, 0);
        act(&mut app, Action::LastEntry);
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn pages_through_the_index() {
        let mut app = loaded(50);
        assert_eq!(app.body_height, 20);
        act(&mut app, Action::NextPage);
        assert_eq!(app.selected, 20);
        act(&mut app, Action::NextPage);
        act(&mut app, Action::NextPage);
        assert_eq!(app.selected, 49);
        act(&mut app, Action::PreviousPage);
        assert_eq!(app.selected, 29);
    }

    #[test]
    fn marks_a_message_read_once_it_is_shown() {
        let mut app = loaded(2);
        let effects = act(&mut app, Action::DisplayMessage);
        assert_eq!(app.menu(), Menu::Pager);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Request(_, Request::FetchMessage(id))] if id.as_str() == "m0"
        ));
        assert!(!app.messages[0].flags.read, "not read before it is shown");

        act(&mut app, Action::Exit);
        let effects = open(&mut app, 1);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Request(_, Request::UpdateFlags(id, FlagsUpdate { read: Some(true), .. }))]
                if id.as_str() == "m0"
        ));
        assert!(app.messages[0].flags.read);

        // Reopening a read message doesn't update its flags again.
        act(&mut app, Action::Exit);
        assert!(open(&mut app, 1).is_empty());
    }

    #[test]
    fn finishing_the_listing_keeps_an_error_reported_meanwhile() {
        let (mut app, effects) = App::new(80, 23);
        let Effect::Request(open_inbox, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let effects = app.update(Msg::Response(Response {
            id: open_inbox,
            event: Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
        }));
        let Effect::Request(first, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let effects = app.update(Msg::Response(Response {
            id: first,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: summaries(1),
                next: Some(PageCursor::new("c")),
            })),
        }));
        let Effect::Request(second, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };

        // An error is reported while the second page is loading…
        let effects = open(&mut app, 1);
        let Effect::Request(update, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        app.update(Msg::Response(Response {
            id: update,
            event: Event::FlagsUpdated(Err(Error::Network("offline".into()))),
        }));
        // …and survives the end of the listing.
        app.update(Msg::Response(Response {
            id: second,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: Vec::new(),
                next: None,
            })),
        }));
        assert_eq!(app.status, "network error: offline");
    }

    #[test]
    fn a_failed_page_leaves_the_index_incomplete_until_retried() {
        let (mut app, effects) = App::new(80, 23);
        let Effect::Request(open_inbox, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let effects = app.update(Msg::Response(Response {
            id: open_inbox,
            event: Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
        }));
        let Effect::Request(first, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let effects = app.update(Msg::Response(Response {
            id: first,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: summaries(2),
                next: Some(PageCursor::new("c")),
            })),
        }));
        let Effect::Request(second, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        app.update(Msg::Response(Response {
            id: second,
            event: Event::MessagesListed(Err(Error::Network("offline".into()))),
        }));
        assert!(app.incomplete());

        // Reaching the end of the index retries the failed page.
        act(&mut app, Action::NextEntry);
        let effects = act(&mut app, Action::NextEntry);
        let [Effect::Request(retry, Request::ListMessages { page, .. })] = effects.as_slice()
        else {
            panic!("expected a retry, got {effects:?}");
        };
        assert_eq!(page.cursor(), Some(&PageCursor::new("c")));
        assert!(!app.incomplete());
        app.update(Msg::Response(Response {
            id: *retry,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: summaries(1),
                next: None,
            })),
        }));
        assert_eq!(app.messages.len(), 3);
        assert!(!app.incomplete());
    }

    #[test]
    fn the_pager_also_retries_a_failed_page() {
        let (mut app, effects) = App::new(80, 23);
        let Effect::Request(open_inbox, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let effects = app.update(Msg::Response(Response {
            id: open_inbox,
            event: Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
        }));
        let Effect::Request(first, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let effects = app.update(Msg::Response(Response {
            id: first,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: summaries(1),
                next: Some(PageCursor::new("c")),
            })),
        }));
        let Effect::Request(second, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        app.update(Msg::Response(Response {
            id: second,
            event: Event::MessagesListed(Err(Error::Network("offline".into()))),
        }));

        open(&mut app, 1);
        let effects = act(&mut app, Action::NextEntry);
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::Request(_, Request::ListMessages { .. })]
            ),
            "{effects:?}"
        );
        assert_eq!(app.status, "Loading more messages...");
    }

    #[test]
    fn records_every_failed_save() {
        let mut app = loaded(2);
        let first = open(&mut app, 1);
        act(&mut app, Action::Exit);
        act(&mut app, Action::NextEntry);
        // One change at a time: the second waits for the first.
        assert!(open(&mut app, 1).is_empty());
        let [Effect::Request(first, _)] = first.as_slice() else {
            panic!("expected a flag update, got {first:?}");
        };
        let second = app.update(Msg::Response(Response {
            id: *first,
            event: Event::FlagsUpdated(Err(Error::Network("offline".into()))),
        }));
        let [Effect::Request(second, _)] = second.as_slice() else {
            panic!("expected a flag update, got {second:?}");
        };
        // A later success doesn't hide the earlier failure.
        app.update(Msg::Response(Response {
            id: *second,
            event: Event::FlagsUpdated(Ok(())),
        }));
        assert_eq!(app.save_failures(), ["network error: offline"]);
        assert!(!app.saving());

        // Saving the same message again successfully clears its failure.
        act(&mut app, Action::Exit);
        act(&mut app, Action::FirstEntry);
        let retry = open(&mut app, 1);
        let [Effect::Request(retry, _)] = retry.as_slice() else {
            panic!("expected a retried flag update, got {retry:?}");
        };
        app.update(Msg::Response(Response {
            id: *retry,
            event: Event::FlagsUpdated(Ok(())),
        }));
        assert!(app.save_failures().is_empty());
    }

    /// An app whose first page of `n` messages has loaded, with another page
    /// still loading; returns the app and that request.
    fn loading_more(n: usize) -> (App, RequestId) {
        let (mut app, effects) = App::new(80, 23);
        let Effect::Request(open_inbox, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let effects = app.update(Msg::Response(Response {
            id: open_inbox,
            event: Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
        }));
        let Effect::Request(first, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let effects = app.update(Msg::Response(Response {
            id: first,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: summaries(n),
                next: Some(PageCursor::new("c")),
            })),
        }));
        let Effect::Request(second, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        (app, second)
    }

    fn deliver_last_page(
        app: &mut App,
        id: RequestId,
        messages: Vec<MessageSummary>,
    ) -> Vec<Effect> {
        app.update(Msg::Response(Response {
            id,
            event: Event::MessagesListed(Ok(MessagePage {
                messages,
                next: None,
            })),
        }))
    }

    #[test]
    fn next_entry_waits_for_the_next_page_in_the_index() {
        let (mut app, second) = loading_more(1);
        act(&mut app, Action::NextEntry);
        assert_eq!(app.selected, 0);
        assert_eq!(app.status, "Loading more messages...");
        let mut more = summaries(2);
        more.remove(0);
        deliver_last_page(&mut app, second, more);
        assert_eq!(app.selected, 1, "moved on once the page arrived");
    }

    #[test]
    fn next_entry_waits_for_the_next_page_in_the_pager() {
        let (mut app, second) = loading_more(1);
        open(&mut app, 1);
        act(&mut app, Action::NextEntry);
        let mut more = summaries(2);
        more.remove(0);
        let effects = deliver_last_page(&mut app, second, more);
        assert_eq!(app.selected, 1);
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::Request(_, Request::FetchMessage(id))] if id.as_str() == "m1"
            ),
            "{effects:?}"
        );
    }

    #[test]
    fn last_entry_follows_the_folder_while_it_loads() {
        let (mut app, second) = loading_more(2);
        act(&mut app, Action::LastEntry);
        assert_eq!(app.selected, 1);
        deliver_last_page(&mut app, second, summaries(5));
        assert_eq!(app.selected, 6, "the real last message");
    }

    #[test]
    fn next_page_completes_once_the_page_arrives() {
        let (mut app, second) = loading_more(5);
        // 20 rows per page: the target, entry 20, isn't loaded yet.
        act(&mut app, Action::NextPage);
        assert_eq!(app.selected, 4);
        deliver_last_page(&mut app, second, summaries(30));
        assert_eq!(app.selected, 20);
    }

    #[test]
    fn another_action_cancels_a_pending_next_entry() {
        let (mut app, second) = loading_more(2);
        act(&mut app, Action::NextEntry);
        act(&mut app, Action::NextEntry);
        act(&mut app, Action::PreviousEntry);
        deliver_last_page(&mut app, second, Vec::new());
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn tracks_unsaved_flag_changes() {
        let mut app = loaded(1);
        assert!(!app.saving());
        let effects = open(&mut app, 1);
        assert!(app.saving());
        let Effect::Request(update, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        app.update(Msg::Response(Response {
            id: update,
            event: Event::FlagsUpdated(Ok(())),
        }));
        assert!(!app.saving());
    }

    #[test]
    fn a_failed_flag_update_marks_the_message_unread_again() {
        let mut app = loaded(1);
        let effects = open(&mut app, 1);
        let Effect::Request(update, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        app.update(Msg::Response(Response {
            id: update,
            event: Event::FlagsUpdated(Err(Error::Network("offline".into()))),
        }));
        assert!(!app.messages[0].flags.read);
        assert_eq!(app.status, "network error: offline");
    }

    #[test]
    fn shows_the_fetched_message_and_ignores_stale_ones() {
        let mut app = loaded(2);
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(first, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        // Move on to the next message before the first one arrives.
        let effects = act(&mut app, Action::NextEntry);
        let Effect::Request(second, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };

        app.update(Msg::Response(Response {
            id: first,
            event: Event::MessageFetched(Ok(message("m0", 1))),
        }));
        assert_eq!(pager(&app).message, None, "stale response ignored");
        assert!(!app.messages[0].flags.read);

        app.update(Msg::Response(Response {
            id: second,
            event: Event::MessageFetched(Ok(message("m1", 1))),
        }));
        let shown = pager(&app).message.as_ref().unwrap();
        assert_eq!(shown.summary.id.as_str(), "m1");
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn a_failed_fetch_returns_to_the_index_and_leaves_it_unread() {
        let mut app = loaded(1);
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let effects = app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Err(Error::Network("offline".into()))),
        }));
        assert!(effects.is_empty());
        assert_eq!(app.menu(), Menu::Index);
        assert_eq!(app.status, "network error: offline");
        assert!(!app.messages[0].flags.read);
    }

    #[test]
    fn pager_scrolls_within_the_message() {
        let mut app = loaded(1);
        // 4 header lines and a blank line, 40 body lines; 20 visible rows.
        open(&mut app, 40);
        let rows = pager(&app).rows.len();
        assert_eq!(rows, text::wrap_all(&pager(&app).lines, 80).len());
        act(&mut app, Action::NextLine);
        act(&mut app, Action::NextPage);
        act(&mut app, Action::PreviousLine);
        assert_eq!(pager(&app).scroll, 20);

        for _ in 0..5 {
            act(&mut app, Action::NextPage);
        }
        assert_eq!(pager(&app).scroll, rows - 20, "stops at the last full page");
        act(&mut app, Action::PreviousPage);
        assert_eq!(
            pager(&app).scroll,
            rows - 40,
            "responds at once after the end"
        );

        act(&mut app, Action::Exit);
        assert_eq!(app.screen(), &Screen::Index);
        act(&mut app, Action::Quit);
        assert!(app.quit);
    }

    #[test]
    fn long_lines_wrap_and_can_be_scrolled_to() {
        let mut app = loaded(1);
        app.update(Msg::Resize {
            width: 10,
            height: 8,
        });
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let mut message = message("m0", 0);
        message.body.content = "x".repeat(100);
        app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Ok(message)),
        }));
        let rows = pager(&app).rows.len();
        assert_eq!(rows, text::wrap_all(&pager(&app).lines, 10).len());
        assert!(rows > 10);
        for _ in 0..rows {
            act(&mut app, Action::NextLine);
        }
        assert_eq!(pager(&app).scroll, rows - 5, "every row is reachable");
    }

    fn finished(outcome: Result<(), String>) -> SignInEvent {
        SignInEvent::Finished {
            account: AccountId::new("alice@contoso.com"),
            outcome,
        }
    }

    fn browser_prompt() -> SignInEvent {
        SignInEvent::Prompt {
            account: AccountId::new("alice@contoso.com"),
            prompt: SignInPrompt::OpenBrowser {
                url: "https://login.example/authorize".parse().unwrap(),
            },
        }
    }

    #[test]
    fn the_sign_in_window_opens_the_browser_and_closes_once_signed_in() {
        let (mut app, _) = App::new(80, 23);
        let effects = app.update(Msg::SignIn(browser_prompt()));
        assert_eq!(
            effects,
            [Effect::OpenUrl("https://login.example/authorize".into())]
        );
        assert_eq!(app.menu(), Menu::SignIn);
        // o opens it again.
        assert_eq!(
            act(&mut app, Action::OpenBrowser),
            [Effect::OpenUrl("https://login.example/authorize".into())]
        );
        assert_eq!(
            act(&mut app, Action::CopyLink),
            [Effect::Copy("https://login.example/authorize".into())]
        );
        app.update(Msg::SignIn(finished(Ok(()))));
        assert_eq!(app.sign_in, None);
        assert_eq!(app.menu(), Menu::Index);
    }

    #[test]
    fn the_device_code_window_opens_the_page_only_when_asked() {
        let (mut app, _) = App::new(80, 23);
        let effects = app.update(Msg::SignIn(SignInEvent::Prompt {
            account: AccountId::new("alice@contoso.com"),
            prompt: SignInPrompt::EnterCode {
                verification_uri: "https://microsoft.com/devicelogin".into(),
                user_code: "ABCD".into(),
                message: "Enter ABCD".into(),
            },
        }));
        assert!(effects.is_empty());
        assert_eq!(
            act(&mut app, Action::OpenBrowser),
            [Effect::OpenUrl("https://microsoft.com/devicelogin".into())]
        );
    }

    #[test]
    fn cancelling_closes_the_window_without_reporting_a_failure() {
        let (mut app, _) = App::new(80, 23);
        app.update(Msg::SignIn(browser_prompt()));
        app.retry_after.insert(RequestId(1));
        assert_eq!(act(&mut app, Action::Exit), [Effect::CancelSignIn]);
        assert_eq!(app.sign_in, None);
        assert!(app.retry_after.is_empty());
        app.update(Msg::SignIn(finished(Err("sign-in cancelled".into()))));
        assert_eq!(app.sign_in, None);
        assert_eq!(app.status, "Sign-in cancelled.");
        // The request it held up says the same, in the same words.
        let opening = app.opening.unwrap();
        app.update(Msg::Response(Response {
            id: opening,
            event: Event::FolderOpened(Err(Error::Auth("sign-in cancelled".into()))),
        }));
        assert_eq!(app.status, "Sign-in cancelled.");
    }

    #[test]
    fn a_failed_sign_in_can_be_retried() {
        let (mut app, effects) = App::new(80, 23);
        let [Effect::Request(first, Request::OpenFolder(_))] = effects.as_slice() else {
            panic!("expected OpenFolder, got {effects:?}");
        };
        // The sign-in fails before any prompt, and so does the request.
        app.update(Msg::SignIn(finished(Err("bad app".into()))));
        app.update(Msg::Response(Response {
            id: *first,
            event: Event::FolderOpened(Err(Error::Auth("bad app".into()))),
        }));
        let window = app.sign_in.as_ref().expect("the failure is shown");
        assert_eq!(window.failure.as_deref(), Some("bad app"));
        assert_eq!(window.prompt, None);

        // Retrying closes the window and opens the folder again.
        let effects = act(&mut app, Action::Retry);
        let [Effect::Request(second, Request::OpenFolder(_))] = effects.as_slice() else {
            panic!("expected OpenFolder, got {effects:?}");
        };
        assert_eq!(app.sign_in, None);
        assert_eq!(app.status, "Opening folder...");
        app.update(Msg::Response(Response {
            id: *second,
            event: Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
        }));
        assert_eq!(app.folder.as_ref().unwrap().name, "Inbox");
    }

    #[test]
    fn enter_in_an_empty_index_retries_opening_the_folder() {
        let (mut app, effects) = App::new(80, 23);
        let Effect::Request(open_inbox, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        // Pressing Enter while the folder is still opening does nothing.
        assert!(act(&mut app, Action::DisplayMessage).is_empty());
        assert_eq!(app.status, "Opening folder...");
        app.update(Msg::Response(Response {
            id: open_inbox,
            event: Event::FolderOpened(Err(Error::Network("offline".into()))),
        }));
        assert!(matches!(
            act(&mut app, Action::DisplayMessage).as_slice(),
            [Effect::Request(_, Request::OpenFolder(_))]
        ));
    }

    #[test]
    fn retrying_after_a_sign_in_resends_what_failed() {
        let mut app = loaded(2);
        // Opening the first message works, but saving it as read fails…
        let effects = open(&mut app, 1);
        let [Effect::Request(update, Request::UpdateFlags(..))] = effects.as_slice() else {
            panic!("expected a flag update, got {effects:?}");
        };
        app.update(Msg::Response(Response {
            id: *update,
            event: Event::FlagsUpdated(Err(Error::Auth("expired".into()))),
        }));
        // …and opening the second one fails too.
        act(&mut app, Action::Exit);
        act(&mut app, Action::NextEntry);
        let effects = act(&mut app, Action::DisplayMessage);
        let [Effect::Request(fetch, Request::FetchMessage(_))] = effects.as_slice() else {
            panic!("expected a fetch, got {effects:?}");
        };
        app.update(Msg::Response(Response {
            id: *fetch,
            event: Event::MessageFetched(Err(Error::Auth("expired".into()))),
        }));
        assert_eq!(app.menu(), Menu::Index);
        app.update(Msg::SignIn(finished(Err("expired".into()))));

        let effects = act(&mut app, Action::Retry);
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::Request(_, Request::UpdateFlags(first, _)),
                    Effect::Request(_, Request::FetchMessage(second)),
                ] if first.as_str() == "m0" && second.as_str() == "m1"
            ),
            "{effects:?}"
        );
        assert_eq!(app.menu(), Menu::Pager);
    }

    #[test]
    fn retry_waits_for_the_failed_request_to_report_back() {
        let (mut app, effects) = App::new(80, 23);
        let [Effect::Request(first, Request::OpenFolder(_))] = effects.as_slice() else {
            panic!("expected OpenFolder, got {effects:?}");
        };
        // The sign-in failure arrives before the request's own failure, and
        // the user retries straight away.
        app.update(Msg::SignIn(finished(Err("denied".into()))));
        assert!(act(&mut app, Action::Retry).is_empty());
        let effects = app.update(Msg::Response(Response {
            id: *first,
            event: Event::FolderOpened(Err(Error::Auth("denied".into()))),
        }));
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::Request(_, Request::OpenFolder(_))]
            ),
            "{effects:?}"
        );
    }

    #[test]
    fn a_successful_sign_in_keeps_an_error_already_reported() {
        let (mut app, effects) = App::new(80, 23);
        let Effect::Request(open_inbox, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        app.update(Msg::SignIn(browser_prompt()));
        app.update(Msg::Response(Response {
            id: open_inbox,
            event: Event::FolderOpened(Err(Error::not_found("folder", "x"))),
        }));
        app.update(Msg::SignIn(finished(Ok(()))));
        assert_eq!(app.status, "folder not found: x");
    }

    #[test]
    fn retry_waits_for_every_pending_request() {
        let (mut app, list) = loading_more(1);
        // Opening a message fails while the next page is still loading.
        let effects = act(&mut app, Action::DisplayMessage);
        let [Effect::Request(fetch, _)] = effects.as_slice() else {
            panic!("expected a fetch, got {effects:?}");
        };
        app.update(Msg::SignIn(finished(Err("denied".into()))));
        assert!(act(&mut app, Action::Retry).is_empty());
        let effects = app.update(Msg::Response(Response {
            id: *fetch,
            event: Event::MessageFetched(Err(Error::Auth("denied".into()))),
        }));
        assert!(
            effects.is_empty(),
            "still waiting for the page: {effects:?}"
        );
        let effects = app.update(Msg::Response(Response {
            id: list,
            event: Event::MessagesListed(Err(Error::Auth("denied".into()))),
        }));
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::Request(_, Request::ListMessages { .. }),
                    Effect::Request(_, Request::FetchMessage(_)),
                ]
            ),
            "{effects:?}"
        );
    }

    #[test]
    fn retry_resends_failures_at_once_and_waits_for_the_rest() {
        let (mut app, list) = loading_more(1);
        let effects = act(&mut app, Action::DisplayMessage);
        let [Effect::Request(fetch, _)] = effects.as_slice() else {
            panic!("expected a fetch, got {effects:?}");
        };
        // The fetch has failed; the page hasn't reported back yet.
        app.update(Msg::Response(Response {
            id: *fetch,
            event: Event::MessageFetched(Err(Error::Auth("denied".into()))),
        }));
        app.update(Msg::SignIn(finished(Err("denied".into()))));
        let effects = act(&mut app, Action::Retry);
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::Request(_, Request::FetchMessage(_))]
            ),
            "{effects:?}"
        );
        // The retried fetch asks the user to sign in again meanwhile.
        app.update(Msg::SignIn(browser_prompt()));
        let effects = app.update(Msg::Response(Response {
            id: list,
            event: Event::MessagesListed(Err(Error::Auth("denied".into()))),
        }));
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::Request(_, Request::ListMessages { .. })]
            ),
            "{effects:?}"
        );
    }

    #[test]
    fn the_sign_in_window_keeps_a_pending_move() {
        let (mut app, second) = loading_more(1);
        act(&mut app, Action::NextEntry);
        app.update(Msg::SignIn(browser_prompt()));
        act(&mut app, Action::OpenBrowser);
        app.update(Msg::SignIn(finished(Ok(()))));
        let mut more = summaries(2);
        more.remove(0);
        deliver_last_page(&mut app, second, more);
        assert_eq!(app.selected, 1, "moved on once the page arrived");
    }

    #[test]
    fn closing_a_failed_sign_in_drops_waiting_retries() {
        let (mut app, _) = App::new(80, 23);
        app.update(Msg::SignIn(finished(Err("denied".into()))));
        app.retry_after.insert(RequestId(1));
        act(&mut app, Action::Exit);
        assert!(app.retry_after.is_empty());
    }

    #[test]
    fn a_closed_failure_window_stays_closed_until_the_user_acts() {
        let (mut app, _) = App::new(80, 23);
        app.update(Msg::SignIn(finished(Err("unavailable".into()))));
        act(&mut app, Action::Exit);
        // A late failure from a request that was already waiting.
        app.update(Msg::SignIn(finished(Err("unavailable".into()))));
        assert_eq!(app.sign_in, None);
        // Once the user does something else, failures show again.
        act(&mut app, Action::NextEntry);
        app.update(Msg::SignIn(finished(Err("unavailable".into()))));
        assert!(app.sign_in.is_some());
    }

    #[test]
    fn a_cancelled_sign_in_stays_quiet_after_other_keys() {
        let (mut app, _) = App::new(80, 23);
        app.update(Msg::SignIn(browser_prompt()));
        act(&mut app, Action::Exit);
        // A key before the cancellation is reported.
        act(&mut app, Action::NextEntry);
        app.update(Msg::SignIn(finished(Err("sign-in cancelled".into()))));
        assert_eq!(app.sign_in, None);
        // A later failure is shown.
        app.update(Msg::SignIn(finished(Err("denied".into()))));
        assert!(app.sign_in.is_some());
    }

    #[test]
    fn retrying_while_quitting_waits_only_for_saves() {
        let (mut app, list) = loading_more(1);
        let effects = open(&mut app, 1);
        let [Effect::Request(update, _)] = effects.as_slice() else {
            panic!("expected a flag update, got {effects:?}");
        };
        act(&mut app, Action::Exit);
        act(&mut app, Action::Quit);
        assert!(app.quit);
        app.update(Msg::SignIn(finished(Err("denied".into()))));
        act(&mut app, Action::Retry);
        assert!(!app.retry_after.contains(&list), "listing isn't waited for");
        let effects = app.update(Msg::Response(Response {
            id: *update,
            event: Event::FlagsUpdated(Err(Error::Auth("denied".into()))),
        }));
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::Request(_, Request::UpdateFlags(..))]
            ),
            "{effects:?}"
        );
    }

    #[test]
    fn a_failed_fetch_is_retried_under_a_new_sign_in_window() {
        let mut app = loaded(1);
        let effects = act(&mut app, Action::DisplayMessage);
        let [Effect::Request(fetch, _)] = effects.as_slice() else {
            panic!("expected a fetch, got {effects:?}");
        };
        app.update(Msg::SignIn(finished(Err("denied".into()))));
        act(&mut app, Action::Retry);
        // Another sign-in starts before the fetch reports its failure.
        app.update(Msg::SignIn(browser_prompt()));
        let effects = app.update(Msg::Response(Response {
            id: *fetch,
            event: Event::MessageFetched(Err(Error::Auth("denied".into()))),
        }));
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::Request(_, Request::FetchMessage(_))]
            ),
            "{effects:?}"
        );
    }

    #[test]
    fn a_failed_sign_in_offers_no_stale_link() {
        let (mut app, _) = App::new(80, 23);
        app.update(Msg::SignIn(browser_prompt()));
        app.update(Msg::SignIn(finished(Err("denied".into()))));
        assert!(act(&mut app, Action::OpenBrowser).is_empty());
        assert!(act(&mut app, Action::CopyLink).is_empty());
        let rows = app.sign_in.as_ref().unwrap().rows(80);
        assert!(rows.iter().all(|(_, kind)| *kind != Row::Link), "{rows:?}");
    }

    #[test]
    fn a_cancellation_too_late_is_forgotten() {
        let (mut app, _) = App::new(80, 23);
        app.update(Msg::SignIn(browser_prompt()));
        act(&mut app, Action::Exit);
        assert_eq!(app.status, "Sign-in cancelled.");
        app.update(Msg::SignIn(finished(Ok(()))));
        assert_eq!(app.status, "");
    }

    #[test]
    fn the_sign_in_window_scroll_follows_resizes() {
        let (mut app, _) = App::new(40, 6);
        let url = format!("https://login.example/authorize?{}", "x".repeat(400));
        app.update(Msg::SignIn(SignInEvent::Prompt {
            account: AccountId::new("alice@contoso.com"),
            prompt: SignInPrompt::OpenBrowser {
                url: url.parse().unwrap(),
            },
        }));
        for _ in 0..30 {
            act(&mut app, Action::NextLine);
        }
        let deep = app.sign_in.as_ref().unwrap().scroll;
        assert!(deep > 0);
        app.update(Msg::Resize {
            width: 40,
            height: 60,
        });
        assert_eq!(app.sign_in.as_ref().unwrap().scroll, 0, "it all fits now");
    }

    #[test]
    fn a_failure_shows_in_a_scrolled_sign_in_window() {
        let (mut app, _) = App::new(40, 6);
        let url = format!("https://login.example/authorize?{}", "x".repeat(400));
        app.update(Msg::SignIn(SignInEvent::Prompt {
            account: AccountId::new("alice@contoso.com"),
            prompt: SignInPrompt::OpenBrowser {
                url: url.parse().unwrap(),
            },
        }));
        for _ in 0..30 {
            act(&mut app, Action::NextLine);
        }
        assert!(app.sign_in.as_ref().unwrap().scroll > 0);
        app.update(Msg::SignIn(finished(Err("expired".into()))));
        let window = app.sign_in.as_ref().unwrap();
        let room = (app.body_height + CHROME_ROWS).saturating_sub(3);
        assert!(window.scroll <= window.rows(app.width).len().saturating_sub(room));
    }

    #[test]
    fn the_mailbox_tells_the_users_own_addresses_once() {
        let (mut app, effects) = App::new(80, 23);
        let Effect::Request(open, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        // Asked as soon as the folder opens, before any reply can be made.
        let effects = app.update(Msg::Response(Response {
            id: open,
            event: Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
        }));
        let Some(Effect::Request(id, _)) = effects
            .iter()
            .find(|e| matches!(e, Effect::Request(_, Request::OwnAddresses)))
        else {
            panic!("expected OwnAddresses, got {effects:?}");
        };
        app.update(Msg::Response(Response {
            id: *id,
            event: Event::OwnAddresses(Ok(vec!["Alice@Contoso.com".into()])),
        }));
        assert!(app.settings.me.contains(&"alice@contoso.com".to_owned()));
        // Not asked again when the folder is listed again.
        let effects = app.relist();
        let (list, _) = match effects.as_slice() {
            [Effect::Request(id, request)] => (*id, request.clone()),
            _ => panic!("{effects:?}"),
        };
        let effects = deliver_last_page(&mut app, list, Vec::new());
        assert!(effects.is_empty(), "{effects:?}");
    }
}
