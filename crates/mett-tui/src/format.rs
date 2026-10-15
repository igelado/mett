//! mutt's printf-like format strings, such as `index_format`
//! (`docs/MUTTRC.md` §3).
//!
//! A format is text with `%` expandos:
//!
//! | Syntax | Meaning |
//! |--------|---------|
//! | `%X` | The value of expando `X`, which depends on the format. |
//! | `%-10.20X` | At least 10 cells (`-` aligns left, `0` pads numbers with zeros), at most 20. |
//! | `%{fmt}`, `%[fmt]`, `%(fmt)` | A date in `strftime` format. |
//! | `%?X?then&else?` | `then` if `X` is non-empty and not zero, otherwise `else` (which may be left out). Both may hold expandos. |
//! | `%>X` | Right-aligns the rest of the line, filling the gap with `X`. |
//! | `%*X` | Like `%>X`, but when there isn't room, the text before it is cut rather than the rest. |
//! | `%\|X` | Fills the rest of the line with `X`. |
//! | `%%` | A `%`. |
//!
//! Widths are terminal cells, so wide characters line up.

use chrono::format::{Item, StrftimeItems};
use chrono::{DateTime, Local, Utc};
use unicode_width::UnicodeWidthStr;

use crate::text::truncate;

/// What a format's expandos stand for.
pub trait Expandos {
    /// The text of expando `code`; `arg` is the `strftime` format of a
    /// date expando (`{`, `[` or `(`). Unknown expandos are empty.
    fn expando(&self, code: char, arg: Option<&str>) -> String;

    /// Whether `%?code?` takes its `then` part: by default, when the
    /// expando isn't empty or zero.
    fn is_set(&self, code: char) -> bool {
        truthy(&self.expando(code, None))
    }
}

/// Expands `format` for a line `width` cells wide.
pub fn expand(format: &str, values: &dyn Expandos, width: usize) -> String {
    let chars: Vec<char> = format.chars().collect();
    let line = expand_chars(&chars, values, width);
    truncate(&line, width)
}

/// Expands `chars` (part of a format) on a line `width` cells wide.
fn expand_chars(chars: &[char], values: &dyn Expandos, width: usize) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if c != '%' || i >= chars.len() {
            out.push(c);
            continue;
        }
        match chars[i] {
            '%' => {
                out.push('%');
                i += 1;
            }
            '?' => {
                let Some((code, then, otherwise, next)) = conditional(chars, i + 1) else {
                    // Not a conditional after all: taken as written.
                    out.push('%');
                    continue;
                };
                let part = if values.is_set(code) { then } else { otherwise };
                // In what is left of the line, so that a fill in it lines
                // up with the edge.
                let left = width.saturating_sub(out.width());
                out.push_str(&expand_chars(part, values, left));
                i = next;
            }
            fill @ ('>' | '*' | '|') => {
                let pad = chars.get(i + 1).copied().unwrap_or(' ');
                let rest = chars.get(i + 2..).unwrap_or(&[]);
                return match fill {
                    '|' => {
                        let gap = width.saturating_sub(out.width());
                        out.push_str(&filler(pad, gap));
                        out
                    }
                    _ => {
                        let right = expand_chars(rest, values, width);
                        justify(out, right, pad, width, fill == '*')
                    }
                };
            }
            _ => {
                let (spec, next) = spec(chars, i);
                i = next;
                let Some(&code) = chars.get(i) else {
                    break;
                };
                i += 1;
                let arg = match code {
                    '{' | '[' | '(' => {
                        let close = match code {
                            '{' => '}',
                            '[' => ']',
                            _ => ')',
                        };
                        let end = chars[i..]
                            .iter()
                            .position(|&c| c == close)
                            .map_or(chars.len(), |n| i + n);
                        let arg: String = chars[i..end].iter().collect();
                        i = (end + 1).min(chars.len());
                        Some(arg)
                    }
                    _ => None,
                };
                let value = values.expando(code, arg.as_deref());
                out.push_str(&spec.apply(&value, width));
            }
        }
    }
    out
}

/// A conditional's parts, starting after `%?`: its expando, the `then`
/// and `else` parts, and where the format goes on. The parts may hold
/// expandos, and conditionals of their own.
fn conditional(chars: &[char], start: usize) -> Option<(char, &[char], &[char], usize)> {
    let code = *chars.get(start)?;
    if chars.get(start + 1) != Some(&'?') {
        return None;
    }
    let then_start = start + 2;
    let mut i = then_start;
    let mut else_start = None;
    while i < chars.len() {
        match chars[i] {
            // An expando's `?` or `&` doesn't end a part.
            '%' => {
                i = skip_expando(chars, i + 1);
                continue;
            }
            '&' if else_start.is_none() => else_start = Some(i),
            '?' => {
                let (then, otherwise) = match else_start {
                    Some(amp) => (&chars[then_start..amp], &chars[amp + 1..i]),
                    None => (&chars[then_start..i], &chars[i..i]),
                };
                return Some((code, then, otherwise, i + 1));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Where the format goes on after the expando starting at `start` (just
/// after its `%`): a whole conditional, a fill, or an expando with its
/// spec and date format.
fn skip_expando(chars: &[char], start: usize) -> usize {
    match chars.get(start) {
        None => start,
        Some('%') => start + 1,
        Some('?') => conditional(chars, start + 1).map_or(start + 1, |(.., next)| next),
        Some('>' | '*' | '|') => (start + 2).min(chars.len()),
        Some(_) => {
            let (_, code) = spec(chars, start);
            let close = match chars.get(code) {
                Some('{') => '}',
                Some('[') => ']',
                Some('(') => ')',
                Some(_) => return code + 1,
                None => return code,
            };
            chars[code..]
                .iter()
                .position(|&c| c == close)
                .map_or(chars.len(), |n| code + n + 1)
        }
    }
}

/// Whether a conditional's expando counts as true: not empty, and not
/// zero.
pub fn truthy(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value.parse::<i64>().ok().is_none_or(|n| n != 0)
}

/// `left` and `right` on one line `width` cells wide, `right` against the
/// right edge and the gap filled with `pad`. Without room for both,
/// `right` is cut, or with `soft`, `left`.
fn justify(left: String, right: String, pad: char, width: usize, soft: bool) -> String {
    let (left_width, right_width) = (left.width(), right.width());
    if left_width + right_width <= width {
        let gap = width - left_width - right_width;
        return format!("{left}{}{right}", filler(pad, gap));
    }
    if soft {
        let right = truncate(&right, width);
        let left = truncate(&left, width - right.width());
        format!("{left}{right}")
    } else {
        format!("{left}{right}")
    }
}

/// `pad` repeated to fill `cells` terminal cells (and a space if a wide
/// `pad` leaves one over).
fn filler(pad: char, cells: usize) -> String {
    let each = unicode_width::UnicodeWidthChar::width(pad)
        .unwrap_or(1)
        .max(1);
    let mut fill: String = std::iter::repeat_n(pad, cells / each).collect();
    fill.extend(std::iter::repeat_n(' ', cells % each));
    fill
}

/// An expando's `-0WIDTH.MAX`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Spec {
    left: bool,
    zero: bool,
    min: usize,
    max: Option<usize>,
}

/// Reads an expando's spec from `start`, and where its code is.
fn spec(chars: &[char], start: usize) -> (Spec, usize) {
    let mut spec = Spec::default();
    let mut i = start;
    while let Some(&c) = chars.get(i) {
        match c {
            '-' => spec.left = true,
            '0' if spec.min == 0 => spec.zero = true,
            _ => break,
        }
        i += 1;
    }
    let number = |i: &mut usize| {
        let mut n: Option<usize> = None;
        while let Some(digit) = chars.get(*i).and_then(|c| c.to_digit(10)) {
            n = Some(
                n.unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(digit as usize),
            );
            *i += 1;
        }
        n
    };
    spec.min = number(&mut i).unwrap_or(0);
    if chars.get(i) == Some(&'.') {
        i += 1;
        spec.max = Some(number(&mut i).unwrap_or(0));
    }
    (spec, i)
}

impl Spec {
    /// `value` cut to the maximum width, then padded to the minimum (no
    /// wider than the `line`, which is cut to it anyway).
    fn apply(self, value: &str, line: usize) -> String {
        let value = match self.max {
            Some(max) => truncate(value, max),
            None => value.to_owned(),
        };
        let gap = self.min.min(line).saturating_sub(value.width());
        if gap == 0 {
            return value;
        }
        if self.left {
            return format!("{value}{}", " ".repeat(gap));
        }
        let pad = if self.zero && value.parse::<i64>().is_ok() {
            "0"
        } else {
            " "
        };
        format!("{}{value}", pad.repeat(gap))
    }
}

/// `date` in the local time zone, in `strftime` format `format`. A leading
/// `!` (which in mutt keeps the C locale) is dropped. A format chrono
/// doesn't understand is shown as written, rather than failing.
pub fn date(date: DateTime<Utc>, format: &str) -> String {
    let format = format.strip_prefix('!').unwrap_or(format);
    let items: Vec<Item> = StrftimeItems::new(format).collect();
    if items.iter().any(|item| matches!(item, Item::Error)) {
        return format.to_owned();
    }
    date.with_timezone(&Local)
        .format_with_items(items.into_iter())
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Values;

    impl Expandos for Values {
        fn expando(&self, code: char, arg: Option<&str>) -> String {
            match code {
                'n' => "Alice".into(),
                's' => "Hello world".into(),
                'C' => "7".into(),
                'z' => "0".into(),
                'w' => "宽字".into(),
                '{' => format!("date({})", arg.unwrap_or("")),
                _ => String::new(),
            }
        }
    }

    fn x(format: &str) -> String {
        expand(format, &Values, 30)
    }

    #[test]
    fn expands_with_widths() {
        assert_eq!(x("%4C|%-4C|%04C|%%"), "   7|7   |0007|%");
        assert_eq!(x("%-8.3n|%.2s|%5.5w|"), "Ali     |He| 宽字|");
        assert_eq!(x("%{%b %d} %q."), "date(%b %d) .");
        // Padding is never wider than the line.
        assert_eq!(x("%999999999C"), format!("{}7", " ".repeat(29)));
    }

    #[test]
    fn expands_conditionals() {
        assert_eq!(x("%?C?(%C)&none?"), "(7)");
        assert_eq!(x("%?z?(%z)&none?"), "none");
        assert_eq!(x("a%?q? q?b"), "ab");
        // Conditionals inside conditionals, and expandos with `?` or `&`.
        assert_eq!(x("%?C?<%?z?z&no z?>&none?."), "<no z>.");
        assert_eq!(x("%?C?%{a?b&c}&none?"), "date(a?b&c)");
        // A fill in a conditional fills the rest of the line.
        assert_eq!(x("ab%?C?c%>-%C?"), format!("abc{}7", "-".repeat(26)));
        // Not a conditional: taken as written.
        assert_eq!(x("%?"), "%?");
    }

    #[test]
    fn fills_the_line() {
        assert_eq!(x("%n%>-%C"), format!("Alice{}7", "-".repeat(24)));
        assert_eq!(x("%n%|."), format!("Alice{}", ".".repeat(25)));
        // Wide fill characters take two cells.
        assert_eq!(x("%n%>界%C"), format!("Alice{}7", "界".repeat(12)));
        assert_eq!(
            expand("%n%>界%C", &Values, 31),
            format!("Alice{} 7", "界".repeat(12))
        );
        let long = "x".repeat(28);
        // Without room, the right side is cut, or the left with `%*`.
        assert_eq!(
            expand(&format!("{long}%> %s"), &Values, 30),
            format!("{long}He")
        );
        assert_eq!(
            expand(&format!("{long}%* %s"), &Values, 30),
            format!("{}Hello world", "x".repeat(19))
        );
    }

    #[test]
    fn dates_survive_bad_formats() {
        let epoch = DateTime::UNIX_EPOCH;
        assert_eq!(
            date(epoch, "%Y"),
            epoch.with_timezone(&Local).format("%Y").to_string()
        );
        assert_eq!(date(epoch, "!%Y"), date(epoch, "%Y"));
        assert_eq!(date(epoch, "%Q"), "%Q");
    }
}
