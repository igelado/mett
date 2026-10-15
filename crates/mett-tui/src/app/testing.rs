//! What the app's tests share: opening a folder, answering requests and
//! typing keys.

use mett_core::{MessagePage, PageCursor, WellKnownFolder, mock};
use ratatui::crossterm::event::KeyCode;

use super::*;

/// The Inbox, as the app opens it by default.
pub(super) fn inbox() -> FolderPath {
    FolderPath::WellKnown(WellKnownFolder::Inbox, vec![])
}

/// An 80-column, `height`-row app with `settings` that has opened the
/// Inbox; returns the app and the request listing it.
pub(super) fn opening(settings: Settings, height: u16) -> (App, RequestId) {
    let (mut app, effects) = App::open(80, height, inbox(), settings);
    let open = first_request(&effects);
    let list = folder_opened(&mut app, open, mock::folder("in", "Inbox", None));
    (app, list)
}

/// An app [`opening`] the Inbox that has listed `messages`, with `next`
/// as the cursor of a page to come; returns the app and that page's
/// request.
pub(super) fn opened(
    settings: Settings,
    height: u16,
    messages: Vec<MessageSummary>,
    next: Option<&str>,
) -> (App, Option<RequestId>) {
    let (mut app, list) = opening(settings, height);
    let more = match next {
        Some(next) => Some(page(&mut app, list, messages, next)),
        None => {
            listed(&mut app, list, messages, None);
            None
        }
    };
    (app, more)
}

/// Answers the opening of a folder with `folder`; returns the request
/// that lists it.
pub(super) fn folder_opened(app: &mut App, id: RequestId, folder: Folder) -> RequestId {
    first_request(&respond(app, id, Event::FolderOpened(Ok(folder))))
}

/// Answers a listing with `messages`, and `next` as the cursor of a page
/// to come; returns the effects.
pub(super) fn listed(
    app: &mut App,
    id: RequestId,
    messages: Vec<MessageSummary>,
    next: Option<&str>,
) -> Vec<Effect> {
    let page = MessagePage {
        messages,
        next: next.map(PageCursor::new),
    };
    respond(app, id, Event::MessagesListed(Ok(page)))
}

/// Answers a listing with `messages`, and `next` as the cursor of a page
/// to come; returns that page's request.
pub(super) fn page(
    app: &mut App,
    id: RequestId,
    messages: Vec<MessageSummary>,
    next: &str,
) -> RequestId {
    first_request(&listed(app, id, messages, Some(next)))
}

pub(super) fn respond(app: &mut App, id: RequestId, event: Event) -> Vec<Effect> {
    app.update(Msg::Response(Response { id, event }))
}

pub(super) fn act(app: &mut App, action: Action) -> Vec<Effect> {
    app.update(Msg::Action(action))
}

pub(super) fn key(app: &mut App, code: KeyCode) -> Vec<Effect> {
    app.update(Msg::Key(KeyEvent::from(code)))
}

/// Types `text`.
pub(super) fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        key(app, KeyCode::Char(c));
    }
}

/// Types `text` and presses Enter.
pub(super) fn answer(app: &mut App, text: &str) -> Vec<Effect> {
    typed(app, text);
    key(app, KeyCode::Enter)
}

/// The question on the message line, with what is typed so far.
pub(super) fn prompt(app: &App) -> String {
    match &app.input {
        Some(Input::Line { prompt, editor, .. }) => format!("{prompt}{}", editor.text()),
        Some(Input::YesNo { prompt, .. }) => prompt.clone(),
        None => String::new(),
    }
}

/// The request that comes first among `effects`.
pub(super) fn first_request(effects: &[Effect]) -> RequestId {
    let Some(Effect::Request(id, _)) = effects.first() else {
        panic!("expected a request, got {effects:?}");
    };
    *id
}

/// The single request among `effects`.
pub(super) fn sent(effects: &[Effect]) -> (RequestId, Request) {
    match effects {
        [Effect::Request(id, request)] => (*id, request.clone()),
        _ => panic!("expected one request, got {effects:?}"),
    }
}

/// The requests among `effects`.
pub(super) fn requests(effects: &[Effect]) -> Vec<(RequestId, Request)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Request(id, request) => Some((*id, request.clone())),
            _ => None,
        })
        .collect()
}

pub(super) fn ids(app: &App) -> Vec<&str> {
    app.messages.iter().map(|m| m.id.as_str()).collect()
}

pub(super) fn pager(app: &App) -> &Pager {
    let Screen::Pager(pager) = app.screen() else {
        panic!("not in the pager");
    };
    pager
}

/// Settings that sort the index by `sort`, given as `sort` is set.
pub(super) fn sorted(sort: &str) -> Settings {
    Settings {
        ordering: Ordering {
            sort: crate::sort::Sort::parse(sort, true).unwrap(),
            ..Ordering::default()
        },
        ..Settings::default()
    }
}

/// A `:` command that changed nothing but the settings, to `settings`.
pub(super) fn configured(settings: Settings) -> Msg {
    Msg::Configured(Reconfigured {
        outcome: Ok(Vec::new()),
        settings: Ok(settings),
        key_commands: Vec::new(),
        pushed: Vec::new(),
    })
}

/// The end of Alice's sign-in.
pub(super) fn finished(outcome: Result<(), String>) -> SignInEvent {
    SignInEvent::Finished {
        account: AccountId::new("alice@contoso.com"),
        outcome,
    }
}

/// Alice is asked to sign in in her browser.
pub(super) fn browser_prompt() -> SignInEvent {
    SignInEvent::Prompt {
        account: AccountId::new("alice@contoso.com"),
        prompt: SignInPrompt::OpenBrowser {
            url: "https://login.example/authorize".parse().unwrap(),
        },
    }
}

/// Alice's sign-in failed as it expired.
pub(super) fn signed_out(app: &mut App) {
    app.update(Msg::SignIn(finished(Err("expired".into()))));
}
