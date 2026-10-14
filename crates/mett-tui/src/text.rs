//! Turning message bodies into terminal text.
//!
//! This is a minimal built-in renderer. The `Renderer` strategy from
//! `docs/DESIGN.md` §5.7 (for example, an external mailcap command) will
//! replace it.

use chrono::Local;
use mett_core::{Address, Body, BodyKind, Message};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// The lines the pager shows for `message`: the headers mutt shows by
/// default, a blank line, then the body. They are not wrapped yet; see
/// [`wrap`].
pub fn pager_lines(message: &Message) -> Vec<String> {
    let addresses = |list: &[Address]| {
        list.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let summary = &message.summary;
    let mut lines = vec![format!(
        "Date: {}",
        summary
            .received
            .with_timezone(&Local)
            .format("%a, %d %b %Y %H:%M:%S %z")
    )];
    if let Some(from) = &summary.from {
        lines.push(format!("From: {from}"));
    }
    if !summary.to.is_empty() {
        lines.push(format!("To: {}", addresses(&summary.to)));
    }
    if !message.cc.is_empty() {
        lines.push(format!("Cc: {}", addresses(&message.cc)));
    }
    lines.push(format!("Subject: {}", summary.subject));
    lines.push(String::new());
    lines.extend(body_lines(&message.body));
    lines
}

/// Wraps every line to `width` terminal cells.
pub fn wrap_all(lines: &[String], width: usize) -> Vec<String> {
    lines.iter().flat_map(|line| wrap(line, width)).collect()
}

/// Wraps `line` into rows of at most `width` terminal cells, breaking after
/// a space where possible. Text is measured and split in grapheme clusters
/// (what the terminal shows as one character, such as an emoji made of
/// several code points), and widths are display cells, so wide (CJK, emoji)
/// characters count double.
pub fn wrap(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![line.to_owned()];
    }
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut row_width = 0;
    // Byte offset in `row` just after its last space.
    let mut break_at = None;
    for g in line.graphemes(true) {
        let w = g.width();
        if row_width + w > width && !row.is_empty() {
            if g == " " {
                // The row ends at this space, which is dropped.
                rows.push(std::mem::take(&mut row));
                row_width = 0;
                break_at = None;
                continue;
            }
            // Move the last word to a new row, unless only indentation
            // comes before it (which would leave a blank row).
            if let Some(i) = break_at.filter(|&i| !row[..i].trim().is_empty()) {
                let rest = row.split_off(i);
                rows.push(row.trim_end().to_owned());
                row_width = rest.width();
                row = rest;
            }
            break_at = None;
            // Still too wide (no break point, or the word itself is too
            // long): break right here.
            if row_width + w > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                row_width = 0;
            }
        }
        row.push_str(g);
        row_width += w;
        if g == " " {
            break_at = Some(row.len());
        }
    }
    rows.push(row);
    rows
}

/// Truncates `text` to at most `width` terminal cells, without splitting a
/// grapheme cluster.
pub fn truncate(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for g in text.graphemes(true) {
        let w = g.width();
        if used + w > width {
            break;
        }
        out.push_str(g);
        used += w;
    }
    out
}

/// Truncates or pads `text` to exactly `width` terminal cells.
pub fn fit(text: &str, width: usize) -> String {
    let mut out = truncate(text, width);
    let used = out.width();
    out.extend(std::iter::repeat_n(' ', width - used));
    out
}

/// The body as plain-text lines.
pub fn body_lines(body: &Body) -> Vec<String> {
    let text = match body.kind {
        BodyKind::Text => body.content.replace("\r\n", "\n"),
        BodyKind::Html => html_to_text(&body.content),
    };
    // Tabs would be dropped by the terminal buffer, so expand them.
    text.lines().map(expand_tabs).collect()
}

/// Replaces tabs with spaces up to the next 8-column tab stop, counting
/// columns in display cells.
fn expand_tabs(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut column = 0;
    for g in line.graphemes(true) {
        if g == "\t" {
            let n = 8 - column % 8;
            out.extend(std::iter::repeat_n(' ', n));
            column += n;
        } else {
            out.push_str(g);
            column += g.width();
        }
    }
    out
}

/// A crude HTML-to-text conversion: drops tags, `<script>` and `<style>`
/// contents and comments, turns block elements into line breaks and decodes
/// the common entities.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&decode_entities(&rest[..start]));
        rest = &rest[start..];
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.find("-->").map_or("", |end| &after[end + 3..]);
            continue;
        }
        // A `<` only starts a tag if a name, `/` or `!` follows it;
        // otherwise it is text, as in `2 < 3`.
        if !rest[1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!')
        {
            out.push('<');
            rest = &rest[1..];
            continue;
        }
        let Some(end) = rest.find('>') else {
            rest = "";
            break;
        };
        let tag = rest[1..end].trim().to_ascii_lowercase();
        rest = &rest[end + 1..];
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        match name {
            "script" | "style" if !tag.starts_with('/') => {
                let close = format!("</{name}");
                rest = find_ignore_case(rest, &close)
                    .and_then(|i| rest[i..].find('>').map(|j| &rest[i + j + 1..]))
                    .unwrap_or("");
            }
            "br" | "p" | "div" | "tr" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
            | "blockquote" | "table"
                if !out.ends_with('\n') =>
            {
                out.push('\n');
            }
            _ => {}
        }
    }
    out.push_str(&decode_entities(rest));
    collapse_blank_lines(&out)
}

fn find_ignore_case(haystack: &str, needle: &str) -> Option<usize> {
    haystack.to_ascii_lowercase().find(needle)
}

fn decode_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Trims each line and keeps at most one blank line in a row.
fn collapse_blank_lines(text: &str) -> String {
    let mut out = Vec::new();
    for line in text.lines().map(str::trim_end) {
        if line.trim().is_empty() && out.last().is_none_or(|l: &&str| l.trim().is_empty()) {
            continue;
        }
        out.push(line);
    }
    out.join("\n").trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(content: &str) -> Vec<String> {
        body_lines(&Body {
            kind: BodyKind::Html,
            content: content.into(),
        })
    }

    #[test]
    fn wraps_at_spaces_by_display_width() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 4), [""]);
        // Each of these characters is two cells wide.
        assert_eq!(wrap("日本語です", 4), ["日本", "語で", "す"]);
        // After moving a word, a wide character must still fit.
        assert_eq!(wrap("ab cdef界", 5), ["ab", "cdef", "界"]);
        // Indentation alone doesn't make a blank row.
        assert_eq!(wrap(" bbbb界", 5), [" bbbb", "界"]);
        // A multi-code-point emoji is one two-cell grapheme.
        assert_eq!(wrap("ab👩‍💻", 4), ["ab👩‍💻"]);
        assert_eq!(wrap("abc👩‍💻", 4), ["abc", "👩‍💻"]);
        for row in wrap("aa bb 界界界 cc 日本語 d", 5) {
            assert!(row.width() <= 5, "{row:?} is too wide");
        }
    }

    #[test]
    fn fits_text_in_cells() {
        assert_eq!(fit("Alice", 7), "Alice  ");
        assert_eq!(fit("日本語", 5), "日本 ");
        assert_eq!(truncate("Alice Example", 5), "Alice");
        assert_eq!(truncate("a👩‍💻b", 2), "a");
        assert_eq!(truncate("a👩‍💻b", 3), "a👩‍💻");
    }

    #[test]
    fn tabs_are_expanded_to_tab_stops() {
        let body = Body {
            kind: BodyKind::Text,
            content: "a\tb\n\tindented\n日本\tx".into(),
        };
        assert_eq!(
            body_lines(&body),
            ["a       b", "        indented", "日本    x"]
        );
    }

    #[test]
    fn plain_text_is_split_into_lines() {
        let body = Body {
            kind: BodyKind::Text,
            content: "one\r\ntwo\n".into(),
        };
        assert_eq!(body_lines(&body), ["one", "two"]);
    }

    #[test]
    fn html_tags_become_text_and_line_breaks() {
        assert_eq!(
            html(
                "<html><body><p>Hi <b>Alice</b>,</p><p>Fish &amp; chips?<br>Bob</p></body></html>"
            ),
            ["Hi Alice,", "Fish & chips?", "Bob"]
        );
    }

    #[test]
    fn html_keeps_literal_less_than_signs() {
        assert_eq!(html("<p>2 < 3 and 4 <5</p>"), ["2 < 3 and 4 <5"]);
    }

    #[test]
    fn html_scripts_styles_and_comments_are_dropped() {
        assert_eq!(
            html("<style>p { color: red }</style><!-- hidden --><p>shown</p><SCRIPT>x()</SCRIPT>"),
            ["shown"]
        );
    }
}
