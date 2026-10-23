//! What the app is drawn on: the terminal, or the GUI's window.

use crate::app::App;
use crate::editor::Editor;
use crate::input::Input;
use crate::input::InputEvents;
use crate::view;
use ratatui::DefaultTerminal;

/// What the app is drawn on, and what it can't do without: the terminal,
/// or the GUI's window.
pub(crate) trait Frontend {
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
pub(crate) struct TerminalScreen {
    pub(crate) terminal: DefaultTerminal,
    pub(crate) input: Input,
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

/// Clears the screen. Unlike `Terminal::clear`, this doesn't ask the
/// terminal where the cursor is, which one that doesn't answer would make
/// fail; a terminal just made has nothing drawn to forget.
pub(crate) fn clear_screen() -> std::io::Result<()> {
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
