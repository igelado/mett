//! mett: a mutt-like terminal mail client for Office365.
//!
//! The main loop wires the pieces of `docs/DESIGN.md` together: key presses
//! become [`action::Action`]s through the [`Keymap`], [`App::update`] turns them and
//! worker responses into a new state plus [`Effect`]s, effects go to the
//! [`worker`], and [`view::render`] draws the state.

mod action;
mod app;
mod config;
mod demo;
mod keymap;
mod text;
mod view;
mod worker;

use std::process::ExitCode;
use std::sync::Arc;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as TermEvent, KeyEventKind};
use tokio::sync::mpsc;

use crate::app::{App, Effect, Msg};
use crate::keymap::{Key, Keymap};
use crate::worker::{Request, RequestId};

const USAGE: &str = "usage: mett [-n] [-F <file>] [-D] [--demo]

  -F <file>  read <file> instead of the user's mettrc
  -n         don't read the system-wide mettrc (/etc/mettrc)
  -D         print every setting after reading the configuration, and exit
  --demo     open a built-in sample mailbox
  -v         print the version

mett can't open an Office365 mailbox yet; --demo shows the interface.";

/// What the command line asks for.
#[derive(Debug, Default)]
struct Options {
    files: config::Files,
    dump: bool,
    demo: bool,
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
    if matches!(args.as_slice(), [flag] if flag == "--version" || flag == "-v") {
        println!("mett {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let options = match parse_args(&args) {
        Ok(options) => options,
        Err(e) => {
            eprintln!("mett: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

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
        if config.string("msgraph_client_id").is_empty() {
            eprintln!(
                "mett: msgraph_client_id is not set; see crates/mett-msgraph/README.md to register an app"
            );
            return ExitCode::FAILURE;
        }
        eprintln!("mett: opening {spoolfile} isn't available yet; try mett --demo");
        return ExitCode::FAILURE;
    }

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("mett: cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };
    // `init` also restores the terminal if mett panics.
    let terminal = ratatui::init();
    let result = runtime.block_on(run(terminal, Arc::new(demo::mailbox())));
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
) -> std::io::Result<()> {
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
    let (mut app, effects) = App::new(size.width, size.height);
    dispatch(&request_tx, effects);

    while !app.quit {
        terminal.draw(|frame| view::render(&app, &keymap, frame))?;
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
            else => break,
        };
        if matches!(msg, Msg::Action(_)) {
            // A new action replaces the previous message, as in mutt.
            app.status.clear();
        }
        let effects = app.update(msg);
        dispatch(&request_tx, effects);
    }

    // Give pending changes (such as marking a message read) a moment to
    // reach the mailbox, rather than cancelling them by exiting.
    // The outcome is reported after the terminal is restored, so a change
    // that couldn't be saved isn't lost silently.
    if app.saving() {
        app.status = "Saving changes...".into();
        terminal.draw(|frame| view::render(&app, &keymap, frame))?;
        let deadline = tokio::time::Instant::now() + SAVE_TIMEOUT;
        while app.saving() {
            match tokio::time::timeout_at(deadline, response_rx.recv()).await {
                // Only save results matter now. Anything else (say, a late
                // message fetch) could start new work that would never be
                // sent.
                Ok(Some(response)) => {
                    if matches!(response.event, worker::Event::FlagsUpdated(_)) {
                        app.update(Msg::Response(response));
                    }
                }
                Ok(None) | Err(_) => break,
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

/// How long to wait for pending changes when quitting.
const SAVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn dispatch(requests: &mpsc::UnboundedSender<(RequestId, Request)>, effects: Vec<Effect>) {
    for Effect::Request(id, request) in effects {
        // The worker only stops when this sender is dropped.
        let _ = requests.send((id, request));
    }
}
