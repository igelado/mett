//! A bare SMTP server in this process: it takes every message and keeps it.

#![allow(dead_code, missing_docs)]

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

#[derive(Clone, Debug, Default)]
pub struct Taken {
    pub from: String,
    pub to: Vec<String>,
    pub data: String,
}

pub struct Smtp {
    pub port: u16,
    pub taken: Arc<Mutex<Vec<Taken>>>,
}

pub async fn smtp() -> Smtp {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let taken = Arc::new(Mutex::new(Vec::new()));
    let record = taken.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let record = record.clone();
            tokio::spawn(async move {
                let mut stream = BufReader::new(stream);
                let _ = stream.write_all(b"220 localhost ready\r\n").await;
                let mut seen = Taken::default();
                let mut line = String::new();
                loop {
                    line.clear();
                    if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let command = line.trim_end().to_owned();
                    let upper = command.to_uppercase();
                    let reply = if upper.starts_with("EHLO") {
                        "250-localhost\r\n250 8BITMIME\r\n".to_owned()
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
            });
        }
    });
    Smtp { port, taken }
}

fn between(command: &str) -> String {
    let (_, rest) = command.split_once('<').unwrap_or(("", ""));
    rest.split('>').next().unwrap_or_default().to_owned()
}
