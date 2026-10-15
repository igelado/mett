//! mett: a mutt-like terminal mail client for Office365.
//!
//! The main loop wires the pieces of `docs/DESIGN.md` together: key presses
//! become [`action::Action`]s through the [`Keymap`], [`App::update`] turns them and
//! worker responses into a new state plus [`Effect`]s, effects go to the
//! [`worker`], and [`view::render`] draws the state.

mod account;
mod action;
mod app;
mod color;
mod compose;
mod config;
mod demo;
mod editor;
mod expando;
mod format;
mod keymap;
mod lineedit;
mod pattern;
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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse_args(&args) {
        Ok(options) => options,
        Err(e) => {
            eprintln!("mett: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if options.version {
        println!("mett {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    // The demo mailbox needs no configuration.
    if !options.demo {
        let home = std::env::var("HOME").ok().filter(|h| !h.is_empty());
        let mut config = config::Config::new(home);
        for file in config::locate(&options.files, &|name| std::env::var_os(name)) {
            config.load_file(&file);
        }
        for warning in &config.warnings {
            eprintln!("mett: warning: {warning}");
        }
        for error in &config.errors {
            eprintln!("mett: {error}");
        }
        if !config.errors.is_empty() {
            return ExitCode::FAILURE;
        }
        if options.dump {
            for line in config.dump() {
                println!("{line}");
            }
            return ExitCode::SUCCESS;
        }
        let spoolfile = match config.spoolfile() {
            Ok(url) => url,
            Err(e) => {
                eprintln!("mett: {e}");
                return ExitCode::FAILURE;
            }
        };
        let settings = match settings_from_config(&config, &spoolfile.account) {
            Ok(settings) => settings,
            Err(e) => {
                eprintln!("mett: {e}");
                return ExitCode::FAILURE;
            }
        };
        let (keymap, problems) = keymap_from_config(&mut config);
        for problem in problems {
            eprintln!("mett: warning: {problem}; ignored");
        }
        let editor = Editor::new(config.string("editor"), config.string("tmpdir"), &|name| {
            std::env::var(name).ok()
        });
        let Some(runtime) = runtime() else {
            return ExitCode::FAILURE;
        };
        // Signing in happens in the TUI, in the sign-in window, when the
        // folder is first opened.
        let (events, sign_in) = mpsc::unbounded_channel();
        let (prompter, cancel) = TuiPrompter::new(spoolfile.account.clone(), events);
        let mailbox = match account::mailbox(&config, &spoolfile, prompter) {
            Ok(mailbox) => mailbox,
            Err(e) => {
                eprintln!("mett: {e}");
                return ExitCode::FAILURE;
            }
        };
        let mut pushed = Vec::new();
        for keys in config.take_pushed() {
            match keymap::parse_strokes(&keys) {
                Ok(strokes) => pushed.extend(strokes),
                Err(e) => eprintln!("mett: warning: push: {e}; ignored"),
            }
        }
        let session = Session {
            folder: spoolfile.folder,
            settings,
            keymap,
            pushed,
            account: spoolfile.account,
            config,
            editor,
            sign_in,
            cancel: Some(cancel),
        };
        return start(&runtime, Arc::new(mailbox), session);
    }

    let Some(runtime) = runtime() else {
        return ExitCode::FAILURE;
    };
    // The demo mailbox never signs in, so nothing is ever sent here.
    let (_, sign_in) = mpsc::unbounded_channel();
    // The demo reads no mettrc, but `:` commands work on the defaults.
    let (config, settings) = demo_config(std::env::var("HOME").ok().filter(|h| !h.is_empty()));
    let session = Session {
        folder: FolderPath::WellKnown(mett_core::WellKnownFolder::Inbox, Vec::new()),
        account: settings.account.clone(),
        settings,
        keymap: Keymap::mutt_defaults(),
        pushed: Vec::new(),
        config,
        editor: Editor::new("", "", &|name| std::env::var(name).ok()),
        sign_in,
        cancel: None,
    };
    start(&runtime, Arc::new(demo::mailbox()), session)
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
    /// Cancels the sign-in in progress, for mailboxes that sign in.
    cancel: Option<CancelHandle>,
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
    let compose = compose::Settings::from_config_keeping(config, account, signature)?;
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
    let page_size = match config.get("msgraph_page_size") {
        Some(config::Value::Number(n)) if *n > 0 => {
            u32::try_from(*n).map_err(|_| format!("msgraph_page_size {n} is too large"))?
        }
        _ => return Err("msgraph_page_size must be a positive number".into()),
    };
    let sync_interval = match config.get("msgraph_sync_interval") {
        Some(config::Value::Number(0)) => None,
        Some(config::Value::Number(n)) if *n > 0 => {
            Some(std::time::Duration::from_secs(n.unsigned_abs()))
        }
        _ => return Err("msgraph_sync_interval must be 0 (off) or a number of seconds".into()),
    };
    let folder = config.string("folder");
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
            .active(color::mono_terminal(&|name| std::env::var(name).ok()))
            .clone(),
    };
    let ordering = app::Ordering {
        sort: sort::Sort::parse(config.string("sort"), true).map_err(|e| format!("sort: {e}"))?,
        aux: sort::Sort::parse(config.string("sort_aux"), false)
            .map_err(|e| format!("sort_aux: {e}"))?,
        ascii: config.flag("ascii_chars"),
    };
    Ok(Settings {
        home: config.home().map(str::to_owned),
        page_size,
        mailboxes,
        display,
        ordering,
        sync_interval,
        compose,
        account: account.clone(),
        folder: config.string("folder").to_owned(),
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

fn runtime() -> Option<tokio::runtime::Runtime> {
    match tokio::runtime::Runtime::new() {
        Ok(runtime) => Some(runtime),
        Err(e) => {
            eprintln!("mett: cannot start: {e}");
            None
        }
    }
}

/// Runs the TUI on `store`.
fn start<S: mett_core::MailStore + mett_core::MailSender + 'static>(
    runtime: &tokio::runtime::Runtime,
    store: Arc<S>,
    session: Session,
) -> ExitCode {
    // `init` also restores the terminal if mett panics.
    remove_viewed_before();
    let terminal = ratatui::init();
    let result = runtime.block_on(run(terminal, store, session));
    ratatui::restore();
    wait_for_saves();
    remove_viewed();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mett: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run<S: mett_core::MailStore + mett_core::MailSender + 'static>(
    mut terminal: DefaultTerminal,
    store: Arc<S>,
    session: Session,
) -> std::io::Result<()> {
    let Session {
        folder,
        settings,
        keymap,
        pushed,
        account,
        config,
        mut editor,
        sign_in: mut sign_in_rx,
        cancel,
    } = session;

    let (request_tx, request_rx) = mpsc::unbounded_channel::<(RequestId, Request)>();
    let (response_tx, mut response_rx) = mpsc::unbounded_channel();
    tokio::spawn(worker::run(store, request_rx, response_tx));

    let (input, mut input_rx) = Input::start();
    let (timer_tx, mut timer_rx) = mpsc::unbounded_channel();
    // What reading, saving and opening files (on threads of their own)
    // has to say.
    let (file_tx, mut file_rx) = mpsc::unbounded_channel();

    // `:` commands run on their own thread, one at a time: `source` and
    // backticks may take a while, and the UI doesn't wait for them.
    let mut commands = Commands::new(Configuration {
        // Kept for `:` commands that don't change it.
        signature: settings.compose.signature.clone(),
        config,
    });
    let mut running: Option<oneshot::Receiver<(Configuration, app::Reconfigured)>> = None;
    let size = terminal.size()?;
    let (mut app, effects) = App::open(size.width, size.height, folder, settings);
    app.keymap = keymap;
    // Played once the folder is listed, so that `push <last-entry>` has
    // somewhere to go.
    app.push_after_load(pushed);
    let mut deferred = dispatch(
        &mut app,
        &request_tx,
        &timer_tx,
        &file_tx,
        cancel.as_ref(),
        effects,
    );

    while !app.quit {
        // What effects done at once answered goes to the app first.
        while !deferred.msgs.is_empty() {
            for msg in std::mem::take(&mut deferred.msgs) {
                let effects = app.update(msg);
                deferred.absorb(dispatch(
                    &mut app,
                    &request_tx,
                    &timer_tx,
                    &file_tx,
                    cancel.as_ref(),
                    effects,
                ));
            }
        }
        // The editor runs between messages, with the terminal handed over.
        commands.queue.extend(deferred.commands.drain(..));
        while let Some(text) = deferred.edit.take() {
            let result = input.suspended(&mut terminal, &mut input_rx, || editor.edit(&text))?;
            // The terminal may have been resized meanwhile.
            let size = terminal.size()?;
            app.update(Msg::Resize {
                width: size.width,
                height: size.height,
            });
            let effects = app.update(Msg::Edited(result));
            deferred.absorb(dispatch(
                &mut app,
                &request_tx,
                &timer_tx,
                &file_tx,
                cancel.as_ref(),
                effects,
            ));
            commands.queue.extend(deferred.commands.drain(..));
        }
        if running.is_none()
            && let Some((command, mut configuration)) = commands.start()
        {
            let account = account.clone();
            let (done, result) = oneshot::channel();
            running = Some(result);
            // A thread of its own, not the runtime's blocking pool: quitting
            // doesn't wait for a command that takes its time.
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
        draw(&mut terminal, &app)?;
        // Keys a macro or `push` queued come before new input, once the
        // last action has what it was waiting for.
        let stroke = app.next_stroke();
        if stroke.is_none() && app.take_redraw() {
            continue;
        }
        let msg = match stroke {
            Some(msg) => msg,
            None => tokio::select! {
                // Without input mett can't be used or quit, so a terminal
                // read error ends it (and the terminal is restored).
                event = input_rx.recv() => match event.transpose()? {
                    None => return Err(std::io::Error::other("terminal input closed")),
                    Some(event) => match event {
                        // Repeats too, so holding a key keeps moving.
                        TermEvent::Key(key) if key.kind != KeyEventKind::Release => {
                            app.typed();
                            Msg::Key(key)
                        }
                        TermEvent::Resize(width, height) => Msg::Resize { width, height },
                        _ => continue,
                    },
                },
                Some(response) = response_rx.recv() => Msg::Response(response),
                Some(event) = sign_in_rx.recv() => Msg::SignIn(event),
                Some(timer) = timer_rx.recv() => Msg::Timer(timer),
                Some(msg) = file_rx.recv() => msg,
                done = async { running.as_mut().expect("checked").await }, if running.is_some() => {
                    running = None;
                    let (configuration, reconfigured) =
                        done.map_err(|_| std::io::Error::other("a : command stopped"))?;
                    // `editor` and `tmpdir` may have changed.
                    editor = Editor::new(
                        configuration.config.string("editor"),
                        configuration.config.string("tmpdir"),
                        &|name| std::env::var(name).ok(),
                    );
                    commands.finish(configuration);
                    Msg::Configured(reconfigured)
                }
                else => break,
            },
        };
        let effects = app.update(msg);
        deferred.absorb(dispatch(
            &mut app,
            &request_tx,
            &timer_tx,
            &file_tx,
            cancel.as_ref(),
            effects,
        ));
    }

    // Give pending changes (such as marking a message read) a moment to
    // reach the mailbox, rather than cancelling them by exiting.
    // The outcome is reported after the terminal is restored, so a change
    // that couldn't be saved isn't lost silently.
    // A save's sign-in failure may still be queued, behind its response
    // and the key that quit: take it in, so its window can offer a retry.
    while let Ok(event) = sign_in_rx.try_recv() {
        // A sign-in that nothing left to save needs: the user has quit, so
        // don't send them to the browser.
        if matches!(event, SignInEvent::Prompt { .. }) && !app.saving() {
            if let Some(cancel) = &cancel {
                cancel.cancel();
            }
            continue;
        }
        let effects = app.update(Msg::SignIn(event));
        dispatch(
            &mut app,
            &request_tx,
            &timer_tx,
            &file_tx,
            cancel.as_ref(),
            effects,
        );
    }
    let unsaved = !app.save_failures().is_empty();
    if app.saving() || (unsaved && app.sign_in.is_some()) {
        app.status = "Saving changes...".into();
        let mut deadline = tokio::time::Instant::now() + SAVE_TIMEOUT;
        loop {
            // A failed save's sign-in failure is sent before the save's own
            // response; take it in first, so the window opens and the
            // save can be retried even after nothing is left pending.
            while let Ok(event) = sign_in_rx.try_recv() {
                let effects = app.update(Msg::SignIn(event));
                dispatch(
                    &mut app,
                    &request_tx,
                    &timer_tx,
                    &file_tx,
                    cancel.as_ref(),
                    effects,
                );
            }
            // A macro run in the sign-in window plays there.
            while let Some(msg) = app.next_sign_in_stroke() {
                let effects = app.update(msg);
                dispatch(
                    &mut app,
                    &request_tx,
                    &timer_tx,
                    &file_tx,
                    cancel.as_ref(),
                    effects,
                );
            }
            // A sign-in window keeps mett here only while a change is
            // still unsaved, so the user can sign in or retry it; other
            // sign-ins don't matter any more.
            let unsaved = !app.save_failures().is_empty();
            let waiting = app.saving() || (unsaved && app.sign_in.is_some());
            if !waiting {
                break;
            }
            draw(&mut terminal, &app)?;
            let msg = tokio::select! {
                // Saving may need the user to sign in: the sign-in window
                // still works, and only it takes keys.
                event = input_rx.recv() => match event.transpose()? {
                    None => break,
                    Some(TermEvent::Key(key))
                        if key.kind != KeyEventKind::Release && app.sign_in.is_some() =>
                    {
                        Msg::Key(key)
                    }
                    Some(TermEvent::Resize(width, height)) => Msg::Resize { width, height },
                    Some(_) => continue,
                },
                response = response_rx.recv() => match response {
                    // Only save results matter now. Anything else (say, a
                    // late message fetch) could start new work that would
                    // never be sent.
                    Some(response)
                        if matches!(
                            response.event,
                            worker::Event::FlagsUpdated(_)
                                | worker::Event::Sent(..)
                                | worker::Event::DraftSaved(..)
                                | worker::Event::KeptDiscarded(_)
                                | worker::Event::Transferred(_)
                                | worker::Event::Purged(_)
                        ) =>
                    {
                        Msg::Response(response)
                    }
                    Some(_) => continue,
                    None => break,
                },
                Some(event) = sign_in_rx.recv() => Msg::SignIn(event),
                // The user may take a while to sign in; the wait only
                // counts while they don't have to.
                () = tokio::time::sleep_until(deadline), if app.sign_in.is_none() => break,
            };
            let window_was_open = app.sign_in.is_some();
            let effects = app.update(msg);
            // Nothing is edited, nor configured, any more.
            let _ = dispatch(
                &mut app,
                &request_tx,
                &timer_tx,
                &file_tx,
                cancel.as_ref(),
                effects,
            );
            // The wait starts again once the user is done signing in.
            if window_was_open || app.sign_in.is_some() {
                deadline = tokio::time::Instant::now() + SAVE_TIMEOUT;
            }
        }
        if app.saving() {
            return Err(std::io::Error::other(
                "gave up waiting for changes to be saved; some messages may still be unread",
            ));
        }
    }
    let failures = app.save_failures();
    if !failures.is_empty() {
        return Err(std::io::Error::other(format!(
            "some changes could not be saved: {}",
            failures.join("; ")
        )));
    }
    Ok(())
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

/// Effects that the main loop carries out itself, between messages.
#[derive(Default)]
struct Deferred {
    /// Text to edit, which needs the terminal.
    edit: Option<String>,
    /// `:` commands, which need the configuration.
    commands: Vec<String>,
    /// What effects done at once answered, for the app.
    msgs: Vec<Msg>,
}

impl Deferred {
    fn absorb(&mut self, other: Deferred) {
        if other.edit.is_some() {
            self.edit = other.edit;
        }
        self.commands.extend(other.commands);
        self.msgs.extend(other.msgs);
    }
}

/// The largest file Office365 takes as an attachment.
const MAX_ATTACHMENT: u64 = 150 * 1024 * 1024;

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
        write_new_file(&path, &data)
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
/// fails partway.
fn write_new_file(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    write_new(path, data, None)
}

/// [`write_new_file`], with `permissions` given to the file before
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
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    // A name no earlier run left behind: the process, the time, and a
    // count, tried again if taken all the same.
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = loop {
        let n = OPENED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("mett-{}-{started}-{n}", std::process::id()));
        match builder.create(&dir) {
            Ok(()) => break dir,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 1000 => continue,
            Err(e) => return Err(e),
        }
    };
    // Taken away when mett exits (the viewer may still need it until
    // then), or now if it can't be opened.
    VIEWED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((dir.clone(), std::time::Instant::now()));
    let path = dir.join(name);
    let opened = std::fs::write(&path, data).and_then(|()| open::that_detached(&path));
    if opened.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    opened
}

/// The directories of attachments opened for viewing, and when.
static VIEWED: std::sync::Mutex<Vec<(std::path::PathBuf, std::time::Instant)>> =
    std::sync::Mutex::new(Vec::new());

/// How long a viewer may take to open its file: one opened since is left
/// for the next run of mett to take away.
const VIEWER_START: std::time::Duration = std::time::Duration::from_secs(60);

/// How old a copy left behind by an earlier run has to be to be taken
/// away.
const VIEWED_LEFT: std::time::Duration = std::time::Duration::from_secs(3600);

/// Removes the copies of attachments opened for viewing, but those just
/// opened, which their viewers may not have loaded yet.
fn remove_viewed() {
    for (dir, opened) in VIEWED.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
        if opened.elapsed() >= VIEWER_START {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
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
    let changed = config.changed_since(&before);
    // The mailbox and its sign-in were set up when mett started.
    if let Ok(warnings) = &mut outcome {
        warnings.extend(
            changed
                .iter()
                .filter(|name| {
                    name.starts_with("msgraph_")
                        && !["msgraph_page_size", "msgraph_sync_interval"].contains(*name)
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
            Effect::Copy(text) => {
                if let Err(e) = copy_to_clipboard(&text) {
                    app.status = format!("Could not copy: {e}");
                }
            }
            Effect::Edit(text) => deferred.edit = Some(text),
            // Files are read and written on threads of their own: a slow
            // disk (or a FIFO) doesn't hold up the screen, nor quitting.
            Effect::SaveFile {
                op,
                path,
                data,
                overwrite,
            } => {
                let files = files.clone();
                SAVING.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::thread::spawn(move || {
                    let saved = save_file(path, data, overwrite);
                    SAVING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = files.send(app::Msg::FileSaved(op, saved));
                });
            }
            Effect::OpenFile { name, data } => {
                let files = files.clone();
                SAVING.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::thread::spawn(move || {
                    let opened = open_file(&name, &data);
                    SAVING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    if let Err(e) = opened {
                        let _ = files.send(app::Msg::Failed(format!("Could not open {name}: {e}")));
                    }
                });
            }
            Effect::ReadFile(op, path) => {
                let files = files.clone();
                std::thread::spawn(move || {
                    let read = read_attachment(&path);
                    let _ = files.send(app::Msg::FileRead(op, path, read));
                });
            }
            Effect::Command(command) => deferred.commands.push(command),
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

type InputEvents = mpsc::UnboundedReceiver<std::io::Result<TermEvent>>;

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
                if events.send(event).is_err() || failed {
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
        terminal.clear()?;
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
    use super::*;

    #[test]
    fn a_command_that_breaks_the_settings_is_undone() {
        let (mut config, settings) = demo_config(Some("/home/alice".into()));
        let mut signature = None;
        let broken = run_command(
            &mut config,
            &settings.account,
            &mut signature,
            "set signature=/nonexistent/mett/signature",
        );
        assert!(broken.settings.is_err());
        // The next command works on what was there before.
        let next = run_command(
            &mut config,
            &settings.account,
            &mut signature,
            "set resolve=no",
        );
        assert!(!next.settings.unwrap().resolve);
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
        let (mut config, settings) = demo_config(Some(home.display().to_string()));
        let mut signature = None;
        // ~/.signature doesn't exist: fine as the default, not when named.
        let named = run_command(
            &mut config,
            &settings.account,
            &mut signature,
            "set signature=~/.signature",
        );
        assert!(named.settings.is_err());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn mailbox_settings_wait_for_a_restart() {
        let (mut config, settings) = demo_config(Some("/home/alice".into()));
        let mut signature = None;
        let done = run_command(
            &mut config,
            &settings.account,
            &mut signature,
            "set msgraph_client_id=abc msgraph_page_size=10",
        );
        assert_eq!(
            done.outcome,
            Ok(vec![
                "msgraph_client_id: takes effect when mett starts again".to_owned()
            ])
        );
        assert_eq!(config.string("msgraph_client_id"), "abc", "kept");
    }

    #[test]
    fn an_unreadable_signature_only_fails_commands_that_set_it() {
        let dir = std::env::temp_dir().join(format!("mett-sig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("signature");
        std::fs::write(&path, "-- \nAlice\n").unwrap();
        let (mut config, settings) = demo_config(Some("/home/alice".into()));
        let mut signature = None;
        let set = format!("set signature={}", path.display());
        let first = run_command(&mut config, &settings.account, &mut signature, &set);
        assert!(first.settings.is_ok());
        assert_eq!(signature.as_deref(), Some("-- \nAlice\n"));
        // The file goes away; commands about other things still work.
        std::fs::remove_file(&path).unwrap();
        let other = run_command(
            &mut config,
            &settings.account,
            &mut signature,
            "set resolve=no",
        );
        let other = other.settings.unwrap();
        assert!(!other.resolve);
        assert_eq!(other.compose.signature.as_deref(), Some("-- \nAlice\n"));
        // Setting it again reads it, and fails.
        let again = run_command(&mut config, &settings.account, &mut signature, &set);
        assert!(again.settings.is_ok(), "unchanged value: not read again");
        let reset = run_command(
            &mut config,
            &settings.account,
            &mut signature,
            "set signature=/nonexistent/mett/other",
        );
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
            "set msgraph_sync_interval=0",
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
    fn a_failing_command_changes_nothing() {
        let (mut config, settings) = demo_config(Some("/home/alice".into()));
        let mut signature = settings.compose.signature.clone();
        let before = config.clone();
        // The first command works, the second doesn't: neither stays.
        let done = run_command(
            &mut config,
            &settings.account,
            &mut signature,
            "set resolve=no; set timeout=soon",
        );
        assert!(done.outcome.is_err());
        assert!(config.changed_since(&before).is_empty());
    }
}
