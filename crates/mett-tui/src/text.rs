//! Turning message bodies into terminal text.
//!
//! This is a minimal built-in renderer. The `Renderer` strategy from
//! `docs/DESIGN.md` §5.7 (for example, an external mailcap command) will
//! replace it.

use mett_core::{Body, BodyKind};

/// The body as plain-text lines.
pub fn body_lines(body: &Body) -> Vec<String> {
    let text = match body.kind {
        BodyKind::Text => body.content.replace("\r\n", "\n"),
        BodyKind::Html => html_to_text(&body.content),
    };
    text.lines().map(str::to_owned).collect()
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
    fn html_scripts_styles_and_comments_are_dropped() {
        assert_eq!(
            html("<style>p { color: red }</style><!-- hidden --><p>shown</p><SCRIPT>x()</SCRIPT>"),
            ["shown"]
        );
    }
}
