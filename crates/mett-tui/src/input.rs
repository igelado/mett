//! The thread that reads the terminal's input.

use crate::UserEvent;
use crate::frontend::clear_screen;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event;
use tokio::sync::mpsc;

/// The thread that reads terminal input. crossterm's reads block, so they
/// happen on a plain thread and are forwarded to the async loop. The
/// thread can be paused, so that it doesn't take keys meant for the
/// editor.
pub(crate) struct Input {
    control: std::sync::mpsc::Sender<Pause>,
}

/// A request to stop reading input: the thread acknowledges it, then waits
/// until `resume` is dropped.
struct Pause {
    paused: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

pub(crate) type InputEvents = mpsc::UnboundedReceiver<std::io::Result<UserEvent>>;

impl Input {
    /// How long a read waits before checking for a pause.
    const POLL: std::time::Duration = std::time::Duration::from_millis(50);

    pub(crate) fn start() -> (Self, InputEvents) {
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
    pub(crate) fn suspended<T>(
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
