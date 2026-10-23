//! Searching (`/`, `n`) and limiting (`l`) the index with mutt's patterns,
//! and searching the text in the pager.

use super::StatusKind;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
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
        .unwrap_or(pager.row_count());
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
                self.status.say("Searching...", StatusKind::Searching);
                return Vec::new();
            }
            let position = self.screens.len() - 1;
            self.search_pager(position, text);
            return Vec::new();
        }
        match Pattern::parse(text) {
            Ok(pattern) => self.search_index(text, pattern),
            Err(e) => {
                self.fail(e.to_string());
                Vec::new()
            }
        }
    }

    /// Finds the next message of the index matching `pattern`, parsed
    /// from `text`.
    fn search_index(&mut self, text: &str, pattern: Pattern) -> Vec<Effect> {
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
                self.status.clear();
                if i <= self.selected && !from_top {
                    self.status.info("Search wrapped to top.");
                }
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
                self.pending_search = Some((text.to_owned(), pattern));
                self.search_from_top = from_top;
                let effects = self.retry_listing();
                self.status.say("Searching...", StatusKind::Searching);
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
            Some((text, pattern)) => self.search_index(&text, pattern),
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
        let (width, height) = (self.wrap_width(), self.pager_height());
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
                self.status.clear();
                if wrapped {
                    self.status.info("Search wrapped to top.");
                }
                // Show the row with the match at the top, unless that would
                // scroll past the end.
                let row = match_row(pager, line, start);
                pager.scroll = row.min(pager.row_count().saturating_sub(height));
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
            deleted: pattern.asks_deleted() && self.deleted.contains(&message.id),
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
        all.sort_by_cached_key(|m| self.positions.get(&m.id).copied().unwrap_or(usize::MAX));
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
                self.status.say(NO_MATCHES, StatusKind::NoMatches);
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
        // The first page starts at the top, wherever its first message
        // goes.
        let first = self.messages.is_empty() && self.hidden.is_empty() && self.folded.is_empty();
        if first {
            self.listed_twice = false;
        }
        for message in &messages {
            let next = self.positions.len();
            match self.positions.entry(message.id.clone()) {
                Entry::Occupied(_) => self.listed_twice = true,
                Entry::Vacant(entry) => {
                    entry.insert(next);
                }
            }
        }
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
        if limited && !self.messages.is_empty() && self.status.is(StatusKind::NoMatches) {
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
mod tests;
