//! What the expandos of mutt's `index_format`, `pager_format` and
//! `status_format` stand for (`docs/MUTTRC.md` §3).

use mett_core::{Address, MessageSummary};
use regex::Regex;

use crate::color::Colors;
use crate::format::{Expandos, date};

/// How the screen is drawn: the formats and colors.
#[derive(Clone, Debug)]
pub struct Display {
    /// `index_format`.
    pub index_format: String,
    /// `pager_format`.
    pub pager_format: String,
    /// `status_format`.
    pub status_format: String,
    /// `date_format`, for `%d` and `%D`.
    pub date_format: String,
    /// `quote_regex`: what quoted lines start with.
    pub quote: Regex,
    /// `color` and `mono`.
    pub colors: Colors,
}

impl PartialEq for Display {
    fn eq(&self, other: &Self) -> bool {
        self.index_format == other.index_format
            && self.pager_format == other.pager_format
            && self.status_format == other.status_format
            && self.date_format == other.date_format
            && self.quote.as_str() == other.quote.as_str()
            && self.colors == other.colors
    }
}

impl Eq for Display {}

impl Default for Display {
    /// The variables' defaults.
    fn default() -> Self {
        let var = |name| {
            crate::config::VARS
                .iter()
                .find(|v| v.name == name)
                .map_or("", |v| v.default)
                .to_owned()
        };
        Self {
            index_format: var("index_format"),
            pager_format: var("pager_format"),
            status_format: var("status_format"),
            date_format: var("date_format"),
            quote: Regex::new(&var("quote_regex")).expect("the default is valid"),
            colors: Colors::default(),
        }
    }
}

/// An index entry, for `index_format` (and `pager_format`).
pub struct Entry<'a> {
    /// Its number in the index, from 1.
    pub number: usize,
    /// The message.
    pub summary: &'a MessageSummary,
    /// Whether it is marked for deletion.
    pub deleted: bool,
    /// Whether the user sent it.
    pub from_me: bool,
    /// How many messages the folder has.
    pub total: usize,
    /// `date_format`.
    pub date_format: &'a str,
    /// For the pager, where it is in the message (`%P`).
    pub position: Option<&'a str>,
    /// Where it is in its thread, if the index shows threads.
    pub thread: Option<&'a crate::app::Threaded>,
}

/// The name of `address`, or the address itself without one.
pub fn display_name(address: &Address) -> &str {
    address
        .name
        .as_deref()
        .filter(|n| !n.is_empty())
        .unwrap_or(&address.email)
}

impl Expandos for Entry<'_> {
    fn expando(&self, code: char, arg: Option<&str>) -> String {
        let summary = self.summary;
        let author = summary.from.as_ref();
        let flags = summary.flags;
        match code {
            'a' => author.map(|a| a.email.clone()).unwrap_or_default(),
            'C' => self.number.to_string(),
            'D' | 'd' => date(summary.received, self.date_format),
            '{' | '[' | '(' => date(summary.received, arg.unwrap_or("")),
            // The sender, or for the user's own mail, who it went to.
            'F' | 'L' => match summary.to.first() {
                Some(to) if self.from_me => format!("To {}", display_name(to)),
                _ => author.map(display_name).unwrap_or_default().to_owned(),
            },
            'f' => author.map(ToString::to_string).unwrap_or_default(),
            'm' => self.total.to_string(),
            'n' => author.map(display_name).unwrap_or_default().to_owned(),
            'P' => self.position.unwrap_or_default().to_owned(),
            // The thread's lines, then the subject, unless it is its
            // parent's.
            's' => match self.thread {
                Some(thread) if thread.same_subject => thread.tree.clone(),
                Some(thread) => format!("{}{}", thread.tree, summary.subject),
                None => summary.subject.clone(),
            },
            // How many messages a collapsed thread folds away.
            'M' => self
                .thread
                .filter(|t| t.folded > 0)
                .map(|t| t.folded.to_string())
                .unwrap_or_default(),
            'S' => if self.deleted {
                "D"
            } else if flags.flagged {
                "!"
            } else if !flags.read {
                "N"
            } else {
                " "
            }
            .to_owned(),
            't' => summary
                .to
                .first()
                .map(|to| to.email.clone())
                .unwrap_or_default(),
            'u' => author
                .and_then(|a| a.email.split('@').next())
                .unwrap_or_default()
                .to_owned(),
            'v' => author
                .map(|a| match a.name.as_deref().filter(|n| !n.is_empty()) {
                    Some(name) => name.split_whitespace().next().unwrap_or(name).to_owned(),
                    None => a.email.split('@').next().unwrap_or_default().to_owned(),
                })
                .unwrap_or_default(),
            // The attachments, as far as the index knows: some or none.
            'X' => u8::from(summary.has_attachments).to_string(),
            'Z' => {
                let status = if self.deleted {
                    'D'
                } else if flags.read {
                    ' '
                } else {
                    'N'
                };
                let flagged = if flags.flagged { '!' } else { ' ' };
                let attachments = if summary.has_attachments { '+' } else { ' ' };
                format!("{status}{flagged}{attachments}")
            }
            _ => String::new(),
        }
    }
}

/// The open folder, for `status_format`.
pub struct Status<'a> {
    /// `%f`: its name.
    pub folder: &'a str,
    /// `%m`: how many messages it has.
    pub total: usize,
    /// `%M`: how many the limit shows, if there is one.
    pub shown: Option<usize>,
    /// `%n`: how many are unread.
    pub new: usize,
    /// `%d`: how many are marked for deletion.
    pub deleted: usize,
    /// `%F`: how many are flagged.
    pub flagged: usize,
    /// `%b`: how many mailboxes have new mail.
    pub incoming: usize,
    /// `%V`: the limit's pattern.
    pub limit: Option<&'a str>,
    /// `%I` (mett's): whether part of the folder couldn't be listed.
    pub incomplete: bool,
    /// `%r`: whether changes are waiting to be made.
    pub changed: bool,
    /// `%P`: where the index is in the folder.
    pub position: &'a str,
    /// `%s` and `%S`: `sort` and `sort_aux`.
    pub sort: (&'a str, &'a str),
}

impl Expandos for Status<'_> {
    /// As in mutt, `%?M?` says whether there is a limit, even one that
    /// shows nothing.
    fn is_set(&self, code: char) -> bool {
        match code {
            'M' => self.shown.is_some(),
            // Any limit, even one written `0`.
            'V' => self.limit.is_some(),
            // `%r` shows `-` when nothing changed.
            'r' => self.changed,
            code => crate::format::truthy(&self.expando(code, None)),
        }
    }

    fn expando(&self, code: char, _arg: Option<&str>) -> String {
        // A count of none shows as 0 (which conditionals take as unset).
        let count = |n: usize| n.to_string();
        match code {
            'b' => count(self.incoming),
            'd' => count(self.deleted),
            'F' => count(self.flagged),
            'f' => self.folder.to_owned(),
            'h' => hostname(),
            'I' => if self.incomplete { "1" } else { "" }.to_owned(),
            'M' => self.shown.map(|n| n.to_string()).unwrap_or_default(),
            'm' => self.total.to_string(),
            'n' | 'u' => count(self.new),
            'P' => self.position.to_owned(),
            'r' => if self.changed { "*" } else { "-" }.to_owned(),
            's' => self.sort.0.to_owned(),
            'S' => self.sort.1.to_owned(),
            'V' => self.limit.unwrap_or_default().to_owned(),
            'v' => format!("mett {}", env!("CARGO_PKG_VERSION")),
            _ => String::new(),
        }
    }
}

/// The computer's name, as `%h` shows it: without its domain.
fn hostname() -> String {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        let name = ["/proc/sys/kernel/hostname", "/etc/hostname"]
            .iter()
            .find_map(|file| std::fs::read_to_string(file).ok())
            .or_else(|| std::env::var("HOSTNAME").ok())
            .or_else(|| std::env::var("COMPUTERNAME").ok())
            // Elsewhere (macOS, the BSDs), the system says.
            .or_else(|| {
                std::process::Command::new("hostname")
                    .stderr(std::process::Stdio::null())
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            })
            .unwrap_or_default();
        let name = name.trim();
        name.split('.').next().unwrap_or(name).to_owned()
    })
    .clone()
}

/// Where a view of `len` rows, `height` of them shown from `top`, is, as
/// mutt's `%P` says: `all`, `end`, or how far down, in percent.
pub fn position(top: usize, height: usize, len: usize) -> String {
    if top + height >= len {
        if top == 0 { "all" } else { "end" }.to_owned()
    } else {
        format!("{}%", (top + height) * 100 / len.max(1))
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use mett_core::mock;

    use super::*;
    use crate::format::expand;

    #[test]
    fn index_expandos() {
        let mut summary = mock::message("m1", "in", "Lunch?", DateTime::UNIX_EPOCH).summary;
        summary.from = Some(Address::new("alice@contoso.com").with_name("Alice Example"));
        summary.to = vec![Address::new("bob@x.org").with_name("Bob")];
        summary.flags.flagged = true;
        let mut entry = Entry {
            number: 3,
            summary: &summary,
            deleted: false,
            from_me: false,
            total: 10,
            date_format: "%Y",
            position: None,
            thread: None,
        };
        let line = expand("%3C %Z %-8.8F|%a|%u|%v|%n|%S|%t|%m", &entry, 80);
        assert_eq!(
            line,
            "  3 N!  Alice Ex|alice@contoso.com|alice|Alice|Alice Example|!|bob@x.org|10"
        );
        entry.from_me = true;
        assert_eq!(expand("%F", &entry, 80), "To Bob");
    }

    #[test]
    fn status_expandos_leave_out_what_is_nothing() {
        let status = Status {
            folder: "Inbox",
            total: 5,
            shown: None,
            new: 0,
            deleted: 1,
            flagged: 0,
            incoming: 0,
            limit: None,
            incomplete: false,
            changed: true,
            position: "all",
            sort: ("date", "date"),
        };
        let line = expand(&Display::default().status_format, &status, 50);
        assert_eq!(
            line,
            format!("-*-mett: Inbox [Msgs:5 Del:1]{}(all)---", "-".repeat(13))
        );
        // Nothing changed: `%?r?` takes its else.
        let clean = Status {
            changed: false,
            ..status
        };
        assert_eq!(expand("%?r?changed&clean?", &clean, 20), "clean");
        let zero = Status {
            limit: Some("0"),
            ..status
        };
        assert_eq!(expand("%?V?[Limit: %V]?", &zero, 20), "[Limit: 0]");
        // Shown directly, a count of none is 0.
        assert_eq!(expand("New:%n Del:%d", &status, 20), "New:0 Del:1");
    }

    #[test]
    fn an_empty_limit_still_shows_its_count() {
        let status = Status {
            folder: "Inbox",
            total: 5,
            shown: Some(0),
            new: 0,
            deleted: 0,
            flagged: 0,
            incoming: 0,
            limit: Some("~F"),
            incomplete: false,
            changed: false,
            position: "all",
            sort: ("date", "date"),
        };
        let line = expand("%?M?%M/?%m%?V? [Limit: %V]?", &status, 50);
        assert_eq!(line, "0/5 [Limit: ~F]");
    }

    #[test]
    fn positions_like_mutt() {
        assert_eq!(position(0, 10, 5), "all");
        assert_eq!(position(5, 10, 12), "end");
        assert_eq!(position(0, 10, 40), "25%");
    }
}
