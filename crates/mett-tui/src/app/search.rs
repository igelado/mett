//! Searching (`/`, `n`) and limiting (`l`) the index with mutt's patterns,
//! and searching the text in the pager.

use std::collections::HashMap;
use std::ops::Range;

use mett_core::{MessageId, MessageSummary};

use super::composing::Question;
use super::{App, Effect, Screen};
use crate::action::Action;
use crate::pattern::{self, Candidate, Pattern};

/// What the index says when a limit leaves it empty.
const NO_MATCHES: &str = "No messages matched the criteria.";

/// The rows of the pager that show `line`.
fn rows_of(pager: &crate::app::Pager, line: usize) -> Range<usize> {
    let first = pager.line_rows.get(line).copied().unwrap_or(0);
    let last = pager
        .line_rows
        .get(line + 1)
        .copied()
        .unwrap_or(pager.rows.len());
    first..last
}

/// The byte offset in `line` where wrapped row `row` of the pager starts
/// (0 if the row isn't part of that line).
fn row_offset(pager: &crate::app::Pager, line: usize, row: usize) -> usize {
    if rows_of(pager, line).contains(&row) {
        pager.row_ranges[row].start
    } else {
        0
    }
}

/// The wrapped row of the pager that shows byte `offset` of `line` (or,
/// for a space dropped where the line wraps, the row after it).
fn match_row(pager: &crate::app::Pager, line: usize, offset: usize) -> usize {
    let rows = rows_of(pager, line);
    rows.clone()
        .find(|&row| offset < pager.row_ranges[row].end)
        .unwrap_or_else(|| rows.end.saturating_sub(1).max(rows.start))
}

/// The parts of the pager's rows that show its highlighted match, as a
/// row and a byte range in that row.
pub fn highlighted(pager: &crate::app::Pager) -> Vec<(usize, Range<usize>)> {
    let Some((line, found)) = &pager.highlight else {
        return Vec::new();
    };
    if *line >= pager.lines.len() {
        return Vec::new();
    }
    rows_of(pager, *line)
        .filter_map(|row| {
            let shown = &pager.row_ranges[row];
            let from = found.start.max(shown.start);
            let to = found.end.min(shown.end);
            (from < to).then(|| (row, from - shown.start..to - shown.start))
        })
        .collect()
}

/// The pattern the index is limited to.
#[derive(Clone, Debug)]
pub struct Limit {
    /// The pattern as the user typed it, for the status bar.
    pub text: String,
    pattern: Pattern,
}

impl App {
    /// Handles the search and limit actions. Returns `None` for other
    /// actions.
    pub(super) fn on_search_action(&mut self, action: Action) -> Option<Vec<Effect>> {
        match action {
            Action::Search => {
                let last = self.last_search().clone().unwrap_or_default();
                self.ask(Question::Search, "Search for: ", &last);
            }
            Action::SearchNext => match self.last_search().clone() {
                Some(text) => return Some(self.new_search(&text)),
                None => self.fail("No search pattern."),
            },
            Action::Limit if matches!(self.screen(), Screen::Index) => {
                let current = self
                    .limit
                    .as_ref()
                    .map(|l| l.text.clone())
                    .unwrap_or_default();
                self.ask(Question::Limit, "Limit to messages matching: ", &current);
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    /// Acts on a search or limit pattern the user typed.
    pub(super) fn answer_search(&mut self, question: Question, text: String) -> Vec<Effect> {
        if question == Question::Limit {
            self.set_limit(&text);
            return Vec::new();
        }
        if text.trim().is_empty() {
            return Vec::new();
        }
        *self.last_search() = Some(text.clone());
        // A new query starts at what is on screen; only `n` goes on from
        // the last match.
        if let Some(Screen::Pager(pager)) = self.screens.last_mut() {
            pager.search_match = None;
            pager.highlight = None;
        }
        self.new_search(&text)
    }

    /// The last search of the current screen: the index searches with
    /// patterns and the pager with text, so each has its own.
    fn last_search(&mut self) -> &mut Option<String> {
        if matches!(self.screen(), Screen::Pager(_)) {
            &mut self.last_pager_search
        } else {
            &mut self.last_search
        }
    }

    /// Starts looking for `text`, from the first message if the index is
    /// empty (it may be loading).
    pub(super) fn new_search(&mut self, text: &str) -> Vec<Effect> {
        self.search_from_top = self.messages.is_empty();
        self.search(text)
    }

    /// Finds the next match of `text`: in the index, the next message
    /// matching the pattern; in the pager, the next line matching the
    /// regular expression.
    fn search(&mut self, text: &str) -> Vec<Effect> {
        if let Some(Screen::Pager(pager)) = self.screens.last_mut() {
            // The message is still on its way: search it once it comes.
            if pager.message.is_none() {
                pager.pending_search = Some(text.to_owned());
                self.status = "Searching...".into();
                return Vec::new();
            }
            let position = self.screens.len() - 1;
            self.search_pager(position, text);
            return Vec::new();
        }
        let pattern = match Pattern::parse(text) {
            Ok(pattern) => pattern,
            Err(e) => {
                self.fail(e.to_string());
                return Vec::new();
            }
        };
        // Replies folded under the selected (collapsed) thread come before
        // the next row.
        let folded = self.folded_matches(&pattern);
        if !self.search_from_top
            && let Some(entry) = self.messages.get(self.selected)
            && let Some(id) = folded.get(&entry.id).cloned()
        {
            self.status.clear();
            self.uncollapse_to(&id);
            return Vec::new();
        }
        let count = self.messages.len();
        let matches = |i: &usize| self.search_match(&pattern, &folded, *i).is_some();
        // From the next message on, then (once the whole folder is listed:
        // the next match may be on a page still to come) wrapping around
        // to the top.
        // Messages still to come: the folder is being opened or listed, or
        // a page failed to load (and is loaded again now).
        let more = self.opening.is_some() || self.more_to_list();
        // With no message to start after (the index was empty when the
        // search began), the first one counts too.
        let first = if self.search_from_top {
            0
        } else {
            self.selected + 1
        };
        let mut found = (first..count).find(matches);
        if found.is_none() && !more && !self.search_from_top {
            found = (0..count.min(self.selected + 1)).find(matches);
        }
        let from_top = std::mem::take(&mut self.search_from_top);
        match found {
            Some(i) => {
                // Replaces an earlier "Not found." too.
                self.status = if i <= self.selected && !from_top {
                    "Search wrapped to top.".into()
                } else {
                    String::new()
                };
                let id = self
                    .search_match(&pattern, &folded, i)
                    .expect("found above");
                self.selected = i;
                // A collapsed thread with matches folded into it opens, so
                // that `n` goes on to them.
                if folded.contains_key(&self.messages[i].id) {
                    self.uncollapse_to(&id);
                }
            }
            // More of the folder is on its way: look again when it comes.
            None if more => {
                self.pending_search = Some(text.to_owned());
                self.search_from_top = from_top;
                let effects = self.retry_listing();
                self.status = "Searching...".into();
                return effects;
            }
            None => self.fail("Not found."),
        }
        Vec::new()
    }

    /// Goes on with a search that found nothing among the messages listed
    /// so far, now that more have arrived.
    pub(super) fn continue_search(&mut self) -> Vec<Effect> {
        if !matches!(self.screen(), Screen::Index) {
            self.pending_search = None;
            return Vec::new();
        }
        match self.pending_search.take() {
            Some(text) => self.search(&text),
            None => Vec::new(),
        }
    }

    /// Scrolls the pager at `position` in the screen stack to the next
    /// line that matches `text`, wrapping
    /// around to the top. Lines are searched as written, not as wrapped,
    /// so a phrase split across rows is still found.
    pub(super) fn search_pager(&mut self, position: usize, text: &str) {
        let regex = match pattern::regex(text) {
            Ok(regex) => regex,
            Err(e) => {
                self.fail(e.to_string());
                return;
            }
        };
        let (width, height) = (self.width, self.body_height);
        let Some(Screen::Pager(pager)) = self.screens.get_mut(position) else {
            return;
        };
        // The terminal may have been resized while another screen covered
        // the pager: search the rows it will show.
        pager.fit(width, height);
        let count = pager.lines.len();
        if count == 0 {
            self.fail("Not found.");
            return;
        }
        // After the last match, or from the line at the top of the screen.
        let top = pager
            .line_rows
            .partition_point(|&row| row <= pager.scroll)
            .saturating_sub(1);
        // A new search starts at the first row shown, which may be partway
        // through a long line.
        let (from_line, from_offset) = pager
            .search_match
            .unwrap_or_else(|| (top, row_offset(pager, top, pager.scroll)));
        // The rest of that line, the lines after it, then (wrapping) the
        // lines before it and the start of that line.
        let found = std::iter::once((from_line, from_offset))
            .chain((1..=count).map(|step| ((from_line + step) % count, 0)))
            .find_map(|(line, offset)| {
                let text = &pager.lines[line];
                // Past the end: an empty match there was the last one.
                if offset > text.len() {
                    return None;
                }
                let found = regex.find_at(text, offset)?;
                // An empty match still moves the next search on, by a
                // whole character.
                let next = text[found.start()..]
                    .chars()
                    .next()
                    .map_or(found.start() + 1, |c| found.start() + c.len_utf8());
                Some((line, found.start(), found.end(), found.end().max(next)))
            });
        match found {
            Some((line, start, end, next)) => {
                let wrapped = (line, start) < (from_line, from_offset);
                self.status = if wrapped {
                    "Search wrapped to top.".into()
                } else {
                    String::new()
                };
                // Show the row with the match at the top, unless that would
                // scroll past the end.
                let row = match_row(pager, line, start);
                pager.scroll = row.min(pager.rows.len().saturating_sub(height));
                pager.search_match = Some((line, next));
                pager.highlight = Some((line, start..end));
            }
            None => self.fail("Not found."),
        }
    }

    /// Whether `message` matches `pattern`.
    fn matches(&self, pattern: &Pattern, message: &MessageSummary) -> bool {
        pattern.matches(&Candidate {
            summary: message,
            deleted: self.deleted.contains(&message.id),
        })
    }

    /// What matches `pattern` at entry `i` of the index: the message, or
    /// one folded under it into its collapsed thread.
    fn search_match(
        &self,
        pattern: &Pattern,
        folded: &HashMap<MessageId, MessageId>,
        i: usize,
    ) -> Option<MessageId> {
        let entry = &self.messages[i];
        if self.matches(pattern, entry) {
            return Some(entry.id.clone());
        }
        folded.get(&entry.id).cloned()
    }

    /// For each collapsed thread, the first message folded into it that
    /// matches `pattern`, by the thread's first message: all found in one
    /// pass.
    fn folded_matches(&self, pattern: &Pattern) -> HashMap<MessageId, MessageId> {
        let mut found = HashMap::new();
        for message in &self.folded {
            let Some(thread) = self.thread_of(&message.id) else {
                continue;
            };
            if found.contains_key(&thread.root) {
                continue;
            }
            if self.matches(pattern, message) {
                found.insert(thread.root.clone(), message.id.clone());
            }
        }
        found
    }

    /// Limits the index to the messages matching `text`, or shows them all
    /// again for an empty pattern, `all` or `~A`. A bad pattern changes
    /// nothing.
    pub(super) fn set_limit(&mut self, text: &str) {
        let text = text.trim();
        let pattern = if text.is_empty() || text == "all" || text == "~A" {
            None
        } else {
            match Pattern::parse(text) {
                Ok(pattern) => Some(pattern),
                Err(e) => {
                    self.fail(e.to_string());
                    return;
                }
            }
        };
        let selected = self.messages.get(self.selected).map(|m| m.id.clone());
        // Start from the whole folder, in the order it was listed (with
        // mail that arrived since in its place).
        let mut all = std::mem::take(&mut self.messages);
        all.append(&mut self.hidden);
        all.append(&mut self.folded);
        all.sort_by_key(|m| self.positions.get(&m.id).copied().unwrap_or(usize::MAX));
        self.messages = all;
        self.limit = None;
        self.limited.clear();
        // Replaces an earlier "No messages matched" or bad pattern.
        self.status.clear();
        if let Some(pattern) = pattern {
            self.apply_limit(Limit {
                text: text.to_owned(),
                pattern,
            });
            if self.messages.is_empty() {
                self.status = NO_MATCHES.into();
            }
        }
        self.arrange();
        self.limit_index_to(selected.as_ref());
    }

    /// Moves the messages that don't match `limit` aside.
    ///
    /// As in mutt, the limit is applied when it is set and to messages
    /// listed later, but a message whose state changes afterwards (read
    /// under `~N`, unflagged under `~F`) stays in view until the limit is
    /// set again. Otherwise the message being read would vanish from under
    /// the pager, and `j` and `k` would skip around.
    fn apply_limit(&mut self, limit: Limit) {
        let (shown, hidden): (Vec<MessageSummary>, Vec<MessageSummary>) =
            std::mem::take(&mut self.messages)
                .into_iter()
                .partition(|m| self.matches(&limit.pattern, m));
        self.messages = shown;
        self.hidden = hidden;
        self.limit = Some(limit);
    }

    /// Selects the message `id` if it is shown (or the collapsed thread
    /// it is in), or the first one.
    fn limit_index_to(&mut self, id: Option<&mett_core::MessageId>) {
        self.selected = id.and_then(|id| self.entry_of(id)).unwrap_or(0);
    }

    /// Adds newly listed messages to the index, or aside if the index is
    /// limited and they don't match.
    pub(super) fn add_listed(&mut self, messages: Vec<MessageSummary>, last: bool) {
        for message in &messages {
            let next = self.positions.len();
            self.positions.entry(message.id.clone()).or_insert(next);
        }
        // The first page starts at the top, wherever its first message
        // goes.
        let first = self.messages.is_empty() && self.hidden.is_empty() && self.folded.is_empty();
        let limited = self.limit.is_some();
        let mut shown = Vec::with_capacity(messages.len());
        for message in messages {
            if self.limit_shows(&message) {
                shown.push(message);
            } else {
                self.hidden.push(message);
            }
        }
        self.arrange_listed(shown, last);
        if first {
            self.selected = 0;
        }
        // A later page had matches after all.
        if limited && !self.messages.is_empty() && self.status == NO_MATCHES {
            self.status.clear();
        }
    }

    /// Whether the limit, if any, shows `message`: as it did before the
    /// folder was listed again, or if it matches.
    pub(super) fn limit_shows(&self, message: &MessageSummary) -> bool {
        let Some(limit) = &self.limit else {
            return true;
        };
        self.limited
            .get(&message.id)
            .copied()
            .unwrap_or_else(|| self.matches(&limit.pattern, message))
    }

    /// The number of messages in the folder, shown or not.
    pub fn total_messages(&self) -> usize {
        self.messages.len() + self.hidden.len() + self.folded.len()
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeDelta};
    use mett_core::{Address, MessageSummary, WellKnownFolder, mock};
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    use super::*;
    use crate::app::composing::Input;
    use crate::app::testing::*;
    use crate::app::{Msg, Settings};
    use crate::lineedit::LineEditor;

    use crate::worker::Event;

    /// Messages from alternating senders, newest first: m0 from Alice
    /// ("Lunch 0"), m1 from Bob ("Report 1"), and so on; m0 and m1 unread.
    fn app() -> App {
        let (mut app, list) = opening(Settings::default(), 24);
        let messages = (0..6)
            .map(|i| {
                let date = DateTime::UNIX_EPOCH + TimeDelta::minutes(100 - i);
                let (from, subject) = if i % 2 == 0 {
                    ("alice@contoso.com", "Lunch")
                } else {
                    ("bob@fabrikam.com", "Report")
                };
                let mut m =
                    mock::message(&format!("m{i}"), "in", &format!("{subject} {i}"), date).summary;
                m.from = Some(Address::new(from));
                m.flags.read = i >= 2;
                m
            })
            .collect();
        listed(&mut app, list, messages, None);
        app
    }

    fn answer(app: &mut App, text: &str) {
        if let Some(Input::Line { editor, .. }) = &mut app.input {
            *editor = LineEditor::new("");
        }
        for c in text.chars() {
            app.update(Msg::Key(KeyEvent::from(KeyCode::Char(c))));
        }
        app.update(Msg::Key(KeyEvent::from(KeyCode::Enter)));
    }

    #[test]
    fn searches_the_index_and_wraps() {
        let mut app = app();
        act(&mut app, Action::Search);
        answer(&mut app, "~f bob");
        assert_eq!(app.selected, 1);
        act(&mut app, Action::SearchNext);
        act(&mut app, Action::SearchNext);
        assert_eq!(app.selected, 5);
        act(&mut app, Action::SearchNext);
        assert_eq!(app.selected, 1);
        assert_eq!(app.status, "Search wrapped to top.");
        // Plain text searches the sender and the subject.
        act(&mut app, Action::Search);
        answer(&mut app, "lunch 4");
        assert_eq!(app.selected, 4);
        act(&mut app, Action::Search);
        answer(&mut app, "nobody");
        assert_eq!(app.status, "Not found.");
        assert_eq!(app.selected, 4);
        act(&mut app, Action::Search);
        answer(&mut app, "~q");
        assert!(app.status.contains("not supported"), "{}", app.status);
    }

    #[test]
    fn limits_the_index_and_lifts_the_limit() {
        let mut app = app();
        act(&mut app, Action::NextEntry);
        act(&mut app, Action::Limit);
        answer(&mut app, "~f bob");
        assert_eq!(ids(&app), ["m1", "m3", "m5"]);
        assert_eq!(app.selected, 0, "still on m1");
        assert_eq!(app.total_messages(), 6);
        assert_eq!(app.limit.as_ref().unwrap().text, "~f bob");

        act(&mut app, Action::Limit);
        answer(&mut app, "~N");
        assert_eq!(ids(&app), ["m0", "m1"]);
        assert_eq!(app.selected, 1, "still on m1");

        // A bad pattern changes nothing.
        act(&mut app, Action::Limit);
        answer(&mut app, "~N (");
        assert_eq!(ids(&app), ["m0", "m1"]);

        act(&mut app, Action::Limit);
        answer(&mut app, "all");
        assert_eq!(ids(&app), ["m0", "m1", "m2", "m3", "m4", "m5"]);
        assert!(app.limit.is_none());

        act(&mut app, Action::Limit);
        answer(&mut app, "~F");
        assert!(app.messages.is_empty());
        assert_eq!(app.status, "No messages matched the criteria.");
    }

    #[test]
    fn searches_the_text_in_the_pager() {
        let mut app = app();
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a fetch, got {effects:?}");
        };
        let mut message = mock::message("m0", "in", "Lunch 0", DateTime::UNIX_EPOCH);
        message.body.content = (0..60).map(|i| format!("line {i}\n")).collect();
        respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
        act(&mut app, Action::Search);
        answer(&mut app, "line 1[0-9]");
        let Screen::Pager(pager) = app.screen() else {
            panic!("not in the pager");
        };
        assert_eq!(pager.rows[pager.scroll], "line 10");
        act(&mut app, Action::SearchNext);
        let Screen::Pager(pager) = app.screen() else {
            panic!("not in the pager");
        };
        assert_eq!(pager.rows[pager.scroll], "line 11");
    }

    #[test]
    fn pager_search_moves_on_from_a_match_near_the_end() {
        let mut app = app();
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a fetch, got {effects:?}");
        };
        let mut message = mock::message("m0", "in", "Lunch 0", DateTime::UNIX_EPOCH);
        message.body.content = (0..60).map(|i| format!("line {i}\n")).collect();
        respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
        // Both matches are on the last screen.
        act(&mut app, Action::Search);
        answer(&mut app, "line 5[89]");
        act(&mut app, Action::SearchNext);
        let Screen::Pager(pager) = app.screen() else {
            panic!("not in the pager");
        };
        assert_eq!(pager.lines[pager.search_match.unwrap().0], "line 59");
        act(&mut app, Action::SearchNext);
        let Screen::Pager(pager) = app.screen() else {
            panic!("not in the pager");
        };
        assert_eq!(pager.lines[pager.search_match.unwrap().0], "line 58");
        assert_eq!(app.status, "Search wrapped to top.");
    }

    #[test]
    fn a_failed_flag_change_is_undone_for_a_hidden_message() {
        let mut app = app();
        let effects = act(&mut app, Action::FlagMessage);
        let Effect::Request(flag, _) = effects[0].clone() else {
            panic!("expected a flag update, got {effects:?}");
        };
        // Limit to Bob's messages: m0, being flagged, is hidden.
        act(&mut app, Action::Limit);
        answer(&mut app, "~f bob");
        respond(
            &mut app,
            flag,
            Event::FlagsUpdated(Err(mett_core::Error::Network("offline".into()))),
        );
        act(&mut app, Action::Limit);
        answer(&mut app, "all");
        assert!(!app.messages[0].flags.flagged, "back as in the mailbox");
    }

    #[test]
    fn lifting_a_limit_keeps_the_listing_order() {
        let mut app = app();
        // Same dates: the order comes from the listing, not the date.
        for m in &mut app.messages {
            m.received = DateTime::UNIX_EPOCH;
        }
        act(&mut app, Action::Limit);
        answer(&mut app, "~f alice");
        assert_eq!(ids(&app), ["m0", "m2", "m4"]);
        act(&mut app, Action::Limit);
        answer(&mut app, "all");
        assert_eq!(ids(&app), ["m0", "m1", "m2", "m3", "m4", "m5"]);
    }

    #[test]
    fn pager_search_finds_text_across_a_wrap() {
        let mut app = app();
        app.update(Msg::Resize {
            width: 20,
            height: 24,
        });
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a fetch, got {effects:?}");
        };
        let mut message = mock::message("m0", "in", "Lunch 0", DateTime::UNIX_EPOCH);
        message.body.content = "first line\nsome words before the important phrase\n".into();
        respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
        act(&mut app, Action::Search);
        answer(&mut app, "important phrase");
        assert_ne!(app.status, "Not found.");
        let Screen::Pager(pager) = app.screen() else {
            panic!("not in the pager");
        };
        assert!(pager.lines[pager.search_match.unwrap().0].contains("important phrase"));
    }

    #[test]
    fn retrying_a_save_reaches_a_hidden_message() {
        let mut app = app();
        let effects = act(&mut app, Action::FlagMessage);
        let Effect::Request(flag, _) = effects[0].clone() else {
            panic!("expected a flag update, got {effects:?}");
        };
        act(&mut app, Action::Limit);
        answer(&mut app, "~f bob");
        respond(
            &mut app,
            flag,
            Event::FlagsUpdated(Err(mett_core::Error::Auth("expired".into()))),
        );
        app.update(Msg::SignIn(crate::signin::SignInEvent::Finished {
            account: mett_core::AccountId::new("demo@example.com"),
            outcome: Err("expired".into()),
        }));
        let effects = act(&mut app, Action::Retry);
        assert!(
            effects.iter().any(|e| matches!(e, Effect::Request(_, crate::worker::Request::UpdateFlags(id, _)) if id.as_str() == "m0")),
            "{effects:?}"
        );
    }

    #[test]
    fn index_and_pager_searches_are_kept_apart() {
        let mut app = app();
        act(&mut app, Action::Search);
        answer(&mut app, "nobody");
        assert_eq!(app.status, "Not found.");
        act(&mut app, Action::Search);
        answer(&mut app, "~f bob");
        assert_eq!(app.selected, 1);
        assert_eq!(app.status, "", "the earlier Not found. is gone");

        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a fetch, got {effects:?}");
        };
        respond(
            &mut app,
            fetch,
            Event::MessageFetched(Ok(mock::message("m1", "in", "s", DateTime::UNIX_EPOCH))),
        );
        act(&mut app, Action::SearchNext);
        assert_eq!(app.status, "No search pattern.", "not the index's ~f bob");
        act(&mut app, Action::Search);
        let Some(Input::Line { editor, .. }) = &app.input else {
            panic!("no question");
        };
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn pager_search_shows_matches_deep_in_a_long_line() {
        let mut app = app();
        app.update(Msg::Resize {
            width: 20,
            height: 8,
        });
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a fetch, got {effects:?}");
        };
        let mut message = mock::message("m0", "in", "Lunch 0", DateTime::UNIX_EPOCH);
        // One line, many rows, with two matches far down it.
        message.body.content = format!("{} needle one needle two\nend\n", "word ".repeat(60));
        respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
        act(&mut app, Action::Search);
        answer(&mut app, "needle");
        let shown = |app: &App| {
            let Screen::Pager(pager) = app.screen() else {
                panic!("not in the pager");
            };
            pager.rows[pager.scroll..]
                .iter()
                .take(app.body_height)
                .cloned()
                .collect::<Vec<_>>()
                .join("|")
        };
        assert!(shown(&app).contains("needle"), "{}", shown(&app));
        // The second match in the same line, not the first again.
        act(&mut app, Action::SearchNext);
        let Screen::Pager(pager) = app.screen() else {
            panic!("not in the pager");
        };
        let (line, end) = pager.search_match.unwrap();
        assert!(pager.lines[line][..end].ends_with("needle one needle"));
        assert!(shown(&app).contains("two"), "{}", shown(&app));
        assert_eq!(app.status, "");
        act(&mut app, Action::SearchNext);
        assert_eq!(app.status, "Search wrapped to top.");
    }

    #[test]
    fn a_working_limit_clears_an_earlier_complaint() {
        let mut app = app();
        act(&mut app, Action::Limit);
        answer(&mut app, "~F");
        assert_eq!(app.status, "No messages matched the criteria.");
        act(&mut app, Action::Limit);
        answer(&mut app, "all");
        assert_eq!(app.status, "");
    }

    /// An app whose first page (m0 from Alice) is listed, with more to
    /// come; returns the app and the request for the next page.
    fn loading() -> (App, crate::worker::RequestId) {
        let (mut app, list) = opening(Settings::default(), 24);
        let mut first = mock::message("m0", "in", "Lunch", DateTime::UNIX_EPOCH).summary;
        first.from = Some(Address::new("alice@contoso.com"));
        let next = page(&mut app, list, vec![first], "c");
        (app, next)
    }

    fn bob() -> MessageSummary {
        let mut m = mock::message("m1", "in", "Report", DateTime::UNIX_EPOCH).summary;
        m.from = Some(Address::new("bob@fabrikam.com"));
        m
    }

    #[test]
    fn a_search_goes_on_as_pages_arrive() {
        let (mut app, next) = loading();
        act(&mut app, Action::Search);
        answer(&mut app, "~f bob");
        assert_eq!(app.status, "Searching...");
        listed(&mut app, next, vec![bob()], None);
        assert_eq!(app.selected, 1);
        assert_eq!(app.status, "");
    }

    #[test]
    fn a_limit_that_matched_nothing_yet_stops_saying_so() {
        let (mut app, next) = loading();
        act(&mut app, Action::Limit);
        answer(&mut app, "~f bob");
        assert_eq!(app.status, "No messages matched the criteria.");
        listed(&mut app, next, vec![bob()], None);
        assert_eq!(ids(&app), ["m1"]);
        assert_eq!(app.status, "");
    }

    #[test]
    fn empty_matches_at_line_ends_move_on() {
        let mut app = app();
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a fetch, got {effects:?}");
        };
        let mut message = mock::message("m0", "in", "Lunch 0", DateTime::UNIX_EPOCH);
        message.body.content = "one\ntwo\n".into();
        respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
        act(&mut app, Action::Search);
        answer(&mut app, "$");
        let line = |app: &App| {
            let Screen::Pager(pager) = app.screen() else {
                panic!("not in the pager");
            };
            pager.search_match.unwrap().0
        };
        let first = line(&app);
        act(&mut app, Action::SearchNext);
        assert_ne!(line(&app), first, "moved to another line");
    }

    #[test]
    fn a_new_pager_query_starts_on_screen() {
        let mut app = app();
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a fetch, got {effects:?}");
        };
        let mut message = mock::message("m0", "in", "Lunch 0", DateTime::UNIX_EPOCH);
        message.body.content = "apple banana apple banana\n".into();
        respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
        act(&mut app, Action::Search);
        answer(&mut app, "banana");
        act(&mut app, Action::Search);
        answer(&mut app, "apple");
        let Screen::Pager(pager) = app.screen() else {
            panic!("not in the pager");
        };
        let (line, end) = pager.search_match.unwrap();
        // The first apple on the line, not the one after the banana.
        assert!(
            pager.lines[line][..end].ends_with("apple") && end < 20,
            "{end}"
        );
    }

    #[test]
    fn a_search_reads_the_whole_folder_before_wrapping() {
        // m0 (alice) is selected and matches, but so may later pages.
        let (mut app, next) = loading();
        act(&mut app, Action::Search);
        answer(&mut app, "~f alice|~f bob");
        assert_eq!(app.status, "Searching...");
        assert_eq!(app.selected, 0);
        listed(&mut app, next, vec![bob()], None);
        assert_eq!(app.selected, 1, "the next match, not the wrap");
        act(&mut app, Action::SearchNext);
        assert_eq!(app.selected, 0);
        assert_eq!(app.status, "Search wrapped to top.");
    }

    #[test]
    fn a_pager_search_waits_for_the_message() {
        let mut app = app();
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a fetch, got {effects:?}");
        };
        act(&mut app, Action::Search);
        answer(&mut app, "line 1[0-9]");
        assert_eq!(app.status, "Searching...");
        let mut message = mock::message("m0", "in", "Lunch 0", DateTime::UNIX_EPOCH);
        message.body.content = (0..60).map(|i| format!("line {i}\n")).collect();
        respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
        let Screen::Pager(pager) = app.screen() else {
            panic!("not in the pager");
        };
        assert_eq!(pager.rows[pager.scroll], "line 10");
        assert_eq!(app.status, "");
    }

    #[test]
    fn the_pager_highlights_what_it_found() {
        let mut app = app();
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a fetch, got {effects:?}");
        };
        let mut message = mock::message("m0", "in", "Lunch 0", DateTime::UNIX_EPOCH);
        message.body.content = "one two one\n".into();
        respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
        act(&mut app, Action::Search);
        answer(&mut app, "one");
        let Screen::Pager(pager) = app.screen() else {
            panic!("not in the pager");
        };
        let found = highlighted(pager);
        assert_eq!(found.len(), 1);
        let (row, range) = &found[0];
        assert_eq!(&pager.rows[*row][range.clone()], "one");
        let first = range.start;
        act(&mut app, Action::SearchNext);
        let Screen::Pager(pager) = app.screen() else {
            panic!("not in the pager");
        };
        let (_, range) = &highlighted(pager)[0];
        assert!(range.start > first, "the second one");
    }

    #[test]
    fn a_resized_pager_is_searched_as_shown() {
        let mut app = app();
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a fetch, got {effects:?}");
        };
        let mut message = mock::message("m0", "in", "Lunch 0", DateTime::UNIX_EPOCH);
        message.body.content = format!("{} needle\n{}", "word ".repeat(80), "more\n".repeat(40));
        respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
        // Resized while something else covered the pager.
        app.width = 40;
        act(&mut app, Action::Search);
        answer(&mut app, "needle");
        let Screen::Pager(pager) = app.screen() else {
            panic!("not in the pager");
        };
        assert_eq!(pager.rows_width, Some(40));
        assert!(pager.rows[pager.scroll].contains("needle"));
    }

    #[test]
    fn a_search_waits_for_the_folder_to_open() {
        let inbox = crate::config::mailbox::FolderPath::WellKnown(WellKnownFolder::Inbox, vec![]);
        let (mut app, effects) = App::open(80, 24, inbox, Settings::default());
        let open = first_request(&effects);
        act(&mut app, Action::Search);
        answer(&mut app, "~f bob");
        assert_eq!(app.status, "Searching...");
        let effects = respond(
            &mut app,
            open,
            Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
        );
        let list = first_request(&effects);
        let mut alice = mock::message("m0", "in", "Lunch", DateTime::UNIX_EPOCH).summary;
        alice.from = Some(Address::new("alice@contoso.com"));
        listed(&mut app, list, vec![alice, bob()], None);
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn a_search_loads_a_page_that_failed() {
        let (mut app, next) = loading();
        respond(
            &mut app,
            next,
            Event::MessagesListed(Err(mett_core::Error::Network("offline".into()))),
        );
        act(&mut app, Action::Search);
        let effects = {
            if let Some(Input::Line { editor, .. }) = &mut app.input {
                *editor = LineEditor::new("");
            }
            for c in "~f bob".chars() {
                app.update(Msg::Key(KeyEvent::from(KeyCode::Char(c))));
            }
            app.update(Msg::Key(KeyEvent::from(KeyCode::Enter)))
        };
        let Some(Effect::Request(again, _)) = effects.first().cloned() else {
            panic!("expected the page again, got {effects:?}");
        };
        assert_eq!(app.status, "Searching...");
        listed(&mut app, again, vec![bob()], None);
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn a_search_stays_with_its_folder() {
        let (mut app, _) = loading();
        act(&mut app, Action::ChangeFolder);
        let effects = {
            if let Some(Input::Line { editor, .. }) = &mut app.input {
                *editor = LineEditor::new("");
            }
            for c in "=Archive".chars() {
                app.update(Msg::Key(KeyEvent::from(KeyCode::Char(c))));
            }
            app.update(Msg::Key(KeyEvent::from(KeyCode::Enter)))
        };
        let Some(Effect::Request(open, _)) = effects.first().cloned() else {
            panic!("expected a request, got {effects:?}");
        };
        // Searched while the other folder is being opened, and this one's
        // pages are still on their way.
        act(&mut app, Action::Search);
        answer(&mut app, "~f bob");
        assert_eq!(app.status, "Searching...");
        let effects = respond(
            &mut app,
            open,
            Event::FolderOpened(Ok(mock::folder("ar", "Archive", None))),
        );
        let Some(Effect::Request(list, _)) = effects.first().cloned() else {
            panic!("expected a request, got {effects:?}");
        };
        let mut other = mock::message("m9", "ar", "Old", DateTime::UNIX_EPOCH).summary;
        other.from = Some(Address::new("carol@contoso.com"));
        listed(&mut app, list, vec![other, bob()], None);
        assert_eq!(app.selected, 0, "the old search doesn't run here");
    }

    #[test]
    fn moving_on_stops_saying_searching() {
        let (mut app, _) = loading();
        act(&mut app, Action::Search);
        answer(&mut app, "~f bob");
        assert_eq!(app.status, "Searching...");
        act(&mut app, Action::DisplayMessage);
        assert_ne!(app.status, "Searching...");
    }

    #[test]
    fn a_widened_pager_is_searched_from_what_it_shows() {
        let mut app = app();
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a fetch, got {effects:?}");
        };
        let mut message = mock::message("m0", "in", "Lunch 0", DateTime::UNIX_EPOCH);
        message.body.content = (0..60)
            .map(|i| format!("row {i:02} {}\n", "x".repeat(140)))
            .collect();
        respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
        let Some(Screen::Pager(pager)) = app.screens.last_mut() else {
            panic!("not in the pager");
        };
        // At the end, then widened while another screen covered it.
        pager.scroll = pager.rows.len() - app.body_height;
        app.width = 200;
        act(&mut app, Action::Search);
        answer(&mut app, "row 4[0-9]");
        assert_eq!(app.status, "", "found below, without wrapping");
    }

    #[test]
    fn a_search_before_any_message_includes_the_first() {
        let inbox = crate::config::mailbox::FolderPath::WellKnown(WellKnownFolder::Inbox, vec![]);
        let (mut app, effects) = App::open(80, 24, inbox, Settings::default());
        let open = first_request(&effects);
        act(&mut app, Action::Search);
        answer(&mut app, "~f bob");
        let effects = respond(
            &mut app,
            open,
            Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
        );
        let list = first_request(&effects);
        let mut other = bob();
        other.id = "m2".into();
        listed(&mut app, list, vec![bob(), other], None);
        assert_eq!(app.selected, 0, "the first match");
        assert_eq!(app.status, "");
    }

    #[test]
    fn leaving_a_loading_message_stops_searching() {
        let mut app = app();
        act(&mut app, Action::DisplayMessage);
        act(&mut app, Action::Search);
        answer(&mut app, "anything");
        assert_eq!(app.status, "Searching...");
        act(&mut app, Action::Exit);
        assert_eq!(app.screen(), &Screen::Index);
        assert_ne!(app.status, "Searching...");
    }

    #[test]
    fn a_search_waits_for_a_page_through_a_sign_in() {
        let (mut app, next) = loading();
        act(&mut app, Action::Search);
        answer(&mut app, "~f bob");
        signed_out(&mut app);
        respond(
            &mut app,
            next,
            Event::MessagesListed(Err(mett_core::Error::Auth("expired".into()))),
        );
        let effects = app.retry();
        let again = effects
            .iter()
            .find_map(|e| match e {
                Effect::Request(id, crate::worker::Request::ListMessages { .. }) => Some(*id),
                _ => None,
            })
            .expect("the page again");
        listed(&mut app, again, vec![bob()], None);
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn a_pager_search_waits_for_its_message_through_a_sign_in() {
        let mut app = app();
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a fetch, got {effects:?}");
        };
        act(&mut app, Action::Search);
        answer(&mut app, "line 1[0-9]");
        signed_out(&mut app);
        respond(
            &mut app,
            fetch,
            Event::MessageFetched(Err(mett_core::Error::Auth("expired".into()))),
        );
        let effects = app.retry();
        let again = effects
            .iter()
            .find_map(|e| match e {
                Effect::Request(id, crate::worker::Request::FetchMessage(_)) => Some(*id),
                _ => None,
            })
            .expect("fetched again");
        let mut message = mock::message("m0", "in", "Lunch 0", DateTime::UNIX_EPOCH);
        message.body.content = (0..60).map(|i| format!("line {i}\n")).collect();
        respond(&mut app, again, Event::MessageFetched(Ok(message)));
        let Screen::Pager(pager) = app.screen() else {
            panic!("not in the pager");
        };
        assert_eq!(pager.rows[pager.scroll], "line 10");
    }

    #[test]
    fn next_in_an_empty_limited_index_takes_the_first_match() {
        let (mut app, next) = loading();
        act(&mut app, Action::Limit);
        answer(&mut app, "~f bob");
        assert!(app.messages.is_empty(), "alice is hidden");
        act(&mut app, Action::NextEntry);
        let mut second = bob();
        second.id = "m2".into();
        listed(&mut app, next, vec![bob(), second], None);
        assert_eq!(app.messages[app.selected].id.as_str(), "m1");
    }

    #[test]
    fn a_covered_pager_keeps_its_highlight_through_a_resize() {
        let mut app = app();
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a fetch, got {effects:?}");
        };
        let mut message = mock::message("m0", "in", "Lunch 0", DateTime::UNIX_EPOCH);
        message.body.content = format!("{} needle\n", "word ".repeat(30));
        respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
        act(&mut app, Action::Search);
        answer(&mut app, "needle");
        act(&mut app, Action::BrowseMailboxes);
        app.update(Msg::Resize {
            width: 50,
            height: 24,
        });
        let pager = app
            .screens
            .iter()
            .find_map(|s| match s {
                Screen::Pager(p) => Some(p),
                _ => None,
            })
            .unwrap();
        assert_eq!(pager.rows_width, Some(50));
        let found = highlighted(pager);
        assert_eq!(&pager.rows[found[0].0][found[0].1.clone()], "needle");
    }

    #[test]
    fn a_space_shown_after_a_wrap_is_highlighted_where_it_is() {
        let mut app = app();
        let effects = act(&mut app, Action::DisplayMessage);
        let Effect::Request(fetch, _) = effects[0].clone() else {
            panic!("expected a fetch, got {effects:?}");
        };
        let mut message = mock::message("m0", "in", "Lunch 0", DateTime::UNIX_EPOCH);
        message.body.content = format!("{}  y\n", "x".repeat(80));
        respond(&mut app, fetch, Event::MessageFetched(Ok(message)));
        act(&mut app, Action::Search);
        answer(&mut app, "x  y");
        act(&mut app, Action::Search);
        answer(&mut app, r"\s y");
        let Screen::Pager(pager) = app.screen() else {
            panic!("not in the pager");
        };
        let found = highlighted(pager);
        assert_eq!(found.len(), 1, "{found:?}");
        let (row, range) = &found[0];
        assert_eq!(&pager.rows[*row][range.clone()], " y");
    }

    #[test]
    fn a_search_waits_for_the_folder_through_a_sign_in() {
        let inbox = crate::config::mailbox::FolderPath::WellKnown(WellKnownFolder::Inbox, vec![]);
        let (mut app, effects) = App::open(80, 24, inbox, Settings::default());
        let open = first_request(&effects);
        act(&mut app, Action::Search);
        answer(&mut app, "~f bob");
        signed_out(&mut app);
        respond(
            &mut app,
            open,
            Event::FolderOpened(Err(mett_core::Error::Auth("expired".into()))),
        );
        let (again, _) = request_of(&app.retry());
        let effects = respond(
            &mut app,
            again,
            Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
        );
        let (list, _) = request_of(&effects);
        let mut alice = mock::message("m0", "in", "Lunch", DateTime::UNIX_EPOCH).summary;
        alice.from = Some(Address::new("alice@contoso.com"));
        listed(&mut app, list, vec![alice, bob()], None);
        assert_eq!(app.selected, 1);
    }

    fn request_of(effects: &[Effect]) -> (crate::worker::RequestId, crate::worker::Request) {
        effects
            .iter()
            .find_map(|e| match e {
                Effect::Request(id, request) => Some((*id, request.clone())),
                _ => None,
            })
            .expect("a request")
    }

    #[test]
    fn next_in_an_empty_limited_index_keeps_waiting_through_empty_pages() {
        let (mut app, next) = loading();
        act(&mut app, Action::Limit);
        answer(&mut app, "~f bob");
        act(&mut app, Action::NextEntry);
        // A page without a match: still nothing to move to.
        let mut alice = mock::message("m5", "in", "Lunch", DateTime::UNIX_EPOCH).summary;
        alice.from = Some(Address::new("alice@contoso.com"));
        let effects = listed(&mut app, next, vec![alice], Some("d"));
        assert!(app.pending.is_some(), "still waiting");
        let Some(Effect::Request(last, _)) = effects.first().cloned() else {
            panic!("expected the next page, got {effects:?}");
        };
        listed(&mut app, last, vec![bob()], None);
        assert_eq!(app.messages[app.selected].id.as_str(), "m1");
        assert!(app.pending.is_none());
    }

    #[test]
    fn a_relist_keeps_the_limit_s_messages() {
        let mut app = app();
        act(&mut app, Action::Limit);
        answer(&mut app, "~N");
        let shown = ids(&app)
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>();
        assert_eq!(shown, ["m0", "m1"]);
        // m0 is read now; a relist (say, new mail) brings it back read.
        let effects = app.relist();
        let Some(Effect::Request(list, _)) = effects.first().cloned() else {
            panic!("expected a request, got {effects:?}");
        };
        let messages = (0..6)
            .map(|i| {
                let mut m =
                    mock::message(&format!("m{i}"), "in", "s", DateTime::UNIX_EPOCH).summary;
                m.flags.read = i != 1;
                m
            })
            .collect();
        listed(&mut app, list, messages, None);
        assert_eq!(ids(&app), ["m0", "m1"], "still shown, as in mutt");
    }
}
