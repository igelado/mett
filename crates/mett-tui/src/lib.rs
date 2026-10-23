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
mod commands;
mod compose;
mod config;
mod demo;
mod editor;
mod expando;
mod files;
mod format;
mod frontend;
#[cfg(feature = "gui")]
mod gui;
mod imap;
mod input;
mod keymap;
mod lineedit;
mod pattern;
mod platform;
mod quit;
mod settings;
mod signin;
mod sort;
mod text;
mod view;
mod worker;

use std::process::ExitCode;
use std::sync::Arc;

use ratatui::crossterm::event::{Event as TermEvent, KeyEventKind};
use tokio::sync::{mpsc, oneshot};

use crate::app::{App, Effect, Msg, Settings};
use crate::commands::{Commands, Configuration, run_command};
use crate::config::mailbox::FolderPath;
use crate::editor::Editor;
use crate::files::{
    append_line, open_file, read_attachment, remove_viewed_before, save_file, spawn_file,
    wait_for_saves,
};
use crate::frontend::{Frontend, TerminalScreen, clear_screen};
use crate::input::{Input, InputEvents};
use crate::keymap::Keymap;
use crate::quit::finish;
use crate::settings::settings_from_config;
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
        let mut list: Vec<Opened<_>> = Vec::with_capacity(named.len());
        // The caches the accounts use, named by the account (and for IMAP the
        // server): two tabs for one would share it, though their backends
        // may not agree on what is in it.
        let mut cached: Vec<std::path::PathBuf> = Vec::new();
        for (label, config) in named {
            match open_account(&mut launch, label.clone(), config, &keymap) {
                Ok((opened, Some(cache))) if cached.contains(&cache) => {
                    return fail(
                        &mut launch,
                        format!(
                            "account {}: {} is the account of another tab already",
                            opened.label, opened.session.account
                        ),
                    );
                }
                Ok((opened, cache)) => {
                    cached.extend(cache);
                    list.push(opened);
                }
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
/// used, in the TUI's sign-in window), its cache and the app's settings; and
/// its cache file, if it has one (two tabs can't share it).
fn open_account(
    launch: &mut impl Launch,
    label: String,
    mut config: config::Config,
    keymap: &Keymap,
) -> Result<
    (
        Opened<mett_sqlite::CachedStore<account::Mailbox>>,
        Option<std::path::PathBuf>,
    ),
    String,
> {
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
    let (mailbox, warning, cache) = account::cached(&config, &spoolfile, mailbox, progress);
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
    Ok((
        Opened {
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
        },
        cache,
    ))
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
/// Called on every pass of the main loop: an app's tabs are replaced only
/// when they changed (the active tab, a label or a tab's new-mail mark), and
/// the apps share one list.
fn show_tabs(slots: &mut [Slot], active: usize, position: app::TabPosition) {
    if slots.len() < 2 {
        return;
    }
    // What is shown is looked at.
    slots[active].app.arrived = false;
    let fresh: std::sync::Arc<[app::Tab]> = slots
        .iter()
        .map(|slot| app::Tab {
            label: slot.label.clone(),
            new_mail: slot.app.mailboxes_with_new_mail() > 0 || slot.app.arrived,
        })
        .collect();
    // The list the apps have, if it is still right.
    let tabs = match &slots[active].app.tabs {
        Some(shown) if *shown.tabs == *fresh => std::sync::Arc::clone(&shown.tabs),
        _ => fresh,
    };
    for slot in slots.iter_mut() {
        let current = matches!(
            &slot.app.tabs,
            Some(shown) if std::sync::Arc::ptr_eq(&shown.tabs, &tabs)
                && shown.active == active
                && shown.position == position
        );
        if !current {
            slot.app.tabs = Some(app::Tabs {
                tabs: std::sync::Arc::clone(&tabs),
                active,
                position,
            });
        }
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
                // A question there (a purge, asked by a `push <quit>`) needs
                // the user: that account is shown.
                if slots[at].app.input.is_some() {
                    active = at;
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

/// Puts each of `copies` on the clipboard, saying why one couldn't be.
fn copy_all(screen: &mut impl Frontend, app: &mut App, copies: Vec<String>) {
    for text in copies {
        if let Err(e) = screen.copy(&text) {
            app.status.info(format!("Could not copy: {e}"));
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
                    app.status.info(format!("Could not open a browser: {e}"));
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
                let _writing = platform::lock(&WRITING);
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

#[cfg(test)]
mod tests {
    use super::*;

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
                app.index.messages.len(),
                app.status.text().to_owned(),
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
    async fn a_question_asked_in_the_background_shows_the_account() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let drawn = Arc::default();
                let screen = Recording {
                    drawn: Arc::clone(&drawn),
                };
                let (input, events) = mpsc::unbounded_channel();
                // Home's file deletes a message and quits: asking whether to
                // purge it.
                let push = vec![
                    keymap::Stroke::Key(keymap::Key::char('d')),
                    keymap::Stroke::Key(keymap::Key::char('q')),
                ];
                let accounts = Accounts {
                    list: vec![demo_account("Work"), demo_account_pushing("Home", push)],
                    position: app::TabPosition::Top,
                };
                let running = tokio::task::spawn_local(run(screen, events, accounts));
                // Home is shown, with the question, though Work was first.
                drawn_until(&drawn, |(tab, _, _, question)| {
                    tab.as_deref() == Some("Home") && *question
                })
                .await;
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
