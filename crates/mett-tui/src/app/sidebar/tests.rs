use ratatui::crossterm::event::KeyCode;

use super::*;
use crate::app::testing::*;
use crate::app::{Msg, Timer};
use crate::keymap::Menu;
use crate::worker::{Event, Request};

/// The names of the entries, `*` marking the cursor's.
fn shown(app: &App) -> Vec<String> {
    let sidebar = &app.sidebar;
    (sidebar.entries.iter().enumerate())
        .map(|(i, e)| {
            let mark = if i == sidebar.highlight { "*" } else { "" };
            format!("{mark}{}", e.name)
        })
        .collect()
}

fn sorted_by(method: &str) -> SidebarSettings {
    SidebarSettings {
        sort: SidebarSort::parse(method).unwrap(),
        ..SidebarSettings::default()
    }
}

#[test]
fn lists_the_mailboxes_with_their_counts() {
    let (mut app, check) = mailboxes_app(with_mailboxes(SidebarSettings::default()), 80, 24);
    assert_eq!(shown(&app), ["*Inbox", "Work", "Lists"]);
    mailboxes_checked(
        &mut app,
        check,
        &[
            ("Inbox", 1, 5, true),
            ("Work", 2, 7, true),
            ("Lists", 0, 9, false),
        ],
    );
    let entries = &app.sidebar.entries;
    assert!(entries[0].open && !entries[0].new, "not new while open");
    assert_eq!(entries[1].counts, Some((2, 7)));
    assert!(entries[1].new && entries[1].has_mail());
    assert!(!entries[2].has_mail());
}

#[test]
fn sorts_the_mailboxes() {
    let counts = [
        ("Inbox", 1, 5, false),
        ("Work", 2, 7, false),
        ("Lists", 0, 9, false),
    ];
    let order = |method: &str| {
        let (mut app, check) = mailboxes_app(with_mailboxes(sorted_by(method)), 80, 24);
        mailboxes_checked(&mut app, check, &counts);
        shown(&app)
    };
    assert_eq!(order("count"), ["Lists", "Work", "*Inbox"]);
    assert_eq!(order("reverse-count"), ["*Inbox", "Work", "Lists"]);
    assert_eq!(order("unread"), ["Work", "*Inbox", "Lists"]);
    // By label, or path: `Inbox` before `Lists`.
    assert_eq!(order("alpha"), ["*Inbox", "Lists", "Work"]);
    // As shown: `Apple` before a label (`Banana`) after it.
    let mut settings = with_mailboxes(sorted_by("alpha"));
    settings.mailboxes[0].label = Some("Banana".into());
    settings.mailboxes[2].text = "=Apple".into();
    let (app, _) = mailboxes_app(settings, 80, 24);
    assert_eq!(shown(&app), ["Apple", "*Banana", "Work"]);
    // By path: `=@inbox` comes first.
    assert_eq!(order("path"), ["*Inbox", "Lists", "Work"]);
    assert_eq!(order("reverse-path"), ["Work", "Lists", "*Inbox"]);
    // Office365 doesn't count flagged mail: as listed.
    assert_eq!(order("flagged"), ["*Inbox", "Work", "Lists"]);
}

#[test]
fn new_mail_only_shows_mailboxes_with_mail() {
    let mut settings = SidebarSettings {
        new_mail_only: true,
        ..SidebarSettings::default()
    };
    let (mut app, check) = mailboxes_app(with_mailboxes(settings.clone()), 80, 24);
    // Not known yet: only the open folder.
    assert_eq!(shown(&app), ["*Inbox"]);
    let counts = [
        ("Inbox", 0, 5, false),
        ("Work", 0, 7, true),
        ("Lists", 0, 9, false),
    ];
    mailboxes_checked(&mut app, check, &counts);
    assert_eq!(shown(&app), ["*Inbox", "Work"]);
    settings.whitelist = vec![mailbox_path("Lists")];
    let whitelisted = with_mailboxes(settings);
    app.update(configured(whitelisted));
    assert_eq!(shown(&app), ["*Inbox", "Work", "Lists"]);
}

#[test]
fn knows_the_open_folder_by_another_name() {
    let mut settings = with_mailboxes(SidebarSettings {
        new_mail_only: true,
        sort: SidebarSort::parse("reverse-unsorted").unwrap(),
        ..SidebarSettings::default()
    });
    // `=Inbox`, unchecked, is the Inbox opened as `=@inbox`.
    settings.mailboxes[0].path = FolderPath::Named(vec!["Inbox".into()]);
    settings.mailboxes[0].poll = false;
    // Named as Office365 names folders, ignoring case.
    settings.sidebar.whitelist = vec![mailbox_path("WORK"), mailbox_path("lists")];
    let (app, _) = mailboxes_app(settings, 80, 24);
    assert_eq!(shown(&app), ["Lists", "Work", "*Inbox"], "reversed");
    let inbox = &app.sidebar.entries[2];
    assert!(inbox.open);
    assert_eq!(inbox.counts, Some((0, 0)), "the open folder's");
}

#[test]
fn the_cursor_stays_on_its_mailbox() {
    let (mut app, check) = mailboxes_app(with_mailboxes(sorted_by("unread")), 80, 24);
    mailboxes_checked(&mut app, check, &[("Work", 3, 3, false)]);
    assert_eq!(shown(&app), ["Work", "*Inbox", "Lists"]);
    app.sidebar.highlight = 2;
    // Lists gains more unread mail than Work: the cursor goes with it.
    let check = first_request(&app.check_mailboxes());
    mailboxes_checked(&mut app, check, &[("Lists", 5, 5, false)]);
    assert_eq!(shown(&app), ["*Lists", "Work", "Inbox"]);
    // Another order puts it back on the open folder.
    app.update(configured(with_mailboxes(sorted_by("alpha"))));
    assert_eq!(shown(&app), ["*Inbox", "Lists", "Work"]);
}

fn mailbox(text: &str, label: Option<&str>) -> MailboxEntry {
    MailboxEntry {
        name: label.unwrap_or(text).into(),
        label: label.map(str::to_owned),
        text: text.into(),
        path: FolderPath::Named(vec![text.into()]),
        notify: true,
        poll: true,
    }
}

fn name(text: &str, label: Option<&str>, settings: &SidebarSettings) -> String {
    sidebar_name(&mailbox(text, label), settings)
}

#[test]
fn sorts_by_mutt_s_names() {
    let sort = |text| SidebarSort::parse(text).unwrap();
    assert_eq!(sort("unsorted"), SidebarSort::default());
    assert_eq!(sort("new").method, SortMethod::Unread);
    assert_eq!(sort("name").method, SortMethod::Alpha);
    assert!(sort("reverse-count").reverse);
    assert!(SidebarSort::parse("size").is_err());
}

#[test]
fn names_mailboxes_like_mutt() {
    let mut settings = SidebarSettings::default();
    assert_eq!(name("=fruit.apple", None, &settings), "fruit.apple");
    assert_eq!(
        name("msgraph://a@b.c/Work", None, &settings),
        "msgraph://a@b.c/Work"
    );
    assert_eq!(name("=Work", Some("Job"), &settings), "Job");
    settings.short_path = true;
    assert_eq!(name("=fruit", None, &settings), "fruit");
    assert_eq!(name("=fruit.apple", None, &settings), "apple");
    assert_eq!(name("=a/b/c", None, &settings), "c");
    assert_eq!(name("=a/b/", None, &settings), "b/");
    settings.folder_indent = true;
    settings.indent_string = "..".into();
    assert_eq!(name("=fruit.apple", None, &settings), "..apple");
    assert_eq!(name("=a/b/c", Some("Mine"), &settings), "....Mine");
    // Only below `folder`.
    assert_eq!(name("msgraph://a@b.c/Work", None, &settings), "Work");
    settings.short_path = false;
    assert_eq!(name("=a/b", None, &settings), "..a/b");
    settings.delim_chars = "/".into();
    assert_eq!(name("=fruit.apple", None, &settings), "fruit.apple");
    settings.delim_chars.clear();
    assert_eq!(name("=a/b", None, &settings), "a/b");
}

/// An app whose sidebar is shown, on a terminal `height` rows tall,
/// with Work's and Lists' counts.
fn shown_app(sidebar: SidebarSettings, height: u16, counts: &[(&str, u32, u32, bool)]) -> App {
    let sidebar = SidebarSettings {
        visible: true,
        ..sidebar
    };
    let (mut app, check) = mailboxes_app(with_mailboxes(sidebar), 120, height);
    mailboxes_checked(&mut app, check, counts);
    app
}

#[test]
fn moves_the_cursor() {
    let mut app = shown_app(SidebarSettings::default(), 24, &[]);
    act(&mut app, Action::SidebarNext);
    assert_eq!(shown(&app), ["Inbox", "*Work", "Lists"]);
    act(&mut app, Action::SidebarNext);
    act(&mut app, Action::SidebarNext);
    assert_eq!(shown(&app), ["Inbox", "Work", "*Lists"], "stops at the end");
    act(&mut app, Action::SidebarPrev);
    assert_eq!(shown(&app), ["Inbox", "*Work", "Lists"]);
    act(&mut app, Action::SidebarFirst);
    assert_eq!(app.sidebar.highlight, 0);
    act(&mut app, Action::SidebarLast);
    assert_eq!(app.sidebar.highlight, 2);
    // The open folder stays the open one.
    assert!(app.sidebar.entries[0].open);
    assert!(app.status.is_empty(), "{}", app.status);
    // Text is read in the pager, which has them too.
    let text = crate::app::TextView::new("notes.txt".into(), vec!["a".into()], 50);
    app.screens.push(crate::app::Screen::Text(text));
    act(&mut app, Action::SidebarFirst);
    assert_eq!(app.sidebar.highlight, 0);
    assert!(matches!(app.screen(), crate::app::Screen::Text(_)));
}

#[test]
fn does_nothing_while_hidden() {
    let (mut app, _) = mailboxes_app(with_mailboxes(SidebarSettings::default()), 80, 24);
    assert!(act(&mut app, Action::SidebarNext).is_empty());
    assert!(act(&mut app, Action::SidebarOpen).is_empty());
    assert_eq!(shown(&app), ["*Inbox", "Work", "Lists"]);
}

#[test]
fn finds_mailboxes_with_new_mail() {
    let counts = [("Work", 0, 3, false), ("Lists", 2, 9, false)];
    let mut app = shown_app(SidebarSettings::default(), 24, &counts);
    act(&mut app, Action::SidebarNextNew);
    assert_eq!(shown(&app), ["Inbox", "Work", "*Lists"]);
    act(&mut app, Action::SidebarNextNew);
    assert_eq!(app.sidebar.highlight, 2, "no wrapping");
    act(&mut app, Action::SidebarPrevNew);
    assert_eq!(app.sidebar.highlight, 2, "none before");
    let wrapping = SidebarSettings {
        next_new_wrap: true,
        ..SidebarSettings::default()
    };
    let mut app = shown_app(wrapping, 24, &[("Work", 0, 3, true)]);
    act(&mut app, Action::SidebarLast);
    act(&mut app, Action::SidebarNextNew);
    assert_eq!(shown(&app), ["Inbox", "*Work", "Lists"], "wrapped");
    act(&mut app, Action::SidebarPrevNew);
    assert_eq!(app.sidebar.highlight, 1, "the only one");
}

#[test]
fn pages_through_the_mailboxes() {
    // Two rows: pages of Inbox and Work, then Lists.
    let mut app = shown_app(SidebarSettings::default(), 5, &[]);
    act(&mut app, Action::SidebarPageDown);
    assert_eq!(app.sidebar.highlight, 2);
    act(&mut app, Action::SidebarPageDown);
    assert_eq!(app.sidebar.highlight, 2, "the last");
    act(&mut app, Action::SidebarPageUp);
    assert_eq!(app.sidebar.highlight, 1, "the previous page's last");
    act(&mut app, Action::SidebarPageUp);
    assert_eq!(app.sidebar.highlight, 0);
}

#[test]
fn opens_the_highlighted_mailbox() {
    let mut app = shown_app(SidebarSettings::default(), 24, &[]);
    act(&mut app, Action::SidebarNext);
    let effects = act(&mut app, Action::SidebarOpen);
    assert!(
        matches!(&requests(&effects)[..], [(_, Request::OpenFolder(path))] if *path == mailbox_path("Work")),
        "{effects:?}"
    );
    let open = first_request(&effects);
    let work = mett_core::mock::folder("work", "Work", None);
    respond(&mut app, open, Event::FolderOpened(Ok(work)));
    assert_eq!(shown(&app), ["Inbox", "*Work", "Lists"]);
    assert!(app.sidebar.entries[1].open);
}

#[test]
fn toggles_the_sidebar_and_sidebar_visible() {
    let mut app = shown_app(SidebarSettings::default(), 24, &[]);
    assert_eq!(app.width, 90);
    let effects = act(&mut app, Action::SidebarToggleVisible);
    assert_eq!(effects, [Effect::Command("toggle sidebar_visible".into())]);
    assert_eq!(app.width, 120, "hidden at once");
    assert!(app.waiting(), "keys wait for the command");
    // The configuration agrees.
    app.update(configured(with_mailboxes(SidebarSettings::default())));
    assert_eq!(app.width, 120);
    assert!(!app.waiting());
    act(&mut app, Action::SidebarToggleVisible);
    assert_eq!(app.width, 90);
}

#[test]
fn hides_on_a_narrow_terminal() {
    let mut app = shown_app(SidebarSettings::default(), 24, &[]);
    let resize = |app: &mut App, width| {
        app.update(crate::app::Msg::Resize { width, height: 24 });
    };
    resize(&mut app, 99);
    assert_eq!(app.width, 99, "narrower than sidebar_min_width");
    act(&mut app, Action::SidebarNext);
    assert_eq!(app.sidebar.highlight, 0);
    assert_eq!(
        app.status,
        "The sidebar shows once the terminal is 100 columns wide."
    );
    resize(&mut app, 100);
    assert_eq!(app.width, 70, "back");
    // Nor does it leave the menus fewer than 40 columns.
    let wide = with_mailboxes(SidebarSettings {
        visible: true,
        width: 70,
        ..SidebarSettings::default()
    });
    app.update(configured(wide));
    assert_eq!(app.width, 100);
    resize(&mut app, 110);
    assert_eq!(app.width, 40);
    // Without sidebar_min_width, as in mutt: a column at least.
    let mutt = with_mailboxes(SidebarSettings {
        visible: true,
        width: 70,
        min_width: 0,
        ..SidebarSettings::default()
    });
    app.update(configured(mutt));
    resize(&mut app, 50);
    assert_eq!(app.width, 1);
}

#[test]
fn ordered_by_counts_follows_the_open_folder_s() {
    let mut settings = with_mailboxes(sorted_by("unread"));
    settings.sidebar.visible = true;
    let unread = (0..5)
        .map(|i| {
            let date = chrono::DateTime::UNIX_EPOCH + chrono::TimeDelta::minutes(i);
            mett_core::mock::message(&format!("m{i}"), "in", "s", date).summary
        })
        .collect();
    let (mut app, check) = mailboxes_listed(settings, 120, 24, unread);
    mailboxes_checked(&mut app, check, &[("Work", 3, 3, false)]);
    app.before_draw();
    assert_eq!(shown(&app), ["*Inbox", "Work", "Lists"]);
    for _ in 0..3 {
        act(&mut app, Action::ToggleNew);
        act(&mut app, Action::NextEntry);
    }
    app.before_draw();
    assert_eq!(shown(&app), ["Work", "*Inbox", "Lists"]);
}

/// An app without `mailboxes` and with `sidebar`, `width` columns
/// wide, that has opened the Inbox; returns it and the effects of the
/// opening.
fn folders_app_at(sidebar: SidebarSettings, width: u16) -> (App, Vec<Effect>) {
    let settings = crate::app::Settings {
        sync_interval: Some(std::time::Duration::from_secs(60)),
        sidebar,
        ..crate::app::Settings::default()
    };
    let (mut app, effects) = App::open(width, 24, inbox(), settings);
    let open = first_request(&effects);
    let inbox = mett_core::mock::folder("in", "Inbox", None);
    let effects = respond(&mut app, open, Event::FolderOpened(Ok(inbox)));
    (app, effects)
}

/// [`folders_app_at`] 120 columns, with `sidebar` visible.
fn folders_app(sidebar: SidebarSettings) -> (App, Vec<Effect>) {
    let visible = SidebarSettings {
        visible: true,
        ..sidebar
    };
    folders_app_at(visible, 120)
}

/// The request listing the folders among `effects`.
fn folders_listing(effects: &[Effect]) -> Option<RequestId> {
    let requests = requests(effects);
    let mut listings = requests
        .iter()
        .filter(|(_, request)| *request == Request::ListFolders);
    let listing = listings.next().map(|(id, _)| *id);
    assert!(listings.next().is_none(), "listed once");
    listing
}

/// Inbox, with 2 unread of 5, its subfolder Projects and Archive.
fn every_folder() -> Vec<Folder> {
    let folder = |id: &str, name: &str, parent: Option<&str>, unread| Folder {
        parent: parent.map(mett_core::FolderId::new),
        unread_count: unread,
        total_count: 5,
        ..mett_core::mock::folder(id, name, None)
    };
    vec![
        folder("in", "Inbox", None, 2),
        folder("p", "Projects", Some("in"), 0),
        folder("arc", "Archive", None, 1),
    ]
}

/// [`folders_app`], its folders listed.
fn folders_listed(sidebar: SidebarSettings) -> App {
    let (mut app, effects) = folders_app(sidebar);
    let listing = folders_listing(&effects).expect("folders listed");
    respond(&mut app, listing, Event::FoldersListed(Ok(every_folder())));
    app
}

#[test]
fn lists_every_folder_without_mailboxes() {
    let mut app = folders_listed(SidebarSettings::default());
    assert_eq!(shown(&app), ["Archive", "*Inbox", "Inbox/Projects"]);
    assert!(app.sidebar.entries[1].open);
    assert_eq!(app.sidebar.entries[0].counts, Some((1, 5)));
    assert!(app.sidebar.entries[0].has_mail());
    // Listed once, until they may have changed: the next check.
    assert_eq!(folders_listing(&act(&mut app, Action::SidebarNext)), None);
    let effects = app.update(Msg::Timer(Timer::Poll(0)));
    let listing = folders_listing(&effects).expect("listed again");
    let mut fewer = every_folder();
    fewer.pop();
    respond(&mut app, listing, Event::FoldersListed(Ok(fewer)));
    assert_eq!(
        shown(&app),
        ["Inbox", "*Inbox/Projects"],
        "the cursor stays"
    );
    // A failed listing keeps them, says why, and is tried again once
    // the user does something, not by itself.
    let effects = app.update(Msg::Timer(Timer::Poll(0)));
    let listing = folders_listing(&effects).expect("listed again");
    respond(&mut app, listing, Event::FoldersListed(Err(offline())));
    assert_eq!(shown(&app), ["Inbox", "*Inbox/Projects"]);
    assert!(!app.status.is_empty(), "the failure is reported");
    let effects = app.update(Msg::Timer(Timer::Poll(0)));
    assert_eq!(folders_listing(&effects), None);
    assert!(folders_listing(&act(&mut app, Action::SidebarFirst)).is_some());
}

#[test]
fn lists_the_folders_again_once_the_mailboxes_go() {
    let mut app = folders_listed(SidebarSettings::default());
    let visible = SidebarSettings {
        visible: true,
        ..SidebarSettings::default()
    };
    let mut settings = with_mailboxes(visible);
    let effects = app.update(configured(settings.clone()));
    assert_eq!(folders_listing(&effects), None);
    assert_eq!(shown(&app), ["*Inbox", "Work", "Lists"]);
    settings.mailboxes.clear();
    let effects = app.update(configured(settings));
    assert!(folders_listing(&effects).is_some(), "maybe changed since");
}

#[test]
fn opens_a_listed_folder() {
    let mut app = folders_listed(SidebarSettings::default());
    act(&mut app, Action::SidebarLast);
    let effects = act(&mut app, Action::SidebarOpen);
    let projects = FolderPath::Named(vec!["Inbox".into(), "Projects".into()]);
    assert!(
        matches!(&requests(&effects)[..], [(_, Request::OpenFolder(path))] if *path == projects),
        "{effects:?}"
    );
}

#[test]
fn shortens_and_indents_listed_folders() {
    let app = folders_listed(SidebarSettings {
        short_path: true,
        folder_indent: true,
        ..SidebarSettings::default()
    });
    assert_eq!(shown(&app), ["Archive", "*Inbox", "  Projects"]);
}

#[test]
fn lists_folders_only_when_shown_without_mailboxes() {
    let (_, effects) = folders_app_at(SidebarSettings::default(), 120);
    assert_eq!(folders_listing(&effects), None, "hidden");
    let visible = SidebarSettings {
        visible: true,
        ..SidebarSettings::default()
    };
    let (mut app, effects) = folders_app_at(visible, 80);
    assert_eq!(folders_listing(&effects), None, "too narrow");
    let effects = app.update(Msg::Resize {
        width: 120,
        height: 24,
    });
    assert!(folders_listing(&effects).is_some(), "widened");
    // The mailboxes are shown instead, if there are any.
    let visible = SidebarSettings {
        visible: true,
        ..SidebarSettings::default()
    };
    let (mut app, _) = mailboxes_app(with_mailboxes(visible), 120, 24);
    let effects = app.update(Msg::Timer(Timer::Poll(0)));
    assert_eq!(folders_listing(&effects), None);
}

#[test]
fn tab_gives_the_sidebar_the_keys() {
    let mut app = shown_app(SidebarSettings::default(), 24, &[]);
    key(&mut app, KeyCode::Tab);
    assert_eq!(app.menu(), Menu::Sidebar);
    assert!(app.sidebar_focused());
    // The index's keys move the sidebar's cursor instead.
    let selected = app.selected;
    key(&mut app, KeyCode::Char('j'));
    key(&mut app, KeyCode::Down);
    assert_eq!(shown(&app), ["Inbox", "Work", "*Lists"]);
    key(&mut app, KeyCode::Char('k'));
    assert_eq!(shown(&app), ["Inbox", "*Work", "Lists"]);
    assert_eq!(app.selected, selected);
    // Tab, or Esc, gives the keys back.
    key(&mut app, KeyCode::Tab);
    assert_eq!(app.menu(), Menu::Index);
    key(&mut app, KeyCode::Tab);
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.menu(), Menu::Index);
    // Enter opens the folder, and gives the keys back.
    key(&mut app, KeyCode::Tab);
    let effects = key(&mut app, KeyCode::Enter);
    assert!(
        matches!(&requests(&effects)[..], [(_, Request::OpenFolder(path))] if *path == mailbox_path("Work")),
        "{effects:?}"
    );
    assert_eq!(app.menu(), Menu::Index);
}

#[test]
fn the_pager_gives_the_sidebar_the_keys_too() {
    let mut app = shown_app(SidebarSettings::default(), 24, &[]);
    let text = crate::app::TextView::new("notes.txt".into(), vec!["a".into()], 50);
    app.screens.push(crate::app::Screen::Text(text));
    key(&mut app, KeyCode::Tab);
    assert_eq!(app.menu(), Menu::Sidebar);
    key(&mut app, KeyCode::Char('j'));
    assert_eq!(shown(&app), ["Inbox", "*Work", "Lists"]);
    key(&mut app, KeyCode::Char('q'));
    assert_eq!(app.menu(), Menu::Pager);
    assert!(matches!(app.screen(), crate::app::Screen::Text(_)));
}

#[test]
fn a_hidden_sidebar_does_not_take_the_keys() {
    let (mut app, _) = mailboxes_app(with_mailboxes(SidebarSettings::default()), 120, 24);
    key(&mut app, KeyCode::Tab);
    assert_eq!(app.menu(), Menu::Index);
    assert!(app.status.contains("sidebar_visible"), "{}", app.status);
    // Hidden while it had them, it gives them back.
    let mut app = shown_app(SidebarSettings::default(), 24, &[]);
    key(&mut app, KeyCode::Tab);
    act(&mut app, Action::SidebarToggleVisible);
    assert_eq!(app.menu(), Menu::Index);
    // Hidden by a narrower terminal, it gives them back too, and
    // hasn't them once shown again.
    let mut app = shown_app(SidebarSettings::default(), 24, &[]);
    key(&mut app, KeyCode::Tab);
    for width in [80, 120] {
        app.update(Msg::Resize { width, height: 24 });
        assert_eq!(app.menu(), Menu::Index);
    }
    // Too narrow, it says how wide the terminal has to be.
    let mut app = shown_app(SidebarSettings::default(), 24, &[]);
    app.update(Msg::Resize {
        width: 80,
        height: 24,
    });
    key(&mut app, KeyCode::Tab);
    assert_eq!(app.menu(), Menu::Index);
    assert!(app.status.contains("columns wide"), "{}", app.status);
}

#[test]
fn lists_the_folders_again_if_they_changed_while_listed() {
    let (mut app, effects) = folders_app(SidebarSettings::default());
    let listing = folders_listing(&effects).expect("folders listed");
    let effects = app.update(Msg::Timer(Timer::Poll(0)));
    assert_eq!(folders_listing(&effects), None, "one at a time");
    let effects = respond(&mut app, listing, Event::FoldersListed(Ok(every_folder())));
    assert!(folders_listing(&effects).is_some(), "maybe outdated");
}

#[test]
fn lists_the_folders_if_the_folder_fails_to_open() {
    let settings = crate::app::Settings {
        sidebar: SidebarSettings {
            visible: true,
            ..SidebarSettings::default()
        },
        ..crate::app::Settings::default()
    };
    let (mut app, effects) = App::open(120, 24, inbox(), settings);
    let open = first_request(&effects);
    let gone = mett_core::Error::not_found("folder", "Inbox");
    let effects = respond(&mut app, open, Event::FolderOpened(Err(gone)));
    assert!(folders_listing(&effects).is_some(), "to choose another");
}

#[test]
fn well_known_paths_name_listed_folders() {
    let (mut app, effects) = folders_app(SidebarSettings {
        new_mail_only: true,
        whitelist: vec![FolderPath::WellKnown(
            mett_core::WellKnownFolder::SentItems,
            vec!["Old".into()],
        )],
        ..SidebarSettings::default()
    });
    let listing = folders_listing(&effects).expect("folders listed");
    let sent = Folder {
        well_known: Some(mett_core::WellKnownFolder::SentItems),
        ..mett_core::mock::folder("sent", "Sent Items", None)
    };
    let old = Folder {
        parent: Some(mett_core::FolderId::new("sent")),
        ..mett_core::mock::folder("old", "Old", None)
    };
    let quiet = mett_core::mock::folder("q", "Quiet", None);
    let folders = vec![
        mett_core::mock::folder("in", "Inbox", None),
        sent,
        old,
        quiet,
    ];
    respond(&mut app, listing, Event::FoldersListed(Ok(folders)));
    assert_eq!(shown(&app), ["*Inbox", "Sent Items/Old"]);
}

#[test]
fn imap_folders_are_told_apart_by_case_except_the_inbox() {
    let named = |name: &str| FolderPath::Named(vec![name.into()]);
    assert!(same_path(&named("Foo"), &named("foo"), Api::Graph));
    assert!(!same_path(&named("Foo"), &named("foo"), Api::Imap));
    assert!(same_path(&named("Foo"), &named("Foo"), Api::Imap));
    assert!(same_path(&named("INBOX"), &named("Inbox"), Api::Imap));
    // Only the top-level one: `INBOX/Inbox` and `INBOX/INBOX` are two.
    let below = |name: &str| FolderPath::Named(vec!["INBOX".into(), name.into()]);
    assert!(!same_path(&below("Inbox"), &below("INBOX"), Api::Imap));
    assert!(same_path(&below("Inbox"), &below("Inbox"), Api::Imap));
}
