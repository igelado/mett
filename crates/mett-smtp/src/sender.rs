//! Sending over SMTP.

use std::fmt;
use std::time::Duration;

use lettre::address::Envelope;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{Certificate, Tls, TlsParameters};
use lettre::transport::smtp::{self, AsyncSmtpTransport};
use lettre::{Address as MailAddress, AsyncTransport, Tokio1Executor};
use mett_core::{AccessToken, Address, Authenticator, Draft, Error, Result};

use crate::compose::{Threading, compose};

/// How the connection to the server is protected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Security {
    /// None: everything, passwords included, goes in plain text.
    None,
    /// The connection starts in plain text and is upgraded with STARTTLS
    /// if the server offers it (mutt's `ssl_starttls` without
    /// `ssl_force_tls`); one that doesn't is used in plain text.
    StartTlsIfOffered,
    /// The connection starts in plain text and is upgraded with STARTTLS
    /// (and fails if the server won't).
    #[default]
    StartTls,
    /// TLS from the first byte (the `smtps` port, 465).
    Tls,
}

/// How to reach an SMTP server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmtpSettings {
    /// The server's name.
    pub host: String,
    /// Its port.
    pub port: u16,
    /// How the connection is protected.
    pub security: Security,
    /// Whether to check the server's certificate (and its name). Turning
    /// this off leaves the connection open to impersonation.
    pub verify: bool,
    /// Certificate authorities to trust besides the system's, as PEM.
    pub ca_certificates: Vec<Vec<u8>>,
    /// The sender's address, for messages that don't name another.
    pub from: Address,
    /// How long to wait for the server at each step; a whole send is given
    /// three times as long.
    pub timeout: Duration,
}

impl SmtpSettings {
    /// Settings for `host`, with the port that goes with `security` and
    /// the checks on: 25 (and 587 for STARTTLS, which is what servers
    /// ask clients to use) or 465.
    pub fn new(host: impl Into<String>, security: Security, from: Address) -> Self {
        Self {
            host: host.into(),
            port: match security {
                Security::None => 25,
                Security::StartTls | Security::StartTlsIfOffered => 587,
                Security::Tls => 465,
            },
            security,
            verify: true,
            ca_certificates: Vec::new(),
            from,
            timeout: Duration::from_secs(60),
        }
    }
}

/// How to log in to the server.
pub enum Login<A = NoTokens> {
    /// Without logging in.
    None,
    /// With a password (SASL PLAIN or LOGIN).
    Password {
        /// The user name.
        user: String,
        /// The password.
        password: String,
    },
    /// With an OAuth2 access token (SASL XOAUTH2).
    OAuth {
        /// The user name.
        user: String,
        /// The source of tokens.
        tokens: A,
    },
}

impl<A> fmt::Debug for Login<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Password { user, .. } => f
                .debug_struct("Password")
                .field("user", user)
                .field("password", &"<redacted>")
                .finish(),
            Self::OAuth { user, .. } => f.debug_struct("OAuth").field("user", user).finish(),
        }
    }
}

/// The tokens of a [`Login`] that uses none.
#[derive(Clone, Copy, Debug)]
pub struct NoTokens;

impl Authenticator for NoTokens {
    async fn access_token(&self) -> Result<AccessToken> {
        Err(Error::Auth("there is no OAuth2 sign-in".into()))
    }
}

/// Sends mail through an SMTP server.
pub struct SmtpSender<A = NoTokens> {
    settings: SmtpSettings,
    login: Login<A>,
}

impl<A> fmt::Debug for SmtpSender<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmtpSender")
            .field("settings", &self.settings)
            .field("login", &self.login)
            .finish()
    }
}

impl<A: Authenticator> SmtpSender<A> {
    /// A sender for the server `settings` describe, logging in as `login`.
    pub fn new(settings: SmtpSettings, login: Login<A>) -> Self {
        Self { settings, login }
    }

    /// Sends `draft`, threaded as `threading` says. Returns the message's
    /// `Message-ID:` and the message as the sender keeps it (see
    /// [`Composed::kept`](crate::Composed)).
    ///
    /// This isn't a [`mett_core::MailSender`]: that port promises a copy in
    /// the sent-items folder if the draft asks for one, which takes a
    /// mailbox. The mailbox (IMAP) implements the port, and stores the
    /// copy this returns.
    pub async fn send(&self, draft: &Draft, threading: &Threading) -> Result<(String, Vec<u8>)> {
        if !draft.has_recipients() {
            return Err(Error::NotSent("the message has no recipients".into()));
        }
        let composed = compose(draft, &self.settings.from, threading)?;
        let envelope = Envelope::new(
            Some(address(&composed.sender)?),
            composed
                .recipients
                .iter()
                .map(|r| address(r))
                .collect::<Result<_>>()?,
        )
        .map_err(|e| Error::NotSent(e.to_string()))?;
        match &self.login {
            Login::OAuth { user, tokens } => {
                let token = tokens.access_token().await.map_err(not_sent)?;
                let sent = self
                    .deliver(&envelope, &composed.sent, Some(oauth(user, &token)), true)
                    .await;
                let Err(Error::Auth(_)) = &sent else {
                    return sent.map(|()| (composed.message_id, composed.kept));
                };
                // The server turned down a token that hadn't expired (it
                // may have been revoked): get a new one, and try again.
                tokens.token_rejected(&token).await;
                let fresh = tokens.access_token().await.map_err(not_sent)?;
                if fresh == token {
                    return sent.map(|()| (composed.message_id, composed.kept));
                }
                self.deliver(&envelope, &composed.sent, Some(oauth(user, &fresh)), true)
                    .await?;
            }
            Login::Password { user, password } => {
                let credentials = Credentials::new(user.clone(), password.clone());
                self.deliver(&envelope, &composed.sent, Some(credentials), false)
                    .await?;
            }
            Login::None => {
                self.deliver(&envelope, &composed.sent, None, false).await?;
            }
        }
        Ok((composed.message_id, composed.kept))
    }

    async fn deliver(
        &self,
        envelope: &Envelope,
        message: &[u8],
        credentials: Option<Credentials>,
        xoauth2: bool,
    ) -> Result<()> {
        let settings = &self.settings;
        let tls = || {
            let mut tls = TlsParameters::builder(settings.host.clone());
            for pem in &settings.ca_certificates {
                tls = tls.add_root_certificate(
                    Certificate::from_pem(pem)
                        .map_err(|e| Error::NotSent(format!("CA certificate: {e}")))?,
                );
            }
            if !settings.verify {
                tls = tls
                    .dangerous_accept_invalid_certs(true)
                    .dangerous_accept_invalid_hostnames(true);
            }
            tls.build_rustls()
                .map_err(|e| Error::NotSent(format!("TLS: {e}")))
        };
        let tls = match settings.security {
            Security::None => Tls::None,
            Security::StartTlsIfOffered => Tls::Opportunistic(tls()?),
            Security::StartTls => Tls::Required(tls()?),
            Security::Tls => Tls::Wrapper(tls()?),
        };
        let mut transport =
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(settings.host.clone())
                .port(settings.port)
                .tls(tls)
                .timeout(Some(settings.timeout));
        if let Some(credentials) = credentials {
            transport = transport.credentials(credentials);
            if xoauth2 {
                transport = transport.authentication(vec![Mechanism::Xoauth2]);
            }
        }
        // Lettre's timeout is for each step of the exchange, not for all of
        // it: a server that answers slowly can't keep the send pending.
        let transport = transport.build();
        let sent = transport.send_raw(envelope, message);
        match tokio::time::timeout(settings.timeout * 3, sent).await {
            Ok(sent) => sent.map(drop).map_err(|e| failure(&e, &settings.host)),
            Err(_) => Err(Error::Network(format!(
                "{}: the server took too long",
                settings.host
            ))),
        }
    }
}

fn oauth(user: &str, token: &AccessToken) -> Credentials {
    Credentials::new(user.to_owned(), token.secret().to_owned())
}

fn address(email: &str) -> Result<MailAddress> {
    email
        .parse()
        .map_err(|e| Error::NotSent(format!("{email}: {e}")))
}

/// An error getting a token, as one before anything was handed over.
fn not_sent(e: Error) -> Error {
    match e {
        Error::Auth(_) => e,
        e => Error::NotSent(e.to_string()),
    }
}

/// What a failed delivery means for mett.
///
/// A server's answer (4xx or 5xx) tells that the message wasn't taken:
/// it is a [`Error::Backend`] or, for refused logins, an [`Error::Auth`].
/// A failure to set up TLS (including a certificate not trusted), or of the
/// client's own, comes before anything is sent. Anything else
/// (a lost connection, a timeout) may have come after the server took the
/// message, so it is an [`Error::Network`], which the user is told may have
/// gone out.
fn failure(e: &smtp::Error, host: &str) -> Error {
    if let Some(code) = e.status() {
        let code = code.to_string();
        return match code.as_str() {
            // 530 asks for authentication, 534 and 535 refuse it.
            "530" | "534" | "535" => Error::Auth(format!("{host}: {e}")),
            _ => Error::Backend(format!("{host}: {e}")),
        };
    }
    // A server may turn an XOAUTH2 token down with a `334` challenge
    // (Google's and Microsoft's do), which lettre can't answer: it says so
    // as a failure of its own, which is the server's refusal all the same.
    if e.is_client() && e.to_string().contains("does not expect a challenge") {
        return Error::Auth(format!("{host}: the server refused the token"));
    }
    // A failure of the client's own (the server has no STARTTLS or no login
    // we can use) comes before anything is sent.
    if e.is_tls() || e.is_client() || handshake_failed(e) {
        return Error::NotSent(format!("{host}: {e}"));
    }
    Error::Network(format!("{host}: {e}"))
}

/// Whether `e` is a TLS handshake that failed (say, on a certificate we
/// don't trust), which lettre reports as a connection error like a
/// refused connection: the I/O error under it is the handshake's.
fn handshake_failed(e: &smtp::Error) -> bool {
    let mut source = std::error::Error::source(e);
    while let Some(error) = source {
        if let Some(io) = error.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::InvalidData
        {
            return true;
        }
        source = error.source();
    }
    false
}
