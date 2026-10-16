//! mutt's sidebar: the `mailboxes` in a column beside the menus, with
//! their counts, and a cursor of its own that `sidebar-open` opens.
//!
//! The entries (which mailboxes show, in what order, under what name) are
//! made again only when what they show changes: the mailboxes are checked,
//! the settings change or another folder opens. Drawing them only formats
//! the rows on screen (see `view/sidebar.rs`).

use super::{App, MailboxEntry};
use crate::config::mailbox::FolderPath;

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
}

impl App {
    /// Takes in the sidebar's settings. A new order puts the cursor back
    /// on the open folder, as in mutt.
    pub(super) fn apply_sidebar(&mut self, settings: SidebarSettings) {
        let resorted = settings.sort != self.sidebar.settings.sort;
        self.sidebar.settings = settings;
        self.rebuild_sidebar(resorted);
    }

    /// Makes the sidebar's entries again from the mailboxes and what is
    /// known of them. The cursor stays on its mailbox if it is still
    /// shown, or with `follow_open` (another folder opened), moves to the
    /// open one.
    pub(super) fn rebuild_sidebar(&mut self, follow_open: bool) {
        let settings = &self.sidebar.settings;
        let mut entries: Vec<(SidebarEntry, String, u32)> = self
            .mailboxes
            .iter()
            .filter_map(|mailbox| {
                let folder = self
                    .mailbox_state
                    .get(&mailbox.path)
                    .and_then(|state| state.folder.as_ref());
                let entry = SidebarEntry {
                    path: mailbox.path.clone(),
                    name: sidebar_name(mailbox, settings),
                    open: self.is_open(&mailbox.path, folder),
                    new: self.has_new_mail(mailbox),
                    counts: folder.map(|f| (f.unread_count, f.total_count)),
                    spoolfile: settings.spoolfile.as_ref() == Some(&mailbox.path),
                };
                // Office365 doesn't count flagged mail: with
                // `sidebar_new_mail_only`, only new and unread mail shows
                // a mailbox (besides the open one and the whitelisted).
                let shown = !settings.new_mail_only
                    || entry.open
                    || entry.has_mail()
                    || settings.whitelist.contains(&mailbox.path);
                if !shown {
                    return None;
                }
                let expanded =
                    crate::config::mailbox::expand_shortcut(&mailbox.text, &self.mailbox_root);
                let key = match settings.sort.method {
                    SortMethod::Alpha => mailbox.label.clone().unwrap_or(expanded),
                    _ => expanded,
                }
                .to_lowercase();
                let count = match (settings.sort.method, entry.counts) {
                    (SortMethod::Count, Some((_, total))) => total,
                    (SortMethod::Unread, Some((unread, _))) => unread,
                    _ => 0,
                };
                Some((entry, key, count))
            })
            .collect();
        let sort = settings.sort;
        if sort.method != SortMethod::Unsorted {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testing::*;

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
}
