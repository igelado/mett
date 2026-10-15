//! Drawing the [`App`]: the "view" of the Elm Architecture. It reads the
//! model and never changes it.
//!
//! The layout follows mutt: a help line at the top, the index or pager in
//! the middle, a status bar, and a message line at the bottom.

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
use crate::app::{App, Browser, Compose, Help, Input, Pager, Row, Screen, SignIn, highlighted};
use crate::color::Colors;
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
    let keymap = &app.keymap;
    let [help, body, status, message] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let colors = &app.display.colors;
    // Text without colors of its own is `normal`.
    frame.render_widget(Block::new().style(colors.normal), frame.area());

    frame.render_widget(
        Paragraph::new(help_line(app, keymap, app.menu())).style(colors.status),
        help,
    );
    let status_line = match app.screen() {
        Screen::Index => {
            render_index(app, frame, body);
            index_status(app, usize::from(body.height), usize::from(body.width))
        }
        Screen::Pager(pager) => render_pager(app, pager, frame, body),
        Screen::Compose(compose) => render_compose(compose, colors, frame, body),
        Screen::Browser(browser) => render_browser(browser, colors, frame, body),
        Screen::Help(help) => render_help(app, help, frame, body),
    };
    frame.render_widget(Paragraph::new(status_line).style(colors.status), status);
    match &app.input {
        Some(input) => render_input(input, colors.prompt, frame, message),
        None => {
            let style = if app.error.as_deref() == Some(app.status.as_str()) {
                colors.error
            } else {
                colors.message
            };
            frame.render_widget(Paragraph::new(app.status.as_str()).style(style), message);
        }
    }
    // The window may cover everything but the help line, which lists its
    // keys: on a small terminal, the sign-in instructions matter more than
    // the index behind them.
    if let Some(window) = &app.sign_in {
        let [_, below_help] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(frame.area());
        return render_sign_in(window, keymap, colors.normal, frame, below_help);
    }
    None
}

/// Draws the help screen and returns its status line.
fn render_help(app: &App, help: &Help, frame: &mut Frame, area: Rect) -> String {
    let width = usize::from(area.width);
    let lines: Vec<Line> = app
        .help_lines(help.menu)
        .iter()
        .skip(help.scroll)
        .take(usize::from(area.height))
        .map(|line| Line::raw(truncate(line, width)))
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
    let height = usize::from(area.height).max(1);
    let first = browser.selected / height * height;
    let width = usize::from(area.width);
    let lines: Vec<Line> = browser
        .entries
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(i, entry)| {
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
            let line = Line::raw(truncate(&line, width));
            if i == browser.selected {
                line.style(colors.indicator)
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
    format!("---mett: {title} [{}]---", browser.entries.len())
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
    let size = format_size(draft.body.len());
    let mut lines: Vec<Line> = headers
        .into_iter()
        .map(|(name, value)| Line::raw(truncate(&format!("{name:>8}: {value}"), width)))
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::raw("-- Attachments"));
    lines.push(
        Line::raw(truncate(
            &format!("- I     1 <message text>  [text/plain, utf-8, {size}]"),
            width,
        ))
        .style(colors.indicator),
    );
    let mut parts = 1;
    if compose.forwarded_attachments {
        parts += 1;
        lines.push(
            Line::raw(truncate(
                "- A     2 <the forwarded message's attachments>",
                width,
            ))
            .style(colors.attachment),
        );
    }
    frame.render_widget(Paragraph::new(lines), area);
    format!("-- mett: Compose  [Approx. msg size: {size}   Atts: {parts}]---")
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

fn render_index(app: &App, frame: &mut Frame, area: Rect) {
    let height = usize::from(area.height).max(1);
    // Like mutt, scroll a page at a time.
    let first = app.selected / height * height;
    let width = usize::from(area.width);
    let colors = &app.display.colors;
    let total = app.total_messages();
    let lines: Vec<Line> = app
        .messages
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(i, message)| {
            let deleted = app.is_deleted(&message.id);
            let entry = Entry {
                number: i + 1,
                summary: message,
                deleted,
                from_me: message.from.as_ref().is_some_and(|a| app.is_me(a)),
                total,
                date_format: &app.display.date_format,
                position: None,
            };
            // Padded to the right edge, so that the row's colors cover it.
            let mut text = format::expand(&app.display.index_format, &entry, width);
            let used = text.width();
            text.push_str(&" ".repeat(width.saturating_sub(used)));
            let line = Line::raw(text);
            if i == app.selected {
                line.style(colors.indicator)
            } else {
                line.style(colors.index_style(|pattern| {
                    pattern.matches(&Candidate {
                        summary: message,
                        deleted,
                    })
                }))
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// The status bar under the index, from `status_format`.
fn index_status(app: &App, height: usize, width: usize) -> String {
    let height = height.max(1);
    let first = app.selected / height * height;
    let position = expando::position(first, height, app.messages.len());
    let status = Status {
        folder: app.folder.as_ref().map_or("", |f| f.name.as_str()),
        total: app.total_messages(),
        shown: app.limit.as_ref().map(|_| app.messages.len()),
        // The whole folder, as in mutt, whatever the limit shows.
        new: app.folder_messages().filter(|m| !m.flags.read).count(),
        deleted: app.deleted_count(),
        flagged: app.folder_messages().filter(|m| m.flags.flagged).count(),
        incoming: app.mailboxes_with_new_mail(),
        limit: app.limit.as_ref().map(|limit| limit.text.as_str()),
        incomplete: app.incomplete(),
        changed: app.deleted_count() > 0 || app.saving(),
        position: &position,
    };
    format::expand(&app.display.status_format, &status, width)
}

/// Draws the pager body and returns its status line.
fn render_pager(app: &App, pager: &Pager, frame: &mut Frame, area: Rect) -> String {
    let Some(message) = &pager.message else {
        frame.render_widget(Paragraph::new("Loading message..."), area);
        return "---mett: loading---".into();
    };
    // The app keeps the message wrapped to the terminal width; wrap here
    // only if the drawing area differs, so no text is clipped.
    let width = usize::from(area.width);
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
        pager.rows.len()
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
                let line = pager.line_rows.partition_point(|&first| first <= row) - 1;
                (line, pager.row_ranges[row].clone())
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
            let mut mark = |start: usize, end: usize, style: Style| {
                let (start, end) = (start.max(range.start), end.min(range.end));
                for slot in marks
                    .iter_mut()
                    .take(end.saturating_sub(range.start))
                    .skip(start.saturating_sub(range.start))
                {
                    *slot = Some(style);
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
                    mark(found.start, found.end, style.patch(*rule));
                }
            }
            for (_, found) in found.iter().filter(|(r, _)| r == row) {
                mark(
                    range.start + found.start,
                    range.start + found.end,
                    style.patch(colors.search),
                );
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
    let index = app.messages.iter().position(|m| m.id == pager.id);
    let summary = index.map_or(&message.summary, |i| &app.messages[i]);
    let entry = Entry {
        number: index.unwrap_or(app.selected) + 1,
        summary,
        deleted: app.is_deleted(&pager.id),
        from_me: summary.from.as_ref().is_some_and(|a| app.is_me(a)),
        total: app.total_messages(),
        date_format: &app.display.date_format,
        position: Some(&position),
    };
    format::expand(&app.display.pager_format, &entry, width)
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
    use mett_core::mock;
    use mett_core::{MessagePage, WellKnownFolder};
    use mett_msgraph::SignInPrompt;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use unicode_width::UnicodeWidthStr;

    use super::*;
    use crate::app::{Effect, Msg};
    use crate::worker::{Event, Response};

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
        let Effect::Request(open, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let effects = app.update(Msg::Response(Response {
            id: open,
            event: Event::FolderOpened(Ok(mock::folder(
                "in",
                "Inbox",
                Some(WellKnownFolder::Inbox),
            ))),
        }));
        let Effect::Request(list, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let messages = (0..n)
            .map(|i| {
                let mut m =
                    mock::message(&format!("m{i}"), "in", &format!("Subject {i}"), received(i));
                m.summary.from = Some(Address::new("alice@contoso.com").with_name("Alice Example"));
                m.summary.flags.flagged = i == 1;
                m.summary
            })
            .collect();
        app.update(Msg::Response(Response {
            id: list,
            event: Event::MessagesListed(Ok(MessagePage {
                messages,
                next: None,
            })),
        }));
        app
    }

    fn screen(app: &App) -> Vec<String> {
        sized_screen(app, WIDTH, HEIGHT)
    }

    fn sized_screen(app: &App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                render(app, frame);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
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
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, height)).unwrap();
        terminal
            .draw(|frame| {
                render(app, frame);
            })
            .unwrap();
        terminal.backend().buffer().clone()
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
        app.update(Msg::Action(Action::PreviousEntry));
        assert_eq!(styles(&app)[(0, 7)].fg, Color::LightRed);
        let effects = app.update(Msg::Action(Action::DisplayMessage));
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Err(mett_core::Error::Network("down".into()))),
        }));
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
        let effects = app.update(Msg::Action(Action::DisplayMessage));
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let mut message = mock::message("m0", "in", "Hi", received(0));
        message.summary.from = Some(Address::new("alice@contoso.com"));
        message.body.content = "> a\n>> b\nhello world\ncafe\u{301} ok\n-- \nAlice".into();
        app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Ok(message)),
        }));
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
    fn draws_the_index_like_mutt() {
        let lines = screen(&app(2));
        assert_eq!(lines[0], "q:Quit  j:Next  k:Prev  <Enter>:Display");
        assert_eq!(
            lines[1],
            format!("   1 N   {} Alice Example    Subject 0", index_date(0))
        );
        assert_eq!(
            lines[2],
            format!("   2 N!  {} Alice Example    Subject 1", index_date(1))
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
            app.update(Msg::Action(Action::NextEntry));
        }
        let lines = screen(&app);
        assert!(lines[1].starts_with("   6 "), "{lines:#?}");
        assert!(lines[2].starts_with("   7 "), "{lines:#?}");
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
        let effects = app.update(Msg::Action(Action::DisplayMessage));
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        assert_eq!(screen(&app)[1], "Loading message...");

        let mut message = mock::message("m0", "in", "Subject 0", received(0));
        message.summary.from = Some(Address::new("alice@contoso.com").with_name("Alice Example"));
        app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Ok(message)),
        }));
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
        app.update(Msg::Action(Action::DeleteMessage));
        assert_eq!(screen(&app)[6], pager_status("D  "));
    }

    #[test]
    fn the_sign_in_window_takes_the_normal_colors() {
        use ratatui::style::Color;
        let mut app = app(2);
        colored(&mut app, &["color normal white blue"]);
        app.update(Msg::SignIn(crate::signin::SignInEvent::Prompt {
            account: mett_core::AccountId::new("alice@contoso.com"),
            prompt: SignInPrompt::EnterCode {
                verification_uri: "https://microsoft.com/devicelogin".into(),
                user_code: "ABCD-EFGH".into(),
                message: String::new(),
            },
        }));
        let buffer = styles(&app);
        let lines = screen(&app);
        let code = lines.iter().position(|l| l.contains("ABCD-EFGH")).unwrap();
        assert_eq!(buffer[(0, u16::try_from(code).unwrap())].bg, Color::Blue);
    }

    #[test]
    fn draws_the_sign_in_window_over_the_index() {
        let mut app = app(2);
        app.update(Msg::SignIn(crate::signin::SignInEvent::Prompt {
            account: mett_core::AccountId::new("alice@contoso.com"),
            prompt: SignInPrompt::EnterCode {
                verification_uri: "https://microsoft.com/devicelogin".into(),
                user_code: "ABCD-EFGH".into(),
                message: String::new(),
            },
        }));
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

        app.update(Msg::SignIn(crate::signin::SignInEvent::Finished {
            account: mett_core::AccountId::new("alice@contoso.com"),
            outcome: Err("denied".into()),
        }));
        let lines = sized_screen(&app, 60, 12);
        assert_eq!(lines[0], "q:Close  <Enter>:Retry");
        let window = lines.join("\n");
        assert!(window.contains("Sign-in failed: denied"), "{window}");
    }

    #[test]
    fn a_long_sign_in_link_can_be_scrolled_into_view() {
        let mut app = app(1);
        let url = format!("https://login.example/authorize?{}end", "x".repeat(400));
        app.update(Msg::SignIn(crate::signin::SignInEvent::Prompt {
            account: mett_core::AccountId::new("alice@contoso.com"),
            prompt: SignInPrompt::OpenBrowser {
                url: url.parse().unwrap(),
            },
        }));
        let lines = screen(&app);
        assert!(lines[1].contains("j/k to scroll"), "{lines:#?}");
        for _ in 0..20 {
            app.update(Msg::Action(Action::NextLine));
        }
        let shown = screen(&app).join("\n");
        assert!(shown.contains("end"), "{shown}");
        assert!(shown.contains("Waiting for you to sign in..."), "{shown}");
    }

    #[test]
    fn the_sign_in_link_is_handed_over_whole() {
        let mut app = app(1);
        let url = format!("https://login.example/authorize?{}end", "x".repeat(80));
        app.update(Msg::SignIn(crate::signin::SignInEvent::Prompt {
            account: mett_core::AccountId::new("alice@contoso.com"),
            prompt: SignInPrompt::OpenBrowser {
                url: url.parse().unwrap(),
            },
        }));
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
        app.update(Msg::Action(Action::Mail));
        for c in "bob@x.org".chars() {
            app.update(Msg::Key(ratatui::crossterm::event::KeyEvent::from(
                ratatui::crossterm::event::KeyCode::Char(c),
            )));
        }
        assert_eq!(screen(&app)[7], "To: bob@x.org");
        let enter = || {
            Msg::Key(ratatui::crossterm::event::KeyEvent::from(
                ratatui::crossterm::event::KeyCode::Enter,
            ))
        };
        app.update(enter());
        app.update(enter());
        assert_eq!(screen(&app)[7], "No subject, abort? ([yes]/no):");
        app.update(Msg::Key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Char('n'),
        )));
        app.update(Msg::Edited(Ok("x".repeat(2048))));
        let lines = sized_screen(&app, WIDTH, 12);
        assert_eq!(
            sized_screen(&app, 80, 12)[0],
            "y:Send  q:Abort  t:To  c:CC  b:Bcc  s:Subj  e:Edit  P:Postpone"
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
    fn sizes_read_like_mutt() {
        assert_eq!(format_size(100), "0.1K");
        assert_eq!(format_size(20 * 1024), "20K");
        assert_eq!(format_size(3 * 1024 * 1024 / 2), "1.5M");
    }

    #[test]
    fn a_forward_shows_the_attachments_it_carries() {
        let mut app = app(1);
        app.update(Msg::Action(Action::Forward));
        let effects = app.update(Msg::Action(Action::Forward));
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a fetch, got {effects:?}");
        };
        let mut message = mock::message("m0", "in", "Subject 0", received(0));
        message.summary.has_attachments = true;
        app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Ok(message)),
        }));
        let key = |code| Msg::Key(ratatui::crossterm::event::KeyEvent::from(code));
        for c in "bob@x.org".chars() {
            app.update(key(ratatui::crossterm::event::KeyCode::Char(c)));
        }
        app.update(key(ratatui::crossterm::event::KeyCode::Enter));
        app.update(key(ratatui::crossterm::event::KeyCode::Enter));
        app.update(Msg::Edited(Ok("see below".into())));
        let lines = sized_screen(&app, WIDTH, 13);
        assert_eq!(lines[9], "- A     2 <the forwarded message's attachments>");
        assert!(lines[11].contains("Atts: 2"), "{lines:#?}");
    }

    #[test]
    fn marks_deleted_messages_and_lists_folders() {
        let mut app = app(2);
        app.update(Msg::Action(Action::DeleteMessage));
        let lines = screen(&app);
        assert!(lines[1].starts_with("   1 D   "), "{lines:#?}");
        assert_eq!(
            lines[6],
            status_bar("-*-mett: Inbox [Msgs:2 New:2 Del:1 Flag:1]---", "(all)---")
        );

        let effects = app.update(Msg::Action(Action::BrowseMailboxes));
        let Effect::Request(list, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        assert_eq!(screen(&app)[1], "Listing folders...");
        let mut inbox = mock::folder("in", "Inbox", Some(WellKnownFolder::Inbox));
        inbox.unread_count = 2;
        inbox.total_count = 10;
        let projects = mett_core::Folder {
            parent: Some("in".into()),
            ..mock::folder("p", "Projects", None)
        };
        app.update(Msg::Response(Response {
            id: list,
            event: Event::FoldersListed(Ok(vec![inbox, projects])),
        }));
        let lines = screen(&app);
        assert_eq!(lines[0], "q:Exit  <Enter>:Select  j:Next  k:Prev");
        assert_eq!(lines[1], "   1 N     2/10     Inbox");
        assert_eq!(lines[2], "   2       0/0        Projects");
        assert_eq!(lines[6], "---mett: Folders [2]---");
    }

    #[test]
    fn an_empty_folder_list_says_so() {
        let mut app = app(1);
        let effects = app.update(Msg::Action(Action::BrowseMailboxes));
        let Effect::Request(list, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        app.update(Msg::Response(Response {
            id: list,
            event: Event::FoldersListed(Ok(Vec::new())),
        }));
        assert_eq!(screen(&app)[1], "No folders.");
    }
}
