use super::*;
use crate::app::Settings;
use crate::app::testing::{act, empty_app, opened};

fn app() -> App {
    empty_app()
}

fn tabs(active: usize, count: usize) -> Tabs {
    Tabs {
        tabs: (0..count)
            .map(|n| Tab {
                label: format!("Account {n}"),
                new_mail: false,
            })
            .collect(),
        active,
        position: TabPosition::Top,
    }
}

#[test]
fn next_and_previous_go_round() {
    assert_eq!(Switch::Next.target(0, 3), Some(1));
    assert_eq!(Switch::Next.target(2, 3), Some(0));
    assert_eq!(Switch::Previous.target(0, 3), Some(2));
    assert_eq!(Switch::Previous.target(2, 3), Some(1));
    assert_eq!(Switch::To(1).target(0, 3), Some(1));
    assert_eq!(Switch::To(3).target(0, 3), None);
}

#[test]
fn tab_position_is_top_or_bottom() {
    assert_eq!("top".parse(), Ok(TabPosition::Top));
    assert_eq!("bottom".parse(), Ok(TabPosition::Bottom));
    assert!(
        "left"
            .parse::<TabPosition>()
            .unwrap_err()
            .contains("top or bottom")
    );
}

#[test]
fn the_account_actions_ask_the_main_loop_to_switch() {
    let mut app = app();
    app.tabs = Some(tabs(0, 3));
    assert_eq!(
        act(&mut app, Action::NextAccount),
        [Effect::Account(Switch::Next)]
    );
    assert_eq!(
        act(&mut app, Action::PreviousAccount),
        [Effect::Account(Switch::Previous)]
    );
    assert_eq!(
        act(&mut app, Action::Account(3)),
        [Effect::Account(Switch::To(2))]
    );
    // The one shown already: nothing to do.
    assert!(act(&mut app, Action::Account(1)).is_empty());
    // One there isn't.
    assert!(act(&mut app, Action::Account(5)).is_empty());
    assert_eq!(app.status, "There is no account 5.");
}

#[test]
fn keys_still_to_play_can_be_handed_over() {
    let mut from = app();
    from.strokes.extend([
        crate::keymap::Stroke::Key(crate::keymap::Key::char('j')),
        crate::keymap::Stroke::Key(crate::keymap::Key::char('k')),
    ]);
    let mut to = app();
    from.played = 7;
    let strokes = from.take_strokes();
    assert_eq!(strokes.0.len(), 2);
    assert!(from.take_strokes().0.is_empty());
    to.give_strokes(strokes);
    // The count goes on, so that a macro going back and forth stops.
    let (queued, played) = to.take_strokes();
    assert_eq!(queued.len(), 2);
    assert_eq!(played, 7);
}

#[test]
fn going_to_another_account_leaves_a_reply_being_fetched_pending() {
    let message =
        mett_core::mock::message("m", "in", "Hello", chrono::DateTime::UNIX_EPOCH).summary;
    let mut app = opened(Settings::default(), 24, vec![message], None).0;
    app.tabs = Some(tabs(0, 2));
    // A reply asks for its message first.
    let effects = act(&mut app, Action::Reply);
    assert!(!effects.is_empty());
    assert!(app.compose_fetch.is_some());
    act(&mut app, Action::NextAccount);
    assert!(app.compose_fetch.is_some(), "still waiting for it");
}

#[test]
fn with_one_account_there_is_nothing_to_go_to() {
    let mut app = app();
    assert!(act(&mut app, Action::NextAccount).is_empty());
    assert_eq!(app.status, "There is only one account.");
}
