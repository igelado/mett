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
                        // A top-level name is only the open folder if that is.
                        open.well_known.is_some()
                            && open.parent.is_none()
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
mod tests;
