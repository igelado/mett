//! Drawing the [`App`]: the "view" of the Elm Architecture. It reads the
//! model and never changes it.
//!
//! The layout follows mutt: a help line at the top, the index or pager in
//! the middle, a status bar, and a message line at the bottom.

use chrono::Local;
use mett_core::{Address, MessageSummary};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::action::Action;
use crate::app::{App, Pager, Screen};
use crate::keymap::{Keymap, Menu};
use crate::text::{fit, truncate, wrap_all};

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
];

const REVERSED: Style = Style::new().add_modifier(Modifier::REVERSED);

/// Draws the whole screen.
pub fn render(app: &App, keymap: &Keymap, frame: &mut Frame) {
    let [help, body, status, message] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    frame.render_widget(
        Paragraph::new(help_line(keymap, app.menu())).style(REVERSED),
        help,
    );
    let status_line = match app.screen() {
        Screen::Index => {
            render_index(app, frame, body);
            index_status(app)
        }
        Screen::Pager(pager) => render_pager(app, pager, frame, body),
    };
    frame.render_widget(Paragraph::new(status_line).style(REVERSED), status);
    frame.render_widget(Paragraph::new(app.status.as_str()), message);
}

/// `q:Quit  j:Next  …`, from the current key bindings.
fn help_line(keymap: &Keymap, menu: Menu) -> String {
    let entries = HELP
        .iter()
        .find(|(m, _)| *m == menu)
        .map_or(&[][..], |(_, entries)| entries);
    entries
        .iter()
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
    let lines: Vec<Line> = app
        .messages
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(i, message)| {
            let line = Line::raw(index_entry(i + 1, message, width));
            if i == app.selected {
                line.style(REVERSED)
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// One index line, like mutt's default `%4C %Z %{%b %d} %-15.15L %s`.
fn index_entry(number: usize, message: &MessageSummary, width: usize) -> String {
    let flags = format!(
        "{}{}{}",
        if message.flags.read { ' ' } else { 'N' },
        if message.flags.flagged { '!' } else { ' ' },
        if message.has_attachments { '+' } else { ' ' },
    );
    let date = message.received.with_timezone(&Local).format("%b %d");
    let from = message.from.as_ref().map_or("", display_name);
    // Columns are measured in terminal cells, so wide characters line up.
    let line = format!(
        "{number:>4} {flags} {date} {}  {}",
        fit(from, 15),
        message.subject
    );
    truncate(&line, width)
}

fn display_name(address: &Address) -> &str {
    address
        .name
        .as_deref()
        .filter(|n| !n.is_empty())
        .unwrap_or(&address.email)
}

fn index_status(app: &App) -> String {
    let folder = app.folder.as_ref().map_or("", |f| f.name.as_str());
    let new = app.messages.iter().filter(|m| !m.flags.read).count();
    format!(
        "-*-mett: {folder} [Msgs:{} New:{new}]---",
        app.messages.len()
    )
}

/// Draws the pager body and returns its status line.
fn render_pager(app: &App, pager: &Pager, frame: &mut Frame, area: Rect) -> String {
    let Some(message) = &pager.message else {
        frame.render_widget(Paragraph::new("Loading message..."), area);
        return "---mett: loading---".into();
    };
    // Wrapped here, at the width being drawn, so no text is clipped.
    let lines = wrap_all(&pager.lines, usize::from(area.width));
    let height = usize::from(area.height);
    let last_top = lines.len().saturating_sub(height);
    let top = pager.scroll.min(last_top);
    let visible: Vec<Line> = lines
        .iter()
        .skip(top)
        .take(height)
        .map(|l| Line::raw(l.as_str()))
        .collect();
    frame.render_widget(Paragraph::new(visible), area);

    let position = if lines.len() <= height {
        "all".to_owned()
    } else if top >= last_top {
        "end".to_owned()
    } else {
        format!("{}%", (top + height) * 100 / lines.len())
    };
    let from = message.summary.from.as_ref().map_or("", display_name);
    format!(
        "-N- {}/{}: {from}  {}   -- ({position})",
        app.selected + 1,
        app.messages.len(),
        message.summary.subject
    )
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeDelta, Utc};
    use mett_core::mock;
    use mett_core::{MessagePage, WellKnownFolder};
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
        let Effect::Request(open, _) = effects[0].clone();
        let effects = app.update(Msg::Response(Response {
            id: open,
            event: Event::InboxOpened(Ok(mock::folder(
                "in",
                "Inbox",
                Some(WellKnownFolder::Inbox),
            ))),
        }));
        let Effect::Request(list, _) = effects[0].clone();
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
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        terminal
            .draw(|frame| render(app, &Keymap::mutt_defaults(), frame))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..HEIGHT)
            .map(|y| {
                let mut line = String::new();
                let mut x = 0;
                while x < WIDTH {
                    let symbol = buffer[(x, y)].symbol();
                    line.push_str(symbol);
                    // A wide character also covers the next cell.
                    x += u16::try_from(symbol.width().max(1)).unwrap();
                }
                line.trim_end().to_owned()
            })
            .collect()
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
        assert_eq!(lines[6], "-*-mett: Inbox [Msgs:2 New:2]---");
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
        let Effect::Request(fetch, _) = effects[0].clone();
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
        assert_eq!(lines[6], "-N- 1/1: Alice Example  Subject 0   -- (all)");
    }
}
