use chrono::DateTime;
use mett_core::{MessageSummary, mock};
use ratatui::crossterm::event::KeyCode;

use super::*;
use crate::app::composing::Input;
use crate::app::testing::*;
use crate::worker::Event;

/// Messages m0, m1 and m2.
fn three() -> Vec<MessageSummary> {
    plain(&["m0", "m1", "m2"])
}

/// An app with three messages in its index.
fn app() -> App {
    opened(Settings::default(), 10, three(), None).0
}

fn press(app: &mut App, key: Key) -> Vec<Effect> {
    app.update(Msg::Key(key.event()))
}

/// Plays what is queued, as the main loop does: not while the last
/// action waits for the mailbox.
fn play(app: &mut App) {
    while let Some(msg) = app.next_stroke() {
        app.update(msg);
    }
}

#[test]
fn keys_run_their_functions() {
    let mut app = app();
    press(&mut app, Key::char('j'));
    assert_eq!(app.index.selected, 1);
    press(&mut app, Key::char('#'));
    assert_eq!(app.status, "Key is not bound.  Press '?' for help.");
    // The next key clears the message.
    press(&mut app, Key::char('k'));
    assert_eq!(app.status, "");
}

#[test]
fn macros_play_their_keys_and_functions() {
    let mut app = app();
    app.keymap
        .apply(&KeyCommand::Macro {
            menus: "index".into(),
            key: "X".into(),
            sequence: "<last-entry>k/s<enter>".into(),
            description: None,
        })
        .unwrap();
    press(&mut app, Key::char('X'));
    play(&mut app);
    // To the last message, up one, then a search that finds the next.
    assert_eq!(app.index.selected, 2);
    assert_eq!(app.input, None, "the search question was answered");
}

#[test]
fn push_queues_keys_to_play() {
    let mut app = app();
    app.push_keys("jj").unwrap();
    play(&mut app);
    assert_eq!(app.index.selected, 2);
    assert!(app.push_keys("<frobnicate>").is_err());
}

#[test]
fn the_help_screen_lists_the_menu_s_keys() {
    let mut app = app();
    press(&mut app, Key::char('?'));
    assert_eq!(app.menu(), Menu::Help);
    let lines: Vec<String> = app.help_lines(Menu::Index).collect();
    assert!(lines[0].starts_with("q            quit"), "{lines:#?}");
    assert!(
        lines.iter().any(|l| l.contains("enter-command")),
        "generic keys too"
    );
    // Scrolls, within the list.
    for _ in 0..100 {
        press(&mut app, Key::char('j'));
    }
    let Screen::Help(help) = app.screen() else {
        panic!("not in help");
    };
    assert_eq!(help.scroll, lines.len() - app.body_height);
    press(&mut app, Key::char('q'));
    assert_eq!(app.menu(), Menu::Index);
}

#[test]
fn commands_typed_at_the_prompt_reconfigure_the_app() {
    let mut app = app();
    press(&mut app, Key::char(':'));
    for c in "bind index J last-entry".chars() {
        press(&mut app, Key::char(c));
    }
    let effects = press(&mut app, Key::new(KeyCode::Enter));
    assert_eq!(effects, [Effect::Command("bind index J last-entry".into())]);

    // What the main loop sends back.
    app.update(Msg::Configured(Reconfigured {
        outcome: Ok(vec![]),
        settings: Ok(Settings {
            resolve: false,
            ..Settings::default()
        }),
        key_commands: vec![KeyCommand::Bind {
            menus: "index".into(),
            key: "J".into(),
            function: "last-entry".into(),
        }],
        pushed: vec!["J".into()],
    }));
    assert_eq!(app.status, "");
    assert!(!app.resolve);
    play(&mut app);
    assert_eq!(app.index.selected, 2);

    app.update(Msg::Configured(Reconfigured {
        outcome: Err("timeout: expected a number".into()),
        settings: Ok(Settings::default()),
        key_commands: vec![KeyCommand::Bind {
            menus: "nowhere".into(),
            key: "J".into(),
            function: "quit".into(),
        }],
        pushed: vec![],
    }));
    assert_eq!(
        app.status,
        "timeout: expected a number; unknown menu \"nowhere\""
    );
}

#[test]
fn a_macro_that_plays_itself_is_stopped() {
    let mut app = app();
    app.keymap
        .apply(&KeyCommand::Macro {
            menus: "index".into(),
            key: "x".into(),
            sequence: "x".into(),
            description: None,
        })
        .unwrap();
    press(&mut app, Key::char('x'));
    play(&mut app);
    assert_eq!(app.status, "Macro loop detected: stopped.");
    assert!(app.take_redraw(), "the status is shown");
    assert!(!app.take_redraw());
    assert!(app.next_stroke().is_none());
}

#[test]
fn startup_keys_wait_for_the_folder() {
    let (mut app, effects) = App::open(80, 10, inbox(), Settings::default());
    app.push_after_load(crate::keymap::parse_strokes("<last-entry>").unwrap());
    assert!(app.next_stroke().is_none(), "nothing to move through yet");
    let open = first_request(&effects);
    let list = folder_opened(&mut app, open, mock::folder("in", "Inbox", None));
    listed(&mut app, list, three(), None);
    play(&mut app);
    assert_eq!(app.index.selected, 2);
}

#[test]
fn a_notice_is_said_again_once_the_folder_is_listed() {
    let (mut app, effects) = App::open(80, 10, inbox(), Settings::default());
    app.notice("warning: gui_font: x: no such file".into());
    assert_eq!(app.status, "warning: gui_font: x: no such file");
    let open = first_request(&effects);
    let list = folder_opened(&mut app, open, mock::folder("in", "Inbox", None));
    listed(&mut app, list, three(), None);
    assert_eq!(app.status, "warning: gui_font: x: no such file");
}

#[test]
fn a_macro_waits_for_what_its_keys_started() {
    let mut app = app();
    app.keymap
        .apply(&KeyCommand::Macro {
            menus: "index".into(),
            key: "R".into(),
            sequence: "r<last-entry>".into(),
            description: None,
        })
        .unwrap();
    press(&mut app, Key::char('R'));
    // `r` fetches the message: <last-entry> waits, rather than
    // cancelling the reply.
    play(&mut app);
    assert!(app.waiting());
    assert_eq!(app.strokes.len(), 1);
}

#[test]
fn help_and_commands_cancel_a_pending_reply() {
    let mut app = app();
    press(&mut app, Key::char('r'));
    assert!(app.waiting());
    press(&mut app, Key::char('?'));
    assert!(!app.waiting(), "the reply was given up");
}

#[test]
fn keys_pushed_by_a_macro_play_before_its_rest() {
    let mut app = app();
    app.strokes.push_back(Stroke::Key(Key::char('k')));
    app.push_keys("<last-entry>").unwrap();
    play(&mut app);
    assert_eq!(app.index.selected, 1, "last entry, then up one");
}

#[test]
fn queued_keys_wait_for_a_sign_in() {
    let mut app = app();
    app.update(Msg::SignIn(crate::signin::SignInEvent::Finished {
        account: mett_core::AccountId::new("a@b.c"),
        outcome: Err("expired".into()),
    }));
    app.push_keys("j").unwrap();
    assert!(app.waiting());
}

#[test]
fn help_waits_for_the_folder_list() {
    let mut app = app();
    let effects = act(&mut app, Action::BrowseMailboxes);
    assert!(matches!(effects[..], [Effect::Request(..)]));
    act(&mut app, Action::Help);
    assert!(matches!(app.screen(), Screen::Browser(_)));
    assert_eq!(app.status, "Listing folders; wait a moment.");
}

#[test]
fn pushed_sequences_play_in_order() {
    let mut app = app();
    app.update(Msg::Configured(Reconfigured {
        outcome: Ok(vec![]),
        settings: Ok(Settings::default()),
        key_commands: vec![],
        pushed: vec!["<last-entry>".into(), "<previous-entry>".into()],
    }));
    play(&mut app);
    assert_eq!(app.index.selected, 1);
}

#[test]
fn macros_wait_for_mailbox_changes() {
    let mut app = app();
    app.push_keys("<save-message>=Archive<enter><delete-message>")
        .unwrap();
    let mut effects = Vec::new();
    while let Some(msg) = app.next_stroke() {
        effects.extend(app.update(msg));
        if app.waiting() {
            break;
        }
    }
    assert!(app.waiting(), "the move is on its way");
    assert!(!app.strokes.is_empty(), "d waits for it");
    let Some(Effect::Request(id, _)) = effects.last().cloned() else {
        panic!("expected a request, got {effects:?}");
    };
    respond(
        &mut app,
        id,
        Event::Transferred(Ok((
            mett_core::FolderId::new("archive"),
            vec![("m0".into(), Ok("a0".into()))],
        ))),
    );
    assert!(!app.waiting());
}

#[test]
fn sign_in_macros_play_in_the_window() {
    let mut app = app();
    app.keymap
        .apply(&crate::config::KeyCommand::Macro {
            menus: "signin".into(),
            key: "x".into(),
            sequence: "<copy-link>".into(),
            description: None,
        })
        .unwrap();
    app.push_keys("<last-entry>").unwrap();
    app.update(Msg::SignIn(browser_prompt()));
    press(&mut app, Key::char('x'));
    let Some(msg) = app.next_stroke() else {
        panic!("the macro waits");
    };
    assert_eq!(
        app.update(msg),
        [Effect::Copy("https://login.example/authorize".into())]
    );
    assert!(app.next_stroke().is_none(), "the pushed keys wait");
    // The same, while quitting: only the window's macro plays.
    press(&mut app, Key::char('x'));
    assert!(app.next_sign_in_stroke().is_some());
    assert!(app.next_sign_in_stroke().is_none());
    assert_eq!(app.index.selected, 0);
}

#[test]
fn back_tab_bindings_match_shift_tab() {
    let event = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
    assert_eq!(
        Key::from(event),
        crate::keymap::parse_strokes("<BackTab>")
            .unwrap()
            .into_iter()
            .find_map(|s| match s {
                Stroke::Key(key) => Some(key),
                Stroke::Function(_) => None,
            })
            .unwrap()
    );
}

#[test]
fn ctrl_j_is_enter_unless_bound() {
    let mut first = app();
    press(&mut first, Key::ctrl('j'));
    assert!(
        matches!(first.screen(), Screen::Pager(_)),
        "opens the message"
    );
    let mut app = app();
    app.keymap
        .apply(&crate::config::KeyCommand::Bind {
            menus: "index".into(),
            key: "\\Cj".into(),
            function: "last-entry".into(),
        })
        .unwrap();
    press(&mut app, Key::ctrl('j'));
    assert_eq!(app.index.selected, 2);
    assert_eq!(app.screen(), &Screen::Index, "Enter is left alone");
    press(&mut app, Key::new(KeyCode::Enter));
    assert!(matches!(app.screen(), Screen::Pager(_)));
}

#[test]
fn help_waits_for_another_folder_to_open() {
    let mut app = app();
    act(&mut app, Action::ChangeFolder);
    for c in "=Archive".chars() {
        press(&mut app, Key::char(c));
    }
    press(&mut app, Key::new(KeyCode::Enter));
    assert!(app.switching.is_some());
    act(&mut app, Action::Help);
    assert_eq!(app.screen(), &Screen::Index);
    assert_eq!(app.status, "Opening folder; wait a moment.");
}

#[test]
fn help_waits_for_the_message() {
    let mut app = app();
    act(&mut app, Action::DisplayMessage);
    act(&mut app, Action::Help);
    assert!(matches!(app.screen(), Screen::Pager(_)));
    assert_eq!(app.status, "Loading message; wait a moment.");
}

#[test]
fn a_taller_help_screen_scrolls_back() {
    let mut app = app();
    act(&mut app, Action::Help);
    act(&mut app, Action::LastEntry);
    let Screen::Help(help) = app.screen() else {
        panic!("not in help");
    };
    assert!(help.scroll > 0);
    app.update(Msg::Resize {
        width: 80,
        height: 500,
    });
    let Screen::Help(help) = app.screen() else {
        panic!("not in help");
    };
    assert_eq!(help.scroll, 0, "it all fits now");
}

#[test]
fn typing_while_a_macro_waits_stops_it() {
    let mut app = app();
    app.push_keys("<reply><delete-message>").unwrap();
    let msg = app.next_stroke().expect("the reply plays");
    app.update(msg);
    assert!(app.waiting(), "for the message to reply to");
    // The user moves on meanwhile.
    app.typed();
    press(&mut app, Key::char('j'));
    assert!(app.next_stroke().is_none());
    assert!(!app.is_deleted(&mett_core::MessageId::new("m1")));
}

#[test]
fn typing_between_macro_keys_that_wait_for_nothing_keeps_them() {
    let mut app = app();
    app.push_keys("<next-entry><next-entry>").unwrap();
    app.typed();
    play(&mut app);
    assert_eq!(app.index.selected, 2);
}

#[test]
fn help_waits_for_a_delivery() {
    let mut app = app();
    app.screens.push(Screen::Compose(crate::app::Compose {
        draft: mett_core::Draft::default(),
        delivery: Some((
            crate::worker::RequestId(99),
            crate::app::composing::Delivery::Send,
        )),
        unauthorized: None,
        forwarded_attachments: false,
        selected: 0,
        left_draft: false,
        kept_draft: false,
        kept: Vec::new(),
    }));
    act(&mut app, Action::Help);
    assert!(matches!(app.screen(), Screen::Compose(_)));
    assert_eq!(app.status, "Sending message; wait a moment.");
}

#[test]
fn commands_change_the_mailbox_shortcuts() {
    let mut app = app();
    app.update(configured(Settings {
        shortcuts: crate::app::Shortcuts {
            mbox: "=@archive".into(),
            ..Default::default()
        },
        ..Settings::default()
    }));
    assert_eq!(app.shortcuts.mbox, "=@archive");
}

#[test]
fn signing_in_keeps_a_waiting_macro() {
    let mut app = app();
    app.push_keys("<last-entry>").unwrap();
    app.update(Msg::SignIn(browser_prompt()));
    assert!(app.waiting());
    app.typed();
    assert!(!app.strokes.is_empty(), "the macro waits for the sign-in");
}

#[test]
fn a_prompt_ignores_a_macro_s_functions() {
    for then in ["<help>", "<enter-command>"] {
        let mut app = app();
        app.push_keys(&format!("<save-message>{then}")).unwrap();
        play(&mut app);
        assert_eq!(app.screen(), &Screen::Index);
        assert!(matches!(
            app.input,
            Some(Input::Line {
                question: Question::SaveTo,
                ..
            })
        ));
        assert_eq!(app.status, "Not available in a prompt.");
    }
}

#[test]
fn macros_wait_for_a_reply_s_recipients() {
    let mut app = app();
    let message = mock::message("m0", "in", "s", DateTime::UNIX_EPOCH);
    app.awaiting_addresses = Some((crate::compose::Kind::Reply, message));
    app.push_keys("<last-entry>").unwrap();
    assert!(app.waiting());
    assert!(app.next_stroke().is_none());
}

#[test]
fn startup_keys_wait_for_a_sign_in_to_open_the_folder() {
    let (mut app, effects) = App::open(80, 10, inbox(), Settings::default());
    app.push_after_load(crate::keymap::parse_strokes("<last-entry>").unwrap());
    let open = first_request(&effects);
    respond(&mut app, open, Event::FolderOpened(Err(expired())));
    assert!(app.next_stroke().is_none(), "not before the folder is open");
    assert!(!app.after_load.is_empty(), "still waiting");
}

#[test]
fn commands_keep_the_mailbox_s_addresses() {
    let mut app = app();
    respond(
        &mut app,
        crate::worker::RequestId(999),
        Event::OwnAddresses(Ok(vec!["Alias@Contoso.com".into()])),
    );
    assert!(app.settings.me.contains(&"alias@contoso.com".to_owned()));
    app.update(configured(Settings::default()));
    assert!(app.settings.me.contains(&"alias@contoso.com".to_owned()));
}
