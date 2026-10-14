//! The loopback HTTP listener that receives the authorization code.
//!
//! The browser is redirected to `http://localhost:<port>/?code=…&state=…`.
//! `localhost` can resolve to either 127.0.0.1 or ::1, so the listener binds
//! both when it can.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

/// How long a single connection may take to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound on the request head, to ignore misbehaving clients.
const MAX_HEAD_BYTES: usize = 16 * 1024;

pub(crate) struct Listener {
    v4: TcpListener,
    v6: Option<TcpListener>,
    port: u16,
}

impl Listener {
    /// Binds 127.0.0.1 (and ::1 when available) on `port`; `0` picks a free
    /// port.
    pub async fn bind(port: u16) -> io::Result<Self> {
        let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
        let port = v4.local_addr()?.port();
        let v6 = TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await.ok();
        Ok(Self { v4, v6, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The redirect URI to register the request with.
    pub fn redirect_uri(&self) -> String {
        format!("http://localhost:{}", self.port())
    }

    /// Waits for the browser's redirect and returns its query parameters.
    /// Other requests (such as `/favicon.ico`) get a 404 and are ignored.
    pub async fn wait_for_redirect(&self) -> io::Result<HashMap<String, String>> {
        loop {
            let stream = self.accept().await?;
            match tokio::time::timeout(REQUEST_TIMEOUT, handle(stream)).await {
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

/// Reads one request. Returns its query parameters if it is the redirect.
async fn handle(stream: TcpStream) -> io::Result<Option<HashMap<String, String>>> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;

    // Drain the headers so the browser sees a clean response.
    let mut read = request_line.len();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        read += n;
        if n == 0 || line == "\r\n" || line == "\n" || read > MAX_HEAD_BYTES {
            break;
        }
    }

    let params = parse_request_line(&request_line);
    let (status, body) = match &params {
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
    let mut stream = reader.into_inner();
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(params)
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
            let ok = http
                .get(format!("{base}/?code=c&state=s"))
                .send()
                .await
                .unwrap();
            assert_eq!(ok.status(), 200);
        });
        let params = listener.wait_for_redirect().await.unwrap();
        assert_eq!(params["code"], "c");
        client.await.unwrap();
    }
}
