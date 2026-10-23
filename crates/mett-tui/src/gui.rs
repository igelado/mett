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
use ratatui::style::{Color, Modifier};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};
use ratatui_wgpu::shaders::AspectPreservingDefaultPostProcessor;
use ratatui_wgpu::{Builder, ColorTable, Dimensions, Font, WgpuBackend};
use tokio::sync::mpsc;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{Window, WindowId};

use crate::app::App;
use crate::editor::Editor;
use crate::{Accounts, InputEvents, Launch, UserEvent};

/// The colors of a terminal (xterm's), so that the window, like most
/// terminals, shows light text on black.
const PALETTE: ColorTable = ColorTable {
    BLACK: [0, 0, 0],
    RED: [205, 0, 0],
    GREEN: [0, 205, 0],
    YELLOW: [205, 205, 0],
    BLUE: [0, 0, 238],
    MAGENTA: [205, 0, 205],
    CYAN: [0, 205, 205],
    GRAY: [229, 229, 229],
    DARKGRAY: [127, 127, 127],
    LIGHTRED: [255, 0, 0],
    LIGHTGREEN: [0, 255, 0],
    LIGHTYELLOW: [255, 255, 0],
    LIGHTBLUE: [92, 92, 255],
    LIGHTMAGENTA: [255, 0, 255],
    LIGHTCYAN: [0, 255, 255],
    WHITE: [255, 255, 255],
};

/// The window's icon (the packages' launchers have it too).
static ICON: &[u8] = include_bytes!("../assets/icon-64.png");

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

/// Says `message` on the standard error, if there is one (a program
/// started from Windows' Explorer has none).
fn log(message: &str) {
    use std::io::Write;

    let _ = writeln!(std::io::stderr(), "mett: {message}");
}

/// mett in a window: what it said before starting, and whether it did.
#[derive(Default)]
struct InWindow {
    said: Vec<String>,
    started: bool,
}

impl Launch for &mut InWindow {
    fn say(&mut self, message: &str) {
        log(message);
        self.said.push(message.to_owned());
    }

    fn start<S: mett_core::MailStore + mett_core::MailSender + 'static>(
        self,
        runtime: tokio::runtime::Runtime,
        mut accounts: Accounts<S>,
    ) -> ExitCode {
        self.started = true;
        crate::remove_viewed_before();
        // The first account's configuration is the window's.
        let first = &mut accounts.list[0].session;
        let font = Fonts::from_config(&first.config, &mut |warning| self.said.push(warning));
        // Warnings from the configuration go on the message line.
        first.notice = Some(self.said.join("; ")).filter(|notice| !notice.is_empty());
        let start: StartApp = Box::new(move |screen, events| {
            let result = runtime.block_on(crate::run(screen, events, accounts));
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
    /// The fonts the configuration sets; `warn` is told why one it names
    /// can't be used.
    fn from_config(config: &crate::config::Config, warn: &mut dyn FnMut(String)) -> Self {
        let main = match config.string("gui_font") {
            "" => FONT,
            path => match std::fs::read(path) {
                // Kept for as long as mett runs.
                // (Its `m` tells how wide its cells are.)
                Ok(data) if Font::new(&data).is_some() && aspect(&data).is_some() => {
                    Vec::leak(data)
                }
                Ok(_) => {
                    warn(format!(
                        "warning: gui_font: {path} isn't a font mett can use"
                    ));
                    FONT
                }
                Err(e) => {
                    warn(format!("warning: gui_font: {path}: {e}"));
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

/// The window's icon, decoded.
fn icon() -> Option<winit::window::Icon> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(ICON));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().ok()?;
    let mut rgba = vec![0; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut rgba).ok()?;
    rgba.truncate(frame.buffer_size());
    winit::window::Icon::from_rgba(rgba, frame.width, frame.height).ok()
}

/// How wide a font's `m` is, for its height; `None` unless the font is
/// monospaced (as wide an `i` as an `m`), with cells a terminal's could be
/// (a tenth as wide as high, to twice).
fn aspect(data: &[u8]) -> Option<f32> {
    let face = ttf_parser::Face::parse(data, 0).ok()?;
    let width = |c| face.glyph_hor_advance(face.glyph_index(c)?);
    let advance = width('m')?;
    if width('i') != Some(advance) {
        return None;
    }
    let aspect = f32::from(advance) / f32::from(face.height());
    (0.1..=2.0).contains(&aspect).then_some(aspect)
}

/// Shows the window until it closes: the app's screens, started with
/// `start` (with `fonts`), or else what `shown` says. Returns the app's
/// exit code.
fn show(shown: Shown, start: Option<(StartApp, Fonts)>) -> ExitCode {
    let event_loop = match EventLoop::<Shown>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(e) => {
            log(&format!("cannot open a window: {e}"));
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
    // Why mett didn't start: its exit code, until the app says another.
    let code = match &shown {
        Shown::Ended(code, _) => *code,
        _ => ExitCode::SUCCESS,
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
        composing: false,
        typed: None,
        cell: winit::dpi::PhysicalSize::new(1, 1),
        fallbacks: Vec::new(),
        code,
    };
    if let Err(e) = event_loop.run_app(&mut gui) {
        log(&e.to_string());
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
    /// Text an input method is composing: keys go to it meanwhile.
    composing: bool,
    /// The text the last key press typed, which an input method may
    /// commit again.
    typed: Option<String>,
    /// A cell's size, in pixels: the window is never smaller.
    cell: winit::dpi::PhysicalSize<u32>,
    /// The system's fonts the font falls back on.
    fallbacks: Vec<Font<'static>>,
    /// How the app ended.
    code: ExitCode,
}

impl Gui {
    /// Opens the window, and starts the app in it.
    fn open(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let attributes = Window::default_attributes()
            .with_title("mett")
            .with_window_icon(icon())
            .with_inner_size(WINDOW_SIZE);
        // The name mett.desktop knows the window by: its Wayland app ID,
        // its X11 class.
        #[cfg(all(unix, not(target_os = "macos")))]
        let attributes = {
            use winit::platform::wayland::WindowAttributesExtWayland;
            use winit::platform::x11::WindowAttributesExtX11;
            let attributes = WindowAttributesExtWayland::with_name(attributes, "mett", "mett");
            WindowAttributesExtX11::with_name(attributes, "mett", "mett")
        };
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .map_err(|e| e.to_string())?,
        );
        let font_px = self.font_px(&window)?;
        // Text typed with an input method (Chinese, Japanese, ...) comes
        // as `Ime` events.
        window.set_ime_allowed(true);
        let font = Font::new(self.fonts.main).ok_or("the font can't be read")?;
        let size = window.inner_size();
        self.fallbacks = self.fonts.fallbacks();
        // The font first, then the others; it is also the last resort.
        let builder = Builder::from_font(font.clone())
            .with_regular_fonts([font])
            .with_regular_fonts(self.fallbacks.clone())
            .with_font_size_px(font_px)
            .with_bg_color(Color::Black)
            .with_fg_color(Color::Rgb(229, 229, 229))
            .with_color_table(PALETTE)
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

    /// The font's size in `window`'s pixels; the window grows and shrinks
    /// by cells of that font, where the system lets it (the cells are
    /// drawn from its top left corner).
    fn font_px(&mut self, window: &Window) -> Result<u32, String> {
        let aspect = f64::from(aspect(self.fonts.main).ok_or("the font has no `m`")?);
        // Points to pixels: a point is 1/72 inch, a logical pixel 1/96.
        // Cells at least a pixel wide (ratatui-wgpu divides by their
        // width), and not larger than the window can show.
        let font_px = (f64::from(self.fonts.size) * 96.0 / 72.0 * window.scale_factor())
            .round()
            .clamp((1.0 / aspect).ceil() + 1.0, 512.0);
        // Whole numbers: clamped above.
        let cell = winit::dpi::PhysicalSize::new((aspect * font_px).floor() as u32, font_px as u32);
        window.set_resize_increments(Some(cell));
        window.set_min_inner_size(Some(cell));
        self.cell = cell;
        // A whole number, clamped above.
        Ok(font_px as u32)
    }

    /// The window moved to a display of another scale: the text keeps its
    /// size in points.
    fn rescaled(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let (Ok(font_px), Some(font)) = (self.font_px(&window), Font::new(self.fonts.main)) else {
            return;
        };
        let mut fonts = ratatui_wgpu::Fonts::new(font.clone(), font_px);
        fonts.add_regular_fonts([font]);
        fonts.add_regular_fonts(self.fallbacks.clone());
        let size = window.inner_size();
        if let Some(terminal) = &mut self.terminal {
            let backend = terminal.backend_mut();
            backend.update_fonts(fonts);
            // ratatui-wgpu lays its cells out again only when the size
            // changes: it changes for a moment.
            let other = if size.height > 1 { size.height - 1 } else { 2 };
            backend.resize(size.width, other);
        }
        self.resized(size);
    }

    /// The window's size changed: so may the number of cells in it.
    fn resized(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        // At least a cell, where the system lets it be smaller.
        let (width, height) = (
            size.width.max(self.cell.width),
            size.height.max(self.cell.height),
        );
        terminal.backend_mut().resize(width, height);
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

    /// Sends the app what the user did.
    fn send(&self, event: UserEvent) {
        if let Some(input) = &self.input {
            let _ = input.send(Ok(event));
        }
    }

    /// Tells the app to paste: the clipboard's text is read there, where
    /// the built-in editor's own is if the system's can't be.
    fn paste(&mut self) {
        self.send(UserEvent::Paste);
    }

    /// Draws what is shown.
    fn draw(&mut self) {
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        // Not even a cell fits (ratatui-wgpu would panic).
        if terminal
            .size()
            .is_ok_and(|size| size.width == 0 || size.height == 0)
        {
            return;
        }
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
            log(&format!("cannot draw: {e}"));
        }
    }
}

impl ApplicationHandler<Shown> for Gui {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        if let Err(e) = self.open(event_loop) {
            log(&e.to_string());
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
            WindowEvent::Resized(size) => self.resized(size),
            WindowEvent::ScaleFactorChanged { .. } => self.rescaled(),
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if ended {
                    event_loop.exit();
                    return;
                }
                if self.composing {
                    return;
                }
                let plain = event.key_without_modifiers();
                self.typed = event.text.as_ref().map(ToString::to_string);
                let typing = !(self.modifiers.control_key()
                    || self.modifiers.alt_key()
                    || self.modifiers.super_key());
                let several = event
                    .text
                    .as_deref()
                    .filter(|text| typing && text.chars().nth(1).is_some());
                if is_paste(&plain, self.modifiers) {
                    self.paste();
                } else if let Some(text) = several {
                    // A letter and a combining accent, say: each is typed.
                    for c in text.chars().filter(|c| !c.is_control()) {
                        self.send(UserEvent::Term(TermEvent::Key(KeyEvent::from(
                            KeyCode::Char(c),
                        ))));
                    }
                } else if let Some(key) = key(
                    &event.logical_key,
                    &plain,
                    event.text.as_deref(),
                    self.modifiers,
                ) {
                    self.send(UserEvent::Term(TermEvent::Key(key)));
                }
            }
            WindowEvent::Ime(Ime::Preedit(text, _)) => {
                self.composing = !text.is_empty();
                // What it commits is its own, not the last key's text.
                if self.composing {
                    self.typed = None;
                }
            }
            WindowEvent::Ime(Ime::Disabled) => self.composing = false,
            WindowEvent::Ime(Ime::Commit(text)) => {
                self.composing = false;
                // Committed as the key press typed it already.
                if self.typed.take().as_deref() == Some(text.as_str()) {
                    return;
                }
                for c in text.chars() {
                    self.send(UserEvent::Term(TermEvent::Key(KeyEvent::from(
                        KeyCode::Char(c),
                    ))));
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
            // A character of the layout's own, typed with AltGr (Control
            // and Alt on Windows) or a Mac's Option: `@` on German
            // keyboards. It is just typed, as a Mac's Terminal does
            // (Escape and the key is still Meta).
            let own = !typed.eq_ignore_ascii_case(&base);
            let c = if alt && own && !typed.is_control() {
                return Some(KeyEvent::from(KeyCode::Char(typed)));
            } else if ctrl {
                // Control and a key, as a terminal reports it.
                return Some(control(base.to_ascii_lowercase(), held));
            } else if alt {
                // Meta and the key.
                typed
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

/// Control and `c` (and the other modifiers `held`), as a terminal reports
/// it: `^M` is Enter, `^I` Tab, `^[` Esc; `^\\`, `^]`, `^^`, `^_` (and
/// `^/`) are the bytes crossterm reads as Control-4 to Control-7, and `^@`
/// is Control-Space.
fn control(c: char, held: KeyModifiers) -> KeyEvent {
    let without = held.difference(KeyModifiers::CONTROL);
    let code = match c {
        'm' => return KeyEvent::new(KeyCode::Enter, without),
        'i' => return KeyEvent::new(KeyCode::Tab, without),
        '[' => return KeyEvent::new(KeyCode::Esc, without),
        '\\' => '4',
        ']' => '5',
        '^' => '6',
        '_' | '/' => '7',
        '@' | '2' => ' ',
        c => c,
    };
    KeyEvent::new(KeyCode::Char(code), held)
}

/// Whether the key `plain`, with `modifiers` held, pastes: Command+V on a
/// Mac, Control+Shift+V elsewhere (as in terminals: Control+V is a key of
/// mett's own).
fn is_paste(plain: &Key, modifiers: ModifiersState) -> bool {
    let v = matches!(plain, Key::Character(c) if c.eq_ignore_ascii_case("v"));
    let held = if cfg!(target_os = "macos") {
        ModifiersState::SUPER
    } else {
        ModifiersState::CONTROL | ModifiersState::SHIFT
    };
    v && modifiers == held
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
        crate::platform::with_clipboard(|clipboard| clipboard.set_text(text))
            .map_err(std::io::Error::other)
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
    fn control_keys_are_those_a_terminal_reports() {
        let event = |code, modifiers| Some(KeyEvent::new(code, modifiers));
        let ctrl = ModifiersState::CONTROL;
        assert_eq!(
            press(ch("m"), ch("m"), None, ctrl),
            event(KeyCode::Enter, KeyModifiers::NONE)
        );
        assert_eq!(
            press(ch("i"), ch("i"), None, ctrl),
            event(KeyCode::Tab, KeyModifiers::NONE)
        );
        assert_eq!(
            press(ch("["), ch("["), None, ctrl),
            event(KeyCode::Esc, KeyModifiers::NONE)
        );
        assert_eq!(
            press(ch("/"), ch("/"), None, ctrl),
            event(KeyCode::Char('7'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            press(ch("g"), ch("g"), None, ctrl),
            event(KeyCode::Char('g'), KeyModifiers::CONTROL)
        );
    }

    #[test]
    fn meta_altgr_and_dead_keys() {
        let event = |code, modifiers| Some(KeyEvent::new(code, modifiers));
        // Alt and a key that types nothing else is Meta and the key.
        assert_eq!(
            press(ch("x"), ch("x"), Some("x"), ModifiersState::ALT),
            event(KeyCode::Char('x'), KeyModifiers::ALT)
        );
        assert_eq!(
            press(
                ch("X"),
                ch("x"),
                Some("X"),
                ModifiersState::ALT | ModifiersState::SHIFT
            ),
            event(KeyCode::Char('X'), KeyModifiers::ALT | KeyModifiers::SHIFT)
        );
        // Option+L on a German Mac types `@`, as Terminal does.
        assert_eq!(
            press(ch("@"), ch("l"), Some("@"), ModifiersState::ALT),
            event(KeyCode::Char('@'), KeyModifiers::NONE)
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
    fn pasting_is_the_system_s_shortcut() {
        let v = ch("v");
        let mac = cfg!(target_os = "macos");
        assert_eq!(is_paste(&v, ModifiersState::SUPER), mac);
        assert_eq!(
            is_paste(&v, ModifiersState::CONTROL | ModifiersState::SHIFT),
            !mac
        );
        // Control+V is mett's (Notepad's paste in the built-in editor).
        assert!(!is_paste(&v, ModifiersState::CONTROL));
        assert!(!is_paste(&ch("c"), ModifiersState::SUPER));
    }

    #[test]
    fn the_icon_decodes() {
        assert!(icon().is_some());
    }

    #[test]
    fn fonts_are_measured_by_their_m() {
        let aspect = aspect(FONT).expect("Cascadia Mono");
        assert!((0.5..0.7).contains(&aspect), "{aspect}");
        assert_eq!(super::aspect(b"not a font"), None);
        // A proportional font, the system's, if it has one.
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let proportional = db.faces().find(|face| !face.monospaced && face.index == 0);
        if let Some(face) = proportional
            && let Some(data) = db.with_face_data(face.id, |data, _| data.to_vec())
            && ttf_parser::Face::parse(&data, 0)
                .is_ok_and(|f| f.glyph_index('i').is_some() && f.glyph_index('m').is_some())
        {
            assert_eq!(super::aspect(&data), None, "{:?}", face.families);
        }
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
