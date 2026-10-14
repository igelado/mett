//! The application model and its `update` function: the Elm Architecture
//! (`docs/DESIGN.md` §5.2).
//!
//! [`App`] holds all UI state. Only [`App::update`] changes it, in response
//! to a [`Msg`]; side effects come back as [`Effect`]s for the main loop to
//! carry out. This keeps the UI logic testable without a terminal.

use std::collections::HashMap;

use mett_core::{Error, FlagsUpdate, Folder, Message, MessageId, MessageSummary, Page};

use crate::action::Action;
use crate::keymap::Menu;
use crate::text;
use crate::worker::{Event, Request, RequestId, Response};

/// How many messages to request per page.
const PAGE_SIZE: u32 = 100;

/// Where a pending move goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    /// The entry at this position.
    Entry(usize),
    /// The last message of the folder.
    Last,
}

/// Rows used by the help line, the status bar and the message line.
const CHROME_ROWS: usize = 3;

/// Input to [`App::update`].
// Messages are handled one at a time, so their size doesn't matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Msg {
    /// The user triggered an action.
    Action(Action),
    /// The worker answered a request.
    Response(Response),
    /// The terminal was resized.
    Resize {
        /// Terminal width, in cells.
        width: u16,
        /// Terminal height, in rows.
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
    /// The message laid out as lines (headers and body), not yet wrapped.
    pub lines: Vec<String>,
    /// `lines` wrapped to [`Pager::rows_width`]. Cached, so redrawing and
    /// scrolling don't re-wrap the whole message.
    pub rows: Vec<String>,
    /// The width `rows` were wrapped to, if they have been.
    pub rows_width: Option<usize>,
    /// The request fetching it.
    pub request: RequestId,
    /// The first visible row, after wrapping.
    pub scroll: usize,
}

impl Pager {
    /// Re-wraps the message if the width changed.
    fn layout(&mut self, width: usize) {
        if self.rows_width != Some(width) {
            self.rows = text::wrap_all(&self.lines, width);
            self.rows_width = Some(width);
        }
    }
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
    /// Terminal width, in cells.
    pub width: usize,
    /// Rows available for the index or pager body.
    pub body_height: usize,
    /// The request whose messages the index is waiting for, if any.
    listing: Option<RequestId>,
    /// The page that `listing` asked for.
    listing_page: Option<Page>,
    /// A page that failed to load, so the index is incomplete. Loading it
    /// is retried when the user reaches the end of the index.
    unlisted: Option<Page>,
    /// Changes that failed to save, by message, kept apart from the status
    /// line so that later messages can't hide them. A later successful save
    /// of the same message clears its entry.
    save_failures: HashMap<MessageId, String>,
    /// Where the user asked to move while more messages were loading; the
    /// selection moves there once they arrive.
    pending: Option<Target>,
    /// Pending flag updates, so a failure can be undone in the index.
    flag_updates: HashMap<RequestId, MessageId>,
    next_request: u64,
}

impl App {
    /// A new app for a `width`×`height` terminal, and the effects that start
    /// loading the Inbox.
    pub fn new(width: u16, height: u16) -> (Self, Vec<Effect>) {
        let mut app = Self {
            screens: vec![Screen::Index],
            folder: None,
            messages: Vec::new(),
            selected: 0,
            status: "Opening Inbox...".into(),
            quit: false,
            width: usize::from(width),
            body_height: body_height(height),
            listing: None,
            listing_page: None,
            unlisted: None,
            save_failures: HashMap::new(),
            pending: None,
            flag_updates: HashMap::new(),
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
            Msg::Resize { width, height } => {
                self.width = usize::from(width);
                self.body_height = body_height(height);
                self.clamp_scroll();
                Vec::new()
            }
        }
    }

    fn request(&mut self, request: Request) -> Effect {
        self.next_request += 1;
        Effect::Request(RequestId(self.next_request), request)
    }

    fn on_action(&mut self, action: Action) -> Vec<Effect> {
        // Any new action replaces a pending move.
        self.pending = None;
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
            Action::NextEntry if self.more_to_list() => {
                return self.wait_for(Target::Entry(self.selected + 1));
            }
            Action::NextEntry => self.status = "You are on the last message.".into(),
            Action::PreviousEntry if self.selected > 0 => self.selected -= 1,
            Action::PreviousEntry => self.status = "You are on the first message.".into(),
            Action::FirstEntry => self.selected = 0,
            Action::LastEntry => {
                self.selected = last;
                if self.more_to_list() {
                    return self.wait_for(Target::Last);
                }
            }
            Action::NextPage => {
                let target = self.selected + page;
                self.selected = target.min(last);
                if target > last && self.more_to_list() {
                    return self.wait_for(Target::Entry(target));
                }
            }
            Action::PreviousPage => self.selected = self.selected.saturating_sub(page),
            Action::DisplayMessage => return self.open_selected(),
            Action::Exit | Action::NextLine | Action::PreviousLine => {}
        }
        Vec::new()
    }

    fn on_pager_action(&mut self, action: Action) -> Vec<Effect> {
        let page = self.body_height.max(1);
        match action {
            Action::Quit => self.quit = true,
            Action::Exit => {
                self.screens.pop();
            }
            // As in mutt, j and k in the pager show the next or previous
            // message.
            Action::NextEntry if self.selected + 1 >= self.messages.len() => {
                if self.more_to_list() {
                    return self.wait_for(Target::Entry(self.selected + 1));
                }
                self.status = "You are on the last message.".into();
            }
            Action::PreviousEntry if self.selected == 0 => {
                self.status = "You are on the first message.".into();
            }
            Action::NextEntry | Action::PreviousEntry => {
                if action == Action::NextEntry {
                    self.selected += 1;
                } else {
                    self.selected -= 1;
                }
                self.screens.pop();
                return self.open_selected();
            }
            Action::NextLine => self.scroll_by(1),
            Action::PreviousLine => self.scroll_by(-1),
            Action::NextPage => self.scroll_by(page as isize),
            Action::PreviousPage => self.scroll_by(-(page as isize)),
            Action::FirstEntry | Action::LastEntry | Action::DisplayMessage => {}
        }
        Vec::new()
    }

    /// Scrolls the pager by `rows`, within the message.
    fn scroll_by(&mut self, rows: isize) {
        if let Some(Screen::Pager(pager)) = self.screens.last_mut() {
            pager.scroll = pager.scroll.saturating_add_signed(rows);
        }
        self.clamp_scroll();
    }

    /// Keeps the pager's scroll position within the wrapped message, so that
    /// the last page stays full and scrolling back responds at once.
    fn clamp_scroll(&mut self) {
        let (width, height) = (self.width, self.body_height);
        if let Some(Screen::Pager(pager)) = self.screens.last_mut() {
            pager.layout(width);
            pager.scroll = pager.scroll.min(pager.rows.len().saturating_sub(height));
        }
    }

    /// Opens the pager on the selected message.
    fn open_selected(&mut self) -> Vec<Effect> {
        let Some(summary) = self.messages.get(self.selected) else {
            self.status = "No messages.".into();
            return Vec::new();
        };
        let id = summary.id.clone();
        let fetch = self.request(Request::FetchMessage(id.clone()));
        self.screens.push(Screen::Pager(Pager {
            id,
            message: None,
            lines: Vec::new(),
            rows: Vec::new(),
            rows_width: None,
            request: effect_id(&fetch),
            scroll: 0,
        }));
        vec![fetch]
    }

    fn on_response(&mut self, Response { id, event }: Response) -> Vec<Effect> {
        match event {
            Event::InboxOpened(Ok(folder)) => {
                self.status = format!("Loading {}...", folder.name);
                self.folder = Some(folder);
                return self.list(Page::first(PAGE_SIZE));
            }
            Event::MessagesListed(result) if self.listing == Some(id) => match result {
                Ok(page) => {
                    self.messages.extend(page.messages);
                    let mut effects = Vec::new();
                    if let Some(cursor) = page.next {
                        // Keep loading until the whole folder is listed.
                        effects = self.list(Page::after(cursor, PAGE_SIZE));
                    } else {
                        self.listing = None;
                        self.listing_page = None;
                        // Clear the "Loading" message, but not an error that
                        // was reported while the folder was loading.
                        if self.status.starts_with("Loading ") {
                            self.status.clear();
                        }
                    }
                    effects.extend(self.apply_pending());
                    return effects;
                }
                Err(e) => {
                    self.pending = None;
                    self.listing = None;
                    self.unlisted = self.listing_page.take();
                    self.report(e);
                }
            },
            Event::MessageFetched(result) => return self.on_message_fetched(id, result),
            Event::FlagsUpdated(result) => {
                let Some(message) = self.flag_updates.remove(&id) else {
                    return Vec::new();
                };
                match result {
                    Ok(()) => {
                        self.save_failures.remove(&message);
                    }
                    Err(e) => {
                        // Undo the optimistic change, so the index is right
                        // and reopening the message tries again.
                        if let Some(summary) = self.messages.iter_mut().find(|s| s.id == message) {
                            summary.flags.read = false;
                        }
                        self.save_failures.insert(message, e.to_string());
                        self.report(e);
                    }
                }
            }
            Event::InboxOpened(Err(e)) => self.report(e),
            Event::MessagesListed(_) => {}
        }
        Vec::new()
    }

    fn on_message_fetched(
        &mut self,
        id: RequestId,
        result: mett_core::Result<Message>,
    ) -> Vec<Effect> {
        // Only the pager that asked for it cares; anything else is a stale
        // response for a message the user already left.
        let Some(Screen::Pager(pager)) = self.screens.last_mut() else {
            return Vec::new();
        };
        if pager.request != id {
            return Vec::new();
        }
        let message = match result {
            Ok(message) => message,
            Err(e) => {
                self.screens.pop();
                self.report(e);
                return Vec::new();
            }
        };
        pager.lines = text::pager_lines(&message);
        pager.rows_width = None;
        pager.layout(self.width);
        let message_id = message.summary.id.clone();
        pager.message = Some(message);

        // Now that it has been shown, mark it read (as mutt does), updating
        // the index straight away.
        let Some(summary) = self.messages.iter_mut().find(|s| s.id == message_id) else {
            return Vec::new();
        };
        if summary.flags.read {
            return Vec::new();
        }
        summary.flags.read = true;
        let update = self.request(Request::UpdateFlags(
            message_id.clone(),
            FlagsUpdate {
                read: Some(true),
                flagged: None,
            },
        ));
        self.flag_updates.insert(effect_id(&update), message_id);
        vec![update]
    }

    /// Whether part of the folder could not be listed. The index then
    /// shows only the messages loaded so far.
    pub fn incomplete(&self) -> bool {
        self.unlisted.is_some()
    }

    /// Lists `page` of the open folder.
    fn list(&mut self, page: Page) -> Vec<Effect> {
        let Some(folder) = &self.folder else {
            return Vec::new();
        };
        let list = self.request(Request::ListMessages {
            folder: folder.id.clone(),
            page: page.clone(),
        });
        self.listing = Some(effect_id(&list));
        self.listing_page = Some(page);
        vec![list]
    }

    /// Whether more messages are loading or could be loaded.
    fn more_to_list(&self) -> bool {
        self.listing.is_some() || self.unlisted.is_some()
    }

    /// Remembers that the user asked for the next message, and makes sure
    /// more messages are on their way.
    fn wait_for(&mut self, target: Target) -> Vec<Effect> {
        self.pending = Some(target);
        self.status = "Loading more messages...".into();
        self.retry_listing()
    }

    /// Moves towards a pending target now that more messages have arrived.
    /// An entry target is reached once it is loaded, or settles on the last
    /// message once the folder is fully loaded; the last-message target
    /// follows the end of the index until loading finishes.
    fn apply_pending(&mut self) -> Vec<Effect> {
        let Some(target) = self.pending else {
            return Vec::new();
        };
        let last = self.messages.len().saturating_sub(1);
        let loading = self.listing.is_some();
        let selected = match target {
            Target::Entry(entry) if entry <= last => entry,
            Target::Entry(_) | Target::Last if !loading => last,
            Target::Last => {
                self.selected = last;
                return Vec::new();
            }
            Target::Entry(_) => return Vec::new(),
        };
        self.pending = None;
        if self.status.starts_with("Loading ") {
            self.status.clear();
        }
        if selected == self.selected {
            return Vec::new();
        }
        self.selected = selected;
        match self.menu() {
            Menu::Index => Vec::new(),
            Menu::Pager => {
                self.screens.pop();
                self.open_selected()
            }
        }
    }

    /// Changes that could not be saved, one per message.
    pub fn save_failures(&self) -> Vec<String> {
        let mut failures: Vec<_> = self.save_failures.values().cloned().collect();
        failures.sort();
        failures
    }

    /// Retries loading the rest of an incomplete folder.
    fn retry_listing(&mut self) -> Vec<Effect> {
        if self.listing.is_some() {
            return Vec::new();
        }
        let Some(page) = self.unlisted.take() else {
            return Vec::new();
        };
        self.status = "Loading more messages...".into();
        self.list(page)
    }

    /// Whether changes to the mailbox are still being saved. mett waits
    /// for them before exiting, so they aren't lost.
    pub fn saving(&self) -> bool {
        !self.flag_updates.is_empty()
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
        let (mut app, effects) = App::new(80, 23);
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

    fn message(id: &str, body_lines: usize) -> Message {
        let mut message = mock::message(id, "in", id, DateTime::UNIX_EPOCH);
        message.body.content = (0..body_lines).map(|i| format!("line {i}\n")).collect();
        message
    }

    /// Opens the selected message and delivers it with `body_lines` lines;
    /// returns the effects of the delivery.
    fn open(app: &mut App, body_lines: usize) -> Vec<Effect> {
        let effects = act(app, Action::DisplayMessage);
        let [Effect::Request(fetch, Request::FetchMessage(id))] = effects.as_slice() else {
            panic!("expected FetchMessage, got {effects:?}");
        };
        let message = message(id.as_str(), body_lines);
        app.update(Msg::Response(Response {
            id: *fetch,
            event: Event::MessageFetched(Ok(message)),
        }))
    }

    fn pager(app: &App) -> &Pager {
        let Screen::Pager(pager) = app.screen() else {
            panic!("not in the pager");
        };
        pager
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
        let (mut app, effects) = App::new(80, 23);
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
    fn marks_a_message_read_once_it_is_shown() {
        let mut app = loaded(2);
        let effects = act(&mut app, Action::DisplayMessage);
        assert_eq!(app.menu(), Menu::Pager);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Request(_, Request::FetchMessage(id))] if id.as_str() == "m0"
        ));
        assert!(!app.messages[0].flags.read, "not read before it is shown");

        act(&mut app, Action::Exit);
        let effects = open(&mut app, 1);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Request(_, Request::UpdateFlags(id, FlagsUpdate { read: Some(true), .. }))]
                if id.as_str() == "m0"
        ));
        assert!(app.messages[0].flags.read);

        // Reopening a read message doesn't update its flags again.
        act(&mut app, Action::Exit);
        assert!(open(&mut app, 1).is_empty());
    }

    #[test]
    fn finishing_the_listing_keeps_an_error_reported_meanwhile() {
        let (mut app, effects) = App::new(80, 23);
        let Effect::Request(open_inbox, _) = effects[0].clone();
        let effects = app.update(Msg::Response(Response {
            id: open_inbox,
            event: Event::InboxOpened(Ok(mock::folder("in", "Inbox", None))),
        }));
        let Effect::Request(first, _) = effects[0].clone();
        let effects = app.update(Msg::Response(Response {
            id: first,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: summaries(1),
                next: Some(PageCursor::new("c")),
            })),
        }));
        let Effect::Request(second, _) = effects[0].clone();

        // An error is reported while the second page is loading…
        let effects = open(&mut app, 1);
        let Effect::Request(update, _) = effects[0].clone();
        app.update(Msg::Response(Response {
            id: update,
            event: Event::FlagsUpdated(Err(Error::Network("offline".into()))),
        }));
        // …and survives the end of the listing.
        app.update(Msg::Response(Response {
            id: second,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: Vec::new(),
                next: None,
            })),
        }));
        assert_eq!(app.status, "network error: offline");
    }

    #[test]
    fn a_failed_page_leaves_the_index_incomplete_until_retried() {
        let (mut app, effects) = App::new(80, 23);
        let Effect::Request(open_inbox, _) = effects[0].clone();
        let effects = app.update(Msg::Response(Response {
            id: open_inbox,
            event: Event::InboxOpened(Ok(mock::folder("in", "Inbox", None))),
        }));
        let Effect::Request(first, _) = effects[0].clone();
        let effects = app.update(Msg::Response(Response {
            id: first,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: summaries(2),
                next: Some(PageCursor::new("c")),
            })),
        }));
        let Effect::Request(second, _) = effects[0].clone();
        app.update(Msg::Response(Response {
            id: second,
            event: Event::MessagesListed(Err(Error::Network("offline".into()))),
        }));
        assert!(app.incomplete());

        // Reaching the end of the index retries the failed page.
        act(&mut app, Action::NextEntry);
        let effects = act(&mut app, Action::NextEntry);
        let [Effect::Request(retry, Request::ListMessages { page, .. })] = effects.as_slice()
        else {
            panic!("expected a retry, got {effects:?}");
        };
        assert_eq!(page.cursor(), Some(&PageCursor::new("c")));
        assert!(!app.incomplete());
        app.update(Msg::Response(Response {
            id: *retry,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: summaries(1),
                next: None,
            })),
        }));
        assert_eq!(app.messages.len(), 3);
        assert!(!app.incomplete());
    }

    #[test]
    fn the_pager_also_retries_a_failed_page() {
        let (mut app, effects) = App::new(80, 23);
        let Effect::Request(open_inbox, _) = effects[0].clone();
        let effects = app.update(Msg::Response(Response {
            id: open_inbox,
            event: Event::InboxOpened(Ok(mock::folder("in", "Inbox", None))),
        }));
        let Effect::Request(first, _) = effects[0].clone();
        let effects = app.update(Msg::Response(Response {
            id: first,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: summaries(1),
                next: Some(PageCursor::new("c")),
            })),
        }));
        let Effect::Request(second, _) = effects[0].clone();
        app.update(Msg::Response(Response {
            id: second,
            event: Event::MessagesListed(Err(Error::Network("offline".into()))),
        }));

        open(&mut app, 1);
        let effects = act(&mut app, Action::NextEntry);
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::Request(_, Request::ListMessages { .. })]
            ),
            "{effects:?}"
        );
        assert_eq!(app.status, "Loading more messages...");
    }

    #[test]
    fn records_every_failed_save() {
        let mut app = loaded(2);
        let first = open(&mut app, 1);
        act(&mut app, Action::Exit);
        act(&mut app, Action::NextEntry);
        let second = open(&mut app, 1);
        let ([Effect::Request(first, _)], [Effect::Request(second, _)]) =
            (first.as_slice(), second.as_slice())
        else {
            panic!("expected one flag update per message");
        };

        app.update(Msg::Response(Response {
            id: *first,
            event: Event::FlagsUpdated(Err(Error::Network("offline".into()))),
        }));
        // A later success doesn't hide the earlier failure.
        app.update(Msg::Response(Response {
            id: *second,
            event: Event::FlagsUpdated(Ok(())),
        }));
        assert_eq!(app.save_failures(), ["network error: offline"]);
        assert!(!app.saving());

        // Saving the same message again successfully clears its failure.
        act(&mut app, Action::Exit);
        act(&mut app, Action::FirstEntry);
        let retry = open(&mut app, 1);
        let [Effect::Request(retry, _)] = retry.as_slice() else {
            panic!("expected a retried flag update, got {retry:?}");
        };
        app.update(Msg::Response(Response {
            id: *retry,
            event: Event::FlagsUpdated(Ok(())),
        }));
        assert!(app.save_failures().is_empty());
    }

    /// An app whose first page of `n` messages has loaded, with another page
    /// still loading; returns the app and that request.
    fn loading_more(n: usize) -> (App, RequestId) {
        let (mut app, effects) = App::new(80, 23);
        let Effect::Request(open_inbox, _) = effects[0].clone();
        let effects = app.update(Msg::Response(Response {
            id: open_inbox,
            event: Event::InboxOpened(Ok(mock::folder("in", "Inbox", None))),
        }));
        let Effect::Request(first, _) = effects[0].clone();
        let effects = app.update(Msg::Response(Response {
            id: first,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: summaries(n),
                next: Some(PageCursor::new("c")),
            })),
        }));
        let Effect::Request(second, _) = effects[0].clone();
        (app, second)
    }

    fn deliver_last_page(
        app: &mut App,
        id: RequestId,
        messages: Vec<MessageSummary>,
    ) -> Vec<Effect> {
        app.update(Msg::Response(Response {
            id,
            event: Event::MessagesListed(Ok(MessagePage {
                messages,
                next: None,
            })),
        }))
    }

    #[test]
    fn next_entry_waits_for_the_next_page_in_the_index() {
        let (mut app, second) = loading_more(1);
        act(&mut app, Action::NextEntry);
        assert_eq!(app.selected, 0);
        assert_eq!(app.status, "Loading more messages...");
        let mut more = summaries(2);
        more.remove(0);
        deliver_last_page(&mut app, second, more);
        assert_eq!(app.selected, 1, "moved on once the page arrived");
    }

    #[test]
    fn next_entry_waits_for_the_next_page_in_the_pager() {
        let (mut app, second) = loading_more(1);
        open(&mut app, 1);
        act(&mut app, Action::NextEntry);
        let mut more = summaries(2);
        more.remove(0);
        let effects = deliver_last_page(&mut app, second, more);
        assert_eq!(app.selected, 1);
        assert!(
            matches!(effects.as_slice(), [Effect::Request(_, Request::FetchMessage(id))] if id.as_str() == "m1"),
            "{effects:?}"
        );
    }

    #[test]
    fn last_entry_follows_the_folder_while_it_loads() {
        let (mut app, second) = loading_more(2);
        act(&mut app, Action::LastEntry);
        assert_eq!(app.selected, 1);
        deliver_last_page(&mut app, second, summaries(5));
        assert_eq!(app.selected, 6, "the real last message");
    }

    #[test]
    fn next_page_completes_once_the_page_arrives() {
        let (mut app, second) = loading_more(5);
        // 20 rows per page: the target, entry 20, isn't loaded yet.
        act(&mut app, Action::NextPage);
        assert_eq!(app.selected, 4);
        deliver_last_page(&mut app, second, summaries(30));
        assert_eq!(app.selected, 20);
    }

    #[test]
    fn another_action_cancels_a_pending_next_entry() {
        let (mut app, second) = loading_more(2);
        act(&mut app, Action::NextEntry);
        act(&mut app, Action::NextEntry);
        act(&mut app, Action::PreviousEntry);
        deliver_last_page(&mut app, second, Vec::new());
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn tracks_unsaved_flag_changes() {
        let mut app = loaded(1);
        assert!(!app.saving());
        let effects = open(&mut app, 1);
        assert!(app.saving());
        let Effect::Request(update, _) = effects[0].clone();
        app.update(Msg::Response(Response {
            id: update,
            event: Event::FlagsUpdated(Ok(())),
        }));
        assert!(!app.saving());
    }

    #[test]
    fn a_failed_flag_update_marks_the_message_unread_again() {
        let mut app = loaded(1);
        let effects = open(&mut app, 1);
        let Effect::Request(update, _) = effects[0].clone();
        app.update(Msg::Response(Response {
            id: update,
            event: Event::FlagsUpdated(Err(Error::Network("offline".into()))),
        }));
        assert!(!app.messages[0].flags.read);
        assert_eq!(app.status, "network error: offline");
    }

    #[test]
    fn shows_the_fetched_message_and_ignores_stale_ones() {
        let mut app = loaded(2);
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(first, _) = effects[0].clone();
        // Move on to the next message before the first one arrives.
        let effects = act(&mut app, Action::NextEntry);
        let Effect::Request(second, _) = effects[0].clone();

        app.update(Msg::Response(Response {
            id: first,
            event: Event::MessageFetched(Ok(message("m0", 1))),
        }));
        assert_eq!(pager(&app).message, None, "stale response ignored");
        assert!(!app.messages[0].flags.read);

        app.update(Msg::Response(Response {
            id: second,
            event: Event::MessageFetched(Ok(message("m1", 1))),
        }));
        let shown = pager(&app).message.as_ref().unwrap();
        assert_eq!(shown.summary.id.as_str(), "m1");
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn a_failed_fetch_returns_to_the_index_and_leaves_it_unread() {
        let mut app = loaded(1);
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone();
        let effects = app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Err(Error::Network("offline".into()))),
        }));
        assert!(effects.is_empty());
        assert_eq!(app.menu(), Menu::Index);
        assert_eq!(app.status, "network error: offline");
        assert!(!app.messages[0].flags.read);
    }

    #[test]
    fn pager_scrolls_within_the_message() {
        let mut app = loaded(1);
        // 4 header lines and a blank line, 40 body lines; 20 visible rows.
        open(&mut app, 40);
        let rows = pager(&app).rows.len();
        assert_eq!(rows, text::wrap_all(&pager(&app).lines, 80).len());
        act(&mut app, Action::NextLine);
        act(&mut app, Action::NextPage);
        act(&mut app, Action::PreviousLine);
        assert_eq!(pager(&app).scroll, 20);

        for _ in 0..5 {
            act(&mut app, Action::NextPage);
        }
        assert_eq!(pager(&app).scroll, rows - 20, "stops at the last full page");
        act(&mut app, Action::PreviousPage);
        assert_eq!(
            pager(&app).scroll,
            rows - 40,
            "responds at once after the end"
        );

        act(&mut app, Action::Exit);
        assert_eq!(app.screen(), &Screen::Index);
        act(&mut app, Action::Quit);
        assert!(app.quit);
    }

    #[test]
    fn long_lines_wrap_and_can_be_scrolled_to() {
        let mut app = loaded(1);
        app.update(Msg::Resize {
            width: 10,
            height: 8,
        });
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone();
        let mut message = message("m0", 0);
        message.body.content = "x".repeat(100);
        app.update(Msg::Response(Response {
            id: fetch,
            event: Event::MessageFetched(Ok(message)),
        }));
        let rows = pager(&app).rows.len();
        assert_eq!(rows, text::wrap_all(&pager(&app).lines, 10).len());
        assert!(rows > 10);
        for _ in 0..rows {
            act(&mut app, Action::NextLine);
        }
        assert_eq!(pager(&app).scroll, rows - 5, "every row is reachable");
    }
}
