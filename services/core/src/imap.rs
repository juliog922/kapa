//! Minimal `IMAP4rev1` client (RFC 3501 + IDLE + Gmail's `X-GM-EXT-1`): exactly what watching an
//! inbox needs. Generic over the byte stream so tests can drive it with an in-memory server.

use crate::{Error, Result, mail::parse_internal_date};
use std::{sync::Arc, sync::OnceLock, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream, rustls};

/// Gmail closes IDLE after about 29 minutes; re-issue it before that.
pub const IDLE_RENEW: Duration = Duration::from_mins(25);

/// One untagged response line with its literals (`{n}` blocks) in order.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Line {
    /// Text of the line with literals left out.
    pub text: String,
    /// Literal payloads.
    pub literals: Vec<Vec<u8>>,
}

/// One message fetched from the server.
#[derive(Debug, PartialEq, Eq)]
pub struct Fetched {
    /// IMAP UID.
    pub uid: u32,
    /// Gmail message ID.
    pub gmail_id: u64,
    /// Gmail thread ID.
    pub thread_id: u64,
    /// INTERNALDATE, Unix seconds.
    pub internal_at: i64,
    /// Raw RFC 5322 bytes.
    pub raw: Vec<u8>,
}

/// An IMAP session over any byte stream.
pub struct Imap<S> {
    /// Buffered stream.
    io: BufReader<S>,
    /// Last command tag number.
    tag: u32,
}

/// Installs `ring` as the process-wide rustls crypto provider (once). Required because
/// `tiberius` also enables rustls' `aws-lc-rs` backend: with two backends compiled in, rustls
/// refuses to pick one and panics. Every TLS in core (IMAP, SMTP, SQL Server) then uses ring.
pub fn install_crypto_provider() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        // Err only means a provider was already installed, which is what we want.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Shared TLS configuration with the Mozilla root certificates (built once).
fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            install_crypto_provider();
            let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            Arc::new(rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth())
        })
        .clone()
}

/// Wraps `stream` in TLS, verifying the certificate of `host`.
///
/// # Errors
/// Invalid host name or TLS handshake failure.
pub async fn tls<S: AsyncRead + AsyncWrite + Unpin>(stream: S, host: &str) -> Result<TlsStream<S>> {
    let name = rustls::pki_types::ServerName::try_from(host.to_owned()).map_err(|e| Error::Invalid(format!("host {host}: {e}")))?;
    Ok(TlsConnector::from(tls_config()).connect(name, stream).await?)
}

/// Opens a TCP connection to `host:port`, failing after 30 seconds.
///
/// # Errors
/// Connection failure or timeout.
pub async fn tcp(host: &str, port: u16) -> Result<TcpStream> {
    tokio::time::timeout(Duration::from_secs(30), TcpStream::connect((host, port)))
        .await
        .map_err(|_| Error::Protocol(format!("connect {host}:{port}: timeout")))?
        .map_err(Error::from)
}

/// Quotes a string for IMAP (RFC 3501 quoted string).
fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Returns `n` when `text` ends with a literal marker `{n}`.
fn literal_len(text: &str) -> Option<usize> {
    text.strip_suffix('}')?.rsplit_once('{')?.1.parse().ok()
}

/// Returns the number following `key` in `text`.
fn number_after(text: &str, key: &str) -> Option<u64> {
    let start = text.find(key)? + key.len();
    text[start..].split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// Returns the quoted string following `key` in `text`.
fn quoted_after<'t>(text: &'t str, key: &str) -> Option<&'t str> {
    let start = text.find(key)? + key.len();
    text[start..].split('"').next()
}

/// Turns an untagged FETCH line into a [`Fetched`] message.
fn fetched(line: Line) -> Option<Fetched> {
    let text = line.text.as_str();
    let uid = number_after(text, "(UID ").or_else(|| number_after(text, " UID "));
    Some(Fetched {
        uid: uid.and_then(|u| u32::try_from(u).ok())?,
        gmail_id: number_after(text, "X-GM-MSGID ")?,
        thread_id: number_after(text, "X-GM-THRID ")?,
        internal_at: quoted_after(text, "INTERNALDATE \"").and_then(parse_internal_date)?,
        raw: line.literals.into_iter().next_back().filter(|_| text.contains(" FETCH ("))?,
    })
}

impl<S: AsyncRead + AsyncWrite + Unpin> Imap<S> {
    /// Starts a session on `stream` and reads the server greeting.
    ///
    /// # Errors
    /// I/O failure or a greeting other than `* OK`.
    pub async fn connect(stream: S) -> Result<Self> {
        let mut imap = Self { io: BufReader::new(stream), tag: 0 };
        let greeting = imap.read_line().await?;
        if !greeting.text.starts_with("* OK") {
            return Err(Error::Protocol(format!("unexpected greeting {:?}", greeting.text)));
        }
        Ok(imap)
    }

    /// Reads one response line, including any literals it carries.
    async fn read_line(&mut self) -> Result<Line> {
        let mut line = Line::default();
        loop {
            let mut buf = Vec::new();
            if self.io.read_until(b'\n', &mut buf).await? == 0 {
                return Err(Error::Protocol("connection closed".into()));
            }
            let text = String::from_utf8_lossy(&buf);
            let text = text.trim_end_matches(['\r', '\n']);
            line.text.push_str(text);
            let Some(len) = literal_len(text) else { return Ok(line) };
            let mut literal = vec![0; len];
            self.io.read_exact(&mut literal).await?;
            line.literals.push(literal);
        }
    }

    /// Sends raw bytes and flushes.
    async fn write(&mut self, data: &str) -> Result<()> {
        self.io.get_mut().write_all(data.as_bytes()).await?;
        Ok(self.io.get_mut().flush().await?)
    }

    /// Runs a tagged command and returns its untagged responses.
    async fn command(&mut self, cmd: &str) -> Result<Vec<Line>> {
        self.tag += 1;
        let tag = format!("A{} ", self.tag);
        self.write(&format!("{tag}{cmd}\r\n")).await?;
        let mut lines = Vec::new();
        loop {
            let line = self.read_line().await?;
            if let Some(status) = line.text.strip_prefix(&tag) {
                return if status.starts_with("OK") { Ok(lines) } else { Err(Error::Server(status.to_owned())) };
            }
            lines.push(line);
        }
    }

    /// Logs in.
    ///
    /// # Errors
    /// Wrong credentials (server `NO`) or I/O failure.
    pub async fn login(&mut self, user: &str, password: &str) -> Result<()> {
        self.command(&format!("LOGIN {} {}", quote(user), quote(password))).await.map(drop)
    }

    /// Selects `mailbox` and returns its `(UIDVALIDITY, UIDNEXT)`.
    ///
    /// # Errors
    /// Unknown mailbox, missing response codes or I/O failure.
    pub async fn select(&mut self, mailbox: &str) -> Result<(u32, u32)> {
        let lines = self.command(&format!("SELECT {}", quote(mailbox))).await?;
        let code = |key: &str| lines.iter().find_map(|l| number_after(&l.text, key)).and_then(|n| u32::try_from(n).ok());
        match (code("[UIDVALIDITY "), code("[UIDNEXT ")) {
            (Some(validity), Some(next)) => Ok((validity, next)),
            _ => Err(Error::Protocol("SELECT without UIDVALIDITY/UIDNEXT".into())),
        }
    }

    /// Fetches every message with UID >= `uid`.
    ///
    /// # Errors
    /// Server error or I/O failure.
    pub async fn fetch_since(&mut self, uid: u32) -> Result<Vec<Fetched>> {
        let lines = self.command(&format!("UID FETCH {uid}:* (UID X-GM-MSGID X-GM-THRID INTERNALDATE BODY.PEEK[])")).await?;
        // "n:*" always includes the highest UID, even when it is below n.
        Ok(lines.into_iter().filter_map(fetched).filter(|f| f.uid >= uid).collect())
    }

    /// Waits in IDLE up to `wait` and reports whether new mail (EXISTS) arrived.
    ///
    /// # Errors
    /// IDLE refused, server error or I/O failure.
    pub async fn idle(&mut self, wait: Duration) -> Result<bool> {
        self.tag += 1;
        let tag = format!("A{} ", self.tag);
        self.write(&format!("{tag}IDLE\r\n")).await?;
        let first = self.read_line().await?;
        if !first.text.starts_with('+') {
            return Err(Error::Server(format!("IDLE refused: {}", first.text)));
        }
        let news = tokio::time::timeout(wait, self.wait_exists()).await.unwrap_or(Ok(false))?;
        self.write("DONE\r\n").await?;
        loop {
            let line = self.read_line().await?;
            if let Some(status) = line.text.strip_prefix(&tag) {
                return if status.starts_with("OK") { Ok(news) } else { Err(Error::Server(status.to_owned())) };
            }
        }
    }

    /// Reads untagged lines until one announces new messages.
    async fn wait_exists(&mut self) -> Result<bool> {
        loop {
            if self.read_line().await?.text.ends_with(" EXISTS") {
                return Ok(true);
            }
        }
    }

    /// Ends the session politely.
    ///
    /// # Errors
    /// I/O failure.
    pub async fn logout(&mut self) -> Result<()> {
        self.command("LOGOUT").await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, duplex};

    /// A scripted fake server: for each expected client line, the reply to send.
    async fn server(io: DuplexStream, script: Vec<(&'static str, String)>) -> Vec<String> {
        let (read, mut write) = tokio::io::split(io);
        let mut read = BufReader::new(read);
        write.write_all(b"* OK Gimap ready\r\n").await.unwrap();
        let mut seen = Vec::new();
        for (expect, reply) in script {
            let mut line = String::new();
            read.read_line(&mut line).await.unwrap();
            assert!(line.starts_with(expect), "client sent {line:?}, expected {expect:?}");
            seen.push(line.trim_end().to_owned());
            write.write_all(reply.as_bytes()).await.unwrap();
        }
        seen
    }

    #[tokio::test]
    async fn login_select_fetch_idle() {
        let raw = "Subject: hi\r\n\r\nbody\r\n";
        let script = vec![
            ("A1 LOGIN \"bot@kapalua.es\" \"p\\\"w\"", "A1 OK LOGIN completed\r\n".to_owned()),
            ("A2 SELECT \"INBOX\"", "* 3 EXISTS\r\n* OK [UIDVALIDITY 7] ok\r\n* OK [UIDNEXT 41] ok\r\nA2 OK [READ-WRITE]\r\n".to_owned()),
            (
                "A3 UID FETCH 40:* ",
                format!(
                    "* 2 FETCH (UID 39 X-GM-MSGID 1 X-GM-THRID 1 INTERNALDATE \"24-Oct-2026 11:06:42 +0000\" BODY[] {{2}}\r\nxx)\r\n\
                     * 3 FETCH (UID 40 X-GM-MSGID 1786 X-GM-THRID 1785 INTERNALDATE \"24-Oct-2026 11:06:42 +0000\" BODY[] {{{}}}\r\n{raw})\r\nA3 OK done\r\n",
                    raw.len()
                ),
            ),
            ("A4 IDLE", "+ idling\r\n* 4 EXISTS\r\n".to_owned()),
            ("DONE", "A4 OK IDLE terminated\r\n".to_owned()),
            ("A5 LOGOUT", "* BYE\r\nA5 OK\r\n".to_owned()),
        ];
        let (client, srv) = duplex(1 << 16);
        let server = tokio::spawn(server(srv, script));
        let mut imap = Imap::connect(client).await.unwrap();
        imap.login("bot@kapalua.es", "p\"w").await.unwrap();
        assert_eq!(imap.select("INBOX").await.unwrap(), (7, 41));
        let got = imap.fetch_since(40).await.unwrap();
        assert_eq!(
            got,
            vec![Fetched { uid: 40, gmail_id: 1786, thread_id: 1785, internal_at: 1_792_840_002, raw: raw.as_bytes().to_vec() }]
        );
        assert!(imap.idle(Duration::from_secs(5)).await.unwrap());
        imap.logout().await.unwrap();
        assert_eq!(server.await.unwrap().len(), 6);
    }

    #[tokio::test]
    async fn idle_times_out_and_errors_surface() {
        let script = vec![
            ("A1 LOGIN", "A1 NO [AUTHENTICATIONFAILED] Invalid credentials\r\n".to_owned()),
            ("A2 IDLE", "+ idling\r\n".to_owned()),
            ("DONE", "A2 OK IDLE terminated\r\n".to_owned()),
        ];
        let (client, srv) = duplex(1 << 12);
        let server = tokio::spawn(server(srv, script));
        let mut imap = Imap::connect(client).await.unwrap();
        let err = imap.login("a@b.c", "wrong").await.unwrap_err();
        assert!(err.to_string().contains("AUTHENTICATIONFAILED"), "{err}");
        assert!(!imap.idle(Duration::from_millis(50)).await.unwrap());
        server.await.unwrap();
    }

    #[test]
    fn tls_config_builds_with_both_crypto_backends_compiled_in() {
        // Regression: tiberius enables aws-lc-rs next to ring; building a config must not panic.
        let config = tls_config();
        assert!(Arc::ptr_eq(&config, &tls_config()), "built once and shared");
        assert!(rustls::crypto::CryptoProvider::get_default().is_some(), "a process-wide provider is installed");
    }

    #[test]
    fn parsing_helpers() {
        assert_eq!(literal_len("* 1 FETCH (BODY[] {123}"), Some(123));
        assert_eq!(literal_len("* OK done"), None);
        assert_eq!(quote(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(number_after("[UIDNEXT 41] Predicted", "[UIDNEXT "), Some(41));
        assert!(fetched(Line { text: "* 1 FETCH (UID 1 FLAGS ())".into(), literals: vec![] }).is_none());
    }
}
