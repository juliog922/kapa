//! Minimal SMTP submission client (RFC 5321 + STARTTLS + AUTH PLAIN). Generic over the byte
//! stream after TLS so tests can drive it with an in-memory server.

use crate::{
    Error, Result,
    config::{Credentials, Server},
    imap,
    mail::base64,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// Reads a (possibly multi-line) reply and returns its code and text.
async fn reply<S: AsyncRead + AsyncWrite + Unpin>(io: &mut BufReader<S>) -> Result<(u16, String)> {
    let mut text = String::new();
    loop {
        let mut line = String::new();
        if io.read_line(&mut line).await? == 0 {
            return Err(Error::Protocol("connection closed".into()));
        }
        let code = line.get(..3).and_then(|c| c.parse::<u16>().ok()).ok_or_else(|| Error::Protocol(format!("bad reply {line:?}")))?;
        text.push_str(line.get(4..).unwrap_or_default().trim_end());
        if line.as_bytes().get(3) != Some(&b'-') {
            return Ok((code, text));
        }
        text.push(' ');
    }
}

/// Expects a reply whose code is in the same class (first digit) as `expect`.
async fn expect<S: AsyncRead + AsyncWrite + Unpin>(io: &mut BufReader<S>, expect: u16) -> Result<()> {
    let (code, text) = reply(io).await?;
    if code / 100 == expect / 100 { Ok(()) } else { Err(Error::Server(format!("{code} {text}"))) }
}

/// Sends a command line and expects a reply of the class of `class`.
async fn cmd<S: AsyncRead + AsyncWrite + Unpin>(io: &mut BufReader<S>, line: &str, class: u16) -> Result<()> {
    io.get_mut().write_all(format!("{line}\r\n").as_bytes()).await?;
    io.get_mut().flush().await?;
    expect(io, class).await
}

/// Dot-stuffs a message for DATA (lines starting with "." get another "."), CRLF-terminated.
#[must_use]
pub fn dot_stuff(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 64 + 2);
    let mut line_start = true;
    for &b in data {
        if line_start && b == b'.' {
            out.push(b'.');
        }
        out.push(b);
        line_start = b == b'\n';
    }
    if !out.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Runs an authenticated session on an already-encrypted stream and delivers `data`.
///
/// # Errors
/// Rejected credentials, sender or recipients, or I/O failure.
pub async fn session<S: AsyncRead + AsyncWrite + Unpin>(
    io: &mut BufReader<S>,
    creds: &Credentials,
    rcpts: &[String],
    data: &[u8],
) -> Result<()> {
    cmd(io, "EHLO kapa", 250).await?;
    let token = base64(format!("\0{}\0{}", creds.address, creds.password).as_bytes());
    cmd(io, &format!("AUTH PLAIN {token}"), 235).await?;
    cmd(io, &format!("MAIL FROM:<{}>", creds.address), 250).await?;
    for rcpt in rcpts {
        cmd(io, &format!("RCPT TO:<{rcpt}>"), 250).await?;
    }
    cmd(io, "DATA", 354).await?;
    io.get_mut().write_all(&dot_stuff(data)).await?;
    cmd(io, ".", 250).await?;
    let _ = cmd(io, "QUIT", 221).await;
    Ok(())
}

/// Delivers `data` from the account `creds` to `rcpts` through `server` (STARTTLS).
///
/// # Errors
/// Connection, TLS, authentication or delivery failure.
pub async fn send(server: &Server, creds: &Credentials, rcpts: &[String], data: &[u8]) -> Result<()> {
    let mut plain = BufReader::new(imap::tcp(&server.host, server.port).await?);
    expect(&mut plain, 220).await?;
    cmd(&mut plain, "EHLO kapa", 250).await?;
    cmd(&mut plain, "STARTTLS", 220).await?;
    let mut secure = BufReader::new(imap::tls(plain.into_inner(), &server.host).await?);
    session(&mut secure, creds, rcpts, data).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, duplex};

    #[test]
    fn dot_stuffing() {
        assert_eq!(dot_stuff(b".a\r\nb\r\n..c\r\n"), b"..a\r\nb\r\n...c\r\n");
        assert_eq!(dot_stuff(b"no newline"), b"no newline\r\n");
    }

    #[tokio::test]
    async fn authenticated_delivery() {
        let (client, srv) = duplex(1 << 16);
        let server = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(srv);
            let mut read = BufReader::new(read);
            let mut transcript = Vec::new();
            for reply in [
                "250-smtp.gmail.com\r\n250-AUTH PLAIN\r\n250 SMTPUTF8\r\n",
                "235 ok\r\n",
                "250 ok\r\n",
                "250 ok\r\n",
                "250 ok\r\n",
                "354 go\r\n",
            ] {
                let mut line = String::new();
                read.read_line(&mut line).await.unwrap();
                transcript.push(line);
                write.write_all(reply.as_bytes()).await.unwrap();
            }
            let mut data = Vec::new();
            while !data.ends_with(b"\r\n.\r\n") {
                let mut byte = [0u8];
                read.read_exact(&mut byte).await.unwrap();
                data.push(byte[0]);
            }
            write.write_all(b"250 queued\r\n221 bye\r\n").await.unwrap();
            (transcript, data)
        });
        let creds = Credentials { address: "bot@kapalua.es".into(), password: "pw".into() };
        let mut io = BufReader::new(client);
        session(&mut io, &creds, &["a@x.y".into(), "b@x.y".into()], b"Subject: s\r\n\r\n.dot\r\n").await.unwrap();
        let (transcript, data) = server.await.unwrap();
        assert_eq!(transcript[1], format!("AUTH PLAIN {}\r\n", base64(b"\0bot@kapalua.es\0pw")));
        assert_eq!(transcript[2], "MAIL FROM:<bot@kapalua.es>\r\n");
        assert_eq!(&transcript[3..5], ["RCPT TO:<a@x.y>\r\n", "RCPT TO:<b@x.y>\r\n"]);
        assert_eq!(data, b"Subject: s\r\n\r\n..dot\r\n.\r\n");
    }

    #[tokio::test]
    async fn rejection_is_an_error() {
        let (client, srv) = duplex(1 << 12);
        tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(srv);
            let mut read = BufReader::new(read);
            for reply in ["250 hi\r\n", "535 5.7.8 Username and Password not accepted\r\n"] {
                let mut line = String::new();
                read.read_line(&mut line).await.unwrap();
                write.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let creds = Credentials { address: "a@b.c".into(), password: "x".into() };
        let err = session(&mut BufReader::new(client), &creds, &["d@e.f".into()], b"x").await.unwrap_err();
        assert!(err.to_string().contains("535"), "{err}");
    }
}
