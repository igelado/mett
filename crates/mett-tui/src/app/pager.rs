//! The pager: a message laid out in lines, wrapped and scrolled.

use super::*;

/// Pager state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pager {
    /// The message being shown.
    pub id: MessageId,
    /// The message, once fetched.
    pub message: Option<Message>,
    /// The message laid out as lines (headers and body), not yet wrapped.
    pub lines: Vec<String>,
    /// The width `lines` were wrapped to, if they have been, in
    /// [`Pager::line_rows`] and [`Pager::row_ranges`]. Kept, so redrawing
    /// and scrolling don't re-wrap the whole message.
    pub rows_width: Option<usize>,
    /// The request fetching it.
    pub request: RequestId,
    /// The first visible row, after wrapping.
    pub scroll: usize,
    /// The user set the message read or unread while it was loading:
    /// showing it doesn't mark it read.
    pub read_chosen: bool,
    /// For each of `lines`, its first row, wrapped.
    pub line_rows: Vec<usize>,
    /// For each row, wrapped, the bytes of its line it shows.
    pub row_ranges: Vec<std::ops::Range<usize>>,
    /// Where the last search match ended, as a line and a byte offset in
    /// it: the next search starts there. (The match may be shown lower
    /// down when it is near the end.)
    pub search_match: Option<(usize, usize)>,
    /// The text the last search found, as a line and a byte range in it,
    /// shown highlighted.
    pub highlight: Option<(usize, std::ops::Range<usize>)>,
    /// A search made before the message arrived, to run when it does.
    pub pending_search: Option<String>,
    /// The first line of the body, after the headers and the blank line.
    pub body_start: usize,
    /// The line the signature starts at (`-- `), if there is one.
    pub signature: Option<usize>,
    /// The links in `lines`, drawn underlined.
    pub links: Vec<text::Link>,
    /// Links show only their text, not their URLs (`collapse_links`, until
    /// `toggle-links`).
    pub collapse_links: bool,
}

impl Pager {
    /// Lays out `message` to show, wrapped to `width`.
    pub(super) fn show(&mut self, message: &Message, width: usize) {
        (self.lines, self.links) = text::pager_view(message, self.collapse_links);
        self.body_start = self
            .lines
            .iter()
            .position(String::is_empty)
            .map_or(self.lines.len(), |blank| blank + 1);
        self.signature = self.lines[self.body_start..]
            .iter()
            .position(|line| line == "-- ")
            .map(|at| self.body_start + at);
        self.rows_width = None;
        self.layout(width);
    }

    /// Shows the message's links collapsed or not, keeping the line at the
    /// top of the screen there.
    pub(super) fn collapse_links(&mut self, collapse: bool, width: usize) {
        if self.collapse_links == collapse {
            return;
        }
        self.collapse_links = collapse;
        let Some(message) = self.message.take() else {
            // It is laid out as chosen once it arrives.
            return;
        };
        let top = (self.scroll < self.row_count()).then(|| self.line_of(self.scroll));
        self.show(&message, width);
        self.message = Some(message);
        if let Some(line) = top {
            self.scroll = self.line_rows[line];
        }
        // Where the last search found text has moved.
        self.search_match = None;
        self.highlight = None;
    }

    /// Re-wraps the message if the width changed.
    fn layout(&mut self, width: usize) {
        if self.rows_width != Some(width) {
            self.line_rows.clear();
            self.row_ranges.clear();
            for line in &self.lines {
                self.line_rows.push(self.row_ranges.len());
                self.row_ranges.extend(text::wrap_ranges(line, width));
            }
            self.rows_width = Some(width);
        }
    }

    /// Wraps the message to `width`, and keeps the scroll position within
    /// it for a screen `height` rows tall.
    pub(super) fn fit(&mut self, width: usize, height: usize) {
        self.layout(width);
        self.scroll = self.scroll.min(self.row_count().saturating_sub(height));
    }

    /// How many rows the message takes, wrapped.
    pub fn row_count(&self) -> usize {
        self.row_ranges.len()
    }

    /// The line that wrapped row `row` is part of.
    pub fn line_of(&self, row: usize) -> usize {
        self.line_rows.partition_point(|&first| first <= row) - 1
    }

    /// The text of wrapped row `row`.
    #[cfg(test)]
    pub fn row(&self, row: usize) -> &str {
        &self.lines[self.line_of(row)][self.row_ranges[row].clone()]
    }
}
