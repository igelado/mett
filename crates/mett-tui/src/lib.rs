//! mett: a mutt-like mail client for Office365. The `mett` program runs
//! it in the terminal ([`terminal_main`]); `mett-gui`, with the `gui`
//! feature, in a window of its own, with the same screens.
//!
//! The main loop wires the pieces of `docs/DESIGN.md` together: key presses
//! become [`action::Action`]s through the [`Keymap`], [`App::update`] turns them and
//! worker responses into a new state plus [`Effect`]s, effects go to the
//! [`worker`], and [`view::render`] draws the state.

mod account;
mod action;
mod alias;
mod app;
mod color;
mod compose;
mod config;
mod demo;
mod editor;
mod expando;
mod format;
#[cfg(feature = "gui")]
mod gui;
mod imap;
mod keymap;
mod lineedit;
mod pattern;
mod platform;
mod signin;
mod sort;
mod text;
mod view;
mod worker;

use std::process::ExitCode;
use std::sync::Arc;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as TermEvent, KeyEventKind};
use tokio::sync::{mpsc, oneshot};

use crate::app::{App, Effect, Msg, Settings};
use crate::config::mailbox::FolderPath;
use crate::editor::Editor;
use crate::keymap::Keymap;
use crate::signin::{CancelHandle, SignInEvent, TuiPrompter};
use crate::worker::{Request, RequestId};

const USAGE: &str = "usage: mett [-n] [-F <file>] [-D] [--demo]

  -F <file>  read <file> instead of the user's mettrc
  -n         don't read the system-wide mettrc (/etc/mettrc)
  -D         print every setting after reading the configuration, and exit
  --demo     open a built-in sample mailbox
  -v         print the version

mett opens the mailbox set in spoolfile (see docs/CONFIG-OFFICE365.md).";

/// What the command line asks for.
#[derive(Debug, Default)]
struct Options {
    files: config::Files,
    dump: bool,
    demo: bool,
    version: bool,
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-F" => {
                let file = args.next().ok_or("-F needs a file name")?;
                options.files.user_file = Some(file.into());
            }
            "-n" => options.files.no_system = true,
            "-D" => options.dump = true,
            "--demo" => options.demo = true,
            "-v" | "--version" => options.version = true,
            other => return Err(format!("unknown option {other}")),
        }
    }
    if options.dump && options.demo {
        return Err("-D and --demo can't be used together".into());
    }
    Ok(options)
}

/// mett in the terminal: the `mett` program.
pub fn terminal_main() -> ExitCode {
    mett_main(InTerminal)
}

/// mett in a window of its own: the `mett-gui` program.
#[cfg(feature = "gui")]
pub fn gui_main() -> ExitCode {
    gui::main()
}

/// Where mett shows itself: the terminal, or the GUI's window.
trait Launch {
    /// Tells the user `message`, before mett starts or when it can't.
    fn say(&mut self, message: &str);

    /// Runs mett on `accounts`.
    fn start<S: mett_core::MailStore + mett_core::MailSender + 'static>(
        self,
        runtime: tokio::runtime::Runtime,
        accounts: Accounts<S>,
    ) -> ExitCode;
}

/// Says `error`, and fails.
fn fail(launch: &mut impl Launch, error: impl std::fmt::Display) -> ExitCode {
    launch.say(&error.to_string());
    ExitCode::FAILURE
}

/// Reads the command line and the configuration, and starts mett with
/// `launch`.
fn mett_main(mut launch: impl Launch) -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse_args(&args) {
        Ok(options) => options,
        Err(e) => {
            launch.say(&format!("{e}\n\n{USAGE}"));
            return ExitCode::from(2);
        }
    };
    if options.version {
        println!("mett {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    // The demo mailbox needs no configuration.
    if !options.demo {
        let home = platform::home();
        let mut config = config::Config::new(home.clone());
        let env = |name: &str| match name {
            "HOME" => home.clone().map(Into::into),
            _ => std::env::var_os(name),
        };
        let files = config::locate(&options.files, &env);
        // The user's own file is the one given (`-F`), or the last one
        // read (not /etc/mettrc), or the one they would have.
        let user_file = options
            .files
            .user_file
            .clone()
            .or_else(|| {
                files
                    .last()
                    .filter(|file| *file != std::path::Path::new("/etc/mettrc"))
                    .cloned()
            })
            .or_else(|| config::new_user_file(&env));
        config.set_user_file(user_file);
        for file in files {
            config.load_file(&file);
        }
        config.share_aliases_with_accounts();
        for warning in &config.warnings {
            launch.say(&format!("warning: {warning}"));
        }
        for error in &config.errors {
            launch.say(error);
        }
        if !config.errors.is_empty() {
            return ExitCode::FAILURE;
        }
        if options.dump {
            for line in config.dump() {
                println!("{line}");
            }
            // Each account's own settings, which are the ones it opens with.
            for account in config.accounts() {
                println!("# account {}", account.label);
                for line in account.config.dump() {
                    println!("{line}");
                }
            }
            return ExitCode::SUCCESS;
        }
        let (keymap, problems) = keymap_from_config(&mut config);
        for problem in problems {
            launch.say(&format!("warning: {problem}; ignored"));
        }
        let position = match config.string("tab_position").parse::<app::TabPosition>() {
            Ok(position) => position,
            Err(e) => return fail(&mut launch, e),
        };
        let runtime = match runtime() {
            Ok(runtime) => runtime,
            Err(e) => return fail(&mut launch, e),
        };
        // With `account` commands, each is a tab, with the configuration of
        // its own; without, the configuration is the one account's.
        let configured = config.take_accounts();
        let named: Vec<(String, config::Config)> = if configured.is_empty() {
            vec![(String::new(), config)]
        } else {
            configured
                .into_iter()
                .map(|account| (account.label, *account.config))
                .collect()
        };
        let several = named.len() > 1;
        let mut list = Vec::with_capacity(named.len());
        for (label, config) in named {
            match open_account(&mut launch, label.clone(), config, &keymap) {
                Ok(opened) => list.push(opened),
                Err(e) if several || !label.is_empty() => {
                    return fail(&mut launch, format!("account {label}: {e}"));
                }
                Err(e) => return fail(&mut launch, e),
            }
        }
        return launch.start(runtime, Accounts { list, position });
    }

    let runtime = match runtime() {
        Ok(runtime) => runtime,
        Err(e) => return fail(&mut launch, e),
    };
    // The demo mailbox never signs in, so nothing is ever sent here.
    let (_, sign_in) = mpsc::unbounded_channel();
    // The demo reads no mettrc, but `:` commands work on the defaults.
    let (config, settings) = demo_config(platform::home());
    let session = Session {
        folder: FolderPath::WellKnown(mett_core::WellKnownFolder::Inbox, Vec::new()),
        account: settings.account.clone(),
        settings,
        keymap: Keymap::mutt_defaults(),
        pushed: Vec::new(),
        config,
        editor: Editor::from_env("", ""),
        sign_in,
        syncing: mpsc::unbounded_channel().1,
        cancel: None,
        notice: None,
    };
    let account = Opened {
        label: String::new(),
        store: Arc::new(demo::mailbox()),
        session,
    };
    launch.start(
        runtime,
        Accounts {
            list: vec![account],
            position: app::TabPosition::default(),
        },
    )
}

/// The account of `config`, opened: its mailbox (which signs in when first
/// used, in the TUI's sign-in window), its cache and the app's settings.
fn open_account(
    launch: &mut impl Launch,
    label: String,
    mut config: config::Config,
    keymap: &Keymap,
) -> Result<Opened<mett_sqlite::CachedStore<account::Mailbox>>, String> {
    let spoolfile = config.spoolfile().map_err(|e| e.to_string())?;
    let settings = settings_from_config(&config, &spoolfile.account)?;
    let editor = Editor::from_env(config.string("editor"), config.string("tmpdir"));
    // Signing in happens in the TUI, in the sign-in window, when the
    // folder is first opened.
    let (events, sign_in) = mpsc::unbounded_channel();
    let (prompter, cancel) = TuiPrompter::new(spoolfile.account.clone(), events);
    let mailbox = account::mailbox(&config, &spoolfile, prompter)?;
    // The cache tells the app how far a folder's sync has got.
    let (syncs, syncing) = mpsc::unbounded_channel();
    let progress = Box::new(move |folder: &mett_core::FolderId, taken: u64| {
        let _ = syncs.send(Msg::Syncing {
            folder: folder.clone(),
            taken,
        });
    });
    let (mailbox, warning) = account::cached(&config, &spoolfile, mailbox, progress);
    if let Some(warning) = warning {
        launch.say(&format!("warning: {warning}"));
    }
    let mut pushed = Vec::new();
    for keys in config.take_pushed() {
        match keymap::parse_strokes(&keys) {
            Ok(strokes) => pushed.extend(strokes),
            Err(e) => launch.say(&format!("warning: push: {e}; ignored")),
        }
    }
    let label = if label.is_empty() {
        spoolfile.account.to_string()
    } else {
        label
    };
    Ok(Opened {
        label,
        store: Arc::new(mailbox),
        session: Session {
            folder: spoolfile.folder,
            settings,
            keymap: keymap.clone(),
            pushed,
            account: spoolfile.account,
            config,
            editor,
            sign_in,
            syncing,
            cancel: Some(cancel),
            notice: None,
        },
    })
}

/// The demo's configuration, and the settings it gives. Its folders are
/// the demo mailbox's own (`=Archive`, not `~/Mail/Archive`), and stay so
/// when `:` commands change the configuration.
fn demo_config(home: Option<String>) -> (config::Config, Settings) {
    let mut config = config::Config::new(home);
    let defaults = demo::settings();
    // The value is quoted; an empty one is always valid.
    let _ = config.run_line(&format!("set folder=\"{}\"", defaults.folder));
    // Sample mail goes out without the user's own signature.
    let _ = config.run_line("set signature=\"\"");
    // The demo user's own address, which group replies leave out.
    let account = mett_core::AccountId::new(demo::YOU);
    let settings = settings_from_config(&config, &account).unwrap_or(defaults);
    (config, settings)
}

/// What the TUI works with, besides the mailbox.
struct Session {
    /// The folder to open.
    folder: FolderPath,
    /// The app's settings.
    settings: Settings,
    /// The key bindings.
    keymap: Keymap,
    /// Keys to play first, from `push` and `exec`.
    pushed: Vec<keymap::Stroke>,
    /// The account, whose folders mailbox paths name.
    account: mett_core::AccountId,
    /// The configuration, which `:` commands change.
    config: config::Config,
    /// The editor for writing messages.
    editor: Editor,
    /// Events from the mailbox's sign-in.
    sign_in: mpsc::UnboundedReceiver<SignInEvent>,
    /// How far the cache's syncs have got.
    syncing: mpsc::UnboundedReceiver<Msg>,
    /// Cancels the sign-in in progress, for mailboxes that sign in.
    cancel: Option<CancelHandle>,
    /// What to say on the message line first: warnings from the
    /// configuration, where there is no terminal to say them in.
    notice: Option<String>,
}

/// The app's settings from the configuration, for `account`.
fn settings_from_config(
    config: &config::Config,
    account: &mett_core::AccountId,
) -> Result<Settings, String> {
    settings_keeping(config, account, None)
}

/// Like [`settings_from_config`], with `signature`, if given, instead of
/// reading the signature file again.
fn settings_keeping(
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
    let interval = match account::Api::from_config(config) {
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
    let folder = config::server::strip_password(config.string("folder"));
    let folder = folder.as_str();
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
        api: account::Api::from_config(config),
        display,
        ordering,
        sync_interval,
        compose,
        account: account.clone(),
        // The mailboxes are shown relative to it: without a password in it.
        folder: config::server::strip_password(config.string("folder")),
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
fn sidebar_settings(
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

/// mutt's default key bindings, changed by the configuration's `bind` and
/// `macro` commands, and the bindings that couldn't be made. Those don't
/// stop mett: a muttrc it sources may bind mutt menus and functions mett
/// doesn't have.
fn keymap_from_config(config: &mut config::Config) -> (Keymap, Vec<String>) {
    let mut keymap = Keymap::mutt_defaults();
    let problems = config
        .take_key_commands()
        .iter()
        .filter_map(|command| keymap.apply(command).err())
        .collect();
    (keymap, problems)
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Runtime::new().map_err(|e| format!("cannot start: {e}"))
}

/// mett in the terminal.
struct InTerminal;

impl Launch for InTerminal {
    fn say(&mut self, message: &str) {
        eprintln!("mett: {message}");
    }

    fn start<S: mett_core::MailStore + mett_core::MailSender + 'static>(
        mut self,
        runtime: tokio::runtime::Runtime,
        accounts: Accounts<S>,
    ) -> ExitCode {
        remove_viewed_before();
        // `init` also restores the terminal if mett panics.
        let terminal = ratatui::init();
        // Drawing writes only the cells that changed, so what the terminal
        // showed before goes first: without an alternate screen (GNU screen
        // without `altscreen`), the shell's output would show through.
        let result = clear_screen().and_then(|()| {
            let (input, events) = Input::start();
            let screen = TerminalScreen { terminal, input };
            runtime.block_on(run(screen, events, accounts))
        });
        ratatui::restore();
        wait_for_saves();
        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(&mut self, e),
        }
    }
}

/// What the app is drawn on, and what it can't do without: the terminal,
/// or the GUI's window.
trait Frontend {
    /// The screen's size, in cells.
    fn size(&self) -> std::io::Result<ratatui::layout::Size>;

    /// Draws the app.
    fn draw(&mut self, app: &App) -> std::io::Result<()>;

    /// Lets the user edit `text` in `editor`, their own, with the input
    /// (`events`) handed over to it meanwhile.
    fn edit(
        &mut self,
        editor: &Editor,
        events: &mut InputEvents,
        text: &str,
    ) -> std::io::Result<Result<String, String>>;

    /// Puts `text` on the clipboard.
    fn copy(&mut self, text: &str) -> std::io::Result<()>;
}

/// The terminal, with the thread reading its input.
struct TerminalScreen {
    terminal: DefaultTerminal,
    input: Input,
}

impl Frontend for TerminalScreen {
    fn size(&self) -> std::io::Result<ratatui::layout::Size> {
        self.terminal.size()
    }

    fn draw(&mut self, app: &App) -> std::io::Result<()> {
        draw(&mut self.terminal, app)
    }

    fn edit(
        &mut self,
        editor: &Editor,
        events: &mut InputEvents,
        text: &str,
    ) -> std::io::Result<Result<String, String>> {
        self.input
            .suspended(&mut self.terminal, events, || editor.edit(text))
    }

    fn copy(&mut self, text: &str) -> std::io::Result<()> {
        copy_to_clipboard(text)
    }
}

/// What the user did: something in the terminal (the GUI's window makes
/// the same events), or closing the window.
pub(crate) enum UserEvent {
    /// A key press, a resize, ...
    Term(TermEvent),
    /// The window was closed.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    Close,
    /// The user asked to paste from the clipboard (in the window).
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    Paste,
}

/// An account ready to run: what its tab shows, its mailbox and what the
/// app needs to start.
struct Opened<S> {
    /// What the tab says.
    label: String,
    /// The mailbox.
    store: Arc<S>,
    /// The rest.
    session: Session,
}

/// The accounts to run: one, or several, each in a tab.
struct Accounts<S> {
    /// The accounts, in the order of their tabs.
    list: Vec<Opened<S>>,
    /// Where the tabs are drawn, if there are several.
    position: app::TabPosition,
}

/// What an account's app waits on, besides what the user types.
#[allow(clippy::large_enum_variant)]
enum Incoming {
    /// A message for the app.
    Msg(Msg),
    /// A `:` command finished running, with the configuration it leaves.
    Configured(Result<(Configuration, app::Reconfigured), oneshot::error::RecvError>),
}

/// One account while mett runs: its app, and the worker, the timers and the
/// files it uses.
struct Slot {
    label: String,
    app: App,
    requests: mpsc::UnboundedSender<(RequestId, Request)>,
    timers: mpsc::UnboundedSender<app::Timer>,
    files: mpsc::UnboundedSender<Msg>,
    cancel: Option<CancelHandle>,
    account: mett_core::AccountId,
    editor: Editor,
    commands: Commands,
    /// Whether a `:` command is running.
    running: bool,
    /// What effects done at once left for the main loop.
    deferred: Deferred,
}

impl Slot {
    /// Carries out `effects`.
    fn carry_out(&mut self, effects: Vec<Effect>) -> Deferred {
        dispatch(
            &mut self.app,
            &self.requests,
            &self.timers,
            &self.files,
            self.cancel.as_ref(),
            effects,
        )
    }

    /// Puts what `effects` did at once in `deferred`.
    fn absorb(&mut self, effects: Vec<Effect>) {
        let done = self.carry_out(effects);
        self.deferred.absorb(done);
    }
}

/// Sends the app of account `id` what its sign-in, worker, cache, timers and
/// files have to say, in that order of importance (a failed save's sign-in
/// failure comes before the save's own response).
#[allow(clippy::too_many_arguments)]
fn forward(
    id: usize,
    mut sign_in: mpsc::UnboundedReceiver<SignInEvent>,
    mut responses: mpsc::UnboundedReceiver<worker::Response>,
    mut syncing: mpsc::UnboundedReceiver<Msg>,
    mut timers: mpsc::UnboundedReceiver<app::Timer>,
    mut files: mpsc::UnboundedReceiver<Msg>,
    events: mpsc::UnboundedSender<(usize, Incoming)>,
) {
    tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                biased;
                Some(event) = sign_in.recv() => Msg::SignIn(event),
                Some(response) = responses.recv() => Msg::Response(response),
                Some(msg) = syncing.recv() => msg,
                Some(timer) = timers.recv() => Msg::Timer(timer),
                Some(msg) = files.recv() => msg,
                else => break,
            };
            if events.send((id, Incoming::Msg(msg))).is_err() {
                break;
            }
        }
    });
}

/// The rows an app has, of a screen of `height`: with tabs, one is theirs.
fn rows(height: u16, tabs: bool) -> u16 {
    height.saturating_sub(u16::from(tabs))
}

/// Gives every account's app the tabs, as the account `active` shows them.
fn show_tabs(slots: &mut [Slot], active: usize, position: app::TabPosition) {
    if slots.len() < 2 {
        return;
    }
    // What is shown is looked at.
    slots[active].app.arrived = false;
    let tabs: Vec<app::Tab> = slots
        .iter()
        .map(|slot| app::Tab {
            label: slot.label.clone(),
            new_mail: slot.app.mailboxes_with_new_mail() > 0 || slot.app.arrived,
        })
        .collect();
    for slot in slots.iter_mut() {
        slot.app.tabs = Some(app::Tabs {
            tabs: tabs.clone(),
            active,
            position,
        });
    }
}

/// Runs the accounts, each with an app of its own, and shows the one that is
/// current on `screen`, with input from `input_rx`.
async fn run<S: mett_core::MailStore + mett_core::MailSender + 'static>(
    mut screen: impl Frontend,
    mut input_rx: InputEvents,
    accounts: Accounts<S>,
) -> std::io::Result<()> {
    let Accounts { list, position } = accounts;
    let tabbed = list.len() > 1;
    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<(usize, Incoming)>();
    let size = screen.size()?;
    let mut slots: Vec<Slot> = Vec::with_capacity(list.len());
    for (id, opened) in list.into_iter().enumerate() {
        let Opened {
            label,
            store,
            session,
        } = opened;
        let Session {
            folder,
            settings,
            keymap,
            pushed,
            account,
            config,
            editor,
            sign_in: sign_in_rx,
            syncing: syncing_rx,
            cancel,
            notice,
        } = session;

        let (request_tx, request_rx) = mpsc::unbounded_channel::<(RequestId, Request)>();
        let (response_tx, response_rx) = mpsc::unbounded_channel();
        tokio::spawn(worker::run(store, request_rx, response_tx));
        let (timer_tx, timer_rx) = mpsc::unbounded_channel();
        // What reading, saving and opening files (on threads of their own)
        // has to say.
        let (file_tx, file_rx) = mpsc::unbounded_channel();
        forward(
            id,
            sign_in_rx,
            response_rx,
            syncing_rx,
            timer_rx,
            file_rx,
            events_tx.clone(),
        );

        // `:` commands run on their own thread, one at a time: `source` and
        // backticks may take a while, and the UI doesn't wait for them.
        let commands = Commands::new(Configuration {
            // Kept for `:` commands that don't change it.
            signature: settings.compose.signature.clone(),
            config,
        });
        let (mut app, effects) = App::open(size.width, rows(size.height, tabbed), folder, settings);
        app.keymap = keymap;
        if let Some(notice) = notice {
            app.notice(notice);
        }
        // Played once the folder is listed, so that `push <last-entry>` has
        // somewhere to go.
        app.push_after_load(pushed);
        let mut slot = Slot {
            label,
            app,
            requests: request_tx,
            timers: timer_tx,
            files: file_tx,
            cancel,
            account,
            editor,
            commands,
            running: false,
            deferred: Deferred::default(),
        };
        slot.absorb(effects);
        slots.push(slot);
    }

    // The account shown; the others go on in the background, checking for
    // mail and saving what the user changed.
    let mut active = 0;
    // Whether the screen may differ from what was drawn last: it is drawn
    // only before waiting, once everything ready has been taken in.
    let mut stale = true;
    // The accounts that have said they are done (quit), and those asked
    // about their deleted messages first.
    let mut done = vec![false; slots.len()];
    let mut asked = vec![false; slots.len()];
    'run: loop {
        // Quitting in any account quits mett, as it would in one alone: it
        // may be one that isn't shown (a `push <quit>`, or a purge that was
        // being done when the user went to another tab).
        for (at, slot) in slots.iter().enumerate() {
            done[at] |= slot.app.quit;
        }
        if done.iter().any(|done| *done) {
            // What is unfinished in another account (messages marked deleted,
            // a message being written) is the user's to decide on, as it
            // would be quitting that account alone: it is shown, and asked,
            // once.
            let unfinished = |slot: &Slot| slot.app.has_deleted() || slot.app.is_composing();
            let to_ask =
                (0..slots.len()).find(|at| !done[*at] && !asked[*at] && unfinished(&slots[*at]));
            match to_ask {
                Some(other) => {
                    asked[other] = true;
                    active = other;
                    stale = true;
                    // The purge is asked by quitting; a message being written
                    // is shown, for the user to send, postpone or abandon. An
                    // account that needs a sign-in first has its window
                    // shown: the user deals with it, and quits again.
                    if slots[other].app.sign_in.is_none() && slots[other].app.has_deleted() {
                        let effects = slots[other].app.update(Msg::Action(action::Action::Quit));
                        slots[other].absorb(effects);
                    }
                }
                // Nothing is left to ask: mett quits. (An account asked and
                // not answered keeps it going: the user may have said no.)
                None if (0..slots.len()).all(|at| done[at] || !unfinished(&slots[at])) => {
                    break 'run;
                }
                None => {}
            }
        }
        for at in 0..slots.len() {
            // What effects done at once answered goes to the app first.
            while !slots[at].deferred.msgs.is_empty() {
                for msg in std::mem::take(&mut slots[at].deferred.msgs) {
                    let slot = &mut slots[at];
                    let effects = slot.app.update(msg);
                    slot.absorb(effects);
                    stale = true;
                }
            }
            if !slots[at].deferred.copies.is_empty() {
                let copies = std::mem::take(&mut slots[at].deferred.copies);
                copy_all(&mut screen, &mut slots[at].app, copies);
                stale = true;
            }
            // The editor runs between messages, with the terminal handed
            // over.
            let slot = &mut slots[at];
            let commands: Vec<String> = slot.deferred.commands.drain(..).collect();
            slot.commands.queue.extend(commands);
            // The editor takes over the terminal: only for the account
            // shown (another's waits until it is).
            while at == active
                && let Some(text) = slots[at].deferred.edit.take()
            {
                let result = screen.edit(&slots[at].editor, &mut input_rx, &text)?;
                // The terminal was cleared: all of it is drawn again.
                stale = true;
                // The terminal may have been resized meanwhile.
                let size = screen.size()?;
                for slot in &mut slots {
                    slot.app.update(Msg::Resize {
                        width: size.width,
                        height: rows(size.height, tabbed),
                    });
                }
                let slot = &mut slots[at];
                let effects = slot.app.update(Msg::Edited(result));
                slot.absorb(effects);
                let commands: Vec<String> = slot.deferred.commands.drain(..).collect();
                slot.commands.queue.extend(commands);
            }
            if let Some(switch) = slots[at].deferred.switch.take()
                && at == active
                && let Some(to) = switch.target(active, slots.len())
            {
                // The keys a macro has still to play go on in the account
                // gone to.
                let strokes = slots[active].app.take_strokes();
                slots[to].app.give_strokes(strokes);
                active = to;
                stale = true;
            }
            if !slots[at].running
                && let Some((command, mut configuration)) = slots[at].commands.start()
            {
                let account = slots[at].account.clone();
                let (done, result) = oneshot::channel();
                slots[at].running = true;
                let events = events_tx.clone();
                tokio::spawn(async move {
                    let _ = events.send((at, Incoming::Configured(result.await)));
                });
                // A thread of its own, not the runtime's blocking pool:
                // quitting doesn't wait for a command that takes its time.
                std::thread::spawn(move || {
                    let reconfigured = run_command(
                        &mut configuration.config,
                        &account,
                        &mut configuration.signature,
                        &command,
                    );
                    let _ = done.send((configuration, reconfigured));
                });
            }
        }
        if slots
            .iter()
            .zip(&done)
            .any(|(slot, done)| slot.app.quit && !done)
        {
            continue 'run;
        }
        // The keys a `push` in an account's own file queued play there even
        // if it isn't the one shown.
        for at in (0..slots.len()).filter(|at| *at != active) {
            while let Some(msg) = slots[at].app.next_stroke() {
                let slot = &mut slots[at];
                let effects = slot.app.update(msg);
                slot.absorb(effects);
                stale = true;
                // A `push <quit>` there quits mett, as it would in any
                // account (see above).
                if slots[at].app.quit && !done[at] {
                    continue 'run;
                }
            }
        }
        show_tabs(&mut slots, active, position);
        // Keys a macro or `push` queued come before new input, once the
        // last action has what it was waiting for. They are played without
        // drawing the screen between them.
        let stroke = slots[active].app.next_stroke();
        stale |= slots[active].app.take_redraw();
        let (at, received) = match stroke {
            Some(msg) => (active, Received::Msg(msg)),
            None => {
                let mut next = std::pin::pin!(async {
                    tokio::select! {
                        event = input_rx.recv() => (active, Received::Input(event)),
                        Some((at, incoming)) = events_rx.recv() => (at, match incoming {
                            Incoming::Msg(msg) => Received::Msg(msg),
                            Incoming::Configured(done) => Received::Configured(done),
                        }),
                        else => (active, Received::Closed),
                    }
                });
                // What is ready already is taken in first; the screen is
                // drawn only before waiting for more.
                match ready(next.as_mut()).await {
                    Some(received) => received,
                    None => {
                        if std::mem::take(&mut stale) {
                            slots[active].app.before_draw();
                            screen.draw(&slots[active].app)?;
                        }
                        next.await
                    }
                }
            }
        };
        let msg = match received {
            Received::Msg(msg) => msg,
            // Without input mett can't be used or quit, so a terminal read
            // error ends it (and the terminal is restored).
            Received::Input(event) => match event.transpose()? {
                None => return Err(std::io::Error::other("terminal input closed")),
                Some(event) => match event {
                    // Repeats too, so holding a key keeps moving.
                    UserEvent::Term(TermEvent::Key(key)) if key.kind != KeyEventKind::Release => {
                        slots[active].app.typed();
                        Msg::Key(key)
                    }
                    UserEvent::Term(TermEvent::Resize(width, height)) => {
                        // Every account's app has the new size.
                        for slot in &mut slots {
                            slot.app.update(Msg::Resize {
                                width,
                                height: rows(height, tabbed),
                            });
                        }
                        stale = true;
                        continue;
                    }
                    UserEvent::Term(TermEvent::Paste(text)) => {
                        slots[active].app.typed();
                        Msg::Paste(text)
                    }
                    UserEvent::Close => Msg::Close,
                    UserEvent::Paste => {
                        slots[active].app.typed();
                        Msg::PasteClipboard
                    }
                    // Nothing changed: nothing to draw again.
                    UserEvent::Term(_) => continue,
                },
            },
            Received::Configured(done) => {
                let slot = &mut slots[at];
                slot.running = false;
                let (configuration, reconfigured) =
                    done.map_err(|_| std::io::Error::other("a : command stopped"))?;
                // `editor` and `tmpdir` may have changed.
                let config = &configuration.config;
                slot.editor = Editor::from_env(config.string("editor"), config.string("tmpdir"));
                slot.commands.finish(configuration);
                Msg::Configured(reconfigured)
            }
            Received::Closed => break,
        };
        let slot = &mut slots[at];
        let effects = slot.app.update(msg);
        slot.absorb(effects);
        stale = true;
    }

    finish(
        &mut screen,
        &mut input_rx,
        &mut events_rx,
        &mut slots,
        active,
        position,
    )
    .await
}

/// What a main loop that is quitting takes in of what an account says, and
/// how much longer to wait for it, if it should be: only what concerns
/// saving matters. Anything else (say, a late message fetch) could start new
/// work that would never be sent.
fn when_quitting(slot: &Slot, incoming: Incoming) -> Option<(Msg, Option<std::time::Duration>)> {
    let Incoming::Msg(msg) = incoming else {
        return None;
    };
    match msg {
        Msg::Response(response) => match &response.event {
            worker::Event::Throttled { wait, .. } if slot.app.saves(response.id) => {
                let wait = *wait + SAVE_TIMEOUT;
                Some((Msg::Response(response), Some(wait)))
            }
            worker::Event::FlagsUpdated(_)
            | worker::Event::Sent(..)
            | worker::Event::DraftSaved(..)
            | worker::Event::KeptDiscarded(_)
            | worker::Event::Transferred(_)
            | worker::Event::Purged(_)
            | worker::Event::Watched(_) => Some((Msg::Response(response), None)),
            _ => None,
        },
        // A sign-in that nothing left to save needs: the user has quit, so
        // don't send them to the browser.
        Msg::SignIn(SignInEvent::Prompt { .. }) if !slot.app.saving() => {
            if let Some(cancel) = &slot.cancel {
                cancel.cancel();
            }
            None
        }
        Msg::SignIn(event) => Some((Msg::SignIn(event), None)),
        // The countdown of a throttled save goes on; other timers would
        // start new work.
        Msg::Timer(timer @ app::Timer::Throttled(_)) => Some((Msg::Timer(timer), None)),
        _ => None,
    }
}

/// What the main loop is waiting for, once the user has quit: the changes
/// not yet saved, and watching stopped on the server.
async fn finish(
    screen: &mut impl Frontend,
    input_rx: &mut InputEvents,
    events_rx: &mut mpsc::UnboundedReceiver<(usize, Incoming)>,
    slots: &mut [Slot],
    active: usize,
    position: app::TabPosition,
) -> std::io::Result<()> {
    // Give pending changes (such as marking a message read) a moment to
    // reach the mailbox, rather than cancelling them by exiting.
    // The outcome is reported after the terminal is restored, so a change
    // that couldn't be saved isn't lost silently.
    // A save's sign-in failure may still be queued, behind its response
    // and the key that quit: take it in, so its window can offer a retry.
    while let Ok((at, incoming)) = events_rx.try_recv() {
        let slot = &mut slots[at];
        // What was saved meanwhile is saved: its response is taken in, or
        // the app would wait for it, and call it a failure.
        let Some((msg, _)) = when_quitting(slot, incoming) else {
            continue;
        };
        let effects = slot.app.update(msg);
        slot.carry_out(effects);
    }
    // Watching stops on the server too, while changes are saved.
    for slot in slots.iter_mut() {
        let effects = slot.app.unwatch();
        slot.carry_out(effects);
    }
    // The account with a sign-in window to use shows it.
    let mut shown = active;
    let unsaved = |slot: &Slot| !slot.app.save_failures().is_empty();
    let waiting = |slots: &[Slot]| {
        slots
            .iter()
            .any(|s| s.app.saving() || (unsaved(s) && s.app.sign_in.is_some()))
    };
    // Whether the user is signing in for changes that wait for it.
    let signing_in = |slots: &[Slot]| {
        slots
            .iter()
            .any(|s| s.app.sign_in.is_some() && (s.app.saving() || unsaved(s)))
    };
    if waiting(slots) {
        for slot in slots.iter_mut() {
            if slot.app.saving() {
                slot.app.status = "Saving changes...".into();
            }
        }
        let mut stale = true;
        // A save the server throttled before the user quit waits that much
        // longer.
        let wait = |slots: &[Slot]| {
            slots
                .iter()
                .filter_map(|s| s.app.save_wait())
                .max()
                .unwrap_or_default()
        };
        let mut deadline = tokio::time::Instant::now() + SAVE_TIMEOUT + wait(slots);
        loop {
            // The window that takes the keys is the one of an account whose
            // changes wait for a sign-in (not one that is only signing in).
            if let Some(at) = slots
                .iter()
                .position(|s| s.app.sign_in.is_some() && (s.app.saving() || unsaved(s)))
            {
                shown = at;
            } else if let Some(at) = slots.iter().position(|s| s.app.saving()) {
                shown = at;
            }
            // A macro run in the sign-in window plays there.
            while let Some(msg) = slots[shown].app.next_sign_in_stroke() {
                let slot = &mut slots[shown];
                let effects = slot.app.update(msg);
                let copies = slot.carry_out(effects).copies;
                copy_all(screen, &mut slots[shown].app, copies);
                stale = true;
            }
            stale |= slots[shown].app.take_redraw();
            // A sign-in window keeps mett here only while a change is
            // still unsaved, so the user can sign in or retry it; other
            // sign-ins don't matter any more.
            if !waiting(slots) {
                break;
            }
            // What was ignored changed nothing to draw.
            if std::mem::take(&mut stale) {
                // The tab shown may be another's now.
                show_tabs(slots, shown, position);
                slots[shown].app.before_draw();
                screen.draw(&slots[shown].app)?;
            }
            let (at, msg) = tokio::select! {
                // Saving may need the user to sign in: the sign-in window
                // still works, and only it takes keys.
                event = input_rx.recv() => match event.transpose()? {
                    None => break,
                    Some(UserEvent::Term(TermEvent::Key(key)))
                        if key.kind != KeyEventKind::Release
                            && slots[shown].app.sign_in.is_some() =>
                    {
                        (shown, Msg::Key(key))
                    }
                    Some(UserEvent::Term(TermEvent::Resize(width, height))) => {
                        let tabbed = slots.len() > 1;
                        for slot in slots.iter_mut() {
                            slot.app.update(Msg::Resize {
                                width,
                                height: rows(height, tabbed),
                            });
                        }
                        stale = true;
                        continue;
                    }
                    // Closing the window again: no more waiting.
                    Some(UserEvent::Close) => break,
                    // Already quitting.
                    Some(_) => continue,
                },
                incoming = events_rx.recv() => match incoming {
                    Some((at, incoming)) => match when_quitting(&slots[at], incoming) {
                        Some((msg, wait)) => {
                            // A save the server throttled is tried again
                            // after the wait it asked for: wait for it that
                            // much longer.
                            if let Some(wait) = wait {
                                deadline = deadline.max(tokio::time::Instant::now() + wait);
                            }
                            (at, msg)
                        }
                        None => continue,
                    },
                    None => break,
                },
                // The user may take a while to sign in; the wait only
                // counts while they don't have to.
                () = tokio::time::sleep_until(deadline), if !signing_in(slots) => break,
            };
            let window_was_open = signing_in(slots);
            let slot = &mut slots[at];
            let effects = slot.app.update(msg);
            stale = true;
            // Nothing is edited, nor configured, any more; the sign-in
            // window may copy its link.
            let copies = slot.carry_out(effects).copies;
            copy_all(screen, &mut slots[at].app, copies);
            // The wait starts again once the user is done signing in.
            // (Still with what a throttled save has left to wait.)
            if window_was_open || signing_in(slots) {
                deadline = tokio::time::Instant::now() + SAVE_TIMEOUT + wait(slots);
            }
        }
    }
    // Not for long: what is left on the server expires anyway.
    let deadline = tokio::time::Instant::now() + UNWATCH_TIMEOUT;
    while slots.iter().any(|s| s.app.unwatching()) {
        tokio::select! {
            incoming = events_rx.recv() => match incoming {
                Some((at, Incoming::Msg(Msg::Response(response))))
                    if matches!(response.event, worker::Event::Watched(_)) =>
                {
                    let slot = &mut slots[at];
                    let effects = slot.app.update(Msg::Response(response));
                    slot.carry_out(effects);
                }
                Some(_) => {}
                None => break,
            },
            () = tokio::time::sleep_until(deadline) => break,
        }
    }
    if slots.iter().any(|s| s.app.saving()) {
        return Err(std::io::Error::other(
            "gave up waiting for changes to be saved; some messages may still be unread",
        ));
    }
    let failures: Vec<String> = slots
        .iter()
        .flat_map(|slot| {
            let many = slots_len_label(slot);
            slot.app
                .save_failures()
                .into_iter()
                .map(move |failure| format!("{many}{failure}"))
        })
        .collect();
    if !failures.is_empty() {
        return Err(std::io::Error::other(format!(
            "some changes could not be saved: {}",
            failures.join("; ")
        )));
    }
    Ok(())
}

/// What a saving failure of `slot`'s account starts with: its tab, if it
/// has one.
fn slots_len_label(slot: &Slot) -> String {
    match &slot.app.tabs {
        Some(_) => format!("{}: ", slot.label),
        None => String::new(),
    }
}

/// Puts each of `copies` on the clipboard, saying why one couldn't be.
fn copy_all(screen: &mut impl Frontend, app: &mut App, copies: Vec<String>) {
    for text in copies {
        if let Err(e) = screen.copy(&text) {
            app.status = format!("Could not copy: {e}");
        }
    }
}

/// What the main loop takes in next. Each is handled at once, so the
/// size of the large ones doesn't matter.
#[allow(clippy::large_enum_variant)]
enum Received {
    /// A message for an app.
    Msg(Msg),
    /// What the user did, a read error, or `None` once input closed.
    Input(Option<std::io::Result<UserEvent>>),
    /// A `:` command finished running, with the configuration it leaves.
    Configured(Result<(Configuration, app::Reconfigured), oneshot::error::RecvError>),
    /// Nothing more can come.
    Closed,
}

/// What `future` gives if it is ready now, without waiting for it.
async fn ready<F: Future + Unpin>(mut future: F) -> Option<F::Output> {
    std::future::poll_fn(|cx| {
        std::task::Poll::Ready(match std::pin::Pin::new(&mut future).poll(cx) {
            std::task::Poll::Ready(output) => Some(output),
            std::task::Poll::Pending => None,
        })
    })
    .await
}

/// Clears the screen. Unlike `Terminal::clear`, this doesn't ask the
/// terminal where the cursor is, which one that doesn't answer would make
/// fail; a terminal just made has nothing drawn to forget.
fn clear_screen() -> std::io::Result<()> {
    use ratatui::crossterm::{execute, terminal};

    execute!(std::io::stdout(), terminal::Clear(terminal::ClearType::All))
}

/// Draws the screen, then prints the view's overlay, if any, as plain text
/// that the terminal wraps by itself.
fn draw(terminal: &mut DefaultTerminal, app: &App) -> std::io::Result<()> {
    use std::io::Write;

    use ratatui::crossterm::{cursor, queue, style};

    let mut overlay = None;
    terminal.draw(|frame| overlay = view::render(app, frame))?;
    if let Some(overlay) = overlay {
        // The cells already hold the same text, so ratatui's next draw,
        // which compares against what it drew, leaves this alone.
        let mut out = std::io::stdout();
        queue!(
            out,
            cursor::MoveTo(overlay.x, overlay.y),
            style::Print(&overlay.text)
        )?;
        out.flush()?;
    }
    Ok(())
}

/// Puts `text` on the clipboard with the OSC 52 escape sequence, which
/// many terminals (and tmux, with `set-clipboard on`) support.
fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    use std::io::Write;

    use base64::Engine;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = std::io::stdout();
    write!(out, "\x1b]52;c;{encoded}\x07")?;
    out.flush()
}

/// How long to wait for pending changes when quitting.
const SAVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long to wait, when quitting, for watching to stop on the server.
const UNWATCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Effects that the main loop carries out itself, between messages.
#[derive(Default)]
struct Deferred {
    /// Text to edit, which needs the terminal.
    edit: Option<String>,
    /// `:` commands, which need the configuration.
    commands: Vec<String>,
    /// Text to put on the clipboard.
    copies: Vec<String>,
    /// What effects done at once answered, for the app.
    msgs: Vec<Msg>,
    /// Another account's tab to show.
    switch: Option<app::Switch>,
}

impl Deferred {
    fn absorb(&mut self, other: Deferred) {
        if other.edit.is_some() {
            self.edit = other.edit;
        }
        self.commands.extend(other.commands);
        self.copies.extend(other.copies);
        self.msgs.extend(other.msgs);
        if other.switch.is_some() {
            self.switch = other.switch;
        }
    }
}

/// The largest file Office365 takes as an attachment.
const MAX_ATTACHMENT: u64 = 150 * 1024 * 1024;

/// Appends `text` to the file at `path` (created if missing), on a line of
/// its own even if the file doesn't end with a newline.
fn append_line(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    // The end of the file, to tell whether its last command is over.
    let length = file.metadata()?.len();
    let mut tail = vec![0; usize::try_from(length.min(64)).unwrap_or(64)];
    if !tail.is_empty() {
        file.seek(SeekFrom::End(-i64::try_from(tail.len()).unwrap_or(64)))?;
        file.read_exact(&mut tail)?;
    }
    let (line, newline) = match tail.strip_suffix(b"\n") {
        // (A CRLF file reads as LF.)
        Some(line) => (line.strip_suffix(b"\r").unwrap_or(line), true),
        None => (tail.as_slice(), false),
    };
    // An odd number of backslashes at the end of the line carries the
    // command on: an empty line ends it.
    let continued = line.iter().rev().take_while(|&&b| b == b'\\').count() % 2 == 1;
    let separator = match (newline || tail.is_empty(), continued) {
        (true, false) => "",
        (true, true) | (false, false) => "\n",
        (false, true) => "\n\n",
    };
    file.write_all(format!("{separator}{text}").as_bytes())
}

/// Reads a file to attach, stopping past [`MAX_ATTACHMENT`]: one larger
/// (or without end, such as `/dev/zero`) can't be sent.
fn read_attachment(path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::io::Read;

    let mut data = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_ATTACHMENT + 1).read_to_end(&mut data))
        .map_err(|e| e.to_string())?;
    if data.len() as u64 > MAX_ATTACHMENT {
        return Err(format!(
            "larger than {} MB, which Office365 doesn't take",
            MAX_ATTACHMENT / 1024 / 1024
        ));
    }
    Ok(data)
}

/// How many files are being saved, or written for a viewer and opened:
/// mett waits for them before exiting, so none is left half written (nor
/// a viewer without its file).
static SAVING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Waits (a while) for the files being saved.
fn wait_for_saves() {
    // However long it takes: cut short, a file would be left half written.
    while SAVING.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Writes `data` to `path`; a file there is replaced only if `overwrite`,
/// and only once the new one is written in full.
fn save_file(path: std::path::PathBuf, data: Vec<u8>, overwrite: bool) -> app::FileOutcome {
    let written = if overwrite {
        replace_file(&path, &data)
    } else {
        write_new(&path, &data, None)
    };
    match written {
        Ok(()) => app::FileOutcome::Done(path),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            app::FileOutcome::Exists(path, data)
        }
        Err(e) => {
            let why = format!("{}: {e}", path.display());
            app::FileOutcome::Failed(path, why)
        }
    }
}

/// Writes `data` to a new file at `path`, which is removed again if that
/// fails partway; with `permissions`, if given, given to the file before
/// anything is written to it (it is private until then).
fn write_new(
    path: &std::path::Path,
    data: &[u8],
    permissions: Option<std::fs::Permissions>,
) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if permissions.is_some() {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let written = permissions
        .map_or(Ok(()), |permissions| file.set_permissions(permissions))
        .and_then(|()| file.write_all(data));
    written.inspect_err(|_| {
        let _ = std::fs::remove_file(path);
    })
}

/// Writes `data` beside `path`, then puts it in its place: a failure
/// leaves the file there as it was.
fn replace_file(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    // A link is followed: the file it names is what is replaced.
    let resolved = std::fs::canonicalize(path)?;
    let path = resolved.as_path();
    static WRITTEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("not a file name"))?;
    let count = WRITTEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = path.with_file_name(format!(
        ".{}.mett-{}-{count}",
        name.to_string_lossy(),
        std::process::id()
    ));
    // The file keeps its permissions, from the start: a private one is
    // never readable by others meanwhile.
    let permissions = std::fs::metadata(path)?.permissions();
    write_new(&temporary, data, Some(permissions))?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

/// Writes an attachment to a directory of its own, readable only by the
/// user, and opens it with the program the system has for it.
fn open_file(name: &str, data: &[u8]) -> std::io::Result<()> {
    static OPENED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // Changed on Unix only.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    // A name no earlier run left behind: the process, the time, and a
    // count, tried again if taken all the same.
    let started = now_nanos();
    let dir = loop {
        let n = OPENED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("mett-{}-{started}-{n}", std::process::id()));
        match builder.create(&dir) {
            Ok(()) => break dir,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 1000 => continue,
            Err(e) => return Err(e),
        }
    };
    // Taken away by a later run of mett, once old (a viewer it was given
    // to may still need it after mett exits), or now if it can't be
    // opened.
    let path = dir.join(name);
    let opened = std::fs::write(&path, data).and_then(|()| open::that_detached(&path));
    if opened.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    opened
}

/// The time, in nanoseconds since the epoch.
fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// How old a copy of an attachment given to a viewer has to be to be
/// taken away: mett can't tell when the viewer is done with it.
const VIEWED_LEFT: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// How long another mett may keep a viewer's copy, where mett can't tell
/// whether it still runs.
const VIEWED_KEPT: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// Whether process `pid`, which made a copy `age` ago, may still be
/// running (another mett, whose viewers still use their copies). Without
/// `/proc` to tell, one made in the last week may be.
fn running(pid: &str, age: std::time::Duration) -> bool {
    let proc = std::path::Path::new("/proc");
    if proc.is_dir() {
        proc.join(pid).exists()
    } else {
        age < VIEWED_KEPT
    }
}

/// Removes the copies of attachments that earlier runs left for their
/// viewers, once they are old (named `mett-PID-NANOS-N`, by when).
fn remove_viewed_before() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let now = now_nanos();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix("mett-")) else {
            continue;
        };
        let parts: Vec<&str> = rest.split('-').collect();
        let [pid, started, count] = parts.as_slice() else {
            continue;
        };
        let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let Ok(started) = started.parse::<u128>() else {
            continue;
        };
        if numeric(pid)
            && numeric(count)
            && now.saturating_sub(started) > VIEWED_LEFT.as_nanos()
            && !running(
                pid,
                std::time::Duration::from_nanos(
                    u64::try_from(now.saturating_sub(started)).unwrap_or(u64::MAX),
                ),
            )
            && entry.file_type().is_ok_and(|t| t.is_dir())
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The configuration that `:` commands change.
struct Configuration {
    config: config::Config,
    /// The signature in use: see [`run_command`].
    signature: Option<String>,
}

/// `:` commands waiting to run, one at a time, in order, and the
/// configuration while none is running.
struct Commands<T = Configuration> {
    /// The configuration; `None` while a command has it.
    idle: Option<T>,
    queue: std::collections::VecDeque<String>,
}

impl<T> Commands<T> {
    fn new(configuration: T) -> Self {
        Self {
            idle: Some(configuration),
            queue: std::collections::VecDeque::new(),
        }
    }

    /// The next command to run, with the configuration it changes, unless
    /// one is running.
    fn start(&mut self) -> Option<(String, T)> {
        self.idle.as_ref()?;
        let command = self.queue.pop_front()?;
        Some((command, self.idle.take()?))
    }

    /// Takes the configuration back from the command that ran.
    fn finish(&mut self, configuration: T) {
        self.idle = Some(configuration);
    }
}

/// Runs a `:` command and reports what the configuration is now.
///
/// `signature` is the signature in use, kept unless the command changes
/// the `signature` variable.
fn run_command(
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

/// Carries out `effects`, except those the main loop does itself.
fn dispatch(
    app: &mut App,
    requests: &mpsc::UnboundedSender<(RequestId, Request)>,
    timers: &mpsc::UnboundedSender<app::Timer>,
    files: &mpsc::UnboundedSender<app::Msg>,
    cancel: Option<&CancelHandle>,
    effects: Vec<Effect>,
) -> Deferred {
    let mut deferred = Deferred::default();
    for effect in effects {
        match effect {
            Effect::Request(id, request) => {
                // The worker only stops when this sender is dropped.
                let _ = requests.send((id, request));
            }
            // The browser's output would garble the screen; `open` sends
            // it nowhere.
            Effect::OpenUrl(url) => {
                if let Err(e) = open::that_detached(&url) {
                    app.status = format!("Could not open a browser: {e}");
                }
            }
            Effect::CancelSignIn => {
                if let Some(cancel) = cancel {
                    cancel.cancel();
                }
            }
            Effect::Copy(text) => deferred.copies.push(text),
            Effect::Account(switch) => deferred.switch = Some(switch),
            Effect::Edit(text) => deferred.edit = Some(text),
            // Files are read and written on threads of their own: a slow
            // disk (or a FIFO) doesn't hold up the screen, nor quitting.
            Effect::SaveFile {
                op,
                path,
                data,
                overwrite,
            } => spawn_file(files, true, move || {
                Some(app::Msg::FileSaved(op, save_file(path, data, overwrite)))
            }),
            Effect::OpenFile { name, data } => spawn_file(files, true, move || {
                let opened = open_file(&name, &data);
                opened
                    .err()
                    .map(|e| app::Msg::Failed(format!("Could not open {name}: {e}")))
            }),
            Effect::AppendFile { path, text } => spawn_file(files, true, move || {
                let appended = append_line(&path, &text)
                    .map(|()| path.clone())
                    .map_err(|e| format!("{}: {e}", path.display()));
                Some(app::Msg::Appended(appended))
            }),
            Effect::ReadFile(op, path) => spawn_file(files, false, move || {
                let read = read_attachment(&path);
                Some(app::Msg::FileRead(op, path, read))
            }),
            Effect::Command(command) => deferred.commands.push(command),
            // Loading dictionaries takes a moment: on a thread of its own.
            Effect::LoadSpeller(settings) => spawn_file(files, false, move || {
                let home = platform::home();
                let loaded = editor::spell::Speller::load(&settings, home.as_deref());
                Some(Msg::SpellerLoaded(settings, loaded.map(Arc::new)))
            }),
            Effect::AddWord { path, word } => spawn_file(files, true, move || {
                // One at a time, each on a line of its own: two words added
                // at once don't run together.
                static WRITING: std::sync::Mutex<()> = std::sync::Mutex::new(());
                let _writing = WRITING
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                append_line(&path, &format!("{word}\n"))
                    .err()
                    .map(|e| Msg::Failed(format!("spell_words: {}: {e}", path.display())))
            }),
            Effect::Later(after, timer) => {
                let timers = timers.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(after).await;
                    // The app may have quit; nothing to do then.
                    let _ = timers.send(timer);
                });
            }
        }
    }
    deferred
}

/// Runs `work` on a thread of its own, and sends the app what it has to
/// say, if anything. One `counted` among [`SAVING`] is waited for before
/// exiting, until it is done.
fn spawn_file(
    files: &mpsc::UnboundedSender<Msg>,
    counted: bool,
    work: impl FnOnce() -> Option<Msg> + Send + 'static,
) {
    let files = files.clone();
    if counted {
        SAVING.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    std::thread::spawn(move || {
        let msg = work();
        if counted {
            SAVING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        if let Some(msg) = msg {
            let _ = files.send(msg);
        }
    });
}

/// The thread that reads terminal input. crossterm's reads block, so they
/// happen on a plain thread and are forwarded to the async loop. The
/// thread can be paused, so that it doesn't take keys meant for the
/// editor.
struct Input {
    control: std::sync::mpsc::Sender<Pause>,
}

/// A request to stop reading input: the thread acknowledges it, then waits
/// until `resume` is dropped.
struct Pause {
    paused: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

type InputEvents = mpsc::UnboundedReceiver<std::io::Result<UserEvent>>;

impl Input {
    /// How long a read waits before checking for a pause.
    const POLL: std::time::Duration = std::time::Duration::from_millis(50);

    fn start() -> (Self, InputEvents) {
        let (events, received) = mpsc::unbounded_channel();
        let (control, pauses) = std::sync::mpsc::channel::<Pause>();
        std::thread::spawn(move || {
            loop {
                match pauses.try_recv() {
                    Ok(pause) => {
                        let _ = pause.paused.send(());
                        // Returns once the sender is dropped.
                        let _ = pause.resume.recv();
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
                let event = match event::poll(Self::POLL) {
                    Ok(false) => continue,
                    Ok(true) => event::read(),
                    Err(e) => Err(e),
                };
                let failed = event.is_err();
                if events.send(event.map(UserEvent::Term)).is_err() || failed {
                    break;
                }
            }
        });
        (Self { control }, received)
    }

    /// Runs `f` with the terminal restored to normal and input reading
    /// paused, then takes the terminal back.
    ///
    /// Input that arrived before the pause is dropped: keys typed ahead
    /// while the editor was starting are not meant for mett (a stray `y`
    /// could send the message).
    fn suspended<T>(
        &self,
        terminal: &mut DefaultTerminal,
        events: &mut InputEvents,
        f: impl FnOnce() -> T,
    ) -> std::io::Result<T> {
        let (paused, acknowledged) = std::sync::mpsc::channel();
        let (resume, resumed) = std::sync::mpsc::channel::<()>();
        let result = tokio::task::block_in_place(|| {
            if self
                .control
                .send(Pause {
                    paused,
                    resume: resumed,
                })
                .is_ok()
            {
                // The thread is between reads once it answers.
                let _ = acknowledged.recv();
            }
            while events.try_recv().is_ok() {}
            ratatui::restore();
            let result = f();
            // Take the terminal back while still paused, and drop what was
            // typed as the editor exited: a key read after the compose
            // menu is up would act on it (a stray `y` could send the
            // message).
            *terminal = ratatui::init();
            discard_pending_input();
            drop(resume);
            result
        });
        // A new terminal draws everything: only the screen needs clearing.
        clear_screen()?;
        Ok(result)
    }
}

/// Reads and drops the terminal input that is already waiting.
fn discard_pending_input() {
    while matches!(event::poll(std::time::Duration::ZERO), Ok(true)) {
        if event::read().is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[cfg(unix)]
    #[test]
    fn attachments_stop_at_what_office365_takes() {
        let read = read_attachment(std::path::Path::new("/dev/zero"));
        assert!(read.is_err_and(|e| e.contains("150 MB")));
    }

    #[test]
    fn replacing_a_file_keeps_it_until_the_new_one_is_written() {
        let dir = std::env::temp_dir().join(format!("mett-replace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.txt");
        std::fs::write(&path, "old").unwrap();
        assert!(matches!(
            save_file(path.clone(), b"new".to_vec(), false),
            app::FileOutcome::Exists(..)
        ));
        assert!(matches!(
            save_file(path.clone(), b"new".to_vec(), true),
            app::FileOutcome::Done(_)
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            save_file(path.clone(), b"newer".to_vec(), true);
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Through a link, the file it names is replaced, and the link stays.
        #[cfg(unix)]
        {
            let link = dir.join("link.txt");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            save_file(link.clone(), b"linked".to_vec(), true);
            assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "linked");
            std::fs::remove_file(&link).unwrap();
        }
        // Nothing left beside it.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn appends_on_a_line_of_its_own() {
        let dir = std::env::temp_dir().join(format!("mett-append-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mettrc");
        std::fs::write(&path, "set a = b").unwrap();
        append_line(&path, "alias x x@y\n").unwrap();
        append_line(&path, "alias z z@y\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "set a = b\nalias x x@y\nalias z z@y\n"
        );
        // A line carried on is ended first.
        let continued = dir.join("continued");
        std::fs::write(&continued, "set a = b \\\n").unwrap();
        append_line(&continued, "alias x x@y\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&continued).unwrap(),
            "set a = b \\\n\nalias x x@y\n"
        );
        let crlf = dir.join("crlf");
        std::fs::write(&crlf, "set a = b \\\r\n").unwrap();
        append_line(&crlf, "alias x x@y\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&crlf).unwrap(),
            "set a = b \\\r\n\nalias x x@y\n"
        );
        let new = dir.join("new/mettrc");
        append_line(&new, "alias x x@y\n").unwrap();
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "alias x x@y\n");
        std::fs::remove_dir_all(&dir).unwrap();
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
    fn saving_a_file_replaces_one_there_only_when_asked() {
        let dir = std::env::temp_dir().join(format!("mett-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            save_file(path.clone(), b"one".to_vec(), false),
            app::FileOutcome::Done(path.clone())
        );
        assert_eq!(
            save_file(path.clone(), b"two".to_vec(), false),
            app::FileOutcome::Exists(path.clone(), b"two".to_vec())
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"one");
        save_file(path.clone(), b"two".to_vec(), true);
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        std::fs::remove_dir_all(&dir).unwrap();
    }

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

    /// What a draw showed: the tab, the messages in the index, the status,
    /// and whether a question was on the message line.
    type Shown = (Option<String>, usize, String, bool);

    /// A screen that records what each draw showed: the tab shown (with
    /// several accounts) and how many messages its index had.
    struct Recording {
        drawn: Arc<std::sync::Mutex<Vec<Shown>>>,
    }

    impl Frontend for Recording {
        fn size(&self) -> std::io::Result<ratatui::layout::Size> {
            Ok(ratatui::layout::Size {
                width: 80,
                height: 24,
            })
        }

        fn draw(&mut self, app: &App) -> std::io::Result<()> {
            let shown = app.tabs.as_ref().map(|t| t.tabs[t.active].label.clone());
            self.drawn.lock().unwrap().push((
                shown,
                app.messages.len(),
                app.status.clone(),
                app.input.is_some(),
            ));
            Ok(())
        }

        fn edit(
            &mut self,
            _: &Editor,
            _: &mut InputEvents,
            _: &str,
        ) -> std::io::Result<Result<String, String>> {
            Err(std::io::Error::other("no editor here"))
        }

        fn copy(&mut self, _: &str) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A demo account with this tab `label`.
    fn demo_account(label: &str) -> Opened<mett_core::mock::MemoryStore> {
        demo_account_pushing(label, Vec::new())
    }

    /// A demo account that plays `pushed` once its folder is listed.
    fn demo_account_pushing(
        label: &str,
        pushed: Vec<keymap::Stroke>,
    ) -> Opened<mett_core::mock::MemoryStore> {
        let (_, sign_in) = mpsc::unbounded_channel();
        let settings = demo::settings();
        Opened {
            label: label.to_owned(),
            store: Arc::new(demo::mailbox()),
            session: Session {
                folder: FolderPath::WellKnown(mett_core::WellKnownFolder::Inbox, Vec::new()),
                account: settings.account.clone(),
                settings,
                keymap: Keymap::mutt_defaults(),
                pushed,
                config: config::Config::new(None),
                editor: Editor::from_env("", ""),
                sign_in,
                syncing: mpsc::unbounded_channel().1,
                cancel: None,
                notice: None,
            },
        }
    }

    fn key(c: char, alt: bool) -> std::io::Result<UserEvent> {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let modifiers = if alt {
            KeyModifiers::ALT
        } else {
            KeyModifiers::NONE
        };
        Ok(UserEvent::Term(TermEvent::Key(KeyEvent::new(
            KeyCode::Char(c),
            modifiers,
        ))))
    }

    /// Waits until a draw satisfies `wanted`, and returns it.
    async fn drawn_until(
        drawn: &Arc<std::sync::Mutex<Vec<Shown>>>,
        wanted: impl Fn(&Shown) -> bool,
    ) -> Shown {
        for _ in 0..500 {
            if let Some(found) = drawn.lock().unwrap().iter().rev().find(|d| wanted(d)) {
                return found.clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("never drawn: {:?}", drawn.lock().unwrap());
    }

    #[tokio::test]
    async fn several_accounts_run_at_once_and_the_tabs_switch_between_them() {
        // The app isn't `Send`: it runs on this task's thread.
        tokio::task::LocalSet::new()
            .run_until(async {
                let drawn = Arc::default();
                let screen = Recording {
                    drawn: Arc::clone(&drawn),
                };
                let (input, events) = mpsc::unbounded_channel();
                let accounts = Accounts {
                    list: vec![demo_account("Work"), demo_account("Home")],
                    position: app::TabPosition::Top,
                };
                let running = tokio::task::spawn_local(run(screen, events, accounts));

                // The first is shown, with its mail.
                let first = drawn_until(&drawn, |(tab, messages, ..)| {
                    tab.as_deref() == Some("Work") && *messages > 0
                })
                .await;
                assert_eq!(first.0.as_deref(), Some("Work"));
                // Alt-n goes to the next, whose mail was listed in the
                // background.
                input.send(key('n', true)).unwrap();
                let home = drawn_until(&drawn, |(tab, messages, ..)| {
                    tab.as_deref() == Some("Home") && *messages > 0
                })
                .await;
                assert!(home.1 > 0, "listed while it was in the background");
                // Alt-1 and Alt-2 go to them by number; Alt-9 to none.
                input.send(key('1', true)).unwrap();
                drawn_until(&drawn, |(tab, ..)| tab.as_deref() == Some("Work")).await;
                input.send(key('9', true)).unwrap();
                drawn_until(&drawn, |(_, _, status, _)| {
                    status == "There is no account 9."
                })
                .await;
                input.send(key('2', true)).unwrap();
                drawn_until(&drawn, |(tab, ..)| tab.as_deref() == Some("Home")).await;
                input.send(key('p', true)).unwrap();
                drawn_until(&drawn, |(tab, ..)| tab.as_deref() == Some("Work")).await;

                // Quitting quits them all.
                input.send(key('q', false)).unwrap();
                let ended = tokio::time::timeout(std::time::Duration::from_secs(10), running)
                    .await
                    .expect("quits")
                    .unwrap();
                assert!(ended.is_ok(), "{ended:?}");
            })
            .await;
    }

    #[tokio::test]
    async fn quitting_asks_about_the_deleted_messages_of_an_account_in_the_background() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let drawn = Arc::default();
                let screen = Recording {
                    drawn: Arc::clone(&drawn),
                };
                let (input, events) = mpsc::unbounded_channel();
                let accounts = Accounts {
                    list: vec![demo_account("Work"), demo_account("Home")],
                    position: app::TabPosition::Top,
                };
                let running = tokio::task::spawn_local(run(screen, events, accounts));
                // In Home, a message is marked deleted; then Work is shown.
                input.send(key('2', true)).unwrap();
                drawn_until(&drawn, |(tab, messages, ..)| {
                    tab.as_deref() == Some("Home") && *messages > 0
                })
                .await;
                input.send(key('d', false)).unwrap();
                input.send(key('1', true)).unwrap();
                drawn_until(&drawn, |(tab, ..)| tab.as_deref() == Some("Work")).await;
                let before = drawn.lock().unwrap().len();
                // Quitting from Work shows Home, and asks about the purge.
                input.send(key('q', false)).unwrap();
                for _ in 0..500 {
                    let asked = drawn.lock().unwrap()[before..]
                        .iter()
                        .any(|(tab, _, _, question)| tab.as_deref() == Some("Home") && *question);
                    if asked {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                assert!(
                    drawn.lock().unwrap()[before..]
                        .iter()
                        .any(|(tab, _, _, question)| tab.as_deref() == Some("Home") && *question),
                    "{:?}",
                    drawn.lock().unwrap()
                );
                assert!(!running.is_finished(), "waits for the answer");
                // Not purging, it quits.
                input.send(key('n', false)).unwrap();
                let ended = tokio::time::timeout(std::time::Duration::from_secs(10), running)
                    .await
                    .expect("quits")
                    .unwrap();
                assert!(ended.is_ok(), "{ended:?}");
            })
            .await;
    }

    #[tokio::test]
    async fn a_push_in_an_account_not_shown_plays_there() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let drawn = Arc::default();
                let screen = Recording {
                    drawn: Arc::clone(&drawn),
                };
                let (input, events) = mpsc::unbounded_channel();
                // Home's own file pushes `d`: its first message is marked
                // deleted, though Work is the account shown.
                let delete = keymap::Stroke::Key(keymap::Key::char('d'));
                let accounts = Accounts {
                    list: vec![
                        demo_account("Work"),
                        demo_account_pushing("Home", vec![delete]),
                    ],
                    position: app::TabPosition::Top,
                };
                let running = tokio::task::spawn_local(run(screen, events, accounts));
                drawn_until(&drawn, |(tab, messages, ..)| {
                    tab.as_deref() == Some("Work") && *messages > 0
                })
                .await;
                // So quitting asks about Home's deleted message.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                let before = drawn.lock().unwrap().len();
                input.send(key('q', false)).unwrap();
                drawn_until(&drawn, |(tab, _, _, question)| {
                    tab.as_deref() == Some("Home") && *question
                })
                .await;
                assert!(drawn.lock().unwrap().len() > before);
                input.send(key('n', false)).unwrap();
                let ended = tokio::time::timeout(std::time::Duration::from_secs(10), running)
                    .await
                    .expect("quits")
                    .unwrap();
                assert!(ended.is_ok(), "{ended:?}");
            })
            .await;
    }

    #[tokio::test]
    async fn a_quit_that_finishes_after_going_to_another_account_still_quits() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let drawn = Arc::default();
                let screen = Recording {
                    drawn: Arc::clone(&drawn),
                };
                let (input, events) = mpsc::unbounded_channel();
                let accounts = Accounts {
                    list: vec![demo_account("Work"), demo_account("Home")],
                    position: app::TabPosition::Top,
                };
                let running = tokio::task::spawn_local(run(screen, events, accounts));
                input.send(key('2', true)).unwrap();
                drawn_until(&drawn, |(tab, messages, ..)| {
                    tab.as_deref() == Some("Home") && *messages > 0
                })
                .await;
                // In Home: a message deleted, then quit, purge, and straight
                // to Work, before the purge is done.
                for stroke in [
                    key('d', false),
                    key('q', false),
                    key('y', false),
                    key('1', true),
                ] {
                    input.send(stroke).unwrap();
                }
                let ended = tokio::time::timeout(std::time::Duration::from_secs(10), running)
                    .await
                    .expect("quits")
                    .unwrap();
                assert!(ended.is_ok(), "{ended:?}");
            })
            .await;
    }

    #[tokio::test]
    async fn a_push_that_quits_in_an_account_not_shown_quits_mett() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let screen = Recording {
                    drawn: Arc::default(),
                };
                let (_input, events) = mpsc::unbounded_channel();
                let quit = keymap::Stroke::Key(keymap::Key::char('q'));
                let accounts = Accounts {
                    list: vec![
                        demo_account("Work"),
                        demo_account_pushing("Home", vec![quit]),
                    ],
                    position: app::TabPosition::Top,
                };
                let ended = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    tokio::task::spawn_local(run(screen, events, accounts)),
                )
                .await
                .expect("quits")
                .unwrap();
                assert!(ended.is_ok(), "{ended:?}");
            })
            .await;
    }

    #[tokio::test]
    async fn one_account_has_no_tabs() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let drawn = Arc::default();
                let screen = Recording {
                    drawn: Arc::clone(&drawn),
                };
                let (input, events) = mpsc::unbounded_channel();
                let accounts = Accounts {
                    list: vec![demo_account("Work")],
                    position: app::TabPosition::Top,
                };
                let running = tokio::task::spawn_local(run(screen, events, accounts));
                drawn_until(&drawn, |(_, messages, ..)| *messages > 0).await;
                assert!(drawn.lock().unwrap().iter().all(|(tab, ..)| tab.is_none()));
                // There is nothing to go to.
                input.send(key('n', true)).unwrap();
                drawn_until(&drawn, |(_, _, status, _)| {
                    status == "There is only one account."
                })
                .await;
                input.send(key('q', false)).unwrap();
                let ended = tokio::time::timeout(std::time::Duration::from_secs(10), running)
                    .await
                    .expect("quits")
                    .unwrap();
                assert!(ended.is_ok(), "{ended:?}");
            })
            .await;
    }
}
