//! mett: a mutt-like terminal mail client for Office365.
//!
//! The main loop wires the pieces of `docs/DESIGN.md` together: key presses
//! become [`action::Action`]s through the [`Keymap`], [`App::update`] turns them and
//! worker responses into a new state plus [`Effect`]s, effects go to the
//! [`worker`], and [`view::render`] draws the state.

mod account;
mod action;
mod app;
mod compose;
mod config;
mod demo;
mod editor;
mod keymap;
mod lineedit;
mod signin;
mod text;
mod view;
mod worker;

use std::process::ExitCode;
use std::sync::Arc;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as TermEvent, KeyEventKind};
use tokio::sync::mpsc;

use crate::app::{App, Effect, Msg, Settings};
use crate::config::mailbox::FolderPath;
use crate::editor::Editor;
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
        let compose = match compose::Settings::from_config(&config, &spoolfile.account) {
            Ok(compose) => compose,
            Err(e) => {
                eprintln!("mett: {e}");
                return ExitCode::FAILURE;
            }
        };
        let editor = Editor::new(config.string("editor"), config.string("tmpdir"), &|name| {
            std::env::var(name).ok()
        });
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
            settings: Settings { page_size, compose },
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
    let session = Session {
        folder: FolderPath::WellKnown(mett_core::WellKnownFolder::Inbox, Vec::new()),
        settings: demo::settings(),
        editor: Editor::new("", "", &|name| std::env::var(name).ok()),
        sign_in,
        cancel: None,
    };
    start(&runtime, Arc::new(demo::mailbox()), session)
}

/// What the TUI works with, besides the mailbox.
struct Session {
    /// The folder to open.
    folder: FolderPath,
    /// The app's settings.
    settings: Settings,
    /// The editor for writing messages.
    editor: Editor,
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
fn start<S: mett_core::MailStore + mett_core::MailSender + 'static>(
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

async fn run<S: mett_core::MailStore + mett_core::MailSender + 'static>(
    mut terminal: DefaultTerminal,
    store: Arc<S>,
    session: Session,
) -> std::io::Result<()> {
    let Session {
        folder,
        settings,
        editor,
        sign_in: mut sign_in_rx,
        cancel,
    } = session;
    let keymap = Keymap::mutt_defaults();

    let (request_tx, request_rx) = mpsc::unbounded_channel::<(RequestId, Request)>();
    let (response_tx, mut response_rx) = mpsc::unbounded_channel();
    tokio::spawn(worker::run(store, request_rx, response_tx));

    let (input, mut input_rx) = Input::start();
    let (timer_tx, mut timer_rx) = mpsc::unbounded_channel();

    let size = terminal.size()?;
    let (mut app, effects) = App::open(size.width, size.height, folder, settings);
    let mut edit = dispatch(&mut app, &request_tx, &timer_tx, cancel.as_ref(), effects);

    while !app.quit {
        // The editor runs between messages, with the terminal handed over.
        while let Some(text) = edit.take() {
            let result = input.suspended(&mut terminal, &mut input_rx, || editor.edit(&text))?;
            // The terminal may have been resized meanwhile.
            let size = terminal.size()?;
            app.update(Msg::Resize {
                width: size.width,
                height: size.height,
            });
            let effects = app.update(Msg::Edited(result));
            edit = dispatch(&mut app, &request_tx, &timer_tx, cancel.as_ref(), effects);
        }
        draw(&mut terminal, &app, &keymap)?;
        let msg = tokio::select! {
            // Without input mett can't be used or quit, so a terminal read
            // error ends it (and the terminal is restored).
            event = input_rx.recv() => match event.transpose()? {
                None => return Err(std::io::Error::other("terminal input closed")),
                Some(event) => match event {
                // Repeats too, so holding a key keeps moving.
                // A question on the message line takes keys as typed.
                TermEvent::Key(key) if key.kind != KeyEventKind::Release
                    && app.input.is_some() && app.sign_in.is_none() => Msg::Key(key),
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
            Some(timer) = timer_rx.recv() => Msg::Timer(timer),
            else => break,
        };
        if matches!(msg, Msg::Action(_)) {
            // A new action replaces the previous message, as in mutt.
            app.status.clear();
        }
        let effects = app.update(msg);
        edit = dispatch(&mut app, &request_tx, &timer_tx, cancel.as_ref(), effects);
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
        dispatch(&mut app, &request_tx, &timer_tx, cancel.as_ref(), effects);
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
                dispatch(&mut app, &request_tx, &timer_tx, cancel.as_ref(), effects);
            }
            // A sign-in window keeps mett here only while a change is
            // still unsaved, so the user can sign in or retry it; other
            // sign-ins don't matter any more.
            let unsaved = !app.save_failures().is_empty();
            let waiting = app.saving() || (unsaved && app.sign_in.is_some());
            if !waiting {
                break;
            }
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
                    Some(response)
                        if matches!(
                            response.event,
                            worker::Event::FlagsUpdated(_)
                                | worker::Event::Sent(_)
                                | worker::Event::DraftSaved(_)
                                | worker::Event::KeptDiscarded(_)
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
            // Nothing is edited any more.
            let _ = dispatch(&mut app, &request_tx, &timer_tx, cancel.as_ref(), effects);
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

/// Carries out `effects`, except editing, which needs the terminal: the
/// text to edit, if any, is returned.
fn dispatch(
    app: &mut App,
    requests: &mpsc::UnboundedSender<(RequestId, Request)>,
    timers: &mpsc::UnboundedSender<app::Timer>,
    cancel: Option<&CancelHandle>,
    effects: Vec<Effect>,
) -> Option<String> {
    let mut edit = None;
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
            Effect::Edit(text) => edit = Some(text),
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
    edit
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
