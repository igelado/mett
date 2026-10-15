//! Drawing the [`App`]: the "view" of the Elm Architecture. It reads the
//! model and never changes it.
//!
//! The layout follows mutt: a help line at the top, the index or pager in
//! the middle, a status bar, and a message line at the bottom.

use chrono::Local;
use mett_core::{Address, MessageSummary};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::action::Action;
use crate::app::{App, Pager, Row, Screen, SignIn};
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
    (
        Menu::SignIn,
        &[
            (Action::Exit, "Close"),
            (Action::OpenBrowser, "Browser"),
            (Action::Retry, "Retry"),
        ],
    ),
];

const REVERSED: Style = Style::new().add_modifier(Modifier::REVERSED);

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
pub fn render(app: &App, keymap: &Keymap, frame: &mut Frame) -> Option<Overlay> {
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
    // The window may cover everything but the help line, which lists its
    // keys: on a small terminal, the sign-in instructions matter more than
    // the index behind them.
    if let Some(window) = &app.sign_in {
        let [_, below_help] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(frame.area());
        return render_sign_in(window, keymap, frame, below_help);
    }
    None
}

/// Draws the sign-in window across the middle of `area`. It spans the
/// whole width, without side borders, so that the sign-in link can be
/// copied from the terminal without stray characters.
fn render_sign_in(
    window: &SignIn,
    keymap: &Keymap,
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
        Paragraph::new(visible).block(
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
    let incomplete = if app.incomplete() {
        " [incomplete]"
    } else {
        ""
    };
    format!(
        "-*-mett: {folder} [Msgs:{} New:{new}]{incomplete}---",
        app.messages.len()
    )
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
    let rewrapped;
    let lines = if pager.rows_width == Some(width) {
        &pager.rows
    } else {
        rewrapped = wrap_all(&pager.lines, width);
        &rewrapped
    };
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
    // The flags of the message as the index currently shows them, as in
    // mutt's `%Z`: N for unread, ! for flagged.
    let flags = app
        .messages
        .get(app.selected)
        .map_or(Default::default(), |m| m.flags);
    format!(
        "-{}{}- {}/{}: {from}  {}   -- ({position})",
        if flags.read { '-' } else { 'N' },
        if flags.flagged { '!' } else { '-' },
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
                render(app, &Keymap::mutt_defaults(), frame);
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
        assert_eq!(lines[6], "---- 1/1: Alice Example  Subject 0   -- (all)");
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
        assert_eq!(lines[0], "q:Close  o:Browser  <Enter>:Retry");
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
        let window = sized_screen(&app, 60, 12).join("\n");
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
            .draw(|frame| overlay = render(&app, &Keymap::mutt_defaults(), frame))
            .unwrap();
        let link = overlay.take().expect("the link fits");
        assert_eq!(link.text, url);
        assert_eq!(link.x, 0);
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, link.y)].symbol(), "h", "where the link starts");

        // A link that doesn't fit isn't handed over.
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, 5)).unwrap();
        terminal
            .draw(|frame| overlay = render(&app, &Keymap::mutt_defaults(), frame))
            .unwrap();
        assert!(overlay.is_none());
    }
}
