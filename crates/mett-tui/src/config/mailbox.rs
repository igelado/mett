//! Mailbox paths: `msgraph://` URLs and mutt's shortcuts
//! (`docs/CONFIG-OFFICE365.md` §3, `docs/MUTTRC.md` §5).

use std::fmt;

use mett_core::{AccountId, WellKnownFolder};

/// A folder in an Office365 mailbox, from a `msgraph://<account>/<path>`
/// URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxUrl {
    /// The account (user principal name).
    pub account: AccountId,
    /// The folder below the mailbox root.
    pub folder: FolderPath,
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

impl MailboxUrl {
    /// Parses a `msgraph://` URL.
    pub fn parse(url: &str) -> Result<Self, MailboxError> {
        let rest = url
            .strip_prefix(SCHEME)
            .ok_or_else(|| MailboxError(format!("{url}: not a msgraph:// mailbox")))?;
        let (account, path) = rest.split_once('/').unwrap_or((rest, ""));
        let valid_upn = account.split_once('@').is_some_and(|(user, domain)| {
            !user.is_empty() && !domain.is_empty() && !domain.contains('@')
        });
        if !valid_upn {
            return Err(MailboxError(format!(
                "{url}: expected msgraph://<user@domain>/<folder>"
            )));
        }
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let folder = match segments.split_first() {
            None => FolderPath::Root,
            Some((first, rest)) => match first.strip_prefix('@') {
                Some(name) => {
                    let which = name.parse::<WellKnownFolder>().map_err(|_| {
                        MailboxError(format!("{url}: unknown well-known folder @{name}"))
                    })?;
                    // `@inbox/@focused` and `@inbox/@other` are the Focused
                    // Inbox views; any other name is a real folder.
                    if which == WellKnownFolder::Inbox
                        && let Some(view @ (&"@focused" | &"@other")) = rest.first()
                    {
                        return Err(MailboxError(format!("{url}: {view} is not supported yet")));
                    }
                    FolderPath::WellKnown(which, rest.iter().map(|s| (*s).to_owned()).collect())
                }
                None => FolderPath::Named(segments.iter().map(|s| (*s).to_owned()).collect()),
            },
        };
        Ok(Self {
            account: AccountId::new(account),
            folder,
        })
    }
}

impl fmt::Display for MailboxUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{SCHEME}{}/", self.account)?;
        match &self.folder {
            FolderPath::Root => Ok(()),
            FolderPath::WellKnown(which, rest) => {
                write!(f, "@{which}")?;
                rest.iter().try_for_each(|s| write!(f, "/{s}"))
            }
            FolderPath::Named(names) => f.write_str(&names.join("/")),
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
    let url = if expanded.starts_with(SCHEME) {
        let url = MailboxUrl::parse(&expanded)?;
        if !url.account.as_str().eq_ignore_ascii_case(account.as_str()) {
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
        assert!(MailboxUrl::parse("imaps://outlook.office365.com/").is_err());
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
}
