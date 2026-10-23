//! `SmtpSender` against a server in this process: plain, with STARTTLS and
//! with TLS from the start.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use mett_core::{AccessToken, Address, Authenticator, Draft, Error, OutgoingAttachment};
use mett_smtp::{Login, NoTokens, Security, SmtpSender, SmtpSettings, Threading};
use rcgen::{CertificateParams, KeyPair, SanType};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

/// What the server saw of one connection.
#[derive(Clone, Debug, Default)]
struct Seen {
    /// Whether the conversation was encrypted when the message came.
    encrypted: bool,
    /// The `AUTH` command's arguments.
    auth: Vec<String>,
    from: String,
    to: Vec<String>,
    data: String,
}

struct Server {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
    /// The certificate the server uses, as PEM.
    ca: Vec<u8>,
}

/// How the server answers the login.
#[derive(Clone, Copy, PartialEq)]
enum Accept {
    Everything,
    /// Only logins with this token (XOAUTH2) or password (PLAIN).
    Only(&'static str),
    /// Like `Only`, but a refusal is a `334` challenge, as Google and
    /// Microsoft answer XOAUTH2.
    Challenge(&'static str),
}

fn acceptor(name: &str) -> (TlsAcceptor, Vec<u8>) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::new()).unwrap();
    params.subject_alt_names = vec![SanType::IpAddress(name.parse().unwrap())];
    let cert = params.self_signed(&key).unwrap();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .unwrap();
    (TlsAcceptor::from(Arc::new(config)), cert.pem().into_bytes())
}

async fn server(wrapper: bool, accept: Accept) -> Server {
    server_for(wrapper, accept, "127.0.0.1").await
}

/// A server whose certificate is for the address `name`.
async fn server_for(wrapper: bool, accept: Accept, name: &str) -> Server {
    let (acceptor, ca) = acceptor(name);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, record) = (acceptor.clone(), record.clone());
            tokio::spawn(async move {
                if wrapper {
                    let Ok(stream) = acceptor.clone().accept(stream).await else {
                        return;
                    };
                    converse(Box::new(stream), acceptor, record, true, true, accept).await;
                } else {
                    converse(Box::new(stream), acceptor, record, false, true, accept).await;
                }
            });
        }
    });
    Server { port, seen, ca }
}

/// A connection, whether or not it is encrypted.
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

fn converse(
    stream: Box<dyn Io>,
    acceptor: TlsAcceptor,
    record: Arc<Mutex<Vec<Seen>>>,
    encrypted: bool,
    greet: bool,
    accept: Accept,
) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(async move {
        let mut stream = BufReader::new(stream);
        let mut seen = Seen {
            encrypted,
            ..Seen::default()
        };
        if greet {
            let _ = stream.write_all(b"220 localhost ready\r\n").await;
        }
        let mut line = String::new();
        loop {
            line.clear();
            if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            let command = line.trim_end().to_owned();
            let upper = command.to_uppercase();
            let reply = if upper.starts_with("EHLO") {
                let starttls = if encrypted { "" } else { "250-STARTTLS\r\n" };
                format!(
                    "250-localhost\r\n{starttls}250-AUTH PLAIN LOGIN XOAUTH2\r\n250 8BITMIME\r\n"
                )
            } else if upper == "STARTTLS" {
                let _ = stream.write_all(b"220 go ahead\r\n").await;
                let Ok(tls) = acceptor.clone().accept(stream.into_inner()).await else {
                    return;
                };
                return converse(Box::new(tls), acceptor, record, true, false, accept).await;
            } else if upper.starts_with("AUTH ") {
                seen.auth = command.split(' ').skip(1).map(str::to_owned).collect();
                let credentials = seen.auth.last().cloned().unwrap_or_default();
                let decoded = decode(&credentials);
                match accept {
                    Accept::Only(wanted) if !decoded.contains(wanted) => {
                        "535 5.7.8 authentication failed\r\n".to_owned()
                    }
                    Accept::Challenge(wanted) if !decoded.contains(wanted) => {
                        let _ = stream.write_all(b"334 eyJzdGF0dXMiOiI0MDEifQ==\r\n").await;
                        line.clear();
                        let _ = stream.read_line(&mut line).await;
                        "535 5.7.8 authentication failed\r\n".to_owned()
                    }
                    _ => "235 2.7.0 welcome\r\n".to_owned(),
                }
            } else if upper.starts_with("MAIL FROM:") {
                seen.from = between(&command);
                "250 ok\r\n".to_owned()
            } else if upper.starts_with("RCPT TO:") {
                seen.to.push(between(&command));
                "250 ok\r\n".to_owned()
            } else if upper == "DATA" {
                let _ = stream.write_all(b"354 go on\r\n").await;
                loop {
                    line.clear();
                    if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    if line == ".\r\n" {
                        break;
                    }
                    seen.data.push_str(&line);
                }
                record.lock().unwrap().push(seen.clone());
                "250 queued\r\n".to_owned()
            } else if upper == "QUIT" {
                let _ = stream.write_all(b"221 bye\r\n").await;
                return;
            } else {
                "250 ok\r\n".to_owned()
            };
            let _ = stream.write_all(reply.as_bytes()).await;
        }
    })
}

fn between(command: &str) -> String {
    let (_, rest) = command.split_once('<').unwrap_or(("", ""));
    rest.split('>').next().unwrap_or_default().to_owned()
}

/// Base64 decoding, for what the tests check of the credentials.
fn decode(text: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let (mut bits, mut have, mut out) = (0u32, 0, Vec::new());
    for c in text.bytes().filter(|c| *c != b'=') {
        let Some(value) = ALPHABET.iter().position(|a| *a == c) else {
            return String::new();
        };
        bits = bits << 6 | value as u32;
        have += 6;
        if have >= 8 {
            have -= 8;
            out.push((bits >> have) as u8);
            bits &= (1 << have) - 1;
        }
    }
    // PLAIN and XOAUTH2 separate their fields with NUL and ^A.
    String::from_utf8_lossy(&out).replace(['\0', '\u{1}'], " ")
}

fn settings(server: &Server, security: Security) -> SmtpSettings {
    let mut settings = SmtpSettings::new("127.0.0.1", security, Address::new("me@example.com"));
    settings.port = server.port;
    settings.ca_certificates = vec![server.ca.clone()];
    settings
}

fn draft() -> Draft {
    Draft {
        to: vec![Address::new("alice@example.org").with_name("Alice")],
        cc: vec![Address::new("carol@example.org")],
        bcc: vec![Address::new("secret@example.org")],
        subject: "Hello".into(),
        body: "Hi there\n".into(),
        ..Draft::default()
    }
}

#[tokio::test]
async fn sends_in_plain_text() {
    let server = server(false, Accept::Everything).await;
    let sender = SmtpSender::<NoTokens>::new(settings(&server, Security::None), Login::None);
    sender.send(&draft(), &Threading::default()).await.unwrap();

    let seen = server.seen.lock().unwrap();
    let seen = &seen[0];
    assert!(!seen.encrypted);
    assert_eq!(seen.from, "me@example.com");
    assert_eq!(
        seen.to,
        [
            "alice@example.org",
            "carol@example.org",
            "secret@example.org"
        ]
    );
    assert!(seen.data.contains("Subject: Hello"));
    assert!(seen.data.contains("From: me@example.com"));
    assert!(seen.data.contains("To: Alice <alice@example.org>"));
    assert!(seen.data.contains("Hi there"));
    // Blind copies are told apart by the envelope, not the message.
    assert!(!seen.data.contains("secret@example.org"));
}

#[tokio::test]
async fn upgrades_with_starttls() {
    let server = server(false, Accept::Everything).await;
    let login = Login::Password {
        user: "me".into(),
        password: "hunter2".into(),
    };
    let sender = SmtpSender::<NoTokens>::new(settings(&server, Security::StartTls), login);
    sender.send(&draft(), &Threading::default()).await.unwrap();

    let seen = server.seen.lock().unwrap();
    assert!(seen[0].encrypted);
    assert_eq!(seen[0].auth[0], "PLAIN");
    assert!(decode(&seen[0].auth[1]).contains("me hunter2"));
}

#[tokio::test]
async fn is_encrypted_from_the_start_with_tls() {
    let server = server(true, Accept::Everything).await;
    let sender = SmtpSender::<NoTokens>::new(settings(&server, Security::Tls), Login::None);
    sender.send(&draft(), &Threading::default()).await.unwrap();
    assert!(server.seen.lock().unwrap()[0].encrypted);
}

#[tokio::test]
async fn refuses_a_certificate_it_does_not_trust() {
    let server = server(true, Accept::Everything).await;
    let mut settings = settings(&server, Security::Tls);
    settings.ca_certificates.clear();
    let sender = SmtpSender::<NoTokens>::new(settings, Login::None);
    let error = sender
        .send(&draft(), &Threading::default())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::NotSent(_)), "{error}");
    assert!(server.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn checks_the_name_of_the_certificate_and_the_certificate_itself() {
    // Its certificate is for 127.0.0.2, the connection is to 127.0.0.1.
    let server = server_for(true, Accept::Everything, "127.0.0.2").await;
    let mut settings = settings(&server, Security::Tls);
    let sender = SmtpSender::<NoTokens>::new(settings.clone(), Login::None);
    let error = sender
        .send(&draft(), &Threading::default())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::NotSent(_)), "{error}");

    // Not checking the name doesn't mean taking any certificate: this one
    // is from no authority the sender knows.
    settings.verify_host = false;
    settings.ca_certificates.clear();
    let sender = SmtpSender::<NoTokens>::new(settings.clone(), Login::None);
    assert!(sender.send(&draft(), &Threading::default()).await.is_err());

    // Nor can it be combined with the extra authorities, which lettre
    // would check the name with anyway: that is said, not done wrong.
    settings.ca_certificates = vec![server.ca.clone()];
    let sender = SmtpSender::<NoTokens>::new(settings, Login::None);
    let error = sender
        .send(&draft(), &Threading::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("can't be combined"), "{error}");
    assert!(server.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_wrong_password_is_an_authentication_error() {
    let server = server(false, Accept::Only("right")).await;
    let login = Login::Password {
        user: "me".into(),
        password: "wrong".into(),
    };
    let sender = SmtpSender::<NoTokens>::new(settings(&server, Security::StartTls), login);
    let error = sender
        .send(&draft(), &Threading::default())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Auth(_)), "{error}");
}

/// Tokens that change: what the server turns down, then what it takes.
struct Tokens(Mutex<Vec<&'static str>>, Mutex<Vec<String>>);

impl Authenticator for Tokens {
    async fn access_token(&self) -> mett_core::Result<AccessToken> {
        let mut tokens = self.0.lock().unwrap();
        let token = if tokens.len() > 1 {
            tokens.remove(0)
        } else {
            tokens[0]
        };
        Ok(AccessToken::new(token, chrono_far()))
    }

    async fn token_rejected(&self, token: &AccessToken) {
        self.1.lock().unwrap().push(token.secret().to_owned());
    }
}

fn chrono_far() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::<chrono::Utc>::MAX_UTC
}

#[tokio::test]
async fn logs_in_with_an_oauth_token_and_asks_for_another_when_refused() {
    let server = server(false, Accept::Only("good-token")).await;
    let tokens = Tokens(
        Mutex::new(vec!["stale-token", "good-token"]),
        Mutex::default(),
    );
    let login = Login::OAuth {
        user: "me@example.com".into(),
        tokens,
    };
    let sender = SmtpSender::new(settings(&server, Security::StartTls), login);
    sender.send(&draft(), &Threading::default()).await.unwrap();

    let seen = server.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].auth[0], "XOAUTH2");
    assert!(decode(&seen[0].auth[1]).contains("user=me@example.com auth=Bearer good-token"));
}

#[tokio::test]
async fn threads_a_reply_and_attaches_files() {
    let server = server(false, Accept::Everything).await;
    let sender = SmtpSender::<NoTokens>::new(settings(&server, Security::None), Login::None);
    let mut draft = draft();
    draft.attachments = vec![OutgoingAttachment {
        name: "notes.txt".into(),
        content_type: "text/plain".into(),
        data: b"the notes".to_vec(),
    }];
    let threading = Threading {
        in_reply_to: Some("<one@example.org>".into()),
        references: vec!["<zero@example.org>".into(), "<one@example.org>".into()],
    };
    let (id, kept) = sender.send(&draft, &threading).await.unwrap();

    let seen = server.seen.lock().unwrap();
    let data = &seen[0].data;
    assert!(data.contains("In-Reply-To: <one@example.org>"));
    assert!(data.contains("References: <zero@example.org> <one@example.org>"));
    assert!(data.contains("multipart/mixed"));
    assert!(data.contains("notes.txt"));
    assert!(data.contains(&format!("Message-ID: {id}")));
    // The copy kept is the message sent, with the blind copy in it.
    let kept = String::from_utf8(kept).unwrap();
    assert!(kept.contains(&format!("Message-ID: {id}")));
    assert!(kept.contains("Bcc: secret@example.org"));
}

#[tokio::test]
async fn a_message_without_recipients_is_not_sent() {
    let server = server(false, Accept::Everything).await;
    let sender = SmtpSender::<NoTokens>::new(settings(&server, Security::None), Login::None);
    let error = sender
        .send(&Draft::default(), &Threading::default())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::NotSent(_)), "{error}");
}

#[tokio::test]
async fn an_unreachable_server_is_a_network_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut settings =
        SmtpSettings::new("127.0.0.1", Security::None, Address::new("me@example.com"));
    settings.port = port;
    let sender = SmtpSender::<NoTokens>::new(settings, Login::None);
    let error = sender
        .send(&draft(), &Threading::default())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Network(_)), "{error}");
}

#[tokio::test]
async fn a_server_that_never_answers_is_given_up_on() {
    // It takes the connection and says nothing.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let _silent = tokio::spawn(async move {
        let held = listener.accept().await;
        std::future::pending::<()>().await;
        drop(held);
    });
    let mut settings =
        SmtpSettings::new("127.0.0.1", Security::None, Address::new("me@example.com"));
    settings.port = port;
    settings.timeout = std::time::Duration::from_millis(100);
    let sender = SmtpSender::<NoTokens>::new(settings, Login::None);
    let error = sender
        .send(&draft(), &Threading::default())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Network(_)), "{error}");
}

#[tokio::test]
async fn asks_for_another_token_when_refused_with_a_challenge() {
    let server = server(false, Accept::Challenge("good-token")).await;
    let tokens = Tokens(
        Mutex::new(vec!["stale-token", "good-token"]),
        Mutex::default(),
    );
    let login = Login::OAuth {
        user: "me@example.com".into(),
        tokens,
    };
    let sender = SmtpSender::new(settings(&server, Security::StartTls), login);
    sender.send(&draft(), &Threading::default()).await.unwrap();
    assert_eq!(server.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn upgrades_when_the_server_offers_starttls_if_asked_to_only_then() {
    let server = server(false, Accept::Everything).await;
    let sender =
        SmtpSender::<NoTokens>::new(settings(&server, Security::StartTlsIfOffered), Login::None);
    sender.send(&draft(), &Threading::default()).await.unwrap();
    assert!(server.seen.lock().unwrap()[0].encrypted);
}
