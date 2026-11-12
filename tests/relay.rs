//! End-to-end relay test: external SMTP client -> smtpmaster -> fake upstream.
//!
//! Run with `SMTPMASTER_TEST_MONGO_URI=mongodb://127.0.0.1:27017 cargo test`.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bson::oid::ObjectId;
use smtpmaster::config::{Credential, SelectionMode, ServerConfig};
use smtpmaster::model::{Account, Provider, TlsMode};
use smtpmaster::router::Router;
use smtpmaster::smtp::server;
use smtpmaster::store::Store;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

fn test_uri() -> Option<String> {
    std::env::var("SMTPMASTER_TEST_MONGO_URI").ok()
}

async fn test_store(prefix: &str) -> Option<Arc<Store>> {
    let uri = test_uri()?;
    let database = format!("{prefix}_{}", ObjectId::new().to_hex());
    let store = Store::connect(&uri, &database, 1)
        .await
        .expect("connect to test MongoDB");
    store.ensure_indexes().await.expect("create indexes");
    Some(Arc::new(store))
}

struct FakeUpstream {
    address: SocketAddr,
    received: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl FakeUpstream {
    fn messages(&self) -> Vec<Vec<u8>> {
        self.received.lock().unwrap().clone()
    }
}

/// Start a tiny SMTP server; when `auth` is set it requires AUTH PLAIN first.
async fn start_upstream(auth: Option<(&str, &str)>) -> FakeUpstream {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let received: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let received_task = Arc::clone(&received);
    let auth = auth.map(|(user, pass)| (user.to_string(), pass.to_string()));
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let received = Arc::clone(&received_task);
            let auth = auth.clone();
            tokio::spawn(async move {
                let _ = upstream_session(stream, received, auth).await;
            });
        }
    });
    FakeUpstream { address, received }
}

async fn upstream_session(
    stream: TcpStream,
    received: Arc<Mutex<Vec<Vec<u8>>>>,
    auth: Option<(String, String)>,
) -> io::Result<()> {
    let mut reader = BufReader::new(stream);
    write_line(&mut reader, "220 fake ESMTP").await?;
    let mut authenticated = auth.is_none();
    let mut data: Option<Vec<u8>> = None;

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        if let Some(buffer) = data.as_mut() {
            if line.trim_end() == "." {
                received.lock().unwrap().push(std::mem::take(buffer));
                data = None;
                write_line(&mut reader, "250 ok").await?;
            } else {
                buffer.extend_from_slice(line.as_bytes());
            }
            continue;
        }
        let trimmed = line.trim_end();
        let upper = trimmed.to_ascii_uppercase();
        if upper.starts_with("EHLO") {
            write_line(&mut reader, "250-fake\r\n250 AUTH PLAIN LOGIN").await?;
        } else if let Some(payload) = trimmed.strip_prefix("AUTH PLAIN ") {
            let allowed = auth.as_ref().is_some_and(|(user, pass)| {
                BASE64
                    .decode(payload.trim())
                    .ok()
                    .and_then(|decoded| {
                        let parts: Vec<&[u8]> = decoded.split(|&byte| byte == 0).collect();
                        let user_ok = parts
                            .get(parts.len().checked_sub(2)?)
                            .is_some_and(|value| *value == user.as_bytes());
                        let pass_ok = parts.last().is_some_and(|value| *value == pass.as_bytes());
                        Some(user_ok && pass_ok)
                    })
                    .unwrap_or(false)
            });
            if allowed {
                authenticated = true;
                write_line(&mut reader, "235 ok").await?;
            } else {
                write_line(&mut reader, "535 denied").await?;
            }
        } else if upper.starts_with("MAIL FROM") {
            if authenticated {
                write_line(&mut reader, "250 ok").await?;
            } else {
                write_line(&mut reader, "530 auth required").await?;
            }
        } else if upper.starts_with("RCPT TO") {
            write_line(&mut reader, "250 ok").await?;
        } else if upper == "DATA" {
            if authenticated {
                data = Some(Vec::new());
                write_line(&mut reader, "354 go ahead").await?;
            } else {
                write_line(&mut reader, "530 auth required").await?;
            }
        } else if upper == "QUIT" {
            write_line(&mut reader, "221 bye").await?;
            return Ok(());
        } else {
            write_line(&mut reader, "250 ok").await?;
        }
    }
}

async fn write_line<W: AsyncWrite + Unpin>(stream: &mut W, line: &str) -> io::Result<()> {
    stream.write_all(line.as_bytes()).await?;
    stream.write_all(b"\r\n").await?;
    stream.flush().await
}

struct TestClient {
    reader: BufReader<TcpStream>,
}

impl TestClient {
    async fn connect(address: SocketAddr) -> io::Result<Self> {
        let stream = TcpStream::connect(address).await?;
        let mut client = Self {
            reader: BufReader::new(stream),
        };
        let (code, _) = client.read_reply().await?;
        assert_eq!(code, 220, "greeting");
        Ok(client)
    }

    async fn read_reply(&mut self) -> io::Result<(u16, String)> {
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line).await? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "server closed",
                ));
            }
            let trimmed = line.trim_end().to_string();
            let code = trimmed
                .get(..3)
                .and_then(|prefix| prefix.parse::<u16>().ok())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, trimmed.clone()))?;
            let more = trimmed.as_bytes().get(3) == Some(&b'-');
            lines.push(trimmed);
            if !more {
                return Ok((code, lines.join(" | ")));
            }
        }
    }

    async fn command(&mut self, line: &str) -> io::Result<(u16, String)> {
        let stream = self.reader.get_mut();
        stream.write_all(line.as_bytes()).await?;
        stream.write_all(b"\r\n").await?;
        stream.flush().await?;
        self.read_reply().await
    }

    async fn authenticate(&mut self, user: &str, password: &str) -> io::Result<u16> {
        let payload = BASE64.encode(format!("\0{user}\0{password}"));
        let (code, _) = self.command(&format!("AUTH PLAIN {payload}")).await?;
        Ok(code)
    }

    async fn deliver(&mut self, subject: &str) -> io::Result<u16> {
        let (code, _) = self.command("MAIL FROM:<sender@example.com>").await?;
        if code != 250 {
            return Ok(code);
        }
        assert_eq!(self.command("RCPT TO:<rcpt@example.com>").await?.0, 250);
        assert_eq!(self.command("DATA").await?.0, 354);
        let body = format!(
            "From: Sender <sender@example.com>\r\nTo: rcpt@example.com\r\nSubject: {subject}\r\n\r\nhello\r\n"
        );
        let stream = self.reader.get_mut();
        stream.write_all(body.as_bytes()).await?;
        stream.write_all(b".\r\n").await?;
        stream.flush().await?;
        Ok(self.read_reply().await?.0)
    }
}

fn relay_account(name: &str, upstream: &FakeUpstream, count_max: i64) -> Account {
    let mut account = Account::new(
        name,
        Provider::Custom,
        "",
        "",
        "sender@example.com",
        chrono::Utc::now(),
    );
    account.host = upstream.address.ip().to_string();
    account.port = upstream.address.port();
    account.tls = TlsMode::None;
    account.count_max = count_max;
    account
}

async fn start_relay(store: Arc<Store>, config: ServerConfig) -> SocketAddr {
    let router = Arc::new(Router::new(Arc::clone(&store), &config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = server::serve(listener, router, config).await;
    });
    address
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("smtpmaster=debug,warn"))
        .with_test_writer()
        .try_init();
}

#[tokio::test]
async fn relay_distributes_messages_and_enforces_quota() {
    init_tracing();
    let Some(store) = test_store("smtpmaster_e2e").await else {
        eprintln!("skipping: SMTPMASTER_TEST_MONGO_URI not set");
        return;
    };
    let upstream_a = start_upstream(None).await;
    let upstream_b = start_upstream(None).await;
    let account_a = relay_account("a", &upstream_a, 1);
    let account_b = relay_account("b", &upstream_b, 1);
    store.insert_account(&account_a).await.unwrap();
    store.insert_account(&account_b).await.unwrap();

    let mut config = ServerConfig {
        sequential: true,
        concurrency: 1,
        max_attempts: 2,
        send_timeout_secs: 5,
        max_message_size: 1024 * 1024,
        mode: SelectionMode::RoundRobin,
        ..ServerConfig::default()
    };
    config.listen = "127.0.0.1:0".to_string();
    let address = start_relay(Arc::clone(&store), config).await;

    let mut client = TestClient::connect(address).await.unwrap();
    assert_eq!(client.command("EHLO test.local").await.unwrap().0, 250);

    assert_eq!(client.deliver("one").await.unwrap(), 250);
    assert_eq!(client.deliver("two").await.unwrap(), 250);
    assert_eq!(
        client.deliver("three").await.unwrap(),
        451,
        "both accounts are out of quota"
    );

    let total = upstream_a.messages().len() + upstream_b.messages().len();
    assert_eq!(total, 2);
    assert_eq!(upstream_a.messages().len(), 1, "round-robin split");
    assert_eq!(upstream_b.messages().len(), 1, "round-robin split");

    let stored_a = store.find_account("a").await.unwrap();
    let stored_b = store.find_account("b").await.unwrap();
    assert_eq!(stored_a.count_used, 1);
    assert_eq!(stored_b.count_used, 1);
    assert_eq!(stored_a.total_sent, 1);
    assert_eq!(stored_b.total_sent, 1);

    let stats = store.stats(chrono::Utc::now()).await.unwrap();
    assert_eq!(stats.sent_24h, 2);
    assert_eq!(stats.failed_24h, 0);

    let _ = store.database().drop().await;
}

#[tokio::test]
async fn relay_authenticates_clients_and_upstreams() {
    init_tracing();
    let Some(store) = test_store("smtpmaster_auth").await else {
        eprintln!("skipping: SMTPMASTER_TEST_MONGO_URI not set");
        return;
    };
    let upstream = start_upstream(Some(("upstream-user", "upstream-pass"))).await;
    let mut account = relay_account("authed", &upstream, 10);
    account.username = "upstream-user".to_string();
    account.password = "upstream-pass".to_string();
    store.insert_account(&account).await.unwrap();

    let config = ServerConfig {
        sequential: true,
        concurrency: 1,
        max_attempts: 1,
        send_timeout_secs: 5,
        auth_required: true,
        users: vec![Credential {
            username: "relay".to_string(),
            password: "secret".to_string(),
        }],
        ..ServerConfig::default()
    };
    let address = start_relay(Arc::clone(&store), config).await;

    let mut client = TestClient::connect(address).await.unwrap();
    assert_eq!(client.command("EHLO test.local").await.unwrap().0, 250);
    assert_eq!(
        client.deliver("blocked").await.unwrap(),
        530,
        "unauthenticated client must be rejected"
    );
    assert_eq!(client.authenticate("relay", "wrong").await.unwrap(), 535);
    assert_eq!(client.authenticate("relay", "secret").await.unwrap(), 235);
    assert_eq!(client.deliver("allowed").await.unwrap(), 250);

    let messages = upstream.messages();
    assert_eq!(messages.len(), 1);
    let text = String::from_utf8_lossy(&messages[0]);
    assert!(text.contains("Subject: allowed"), "{text}");
    assert!(text.contains("hello"));

    let _ = store.database().drop().await;
}
