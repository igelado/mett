//! The connection to the server: plain or TLS, and logging in.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_imap::{Client, Session};
use mett_core::{AccessToken, Error, Result, Security};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::settings::ImapSettings;
use crate::tls::client_config;

/// A connection, encrypted or not.
#[derive(Debug)]
pub(crate) enum Conn {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for Conn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// How to log in.
pub(crate) enum Credentials<'a> {
    Password(&'a str, &'a str),
    OAuth(&'a str, &'a AccessToken),
}

/// What a server said to a login, or why we couldn't ask.
pub(crate) enum LoginFailure {
    /// The server refused the credentials.
    Refused(Error),
    /// Something else went wrong.
    Other(Error),
}

/// SASL XOAUTH2: the first response says who and with what token; to a
/// challenge (the server's error), the answer is an empty line.
struct Xoauth2(String);

impl async_imap::Authenticator for Xoauth2 {
    type Response = String;

    fn process(&mut self, challenge: &[u8]) -> String {
        if challenge.is_empty() {
            self.0.clone()
        } else {
            String::new()
        }
    }
}

/// Connects to the server and logs in.
pub(crate) async fn connect(
    settings: &ImapSettings,
    credentials: Credentials<'_>,
) -> std::result::Result<Session<Conn>, LoginFailure> {
    let settings = &settings.server;
    let other = LoginFailure::Other;
    let timeout = settings.timeout;
    let tcp = within(
        timeout,
        TcpStream::connect((settings.host.as_str(), settings.port)),
    )
    .await
    .map_err(other)?
    .map_err(|e| other(Error::Network(format!("{}: {e}", settings.host))))?;
    let name = || {
        ServerName::try_from(settings.host.clone())
            .map_err(|e| other(Error::Backend(format!("{}: {e}", settings.host))))
    };
    let host = settings.host.as_str();
    let tls = |stream| async move {
        let config =
            client_config(settings.verify_host, &settings.ca_certificates).map_err(other)?;
        within(timeout, TlsConnector::from(config).connect(name()?, stream))
            .await
            .map_err(other)?
            .map_err(|e| other(Error::Network(format!("{host}: TLS: {e}"))))
    };
    let greet = |mut client: Client<Conn>| async move {
        within(timeout, client.read_response())
            .await
            .map_err(other)?
            .map_err(|e| other(lost(host, &e.to_string())))?;
        Ok::<_, LoginFailure>(client)
    };
    let client = match settings.security {
        Security::Tls => greet(Client::new(Conn::Tls(Box::new(tls(tcp).await?)))).await?,
        Security::None => greet(Client::new(Conn::Plain(tcp))).await?,
        Security::StartTls | Security::StartTlsIfOffered => {
            let mut client = greet(Client::new(Conn::Plain(tcp))).await?;
            // The password may only go over a connection that is as protected
            // as the settings say: a server that offers STARTTLS and then
            // refuses it is not one that can't, and isn't gone on with.
            let offered = within(timeout, advertises_starttls(&mut client))
                .await
                .map_err(other)?
                .map_err(|e| other(lost(host, &e.to_string())))?;
            if !offered {
                if settings.security == Security::StartTls {
                    return Err(other(Error::Backend(format!(
                        "{host}: the server doesn't offer STARTTLS; to go on in plain text, turn ssl_force_tls off"
                    ))));
                }
                client
            } else {
                within(timeout, client.run_command_and_check_ok("STARTTLS", None))
                    .await
                    .map_err(other)?
                    .map_err(|e| {
                        other(Error::Backend(format!("{host}: STARTTLS was refused: {e}")))
                    })?;
                let Conn::Plain(tcp) = client.into_inner() else {
                    unreachable!("the connection was plain")
                };
                Client::new(Conn::Tls(Box::new(tls(tcp).await?)))
            }
        }
    };
    let session = match credentials {
        Credentials::Password(user, password) => {
            within(timeout, client.login(user, password)).await
        }
        Credentials::OAuth(user, token) => {
            let first = format!("user={user}\x01auth=Bearer {}\x01\x01", token.secret());
            within(timeout, client.authenticate("XOAUTH2", Xoauth2(first))).await
        }
    }
    .map_err(other)?;
    session.map_err(|(e, _)| match e {
        async_imap::error::Error::No(text) => {
            LoginFailure::Refused(Error::Auth(format!("{host}: {text}")))
        }
        e => other(lost(host, &e.to_string())),
    })
}

/// Whether the server offers STARTTLS, by what it says it can do.
async fn advertises_starttls(client: &mut Client<Conn>) -> async_imap::error::Result<bool> {
    use async_imap::imap_proto::{Capability, Response};
    use async_imap::types::UnsolicitedResponse;
    // What the server says besides the answer comes to the channel.
    let (said, hears) = async_channel::unbounded();
    client
        .run_command_and_check_ok("CAPABILITY", Some(said))
        .await?;
    let mut offered = false;
    while let Ok(response) = hears.try_recv() {
        if let UnsolicitedResponse::Other(response) = response
            && let Response::Capabilities(capabilities) = response.parsed()
        {
            offered |= capabilities
                .iter()
                .any(|c| matches!(c, Capability::Atom(a) if a.eq_ignore_ascii_case("STARTTLS")));
        }
    }
    Ok(offered)
}

/// A lost connection.
pub(crate) fn lost(host: &str, why: &str) -> Error {
    Error::Network(format!("{host}: {why}"))
}

/// `future`, or a [`Error::Network`] if it takes longer than `timeout`.
pub(crate) async fn within<F: std::future::Future>(
    timeout: Duration,
    future: F,
) -> Result<F::Output> {
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| Error::Network("the server took too long".into()))
}
