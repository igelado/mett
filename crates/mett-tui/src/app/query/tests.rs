use chrono::DateTime;
use mett_core::mock;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use crate::alias::Alias;
use crate::app::testing::*;
use crate::app::{Msg, Settings};

use crate::worker::Event;

fn settings(query: QuerySettings) -> Settings {
    let alias = |key: &str, value: &str| Alias {
        key: key.into(),
        value: value.into(),
    };
    Settings {
        aliases: vec![
            alias("bob", "Bob Smith <bob@x.org>"),
            alias("bobby", "bobby@y.org"),
            alias("team", "bob, jane@x.org"),
        ],
        alias_file: Some("/home/alice/.mettrc".into()),
        query,
        ..Settings::default()
    }
}

fn app(query: QuerySettings) -> App {
    let mut message = mock::message("m0", "in", "Hi", DateTime::UNIX_EPOCH).summary;
    message.from = Some(Address::new("carol@z.org").with_name("Carol Jones"));
    opened(settings(query), 24, vec![message], None).0
}

/// Starts an alias named `name`, without the questions.
fn alias_named(app: &mut App, name: &str) {
    act(app, Action::CreateAlias);
    app.input = None;
    app.answer_alias_name(name.into());
    app.input = None;
}

fn answer_text(app: &App) -> String {
    match &app.input {
        Some(Input::Line { editor, .. }) => editor.text(),
        _ => panic!("expected a question, got {:?}", app.input),
    }
}

#[test]
fn aliases_complete_and_expand_in_address_questions() {
    let mut app = app(QuerySettings::default());
    act(&mut app, Action::Mail);
    typed(&mut app, "jane@x.org, te");
    key(&mut app, KeyCode::Tab);
    assert_eq!(answer_text(&app), "jane@x.org, team");
    // Several: as far as they all go, then the list.
    typed(&mut app, ", b");
    key(&mut app, KeyCode::Tab);
    assert_eq!(answer_text(&app), "jane@x.org, team, bob");
    key(&mut app, KeyCode::Tab);
    let Screen::Query(menu) = app.screen() else {
        panic!("expected the alias list");
    };
    assert_eq!(menu.entries.len(), 2);
    act(&mut app, Action::NextEntry);
    act(&mut app, Action::DisplayMessage);
    assert_eq!(answer_text(&app), "jane@x.org, team, bobby");
    key(&mut app, KeyCode::Enter);
    let to: Vec<String> = app
        .composing
        .as_ref()
        .unwrap()
        .draft
        .to
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        to,
        [
            "jane@x.org",
            "Bob Smith <bob@x.org>",
            "jane@x.org",
            "bobby@y.org"
        ]
    );
}

#[test]
fn queries_fill_in_address_questions() {
    let mut app = app(QuerySettings {
        command: String::new(),
        directory: true,
    });
    act(&mut app, Action::Mail);
    typed(&mut app, "car");
    let effects = app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('t'),
        KeyModifiers::CONTROL,
    )));
    let [Effect::Request(id, Request::Query(QuerySource::Directory, text))] = effects.as_slice()
    else {
        panic!("expected a query, got {effects:?}");
    };
    assert_eq!(text, "car");
    assert!(app.waiting());
    respond(
        &mut app,
        *id,
        Event::Queried(Ok(vec![Address::new("carol@z.org").with_name("Carol")])),
    );
    act(&mut app, Action::DisplayMessage);
    assert_eq!(answer_text(&app), "Carol <carol@z.org>");
}

#[test]
fn a_query_with_nothing_found_goes_back_to_the_question() {
    let mut app = app(QuerySettings::default());
    act(&mut app, Action::Mail);
    typed(&mut app, "x");
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('t'),
        KeyModifiers::CONTROL,
    )));
    // No query_command, and the directory is off.
    assert_eq!(answer_text(&app), "x");
    assert_eq!(app.status, "Query command not defined.");
}

#[test]
fn q_mails_who_a_query_finds() {
    let mut app = app(QuerySettings {
        command: "lookup %s".into(),
        directory: false,
    });
    act(&mut app, Action::Query);
    typed(&mut app, "carol");
    let effects = key(&mut app, KeyCode::Enter);
    let [Effect::Request(id, Request::Query(QuerySource::Command(command), _))] =
        effects.as_slice()
    else {
        panic!("expected a query, got {effects:?}");
    };
    assert_eq!(command, "lookup %s");
    respond(
        &mut app,
        *id,
        Event::Queried(Ok(vec![Address::new("carol@z.org")])),
    );
    act(&mut app, Action::Mail);
    assert_eq!(answer_text(&app), "carol@z.org");
}

#[test]
fn completes_the_alias_at_the_cursor() {
    let mut app = app(QuerySettings::default());
    act(&mut app, Action::Mail);
    typed(&mut app, "bo, x@y.org");
    key(&mut app, KeyCode::Home);
    key(&mut app, KeyCode::Right);
    key(&mut app, KeyCode::Right);
    key(&mut app, KeyCode::Tab);
    assert_eq!(answer_text(&app), "bob, x@y.org");
    // The cursor stays after what was completed.
    key(&mut app, KeyCode::Char('b'));
    assert_eq!(answer_text(&app), "bobb, x@y.org");
    // Giving up the list leaves what was typed as it was.
    key(&mut app, KeyCode::Home);
    for _ in 0..3 {
        key(&mut app, KeyCode::Right);
    }
    key(&mut app, KeyCode::Tab);
    assert!(matches!(app.screen(), Screen::Query(_)));
    act(&mut app, Action::Exit);
    assert_eq!(answer_text(&app), "bobb, x@y.org");
    // The rest of the entry is replaced.
    key(&mut app, KeyCode::Home);
    key(&mut app, KeyCode::Right);
    key(&mut app, KeyCode::Right);
    key(&mut app, KeyCode::Tab);
    assert_eq!(answer_text(&app), "bob, x@y.org");
}

#[test]
fn no_help_over_a_lookup_and_leaving_it_gives_it_up() {
    let mut app = app(QuerySettings {
        command: "lookup %s".into(),
        directory: false,
    });
    act(&mut app, Action::Query);
    typed(&mut app, "car");
    let (query, _) = sent(&key(&mut app, KeyCode::Enter));
    act(&mut app, Action::Help);
    assert!(matches!(app.screen(), Screen::Query(_)));
    let effects = act(&mut app, Action::Exit);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Request(_, Request::Cancel(q)) if *q == query))
    );
}

#[test]
fn a_failed_completion_shows_over_the_question() {
    let mut app = app(QuerySettings::default());
    act(&mut app, Action::Mail);
    typed(&mut app, "zz");
    key(&mut app, KeyCode::Tab);
    assert_eq!(app.input_note.as_deref(), Some("No alias matches."));
    // The next key brings the question back.
    key(&mut app, KeyCode::Char('z'));
    assert_eq!(app.input_note, None);
    assert_eq!(answer_text(&app), "zzz");
}

#[test]
fn without_an_alias_file_says_the_alias_is_not_saved() {
    let mut app = app(QuerySettings::default());
    app.alias_file = None;
    act(&mut app, Action::CreateAlias);
    key(&mut app, KeyCode::Enter);
    let effects = key(&mut app, KeyCode::Enter);
    assert_eq!(effects.len(), 1);
    app.update(configured(settings(QuerySettings::default())));
    assert_eq!(
        app.status,
        "Alias carol added (no alias_file to save it in)."
    );
}

#[test]
fn saves_the_sender_as_an_alias() {
    let mut app = app(QuerySettings::default());
    act(&mut app, Action::CreateAlias);
    assert_eq!(answer_text(&app), "carol");
    let effects = key(&mut app, KeyCode::Enter);
    assert!(effects.is_empty());
    assert_eq!(answer_text(&app), "Carol Jones <carol@z.org>");
    let effects = key(&mut app, KeyCode::Enter);
    let line = "alias carol Carol Jones <carol@z.org>";
    assert_eq!(
        effects,
        [
            Effect::Command(line.into()),
            Effect::AppendFile {
                path: "/home/alice/.mettrc".into(),
                text: format!("{line}\n"),
            },
        ]
    );
    app.update(Msg::Appended(Ok("/home/alice/.mettrc".into())));
    // Said once the command is done.
    app.update(configured(settings(QuerySettings::default())));
    assert_eq!(app.status, "Alias added to /home/alice/.mettrc.");
    // An alias needs an address.
    alias_named(&mut app, "dora");
    assert!(app.answer_alias_address("  ".into()).is_empty());
    alias_named(&mut app, "dora");
    let effects = app.answer_alias_address(r#"A"B <a@b.org>"#.into());
    let Some(Effect::Command(line)) = effects.first() else {
        panic!("expected the alias command, got {effects:?}");
    };
    let mut config = crate::config::Config::new(None);
    config.run_line(line).unwrap();
    let expanded = crate::alias::expand("dora", config.aliases()).unwrap();
    assert_eq!(Address::parse_list(&expanded).unwrap()[0].email, "a@b.org");
    alias_named(&mut app, "dora");
    assert!(
        app.answer_alias_address("\"x\nunalias *\" <d@x.org>".into())
            .is_empty()
    );
    assert_eq!(
        app.status,
        "An alias's address can't have control characters."
    );
    alias_named(&mut app, "dora");
    assert!(app.answer_alias_address("  ".into()).is_empty());
    assert_eq!(app.status, "No address: the alias wasn't added.");
    // An alias by that name already exists.
    act(&mut app, Action::CreateAlias);
    app.input = None;
    app.answer_alias_name("bob".into());
    assert_eq!(
        app.status,
        "You already have an alias defined with that name!"
    );
}
