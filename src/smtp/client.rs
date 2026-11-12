//! Upstream SMTP client used to relay a message through one account.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream};

use crate::error::{AppError, Result};
use crate::model::{Account, TlsMode};
use crate::smtp::protocol::{self, Reply};

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// Relay `raw` through `account`, with a hard timeout for the whole session.
pub async fn send(
    account: &Account,
    mail_from: &str,
    recipients: &[String],
    raw: &[u8],
    hostname: &str,
    timeout: Duration,
) -> Result<()> {
    tokio::time::timeout(
        timeout,
        session(account, mail_from, recipients, raw, hostname),
    )
    .await
    .map_err(|_| {
        AppError::Smtp(format!(
            "timed out after {}s talking to {}:{}",
            timeout.as_secs(),
            account.host,
            account.port
        ))
    })?
}

async fn session(
    account: &Account,
    mail_from: &str,
    recipients: &[String],
    raw: &[u8],
    hostname: &str,
) -> Result<()> {
    if recipients.is_empty() {
        return Err(AppError::Invalid("no recipients".into()));
    }
    let address = format!("{}:{}", account.host, account.port);
    let tcp = TcpStream::connect(&address)
        .await
        .map_err(|e| AppError::Smtp(format!("connect {address}: {e}")))?;
    let _ = tcp.set_nodelay(true);

    let stream: Box<dyn Stream> = match account.tls {
        TlsMode::Implicit => Box::new(tls_connect(tcp, &account.host).await?),
        TlsMode::None => Box::new(tcp),
        TlsMode::Starttls => {
            let mut plain = BufReader::new(tcp);
            greeting(&mut plain).await?;
            let capabilities = ehlo(&mut plain, hostname).await?;
            if !capabilities.iter().any(|cap| cap == "STARTTLS") {
                return Err(AppError::Smtp(format!(
                    "{} does not advertise STARTTLS",
                    account.host
                )));
            }
            write_line(&mut plain, "STARTTLS").await?;
            let reply = protocol::read_reply(&mut plain).await?;
            expect(&reply, &[220], "STARTTLS")?;
            let tcp = plain.into_inner();
            Box::new(tls_connect(tcp, &account.host).await?)
        }
    };

    let mut connection = BufReader::new(stream);
    if account.tls != TlsMode::Starttls {
        // STARTTLS sessions already consumed the greeting before upgrading.
        greeting(&mut connection).await?;
    }
    let capabilities = ehlo(&mut connection, hostname).await?;
    authenticate(&mut connection, account, &capabilities).await?;
    transmit(&mut connection, mail_from, recipients, raw).await?;
    let _ = write_line(&mut connection, "QUIT").await;
    Ok(())
}

async fn greeting<S: AsyncBufRead + Unpin>(stream: &mut S) -> Result<()> {
    let reply = protocol::read_reply(stream).await?;
    expect(&reply, &[220], "greeting")
}

async fn ehlo<S: AsyncBufRead + AsyncWrite + Unpin>(
    stream: &mut S,
    hostname: &str,
) -> Result<Vec<String>> {
    write_line(stream, &format!("EHLO {hostname}")).await?;
    let reply = protocol::read_reply(stream).await?;
    if reply.code == 250 {
        return Ok(protocol::capabilities(&reply));
    }
    if reply.code / 100 == 5 {
        write_line(stream, &format!("HELO {hostname}")).await?;
        let reply = protocol::read_reply(stream).await?;
        expect(&reply, &[250], "HELO")?;
        return Ok(Vec::new());
    }
    Err(AppError::Smtp(format!(
        "EHLO rejected: {}",
        reply.message()
    )))
}

async fn authenticate<S: AsyncBufRead + AsyncWrite + Unpin>(
    stream: &mut S,
    account: &Account,
    capabilities: &[String],
) -> Result<()> {
    if account.username.is_empty() {
        return Ok(());
    }
    let mut mechanisms: Vec<String> = capabilities
        .iter()
        .find(|cap| cap.starts_with("AUTH"))
        .map(|line| {
            line.split_whitespace()
                .skip(1)
                .map(str::to_ascii_uppercase)
                .collect()
        })
        .unwrap_or_default();
    if mechanisms.is_empty() {
        // Some relays advertise AUTH only after STARTTLS; that is already done.
        mechanisms = vec!["PLAIN".to_string(), "LOGIN".to_string()];
    }

    if mechanisms.iter().any(|m| m == "PLAIN") {
        let payload = BASE64.encode(format!("\0{}\0{}", account.username, account.password));
        write_line(stream, &format!("AUTH PLAIN {payload}")).await?;
        let mut reply = protocol::read_reply(stream).await?;
        if reply.code == 334 {
            write_line(stream, &payload).await?;
            reply = protocol::read_reply(stream).await?;
        }
        if reply.code == 235 {
            return Ok(());
        }
        // Only fall back to LOGIN when PLAIN is not implemented. A 535 means
        // the credentials are wrong and retrying will not help.
        let unsupported = matches!(reply.code, 500 | 501 | 502 | 504);
        if !unsupported || !mechanisms.iter().any(|m| m == "LOGIN") {
            return Err(AppError::Smtp(format!(
                "AUTH PLAIN rejected: {}",
                reply.message()
            )));
        }
    }

    write_line(stream, "AUTH LOGIN").await?;
    let reply = protocol::read_reply(stream).await?;
    if reply.code != 334 {
        return Err(AppError::Smtp(format!(
            "AUTH LOGIN rejected: {}",
            reply.message()
        )));
    }
    write_line(stream, &BASE64.encode(account.username.as_bytes())).await?;
    let reply = protocol::read_reply(stream).await?;
    if reply.code != 334 {
        return Err(AppError::Smtp(format!(
            "AUTH LOGIN username rejected: {}",
            reply.message()
        )));
    }
    write_line(stream, &BASE64.encode(account.password.as_bytes())).await?;
    let reply = protocol::read_reply(stream).await?;
    expect(&reply, &[235], "AUTH LOGIN")?;
    Ok(())
}

async fn transmit<S: AsyncBufRead + AsyncWrite + Unpin>(
    stream: &mut S,
    mail_from: &str,
    recipients: &[String],
    raw: &[u8],
) -> Result<()> {
    let sender = if mail_from.trim().is_empty() {
        "<>".to_string()
    } else {
        format!("<{}>", mail_from.trim())
    };
    write_line(stream, &format!("MAIL FROM:{sender}")).await?;
    let reply = protocol::read_reply(stream).await?;
    expect(&reply, &[250, 251], "MAIL FROM")?;

    let mut accepted = 0usize;
    let mut last_rejected = None;
    for recipient in recipients {
        write_line(stream, &format!("RCPT TO:<{}>", recipient.trim())).await?;
        let reply = protocol::read_reply(stream).await?;
        if reply.code == 250 || reply.code == 251 {
            accepted += 1;
        } else {
            last_rejected = Some(reply.message());
        }
    }
    if accepted == 0 {
        return Err(AppError::Smtp(format!(
            "all recipients rejected: {}",
            last_rejected.unwrap_or_else(|| "unknown reason".to_string())
        )));
    }

    write_line(stream, "DATA").await?;
    let reply = protocol::read_reply(stream).await?;
    expect(&reply, &[354], "DATA")?;
    stream.write_all(&protocol::dot_stuff(raw)).await?;
    stream.write_all(b".\r\n").await?;
    stream.flush().await?;
    let reply = protocol::read_reply(stream).await?;
    expect(&reply, &[250], "message body")?;
    Ok(())
}

fn expect(reply: &Reply, codes: &[u16], stage: &str) -> Result<()> {
    if codes.contains(&reply.code) {
        return Ok(());
    }
    Err(AppError::Smtp(format!(
        "{stage} failed with {}: {}",
        reply.code,
        reply.message()
    )))
}

async fn write_line<W: AsyncWrite + Unpin>(stream: &mut W, line: &str) -> Result<()> {
    stream.write_all(line.as_bytes()).await?;
    stream.write_all(b"\r\n").await?;
    stream.flush().await?;
    Ok(())
}

async fn tls_connect(tcp: TcpStream, host: &str) -> Result<TlsStream<TcpStream>> {
    let connector = tls_connector();
    let server_name = ServerName::try_from(host.to_string())
        .map_err(|_| AppError::Smtp(format!("invalid TLS server name `{host}`")))?;
    connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| AppError::Smtp(format!("TLS handshake with {host} failed: {e}")))
}

fn tls_connector() -> Arc<TlsConnector> {
    static CONNECTOR: OnceLock<Arc<TlsConnector>> = OnceLock::new();
    CONNECTOR
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("the ring provider supports the default TLS versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(TlsConnector::from(Arc::new(config)))
        })
        .clone()
}
