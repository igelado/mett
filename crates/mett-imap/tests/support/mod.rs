//! An IMAP server in this process, with mailboxes in memory.
//!
//! It speaks as much IMAP as `mett-imap` uses: plain, with STARTTLS and
//! with TLS from the start; `LOGIN` and `AUTHENTICATE XOAUTH2`; `LIST`,
//! `STATUS`, `SELECT`, `UID SEARCH`, `UID FETCH`, `UID STORE`,
//! `UID COPY`, `UID MOVE`, `UID EXPUNGE`, `EXPUNGE` and `APPEND`.

#![allow(dead_code, missing_docs)]

pub mod smtp;

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use rcgen::{CertificateParams, KeyPair, SanType};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

#[derive(Clone, Debug)]
pub struct Msg {
    pub uid: u32,
    pub modseq: u64,
    pub flags: BTreeSet<String>,
    pub internal: String,
    pub raw: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Folder {
    pub name: String,
    pub attributes: Vec<String>,
    pub validity: u32,
    pub next_uid: u32,
    pub modseq: u64,
    pub messages: Vec<Msg>,
}

impl Folder {
    pub fn new(name: &str, attributes: &[&str]) -> Self {
        Self {
            name: name.into(),
            attributes: attributes.iter().map(|a| (*a).to_owned()).collect(),
            validity: 1000,
            next_uid: 1,
            modseq: 1,
            messages: Vec::new(),
        }
    }
}

#[derive(Default)]
pub struct State {
    pub folders: Vec<Folder>,
    /// How many logins the server turned down.
    pub refused: u32,
    /// What the clients sent as login, in order.
    pub logins: Vec<String>,
    /// Whether each connection was encrypted when it logged in.
    pub encrypted: Vec<bool>,
    /// Leaves `UIDPLUS` out of what the server says it can do.
    pub no_uidplus: bool,
    /// Leaves `MOVE` out of it.
    pub no_move: bool,
    /// Leaves `SORT` out of it.
    pub no_sort: bool,
    /// Leaves `CONDSTORE` out of it.
    pub no_condstore: bool,
    /// Offers STARTTLS and then refuses it.
    pub refuse_starttls: bool,
    /// How many times every folder was listed.
    pub full_lists: u32,
    /// Says which folders are Sent, Drafts and so on only when asked to
    /// (`RETURN (SPECIAL-USE)`), as the servers allowed to do.
    pub special_use_only_on_request: bool,
}

impl State {
    /// Another client changes the flags of a message.
    pub fn set_flags(&mut self, name: &str, uid: u32, flags: &[&str]) {
        let folder = self.folder(name);
        folder.modseq += 1;
        let modseq = folder.modseq;
        let m = folder.messages.iter_mut().find(|m| m.uid == uid).unwrap();
        m.flags = flags.iter().map(|f| (*f).to_owned()).collect();
        m.modseq = modseq;
    }

    /// Mail arrives with the date it was first delivered with.
    pub fn deliver_dated(&mut self, name: &str, raw: &str, internal: &str) -> u32 {
        let uid = self.deliver(name, raw, &[]);
        let folder = self.folder(name);
        folder.messages.last_mut().unwrap().internal = internal.into();
        uid
    }
}

impl State {
    pub fn folder(&mut self, name: &str) -> &mut Folder {
        self.folders
            .iter_mut()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("no folder {name}"))
    }

    /// Puts a message in a folder, the way mail arrives, and returns its UID.
    pub fn deliver(&mut self, name: &str, raw: &str, flags: &[&str]) -> u32 {
        let folder = self.folder(name);
        let uid = folder.next_uid;
        folder.next_uid += 1;
        folder.modseq += 1;
        folder.messages.push(Msg {
            uid,
            modseq: folder.modseq,
            flags: flags.iter().map(|f| (*f).to_owned()).collect(),
            internal: "20-Oct-2026 10:00:00 +0000".into(),
            raw: raw.replace('\n', "\r\n").into_bytes(),
        });
        uid
    }
}

/// What the server takes as login.
#[derive(Clone)]
pub enum Accept {
    Password(&'static str, &'static str),
    /// A token, which it turns down with a `334` challenge when it is not
    /// this one.
    Token(&'static str),
}

pub struct Server {
    pub port: u16,
    pub state: Arc<Mutex<State>>,
    /// The certificate the server uses, as PEM.
    pub ca: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Plain,
    /// Plain, with STARTTLS on offer.
    StartTls,
    /// TLS from the first byte.
    Tls,
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

pub async fn server(mode: Mode, accept: Accept, folders: Vec<Folder>) -> Server {
    server_for(mode, accept, folders, "127.0.0.1").await
}

/// A server whose certificate is for the address `name`.
pub async fn server_for(mode: Mode, accept: Accept, folders: Vec<Folder>, name: &str) -> Server {
    let (acceptor, ca) = acceptor(name);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(Mutex::new(State {
        folders,
        ..State::default()
    }));
    let shared = state.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, state, accept) = (acceptor.clone(), shared.clone(), accept.clone());
            tokio::spawn(async move {
                let session = Session {
                    state,
                    accept,
                    starttls: mode == Mode::StartTls,
                    encrypted: mode == Mode::Tls,
                    selected: None,
                    acceptor: acceptor.clone(),
                };
                if mode == Mode::Tls {
                    let Ok(stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    session.run(Box::new(stream), true).await;
                } else {
                    session.run(Box::new(stream), true).await;
                }
            });
        }
    });
    Server { port, state, ca }
}

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

struct Session {
    state: Arc<Mutex<State>>,
    accept: Accept,
    starttls: bool,
    encrypted: bool,
    selected: Option<String>,
    acceptor: TlsAcceptor,
}

/// A line of the client's, with its literal if it ends in one.
async fn command(stream: &mut BufReader<Box<dyn Io>>) -> Option<(String, Option<Vec<u8>>)> {
    let mut line = String::new();
    if stream.read_line(&mut line).await.ok()? == 0 {
        return None;
    }
    let line = line.trim_end_matches(['\r', '\n']).to_owned();
    if let Some(open) = line.rfind('{')
        && line.ends_with('}')
        && let Ok(size) = line[open + 1..line.len() - 1].parse::<usize>()
    {
        stream.write_all(b"+ go ahead\r\n").await.ok()?;
        let mut data = vec![0; size];
        stream.read_exact(&mut data).await.ok()?;
        let mut end = String::new();
        stream.read_line(&mut end).await.ok()?;
        return Some((line[..open].trim_end().to_owned(), Some(data)));
    }
    Some((line, None))
}

fn unquote(text: &str) -> String {
    text.trim().trim_matches('"').replace("\\\"", "\"")
}

/// UIDs from a set like `1,3:5,7:*`.
fn uids(set: &str, all: &[u32]) -> Vec<u32> {
    let max = all.iter().copied().max().unwrap_or(0);
    let mut out = Vec::new();
    for part in set.split(',') {
        let (from, to) = part.split_once(':').unwrap_or((part, part));
        let number = |t: &str| {
            if t == "*" {
                max
            } else {
                t.parse().unwrap_or(0)
            }
        };
        let (a, b) = (number(from), number(to));
        out.extend(
            all.iter()
                .copied()
                .filter(|u| *u >= a.min(b) && *u <= a.max(b)),
        );
    }
    out
}

fn header_of(raw: &[u8]) -> Vec<u8> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(raw.len(), |at| at + 4);
    raw[..end].to_vec()
}

impl Session {
    fn run(mut self, stream: Box<dyn Io>, greet: bool) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            let mut stream = BufReader::new(stream);
            if greet {
                let _ = stream.write_all(b"* OK IMAP4rev1 ready\r\n").await;
            }
            while let Some((line, literal)) = command(&mut stream).await {
                let mut words = line.splitn(2, ' ');
                let tag = words.next().unwrap_or("*").to_owned();
                let rest = words.next().unwrap_or("").to_owned();
                let upper = rest.to_uppercase();
                let mut out = String::new();
                let ok =
                    |out: &mut String, text: &str| out.push_str(&format!("{tag} OK {text}\r\n"));
                let no =
                    |out: &mut String, text: &str| out.push_str(&format!("{tag} NO {text}\r\n"));
                if upper == "CAPABILITY" {
                    let starttls = if self.starttls && !self.encrypted {
                        " STARTTLS"
                    } else {
                        ""
                    };
                    let (uidplus, moves, sort, condstore) = {
                        let state = self.state.lock().unwrap();
                        (
                            if state.no_uidplus { "" } else { " UIDPLUS" },
                            if state.no_move { "" } else { " MOVE" },
                            if state.no_sort { "" } else { " SORT" },
                            if state.no_condstore { "" } else { " CONDSTORE" },
                        )
                    };
                    out.push_str(&format!(
                        "* CAPABILITY IMAP4rev1{starttls} AUTH=XOAUTH2 SPECIAL-USE LIST-EXTENDED{uidplus}{moves}{sort}{condstore}\r\n"
                    ));
                    ok(&mut out, "done");
                } else if upper == "STARTTLS" {
                    let refused = self.state.lock().unwrap().refuse_starttls;
                    if !self.starttls || self.encrypted || refused {
                        no(&mut out, "no STARTTLS here");
                    } else {
                        let _ = stream
                            .write_all(format!("{tag} OK go ahead\r\n").as_bytes())
                            .await;
                        let Ok(tls) = self.acceptor.clone().accept(stream.into_inner()).await
                        else {
                            return;
                        };
                        self.encrypted = true;
                        return self.run(Box::new(tls), false).await;
                    }
                } else if upper.starts_with("LOGIN ") {
                    let mut args = rest[6..].split(' ');
                    let (user, pass) = (
                        unquote(args.next().unwrap_or("")),
                        unquote(args.next().unwrap_or("")),
                    );
                    let mut state = self.state.lock().unwrap();
                    state.logins.push(format!("{user}:{pass}"));
                    match &self.accept {
                        Accept::Password(u, p) if user == *u && pass == *p => {
                            state.encrypted.push(self.encrypted);
                            ok(&mut out, "welcome");
                        }
                        _ => {
                            state.refused += 1;
                            no(&mut out, "[AUTHENTICATIONFAILED] wrong user or password");
                        }
                    }
                } else if upper.starts_with("AUTHENTICATE XOAUTH2") {
                    let _ = stream.write_all(b"+ \r\n").await;
                    let mut first = String::new();
                    if stream.read_line(&mut first).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let decoded = base64_decode(first.trim());
                    let good = {
                        let mut state = self.state.lock().unwrap();
                        state.logins.push(decoded.clone());
                        let good = matches!(
                            &self.accept,
                            Accept::Token(token) if decoded.contains(&format!("auth=Bearer {token}"))
                        );
                        if good {
                            state.encrypted.push(self.encrypted);
                        } else {
                            state.refused += 1;
                        }
                        good
                    };
                    if good {
                        ok(&mut out, "welcome");
                    } else {
                        // What Google and Microsoft do: a challenge that
                        // tells why, to which the client answers with an
                        // empty line.
                        let _ = stream.write_all(b"+ eyJzdGF0dXMiOiI0MDEifQ==\r\n").await;
                        let mut empty = String::new();
                        let _ = stream.read_line(&mut empty).await;
                        no(&mut out, "[AUTHENTICATIONFAILED] invalid token");
                    }
                } else if upper == "NOOP" {
                    ok(&mut out, "done");
                } else if upper == "LOGOUT" {
                    let _ = stream
                        .write_all(format!("* BYE bye\r\n{tag} OK done\r\n").as_bytes())
                        .await;
                    return;
                } else {
                    self.data(&tag, &rest, &upper, literal, &mut out);
                }
                if stream.write_all(out.as_bytes()).await.is_err() {
                    return;
                }
            }
        })
    }

    /// The commands that work on the mailboxes.
    fn data(
        &mut self,
        tag: &str,
        rest: &str,
        upper: &str,
        literal: Option<Vec<u8>>,
        out: &mut String,
    ) {
        let shared = self.state.clone();
        let mut state = shared.lock().unwrap();
        if upper.starts_with("LIST ") {
            // LIST "" "pattern" [RETURN (SPECIAL-USE)]
            let args = rest[5..].trim().split_once(' ').map_or("", |(_, p)| p);
            let asked = args.to_uppercase().contains("RETURN (SPECIAL-USE)");
            let pattern = unquote(args.split(" RETURN").next().unwrap_or(""));
            if pattern == "*" {
                state.full_lists += 1;
            }
            for folder in &state.folders {
                if pattern == "*" || pattern == folder.name {
                    let attributes = if state.special_use_only_on_request && !asked {
                        String::new()
                    } else {
                        folder.attributes.join(" ")
                    };
                    out.push_str(&format!(
                        "* LIST ({attributes}) \"/\" \"{}\"\r\n",
                        folder.name
                    ));
                }
            }
            out.push_str(&format!("{tag} OK done\r\n"));
        } else if upper.starts_with("STATUS ") {
            let name = unquote(rest[7..].split(" (").next().unwrap_or(""));
            match state.folders.iter().find(|f| f.name == name) {
                Some(f) => {
                    let unseen = f
                        .messages
                        .iter()
                        .filter(|m| !m.flags.contains("\\Seen"))
                        .count();
                    out.push_str(&format!(
                        "* STATUS \"{name}\" (MESSAGES {} UNSEEN {unseen} UIDNEXT {} UIDVALIDITY {})\r\n{tag} OK done\r\n",
                        f.messages.len(),
                        f.next_uid,
                        f.validity
                    ));
                }
                None => out.push_str(&format!("{tag} NO [NONEXISTENT] no such folder\r\n")),
            }
        } else if upper.starts_with("SELECT ") {
            let (name, condstore) = match rest[7..].split_once(" (") {
                Some((name, _)) => (unquote(name), true),
                None => (unquote(&rest[7..]), false),
            };
            let modseq = if condstore && !state.no_condstore {
                state
                    .folders
                    .iter()
                    .find(|f| f.name == name)
                    .map(|f| format!("* OK [HIGHESTMODSEQ {}] ok\r\n", f.modseq))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            match state.folders.iter().find(|f| f.name == name) {
                Some(f) => {
                    out.push_str(&format!(
                        "* {} EXISTS\r\n* 0 RECENT\r\n* OK [UIDVALIDITY {}] ok\r\n* OK [UIDNEXT {}] ok\r\n{modseq}* FLAGS (\\Seen \\Flagged \\Draft \\Deleted)\r\n{tag} OK [READ-WRITE] done\r\n",
                        f.messages.len(),
                        f.validity,
                        f.next_uid
                    ));
                    self.selected = Some(name);
                }
                None => out.push_str(&format!("{tag} NO [NONEXISTENT] no such folder\r\n")),
            }
        } else if upper.starts_with("APPEND ") {
            let args = &rest[7..];
            let name = unquote(args.split(' ').next().unwrap_or(""));
            let flags: BTreeSet<String> = args
                .split_once('(')
                .and_then(|(_, f)| f.split_once(')'))
                .map(|(f, _)| f.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default();
            let Some(folder) = state.folders.iter_mut().find(|f| f.name == name) else {
                out.push_str(&format!("{tag} NO [TRYCREATE] no such folder\r\n"));
                return;
            };
            let uid = folder.next_uid;
            folder.next_uid += 1;
            folder.modseq += 1;
            folder.messages.push(Msg {
                uid,
                modseq: folder.modseq,
                flags,
                internal: "21-Oct-2026 09:00:00 +0000".into(),
                raw: literal.unwrap_or_default(),
            });
            out.push_str(&format!(
                "{tag} OK [APPENDUID {} {uid}] done\r\n",
                folder.validity
            ));
        } else if upper == "EXPUNGE" || upper.starts_with("UID EXPUNGE ") {
            if upper.starts_with("UID") && state.no_uidplus {
                out.push_str(&format!("{tag} BAD not supported\r\n"));
                return;
            }
            let Some(name) = self.selected.clone() else {
                out.push_str(&format!("{tag} BAD nothing selected\r\n"));
                return;
            };
            let folder = state.folder(&name);
            let limit = upper.strip_prefix("UID EXPUNGE ").map(|set| {
                uids(
                    set,
                    &folder.messages.iter().map(|m| m.uid).collect::<Vec<_>>(),
                )
            });
            let mut seq = 0;
            let mut gone = Vec::new();
            folder.messages.retain(|m| {
                seq += 1;
                let wanted = m.flags.contains("\\Deleted")
                    && limit.as_ref().is_none_or(|l| l.contains(&m.uid));
                if wanted {
                    gone.push(seq - gone.len());
                }
                !wanted
            });
            for n in gone {
                out.push_str(&format!("* {n} EXPUNGE\r\n"));
            }
            out.push_str(&format!("{tag} OK done\r\n"));
        } else if let Some(args) = upper.strip_prefix("UID ") {
            if (args.starts_with("EXPUNGE") && state.no_uidplus)
                || (args.starts_with("MOVE") && state.no_move)
            {
                out.push_str(&format!("{tag} BAD not supported\r\n"));
                return;
            }
            self.uid(&mut state, tag, &rest[4..], args, out);
        } else {
            out.push_str(&format!("{tag} BAD unknown command\r\n"));
        }
    }

    fn uid(&mut self, state: &mut State, tag: &str, rest: &str, upper: &str, out: &mut String) {
        let Some(name) = self.selected.clone() else {
            out.push_str(&format!("{tag} BAD nothing selected\r\n"));
            return;
        };
        if upper.starts_with("SEARCH ") {
            let words: Vec<&str> = rest[7..].split_whitespace().collect();
            let found = matching(state.folder(&name), &words);
            let found: Vec<String> = found.iter().map(u32::to_string).collect();
            let list = if found.is_empty() {
                String::new()
            } else {
                format!(" {}", found.join(" "))
            };
            out.push_str(&format!("* SEARCH{list}\r\n{tag} OK done\r\n"));
        } else if upper.starts_with("SORT ") {
            // SORT (REVERSE DATE) US-ASCII criteria...
            let words: Vec<&str> = rest[5..].split_whitespace().skip(3).collect();
            let folder = state.folder(&name);
            let mut found = matching(folder, &words);
            let date = |uid: &u32| {
                let m = folder.messages.iter().find(|m| m.uid == *uid).unwrap();
                String::from_utf8_lossy(&header_of(&m.raw))
                    .lines()
                    .find_map(|l| l.strip_prefix("Date: "))
                    .and_then(|d| chrono::DateTime::parse_from_rfc2822(d.trim()).ok())
                    .map(|d| d.timestamp())
                    .unwrap_or(0)
            };
            found.sort_by_key(|uid| std::cmp::Reverse((date(uid), *uid)));
            let found: Vec<String> = found.iter().map(u32::to_string).collect();
            let list = if found.is_empty() {
                String::new()
            } else {
                format!(" {}", found.join(" "))
            };
            out.push_str(&format!("* SORT{list}\r\n{tag} OK done\r\n"));
        } else if upper.starts_with("FETCH ") {
            let folder = state.folder(&name);
            let all: Vec<u32> = folder.messages.iter().map(|m| m.uid).collect();
            let (set, items) = rest[6..].split_once(' ').unwrap_or((&rest[6..], ""));
            let wanted = uids(set, &all);
            let changed_since: Option<u64> = items
                .split_once("CHANGEDSINCE ")
                .and_then(|(_, n)| n.trim_end_matches(')').trim().parse().ok());
            for (n, m) in folder.messages.iter().enumerate() {
                if !wanted.contains(&m.uid) || changed_since.is_some_and(|c| m.modseq <= c) {
                    continue;
                }
                let flags = m.flags.iter().cloned().collect::<Vec<_>>().join(" ");
                let mut line = format!("* {} FETCH (UID {}", n + 1, m.uid);
                if items.contains("FLAGS") {
                    line.push_str(&format!(" FLAGS ({flags})"));
                }
                if items.contains("INTERNALDATE") {
                    line.push_str(&format!(" INTERNALDATE \"{}\"", m.internal));
                }
                if items.contains("BODYSTRUCTURE") {
                    line.push_str(&format!(" BODYSTRUCTURE {}", structure(&m.raw)));
                }
                let mut bytes = line.into_bytes();
                if items.contains("BODY.PEEK[HEADER]") {
                    let header = header_of(&m.raw);
                    bytes.extend(format!(" BODY[HEADER] {{{}}}\r\n", header.len()).into_bytes());
                    bytes.extend(header);
                } else if items.contains("BODY.PEEK[]") {
                    bytes.extend(format!(" BODY[] {{{}}}\r\n", m.raw.len()).into_bytes());
                    bytes.extend(&m.raw);
                }
                bytes.extend(b")\r\n");
                // The reply holds the message's bytes: take them as they
                // are, whatever they are.
                out.push_str(&String::from_utf8_lossy(&bytes));
            }
            out.push_str(&format!("{tag} OK done\r\n"));
        } else if upper.starts_with("STORE ") {
            let folder = state.folder(&name);
            let all: Vec<u32> = folder.messages.iter().map(|m| m.uid).collect();
            let mut args = rest[6..].splitn(3, ' ');
            let set = args.next().unwrap_or("");
            let how = args.next().unwrap_or("").to_uppercase();
            let flags: Vec<String> = args
                .next()
                .unwrap_or("")
                .trim_matches(['(', ')'])
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            let silent = how.contains("SILENT");
            folder.modseq += 1;
            let modseq = folder.modseq;
            for (n, m) in folder.messages.iter_mut().enumerate() {
                if uids(set, &all).contains(&m.uid) {
                    m.modseq = modseq;
                    for flag in &flags {
                        if how.starts_with('+') {
                            m.flags.insert(flag.clone());
                        } else {
                            m.flags.remove(flag);
                        }
                    }
                    if !silent {
                        let now = m.flags.iter().cloned().collect::<Vec<_>>().join(" ");
                        out.push_str(&format!(
                            "* {} FETCH (UID {} FLAGS ({now}))\r\n",
                            n + 1,
                            m.uid
                        ));
                    }
                }
            }
            out.push_str(&format!("{tag} OK done\r\n"));
        } else if upper.starts_with("COPY ") || upper.starts_with("MOVE ") {
            let moving = upper.starts_with("MOVE ");
            let (set, dest) = rest[5..].split_once(' ').unwrap_or((&rest[5..], ""));
            let dest = unquote(dest);
            let source = state.folder(&name);
            let all: Vec<u32> = source.messages.iter().map(|m| m.uid).collect();
            let chosen = uids(set, &all);
            let taken: Vec<Msg> = source
                .messages
                .iter()
                .filter(|m| chosen.contains(&m.uid))
                .cloned()
                .collect();
            if moving {
                source.messages.retain(|m| !chosen.contains(&m.uid));
            }
            let Some(target) = state.folders.iter_mut().find(|f| f.name == dest) else {
                out.push_str(&format!("{tag} NO [TRYCREATE] no such folder\r\n"));
                return;
            };
            for mut m in taken {
                m.uid = target.next_uid;
                target.next_uid += 1;
                target.modseq += 1;
                m.modseq = target.modseq;
                target.messages.push(m);
            }
            out.push_str(&format!("{tag} OK done\r\n"));
        } else {
            out.push_str(&format!("{tag} BAD unknown UID command\r\n"));
        }
    }
}

/// What the server says of the parts of a message (`BODYSTRUCTURE`).
fn structure(raw: &[u8]) -> String {
    let Some(message) = mail_parser::MessageParser::default().parse(raw) else {
        return "(\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 0 0)".into();
    };
    part(&message, 0)
}

fn part(message: &mail_parser::Message<'_>, at: usize) -> String {
    use mail_parser::{MimeHeaders, PartType};
    let part = &message.parts[at];
    let (ty, sub) = part
        .content_type()
        .map_or(("TEXT".into(), "PLAIN".into()), |c| {
            (
                c.ctype().to_uppercase(),
                c.subtype().unwrap_or("plain").to_uppercase(),
            )
        });
    if let PartType::Multipart(children) = &part.body {
        let children: String = children.iter().map(|c| part_of(message, *c)).collect();
        return format!("({children} \"{sub}\")");
    }
    let id = part
        .content_id()
        .map_or("NIL".to_owned(), |id| format!("\"{id}\""));
    let disposition = part.content_disposition().map_or("NIL".to_owned(), |d| {
        format!("(\"{}\" NIL)", d.ctype().to_uppercase())
    });
    let size = part.len();
    if ty == "TEXT" {
        format!(
            "(\"TEXT\" \"{sub}\" (\"CHARSET\" \"UTF-8\") {id} NIL \"7BIT\" {size} 1 NIL {disposition})"
        )
    } else {
        format!("(\"{ty}\" \"{sub}\" NIL {id} NIL \"BASE64\" {size} NIL {disposition})")
    }
}

fn part_of(message: &mail_parser::Message<'_>, at: u32) -> String {
    part(message, at as usize)
}

/// The UIDs of the messages of `folder` that the search keys match.
fn matching(folder: &Folder, words: &[&str]) -> Vec<u32> {
    let folder_uids: Vec<u32> = folder.messages.iter().map(|m| m.uid).collect();
    let mut found = Vec::new();
    for m in &folder.messages {
        let mut keep = true;
        let mut i = 0;
        while i < words.len() {
            match words[i].to_uppercase().as_str() {
                "ALL" => {}
                "UNSEEN" => keep &= !m.flags.contains("\\Seen"),
                "DELETED" => keep &= m.flags.contains("\\Deleted"),
                "UID" => {
                    i += 1;
                    keep &= uids(words[i], &folder_uids).contains(&m.uid);
                }
                "SINCE" => {
                    // The day, which the server compares with the day it
                    // took the message in.
                    i += 1;
                    keep &= since(&m.internal, words[i]);
                }
                "HEADER" => {
                    let (field, value) = (words[i + 1], unquote(words[i + 2]));
                    i += 2;
                    let header = String::from_utf8_lossy(&header_of(&m.raw)).to_string();
                    keep &= header.lines().any(|l| {
                        l.to_lowercase()
                            .starts_with(&format!("{}:", field.to_lowercase()))
                            && l.contains(&value)
                    });
                }
                other => panic!("search key {other}"),
            }
            i += 1;
        }
        if keep {
            found.push(m.uid);
        }
    }
    found
}

/// Whether a message dated `internal` (`20-Oct-2026 10:00:00 +0000`) is
/// within `day` (`20-Oct-2026`) or later.
fn since(internal: &str, day: &str) -> bool {
    let parse = |text: &str| chrono::NaiveDate::parse_from_str(text, "%d-%b-%Y").unwrap();
    parse(internal.split(' ').next().unwrap()) >= parse(day)
}

fn base64_decode(text: &str) -> String {
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
    String::from_utf8_lossy(&out).replace('\u{1}', " ")
}
