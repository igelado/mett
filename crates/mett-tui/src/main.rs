//! mett: a mutt-like terminal mail client for Office365.
//!
//! The main loop wires the pieces of `docs/DESIGN.md` together: key presses
//! become [`action::Action`]s through the [`Keymap`], [`App::update`] turns them and
//! worker responses into a new state plus [`Effect`]s, effects go to the
//! [`worker`], and [`view::render`] draws the state.

mod action;
mod app;
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

const USAGE: &str = "usage: mett --demo

mett can't connect to Office365 yet. `mett --demo` opens a built-in sample
mailbox to try the interface.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [flag] if flag == "--demo" => {}
        [flag] if flag == "--version" || flag == "-v" => {
            println!("mett {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
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
                TermEvent::Key(key) if key.kind == KeyEventKind::Press => {
                    match keymap.lookup(app.menu(), Key::from(key)) {
                        Some(action) => Msg::Action(action),
                        None => {
                            app.status = "Key is not bound.".into();
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
    if app.saving() {
        app.status = "Saving changes...".into();
        terminal.draw(|frame| view::render(&app, &keymap, frame))?;
        let deadline = tokio::time::Instant::now() + SAVE_TIMEOUT;
        while app.saving() {
            match tokio::time::timeout_at(deadline, response_rx.recv()).await {
                Ok(Some(response)) => {
                    app.update(Msg::Response(response));
                }
                Ok(None) | Err(_) => break,
            }
        }
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
