//! SMTP protocol helpers shared by the relay server and the upstream client.

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

use crate::error::{AppError, Result};

pub const MAX_LINE: usize = 4096;

/// Read one CRLF/LF terminated line, never buffering more than `max` bytes.
///
/// `Ok(None)` means the peer closed the connection before sending anything.
pub async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return Ok(if out.is_empty() { None } else { Some(out) });
        }
        let newline = buffer.iter().position(|&b| b == b'\n');
        let (take, done) = match newline {
            Some(pos) => (pos + 1, true),
            None => (buffer.len(), false),
        };
        out.extend_from_slice(&buffer[..take]);
        reader.consume(take);
        if out.len() > max {
            return Err(AppError::Smtp("line exceeds the configured limit".into()));
        }
        if done {
            return Ok(Some(out));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub code: u16,
    pub lines: Vec<String>,
}

impl Reply {
    pub fn message(&self) -> String {
        self.lines.join(" ")
    }
}

/// Read a (possibly multi-line) SMTP reply.
pub async fn read_reply<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Reply> {
    let mut lines = Vec::new();
    loop {
        let raw = read_line(reader, MAX_LINE)
            .await?
            .ok_or_else(|| AppError::Smtp("server closed the connection".into()))?;
        let line = String::from_utf8_lossy(&raw).trim_end().to_string();
        if line.len() < 3 {
            return Err(AppError::Smtp(format!(
                "malformed SMTP reply line `{line}`"
            )));
        }
        let code: u16 = line[..3]
            .parse()
            .map_err(|_| AppError::Smtp(format!("malformed SMTP reply code in `{line}`")))?;
        let more = line.as_bytes().get(3) == Some(&b'-');
        lines.push(line);
        if !more {
            return Ok(Reply { code, lines });
        }
    }
}

/// Capability tokens advertised in an EHLO reply, uppercased.
pub fn capabilities(reply: &Reply) -> Vec<String> {
    reply
        .lines
        .iter()
        .skip(1)
        .map(|line| line.get(4..).unwrap_or("").trim().to_ascii_uppercase())
        .filter(|token| !token.is_empty())
        .collect()
}

/// Extract the address from `FROM:<a@b> SIZE=1` / `TO:<a@b>`.
pub fn parse_path(argument: &str) -> Option<String> {
    let open = argument.find('<')?;
    let close = argument[open + 1..].find('>')? + open + 1;
    Some(argument[open + 1..close].trim().to_string())
}

/// Canonicalise every line ending to CRLF.
pub fn normalize_crlf(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 2);
    let mut index = 0;
    while index < raw.len() {
        match raw[index] {
            b'\r' => {
                if raw.get(index + 1) == Some(&b'\n') {
                    index += 2;
                } else {
                    index += 1;
                }
                out.extend_from_slice(b"\r\n");
            }
            b'\n' => {
                index += 1;
                out.extend_from_slice(b"\r\n");
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    out
}

/// Normalise, terminate and dot-stuff a message body for DATA.
pub fn dot_stuff(raw: &[u8]) -> Vec<u8> {
    let normalized = normalize_crlf(raw);
    let mut out = Vec::with_capacity(normalized.len() + 16);
    for line in normalized.split_inclusive(|&byte| byte == b'\n') {
        if line.first() == Some(&b'.') {
            out.push(b'.');
        }
        out.extend_from_slice(line);
    }
    if !out.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Replace (or insert) the `From:` header with the account identity.
pub fn rewrite_from_header(raw: &[u8], from_name: Option<&str>, from_address: &str) -> Vec<u8> {
    let normalized = normalize_crlf(raw);
    let value = match from_name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => format!("From: {name} <{from_address}>"),
        None => format!("From: <{from_address}>"),
    };
    let header_end = normalized
        .windows(4)
        .position(|window| window == b"\r\n\r\n");
    let (header, body) = match header_end {
        Some(pos) => (&normalized[..pos], &normalized[pos..]),
        None => (&normalized[..], &b""[..]),
    };
    let mut out = Vec::with_capacity(normalized.len() + value.len());
    let mut replaced = false;
    for line in header.split(|&byte| byte == b'\n') {
        let line_no_cr = line.strip_suffix(b"\r").unwrap_or(line);
        if line_no_cr.len() >= 5 && line_no_cr[..5].eq_ignore_ascii_case(b"From:") {
            out.extend_from_slice(value.as_bytes());
            out.extend_from_slice(b"\r\n");
            replaced = true;
        } else {
            out.extend_from_slice(line_no_cr);
            out.extend_from_slice(b"\r\n");
        }
    }
    if !replaced {
        let mut prefixed = Vec::with_capacity(out.len() + value.len() + 2);
        prefixed.extend_from_slice(value.as_bytes());
        prefixed.extend_from_slice(b"\r\n");
        prefixed.extend_from_slice(&out);
        out = prefixed;
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body.strip_prefix(b"\r\n\r\n").unwrap_or(body));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_line_endings() {
        assert_eq!(
            normalize_crlf(b"a\nb\r\nc\rd"),
            b"a\r\nb\r\nc\r\nd".to_vec()
        );
        assert_eq!(normalize_crlf(b"a\r\n"), b"a\r\n".to_vec());
    }

    #[test]
    fn dot_stuffing_is_transparent() {
        let raw = b"Subject: hi\n\n.leading dot\n..two\n";
        let stuffed = dot_stuff(raw);
        assert_eq!(
            stuffed,
            b"Subject: hi\r\n\r\n..leading dot\r\n...two\r\n".to_vec()
        );
        // message without trailing newline still gets one
        assert_eq!(dot_stuff(b"hello"), b"hello\r\n".to_vec());
    }

    #[test]
    fn parses_paths() {
        assert_eq!(parse_path("FROM:<a@b.c> SIZE=10").unwrap(), "a@b.c");
        assert_eq!(parse_path("FROM:<>").unwrap(), "");
        assert_eq!(parse_path("TO: <x@y>").unwrap(), "x@y");
        assert!(parse_path("FROM:no-brackets").is_none());
    }

    #[test]
    fn rewrites_existing_from_header() {
        let raw = b"From: Old <old@example.com>\r\nTo: x@y\r\nSubject: s\r\n\r\nbody";
        let out = rewrite_from_header(raw, Some("New Name"), "new@example.com");
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("From: New Name <new@example.com>\r\nTo: x@y\r\n"));
        assert!(text.ends_with("\r\n\r\nbody"));
        assert!(!text.contains("old@example.com"));
    }

    #[test]
    fn inserts_from_header_when_missing() {
        let out = rewrite_from_header(b"To: x@y\r\n\r\nbody", None, "acc@example.com");
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, "From: <acc@example.com>\r\nTo: x@y\r\n\r\nbody");
    }

    #[tokio::test]
    async fn reads_multiline_reply() {
        let (mut one, mut two) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            two.write_all(b"250-first\r\n250-SECOND extra\r\n250 last\r\n")
                .await
                .unwrap();
        });
        let mut reader = tokio::io::BufReader::new(&mut one);
        let reply = read_reply(&mut reader).await.unwrap();
        assert_eq!(reply.code, 250);
        assert_eq!(reply.lines.len(), 3);
        assert_eq!(capabilities(&reply), vec!["SECOND EXTRA", "LAST"]);
    }

    #[tokio::test]
    async fn rejects_overlong_lines() {
        let (mut one, mut two) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            two.write_all(b"AAAAAAAAAAAAAAAAAAAAAAAAAAAA\r\n")
                .await
                .unwrap();
        });
        let mut reader = tokio::io::BufReader::new(&mut one);
        assert!(read_line(&mut reader, 8).await.is_err());
    }
}
