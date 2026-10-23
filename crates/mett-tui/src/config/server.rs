//! The URLs of IMAP and SMTP servers: `imap://`, `imaps://`, `smtp://` and
//! `smtps://`, as mutt writes them.

/// A server, from `scheme://[user[:password]@]host[:port][/path]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerUrl {
    /// Whether the scheme says TLS from the first byte (`imaps`, `smtps`).
    pub tls: bool,
    /// The server's name.
    pub host: String,
    /// Its port, if the URL names one.
    pub port: Option<u16>,
    /// The user, if the URL names one.
    pub user: Option<String>,
    /// The password, if the URL gives one.
    pub password: Option<String>,
    /// What follows the first `/` after the server, still percent-encoded.
    pub path: String,
}

/// `url` with the password in it, if any, hidden: for messages, which are
/// shown and may be logged. What comes before the last `@` and has a `:` in
/// it may be a password (which may hold a `/` or an `@` a mistake left
/// unescaped), so it is hidden from the `:` on.
pub fn redact(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, url),
    };
    let Some((userinfo, after)) = rest.rsplit_once('@') else {
        return url.to_owned();
    };
    let Some((user, _)) = userinfo.split_once(':') else {
        return url.to_owned();
    };
    match scheme {
        Some(scheme) => format!("{scheme}://{user}:***@{after}"),
        None => format!("{user}:***@{after}"),
    }
}

/// `url` without its password, if it has one, and otherwise as it is: for
/// what is kept and shown (unlike [`redact`], which is for the messages
/// about a URL that may be a mistake, and hides all it can't tell is not a
/// password). Only the authority is looked at, so a path with an `@` in it
/// stays as it is.
pub fn strip_password(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, Some(path)),
        None => (rest, None),
    };
    let authority = match authority.rsplit_once('@') {
        Some((userinfo, host)) => match userinfo.split_once(':') {
            Some((user, _)) => format!("{user}@{host}"),
            None => authority.to_owned(),
        },
        None => authority.to_owned(),
    };
    match path {
        Some(path) => format!("{scheme}://{authority}/{path}"),
        None => format!("{scheme}://{authority}"),
    }
}

impl ServerUrl {
    /// Parses `url`, whose scheme is `plain` (`imap`) or `secure`
    /// (`imaps`).
    pub fn parse(url: &str, plain: &str, secure: &str) -> Result<Self, String> {
        // The URL may hold a password: messages show it without.
        let shown = redact(url);
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| format!("{shown}: not a {plain}:// or {secure}:// URL"))?;
        let tls = if scheme.eq_ignore_ascii_case(secure) {
            true
        } else if scheme.eq_ignore_ascii_case(plain) {
            false
        } else {
            return Err(format!("{shown}: expected {plain}:// or {secure}://"));
        };
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((userinfo, hostport)) => (Some(userinfo), hostport),
            None => (None, authority),
        };
        let (host, port) = match hostport.strip_prefix('[') {
            // `[::1]:993`
            Some(v6) => match v6.split_once(']') {
                Some((host, "")) => (host, None),
                Some((host, port)) => match port.strip_prefix(':') {
                    Some(port) => (host, Some(port)),
                    None => return Err(format!("{shown}: text after the host in [ ]")),
                },
                None => return Err(format!("{shown}: unclosed [ in the host")),
            },
            None => match hostport.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (hostport, None),
            },
        };
        if host.is_empty() {
            return Err(format!("{shown}: no host"));
        }
        let port = port
            .map(|p| {
                p.parse::<u16>()
                    .map_err(|_| format!("{shown}: the port is not a number up to 65535"))
            })
            .transpose()?;
        let (user, password) = match userinfo {
            None => (None, None),
            Some(info) => match info.split_once(':') {
                Some((user, password)) => (Some(decode(user)?), Some(decode(password)?)),
                None => (Some(decode(info)?), None),
            },
        };
        Ok(Self {
            tls,
            host: host.to_owned(),
            port,
            user: user.filter(|u| !u.is_empty()),
            password,
            path: path.to_owned(),
        })
    }
}

/// `text` with its `%XX` escapes undone.
pub fn decode(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or("a bad % escape in a user name or password")?;
            out.push(hex);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "not UTF-8 once unescaped".to_owned())
}

/// `text` with what a URL's user name can't hold escaped.
pub fn encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imap(url: &str) -> Result<ServerUrl, String> {
        ServerUrl::parse(url, "imap", "imaps")
    }

    #[test]
    fn parses_a_server() {
        let url = imap("imaps://me%40example.com:p%3Ass@mail.example.com:1993/INBOX/Work").unwrap();
        assert!(url.tls);
        assert_eq!(url.host, "mail.example.com");
        assert_eq!(url.port, Some(1993));
        assert_eq!(url.user.as_deref(), Some("me@example.com"));
        assert_eq!(url.password.as_deref(), Some("p:ss"));
        assert_eq!(url.path, "INBOX/Work");

        let url = imap("imap://mail.example.com").unwrap();
        assert!(!url.tls);
        assert_eq!((url.port, url.user, url.password), (None, None, None));
        assert_eq!(url.path, "");
    }

    #[test]
    fn takes_the_last_at_sign_as_the_end_of_the_user() {
        let url = imap("imap://me@example.com@mail.example.com/").unwrap();
        assert_eq!(url.user.as_deref(), Some("me@example.com"));
        assert_eq!(url.host, "mail.example.com");
    }

    #[test]
    fn parses_an_ipv6_host() {
        let url = imap("imap://[::1]:143/").unwrap();
        assert_eq!((url.host.as_str(), url.port), ("::1", Some(143)));
        assert!(imap("imap://[::1/").is_err());
        // Nothing but a port may follow the brackets.
        assert!(imap("imap://[::1]junk/INBOX").is_err());
        assert!(imap("imap://[::1]:x/INBOX").is_err());
    }

    #[test]
    fn rejects_what_is_not_a_server() {
        for bad in [
            "msgraph://a@b.c/",
            "imap:///INBOX",
            "imap://host:port/",
            "imap://host:99999/",
            "imap://a%zz@host/",
            "imap://me:pw@host:port/",
            "host",
        ] {
            assert!(imap(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn escapes_round_trip() {
        for text in ["me@example.com", "p:ss/w%rd", "ünï"] {
            assert_eq!(decode(&encode(text)).unwrap(), text);
        }
    }

    #[test]
    fn a_password_is_dropped_from_the_authority_alone() {
        assert_eq!(
            strip_password("imaps://alice:secret@mail.example.com/@inbox"),
            "imaps://alice@mail.example.com/@inbox"
        );
        assert_eq!(
            strip_password("imaps://alice:secret@mail.example.com"),
            "imaps://alice@mail.example.com"
        );
        for kept in [
            "imaps://alice@mail.example.com/@inbox/Work",
            "imaps://mail.example.com:993/@inbox",
            "msgraph://a@b.c/@inbox",
            "=Work",
            "/var/mail/box",
        ] {
            assert_eq!(strip_password(kept), kept);
        }
    }

    #[test]
    fn errors_do_not_show_passwords() {
        for bad in [
            "imap://me:hunter2@host:port/",
            "imap://me:hunter2@:993/",
            "imapx://me:hunter2@host/",
            "imap://me:hunter2@[::1/",
            "imaps://alice:hunter2/x@host/INBOX",
            "imap://me:hunter2@[::1]junk/",
            "imap://me:h%zzter2@host/",
        ] {
            let error = imap(bad).unwrap_err();
            assert!(!error.contains("hunter2"), "{error}");
        }
        assert_eq!(
            redact("imaps://me:hunter2@host/INBOX"),
            "imaps://me:***@host/INBOX"
        );
        assert_eq!(redact("imaps://me@host/INBOX"), "imaps://me@host/INBOX");
        assert_eq!(redact("INBOX"), "INBOX");
        assert_eq!(redact("imap:/me:hunter2@host/INBOX"), "imap:***@host/INBOX");
        assert_eq!(
            redact("imaps://alice:p/ss@host/INBOX"),
            "imaps://alice:***@host/INBOX"
        );
        assert!(
            !imap("imap:/me:hunter2@host/INBOX")
                .unwrap_err()
                .contains("hunter2")
        );
    }
}
