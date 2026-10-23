//! Reading `mettrc`: mutt's configuration format with mett's extensions
//! (`docs/MUTTRC.md`, `docs/CONFIG-OFFICE365.md`).
//!
//! This first version supports the commands needed to configure an
//! account: `set`, `unset`, `toggle`, `reset` and `source`, plus `my_`
//! variables, `$` expansion and backticks. Other mutt commands are
//! recognised and ignored with a warning, so a mettrc that `source`s an
//! existing muttrc still loads.

mod lexer;
pub mod mailbox;
pub mod server;
mod vars;

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use lexer::{Command, Expander, Lexer, Word};
pub use vars::{Kind, Quad, VARS, Value, expand_home};

/// How deeply `source` may nest, to stop loops.
const MAX_SOURCE_DEPTH: usize = 16;

/// mutt commands that mett doesn't implement yet; they are ignored with a
/// warning rather than rejected.
const NOT_YET_SUPPORTED: &[&str] = &[
    "account-hook",
    "alternative_order",
    "append-hook",
    "attachments",
    "auto_view",
    "cd",
    "charset-hook",
    "close-hook",
    "crypt-hook",
    "echo",
    "fcc-hook",
    "fcc-save-hook",
    "folder-hook",
    "group",
    "hdr_order",
    "iconv-hook",
    "ignore",
    "index-format-hook",
    "lists",
    "mailto_allow",
    "mbox-hook",
    "message-hook",
    "mime_lookup",
    "my_hdr",
    "nospam",
    "open-hook",
    "reply-hook",
    "run",
    "save-hook",
    "score",
    "send-hook",
    "send2-hook",
    "setenv",
    "spam",
    "subjectrx",
    "subscribe",
    "unalternative_order",
    "unattachments",
    "unauto_view",
    "ungroup",
    "unhook",
    "unignore",
    "unlists",
    "unmailto_allow",
    "unmime_lookup",
    "unmy_hdr",
    "unscore",
    "unsetenv",
    "unsubjectrx",
    "unsubscribe",
];

/// A `bind` or `macro` command, for the key bindings to carry out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyCommand {
    /// `bind MENUS KEY FUNCTION`.
    Bind {
        /// Comma-separated menu names.
        menus: String,
        /// The key, in mutt's notation.
        key: String,
        /// The function's name.
        function: String,
    },
    /// `macro MENUS KEY SEQUENCE [DESCRIPTION]`.
    Macro {
        /// Comma-separated menu names.
        menus: String,
        /// The key, in mutt's notation.
        key: String,
        /// The keys and `<functions>` to play.
        sequence: String,
        /// What it does, for the help screen.
        description: Option<String>,
    },
}

/// A mailbox from the `mailboxes` command, checked for new mail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mailbox {
    /// The mailbox, with mutt's `=`, `+`, `!`, `>` and `<` shortcuts
    /// expanded as they stood when the command ran, as mutt does.
    pub path: String,
    /// What to call it (`-label`), instead of its path.
    pub label: Option<String>,
    /// Whether to say when new mail arrives in it (`-notify`, the
    /// default, or `-nonotify`).
    pub notify: bool,
    /// Whether to check it for new mail at all (`-poll`, the default, or
    /// `-nopoll`).
    pub poll: bool,
}

/// An account from the `account` command: a configuration of its own, which
/// is the one when the command ran plus what its file sets.
#[derive(Clone, Debug)]
pub struct Account {
    /// What the account's tab says (`account LABEL FILE`).
    pub label: String,
    /// Its configuration.
    pub config: Box<Config>,
}

/// The loaded configuration.
#[derive(Clone, Debug)]
pub struct Config {
    values: HashMap<&'static str, Value>,
    /// User-defined `my_` variables.
    user: HashMap<String, String>,
    /// Problems that stop the configuration from being used, as
    /// `file:line: message`.
    pub errors: Vec<String>,
    /// `alternates`: regular expressions for the user's other addresses.
    alternates: Vec<String>,
    /// `color` and `mono`.
    colors: crate::color::Palette,
    /// `alias`, in the order given.
    aliases: Vec<crate::alias::Alias>,
    /// Where each alias is in `aliases`, by its name ignoring case (see
    /// [`crate::alias::folded_key`]).
    alias_places: HashMap<String, usize>,
    /// `mailboxes`, in the order given.
    mailboxes: Vec<Mailbox>,
    /// `sidebar_whitelist`: mailboxes the sidebar shows even with
    /// `sidebar_new_mail_only`, expanded as `mailboxes` expands them.
    sidebar_whitelist: Vec<String>,
    /// `bind` and `macro` commands not yet taken by the key bindings.
    key_commands: Vec<KeyCommand>,
    /// Keys to play, from `push` and `exec`, in mutt's notation.
    pushed: Vec<String>,
    /// Things that were ignored, such as unsupported mutt commands.
    pub warnings: Vec<String>,
    /// Warnings from the command being run, waiting for its location.
    pending: Vec<String>,
    /// Ignored commands and variables already warned about: each is
    /// reported once, where it first appears.
    warned: std::collections::HashSet<String>,
    /// Old names of renamed variables that expansions (`$name`) used, to
    /// warn about once the command has run (expanding only reads).
    expanded_renamed: std::cell::RefCell<Vec<&'static str>>,
    /// Variables given a value by `set`, `unset` or `toggle`, as opposed
    /// to their defaults.
    assigned: std::collections::HashSet<&'static str>,
    /// What `set ?var` asked about, as `var=value`: shown at the `:`
    /// prompt (a mettrc has no one to show it to).
    answers: Vec<String>,
    home: Option<String>,
    /// See [`Config::set_user_file`].
    user_file: Option<PathBuf>,
    /// `account`, in the order given.
    accounts: Vec<Account>,
    /// Whether this is an account's configuration, which has no accounts
    /// of its own.
    in_account: bool,
}

/// Where a command came from, for messages and relative `source` paths.
struct Origin<'a> {
    file: &'a Path,
    depth: usize,
}

impl Config {
    /// A configuration with every variable at its default. `home` is used
    /// to expand `~` in paths.
    pub fn new(home: Option<String>) -> Self {
        let mut config = Self {
            values: HashMap::new(),
            user: HashMap::new(),
            errors: Vec::new(),
            alternates: Vec::new(),
            colors: crate::color::Palette::default(),
            aliases: Vec::new(),
            alias_places: HashMap::new(),
            mailboxes: Vec::new(),
            sidebar_whitelist: Vec::new(),
            key_commands: Vec::new(),
            pushed: Vec::new(),
            warnings: Vec::new(),
            pending: Vec::new(),
            warned: Default::default(),
            expanded_renamed: Default::default(),
            assigned: Default::default(),
            answers: Vec::new(),
            home,
            user_file: None,
            accounts: Vec::new(),
            in_account: false,
        };
        for var in VARS {
            config.reset(var);
        }
        config
    }

    /// The user's own mettrc, which `alias_file` defaults to, as mutt's
    /// does to the muttrc.
    pub fn set_user_file(&mut self, file: Option<PathBuf>) {
        self.user_file = file;
    }

    /// See [`Self::set_user_file`].
    pub fn user_file(&self) -> Option<&Path> {
        self.user_file.as_deref()
    }

    /// The home directory, for `~` in file names.
    pub fn home(&self) -> Option<&str> {
        self.home.as_deref()
    }

    /// The value of a known variable.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.values.get(name)
    }

    /// Whether a known variable was given a value by the configuration,
    /// rather than left at (or reset to) its default.
    pub fn was_set(&self, name: &str) -> bool {
        self.assigned.contains(name)
    }

    /// The variables whose values differ from `before`'s.
    pub fn changed_since(&self, before: &Config) -> Vec<&'static str> {
        let mut changed: Vec<&'static str> = self
            .values
            .iter()
            .filter(|(name, value)| before.values.get(*name) != Some(*value))
            .map(|(name, _)| *name)
            .collect();
        changed.sort_unstable();
        changed
    }

    /// The value of a known string or path variable, or `""`.
    pub fn string(&self, name: &str) -> &str {
        match self.get(name) {
            Some(Value::Str(s)) => s,
            _ => "",
        }
    }

    /// The value of a known boolean variable.
    pub fn flag(&self, name: &str) -> bool {
        self.get(name) == Some(&Value::Bool(true))
    }

    /// The value of a known quad-option, or `no`.
    pub fn quad(&self, name: &str) -> Quad {
        match self.get(name) {
            Some(Value::Quad(q)) => *q,
            _ => Quad::No,
        }
    }

    /// The user's other addresses, as regular expressions (`alternates`).
    pub fn alternates(&self) -> &[String] {
        &self.alternates
    }

    /// How each part of the screen is drawn (`color`, `mono`).
    pub fn colors(&self) -> &crate::color::Palette {
        &self.colors
    }

    /// The aliases (`alias`).
    pub fn aliases(&self) -> &[crate::alias::Alias] {
        &self.aliases
    }

    /// The accounts the `account` commands made, in order: with any, mett
    /// opens each in a tab, with its own configuration.
    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    /// With accounts, an alias made with `a` is saved by default in `aliases`
    /// beside the mettrc, which every account reads: the mettrc itself is
    /// read once, before the accounts are made, so what is added to it later
    /// would be in none of them. An account that sets `alias_file` keeps its
    /// own. Call it once the files are loaded.
    pub fn share_aliases_with_accounts(&mut self) {
        if self.accounts.is_empty() {
            return;
        }
        let Some(file) = self
            .user_file
            .as_deref()
            .and_then(Path::parent)
            .map(|dir| dir.join("aliases"))
        else {
            return;
        };
        // Only if an account will use it.
        if !self
            .accounts
            .iter()
            .any(|account| !account.config.was_set("alias_file"))
        {
            return;
        }
        // Read once, with what the mettrc has set (`$my_addr`) but on its
        // own: what an account has set is above it.
        let mut shared = self.clone();
        shared.accounts.clear();
        shared.aliases.clear();
        shared.alias_places.clear();
        (shared.errors, shared.warnings) = (Vec::new(), Vec::new());
        if file.exists() {
            shared.source(&file, 1);
            self.errors.append(&mut shared.errors);
            self.warnings.append(&mut shared.warnings);
        }
        for account in &mut self.accounts {
            let config = &mut account.config;
            if config.was_set("alias_file") {
                continue;
            }
            // Set as it is, not written in a mettrc line: a path may hold
            // what a mettrc takes for quotes.
            config.store("alias_file", Value::Str(file.display().to_string()));
            for alias in &shared.aliases {
                let key = crate::alias::folded_key(&alias.key);
                if !config.alias_places.contains_key(&key) {
                    config.alias_places.insert(key, config.aliases.len());
                    config.aliases.push(alias.clone());
                }
            }
        }
    }

    /// Takes the accounts (see [`Config::accounts`]).
    pub fn take_accounts(&mut self) -> Vec<Account> {
        std::mem::take(&mut self.accounts)
    }

    /// The mailboxes to check for new mail (`mailboxes`).
    pub fn mailboxes(&self) -> &[Mailbox] {
        &self.mailboxes
    }

    /// The mailboxes the sidebar always shows (`sidebar_whitelist`).
    pub fn sidebar_whitelist(&self) -> &[String] {
        &self.sidebar_whitelist
    }

    /// The `bind` and `macro` commands read since the last call.
    pub fn take_key_commands(&mut self) -> Vec<KeyCommand> {
        std::mem::take(&mut self.key_commands)
    }

    /// The keys `push` and `exec` asked to play since the last call.
    pub fn take_pushed(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pushed)
    }

    /// Runs one command typed at mett's `:` prompt. Returns its warnings,
    /// or its first error.
    pub fn run_line(&mut self, line: &str) -> Result<Vec<String>, String> {
        let (errors, warnings) = (self.errors.len(), self.warnings.len());
        self.answers.clear();
        // Each command typed is answered, even if it was ignored before.
        let warned = std::mem::take(&mut self.warned);
        self.run(
            line,
            &Origin {
                file: Path::new(""),
                depth: 0,
            },
        );
        self.warned = warned;
        let new_errors: Vec<String> = self.errors.drain(errors..).collect();
        let new_warnings: Vec<String> = self.warnings.drain(warnings..).collect();
        // The location is just the prompt: leave it out.
        let strip = |m: String| m.strip_prefix(":1: ").map(str::to_owned).unwrap_or(m);
        match new_errors.into_iter().map(strip).next() {
            Some(error) => Err(error),
            // What was asked about comes first.
            None => Ok(std::mem::take(&mut self.answers)
                .into_iter()
                .chain(new_warnings.into_iter().map(strip))
                .collect()),
        }
    }

    /// Reads and runs the mettrc at `path`.
    pub fn load_file(&mut self, path: &Path) {
        self.source(path, 0);
    }

    /// Runs mettrc commands from `source`; `file` names it in messages and
    /// anchors relative `source` paths.
    #[cfg(test)]
    pub fn load_str(&mut self, source: &str, file: &Path) {
        self.run(source, &Origin { file, depth: 0 });
    }

    /// The mailbox to open at start-up: `spoolfile`, with `=` and `+`
    /// relative to `folder`.
    pub fn spoolfile(&self) -> Result<mailbox::MailboxUrl, mailbox::MailboxError> {
        let spoolfile = self.string("spoolfile");
        if spoolfile.is_empty() {
            return Err(mailbox::MailboxError(
                "spoolfile is not set; set it to a mailbox such as msgraph://you@example.com/@inbox or imaps://you@example.com/INBOX"
                    .into(),
            ));
        }
        let url = mailbox::expand_shortcut(spoolfile, self.string("folder"));
        mailbox::MailboxUrl::parse(&url)
            .map(|url| url.with_user(self.string("imap_user")))
            .map_err(|e| mailbox::MailboxError(format!("spoolfile: {e}")))
    }

    /// `set` commands that recreate the current values, like `mutt -D`.
    pub fn dump(&self) -> Vec<String> {
        let mut lines: Vec<String> = VARS
            .iter()
            .map(|var| format!("set {}={}", var.name, self.values[var.name]))
            .collect();
        let mut user: Vec<_> = self.user.iter().collect();
        user.sort();
        lines.extend(
            user.into_iter()
                .map(|(name, value)| format!("set {name}={}", Value::Str(value.clone()))),
        );
        lines.extend(
            self.aliases
                .iter()
                .map(|a| crate::alias::line(&a.key, &a.value)),
        );
        lines.extend(self.mailboxes.iter().map(|mailbox| {
            let mut line = "mailboxes".to_owned();
            if let Some(label) = &mailbox.label {
                line.push_str(&format!(" -label {}", Value::Str(label.clone())));
            }
            if !mailbox.notify {
                line.push_str(" -nonotify");
            }
            if !mailbox.poll {
                line.push_str(" -nopoll");
            }
            format!("{line} {}", Value::Str(mailbox.path.clone()))
        }));
        lines.extend(
            self.sidebar_whitelist
                .iter()
                .map(|path| format!("sidebar_whitelist {}", Value::Str(path.clone()))),
        );
        lines
    }

    fn source(&mut self, path: &Path, depth: usize) {
        if depth > MAX_SOURCE_DEPTH {
            self.errors.push(format!(
                "{}: too many nested source commands",
                path.display()
            ));
            return;
        }
        match fs::read_to_string(path) {
            Ok(text) => self.run(&text, &Origin { file: path, depth }),
            Err(e) => self
                .errors
                .push(format!("{}: cannot read: {e}", path.display())),
        }
    }

    /// `account LABEL FILE`: the configuration so far, and what the file
    /// sets, as an account of its own. What the commands after it set is not
    /// in it: share settings by putting them before the accounts.
    fn add_account(&mut self, label: String, path: &Path, depth: usize) -> Result<(), String> {
        if self.in_account {
            return Err("account: not in an account's own file".to_owned());
        }
        if label.trim().is_empty() {
            return Err("account: the label is empty".to_owned());
        }
        if self.accounts.iter().any(|a| a.label == label) {
            return Err(format!("account: there is an account {label} already"));
        }
        let mut config = self.clone();
        config.accounts.clear();
        config.in_account = true;
        // Its own: what it says, not what was said before.
        (config.errors, config.warnings) = (Vec::new(), Vec::new());
        config.key_commands.clear();
        config.pushed.clear();
        config.source(path, depth);
        self.errors.append(&mut config.errors);
        self.warnings.append(&mut config.warnings);
        // Keys are the same in every account.
        self.key_commands.append(&mut config.key_commands);
        self.accounts.push(Account {
            label,
            config: Box::new(config),
        });
        Ok(())
    }

    fn run(&mut self, source: &str, origin: &Origin<'_>) {
        // Files written on Windows use CRLF; treat them like LF so that a
        // trailing backslash still continues the line.
        let source = source.replace("\r\n", "\n");
        let mut lexer = Lexer::new(&source);
        loop {
            match lexer.next_command() {
                None => break,
                Some(Ok(command)) => self.execute(command, origin),
                Some(Err(e)) => self.errors.push(format!(
                    "{}:{}: {}",
                    origin.file.display(),
                    e.line,
                    e.message
                )),
            }
        }
    }

    fn execute(&mut self, command: Command, origin: &Origin<'_>) {
        let at = format!("{}:{}", origin.file.display(), command.line);
        let (name, args) = command.words.split_first().expect("commands are not empty");
        // Command names are taken literally, never expanded: deciding
        // whether a command is supported must not run anything.
        let result = match name.literal() {
            None => Err("a command name can't contain $variables or backticks".to_owned()),
            Some(name) => match name.as_str() {
                "set" => self.set(args),
                "unset" => self.each(args, |c, name| c.unset(name)),
                "toggle" => self.each(args, |c, name| c.toggle(name)),
                "reset" => self.each(args, |c, name| c.reset_named(name)),
                "alternates" => self.patterns(args).and_then(|regexes| {
                    for regex in regexes {
                        regex::Regex::new(&regex).map_err(|e| {
                            format!("alternates: bad regular expression {regex:?}: {e}")
                        })?;
                        if !self.alternates.contains(&regex) {
                            self.alternates.push(regex);
                        }
                    }
                    Ok(())
                }),
                "unalternates" => self.patterns(args).map(|regexes| {
                    for regex in regexes {
                        if regex == "*" {
                            self.alternates.clear();
                        } else {
                            self.alternates.retain(|a| *a != regex);
                        }
                    }
                }),
                "color" | "mono" | "uncolor" | "unmono" => {
                    // An object mett doesn't do yet is ignored before the
                    // rest is expanded, so its backticks never run.
                    let object = args.first().map(|word| self.expand(word)).transpose();
                    let unsupported = match &object {
                        Ok(Some(object)) => match crate::color::object_supported(object) {
                            Err(crate::color::ColorError::Unsupported(what)) => Some(what),
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some(what) = unsupported {
                        self.warn_once("color", &what, format!("{what}; ignored"));
                        Ok(())
                    } else {
                        // The object is expanded once (its backticks run
                        // once).
                        let words = object.and_then(|object| {
                            object
                                .into_iter()
                                .map(Ok)
                                .chain(args.iter().skip(1).map(|word| self.expand(word)))
                                .collect::<Result<Vec<_>, _>>()
                        });
                        words.and_then(|words| {
                            let palette = &mut self.colors;
                            let result = match name.as_str() {
                                "color" => palette.color.color(&words),
                                "mono" => palette.mono.mono(&words),
                                "uncolor" => palette.color.uncolor(&words),
                                _ => palette.mono.uncolor(&words),
                            };
                            match result {
                                Ok(()) => Ok(()),
                                // As other things mett doesn't do yet.
                                Err(crate::color::ColorError::Unsupported(what)) => {
                                    self.warn_once("color", &what, format!("{what}; ignored"));
                                    Ok(())
                                }
                                Err(e) => Err(e.to_string().replacen("uncolor", name.as_str(), 1)),
                            }
                        })
                    }
                }
                "alias" => self.alias(args),
                "unalias" => self.each(args, |c, key| {
                    if key == "*" {
                        c.aliases.clear();
                        c.alias_places.clear();
                    } else {
                        c.remove_alias(key);
                    }
                    Ok(())
                }),
                "mailboxes" => self.add_mailboxes(args),
                "unmailboxes" => self.each(args, |c, path| {
                    if path == "*" {
                        c.mailboxes.clear();
                    } else {
                        let path = c.expand_mailbox(path);
                        c.mailboxes.retain(|m| m.path != path);
                    }
                    Ok(())
                }),
                "sidebar_whitelist" => self.each(args, |c, path| {
                    let path = c.expand_mailbox(path);
                    if !c.sidebar_whitelist.contains(&path) {
                        c.sidebar_whitelist.push(path);
                    }
                    Ok(())
                }),
                "unsidebar_whitelist" => self.each(args, |c, path| {
                    if path == "*" {
                        c.sidebar_whitelist.clear();
                    } else {
                        let path = c.expand_mailbox(path);
                        c.sidebar_whitelist.retain(|m| *m != path);
                    }
                    Ok(())
                }),
                "bind" => match args {
                    [menus, key, function] => self.bind(menus, key, function),
                    _ => Err("bind: expected a menu, a key and a function".to_owned()),
                },
                "macro" => match args {
                    [menus, key, sequence, rest @ ..] if rest.len() <= 1 => {
                        self.macro_(menus, key, sequence, rest.first())
                    }
                    _ => Err(
                        "macro: expected a menu, a key, the keys to play and maybe a description"
                            .to_owned(),
                    ),
                },
                "push" => match args {
                    [keys] => self.expand(keys).map(|keys| self.pushed.push(keys)),
                    _ => Err("push: expected one string".to_owned()),
                },
                "exec" if !args.is_empty() => self.each(args, |c, function| {
                    c.pushed.push(format!("<{function}>"));
                    Ok(())
                }),
                "exec" => Err("exec: expected a function".to_owned()),
                // The accounts are opened when mett starts: one made at the
                // prompt (which has no file) would open nothing.
                "account" if origin.file.as_os_str().is_empty() => Err(
                    "account: can only be used in a mettrc, which is read when mett starts"
                        .to_owned(),
                ),
                "account" => match args {
                    [label, path] => self.expand(label).and_then(|label| {
                        let path = self.expand(path)?;
                        let path = self.resolve_source(&path, origin.file);
                        // This line's warnings are this line's, not the
                        // account file's.
                        self.flush_warnings(&at);
                        self.add_account(label, &path, origin.depth + 1)
                    }),
                    _ => Err("account: expected a label and a file name".to_owned()),
                },
                "source" => match args {
                    [path] => self.expand(path).map(|path| {
                        let path = self.resolve_source(&path, origin.file);
                        // This line's warnings are this line's, not the
                        // sourced file's.
                        self.flush_warnings(&at);
                        self.source(&path, origin.depth + 1);
                    }),
                    _ => Err("source: expected one file name".to_owned()),
                },
                // Ignored commands are not expanded, so their backticks
                // never run.
                other if NOT_YET_SUPPORTED.contains(&other) => {
                    self.warn_once(
                        "command",
                        other,
                        format!("{other} is not supported yet; ignored"),
                    );
                    Ok(())
                }
                other => {
                    self.warn_once(
                        "command",
                        other,
                        format!("unknown command {other}; ignored"),
                    );
                    Ok(())
                }
            },
        };
        if let Err(message) = result {
            self.errors.push(format!("{at}: {message}"));
        }
        self.flush_warnings(&at);
    }

    /// Reports the warnings of the command at `at`, those about the old
    /// names its expansions used too.
    fn flush_warnings(&mut self, at: &str) {
        for old in self.expanded_renamed.take() {
            // Warns (once) as using the old name does.
            let _ = self.find(old);
        }
        let pending = std::mem::take(&mut self.pending);
        self.warnings
            .extend(pending.into_iter().map(|w| format!("{at}: {w}")));
    }

    /// The expanded patterns of `alternates`-like commands, leaving out
    /// `-group NAME` (mett has no address groups yet).
    fn patterns(&self, args: &[Word]) -> Result<Vec<String>, String> {
        let mut patterns = Vec::new();
        let mut words = args.iter();
        while let Some(word) = words.next() {
            let word = self.expand(word)?;
            if word == "-group" {
                words.next();
            } else {
                patterns.push(word);
            }
        }
        Ok(patterns)
    }

    /// `alias [-group NAME ...] KEY ADDRESS[, ADDRESS ...]`: the addresses
    /// may span several words. An alias given again takes its new
    /// addresses (mett has no address groups yet).
    fn alias(&mut self, args: &[Word]) -> Result<(), String> {
        let mut words = args.iter();
        let mut key = None;
        while let Some(word) = words.next() {
            let word = self.expand(word)?;
            if word == "-group" {
                words.next();
                continue;
            }
            key = Some(word);
            break;
        }
        let key = key.ok_or("alias: expected a name and addresses")?;
        if !crate::alias::valid_key(&key) {
            return Err(format!("alias: {key:?} is not a name"));
        }
        // The quotes stay, as mutt keeps them: `"Doe, Jane" <jane@x.org>`
        // is one address.
        let value = words
            .map(|word| word.expand_kept(&Expand { config: self }))
            .collect::<Result<Vec<_>, _>>()?
            .join(" ");
        if value.trim().is_empty() {
            return Err(format!("alias {key}: expected addresses"));
        }
        self.remove_alias(&key);
        self.alias_places
            .insert(crate::alias::folded_key(&key), self.aliases.len());
        self.aliases.push(crate::alias::Alias { key, value });
        Ok(())
    }

    /// Forgets the alias called `key`, ignoring case, if there is one.
    fn remove_alias(&mut self, key: &str) {
        let Some(place) = self.alias_places.remove(&crate::alias::folded_key(key)) else {
            return;
        };
        self.aliases.remove(place);
        for later in self.alias_places.values_mut().filter(|p| **p > place) {
            *later -= 1;
        }
    }

    /// `mailboxes [-label NAME | -nolabel] [-notify | -nonotify] [-poll |
    /// -nopoll] MAILBOX ...`: the options go with the next mailbox. Adding
    /// a mailbox again changes its options, but keeps its place.
    fn add_mailboxes(&mut self, args: &[Word]) -> Result<(), String> {
        let mut words = args.iter();
        let (mut label, mut notify, mut poll) = (None, None, None);
        while let Some(word) = words.next() {
            let word = self.expand(word)?;
            match word.as_str() {
                "-label" => {
                    let name = words.next().ok_or("mailboxes: -label needs a name")?;
                    label = Some(Some(self.expand(name)?));
                }
                "-nolabel" => label = Some(None),
                "-notify" => notify = Some(true),
                "-nonotify" => notify = Some(false),
                "-poll" => poll = Some(true),
                "-nopoll" => poll = Some(false),
                _ => {
                    let path = self.expand_mailbox(&word);
                    let mailbox = match self.mailboxes.iter_mut().find(|m| m.path == path) {
                        Some(mailbox) => mailbox,
                        None => {
                            self.mailboxes.push(Mailbox {
                                path,
                                label: None,
                                notify: true,
                                poll: true,
                            });
                            self.mailboxes.last_mut().expect("just added")
                        }
                    };
                    if let Some(label) = label.take() {
                        mailbox.label = label;
                    }
                    if let Some(notify) = notify.take() {
                        mailbox.notify = notify;
                    }
                    if let Some(poll) = poll.take() {
                        mailbox.poll = poll;
                    }
                }
            }
        }
        if label.is_some() || notify.is_some() || poll.is_some() {
            return Err("mailboxes: options must come before a mailbox".into());
        }
        Ok(())
    }

    /// A mailbox path with mutt's `=`, `+`, `!`, `>` and `<` shortcuts
    /// expanded with the current settings.
    fn expand_mailbox(&self, path: &str) -> String {
        let path = match path.trim() {
            "!" => self.string("spoolfile"),
            ">" => self.string("mbox"),
            "<" => self.string("record"),
            _ => path.trim(),
        };
        // A password in a mailbox's URL is no use to a mailbox of the
        // account (only its server and user are looked at), and the path is
        // shown: it is kept without.
        server::strip_password(&mailbox::expand_shortcut(path, self.string("folder")))
    }

    fn bind(&mut self, menus: &Word, key: &Word, function: &Word) -> Result<(), String> {
        let command = KeyCommand::Bind {
            menus: self.expand(menus)?,
            key: self.expand(key)?,
            function: self.expand(function)?,
        };
        self.key_commands.push(command);
        Ok(())
    }

    fn macro_(
        &mut self,
        menus: &Word,
        key: &Word,
        sequence: &Word,
        description: Option<&Word>,
    ) -> Result<(), String> {
        let command = KeyCommand::Macro {
            menus: self.expand(menus)?,
            key: self.expand(key)?,
            sequence: self.expand(sequence)?,
            description: description.map(|d| self.expand(d)).transpose()?,
        };
        self.key_commands.push(command);
        Ok(())
    }

    /// Expands a word now, with the values set so far.
    fn expand(&self, word: &Word) -> Result<String, String> {
        word.expand(&Expand { config: self })
    }

    /// Applies `f` to each (expanded) name, collecting the first error.
    fn each(
        &mut self,
        names: &[Word],
        mut f: impl FnMut(&mut Self, &str) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut result = Ok(());
        for name in names {
            let outcome = self.expand(name).and_then(|name| f(self, &name));
            result = result.and(outcome);
        }
        result
    }

    /// `set` with mutt's forms: `var`, `novar`, `invvar`, `&var`, `?var`,
    /// `var=value`, `var= value` and `var = value`, several per command.
    fn set(&mut self, args: &[Word]) -> Result<(), String> {
        let mut args = args.iter().peekable();
        let mut result = Ok(());
        while let Some(arg) = args.next() {
            let is_equals = |w: &&Word| w.literal().as_deref() == Some("=");
            let outcome = if let Some((name, value)) = arg.split_assignment() {
                // `var=value`; for a bare `var=`, mutt takes the next word as
                // the value (so `set spoolfile= =@inbox` works).
                let value = value.or_else(|| args.next().cloned()).unwrap_or_default();
                self.assign(&name, &value)
            } else if args.peek().is_some_and(is_equals) {
                // `var = value`
                args.next();
                let value = args.next().cloned().unwrap_or_default();
                self.expand(arg).and_then(|name| self.assign(&name, &value))
            } else if let Some(value) = args.peek().and_then(|w| w.strip_leading_equals()) {
                // `var =value`
                args.next();
                self.expand(arg).and_then(|name| self.assign(&name, &value))
            } else {
                self.expand(arg).and_then(|arg| self.set_flag(&arg))
            };
            result = result.and(outcome);
        }
        result
    }

    /// `set` without a value: `var`, `novar`, `invvar`, `&var` or `?var`.
    fn set_flag(&mut self, arg: &str) -> Result<(), String> {
        if let Some(name) = arg.strip_prefix('&') {
            self.reset_named(name)
        } else if let Some(name) = arg.strip_prefix('?') {
            self.query(name)
        } else if let Some(name) = arg.strip_prefix("inv").filter(|n| is_bool(n)) {
            self.toggle(name)
        } else if let Some(name) = arg.strip_prefix("no").filter(|n| is_bool(n)) {
            self.unset(name)
        } else {
            match self.find(arg) {
                Some(var) if var.kind == Kind::Bool => {
                    self.store(var.name, Value::Bool(true));
                    Ok(())
                }
                Some(var) if var.kind == Kind::Quad => {
                    self.store(var.name, Value::Quad(Quad::Yes));
                    Ok(())
                }
                Some(var) => Err(format!("{}: needs a value", var.name)),
                None => self.unknown_variable(arg),
            }
        }
    }

    /// `set ?var`: notes the variable's value, as mutt shows it.
    fn query(&mut self, name: &str) -> Result<(), String> {
        let value = if name.starts_with("my_") {
            match self.user.get(name) {
                Some(value) => Value::Str(value.clone()),
                None => return Err(format!("{name}: unknown variable")),
            }
        } else {
            let Some(var) = self.find(name) else {
                return self.unknown_variable(name);
            };
            match self.values.get(var.name) {
                Some(value) => value.clone(),
                None => return Ok(()),
            }
        };
        self.answers.push(format!("{name}={value}"));
        Ok(())
    }

    /// Assigns `value` to `name`. The value is expanded only for variables
    /// mett uses, so settings it ignores never run their backticks.
    fn assign(&mut self, name: &str, value: &Word) -> Result<(), String> {
        if name.starts_with("my_") {
            let value = self.expand(value)?;
            self.user.insert(name.to_owned(), value);
            return Ok(());
        }
        let Some(var) = self.find(name) else {
            return self.unknown_variable(name);
        };
        let value = self.expand(value)?;
        let value = vars::parse(var, &value, self.home.as_deref())?;
        self.store(var.name, value);
        Ok(())
    }

    /// Sets the variable `name` to `value`, as the user did.
    fn store(&mut self, name: &'static str, value: Value) {
        self.values.insert(name, value);
        self.assigned.insert(name);
    }

    fn unset(&mut self, name: &str) -> Result<(), String> {
        if name.starts_with("my_") {
            self.user.remove(name);
            return Ok(());
        }
        let Some(var) = self.find(name) else {
            return self.unknown_variable(name);
        };
        let value = match var.kind {
            Kind::Bool => Value::Bool(false),
            Kind::Quad => Value::Quad(Quad::No),
            Kind::Number => Value::Number(0),
            Kind::String | Kind::Path => Value::Str(String::new()),
        };
        self.store(var.name, value);
        Ok(())
    }

    fn toggle(&mut self, name: &str) -> Result<(), String> {
        match self.find(name) {
            Some(var) if var.kind == Kind::Bool => {
                let current = self.values.get(var.name) == Some(&Value::Bool(true));
                self.store(var.name, Value::Bool(!current));
                Ok(())
            }
            Some(var) if var.kind == Kind::Quad => {
                let current = self.quad(var.name);
                self.store(var.name, Value::Quad(current.toggled()));
                Ok(())
            }
            Some(var) => Err(format!(
                "{}: only booleans and quad-options can be toggled",
                var.name
            )),
            None => self.unknown_variable(name),
        }
    }

    fn reset_named(&mut self, name: &str) -> Result<(), String> {
        if name == "all" {
            for var in VARS {
                self.reset(var);
            }
            self.user.clear();
            return Ok(());
        }
        if name.starts_with("my_") {
            self.user.remove(name);
            return Ok(());
        }
        match self.find(name) {
            Some(var) => {
                self.reset(var);
                Ok(())
            }
            None => self.unknown_variable(name),
        }
    }

    fn reset(&mut self, var: &'static vars::Var) {
        let value =
            vars::parse(var, var.default, self.home.as_deref()).expect("defaults are valid");
        self.values.insert(var.name, value);
        self.assigned.remove(var.name);
    }

    /// The known variable `name`, or the one a renamed variable's old name
    /// stands for now, with a warning (once) to use the new name.
    fn find(&mut self, name: &str) -> Option<&'static vars::Var> {
        if let Some(new) = vars::renamed(name) {
            self.warn_once(
                "variable",
                name,
                format!("{name} is now {new}; use that name"),
            );
            return vars::find(new);
        }
        vars::find(name)
    }

    /// Unknown variables are reported, but don't stop the configuration
    /// from loading (a shared muttrc sets many mett doesn't use).
    fn unknown_variable(&mut self, name: &str) -> Result<(), String> {
        self.warn_once(
            "variable",
            name,
            format!("unknown variable {name}; ignored"),
        );
        Ok(())
    }

    /// Queues `warning` about the `kind` named `item` (a command or a
    /// variable) unless it was already reported.
    fn warn_once(&mut self, kind: &str, item: &str, warning: String) {
        if self.warned.insert(format!("{kind} {item}")) {
            self.pending.push(warning);
        }
    }

    /// A `source` path: `~` is the home directory, and a relative path is
    /// relative to the file that sources it.
    fn resolve_source(&self, path: &str, from: &Path) -> PathBuf {
        let path = PathBuf::from(expand_home(path, self.home.as_deref()));
        if path.is_absolute() {
            path
        } else {
            from.parent().unwrap_or(Path::new(".")).join(path)
        }
    }
}

/// Whether `name` takes the `no` and `inv` prefixes: booleans and
/// quad-options do.
fn is_bool(name: &str) -> bool {
    vars::find(vars::renamed(name).unwrap_or(name))
        .is_some_and(|v| matches!(v.kind, Kind::Bool | Kind::Quad))
}

/// Expansion for the lexer: configuration variables first, then the
/// environment; backticks run through `sh -c`.
struct Expand<'a> {
    config: &'a Config,
}

impl Expander for Expand<'_> {
    fn variable(&self, name: &str) -> Option<String> {
        // `$msgraph_tenant` still stands for `office365_tenant` (with a
        // warning, once the command has run).
        let known = match vars::renamed_entry(name) {
            Some((old, new)) => {
                self.config.expanded_renamed.borrow_mut().push(old);
                new
            }
            None => name,
        };
        if let Some(value) = self.config.values.get(known) {
            return Some(match value {
                Value::Str(s) => s.clone(),
                other => other.to_string(),
            });
        }
        if let Some(value) = self.config.user.get(name) {
            return Some(value.clone());
        }
        // The home mett found, which on Windows may not be `$HOME`.
        if name == "HOME"
            && let Some(home) = self.config.home()
        {
            return Some(home.to_owned());
        }
        std::env::var(name).ok()
    }

    fn command(&self, command: &str) -> Result<String, String> {
        let output = crate::platform::sh(command, "A backtick command")?
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(format!("exited with {}", output.status));
        }
        String::from_utf8(output.stdout).map_err(|_| "output is not UTF-8".to_owned())
    }
}

/// Command-line choices about which files to read.
#[derive(Clone, Debug, Default)]
pub struct Files {
    /// `-F <file>`: read this instead of the user's mettrc.
    pub user_file: Option<PathBuf>,
    /// `-n`: skip the system-wide mettrc.
    pub no_system: bool,
}

/// Where a user without a mettrc gets one, when mett writes to it
/// (`alias_file`): `$XDG_CONFIG_HOME/mett/mettrc`, or
/// `~/.config/mett/mettrc`.
pub fn new_user_file(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            env("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .map(|c| c.join("mett/mettrc"))
}

/// The files to read, in order (`docs/CONFIG-OFFICE365.md` §1): the system
/// file, then the first user file found.
pub fn locate(files: &Files, env: &dyn Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let system = PathBuf::from("/etc/mettrc");
    if !files.no_system && system.is_file() {
        found.push(system);
    }
    if let Some(file) = &files.user_file {
        found.push(file.clone());
        return found;
    }
    let home = env("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    let candidates = [
        home.as_ref().map(|h| h.join(".mettrc")),
        home.map(|h| h.join(".mett/mettrc")),
        new_user_file(env),
    ];
    if let Some(user) = candidates.into_iter().flatten().find(|p| p.is_file()) {
        found.push(user);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(source: &str) -> Config {
        let mut config = Config::new(Some("/home/a".into()));
        config.load_str(source, Path::new("/conf/mettrc"));
        config
    }

    #[test]
    fn sets_variables_in_every_mutt_form() {
        let config = load(
            "set from=alice@contoso.com realname = \"Alice Example\"\n\
             set office365_page_size=25 nouse_from\n\
             set office365_focused_inbox; toggle office365_query_directory\n\
             set signature=~/.sig\n",
        );
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        assert_eq!(config.string("from"), "alice@contoso.com");
        assert_eq!(config.string("realname"), "Alice Example");
        assert_eq!(config.get("office365_page_size"), Some(&Value::Number(25)));
        assert_eq!(config.get("use_from"), Some(&Value::Bool(false)));
        assert_eq!(
            config.get("office365_focused_inbox"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            config.get("office365_query_directory"),
            Some(&Value::Bool(false))
        );
        assert_eq!(config.string("signature"), "/home/a/.sig");
    }

    #[test]
    fn accepts_spaces_on_either_side_of_equals() {
        let config = load("set from =a@b.c realname= Alice timeout = 7\n");
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        assert_eq!(config.string("from"), "a@b.c");
        assert_eq!(config.string("realname"), "Alice");
        assert_eq!(config.get("timeout"), Some(&Value::Number(7)));
    }

    #[test]
    fn reset_all_clears_user_variables() {
        let config = load("set my_account=a@b.c\nreset all\nset from=$my_account\n");
        assert_eq!(config.string("from"), "");
    }

    #[test]
    fn crlf_files_continue_lines() {
        let config = load("set from=a@b.c \\\r\n  realname=Alice\r\nset timeout=7\r\n");
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        assert_eq!(config.string("realname"), "Alice");
        assert_eq!(config.get("timeout"), Some(&Value::Number(7)));
    }

    #[test]
    fn unsets_and_resets() {
        let config = load(
            "set office365_cloud=china timeout=5\n\
             reset office365_cloud\nunset timeout\nset &use_from\n",
        );
        assert_eq!(config.string("office365_cloud"), "global");
        assert_eq!(config.get("timeout"), Some(&Value::Number(0)));
        assert_eq!(config.get("use_from"), Some(&Value::Bool(true)));
    }

    #[test]
    fn expands_configuration_and_user_variables() {
        let config = load(
            "set my_account=alice@contoso.com\n\
             set folder=msgraph://$my_account/\n\
             set spoolfile=\"${folder}@inbox\"\n",
        );
        assert_eq!(config.string("folder"), "msgraph://alice@contoso.com/");
        assert_eq!(
            config.string("spoolfile"),
            "msgraph://alice@contoso.com/@inbox"
        );
    }

    #[test]
    fn home_is_the_one_mett_found() {
        // `/home/a`, whatever `$HOME` is (on Windows, often unset).
        let config = load("set my_sig=$HOME/.sig\n");
        assert_eq!(config.user.get("my_sig").unwrap(), "/home/a/.sig");
    }

    #[test]
    fn runs_backticks() {
        let config = load("set office365_client_id=`printf 'abc-123'`\n");
        assert_eq!(config.string("office365_client_id"), "abc-123");
    }

    #[test]
    fn reports_bad_values_as_errors_and_unknowns_as_warnings() {
        let config =
            load("set timeout=soon\nset no_such_var=1\nscore ~N 10\nfrobnicate\nset from\n");
        assert_eq!(config.errors.len(), 2, "{:?}", config.errors);
        assert!(config.errors[0].starts_with("/conf/mettrc:1: timeout"));
        assert!(config.errors[1].contains("from: needs a value"));
        assert_eq!(config.warnings.len(), 3, "{:?}", config.warnings);
        assert!(
            config
                .warnings
                .iter()
                .any(|w| w.contains("score is not supported yet"))
        );
        assert!(
            config
                .warnings
                .iter()
                .any(|w| w.contains("unknown command frobnicate"))
        );
        assert!(
            config
                .warnings
                .iter()
                .any(|w| w == "/conf/mettrc:2: unknown variable no_such_var; ignored"),
            "{:?}",
            config.warnings
        );
    }

    #[test]
    fn command_names_are_never_expanded() {
        let marker = std::env::temp_dir().join(format!("mett-cmd-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let config = load(&format!("`touch {}`x a b\n", marker.display()));
        assert_eq!(config.errors.len(), 1, "{:?}", config.errors);
        assert!(!marker.exists(), "the command name was expanded");
    }

    #[test]
    fn reads_key_commands() {
        let mut config = load(
            "bind index,pager \\Cn next-entry\nmacro index A 's=Archive<enter>' 'archive it'\n\
             push '<last-entry>'\nexec first-entry next-entry\n",
        );
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        assert_eq!(
            config.take_key_commands(),
            [
                KeyCommand::Bind {
                    menus: "index,pager".into(),
                    // The lexer turns `\Cn` into Control-N, as mutt's does.
                    key: "\u{e}".into(),
                    function: "next-entry".into(),
                },
                KeyCommand::Macro {
                    menus: "index".into(),
                    key: "A".into(),
                    sequence: "s=Archive<enter>".into(),
                    description: Some("archive it".into()),
                },
            ]
        );
        assert!(config.take_key_commands().is_empty(), "taken");
        assert_eq!(
            config.take_pushed(),
            ["<last-entry>", "<first-entry>", "<next-entry>"]
        );
        assert_eq!(load("bind index x\n").errors.len(), 1);
        assert_eq!(load("macro index\n").errors.len(), 1);
    }

    #[test]
    fn runs_commands_typed_at_the_prompt() {
        let mut config = load("");
        assert_eq!(config.run_line("set timeout=7"), Ok(vec![]));
        assert_eq!(config.get("timeout"), Some(&Value::Number(7)));
        assert_eq!(
            config.run_line("set timeout=soon"),
            Err("timeout: expected a number, got \"soon\"".into())
        );
        assert!(config.errors.is_empty(), "prompt errors aren't kept");
        assert_eq!(
            config.run_line("frobnicate"),
            Ok(vec!["unknown command frobnicate; ignored".into()])
        );
        // `set ?var` shows the value.
        assert_eq!(
            config.run_line("set ?timeout"),
            Ok(vec!["timeout=7".into()])
        );
        assert_eq!(
            config.run_line("set ?resolve"),
            Ok(vec!["resolve=yes".into()])
        );
        assert_eq!(
            config.run_line("set my_x=hi; set ?my_x"),
            Ok(vec!["my_x=\"hi\"".into()])
        );
        // Ignored commands are reported each time they are typed.
        let first = config.run_line("score a b c").unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(config.run_line("score a b c"), Ok(first));
    }

    #[test]
    fn warns_about_each_ignored_item_once() {
        let config = load("score a b c\nscore d e f\nset imap_keepalive=x\nset imap_keepalive=y\n");
        assert_eq!(config.warnings.len(), 2, "{:?}", config.warnings);
        // A variable and a command with the same name are different items.
        let both = load("set score=1\nscore a b c\n");
        assert_eq!(both.warnings.len(), 2, "{:?}", both.warnings);
        assert!(config.warnings[0].starts_with("/conf/mettrc:1:"));
        assert!(config.warnings[1].starts_with("/conf/mettrc:3:"));
    }

    #[test]
    fn an_old_name_in_a_source_path_is_warned_about_where_it_is() {
        let dir = std::env::temp_dir().join(format!("mett-renamed-{}", std::process::id()));
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(
            dir.join("mettrc"),
            "set office365_tenant=sub\nsource $msgraph_tenant/rc\n",
        )
        .unwrap();
        fs::write(dir.join("sub/rc"), "set realname=Me\n").unwrap();
        let mut config = Config::new(None);
        config.load_file(&dir.join("mettrc"));
        assert_eq!(config.string("realname"), "Me");
        let at = format!("{}:2: ", dir.join("mettrc").display());
        assert_eq!(config.warnings.len(), 1, "{:?}", config.warnings);
        assert!(config.warnings[0].starts_with(&at), "{:?}", config.warnings);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sources_files_relative_to_the_sourcing_file() {
        let dir = std::env::temp_dir().join(format!("mett-config-{}", std::process::id()));
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("mettrc"), "source sub/accounts\nset realname=Me\n").unwrap();
        fs::write(dir.join("sub/accounts"), "set from=me@contoso.com\n").unwrap();
        // A loop is cut off rather than recursing forever.
        fs::write(dir.join("loop"), "source loop\n").unwrap();

        let mut config = Config::new(None);
        config.load_file(&dir.join("mettrc"));
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        assert_eq!(config.string("from"), "me@contoso.com");
        assert_eq!(config.string("realname"), "Me");

        let mut config = Config::new(None);
        config.load_file(&dir.join("loop"));
        assert!(config.errors.iter().any(|e| e.contains("too many nested")));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolves_the_spoolfile_relative_to_the_folder() {
        let config = load("set folder=msgraph://alice@contoso.com/ spoolfile==@inbox\n");
        assert_eq!(
            config.spoolfile().unwrap().to_string(),
            "msgraph://alice@contoso.com/@inbox"
        );
        assert!(load("").spoolfile().is_err());
        let local = load("set spoolfile=/var/mail/alice\n").spoolfile();
        assert!(matches!(local, Err(e) if e.0.contains("not a msgraph://")));
    }

    #[test]
    fn empty_values_and_bare_assignments_follow_mutt() {
        let config = load("set realname=\"\" office365_focused_inbox\nset spoolfile= =@inbox\n");
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        assert_eq!(config.string("realname"), "");
        assert_eq!(
            config.get("office365_focused_inbox"),
            Some(&Value::Bool(true))
        );
        assert_eq!(config.string("spoolfile"), "=@inbox");
    }

    #[test]
    fn ignored_settings_never_run_their_backticks() {
        let marker = std::env::temp_dir().join(format!("mett-ran-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let config = load(&format!(
            "set imap_keepalive=`touch {0}; false`\nscore x `touch {0}`\n",
            marker.display()
        ));
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        assert_eq!(config.warnings.len(), 2, "{:?}", config.warnings);
        assert!(!marker.exists(), "an ignored backtick ran");
    }

    #[test]
    fn old_names_of_renamed_variables_still_work_with_a_warning() {
        let mut config = load(
            "set msgraph_client_id=abc msgraph_tenant=contoso.com\n\
             set msgraph_client_id=def nomsgraph_query_directory\n\
             toggle msgraph_focused_inbox\n\
             set my_t=$msgraph_tenant\n",
        );
        assert_eq!(config.string("office365_client_id"), "def");
        assert_eq!(config.string("office365_tenant"), "contoso.com");
        assert!(!config.flag("office365_query_directory"));
        assert!(config.flag("office365_focused_inbox"));
        assert!(config.was_set("office365_client_id"));
        assert_eq!(
            config.run_line("set ?my_t").unwrap(),
            ["my_t=\"contoso.com\""]
        );
        // Once for each old name, saying the new one.
        let renamed: Vec<&String> = config
            .warnings
            .iter()
            .filter(|w| w.contains(" is now "))
            .collect();
        assert_eq!(renamed.len(), 4, "{:?}", config.warnings);
        assert!(
            renamed[0].contains("msgraph_client_id is now office365_client_id"),
            "{renamed:?}"
        );
        // The dump has the new names only.
        let dump = config.dump();
        assert!(
            dump.contains(&"set office365_client_id=\"def\"".to_owned()),
            "{dump:?}"
        );
        assert!(!dump.iter().any(|line| line.contains("msgraph_client_id")));
        // At the prompt too, and `reset` goes back to the default.
        let answer = config.run_line("reset msgraph_client_id").unwrap();
        assert!(answer[0].contains("office365_client_id"), "{answer:?}");
        assert_eq!(config.string("office365_client_id"), "");
        // An old name in an expansion warns too.
        let config = load("set my_c=$msgraph_cloud\n");
        assert_eq!(config.warnings.len(), 1, "{:?}", config.warnings);
        assert!(config.warnings[0].contains("msgraph_cloud is now office365_cloud"));
        // Graph's own settings keep their names.
        let config = load("set msgraph_scopes=Mail.Read\n");
        assert_eq!(config.string("msgraph_scopes"), "Mail.Read");
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }

    #[test]
    fn dumps_settings_as_set_commands() {
        let config = load("set realname=\"A \\\"B\\\"\" my_x=1 my_lit='$HOME `x`'\n");
        let dump = config.dump();
        assert!(
            dump.contains(&"set realname=\"A \\\"B\\\"\"".to_owned()),
            "{dump:?}"
        );
        assert!(dump.contains(&"set use_from=yes".to_owned()));
        assert!(dump.contains(&"set my_x=\"1\"".to_owned()));
        // The dump reads back to the same settings.
        let again = load(&dump.join("\n"));
        assert_eq!(again.dump(), dump);
    }

    #[test]
    fn colors_mett_does_not_do_yet_are_warnings() {
        let config = load(
            "color compose header red default\ncolor index red default ~T\ncolor status red blue\nmono status bold\n",
        );
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        assert_eq!(config.warnings.len(), 2, "{:?}", config.warnings);
        assert_ne!(config.colors().color, crate::color::Colors::default());
        assert_ne!(config.colors().mono, crate::color::Colors::default());
        assert!(load("color status mauve blue\n").errors[0].contains("mauve"));
        // A color by its RGB value is quoted: `#` starts a comment.
        let rgb = load("color status '#ff0000' default\n");
        assert!(rgb.errors.is_empty(), "{:?}", rgb.errors);
        assert_eq!(
            rgb.colors().color.status.fg,
            Some(ratatui::style::Color::Rgb(255, 0, 0))
        );
    }

    #[test]
    fn ignored_colors_never_run_their_backticks() {
        let marker = std::env::temp_dir().join(format!("mett-color-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let config = load(&format!(
            "color progress red `touch {}`\n",
            marker.display()
        ));
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        assert!(!marker.exists(), "an ignored backtick ran");
    }

    #[test]
    fn keeps_aliases() {
        let mut config = load(
            "alias bob Bob Smith <bob@x.org>\nalias -group g team bob, jane@x.org\nalias BOB \"Bob S\" <bob@y.org>\n",
        );
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        let aliases: Vec<_> = config
            .aliases()
            .iter()
            .map(|a| (a.key.as_str(), a.value.as_str()))
            .collect();
        assert_eq!(
            aliases,
            [
                ("team", "bob, jane@x.org"),
                ("BOB", "\"Bob S\" <bob@y.org>")
            ]
        );
        let again = load(&config.dump().join("\n"));
        assert_eq!(again.aliases(), config.aliases());
        assert!(config.run_line("alias bob@x.org x").is_err());
        assert!(config.run_line("alias foo;bar x@y").is_err());
        // The name's quotes stay before an address in a variable.
        config.run_line("set my_addr = '<dj@x.org>'").unwrap();
        config.run_line(r#"alias dj "Doe, J" $my_addr"#).unwrap();
        assert_eq!(
            crate::alias::find(config.aliases(), "dj").unwrap().value,
            r#""Doe, J" <dj@x.org>"#
        );
        config.run_line("unalias dj").unwrap();
        // Quoted, a variable's quotes stay inside.
        config.run_line(r#"set my_name = "Doe, \"J\"""#).unwrap();
        config.run_line(r#"alias dq "$my_name" <q@x.org>"#).unwrap();
        assert_eq!(
            crate::alias::find(config.aliases(), "dq").unwrap().value,
            r#""Doe, \"J\"" <q@x.org>"#
        );
        config.run_line("unalias dq").unwrap();
        // Quoted names keep their quotes; a quoted list is the list.
        config
            .run_line(r#"alias jane "Doe, Jane" <jane@x.org>, 'Al "B" @ Sales' <al@x.org>"#)
            .unwrap();
        // With its address in the same word, too.
        config
            .run_line(r#"alias carol "Jones, Carol"<carol@z.org>"#)
            .unwrap();
        let value = |key: &str| {
            crate::alias::find(config.aliases(), key)
                .unwrap()
                .value
                .clone()
        };
        assert_eq!(
            value("jane"),
            r#""Doe, Jane" <jane@x.org>, "Al \"B\" @ Sales" <al@x.org>"#
        );
        assert_eq!(value("carol"), r#""Jones, Carol"<carol@z.org>"#);
        config.run_line("alias pals bob, jane").unwrap();
        // What a mettrc would read otherwise is escaped when written.
        config
            .run_line(r#"alias odd "A\$B; #c" <a\$b@x.org>"#)
            .unwrap();
        assert_eq!(
            crate::alias::find(config.aliases(), "odd").unwrap().value,
            r#""A$B; #c" <a$b@x.org>"#
        );
        config.run_line(r#"alias ctl "a\nb" <c@d.org>"#).unwrap();
        // A dump reads back as it was.
        let again = load(&config.dump().join("\n"));
        assert_eq!(again.aliases(), config.aliases());
        assert!(config.run_line("alias lonely").is_err());
        config.run_line("unalias bob").unwrap();
        assert_eq!(config.aliases().len(), 6);
        config.run_line("unalias *").unwrap();
        assert!(config.aliases().is_empty());
    }

    #[test]
    fn aliases_given_again_go_last() {
        let mut config = load(
            "alias a a@x.org\nalias b b@x.org\nalias c c@x.org\nalias d d@x.org\n\
             alias B b2@x.org\nalias a a2@x.org\n",
        );
        let keys = |config: &Config| {
            config
                .aliases()
                .iter()
                .map(|a| format!("{}={}", a.key, a.value))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            keys(&config),
            ["c=c@x.org", "d=d@x.org", "B=b2@x.org", "a=a2@x.org"]
        );
        config.run_line("unalias C").unwrap();
        config.run_line("unalias nobody").unwrap();
        config.run_line("alias e e@x.org").unwrap();
        config.run_line("alias D d2@x.org").unwrap();
        assert_eq!(
            keys(&config),
            ["B=b2@x.org", "a=a2@x.org", "e=e@x.org", "D=d2@x.org"]
        );
        // Case is ignored in any script, as Rust lowers whole names: the
        // Kelvin sign is a `k`, and a final capital sigma a final `ς`.
        config.run_line("unalias *").unwrap();
        config.run_line("alias \u{212a}im k@x.org").unwrap();
        config.run_line("alias kim k2@x.org").unwrap();
        config.run_line("alias ΟΔΟΣ o@x.org").unwrap();
        config.run_line("alias οδος o2@x.org").unwrap();
        config.run_line("alias οδοσ o3@x.org").unwrap();
        assert_eq!(
            keys(&config),
            ["kim=k2@x.org", "οδος=o2@x.org", "οδοσ=o3@x.org"]
        );
        assert_eq!(
            crate::alias::find(config.aliases(), "KIM").unwrap().value,
            "k2@x.org"
        );
        assert_eq!(
            crate::alias::find(config.aliases(), "\u{212a}IM")
                .unwrap()
                .value,
            "k2@x.org"
        );
        assert_eq!(
            crate::alias::find(config.aliases(), "ΟΔΟΣ").unwrap().value,
            "o2@x.org"
        );
        config.run_line("unalias ΟΔΟΣ").unwrap();
        assert_eq!(keys(&config), ["kim=k2@x.org", "οδοσ=o3@x.org"]);
    }

    #[test]
    fn keeps_mailboxes_with_their_options() {
        let mut config = load(
            "set folder=msgraph://a@b.c/ spoolfile==@inbox\n\
             mailboxes ! -label Work -nonotify =Work =Work/Old\n\
             mailboxes -nopoll =Work +Lists\n\
             set folder=msgraph://a@b.c/Elsewhere\n\
             unmailboxes =Work/Old\n",
        );
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        let mailboxes: Vec<_> = config
            .mailboxes()
            .iter()
            .map(|m| (m.path.as_str(), m.label.as_deref(), m.notify, m.poll))
            .collect();
        // `=Work/Old` now means a folder below Elsewhere: it stays.
        assert_eq!(
            mailboxes,
            [
                ("msgraph://a@b.c/@inbox", None, true, true),
                ("msgraph://a@b.c/Work", Some("Work"), false, false),
                ("msgraph://a@b.c/Work/Old", None, true, true),
                ("msgraph://a@b.c/Lists", None, true, true),
            ]
        );
        // The dump reads back to the same mailboxes.
        let again = load(&config.dump().join("\n"));
        assert_eq!(again.mailboxes(), config.mailboxes());
        assert!(config.run_line("mailboxes -label x").is_err());
        assert_eq!(config.run_line("unmailboxes *"), Ok(vec![]));
        assert!(config.mailboxes().is_empty());
    }

    #[test]
    fn keeps_the_sidebar_whitelist() {
        let mut config = load(
            "set folder=msgraph://a@b.c/\n\
             sidebar_whitelist =Work =Lists =Work\n\
             unsidebar_whitelist =Lists\n",
        );
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        assert_eq!(config.sidebar_whitelist(), ["msgraph://a@b.c/Work"]);
        let again = load(&config.dump().join("\n"));
        assert_eq!(again.sidebar_whitelist(), config.sidebar_whitelist());
        assert_eq!(config.run_line("unsidebar_whitelist *"), Ok(vec![]));
        assert!(config.sidebar_whitelist().is_empty());
    }

    #[test]
    fn sets_unsets_and_toggles_quad_options() {
        let config = load("set include=ask-no\nset copy\nunset postpone\ntoggle reply_to\n");
        assert_eq!(config.quad("include"), Quad::AskNo);
        assert_eq!(config.quad("copy"), Quad::Yes);
        assert_eq!(config.quad("postpone"), Quad::No);
        assert_eq!(config.quad("reply_to"), Quad::AskNo);
        let config = load("set noabort_unmodified invabort_nosubject\n");
        assert_eq!(config.quad("abort_unmodified"), Quad::No);
        assert_eq!(config.quad("abort_nosubject"), Quad::AskNo);
        assert!(load("set include=maybe\n").errors.len() == 1);
    }

    #[test]
    fn a_new_user_file_goes_in_the_config_directory() {
        let env = |name: &str| (name == "HOME").then(|| OsString::from("/home/a"));
        assert_eq!(
            new_user_file(&env),
            Some(PathBuf::from("/home/a/.config/mett/mettrc"))
        );
        // An absolute path, on Windows too.
        let x = PathBuf::from(if cfg!(windows) { r"C:\x" } else { "/x" });
        let env = |name: &str| match name {
            "HOME" => Some(OsString::from("/home/a")),
            "XDG_CONFIG_HOME" => Some(x.clone().into_os_string()),
            _ => None,
        };
        assert_eq!(new_user_file(&env), Some(x.join("mett").join("mettrc")));
    }

    #[test]
    fn locates_the_first_user_file() {
        let dir = std::env::temp_dir().join(format!("mett-locate-{}", std::process::id()));
        fs::create_dir_all(dir.join(".mett")).unwrap();
        fs::create_dir_all(dir.join("xdg/mett")).unwrap();
        fs::write(dir.join(".mett/mettrc"), "").unwrap();
        fs::write(dir.join("xdg/mett/mettrc"), "").unwrap();
        let env = |name: &str| match name {
            "HOME" => Some(dir.clone().into_os_string()),
            "XDG_CONFIG_HOME" => Some(dir.join("xdg").into_os_string()),
            _ => None,
        };
        let files = Files {
            user_file: None,
            no_system: true,
        };
        assert_eq!(locate(&files, &env), [dir.join(".mett/mettrc")]);

        let files = Files {
            user_file: Some(PathBuf::from("/x/custom")),
            no_system: true,
        };
        assert_eq!(locate(&files, &env), [PathBuf::from("/x/custom")]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn accounts_are_configurations_of_their_own() {
        let dir = std::env::temp_dir().join(format!("mett-accounts-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("work"),
            "set from=me@work.example.com spoolfile=imaps://me@work.example.com/INBOX\nbind index X quit\nmailboxes =Projects\n",
        )
        .unwrap();
        fs::write(
            dir.join("home"),
            "set from=me@home.example.org spoolfile=imaps://me@home.example.org/INBOX\npush <last-entry>\n",
        )
        .unwrap();
        fs::write(
            dir.join("mettrc"),
            "set realname=Me sort=threads\naccount Work work\naccount Home home\nset realname=Changed\n",
        )
        .unwrap();

        let mut config = Config::new(None);
        config.load_file(&dir.join("mettrc"));
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        let accounts = config.accounts();
        assert_eq!(
            accounts
                .iter()
                .map(|a| a.label.as_str())
                .collect::<Vec<_>>(),
            ["Work", "Home"]
        );
        // Each has what was set before it, and its own.
        let (work, home) = (&accounts[0].config, &accounts[1].config);
        assert_eq!(work.string("realname"), "Me");
        assert_eq!(work.string("sort"), "threads");
        assert_eq!(work.string("from"), "me@work.example.com");
        assert_eq!(home.string("from"), "me@home.example.org");
        assert_eq!(work.mailboxes().len(), 1);
        assert!(home.mailboxes().is_empty());
        // What comes after isn't in them.
        assert_eq!(config.string("realname"), "Changed");
        assert_eq!(work.string("realname"), "Me");
        // The account's own startup keys stay with it; key bindings are shared.
        let mut home = accounts[1].config.clone();
        assert_eq!(home.take_pushed(), ["<last-entry>"]);
        assert!(config.take_pushed().is_empty());
        assert_eq!(config.take_key_commands().len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn accounts_cannot_be_made_at_the_prompt() {
        let mut config = Config::new(None);
        let error = config.run_line("account Work /tmp/work").unwrap_err();
        assert!(error.contains("only be used in a mettrc"), "{error}");
        assert!(config.accounts().is_empty());
    }

    #[test]
    fn accounts_report_their_errors_where_they_are() {
        let dir = std::env::temp_dir().join(format!("mett-accounts-bad-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("bad"),
            "set no_such_thing_at_all=1\nset sort=\"\\\n",
        )
        .unwrap();
        fs::write(dir.join("nested"), "account Inner bad\n").unwrap();
        fs::write(dir.join("ok"), "set from=me@x.org\n").unwrap();
        let mut config = Config::new(None);
        config.load_str(
            "account A ok\naccount A ok\naccount \"\" ok\naccount B nested\naccount C missing\naccount\n",
            &dir.join("mettrc"),
        );
        let errors = config.errors.join("\n");
        assert!(errors.contains("an account A already"), "{errors}");
        assert!(errors.contains("the label is empty"), "{errors}");
        assert!(errors.contains("not in an account's own file"), "{errors}");
        assert!(errors.contains("cannot read"), "{errors}");
        assert!(
            errors.contains("expected a label and a file name"),
            "{errors}"
        );
        // The one that worked is there, once.
        assert_eq!(
            config.accounts().iter().filter(|a| a.label == "A").count(),
            1
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn accounts_share_a_file_of_aliases_beside_the_mettrc() {
        let dir = std::env::temp_dir().join(format!("mett-aliases-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("one"),
            "set from=a@x.org\nalias bob Bob Own <bob@own.org>\n",
        )
        .unwrap();
        fs::write(dir.join("two"), "set from=b@x.org alias_file=/tmp/mine\n").unwrap();
        fs::write(
            dir.join("aliases"),
            "alias bob Bob <bob@x.org>\nalias carol Carol <carol@x.org>\n",
        )
        .unwrap();
        let mut config = Config::new(None);
        config.set_user_file(Some(dir.join("mettrc")));
        config.load_str("account One one\naccount Two two\n", &dir.join("mettrc"));
        config.share_aliases_with_accounts();
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        let accounts = config.accounts();
        let written = dir.join("aliases").display().to_string();
        assert_eq!(accounts[0].config.string("alias_file"), written);
        // Read, but not over what the account says itself.
        assert_eq!(accounts[0].config.aliases().len(), 2);
        assert_eq!(
            accounts[0].config.aliases()[0].value,
            "Bob Own <bob@own.org>"
        );
        assert_eq!(accounts[0].config.aliases()[1].key, "carol");
        // One that names its own file keeps it.
        assert_eq!(accounts[1].config.string("alias_file"), "/tmp/mine");
        assert!(accounts[1].config.aliases().is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_shared_aliases_see_the_mettrcs_variables_and_are_read_only_when_used() {
        let dir = std::env::temp_dir().join(format!("mett-aliases-vars-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("one"), "set from=a@x.org\n").unwrap();
        fs::write(dir.join("aliases"), "alias bob $my_addr\n").unwrap();
        // With an account that takes it, the variable is there.
        let mut config = Config::new(None);
        config.set_user_file(Some(dir.join("mettrc")));
        config.load_str(
            "set my_addr=bob@x.org\naccount One one\n",
            &dir.join("mettrc"),
        );
        config.share_aliases_with_accounts();
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        assert_eq!(config.accounts()[0].config.aliases()[0].value, "bob@x.org");
        // Without accounts, it isn't read: nothing asked for it.
        fs::write(dir.join("aliases"), "this is not a mettrc\n").unwrap();
        let mut single = Config::new(None);
        single.set_user_file(Some(dir.join("mettrc")));
        single.share_aliases_with_accounts();
        assert!(single.errors.is_empty() && single.warnings.is_empty());
        // Nor when every account has a file of its own.
        fs::write(dir.join("two"), "set alias_file=/tmp/mine\n").unwrap();
        let mut own = Config::new(None);
        own.set_user_file(Some(dir.join("mettrc")));
        own.load_str("account Two two\n", &dir.join("mettrc"));
        own.share_aliases_with_accounts();
        assert!(own.errors.is_empty() && own.warnings.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}
