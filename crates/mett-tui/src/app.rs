//! The application model and its `update` function: the Elm Architecture
//! (`docs/DESIGN.md` §5.2).
//!
//! [`App`] holds all UI state. Only [`App::update`] changes it, in response
//! to a [`Msg`]; side effects come back as [`Effect`]s for the main loop to
//! carry out. This keeps the UI logic testable without a terminal.

use mett_core::{Error, FlagsUpdate, Folder, Message, MessageId, MessageSummary, Page};

use crate::action::Action;
use crate::keymap::Menu;
use crate::worker::{Event, Request, RequestId, Response};

/// How many messages to request per page.
const PAGE_SIZE: u32 = 100;

/// Input to [`App::update`].
// Messages are handled one at a time, so their size doesn't matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Msg {
    /// The user triggered an action.
    Action(Action),
    /// The worker answered a request.
    Response(Response),
    /// The terminal now has `height` rows.
    Resize {
        /// Total terminal height, in rows.
        height: u16,
    },
}

/// A side effect for the main loop to carry out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Send `Request` to the worker.
    Request(RequestId, Request),
}

/// A screen on the navigation stack (`docs/DESIGN.md` §5.6).
// The stack holds at most a couple of screens.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Screen {
    /// The message index.
    Index,
    /// The pager, showing one message.
    Pager(Pager),
}

/// Pager state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pager {
    /// The message being shown.
    pub id: MessageId,
    /// The message, once fetched.
    pub message: Option<Message>,
    /// The request fetching it.
    pub request: RequestId,
    /// The first visible line.
    pub scroll: usize,
}

/// The whole UI state.
#[derive(Debug)]
pub struct App {
    /// The navigation stack; the bottom is always [`Screen::Index`].
    pub screens: Vec<Screen>,
    /// The open folder, once known.
    pub folder: Option<Folder>,
    /// The index, newest first.
    pub messages: Vec<MessageSummary>,
    /// The selected index entry.
    pub selected: usize,
    /// The message line at the bottom of the screen.
    pub status: String,
    /// Set when the user quits.
    pub quit: bool,
    /// Rows available for the index or pager body.
    pub body_height: usize,
    /// The request whose messages the index is waiting for, if any.
    listing: Option<RequestId>,
    next_request: u64,
}

/// Rows used by the help line, the status bar and the message line.
const CHROME_ROWS: usize = 3;

impl App {
    /// A new app, and the effects that start loading the Inbox.
    pub fn new(height: u16) -> (Self, Vec<Effect>) {
        let mut app = Self {
            screens: vec![Screen::Index],
            folder: None,
            messages: Vec::new(),
            selected: 0,
            status: "Opening Inbox...".into(),
            quit: false,
            body_height: body_height(height),
            listing: None,
            next_request: 0,
        };
        let effect = app.request(Request::OpenInbox);
        (app, vec![effect])
    }

    /// The menu for the current screen, which selects the key bindings.
    pub fn menu(&self) -> Menu {
        match self.screen() {
            Screen::Index => Menu::Index,
            Screen::Pager(_) => Menu::Pager,
        }
    }

    /// The current screen.
    pub fn screen(&self) -> &Screen {
        self.screens
            .last()
            .expect("the index is always on the stack")
    }

    /// Applies `msg` and returns the side effects to carry out.
    pub fn update(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Action(action) => self.on_action(action),
            Msg::Response(response) => self.on_response(response),
            Msg::Resize { height } => {
                self.body_height = body_height(height);
                Vec::new()
            }
        }
    }

    fn request(&mut self, request: Request) -> Effect {
        self.next_request += 1;
        Effect::Request(RequestId(self.next_request), request)
    }

    fn on_action(&mut self, action: Action) -> Vec<Effect> {
        match self.menu() {
            Menu::Index => self.on_index_action(action),
            Menu::Pager => self.on_pager_action(action),
        }
    }

    fn on_index_action(&mut self, action: Action) -> Vec<Effect> {
        let last = self.messages.len().saturating_sub(1);
        let page = self.body_height.max(1);
        match action {
            Action::Quit => self.quit = true,
            Action::NextEntry if self.selected < last => self.selected += 1,
            Action::NextEntry => self.status = "You are on the last message.".into(),
            Action::PreviousEntry if self.selected > 0 => self.selected -= 1,
            Action::PreviousEntry => self.status = "You are on the first message.".into(),
            Action::FirstEntry => self.selected = 0,
            Action::LastEntry => self.selected = last,
            Action::NextPage => self.selected = (self.selected + page).min(last),
            Action::PreviousPage => self.selected = self.selected.saturating_sub(page),
            Action::DisplayMessage => return self.open_selected(),
            Action::Exit | Action::NextLine | Action::PreviousLine => {}
        }
        Vec::new()
    }

    fn on_pager_action(&mut self, action: Action) -> Vec<Effect> {
        let page = self.body_height.max(1);
        let Some(Screen::Pager(pager)) = self.screens.last_mut() else {
            return Vec::new();
        };
        match action {
            Action::Quit => self.quit = true,
            Action::Exit => {
                self.screens.pop();
            }
            // As in mutt, j and k in the pager show the next or previous
            // message.
            Action::NextEntry | Action::PreviousEntry => {
                let at_end = if action == Action::NextEntry {
                    self.selected + 1 >= self.messages.len()
                } else {
                    self.selected == 0
                };
                if at_end {
                    self.status = if action == Action::NextEntry {
                        "You are on the last message.".into()
                    } else {
                        "You are on the first message.".into()
                    };
                    return Vec::new();
                }
                if action == Action::NextEntry {
                    self.selected += 1;
                } else {
                    self.selected -= 1;
                }
                self.screens.pop();
                return self.open_selected();
            }
            Action::NextLine => pager.scroll += 1,
            Action::PreviousLine => pager.scroll = pager.scroll.saturating_sub(1),
            Action::NextPage => pager.scroll += page,
            Action::PreviousPage => pager.scroll = pager.scroll.saturating_sub(page),
            Action::FirstEntry | Action::LastEntry | Action::DisplayMessage => {}
        }
        Vec::new()
    }

    /// Opens the pager on the selected message, marking it read.
    fn open_selected(&mut self) -> Vec<Effect> {
        let Some(summary) = self.messages.get(self.selected) else {
            self.status = "No messages.".into();
            return Vec::new();
        };
        let id = summary.id.clone();
        let fetch = self.request(Request::FetchMessage(id.clone()));
        self.screens.push(Screen::Pager(Pager {
            id: id.clone(),
            message: None,
            request: effect_id(&fetch),
            scroll: 0,
        }));

        let mut effects = vec![fetch];
        let summary = &mut self.messages[self.selected];
        if !summary.flags.read {
            // Show it as read straight away, as mutt does.
            summary.flags.read = true;
            let update = FlagsUpdate {
                read: Some(true),
                flagged: None,
            };
            effects.push(self.request(Request::UpdateFlags(id, update)));
        }
        effects
    }

    fn on_response(&mut self, Response { id, event }: Response) -> Vec<Effect> {
        match event {
            Event::InboxOpened(Ok(folder)) => {
                let list = self.request(Request::ListMessages {
                    folder: folder.id.clone(),
                    page: Page::first(PAGE_SIZE),
                });
                self.listing = Some(effect_id(&list));
                self.status = format!("Loading {}...", folder.name);
                self.folder = Some(folder);
                return vec![list];
            }
            Event::MessagesListed(result) if self.listing == Some(id) => match result {
                Ok(page) => {
                    self.messages.extend(page.messages);
                    if let Some(cursor) = page.next
                        && let Some(folder) = &self.folder
                    {
                        // Keep loading until the whole folder is listed.
                        let more = self.request(Request::ListMessages {
                            folder: folder.id.clone(),
                            page: Page::after(cursor, PAGE_SIZE),
                        });
                        self.listing = Some(effect_id(&more));
                        return vec![more];
                    }
                    self.listing = None;
                    self.status.clear();
                }
                Err(e) => {
                    self.listing = None;
                    self.report(e);
                }
            },
            Event::MessageFetched(result) => {
                // Only the pager that asked for it cares; anything else is a
                // stale response from a message the user already left.
                if let Some(Screen::Pager(pager)) = self.screens.last_mut()
                    && pager.request == id
                {
                    match result {
                        Ok(message) => pager.message = Some(message),
                        Err(e) => {
                            self.screens.pop();
                            self.report(e);
                        }
                    }
                }
            }
            Event::InboxOpened(Err(e)) | Event::FlagsUpdated(Err(e)) => self.report(e),
            Event::MessagesListed(_) | Event::FlagsUpdated(Ok(())) => {}
        }
        Vec::new()
    }

    fn report(&mut self, error: Error) {
        self.status = error.to_string();
    }
}

fn effect_id(effect: &Effect) -> RequestId {
    let Effect::Request(id, _) = effect;
    *id
}

fn body_height(height: u16) -> usize {
    usize::from(height).saturating_sub(CHROME_ROWS)
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeDelta};
    use mett_core::mock;
    use mett_core::{MessagePage, PageCursor, WellKnownFolder};

    use super::*;

    fn summaries(n: usize) -> Vec<MessageSummary> {
        (0..n)
            .map(|i| {
                let received = DateTime::UNIX_EPOCH + TimeDelta::minutes(i as i64);
                mock::message(&format!("m{i}"), "in", &format!("subject {i}"), received).summary
            })
            .collect()
    }

    /// An app whose Inbox has been opened and listed with `n` messages.
    fn loaded(n: usize) -> App {
        let (mut app, effects) = App::new(23);
        let [Effect::Request(open, Request::OpenInbox)] = effects.as_slice() else {
            panic!("expected OpenInbox, got {effects:?}");
        };
        let folder = mock::folder("in", "Inbox", Some(WellKnownFolder::Inbox));
        let effects = app.update(Msg::Response(Response {
            id: *open,
            event: Event::InboxOpened(Ok(folder)),
        }));
        let [Effect::Request(list, Request::ListMessages { .. })] = effects.as_slice() else {
            panic!("expected ListMessages, got {effects:?}");
        };
        app.update(Msg::Response(Response {
            id: *list,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: summaries(n),
                next: None,
            })),
        }));
        app
    }

    fn act(app: &mut App, action: Action) -> Vec<Effect> {
        app.update(Msg::Action(action))
    }

    #[test]
    fn loads_the_inbox_on_start() {
        let app = loaded(3);
        assert_eq!(app.messages.len(), 3);
        assert_eq!(app.folder.as_ref().unwrap().name, "Inbox");
        assert!(app.status.is_empty());
    }

    #[test]
    fn keeps_listing_until_the_last_page() {
        let (mut app, effects) = App::new(23);
        let Effect::Request(open, _) = effects[0].clone();
        let folder = mock::folder("in", "Inbox", None);
        let effects = app.update(Msg::Response(Response {
            id: open,
            event: Event::InboxOpened(Ok(folder)),
        }));
        let Effect::Request(first, _) = effects[0].clone();
        let effects = app.update(Msg::Response(Response {
            id: first,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: summaries(2),
                next: Some(PageCursor::new("c")),
            })),
        }));
        let [Effect::Request(_, Request::ListMessages { page, .. })] = effects.as_slice() else {
            panic!("expected another page request, got {effects:?}");
        };
        assert_eq!(page.cursor(), Some(&PageCursor::new("c")));
        assert_eq!(app.messages.len(), 2);
    }

    #[test]
    fn ignores_a_stale_listing() {
        let mut app = loaded(1);
        app.update(Msg::Response(Response {
            id: RequestId(999),
            event: Event::MessagesListed(Ok(MessagePage {
                messages: summaries(5),
                next: None,
            })),
        }));
        assert_eq!(app.messages.len(), 1);
    }

    #[test]
    fn moves_through_the_index_within_bounds() {
        let mut app = loaded(3);
        act(&mut app, Action::PreviousEntry);
        assert_eq!(app.selected, 0);
        assert_eq!(app.status, "You are on the first message.");
        act(&mut app, Action::NextEntry);
        act(&mut app, Action::NextEntry);
        act(&mut app, Action::NextEntry);
        assert_eq!(app.selected, 2);
        assert_eq!(app.status, "You are on the last message.");
        act(&mut app, Action::FirstEntry);
        assert_eq!(app.selected, 0);
        act(&mut app, Action::LastEntry);
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn pages_through_the_index() {
        let mut app = loaded(50);
        assert_eq!(app.body_height, 20);
        act(&mut app, Action::NextPage);
        assert_eq!(app.selected, 20);
        act(&mut app, Action::NextPage);
        act(&mut app, Action::NextPage);
        assert_eq!(app.selected, 49);
        act(&mut app, Action::PreviousPage);
        assert_eq!(app.selected, 29);
    }

    #[test]
    fn displaying_a_message_fetches_it_and_marks_it_read() {
        let mut app = loaded(2);
        let effects = act(&mut app, Action::DisplayMessage);
        assert_eq!(app.menu(), Menu::Pager);
        assert!(matches!(
            effects.as_slice(),
            [
                Effect::Request(_, Request::FetchMessage(id)),
                Effect::Request(_, Request::UpdateFlags(id2, FlagsUpdate { read: Some(true), .. })),
            ] if id.as_str() == "m0" && id2.as_str() == "m0"
        ));
        assert!(app.messages[0].flags.read);

        // Reopening a read message doesn't update its flags again.
        act(&mut app, Action::Exit);
        let effects = act(&mut app, Action::DisplayMessage);
        assert_eq!(effects.len(), 1);
    }

    #[test]
    fn shows_the_fetched_message_and_ignores_stale_ones() {
        let mut app = loaded(2);
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(first, _) = effects[0].clone();
        // Move on to the next message before the first one arrives.
        let effects = act(&mut app, Action::NextEntry);
        let Effect::Request(second, _) = effects[0].clone();

        let message = |id: &str| mock::message(id, "in", id, DateTime::UNIX_EPOCH);
        app.update(Msg::Response(Response {
            id: first,
            event: Event::MessageFetched(Ok(message("m0"))),
        }));
        let Screen::Pager(pager) = app.screen() else {
            panic!()
        };
        assert_eq!(pager.message, None, "stale response ignored");

        app.update(Msg::Response(Response {
            id: second,
            event: Event::MessageFetched(Ok(message("m1"))),
        }));
        let Screen::Pager(pager) = app.screen() else {
            panic!()
        };
        assert_eq!(pager.message.as_ref().unwrap().summary.id.as_str(), "m1");
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn a_failed_fetch_returns_to_the_index_with_the_error() {
        let mut app = loaded(1);
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone();
        app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Err(Error::Network("offline".into()))),
        }));
        assert_eq!(app.menu(), Menu::Index);
        assert_eq!(app.status, "network error: offline");
    }

    #[test]
    fn pager_scrolls_and_exits() {
        let mut app = loaded(1);
        act(&mut app, Action::DisplayMessage);
        act(&mut app, Action::NextLine);
        act(&mut app, Action::NextPage);
        act(&mut app, Action::PreviousLine);
        let Screen::Pager(pager) = app.screen() else {
            panic!()
        };
        assert_eq!(pager.scroll, 20);
        act(&mut app, Action::Exit);
        assert_eq!(app.screen(), &Screen::Index);
        act(&mut app, Action::Quit);
        assert!(app.quit);
    }
}
