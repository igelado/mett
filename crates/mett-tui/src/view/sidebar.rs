//! Drawing the sidebar (`app/sidebar.rs`): its divider, and the page of
//! entries with its cursor, each from `sidebar_format`, as mutt draws them.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, SidebarEntry};
use crate::format::{self, Expandos, truthy};

/// `area` split between the menus and the sidebar, if it is shown.
pub(super) fn split(app: &App, area: Rect) -> (Rect, Option<Rect>) {
    let settings = &app.sidebar().settings;
    let columns = settings.columns(usize::from(area.width));
    let Ok(columns @ 1..) = u16::try_from(columns) else {
        return (area, None);
    };
    let rest = area.width - columns;
    let (menu_x, side_x) = if settings.on_right {
        (area.x, area.x + rest)
    } else {
        (area.x + columns, area.x)
    };
    (
        Rect::new(menu_x, area.y, rest, area.height),
        Some(Rect::new(side_x, area.y, columns, area.height)),
    )
}

/// Draws the sidebar in `area`.
pub(super) fn render(app: &App, frame: &mut Frame, area: Rect) {
    let sidebar = app.sidebar();
    let settings = &sidebar.settings;
    let colors = &app.display.colors;
    let columns = usize::from(area.width);
    // A divider wider than the sidebar isn't drawn, as in mutt.
    let divider = settings.divider_char.as_str();
    let divider_width = Some(divider.width()).filter(|&w| w <= columns).unwrap_or(0);
    let buffer = frame.buffer_mut();
    if divider_width > 0 {
        let x = if settings.on_right {
            area.x
        } else {
            area.right() - u16::try_from(divider_width).unwrap_or(area.width)
        };
        let style = colors.normal.patch(colors.sidebar.divider);
        for y in area.top()..area.bottom() {
            buffer.set_stringn(x, y, divider, divider_width, style);
        }
    }
    let width = columns - divider_width;
    if width == 0 {
        return;
    }
    let x = if settings.on_right {
        area.x + u16::try_from(divider_width).unwrap_or(0)
    } else {
        area.x
    };
    // A page at a time, with the cursor on it, as in mutt.
    let height = usize::from(area.height).max(1);
    let top = sidebar.highlight / height * height;
    // The open folder's counts are the index's, counted once if shown.
    let mut live = None;
    for (row, (i, entry)) in sidebar.entries.iter().enumerate().skip(top).enumerate() {
        let Ok(row) = u16::try_from(row) else { break };
        if row >= area.height {
            break;
        }
        let open = entry
            .open
            .then(|| *live.get_or_insert_with(|| Live::of(app)));
        let values = Values { entry, open };
        let mut text = format::expand(&settings.format, &values, width);
        text.push_str(&" ".repeat(width.saturating_sub(text.width())));
        let style = entry_style(app, &values, i == sidebar.highlight);
        buffer.set_stringn(x, area.y + row, &text, width, style);
    }
}

/// How an entry is drawn: by what mail it has, as in mutt, then with the
/// open folder's `sidebar_indicator` (or `indicator`) or the cursor's
/// `sidebar_highlight` over that.
fn entry_style(app: &App, values: &Values, highlighted: bool) -> Style {
    let colors = &app.display.colors;
    let sidebar = &colors.sidebar;
    let entry = values.entry;
    let unset = Style::new();
    let base = if values.unread().is_some_and(|n| n > 0) || entry.new {
        sidebar.new
    } else if values.open.is_some_and(|live| live.flagged > 0) {
        sidebar.flagged
    } else if entry.spoolfile && sidebar.spoolfile != unset {
        sidebar.spoolfile
    } else {
        sidebar.ordinary
    };
    let style = colors.normal.patch(base);
    if entry.open {
        let indicator = if sidebar.indicator == unset {
            colors.indicator
        } else {
            sidebar.indicator
        };
        style.patch(indicator)
    } else if highlighted {
        style.patch(sidebar.highlight)
    } else {
        style
    }
}

/// The open folder's counts, as the index has them.
#[derive(Clone, Copy, Debug)]
struct Live {
    unread: usize,
    total: usize,
    flagged: usize,
    deleted: usize,
    /// Shown by the limit, if there is one.
    limited: Option<usize>,
}

impl Live {
    fn of(app: &App) -> Self {
        let (unread, flagged) = app.folder_messages().fold((0, 0), |(unread, flagged), m| {
            (
                unread + usize::from(!m.flags.read),
                flagged + usize::from(m.flags.flagged),
            )
        });
        Self {
            unread,
            total: app.total_messages(),
            flagged,
            deleted: app.deleted_count(),
            limited: app.limit.as_ref().map(|_| app.shown_count()),
        }
    }
}

/// An entry, for `sidebar_format`.
struct Values<'a> {
    entry: &'a SidebarEntry,
    /// The index's counts, for the open folder.
    open: Option<Live>,
}

impl Values<'_> {
    fn unread(&self) -> Option<usize> {
        match self.open {
            Some(live) => Some(live.unread),
            None => self.entry.counts.map(|(unread, _)| unread as usize),
        }
    }

    fn total(&self) -> Option<usize> {
        match self.open {
            Some(live) => Some(live.total),
            None => self.entry.counts.map(|(_, total)| total as usize),
        }
    }
}

impl Expandos for Values<'_> {
    /// mutt's: counts not known (Office365 counts no flagged mail but the
    /// open folder's, and mett has no tags) are empty.
    fn expando(&self, code: char, _arg: Option<&str>) -> String {
        let number = |n: Option<usize>| n.map_or_else(String::new, |n| n.to_string());
        let flagged = self.open.map(|live| live.flagged);
        match code {
            'B' => self.entry.name.clone(),
            'd' => number(Some(self.open.map_or(0, |live| live.deleted))),
            'F' => number(flagged),
            'L' => number(self.open.and_then(|live| live.limited).or(self.total())),
            'N' => number(self.unread()),
            'n' => if self.entry.new { "N" } else { " " }.into(),
            'S' => number(self.total()),
            't' => "0".into(),
            '!' => match flagged.unwrap_or(0) {
                0 => String::new(),
                1 => "!".into(),
                2 => "!!".into(),
                n => format!("{n}!"),
            },
            _ => String::new(),
        }
    }

    fn is_set(&self, code: char) -> bool {
        match code {
            // Only when the limit hides some of the open folder.
            'L' => self
                .open
                .is_some_and(|live| live.limited.is_some_and(|shown| shown != live.total)),
            _ => truthy(&self.expando(code, None)),
        }
    }
}

#[cfg(test)]
mod tests {
    use mett_core::mock;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::{Color, Modifier};

    use super::*;
    use crate::app::testing::*;
    use crate::app::{Msg, SidebarSettings};
    use crate::worker::Event;

    const WIDTH: u16 = 100;
    const HEIGHT: u16 = 8;

    fn visible() -> SidebarSettings {
        SidebarSettings {
            visible: true,
            ..SidebarSettings::default()
        }
    }

    /// An app with [`with_mailboxes`] and `sidebar`, its mailboxes checked
    /// (Work has unread mail), and the Inbox listing `messages`.
    fn app(sidebar: SidebarSettings, messages: usize) -> crate::app::App {
        let settings = with_mailboxes(sidebar);
        let (mut app, effects) = App::open(WIDTH, HEIGHT, inbox(), settings);
        let open = first_request(&effects);
        let folder = mock::folder("inbox", "Inbox", None);
        let sent = requests(&respond(&mut app, open, Event::FolderOpened(Ok(folder))));
        let summaries = (0..messages)
            .map(|i| {
                let date = chrono::DateTime::UNIX_EPOCH + chrono::TimeDelta::days(i as i64);
                let mut m = mock::message(&format!("m{i}"), "in", &format!("Subject {i}"), date);
                m.summary.flags.flagged = i == 0;
                m.summary
            })
            .collect();
        listed(&mut app, sent[0].0, summaries, None);
        let check = sent[1].0;
        mailboxes_checked(
            &mut app,
            check,
            &[
                ("Inbox", 0, 2, false),
                ("Work", 3, 7, true),
                ("Lists", 0, 9, false),
            ],
        );
        app
    }

    fn drawn(app: &App, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                super::super::render(app, frame);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn lines(buffer: &Buffer) -> Vec<String> {
        let area = buffer.area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn screen(app: &App) -> Vec<String> {
        lines(&drawn(app, WIDTH, HEIGHT))
    }

    #[test]
    fn draws_the_mailboxes_beside_the_index() {
        let app = app(visible(), 2);
        let lines = screen(&app);
        // The help line and the status bar span the screen.
        assert!(lines[0].starts_with("q:Quit"), "{lines:#?}");
        assert!(lines[6].starts_with("---mett: Inbox"), "{lines:#?}");
        assert_eq!(lines[6].chars().count(), usize::from(WIDTH), "{lines:#?}");
        // `%B%*  %n`, 29 columns, then the divider.
        let side = |row: usize| lines[row].chars().take(30).collect::<String>();
        assert_eq!(side(1), format!("Inbox{}|", " ".repeat(24)));
        assert_eq!(side(2), format!("Work{}N|", " ".repeat(24)));
        assert_eq!(side(3), format!("Lists{}|", " ".repeat(24)));
        assert_eq!(side(4), format!("{}|", " ".repeat(29)));
        // The index beside it, narrower.
        let index = |row: usize| lines[row].chars().skip(30).collect::<String>();
        assert!(index(2).starts_with("   2 N!"), "{lines:#?}");
        assert!(lines[1].chars().count() <= usize::from(WIDTH));
    }

    #[test]
    fn stays_hidden_until_asked_for() {
        let app = app(SidebarSettings::default(), 2);
        let lines = screen(&app);
        assert!(lines[2].starts_with("   2 N!"), "{lines:#?}");
        assert!(!lines[2].contains('|'), "{lines:#?}");
        assert_eq!(app.width, usize::from(WIDTH));
    }

    #[test]
    fn hides_on_a_narrow_terminal_and_comes_back() {
        let mut app = app(visible(), 2);
        app.update(Msg::Resize {
            width: 80,
            height: HEIGHT,
        });
        let lines = lines(&drawn(&app, 80, HEIGHT));
        assert!(lines[2].starts_with("   2 N!"), "{lines:#?}");
        app.update(Msg::Resize {
            width: WIDTH,
            height: HEIGHT,
        });
        assert!(screen(&app)[2].starts_with("Work"));
    }

    #[test]
    fn may_be_on_the_right() {
        let app = app(
            SidebarSettings {
                on_right: true,
                divider_char: "│".into(),
                ..visible()
            },
            2,
        );
        let lines = screen(&app);
        assert!(lines[2].starts_with("   2 N!"), "{lines:#?}");
        let side = |row: usize| lines[row].chars().skip(70).collect::<String>();
        assert_eq!(side(1), "│Inbox");
        assert_eq!(side(2), format!("│Work{}N", " ".repeat(24)));
        assert_eq!(app.width, 70);
    }

    #[test]
    fn colors_the_entries_like_mutt() {
        let mut app = app(visible(), 2);
        for command in [
            "sidebar_new yellow default",
            "sidebar_divider blue default",
            "sidebar_spoolfile green default",
        ] {
            let words: Vec<String> = command.split(' ').map(str::to_owned).collect();
            app.display.colors.color(&words).unwrap();
        }
        let buffer = drawn(&app, WIDTH, HEIGHT);
        // The open folder in the indicator's colors, over its own.
        assert!(buffer[(0, 1)].modifier.contains(Modifier::REVERSED));
        assert!(buffer[(28, 1)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buffer[(0, 2)].fg, Color::Yellow, "unread mail");
        assert_eq!(buffer[(0, 3)].fg, Color::Reset);
        assert_eq!(buffer[(29, 3)].fg, Color::Blue, "the divider");
        assert!(!buffer[(29, 1)].modifier.contains(Modifier::REVERSED));
        // `sidebar_indicator`, once set, instead.
        app.display
            .colors
            .color(&["sidebar_indicator".into(), "red".into(), "default".into()])
            .unwrap();
        let buffer = drawn(&app, WIDTH, HEIGHT);
        assert_eq!(buffer[(0, 1)].fg, Color::Red);
        assert!(!buffer[(0, 1)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn formats_the_entries_with_mutt_s_expandos() {
        let app = app(
            SidebarSettings {
                format: "%B:%N/%S:%F:%!:%d:%L:%t:%?N?unread&none?:%?L?lim&all?:%Q.".into(),
                width: 60,
                ..visible()
            },
            3,
        );
        let lines = screen(&app);
        let side = |row: usize| lines[row].chars().take(59).collect::<String>();
        // The open folder's counts are the index's.
        assert_eq!(side(1).trim_end(), "Inbox:3/3:1:!:0:3:0:unread:all:.");
        // Others' are checked; Office365 counts no flagged mail.
        assert_eq!(side(2).trim_end(), "Work:3/7:::0:7:0:unread:all:.");
        assert_eq!(side(3).trim_end(), "Lists:0/9:::0:9:0:none:all:.");
    }

    #[test]
    fn the_pager_and_the_status_bar_fit_around_it() {
        let mut app = app(visible(), 1);
        let fetch = first_request(&act(&mut app, crate::action::Action::DisplayMessage));
        let mut message = mock::message("m0", "in", "Subject 0", chrono::DateTime::UNIX_EPOCH);
        message.body.content = "word ".repeat(30);
        respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
        let crate::app::Screen::Pager(pager) = app.screen() else {
            panic!("expected the pager");
        };
        assert_eq!(pager.rows_width, Some(70), "wrapped beside the sidebar");
        let lines = screen(&app);
        assert!(lines[6].contains("-- ("), "{lines:#?}");
        assert_eq!(lines[6].chars().count(), usize::from(WIDTH));
        // The sidebar's keys work in the pager too: the cursor underlined.
        act(&mut app, crate::action::Action::SidebarNext);
        let buffer = drawn(&app, WIDTH, HEIGHT);
        assert!(buffer[(0, 2)].modifier.contains(Modifier::UNDERLINED));
        assert!(!buffer[(0, 3)].modifier.contains(Modifier::UNDERLINED));
        assert!(matches!(app.screen(), crate::app::Screen::Pager(_)));
        // Shown again at its full width once hidden.
        let mut hidden = with_mailboxes(SidebarSettings::default());
        hidden.sidebar.visible = false;
        app.update(configured(hidden));
        let crate::app::Screen::Pager(pager) = app.screen() else {
            panic!("expected the pager");
        };
        assert_eq!(pager.rows_width, Some(usize::from(WIDTH)));
        app.update(Msg::Resize {
            width: 120,
            height: HEIGHT,
        });
        assert_eq!(app.width, 120);
    }

    /// Opens the selected message, `words` words long.
    fn read(app: &mut App, words: usize) {
        let fetch = first_request(&act(app, crate::action::Action::DisplayMessage));
        let id = app.messages[app.selected].id.as_str().to_owned();
        let mut message = mock::message(&id, "in", "Subject", chrono::DateTime::UNIX_EPOCH);
        message.body.content = "word ".repeat(words);
        respond(app, fetch, Event::MessageFetched(Ok(message)));
    }

    fn rows_width(app: &App) -> Option<usize> {
        match app.screen() {
            crate::app::Screen::Pager(pager) => pager.rows_width,
            _ => None,
        }
    }

    #[test]
    fn wrap_counts_from_the_width_beside_it() {
        let mut app = app(visible(), 1);
        read(&mut app, 40);
        for (wrap, width) in [(0, 70), (50, 50), (90, 70), (-10, 60)] {
            app.display.wrap = wrap;
            app.update(Msg::Resize {
                width: WIDTH,
                height: HEIGHT,
            });
            assert_eq!(rows_width(&app), Some(width), "wrap={wrap}");
        }
        let lines = screen(&app);
        assert!(
            lines[5].starts_with("                             |word"),
            "{lines:#?}"
        );
        assert!(lines[5].chars().count() <= 90, "{lines:#?}");
    }

    #[test]
    fn the_mini_index_is_beside_it_too() {
        let mut app = app(visible(), 3);
        app.display.pager_index_lines = 4;
        let height = 16;
        app.update(Msg::Resize {
            width: WIDTH,
            height,
        });
        read(&mut app, 5);
        let lines = lines(&drawn(&app, WIDTH, height));
        let side = |row: usize| lines[row].chars().take(30).collect::<String>();
        let menu = |row: usize| lines[row].chars().skip(30).collect::<String>();
        assert_eq!(side(1), format!("Inbox{}|", " ".repeat(24)), "{lines:#?}");
        assert!(menu(1).starts_with("   1 "), "{lines:#?}");
        // The mini-index's status bar is in the pager's window, beside the
        // sidebar, as in mutt; the pager's own spans the screen.
        assert!(side(4).ends_with('|'), "{lines:#?}");
        assert!(menu(4).contains("mett: Inbox"), "{lines:#?}");
        assert_eq!(menu(4).chars().count(), 70, "{lines:#?}");
        assert!(menu(5).starts_with("Date:"), "{lines:#?}");
        let status = &lines[usize::from(height) - 2];
        assert!(status.starts_with("-"), "{lines:#?}");
        assert_eq!(status.chars().count(), usize::from(WIDTH), "{lines:#?}");
    }
}
