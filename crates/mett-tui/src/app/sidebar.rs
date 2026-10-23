//! mutt's sidebar: the `mailboxes` in a column beside the menus, with
//! their counts, and a cursor of its own that `sidebar-open` opens.
//! Without `mailboxes`, it lists every folder instead (mett's), and
//! `sidebar-focus` gives it the keys (mett's too).
//!
//! The entries (which mailboxes show, in what order, under what name) are
//! made again only when what they show changes: the mailboxes are checked,
//! the settings change, another folder opens, or ordered by counts, the
//! open folder's change. Drawing them only formats the rows on screen
//! (see `view/sidebar.rs`).

use mett_core::Folder;

use super::mailbox::{BrowserEntry, tree};
use super::{AfterPurge, App, Effect, MailboxEntry, effect_id};
use crate::account::Api;
use crate::action::Action;
use crate::config::mailbox::{FolderPath, same_name as same_name_ignoring_case};

/// Whether two folder names are the same, by the rules of the mailbox's
/// `api`: Office365 ignores case, IMAP doesn't (but for the top-level
/// `INBOX`, `root`).
fn same_name(a: &str, b: &str, api: Api, root: bool) -> bool {
    match api {
        Api::Imap => {
            a == b || (root && a.eq_ignore_ascii_case("INBOX") && b.eq_ignore_ascii_case("INBOX"))
        }
        Api::Graph | Api::Ews => same_name_ignoring_case(a, b),
    }
}
use crate::worker::{Request, RequestId};

/// How the sidebar's entries are ordered (`sidebar_sort_method`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortMethod {
    /// As the `mailboxes` commands gave them (`unsorted`,
    /// `mailbox-order`).
    #[default]
    Unsorted,
    /// By path, ignoring case (`path`).
    Path,
    /// By label, or path, ignoring case (`alpha`, `name`).
    Alpha,
    /// Most messages first (`count`).
    Count,
    /// Most unread messages first (`unread`, `new`).
    Unread,
    /// Most flagged messages first (`flagged`).
    Flagged,
}

/// `sidebar_sort_method`: a method, maybe reversed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SidebarSort {
    /// The method.
    pub method: SortMethod,
    /// `reverse-`.
    pub reverse: bool,
}

impl SidebarSort {
    /// Parses a value of `sidebar_sort_method`, as mutt names them.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (reverse, name) = match text.strip_prefix("reverse-") {
            Some(name) => (true, name),
            None => (false, text),
        };
        let method = match name {
            "unsorted" | "mailbox-order" => SortMethod::Unsorted,
            "path" => SortMethod::Path,
            "alpha" | "name" => SortMethod::Alpha,
            "count" => SortMethod::Count,
            "unread" | "new" => SortMethod::Unread,
            "flagged" => SortMethod::Flagged,
            _ => {
                return Err(format!(
                    "{text}: expected unsorted, path, alpha, name, count, unread, new or flagged, \
                     maybe after reverse-"
                ));
            }
        };
        Ok(Self { method, reverse })
    }
}

/// What the sidebar needs from the configuration (`sidebar_*`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebarSettings {
    /// `sidebar_visible`.
    pub visible: bool,
    /// `sidebar_width`, the divider included, as in mutt.
    pub width: usize,
    /// mett's `sidebar_min_width`: on a terminal narrower than this, the
    /// sidebar hides (or 0 not to).
    pub min_width: usize,
    /// `sidebar_format`.
    pub format: String,
    /// `sidebar_short_path`.
    pub short_path: bool,
    /// `sidebar_folder_indent`.
    pub folder_indent: bool,
    /// `sidebar_indent_string`.
    pub indent_string: String,
    /// `sidebar_delim_chars`.
    pub delim_chars: String,
    /// `sidebar_divider_char`.
    pub divider_char: String,
    /// `sidebar_sort_method`.
    pub sort: SidebarSort,
    /// `sidebar_new_mail_only`.
    pub new_mail_only: bool,
    /// `sidebar_next_new_wrap`.
    pub next_new_wrap: bool,
    /// `sidebar_on_right` (neomutt's).
    pub on_right: bool,
    /// `sidebar_whitelist`: mailboxes shown even with `new_mail_only`.
    pub whitelist: Vec<FolderPath>,
    /// The `spoolfile` mailbox, for `color sidebar_spoolfile`.
    pub spoolfile: Option<FolderPath>,
}

impl Default for SidebarSettings {
    /// The variables' defaults.
    fn default() -> Self {
        let var = |name| {
            crate::config::VARS
                .iter()
                .find(|v| v.name == name)
                .map_or("", |v| v.default)
        };
        let number = |name| var(name).parse().unwrap_or_default();
        Self {
            visible: false,
            width: number("sidebar_width"),
            min_width: number("sidebar_min_width"),
            format: var("sidebar_format").to_owned(),
            short_path: false,
            folder_indent: false,
            indent_string: var("sidebar_indent_string").to_owned(),
            delim_chars: var("sidebar_delim_chars").to_owned(),
            divider_char: var("sidebar_divider_char").to_owned(),
            sort: SidebarSort::default(),
            new_mail_only: false,
            next_new_wrap: false,
            on_right: false,
            whitelist: Vec::new(),
            spoolfile: None,
        }
    }
}

/// A mailbox as the sidebar shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebarEntry {
    /// The folder.
    pub path: FolderPath,
    /// What `%B` shows: its label, or its path relative to `folder`,
    /// shortened and indented as the settings say.
    pub name: String,
    /// Whether it is the open folder.
    pub open: bool,
    /// Whether it has new mail.
    pub new: bool,
    /// Its unread and total counts, as last checked, if known.
    pub counts: Option<(u32, u32)>,
    /// Whether it is the `spoolfile`.
    pub spoolfile: bool,
}

impl SidebarEntry {
    /// Whether it has new or unread mail, as `sidebar-next-new` looks for
    /// and `color sidebar_new` shows.
    pub fn has_mail(&self) -> bool {
        self.new || self.counts.is_some_and(|(unread, _)| unread > 0)
    }
}

/// The sidebar: its settings, its entries and its cursor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sidebar {
    /// How it is drawn and ordered.
    pub settings: SidebarSettings,
    /// The mailboxes it shows, in order.
    pub entries: Vec<SidebarEntry>,
    /// The entry its cursor is on (`sidebar_highlight`), when there are
    /// any.
    pub highlight: usize,
    /// The open folder's unread and total counts, as the index has them,
    /// when they order the entries.
    open_counts: Option<(usize, usize)>,
    /// Whether it has the keys (`sidebar-focus`).
    focused: bool,
    /// Every folder, in tree order, as last listed: the sidebar shows them
    /// when there are no `mailboxes`.
    folders: Option<Vec<BrowserEntry>>,
    /// The request listing them, while it is pending, and the checks'
    /// epoch when it was made.
    listing: Option<(RequestId, u64)>,
    /// Whether they were listed since they last may have changed; until
    /// then, they are listed again once the sidebar shows.
    listed: bool,
    /// Whether listing them failed: they are listed again once the user
    /// does something, rather than over and over.
    failed: bool,
}

/// The fewest columns the sidebar leaves the menus, unless
/// `sidebar_min_width` is 0: fewer would cut the index's columns short.
pub const MIN_MENU_WIDTH: usize = 40;

impl SidebarSettings {
    /// How many columns the sidebar takes, its divider included, on a
    /// terminal `total` columns wide: none while it is hidden, or while
    /// the terminal is narrower than `sidebar_min_width` or would leave
    /// the menus fewer than [`MIN_MENU_WIDTH`] columns (mett's); otherwise
    /// `sidebar_width`, leaving the menus at least a column, as in mutt.
    pub fn columns(&self, total: usize) -> usize {
        if !self.visible {
            return 0;
        }
        if self.min_width > 0 && (total < self.min_width || total < self.width + MIN_MENU_WIDTH) {
            return 0;
        }
        self.width.min(total.saturating_sub(1))
    }

    /// Whether the sidebar is set visible but the terminal is too narrow
    /// for it.
    pub fn too_narrow(&self, total: usize) -> bool {
        self.visible && self.width > 0 && self.columns(total) == 0
    }
}

impl App {
    /// The sidebar.
    pub fn sidebar(&self) -> &Sidebar {
        &self.sidebar
    }

    /// Whether the sidebar has the keys: given them, and on screen.
    pub fn sidebar_focused(&self) -> bool {
        self.sidebar.focused && self.sidebar_shown()
    }

    /// Takes in the sidebar's settings. A new order puts the cursor back
    /// on the open folder, as in mutt.
    pub(super) fn apply_sidebar(&mut self, settings: SidebarSettings) {
        let resorted = settings.sort != self.sidebar.settings.sort;
        self.sidebar.settings = settings;
        self.rebuild_sidebar(resorted);
        self.relayout();
    }

    /// Gives the menus the width the sidebar leaves them, and lays out
    /// again what was laid out for another.
    pub(super) fn relayout(&mut self) {
        let sidebar = self.sidebar.settings.columns(self.terminal_width);
        self.width = self.terminal_width - sidebar;
        // Hidden, it gives the keys back: shown again, it hasn't them.
        if sidebar == 0 {
            self.sidebar.focused = false;
        }
        self.clamp_scroll();
        self.clamp_sign_in_scroll();
    }

    /// Makes the sidebar's entries again from the mailboxes and what is
    /// known of them. The cursor stays on its mailbox if it is still
    /// shown, or with `follow_open` (another folder opened), moves to the
    /// open one.
    pub(super) fn rebuild_sidebar(&mut self, follow_open: bool) {
        let mut entries = if self.mailboxes.is_empty() {
            self.folder_entries()
        } else {
            self.mailbox_entries_for_sidebar()
        };
        let settings = &self.sidebar.settings;
        let sort = settings.sort;
        if sort.method == SortMethod::Unsorted {
            if sort.reverse {
                entries.reverse();
            }
        } else {
            // Stable: mailboxes alike stay in their order.
            entries.sort_by(|(_, a_key, a_count), (_, b_key, b_count)| {
                let order = match sort.method {
                    SortMethod::Path | SortMethod::Alpha => a_key.cmp(b_key),
                    // Most first, as in mutt.
                    _ => b_count.cmp(a_count),
                };
                if sort.reverse { order.reverse() } else { order }
            });
        }
        let entries: Vec<SidebarEntry> = entries.into_iter().map(|(entry, ..)| entry).collect();
        let previous = self
            .sidebar
            .entries
            .get(self.sidebar.highlight)
            .map(|entry| &entry.path);
        let kept = previous.and_then(|path| entries.iter().position(|e| e.path == *path));
        let open = entries.iter().position(|e| e.open);
        self.sidebar.highlight = if follow_open {
            open.or(kept)
        } else {
            kept.or(open)
        }
        .unwrap_or(0);
        self.sidebar.entries = entries;
    }

    /// The `mailboxes`, as the sidebar shows them, each with what orders
    /// it: its name or path, and its count.
    fn mailbox_entries_for_sidebar(&self) -> Vec<(SidebarEntry, String, usize)> {
        let settings = &self.sidebar.settings;
        self.mailboxes
            .iter()
            .filter_map(|mailbox| {
                let path = &mailbox.path;
                let open = self
                    .folder
                    .as_ref()
                    .filter(|open| self.folder_at(path) == Some(&open.id));
                // A mailbox that isn't checked has the counts it had when
                // it was opened.
                let folder = self
                    .mailbox_state
                    .get(path)
                    .and_then(|state| state.folder.as_ref())
                    .or(open);
                let entry = SidebarEntry {
                    path: path.clone(),
                    name: sidebar_name(mailbox, settings),
                    open: open.is_some(),
                    new: self.has_new_mail(mailbox),
                    counts: folder.map(|f| (f.unread_count, f.total_count)),
                    spoolfile: (settings.spoolfile.as_ref())
                        .is_some_and(|spoolfile| self.same_mailbox(spoolfile, path)),
                };
                // Office365 doesn't count flagged mail: with
                // `sidebar_new_mail_only`, only new and unread mail shows
                // a mailbox (besides the open one and the whitelisted).
                let whitelisted =
                    || (settings.whitelist.iter()).any(|listed| self.same_mailbox(listed, path));
                if !self.shows(&entry, whitelisted) {
                    return None;
                }
                // `alpha` by the label, or the path as `%B` shows it
                // (relative to `folder`); `path` by the whole path.
                let key = match (settings.sort.method, &mailbox.label) {
                    (SortMethod::Alpha, Some(label)) => label.clone(),
                    (SortMethod::Alpha, None) => mailbox
                        .text
                        .strip_prefix('=')
                        .unwrap_or(&mailbox.text)
                        .to_owned(),
                    _ => crate::config::mailbox::expand_shortcut(&mailbox.text, &self.mailbox_root),
                }
                .to_lowercase();
                let count = self.sort_count(&entry);
                Some((entry, key, count))
            })
            .collect()
    }

    /// Every folder, as listed, as the sidebar shows them (when there are
    /// no `mailboxes`), each with what orders it: its path, and its count.
    fn folder_entries(&self) -> Vec<(SidebarEntry, String, usize)> {
        let settings = &self.sidebar.settings;
        let open = self.folder.as_ref().map(|f| &f.id);
        (self.sidebar.folders.iter().flatten())
            .filter_map(|folder| {
                let entry = SidebarEntry {
                    path: folder.target.clone(),
                    name: folder_name(folder, settings),
                    open: open.is_some() && folder.id.as_ref() == open,
                    // Unread mail shows by its count.
                    new: false,
                    counts: folder.counts,
                    spoolfile: (settings.spoolfile.as_ref())
                        .is_some_and(|spoolfile| self.names_folder(spoolfile, folder)),
                };
                let whitelisted =
                    || (settings.whitelist.iter()).any(|listed| self.names_folder(listed, folder));
                if !self.shows(&entry, whitelisted) {
                    return None;
                }
                let count = self.sort_count(&entry);
                Some((entry, folder.path.to_lowercase(), count))
            })
            .collect()
    }

    /// Whether `path` names the listed `folder`: as written, below the
    /// listed well-known folder it names (`=@sentitems`), or as found.
    fn names_folder(&self, path: &FolderPath, folder: &BrowserEntry) -> bool {
        let below_well_known = match path {
            FolderPath::WellKnown(role, below) => (self.sidebar.folders.iter().flatten())
                .find(|listed| listed.well_known == Some(*role))
                .and_then(|listed| match &listed.target {
                    FolderPath::Named(names) => Some(FolderPath::Named(
                        names.iter().chain(below).cloned().collect(),
                    )),
                    _ => None,
                }),
            _ => None,
        };
        let same = |a: &FolderPath, b: &FolderPath| same_path(a, b, self.api);
        same(path, &folder.target)
            || below_well_known.is_some_and(|named| same(&named, &folder.target))
            || folder
                .id
                .as_ref()
                .is_some_and(|id| self.folder_at(path) == Some(id))
    }

    /// Whether the sidebar shows `entry`, `whitelisted` or not: all of
    /// them, unless `sidebar_new_mail_only`.
    fn shows(&self, entry: &SidebarEntry, whitelisted: impl FnOnce() -> bool) -> bool {
        // Office365 doesn't count flagged mail: with
        // `sidebar_new_mail_only`, only new and unread mail shows a
        // mailbox (besides the open one and the whitelisted).
        !self.sidebar.settings.new_mail_only || entry.open || entry.has_mail() || whitelisted()
    }

    /// The count `sidebar_sort_method` orders `entry` by, if any.
    fn sort_count(&self, entry: &SidebarEntry) -> usize {
        // The open folder's as the index has them, if they are followed.
        let counts = match self.sidebar.open_counts.filter(|_| entry.open) {
            Some(live) => Some(live),
            None => entry.counts.map(|(u, t)| (u as usize, t as usize)),
        };
        match (self.sidebar.settings.sort.method, counts) {
            (SortMethod::Count, Some((_, total))) => total,
            (SortMethod::Unread, Some((unread, _))) => unread,
            _ => 0,
        }
    }

    /// Lists every folder for the sidebar, if it shows them and they may
    /// have changed since last listed. Not while the user is to sign in,
    /// or after a failed sign-in: background requests would only ask
    /// again and again.
    pub(super) fn list_sidebar_folders(&mut self) -> Vec<Effect> {
        let wanted = self.mailboxes.is_empty()
            && !self.sidebar.listed
            && !self.sidebar.failed
            && self.sidebar.listing.is_none()
            && self.sidebar_shown()
            && !self.quit
            && !self.poll_paused
            && self.sign_in.is_none();
        if !wanted {
            return Vec::new();
        }
        let request = self.request(Request::ListFolders);
        self.sidebar.listing = Some((effect_id(&request), self.poll_epoch));
        // Up to date once listed, unless they change meanwhile.
        self.sidebar.listed = true;
        vec![request]
    }

    /// The request listing the sidebar's folders, while it is pending.
    pub(super) fn sidebar_request(&self) -> Option<RequestId> {
        self.sidebar.listing.map(|(id, _)| id)
    }

    /// Takes in the folders listed for the sidebar. If they couldn't be,
    /// those it has stay, and the failure is reported as a check's; they
    /// are listed again once the user does something.
    pub(super) fn on_sidebar_listed(&mut self, result: mett_core::Result<Vec<Folder>>) {
        let Some((_, epoch)) = self.sidebar.listing.take() else {
            return;
        };
        match result {
            Ok(folders) => {
                self.sidebar.folders = Some(tree(folders));
                self.rebuild_sidebar(false);
            }
            Err(e) => {
                self.sidebar.listed = false;
                self.sidebar.failed = true;
                self.background_error(e, epoch);
            }
        }
    }

    /// The folders' counts may have changed: they are listed again.
    pub(super) fn sidebar_outdated(&mut self) {
        self.sidebar.listed = false;
    }

    /// The user did something: folders that failed to list are listed
    /// again.
    pub(super) fn retry_sidebar(&mut self) {
        self.sidebar.failed = false;
    }

    /// Brings the sidebar up to date before the screen is drawn: ordered
    /// by counts, it follows the open folder's as the index has them (mail
    /// was read, moved or listed). They are counted once a frame, as the
    /// status bar counts them, and only then.
    pub fn before_draw(&mut self) {
        let by_counts = matches!(
            self.sidebar.settings.sort.method,
            SortMethod::Count | SortMethod::Unread
        );
        let counts = (by_counts && self.sidebar_shown() && self.folder.is_some()).then(|| {
            let unread = self.folder_messages().filter(|m| !m.flags.read).count();
            (unread, self.total_messages())
        });
        if counts != self.sidebar.open_counts {
            self.sidebar.open_counts = counts;
            self.rebuild_sidebar(false);
        }
    }

    /// Whether the sidebar is on screen: set visible, on a terminal wide
    /// enough.
    fn sidebar_shown(&self) -> bool {
        self.sidebar.settings.columns(self.terminal_width) > 0
    }

    /// Handles the sidebar's actions, in the index and the pager. Returns
    /// `None` for others.
    pub(super) fn on_sidebar_action(&mut self, action: Action) -> Option<Vec<Effect>> {
        let count = self.sidebar.entries.len();
        let last = count.saturating_sub(1);
        let at = self.sidebar.highlight.min(last);
        let page = self.body_height.max(1);
        // The open folder's unread mail is the index's.
        let has_mail = |app: &Self, i: usize| {
            let entry = &app.sidebar.entries[i];
            if entry.open {
                entry.new || app.folder_messages().any(|m| !m.flags.read)
            } else {
                entry.has_mail()
            }
        };
        let to = match action {
            Action::SidebarToggleVisible => return Some(self.toggle_sidebar()),
            Action::SidebarFocus => {
                if self.sidebar.settings.too_narrow(self.terminal_width) {
                    self.status = self.narrow_note();
                } else if !self.sidebar_shown() {
                    self.fail("The sidebar is hidden: set sidebar_visible to show it.");
                } else {
                    self.sidebar.focused = true;
                }
                return Some(Vec::new());
            }
            Action::SidebarFirst
            | Action::SidebarLast
            | Action::SidebarNext
            | Action::SidebarPrev
            | Action::SidebarNextNew
            | Action::SidebarPrevNew
            | Action::SidebarPageDown
            | Action::SidebarPageUp
            | Action::SidebarOpen => {
                // As in mutt, they do nothing while it is hidden.
                if self.sidebar.settings.too_narrow(self.terminal_width) {
                    self.status = self.narrow_note();
                }
                if !self.sidebar_shown() || count == 0 {
                    return Some(Vec::new());
                }
                match action {
                    Action::SidebarFirst => 0,
                    Action::SidebarLast => last,
                    Action::SidebarNext => (at + 1).min(last),
                    Action::SidebarPrev => at.saturating_sub(1),
                    // The first entry of the next page, or the last.
                    Action::SidebarPageDown => (at / page * page + page).min(last),
                    // The last entry of the previous page, or the first.
                    Action::SidebarPageUp => (at / page * page).saturating_sub(1),
                    Action::SidebarNextNew | Action::SidebarPrevNew => {
                        let wrap = self.sidebar.settings.next_new_wrap;
                        let forward = action == Action::SidebarNextNew;
                        let steps = (1..count).map(|step| {
                            if forward {
                                (at + step < count || wrap).then_some((at + step) % count)
                            } else {
                                (step <= at || wrap).then_some((at + count - step) % count)
                            }
                        });
                        match steps.map_while(|i| i).find(|&i| has_mail(self, i)) {
                            Some(i) => i,
                            None => return Some(Vec::new()),
                        }
                    }
                    _ => {
                        let path = self.sidebar.entries[at].path.clone();
                        return Some(self.leave_folder(AfterPurge::Open(path)));
                    }
                }
            }
            _ => return None,
        };
        self.sidebar.highlight = to;
        Some(Vec::new())
    }

    /// Handles the actions of the sidebar while it has the keys: they
    /// move its cursor, and opening its mailbox, or leaving, gives the
    /// keys back.
    pub(super) fn on_sidebar_menu_action(&mut self, action: Action) -> Vec<Effect> {
        match action {
            Action::Exit | Action::SidebarFocus => {
                self.sidebar.focused = false;
                Vec::new()
            }
            Action::SidebarOpen => {
                self.sidebar.focused = false;
                self.on_sidebar_action(action).unwrap_or_default()
            }
            _ => self.on_sidebar_action(action).unwrap_or_default(),
        }
    }

    /// Says how wide the terminal has to be for the sidebar.
    fn narrow_note(&self) -> String {
        let settings = &self.sidebar.settings;
        let wide = settings.min_width.max(settings.width + MIN_MENU_WIDTH);
        format!("The sidebar shows once the terminal is {wide} columns wide.")
    }

    /// `sidebar-toggle-visible`: shows or hides the sidebar at once, and
    /// toggles `sidebar_visible` in the configuration, which the next
    /// settings take.
    fn toggle_sidebar(&mut self) -> Vec<Effect> {
        self.sidebar.settings.visible = !self.sidebar.settings.visible;
        self.relayout();
        if self.sidebar.settings.too_narrow(self.terminal_width) {
            self.after_configured = Some(self.narrow_note());
        }
        // Like a command typed at `:`: keys after it wait for it.
        self.answer_command("toggle sidebar_visible".into())
    }
}

impl App {
    /// The folder at `path`, as far as known: as last checked, as found
    /// when mail was moved there, or the open folder, if opened by it.
    fn folder_at(&self, path: &FolderPath) -> Option<&mett_core::FolderId> {
        let checked = self.mailbox_state.get(path).and_then(|s| s.folder.as_ref());
        checked
            .map(|folder| &folder.id)
            .or_else(|| self.folder_ids.get(path))
            .or_else(|| {
                let open = self.folder.as_ref()?;
                // A well-known folder (`=@inbox`) is the open one if it
                // has that role; the open well-known folder, which is at
                // the top, is the one named after it (`=Inbox`), even
                // where the mailbox wasn't checked.
                let names_open = match path {
                    FolderPath::WellKnown(role, below) => {
                        below.is_empty() && open.well_known == Some(*role)
                    }
                    FolderPath::Named(names) => {
                        open.well_known.is_some()
                            && matches!(names.as_slice(), [name] if same_name(name, &open.name, self.api, true))
                    }
                    FolderPath::Root => false,
                };
                (same_path(path, &self.path, self.api) || names_open).then_some(&open.id)
            })
    }

    /// Whether `a` and `b` name the same mailbox: as written, ignoring
    /// case as Office365 does (`=@inbox` and `=Inbox` are told apart
    /// until their folders are known), or as found.
    fn same_mailbox(&self, a: &FolderPath, b: &FolderPath) -> bool {
        same_path(a, b, self.api)
            || self
                .folder_at(a)
                .is_some_and(|id| self.folder_at(b) == Some(id))
    }
}

/// Whether `a` and `b` are the same path, ignoring case as Office365
/// does (`api`: IMAP servers tell names apart by case, the inbox's aside).
fn same_path(a: &FolderPath, b: &FolderPath, api: Api) -> bool {
    // `root`: whether the names start at the top of the mailbox.
    let same_names = |a: &[String], b: &[String], root: bool| {
        a.len() == b.len()
            && a.iter()
                .zip(b)
                .enumerate()
                .all(|(at, (a, b))| same_name(a, b, api, root && at == 0))
    };
    match (a, b) {
        (FolderPath::Root, FolderPath::Root) => true,
        (FolderPath::WellKnown(a, a_below), FolderPath::WellKnown(b, b_below)) => {
            a == b && same_names(a_below, b_below, false)
        }
        (FolderPath::Named(a), FolderPath::Named(b)) => same_names(a, b, true),
        _ => false,
    }
}

/// What `%B` shows for `mailbox`, as mutt makes it: its path relative to
/// `folder` (or whole, if it isn't below it), with `sidebar_short_path`
/// only its last part, then its label instead if it has one, and with
/// `sidebar_folder_indent` indented by its depth below `folder`.
fn sidebar_name(mailbox: &MailboxEntry, settings: &SidebarSettings) -> String {
    let (path, below) = match mailbox.text.strip_prefix('=') {
        Some(rest) => (rest, true),
        None => (mailbox.text.as_str(), false),
    };
    let delimiter = |c: char| settings.delim_chars.contains(c);
    if settings.delim_chars.is_empty() {
        return mailbox.label.clone().unwrap_or_else(|| path.to_owned());
    }
    let (mut name, mut indent) = (path, 0);
    if settings.short_path || settings.folder_indent {
        // A trailing delimiter doesn't make it deeper.
        let depth = path.chars().filter(|&c| delimiter(c)).count()
            + usize::from(!path.ends_with(delimiter));
        let parent = depth.saturating_sub(1);
        indent = if below { parent } else { 0 };
        if settings.short_path && parent > 0 {
            // After the delimiter that ends its parent's path.
            let mut left = parent;
            let cut = path
                .char_indices()
                .find(|&(_, c)| {
                    if delimiter(c) {
                        left -= 1;
                    }
                    left == 0
                })
                .map_or(path.len(), |(at, c)| at + c.len_utf8());
            name = &path[cut..];
        }
    }
    let name = mailbox.label.as_deref().unwrap_or(name);
    if settings.folder_indent && indent > 0 {
        format!("{}{name}", settings.indent_string.repeat(indent))
    } else {
        name.to_owned()
    }
}

/// What `%B` shows for a folder listed for the sidebar: its path from
/// the top (`Inbox/Projects`), with `sidebar_short_path` only its name,
/// and with `sidebar_folder_indent` indented by its depth.
fn folder_name(folder: &BrowserEntry, settings: &SidebarSettings) -> String {
    let name = if settings.short_path {
        &folder.name
    } else {
        &folder.path
    };
    if settings.folder_indent && folder.depth > 0 {
        format!("{}{name}", settings.indent_string.repeat(folder.depth))
    } else {
        name.clone()
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyCode;

    use super::*;
    use crate::app::testing::*;
    use crate::app::{Msg, Timer};
    use crate::keymap::Menu;
    use crate::worker::{Event, Request};

    /// The names of the entries, `*` marking the cursor's.
    fn shown(app: &App) -> Vec<String> {
        let sidebar = &app.sidebar;
        (sidebar.entries.iter().enumerate())
            .map(|(i, e)| {
                let mark = if i == sidebar.highlight { "*" } else { "" };
                format!("{mark}{}", e.name)
            })
            .collect()
    }

    fn sorted_by(method: &str) -> SidebarSettings {
        SidebarSettings {
            sort: SidebarSort::parse(method).unwrap(),
            ..SidebarSettings::default()
        }
    }

    #[test]
    fn lists_the_mailboxes_with_their_counts() {
        let (mut app, check) = mailboxes_app(with_mailboxes(SidebarSettings::default()), 80, 24);
        assert_eq!(shown(&app), ["*Inbox", "Work", "Lists"]);
        mailboxes_checked(
            &mut app,
            check,
            &[
                ("Inbox", 1, 5, true),
                ("Work", 2, 7, true),
                ("Lists", 0, 9, false),
            ],
        );
        let entries = &app.sidebar.entries;
        assert!(entries[0].open && !entries[0].new, "not new while open");
        assert_eq!(entries[1].counts, Some((2, 7)));
        assert!(entries[1].new && entries[1].has_mail());
        assert!(!entries[2].has_mail());
    }

    #[test]
    fn sorts_the_mailboxes() {
        let counts = [
            ("Inbox", 1, 5, false),
            ("Work", 2, 7, false),
            ("Lists", 0, 9, false),
        ];
        let order = |method: &str| {
            let (mut app, check) = mailboxes_app(with_mailboxes(sorted_by(method)), 80, 24);
            mailboxes_checked(&mut app, check, &counts);
            shown(&app)
        };
        assert_eq!(order("count"), ["Lists", "Work", "*Inbox"]);
        assert_eq!(order("reverse-count"), ["*Inbox", "Work", "Lists"]);
        assert_eq!(order("unread"), ["Work", "*Inbox", "Lists"]);
        // By label, or path: `Inbox` before `Lists`.
        assert_eq!(order("alpha"), ["*Inbox", "Lists", "Work"]);
        // As shown: `Apple` before a label (`Banana`) after it.
        let mut settings = with_mailboxes(sorted_by("alpha"));
        settings.mailboxes[0].label = Some("Banana".into());
        settings.mailboxes[2].text = "=Apple".into();
        let (app, _) = mailboxes_app(settings, 80, 24);
        assert_eq!(shown(&app), ["Apple", "*Banana", "Work"]);
        // By path: `=@inbox` comes first.
        assert_eq!(order("path"), ["*Inbox", "Lists", "Work"]);
        assert_eq!(order("reverse-path"), ["Work", "Lists", "*Inbox"]);
        // Office365 doesn't count flagged mail: as listed.
        assert_eq!(order("flagged"), ["*Inbox", "Work", "Lists"]);
    }

    #[test]
    fn new_mail_only_shows_mailboxes_with_mail() {
        let mut settings = SidebarSettings {
            new_mail_only: true,
            ..SidebarSettings::default()
        };
        let (mut app, check) = mailboxes_app(with_mailboxes(settings.clone()), 80, 24);
        // Not known yet: only the open folder.
        assert_eq!(shown(&app), ["*Inbox"]);
        let counts = [
            ("Inbox", 0, 5, false),
            ("Work", 0, 7, true),
            ("Lists", 0, 9, false),
        ];
        mailboxes_checked(&mut app, check, &counts);
        assert_eq!(shown(&app), ["*Inbox", "Work"]);
        settings.whitelist = vec![mailbox_path("Lists")];
        let whitelisted = with_mailboxes(settings);
        app.update(configured(whitelisted));
        assert_eq!(shown(&app), ["*Inbox", "Work", "Lists"]);
    }

    #[test]
    fn knows_the_open_folder_by_another_name() {
        let mut settings = with_mailboxes(SidebarSettings {
            new_mail_only: true,
            sort: SidebarSort::parse("reverse-unsorted").unwrap(),
            ..SidebarSettings::default()
        });
        // `=Inbox`, unchecked, is the Inbox opened as `=@inbox`.
        settings.mailboxes[0].path = FolderPath::Named(vec!["Inbox".into()]);
        settings.mailboxes[0].poll = false;
        // Named as Office365 names folders, ignoring case.
        settings.sidebar.whitelist = vec![mailbox_path("WORK"), mailbox_path("lists")];
        let (app, _) = mailboxes_app(settings, 80, 24);
        assert_eq!(shown(&app), ["Lists", "Work", "*Inbox"], "reversed");
        let inbox = &app.sidebar.entries[2];
        assert!(inbox.open);
        assert_eq!(inbox.counts, Some((0, 0)), "the open folder's");
    }

    #[test]
    fn the_cursor_stays_on_its_mailbox() {
        let (mut app, check) = mailboxes_app(with_mailboxes(sorted_by("unread")), 80, 24);
        mailboxes_checked(&mut app, check, &[("Work", 3, 3, false)]);
        assert_eq!(shown(&app), ["Work", "*Inbox", "Lists"]);
        app.sidebar.highlight = 2;
        // Lists gains more unread mail than Work: the cursor goes with it.
        let check = first_request(&app.check_mailboxes());
        mailboxes_checked(&mut app, check, &[("Lists", 5, 5, false)]);
        assert_eq!(shown(&app), ["*Lists", "Work", "Inbox"]);
        // Another order puts it back on the open folder.
        app.update(configured(with_mailboxes(sorted_by("alpha"))));
        assert_eq!(shown(&app), ["*Inbox", "Lists", "Work"]);
    }

    fn mailbox(text: &str, label: Option<&str>) -> MailboxEntry {
        MailboxEntry {
            name: label.unwrap_or(text).into(),
            label: label.map(str::to_owned),
            text: text.into(),
            path: FolderPath::Named(vec![text.into()]),
            notify: true,
            poll: true,
        }
    }

    fn name(text: &str, label: Option<&str>, settings: &SidebarSettings) -> String {
        sidebar_name(&mailbox(text, label), settings)
    }

    #[test]
    fn sorts_by_mutt_s_names() {
        let sort = |text| SidebarSort::parse(text).unwrap();
        assert_eq!(sort("unsorted"), SidebarSort::default());
        assert_eq!(sort("new").method, SortMethod::Unread);
        assert_eq!(sort("name").method, SortMethod::Alpha);
        assert!(sort("reverse-count").reverse);
        assert!(SidebarSort::parse("size").is_err());
    }

    #[test]
    fn names_mailboxes_like_mutt() {
        let mut settings = SidebarSettings::default();
        assert_eq!(name("=fruit.apple", None, &settings), "fruit.apple");
        assert_eq!(
            name("msgraph://a@b.c/Work", None, &settings),
            "msgraph://a@b.c/Work"
        );
        assert_eq!(name("=Work", Some("Job"), &settings), "Job");
        settings.short_path = true;
        assert_eq!(name("=fruit", None, &settings), "fruit");
        assert_eq!(name("=fruit.apple", None, &settings), "apple");
        assert_eq!(name("=a/b/c", None, &settings), "c");
        assert_eq!(name("=a/b/", None, &settings), "b/");
        settings.folder_indent = true;
        settings.indent_string = "..".into();
        assert_eq!(name("=fruit.apple", None, &settings), "..apple");
        assert_eq!(name("=a/b/c", Some("Mine"), &settings), "....Mine");
        // Only below `folder`.
        assert_eq!(name("msgraph://a@b.c/Work", None, &settings), "Work");
        settings.short_path = false;
        assert_eq!(name("=a/b", None, &settings), "..a/b");
        settings.delim_chars = "/".into();
        assert_eq!(name("=fruit.apple", None, &settings), "fruit.apple");
        settings.delim_chars.clear();
        assert_eq!(name("=a/b", None, &settings), "a/b");
    }

    /// An app whose sidebar is shown, on a terminal `height` rows tall,
    /// with Work's and Lists' counts.
    fn shown_app(sidebar: SidebarSettings, height: u16, counts: &[(&str, u32, u32, bool)]) -> App {
        let sidebar = SidebarSettings {
            visible: true,
            ..sidebar
        };
        let (mut app, check) = mailboxes_app(with_mailboxes(sidebar), 120, height);
        mailboxes_checked(&mut app, check, counts);
        app
    }

    #[test]
    fn moves_the_cursor() {
        let mut app = shown_app(SidebarSettings::default(), 24, &[]);
        act(&mut app, Action::SidebarNext);
        assert_eq!(shown(&app), ["Inbox", "*Work", "Lists"]);
        act(&mut app, Action::SidebarNext);
        act(&mut app, Action::SidebarNext);
        assert_eq!(shown(&app), ["Inbox", "Work", "*Lists"], "stops at the end");
        act(&mut app, Action::SidebarPrev);
        assert_eq!(shown(&app), ["Inbox", "*Work", "Lists"]);
        act(&mut app, Action::SidebarFirst);
        assert_eq!(app.sidebar.highlight, 0);
        act(&mut app, Action::SidebarLast);
        assert_eq!(app.sidebar.highlight, 2);
        // The open folder stays the open one.
        assert!(app.sidebar.entries[0].open);
        assert!(app.status.is_empty(), "{}", app.status);
        // Text is read in the pager, which has them too.
        let text = crate::app::TextView::new("notes.txt".into(), vec!["a".into()], 50);
        app.screens.push(crate::app::Screen::Text(text));
        act(&mut app, Action::SidebarFirst);
        assert_eq!(app.sidebar.highlight, 0);
        assert!(matches!(app.screen(), crate::app::Screen::Text(_)));
    }

    #[test]
    fn does_nothing_while_hidden() {
        let (mut app, _) = mailboxes_app(with_mailboxes(SidebarSettings::default()), 80, 24);
        assert!(act(&mut app, Action::SidebarNext).is_empty());
        assert!(act(&mut app, Action::SidebarOpen).is_empty());
        assert_eq!(shown(&app), ["*Inbox", "Work", "Lists"]);
    }

    #[test]
    fn finds_mailboxes_with_new_mail() {
        let counts = [("Work", 0, 3, false), ("Lists", 2, 9, false)];
        let mut app = shown_app(SidebarSettings::default(), 24, &counts);
        act(&mut app, Action::SidebarNextNew);
        assert_eq!(shown(&app), ["Inbox", "Work", "*Lists"]);
        act(&mut app, Action::SidebarNextNew);
        assert_eq!(app.sidebar.highlight, 2, "no wrapping");
        act(&mut app, Action::SidebarPrevNew);
        assert_eq!(app.sidebar.highlight, 2, "none before");
        let wrapping = SidebarSettings {
            next_new_wrap: true,
            ..SidebarSettings::default()
        };
        let mut app = shown_app(wrapping, 24, &[("Work", 0, 3, true)]);
        act(&mut app, Action::SidebarLast);
        act(&mut app, Action::SidebarNextNew);
        assert_eq!(shown(&app), ["Inbox", "*Work", "Lists"], "wrapped");
        act(&mut app, Action::SidebarPrevNew);
        assert_eq!(app.sidebar.highlight, 1, "the only one");
    }

    #[test]
    fn pages_through_the_mailboxes() {
        // Two rows: pages of Inbox and Work, then Lists.
        let mut app = shown_app(SidebarSettings::default(), 5, &[]);
        act(&mut app, Action::SidebarPageDown);
        assert_eq!(app.sidebar.highlight, 2);
        act(&mut app, Action::SidebarPageDown);
        assert_eq!(app.sidebar.highlight, 2, "the last");
        act(&mut app, Action::SidebarPageUp);
        assert_eq!(app.sidebar.highlight, 1, "the previous page's last");
        act(&mut app, Action::SidebarPageUp);
        assert_eq!(app.sidebar.highlight, 0);
    }

    #[test]
    fn opens_the_highlighted_mailbox() {
        let mut app = shown_app(SidebarSettings::default(), 24, &[]);
        act(&mut app, Action::SidebarNext);
        let effects = act(&mut app, Action::SidebarOpen);
        assert!(
            matches!(&requests(&effects)[..], [(_, Request::OpenFolder(path))] if *path == mailbox_path("Work")),
            "{effects:?}"
        );
        let open = first_request(&effects);
        let work = mett_core::mock::folder("work", "Work", None);
        respond(&mut app, open, Event::FolderOpened(Ok(work)));
        assert_eq!(shown(&app), ["Inbox", "*Work", "Lists"]);
        assert!(app.sidebar.entries[1].open);
    }

    #[test]
    fn toggles_the_sidebar_and_sidebar_visible() {
        let mut app = shown_app(SidebarSettings::default(), 24, &[]);
        assert_eq!(app.width, 90);
        let effects = act(&mut app, Action::SidebarToggleVisible);
        assert_eq!(effects, [Effect::Command("toggle sidebar_visible".into())]);
        assert_eq!(app.width, 120, "hidden at once");
        assert!(app.waiting(), "keys wait for the command");
        // The configuration agrees.
        app.update(configured(with_mailboxes(SidebarSettings::default())));
        assert_eq!(app.width, 120);
        assert!(!app.waiting());
        act(&mut app, Action::SidebarToggleVisible);
        assert_eq!(app.width, 90);
    }

    #[test]
    fn hides_on_a_narrow_terminal() {
        let mut app = shown_app(SidebarSettings::default(), 24, &[]);
        let resize = |app: &mut App, width| {
            app.update(crate::app::Msg::Resize { width, height: 24 });
        };
        resize(&mut app, 99);
        assert_eq!(app.width, 99, "narrower than sidebar_min_width");
        act(&mut app, Action::SidebarNext);
        assert_eq!(app.sidebar.highlight, 0);
        assert_eq!(
            app.status,
            "The sidebar shows once the terminal is 100 columns wide."
        );
        resize(&mut app, 100);
        assert_eq!(app.width, 70, "back");
        // Nor does it leave the menus fewer than 40 columns.
        let wide = with_mailboxes(SidebarSettings {
            visible: true,
            width: 70,
            ..SidebarSettings::default()
        });
        app.update(configured(wide));
        assert_eq!(app.width, 100);
        resize(&mut app, 110);
        assert_eq!(app.width, 40);
        // Without sidebar_min_width, as in mutt: a column at least.
        let mutt = with_mailboxes(SidebarSettings {
            visible: true,
            width: 70,
            min_width: 0,
            ..SidebarSettings::default()
        });
        app.update(configured(mutt));
        resize(&mut app, 50);
        assert_eq!(app.width, 1);
    }

    #[test]
    fn ordered_by_counts_follows_the_open_folder_s() {
        let mut settings = with_mailboxes(sorted_by("unread"));
        settings.sidebar.visible = true;
        let unread = (0..5)
            .map(|i| {
                let date = chrono::DateTime::UNIX_EPOCH + chrono::TimeDelta::minutes(i);
                mett_core::mock::message(&format!("m{i}"), "in", "s", date).summary
            })
            .collect();
        let (mut app, check) = mailboxes_listed(settings, 120, 24, unread);
        mailboxes_checked(&mut app, check, &[("Work", 3, 3, false)]);
        app.before_draw();
        assert_eq!(shown(&app), ["*Inbox", "Work", "Lists"]);
        for _ in 0..3 {
            act(&mut app, Action::ToggleNew);
            act(&mut app, Action::NextEntry);
        }
        app.before_draw();
        assert_eq!(shown(&app), ["Work", "*Inbox", "Lists"]);
    }

    /// An app without `mailboxes` and with `sidebar`, `width` columns
    /// wide, that has opened the Inbox; returns it and the effects of the
    /// opening.
    fn folders_app_at(sidebar: SidebarSettings, width: u16) -> (App, Vec<Effect>) {
        let settings = crate::app::Settings {
            sync_interval: Some(std::time::Duration::from_secs(60)),
            sidebar,
            ..crate::app::Settings::default()
        };
        let (mut app, effects) = App::open(width, 24, inbox(), settings);
        let open = first_request(&effects);
        let inbox = mett_core::mock::folder("in", "Inbox", None);
        let effects = respond(&mut app, open, Event::FolderOpened(Ok(inbox)));
        (app, effects)
    }

    /// [`folders_app_at`] 120 columns, with `sidebar` visible.
    fn folders_app(sidebar: SidebarSettings) -> (App, Vec<Effect>) {
        let visible = SidebarSettings {
            visible: true,
            ..sidebar
        };
        folders_app_at(visible, 120)
    }

    /// The request listing the folders among `effects`.
    fn folders_listing(effects: &[Effect]) -> Option<RequestId> {
        let requests = requests(effects);
        let mut listings = requests
            .iter()
            .filter(|(_, request)| *request == Request::ListFolders);
        let listing = listings.next().map(|(id, _)| *id);
        assert!(listings.next().is_none(), "listed once");
        listing
    }

    /// Inbox, with 2 unread of 5, its subfolder Projects and Archive.
    fn every_folder() -> Vec<Folder> {
        let folder = |id: &str, name: &str, parent: Option<&str>, unread| Folder {
            parent: parent.map(mett_core::FolderId::new),
            unread_count: unread,
            total_count: 5,
            ..mett_core::mock::folder(id, name, None)
        };
        vec![
            folder("in", "Inbox", None, 2),
            folder("p", "Projects", Some("in"), 0),
            folder("arc", "Archive", None, 1),
        ]
    }

    /// [`folders_app`], its folders listed.
    fn folders_listed(sidebar: SidebarSettings) -> App {
        let (mut app, effects) = folders_app(sidebar);
        let listing = folders_listing(&effects).expect("folders listed");
        respond(&mut app, listing, Event::FoldersListed(Ok(every_folder())));
        app
    }

    #[test]
    fn lists_every_folder_without_mailboxes() {
        let mut app = folders_listed(SidebarSettings::default());
        assert_eq!(shown(&app), ["Archive", "*Inbox", "Inbox/Projects"]);
        assert!(app.sidebar.entries[1].open);
        assert_eq!(app.sidebar.entries[0].counts, Some((1, 5)));
        assert!(app.sidebar.entries[0].has_mail());
        // Listed once, until they may have changed: the next check.
        assert_eq!(folders_listing(&act(&mut app, Action::SidebarNext)), None);
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        let listing = folders_listing(&effects).expect("listed again");
        let mut fewer = every_folder();
        fewer.pop();
        respond(&mut app, listing, Event::FoldersListed(Ok(fewer)));
        assert_eq!(
            shown(&app),
            ["Inbox", "*Inbox/Projects"],
            "the cursor stays"
        );
        // A failed listing keeps them, says why, and is tried again once
        // the user does something, not by itself.
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        let listing = folders_listing(&effects).expect("listed again");
        respond(&mut app, listing, Event::FoldersListed(Err(offline())));
        assert_eq!(shown(&app), ["Inbox", "*Inbox/Projects"]);
        assert!(!app.status.is_empty(), "the failure is reported");
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        assert_eq!(folders_listing(&effects), None);
        assert!(folders_listing(&act(&mut app, Action::SidebarFirst)).is_some());
    }

    #[test]
    fn lists_the_folders_again_once_the_mailboxes_go() {
        let mut app = folders_listed(SidebarSettings::default());
        let visible = SidebarSettings {
            visible: true,
            ..SidebarSettings::default()
        };
        let mut settings = with_mailboxes(visible);
        let effects = app.update(configured(settings.clone()));
        assert_eq!(folders_listing(&effects), None);
        assert_eq!(shown(&app), ["*Inbox", "Work", "Lists"]);
        settings.mailboxes.clear();
        let effects = app.update(configured(settings));
        assert!(folders_listing(&effects).is_some(), "maybe changed since");
    }

    #[test]
    fn opens_a_listed_folder() {
        let mut app = folders_listed(SidebarSettings::default());
        act(&mut app, Action::SidebarLast);
        let effects = act(&mut app, Action::SidebarOpen);
        let projects = FolderPath::Named(vec!["Inbox".into(), "Projects".into()]);
        assert!(
            matches!(&requests(&effects)[..], [(_, Request::OpenFolder(path))] if *path == projects),
            "{effects:?}"
        );
    }

    #[test]
    fn shortens_and_indents_listed_folders() {
        let app = folders_listed(SidebarSettings {
            short_path: true,
            folder_indent: true,
            ..SidebarSettings::default()
        });
        assert_eq!(shown(&app), ["Archive", "*Inbox", "  Projects"]);
    }

    #[test]
    fn lists_folders_only_when_shown_without_mailboxes() {
        let (_, effects) = folders_app_at(SidebarSettings::default(), 120);
        assert_eq!(folders_listing(&effects), None, "hidden");
        let visible = SidebarSettings {
            visible: true,
            ..SidebarSettings::default()
        };
        let (mut app, effects) = folders_app_at(visible, 80);
        assert_eq!(folders_listing(&effects), None, "too narrow");
        let effects = app.update(Msg::Resize {
            width: 120,
            height: 24,
        });
        assert!(folders_listing(&effects).is_some(), "widened");
        // The mailboxes are shown instead, if there are any.
        let visible = SidebarSettings {
            visible: true,
            ..SidebarSettings::default()
        };
        let (mut app, _) = mailboxes_app(with_mailboxes(visible), 120, 24);
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        assert_eq!(folders_listing(&effects), None);
    }

    #[test]
    fn tab_gives_the_sidebar_the_keys() {
        let mut app = shown_app(SidebarSettings::default(), 24, &[]);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.menu(), Menu::Sidebar);
        assert!(app.sidebar_focused());
        // The index's keys move the sidebar's cursor instead.
        let selected = app.selected;
        key(&mut app, KeyCode::Char('j'));
        key(&mut app, KeyCode::Down);
        assert_eq!(shown(&app), ["Inbox", "Work", "*Lists"]);
        key(&mut app, KeyCode::Char('k'));
        assert_eq!(shown(&app), ["Inbox", "*Work", "Lists"]);
        assert_eq!(app.selected, selected);
        // Tab, or Esc, gives the keys back.
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.menu(), Menu::Index);
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.menu(), Menu::Index);
        // Enter opens the folder, and gives the keys back.
        key(&mut app, KeyCode::Tab);
        let effects = key(&mut app, KeyCode::Enter);
        assert!(
            matches!(&requests(&effects)[..], [(_, Request::OpenFolder(path))] if *path == mailbox_path("Work")),
            "{effects:?}"
        );
        assert_eq!(app.menu(), Menu::Index);
    }

    #[test]
    fn the_pager_gives_the_sidebar_the_keys_too() {
        let mut app = shown_app(SidebarSettings::default(), 24, &[]);
        let text = crate::app::TextView::new("notes.txt".into(), vec!["a".into()], 50);
        app.screens.push(crate::app::Screen::Text(text));
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.menu(), Menu::Sidebar);
        key(&mut app, KeyCode::Char('j'));
        assert_eq!(shown(&app), ["Inbox", "*Work", "Lists"]);
        key(&mut app, KeyCode::Char('q'));
        assert_eq!(app.menu(), Menu::Pager);
        assert!(matches!(app.screen(), crate::app::Screen::Text(_)));
    }

    #[test]
    fn a_hidden_sidebar_does_not_take_the_keys() {
        let (mut app, _) = mailboxes_app(with_mailboxes(SidebarSettings::default()), 120, 24);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.menu(), Menu::Index);
        assert!(app.status.contains("sidebar_visible"), "{}", app.status);
        // Hidden while it had them, it gives them back.
        let mut app = shown_app(SidebarSettings::default(), 24, &[]);
        key(&mut app, KeyCode::Tab);
        act(&mut app, Action::SidebarToggleVisible);
        assert_eq!(app.menu(), Menu::Index);
        // Hidden by a narrower terminal, it gives them back too, and
        // hasn't them once shown again.
        let mut app = shown_app(SidebarSettings::default(), 24, &[]);
        key(&mut app, KeyCode::Tab);
        for width in [80, 120] {
            app.update(Msg::Resize { width, height: 24 });
            assert_eq!(app.menu(), Menu::Index);
        }
        // Too narrow, it says how wide the terminal has to be.
        let mut app = shown_app(SidebarSettings::default(), 24, &[]);
        app.update(Msg::Resize {
            width: 80,
            height: 24,
        });
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.menu(), Menu::Index);
        assert!(app.status.contains("columns wide"), "{}", app.status);
    }

    #[test]
    fn lists_the_folders_again_if_they_changed_while_listed() {
        let (mut app, effects) = folders_app(SidebarSettings::default());
        let listing = folders_listing(&effects).expect("folders listed");
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        assert_eq!(folders_listing(&effects), None, "one at a time");
        let effects = respond(&mut app, listing, Event::FoldersListed(Ok(every_folder())));
        assert!(folders_listing(&effects).is_some(), "maybe outdated");
    }

    #[test]
    fn lists_the_folders_if_the_folder_fails_to_open() {
        let settings = crate::app::Settings {
            sidebar: SidebarSettings {
                visible: true,
                ..SidebarSettings::default()
            },
            ..crate::app::Settings::default()
        };
        let (mut app, effects) = App::open(120, 24, inbox(), settings);
        let open = first_request(&effects);
        let gone = mett_core::Error::not_found("folder", "Inbox");
        let effects = respond(&mut app, open, Event::FolderOpened(Err(gone)));
        assert!(folders_listing(&effects).is_some(), "to choose another");
    }

    #[test]
    fn well_known_paths_name_listed_folders() {
        let (mut app, effects) = folders_app(SidebarSettings {
            new_mail_only: true,
            whitelist: vec![FolderPath::WellKnown(
                mett_core::WellKnownFolder::SentItems,
                vec!["Old".into()],
            )],
            ..SidebarSettings::default()
        });
        let listing = folders_listing(&effects).expect("folders listed");
        let sent = Folder {
            well_known: Some(mett_core::WellKnownFolder::SentItems),
            ..mett_core::mock::folder("sent", "Sent Items", None)
        };
        let old = Folder {
            parent: Some(mett_core::FolderId::new("sent")),
            ..mett_core::mock::folder("old", "Old", None)
        };
        let quiet = mett_core::mock::folder("q", "Quiet", None);
        let folders = vec![
            mett_core::mock::folder("in", "Inbox", None),
            sent,
            old,
            quiet,
        ];
        respond(&mut app, listing, Event::FoldersListed(Ok(folders)));
        assert_eq!(shown(&app), ["*Inbox", "Sent Items/Old"]);
    }

    #[test]
    fn imap_folders_are_told_apart_by_case_except_the_inbox() {
        let named = |name: &str| FolderPath::Named(vec![name.into()]);
        assert!(same_path(&named("Foo"), &named("foo"), Api::Graph));
        assert!(!same_path(&named("Foo"), &named("foo"), Api::Imap));
        assert!(same_path(&named("Foo"), &named("Foo"), Api::Imap));
        assert!(same_path(&named("INBOX"), &named("Inbox"), Api::Imap));
        // Only the top-level one: `INBOX/Inbox` and `INBOX/INBOX` are two.
        let below = |name: &str| FolderPath::Named(vec!["INBOX".into(), name.into()]);
        assert!(!same_path(&below("Inbox"), &below("INBOX"), Api::Imap));
        assert!(same_path(&below("Inbox"), &below("Inbox"), Api::Imap));
    }
}
