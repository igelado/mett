//! Sending over SMTP.

use std::fmt;
use std::sync::OnceLock;

use lettre::address::Envelope;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{Certificate, CertificateStore, Tls, TlsParameters};
use lettre::transport::smtp::{self, AsyncSmtpTransport};
use lettre::{Address as MailAddress, AsyncTransport, Tokio1Executor};
use mett_core::{
    AccessToken, Address, Authenticator, Draft, Error, Login, NoTokens, Result, Security,
    ServerSettings,
};

use crate::compose::{Threading, compose};

/// How to reach an SMTP server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmtpSettings {
    /// The server, and how to trust it. A whole send is given three times
    /// the server's `timeout`.
    pub server: ServerSettings,
    /// The sender's address, for messages that don't name another.
    pub from: Address,
}

impl SmtpSettings {
    /// Settings for `host`, with the port that goes with `security` and
    /// the checks on: 25 (and 587 for STARTTLS, which is what servers
    /// ask clients to use) or 465.
    pub fn new(host: impl Into<String>, security: Security, from: Address) -> Self {
        let port = match security {
            Security::None => 25,
            Security::StartTls | Security::StartTlsIfOffered => 587,
            Security::Tls => 465,
        };
        Self {
            server: ServerSettings::new(host, port, security),
            from,
        }
    }
}

/// Sends mail through an SMTP server.
pub struct SmtpSender<A = NoTokens> {
    settings: SmtpSettings,
    login: Option<Login<A>>,
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
    /// A sender for the server `settings` describe, logging in as `login`
    /// (or, with `None`, without logging in).
    pub fn new(settings: SmtpSettings, login: Option<Login<A>>) -> Self {
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
            Some(Login::OAuth { user, tokens }) => {
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
            Some(Login::Password { user, password }) => {
                let credentials = Credentials::new(user.clone(), password.clone());
                self.deliver(&envelope, &composed.sent, Some(credentials), false)
                    .await?;
            }
            None => {
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
        let settings = &self.settings.server;
        let tls = || {
            let mut tls = TlsParameters::builder(settings.host.clone());
            for pem in &settings.ca_certificates {
                tls = tls.add_root_certificate(
                    Certificate::from_pem(pem)
                        .map_err(|e| Error::NotSent(format!("CA certificate: {e}")))?,
                );
            }
            if !settings.verify_host {
                // Lettre skips the name check only with roots it is given
                // (its platform verifier is all or nothing): the system's,
                // which are loaded here, and the extra ones above.
                tls = tls.certificate_store(CertificateStore::None);
                for root in native_roots() {
                    tls = tls.add_root_certificate(root.clone());
                }
                tls = tls.dangerous_accept_invalid_hostnames(true);
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

/// The system's certificate authorities, as lettre takes them. They are
/// loaded once per process: loading reads the system's store, which is too
/// much to do for each message.
///
/// Only `verify_host` off needs them. TLS here and in `mett-imap` is
/// verified by two stacks with the same intent but one difference: with
/// `verify_host` on, both use the platform verifier (system roots plus the
/// extra ones), but with it off lettre can only skip the name check by
/// verifying the chain itself against a root store, so it doesn't apply the
/// platform's policy; IMAP's verifier forgives only a name mismatch.
fn native_roots() -> &'static [Certificate] {
    static ROOTS: OnceLock<Vec<Certificate>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        rustls_native_certs::load_native_certs()
            .certs
            .into_iter()
            .filter_map(|root| Certificate::from_der(root.to_vec()).ok())
            .collect()
    })
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

#[cfg(test)]
mod tests {
    use super::native_roots;

    #[test]
    fn the_system_roots_are_loaded_once() {
        assert!(std::ptr::eq(native_roots(), native_roots()));
    }
}
