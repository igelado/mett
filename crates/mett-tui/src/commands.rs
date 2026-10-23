//! `:` commands, which change the configuration while mett runs.

use crate::app;
use crate::config;
use crate::settings::settings_keeping;

/// The configuration that `:` commands change.
pub(crate) struct Configuration {
    pub(crate) config: config::Config,
    /// The signature in use: see [`run_command`].
    pub(crate) signature: Option<String>,
}

/// `:` commands waiting to run, one at a time, in order, and the
/// configuration while none is running.
pub(crate) struct Commands<T = Configuration> {
    /// The configuration; `None` while a command has it.
    idle: Option<T>,
    pub(crate) queue: std::collections::VecDeque<String>,
}

impl<T> Commands<T> {
    pub(crate) fn new(configuration: T) -> Self {
        Self {
            idle: Some(configuration),
            queue: std::collections::VecDeque::new(),
        }
    }

    /// The next command to run, with the configuration it changes, unless
    /// one is running.
    pub(crate) fn start(&mut self) -> Option<(String, T)> {
        self.idle.as_ref()?;
        let command = self.queue.pop_front()?;
        Some((command, self.idle.take()?))
    }

    /// Takes the configuration back from the command that ran.
    pub(crate) fn finish(&mut self, configuration: T) {
        self.idle = Some(configuration);
    }
}

/// Runs a `:` command and reports what the configuration is now.
///
/// `signature` is the signature in use, kept unless the command changes
/// the `signature` variable.
pub(crate) fn run_command(
    config: &mut config::Config,
    account: &mett_core::AccountId,
    signature: &mut Option<String>,
    command: &str,
) -> app::Reconfigured {
    let before = config.clone();
    let mut outcome = config.run_line(command);
    // A command that fails changes nothing: what it did before failing is
    // undone.
    if outcome.is_err() {
        *config = before;
        let _ = config.take_key_commands();
        let _ = config.take_pushed();
        return app::Reconfigured {
            outcome,
            settings: settings_keeping(config, account, Some(signature.clone())),
            key_commands: Vec::new(),
            pushed: Vec::new(),
        };
    }
    let mut changed = config.changed_since(&before);
    // `unset header_cache` keeps its (empty) value, but turns caching off.
    if config.was_set("header_cache") != before.was_set("header_cache")
        && !changed.contains(&"header_cache")
    {
        changed.push("header_cache");
        changed.sort_unstable();
    }
    // The mailbox, its sign-in and its connections were set up when mett
    // started.
    if let Ok(warnings) = &mut outcome {
        warnings.extend(
            changed
                .iter()
                .filter(|name| {
                    (name.starts_with("office365_")
                        || name.starts_with("msgraph_")
                        || name.starts_with("ssl_")
                        || [
                            "use_ews",
                            "ews_url",
                            "header_cache",
                            "imap_user",
                            "imap_pass",
                            "smtp_url",
                            "smtp_pass",
                            "tab_position",
                        ]
                        .contains(*name))
                        && ![
                            "office365_page_size",
                            "office365_sync_interval",
                            "office365_query_directory",
                        ]
                        .contains(*name)
                })
                .map(|name| format!("{name}: takes effect when mett starts again")),
        );
    }
    // The signature file is read again only if the command changed it: a
    // command about something else doesn't fail because of it.
    // Setting it to its default value counts: a missing default is fine,
    // a missing file the user named isn't.
    let signature_changed = changed.contains(&"signature")
        || config.was_set("signature") != before.was_set("signature");
    let keep = (!signature_changed).then(|| signature.clone());
    match settings_keeping(config, account, keep) {
        Ok(settings) => app::Reconfigured {
            outcome,
            settings: Ok({
                signature.clone_from(&settings.compose.signature);
                settings
            }),
            key_commands: config.take_key_commands(),
            pushed: config.take_pushed(),
        },
        // A command that leaves the settings unusable is undone, so the
        // next one starts from settings that work.
        Err(error) => {
            *config = before;
            app::Reconfigured {
                outcome,
                settings: Err(error),
                key_commands: Vec::new(),
                pushed: Vec::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Settings;
    use crate::settings::settings_from_config;
    use crate::{demo, demo_config};

    /// The demo's configuration, as `:` commands change it.
    struct Commanded {
        config: config::Config,
        settings: Settings,
        signature: Option<String>,
    }

    impl Commanded {
        /// With `home` as the home directory, and no signature read yet.
        fn demo(home: &str) -> Self {
            let (config, settings) = demo_config(Some(home.into()));
            Self {
                config,
                settings,
                signature: None,
            }
        }

        fn run(&mut self, command: &str) -> app::Reconfigured {
            run_command(
                &mut self.config,
                &self.settings.account,
                &mut self.signature,
                command,
            )
        }
    }

    #[test]
    fn a_command_that_breaks_the_settings_is_undone() {
        let mut demo = Commanded::demo("/home/alice");
        let broken = demo.run("set signature=/nonexistent/mett/signature");
        assert!(broken.settings.is_err());
        // The next command works on what was there before.
        let next = demo.run("set resolve=no");
        assert!(!next.settings.unwrap().resolve);
    }

    #[test]
    fn wrap_takes_negative_numbers() {
        let mut demo = Commanded::demo("/home/alice");
        assert_eq!(demo.settings.display.wrap, 0, "the default");
        let set = demo.run("set wrap=-10");
        assert_eq!(set.settings.unwrap().display.wrap, -10);
    }

    #[test]
    fn pager_index_lines_takes_no_negative_numbers() {
        let mut demo = Commanded::demo("/home/alice");
        let set = demo.run("set pager_index_lines=5");
        assert_eq!(set.settings.unwrap().display.pager_index_lines, 5);
        assert!(demo.run("set pager_index_lines=-1").settings.is_err());
    }

    #[test]
    fn commands_run_one_at_a_time_in_order() {
        let mut commands = Commands::new(0);
        commands.queue.extend(["a".to_owned(), "b".to_owned()]);
        let (first, n) = commands.start().unwrap();
        assert_eq!(first, "a");
        assert!(commands.start().is_none(), "a is running");
        commands.queue.push_back("c".into());
        commands.finish(n + 1);
        let (second, n) = commands.start().unwrap();
        assert_eq!((second.as_str(), n), ("b", 1));
        commands.finish(n + 1);
        assert_eq!(commands.start().unwrap().0, "c");
        commands.finish(3);
        assert!(commands.start().is_none(), "nothing left");
    }

    #[test]
    fn naming_the_default_signature_checks_it() {
        let home = std::env::temp_dir().join(format!("mett-home-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let mut demo = Commanded::demo(&home.display().to_string());
        // ~/.signature doesn't exist: fine as the default, not when named.
        let named = demo.run("set signature=~/.signature");
        assert!(named.settings.is_err());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn mailbox_settings_wait_for_a_restart() {
        let mut demo = Commanded::demo("/home/alice");
        let done = demo.run("set office365_client_id=abc office365_page_size=10");
        assert_eq!(
            done.outcome,
            Ok(vec![
                "office365_client_id: takes effect when mett starts again".to_owned()
            ])
        );
        assert_eq!(demo.config.string("office365_client_id"), "abc", "kept");
        // The IMAP and SMTP connections were made then too.
        let done = demo.run(
            "set imap_pass=x smtp_url=smtps://smtp.example.com ssl_force_tls=yes tab_position=bottom",
        );
        assert_eq!(
            done.outcome,
            Ok(vec![
                "imap_pass: takes effect when mett starts again".to_owned(),
                "smtp_url: takes effect when mett starts again".to_owned(),
                "ssl_force_tls: takes effect when mett starts again".to_owned(),
                "tab_position: takes effect when mett starts again".to_owned(),
            ])
        );
        // Unsetting the cache keeps its empty value, but stops caching.
        let done = demo.run("unset header_cache");
        assert_eq!(
            done.outcome,
            Ok(vec![
                "header_cache: takes effect when mett starts again".to_owned()
            ])
        );
    }

    #[test]
    fn an_unreadable_signature_only_fails_commands_that_set_it() {
        let dir = std::env::temp_dir().join(format!("mett-sig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("signature");
        std::fs::write(&path, "-- \nAlice\n").unwrap();
        let mut demo = Commanded::demo("/home/alice");
        // Quoted: a Windows path's `\` would be an escape.
        let set = format!("set signature='{}'", path.display());
        let first = demo.run(&set);
        assert!(first.settings.is_ok());
        assert_eq!(demo.signature.as_deref(), Some("-- \nAlice\n"));
        // The file goes away; commands about other things still work.
        std::fs::remove_file(&path).unwrap();
        let other = demo.run("set resolve=no").settings.unwrap();
        assert!(!other.resolve);
        assert_eq!(other.compose.signature.as_deref(), Some("-- \nAlice\n"));
        // Setting it again reads it, and fails.
        let again = demo.run(&set);
        assert!(again.settings.is_ok(), "unchanged value: not read again");
        let reset = demo.run("set signature=/nonexistent/mett/other");
        assert!(reset.settings.is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn demo_commands_keep_the_demo_folders() {
        let (mut config, settings) = demo_config(Some("/home/alice".into()));
        assert_eq!(settings.folder, "");
        // An unrelated `:` command leaves `=Archive` meaning the demo's.
        config.run_line("set resolve=no").unwrap();
        let after = settings_from_config(&config, &settings.account).unwrap();
        assert_eq!(after.folder, "");
        assert!(!after.resolve);
        assert!(
            after.compose.me.iter().any(|me| me == demo::YOU),
            "still the demo user"
        );
        assert_eq!(after.compose.signature, None, "not the user's signature");
    }

    #[test]
    fn the_sidebar_is_set_at_the_prompt() {
        let mut demo = Commanded::demo("/home/alice");
        let done =
            demo.run("set sidebar_visible sidebar_width=25 sidebar_sort_method=reverse-alpha");
        let sidebar = done.settings.unwrap().sidebar;
        assert!(sidebar.visible);
        assert_eq!(sidebar.width, 25);
        assert!(sidebar.sort.reverse);
        assert!(demo.run("set sidebar_sort_method=size").settings.is_err());
        assert!(demo.run("set sidebar_min_width=-1").settings.is_err());
        let done = demo.run("toggle sidebar_visible");
        assert!(!done.settings.unwrap().sidebar.visible);
    }

    #[test]
    fn a_failing_command_changes_nothing() {
        let mut demo = Commanded::demo("/home/alice");
        demo.signature = demo.settings.compose.signature.clone();
        let before = demo.config.clone();
        // The first command works, the second doesn't: neither stays.
        let done = demo.run("set resolve=no; set timeout=soon");
        assert!(done.outcome.is_err());
        assert!(demo.config.changed_since(&before).is_empty());
    }
}
