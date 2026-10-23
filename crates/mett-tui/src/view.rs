//! Drawing the [`App`]: the "view" of the Elm Architecture. It reads the
//! model and never changes it.
//!
//! The layout follows mutt: a help line at the top, the index or pager in
//! the middle (with the sidebar beside it, when shown), a status bar, and
//! a message line at the bottom.

mod sidebar;

use std::ops::Range;

use mett_core::Address;
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::action::Action;
use crate::app::{
    App, AttachMenu, Browser, Compose, Help, Input, Pager, PostponedMenu, QueryMenu, Row, Screen,
    SignIn, TabPosition, TextView, highlighted,
};
use crate::color::Colors;
use crate::editor::{self, builtin::Builtin};
use crate::expando::{self, Display, Entry, Status};
use crate::format;
use crate::keymap::{Keymap, Menu};
use crate::pattern::Candidate;
use crate::text::{self, truncate};

/// The actions shown in each menu's help line.
const HELP: &[(Menu, &[(Action, &str)])] = &[
    (
        Menu::Index,
        &[
            (Action::Quit, "Quit"),
            (Action::NextEntry, "Next"),
            (Action::PreviousEntry, "Prev"),
            (Action::DisplayMessage, "Display"),
        ],
    ),
    (
        Menu::Pager,
        &[
            (Action::Exit, "Exit"),
            (Action::PreviousPage, "PrevPg"),
            (Action::NextPage, "NextPg"),
            (Action::NextEntry, "Next"),
            (Action::PreviousEntry, "Prev"),
        ],
    ),
    (
        Menu::Help,
        &[
            (Action::Exit, "Exit"),
            (Action::NextPage, "NextPg"),
            (Action::PreviousPage, "PrevPg"),
        ],
    ),
    (
        Menu::Browser,
        &[
            (Action::Exit, "Exit"),
            (Action::DisplayMessage, "Select"),
            (Action::NextEntry, "Next"),
            (Action::PreviousEntry, "Prev"),
        ],
    ),
    (
        Menu::Compose,
        &[
            (Action::SendMessage, "Send"),
            (Action::Exit, "Abort"),
            (Action::EditTo, "To"),
            (Action::EditCc, "CC"),
            (Action::EditBcc, "Bcc"),
            (Action::EditSubject, "Subj"),
            (Action::EditMessage, "Edit"),
            (Action::PostponeMessage, "Postpone"),
            (Action::AttachFile, "Attach"),
        ],
    ),
    (
        Menu::Attach,
        &[
            (Action::Exit, "Exit"),
            (Action::SaveEntry, "Save"),
            (Action::ViewAttach, "View"),
            (Action::NextEntry, "Next"),
            (Action::PreviousEntry, "Prev"),
        ],
    ),
    (
        Menu::Query,
        &[
            (Action::Exit, "Exit"),
            (Action::DisplayMessage, "Select"),
            (Action::Mail, "Mail"),
            (Action::NextEntry, "Next"),
            (Action::PreviousEntry, "Prev"),
        ],
    ),
    (
        Menu::Postpone,
        &[
            (Action::Exit, "Exit"),
            (Action::DisplayMessage, "Select"),
            (Action::NextEntry, "Next"),
            (Action::PreviousEntry, "Prev"),
        ],
    ),
    (
        Menu::Sidebar,
        &[
            (Action::Exit, "Exit"),
            (Action::SidebarOpen, "Open"),
            (Action::SidebarNext, "Next"),
            (Action::SidebarPrev, "Prev"),
        ],
    ),
    (
        Menu::SignIn,
        &[
            (Action::Exit, "Close"),
            (Action::OpenBrowser, "Browser"),
            (Action::CopyLink, "Copy"),
            (Action::Retry, "Retry"),
        ],
    ),
];

/// Text to print over the drawn screen, starting at a cell, and left for
/// the terminal to wrap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overlay {
    /// The column.
    pub x: u16,
    /// The row.
    pub y: u16,
    /// The text.
    pub text: String,
}

/// Draws the whole screen, and returns text that the main loop should print
/// over it, if any.
pub fn render(app: &App, frame: &mut Frame) -> Option<Overlay> {
    // With several accounts, a line of the screen is the tabs'.
    let root = render_tabs(app, frame);
    if let Some(editing) = &app.editing {
        return render_editor(app, &mut editing.borrow_mut(), frame, root);
    }
    let keymap = &app.keymap;
    let [help, body, status, message] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(root);
    let colors = &app.display.colors;
    // Text without colors of its own is `normal`.
    frame.render_widget(Block::new().style(colors.normal), root);
    // As in mutt, the sidebar is beside the menu only: the help line and
    // the status bar span the screen.
    let (body, side) = sidebar::split(app, body);
    if let Some(side) = side {
        sidebar::render(app, frame, side);
    }

    frame.render_widget(
        Paragraph::new(help_line(app, keymap, app.menu())).style(colors.status),
        help,
    );
    let status_line = match app.screen() {
        Screen::Index => {
            let first = page_start(app.selected, usize::from(body.height));
            render_index(app, first, Some(app.selected), frame, body);
            index_status(
                app,
                first,
                usize::from(body.height),
                usize::from(status.width),
            )
        }
        Screen::Pager(pager) => render_pager(app, pager, frame, body, status.width),
        Screen::Compose(compose) => render_compose(compose, colors, frame, body),
        Screen::Browser(browser) => render_browser(browser, colors, frame, body),
        Screen::Help(help) => render_help(app, help, frame, body),
        Screen::Attachments(menu) => render_attachments(menu, colors, frame, body),
        Screen::Text(view) => render_text(view, &app.display, frame, body),
        Screen::Query(menu) => render_query(menu, colors, frame, body),
        Screen::Postponed(menu) => render_postponed(menu, colors, frame, body),
    };
    frame.render_widget(Paragraph::new(status_line).style(colors.status), status);
    match &app.input {
        Some(_) if let Some(note) = &app.input_note => {
            frame.render_widget(Paragraph::new(note.as_str()).style(colors.error), message);
        }
        Some(input) => render_input(input, colors.prompt, frame, message),
        None => {
            let style = if app.error.as_deref() == Some(app.status.as_str()) {
                colors.error
            } else {
                colors.message
            };
            // The wait for a throttling server, at the end of the line, in
            // its own room, so that it doesn't hide what the line says: at
            // most half of it, shortened (or left out) to fit.
            let notice = app.throttled_notices().and_then(|notices| {
                notices
                    .into_iter()
                    .find(|n| n.width() < usize::from(message.width / 2))
            });
            let [text, end] = Layout::horizontal([
                Constraint::Fill(1),
                Constraint::Length(notice.as_deref().map_or(0, |n| n.width() as u16 + 1)),
            ])
            .areas(message);
            frame.render_widget(Paragraph::new(app.status.as_str()).style(style), text);
            if let Some(notice) = notice {
                frame.render_widget(
                    Paragraph::new(notice)
                        .style(colors.message)
                        .alignment(ratatui::layout::Alignment::Right),
                    end,
                );
            }
        }
    }
    // The window may cover everything but the help line, which lists its
    // keys: on a small terminal, the sign-in instructions matter more than
    // the index behind them.
    if let Some(window) = &app.sign_in {
        let [_, below_help] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(root);
        return render_sign_in(window, keymap, colors.normal, frame, below_help);
    }
    None
}

/// Draws the accounts' tabs, if there are several, and returns the part of
/// the screen that is left for the rest.
fn render_tabs(app: &App, frame: &mut Frame) -> Rect {
    let area = frame.area();
    let Some(tabs) = &app.tabs else {
        return area;
    };
    let (bar, rest) = match tabs.position {
        TabPosition::Top => {
            let [bar, rest] =
                Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
            (bar, rest)
        }
        TabPosition::Bottom => {
            let [rest, bar] =
                Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
            (bar, rest)
        }
    };
    let colors = &app.display.colors;
    // Too many tabs for the line: those before the one shown go from the
    // left until it fits.
    let text = |at: usize, tab: &crate::app::Tab| {
        let marker = if tab.new_mail { "*" } else { "" };
        format!(" {} {}{marker} ", at + 1, tab.label)
    };
    let widths: Vec<usize> = tabs
        .tabs
        .iter()
        .enumerate()
        .map(|(at, tab)| text(at, tab).width())
        .collect();
    let mut first = 0;
    while first < tabs.active
        && widths[first..=tabs.active].iter().sum::<usize>() > usize::from(bar.width)
    {
        first += 1;
    }
    let spans: Vec<Span> = tabs
        .tabs
        .iter()
        .enumerate()
        .skip(first)
        .map(|(at, tab)| {
            let style = if at == tabs.active {
                colors.indicator
            } else if tab.new_mail {
                // Mail waiting in another account: the tab stands out.
                colors
                    .status
                    .patch(colors.sidebar.new)
                    .add_modifier(Modifier::BOLD)
            } else {
                colors.status
            };
            Span::styled(text(at, tab), style)
        })
        .collect();
    frame.render_widget(Paragraph::new(Line::from(spans)).style(colors.status), bar);
    rest
}

/// Draws the built-in editor: the text, and below it the line with its
/// commands, questions and messages; the sign-in window over them, if it
/// is up.
fn render_editor(
    app: &App,
    editor: &mut Builtin,
    frame: &mut Frame,
    root: Rect,
) -> Option<Overlay> {
    let colors = &app.display.colors;
    frame.render_widget(Block::new().style(colors.normal), root);
    let [text, bottom] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(root);
    let mut cursor = editor.render(text, frame.buffer_mut(), colors.normal, colors.indicator);
    let width = usize::from(bottom.width);
    let line = match editor.bottom(width) {
        editor::builtin::Bottom::Prompt {
            prompt,
            text,
            cursor: column,
        } => {
            let prompt = truncate(&prompt, width.saturating_sub(1));
            let x = u16::try_from(prompt.width() + column).unwrap_or(u16::MAX);
            cursor = Some((bottom.x + x.min(bottom.width.saturating_sub(1)), bottom.y).into());
            Line::from(vec![Span::styled(prompt, colors.prompt), Span::raw(text)])
        }
        editor::builtin::Bottom::Note(note) => Line::styled(truncate(&note, width), colors.message),
        editor::builtin::Bottom::Hint(hint) => Line::styled(truncate(&hint, width), colors.message),
    };
    frame.render_widget(Paragraph::new(line), bottom);
    if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
    if let Some(window) = &app.sign_in {
        let [_, below_help] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(root);
        return render_sign_in(window, &app.keymap, colors.normal, frame, below_help);
    }
    None
}

/// Draws the help screen and returns its status line.
fn render_help(app: &App, help: &Help, frame: &mut Frame, area: Rect) -> String {
    let width = usize::from(area.width);
    let lines: Vec<Line> = app
        .help_lines(help.menu)
        .skip(help.scroll)
        .take(usize::from(area.height))
        .map(|line| Line::raw(truncate(&line, width)))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
    format!("---mett: Help for {}---", help.menu.name())
}

/// Draws the folder browser and returns its status line. Each line is like
/// mutt's: the position, the unread and total counts, and the name,
/// indented by depth.
fn render_browser(browser: &Browser, colors: &Colors, frame: &mut Frame, area: Rect) -> String {
    if browser.entries.is_empty() {
        let text = if browser.listing() {
            "Listing folders..."
        } else {
            "No folders."
        };
        frame.render_widget(Paragraph::new(text), area);
        return "---mett: Folders---".into();
    }
    let title = if browser.mailboxes {
        "Mailboxes"
    } else {
        "Folders"
    };
    let width = usize::from(area.width);
    let (entries, selected) = (&browser.entries, browser.selected);
    render_page(
        entries,
        selected,
        colors.indicator,
        frame,
        area,
        |i, entry| {
            let new = if entry.new { 'N' } else { ' ' };
            let counts = match entry.counts {
                Some((unread, total)) => format!("{unread:>5}/{total:<6}"),
                None => " ".repeat(12),
            };
            let line = format!(
                "{:>4} {new} {counts} {}{}",
                i + 1,
                "  ".repeat(entry.depth),
                entry.name
            );
            Line::raw(truncate(&line, width))
        },
    );
    format!("---mett: {title} [{}]---", browser.entries.len())
}

/// Draws the page of `entries` with the selected one on it (a page at a
/// time, as mutt scrolls), each as `line` makes it from its position, and
/// the selected one in `indicator`.
fn render_page<T>(
    entries: &[T],
    selected: usize,
    indicator: Style,
    frame: &mut Frame,
    area: Rect,
    line: impl Fn(usize, &T) -> Line<'static>,
) {
    let first = page_start(selected, usize::from(area.height));
    render_rows(entries, first, Some(selected), indicator, frame, area, line);
}

/// The first entry of the page of `height` entries with `selected` on it.
fn page_start(selected: usize, height: usize) -> usize {
    let height = height.max(1);
    selected / height * height
}

/// Draws `entries` from `first` on, as many as fit, as [`render_page`]
/// does (with no entry in `indicator` if none is `selected`).
fn render_rows<T>(
    entries: &[T],
    first: usize,
    selected: Option<usize>,
    indicator: Style,
    frame: &mut Frame,
    area: Rect,
    line: impl Fn(usize, &T) -> Line<'static>,
) {
    let height = usize::from(area.height).max(1);
    let lines: Vec<Line> = entries
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(i, entry)| {
            let line = line(i, entry);
            if Some(i) == selected {
                line.style(indicator)
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// Draws a question on the message line, with the cursor where the answer
/// is typed.
fn render_input(input: &Input, prompt_style: Style, frame: &mut Frame, area: Rect) {
    let width = usize::from(area.width);
    let (line, cursor) = match input {
        Input::Line { prompt, editor, .. } => {
            let prompt = truncate(prompt, width.saturating_sub(1));
            let (text, column) = editor.view(width.saturating_sub(prompt.width()));
            let cursor = prompt.width() + column;
            let line = Line::from(vec![Span::styled(prompt, prompt_style), Span::raw(text)]);
            (line, cursor)
        }
        Input::Choice { prompt, .. } => {
            let line = truncate(prompt, width);
            let cursor = line.width();
            (Line::styled(line, prompt_style), cursor)
        }
        Input::YesNo {
            prompt, default, ..
        } => {
            let choices = if *default { "([yes]/no)" } else { "(yes/[no])" };
            let line = truncate(&format!("{prompt} {choices}: "), width);
            let cursor = line.width();
            (Line::styled(line, prompt_style), cursor)
        }
    };
    frame.render_widget(Paragraph::new(line), area);
    let x = u16::try_from(cursor)
        .unwrap_or(u16::MAX)
        .min(area.width.saturating_sub(1));
    frame.set_cursor_position((area.x + x, area.y));
}

/// Draws the compose menu and returns its status line: the headers, then
/// the message's parts (for now, only its text).
fn render_compose(compose: &Compose, colors: &Colors, frame: &mut Frame, area: Rect) -> String {
    let draft = &compose.draft;
    let list = |addresses: &[Address]| Address::format_list(addresses);
    let from = draft
        .from
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    let headers = [
        ("From", from),
        ("To", list(&draft.to)),
        ("Cc", list(&draft.cc)),
        ("Bcc", list(&draft.bcc)),
        ("Subject", draft.subject.clone()),
    ];
    let width = usize::from(area.width);
    let mut lines: Vec<Line> = headers
        .into_iter()
        .map(|(name, value)| Line::raw(truncate(&format!("{name:>8}: {value}"), width)))
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::raw("-- Attachments"));
    // The parts, as mutt lists them: the text, what a forward carries
    // along, and the attached files.
    let mut parts = vec![format!(
        "- I     1 <message text>  [text/plain, utf-8, {}]",
        format_size(draft.body.len())
    )];
    if compose.forwarded_attachments {
        parts.push(format!(
            "- A     {} <the forwarded message's attachments>",
            parts.len() + 1
        ));
    }
    // A recalled message's own, then the attached files.
    let kept = compose.kept.iter().map(|kept| {
        let size = usize::try_from(kept.size).unwrap_or(usize::MAX);
        (&kept.name, &kept.content_type, size)
    });
    let files = draft.attachments.iter();
    let files = files.map(|f| (&f.name, &f.content_type, f.data.len()));
    for (name, content_type, size) in kept.chain(files) {
        parts.push(format!(
            "- A     {} {name}  [{content_type}, {}]",
            parts.len() + 1,
            format_size(size)
        ));
    }
    let count = parts.len();
    // A page of parts at a time, with the selected one, when they don't
    // all fit under the headers.
    let room = usize::from(area.height).saturating_sub(lines.len()).max(1);
    let first = compose.selected / room * room;
    for (i, part) in parts.into_iter().enumerate().skip(first).take(room) {
        let line = Line::raw(truncate(&part, width));
        lines.push(if i == compose.selected {
            line.style(colors.indicator)
        } else if i > 0 {
            line.style(colors.attachment)
        } else {
            line
        });
    }
    let size = draft.body.len()
        + draft
            .attachments
            .iter()
            .map(|a| a.data.len())
            .sum::<usize>();
    frame.render_widget(Paragraph::new(lines), area);
    format!(
        "-- mett: Compose  [Approx. msg size: {}   Atts: {count}]---",
        format_size(size)
    )
}

/// Draws the attachment menu and returns its status line: each
/// attachment, as mutt shows it, with its type and size.
fn render_attachments(menu: &AttachMenu, colors: &Colors, frame: &mut Frame, area: Rect) -> String {
    if menu.entries.is_empty() {
        let text = if menu.listing() {
            "Listing attachments..."
        } else {
            "No attachments."
        };
        frame.render_widget(Paragraph::new(text), area);
        return "---mett: Attachments---".into();
    }
    let width = usize::from(area.width);
    let (entries, selected) = (&menu.entries, menu.selected);
    render_page(
        entries,
        selected,
        colors.indicator,
        frame,
        area,
        |i, attachment| {
            let size = usize::try_from(attachment.size).unwrap_or(usize::MAX);
            let kind = match attachment.kind {
                mett_core::AttachmentKind::Link => " (link)",
                _ => "",
            };
            let line = format!(
                "{:>4} [{}, {}]  {}{kind}",
                i + 1,
                attachment.content_type,
                format_size(size),
                attachment.name
            );
            Line::raw(truncate(&line, width)).style(colors.attachment)
        },
    );
    format!("---mett: Attachments [{}]---", menu.entries.len())
}

/// Draws a list of addresses to choose from and returns its status line.
fn render_query(menu: &QueryMenu, colors: &Colors, frame: &mut Frame, area: Rect) -> String {
    if menu.entries.is_empty() {
        frame.render_widget(Paragraph::new("Searching..."), area);
        return format!("---mett: {}---", menu.title);
    }
    let width = usize::from(area.width);
    let (entries, selected) = (&menu.entries, menu.selected);
    render_page(
        entries,
        selected,
        colors.indicator,
        frame,
        area,
        |i, entry| Line::raw(truncate(&format!("{:>4} {}", i + 1, entry.label), width)),
    );
    format!("---mett: {} [{}]---", menu.title, menu.entries.len())
}

/// Draws the postponed messages to choose from and returns its status
/// line: when each was written, to whom, and its subject.
fn render_postponed(
    menu: &PostponedMenu,
    colors: &Colors,
    frame: &mut Frame,
    area: Rect,
) -> String {
    let width = usize::from(area.width);
    let (entries, selected) = (&menu.entries, menu.selected);
    render_page(
        entries,
        selected,
        colors.indicator,
        frame,
        area,
        |i, message| {
            let to = message.to.first().map_or_else(String::new, |a| {
                a.name.clone().unwrap_or_else(|| a.email.clone())
            });
            let when = message
                .received
                .with_timezone(&chrono::Local)
                .format("%b %d");
            Line::raw(truncate(
                &format!("{:>4} {when} {:<20.20} {}", i + 1, to, message.subject),
                width,
            ))
        },
    );
    format!("---mett: Postponed Messages [{}]---", menu.entries.len())
}

/// Draws text on its own screen and returns its status line.
fn render_text(view: &TextView, display: &Display, frame: &mut Frame, area: Rect) -> String {
    let width = display.wrap_width(usize::from(area.width));
    let height = usize::from(area.height);
    // Wrapped by the app as `wrap` says for the terminal's width; here only
    // if the drawing area differs.
    let rewrapped;
    let rows = if view.rows_width == Some(width.max(1)) {
        &view.rows
    } else {
        rewrapped = text::wrap_all(&view.lines, width.max(1));
        &rewrapped
    };
    let top = view.scroll.min(rows.len().saturating_sub(height));
    let lines: Vec<Line> = rows
        .iter()
        .skip(top)
        .take(height)
        .map(|row| Line::raw(row.as_str()))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
    let position = expando::position(top, height, rows.len());
    format!("---mett: {}   -- ({position})", view.title)
}

/// A size the way mutt shows it: `0.5K`, `12K`, `1.2M`.
fn format_size(bytes: usize) -> String {
    let kilobytes = bytes as f64 / 1024.0;
    if kilobytes < 10.0 {
        format!("{kilobytes:.1}K")
    } else if kilobytes < 1024.0 {
        format!("{kilobytes:.0}K")
    } else {
        format!("{:.1}M", kilobytes / 1024.0)
    }
}

/// Draws the sign-in window across the middle of `area`. It spans the
/// whole width, without side borders, so that the sign-in link can be
/// copied from the terminal without stray characters.
fn render_sign_in(
    window: &SignIn,
    keymap: &Keymap,
    normal: Style,
    frame: &mut Frame,
    area: Rect,
) -> Option<Overlay> {
    let rows = window.rows(usize::from(area.width));
    let height = usize::from(area.height);
    // Without room for everything, show what fits from the scroll
    // position, and say how to scroll.
    let fits = rows.len() + 2 <= height;
    let top = if fits {
        0
    } else {
        window
            .scroll
            .min(rows.len().saturating_sub(height.saturating_sub(2)))
    };
    let shown = rows.len().saturating_sub(top).min(height.saturating_sub(2));
    let link_rows: Vec<usize> = (0..rows.len())
        .filter(|&i| rows[i].1 == Row::Link)
        .collect();
    let link: String = link_rows.iter().map(|&i| rows[i].0.as_str()).collect();
    let visible: Vec<Line> = rows
        .into_iter()
        .skip(top)
        .map(|(row, kind)| {
            let style = if kind == Row::Code {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            Line::styled(row, style)
        })
        .collect();
    let mut title = format!(" Sign in to {} ", window.account);
    if !fits {
        let key = |action| keymap.key_for(Menu::SignIn, action);
        if let (Some(down), Some(up)) = (key(Action::NextLine), key(Action::PreviousLine)) {
            title.push_str(&format!("({down}/{up} to scroll) "));
        }
    }
    let height = u16::try_from(visible.len() + 2).unwrap_or(u16::MAX);
    let [popup] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(visible).style(normal).block(
            Block::new()
                .borders(Borders::TOP | Borders::BOTTOM)
                .title(title),
        ),
        popup,
    );
    // The link, if it is all on screen, for the main loop to print again
    // as one line that the terminal wraps: then copying it from the screen
    // gives the link without line breaks.
    let (&first, &last) = (link_rows.first()?, link_rows.last()?);
    if first < top || last >= top + shown {
        return None;
    }
    Some(Overlay {
        x: popup.x,
        y: popup.y + 1 + u16::try_from(first - top).ok()?,
        text: link,
    })
}

/// Whether `action` does anything now, so that the help line only offers
/// what works: the sign-in window can't retry before a sign-in failed, nor
/// open or copy a link it hasn't got.
fn available(app: &App, action: Action) -> bool {
    let Some(window) = &app.sign_in else {
        return true;
    };
    match action {
        Action::Retry => window.failure.is_some(),
        // After a failure, the link or code is spent: only Retry helps.
        Action::OpenBrowser | Action::CopyLink => {
            window.prompt.is_some() && window.failure.is_none()
        }
        _ => true,
    }
}

/// `q:Quit  j:Next  …`, from the current key bindings.
fn help_line(app: &App, keymap: &Keymap, menu: Menu) -> String {
    let entries = HELP
        .iter()
        .find(|(m, _)| *m == menu)
        .map_or(&[][..], |(_, entries)| entries);
    entries
        .iter()
        .filter(|(action, _)| available(app, *action))
        .filter_map(|(action, label)| {
            keymap
                .key_for(menu, *action)
                .map(|key| format!("{key}:{label}"))
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// Draws the index from entry `first` on, with entry `current`, if any, in
/// the indicator's colors.
fn render_index(app: &App, first: usize, current: Option<usize>, frame: &mut Frame, area: Rect) {
    let width = usize::from(area.width);
    let colors = &app.display.colors;
    let total = app.total_messages();
    // The threads with unread mail folded into them, found once for the
    // page, and only if a collapsed thread on it needs them.
    let unread_roots = std::cell::OnceCell::new();
    render_rows(
        &app.messages,
        first,
        current,
        colors.indicator,
        frame,
        area,
        |i, message| {
            // A collapsed thread with unread mail folded into it shows as
            // unread.
            let thread = app.thread_of(&message.id);
            let folded_unread;
            let message = if message.flags.read
                && thread.is_some_and(|t| t.folded > 0)
                && unread_roots
                    .get_or_init(|| app.roots_with_folded_unread())
                    .contains(&message.id)
            {
                let mut shown = message.clone();
                shown.flags.read = false;
                folded_unread = shown;
                &folded_unread
            } else {
                message
            };
            let deleted = app.is_deleted(&message.id);
            let entry = Entry {
                number: i + 1,
                summary: message,
                deleted,
                from_me: message.from.as_ref().is_some_and(|a| app.is_me(a)),
                total,
                date_format: &app.display.date_format,
                position: None,
                thread,
            };
            // Padded to the right edge, so that the row's colors cover it.
            let mut text = format::expand(&app.display.index_format, &entry, width);
            let used = text.width();
            text.push_str(&" ".repeat(width.saturating_sub(used)));
            let line = Line::raw(text);
            // The selected one is in the indicator's colors instead.
            if Some(i) == current {
                return line;
            }
            line.style(colors.index_style(|pattern| {
                pattern.matches(&Candidate {
                    summary: message,
                    deleted,
                })
            }))
        },
    );
}

/// The status bar under the index, from `status_format`, with the index
/// shown from entry `first` in `height` rows.
fn index_status(app: &App, first: usize, height: usize, width: usize) -> String {
    let height = height.max(1);
    let position = expando::position(first, height, app.messages.len());
    let (sort, aux) = (app.ordering.sort.name(), app.ordering.aux.name());
    // The whole folder, as in mutt, whatever the limit shows: counted in
    // one pass.
    let (new, flagged) = app.folder_messages().fold((0, 0), |(new, flagged), m| {
        (
            new + usize::from(!m.flags.read),
            flagged + usize::from(m.flags.flagged),
        )
    });
    let status = Status {
        folder: app.folder.as_ref().map_or("", |f| f.name.as_str()),
        total: app.total_messages(),
        shown: app.limit.as_ref().map(|_| app.shown_count()),
        new,
        deleted: app.deleted_count(),
        flagged,
        incoming: app.mailboxes_with_new_mail(),
        limit: app.limit.as_ref().map(|limit| limit.text.as_str()),
        incomplete: app.incomplete(),
        changed: app.deleted_count() > 0 || app.saving(),
        position: &position,
        sort: (&sort, &aux),
    };
    format::expand(&app.display.status_format, &status, width)
}

/// Draws the pager body, under the mini-index if there is one, and returns
/// its status line, `status_width` cells wide.
fn render_pager(
    app: &App,
    pager: &Pager,
    frame: &mut Frame,
    area: Rect,
    status_width: u16,
) -> String {
    // The message as the index shows it: usually the selected one. IDs
    // are unique, so it is the only one.
    let index = if app
        .messages
        .get(app.selected)
        .is_some_and(|m| m.id == pager.id)
    {
        Some(app.selected)
    } else {
        app.messages.iter().position(|m| m.id == pager.id)
    };
    let area = render_mini_index(app, index, frame, area);
    let Some(message) = &pager.message else {
        frame.render_widget(Paragraph::new("Loading message..."), area);
        return "---mett: loading---".into();
    };
    // The app keeps the message wrapped as `wrap` says for the terminal's
    // width; wrap here only if the drawing area differs, so no text is
    // clipped.
    let width = app.display.wrap_width(usize::from(area.width));
    let cached = pager.rows_width == Some(width);
    let rewrapped: Vec<(usize, Range<usize>)> = if cached {
        Vec::new()
    } else {
        pager
            .lines
            .iter()
            .enumerate()
            .flat_map(|(line, text)| {
                text::wrap_ranges(text, width)
                    .into_iter()
                    .map(move |range| (line, range))
            })
            .collect()
    };
    let total = if cached {
        pager.row_count()
    } else {
        rewrapped.len()
    };
    let height = usize::from(area.height);
    let last_top = total.saturating_sub(height);
    let top = pager.scroll.min(last_top);
    let end = (top + height).min(total);
    // Only the rows on screen are looked at: each is part of a line.
    let rows: Vec<(usize, (usize, Range<usize>))> = (top..end)
        .map(|row| {
            let part = if cached {
                (pager.line_of(row), pager.row_ranges[row].clone())
            } else {
                rewrapped[row].clone()
            };
            (row, part)
        })
        .collect();
    // The last search's match, where the rows are the app's.
    let found = if cached {
        highlighted(pager)
    } else {
        Vec::new()
    };
    let colors = &app.display.colors;
    // `color body` matches, found once for each line on screen.
    let mut matches: std::collections::HashMap<usize, Vec<(Range<usize>, Style)>> =
        std::collections::HashMap::new();
    let visible: Vec<Line> = rows
        .iter()
        .map(|(row, (line, range))| {
            let text = &pager.lines[*line];
            let (style, body) = line_style(pager, *line, &app.display);
            // Byte by byte, what colors each part of the row.
            let mut marks: Vec<Option<Style>> = vec![None; range.len()];
            let mut restyle = |start: usize, end: usize, change: &dyn Fn(Style) -> Style| {
                let (start, end) = (start.max(range.start), end.min(range.end));
                for slot in marks
                    .iter_mut()
                    .take(end.saturating_sub(range.start))
                    .skip(start.saturating_sub(range.start))
                {
                    *slot = Some(change(slot.unwrap_or(style)));
                }
            };
            if body {
                for (found, rule) in matches.entry(*line).or_insert_with(|| {
                    colors
                        .body
                        .iter()
                        .flat_map(|rule| {
                            rule.regex
                                .find_iter(text)
                                .map(move |m| (m.range(), rule.style))
                        })
                        .collect()
                }) {
                    restyle(found.start, found.end, &|_| style.patch(*rule));
                }
            }
            // Links are underlined, in whatever color they have.
            for link in pager.links.iter().filter(|link| link.line == *line) {
                restyle(link.range.start, link.range.end, &|s| {
                    s.add_modifier(Modifier::UNDERLINED)
                });
            }
            for (_, found) in found.iter().filter(|(r, _)| r == row) {
                // Over a link, still underlined.
                restyle(range.start + found.start, range.start + found.end, &|s| {
                    let link = s.add_modifier & Modifier::UNDERLINED;
                    style.patch(colors.search).add_modifier(link)
                });
            }
            let row_text = &text[range.clone()];
            // Runs of one style, cut only between graphemes (a match in
            // part of one colors all of it), so that each is drawn whole.
            let mut spans: Vec<Span> = Vec::new();
            let mut run: Option<(usize, Style)> = None;
            for (at, grapheme) in row_text.grapheme_indices(true) {
                let marked = marks[at..at + grapheme.len()].iter().find_map(|m| *m);
                let this = marked.unwrap_or(style);
                match run {
                    Some((_, current)) if current == this => {}
                    Some((start, current)) => {
                        spans.push(Span::styled(row_text[start..at].to_owned(), current));
                        run = Some((at, this));
                    }
                    None => run = Some((at, this)),
                }
            }
            if let Some((start, current)) = run {
                spans.push(Span::styled(row_text[start..].to_owned(), current));
            }
            Line::from(spans).style(style)
        })
        .collect();
    frame.render_widget(Paragraph::new(visible), area);

    let position = expando::position(top, height, total);
    // The message as the index currently shows it, as in mutt's `%Z`: its
    // flags may have changed since it was fetched.
    let summary = index.map_or(&message.summary, |i| &app.messages[i]);
    let entry = Entry {
        number: index.unwrap_or(app.selected) + 1,
        summary,
        deleted: app.is_deleted(&pager.id),
        from_me: summary.from.as_ref().is_some_and(|a| app.is_me(a)),
        total: app.total_messages(),
        date_format: &app.display.date_format,
        position: Some(&position),
        thread: None,
    };
    format::expand(&app.display.pager_format, &entry, usize::from(status_width))
}

/// Draws the mini-index (`pager_index_lines`) at the top of `area`, as
/// mutt does: the index with entry `current` highlighted (the open
/// message, if the index shows it), about a third of
/// the way down when there are entries before and after it, and the
/// index's status bar under it. Returns the rest of `area`, for the
/// message.
fn render_mini_index(app: &App, current: Option<usize>, frame: &mut Frame, area: Rect) -> Rect {
    let rows = app
        .display
        .mini_index_rows(app.messages.len(), usize::from(area.height));
    if rows == 0 {
        return area;
    }
    let rows = u16::try_from(rows).unwrap_or(area.height);
    let [index, status, rest] = Layout::vertical([
        Constraint::Length(rows - 1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    let height = usize::from(index.height);
    // Until the index lists the open message again (as a refresh does),
    // around the selected entry, with none highlighted.
    let around = current.unwrap_or(app.selected);
    let first = mini_index_top(around, app.messages.len(), height, usize::from(rows) / 3);
    render_index(app, first, current, frame, index);
    let line = index_status(app, first, height, usize::from(status.width));
    frame.render_widget(
        Paragraph::new(line).style(app.display.colors.status),
        status,
    );
    rest
}

/// The first entry of a mini-index `height` rows tall over `count`
/// entries, so that entry `current` is on row `indicator`, unless that
/// would leave rows empty at the top or bottom (as mutt places it).
fn mini_index_top(current: usize, count: usize, height: usize, indicator: usize) -> usize {
    if current < indicator {
        0
    } else if count.saturating_sub(current) < height.saturating_sub(indicator) {
        count.saturating_sub(height)
    } else {
        current - indicator
    }
}

/// How a line of the message in the pager is drawn, and whether it is in
/// the body (which `color body` colors): headers with `color header` or
/// `hdrdefault`, quoted text by its depth, and the signature.
fn line_style(pager: &Pager, line: usize, display: &Display) -> (Style, bool) {
    let colors = &display.colors;
    let text = &pager.lines[line];
    if line < pager.body_start {
        return (colors.header_style(text), false);
    }
    if pager.signature.is_some_and(|start| line >= start) {
        return (colors.normal.patch(colors.signature), true);
    }
    let depth = display
        .quote
        .find(text)
        .filter(|found| found.start() == 0)
        .map_or(0, |found| {
            found
                .as_str()
                .chars()
                .filter(|c| !c.is_whitespace())
                .count()
        });
    if depth > 0 {
        (colors.quoted_style(depth), true)
    } else {
        (colors.normal, true)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Local, TimeDelta, Utc};
    use mett_core::WellKnownFolder;
    use mett_core::mock;
    use mett_msgraph::SignInPrompt;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use unicode_width::UnicodeWidthStr;

    use super::*;
    use crate::app::Msg;
    use crate::app::testing::*;
    use crate::worker::Event;

    const WIDTH: u16 = 60;
    const HEIGHT: u16 = 8;

    fn received(i: i64) -> DateTime<Utc> {
        // Midday, so the local date doesn't depend on the time zone.
        DateTime::UNIX_EPOCH + TimeDelta::hours(12) + TimeDelta::days(i)
    }

    /// The date as the index shows it, in this machine's time zone.
    fn index_date(i: i64) -> String {
        received(i)
            .with_timezone(&Local)
            .format("%b %d")
            .to_string()
    }

    fn app(n: i64) -> App {
        let (mut app, effects) = App::new(WIDTH, HEIGHT);
        let inbox = mock::folder("in", "Inbox", Some(WellKnownFolder::Inbox));
        let list = folder_opened(&mut app, first_request(&effects), inbox);
        let messages = (0..n)
            .map(|i| {
                // Newest first, as Graph lists them.
                let mut m = mock::message(
                    &format!("m{i}"),
                    "in",
                    &format!("Subject {i}"),
                    received(-i),
                );
                m.summary.from = Some(Address::new("alice@contoso.com").with_name("Alice Example"));
                m.summary.flags.flagged = i == 1;
                m.summary
            })
            .collect();
        listed(&mut app, list, messages, None);
        app
    }

    #[test]
    fn the_wait_for_a_throttling_server_ends_the_message_line() {
        let mut app = app(3);
        app.status = "Mail sent.".into();
        respond(
            &mut app,
            crate::worker::RequestId(90),
            crate::worker::Event::Throttled {
                wait: std::time::Duration::from_secs(12),
                attempt: 1,
            },
        );
        let line = sized_screen(&app, 100, HEIGHT).pop().unwrap();
        assert!(line.starts_with("Mail sent."), "{line}");
        // A narrow line has room for a short one, then for none.
        let narrow = sized_screen(&app, 40, HEIGHT).pop().unwrap();
        assert!(narrow.starts_with("Mail sent."), "{narrow}");
        assert!(narrow.ends_with("Throttled: 12s"), "{narrow}");
        let tiny = sized_screen(&app, 20, HEIGHT).pop().unwrap();
        assert_eq!(tiny, "Mail sent.");
        // An error keeps its own color, and its room.
        colored(&mut app, &["color error brightred default"]);
        app.status = "x".repeat(100);
        app.error = Some(app.status.clone());
        let error = styles(&app);
        let row = HEIGHT - 1;
        assert_eq!(error[(0, row)].fg, ratatui::style::Color::LightRed);
        assert_eq!(
            error[(WIDTH - 1, row)].fg,
            ratatui::style::Color::Reset,
            "the notice isn't an error"
        );
        assert!(
            line.ends_with("Throttled by the server: retry 1/5 in 12s"),
            "{line}"
        );
    }

    fn screen(app: &App) -> Vec<String> {
        sized_screen(app, WIDTH, HEIGHT)
    }

    /// `app` drawn on a `width`×`height` terminal.
    fn drawn(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                render(app, frame);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn sized_screen(app: &App, width: u16, height: u16) -> Vec<String> {
        let buffer = drawn(app, width, height);
        (0..height)
            .map(|y| {
                let mut line = String::new();
                let mut x = 0;
                while x < width {
                    let symbol = buffer[(x, y)].symbol();
                    line.push_str(symbol);
                    // A wide character also covers the next cell.
                    x += u16::try_from(symbol.width().max(1)).unwrap();
                }
                line.trim_end().to_owned()
            })
            .collect()
    }

    /// The style of each cell of `app` drawn.
    fn styles(app: &App) -> ratatui::buffer::Buffer {
        sized_styles(app, HEIGHT)
    }

    fn sized_styles(app: &App, height: u16) -> ratatui::buffer::Buffer {
        drawn(app, WIDTH, height)
    }

    /// Alice is asked to sign in with `prompt`.
    fn sign_in(app: &mut App, prompt: SignInPrompt) {
        app.update(Msg::SignIn(crate::signin::SignInEvent::Prompt {
            account: mett_core::AccountId::new("alice@contoso.com"),
            prompt,
        }));
    }

    fn code_prompt() -> SignInPrompt {
        SignInPrompt::EnterCode {
            verification_uri: "https://microsoft.com/devicelogin".into(),
            user_code: "ABCD-EFGH".into(),
            message: String::new(),
        }
    }

    fn colored(app: &mut App, commands: &[&str]) {
        for command in commands {
            let words: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
            let (name, args) = words.split_first().unwrap();
            assert_eq!(name, "color");
            app.display.colors.color(args).unwrap();
        }
    }

    #[test]
    fn colors_the_index_and_the_status_bar() {
        use ratatui::style::Color;
        let mut app = app(3);
        colored(
            &mut app,
            &[
                "color index red default ~F",
                "color indicator black yellow",
                "color status white blue",
            ],
        );
        let buffer = styles(&app);
        assert_eq!(buffer[(5, 1)].bg, Color::Yellow, "the selected entry");
        let right = buffer.area.width - 1;
        assert_eq!(buffer[(right, 1)].bg, Color::Yellow, "to the right edge");
        assert_eq!(buffer[(5, 2)].fg, Color::Red, "the flagged one");
        assert_eq!(buffer[(5, 3)].fg, Color::Reset);
        assert_eq!(buffer[(0, 6)].bg, Color::Blue);
        assert_eq!(buffer[(0, 0)].bg, Color::Blue, "the help line too");
        // Errors in their own color.
        colored(&mut app, &["color error brightred default"]);
        app.status = "Mail sent.".into();
        assert_eq!(styles(&app)[(0, 7)].fg, Color::Reset, "not an error");
        // As in mutt, this is an error.
        act(&mut app, Action::PreviousEntry);
        assert_eq!(styles(&app)[(0, 7)].fg, Color::LightRed);
        let fetch = first_request(&act(&mut app, Action::DisplayMessage));
        respond(
            &mut app,
            fetch,
            Event::MessageFetched(Err(mett_core::Error::Network("down".into()))),
        );
        assert_eq!(styles(&app)[(0, 7)].fg, Color::LightRed, "{}", app.status);
    }

    #[test]
    fn colors_headers_quotes_and_the_signature() {
        use ratatui::style::Color;
        let mut app = app(1);
        colored(
            &mut app,
            &[
                "color hdrdefault cyan default",
                "color header yellow default ^From:",
                "color quoted green default",
                "color quoted1 magenta default",
                "color signature blue default",
                "color body brightwhite default wor..",
                "color body red default afe",
            ],
        );
        app.update(Msg::Resize {
            width: WIDTH,
            height: 16,
        });
        let fetch = first_request(&act(&mut app, Action::DisplayMessage));
        let mut message = mock::message("m0", "in", "Hi", received(0));
        message.summary.from = Some(Address::new("alice@contoso.com"));
        message.body.content = "> a\n>> b\nhello world\ncafe\u{301} ok\n-- \nAlice".into();
        respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
        let buffer = sized_styles(&app, 16);
        let lines = sized_screen(&app, WIDTH, 16);
        let row = |text: &str| {
            let y = lines.iter().position(|l| l.starts_with(text)).unwrap();
            u16::try_from(y).unwrap()
        };
        assert_eq!(buffer[(0, row("Date:"))].fg, Color::Cyan);
        assert_eq!(buffer[(0, row("From:"))].fg, Color::Yellow);
        assert_eq!(buffer[(0, row("> a"))].fg, Color::Green);
        assert_eq!(buffer[(0, row(">> b"))].fg, Color::Magenta);
        assert_eq!(buffer[(0, row("hello"))].fg, Color::Reset);
        assert_eq!(
            buffer[(6, row("hello"))].fg,
            Color::White,
            "matched by the body rule"
        );
        assert_eq!(buffer[(0, row("Alice"))].fg, Color::Blue);
        // A match in part of a grapheme colors all of it, drawn whole.
        let cafe = row("caf");
        assert_eq!(buffer[(3, cafe)].symbol(), "e\u{301}");
        assert_eq!(buffer[(3, cafe)].fg, Color::Red);
        assert_eq!(buffer[(4, cafe)].symbol(), " ");
    }

    #[test]
    fn draws_the_builtin_editor() {
        let mut app = app(1);
        let editor =
            crate::editor::builtin::Builtin::new("Hi!\n-- \nMe\n", crate::editor::Keys::Vim);
        app.editing = Some(std::cell::RefCell::new(editor));
        let lines = sized_screen(&app, WIDTH, 6);
        assert_eq!(lines[..3], ["Hi!", "--", "Me"]);
        assert_eq!(lines[5], ":wq sends the text on, :q! drops the changes");
        // The bottom line has the cursor while a command is typed.
        app.update(Msg::Key(KeyEvent::from(KeyCode::Char(':'))));
        app.update(Msg::Key(KeyEvent::from(KeyCode::Char('w'))));
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, 6)).unwrap();
        terminal
            .draw(|frame| {
                render(&app, frame);
            })
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(0, 5)].symbol(), ":");
        terminal.backend_mut().assert_cursor_position((2, 5));
    }

    #[test]
    fn the_compose_menu_keeps_the_selected_part_in_view() {
        let mut app = app(1);
        let mut draft = mett_core::Draft::default();
        for i in 0..6 {
            draft.attachments.push(mett_core::OutgoingAttachment {
                name: format!("file{i}.txt"),
                content_type: "text/plain".into(),
                data: Vec::new(),
            });
        }
        let mut compose = crate::app::Compose::new(draft, false);
        compose.selected = 6;
        app.screens.push(Screen::Compose(compose));
        let lines = sized_screen(&app, WIDTH, 12);
        assert!(lines.iter().any(|l| l.contains("file5.txt")), "{lines:#?}");
    }

    #[test]
    fn draws_the_index_like_mutt() {
        let lines = screen(&app(2));
        assert_eq!(lines[0], "q:Quit  j:Next  k:Prev  <Enter>:Display");
        assert_eq!(
            lines[1],
            format!("   1 N   {} Alice Example    Subject 0", index_date(0))
        );
        assert_eq!(
            lines[2],
            format!("   2 N!  {} Alice Example    Subject 1", index_date(-1))
        );
        assert_eq!(
            lines[6],
            status_bar("---mett: Inbox [Msgs:2 New:2 Flag:1]---", "(all)---")
        );
    }

    /// A status bar `WIDTH` cells wide, with `left` and `right` and dashes
    /// between, as `%>-` makes them.
    fn status_bar(left: &str, right: &str) -> String {
        filled(left, '-', right)
    }

    /// `left` and `right`, `WIDTH` cells apart, with `fill` between.
    fn filled(left: &str, fill: char, right: &str) -> String {
        let gap = usize::from(WIDTH) - left.width() - right.width();
        format!("{left}{}{right}", fill.to_string().repeat(gap))
    }

    #[test]
    fn the_index_scrolls_a_page_at_a_time() {
        let mut app = app(12);
        // 5 body rows: entry 7 is on the second page.
        for _ in 0..6 {
            act(&mut app, Action::NextEntry);
        }
        let lines = screen(&app);
        assert!(lines[1].starts_with("   6 "), "{lines:#?}");
        assert!(lines[2].starts_with("   7 "), "{lines:#?}");
    }

    #[test]
    fn a_collapsed_thread_with_unread_mail_folded_in_shows_as_unread() {
        // Threads "a" (a0, a1) and "b" (b0, b1): only a1 is unread.
        let messages = [
            ("b1", "b", 1),
            ("a1", "a", 1),
            ("b0", "b", 0),
            ("a0", "a", 0),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (id, conversation, depth))| {
            let mut m = mock::message(id, "in", "Plans", received(-(i as i64))).summary;
            m.flags.read = id != "a1";
            m.thread = Some(mett_core::ThreadPosition {
                conversation: conversation.into(),
                path: (0..=depth).map(|step| vec![step]).collect(),
            });
            m
        })
        .collect();
        let (mut app, _) = opened(sorted("threads"), HEIGHT, messages, None);
        act(&mut app, Action::CollapseAll);
        // Collapsing every thread leaves "a" open, with unread mail: it is
        // collapsed on its own.
        app.selected = app
            .messages
            .iter()
            .position(|m| m.id.as_str() == "a0")
            .unwrap();
        act(&mut app, Action::CollapseThread);
        let lines = sized_screen(&app, WIDTH, HEIGHT);
        let row = |id: &str| {
            let i = app
                .messages
                .iter()
                .position(|m| m.id.as_str() == id)
                .unwrap();
            lines[i + 1].clone()
        };
        assert!(row("a0").starts_with("   1 N"), "{lines:#?}");
        assert!(row("b0").starts_with("   2  "), "{lines:#?}");
    }

    #[test]
    fn index_columns_line_up_with_wide_characters() {
        let mut app = app(1);
        app.messages[0].from = Some(Address::new("x@example.com").with_name("山田太郎さん"));
        let lines = screen(&app);
        // The 15-cell sender column: 6 two-cell characters and 3 spaces of
        // padding, then the 2-space gap before the subject.
        assert!(
            lines[1].ends_with("山田太郎さん     Subject 0"),
            "{lines:#?}"
        );
    }

    #[test]
    fn draws_the_pager_with_headers_and_body() {
        let mut app = app(1);
        let fetch = first_request(&act(&mut app, Action::DisplayMessage));
        assert_eq!(screen(&app)[1], "Loading message...");

        let mut message = mock::message("m0", "in", "Subject 0", received(0));
        message.summary.from = Some(Address::new("alice@contoso.com").with_name("Alice Example"));
        respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
        let lines = screen(&app);
        assert_eq!(lines[0], "i:Exit  -:PrevPg  <Space>:NextPg  j:Next  k:Prev");
        let date = received(0).with_timezone(&Local).format("%a, %d %b %Y");
        assert!(lines[1].starts_with(&format!("Date: {date}")), "{lines:#?}");
        assert_eq!(lines[2], "From: Alice Example <alice@contoso.com>");
        assert_eq!(lines[3], "Subject: Subject 0");
        assert_eq!(lines[5], "Body of Subject 0");
        // Shown, so now read.
        let pager_status = |flags: &str| {
            let left = format!("-{flags}- 1/1: Alice Example          Subject 0");
            filled(&left, ' ', "  -- (all)")
        };
        assert_eq!(lines[6], pager_status("   "));
        // Marked for deletion from the pager (the only message, so it stays).
        act(&mut app, Action::DeleteMessage);
        assert_eq!(screen(&app)[6], pager_status("D  "));
    }

    #[test]
    fn the_pager_wraps_where_wrap_says_under_a_full_status_bar() {
        let mut app = app(1);
        let mut settings = crate::app::Settings::default();
        settings.display.wrap = 20;
        app.update(configured(settings));
        app.update(Msg::Resize {
            width: WIDTH,
            height: 16,
        });
        let fetch = first_request(&act(&mut app, Action::DisplayMessage));
        let mut message = mock::message("m0", "in", "Subject 0", received(0));
        message.body.content = "alpha beta gamma delta epsilon zeta eta theta".into();
        respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
        let lines = sized_screen(&app, WIDTH, 16);
        let body = lines.iter().position(|l| l.starts_with("alpha")).unwrap();
        assert_eq!(
            lines[body..body + 3],
            ["alpha beta gamma", "delta epsilon zeta", "eta theta"],
            "{lines:#?}"
        );
        assert!(lines[1..14].iter().all(|l| l.width() <= 20), "{lines:#?}");
        assert_eq!(lines[14].width(), usize::from(WIDTH), "{lines:#?}");
        assert!(lines[14].ends_with("-- (all)"), "{lines:#?}");
    }

    /// `app` with `pager_index_lines` set to `lines`, on a `height`-row
    /// terminal.
    fn with_mini_index(app: &mut App, lines: usize, height: u16) {
        let mut settings = crate::app::Settings::default();
        settings.display.pager_index_lines = lines;
        app.update(configured(settings));
        app.update(Msg::Resize {
            width: WIDTH,
            height,
        });
    }

    /// Opens the selected message, as m0 with `body`.
    fn read(app: &mut App, action: Action, body: &str) {
        let fetch = first_request(&act(app, action));
        let mut message = mock::message("m0", "in", "Subject 0", received(0));
        message.body.content = body.into();
        respond(app, fetch, Event::MessageFetched(Ok(message)));
    }

    /// The numbers of the index entries on `lines`, in order.
    fn numbers(lines: &[String]) -> Vec<usize> {
        lines
            .iter()
            .map(|l| l[..4].trim().parse().unwrap())
            .collect()
    }

    #[test]
    fn the_pager_shows_a_mini_index_above_the_message() {
        use ratatui::style::Color;
        let mut app = app(10);
        with_mini_index(&mut app, 4, 16);
        colored(&mut app, &["color indicator black yellow"]);
        read(&mut app, Action::DisplayMessage, "Body");
        let lines = sized_screen(&app, WIDTH, 16);
        // 3 entries, then the index's status bar, then the message.
        assert_eq!(numbers(&lines[1..4]), [1, 2, 3], "{lines:#?}");
        assert!(lines[4].contains("-mett: Inbox [Msgs:10"), "{lines:#?}");
        assert!(lines[5].starts_with("Date: "), "{lines:#?}");
        assert!(lines[14].starts_with("-   - 1/10: "), "{lines:#?}");
        let buffer = drawn(&app, WIDTH, 16);
        assert_eq!(buffer[(5, 1)].bg, Color::Yellow, "the open message");
        assert_eq!(buffer[(5, 2)].bg, Color::Reset);

        // The next message, as mutt places it: a third of the way down.
        for _ in 0..5 {
            read(&mut app, Action::NextEntry, "Body");
        }
        let lines = sized_screen(&app, WIDTH, 16);
        assert_eq!(numbers(&lines[1..4]), [5, 6, 7], "{lines:#?}");
        assert_eq!(drawn(&app, WIDTH, 16)[(5, 2)].bg, Color::Yellow);
        // Near the end, the index stays full.
        for _ in 0..4 {
            read(&mut app, Action::NextEntry, "Body");
        }
        let lines = sized_screen(&app, WIDTH, 16);
        assert_eq!(numbers(&lines[1..4]), [8, 9, 10], "{lines:#?}");
        assert_eq!(drawn(&app, WIDTH, 16)[(5, 3)].bg, Color::Yellow);
    }

    #[test]
    fn the_mini_index_highlights_nothing_while_the_open_message_isnt_listed() {
        use ratatui::style::Color;
        let mut app = app(10);
        with_mini_index(&mut app, 4, 16);
        colored(&mut app, &["color indicator black yellow"]);
        read(&mut app, Action::DisplayMessage, "Body");
        // As while a refresh lists the folder again.
        app.messages.retain(|m| m.id.as_str() != "m0");
        let buffer = drawn(&app, WIDTH, 16);
        for y in 1..4 {
            assert_ne!(buffer[(5, y)].bg, Color::Yellow, "row {y}");
        }
    }

    #[test]
    fn the_mini_index_takes_only_the_rows_it_needs() {
        let mut app = app(2);
        with_mini_index(&mut app, 10, 16);
        read(&mut app, Action::DisplayMessage, "Body");
        let lines = sized_screen(&app, WIDTH, 16);
        assert_eq!(numbers(&lines[1..3]), [1, 2], "{lines:#?}");
        assert!(lines[3].contains("-mett: Inbox"), "{lines:#?}");
        assert!(lines[4].starts_with("Date: "), "{lines:#?}");
    }

    #[test]
    fn a_small_terminal_shrinks_the_mini_index_then_leaves_it_out() {
        let mut app = app(10);
        with_mini_index(&mut app, 6, HEIGHT);
        read(&mut app, Action::DisplayMessage, "Body");
        // 5 rows: one entry and the status bar, and 3 for the message.
        let lines = screen(&app);
        assert_eq!(numbers(&lines[1..2]), [1], "{lines:#?}");
        assert!(lines[2].contains("-mett: Inbox"), "{lines:#?}");
        assert!(lines[3].starts_with("Date: "), "{lines:#?}");
        // 4 rows: only the message.
        app.update(Msg::Resize {
            width: WIDTH,
            height: 7,
        });
        let lines = sized_screen(&app, WIDTH, 7);
        assert!(lines[1].starts_with("Date: "), "{lines:#?}");
    }

    #[test]
    fn the_sign_in_window_takes_the_normal_colors() {
        use ratatui::style::Color;
        let mut app = app(2);
        colored(&mut app, &["color normal white blue"]);
        sign_in(&mut app, code_prompt());
        let buffer = styles(&app);
        let lines = screen(&app);
        let code = lines.iter().position(|l| l.contains("ABCD-EFGH")).unwrap();
        assert_eq!(buffer[(0, u16::try_from(code).unwrap())].bg, Color::Blue);
    }

    #[test]
    fn draws_the_sign_in_window_over_the_index() {
        let mut app = app(2);
        sign_in(&mut app, code_prompt());
        let lines = sized_screen(&app, 60, 12);
        assert_eq!(lines[0], "q:Close  o:Browser  c:Copy");
        let window = lines.join("\n");
        assert!(
            window.contains("Sign in to alice@contoso.com"),
            "{lines:#?}"
        );
        assert!(window.contains("    ABCD-EFGH"), "{lines:#?}");
        assert!(
            window.contains("Waiting for you to sign in..."),
            "{lines:#?}"
        );

        signed_out_as(&mut app, "denied");
        let lines = sized_screen(&app, 60, 12);
        assert_eq!(lines[0], "q:Close  <Enter>:Retry");
        let window = lines.join("\n");
        assert!(window.contains("Sign-in failed: denied"), "{window}");
    }

    #[test]
    fn a_long_sign_in_link_can_be_scrolled_into_view() {
        let mut app = app(1);
        let url = format!("https://login.example/authorize?{}end", "x".repeat(400));
        sign_in(
            &mut app,
            SignInPrompt::OpenBrowser {
                url: url.parse().unwrap(),
            },
        );
        let lines = screen(&app);
        assert!(lines[1].contains("j/k to scroll"), "{lines:#?}");
        for _ in 0..20 {
            act(&mut app, Action::NextLine);
        }
        let shown = screen(&app).join("\n");
        assert!(shown.contains("end"), "{shown}");
        assert!(shown.contains("Waiting for you to sign in..."), "{shown}");
    }

    #[test]
    fn the_sign_in_link_is_handed_over_whole() {
        let mut app = app(1);
        let url = format!("https://login.example/authorize?{}end", "x".repeat(80));
        sign_in(
            &mut app,
            SignInPrompt::OpenBrowser {
                url: url.parse().unwrap(),
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, 16)).unwrap();
        let mut overlay = None;
        terminal
            .draw(|frame| overlay = render(&app, frame))
            .unwrap();
        let link = overlay.take().expect("the link fits");
        assert_eq!(link.text, url);
        assert_eq!(link.x, 0);
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, link.y)].symbol(), "h", "where the link starts");

        // A link that doesn't fit isn't handed over.
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, 5)).unwrap();
        terminal
            .draw(|frame| overlay = render(&app, frame))
            .unwrap();
        assert!(overlay.is_none());
    }

    #[test]
    fn draws_prompts_and_the_compose_menu() {
        let mut app = app(1);
        act(&mut app, Action::Mail);
        typed(&mut app, "bob@x.org");
        assert_eq!(screen(&app)[7], "To: bob@x.org");
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);
        assert_eq!(screen(&app)[7], "No subject, abort? ([yes]/no):");
        key(&mut app, KeyCode::Char('n'));
        app.update(Msg::Edited(Ok("x".repeat(2048))));
        let lines = sized_screen(&app, WIDTH, 12);
        assert_eq!(
            sized_screen(&app, 80, 12)[0],
            "y:Send  q:Abort  t:To  c:CC  b:Bcc  s:Subj  e:Edit  P:Postpone  a:Attach"
        );
        assert_eq!(lines[2], "      To: bob@x.org");
        assert_eq!(lines[5], " Subject:");
        assert_eq!(
            lines[8],
            "- I     1 <message text>  [text/plain, utf-8, 2.0K]"
        );
        assert_eq!(
            lines[10],
            "-- mett: Compose  [Approx. msg size: 2.0K   Atts: 1]---"
        );
    }

    #[test]
    fn links_are_underlined() {
        let mut app = app(1);
        let fetch = first_request(&act(&mut app, Action::DisplayMessage));
        let mut message = mock::message("m0", "in", "Subject 0", received(0));
        message.body.content = "Read the docs<https://example.com>.".into();
        respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
        assert_eq!(screen(&app)[4], "Read the docs.");
        let buffer = drawn(&app, WIDTH, HEIGHT);
        let underlined = |x: u16| buffer[(x, 4)].modifier.contains(Modifier::UNDERLINED);
        assert!((9..13).all(underlined), "the link's text");
        assert!(!underlined(8) && !underlined(13));

        // Found by a search, it is still underlined.
        act(&mut app, Action::Search);
        typed(&mut app, "the d");
        key(&mut app, KeyCode::Enter);
        let buffer = drawn(&app, WIDTH, HEIGHT);
        let found = |x: u16| buffer[(x, 4)].modifier.contains(Modifier::REVERSED);
        let underlined = |x: u16| buffer[(x, 4)].modifier.contains(Modifier::UNDERLINED);
        assert!((5..10).all(found), "the match");
        assert!((9..13).all(underlined) && !underlined(5) && !underlined(8));

        act(&mut app, Action::ToggleLinks);
        assert_eq!(screen(&app)[4], "Read the docs<https://example.com>.");
        let buffer = drawn(&app, WIDTH, HEIGHT);
        let underlined = |x: u16| buffer[(x, 4)].modifier.contains(Modifier::UNDERLINED);
        assert!((14..33).all(underlined), "the URL");
        assert!(!underlined(10) && !underlined(13) && !underlined(33));
    }

    #[test]
    fn sizes_read_like_mutt() {
        assert_eq!(format_size(100), "0.1K");
        assert_eq!(format_size(20 * 1024), "20K");
        assert_eq!(format_size(3 * 1024 * 1024 / 2), "1.5M");
    }

    #[test]
    fn a_forward_shows_the_attachments_it_carries() {
        let mut app = app(1);
        act(&mut app, Action::Forward);
        let fetch = first_request(&act(&mut app, Action::Forward));
        let mut message = mock::message("m0", "in", "Subject 0", received(0));
        message.summary.has_attachments = true;
        respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
        typed(&mut app, "bob@x.org");
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);
        app.update(Msg::Edited(Ok("see below".into())));
        let lines = sized_screen(&app, WIDTH, 13);
        assert_eq!(lines[9], "- A     2 <the forwarded message's attachments>");
        assert!(lines[11].contains("Atts: 2"), "{lines:#?}");
    }

    #[test]
    fn marks_deleted_messages_and_lists_folders() {
        let mut app = app(2);
        act(&mut app, Action::DeleteMessage);
        let lines = screen(&app);
        assert!(lines[1].starts_with("   1 D   "), "{lines:#?}");
        assert_eq!(
            lines[6],
            status_bar("-*-mett: Inbox [Msgs:2 New:2 Del:1 Flag:1]---", "(all)---")
        );

        let list = first_request(&act(&mut app, Action::BrowseMailboxes));
        assert_eq!(screen(&app)[1], "Listing folders...");
        let mut inbox = mock::folder("in", "Inbox", Some(WellKnownFolder::Inbox));
        inbox.unread_count = 2;
        inbox.total_count = 10;
        let projects = mett_core::Folder {
            parent: Some("in".into()),
            ..mock::folder("p", "Projects", None)
        };
        respond(
            &mut app,
            list,
            Event::FoldersListed(Ok(vec![inbox, projects])),
        );
        let lines = screen(&app);
        assert_eq!(lines[0], "q:Exit  <Enter>:Select  j:Next  k:Prev");
        assert_eq!(lines[1], "   1 N     2/10     Inbox");
        assert_eq!(lines[2], "   2       0/0        Projects");
        assert_eq!(lines[6], "---mett: Folders [2]---");
    }

    #[test]
    fn an_empty_folder_list_says_so() {
        let mut app = app(1);
        let list = first_request(&act(&mut app, Action::BrowseMailboxes));
        respond(&mut app, list, Event::FoldersListed(Ok(Vec::new())));
        assert_eq!(screen(&app)[1], "No folders.");
    }

    /// Tabs for three accounts, the second shown, the third with new mail.
    fn tabs(position: TabPosition) -> crate::app::Tabs {
        use crate::app::{Tab, Tabs};
        Tabs {
            tabs: ["Work", "Home", "Club"]
                .iter()
                .enumerate()
                .map(|(at, label)| Tab {
                    label: (*label).to_owned(),
                    new_mail: at == 2,
                })
                .collect(),
            active: 1,
            position,
        }
    }

    #[test]
    fn the_tabs_take_the_top_line_or_the_bottom_one() {
        // One row of the screen is the tabs': the app has the rest.
        let mut app = App::new(WIDTH, HEIGHT - 1).0;
        app.tabs = Some(tabs(TabPosition::Top));
        let lines = screen(&app);
        assert_eq!(lines[0], " 1 Work  2 Home  3 Club*");
        // What follows is the app's own screen, from its help line down.
        assert!(lines[1].starts_with("q:Quit"), "{:?}", lines[1]);

        app.tabs = Some(tabs(TabPosition::Bottom));
        let lines = screen(&app);
        assert!(lines[0].starts_with("q:Quit"), "{:?}", lines[0]);
        assert_eq!(lines[HEIGHT as usize - 1], " 1 Work  2 Home  3 Club*");
    }

    #[test]
    fn the_tab_shown_is_the_indicator_and_one_with_new_mail_stands_out() {
        use ratatui::style::Color;
        let mut app = App::new(WIDTH, HEIGHT - 1).0;
        colored(
            &mut app,
            &["color status white blue", "color indicator black yellow"],
        );
        app.tabs = Some(tabs(TabPosition::Top));
        let buffer = styles(&app);
        // " 1 Work " is 8 cells, " 2 Home " the next 8.
        assert_eq!(buffer[(2, 0)].bg, Color::Blue, "another account's tab");
        assert_eq!(buffer[(10, 0)].bg, Color::Yellow, "the one shown");
        assert!(
            buffer[(20, 0)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert_eq!(
            buffer[(50, 0)].bg,
            Color::Blue,
            "the bar goes on to the edge"
        );
    }

    #[test]
    fn without_tabs_the_screen_is_the_apps_alone() {
        let app = App::new(WIDTH, HEIGHT).0;
        assert!(screen(&app)[0].starts_with("q:Quit"));
    }

    #[test]
    fn the_tab_shown_stays_in_view_when_the_tabs_are_too_many_for_the_line() {
        use crate::app::{Tab, Tabs};
        let mut app = App::new(WIDTH, HEIGHT - 1).0;
        let labels = [
            "First account",
            "Second account",
            "Third account",
            "Fourth account",
        ];
        app.tabs = Some(Tabs {
            tabs: labels
                .iter()
                .map(|label| Tab {
                    label: (*label).to_owned(),
                    new_mail: false,
                })
                .collect(),
            active: 3,
            position: TabPosition::Top,
        });
        // 60 columns: the four need more than that.
        let line = screen(&app)[0].clone();
        assert!(line.contains("4 Fourth account"), "{line}");
        assert!(!line.contains("1 First account"), "{line}");
        // With the first shown, they start from it.
        if let Some(tabs) = &mut app.tabs {
            tabs.active = 0;
        }
        assert!(screen(&app)[0].starts_with(" 1 First account"));
    }
}
