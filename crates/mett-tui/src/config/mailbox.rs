//! Mailbox paths: `msgraph://` and `imap://` URLs and mutt's shortcuts
//! (`docs/CONFIG-OFFICE365.md` §3, `docs/MUTTRC.md` §5).

use std::fmt;

use mett_core::{AccountId, WellKnownFolder};

use super::server::{self, ServerUrl};

/// A folder in a mailbox, from a `msgraph://<account>/<path>` URL
/// (Office365) or an `imap://[user@]<host>/<path>` one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxUrl {
    /// The account: the user principal name of an Office365 mailbox, or
    /// the user at the server of an IMAP one.
    pub account: AccountId,
    /// The folder below the mailbox root.
    pub folder: FolderPath,
    /// Where the mailbox is.
    pub server: Server,
}

/// Where a mailbox is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Server {
    /// Office365, through Microsoft Graph or Exchange Web Services.
    Office365,
    /// An IMAP server.
    Imap(ServerUrl),
}

/// Where a folder is in the mailbox.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FolderPath {
    /// The mailbox root (`msgraph://<account>/`).
    Root,
    /// A well-known folder (`@inbox`), possibly followed by display names
    /// of folders below it.
    WellKnown(WellKnownFolder, Vec<String>),
    /// Display names from the root (`Inbox/Projects`).
    Named(Vec<String>),
}

/// Whether two folder names are the same, ignoring case as Office365
/// does.
pub fn same_name(a: &str, b: &str) -> bool {
    a == b || a.to_lowercase() == b.to_lowercase()
}

/// Why a mailbox path is not usable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxError(pub String);

impl fmt::Display for MailboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

const SCHEME: &str = "msgraph://";

/// The schemes of IMAP mailboxes.
const IMAP: [&str; 2] = ["imap://", "imaps://"];

/// Whether `text` starts like a mailbox URL.
fn is_url(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.starts_with(SCHEME) || IMAP.iter().any(|s| lower.starts_with(s))
}

/// The account of the user at an IMAP server.
fn imap_account(host: &str, user: &str) -> AccountId {
    if user.contains('@') {
        AccountId::new(user)
    } else {
        AccountId::new(format!("{user}@{host}"))
    }
}

impl MailboxUrl {
    /// Parses a `msgraph://` or `imap://` URL.
    pub fn parse(url: &str) -> Result<Self, MailboxError> {
        if IMAP.iter().any(|s| url.to_ascii_lowercase().starts_with(s)) {
            return Self::parse_imap(url);
        }
        let rest = url
            .strip_prefix(SCHEME)
            .ok_or_else(|| MailboxError(format!("{url}: not a msgraph:// or imap:// mailbox")))?;
        let (account, path) = rest.split_once('/').unwrap_or((rest, ""));
        let valid_upn = account.split_once('@').is_some_and(|(user, domain)| {
            !user.is_empty() && !domain.is_empty() && !domain.contains('@')
        });
        if !valid_upn {
            return Err(MailboxError(format!(
                "{url}: expected msgraph://<user@domain>/<folder>"
            )));
        }
        let segments: Vec<String> = path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        Ok(Self {
            account: AccountId::new(account),
            folder: folder_path_of(url, &segments)?,
            server: Server::Office365,
        })
    }

    fn parse_imap(url: &str) -> Result<Self, MailboxError> {
        let server = ServerUrl::parse(url, "imap", "imaps").map_err(MailboxError)?;
        let segments = server
            .path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(server::decode)
            .collect::<Result<Vec<_>, _>>()
            .map_err(MailboxError)?;
        let account = match &server.user {
            Some(user) => imap_account(&server.host, user),
            // Until `imap_user` says (see `with_user`).
            None => AccountId::new(&server.host),
        };
        Ok(Self {
            account,
            folder: folder_path_of(url, &segments)?,
            server: Server::Imap(server),
        })
    }

    /// An IMAP mailbox whose URL names no user, as `user`'s (the
    /// `imap_user`).
    #[must_use]
    pub fn with_user(mut self, user: &str) -> Self {
        if let Server::Imap(server) = &mut self.server
            && server.user.is_none()
            && !user.is_empty()
        {
            self.account = imap_account(&server.host, user);
            server.user = Some(user.to_owned());
        }
        self
    }

    /// Whether the mailbox is `account`'s, at the server of `base` (the
    /// URL of the account's mailboxes, `folder`), if that is a mailbox URL:
    /// the same kind of server and, for IMAP, the same host and port. An
    /// IMAP URL that names no user is of any user at its server.
    pub fn is_of(&self, account: &AccountId, base: &str) -> bool {
        let base = MailboxUrl::parse(base).ok();
        if let Some(base) = &base
            && !self.same_server(base)
        {
            return false;
        }
        if self.account.as_str().eq_ignore_ascii_case(account.as_str()) {
            return true;
        }
        // An IMAP URL that names no user is of whoever logs in at its
        // server: the base says it is the server (without one, any is).
        matches!(&self.server, Server::Imap(server) if server.user.is_none())
            && base.is_some_and(|base| matches!(base.server, Server::Imap(_)))
    }

    /// Whether both mailboxes are on the same server.
    fn same_server(&self, other: &Self) -> bool {
        match (&self.server, &other.server) {
            (Server::Office365, Server::Office365) => true,
            (Server::Imap(a), Server::Imap(b)) => {
                let port = |s: &ServerUrl| s.port.unwrap_or(if s.tls { 993 } else { 143 });
                a.host.eq_ignore_ascii_case(&b.host) && port(a) == port(b)
            }
            _ => false,
        }
    }
}

/// The folder that the segments of the path in `url` name.
fn folder_path_of(url: &str, segments: &[String]) -> Result<FolderPath, MailboxError> {
    Ok(match segments.split_first() {
        None => FolderPath::Root,
        Some((first, rest)) => match first.strip_prefix('@') {
            Some(name) => {
                let which = name.parse::<WellKnownFolder>().map_err(|_| {
                    MailboxError(format!("{url}: unknown well-known folder @{name}"))
                })?;
                // `@inbox/@focused` and `@inbox/@other` are the Focused
                // Inbox views; any other name is a real folder.
                if which == WellKnownFolder::Inbox
                    && let Some(view @ ("@focused" | "@other")) = rest.first().map(String::as_str)
                {
                    return Err(MailboxError(format!("{url}: {view} is not supported yet")));
                }
                FolderPath::WellKnown(which, rest.to_vec())
            }
            None => FolderPath::Named(segments.to_vec()),
        },
    })
}

impl fmt::Display for MailboxUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.server {
            Server::Office365 => write!(f, "{SCHEME}{}/", self.account)?,
            Server::Imap(server) => {
                f.write_str(if server.tls { "imaps://" } else { "imap://" })?;
                if let Some(user) = &server.user {
                    write!(f, "{}@", server::encode(user))?;
                }
                f.write_str(&server.host)?;
                if let Some(port) = server.port {
                    write!(f, ":{port}")?;
                }
                f.write_str("/")?;
            }
        }
        let encoded = |name: &str| match self.server {
            Server::Office365 => name.to_owned(),
            Server::Imap(_) => name
                .bytes()
                .map(|b| match b {
                    b'%' | b'/' | b' ' | b'#' | b'?' => format!("%{b:02X}"),
                    _ => (b as char).to_string(),
                })
                .collect::<String>(),
        };
        match &self.folder {
            FolderPath::Root => Ok(()),
            FolderPath::WellKnown(which, rest) => {
                write!(f, "@{which}")?;
                rest.iter().try_for_each(|s| write!(f, "/{}", encoded(s)))
            }
            FolderPath::Named(names) => f.write_str(
                &names
                    .iter()
                    .map(|n| encoded(n))
                    .collect::<Vec<_>>()
                    .join("/"),
            ),
        }
    }
}

/// The folder of `account` that the user typed as `text`: a `msgraph://`
/// URL for that account, mutt's `=` or `+` shortcut (relative to
/// `folder`), or a path below the mailbox root (`Inbox/Projects` or
/// `@archive`).
/// The folder `text` names, with mutt's `!`, `>` and `<` shortcuts for
/// `$spoolfile`, `$mbox` and `$record` (given as set, possibly empty).
pub fn configured_path(
    text: &str,
    folder: &str,
    account: &AccountId,
    spoolfile: &str,
    mbox: &str,
    record: &str,
) -> Result<FolderPath, MailboxError> {
    let (name, set) = match text.trim() {
        "!" => ("spoolfile", spoolfile),
        ">" => ("mbox", mbox),
        "<" => ("record", record),
        _ => return folder_path(text, folder, account),
    };
    if set.is_empty() {
        return Err(MailboxError(format!("{name} is not set")));
    }
    folder_path(set, folder, account).map_err(|e| MailboxError(format!("{name}: {e}")))
}

pub fn folder_path(
    text: &str,
    folder: &str,
    account: &AccountId,
) -> Result<FolderPath, MailboxError> {
    let expanded = expand_shortcut(text.trim(), folder);
    let url = if is_url(&expanded) {
        let url = MailboxUrl::parse(&expanded)?;
        if !url.is_of(account, folder) {
            return Err(MailboxError(format!(
                "{expanded}: mett can only use folders of {account}"
            )));
        }
        url
    } else {
        MailboxUrl::parse(&format!("{SCHEME}{account}/{expanded}"))?
    };
    match url.folder {
        FolderPath::Root => Err(MailboxError(format!(
            "{text}: the mailbox itself isn't a folder"
        ))),
        folder => Ok(folder),
    }
}

/// Expands mutt's mailbox shortcuts in `path`: a leading `=` or `+` is
/// relative to `folder`. Other shortcuts (`!`, `<`, `>`, `-`, `^`) are
/// resolved by the caller, which knows the other variables.
pub fn expand_shortcut(path: &str, folder: &str) -> String {
    match path.strip_prefix(['=', '+']) {
        Some(rest) => {
            let separator = if folder.ends_with('/') || folder.is_empty() {
                ""
            } else {
                "/"
            };
            format!("{folder}{separator}{rest}")
        }
        None => path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_known_and_named_folders() {
        let url = MailboxUrl::parse("msgraph://alice@contoso.com/@inbox").unwrap();
        assert_eq!(url.account, AccountId::new("alice@contoso.com"));
        assert_eq!(
            url.folder,
            FolderPath::WellKnown(WellKnownFolder::Inbox, vec![])
        );

        let url = MailboxUrl::parse("msgraph://alice@contoso.com/@inbox/Projects/mett").unwrap();
        assert_eq!(
            url.folder,
            FolderPath::WellKnown(
                WellKnownFolder::Inbox,
                vec!["Projects".into(), "mett".into()]
            )
        );

        let url = MailboxUrl::parse("msgraph://alice@contoso.com/Inbox/Projects").unwrap();
        assert_eq!(
            url.folder,
            FolderPath::Named(vec!["Inbox".into(), "Projects".into()])
        );
        assert_eq!(
            MailboxUrl::parse("msgraph://alice@contoso.com")
                .unwrap()
                .folder,
            FolderPath::Root
        );
    }

    #[test]
    fn rejects_bad_urls() {
        assert!(MailboxUrl::parse("pop3://mail.example.com/").is_err());
        assert!(MailboxUrl::parse("msgraph:///@inbox").is_err());
        for bad in [
            "msgraph://@/@inbox",
            "msgraph://alice@/",
            "msgraph://@contoso.com/",
            "msgraph://a@b@c/",
        ] {
            assert!(MailboxUrl::parse(bad).is_err(), "{bad}");
        }
        assert!(MailboxUrl::parse("msgraph://alice@contoso.com/@spam").is_err());
        let focused = MailboxUrl::parse("msgraph://alice@contoso.com/@inbox/@focused");
        assert!(matches!(focused, Err(MailboxError(m)) if m.contains("not supported yet")));
        // Other @-names below a folder are ordinary folder names.
        let url = MailboxUrl::parse("msgraph://alice@contoso.com/@inbox/@Projects").unwrap();
        assert_eq!(
            url.folder,
            FolderPath::WellKnown(WellKnownFolder::Inbox, vec!["@Projects".into()])
        );
    }

    #[test]
    fn round_trips_through_display() {
        for url in [
            "msgraph://alice@contoso.com/",
            "msgraph://alice@contoso.com/@sentitems",
            "msgraph://alice@contoso.com/@inbox/Projects",
            "msgraph://alice@contoso.com/Inbox/Projects",
        ] {
            assert_eq!(MailboxUrl::parse(url).unwrap().to_string(), url);
        }
    }

    #[test]
    fn finds_the_folder_the_user_typed() {
        let account = AccountId::new("alice@contoso.com");
        let folder = "msgraph://alice@contoso.com/";
        let path = |text| folder_path(text, folder, &account);
        assert_eq!(
            path("=@archive"),
            Ok(FolderPath::WellKnown(WellKnownFolder::Archive, vec![]))
        );
        assert_eq!(
            path("Inbox/Projects"),
            Ok(FolderPath::Named(vec!["Inbox".into(), "Projects".into()]))
        );
        assert_eq!(
            path("msgraph://ALICE@contoso.com/@inbox"),
            Ok(FolderPath::WellKnown(WellKnownFolder::Inbox, vec![]))
        );
        assert!(path("msgraph://bob@contoso.com/@inbox").is_err());
        assert!(path("=").is_err());
        assert!(path("@nonsense").is_err());
    }

    #[test]
    fn expands_folder_shortcuts() {
        let folder = "msgraph://alice@contoso.com/";
        assert_eq!(
            expand_shortcut("=@inbox", folder),
            "msgraph://alice@contoso.com/@inbox"
        );
        assert_eq!(
            expand_shortcut("+Projects", "msgraph://alice@contoso.com"),
            "msgraph://alice@contoso.com/Projects"
        );
        assert_eq!(expand_shortcut("/var/mail/a", folder), "/var/mail/a");
    }

    #[test]
    fn configured_paths_take_mutts_shortcuts() {
        let account = AccountId::new("alice@contoso.com");
        let folder = "msgraph://alice@contoso.com/";
        let path = |text: &str| {
            configured_path(
                text,
                folder,
                &account,
                "msgraph://alice@contoso.com/@inbox",
                "",
                "=@sentitems",
            )
        };
        assert_eq!(
            path("!"),
            Ok(FolderPath::WellKnown(WellKnownFolder::Inbox, vec![]))
        );
        assert_eq!(
            path("<"),
            Ok(FolderPath::WellKnown(WellKnownFolder::SentItems, vec![]))
        );
        assert_eq!(path(">"), Err(MailboxError("mbox is not set".into())));
        assert_eq!(
            path("=Archive"),
            Ok(FolderPath::Named(vec!["Archive".into()]))
        );
    }

    #[test]
    fn parses_imap_mailboxes() {
        let url = MailboxUrl::parse("imaps://me@mail.example.com:1993/INBOX/Work").unwrap();
        assert_eq!(url.account, AccountId::new("me@mail.example.com"));
        assert_eq!(
            url.folder,
            FolderPath::Named(vec!["INBOX".into(), "Work".into()])
        );
        let Server::Imap(server) = &url.server else {
            panic!("not IMAP")
        };
        assert!(server.tls);
        assert_eq!(server.port, Some(1993));

        // A user that is an address is the account.
        let url = MailboxUrl::parse("imap://me%40example.org@mail.example.com/@sentitems").unwrap();
        assert_eq!(url.account, AccountId::new("me@example.org"));
        assert_eq!(
            url.folder,
            FolderPath::WellKnown(WellKnownFolder::SentItems, vec![])
        );
        assert_eq!(
            MailboxUrl::parse("imap://me@h/").unwrap().folder,
            FolderPath::Root
        );
        assert!(MailboxUrl::parse("imap://me@h/@spam").is_err());
    }

    #[test]
    fn an_imap_user_comes_from_imap_user_when_the_url_has_none() {
        let url = MailboxUrl::parse("imaps://mail.example.com/INBOX").unwrap();
        assert_eq!(url.account, AccountId::new("mail.example.com"));
        let url = url.with_user("me");
        assert_eq!(url.account, AccountId::new("me@mail.example.com"));
        // A user in the URL stays.
        let url = MailboxUrl::parse("imaps://you@mail.example.com/")
            .unwrap()
            .with_user("me");
        assert_eq!(url.account, AccountId::new("you@mail.example.com"));
        // An Office365 mailbox has no user to set.
        let url = MailboxUrl::parse("msgraph://a@b.c/")
            .unwrap()
            .with_user("me");
        assert_eq!(url.account, AccountId::new("a@b.c"));
    }

    #[test]
    fn imap_urls_round_trip_without_the_password() {
        for url in [
            "imaps://me@mail.example.com/",
            "imap://me@mail.example.com:143/INBOX/Work",
            "imaps://mail.example.com/@inbox/Sub%2Ffolder",
            "imaps://me%40example.org@mail.example.com/Sent",
        ] {
            assert_eq!(MailboxUrl::parse(url).unwrap().to_string(), url);
        }
        let url = MailboxUrl::parse("imaps://me:secret@mail.example.com/INBOX").unwrap();
        assert_eq!(url.to_string(), "imaps://me@mail.example.com/INBOX");
    }

    #[test]
    fn folders_of_an_imap_account_are_found_by_name() {
        let account = AccountId::new("me@mail.example.com");
        let folder = "imaps://me@mail.example.com/";
        let path = |text| folder_path(text, folder, &account);
        assert_eq!(path("=Sent"), Ok(FolderPath::Named(vec!["Sent".into()])));
        assert_eq!(
            path("=@sentitems"),
            Ok(FolderPath::WellKnown(WellKnownFolder::SentItems, vec![]))
        );
        assert_eq!(
            path("imaps://me@mail.example.com/INBOX/Work"),
            Ok(FolderPath::Named(vec!["INBOX".into(), "Work".into()]))
        );
        // A URL without a user is of the user at that server.
        assert!(path("imaps://mail.example.com/INBOX").is_ok());
        assert!(path("imaps://other.example.com/INBOX").is_err());
        assert!(path("imaps://you@mail.example.com/INBOX").is_err());
    }

    #[test]
    fn a_folder_is_of_the_server_of_the_account_not_another_with_the_same_user() {
        let account = AccountId::new("me@example.org");
        let folder = "imaps://me%40example.org@mail.example.org/";
        let path = |text| folder_path(text, folder, &account);
        assert!(path("imaps://me%40example.org@mail.example.org/INBOX").is_ok());
        // The same user at another server isn't this mailbox.
        assert!(path("imaps://me%40example.org@other.example.net/INBOX").is_err());
        // Nor is the same host on another port.
        assert!(path("imaps://me%40example.org@mail.example.org:1993/INBOX").is_err());
        // Nor Office365's.
        assert!(path("msgraph://me@example.org/@inbox").is_err());
        // Without a base that is a mailbox, the account is all there is to go by.
        assert!(
            folder_path(
                "imaps://me%40example.org@other.example.net/INBOX",
                "~/Mail",
                &account
            )
            .is_ok()
        );
    }

    #[test]
    fn an_email_login_at_another_domain_than_its_server_has_folders_at_that_server() {
        // alice@example.org logs in at mail.example.org.
        let account = AccountId::new("alice@example.org");
        let folder = "imaps://mail.example.org/";
        let path = |text| folder_path(text, folder, &account);
        assert_eq!(path("=Trash"), Ok(FolderPath::Named(vec!["Trash".into()])));
        assert!(path("imaps://mail.example.org/Trash").is_ok());
        assert!(path("imaps://other.example.org/Trash").is_err());
    }
}
