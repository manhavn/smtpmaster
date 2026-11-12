//! SMTP relay front-end: accepts mail from the external app and hands it to
//! the router.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bson::oid::ObjectId;
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use crate::config::ServerConfig;
use crate::error::{AppError, Result};
use crate::router::Router;
use crate::smtp::protocol::{self, MAX_LINE};
use crate::store::Store;

const SESSION_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_AUTH_FAILURES: u8 = 3;

/// Serve accepted connections until the listener errors out or is dropped.
pub async fn serve(
    listener: TcpListener,
    router: Arc<Router>,
    store: Arc<Store>,
    config: ServerConfig,
) -> Result<()> {
    let config = Arc::new(config);
    let connections = Arc::new(Semaphore::new(config.max_connections.max(1)));
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(error) => {
                warn!(error = %error, "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let permit = match connections.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                warn!(%peer, "connection limit reached, dropping client");
                continue;
            }
        };
        let router = Arc::clone(&router);
        let store = Arc::clone(&store);
        let config = Arc::clone(&config);
        tokio::spawn(async move {
            let _permit = permit;
            debug!(%peer, "client connected");
            if let Err(error) = handle(stream, router, store, config).await {
                debug!(%peer, error = %error, "session closed");
            }
            debug!(%peer, "client disconnected");
        });
    }
}

#[derive(Default)]
struct Transaction {
    mail_from: Option<String>,
    recipients: Vec<String>,
}

#[derive(Default)]
struct Session {
    transaction: Transaction,
    authenticated: bool,
    auth_failures: u8,
    /// Route bound by a successful inbound AUTH, when routes are configured.
    route_id: Option<ObjectId>,
    /// Routes exist in the database: AUTH is mandatory and tenant-scoped.
    routes_enabled: bool,
}

async fn handle(
    stream: TcpStream,
    router: Arc<Router>,
    store: Arc<Store>,
    config: Arc<ServerConfig>,
) -> Result<()> {
    let _ = stream.set_nodelay(true);
    let mut stream = BufReader::new(stream);
    let routes_enabled = store.routes_active().await?;
    let mut session = Session {
        routes_enabled,
        ..Session::default()
    };

    reply(
        &mut stream,
        &format!("220 {} ESMTP smtpmaster", config.hostname),
    )
    .await?;

    loop {
        let Some(raw) = read_with_timeout(&mut stream, MAX_LINE).await? else {
            return Ok(());
        };
        let line = String::from_utf8_lossy(&raw);
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        let (verb, rest) = match line.split_once(' ') {
            Some((verb, rest)) => (verb.to_ascii_uppercase(), rest.trim().to_string()),
            None => (line.to_ascii_uppercase(), String::new()),
        };
        match verb.as_str() {
            "EHLO" => {
                let mut response = format!("250-{}\r\n", config.hostname);
                response.push_str("250-8BITMIME\r\n");
                response.push_str("250-PIPELINING\r\n");
                if !config.users.is_empty() || session.routes_enabled {
                    response.push_str("250-AUTH PLAIN LOGIN\r\n");
                }
                response.push_str(&format!("250 SIZE {}\r\n", config.max_message_size));
                stream.write_all(response.as_bytes()).await?;
                stream.flush().await?;
            }
            "HELO" => {
                reply(&mut stream, &format!("250 {}", config.hostname)).await?;
            }
            "AUTH" => {
                if config.users.is_empty() && !session.routes_enabled {
                    reply(&mut stream, "503 5.5.1 AUTH is not enabled").await?;
                    continue;
                }
                if session.authenticated {
                    reply(&mut stream, "503 5.5.1 already authenticated").await?;
                    continue;
                }
                match authenticate(&mut stream, &rest).await {
                    Ok(Some((username, password))) => {
                        let (valid, route_id) = if session.routes_enabled {
                            match store.authenticate_route(&username, &password).await {
                                Ok(Some(route)) => (true, Some(route.id)),
                                Ok(None) => (false, None),
                                Err(error) => {
                                    debug!(error = %error, "route lookup failed");
                                    (false, None)
                                }
                            }
                        } else {
                            (
                                config.users.iter().any(|user| {
                                    user.username == username && user.password == password
                                }),
                                None,
                            )
                        };
                        if valid {
                            session.authenticated = true;
                            session.route_id = route_id;
                            reply(&mut stream, "235 2.7.0 Authentication successful").await?;
                        } else {
                            session.auth_failures += 1;
                            warn!("rejected SMTP AUTH credentials");
                            reply(&mut stream, "535 5.7.8 Authentication failed").await?;
                            if session.auth_failures >= MAX_AUTH_FAILURES {
                                reply(&mut stream, "421 4.7.0 Too many authentication failures")
                                    .await?;
                                return Ok(());
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        debug!(error = %error, "AUTH aborted");
                        reply(&mut stream, "501 5.5.2 Cannot decode AUTH response").await?;
                    }
                }
            }
            "MAIL" => {
                if (config.auth_required || session.routes_enabled) && !session.authenticated {
                    reply(&mut stream, "530 5.7.0 Authentication required").await?;
                    continue;
                }
                if !rest.to_ascii_uppercase().starts_with("FROM:") {
                    reply(&mut stream, "501 5.5.2 Syntax: MAIL FROM:<address>").await?;
                    continue;
                }
                match protocol::parse_path(&rest) {
                    Some(address) => {
                        session.transaction = Transaction {
                            mail_from: Some(address),
                            recipients: Vec::new(),
                        };
                        reply(&mut stream, "250 2.1.0 Sender OK").await?;
                    }
                    None => {
                        reply(&mut stream, "501 5.1.7 Bad sender address syntax").await?;
                    }
                }
            }
            "RCPT" => {
                if session.transaction.mail_from.is_none() {
                    reply(&mut stream, "503 5.5.1 Need MAIL FROM first").await?;
                    continue;
                }
                if !rest.to_ascii_uppercase().starts_with("TO:") {
                    reply(&mut stream, "501 5.5.2 Syntax: RCPT TO:<address>").await?;
                    continue;
                }
                match protocol::parse_path(&rest) {
                    Some(address) if !address.is_empty() => {
                        if session.transaction.recipients.len() >= config.max_recipients {
                            reply(&mut stream, "452 4.5.3 Too many recipients").await?;
                            continue;
                        }
                        session.transaction.recipients.push(address);
                        reply(&mut stream, "250 2.1.5 Recipient OK").await?;
                    }
                    _ => {
                        reply(&mut stream, "501 5.1.3 Bad recipient address syntax").await?;
                    }
                }
            }
            "DATA" => {
                let Some(mail_from) = session.transaction.mail_from.clone() else {
                    reply(&mut stream, "503 5.5.1 Need MAIL FROM first").await?;
                    continue;
                };
                if session.transaction.recipients.is_empty() {
                    reply(&mut stream, "503 5.5.1 Need RCPT TO first").await?;
                    continue;
                }
                reply(&mut stream, "354 End data with <CR><LF>.<CR><LF>").await?;
                let raw = match read_data(&mut stream, &config).await {
                    Ok(raw) => raw,
                    Err(error) => {
                        let (code, text) = error.smtp_code();
                        let _ = reply(&mut stream, &format!("{code} {text}")).await;
                        return Ok(());
                    }
                };
                let recipients = std::mem::take(&mut session.transaction.recipients);
                session.transaction.mail_from = None;
                match router
                    .dispatch(session.route_id, &mail_from, &recipients, &raw)
                    .await
                {
                    Ok(delivery) => {
                        info!(
                            account = %delivery.account_name,
                            message_id = %delivery.message_id,
                            recipients = recipients.len(),
                            attempts = delivery.attempts,
                            "message relayed"
                        );
                        reply(
                            &mut stream,
                            &format!("250 2.0.0 Ok: queued as {}", delivery.message_id.to_hex()),
                        )
                        .await?;
                    }
                    Err(error) => {
                        let (code, text) = error.smtp_code();
                        warn!(error = %error, "message not relayed");
                        reply(&mut stream, &format!("{code} {text}")).await?;
                    }
                }
            }
            "RSET" => {
                session.transaction = Transaction::default();
                reply(&mut stream, "250 2.0.0 Reset state").await?;
            }
            "NOOP" => reply(&mut stream, "250 2.0.0 OK").await?,
            "QUIT" => {
                reply(&mut stream, "221 2.0.0 Bye").await?;
                return Ok(());
            }
            "VRFY" => reply(&mut stream, "252 2.5.2 Cannot verify user").await?,
            "HELP" => {
                reply(
                    &mut stream,
                    "214 2.0.0 EHLO AUTH MAIL RCPT DATA RSET NOOP QUIT",
                )
                .await?
            }
            _ => reply(&mut stream, "502 5.5.2 Command not implemented").await?,
        }
    }
}

/// Returns the decoded `(username, password)` after a successful exchange.
async fn authenticate<R: AsyncBufRead + AsyncWrite + Unpin>(
    stream: &mut R,
    rest: &str,
) -> Result<Option<(String, String)>> {
    let (mechanism, initial) = match rest.split_once(' ') {
        Some((mechanism, initial)) => (mechanism.to_ascii_uppercase(), Some(initial.trim())),
        None => (rest.to_ascii_uppercase(), None),
    };
    match mechanism.as_str() {
        "PLAIN" => {
            let payload = match initial {
                Some(value) if !value.is_empty() => value.to_string(),
                _ => {
                    reply(stream, "334 ").await?;
                    let Some(line) = read_with_timeout(stream, MAX_LINE).await? else {
                        return Ok(None);
                    };
                    String::from_utf8_lossy(&line).trim().to_string()
                }
            };
            let decoded = BASE64
                .decode(payload.as_bytes())
                .map_err(|_| AppError::Invalid("AUTH PLAIN payload is not base64".into()))?;
            let parts: Vec<&[u8]> = decoded.split(|&byte| byte == 0).collect();
            if parts.len() < 3 {
                return Err(AppError::Invalid("malformed AUTH PLAIN payload".into()));
            }
            let username = String::from_utf8_lossy(parts[parts.len() - 2]).to_string();
            let password = String::from_utf8_lossy(parts[parts.len() - 1]).to_string();
            if username.is_empty() || password.is_empty() {
                return Err(AppError::Invalid("empty AUTH PLAIN credentials".into()));
            }
            Ok(Some((username, password)))
        }
        "LOGIN" => {
            reply(stream, "334 VXNlcm5hbWU6").await?;
            let Some(line) = read_with_timeout(stream, MAX_LINE).await? else {
                return Ok(None);
            };
            let username = BASE64
                .decode(String::from_utf8_lossy(&line).trim().as_bytes())
                .map_err(|_| AppError::Invalid("AUTH LOGIN username is not base64".into()))?;
            reply(stream, "334 UGFzc3dvcmQ6").await?;
            let Some(line) = read_with_timeout(stream, MAX_LINE).await? else {
                return Ok(None);
            };
            let password = BASE64
                .decode(String::from_utf8_lossy(&line).trim().as_bytes())
                .map_err(|_| AppError::Invalid("AUTH LOGIN password is not base64".into()))?;
            Ok(Some((
                String::from_utf8_lossy(&username).to_string(),
                String::from_utf8_lossy(&password).to_string(),
            )))
        }
        _ => {
            warn!(mechanism = %mechanism, "unsupported AUTH mechanism");
            reply(stream, "504 5.5.4 Unsupported authentication mechanism").await?;
            Ok(None)
        }
    }
}

async fn read_data<R: AsyncBufRead + Unpin>(
    stream: &mut R,
    config: &ServerConfig,
) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    loop {
        let Some(line) = read_with_timeout(stream, config.max_message_size.max(MAX_LINE)).await?
        else {
            return Err(AppError::Smtp(
                "client closed the connection during DATA".into(),
            ));
        };
        let content = line.strip_suffix(b"\n").unwrap_or(&line);
        let content = content.strip_suffix(b"\r").unwrap_or(content);
        if content == b"." {
            break;
        }
        // Undo transparency: a leading dot was added by the client.
        let content = content.strip_prefix(b".").unwrap_or(content);
        data.extend_from_slice(content);
        data.extend_from_slice(b"\r\n");
        if data.len() > config.max_message_size {
            return Err(AppError::TooLarge(format!(
                "message exceeds {} bytes",
                config.max_message_size
            )));
        }
    }
    if data.is_empty() {
        return Err(AppError::TooLarge("empty message".into()));
    }
    Ok(protocol::normalize_crlf(&data))
}

async fn read_with_timeout<R: AsyncBufRead + Unpin>(
    stream: &mut R,
    max: usize,
) -> Result<Option<Vec<u8>>> {
    match tokio::time::timeout(SESSION_TIMEOUT, protocol::read_line(stream, max)).await {
        Ok(result) => result,
        Err(_) => Err(AppError::Smtp("client timed out".into())),
    }
}

async fn reply<W: AsyncWrite + Unpin>(stream: &mut W, text: &str) -> Result<()> {
    stream.write_all(text.as_bytes()).await?;
    stream.write_all(b"\r\n").await?;
    stream.flush().await?;
    Ok(())
}
