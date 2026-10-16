//! What the app's tests share: opening a folder, answering requests and
//! typing keys.

use mett_core::{MessagePage, PageCursor, WellKnownFolder, mock};
use ratatui::crossterm::event::KeyCode;

use super::*;

/// The Inbox, as the app opens it by default.
pub(crate) fn inbox() -> FolderPath {
    FolderPath::WellKnown(WellKnownFolder::Inbox, vec![])
}

/// An 80-column, `height`-row app with `settings` that has opened the
/// Inbox; returns the app and the request listing it.
pub(crate) fn opening(settings: Settings, height: u16) -> (App, RequestId) {
    let (mut app, effects) = App::open(80, height, inbox(), settings);
    let open = first_request(&effects);
    let list = folder_opened(&mut app, open, mock::folder("in", "Inbox", None));
    (app, list)
}

/// An app [`opening`] the Inbox that has listed `messages`, with `next`
/// as the cursor of a page to come; returns the app and that page's
/// request.
pub(crate) fn opened(
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
pub(crate) fn folder_opened(app: &mut App, id: RequestId, folder: Folder) -> RequestId {
    first_request(&respond(app, id, Event::FolderOpened(Ok(folder))))
}

/// Answers a listing with `messages`, and `next` as the cursor of a page
/// to come; returns the effects.
pub(crate) fn listed(
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
pub(crate) fn page(
    app: &mut App,
    id: RequestId,
    messages: Vec<MessageSummary>,
    next: &str,
) -> RequestId {
    first_request(&listed(app, id, messages, Some(next)))
}

pub(crate) fn respond(app: &mut App, id: RequestId, event: Event) -> Vec<Effect> {
    app.update(Msg::Response(Response { id, event }))
}

pub(crate) fn act(app: &mut App, action: Action) -> Vec<Effect> {
    app.update(Msg::Action(action))
}

pub(crate) fn key(app: &mut App, code: KeyCode) -> Vec<Effect> {
    app.update(Msg::Key(KeyEvent::from(code)))
}

/// Types `text`.
pub(crate) fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        key(app, KeyCode::Char(c));
    }
}

/// Types `text` and presses Enter.
pub(crate) fn answer(app: &mut App, text: &str) -> Vec<Effect> {
    typed(app, text);
    key(app, KeyCode::Enter)
}

/// The question on the message line, with what is typed so far.
pub(crate) fn prompt(app: &App) -> String {
    match &app.input {
        Some(Input::Line { prompt, editor, .. }) => format!("{prompt}{}", editor.text()),
        Some(Input::YesNo { prompt, .. }) => prompt.clone(),
        None => String::new(),
    }
}

/// The request that comes first among `effects`.
pub(crate) fn first_request(effects: &[Effect]) -> RequestId {
    let Some(Effect::Request(id, _)) = effects.first() else {
        panic!("expected a request, got {effects:?}");
    };
    *id
}

/// The single request among `effects`.
pub(crate) fn sent(effects: &[Effect]) -> (RequestId, Request) {
    match effects {
        [Effect::Request(id, request)] => (*id, request.clone()),
        _ => panic!("expected one request, got {effects:?}"),
    }
}

/// The requests among `effects`.
pub(crate) fn requests(effects: &[Effect]) -> Vec<(RequestId, Request)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Request(id, request) => Some((*id, request.clone())),
            _ => None,
        })
        .collect()
}

pub(crate) fn ids(app: &App) -> Vec<&str> {
    app.messages.iter().map(|m| m.id.as_str()).collect()
}

pub(crate) fn pager(app: &App) -> &Pager {
    let Screen::Pager(pager) = app.screen() else {
        panic!("not in the pager");
    };
    pager
}

/// Settings that sort the index by `sort`, given as `sort` is set.
pub(crate) fn sorted(sort: &str) -> Settings {
    Settings {
        ordering: Ordering {
            sort: crate::sort::Sort::parse(sort, true).unwrap(),
            ..Ordering::default()
        },
        ..Settings::default()
    }
}

/// A `:` command that changed nothing but the settings, to `settings`.
pub(crate) fn configured(settings: Settings) -> Msg {
    Msg::Configured(Reconfigured {
        outcome: Ok(Vec::new()),
        settings: Ok(settings),
        key_commands: Vec::new(),
        pushed: Vec::new(),
    })
}

/// The end of Alice's sign-in.
pub(crate) fn finished(outcome: Result<(), String>) -> SignInEvent {
    SignInEvent::Finished {
        account: AccountId::new("alice@contoso.com"),
        outcome,
    }
}

/// Alice is asked to sign in in her browser.
pub(crate) fn browser_prompt() -> SignInEvent {
    SignInEvent::Prompt {
        account: AccountId::new("alice@contoso.com"),
        prompt: SignInPrompt::OpenBrowser {
            url: "https://login.example/authorize".parse().unwrap(),
        },
    }
}

/// Alice's sign-in failed as it expired.
pub(crate) fn signed_out(app: &mut App) {
    signed_out_as(app, "expired");
}

/// Alice's sign-in failed with `error`.
pub(crate) fn signed_out_as(app: &mut App, error: &str) {
    app.update(Msg::SignIn(finished(Err(error.into()))));
}

/// The network is down.
pub(crate) fn offline() -> Error {
    Error::Network("offline".into())
}

/// The sign-in expired.
pub(crate) fn expired() -> Error {
    Error::Auth("expired".into())
}

/// The first request among `effects` that `wanted` picks.
pub(crate) fn request_for(
    effects: &[Effect],
    wanted: impl Fn(&Request) -> bool,
) -> Option<RequestId> {
    effects.iter().find_map(|e| match e {
        Effect::Request(id, request) if wanted(request) => Some(*id),
        _ => None,
    })
}
