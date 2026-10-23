//! Connecting to an IMAP account from the configuration: the server and its
//! login from the `spoolfile` (or `folder`) URL and `imap_user`/`imap_pass`,
//! the SMTP server from `smtp_url` and `smtp_pass`, and how the connections
//! are protected from the `ssl_*` variables (`docs/MUTTRC.md` §4.13).

use mett_core::Address;
use mett_imap::{ImapMailStore, ImapSettings, Login, NoTokens, Security};
use mett_smtp::{Login as SmtpLogin, SmtpSender, SmtpSettings};

use crate::config::Config;
use crate::config::mailbox::{MailboxUrl, Server};
use crate::config::server::ServerUrl;

/// How a server's connection is protected, by its scheme and the `ssl_*`
/// variables, as mutt does: `imaps` and `smtps` are TLS from the start;
/// `imap` and `smtp` upgrade with STARTTLS when the server offers it
/// (`ssl_starttls`), and fail without it if `ssl_force_tls` is set.
fn security(config: &Config, server: &ServerUrl) -> Result<Security, String> {
    if server.tls {
        return Ok(Security::Tls);
    }
    match (config.flag("ssl_starttls"), config.flag("ssl_force_tls")) {
        (true, true) => Ok(Security::StartTls),
        (true, false) => Ok(Security::StartTlsIfOffered),
        (false, false) => Ok(Security::None),
        (false, true) => Err(format!(
            "ssl_force_tls is set, but ssl_starttls is not: {} can't be protected",
            server.host
        )),
    }
}

/// The certificate authorities of `ssl_ca_certificate_file`, as PEM.
fn certificates(config: &Config) -> Result<Vec<Vec<u8>>, String> {
    match config.string("ssl_ca_certificate_file") {
        "" => Ok(Vec::new()),
        path => std::fs::read(path)
            .map(|pem| vec![pem])
            .map_err(|e| format!("ssl_ca_certificate_file: {path}: {e}")),
    }
}

/// The server of an IMAP mailbox.
fn imap_server(url: &MailboxUrl) -> Result<&ServerUrl, String> {
    match &url.server {
        Server::Imap(server) => Ok(server),
        Server::Office365 => Err(format!("{url}: not an IMAP mailbox")),
    }
}

/// How to reach the IMAP server of `url`.
pub fn imap_settings(config: &Config, url: &MailboxUrl) -> Result<ImapSettings, String> {
    let server = imap_server(url)?;
    let mut settings = ImapSettings::new(&server.host, security(config, server)?);
    if let Some(port) = server.port {
        settings.port = port;
    }
    settings.verify = config.flag("ssl_verify_host");
    settings.ca_certificates = certificates(config)?;
    Ok(settings)
}

/// How to log in to the IMAP server of `url`.
pub fn imap_login(config: &Config, url: &MailboxUrl) -> Result<Login, String> {
    let server = imap_server(url)?;
    let user = server
        .user
        .clone()
        .filter(|u| !u.is_empty())
        .or_else(|| Some(config.string("imap_user").to_owned()).filter(|u| !u.is_empty()))
        .ok_or("imap_user is not set: give the user in the URL, or set it")?;
    let password = server
        .password
        .clone()
        .or_else(|| Some(config.string("imap_pass").to_owned()).filter(|p| !p.is_empty()))
        .ok_or("imap_pass is not set")?;
    Ok(Login::Password { user, password })
}

/// How to reach the SMTP server `smtp_url` names, and log in to it, if it
/// names one; `from` is who the mail is from.
pub fn smtp_sender(
    config: &Config,
    url: &MailboxUrl,
    from: &Address,
) -> Result<Option<SmtpSender<NoTokens>>, String> {
    let text = config.string("smtp_url");
    if text.is_empty() {
        return Ok(None);
    }
    let server = ServerUrl::parse(text, "smtp", "smtps").map_err(|e| format!("smtp_url: {e}"))?;
    let mut settings = SmtpSettings::new(&server.host, security(config, &server)?, from.clone());
    if let Some(port) = server.port {
        settings.port = port;
    }
    settings.verify = config.flag("ssl_verify_host");
    settings.ca_certificates = certificates(config)?;
    // Like mutt, mett logs in only as the user the URL names; the password
    // is the URL's, or `smtp_pass`, or the mailbox's if the user is the same.
    // A URL with no user is a relay that takes mail without a login.
    let login = match &server.user {
        None => SmtpLogin::None,
        Some(user) => {
            let same_user = imap_login(config, url).ok().and_then(|login| match login {
                Login::Password {
                    user: same,
                    password,
                } if same == *user => Some(password),
                _ => None,
            });
            let password = server
                .password
                .clone()
                .or_else(|| Some(config.string("smtp_pass").to_owned()).filter(|p| !p.is_empty()))
                .or(same_user)
                .ok_or("smtp_pass is not set")?;
            SmtpLogin::Password {
                user: user.clone(),
                password,
            }
        }
    };
    Ok(Some(SmtpSender::new(settings, login)))
}

/// Who the mailbox's owner writes as: `from` and `realname`, or the account.
fn sender(config: &Config, url: &MailboxUrl) -> Result<Address, String> {
    let mut from = match config.string("from") {
        "" => Address::new(url.account.as_str()),
        from => Address::parse(from).map_err(|e| format!("from: {e}"))?,
    };
    if from.name.is_none() && !config.string("realname").is_empty() {
        from.name = Some(config.string("realname").to_owned());
    }
    Ok(from)
}

/// The mailbox of the IMAP account in `url`, from the configuration.
/// Nothing is sent until it is used: the first request connects.
pub fn mailbox(config: &Config, url: &MailboxUrl) -> Result<ImapMailStore, String> {
    let from = sender(config, url)?;
    let mut store = ImapMailStore::new(
        imap_settings(config, url)?,
        imap_login(config, url)?,
        from.clone(),
    );
    if let Some(smtp) = smtp_sender(config, url, &from)? {
        store = store.with_smtp(smtp);
    }
    Ok(store)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn config(source: &str) -> Config {
        let mut config = Config::new(Some("/home/a".into()));
        config.load_str(source, Path::new("mettrc"));
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        config
    }

    fn url(text: &str) -> MailboxUrl {
        MailboxUrl::parse(text).unwrap()
    }

    #[test]
    fn imaps_is_tls_and_imap_upgrades_when_it_can() {
        let c = config("");
        let tls = imap_settings(&c, &url("imaps://me@mail.example.com/")).unwrap();
        assert_eq!((tls.security, tls.port), (Security::Tls, 993));
        let plain = imap_settings(&c, &url("imap://me@mail.example.com/")).unwrap();
        assert_eq!(
            (plain.security, plain.port),
            (Security::StartTlsIfOffered, 143)
        );
        let ported = imap_settings(&c, &url("imap://me@mail.example.com:1143/")).unwrap();
        assert_eq!(ported.port, 1143);
    }

    #[test]
    fn the_ssl_variables_say_how_a_plain_scheme_is_protected() {
        let imap = url("imap://me@mail.example.com/");
        let required = config("set ssl_force_tls=yes\n");
        assert_eq!(
            imap_settings(&required, &imap).unwrap().security,
            Security::StartTls
        );
        let none = config("set ssl_starttls=no\n");
        assert_eq!(
            imap_settings(&none, &imap).unwrap().security,
            Security::None
        );
        let both = config("set ssl_starttls=no ssl_force_tls=yes\n");
        assert!(
            imap_settings(&both, &imap)
                .unwrap_err()
                .contains("ssl_force_tls")
        );
        // TLS from the start doesn't depend on them.
        let tls = url("imaps://me@mail.example.com/");
        assert_eq!(imap_settings(&none, &tls).unwrap().security, Security::Tls);
    }

    #[test]
    fn verification_and_extra_authorities_follow_the_variables() {
        let imap = url("imaps://me@mail.example.com/");
        assert!(imap_settings(&config(""), &imap).unwrap().verify);
        assert!(
            !imap_settings(&config("set ssl_verify_host=no\n"), &imap)
                .unwrap()
                .verify
        );
        let missing = config("set ssl_ca_certificate_file=/nonexistent/ca.pem\n");
        assert!(
            imap_settings(&missing, &imap)
                .unwrap_err()
                .contains("ssl_ca_certificate_file")
        );
        let dir = std::env::temp_dir().join(format!("mett-ca-{}", std::process::id()));
        std::fs::write(&dir, b"PEM").unwrap();
        let c = config(&format!("set ssl_ca_certificate_file={}\n", dir.display()));
        assert_eq!(
            imap_settings(&c, &imap).unwrap().ca_certificates,
            [b"PEM".to_vec()]
        );
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn the_login_is_the_urls_or_the_variables() {
        let c = config("set imap_user=me imap_pass=secret\n");
        let from_variables = imap_login(&c, &url("imaps://mail.example.com/")).unwrap();
        assert!(matches!(
            from_variables,
            Login::Password { user, password } if user == "me" && password == "secret"
        ));
        let from_url = imap_login(&c, &url("imaps://you:pw@mail.example.com/")).unwrap();
        assert!(matches!(
            from_url,
            Login::Password { user, password } if user == "you" && password == "pw"
        ));
        let unset = config("");
        assert!(
            imap_login(&unset, &url("imaps://mail.example.com/"))
                .unwrap_err()
                .contains("imap_user")
        );
        assert!(
            imap_login(&unset, &url("imaps://me@mail.example.com/"))
                .unwrap_err()
                .contains("imap_pass")
        );
    }

    #[test]
    fn smtp_takes_its_own_url_and_the_mailboxs_user_and_password() {
        let me = Address::new("me@example.com");
        let imap = url("imaps://me@mail.example.com/");
        assert!(smtp_sender(&config(""), &imap, &me).unwrap().is_none());

        // No user in `smtp_url`: a relay that takes mail without a login.
        let c = config("set imap_pass=secret smtp_url=smtps://smtp.example.com\n");
        let relay = smtp_sender(&c, &imap, &me).unwrap().unwrap();
        assert!(format!("{relay:?}").contains("login: None"), "{relay:?}");

        // The mailbox's user logs in with the mailbox's password.
        let c = config("set imap_pass=secret smtp_url=smtps://me@smtp.example.com\n");
        let same = smtp_sender(&c, &imap, &me).unwrap().unwrap();
        assert!(format!("{same:?}").contains("Password"), "{same:?}");

        // Another user needs a password of its own.
        let c = config("set imap_pass=secret smtp_url=smtp://other@smtp.example.com:2525\n");
        assert_eq!(
            smtp_sender(&c, &imap, &me).unwrap_err(),
            "smtp_pass is not set"
        );
        let c =
            config("set imap_pass=secret smtp_pass=other smtp_url=smtp://other@smtp.example.com\n");
        assert!(smtp_sender(&c, &imap, &me).unwrap().is_some());

        let bad = config("set smtp_url=http://smtp.example.com\n");
        assert!(
            smtp_sender(&bad, &imap, &me)
                .unwrap_err()
                .starts_with("smtp_url:")
        );
    }

    #[test]
    fn mail_is_from_from_and_realname_or_the_account() {
        let imap = url("imaps://me@mail.example.com/");
        let c = config("set imap_pass=x\n");
        assert_eq!(
            sender(&c, &imap).unwrap(),
            Address::new("me@mail.example.com")
        );
        let c = config("set imap_pass=x from=me@example.com realname=\"Me Myself\"\n");
        assert_eq!(
            sender(&c, &imap).unwrap(),
            Address::new("me@example.com").with_name("Me Myself")
        );
        assert!(mailbox(&c, &imap).is_ok());
    }
}
