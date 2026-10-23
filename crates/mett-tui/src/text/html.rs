//! A crude HTML-to-text conversion.

use std::borrow::Cow;

use super::{LINK_START, LINK_URL, scheme_len};

/// A crude HTML-to-text conversion: drops tags, `<script>` and `<style>`
/// contents and comments, turns block elements into line breaks and decodes
/// the common entities.
pub(super) fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    // The link being read: its URL, and where its text starts in `out`.
    let mut anchor: Option<(String, usize)> = None;
    while let Some(start) = rest.find('<') {
        push_text(&mut out, &rest[..start]);
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
        let Some(end) = tag_end(rest) else {
            rest = "";
            break;
        };
        let raw = &rest[1..end];
        let tag = raw.trim().to_ascii_lowercase();
        rest = &rest[end + 1..];
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        match name {
            "a" if !tag.starts_with('/') => {
                if let Some((_, start)) = anchor.take() {
                    // A link in a link: the outer one is left out.
                    out.remove(start);
                }
                // What would end a URL in the text is percent-encoded.
                anchor = attribute(raw, "href")
                    .map(|href| {
                        decode_entities(href.trim())
                            .replace(' ', "%20")
                            .replace('\t', "%09")
                            .replace('\n', "%0A")
                            .replace('\r', "%0D")
                            .replace('<', "%3C")
                            .replace('>', "%3E")
                    })
                    .filter(|href| {
                        scheme_len(href).is_some() && !href.contains([LINK_START, LINK_URL])
                    })
                    .map(|href| (href, out.len()));
                if anchor.is_some() {
                    out.push(LINK_START);
                }
            }
            // The link's URL follows its text, as Exchange writes links in
            // text bodies, unless the text is the URL. A link without text
            // (an image's) is left out.
            "a" => {
                if let Some((href, start)) = anchor.take() {
                    let text = out[start + LINK_START.len_utf8()..].trim();
                    if !text.is_empty() && text != href {
                        // Spaces after the text stay after the URL.
                        let spaces = out[out.trim_end().len()..].to_owned();
                        out.truncate(out.trim_end().len());
                        out.push(LINK_URL);
                        out.push_str(&href);
                        out.push('>');
                        out.push_str(&spaces);
                    } else {
                        out.remove(start);
                    }
                }
            }
            // Separate table cells, so they don't run together.
            "td" | "th" if !tag.starts_with('/') && !out.is_empty() && !out.ends_with('\n') => {
                out.push('\t');
            }
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
    push_text(&mut out, rest);
    if let Some((_, start)) = anchor {
        // A link never closed has no URL to follow it.
        out.remove(start);
    }
    collapse_blank_lines(&settle_marks(&out))
}

/// Adds the text between tags, `html`, to `out`, decoded. It can't have
/// link marks: a [`LINK_START`] or [`LINK_URL`] in it is shown as U+FFFD.
fn push_text(out: &mut String, html: &str) {
    let text = decode_entities(html);
    if text.contains([LINK_START, LINK_URL]) {
        out.push_str(&text.replace([LINK_START, LINK_URL], "\u{FFFD}"));
    } else {
        out.push_str(&text);
    }
}

/// Moves each link mark in `text` past the spaces and line breaks after
/// it, to the text it marks, so that it is never alone on a line.
fn settle_marks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending = false;
    for c in text.chars() {
        if c == LINK_START {
            pending = true;
            continue;
        }
        if pending && !c.is_whitespace() {
            out.push(LINK_START);
            pending = false;
        }
        out.push(c);
    }
    out
}

/// The offset of the `>` that closes the tag at the start of `html`,
/// skipping any `>` inside quoted attribute values.
fn tag_end(html: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in html.char_indices().skip(1) {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '>' => return Some(i),
            None => {}
        }
    }
    None
}

/// The value of attribute `name` (in lowercase ASCII) in `tag`, the text
/// between a tag's angle brackets, quoted or not.
pub(super) fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = tag.split_once(char::is_whitespace)?.1;
    loop {
        rest = rest.trim_start();
        let key_end = rest.find(|c: char| c == '=' || c == '/' || c.is_whitespace())?;
        let key = &rest[..key_end];
        rest = rest[key_end..].trim_start();
        let Some(after) = rest.strip_prefix('=') else {
            // An attribute without a value.
            rest = rest.strip_prefix('/').unwrap_or(rest);
            if rest.is_empty() {
                return None;
            }
            continue;
        };
        let after = after.trim_start();
        let (value, next) = match after.chars().next() {
            Some(q @ ('"' | '\'')) => {
                let end = after[1..].find(q).map_or(after.len(), |i| i + 1);
                (&after[1..end], after.get(end + 1..).unwrap_or(""))
            }
            _ => {
                let end = after.find(char::is_whitespace).unwrap_or(after.len());
                (&after[..end], &after[end..])
            }
        };
        if key.eq_ignore_ascii_case(name) {
            return Some(value);
        }
        rest = next;
    }
}

/// Where `needle` (in lowercase ASCII) first is in `haystack`, in any
/// case, without lowercasing the rest of the document each time.
fn find_ignore_case(haystack: &str, needle: &str) -> Option<usize> {
    let needle = needle.as_bytes();
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

fn decode_entities(text: &str) -> Cow<'_, str> {
    // Most text between tags has no entities at all.
    if !text.contains('&') {
        return Cow::Borrowed(text);
    }
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
        .into()
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
