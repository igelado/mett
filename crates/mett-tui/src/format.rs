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

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use chrono::format::{Item, StrftimeItems};
use chrono::{DateTime, Local, Utc};
use unicode_width::UnicodeWidthStr;

use crate::text::truncate;

/// What a format's expandos stand for.
pub trait Expandos {
    /// The text of expando `code`; `arg` is the `strftime` format of a
    /// date expando (`{`, `[` or `(`). Unknown expandos are empty. Text the
    /// value already holds is lent, not copied.
    fn expando(&self, code: char, arg: Option<&str>) -> Cow<'_, str>;

    /// Whether `%?code?` takes its `then` part: by default, when the
    /// expando isn't empty or zero.
    fn is_set(&self, code: char) -> bool {
        truthy(&self.expando(code, None))
    }
}

/// Expands `format` for a line `width` cells wide.
pub fn expand(format: &str, values: &dyn Expandos, width: usize) -> String {
    compile(format).expand(values, width)
}

/// How many formats [`compile`] remembers: `index_format`,
/// `pager_format`, `status_format` and `sidebar_format`, with room for
/// formats a `:set` replaced.
const COMPILED_KEPT: usize = 16;

thread_local! {
    static COMPILED: RefCell<Vec<(String, Rc<Compiled>)>> = const { RefCell::new(Vec::new()) };
}

/// `format`, compiled. The formats are compiled once, not for every row
/// of every frame: the last few compiled are kept, found by their text,
/// so a variable that changes simply finds (or makes) its new entry.
pub fn compile(format: &str) -> Rc<Compiled> {
    COMPILED.with_borrow_mut(|cache| {
        if let Some((_, compiled)) = cache.iter().find(|(text, _)| text == format) {
            return Rc::clone(compiled);
        }
        let compiled = Rc::new(Compiled::new(format));
        if cache.len() >= COMPILED_KEPT {
            cache.remove(0);
        }
        cache.push((format.to_owned(), Rc::clone(&compiled)));
        compiled
    })
}

/// A format, parsed once into the pieces [`Compiled::expand`] fills in.
#[derive(Debug)]
pub struct Compiled {
    tokens: Vec<Token>,
}

/// A piece of a format.
#[derive(Debug)]
enum Token {
    /// Text as written (and `%%`).
    Literal(String),
    /// `%-10.20X`, with the date format of `%{...}`.
    Expando {
        spec: Spec,
        code: char,
        arg: Option<String>,
    },
    /// `%?X?then&else?`.
    Conditional {
        code: char,
        then: Vec<Token>,
        otherwise: Vec<Token>,
    },
    /// `%>X` and `%*X` (`kind`, with `rest` of the format to the right
    /// of the fill), or `%|X` (with no `rest`).
    Fill {
        kind: char,
        pad: char,
        rest: Vec<Token>,
    },
}

impl Compiled {
    /// Parses `format`.
    pub fn new(format: &str) -> Self {
        let chars: Vec<char> = format.chars().collect();
        Self {
            tokens: parse(&chars),
        }
    }

    /// Expands the format for a line `width` cells wide.
    pub fn expand(&self, values: &dyn Expandos, width: usize) -> String {
        self.run(values, width).0
    }

    /// Like [`Compiled::expand`], padded with spaces to `width` cells.
    pub fn expand_padded(&self, values: &dyn Expandos, width: usize) -> String {
        let (mut text, used) = self.run(values, width);
        text.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
        text
    }

    /// The expanded line, cut to `width`, and its width in cells.
    fn run(&self, values: &dyn Expandos, width: usize) -> (String, usize) {
        let mut line = String::with_capacity(width);
        eval(&self.tokens, values, width, &mut line);
        // Plain ASCII is as many cells as bytes: no need to walk its
        // graphemes.
        if is_plain(&line) {
            line.truncate(width);
            let used = line.len();
            return (line, used);
        }
        let line = truncate(&line, width);
        let used = line.width();
        (line, used)
    }
}

/// Parses `chars` (a format, or part of one).
fn parse(chars: &[char]) -> Vec<Token> {
    fn flush(tokens: &mut Vec<Token>, text: &mut String) {
        if !text.is_empty() {
            tokens.push(Token::Literal(std::mem::take(text)));
        }
    }
    let mut tokens = Vec::new();
    let mut text = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if c != '%' || i >= chars.len() {
            text.push(c);
            continue;
        }
        match chars[i] {
            '%' => {
                text.push('%');
                i += 1;
            }
            '?' => {
                let Some((code, then, otherwise, next)) = conditional(chars, i + 1) else {
                    // Not a conditional after all: taken as written.
                    text.push('%');
                    continue;
                };
                flush(&mut tokens, &mut text);
                tokens.push(Token::Conditional {
                    code,
                    then: parse(then),
                    otherwise: parse(otherwise),
                });
                i = next;
            }
            kind @ ('>' | '*' | '|') => {
                let pad = chars.get(i + 1).copied().unwrap_or(' ');
                let rest = chars.get(i + 2..).unwrap_or(&[]);
                flush(&mut tokens, &mut text);
                tokens.push(Token::Fill {
                    kind,
                    pad,
                    // What follows a `%|` is dropped.
                    rest: if kind == '|' { Vec::new() } else { parse(rest) },
                });
                return tokens;
            }
            _ => {
                let (spec, next) = spec(chars, i);
                i = next;
                let Some(&code) = chars.get(i) else {
                    break;
                };
                i += 1;
                let arg = closing(code).map(|close| {
                    let end = chars[i..]
                        .iter()
                        .position(|&c| c == close)
                        .map_or(chars.len(), |n| i + n);
                    let arg: String = chars[i..end].iter().collect();
                    i = (end + 1).min(chars.len());
                    arg
                });
                flush(&mut tokens, &mut text);
                tokens.push(Token::Expando { spec, code, arg });
            }
        }
    }
    flush(&mut tokens, &mut text);
    tokens
}

/// Whether `b` is a printable ASCII character, which is one cell wide
/// whatever is next to it.
fn printable(b: u8) -> bool {
    (0x20..0x7f).contains(&b)
}

/// Whether `text` is printable ASCII only.
fn is_plain(text: &str) -> bool {
    text.bytes().all(printable)
}

/// The cells `text` takes.
fn width_of(text: &str) -> usize {
    if is_plain(text) {
        text.len()
    } else {
        text.width()
    }
}

/// The width of `line[start..]`, after `line[before..]` (`added` cells
/// wide) was appended to what was `used` cells wide. Widths add up, except
/// where a character can join the one before it (a combining mark, or a
/// joiner): then, the line is measured again. Only the edges of what was
/// appended can do that, so they are what is checked.
fn joined(line: &str, start: usize, before: usize, used: usize, added: usize) -> usize {
    let bytes = line.as_bytes();
    let tail = before == start || printable(bytes[before - 1]);
    let head = line.len() == before || printable(bytes[before]);
    if tail && head {
        used + added
    } else {
        line[start..].width()
    }
}

/// Expands `tokens` on a line `width` cells wide, appending to `line`,
/// and returns how many cells that took.
fn eval(tokens: &[Token], values: &dyn Expandos, width: usize, line: &mut String) -> usize {
    let start = line.len();
    let mut used = 0;
    for token in tokens {
        let before = line.len();
        match token {
            Token::Literal(text) => {
                line.push_str(text);
                used = joined(line, start, before, used, width_of(text));
            }
            Token::Expando { spec, code, arg } => {
                let value = values.expando(*code, arg.as_deref());
                // Padding takes no more than what is left of the line.
                let left = width.saturating_sub(used);
                let added = spec.apply(&value, left, line);
                used = joined(line, start, before, used, added);
            }
            Token::Conditional {
                code,
                then,
                otherwise,
            } => {
                let part = if values.is_set(*code) {
                    then
                } else {
                    otherwise
                };
                // In what is left of the line, so that a fill in it lines
                // up with the edge.
                let left = width.saturating_sub(used);
                let added = eval(part, values, left, line);
                used = joined(line, start, before, used, added);
            }
            Token::Fill { kind, pad, rest } => {
                if *kind == '|' {
                    let gap = width.saturating_sub(used);
                    line.push_str(&filler(*pad, gap));
                } else {
                    let mut right = String::new();
                    eval(rest, values, width, &mut right);
                    justify(line, start, &right, *pad, width, *kind == '*');
                }
                return line[start..].width();
            }
        }
    }
    used
}

/// `line[start..]` and `right` on one line `width` cells wide, `right`
/// against the right edge and the gap filled with `pad`. Without room for
/// both, `right` is cut, or with `soft`, the left.
fn justify(line: &mut String, start: usize, right: &str, pad: char, width: usize, soft: bool) {
    let (left_width, right_width) = (line[start..].width(), right.width());
    if left_width + right_width <= width {
        let gap = width - left_width - right_width;
        line.push_str(&filler(pad, gap));
        line.push_str(right);
    } else if soft {
        let right = truncate(right, width);
        let left = truncate(&line[start..], width - right.width());
        line.truncate(start);
        line.push_str(&left);
        line.push_str(&right);
    } else {
        line.push_str(right);
    }
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
            let Some(&c) = chars.get(code) else {
                return code;
            };
            let Some(close) = closing(c) else {
                return code + 1;
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

/// The bracket closing `c`, if it opens an expando's argument (`%{...}`,
/// `%[...]`, `%(...)`).
fn closing(c: char) -> Option<char> {
    match c {
        '{' => Some('}'),
        '[' => Some(']'),
        '(' => Some(')'),
        _ => None,
    }
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
    /// Appends `value` to `line`, cut to the maximum width, then padded to
    /// the minimum (no wider than the `line` of `left` cells, which is cut
    /// to it anyway), and returns the cells that took.
    fn apply(self, value: &str, left: usize, line: &mut String) -> usize {
        let value = match self.max {
            // Printable ASCII that fits is left as it is.
            Some(max) if !(is_plain(value) && value.len() <= max) => {
                Cow::Owned(truncate(value, max))
            }
            _ => Cow::Borrowed(value),
        };
        let width = width_of(&value);
        let gap = self.min.min(left).saturating_sub(width);
        if gap == 0 {
            line.push_str(&value);
        } else if self.left {
            line.push_str(&value);
            line.extend(std::iter::repeat_n(' ', gap));
        } else {
            let pad = if self.zero && value.parse::<i64>().is_ok() {
                '0'
            } else {
                ' '
            };
            line.extend(std::iter::repeat_n(pad, gap));
            line.push_str(&value);
        }
        width + gap
    }
}

/// A `strftime` format, parsed, or `None` if chrono rejects it.
type DateItems = Option<Vec<Item<'static>>>;

thread_local! {
    /// The `strftime` formats [`date`] was asked for, parsed.
    static DATE_FORMATS: RefCell<Vec<(String, DateItems)>> = const { RefCell::new(Vec::new()) };
}

/// How many `strftime` formats [`date`] remembers.
const DATE_FORMATS_KEPT: usize = 16;

/// `date` in the local time zone, in `strftime` format `format`. A leading
/// `!` (which in mutt keeps the C locale) is dropped. A format chrono
/// doesn't understand is shown as written, rather than failing. The format
/// is parsed once, not for each date.
pub fn date(date: DateTime<Utc>, format: &str) -> String {
    let format = format.strip_prefix('!').unwrap_or(format);
    DATE_FORMATS.with_borrow_mut(|cache| {
        let at = cache
            .iter()
            .position(|(text, _)| text == format)
            .unwrap_or_else(|| {
                let items: Vec<Item<'static>> = StrftimeItems::new(format)
                    .map(|item| item.to_owned())
                    .collect();
                let valid = !items.iter().any(|item| matches!(item, Item::Error));
                if cache.len() >= DATE_FORMATS_KEPT {
                    cache.remove(0);
                }
                cache.push((format.to_owned(), valid.then_some(items)));
                cache.len() - 1
            });
        match &cache[at].1 {
            None => format.to_owned(),
            Some(items) => date
                .with_timezone(&Local)
                .format_with_items(items.iter())
                .to_string(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Values;

    impl Expandos for Values {
        fn expando(&self, code: char, arg: Option<&str>) -> Cow<'_, str> {
            match code {
                'n' => "Alice".into(),
                's' => "Hello world".into(),
                'C' => "7".into(),
                'z' => "0".into(),
                'w' => "宽字".into(),
                '{' => format!("date({})", arg.unwrap_or("")).into(),
                _ => "".into(),
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
        // Nor than what is left of it: the value still shows.
        assert_eq!(expand("A%5C", &Values, 5), "A   7");
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

    /// The formatter as it was before formats were compiled: re-parsing
    /// the format at every call, and measuring the line at every expando.
    /// Kept to check the compiled one against.
    mod reference {
        use super::super::*;

        pub fn expand(format: &str, values: &dyn Expandos, width: usize) -> String {
            let chars: Vec<char> = format.chars().collect();
            let line = expand_chars(&chars, values, width);
            truncate(&line, width)
        }

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
                            out.push('%');
                            continue;
                        };
                        let part = if values.is_set(code) { then } else { otherwise };
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
                        let arg = closing(code).map(|close| {
                            let end = chars[i..]
                                .iter()
                                .position(|&c| c == close)
                                .map_or(chars.len(), |n| i + n);
                            let arg: String = chars[i..end].iter().collect();
                            i = (end + 1).min(chars.len());
                            arg
                        });
                        let value = values.expando(code, arg.as_deref());
                        let left = width.saturating_sub(out.width());
                        out.push_str(&apply(spec, &value, left));
                    }
                }
            }
            out
        }

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

        fn apply(spec: Spec, value: &str, line: usize) -> String {
            let value = match spec.max {
                Some(max) => truncate(value, max),
                None => value.to_owned(),
            };
            let gap = spec.min.min(line).saturating_sub(value.width());
            if gap == 0 {
                return value;
            }
            if spec.left {
                return format!("{value}{}", " ".repeat(gap));
            }
            let pad = if spec.zero && value.parse::<i64>().is_ok() {
                "0"
            } else {
                " "
            };
            format!("{}{value}", pad.repeat(gap))
        }

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
    }

    /// Values with text of every kind: wide characters, combining marks,
    /// joiners, controls, long and empty ones.
    struct Wide;

    impl Wide {
        fn text(code: char) -> &'static str {
            match code {
                'a' => "alice@example.com",
                'b' => "宽字符串 wide text",
                'c' => "e\u{301}a\u{300}\u{301}x",
                'd' => "\u{301}starts combining",
                'e' => "man\u{200d}\u{1f469}\u{200d}\u{1f4bb}!",
                'f' => "\u{fe0f}\u{2764}\u{fe0f}ok",
                'g' => "tab\there\r\nnl",
                'h' => "ends with joiner\u{200d}",
                'n' => "Alice Liddell",
                's' => "Re: [list] A rather long subject line, long enough to be cut",
                'C' => "42",
                'z' => "0",
                'p' => "-17",
                'w' => "宽字",
                'x' => "  ",
                _ => "",
            }
        }
    }

    impl Expandos for Wide {
        fn expando(&self, code: char, arg: Option<&str>) -> Cow<'_, str> {
            match code {
                '{' | '[' | '(' => date(DateTime::UNIX_EPOCH, arg.unwrap_or("%Y-%m-%d")).into(),
                _ => Self::text(code).into(),
            }
        }
    }

    /// The same, using the formatter as it was for dates.
    struct WideOld;

    impl Expandos for WideOld {
        fn expando(&self, code: char, arg: Option<&str>) -> Cow<'_, str> {
            match code {
                '{' | '[' | '(' => {
                    reference::date(DateTime::UNIX_EPOCH, arg.unwrap_or("%Y-%m-%d")).into()
                }
                _ => Wide::text(code).into(),
            }
        }
    }

    #[test]
    fn compiled_formats_expand_as_before() {
        let atoms = [
            "%a",
            "%b",
            "%c",
            "%d",
            "%e",
            "%f",
            "%g",
            "%h",
            "%n",
            "%s",
            "%C",
            "%z",
            "%p",
            "%w",
            "%x",
            "%q",
            "%%",
            "%-20.20s",
            "%-20.20b",
            "%20.5n",
            "%5.3b",
            "%.2c",
            "%.1e",
            "%8C",
            "%08C",
            "%08p",
            "%-8z",
            "%0.3p",
            "%3.0s",
            "%{%b %d}",
            "%[%H:%M]",
            "%(%Y)",
            "%{%Q}",
            "%{!%Y}",
            "%{unterminated",
            "%-30.30{%Y}",
            "%?C?yes&no?",
            "%?z?yes&no?",
            "%?q?never?",
            "%?s?%s&none?",
            "%?C?%?z?z&no z?&none?",
            "%?C?%{a?b&c}&none?",
            "%?C?<%b%>-%C>&none?",
            "%?",
            "%?x",
            "%",
            "%-",
            "%5",
            "%>-",
            "%>",
            "%*",
            "%|",
            "%|.",
            "%>界",
            "%*界",
            "%>-%C",
            "%*-%C",
            "%*-%s%b",
            "%>-%?C?%C&%b?",
            " ",
            "x",
            "宽",
            "é",
            "\u{301}",
            "\u{200d}",
            "\u{fe0f}",
            "a",
            "[",
            "]",
            "&",
            "?",
        ];
        // Every pair of them, and runs of three, with and without text
        // between.
        let mut formats: Vec<String> = Vec::new();
        for a in atoms {
            formats.push(a.to_owned());
            for b in atoms {
                formats.push(format!("{a}{b}"));
                formats.push(format!("{a} {b}"));
            }
        }
        for (i, a) in atoms.iter().enumerate() {
            for (j, b) in atoms.iter().enumerate().skip(i % 5) {
                let c = atoms[(i * 7 + j * 3) % atoms.len()];
                formats.push(format!("{a}{b}{c}"));
                formats.push(format!("{a}|{b}|{c}"));
            }
        }
        formats.push("%4C %Z %-20.20n %s %> %{%b %d} %?z?Z&%?C?C?&n?".to_owned());
        for format in &formats {
            for width in [0, 1, 2, 3, 5, 8, 13, 21, 30, 40, 80, 200] {
                let new = expand(format, &Wide, width);
                let old = reference::expand(format, &WideOld, width);
                assert_eq!(new, old, "format {format:?} at width {width}");
                // The padded line is the line, padded.
                let padded = compile(format).expand_padded(&Wide, width);
                let mut want = old.clone();
                want.push_str(&" ".repeat(width.saturating_sub(old.width())));
                assert_eq!(padded, want, "padded {format:?} at width {width}");
            }
        }
    }

    #[test]
    fn date_formats_survive_the_cache() {
        let epoch = DateTime::UNIX_EPOCH;
        for format in ["%Y", "%Y", "!%Y", "%Q", "%Q", "%b %d %H:%M", "", "plain"] {
            assert_eq!(date(epoch, format), reference::date(epoch, format));
        }
        // More formats than it keeps.
        for n in 0..40 {
            let format = format!("%Y-{n}");
            assert_eq!(date(epoch, &format), reference::date(epoch, &format));
        }
    }

    #[test]
    fn compiled_formats_are_remembered() {
        let one = compile("%a remembered");
        assert!(Rc::ptr_eq(&one, &compile("%a remembered")));
        for n in 0..40 {
            compile(&format!("%a {n}"));
        }
        // Forgotten, it is compiled again, to the same effect.
        assert_eq!(
            compile("%a remembered").expand(&Wide, 40),
            one.expand(&Wide, 40)
        );
    }
}
