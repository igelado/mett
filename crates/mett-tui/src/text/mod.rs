//! Turning message bodies into terminal text.
//!
//! This is a minimal built-in renderer. The `Renderer` strategy from
//! `docs/DESIGN.md` §5.7 (for example, an external mailcap command) will
//! replace it.

mod html;
mod wrap;

use std::borrow::Cow;
use std::ops::Range;

use chrono::{DateTime, Duration, Local, Utc};
use mett_core::{Address, Body, BodyKind, Invitation, InvitationKind, Message};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use self::html::html_to_text;
pub use self::wrap::{truncate, wrap, wrap_all, wrap_ranges};

/// A link in the pager's text: the bytes of a line drawn underlined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// The line it is on.
    pub line: usize,
    /// The bytes of the line it takes.
    pub range: Range<usize>,
}

/// The lines the pager shows for `message`: the headers mutt shows by
/// default, a blank line, then the body, not wrapped yet (see [`wrap`]);
/// and the body's links. With `collapse`, a link
/// written as its text followed by the URL in angle brackets
/// (`text<https://...>`, as Exchange writes them in text bodies, and as
/// [`body_lines`] writes HTML links) shows only its text; otherwise the
/// URLs are shown. Either way, what shows each link is underlined: its
/// text when collapsed, or the URL.
pub fn pager_view(message: &Message, collapse: bool) -> (Vec<String>, Vec<Link>) {
    let mut lines = header_lines(message, true);
    if let Some(invitation) = &message.invitation {
        lines.extend(invitation_lines(invitation));
    }
    lines.push(String::new());
    let mut links = Vec::new();
    let marks = message.body.kind == BodyKind::Html;
    // Where the text of an HTML link starts, while its URL is on a line
    // still to come: as a line and a byte offset in it.
    let mut open: Option<(usize, usize)> = None;
    for line in body_text(&message.body).lines() {
        let i = lines.len();
        let found = find_links(line, collapse, marks, open.is_some());
        if found.ended_open {
            // When collapsed, its text on the lines before is underlined.
            if let Some((first, start)) = open.take().filter(|_| collapse) {
                for (at, text) in lines.iter().enumerate().skip(first) {
                    let start = if at == first { start } else { 0 };
                    if start < text.len() {
                        links.push(Link {
                            line: at,
                            range: start..text.len(),
                        });
                    }
                }
            }
        }
        if let Some(start) = found.opened {
            open = Some((i, start));
        }
        links.extend(found.links.into_iter().map(|range| Link { line: i, range }));
        lines.push(found.shown);
    }
    links.sort_by_key(|link| (link.line, link.range.start));
    (lines, links)
}

/// Where an HTML link's text starts, in the text [`body_text`] makes of
/// HTML (a private-use character, which nothing else shows): text bodies
/// don't say where a link's text starts.
const LINK_START: char = '\u{E000}';

/// What opens an HTML link's URL after its text, in place of `<`, in the
/// text [`body_text`] makes of HTML: a `<https://...>` in the link's text
/// isn't its URL.
const LINK_URL: char = '\u{E001}';

/// The URL schemes links are found with.
const SCHEMES: &[&str] = &["https://", "http://", "ftp://", "mailto:"];

/// How long the URL scheme at the start of `text` is, if one is there.
fn scheme_len(text: &str) -> Option<usize> {
    SCHEMES
        .iter()
        .find(|scheme| {
            text.get(..scheme.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
        })
        .map(|scheme| scheme.len())
}

/// What [`find_links`] found in a line.
struct Found {
    /// The line as shown: its links collapsed (with `collapse`) and its
    /// tabs expanded, so that tab stops count what is shown.
    shown: String,
    /// The bytes of `shown` that show links.
    links: Vec<Range<usize>>,
    /// The line ends the link whose text started on a line before (which
    /// is underlined too, when collapsed).
    ended_open: bool,
    /// Where the text of a link whose URL is on a line still to come
    /// starts.
    opened: Option<usize>,
}

/// Finds the links in `line`: see [`pager_view`]. With `marks`, the line is
/// from an HTML body, where [`LINK_START`] marks where each link's text
/// starts; with `open`, a link's text started on a line before.
fn find_links(line: &str, collapse: bool, marks: bool, open: bool) -> Found {
    let mut out = String::with_capacity(line.len());
    let mut links: Vec<Range<usize>> = Vec::new();
    let mut rest = line;
    // Where the text of the link whose URL comes next starts, if a mark
    // said so.
    let mut text_start = None;
    let mut open = open;
    let mut ended_open = false;
    // The display column at a byte offset of `out`, to expand tabs without
    // measuring the whole line each time.
    let mut measured = (0, 0);
    while let Some(c) = rest.chars().next() {
        if marks && c == LINK_START {
            text_start = Some(out.len());
            open = false;
            rest = &rest[c.len_utf8()..];
            continue;
        }
        // A URL in angle brackets: `<https://...>`, with no space in it.
        // In HTML, only a link's own URL (after LINK_URL) is one.
        let generated = marks && c == LINK_URL;
        let bracketed = rest
            .strip_prefix(['<', LINK_URL])
            .filter(|_| generated || (c == '<' && !marks))
            .filter(|after| scheme_len(after).is_some())
            .and_then(|after| {
                let end = after.find(['>', '<', ' ', '\t'])?;
                after[end..].starts_with('>').then_some(end)
            });
        if let Some(len) = bracketed {
            let rest_url = &rest[c.len_utf8()..];
            // The link's text: from where an HTML link said it starts
            // (maybe on a line before), or else the word right before it,
            // without opening punctuation.
            let earlier = text_start.is_none() && std::mem::take(&mut open);
            ended_open |= earlier;
            let start = match text_start.take() {
                Some(start) => start,
                None if earlier => 0,
                None => {
                    let word = out
                        .char_indices()
                        .rfind(|(_, c)| c.is_whitespace())
                        .map_or(0, |(i, c)| i + c.len_utf8());
                    out.len() - out[word..].trim_start_matches(['(', '[', '"', '\'']).len()
                }
            };
            // Without text (as in `(<https://...>)`), the URL is shown.
            if collapse && (start < out.len() || earlier) {
                // The text may be a URL found already, as in
                // `https://a<https://b>`: then it is underlined already.
                let found = links
                    .last()
                    .is_some_and(|l| l.start <= start && l.end == out.len());
                if start < out.len() && !found {
                    links.push(start..out.len());
                }
            } else {
                out.push('<');
                links.push(out.len()..out.len() + len);
                out.push_str(&rest_url[..len]);
                out.push('>');
            }
            rest = &rest_url[len + 1..];
            continue;
        }
        // A URL on its own, at the start of a word.
        if scheme_len(rest).is_some()
            && out.chars().next_back().is_none_or(|c| !c.is_alphanumeric())
        {
            // A link's mark ends it too, in HTML.
            let end = rest
                .find(|c: char| {
                    c.is_whitespace()
                        || matches!(c, '<' | '>' | '"')
                        || (marks && matches!(c, LINK_START | LINK_URL))
                })
                .unwrap_or(rest.len());
            // Punctuation after a URL usually ends the sentence.
            let url = rest[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '\'']);
            if url.len() > scheme_len(url).unwrap_or(0) {
                links.push(out.len()..out.len() + url.len());
                out.push_str(url);
                rest = &rest[url.len()..];
                continue;
            }
        }
        if c == '\t' {
            // Tabs would be dropped by the terminal buffer, so expand them.
            let column = measured.1 + out[measured.0..].width();
            let spaces = 8 - column % 8;
            out.extend(std::iter::repeat_n(' ', spaces));
            measured = (out.len(), column + spaces);
        } else {
            out.push(c);
        }
        rest = &rest[c.len_utf8()..];
    }
    Found {
        shown: out,
        links,
        ended_open,
        opened: text_start,
    }
}

/// The lines the pager shows for a meeting message, as headers: what the
/// message is, when and where the meeting is, who is in it, and how the
/// owner answered.
pub fn invitation_lines(invitation: &Invitation) -> Vec<String> {
    let kind = match invitation.kind {
        InvitationKind::Request => "invitation",
        InvitationKind::Update => "updated invitation",
        InvitationKind::Cancellation => "cancelled",
        InvitationKind::Accepted => "an attendee accepted",
        InvitationKind::Tentative => "an attendee tentatively accepted",
        InvitationKind::Declined => "an attendee declined",
    };
    let outdated = if invitation.out_of_date {
        " (out of date)"
    } else {
        ""
    };
    let mut lines = vec![format!("Meeting: {kind}{outdated}")];
    if let Some(when) = meeting_time(invitation) {
        lines.push(format!("When: {when}"));
    }
    if let Some(recurrence) = &invitation.recurrence {
        lines.push(format!("Repeats: {recurrence}"));
    }
    if let Some(location) = &invitation.location {
        lines.push(format!("Where: {location}"));
    }
    if let Some(organizer) = &invitation.organizer {
        lines.push(format!("Organizer: {organizer}"));
    }
    if !invitation.attendees.is_empty() {
        let attendees: Vec<_> = invitation
            .attendees
            .iter()
            .map(ToString::to_string)
            .collect();
        lines.push(format!("Attendees: {}", attendees.join(", ")));
    }
    if invitation.kind.is_open() {
        lines.push(format!("Your reply: {}", invitation.reply));
    }
    lines
}

/// When a meeting is, in the local time zone (a meeting of whole days is
/// by its dates).
fn meeting_time(invitation: &Invitation) -> Option<String> {
    let day = "%a %d %b %Y";
    let start = invitation.start?;
    if invitation.all_day {
        let first = start.date_naive();
        // The end is the midnight that ends the last day.
        let last = invitation
            .end
            .map_or(first, |end| (end - Duration::seconds(1)).date_naive())
            .max(first);
        return Some(if first == last {
            format!("{} (all day)", first.format(day))
        } else {
            format!("{} - {} (all day)", first.format(day), last.format(day))
        });
    }
    let local = |time: DateTime<Utc>| time.with_timezone(&Local);
    let from = local(start);
    let zone = from.format("%Z").to_string();
    let Some(end) = invitation.end.map(local) else {
        return Some(format!("{} {zone}", from.format(&format!("{day} %H:%M"))));
    };
    // A meeting can end after the clocks change.
    let end_zone = end.format("%Z").to_string();
    let first = from.format(&format!("{day} %H:%M")).to_string();
    Some(if end_zone != zone {
        format!(
            "{first} {zone} - {} {end_zone}",
            end.format(&format!("{day} %H:%M"))
        )
    } else if from.date_naive() == end.date_naive() {
        format!("{first} - {} {zone}", end.format("%H:%M"))
    } else {
        format!("{first} - {} {zone}", end.format(&format!("{day} %H:%M")))
    })
}

/// The headers the pager shows. `bcc` includes the `Bcc:` recipients,
/// which only the sender's copy has and which must not be passed on.
pub fn header_lines(message: &Message, bcc: bool) -> Vec<String> {
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
    if bcc && !message.bcc.is_empty() {
        lines.push(format!("Bcc: {}", addresses(&message.bcc)));
    }
    lines.push(format!("Subject: {}", summary.subject));
    lines
}

/// The body as plain-text lines.
pub fn body_lines(body: &Body) -> Vec<String> {
    let html = body.kind == BodyKind::Html;
    body_text(body)
        .lines()
        .map(|line| {
            if html {
                expand_tabs(&line.replace(LINK_START, "").replace(LINK_URL, "<"))
            } else {
                expand_tabs(line)
            }
        })
        .collect()
}

/// The body as plain text, where HTML links' text starts marked with
/// [`LINK_START`]. Its tabs aren't expanded yet.
fn body_text(body: &Body) -> Cow<'_, str> {
    match body.kind {
        BodyKind::Text => body.content.replace("\r\n", "\n").into(),
        BodyKind::Html => html_to_text(&body.content).into(),
    }
}

/// Replaces tabs with spaces up to the next 8-column tab stop, counting
/// columns in display cells.
fn expand_tabs(line: &str) -> String {
    if !line.contains('\t') {
        return line.to_owned();
    }
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

#[cfg(test)]
mod tests {
    use super::html::attribute;
    use super::*;

    fn html(content: &str) -> Vec<String> {
        body_lines(&Body {
            kind: BodyKind::Html,
            content: content.into(),
        })
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
    fn html_attributes_may_contain_greater_than_signs() {
        assert_eq!(
            html(r#"<p><a title="a>b" data-x='>'>Link</a></p>"#),
            ["Link"]
        );
    }

    #[test]
    fn html_table_cells_stay_apart() {
        assert_eq!(
            html("<table><tr><td>Alice</td><td>Bob</td></tr><tr><th>Carol</th></tr></table>"),
            ["Alice   Bob", "Carol"]
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
        // Found in any case, after text that isn't ASCII.
        assert_eq!(html("<script>é()</ScRiPt><p>ünï</p>"), ["ünï"]);
    }

    /// `line` with its links collapsed or not, and the text of each link.
    fn links(line: &str, collapse: bool) -> (String, Vec<String>) {
        let Found {
            shown,
            links: ranges,
            ..
        } = find_links(line, collapse, false, false);
        let texts = ranges.iter().map(|r| shown[r.clone()].to_owned()).collect();
        (shown, texts)
    }

    #[test]
    fn links_collapse_to_their_text() {
        let line = "See the docs<https://example.com/a?b=c> or (mail Bob<mailto:bob@x.org>).";
        assert_eq!(
            links(line, true),
            (
                "See the docs or (mail Bob).".to_owned(),
                vec!["docs".to_owned(), "Bob".to_owned()]
            )
        );
        assert_eq!(
            links(line, false),
            (
                line.to_owned(),
                vec![
                    "https://example.com/a?b=c".to_owned(),
                    "mailto:bob@x.org".to_owned()
                ]
            )
        );
    }

    #[test]
    fn urls_on_their_own_stay_and_are_underlined() {
        let line = "Go to https://example.com/x. Or <http://a.org/y>, not xhttp://b";
        for collapse in [true, false] {
            assert_eq!(
                links(line, collapse),
                (
                    line.to_owned(),
                    vec![
                        "https://example.com/x".to_owned(),
                        "http://a.org/y".to_owned()
                    ]
                )
            );
        }
        // A URL followed by its link (as Exchange writes `<a>` whose text
        // is its URL) is one link.
        assert_eq!(
            links("https://a.org<https://safe.links/?u=a.org>", true),
            ("https://a.org".to_owned(), vec!["https://a.org".to_owned()])
        );
        // Not a URL: no scheme, or nothing after the scheme.
        for line in ["a<b@x.org>", "see https:// here"] {
            assert_eq!(links(line, true), (line.to_owned(), Vec::new()));
        }
    }

    #[test]
    fn links_may_follow_any_space() {
        assert_eq!(
            links("See\u{a0}docs<https://e.com>", true),
            ("See\u{a0}docs".to_owned(), vec!["docs".to_owned()])
        );
        assert_eq!(
            links("See\u{3000}<https://e.com>", true),
            (
                "See\u{3000}<https://e.com>".to_owned(),
                vec!["https://e.com".to_owned()]
            )
        );
    }

    #[test]
    fn urls_without_text_are_never_collapsed() {
        let line = "(<https://example.com>) and \"<https://b.org>\"";
        assert_eq!(
            links(line, true),
            (
                line.to_owned(),
                vec!["https://example.com".to_owned(), "https://b.org".to_owned()]
            )
        );
    }

    /// An HTML message's body in the pager, and the text of its links.
    fn html_view(content: &str, collapse: bool) -> (String, Vec<String>) {
        let mut message = mett_core::mock::message("m", "in", "s", chrono::DateTime::UNIX_EPOCH);
        message.body = Body {
            kind: BodyKind::Html,
            content: content.into(),
        };
        let (lines, links) = pager_view(&message, collapse);
        let texts = links
            .iter()
            .map(|l| lines[l.line][l.range.clone()].to_owned())
            .collect();
        (lines.last().unwrap().clone(), texts)
    }

    #[test]
    fn html_links_are_underlined_whole_and_keep_their_spaces() {
        // Tabs are expanded as shown: without the hidden URLs, or with them.
        let body = concat!(
            r#"<p>Read <a href="https://e.com/n">the latest news </a>today, "#,
            r#"(<a href="https://e.com/a">x"#,
            "\t",
            "y</a>)</p>"
        );
        let shown = expand_tabs("Read the latest news today, (x\ty)");
        let tab = "x  y";
        assert!(shown.ends_with("(x  y)"), "{shown:?}");
        assert_eq!(
            html_view(body, true),
            (
                shown.clone(),
                vec!["the latest news".to_owned(), tab.to_owned()]
            )
        );
        let expanded =
            expand_tabs("Read the latest news<https://e.com/n> today, (x\ty<https://e.com/a>)");
        assert_eq!(
            html_view(body, false),
            (
                expanded.clone(),
                vec!["https://e.com/n".to_owned(), "https://e.com/a".to_owned()]
            )
        );
        // Replies and forwards quote the text without the marks.
        assert_eq!(html(body), [expanded]);
        // Marks of links that show no URL are dropped.
        for body in [
            "<a href='https://e.com'>https://e.com</a>",
            "<a href='https://e.com'><img></a>z",
            "<a href='https://e.com'>never closed",
            "<a href='https://e.com'>outer <a href='/rel'>inner</a></a>",
        ] {
            assert!(!html_view(body, true).0.contains(LINK_START), "{body}");
        }
    }

    /// A message's body in the pager, every line, and the line and text of
    /// each link.
    fn view_lines(body: Body, collapse: bool) -> (Vec<String>, Vec<(usize, String)>) {
        let mut message = mett_core::mock::message("m", "in", "s", chrono::DateTime::UNIX_EPOCH);
        message.body = body;
        let (lines, links) = pager_view(&message, collapse);
        let start = lines.iter().position(String::is_empty).unwrap() + 1;
        let texts = links
            .iter()
            .map(|l| (l.line - start, lines[l.line][l.range.clone()].to_owned()))
            .collect();
        (lines[start..].to_vec(), texts)
    }

    #[test]
    fn html_links_over_several_lines_are_underlined_whole() {
        let html = |content: &str| Body {
            kind: BodyKind::Html,
            content: content.into(),
        };
        let body =
            html(r#"<p>See <a href="https://e.com"><p>the first<br>and second</p></a> line</p>"#);
        assert_eq!(
            view_lines(body.clone(), true),
            (
                vec![
                    "See".to_owned(),
                    "the first".to_owned(),
                    "and second".to_owned(),
                    " line".to_owned()
                ],
                vec![(1, "the first".to_owned()), (2, "and second".to_owned())]
            )
        );
        assert_eq!(
            view_lines(body, false),
            (
                vec![
                    "See".to_owned(),
                    "the first".to_owned(),
                    "and second<https://e.com>".to_owned(),
                    " line".to_owned()
                ],
                vec![(2, "https://e.com".to_owned())]
            )
        );
    }

    #[test]
    fn html_attributes_without_values_are_skipped() {
        for tag in [
            r#"a download href="https://e.com""#,
            r#"a download   href = https://e.com"#,
            r#"a hidden/ href='https://e.com'"#,
        ] {
            assert_eq!(attribute(tag, "href"), Some("https://e.com"), "{tag}");
        }
        assert_eq!(attribute("a download", "href"), None);
        // A label ending in a URL is underlined whole.
        let body = Body {
            kind: BodyKind::Html,
            content: r#"<a href="https://target">See https://other</a>"#.into(),
        };
        assert_eq!(
            view_lines(body, true).1,
            [
                (0, "See https://other".to_owned()),
                (0, "https://other".to_owned())
            ]
        );
        // What would end the URL is encoded.
        let body = Body {
            kind: BodyKind::Html,
            content: r#"<a href=" https://e.com/a b&gt;c ">x</a>"#.into(),
        };
        assert_eq!(body_lines(&body), ["x<https://e.com/a%20b%3Ec>"]);
        assert_eq!(view_lines(body, true).0, ["x"]);
        assert_eq!(attribute("a download /", "href"), None);
        assert_eq!(
            html(r#"<a download href="https://e.com">x</a>"#),
            ["x<https://e.com>"]
        );
    }

    #[test]
    fn urls_in_html_link_text_are_part_of_it() {
        let body = Body {
            kind: BodyKind::Html,
            content: r#"<a href="https://target">See &lt;https://example.com&gt;</a>"#.into(),
        };
        assert_eq!(
            body_lines(&body),
            ["See <https://example.com><https://target>"]
        );
        assert_eq!(
            view_lines(body.clone(), true),
            (
                vec!["See <https://example.com>".to_owned()],
                // The whole text, and the URL in it.
                vec![
                    (0, "See <https://example.com>".to_owned()),
                    (0, "https://example.com".to_owned())
                ]
            )
        );
        assert_eq!(
            view_lines(body, false).0,
            ["See <https://example.com><https://target>"]
        );
    }

    #[test]
    fn private_use_characters_in_messages_are_kept() {
        let text = Body {
            kind: BodyKind::Text,
            content: "a\u{E000}b<https://e.com>".into(),
        };
        assert_eq!(body_lines(&text), ["a\u{E000}b<https://e.com>"]);
        assert_eq!(
            view_lines(text, true),
            (
                vec!["a\u{E000}b".to_owned()],
                vec![(0, "a\u{E000}b".to_owned())]
            )
        );
        // In HTML, where it would read as a link's mark, it is replaced.
        let html = Body {
            kind: BodyKind::Html,
            content: "a\u{E000}b <a href='https://e.com'>c</a>".into(),
        };
        assert_eq!(body_lines(&html), ["a\u{FFFD}b c<https://e.com>"]);
        assert_eq!(
            view_lines(html, true),
            (vec!["a\u{FFFD}b c".to_owned()], vec![(0, "c".to_owned())])
        );
    }

    #[test]
    fn html_links_are_followed_by_their_url() {
        let link = r#"Read <A class=x HREF="https://e.com/?a=1&amp;b=2">the <b>news</b></a>,"#;
        let same = "<a href='https://e.com'>https://e.com</a>,";
        let image = "<a href=https://e.com/i><img src=x></a>";
        let relative = r#"<a href="/relative">here</a>."#;
        assert_eq!(
            html(&format!("<p>{link}<br>{same}<br>{image}{relative}</p>")),
            [
                "Read the news<https://e.com/?a=1&b=2>,",
                "https://e.com,",
                "here."
            ]
        );
    }

    /// A request for a meeting from 14:00 to 15:00 (UTC) on 3 November 2026.
    fn invitation() -> Invitation {
        use chrono::TimeZone;
        Invitation {
            kind: InvitationKind::Request,
            title: None,
            start: Some(Utc.with_ymd_and_hms(2026, 11, 3, 14, 0, 0).unwrap()),
            end: Some(Utc.with_ymd_and_hms(2026, 11, 3, 15, 0, 0).unwrap()),
            all_day: false,
            location: Some("Room 4A".into()),
            organizer: Some(Address::new("alice@x.com").with_name("Alice")),
            attendees: vec![Address::new("bob@x.com"), Address::new("carol@x.com")],
            recurrence: None,
            reply: mett_core::Reply::None,
            out_of_date: false,
            response_requested: true,
            in_series: false,
            series_master: false,
            event: None,
        }
    }

    #[test]
    fn shows_an_invitation_with_the_headers() {
        use chrono::TimeZone;
        let from = Utc
            .with_ymd_and_hms(2026, 11, 3, 14, 0, 0)
            .unwrap()
            .with_timezone(&Local);
        let to = Utc
            .with_ymd_and_hms(2026, 11, 3, 15, 0, 0)
            .unwrap()
            .with_timezone(&Local);
        let mut invitation = invitation();
        invitation.recurrence = Some(mett_core::Recurrence::new(
            mett_core::RecurrenceUnit::Week,
            1,
        ));
        let mut message =
            mett_core::mock::message("m", "in", "Planning", chrono::DateTime::UNIX_EPOCH);
        message.invitation = Some(invitation);
        let (lines, _) = pager_view(&message, true);
        let at = lines
            .iter()
            .position(|l| l.starts_with("Meeting:"))
            .unwrap();
        assert!(
            at > 0 && lines[at - 1].starts_with("Subject:"),
            "{lines:#?}"
        );
        assert_eq!(
            lines[at..at + 7],
            [
                "Meeting: invitation".to_owned(),
                format!(
                    "When: {} - {} {}",
                    from.format("%a %d %b %Y %H:%M"),
                    to.format("%H:%M"),
                    from.format("%Z")
                ),
                "Repeats: Weekly".to_owned(),
                "Where: Room 4A".to_owned(),
                "Organizer: Alice <alice@x.com>".to_owned(),
                "Attendees: bob@x.com, carol@x.com".to_owned(),
                "Your reply: not answered".to_owned(),
            ]
        );
        // The invitation is above the body, with the headers.
        assert_eq!(lines[at + 7], "");
    }

    #[test]
    fn shows_meetings_of_whole_days_by_their_dates() {
        use chrono::TimeZone;
        let mut invitation = invitation();
        invitation.all_day = true;
        invitation.start = Some(Utc.with_ymd_and_hms(2026, 11, 3, 0, 0, 0).unwrap());
        invitation.end = Some(Utc.with_ymd_and_hms(2026, 11, 4, 0, 0, 0).unwrap());
        assert_eq!(
            meeting_time(&invitation).unwrap(),
            "Tue 03 Nov 2026 (all day)"
        );
        invitation.end = Some(Utc.with_ymd_and_hms(2026, 11, 6, 0, 0, 0).unwrap());
        assert_eq!(
            meeting_time(&invitation).unwrap(),
            "Tue 03 Nov 2026 - Thu 05 Nov 2026 (all day)"
        );
    }

    #[test]
    fn says_what_a_cancelled_or_old_invitation_is() {
        let mut invitation = invitation();
        invitation.kind = InvitationKind::Cancellation;
        let lines = invitation_lines(&invitation);
        assert_eq!(lines[0], "Meeting: cancelled");
        // Nothing to answer, so no reply is shown.
        assert!(lines.iter().all(|l| !l.starts_with("Your reply")));
        invitation.kind = InvitationKind::Update;
        invitation.out_of_date = true;
        invitation.reply = mett_core::Reply::Accepted;
        let lines = invitation_lines(&invitation);
        assert_eq!(lines[0], "Meeting: updated invitation (out of date)");
        assert_eq!(lines.last().unwrap(), "Your reply: accepted");
    }
}
