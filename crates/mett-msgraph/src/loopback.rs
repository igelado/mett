//! The loopback HTTP listener that receives the authorization code.
//!
//! The browser is redirected to `http://localhost:<port>/?code=…&state=…`.
//! `localhost` can resolve to either 127.0.0.1 or ::1, so the listener binds
//! the same port on both, unless the host has no IPv6 loopback.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

/// How long a single connection may take to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound on the request head. Larger requests are rejected without
/// reading (or allocating) more than this.
const MAX_HEAD_BYTES: u64 = 16 * 1024;
/// How many random ports to try before giving up on one that is free on
/// both loopback addresses.
const BIND_ATTEMPTS: usize = 10;

pub(crate) struct Listener {
    v4: TcpListener,
    v6: Option<TcpListener>,
    port: u16,
}

impl Listener {
    /// Binds `port` on 127.0.0.1 and ::1; `0` picks a port that is free on
    /// both. If the host has no IPv6 loopback, only 127.0.0.1 is bound.
    ///
    /// Fails if ::1 has the port taken: a browser that resolves `localhost`
    /// to ::1 would then send the sign-in response somewhere else.
    pub async fn bind(port: u16) -> io::Result<Self> {
        let attempts = if port == 0 { BIND_ATTEMPTS } else { 1 };
        for _ in 0..attempts {
            let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
            let port = v4.local_addr()?.port();
            match TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await {
                Ok(v6) => {
                    return Ok(Self {
                        v4,
                        v6: Some(v6),
                        port,
                    });
                }
                // Taken on ::1 only: try another random port, if allowed.
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => {}
                // No IPv6 loopback, so `localhost` can only mean 127.0.0.1.
                Err(_) => return Ok(Self { v4, v6: None, port }),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("port {port} is in use on the IPv6 loopback (::1)"),
        ))
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The redirect URI to register the request with.
    pub fn redirect_uri(&self) -> String {
        format!("http://localhost:{}", self.port())
    }

    /// Waits for the browser's redirect for the request with `state` and
    /// returns its query parameters. Other requests, such as `/favicon.ico`
    /// or a late redirect from an earlier sign-in with a different `state`,
    /// are answered and ignored.
    pub async fn wait_for_redirect(&self, state: &str) -> io::Result<HashMap<String, String>> {
        loop {
            let stream = self.accept().await?;
            match tokio::time::timeout(REQUEST_TIMEOUT, handle(stream, state)).await {
                Ok(Ok(Some(params))) => return Ok(params),
                // Not the redirect, a broken connection or a slow client:
                // keep waiting for the real one.
                Ok(Ok(None) | Err(_)) | Err(_) => continue,
            }
        }
    }

    async fn accept(&self) -> io::Result<TcpStream> {
        let (stream, _) = match &self.v6 {
            Some(v6) => tokio::select! {
                r = self.v4.accept() => r?,
                r = v6.accept() => r?,
            },
            None => self.v4.accept().await?,
        };
        Ok(stream)
    }
}

/// Reads one request. Returns its query parameters if it is the redirect for
/// the request with `state`.
async fn handle(stream: TcpStream, state: &str) -> io::Result<Option<HashMap<String, String>>> {
    // `take` caps how much is read, so an oversized line can't make
    // `read_line` allocate without bound.
    let mut reader = BufReader::new(stream.take(MAX_HEAD_BYTES));
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;

    // Read the headers up to the blank line that ends them. Reaching the cap
    // or the end of the stream first means the request is too large or
    // incomplete.
    let mut complete = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            complete = true;
            break;
        }
    }

    let redirect = complete
        .then(|| parse_request_line(&request_line))
        .flatten();
    let stale = redirect
        .as_ref()
        .is_some_and(|p| p.get("state").map(String::as_str) != Some(state));
    let params = redirect.filter(|_| !stale);
    let (status, body) = match &params {
        _ if !complete => ("431 Request Header Fields Too Large", "Request too large."),
        _ if stale => (
            "400 Bad Request",
            "This sign-in response is not for mett's current request.",
        ),
        Some(p) if p.contains_key("error") => {
            ("200 OK", "Sign-in failed. Return to mett for details.")
        }
        Some(_) => ("200 OK", "Signed in to mett. You can close this window."),
        None => ("404 Not Found", "Not found."),
    };
    let html = format!("<!doctype html><title>mett</title><p>{body}</p>\n");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    let mut stream = reader.into_inner().into_inner();
    let sent = async {
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await
    }
    .await;
    // Once a valid redirect has arrived, answering the browser is best
    // effort: it may already have closed the connection.
    match params {
        Some(params) => Ok(Some(params)),
        None => sent.map(|()| None),
    }
}

/// Parses `GET /?a=b HTTP/1.1` into its query parameters, if the request is
/// for the root path and carries `code` or `error`.
fn parse_request_line(line: &str) -> Option<HashMap<String, String>> {
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next()?, parts.next()?);
    if method != "GET" {
        return None;
    }
    let url = Url::parse("http://localhost").ok()?.join(target).ok()?;
    if url.path() != "/" {
        return None;
    }
    let params: HashMap<String, String> = url.query_pairs().into_owned().collect();
    (params.contains_key("code") || params.contains_key("error")).then_some(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_redirect_request_lines() {
        let p = parse_request_line("GET /?code=abc&state=x%20y HTTP/1.1\r\n").unwrap();
        assert_eq!(p["code"], "abc");
        assert_eq!(p["state"], "x y");
        assert!(parse_request_line("GET /?error=access_denied HTTP/1.1").is_some());
        assert!(parse_request_line("GET /favicon.ico HTTP/1.1").is_none());
        assert!(parse_request_line("GET / HTTP/1.1").is_none());
        assert!(parse_request_line("POST /?code=a HTTP/1.1").is_none());
    }

    #[tokio::test]
    async fn rejects_oversized_requests() {
        let listener = Listener::bind(0).await.unwrap();
        let addr = format!("127.0.0.1:{}", listener.port());
        let wait = tokio::spawn(async move { listener.wait_for_redirect("s").await });

        let mut stream = TcpStream::connect(&addr).await.unwrap();
        let huge = "x".repeat(MAX_HEAD_BYTES as usize * 2);
        let request = format!("GET /?code=c&state=s HTTP/1.1\r\nX-Pad: {huge}\r\n\r\n");
        // The listener may close the connection before reading everything.
        let _ = stream.write_all(request.as_bytes()).await;
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response).await;
        assert!(response.starts_with("HTTP/1.1 431"), "{response}");

        // The oversized request was not taken as the redirect.
        assert!(!wait.is_finished());
        wait.abort();
    }

    #[tokio::test]
    async fn binds_a_port_free_on_both_loopbacks() {
        let listener = Listener::bind(0).await.unwrap();
        // If this host has an IPv6 loopback, the same port is bound on it.
        if std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_ok() {
            assert!(listener.v6.is_some());
        }
    }

    #[tokio::test]
    async fn fails_when_the_requested_port_is_taken_on_ipv6() {
        let Ok(v6) = std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)) else {
            return; // No IPv6 loopback on this host.
        };
        let port = v6.local_addr().unwrap().port();
        if std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_err() {
            return; // The port is taken on IPv4 too; nothing to test.
        }
        let err = Listener::bind(port).await.err().expect("bind must fail");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn receives_the_redirect_and_ignores_other_requests() {
        let listener = Listener::bind(0).await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.port());
        let client = tokio::spawn(async move {
            let http = reqwest::Client::new();
            let favicon = http
                .get(format!("{base}/favicon.ico"))
                .send()
                .await
                .unwrap();
            assert_eq!(favicon.status(), 404);
            let stale = http
                .get(format!("{base}/?code=old&state=old"))
                .send()
                .await
                .unwrap();
            assert_eq!(stale.status(), 400);
            let ok = http
                .get(format!("{base}/?code=c&state=s"))
                .send()
                .await
                .unwrap();
            assert_eq!(ok.status(), 200);
        });
        let params = listener.wait_for_redirect("s").await.unwrap();
        assert_eq!(params["code"], "c");
        client.await.unwrap();
    }
}
