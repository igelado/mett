//! The app's settings, from the configuration.

use crate::account;
use crate::app;
use crate::app::Settings;
use crate::color;
use crate::compose;
use crate::config;
use crate::editor;
use crate::expando;
use crate::platform;
use crate::sort;

/// The app's settings from the configuration, for `account`.
pub(crate) fn settings_from_config(
    config: &config::Config,
    account: &mett_core::AccountId,
) -> Result<Settings, String> {
    settings_keeping(config, account, None)
}

/// Like [`settings_from_config`], with `signature`, if given, instead of
/// reading the signature file again.
pub(crate) fn settings_keeping(
    config: &config::Config,
    account: &mett_core::AccountId,
    signature: Option<Option<String>>,
) -> Result<Settings, String> {
    let mut compose = compose::Settings::from_config_keeping(config, account, signature)?;
    compose.builtin_editor = editor::builtin(
        config.string("editor"),
        config.string("editor_keys"),
        platform::gui(),
        &|name| std::env::var(name).ok(),
    )?;
    let trash = match config.string("trash") {
        "" => None,
        // mutt's shortcuts work here too (`^` and `-` name no folder before
        // one is open, and are taken as written).
        trash => Some(
            config::mailbox::configured_path(
                trash,
                config.string("folder"),
                account,
                config.string("spoolfile"),
                config.string("mbox"),
                config.string("record"),
            )
            .map_err(|e| format!("trash: {e}"))?,
        ),
    };
    let page_size = match config.get("office365_page_size") {
        Some(config::Value::Number(n)) if *n > 0 => {
            u32::try_from(*n).map_err(|_| format!("office365_page_size {n} is too large"))?
        }
        _ => return Err("office365_page_size must be a positive number".into()),
    };
    // How often mail is looked for: mutt's `mail_check` for an IMAP mailbox,
    // Office365's own setting for those.
    let api = account::Api::from_config(config);
    let interval = match api {
        account::Api::Imap => "mail_check",
        account::Api::Graph | account::Api::Ews => "office365_sync_interval",
    };
    let sync_interval = match config.get(interval) {
        Some(config::Value::Number(0)) => None,
        Some(config::Value::Number(n)) if *n > 0 => {
            Some(std::time::Duration::from_secs(n.unsigned_abs()))
        }
        _ => return Err(format!("{interval} must be 0 (off) or a number of seconds")),
    };
    // Without the password a URL may hold: the mailboxes, as `mailboxes`
    // keeps them, don't have it either, and are shown relative to this.
    let stripped_folder = config::server::strip_password(config.string("folder"));
    let folder = stripped_folder.as_str();
    let mailboxes = config
        .mailboxes()
        .iter()
        .map(|mailbox| {
            let path = config::mailbox::folder_path(&mailbox.path, folder, account)
                .map_err(|e| format!("mailboxes: {e}"))?;
            // As mutt shows it: relative to `folder`, with `=`, if it is
            // below it.
            let below = mailbox.path.strip_prefix(folder).and_then(|rest| {
                if folder.ends_with('/') {
                    Some(rest)
                } else {
                    rest.strip_prefix('/')
                }
            });
            let text = match below {
                Some(rest) if !folder.is_empty() && !rest.is_empty() => format!("={rest}"),
                _ => mailbox.path.clone(),
            };
            Ok(app::MailboxEntry {
                name: mailbox.label.clone().unwrap_or_else(|| text.clone()),
                label: mailbox.label.clone(),
                text,
                path,
                notify: mailbox.notify,
                poll: mailbox.poll,
            })
        })
        .collect::<Result<_, String>>()?;
    let quote =
        regex::Regex::new(config.string("quote_regex")).map_err(|e| format!("quote_regex: {e}"))?;
    let display = expando::Display {
        index_format: config.string("index_format").to_owned(),
        pager_format: config.string("pager_format").to_owned(),
        status_format: config.string("status_format").to_owned(),
        date_format: config.string("date_format").to_owned(),
        quote,
        colors: config
            .colors()
            // The window has colors, whatever terminal it was started
            // from.
            .active(!platform::gui() && color::mono_terminal(&|name| std::env::var(name).ok()))
            .clone(),
        wrap: match config.get("wrap") {
            Some(config::Value::Number(n)) => *n,
            _ => 0,
        },
        pager_index_lines: match config.get("pager_index_lines") {
            Some(config::Value::Number(n)) => usize::try_from(*n)
                .map_err(|_| "pager_index_lines must be 0 (off) or a number of lines")?,
            _ => 0,
        },
        collapse_links: config.flag("collapse_links"),
    };
    let ordering = app::Ordering {
        sort: sort::Sort::parse(config.string("sort"), true).map_err(|e| format!("sort: {e}"))?,
        aux: sort::Sort::parse(config.string("sort_aux"), false)
            .map_err(|e| format!("sort_aux: {e}"))?,
        ascii: config.flag("ascii_chars"),
    };
    let sidebar = sidebar_settings(config, account)?;
    let alias_file = match config.string("alias_file") {
        "" => config.user_file().map(std::path::Path::to_path_buf),
        file => Some(std::path::PathBuf::from(file)),
    };
    Ok(Settings {
        aliases: config.aliases().to_vec(),
        alias_file,
        query: app::QuerySettings {
            command: config.string("query_command").to_owned(),
            directory: config.flag("office365_query_directory"),
        },
        home: config.home().map(str::to_owned),
        page_size,
        mailboxes,
        sidebar,
        api,
        display,
        ordering,
        sync_interval,
        compose,
        account: account.clone(),
        // The mailboxes are shown relative to it: without a password in it.
        folder: stripped_folder,
        trash,
        delete: config.quad("delete"),
        resolve: config.flag("resolve"),
        shortcuts: app::Shortcuts {
            spoolfile: config.string("spoolfile").to_owned(),
            mbox: config.string("mbox").to_owned(),
            record: config.string("record").to_owned(),
        },
    })
}

/// The sidebar's settings from the configuration, for `account`.
pub(crate) fn sidebar_settings(
    config: &config::Config,
    account: &mett_core::AccountId,
) -> Result<app::SidebarSettings, String> {
    let number = |name: &str| match config.get(name) {
        Some(config::Value::Number(n)) if *n >= 0 => {
            usize::try_from(*n).map_err(|_| format!("{name} {n} is too large"))
        }
        _ => Err(format!("{name} must be 0 or more")),
    };
    let folder = config::server::strip_password(config.string("folder"));
    let folder = folder.as_str();
    // Those that name no folder of the account can't match a mailbox.
    let path = |text: &str| config::mailbox::folder_path(text, folder, account).ok();
    Ok(app::SidebarSettings {
        visible: config.flag("sidebar_visible"),
        width: number("sidebar_width")?,
        min_width: number("sidebar_min_width")?,
        format: config.string("sidebar_format").to_owned(),
        short_path: config.flag("sidebar_short_path"),
        folder_indent: config.flag("sidebar_folder_indent"),
        indent_string: config.string("sidebar_indent_string").to_owned(),
        delim_chars: config.string("sidebar_delim_chars").to_owned(),
        divider_char: config.string("sidebar_divider_char").to_owned(),
        sort: app::SidebarSort::parse(config.string("sidebar_sort_method"))
            .map_err(|e| format!("sidebar_sort_method: {e}"))?,
        new_mail_only: config.flag("sidebar_new_mail_only"),
        next_new_wrap: config.flag("sidebar_next_new_wrap"),
        on_right: config.flag("sidebar_on_right"),
        whitelist: config
            .sidebar_whitelist()
            .iter()
            .filter_map(|text| path(text))
            .collect(),
        spoolfile: Some(config.string("spoolfile"))
            .filter(|spoolfile| !spoolfile.is_empty())
            .and_then(path),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailboxes_are_shown_relative_to_the_folder() {
        let mut config = config::Config::new(None);
        for line in [
            "set folder=msgraph://a@b.c/Work signature=\"\"",
            "mailboxes =Old msgraph://a@b.c/Workshops -label Mine msgraph://a@b.c/@inbox",
            "set office365_sync_interval=0",
        ] {
            config.run_line(line).unwrap();
        }
        let account = mett_core::AccountId::new("a@b.c");
        let settings = settings_from_config(&config, &account).unwrap();
        let shown: Vec<_> = settings
            .mailboxes
            .iter()
            .map(|m| (m.name.as_str(), m.text.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                ("=Old", "=Old"),
                // Not below Work.
                ("msgraph://a@b.c/Workshops", "msgraph://a@b.c/Workshops"),
                ("Mine", "msgraph://a@b.c/@inbox"),
            ]
        );
        assert_eq!(settings.sync_interval, None);
        config.run_line("mailboxes msgraph://z@y.x/@inbox").unwrap();
        assert!(settings_from_config(&config, &account).is_err());
    }

    #[test]
    fn an_imap_mailbox_is_checked_every_mail_check_seconds() {
        let mut config = config::Config::new(None);
        for line in [
            "set spoolfile=imaps://me@mail.example.com/INBOX signature=\"\"",
            "set office365_sync_interval=0",
        ] {
            config.run_line(line).unwrap();
        }
        let account = mett_core::AccountId::new("me@mail.example.com");
        let settings = settings_from_config(&config, &account).unwrap();
        // Office365's setting is not IMAP's.
        assert_eq!(
            settings.sync_interval,
            Some(std::time::Duration::from_secs(5))
        );
        config.run_line("set mail_check=30").unwrap();
        let settings = settings_from_config(&config, &account).unwrap();
        assert_eq!(
            settings.sync_interval,
            Some(std::time::Duration::from_secs(30))
        );
        config.run_line("set mail_check=0").unwrap();
        assert_eq!(
            settings_from_config(&config, &account)
                .unwrap()
                .sync_interval,
            None
        );
        // And an Office365 mailbox is not checked by `mail_check`.
        config
            .run_line("set spoolfile=msgraph://me@contoso.com/@inbox office365_sync_interval=60")
            .unwrap();
        let account = mett_core::AccountId::new("me@contoso.com");
        let settings = settings_from_config(&config, &account).unwrap();
        assert_eq!(
            settings.sync_interval,
            Some(std::time::Duration::from_secs(60))
        );
    }

    #[test]
    fn passwords_in_mailbox_urls_are_not_shown() {
        let mut config = config::Config::new(None);
        for line in [
            "set folder=imaps://alice:secret@mail.example.com/ signature=\"\"",
            "mailboxes =INBOX imaps://alice:secret@mail.example.com/Work",
        ] {
            config.run_line(line).unwrap();
        }
        let account = mett_core::AccountId::new("alice@mail.example.com");
        let settings = settings_from_config(&config, &account).unwrap();
        assert!(!settings.folder.contains("secret"), "{}", settings.folder);
        assert_eq!(settings.folder, "imaps://alice@mail.example.com/");
        for mailbox in &settings.mailboxes {
            assert!(!mailbox.text.contains("secret"), "{}", mailbox.text);
            assert!(!mailbox.name.contains("secret"), "{}", mailbox.name);
        }
        // They still name their folders.
        let names: Vec<_> = settings.mailboxes.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["=INBOX", "=Work"]);
    }
}
