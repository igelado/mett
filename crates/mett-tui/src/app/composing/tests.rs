use chrono::DateTime;
use mett_core::{Error, MessageSummary, mock};

use super::*;
use crate::app::testing::sent as request;
use crate::app::testing::*;
use crate::app::{Msg, Settings};
use crate::keymap::Menu;
use crate::worker::Event;

/// An app that has opened its Inbox; returns it, the request listing
/// the Inbox and the one asking for the user's own addresses.
fn opening(compose: compose::Settings) -> (App, RequestId, RequestId) {
    let inbox = crate::config::mailbox::FolderPath::Named(vec!["Inbox".into()]);
    let settings = Settings {
        compose,
        ..Settings::default()
    };
    let (mut app, effects) = App::open(80, 24, inbox, settings);
    let (open, _) = request(&effects);
    let folder = mock::folder("in", "Inbox", None);
    let effects = respond(&mut app, open, Event::FolderOpened(Ok(folder)));
    let list = first_request(&effects);
    let addresses = first_request(&effects[1..]);
    (app, list, addresses)
}

/// An app with one message, from Bob, in its index.
fn app(compose: compose::Settings) -> App {
    let (mut app, list, addresses) = opening(compose);
    // The mailbox knows of no other addresses of the user's.
    respond(&mut app, addresses, Event::OwnAddresses(Ok(Vec::new())));
    listed(&mut app, list, vec![original().summary], None);
    app
}

fn original() -> Message {
    let mut message = mock::message("m1", "in", "Lunch", DateTime::UNIX_EPOCH);
    message.summary.from = Some(Address::new("bob@x.org").with_name("Bob"));
    message.summary.to = vec![Address::new("me@contoso.com")];
    message.body.content = "Noon?".into();
    message
}

fn settings() -> compose::Settings {
    compose::Settings {
        from: Some(Address::new("me@contoso.com").with_name("Me")),
        me: vec!["me@contoso.com".into()],
        signature: Some("Me".into()),
        attribution: "%n wrote:".into(),
        ..compose::Settings::default()
    }
}

fn compose(app: &App) -> &Compose {
    match app.screen() {
        Screen::Compose(compose) => compose,
        other => panic!("not in the compose menu: {other:?}"),
    }
}

fn signed_out(app: &mut App) {
    app.update(Msg::SignIn(crate::signin::SignInEvent::Finished {
        account: mett_core::AccountId::new("me@contoso.com"),
        outcome: Err("expired".into()),
    }));
}

fn timed_out() -> Error {
    Error::Network("timed out".into())
}

/// Makes the open folder Sent Items.
fn in_sent_items(app: &mut App) {
    app.folder = Some(mock::folder(
        "sent",
        "Sent Items",
        Some(WellKnownFolder::SentItems),
    ));
}

/// Messages in Sent Items with these `ids`.
fn sent_items(ids: &[&str]) -> Vec<MessageSummary> {
    ids.iter()
        .map(|id| mock::message(id, "sent", "s", DateTime::UNIX_EPOCH).summary)
        .collect()
}

/// Writes new mail to bob@x.org, "Hi", with `body`.
fn written(app: &mut App, body: &str) {
    act(app, Action::Mail);
    answer(app, "bob@x.org");
    answer(app, "Hi");
    app.update(Msg::Edited(Ok(body.into())));
}

#[test]
fn writes_in_the_builtin_editor() {
    let mut app = app(compose::Settings {
        builtin_editor: Some(crate::editor::Keys::Vim),
        ..settings()
    });
    act(&mut app, Action::Mail);
    answer(&mut app, "bob@x.org");
    // No editor to run: the built-in one opens, with the signature,
    // and loads the dictionaries.
    let effects = answer(&mut app, "Hi");
    assert!(
        matches!(effects.as_slice(), [Effect::LoadSpeller(_)]),
        "{effects:?}"
    );
    let editor = app.editing.as_ref().expect("the built-in editor");
    assert_eq!(editor.borrow().text(), "\n-- \nMe\n");
    // It takes the keys, `:` included.
    typed(&mut app, "iHello");
    key(&mut app, KeyCode::Esc);
    typed(&mut app, ":wq");
    key(&mut app, KeyCode::Enter);
    assert!(app.editing.is_none());
    assert_eq!(app.menu(), Menu::Compose);
    assert_eq!(compose(&app).draft.body, "Hello\n-- \nMe\n");

    // Editing again opens it on the text; quitting at once leaves it.
    act(&mut app, Action::EditMessage);
    typed(&mut app, ":q");
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.menu(), Menu::Compose);
    assert_eq!(compose(&app).draft.body, "Hello\n-- \nMe\n");
}

#[test]
fn macros_wait_for_the_builtin_editor() {
    let mut app = app(compose::Settings {
        builtin_editor: Some(crate::editor::Keys::Notepad),
        ..settings()
    });
    act(&mut app, Action::Mail);
    answer(&mut app, "bob@x.org");
    answer(&mut app, "Hi");
    let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
    typed(&mut app, "Hi! ");
    app.update(Msg::Key(ctrl_s));
    assert_eq!(app.menu(), Menu::Compose);

    // `e` opens the editor; `<send-message>` waits for it, and for the
    // keys the user types in it.
    app.push_keys("e<send-message>").unwrap();
    let mut effects = Vec::new();
    let play = |app: &mut App, effects: &mut Vec<Effect>| {
        while let Some(msg) = app.next_stroke() {
            effects.extend(app.update(msg));
        }
    };
    play(&mut app, &mut effects);
    assert!(app.editing.is_some());
    for key in "Hello".chars().map(KeyCode::Char) {
        app.typed();
        app.update(Msg::Key(KeyEvent::from(key)));
    }
    app.typed();
    app.update(Msg::Key(ctrl_s));
    play(&mut app, &mut effects);
    let (_, Request::Send(sent)) = request(&effects) else {
        panic!("expected Send, got {effects:?}");
    };
    // The editor opens with the cursor at the start.
    assert_eq!(sent.body, "HelloHi! \n-- \nMe\n");
}

#[test]
fn closing_the_window_quits_unless_a_message_is_being_written() {
    // In the index, it quits; not while a question is asked.
    let mut index = app(settings());
    act(&mut index, Action::Search);
    index.update(Msg::Close);
    assert!(!index.quit);
    assert_eq!(
        index.input_note.as_deref(),
        Some("Answer the question (or ^G) first.")
    );
    index.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('g'),
        KeyModifiers::CONTROL,
    )));
    index.update(Msg::Close);
    assert!(index.quit);

    let mut app = app(compose::Settings {
        builtin_editor: Some(crate::editor::Keys::Vim),
        ..settings()
    });
    act(&mut app, Action::Mail);
    answer(&mut app, "bob@x.org");
    answer(&mut app, "Hi");
    app.update(Msg::Close);
    assert!(!app.quit);
    let editor = app.editing.as_ref().expect("still writing");
    // Said on the editor's bottom line.
    assert_eq!(
        editor.borrow().bottom(80),
        crate::editor::builtin::Bottom::Note(
            "A message is being written: send or postpone it first.".into()
        )
    );
    typed(&mut app, "iHello");
    key(&mut app, KeyCode::Esc);
    typed(&mut app, ":wq");
    key(&mut app, KeyCode::Enter);
    // In the compose menu, too.
    app.update(Msg::Close);
    assert!(!app.quit);
    assert_eq!(app.menu(), Menu::Compose);
}

#[test]
fn pasted_text_goes_where_the_user_types() {
    let mut app = app(compose::Settings {
        builtin_editor: Some(crate::editor::Keys::Notepad),
        ..settings()
    });
    // Nowhere to type: nothing happens.
    app.update(Msg::Paste("x".into()));
    act(&mut app, Action::Mail);
    app.update(Msg::Paste("bob@x.org\n".into()));
    assert_eq!(prompt(&app), "To: bob@x.org ");
    key(&mut app, KeyCode::Enter);
    answer(&mut app, "Hi");
    app.update(Msg::Paste("Hello\n".into()));
    let editor = app.editing.as_ref().expect("the built-in editor");
    assert!(editor.borrow().text().starts_with("Hello\n"));
    // From the clipboard: the editor's own, where the system's can't
    // be read (here, out of a window).
    let shift_end = KeyEvent::new(KeyCode::End, KeyModifiers::SHIFT);
    app.update(Msg::Key(KeyEvent::from(KeyCode::Up)));
    app.update(Msg::Key(shift_end));
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )));
    app.update(Msg::Key(KeyEvent::from(KeyCode::End)));
    app.update(Msg::PasteClipboard);
    let editor = app.editing.as_ref().expect("the built-in editor");
    assert!(
        editor.borrow().text().starts_with("HelloHello\n"),
        "{editor:?}"
    );
}

#[test]
fn the_builtin_editor_checks_spelling() {
    let words = std::path::PathBuf::from("/home/me/.config/mett/words");
    let mut app = app(compose::Settings {
        builtin_editor: Some(crate::editor::Keys::Vim),
        spell: crate::editor::spell::Settings {
            words: Some(words.clone()),
            ..Default::default()
        },
        ..settings()
    });
    act(&mut app, Action::Mail);
    answer(&mut app, "bob@x.org");
    let effects = answer(&mut app, "Hi");
    let [Effect::LoadSpeller(wanted)] = effects.as_slice() else {
        panic!("expected the dictionaries loaded, got {effects:?}");
    };
    // Loaded, they reach the editor.
    let speller = crate::editor::spell::Speller::load(wanted, None).map(std::sync::Arc::new);
    app.update(Msg::SpellerLoaded(wanted.clone(), speller));
    typed(&mut app, "iWrnog");
    key(&mut app, KeyCode::Esc);
    let bottom = |app: &App| app.editing.as_ref().map(|e| e.borrow().bottom(80));
    typed(&mut app, "z=");
    assert!(
        matches!(bottom(&app), Some(crate::editor::builtin::Bottom::Prompt { prompt, .. }) if prompt == "Wrnog: ")
    );
    key(&mut app, KeyCode::Esc);
    // `zg` adds the word to `spell_words`.
    let effects = typed_effects(&mut app, "zg");
    assert_eq!(
        effects,
        [Effect::AddWord {
            path: words,
            word: "Wrnog".into()
        }]
    );
    // Dictionaries for settings since changed are of no use.
    let stale = crate::editor::spell::Settings {
        langs: vec!["xx_XX".into()],
        ..wanted.clone()
    };
    app.update(Msg::SpellerLoaded(stale, Err("no dictionary".into())));
    assert_ne!(
        bottom(&app),
        Some(crate::editor::builtin::Bottom::Note("no dictionary".into()))
    );
    // `:set nospell` sets mett's own `spell_check`.
    let effects = typed_effects(&mut app, ":set nospell\n");
    assert_eq!(effects, [Effect::Command("set spell_check=no".into())]);
}

/// Types `text` (`\n` is Enter), and gives back the effects.
fn typed_effects(app: &mut App, text: &str) -> Vec<Effect> {
    text.chars()
        .flat_map(|c| match c {
            '\n' => key(app, KeyCode::Enter),
            c => key(app, KeyCode::Char(c)),
        })
        .collect()
}

#[test]
fn an_unmodified_message_from_the_builtin_editor_is_aborted() {
    let mut app = app(compose::Settings {
        builtin_editor: Some(crate::editor::Keys::Notepad),
        ..settings()
    });
    act(&mut app, Action::Mail);
    answer(&mut app, "bob@x.org");
    answer(&mut app, "Hi");
    key(&mut app, KeyCode::Esc);
    assert!(app.editing.is_none());
    assert_eq!(app.menu(), Menu::Index);
    assert_eq!(app.status, "Aborted unmodified message.");
}

#[test]
fn writes_and_sends_new_mail() {
    let mut app = app(settings());
    act(&mut app, Action::Mail);
    assert_eq!(prompt(&app), "To: ");
    answer(&mut app, "Alice <alice@x.org>, carol@x.org");
    assert_eq!(prompt(&app), "Subject: ");
    let effects = answer(&mut app, "Hello");
    assert_eq!(effects, [Effect::Edit("\n-- \nMe\n".into())]);
    assert_eq!(app.input, None);

    app.update(Msg::Edited(Ok("Hi!\n\n-- \nMe\n".into())));
    assert_eq!(app.menu(), Menu::Compose);
    let draft = &compose(&app).draft;
    assert_eq!(draft.to.len(), 2);
    assert_eq!(draft.subject, "Hello");

    let (id, request) = request(&act(&mut app, Action::SendMessage));
    let Request::Send(sent) = request else {
        panic!("expected Send, got {request:?}");
    };
    assert_eq!(sent.body, "Hi!\n\n-- \nMe\n");
    assert_eq!(
        sent.from,
        Some(Address::new("me@contoso.com").with_name("Me"))
    );
    assert!(sent.save_copy);
    assert_eq!(sent.origin, None);
    assert!(app.saving(), "waits for the message to be sent");
    // Nothing else can be done meanwhile.
    assert!(act(&mut app, Action::SendMessage).is_empty());

    respond(&mut app, id, Event::Sent(Ok(()), false));
    assert_eq!(app.screen(), &Screen::Index);
    assert_eq!(app.status, "Mail sent.");
    assert!(!app.saving());
}

#[test]
fn a_message_sent_without_its_copy_kept_is_sent() {
    let mut app = app(settings());
    written(&mut app, "text");
    let (id, _) = request(&act(&mut app, Action::SendMessage));
    respond(
        &mut app,
        id,
        Event::Sent(Err(Error::SentNotKept("no Sent folder".into())), false),
    );
    // It left the compose menu: sending it again would send it twice.
    assert_eq!(app.screen(), &Screen::Index);
    assert!(app.status.starts_with("Mail sent, but no copy was kept"));
    assert!(app.status.contains("no Sent folder"));
    assert!(app.status.is_error(), "said as a problem");
    assert!(!app.saving());
}

#[test]
fn a_failed_send_stays_in_the_compose_menu() {
    let mut app = app(settings());
    written(&mut app, "text");
    let (id, _) = request(&act(&mut app, Action::SendMessage));
    respond(
        &mut app,
        id,
        Event::Sent(Err(Error::Backend("bad recipient".into())), false),
    );
    assert_eq!(compose(&app).delivery, None);
    assert_eq!(app.status, "backend error: bad recipient");
}

#[test]
fn replies_quote_the_message_shown_in_the_pager() {
    let mut app = app(settings());
    let (fetch, _) = request(&act(&mut app, Action::DisplayMessage));
    respond(&mut app, fetch, Event::MessageFetched(Ok(original())));
    // The pager has the message: no need to fetch it again.
    act(&mut app, Action::Reply);
    assert_eq!(prompt(&app), "Include message in reply?");
    key(&mut app, KeyCode::Enter);
    assert_eq!(prompt(&app), "To: Bob <bob@x.org>");
    key(&mut app, KeyCode::Enter);
    assert_eq!(prompt(&app), "Subject: Re: Lunch");
    let effects = key(&mut app, KeyCode::Enter);
    assert_eq!(
        effects,
        [Effect::Edit("Bob wrote:\n> Noon?\n\n-- \nMe\n".into())]
    );
    app.update(Msg::Edited(Ok("Sure.\n".into())));
    let draft = &compose(&app).draft;
    assert_eq!(draft.origin, Some(DraftOrigin::Reply("m1".into())));

    // Replies are always copied to Sent Items: no question about it.
    app.settings.copy = Quad::AskNo;
    let effects = act(&mut app, Action::SendMessage);
    assert!(matches!(request(&effects).1, Request::Send(d) if d.save_copy));
    respond(
        &mut app,
        request(&effects).0,
        Event::Sent(Err(offline()), false),
    );

    let (id, request) = request(&act(&mut app, Action::PostponeMessage));
    assert!(matches!(request, Request::SaveDraft(d) if d.body == "Sure.\n"));
    respond(&mut app, id, Event::DraftSaved(Ok("d1".into()), false));
    assert_eq!(app.status, "Message postponed.");
    assert!(matches!(app.screen(), Screen::Pager(_)));
}

#[test]
fn replies_from_the_index_fetch_the_message_first() {
    let mut app = app(compose::Settings {
        include: Quad::No,
        fast_reply: true,
        ..settings()
    });
    let (fetch, _) = request(&act(&mut app, Action::GroupReply));
    let effects = respond(&mut app, fetch, Event::MessageFetched(Ok(original())));
    // fast_reply: straight to the editor, without quoting.
    assert_eq!(effects, [Effect::Edit("\n-- \nMe\n".into())]);
    assert_eq!(app.screen(), &Screen::Index, "the pager isn't opened");
}

#[test]
fn a_late_fetch_does_not_replace_a_newer_message() {
    let mut app = app(settings());
    let (fetch, _) = request(&act(&mut app, Action::Reply));
    act(&mut app, Action::Mail);
    let effects = respond(&mut app, fetch, Event::MessageFetched(Ok(original())));
    assert!(effects.is_empty());
    assert_eq!(prompt(&app), "To: ");
}

#[test]
fn moving_on_forgets_a_reply_still_being_fetched() {
    let mut app = app(settings());
    let (fetch, _) = request(&act(&mut app, Action::Reply));
    act(&mut app, Action::NextEntry);
    let effects = respond(&mut app, fetch, Event::MessageFetched(Ok(original())));
    assert!(effects.is_empty());
    assert!(app.input.is_none(), "no reply was started");
}

#[test]
fn asks_whether_to_reply_to_the_reply_to_address() {
    let mut app = app(settings());
    let mut message = original();
    message.reply_to = vec![Address::new("list@x.org")];
    app.begin(Kind::Reply, Some(message));
    assert_eq!(prompt(&app), "Reply to list@x.org?");
    key(&mut app, KeyCode::Char('n'));
    key(&mut app, KeyCode::Char('n'));
    assert_eq!(prompt(&app), "To: Bob <bob@x.org>");
}

#[test]
fn forwards_inline_with_mutts_subject() {
    let mut app = app(settings());
    app.begin(Kind::Forward, Some(original()));
    answer(&mut app, "carol@x.org");
    assert_eq!(prompt(&app), "Subject: [bob@x.org: Lunch]");
    let effects = key(&mut app, KeyCode::Enter);
    let [Effect::Edit(text)] = effects.as_slice() else {
        panic!("expected the editor, got {effects:?}");
    };
    assert!(text.starts_with("----- Forwarded message from Bob <bob@x.org> -----"));
    app.update(Msg::Edited(Ok(text.clone())));
    // Unchanged, but abort_unmodified gives up on it.
    assert_eq!(app.status, "Aborted unmodified message.");
    assert_eq!(app.screen(), &Screen::Index);
}

#[test]
fn bad_addresses_are_asked_again() {
    let mut app = app(settings());
    act(&mut app, Action::Mail);
    answer(&mut app, "bob");
    assert_eq!(prompt(&app), "bad address \"bob\". To: bob");
}

#[test]
fn giving_up_on_a_question_abandons_the_message() {
    let mut app = app(settings());
    act(&mut app, Action::Mail);
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.input, None);
    assert_eq!(app.screen(), &Screen::Index);
    // No recipients at all gives up too, as in mutt.
    act(&mut app, Action::Mail);
    answer(&mut app, "");
    assert_eq!(app.status, "No recipients were specified.");
    assert_eq!(app.input, None);
    // But a Cc is enough.
    app.settings.askcc = true;
    act(&mut app, Action::Mail);
    answer(&mut app, "");
    assert_eq!(prompt(&app), "Cc: ");
    answer(&mut app, "carol@x.org");
    assert_eq!(prompt(&app), "Subject: ");
}

#[test]
fn an_empty_subject_asks_whether_to_abort() {
    let mut app = app(settings());
    act(&mut app, Action::Mail);
    answer(&mut app, "bob@x.org");
    answer(&mut app, "");
    assert_eq!(prompt(&app), "No subject, abort?");
    key(&mut app, KeyCode::Char('n'));
    assert!(app.input.is_none());
}

#[test]
fn sending_without_a_subject_asks_again() {
    let mut app = app(settings());
    written(&mut app, "text");
    // Cleared in the compose menu.
    act(&mut app, Action::EditSubject);
    key(&mut app, KeyCode::Backspace);
    key(&mut app, KeyCode::Backspace);
    key(&mut app, KeyCode::Enter);
    assert_eq!(compose(&app).draft.subject, "");
    assert!(act(&mut app, Action::SendMessage).is_empty());
    assert_eq!(prompt(&app), "No subject, abort sending?");
    key(&mut app, KeyCode::Char('y'));
    assert_eq!(app.status, "No subject specified.");
    assert!(compose(&app).delivery.is_none(), "still in the menu");
    // Or it is sent anyway.
    act(&mut app, Action::SendMessage);
    let effects = key(&mut app, KeyCode::Char('n'));
    assert!(matches!(request(&effects).1, Request::Send(d) if d.subject.is_empty()));
}

#[test]
fn giving_up_on_a_reply_deletes_the_draft_a_failure_left() {
    let mut app = app(settings());
    written(&mut app, "text");
    // As for a reply, which goes through a server draft.
    if let Some(Screen::Compose(compose)) = app.screens.last_mut() {
        compose.draft.origin = Some(DraftOrigin::Reply("m1".into()));
    }
    let (id, _) = request(&act(&mut app, Action::SendMessage));
    respond(&mut app, id, Event::Sent(Err(timed_out()), false));
    act(&mut app, Action::Exit);
    let effects = key(&mut app, KeyCode::Char('n'));
    let (id, request) = request(&effects);
    assert!(matches!(request, Request::DiscardKept(d) if d.body == "text"));
    assert_eq!(app.status, "Mail not sent.");
    assert!(app.saving(), "quitting waits for it");
    respond(&mut app, id, Event::KeptDiscarded(Err(offline())));
    assert!(!app.saving());
    assert!(
        app.status
            .starts_with("A draft of the message is left in Drafts")
    ); // Reported on exit too: the status line goes with the terminal.
    assert_eq!(app.save_failures().len(), 1);
}

#[test]
fn a_draft_left_for_want_of_a_sign_in_is_deleted_on_retry() {
    let mut app = app(settings());
    written(&mut app, "text");
    if let Some(Screen::Compose(compose)) = app.screens.last_mut() {
        compose.draft.origin = Some(DraftOrigin::Reply("m1".into()));
    }
    let (id, _) = request(&act(&mut app, Action::SendMessage));
    respond(&mut app, id, Event::Sent(Err(timed_out()), false));
    act(&mut app, Action::Exit);
    let (id, _) = request(&key(&mut app, KeyCode::Char('n')));
    signed_out(&mut app);
    respond(&mut app, id, Event::KeptDiscarded(Err(expired())));
    assert_eq!(app.save_failures().len(), 1);
    let effects = act(&mut app, Action::Retry);
    let (id, request) = request(&effects);
    assert!(matches!(request, Request::DiscardKept(d) if d.body == "text"));
    respond(&mut app, id, Event::KeptDiscarded(Ok(())));
    assert!(app.save_failures().is_empty());
}

#[test]
fn retrying_while_quitting_waits_for_a_draft_being_deleted() {
    let mut app = app(settings());
    written(&mut app, "text");
    if let Some(Screen::Compose(compose)) = app.screens.last_mut() {
        compose.draft.origin = Some(DraftOrigin::Reply("m1".into()));
    }
    let (id, _) = request(&act(&mut app, Action::SendMessage));
    respond(&mut app, id, Event::Sent(Err(timed_out()), false));
    act(&mut app, Action::Exit);
    let (discard, _) = request(&key(&mut app, KeyCode::Char('n')));
    app.quit = true;
    signed_out(&mut app);
    // Retry before the delete reports back: it is retried once it has.
    act(&mut app, Action::Retry);
    let effects = respond(&mut app, discard, Event::KeptDiscarded(Err(expired())));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Request(_, Request::DiscardKept(_)))),
        "{effects:?}"
    );
}

#[test]
fn leaving_the_compose_menu_asks_whether_to_postpone() {
    let mut app = app(settings());
    written(&mut app, "text");
    act(&mut app, Action::EditSubject);
    key(&mut app, KeyCode::Char('!'));
    key(&mut app, KeyCode::Enter);
    assert_eq!(compose(&app).draft.subject, "Hi!");

    act(&mut app, Action::Exit);
    assert_eq!(prompt(&app), "Postpone this message?");
    key(&mut app, KeyCode::Char('n'));
    assert_eq!(app.status, "Mail not sent.");
    assert_eq!(app.screen(), &Screen::Index);
}

#[test]
fn editing_again_keeps_the_text_if_the_editor_fails() {
    let mut app = app(settings());
    written(&mut app, "first");
    assert_eq!(
        act(&mut app, Action::EditMessage),
        [Effect::Edit("first".into())]
    );
    app.update(Msg::Edited(Err("Error running \"vi\"".into())));
    assert_eq!(compose(&app).draft.body, "first");
    act(&mut app, Action::EditMessage);
    app.update(Msg::Edited(Ok("second".into())));
    assert_eq!(compose(&app).draft.body, "second");
}

#[test]
fn a_pager_still_loading_under_the_compose_menu_finishes() {
    let mut app = app(compose::Settings {
        fast_reply: true,
        include: Quad::No,
        ..settings()
    });
    let (fetch, _) = request(&act(&mut app, Action::DisplayMessage));
    // Reply before the pager's message arrives: it is fetched again.
    let (reply_fetch, _) = request(&act(&mut app, Action::Reply));
    respond(&mut app, reply_fetch, Event::MessageFetched(Ok(original())));
    app.update(Msg::Edited(Ok("text".into())));
    assert_eq!(app.menu(), Menu::Compose);
    respond(&mut app, fetch, Event::MessageFetched(Ok(original())));
    let Screen::Pager(pager) = &app.screens[1] else {
        panic!("the pager is under the compose menu");
    };
    assert!(pager.message.is_some(), "and it has its message");
}

#[test]
fn sending_from_sent_items_lists_it_again() {
    let mut app = app(settings());
    in_sent_items(&mut app);
    written(&mut app, "text");
    let (id, _) = request(&act(&mut app, Action::SendMessage));
    let effects = respond(&mut app, id, Event::Sent(Ok(()), false));
    assert!(
        matches!(
            effects.as_slice(),
            [
                Effect::Later(_, crate::app::Timer::RelistSentItems),
                Effect::Request(_, Request::ListMessages { .. }),
            ]
        ),
        "{effects:?}"
    );
    // And again when the timer goes off.
    let effects = app.update(Msg::Timer(crate::app::Timer::RelistSentItems));
    assert!(matches!(request(&effects).1, Request::ListMessages { .. }));
    assert!(app.messages.is_empty());
}

#[test]
fn the_later_sent_items_relist_keeps_the_selection() {
    let mut app = app(settings());
    in_sent_items(&mut app);
    app.messages = sent_items(&["a", "b", "c"]);
    app.selected = 1;
    let relisted = |app: &mut App, ids: &[&str]| {
        let effects = app.update(Msg::Timer(crate::app::Timer::RelistSentItems));
        let (id, _) = request(&effects);
        listed(app, id, sent_items(ids), None);
    };
    // The new copy comes first: still on b.
    relisted(&mut app, &["new", "a", "b", "c"]);
    assert_eq!(app.messages[app.selected].id.as_str(), "b");
    // A second refresh before the first is listed keeps the place.
    let first = app.update(Msg::Timer(crate::app::Timer::RelistSentItems));
    assert!(!first.is_empty());
    relisted(&mut app, &["new", "a", "b", "c"]);
    assert_eq!(app.messages[app.selected].id.as_str(), "b");
    // b is gone: the message now where it was.
    relisted(&mut app, &["new", "a", "c"]);
    assert_eq!(app.messages[app.selected].id.as_str(), "c");
}

#[test]
fn sending_from_sent_items_keeps_the_selection() {
    let mut app = app(settings());
    in_sent_items(&mut app);
    app.messages = sent_items(&["a", "b", "c"]);
    app.selected = 1;
    written(&mut app, "text");
    let (id, _) = request(&act(&mut app, Action::SendMessage));
    let effects = respond(&mut app, id, Event::Sent(Ok(()), false));
    let list =
        request_for(&effects, |r| matches!(r, Request::ListMessages { .. })).expect("listed again");
    listed(&mut app, list, sent_items(&["new", "a", "b", "c"]), None);
    assert_eq!(app.messages[app.selected].id.as_str(), "b");
}

#[test]
fn a_send_that_needed_a_sign_in_is_retried() {
    let mut app = app(settings());
    written(&mut app, "text");
    let (id, sent) = request(&act(&mut app, Action::SendMessage));
    signed_out(&mut app);
    respond(&mut app, id, Event::Sent(Err(expired()), false));
    let (_, again) = request(&act(&mut app, Action::Retry));
    assert_eq!(again, sent);
}

#[test]
fn sent_items_is_listed_again_once_the_pager_closes() {
    let mut app = app(settings());
    in_sent_items(&mut app);
    let (fetch, _) = request(&act(&mut app, Action::DisplayMessage));
    respond(&mut app, fetch, Event::MessageFetched(Ok(original())));
    app.settings.include = Quad::No;
    app.settings.fast_reply = true;
    act(&mut app, Action::Reply);
    app.update(Msg::Edited(Ok("text".into())));
    let (id, _) = request(&act(&mut app, Action::SendMessage));
    let effects = respond(&mut app, id, Event::Sent(Ok(()), false));
    assert!(
        matches!(effects.as_slice(), [Effect::Later(..)]),
        "no listing under the pager: {effects:?}"
    );
    assert_eq!(app.messages.len(), 1);
    let effects = act(&mut app, Action::Exit);
    assert!(matches!(request(&effects).1, Request::ListMessages { .. }));
}

#[test]
fn a_reply_whose_message_needed_a_sign_in_starts_again_on_retry() {
    let mut app = app(settings());
    let (fetch, _) = request(&act(&mut app, Action::Reply));
    signed_out(&mut app);
    respond(&mut app, fetch, Event::MessageFetched(Err(expired())));
    let (_, again) = request(&act(&mut app, Action::Retry));
    assert_eq!(again, Request::FetchMessage("m1".into()));
}

#[test]
fn a_reply_retried_after_the_index_moved_is_not_to_another_message() {
    let mut app = app(settings());
    let (fetch, _) = request(&act(&mut app, Action::Reply));
    // A background listing puts another message where m1 was.
    let other = mock::message("m0", "in", "Newer", DateTime::UNIX_EPOCH);
    app.messages.insert(0, other.summary);
    signed_out(&mut app);
    respond(&mut app, fetch, Event::MessageFetched(Err(expired())));
    let effects = act(&mut app, Action::Retry);
    assert!(
        !effects.iter().any(
            |e| matches!(e, Effect::Request(_, Request::FetchMessage(id)) if id.as_str() == "m0")
        ),
        "{effects:?}"
    );
}

#[test]
fn a_lost_send_response_may_have_sent_the_message() {
    let mut app = app(settings());
    written(&mut app, "text");
    let (id, _) = request(&act(&mut app, Action::SendMessage));
    respond(&mut app, id, Event::Sent(Err(timed_out()), false));
    assert!(
        app.status.starts_with("The message may have been sent"),
        "{}",
        app.status
    );
}

#[test]
fn a_message_that_failed_before_sending_was_not_sent() {
    let mut app = app(settings());
    written(&mut app, "text");
    let (id, _) = request(&act(&mut app, Action::SendMessage));
    respond(
        &mut app,
        id,
        Event::Sent(
            Err(Error::NotSent("network error: timed out".into())),
            false,
        ),
    );
    assert_eq!(
        app.status,
        "The message wasn't sent: network error: timed out."
    );
}

#[test]
fn each_composed_message_is_its_own_attempt() {
    let mut app = app(settings());
    let mut attempts = Vec::new();
    for _ in 0..2 {
        written(&mut app, "text");
        let (id, Request::Send(sent)) = request(&act(&mut app, Action::SendMessage)) else {
            panic!("expected Send");
        };
        respond(&mut app, id, Event::Sent(Err(timed_out()), false));
        // Sending the same message again is the same attempt.
        let (id, Request::Send(again)) = request(&act(&mut app, Action::SendMessage)) else {
            panic!("expected Send");
        };
        assert_eq!(again.attempt, sent.attempt);
        respond(&mut app, id, Event::Sent(Ok(()), false));
        attempts.push(sent.attempt);
    }
    assert_ne!(attempts[0], attempts[1]);
}

#[test]
fn sending_without_a_copy_leaves_sent_items_alone() {
    let mut app = app(compose::Settings {
        copy: Quad::No,
        ..settings()
    });
    in_sent_items(&mut app);
    written(&mut app, "text");
    let (id, _) = request(&act(&mut app, Action::SendMessage));
    let effects = respond(&mut app, id, Event::Sent(Ok(()), false));
    assert!(effects.is_empty());
    assert_eq!(app.messages.len(), 1);
}

#[test]
fn a_left_server_draft_is_sent_with_its_copy() {
    let mut app = app(compose::Settings {
        copy: Quad::No,
        ..settings()
    });
    written(&mut app, "text");
    // A failed attempt that kept nothing leaves `copy` as it is…
    let (id, sent) = request(&act(&mut app, Action::SendMessage));
    assert!(matches!(sent, Request::Send(d) if !d.save_copy));
    respond(&mut app, id, Event::Sent(Err(expired()), false));
    let (id, sent) = request(&act(&mut app, Action::SendMessage));
    assert!(matches!(sent, Request::Send(d) if !d.save_copy));
    // …but one that kept a server draft sends that, which Graph keeps
    // a copy of.
    respond(&mut app, id, Event::Sent(Err(timed_out()), true));
    let (_, sent) = request(&act(&mut app, Action::SendMessage));
    assert!(matches!(sent, Request::Send(d) if d.save_copy));
}

#[test]
fn retry_resends_a_delivery_and_failed_saves_together() {
    let mut app = app(settings());
    // Reading the message fails to mark it read…
    let (fetch, _) = request(&act(&mut app, Action::DisplayMessage));
    let effects = respond(&mut app, fetch, Event::MessageFetched(Ok(original())));
    let (flags, _) = request(&effects);
    respond(&mut app, flags, Event::FlagsUpdated(Err(expired())));
    // …and replying fails to send, for the same reason.
    app.settings.include = Quad::No;
    app.settings.fast_reply = true;
    act(&mut app, Action::Reply);
    app.update(Msg::Edited(Ok("text".into())));
    let (send, _) = request(&act(&mut app, Action::SendMessage));
    signed_out(&mut app);
    respond(&mut app, send, Event::Sent(Err(expired()), false));
    let effects = act(&mut app, Action::Retry);
    let requests: Vec<_> = effects
        .iter()
        .map(|e| match e {
            Effect::Request(_, Request::Send(_)) => "send",
            Effect::Request(_, Request::UpdateFlags(..)) => "flags",
            _ => "other",
        })
        .collect();
    assert_eq!(requests, ["send", "flags"]);
}

#[test]
fn a_lost_postpone_response_may_have_saved_the_draft() {
    let mut app = app(settings());
    written(&mut app, "text");
    let (id, _) = request(&act(&mut app, Action::PostponeMessage));
    respond(&mut app, id, Event::DraftSaved(Err(timed_out()), false));
    assert!(
        app.status.starts_with("The message may have been saved"),
        "{}",
        app.status
    );
}

#[test]
fn a_lost_send_without_a_copy_says_sent_items_cannot_tell() {
    let mut app = app(compose::Settings {
        copy: Quad::No,
        ..settings()
    });
    written(&mut app, "text");
    let (id, _) = request(&act(&mut app, Action::SendMessage));
    respond(&mut app, id, Event::Sent(Err(timed_out()), false));
    assert!(app.status.contains("without a copy"), "{}", app.status);
}

#[test]
fn a_reply_that_needed_a_sign_in_is_retried_from_the_pager() {
    let mut app = app(settings());
    let (pager_fetch, _) = request(&act(&mut app, Action::DisplayMessage));
    let (reply_fetch, _) = request(&act(&mut app, Action::Reply));
    respond(&mut app, pager_fetch, Event::MessageFetched(Ok(original())));
    signed_out(&mut app);
    respond(&mut app, reply_fetch, Event::MessageFetched(Err(expired())));
    act(&mut app, Action::Retry);
    // The pager has the message now: the reply starts from it.
    assert_eq!(prompt(&app), "Include message in reply?");
}

#[test]
fn a_relist_due_while_composing_happens_on_leaving() {
    let mut app = app(compose::Settings {
        postpone: Quad::No,
        ..settings()
    });
    in_sent_items(&mut app);
    written(&mut app, "text");
    // The timer from an earlier send goes off in the compose menu.
    assert!(
        app.update(Msg::Timer(crate::app::Timer::RelistSentItems))
            .is_empty()
    );
    let effects = act(&mut app, Action::Exit);
    assert!(matches!(request(&effects).1, Request::ListMessages { .. }));
}

#[test]
fn a_reply_waits_for_the_users_own_addresses() {
    let (mut app, list, addresses) = opening(compose::Settings::default());
    // A message the user sent from an alias, to Bob.
    let mut mine = original();
    mine.summary.from = Some(Address::new("alias@contoso.com"));
    mine.summary.to = vec![Address::new("bob@x.org")];
    listed(&mut app, list, vec![mine.summary.clone()], None);
    let (fetch, _) = request(&act(&mut app, Action::Reply));
    respond(&mut app, fetch, Event::MessageFetched(Ok(mine)));
    assert!(app.input.is_none(), "no recipients chosen yet");
    respond(
        &mut app,
        addresses,
        Event::OwnAddresses(Ok(vec!["alias@contoso.com".into()])),
    );
    assert!(app.input.is_some(), "the reply goes on");
    // Include the message (the default), then To.
    key(&mut app, KeyCode::Enter);
    let Some(Input::Line { editor, .. }) = &app.input else {
        panic!("expected the To question, got {:?}", app.input);
    };
    assert_eq!(editor.text(), "bob@x.org");
}

#[test]
fn a_failed_address_lookup_is_tried_again_by_a_reply() {
    let (mut app, list, addresses) = opening(compose::Settings::default());
    respond(
        &mut app,
        addresses,
        Event::OwnAddresses(Err(Error::Network("busy".into()))),
    );
    let mut mine = original();
    mine.summary.from = Some(Address::new("alias@contoso.com"));
    mine.summary.to = vec![Address::new("bob@x.org")];
    listed(&mut app, list, vec![mine.summary.clone()], None);
    let (fetch, _) = request(&act(&mut app, Action::Reply));
    let effects = respond(&mut app, fetch, Event::MessageFetched(Ok(mine)));
    let (again, asked) = request(&effects);
    assert!(matches!(asked, Request::OwnAddresses), "asked again");
    assert!(app.input.is_none(), "the reply waits");
    respond(
        &mut app,
        again,
        Event::OwnAddresses(Ok(vec!["alias@contoso.com".into()])),
    );
    key(&mut app, KeyCode::Enter);
    let Some(Input::Line { editor, .. }) = &app.input else {
        panic!("expected the To question, got {:?}", app.input);
    };
    assert_eq!(editor.text(), "bob@x.org");
}

#[test]
fn a_reply_given_up_with_its_sign_in_is_not_restarted_later() {
    let mut app = app(settings());
    let (fetch, _) = request(&act(&mut app, Action::Reply));
    signed_out(&mut app);
    respond(&mut app, fetch, Event::MessageFetched(Err(expired())));
    // The user closes the failure window.
    act(&mut app, Action::Exit);
    // Another sign-in fails later, and the user retries that one.
    signed_out(&mut app);
    let effects = act(&mut app, Action::Retry);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::Request(_, Request::FetchMessage(_)))),
        "{effects:?}"
    );
}

#[test]
fn writing_during_a_sent_items_refresh_keeps_the_selection() {
    let mut app = app(settings());
    in_sent_items(&mut app);
    app.messages = sent_items(&["a", "b", "c"]);
    app.selected = 1;
    let effects = app.update(Msg::Timer(crate::app::Timer::RelistSentItems));
    let (list, _) = request(&effects);
    // The user starts a new message before the listing is back.
    act(&mut app, Action::Mail);
    listed(&mut app, list, sent_items(&["new", "a", "b", "c"]), None);
    assert_eq!(app.messages[app.selected].id.as_str(), "b");
}
