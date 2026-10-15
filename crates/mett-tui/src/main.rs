//! mett: a mutt-like terminal mail client for Office365.
//!
//! The main loop wires the pieces of `docs/DESIGN.md` together: key presses
//! become [`action::Action`]s through the [`Keymap`], [`App::update`] turns them and
//! worker responses into a new state plus [`Effect`]s, effects go to the
//! [`worker`], and [`view::render`] draws the state.

mod account;
mod action;
mod app;
mod config;
mod demo;
mod keymap;
mod signin;
mod text;
mod view;
mod worker;

use std::process::ExitCode;
use std::sync::Arc;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as TermEvent, KeyEventKind};
use tokio::sync::mpsc;

use crate::app::{App, Effect, Msg};
use crate::config::mailbox::FolderPath;
use crate::keymap::{Key, Keymap};
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
        let page_size = match config.get("msgraph_page_size") {
            Some(config::Value::Number(n)) if *n > 0 => match u32::try_from(*n) {
                Ok(size) => size,
                Err(_) => {
                    eprintln!("mett: msgraph_page_size {n} is too large");
                    return ExitCode::FAILURE;
                }
            },
            _ => {
                eprintln!("mett: msgraph_page_size must be a positive number");
                return ExitCode::FAILURE;
            }
        };
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
        let session = Session {
            folder: spoolfile.folder,
            page_size,
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
    let session = Session {
        folder: FolderPath::WellKnown(mett_core::WellKnownFolder::Inbox, Vec::new()),
        page_size: app::PAGE_SIZE,
        sign_in,
        cancel: None,
    };
    start(&runtime, Arc::new(demo::mailbox()), session)
}

/// What the TUI works with, besides the mailbox.
struct Session {
    /// The folder to open.
    folder: FolderPath,
    /// How many messages to list per request.
    page_size: u32,
    /// Events from the mailbox's sign-in.
    sign_in: mpsc::UnboundedReceiver<SignInEvent>,
    /// Cancels the sign-in in progress, for mailboxes that sign in.
    cancel: Option<CancelHandle>,
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
fn start<S: mett_core::MailStore + 'static>(
    runtime: &tokio::runtime::Runtime,
    store: Arc<S>,
    session: Session,
) -> ExitCode {
    // `init` also restores the terminal if mett panics.
    let terminal = ratatui::init();
    let result = runtime.block_on(run(terminal, store, session));
    ratatui::restore();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mett: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run<S: mett_core::MailStore + 'static>(
    mut terminal: DefaultTerminal,
    store: Arc<S>,
    session: Session,
) -> std::io::Result<()> {
    let Session {
        folder,
        page_size,
        sign_in: mut sign_in_rx,
        cancel,
    } = session;
    let keymap = Keymap::mutt_defaults();

    let (request_tx, request_rx) = mpsc::unbounded_channel::<(RequestId, Request)>();
    let (response_tx, mut response_rx) = mpsc::unbounded_channel();
    tokio::spawn(worker::run(store, request_rx, response_tx));

    // Terminal input is read on a plain thread, since crossterm's reads
    // block, and forwarded to the async loop.
    let (input_tx, mut input_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        loop {
            let event = event::read();
            let failed = event.is_err();
            if input_tx.send(event).is_err() || failed {
                break;
            }
        }
    });

    let size = terminal.size()?;
    let (mut app, effects) = App::open(size.width, size.height, folder, page_size);
    dispatch(&mut app, &request_tx, cancel.as_ref(), effects);

    while !app.quit {
        draw(&mut terminal, &app, &keymap)?;
        let msg = tokio::select! {
            // Without input mett can't be used or quit, so a terminal read
            // error ends it (and the terminal is restored).
            event = input_rx.recv() => match event.transpose()? {
                None => return Err(std::io::Error::other("terminal input closed")),
                Some(event) => match event {
                // Repeats too, so holding a key keeps moving.
                TermEvent::Key(key) if key.kind != KeyEventKind::Release => {
                    match keymap.lookup(app.menu(), Key::from(key)) {
                        Some(action) => Msg::Action(action),
                        None => {
                            app.status = "Key is not bound.".into();
                            // Restarts the loop, which redraws first.
                            continue;
                        }
                    }
                }
                TermEvent::Resize(width, height) => Msg::Resize { width, height },
                _ => continue,
                },
            },
            Some(response) = response_rx.recv() => Msg::Response(response),
            Some(event) = sign_in_rx.recv() => Msg::SignIn(event),
            else => break,
        };
        if matches!(msg, Msg::Action(_)) {
            // A new action replaces the previous message, as in mutt.
            app.status.clear();
        }
        let effects = app.update(msg);
        dispatch(&mut app, &request_tx, cancel.as_ref(), effects);
    }

    // Give pending changes (such as marking a message read) a moment to
    // reach the mailbox, rather than cancelling them by exiting.
    // The outcome is reported after the terminal is restored, so a change
    // that couldn't be saved isn't lost silently.
    if app.saving() {
        app.status = "Saving changes...".into();
        let mut deadline = tokio::time::Instant::now() + SAVE_TIMEOUT;
        // A failed sign-in stays up even with nothing left pending, so the
        // user can retry the change it held up.
        while app.saving() || app.sign_in.is_some() {
            draw(&mut terminal, &app, &keymap)?;
            let msg = tokio::select! {
                // Saving may need the user to sign in: the sign-in window
                // still works, and only it takes keys.
                event = input_rx.recv() => match event.transpose()? {
                    None => break,
                    Some(TermEvent::Key(key))
                        if key.kind != KeyEventKind::Release && app.sign_in.is_some() =>
                    {
                        match keymap.lookup(app.menu(), Key::from(key)) {
                            Some(action) => Msg::Action(action),
                            None => continue,
                        }
                    }
                    Some(TermEvent::Resize(width, height)) => Msg::Resize { width, height },
                    Some(_) => continue,
                },
                response = response_rx.recv() => match response {
                    // Only save results matter now. Anything else (say, a
                    // late message fetch) could start new work that would
                    // never be sent.
                    Some(response) if matches!(response.event, worker::Event::FlagsUpdated(_)) => {
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
            dispatch(&mut app, &request_tx, cancel.as_ref(), effects);
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
fn draw(terminal: &mut DefaultTerminal, app: &App, keymap: &Keymap) -> std::io::Result<()> {
    use std::io::Write;

    use ratatui::crossterm::{cursor, queue, style};

    let mut overlay = None;
    terminal.draw(|frame| overlay = view::render(app, keymap, frame))?;
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

/// Carries out `effects`.
fn dispatch(
    app: &mut App,
    requests: &mpsc::UnboundedSender<(RequestId, Request)>,
    cancel: Option<&CancelHandle>,
    effects: Vec<Effect>,
) {
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
        }
    }
}
