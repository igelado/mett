//! mett in a window of its own (`mett-gui`), with the same screens as in
//! the terminal.
//!
//! The window is winit's, and ratatui-wgpu draws the screen's cells in it.
//! The app runs as it does in the terminal ([`crate::run`]), on a thread of
//! its own: it draws each screen into cells ([`Capture`]) and hands them to
//! the window's thread, which shows them; keys go the other way, as the
//! crossterm events a terminal would give. Messages are written in the
//! built-in editor: there is no terminal for the user's own.

use std::num::NonZeroU32;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use ratatui::Terminal;
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::{Buffer, Cell};
use ratatui::crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Size};
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};
use ratatui_wgpu::shaders::AspectPreservingDefaultPostProcessor;
use ratatui_wgpu::{Builder, Dimensions, Font, WgpuBackend};
use tokio::sync::mpsc;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{Window, WindowId};

use crate::app::App;
use crate::editor::Editor;
use crate::{InputEvents, Launch, Session, UserEvent};

/// The font used unless `gui_font` names another.
static FONT: &[u8] = include_bytes!("../assets/fonts/CascadiaMono-Regular.ttf");

/// The font size unless `gui_font_size` says otherwise, in points.
const FONT_SIZE: u32 = 14;

/// The size of a new window, in (logical) pixels.
const WINDOW_SIZE: winit::dpi::LogicalSize<u32> = winit::dpi::LogicalSize::new(1000, 700);

/// Runs mett in a window: the `mett-gui` program.
pub fn main() -> ExitCode {
    crate::platform::set_gui();
    let mut launch = InWindow::default();
    let code = crate::mett_main(&mut launch);
    if launch.started {
        return code;
    }
    // mett didn't start: why, in a window, as there may be no terminal to
    // tell it in (and `-v` and the like said it there).
    let said = std::mem::take(&mut launch.said);
    if code != ExitCode::SUCCESS && !said.is_empty() {
        show(Shown::Ended(code, said), None);
    }
    code
}

/// mett in a window: what it said before starting, and whether it did.
#[derive(Default)]
struct InWindow {
    said: Vec<String>,
    started: bool,
}

impl Launch for &mut InWindow {
    fn say(&mut self, message: &str) {
        eprintln!("mett: {message}");
        self.said.push(message.to_owned());
    }

    fn start<S: mett_core::MailStore + mett_core::MailSender + 'static>(
        self,
        runtime: tokio::runtime::Runtime,
        store: Arc<S>,
        mut session: Session,
    ) -> ExitCode {
        self.started = true;
        crate::remove_viewed_before();
        // Warnings from the configuration go on the message line.
        session.notice = Some(self.said.join("; ")).filter(|notice| !notice.is_empty());
        let font = Fonts::from_config(&session.config);
        let start: StartApp = Box::new(move |screen, events| {
            let result = runtime.block_on(crate::run(screen, events, store, session));
            crate::wait_for_saves();
            result
        });
        show(Shown::Starting, Some((start, font)))
    }
}

/// Starts the app, on the screen given, with the input given; returns how
/// it ended.
type StartApp = Box<dyn FnOnce(WindowScreen, InputEvents) -> std::io::Result<()> + Send>;

/// What the window shows.
enum Shown {
    /// Nothing yet: the app is starting.
    Starting,
    /// A screen the app drew: its cells, and the cursor, if shown.
    Screen(Buffer, Option<Position>),
    /// The app ended, with this exit code, and what it had to say. The
    /// window shows that until the user presses a key or closes it.
    Ended(ExitCode, Vec<String>),
}

/// The fonts to draw with: `gui_font` (else Cascadia Mono), at
/// `gui_font_size` points, with the system's monospaced fonts
/// for what it has no glyphs for.
struct Fonts {
    main: &'static [u8],
    size: u32,
}

impl Fonts {
    fn from_config(config: &crate::config::Config) -> Self {
        let main = match config.string("gui_font") {
            "" => FONT,
            path => match std::fs::read(path) {
                // Kept for as long as mett runs.
                Ok(data) if Font::new(&data).is_some() => Vec::leak(data),
                Ok(_) => {
                    eprintln!("mett: gui_font: {path} isn't a font mett can use");
                    FONT
                }
                Err(e) => {
                    eprintln!("mett: gui_font: {path}: {e}");
                    FONT
                }
            },
        };
        let size = match config.get("gui_font_size") {
            Some(crate::config::Value::Number(n)) if *n > 0 => {
                u32::try_from(*n).unwrap_or(FONT_SIZE)
            }
            _ => FONT_SIZE,
        };
        Self { main, size }
    }

    /// The system's monospaced fonts, to fall back on for what the font has
    /// no glyphs for: those at least as wide as it, as ratatui-wgpu makes
    /// every cell as narrow as the narrowest font. At most a few dozen
    /// megabytes of them; they are kept for as long as mett runs.
    fn fallbacks(&self) -> Vec<Font<'static>> {
        const MAX_BYTES: usize = 64 << 20;
        let Some(width) = aspect(self.main) else {
            return Vec::new();
        };
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let mut families = std::collections::HashSet::new();
        let mut bytes = 0;
        let mut fonts = Vec::new();
        for face in db.faces() {
            let regular =
                face.style == fontdb::Style::Normal && face.weight == fontdb::Weight::NORMAL;
            if face.index != 0 || !face.monospaced || !regular {
                continue;
            }
            let family = face.families.first().map(|(name, _)| name.clone());
            if !families.insert(family) {
                continue;
            }
            let Some(data) = db.with_face_data(face.id, |data, _| data.to_vec()) else {
                continue;
            };
            if bytes + data.len() > MAX_BYTES || aspect(&data).is_none_or(|w| w < width) {
                continue;
            }
            bytes += data.len();
            if let Some(font) = Font::new(Vec::leak(data)) {
                fonts.push(font);
            }
        }
        fonts
    }
}

/// How wide a font's `m` is, for its height.
fn aspect(data: &[u8]) -> Option<f32> {
    let face = ttf_parser::Face::parse(data, 0).ok()?;
    let advance = face.glyph_hor_advance(face.glyph_index('m')?)?;
    Some(f32::from(advance) / f32::from(face.height()))
}

/// Shows the window until it closes: the app's screens, started with
/// `start` (with `fonts`), or else what `shown` says. Returns the app's
/// exit code.
fn show(shown: Shown, start: Option<(StartApp, Fonts)>) -> ExitCode {
    let event_loop = match EventLoop::<Shown>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(e) => {
            eprintln!("mett: cannot open a window: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (fonts, start) = match start {
        Some((start, fonts)) => (fonts, Some(start)),
        None => (
            Fonts {
                main: FONT,
                size: FONT_SIZE,
            },
            None,
        ),
    };
    let mut gui = Gui {
        proxy: event_loop.create_proxy(),
        start,
        fonts,
        shown,
        window: None,
        terminal: None,
        size: Arc::new(Mutex::new(Size::default())),
        input: None,
        modifiers: ModifiersState::empty(),
        code: ExitCode::SUCCESS,
    };
    if let Err(e) = event_loop.run_app(&mut gui) {
        eprintln!("mett: {e}");
        return ExitCode::FAILURE;
    }
    gui.code
}

/// How the cells are drawn on the window: pixel for pixel. (Stretched to
/// fill it, some rows would come out blurred.)
type Pixels = AspectPreservingDefaultPostProcessor;

/// The window, and the app shown in it.
struct Gui {
    /// For the app's thread to reach the window's.
    proxy: EventLoopProxy<Shown>,
    /// Starts the app, once the window is open.
    start: Option<StartApp>,
    fonts: Fonts,
    /// What the window shows.
    shown: Shown,
    window: Option<Arc<Window>>,
    terminal: Option<Terminal<WgpuBackend<'static, 'static, Pixels>>>,
    /// The window's size, in cells, for the app.
    size: Arc<Mutex<Size>>,
    /// Where keys go: the app.
    input: Option<mpsc::UnboundedSender<std::io::Result<UserEvent>>>,
    modifiers: ModifiersState,
    /// How the app ended.
    code: ExitCode,
}

impl Gui {
    /// Opens the window, and starts the app in it.
    fn open(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let attributes = Window::default_attributes()
            .with_title("mett")
            .with_inner_size(WINDOW_SIZE);
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .map_err(|e| e.to_string())?,
        );
        let scale = window.scale_factor();
        // Points to pixels: a point is 1/72 inch, a logical pixel 1/96.
        let font_px = (f64::from(self.fonts.size) * 96.0 / 72.0 * scale).round();
        // The window grows and shrinks by whole cells, where the system
        // lets it (the cells are drawn from its top left corner).
        if let Some(aspect) = aspect(self.fonts.main) {
            let cell =
                winit::dpi::PhysicalSize::new((f64::from(aspect) * font_px).floor(), font_px);
            window.set_resize_increments(Some(cell));
        }
        let font = Font::new(self.fonts.main).ok_or("the font can't be read")?;
        let size = window.inner_size();
        // The font first, then the others; it is also the last resort.
        let builder = Builder::from_font(font.clone())
            .with_regular_fonts([font])
            .with_regular_fonts(self.fonts.fallbacks())
            .with_font_size_px(font_px as u32)
            .with_width_and_height(Dimensions {
                width: NonZeroU32::new(size.width.max(1)).expect("not zero"),
                height: NonZeroU32::new(size.height.max(1)).expect("not zero"),
            });
        let backend = pollster::block_on(builder.build_with_target(window.clone()))
            .map_err(|e| format!("cannot draw in the window: {e}"))?;
        let terminal = Terminal::new(backend).map_err(|e| e.to_string())?;
        *self.size.lock().expect("not poisoned") = terminal.size().map_err(|e| e.to_string())?;
        self.terminal = Some(terminal);
        self.window = Some(window.clone());
        if let Some(start) = self.start.take() {
            let (input, events) = mpsc::unbounded_channel();
            self.input = Some(input);
            let screen = WindowScreen {
                size: self.size.clone(),
                proxy: self.proxy.clone(),
                capture: Terminal::new(Capture {
                    size: self.size.clone(),
                    cursor: None,
                })
                .map_err(|e| e.to_string())?,
                clipboard: None,
            };
            let proxy = self.proxy.clone();
            std::thread::spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    start(screen, events)
                }));
                let ended = match result {
                    Ok(Ok(())) => Shown::Ended(ExitCode::SUCCESS, Vec::new()),
                    Ok(Err(e)) => Shown::Ended(ExitCode::FAILURE, vec![e.to_string()]),
                    Err(_) => Shown::Ended(
                        ExitCode::FAILURE,
                        vec!["mett stopped: something went wrong".into()],
                    ),
                };
                let _ = proxy.send_event(ended);
            });
        }
        window.request_redraw();
        Ok(())
    }

    /// Sends the app what the user did.
    fn send(&self, event: UserEvent) {
        if let Some(input) = &self.input {
            let _ = input.send(Ok(event));
        }
    }

    /// Draws what is shown.
    fn draw(&mut self) {
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        let shown = &self.shown;
        let drawn = terminal.draw(|frame| match shown {
            Shown::Starting => {}
            Shown::Screen(buffer, cursor) => {
                let area = frame.area();
                let target = frame.buffer_mut();
                // The window may have changed size since the app drew.
                for y in 0..area.height.min(buffer.area.height) {
                    for x in 0..area.width.min(buffer.area.width) {
                        target[(x, y)] = buffer[(x, y)].clone();
                    }
                }
                // ratatui-wgpu draws no cursor: it is the cell in reverse,
                // unless that is how the cell is already (the built-in
                // editor's cursor).
                if let Some(cursor) = cursor
                    && area.contains(*cursor)
                {
                    let cell: &mut Cell = &mut target[(cursor.x, cursor.y)];
                    if !cell.modifier.contains(Modifier::REVERSED) {
                        cell.modifier.insert(Modifier::REVERSED);
                    }
                }
            }
            Shown::Ended(_, said) => {
                let mut lines: Vec<Line> = said
                    .iter()
                    .map(|s| Line::raw(format!("mett: {s}")))
                    .collect();
                lines.push(Line::raw(""));
                lines.push(Line::raw("Press any key to close this window."));
                frame.render_widget(
                    Paragraph::new(lines).wrap(Wrap { trim: false }),
                    frame.area(),
                );
            }
        });
        if let Err(e) = drawn {
            eprintln!("mett: cannot draw: {e}");
        }
    }
}

impl ApplicationHandler<Shown> for Gui {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        if let Err(e) = self.open(event_loop) {
            eprintln!("mett: {e}");
            self.code = ExitCode::FAILURE;
            event_loop.exit();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, shown: Shown) {
        if let Shown::Ended(code, said) = &shown {
            self.code = *code;
            self.input = None;
            // Nothing to say: the window goes with the app.
            if said.is_empty() {
                event_loop.exit();
                return;
            }
        }
        self.shown = shown;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let ended = matches!(self.shown, Shown::Ended(..));
        match event {
            // The app decides: it may have a message to send first.
            WindowEvent::CloseRequested if !ended && self.input.is_some() => {
                self.send(UserEvent::Close);
            }
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                let Some(terminal) = &mut self.terminal else {
                    return;
                };
                terminal.backend_mut().resize(size.width, size.height);
                if let Ok(cells) = terminal.size() {
                    let changed = {
                        let mut shared = self.size.lock().expect("not poisoned");
                        std::mem::replace(&mut *shared, cells) != cells
                    };
                    if changed {
                        self.send(UserEvent::Term(TermEvent::Resize(
                            cells.width,
                            cells.height,
                        )));
                    }
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if ended {
                    event_loop.exit();
                    return;
                }
                let plain = event.key_without_modifiers();
                if let Some(key) = key(
                    &event.logical_key,
                    &plain,
                    event.text.as_deref(),
                    self.modifiers,
                ) {
                    self.send(UserEvent::Term(TermEvent::Key(key)));
                }
            }
            WindowEvent::RedrawRequested => self.draw(),
            _ => {}
        }
    }
}

/// The key a terminal would report for a key press: `logical`, which the
/// keyboard's layout and modifiers gave, `plain` without the modifiers, and
/// the `text` it types (dead keys and compose sequences included), with
/// `modifiers` held.
fn key(
    logical: &Key,
    plain: &Key,
    text: Option<&str>,
    modifiers: ModifiersState,
) -> Option<KeyEvent> {
    let (shift, ctrl, alt) = (
        modifiers.shift_key(),
        modifiers.control_key(),
        modifiers.alt_key(),
    );
    // Command (and the Windows key) combinations are the system's.
    if modifiers.super_key() {
        return None;
    }
    let mut held = KeyModifiers::NONE;
    held.set(KeyModifiers::CONTROL, ctrl);
    held.set(KeyModifiers::ALT, alt);
    let named = |code: KeyCode| {
        let mut held = held;
        held.set(KeyModifiers::SHIFT, shift);
        Some(KeyEvent::new(code, held))
    };
    let character = |key: &Key| match key {
        Key::Character(s) => single(s),
        Key::Named(NamedKey::Space) => Some(' '),
        _ => None,
    };
    match logical {
        Key::Named(NamedKey::Enter) => named(KeyCode::Enter),
        Key::Named(NamedKey::Tab) if shift => named(KeyCode::BackTab),
        Key::Named(NamedKey::Tab) => named(KeyCode::Tab),
        Key::Named(NamedKey::Backspace) => named(KeyCode::Backspace),
        Key::Named(NamedKey::Escape) => named(KeyCode::Esc),
        Key::Named(NamedKey::Delete) => named(KeyCode::Delete),
        Key::Named(NamedKey::Insert) => named(KeyCode::Insert),
        Key::Named(NamedKey::Home) => named(KeyCode::Home),
        Key::Named(NamedKey::End) => named(KeyCode::End),
        Key::Named(NamedKey::PageUp) => named(KeyCode::PageUp),
        Key::Named(NamedKey::PageDown) => named(KeyCode::PageDown),
        Key::Named(NamedKey::ArrowUp) => named(KeyCode::Up),
        Key::Named(NamedKey::ArrowDown) => named(KeyCode::Down),
        Key::Named(NamedKey::ArrowLeft) => named(KeyCode::Left),
        Key::Named(NamedKey::ArrowRight) => named(KeyCode::Right),
        Key::Named(named_key) if let Some(n) = function_key(*named_key) => named(KeyCode::F(n)),
        _ => {
            let typed = character(logical)?;
            let base = character(plain).unwrap_or(typed);
            let c = if ctrl && alt && typed != base && !typed.is_control() {
                // AltGr (Control and Alt on Windows) typed a character of
                // its own (`@` on German keyboards): it is just typed.
                return Some(KeyEvent::from(KeyCode::Char(typed)));
            } else if ctrl {
                // Control and a letter, as a terminal has it.
                base.to_ascii_lowercase()
            } else if alt {
                // Meta: the key itself, not what Option makes of it on a
                // Mac.
                if typed.is_ascii() {
                    typed
                } else if shift {
                    base.to_ascii_uppercase()
                } else {
                    base
                }
            } else {
                // What it types, after a dead key too (`´` then `e`).
                text.and_then(single)
                    .filter(|c| !c.is_control())
                    .unwrap_or(typed)
            };
            // As crossterm reports them: Shift only with capital letters.
            held.set(KeyModifiers::SHIFT, c.is_uppercase());
            Some(KeyEvent::new(KeyCode::Char(c), held))
        }
    }
}

/// The one character `s` is, if it is one.
fn single(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

/// The number of function key `key`, if it is one.
fn function_key(key: NamedKey) -> Option<u8> {
    use NamedKey::*;
    let keys = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12];
    keys.iter()
        .position(|k| *k == key)
        .and_then(|i| u8::try_from(i + 1).ok())
}

/// The window, as the app's thread draws on it.
pub(crate) struct WindowScreen {
    /// The window's size, in cells.
    size: Arc<Mutex<Size>>,
    /// Where screens go.
    proxy: EventLoopProxy<Shown>,
    /// Draws a screen into cells.
    capture: Terminal<Capture>,
    /// The system's clipboard, once used.
    clipboard: Option<arboard::Clipboard>,
}

impl crate::Frontend for WindowScreen {
    fn size(&self) -> std::io::Result<Size> {
        Ok(*self.size.lock().expect("not poisoned"))
    }

    fn draw(&mut self, app: &App) -> std::io::Result<()> {
        let mut cells = None;
        self.capture.draw(|frame| {
            // (The overlay is plain text the cells already hold.)
            let _ = crate::view::render(app, frame);
            cells = Some(frame.buffer_mut().clone());
        })?;
        let cursor = self.capture.backend().cursor;
        let cells = cells.expect("drawn");
        self.proxy
            .send_event(Shown::Screen(cells, cursor))
            .map_err(|_| std::io::Error::other("the window closed"))
    }

    fn edit(
        &mut self,
        _: &Editor,
        _: &mut InputEvents,
        _: &str,
    ) -> std::io::Result<Result<String, String>> {
        // The built-in editor is always used: there is no terminal.
        Ok(Err("only the built-in editor works in a window".into()))
    }

    fn copy(&mut self, text: &str) -> std::io::Result<()> {
        if self.clipboard.is_none() {
            self.clipboard = Some(arboard::Clipboard::new().map_err(std::io::Error::other)?);
        }
        let clipboard = self.clipboard.as_mut().expect("just set");
        clipboard.set_text(text).map_err(std::io::Error::other)
    }
}

/// A ratatui backend that draws nowhere: the app's screens are taken from
/// the frame, and the cursor from what ratatui tells it.
pub(crate) struct Capture {
    size: Arc<Mutex<Size>>,
    /// Where the cursor is, if shown.
    cursor: Option<Position>,
}

impl Backend for Capture {
    type Error = std::io::Error;

    fn draw<'a, I>(&mut self, _: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        Ok(())
    }

    fn hide_cursor(&mut self) -> std::io::Result<()> {
        self.cursor = None;
        Ok(())
    }

    fn show_cursor(&mut self) -> std::io::Result<()> {
        self.cursor.get_or_insert_default();
        Ok(())
    }

    fn get_cursor_position(&mut self) -> std::io::Result<Position> {
        Ok(self.cursor.unwrap_or_default())
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> std::io::Result<()> {
        self.cursor = Some(position.into());
        Ok(())
    }

    fn clear(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn clear_region(&mut self, _: ClearType) -> std::io::Result<()> {
        Ok(())
    }

    fn size(&self) -> std::io::Result<Size> {
        Ok(*self.size.lock().expect("not poisoned"))
    }

    fn window_size(&mut self) -> std::io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.size()?,
            pixels: Size::default(),
        })
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(
        logical: Key,
        plain: Key,
        text: Option<&str>,
        modifiers: ModifiersState,
    ) -> Option<KeyEvent> {
        key(&logical, &plain, text, modifiers)
    }

    fn ch(s: &str) -> Key {
        Key::Character(s.into())
    }

    const NONE: ModifiersState = ModifiersState::empty();

    #[test]
    fn keys_are_those_a_terminal_reports() {
        let event = |code, modifiers| Some(KeyEvent::new(code, modifiers));
        assert_eq!(
            press(ch("a"), ch("a"), Some("a"), NONE),
            event(KeyCode::Char('a'), KeyModifiers::NONE)
        );
        assert_eq!(
            press(ch("A"), ch("a"), Some("A"), ModifiersState::SHIFT),
            event(KeyCode::Char('A'), KeyModifiers::SHIFT)
        );
        assert_eq!(
            press(ch("!"), ch("1"), Some("!"), ModifiersState::SHIFT),
            event(KeyCode::Char('!'), KeyModifiers::NONE)
        );
        assert_eq!(
            press(ch("a"), ch("a"), None, ModifiersState::CONTROL),
            event(KeyCode::Char('a'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            press(
                Key::Named(NamedKey::Space),
                Key::Named(NamedKey::Space),
                None,
                ModifiersState::CONTROL
            ),
            event(KeyCode::Char(' '), KeyModifiers::CONTROL)
        );
        assert_eq!(
            press(
                Key::Named(NamedKey::Tab),
                Key::Named(NamedKey::Tab),
                None,
                ModifiersState::SHIFT
            ),
            event(KeyCode::BackTab, KeyModifiers::SHIFT)
        );
        assert_eq!(
            press(
                Key::Named(NamedKey::F3),
                Key::Named(NamedKey::F3),
                None,
                NONE
            ),
            event(KeyCode::F(3), KeyModifiers::NONE)
        );
        assert_eq!(
            press(
                Key::Named(NamedKey::ArrowLeft),
                Key::Named(NamedKey::ArrowLeft),
                None,
                ModifiersState::SHIFT
            ),
            event(KeyCode::Left, KeyModifiers::SHIFT)
        );
    }

    #[test]
    fn meta_altgr_and_dead_keys() {
        let event = |code, modifiers| Some(KeyEvent::new(code, modifiers));
        // Option+x on a Mac types `≈`; it is Meta and `x`.
        assert_eq!(
            press(ch("≈"), ch("x"), Some("≈"), ModifiersState::ALT),
            event(KeyCode::Char('x'), KeyModifiers::ALT)
        );
        assert_eq!(
            press(ch("x"), ch("x"), Some("x"), ModifiersState::ALT),
            event(KeyCode::Char('x'), KeyModifiers::ALT)
        );
        // AltGr+Q on a German keyboard, on Windows: `@`.
        assert_eq!(
            press(
                ch("@"),
                ch("q"),
                Some("@"),
                ModifiersState::CONTROL | ModifiersState::ALT
            ),
            event(KeyCode::Char('@'), KeyModifiers::NONE)
        );
        // `´` then `e`.
        assert_eq!(
            press(ch("e"), ch("e"), Some("é"), NONE),
            event(KeyCode::Char('é'), KeyModifiers::NONE)
        );
        // The system's own.
        assert_eq!(
            press(ch("c"), ch("c"), Some("c"), ModifiersState::SUPER),
            None
        );
    }

    #[test]
    fn the_capture_keeps_the_cursor() {
        let size = Arc::new(Mutex::new(Size::new(20, 3)));
        let mut terminal = Terminal::new(Capture { size, cursor: None }).unwrap();
        let mut cells = None;
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new("hello"), frame.area());
                frame.set_cursor_position((2, 1));
                cells = Some(frame.buffer_mut().clone());
            })
            .unwrap();
        assert_eq!(terminal.backend().cursor, Some(Position::new(2, 1)));
        assert_eq!(cells.unwrap()[(1, 0)].symbol(), "e");
        terminal.draw(|_| {}).unwrap();
        assert_eq!(terminal.backend().cursor, None);
    }
}
