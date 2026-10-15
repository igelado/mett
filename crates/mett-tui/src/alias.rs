//! mutt's aliases: short names for addresses, which the `To:`, `Cc:` and
//! `Bcc:` questions expand, and complete with Tab.

/// An alias: `alias KEY ADDRESSES`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alias {
    /// The name typed for it.
    pub key: String,
    /// The addresses it stands for, as written (a list, which may hold
    /// other aliases).
    pub value: String,
}

/// How deep aliases may refer to other aliases, to stop loops.
const MAX_DEPTH: usize = 16;

/// `text` (a list of addresses, as typed) with the aliases in it replaced
/// by what they stand for. An entry is an alias when it is a key (ignoring
/// case), and not an address.
pub fn expand(text: &str, aliases: &[Alias]) -> Result<String, String> {
    expand_at(text, aliases, 0)
}

fn expand_at(text: &str, aliases: &[Alias], depth: usize) -> Result<String, String> {
    let mut out = Vec::new();
    for entry in split(text) {
        let trimmed = entry.trim();
        // An entry that is all one quoted string (`"joe@x.org"`, or a
        // list, as in `alias team "bob, jane"`) stands for what it holds.
        if let Some(inner) = unquote(trimmed) {
            if depth >= MAX_DEPTH {
                return Err(format!("{trimmed} is quoted too deep"));
            }
            let inner = expand_at(&inner, aliases, depth + 1)?;
            if !inner.is_empty() {
                out.push(inner);
            }
            continue;
        }
        match find(aliases, trimmed) {
            Some(alias) if depth >= MAX_DEPTH => {
                return Err(format!("alias {} refers to itself", alias.key));
            }
            Some(alias) => out.push(expand_at(&alias.value, aliases, depth + 1)?),
            None if trimmed.is_empty() => {}
            None => out.push(trimmed.to_owned()),
        }
    }
    Ok(out.join(", "))
}

/// Whether `key` can name an alias: a word that is not an address, and
/// that a mettrc reads back as it is.
pub fn valid_key(key: &str) -> bool {
    // Nor an option, such as `-group`.
    !key.is_empty()
        && !key.starts_with('-')
        && !key
            .contains(|c: char| c.is_whitespace() || c.is_control() || "@<>,;\"'`$#\\".contains(c))
}

/// The mettrc line `alias KEY VALUE`, which reads back as `value`: quotes
/// in it stay (they are part of the addresses), and what a mettrc would
/// take for something else is escaped.
pub fn line(key: &str, value: &str) -> String {
    let mut out = format!("alias {key} ");
    let mut quoted = false;
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            // A quoted pair stays as it is: the mettrc reads it alike.
            '\\' if quoted => {
                out.push(c);
                out.extend(chars.next());
                continue;
            }
            '$' | '`' => out.push('\\'),
            '\\' | '#' | ';' | '\'' if !quoted => out.push('\\'),
            // Control characters (a line break would end the command) go
            // as escapes the mettrc reads back.
            c if c.is_control() => {
                out.push_str(&match c {
                    '\n' => "\\n".to_owned(),
                    '\r' => "\\r".to_owned(),
                    '\t' => "\\t".to_owned(),
                    '\u{1b}' => "\\e".to_owned(),
                    '\u{7f}' => "\\c?".to_owned(),
                    c => format!("\\c{}", char::from(c as u8 | 0x40)),
                });
                continue;
            }
            _ => {}
        }
        out.push(c);
    }
    out
}

/// What `text` holds, if it is all one quoted string.
fn unquote(text: &str) -> Option<String> {
    let inner = text.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            // Another string starts: it isn't all one.
            '"' => return None,
            _ => out.push(c),
        }
    }
    Some(out)
}

/// Whether two alias names are the same, ignoring case (in any script, as
/// completion does).
pub fn same_key(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// The alias called `key`, ignoring case, as mutt does.
pub fn find<'a>(aliases: &'a [Alias], key: &str) -> Option<&'a Alias> {
    if key.is_empty() || key.contains(['@', '<', '"', ' ']) {
        return None;
    }
    aliases.iter().find(|a| same_key(&a.key, key))
}

/// The aliases whose keys start with `prefix`, ignoring case.
pub fn completions<'a>(aliases: &'a [Alias], prefix: &str) -> Vec<&'a Alias> {
    let prefix = prefix.to_lowercase();
    let mut found: Vec<&Alias> = aliases
        .iter()
        .filter(|a| a.key.to_lowercase().starts_with(&prefix))
        .collect();
    found.sort_by_key(|a| a.key.to_lowercase());
    found
}

/// The entry being typed at the end of an address list, and where it
/// starts: what Tab completes.
pub fn last_entry(text: &str) -> (usize, &str) {
    let start = split_points(text).last().copied().unwrap_or(0);
    let entry = &text[start..];
    let trimmed = entry.trim_start();
    (start + entry.len() - trimmed.len(), trimmed)
}

/// What follows the entry the cursor is in, from the comma that ends it:
/// `typed` is the text before the cursor, `rest` after it (read with what
/// came before: the cursor may be inside quotes). Completing an entry
/// replaces the rest of it.
pub fn after_entry<'a>(typed: &str, rest: &'a str) -> &'a str {
    let whole = format!("{typed}{rest}");
    match split_points(&whole).into_iter().find(|&p| p > typed.len()) {
        Some(next) => &rest[next - 1 - typed.len()..],
        None => "",
    }
}

/// The longest prefix all of `keys` share, ignoring case (in the first
/// key's spelling).
pub fn common_prefix(keys: &[&str]) -> String {
    let Some(first) = keys.first() else {
        return String::new();
    };
    let mut len = first.len();
    for key in &keys[1..] {
        len = first
            .char_indices()
            .zip(key.chars())
            .take_while(|((_, a), b)| a.to_lowercase().eq(b.to_lowercase()))
            .last()
            .map_or(0, |((i, c), _)| i + c.len_utf8())
            .min(len);
    }
    first[..len].to_owned()
}

/// The entries of an address list, split at commas outside quotes and
/// angle brackets.
fn split(text: &str) -> Vec<&str> {
    let mut entries = Vec::new();
    let mut start = 0;
    for point in split_points(text).into_iter().skip(1) {
        entries.push(&text[start..point - 1]);
        start = point;
    }
    entries.push(&text[start..]);
    entries
}

/// Where each entry of an address list starts.
fn split_points(text: &str) -> Vec<usize> {
    let mut points = vec![0];
    let (mut quoted, mut angle, mut escaped) = (false, false, false);
    for (i, c) in text.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' if !angle => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' if !quoted && !angle => points.push(i + 1),
            _ => {}
        }
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases() -> Vec<Alias> {
        let alias = |key: &str, value: &str| Alias {
            key: key.into(),
            value: value.into(),
        };
        vec![
            alias("bob", "Bob Smith <bob@x.org>"),
            alias("team", "bob, \"Doe, Jane\" <jane@x.org>"),
            alias("loop", "loop"),
            alias("bobby", "bobby@y.org"),
        ]
    }

    #[test]
    fn expands_aliases_in_address_lists() {
        assert_eq!(
            expand("Team, carol@z.org", &aliases()).unwrap(),
            "Bob Smith <bob@x.org>, \"Doe, Jane\" <jane@x.org>, carol@z.org"
        );
        assert_eq!(expand("bob@x.org, ", &aliases()).unwrap(), "bob@x.org");
        assert!(expand("loop", &aliases()).is_err());
        let accented = [Alias {
            key: "Émile".into(),
            value: "emile@x.org".into(),
        }];
        assert_eq!(expand("émile", &accented).unwrap(), "emile@x.org");
        // A quoted entry stands for what it holds.
        assert_eq!(
            expand(r#""joe@x.org", "bob, carol@z.org""#, &aliases()).unwrap(),
            "joe@x.org, Bob Smith <bob@x.org>, carol@z.org"
        );
        assert_eq!(
            expand(r#""Doe, J" <j@x.org>"#, &aliases()).unwrap(),
            r#""Doe, J" <j@x.org>"#
        );
    }

    #[test]
    fn keys_are_plain_words() {
        assert!(valid_key("bob-smith_2"));
        for key in [
            "", "-group", "a b", "a@b", "foo#bar", "a;b", "a$b", "a'b", "a\\b", "a,b",
        ] {
            assert!(!valid_key(key), "{key:?}");
        }
    }

    #[test]
    fn completes_the_last_entry() {
        assert_eq!(last_entry("a@x.org, bo"), (9, "bo"));
        assert_eq!(last_entry("\"Doe, J\" <j@x>,b"), (15, "b"));
        let aliases = aliases();
        let keys: Vec<&str> = completions(&aliases, "BO")
            .into_iter()
            .map(|a| a.key.as_str())
            .collect();
        assert_eq!(keys, ["bob", "bobby"]);
        assert_eq!(common_prefix(&keys), "bob");
        assert_eq!(common_prefix(&["team"]), "team");
        assert_eq!(after_entry("te", "am, x@y.org"), ", x@y.org");
        assert_eq!(after_entry("te", "am"), "");
        assert_eq!(after_entry("", "\"a, b\" <c@d>, e"), ", e");
        // Inside quotes, their comma is not the end.
        assert_eq!(after_entry("\"Doe", ", Jane\" <d@x.org>, e"), ", e");
    }
}
