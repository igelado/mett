//! mutt's `color`, `mono`, `uncolor` and `unmono` commands
//! (`docs/MUTTRC.md` §4.4): how each part of the screen is drawn.

use ratatui::style::{Color, Modifier, Style};
use regex::{Regex, RegexBuilder};

use crate::pattern::Pattern;

/// A part of the screen that `color` can set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Object {
    /// Text without a color of its own.
    Normal,
    /// The selected entry.
    Indicator,
    /// The status bar and the help line.
    Status,
    /// Error messages.
    Error,
    /// Other messages on the message line.
    Message,
    /// Questions on the message line.
    Prompt,
    /// Search matches in the pager.
    Search,
    /// The thread tree in the index.
    Tree,
    /// Header lines in the pager without a `color header` of their own.
    HeaderDefault,
    /// Quoted text at a depth (`quoted` is 0, `quoted1` is 1, ...).
    Quoted(usize),
    /// The signature, from `-- ` on.
    Signature,
    /// The `~` after the end of a message in the pager.
    Tilde,
    /// The `+` at the start of wrapped lines.
    Markers,
    /// Attachments in the compose menu.
    Attachment,
    /// Index entries matching a pattern.
    Index,
    /// Header lines matching a regular expression.
    Header,
    /// Text in the body matching a regular expression.
    Body,
}

/// How many `quoted` levels there are (`quoted` to `quoted9`).
const QUOTED_LEVELS: usize = 10;

impl Object {
    fn parse(name: &str) -> Result<Self, String> {
        let name = name.to_ascii_lowercase();
        Ok(match name.as_str() {
            "normal" => Self::Normal,
            "indicator" => Self::Indicator,
            "status" => Self::Status,
            "error" => Self::Error,
            "message" => Self::Message,
            "prompt" => Self::Prompt,
            "search" => Self::Search,
            "tree" => Self::Tree,
            "hdrdefault" => Self::HeaderDefault,
            "signature" => Self::Signature,
            "tilde" => Self::Tilde,
            "markers" => Self::Markers,
            "attachment" => Self::Attachment,
            "index" => Self::Index,
            "header" => Self::Header,
            "body" => Self::Body,
            "quoted" => Self::Quoted(0),
            _ => match name.strip_prefix("quoted").and_then(|n| n.parse().ok()) {
                Some(level) if level < QUOTED_LEVELS => Self::Quoted(level),
                _ => return Err(format!("{name}: no such object")),
            },
        })
    }

    /// Whether the object takes a pattern or a regular expression.
    fn matches(self) -> bool {
        matches!(self, Self::Index | Self::Header | Self::Body)
    }
}

/// A color rule for text matching a regular expression.
#[derive(Clone, Debug)]
pub struct RegexRule {
    /// The expression, as written, for `uncolor`.
    pub text: String,
    /// The expression.
    pub regex: Regex,
    /// How to draw what matches.
    pub style: Style,
}

/// A color rule for index entries matching a pattern.
#[derive(Clone, Debug)]
pub struct IndexRule {
    /// The pattern, as written, for `uncolor`.
    pub text: String,
    /// The pattern.
    pub pattern: Pattern,
    /// How to draw what matches.
    pub style: Style,
}

/// How each part of the screen is drawn.
#[derive(Clone, Debug)]
pub struct Colors {
    pub normal: Style,
    pub indicator: Style,
    pub status: Style,
    pub error: Style,
    pub message: Style,
    pub prompt: Style,
    pub search: Style,
    pub tree: Style,
    pub header_default: Style,
    pub quoted: [Style; QUOTED_LEVELS],
    pub signature: Style,
    pub tilde: Style,
    pub markers: Style,
    pub attachment: Style,
    /// `color index`, in the order given: the last that matches counts.
    pub index: Vec<IndexRule>,
    /// `color header`: the last that matches counts.
    pub header: Vec<RegexRule>,
    /// `color body`: each colors what it matches.
    pub body: Vec<RegexRule>,
}

const REVERSED: Style = Style::new().add_modifier(Modifier::REVERSED);

impl Default for Colors {
    /// mutt's defaults on a terminal without colors.
    fn default() -> Self {
        Self {
            normal: Style::new(),
            indicator: REVERSED,
            status: REVERSED,
            error: Style::new(),
            message: Style::new(),
            prompt: Style::new(),
            search: REVERSED,
            tree: Style::new(),
            header_default: Style::new(),
            quoted: [Style::new(); QUOTED_LEVELS],
            signature: Style::new(),
            tilde: Style::new(),
            markers: Style::new(),
            attachment: Style::new(),
            index: Vec::new(),
            header: Vec::new(),
            body: Vec::new(),
        }
    }
}

impl PartialEq for Colors {
    /// Rules compare by what was written.
    fn eq(&self, other: &Self) -> bool {
        let rules = |rules: &[RegexRule]| -> Vec<(String, Style)> {
            rules.iter().map(|r| (r.text.clone(), r.style)).collect()
        };
        let index = |rules: &[IndexRule]| -> Vec<(String, Style)> {
            rules.iter().map(|r| (r.text.clone(), r.style)).collect()
        };
        self.plain() == other.plain()
            && index(&self.index) == index(&other.index)
            && rules(&self.header) == rules(&other.header)
            && rules(&self.body) == rules(&other.body)
    }
}

impl Eq for Colors {}

impl Colors {
    /// The styles of the objects without rules.
    fn plain(&self) -> Vec<Style> {
        let mut styles = vec![
            self.normal,
            self.indicator,
            self.status,
            self.error,
            self.message,
            self.prompt,
            self.search,
            self.tree,
            self.header_default,
            self.signature,
            self.tilde,
            self.markers,
            self.attachment,
        ];
        styles.extend(self.quoted);
        styles
    }

    /// The style of an object without rules.
    fn plain_mut(&mut self, object: Object) -> &mut Style {
        match object {
            Object::Normal => &mut self.normal,
            Object::Indicator => &mut self.indicator,
            Object::Status => &mut self.status,
            Object::Error => &mut self.error,
            Object::Message => &mut self.message,
            Object::Prompt => &mut self.prompt,
            Object::Search => &mut self.search,
            Object::Tree => &mut self.tree,
            Object::HeaderDefault => &mut self.header_default,
            Object::Quoted(level) => &mut self.quoted[level],
            Object::Signature => &mut self.signature,
            Object::Tilde => &mut self.tilde,
            Object::Markers => &mut self.markers,
            Object::Attachment => &mut self.attachment,
            Object::Index | Object::Header | Object::Body => {
                unreachable!("objects with rules have no style of their own")
            }
        }
    }

    /// `color OBJECT [ATTRIBUTE ...] FOREGROUND BACKGROUND [EXPRESSION]`.
    pub fn color(&mut self, args: &[String]) -> Result<(), String> {
        let (object, rest) = args
            .split_first()
            .ok_or("color: expected an object, colors and maybe a pattern")?;
        let object = Object::parse(object)?;
        // Attributes come first; the two colors after them.
        let attributes = rest
            .iter()
            .take_while(|word| attribute(word).is_some())
            .count();
        let (attrs, rest) = rest.split_at(attributes);
        let (fg, bg, expression) = match (rest, object.matches()) {
            ([fg, bg], false) => (fg, bg, None),
            ([fg, bg, expression], true) => (fg, bg, Some(expression.as_str())),
            (_, false) => {
                return Err("color: expected an object, then a foreground and a background".into());
            }
            (_, true) => {
                return Err(
                    "color: expected an object, a foreground, a background and an expression"
                        .into(),
                );
            }
        };
        let mut style = Style::new().fg(color(fg)?.0).bg(color(bg)?.0);
        if color(fg)?.1 {
            style = style.add_modifier(Modifier::BOLD);
        }
        for attr in attrs {
            style = with_attribute(style, attr);
        }
        self.set(object, style, expression)
    }

    /// `mono OBJECT ATTRIBUTE [EXPRESSION]`: like `color`, for terminals
    /// without colors. mett has colors, so the attribute is added to the
    /// object's colors.
    pub fn mono(&mut self, args: &[String]) -> Result<(), String> {
        let [object, attr, rest @ ..] = args else {
            return Err("mono: expected an object and an attribute".into());
        };
        let object = Object::parse(object)?;
        if attribute(attr).is_none() {
            return Err(format!("mono: {attr}: no such attribute"));
        }
        let style = with_attribute(Style::new(), attr);
        match (rest, object.matches()) {
            ([], false) => self.set(object, style, None),
            ([expression], true) => self.set(object, style, Some(expression)),
            _ => Err("mono: expected an object, an attribute and maybe an expression".into()),
        }
    }

    fn set(
        &mut self,
        object: Object,
        style: Style,
        expression: Option<&str>,
    ) -> Result<(), String> {
        let Some(text) = expression else {
            *self.plain_mut(object) = style;
            return Ok(());
        };
        if object == Object::Index {
            let pattern = Pattern::parse(text).map_err(|e| format!("color index: {e}"))?;
            // Given again, it takes its new colors and the last place.
            self.index.retain(|rule| rule.text != text);
            self.index.push(IndexRule {
                text: text.to_owned(),
                pattern,
                style,
            });
            return Ok(());
        }
        // As in mutt, case matters only with an upper-case letter.
        let regex = RegexBuilder::new(text)
            .case_insensitive(!text.chars().any(char::is_uppercase))
            .build()
            .map_err(|e| format!("{text}: {e}"))?;
        let rules = if object == Object::Header {
            &mut self.header
        } else {
            &mut self.body
        };
        rules.retain(|rule| rule.text != text);
        rules.push(RegexRule {
            text: text.to_owned(),
            regex,
            style,
        });
        Ok(())
    }

    /// `uncolor OBJECT [* | EXPRESSION ...]`: for `index`, `header` and
    /// `body`, forgets the rules for the expressions (`*` for all); other
    /// objects go back to their default.
    pub fn uncolor(&mut self, args: &[String]) -> Result<(), String> {
        let (object, expressions) = args.split_first().ok_or("uncolor: expected an object")?;
        let object = Object::parse(object)?;
        if !object.matches() {
            if !expressions.is_empty() {
                return Err(format!("uncolor: {} takes no expression", args[0]));
            }
            *self.plain_mut(object) = *Colors::default().plain_mut(object);
            return Ok(());
        }
        if expressions.is_empty() {
            return Err(format!("uncolor: {}: expected * or an expression", args[0]));
        }
        let all = expressions.iter().any(|e| e == "*");
        let keep = |text: &String| !all && !expressions.contains(text);
        match object {
            Object::Index => self.index.retain(|rule| keep(&rule.text)),
            Object::Header => self.header.retain(|rule| keep(&rule.text)),
            _ => self.body.retain(|rule| keep(&rule.text)),
        }
        Ok(())
    }

    /// The style for an index entry: the last `color index` it matches.
    pub fn index_style(&self, matches: impl Fn(&Pattern) -> bool) -> Style {
        self.index
            .iter()
            .rev()
            .find(|rule| matches(&rule.pattern))
            .map_or(self.normal, |rule| self.normal.patch(rule.style))
    }

    /// The style for a header line in the pager: the last `color header`
    /// it matches, or `hdrdefault`.
    pub fn header_style(&self, line: &str) -> Style {
        let style = self
            .header
            .iter()
            .rev()
            .find(|rule| rule.regex.is_match(line))
            .map_or(self.header_default, |rule| rule.style);
        self.normal.patch(style)
    }

    /// The style for quoted text `depth` levels deep (1 or more); levels
    /// beyond `quoted9` take the deepest set, as in mutt.
    pub fn quoted_style(&self, depth: usize) -> Style {
        let level = depth.saturating_sub(1).min(QUOTED_LEVELS - 1);
        self.normal.patch(self.quoted[level])
    }
}

/// A color by name, and whether it is a `bright` one (bold, in mutt).
fn color(name: &str) -> Result<(Color, bool), String> {
    let lower = name.to_ascii_lowercase();
    let (base, bright) = match lower.strip_prefix("bright") {
        Some(base) => (base, true),
        None => (lower.as_str(), false),
    };
    let color = match base {
        "default" => Color::Reset,
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::Gray,
        _ => match base.strip_prefix("color").map(str::parse::<u8>) {
            Some(Ok(n)) => Color::Indexed(n),
            _ => match base.strip_prefix('#').filter(|hex| hex.len() == 6) {
                Some(hex) => match u32::from_str_radix(hex, 16) {
                    Ok(rgb) => Color::from_u32(rgb),
                    Err(_) => return Err(format!("{name}: no such color")),
                },
                None => return Err(format!("{name}: no such color")),
            },
        },
    };
    // The bright colors of the first eight.
    let brighter = match color {
        Color::Black => Color::DarkGray,
        Color::Red => Color::LightRed,
        Color::Green => Color::LightGreen,
        Color::Yellow => Color::LightYellow,
        Color::Blue => Color::LightBlue,
        Color::Magenta => Color::LightMagenta,
        Color::Cyan => Color::LightCyan,
        Color::Gray => Color::White,
        other => other,
    };
    Ok(if bright {
        (brighter, false)
    } else {
        (color, false)
    })
}

/// The modifier an attribute name stands for (none for `none`/`normal`).
fn attribute(name: &str) -> Option<Modifier> {
    Some(match name.to_ascii_lowercase().as_str() {
        "none" | "normal" => Modifier::empty(),
        "bold" => Modifier::BOLD,
        "underline" => Modifier::UNDERLINED,
        "reverse" | "standout" => Modifier::REVERSED,
        "blink" => Modifier::SLOW_BLINK,
        "italic" => Modifier::ITALIC,
        _ => return None,
    })
}

fn with_attribute(style: Style, name: &str) -> Style {
    match attribute(name) {
        Some(modifier) if modifier.is_empty() => style.remove_modifier(Modifier::all()),
        Some(modifier) => style.add_modifier(modifier),
        None => style,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn sets_objects_and_rules() {
        let mut colors = Colors::default();
        colors
            .color(&words("indicator bold brightwhite blue"))
            .unwrap();
        assert_eq!(
            colors.indicator,
            Style::new()
                .fg(Color::White)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD)
        );
        colors.color(&words("quoted1 color130 default")).unwrap();
        assert_eq!(colors.quoted_style(2).fg, Some(Color::Indexed(130)));
        assert_eq!(colors.quoted_style(20).fg, None, "quoted9 is unset");
        colors.color(&words("index red default ~N")).unwrap();
        colors.color(&words("index green default ~F")).unwrap();
        colors.color(&words("index red default ~N")).unwrap();
        let texts: Vec<_> = colors.index.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["~F", "~N"], "given again, a rule moves last");
        colors
            .color(&words("header yellow default ^subject:"))
            .unwrap();
        assert_eq!(colors.header_style("Subject: hi").fg, Some(Color::Yellow));
        assert_eq!(colors.header_style("From: a").fg, None);
        colors.mono(&words("body underline http://\\S+")).unwrap();
        assert_eq!(colors.body.len(), 1);
    }

    #[test]
    fn rejects_mistakes() {
        let mut colors = Colors::default();
        for bad in [
            "",
            "nothing red blue",
            "status red",
            "status mauve blue",
            "index red blue",
            "index red blue ~Q",
            "body red blue (",
            "status red blue extra",
        ] {
            assert!(colors.color(&words(bad)).is_err(), "{bad}");
        }
        assert!(colors.mono(&words("status loud")).is_err());
        assert_eq!(colors, Colors::default());
    }

    #[test]
    fn uncolor_forgets_rules_or_resets() {
        let mut colors = Colors::default();
        colors.color(&words("status red blue")).unwrap();
        colors.color(&words("index red default ~N")).unwrap();
        colors.color(&words("index red default ~F")).unwrap();
        colors.uncolor(&words("index ~N")).unwrap();
        assert_eq!(colors.index.len(), 1);
        colors.uncolor(&words("index *")).unwrap();
        colors.uncolor(&words("status")).unwrap();
        assert_eq!(colors, Colors::default());
        assert!(colors.uncolor(&words("index")).is_err());
    }
}
