//! The application model and its `update` function: the Elm Architecture
//! (`docs/DESIGN.md` §5.2).
//!
//! [`App`] holds all UI state. Only [`App::update`] changes it, in response
//! to a [`Msg`]; side effects come back as [`Effect`]s for the main loop to
//! carry out. This keeps the UI logic testable without a terminal.

mod attachments;
mod composing;
mod flags;
mod invite;
mod keys;
mod mailbox;
mod newmail;
mod pager;
mod query;
mod recall;
mod retry;
mod search;
mod sidebar;
mod signin;
mod tabs;
#[cfg(test)]
pub(crate) mod testing;
mod threads;

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use mett_core::{AccountId, Error, FlagsUpdate, Folder, Message, MessageId, MessageSummary, Page};
use mett_msgraph::SignInPrompt;
use ratatui::crossterm::event::KeyEvent;

pub use self::attachments::{AttachMenu, FileOp, FileOutcome, TextView};
use self::composing::Composing;
pub use self::composing::{Compose, Input};
pub use self::keys::{Help, Reconfigured};
pub use self::mailbox::Browser;
use self::mailbox::{AfterPurge, Transfer, Write};
pub use self::newmail::MailboxEntry;
pub use self::pager::Pager;
pub use self::query::{QueryMenu, QuerySettings};
pub use self::recall::PostponedMenu;
pub use self::search::{Limit, highlighted};
pub use self::sidebar::{Sidebar, SidebarEntry, SidebarSettings, SidebarSort};
pub use self::signin::{Row, SignIn};
pub use self::tabs::{Switch, Tab, TabPosition, Tabs};
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

/// A second, the step of the throttling countdown.
const SECOND: Duration = Duration::from_secs(1);

/// How many requests given up on are remembered (see `App::given_up`).
const GIVEN_UP_KEPT: usize = 16;

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
    /// The thread before the selected message's.
    PreviousThread,
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
    /// How often to check for new mail (`office365_sync_interval`), or
    /// `None` not to.
    pub sync_interval: Option<std::time::Duration>,
    /// The home directory, for `~` in file names.
    pub home: Option<String>,
    /// `alias`.
    pub aliases: Vec<crate::alias::Alias>,
    /// `alias_file`: where `a` saves aliases.
    pub alias_file: Option<std::path::PathBuf>,
    /// How to find addresses.
    pub query: QuerySettings,
    /// The sidebar's settings.
    pub sidebar: SidebarSettings,
    /// The API the account is reached through. Like the account, it stays
    /// as mett started.
    pub api: crate::account::Api,
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
            home: None,
            aliases: Vec::new(),
            alias_file: None,
            query: QuerySettings::default(),
            sidebar: SidebarSettings::default(),
            api: crate::account::Api::default(),
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
    /// A file was saved, or not.
    FileSaved(FileOp, FileOutcome),
    /// A file was read, or not.
    FileRead(FileOp, std::path::PathBuf, Result<Vec<u8>, String>),
    /// Something done apart from the app failed (opening a file, say).
    Failed(String),
    /// Text was added to a file, or not.
    Appended(Result<std::path::PathBuf, String>),
    /// A timer the app set went off.
    Timer(Timer),
    /// A folder's sync goes on: the cache has this many of its messages.
    Syncing {
        /// The folder.
        folder: mett_core::FolderId,
        /// How many messages.
        taken: u64,
    },
    /// The user closed the window (the GUI's).
    Close,
    /// The dictionaries for these spelling settings were loaded, or not.
    SpellerLoaded(
        crate::editor::spell::Settings,
        Result<std::sync::Arc<crate::editor::spell::Speller>, String>,
    ),
    /// The user pasted text.
    Paste(String),
    /// The user asked to paste from the clipboard (in the GUI's window).
    PasteClipboard,
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
    /// Show another account's tab.
    Account(Switch),
    /// Open a URL in the user's browser.
    OpenUrl(String),
    /// Cancel the sign-in in progress.
    CancelSignIn,
    /// Put the text on the clipboard, through the terminal.
    Copy(String),
    /// Write a file, then send back [`Msg::FileSaved`]. A file that exists
    /// is replaced only if `overwrite`.
    SaveFile {
        /// Which saving it is.
        op: FileOp,
        /// Where.
        path: std::path::PathBuf,
        /// What.
        data: Vec<u8>,
        /// Whether to replace a file there.
        overwrite: bool,
    },
    /// Open a file (an attachment) in the program the system has for it.
    OpenFile {
        /// Its name.
        name: String,
        /// Its content.
        data: Vec<u8>,
    },
    /// Read a file, then send back [`Msg::FileRead`].
    ReadFile(FileOp, std::path::PathBuf),
    /// Add text to the end of a file, then send back [`Msg::Appended`].
    AppendFile {
        /// The file.
        path: std::path::PathBuf,
        /// The text.
        text: String,
    },
    /// Load the dictionaries for these spelling settings, then send back
    /// [`Msg::SpellerLoaded`].
    LoadSpeller(crate::editor::spell::Settings),
    /// Add `word` to the user's own words, in the file at `path`.
    AddWord {
        /// The file (`spell_words`).
        path: std::path::PathBuf,
        /// The word.
        word: String,
    },
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
    /// List Drafts again, if it is open: Graph takes a sent draft out of it
    /// a moment after accepting it.
    RelistDrafts,
    /// Check for new mail, if this is still the timer for it (it counts
    /// the changes of interval).
    Poll(u64),
    /// Watch the folders for changes again, after watching failed.
    Watch,
    /// A second of the wait for a throttling server went by, if this is
    /// still the countdown's timer (it counts the countdowns started).
    Throttled(u64),
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
    /// A message's attachments.
    Attachments(AttachMenu),
    /// Text on its own, such as a text attachment.
    Text(TextView),
    /// Addresses to choose from: aliases, or a query's results.
    Query(QueryMenu),
    /// Postponed messages to choose from.
    Postponed(PostponedMenu),
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
    /// The tabs of the accounts, if there are several: the main loop gives
    /// them to the account shown, to draw.
    pub tabs: Option<Tabs>,
    /// Whether new mail was announced since the account was last looked at,
    /// which its tab says when another account is shown (the main loop
    /// clears it once it is shown).
    pub arrived: bool,
    /// How the screen is drawn: formats and colors.
    pub display: crate::expando::Display,
    /// The last error reported on the message line, so that the view can
    /// tell it from other messages.
    pub error: Option<String>,
    /// Why a question was asked again (a lookup found nothing, say):
    /// shown in its place until the next key.
    pub input_note: Option<String>,
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
    /// What to say on the message line once the folder is listed (the
    /// configuration's warnings, in the GUI's window).
    notice: Option<String>,
    /// Escape was typed where it does nothing by itself, in this menu, and
    /// when: with the next key there, soon after, it is Meta and that key
    /// (`Esc v`), as in mutt.
    escape: Option<(Menu, std::time::Instant)>,
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
    /// The built-in editor, while a message is written in it. (The view
    /// lays the text out in it, hence the cell.)
    pub editing: Option<std::cell::RefCell<crate::editor::builtin::Builtin>>,
    /// The dictionaries, once loaded, with the settings they are for.
    speller: Option<(
        crate::editor::spell::Settings,
        std::sync::Arc<crate::editor::spell::Speller>,
    )>,
    /// The settings whose dictionaries are being loaded.
    speller_loading: Option<crate::editor::spell::Settings>,
    /// The words the user added since mett started, with the `spell_words`
    /// they went to: dictionaries loaded later may have read it before
    /// they were written.
    added_words: Vec<(Option<std::path::PathBuf>, String)>,
    /// The request fetching a message to reply to or forward.
    compose_fetch: Option<(RequestId, Kind, MessageId)>,
    /// The reply or forward whose message couldn't be fetched for want of
    /// a sign-in, to start again on Retry.
    compose_unfetched: Option<(MessageId, Kind)>,
    /// A recall of a postponed message on its way (`R`).
    recall: Option<recall::Recall>,
    /// A recall failed for want of a sign-in: Retry starts it again.
    unrecalled: bool,
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
    /// How many messages the threads were last built from: while a large
    /// folder is listed, they are built again only as it doubles.
    threaded_count: usize,
    /// Whether building the threads again would give what they were last
    /// built as (see [`crate::sort::Arranged::settled`]).
    settled: bool,
    /// The threads (by key) the user collapsed (`true`) or opened
    /// (`false`) themselves. The others are open, or with `collapse_all`
    /// collapsed unless they hold unread mail (as mutt's
    /// `collapse_unread=no`).
    chosen: HashMap<String, bool>,
    /// `Esc V` collapsed every thread, including those listed since.
    collapse_all: bool,
    /// Whether the limit showed each message it was applied to, kept
    /// across listings so that they show the same messages.
    limited: HashMap<MessageId, bool>,
    /// Where each message was in the folder's listing, to restore the
    /// order when the limit is lifted.
    positions: HashMap<MessageId, usize>,
    /// A message was listed twice since the index was last empty: it may
    /// show it twice.
    listed_twice: bool,
    /// The last search (`/`), which `n` repeats.
    last_search: Option<String>,
    /// The last search in the pager, which `n` there repeats.
    last_pager_search: Option<String>,
    /// A search that found nothing yet, waiting for more of the folder,
    /// and its pattern.
    pending_search: Option<(String, crate::pattern::Pattern)>,
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
    /// The width of the menus, in cells: the terminal's, but for the
    /// sidebar's.
    pub width: usize,
    /// Terminal width, in cells.
    terminal_width: usize,
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
    /// An answer to an invitation being asked about, or sent.
    answering: Option<invite::Answering>,
    /// The attachment being fetched, and what for.
    fetching: Option<attachments::Fetching>,
    /// The attachment that failed to be fetched for want of a sign-in,
    /// and what for: a retry fetches it again.
    unfetched_attachment: Option<(MessageId, mett_core::Attachment, attachments::Use)>,
    /// The file being read (to attach) or saved: keys wait for it, as in
    /// mutt, but `^G` gives up on it.
    file_wait: Option<(FileOp, std::path::PathBuf)>,
    /// The last [`FileOp`] given.
    last_file_op: u64,
    /// A file not saved because one was there, waiting for the user to say
    /// whether to replace it.
    unsaved: Option<(std::path::PathBuf, Vec<u8>)>,
    /// See [`Settings::home`].
    home: Option<String>,
    /// See [`Settings::aliases`].
    aliases: Vec<crate::alias::Alias>,
    /// See [`Settings::alias_file`].
    alias_file: Option<std::path::PathBuf>,
    /// See [`Settings::query`].
    query: QuerySettings,
    /// The address of an alias being made, until its name is given.
    new_alias: Option<String>,
    /// The name of an alias being made, until its address is given.
    alias_key: Option<String>,
    /// What to say once the `:` commands on their way are done.
    after_configured: Option<String>,
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
    /// The sidebar.
    sidebar: Sidebar,
    /// The API the account is reached through.
    api: crate::account::Api,
    /// The request watching the folders for changes, while one is, and
    /// which folders it watches.
    watching: Option<RequestId>,
    watched: Vec<mett_core::FolderId>,
    /// The request ending what the mailbox keeps for watching, while it
    /// is under way.
    unwatching: Option<RequestId>,
    /// Ending it failed: it is tried again.
    unwatch_failed: bool,
    /// The `poll_epoch` the watch started in.
    watch_epoch: u64,
    /// Watching is held off until the user signs in again: its sign-in
    /// failed. (Only watching: the mailbox may just not let mett watch.)
    watch_paused: bool,
    /// What a watch said changed and is still to be checked (a check under
    /// way when it said may predate the change): the open folder, and the
    /// folders of `mailboxes` (the open one among them, for its counts).
    recheck_folder: bool,
    recheck_mailboxes: Vec<mett_core::FolderId>,
    /// Whether the mailbox can tell when folders change (until it says it
    /// can't).
    can_watch: bool,
    /// A retry of watching is set, after it failed.
    watch_retry: bool,
    /// The requests the server throttled, by how long until each is tried
    /// again (counted down every second) and which retry that is. A
    /// request's answer, or giving it up, takes it out.
    throttled: HashMap<RequestId, (Duration, u32)>,
    /// The countdown's timer, which goes while a request waits.
    throttle_timer: u64,
    /// The last requests given up on, whose waits that the worker told of
    /// before it stopped them are ignored. Those come right after giving
    /// up, so only the last few are kept.
    given_up: VecDeque<RequestId>,
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
            tabs: None,
            arrived: false,
            display: settings.display,
            error: None,
            input_note: None,
            alternates: compile_alternates(&settings.compose.alternates),
            strokes: VecDeque::new(),
            sign_in_strokes: VecDeque::new(),
            configuring: 0,
            played: 0,
            redraw: false,
            after_load: Vec::new(),
            notice: None,
            escape: None,
            cancelled: false,
            dismissed: false,
            input: None,
            composing: None,
            editing: None,
            speller: None,
            speller_loading: None,
            added_words: Vec::new(),
            compose_fetch: None,
            compose_unfetched: None,
            recall: None,
            unrecalled: false,
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
            threaded_count: 0,
            settled: false,
            chosen: HashMap::new(),
            collapse_all: false,
            limited: HashMap::new(),
            positions: HashMap::new(),
            listed_twice: false,
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
            terminal_width: usize::from(width),
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
            answering: None,
            fetching: None,
            unfetched_attachment: None,
            file_wait: None,
            last_file_op: 0,
            unsaved: None,
            home: settings.home,
            aliases: settings.aliases,
            alias_file: settings.alias_file,
            query: settings.query,
            new_alias: None,
            alias_key: None,
            after_configured: None,
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
            sidebar: Sidebar::default(),
            api: settings.api,
            watching: None,
            unwatching: None,
            unwatch_failed: false,
            watch_epoch: 0,
            watch_paused: false,
            watched: Vec::new(),
            recheck_folder: false,
            recheck_mailboxes: Vec::new(),
            can_watch: true,
            watch_retry: false,
            throttled: HashMap::new(),
            throttle_timer: 0,
            given_up: VecDeque::new(),
        };
        app.apply_sidebar(settings.sidebar);
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
            // The sidebar is beside the index and the pager.
            Screen::Index | Screen::Pager(_) | Screen::Text(_) if self.sidebar_focused() => {
                Menu::Sidebar
            }
            Screen::Index => Menu::Index,
            Screen::Pager(_) => Menu::Pager,
            Screen::Compose(_) => Menu::Compose,
            Screen::Browser(_) => Menu::Browser,
            Screen::Help(_) => Menu::Help,
            Screen::Attachments(_) => Menu::Attach,
            Screen::Query(_) => Menu::Query,
            Screen::Postponed(_) => Menu::Postpone,
            // Read like a message.
            Screen::Text(_) => Menu::Pager,
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
        if matches!(msg, Msg::Action(_)) {
            self.retry_sidebar();
        }
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
        // The sidebar lists every folder, if it shows them, once it is
        // on screen and they may have changed.
        effects.extend(self.list_sidebar_folders());
        effects
    }

    fn handle(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Action(action) => self.on_action(action),
            Msg::Response(response) => self.on_response(response),
            Msg::SignIn(event) => {
                let mut effects = self.on_sign_in(event);
                // Signed in (or not): watching, and the checks it asked
                // for, go on.
                if self.sign_in.is_none() {
                    effects.extend(self.watch());
                    effects.extend(self.recheck());
                }
                effects
            }
            Msg::Key(key) => self.on_key_press(key),
            Msg::Configured(reconfigured) => self.reconfigure(reconfigured),
            Msg::Edited(result) => self.on_edited(result),
            Msg::Close => self.close(),
            Msg::SpellerLoaded(settings, result) => self.on_speller_loaded(settings, result),
            Msg::Paste(text) => {
                self.paste(&text);
                Vec::new()
            }
            Msg::PasteClipboard => {
                self.paste_clipboard();
                Vec::new()
            }
            Msg::FileSaved(op, outcome) => self.on_file_saved(op, outcome),
            Msg::FileRead(op, path, result) => self.on_file_read(op, path, result),
            Msg::Failed(why) => {
                self.fail(why);
                Vec::new()
            }
            Msg::Appended(result) => self.on_appended(result),
            Msg::Timer(timer @ (Timer::RelistSentItems | Timer::RelistDrafts)) => {
                let folder = match timer {
                    Timer::RelistDrafts => mett_core::WellKnownFolder::Drafts,
                    _ => mett_core::WellKnownFolder::SentItems,
                };
                let open = self.folder.as_ref().and_then(|f| f.well_known);
                if open == Some(folder) {
                    if self.screen() == &Screen::Index {
                        return self.refresh();
                    }
                    self.relist_on_return = true;
                }
                Vec::new()
            }
            Msg::Timer(Timer::Poll(timer)) => self.on_poll_timer(timer),
            Msg::Timer(Timer::Watch) => {
                self.watch_retry = false;
                self.watch()
            }
            Msg::Syncing { folder, taken } => {
                self.on_syncing(&folder, taken);
                Vec::new()
            }
            Msg::Timer(Timer::Throttled(timer)) => self.count_down_throttled(timer),
            Msg::Resize { width, height } => {
                self.terminal_width = usize::from(width);
                self.body_height = body_height(height);
                self.relayout();
                Vec::new()
            }
        }
    }

    fn request(&mut self, request: Request) -> Effect {
        // A request given up on isn't tried again.
        if let Request::Cancel(query) = &request {
            self.throttled.remove(query);
            if self.given_up.len() == GIVEN_UP_KEPT {
                self.given_up.pop_front();
            }
            self.given_up.push_back(*query);
        }
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
        // Another account's tab can be gone to from the index and the pager,
        // and leaves what is pending here to be taken up when it is shown
        // again.
        if matches!(menu, Menu::Index | Menu::Pager)
            && matches!(
                action,
                Action::NextAccount | Action::PreviousAccount | Action::Account(_)
            )
        {
            return self.switch_account(action);
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
            // A lookup's answer may bring back the question it came from,
            // over the command.
            Action::EnterCommand if matches!(self.screens.last(), Some(Screen::Query(q)) if q.request.is_some()) =>
            {
                self.fail("Looking up addresses; wait a moment.");
            }
            Action::EnterCommand if menu != Menu::SignIn => return self.ask_command(),
            _ => {}
        }
        match self.menu() {
            Menu::Index => self.on_index_action(action),
            // Text is read in the pager, beside the sidebar too.
            Menu::Pager if matches!(self.screen(), Screen::Text(_)) => self
                .on_sidebar_action(action)
                .unwrap_or_else(|| self.on_text_action(action)),
            Menu::Pager => self.on_pager_action(action),
            Menu::Attach => self.on_attach_action(action),
            Menu::Query => self.on_query_action(action),
            Menu::Postpone => self.on_postponed_action(action),
            Menu::SignIn => self.on_sign_in_action(action),
            Menu::Compose => self.on_compose_action(action),
            Menu::Browser => self.on_browser_action(action),
            Menu::Help => self.on_help_action(action),
            Menu::Sidebar => self.on_sidebar_menu_action(action),
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
                // Signed in: watching can go on.
                self.watch_paused = false;
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
                // Nor a recall.
                self.unrecalled = false;
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
            .chain(self.attachments_request())
            .chain(self.fetching.as_ref().map(|f| f.request))
            .chain(self.recall_request())
            .collect()
    }

    /// The open folder's sync, while it loads, has `taken` of its messages
    /// in the cache: the status line says so, as a large folder's first
    /// sync takes a while.
    fn on_syncing(&mut self, folder: &mett_core::FolderId, taken: u64) {
        let Some(open) = self.folder.as_ref().filter(|f| &f.id == folder) else {
            return;
        };
        // Only before its first page comes: the sync comes before it, and
        // the pages tell how far they have got themselves.
        let first_page = self.listing.is_some()
            && self
                .listing_page
                .as_ref()
                .is_some_and(|page| page.cursor().is_none());
        if first_page && self.status.starts_with("Loading ") {
            self.status = format!("Loading {}: {taken} messages synced...", open.name);
        }
    }

    /// Keeps the sign-in window's scroll position within its text, which
    /// a taller or wider terminal makes shorter.
    fn clamp_sign_in_scroll(&mut self) {
        // The window covers all but the help line, and has a border above
        // and below its text.
        let room = (self.body_height + CHROME_ROWS).saturating_sub(3);
        // Across the whole terminal, over the sidebar too.
        let width = self.terminal_width;
        if let Some(window) = &mut self.sign_in {
            let last = window.rows(width).len().saturating_sub(room);
            window.scroll = window.scroll.min(last);
        }
    }

    /// Handles the actions the index and the pager share. Returns `None`
    /// for others.
    fn on_message_action(&mut self, action: Action) -> Option<Vec<Effect>> {
        if let Some(effects) = self
            .on_mailbox_action(action)
            .or_else(|| self.on_thread_action(action))
            .or_else(|| self.on_search_action(action))
            .or_else(|| self.on_invite_action(action))
            .or_else(|| self.on_sidebar_action(action))
        {
            return Some(effects);
        }
        let effects = match action {
            Action::Quit => self.leave_folder(AfterPurge::Quit),
            Action::Mail => self.start_compose(Kind::New),
            Action::Reply => self.start_compose(Kind::Reply),
            Action::GroupReply => self.start_compose(Kind::GroupReply),
            Action::Forward => self.start_compose(Kind::Forward),
            Action::ViewAttachments => self.view_attachments(),
            Action::Query => {
                self.ask_query();
                Vec::new()
            }
            Action::CreateAlias => self.create_alias(),
            Action::RecallMessage => self.recall_message(),
            _ => return None,
        };
        Some(effects)
    }

    fn on_index_action(&mut self, action: Action) -> Vec<Effect> {
        if let Some(effects) = self.on_message_action(action) {
            return effects;
        }
        let last = self.messages.len().saturating_sub(1);
        let page = self.body_height.max(1);
        match action {
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
                // A page on from where it started: pages to come may put
                // messages before the last one listed.
                let start = self.messages.get(self.selected).map(|m| m.id.clone());
                let target = self.selected + page;
                self.selected = target.min(last);
                if target > last && self.more_to_list() {
                    let target = match start {
                        Some(start) => Target::After(start, page),
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
            Action::DisplayMessage => {
                // A collapsed thread with unread mail folded into it opens
                // at its first unread message.
                self.open_at_folded_unread();
                return self.open_selected();
            }
            _ => {}
        }
        Vec::new()
    }

    fn on_pager_action(&mut self, action: Action) -> Vec<Effect> {
        if let Some(effects) = self.on_message_action(action) {
            return effects;
        }
        let page = self.pager_height().max(1);
        match action {
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
            Action::ToggleLinks => self.toggle_links(),
            _ => {}
        }
        Vec::new()
    }

    /// Shows the open message's links' URLs, or hides them again, keeping
    /// the line at the top of the screen there.
    fn toggle_links(&mut self) {
        let width = self.wrap_width();
        if let Some(Screen::Pager(pager)) = self.screens.last_mut() {
            pager.collapse_links(!pager.collapse_links, width);
        }
        self.clamp_scroll();
    }

    /// The width the pager and text views wrap their text at: `wrap`
    /// applied to the terminal's width.
    pub(super) fn wrap_width(&self) -> usize {
        self.display.wrap_width(self.width)
    }

    /// The rows the pager shows the message in: those under the help line
    /// and above the status bar, but for the mini-index's
    /// (`pager_index_lines`).
    pub(super) fn pager_height(&self) -> usize {
        let index = self
            .display
            .mini_index_rows(self.messages.len(), self.body_height);
        self.body_height - index
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
        let (width, height) = (self.wrap_width(), self.body_height);
        let pager_height = self.pager_height();
        // Pagers under other screens too: their rows (and search
        // highlights) must match the width they will be drawn at.
        for screen in &mut self.screens {
            match screen {
                Screen::Pager(pager) => pager.fit(width, pager_height),
                Screen::Text(view) => {
                    view.layout(width);
                    view.scroll = view.scroll.min(view.rows.len().saturating_sub(height));
                }
                _ => {}
            }
        }
        // A taller help screen shows more lines, not empty space.
        if let Some(Screen::Help(help)) = self.screens.last() {
            let last = self.help_count(help.menu).saturating_sub(height);
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
            links: Vec::new(),
            collapse_links: self.display.collapse_links,
        }));
        vec![fetch]
    }

    fn on_response(&mut self, response: Response) -> Vec<Effect> {
        // Not an outcome: the request carries on after the wait.
        if let Event::Throttled { wait, attempt } = response.event {
            return self.on_throttled(response.id, wait, attempt);
        }
        let id = response.id;
        self.throttled.remove(&id);
        let mut effects = self.handle_response(response);
        effects.extend(self.retry_if_requested(id));
        effects
    }

    fn handle_response(&mut self, Response { id, event }: Response) -> Vec<Effect> {
        match event {
            Event::Throttled { .. } => unreachable!("handled by on_response"),
            // A recall's steps.
            Event::FolderOpened(result) if self.recall_request() == Some(id) => {
                return self.on_recall_opened(result);
            }
            Event::MessagesListed(result) if self.recall_request() == Some(id) => {
                return self.on_recall_listed(result);
            }
            Event::MessageFetched(result) if self.recall_request() == Some(id) => {
                return self.on_recall_fetched(result);
            }
            Event::AttachmentsListed(result) if self.recall_request() == Some(id) => {
                return self.on_recall_attachments(result);
            }
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
            Event::FolderPolled(result) => {
                let mut effects = self.on_polled(id, result);
                effects.extend(self.recheck());
                return effects;
            }
            Event::Watched(result) => return self.on_watched(id, result),
            Event::MessagesListed(result) if self.listing == Some(id) => match result {
                Ok(page) => {
                    self.poll_paused = false;
                    let mut messages = page.messages;
                    messages.retain(|m| !self.removed_while_listing.contains(&m.id));
                    self.overlay_wanted_flags(&mut messages);
                    self.add_listed(messages, page.next.is_none());
                    self.see_index();
                    self.reselect(page.next.is_none());
                    let mut effects = Vec::new();
                    if let Some(cursor) = page.next {
                        // Keep loading until the whole folder is listed,
                        // saying how far it has got.
                        if let Some(folder) = &self.folder
                            && self.status.starts_with("Loading ")
                        {
                            self.status = format!(
                                "Loading {}: {} messages...",
                                folder.name,
                                self.total_messages()
                            );
                        }
                        effects = self.list(Page::after(cursor, self.page_size));
                    } else {
                        self.listing = None;
                        self.listing_page = None;
                        self.saved_flags.clear();
                        self.removed_while_listing.clear();
                        self.forget_missing_marks();
                        self.loaded();
                        self.relisted();
                        // A change told of while it was listed.
                        effects.extend(self.recheck());
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
                    // What was listed is threaded, while the rest waits.
                    self.thread_listed();
                    // Pages asked for again come after the saves.
                    self.saved_flags.clear();
                    self.removed_while_listing.clear();
                    self.load_failed(e);
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
            Event::FoldersListed(result) if self.sidebar_request() == Some(id) => {
                self.on_sidebar_listed(result);
            }
            Event::FoldersListed(result) => return self.on_folders_listed(id, result),
            Event::AttachmentsListed(result) => return self.on_attachments_listed(id, result),
            Event::AttachmentFetched(result) => return self.on_attachment_fetched(id, result),
            Event::InvitationAnswered(result) => return self.on_invitation_answered(id, result),
            Event::Queried(result) => return self.on_queried(id, result),
            Event::MailboxesChecked(checked) => {
                let mut effects = self.on_mailboxes_checked(id, checked);
                effects.extend(self.recheck());
                return effects;
            }
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
            Event::Sent(result, kept) => return self.on_delivered(id, result, kept),
            Event::DraftSaved(result, kept) => {
                return self.on_delivered(id, result.map(drop), kept);
            }
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
                self.load_failed(e);
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
        pager.show(&message, self.display.wrap_width(self.width));
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

    /// The summary of a message in the folder, shown or hidden by the
    /// limit.
    fn summary(&self, id: &MessageId) -> Option<&MessageSummary> {
        // Most often the selected one (IDs are unique).
        if let Some(selected) = self.messages.get(self.selected).filter(|s| &s.id == id) {
            return Some(selected);
        }
        self.folder_messages().find(|s| &s.id == id)
    }

    /// The summary of a message in the folder, to change.
    fn summary_mut(&mut self, id: &MessageId) -> Option<&mut MessageSummary> {
        if self
            .messages
            .get(self.selected)
            .is_some_and(|s| &s.id == id)
        {
            return self.messages.get_mut(self.selected);
        }
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

    /// Counts the addresses the mailbox reported as the user's own.
    fn add_mailbox_addresses(&mut self) {
        for address in &self.mailbox_addresses {
            if !self.settings.me.contains(address) {
                self.settings.me.push(address.clone());
            }
        }
    }

    /// Takes in the settings after a `:` command changed them. The account
    /// stays the same.
    pub(super) fn apply_settings(&mut self, settings: Settings) {
        self.page_size = settings.page_size;
        self.home = settings.home;
        self.aliases = settings.aliases;
        self.alias_file = settings.alias_file;
        self.query = settings.query;
        self.alternates = compile_alternates(&settings.compose.alternates);
        self.settings = settings.compose;
        // The configuration doesn't know what the mailbox told.
        self.add_mailbox_addresses();
        self.mailbox_root = settings.folder;
        self.trash = settings.trash;
        self.delete = settings.delete;
        self.resolve = settings.resolve;
        self.shortcuts = settings.shortcuts;
        let collapse = settings.display.collapse_links;
        let changed = collapse != self.display.collapse_links;
        self.display = settings.display;
        if changed {
            // Open messages follow `collapse_links` (also those shown as
            // `toggle-links` chose).
            let width = self.wrap_width();
            for screen in &mut self.screens {
                if let Screen::Pager(pager) = screen {
                    pager.collapse_links(collapse, width);
                }
            }
        }
        self.apply_sidebar(settings.sidebar);
        // `wrap` may have changed: the pager and text views wrap again.
        self.clamp_scroll();
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
        self.threaded_count = 0;
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
        // Shown, or folded into a collapsed thread that is (once listed:
        // looking through the index for it at every page would be slow).
        let listed = self.positions.contains_key(id);
        if let Some(found) = listed.then(|| self.entry_of(id)).flatten() {
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
        let loading = self.listing.is_some();
        // The next thread needs the threads of what was listed, once it
        // all is (threading at every page would be slow).
        if matches!(target, Target::NextThread | Target::PreviousThread) {
            if loading {
                return Vec::new();
            }
            self.thread_listed();
        }
        let last = self.messages.len().saturating_sub(1);
        let selected = match target {
            // A later page may bring a thread that goes in between.
            Target::NextThread | Target::PreviousThread if loading => return Vec::new(),
            Target::NextThread | Target::PreviousThread => {
                match self.thread_root_toward(matches!(target, Target::NextThread)) {
                    Ok(at) => at,
                    Err(reason) => {
                        self.pending = None;
                        self.fail(reason);
                        return Vec::new();
                    }
                }
            }
            Target::After(message, steps) => {
                // While the folder loads, a later page may put a message
                // in between (mail from the same time, a draft by its own
                // date, a reply's start): wait for all of it, as mutt
                // reads every message before showing any.
                // (Unsorted, later pages only come after; reversed, before.)
                let in_page_order = self.ordering.sort.method == crate::sort::Method::Unsorted
                    && !self.ordering.sort.reverse;
                if loading && !in_page_order {
                    return Vec::new();
                }
                // Folded into a collapsed thread since: from the thread.
                let from = self.entry_of(&message);
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
            Screen::Index
            | Screen::Compose(_)
            | Screen::Browser(_)
            | Screen::Help(_)
            | Screen::Attachments(_)
            | Screen::Text(_)
            | Screen::Query(_)
            | Screen::Postponed(_) => Vec::new(),
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
    /// The longest wait of a save the server throttled, if one is waiting.
    pub fn save_wait(&self) -> Option<Duration> {
        self.throttled
            .iter()
            .filter(|(id, _)| self.saves(**id))
            .map(|(_, (left, _))| *left)
            .max()
    }

    /// Whether request `id` saves a change (one [`App::saving`] waits for).
    pub fn saves(&self, id: RequestId) -> bool {
        self.writing
            .as_ref()
            .is_some_and(|(writing, _)| *writing == id)
            || self.delivery_request() == Some(id)
            || self.discarding.contains_key(&id)
    }

    pub fn saving(&self) -> bool {
        self.writing.is_some()
            || !self.writes.is_empty()
            || self.delivering()
            || !self.discarding.is_empty()
    }

    /// The folder failed to open or load. A search waiting for it goes on,
    /// and the keys waiting for it play, once a sign-in lets it load;
    /// otherwise, the keys play anyway.
    fn load_failed(&mut self, e: Error) {
        if !matches!(e, Error::Auth(_)) {
            self.pending_search = None;
            self.loaded();
        }
        self.report(e);
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
    /// Clears the status if it is one of `statuses`, which no longer hold.
    pub(super) fn clear_status_if(&mut self, statuses: &[&str]) {
        if statuses.contains(&self.status.as_str()) {
            self.status.clear();
        }
    }

    pub(super) fn fail(&mut self, text: impl Into<String>) {
        self.status = text.into();
        self.error = Some(self.status.clone());
        // The built-in editor hides the message line: it says it.
        if let Some(editing) = &mut self.editing {
            editing.get_mut().say(&self.status);
        }
    }

    /// [`Self::fail`], shown over the question being asked too.
    pub(super) fn fail_at_question(&mut self, text: impl Into<String>) {
        self.fail(text);
        self.input_note = Some(self.status.clone());
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

/// Moves the selection `selected` of a list menu of `count` entries as
/// `action` says, by `page` entries a page. Returns whether `action` moves
/// it.
fn navigate(selected: &mut usize, action: Action, count: usize, page: usize) -> bool {
    let last = count.saturating_sub(1);
    *selected = match action {
        Action::NextEntry => (*selected + 1).min(last),
        Action::PreviousEntry => selected.saturating_sub(1),
        Action::FirstEntry => 0,
        Action::LastEntry => last,
        Action::NextPage => (*selected + page).min(last),
        Action::PreviousPage => selected.saturating_sub(page),
        _ => return false,
    };
    true
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
mod tests;
