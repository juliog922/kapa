//! Email messages: building outgoing ones (RFC 5322 + MIME) and turning incoming ones into
//! an [`Email`] with the full envelope. Small encoders (base64, dates) live here instead of
//! extra crates.

use crate::{Error, Result};
use mail_parser::MessageParser;
use ring::digest::{SHA256, digest};
use std::fmt::Write as _;

/// Base64 alphabet (RFC 4648).
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
/// Day names, Sunday first.
const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
/// Month names.
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// An attachment to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    /// File name shown to the recipient.
    pub filename: String,
    /// MIME type, e.g. `application/pdf`.
    pub mime: String,
    /// File bytes.
    pub data: Vec<u8>,
}

/// A message to send from one account.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outgoing {
    /// Recipients.
    pub to: Vec<String>,
    /// Carbon-copy recipients.
    pub cc: Vec<String>,
    /// Subject (any UTF-8).
    pub subject: String,
    /// Plain-text body (any UTF-8).
    pub text: String,
    /// Attachments.
    pub attachments: Vec<Attachment>,
    /// Message-ID being replied to (threads the reply), if any.
    pub in_reply_to: Option<String>,
}

/// A mailbox: optional display name and address.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mailbox {
    /// Display name, if any.
    pub name: Option<String>,
    /// Address, lowercase.
    pub address: String,
}

/// An attachment received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceivedAttachment {
    /// File name, if any.
    pub filename: Option<String>,
    /// MIME type.
    pub mime: String,
    /// Decoded bytes.
    pub data: Vec<u8>,
    /// SHA-256 of `data`.
    pub sha256: [u8; 32],
}

/// An incoming email with everything needed to process it later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Email {
    /// Account (address) whose inbox received it.
    pub account: String,
    /// IMAP UID in the account's INBOX.
    pub uid: u32,
    /// Gmail message ID (stable across folders and sessions).
    pub gmail_id: u64,
    /// Gmail thread ID.
    pub thread_id: u64,
    /// `Message-ID` header (without angle brackets), empty if missing.
    pub message_id: String,
    /// Sender.
    pub from: Mailbox,
    /// Recipients.
    pub to: Vec<Mailbox>,
    /// Carbon-copy recipients.
    pub cc: Vec<Mailbox>,
    /// Reply-To addresses.
    pub reply_to: Vec<Mailbox>,
    /// Subject, decoded.
    pub subject: String,
    /// `In-Reply-To` message IDs.
    pub in_reply_to: Vec<String>,
    /// `References` message IDs.
    pub references: Vec<String>,
    /// `Date` header (sender's clock), Unix seconds, if present.
    pub sent_at: Option<i64>,
    /// When the mail server received it (IMAP INTERNALDATE), Unix seconds.
    pub internal_at: i64,
    /// When core received it, Unix milliseconds.
    pub received_at_ms: i64,
    /// Plain-text body (converted from HTML if there is no text part).
    pub text: Option<String>,
    /// HTML body, if any.
    pub html: Option<String>,
    /// Attachments, decoded.
    pub attachments: Vec<ReceivedAttachment>,
    /// Size of the raw message in bytes.
    pub size: usize,
}

/// Base64-encodes `data` on one line.
#[must_use]
pub fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    base64_into(&mut out, data);
    out
}

/// Appends the base64 of `data` to `out` (no allocation besides growing `out`).
fn base64_into(out: &mut String, data: &[u8]) {
    for chunk in data.chunks(3) {
        let b = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            out.push(if i <= chunk.len() { char::from(B64[(n >> (18 - 6 * i) & 63) as usize]) } else { '=' });
        }
    }
}

/// Base64-encodes `data` in CRLF-terminated lines of 76 characters (MIME bodies).
#[must_use]
pub fn base64_lines(data: &[u8]) -> String {
    let mut out = String::with_capacity(base64_lines_len(data.len()));
    base64_lines_into(&mut out, data);
    out
}

/// Length of [`base64_lines`] output for `n` input bytes.
const fn base64_lines_len(n: usize) -> usize {
    n.div_ceil(3) * 4 + n.div_ceil(57) * 2
}

/// Appends `data` as CRLF-terminated base64 lines of 76 characters to `out`.
fn base64_lines_into(out: &mut String, data: &[u8]) {
    for chunk in data.chunks(57) {
        base64_into(out, chunk);
        out.push_str("\r\n");
    }
}

/// Encodes a header value as an RFC 2047 encoded-word when it is not plain ASCII.
#[must_use]
pub fn encode_header(value: &str) -> String {
    if value.bytes().all(|b| (0x20..0x7f).contains(&b)) { value.to_owned() } else { format!("=?UTF-8?B?{}?=", base64(value.as_bytes())) }
}

/// Converts days since 1970-01-01 to (year, month, day) (proleptic Gregorian).
pub(crate) fn civil(days: i64) -> (i64, usize, i64) {
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, usize::try_from(month).unwrap_or(1), doy - (153 * mp + 2) / 5 + 1)
}

/// Converts (year, month 1-12, day) to days since 1970-01-01 (inverse of [`civil`]).
pub(crate) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let (era, yoe) = (y.div_euclid(400), y.rem_euclid(400));
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + (153 * mp + 2) / 5 + day - 1;
    era * 146_097 + doe - 719_468
}

/// Formats Unix seconds as an RFC 5322 date in UTC, e.g. `Thu, 01 Jan 1970 00:00:00 +0000`.
#[must_use]
pub fn rfc2822(unix: i64) -> String {
    let (days, secs) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    let (y, m, d) = civil(days);
    let weekday = DAYS[usize::try_from((days + 4).rem_euclid(7)).unwrap_or(0)];
    format!("{weekday}, {d:02} {} {y} {:02}:{:02}:{:02} +0000", MONTHS[m - 1], secs / 3600, secs % 3600 / 60, secs % 60)
}

/// Formats Unix seconds as RFC 3339 in UTC, e.g. `1970-01-01T00:00:00Z` (logs).
#[must_use]
pub fn rfc3339(unix: i64) -> String {
    let (days, secs) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, secs % 3600 / 60, secs % 60)
}

/// Parses an IMAP INTERNALDATE such as `24-Oct-2026 16:45:02 +0200` into Unix seconds.
#[must_use]
pub fn parse_internal_date(text: &str) -> Option<i64> {
    let mut fields = text.split_whitespace();
    let (days, secs, offset) = (date_days(fields.next()?)?, time_secs(fields.next()?)?, zone_secs(fields.next()?)?);
    Some(days * 86_400 + secs - offset)
}

/// Days since 1970-01-01 of a `dd-Mon-yyyy` date.
fn date_days(date: &str) -> Option<i64> {
    let mut d = date.split('-');
    let (day, month_name, year) = (d.next()?.parse().ok()?, d.next()?, d.next()?.parse().ok()?);
    let month = MONTHS.iter().position(|m| *m == month_name)?;
    Some(days_from_civil(year, i64::try_from(month).ok()? + 1, day))
}

/// Seconds since midnight of a `hh:mm:ss` time.
fn time_secs(time: &str) -> Option<i64> {
    time.split(':').map(str::parse::<i64>).try_fold(0, |acc, part| part.ok().map(|v| acc * 60 + v))
}

/// Seconds east of UTC of a `+hhmm` / `-hhmm` zone.
fn zone_secs(zone: &str) -> Option<i64> {
    let hhmm: i64 = zone.get(1..)?.parse().ok()?;
    let sign = if zone.starts_with('-') { -1 } else { 1 };
    Some(sign * (hhmm / 100 * 3600 + hhmm % 100 * 60))
}

/// Generates a unique `Message-ID` (without angle brackets) in the sender's domain.
///
/// # Errors
/// The system random generator failed.
pub fn new_message_id(from: &str) -> Result<String> {
    let mut random = [0u8; 12];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut random).map_err(|_| Error::Invalid("no randomness".into()))?;
    let domain = from.rsplit_once('@').map_or("kapa.local", |(_, d)| d);
    let mut id = format!("{}.", crate::now_ms());
    for b in random {
        let _ = write!(id, "{b:02x}");
    }
    Ok(format!("{id}@{domain}"))
}

/// Rejects header values that could inject headers (CR or LF) or are empty when required.
fn header_safe(what: &str, value: &str) -> Result<()> {
    if value.contains(['\r', '\n']) {
        return Err(Error::Invalid(format!("{what} must not contain line breaks")));
    }
    Ok(())
}

/// Validates an outgoing message: recipients look like addresses, no header injection.
///
/// # Errors
/// No recipients, invalid addresses, or line breaks in headers.
pub fn validate(out: &Outgoing) -> Result<()> {
    if out.to.is_empty() && out.cc.is_empty() {
        return Err(Error::Invalid("at least one recipient is required".into()));
    }
    for address in out.to.iter().chain(&out.cc) {
        if !address.contains('@') || address.contains([' ', '<', '>', ',', '\r', '\n']) {
            return Err(Error::Invalid(format!("invalid recipient {address:?}")));
        }
    }
    header_safe("subject", &out.subject)?;
    out.attachments.iter().try_for_each(|a| header_safe("attachment name", &a.filename).and(header_safe("attachment type", &a.mime)))?;
    out.in_reply_to.as_deref().map_or(Ok(()), |id| header_safe("in_reply_to", id))
}

/// Builds the RFC 5322 bytes of `out`, sent by `from` at `date` (Unix seconds).
#[must_use]
pub fn build(from: &str, out: &Outgoing, date: i64, message_id: &str) -> Vec<u8> {
    let bodies = base64_lines_len(out.text.len()) + out.attachments.iter().map(|a| base64_lines_len(a.data.len()) + 256).sum::<usize>();
    let mut m = String::with_capacity(bodies + 1024);
    let _ = write!(m, "From: {from}\r\nDate: {}\r\nMessage-ID: <{message_id}>\r\n", rfc2822(date));
    if !out.to.is_empty() {
        let _ = write!(m, "To: {}\r\n", out.to.join(", "));
    }
    if !out.cc.is_empty() {
        let _ = write!(m, "Cc: {}\r\n", out.cc.join(", "));
    }
    let _ = write!(m, "Subject: {}\r\nMIME-Version: 1.0\r\n", encode_header(&out.subject));
    if let Some(id) = &out.in_reply_to {
        let _ = write!(m, "In-Reply-To: <{id}>\r\nReferences: <{id}>\r\n");
    }
    let boundary = format!("kapa-{message_id}");
    if !out.attachments.is_empty() {
        let _ = write!(m, "Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n--{boundary}\r\n");
    }
    m.push_str("Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n");
    base64_lines_into(&mut m, out.text.as_bytes());
    if out.attachments.is_empty() {
        return m.into_bytes();
    }
    for a in &out.attachments {
        let name = encode_header(&a.filename);
        let _ = write!(
            m,
            "--{boundary}\r\nContent-Type: {}; name=\"{name}\"\r\nContent-Disposition: attachment; filename=\"{name}\"\r\nContent-Transfer-Encoding: base64\r\n\r\n",
            a.mime
        );
        base64_lines_into(&mut m, &a.data);
    }
    let _ = write!(m, "--{boundary}--\r\n");
    m.into_bytes()
}

/// Converts a parsed mailbox list into [`Mailbox`]es.
fn mailboxes(list: Option<mail_parser::AddressList<'_>>) -> Vec<Mailbox> {
    list.map(|l| {
        l.mailboxes()
            .filter_map(|mb| Some(Mailbox { name: mb.name().map(str::to_owned), address: mb.address()?.to_ascii_lowercase() }))
            .collect()
    })
    .unwrap_or_default()
}

/// Copies a list of message IDs.
fn ids(list: Option<mail_parser::TextList<'_>>) -> Vec<String> {
    list.map(|l| l.iter().map(str::to_owned).collect()).unwrap_or_default()
}

/// Turns raw message bytes received by `account` into an [`Email`] (`None` if not a message).
#[must_use]
pub fn parse(account: &str, uid: u32, gmail_id: u64, thread_id: u64, internal_at: i64, raw: &[u8]) -> Option<Email> {
    let msg = MessageParser::new().parse(raw)?;
    let attachments = msg
        .attachments()
        .map(|part| {
            let data = part.decoded().into_owned();
            let mime = part.content_type().map_or_else(
                || "application/octet-stream".to_owned(),
                |ct| format!("{}/{}", ct.ctype(), ct.subtype().unwrap_or("octet-stream")),
            );
            let sha256 = digest(&SHA256, &data).as_ref().try_into().unwrap_or([0; 32]);
            ReceivedAttachment { filename: part.attachment_name().map(str::to_owned), mime, data, sha256 }
        })
        .collect();
    Some(Email {
        account: account.to_owned(),
        uid,
        gmail_id,
        thread_id,
        message_id: msg.message_id().unwrap_or_default().to_owned(),
        from: mailboxes(msg.from()).into_iter().next().unwrap_or_default(),
        to: mailboxes(msg.to()),
        cc: mailboxes(msg.cc()),
        reply_to: mailboxes(msg.reply_to()),
        subject: msg.subject().unwrap_or_default().to_owned(),
        in_reply_to: ids(msg.in_reply_to()),
        references: ids(msg.references()),
        sent_at: msg.date().map(|d| d.to_timestamp()),
        internal_at,
        received_at_ms: crate::now_ms(),
        text: msg.body_text(0).map(std::borrow::Cow::into_owned),
        html: if msg.html_body().any(|p| p.is_content_type("text", "html")) {
            msg.body_html(0).map(std::borrow::Cow::into_owned)
        } else {
            None
        },
        attachments,
        size: raw.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        let cases =
            [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")];
        for (plain, encoded) in cases {
            assert_eq!(base64(plain.as_bytes()), encoded);
        }
        for n in [0, 1, 56, 57, 58, 100, 1000] {
            let lines = base64_lines(&vec![0xffu8; n]);
            assert!(lines.lines().all(|l| l.len() <= 76) && (n == 0 || lines.ends_with("\r\n")));
            assert_eq!(lines.len(), base64_lines_len(n), "exact capacity for {n} bytes");
        }
    }

    #[test]
    fn dates_round_trip() {
        assert_eq!(rfc2822(0), "Thu, 01 Jan 1970 00:00:00 +0000");
        assert_eq!(rfc2822(951_782_400), "Tue, 29 Feb 2000 00:00:00 +0000");
        // Expected values computed independently (Python datetime).
        assert_eq!(rfc2822(1_792_840_002), "Sat, 24 Oct 2026 11:06:42 +0000");
        assert_eq!(rfc3339(1_792_840_002), "2026-10-24T11:06:42Z");
        assert_eq!(parse_internal_date("24-Oct-2026 11:06:42 +0000"), Some(1_792_840_002));
        assert_eq!(parse_internal_date(" 4-Jan-2026 10:00:00 +0200"), Some(1_767_513_600));
        assert_eq!(parse_internal_date("04-Jan-2026 10:00:00 -0130"), Some(1_767_526_200));
        assert_eq!(parse_internal_date("garbage"), None);
        for unix in [0_i64, 86_399, 951_868_799, 4_102_444_800, -1] {
            let (y, m, d) = civil(unix.div_euclid(86_400));
            assert_eq!(days_from_civil(y, i64::try_from(m).unwrap(), d), unix.div_euclid(86_400));
        }
    }

    #[test]
    fn built_messages_parse_back_identically() {
        let out = Outgoing {
            to: vec!["ana@example.com".into(), "bob@example.com".into()],
            cc: vec!["cc@example.com".into()],
            subject: "Factura nº 7 — ¡gracias!".into(),
            text: "Hola,\r\n.\r\nlínea con punto inicial\r\n".into(),
            attachments: vec![Attachment {
                filename: "factura ñ.pdf".into(), mime: "application/pdf".into(), data: (0u8..=255).collect()
            }],
            in_reply_to: Some("orig@example.com".into()),
        };
        let raw = build("bot@kapalua.es", &out, 1_792_840_002, "id1@kapalua.es");
        let got = parse("bot@kapalua.es", 7, 11, 22, 1_792_840_000, &raw).unwrap();
        assert_eq!(got.subject, out.subject);
        assert_eq!(got.from.address, "bot@kapalua.es");
        assert_eq!(got.to.iter().map(|m| m.address.as_str()).collect::<Vec<_>>(), ["ana@example.com", "bob@example.com"]);
        assert_eq!(got.cc[0].address, "cc@example.com");
        assert_eq!(got.message_id, "id1@kapalua.es");
        assert_eq!(got.in_reply_to, ["orig@example.com"]);
        assert_eq!(got.sent_at, Some(1_792_840_002));
        assert_eq!(got.text.as_deref().map(|t| t.replace("\r\n", "\n")), Some(out.text.replace("\r\n", "\n")));
        assert_eq!(got.attachments.len(), 1);
        let a = &got.attachments[0];
        assert_eq!((a.filename.as_deref(), a.mime.as_str(), &a.data), (Some("factura ñ.pdf"), "application/pdf", &out.attachments[0].data));
        assert_eq!(a.sha256.as_slice(), digest(&SHA256, &a.data).as_ref());
        assert_eq!((got.uid, got.gmail_id, got.thread_id, got.internal_at, got.size), (7, 11, 22, 1_792_840_000, raw.len()));
        assert!(got.html.is_none());
    }

    #[test]
    fn parses_html_and_names() {
        let raw = b"From: \"Ana Ruiz\" <Ana@Example.com>\r\nReply-To: r@example.com\r\nSubject: hi\r\nContent-Type: text/html\r\n\r\n<p>Hola <b>Ana</b></p>\r\n";
        let got = parse("bot@kapalua.es", 1, 1, 1, 0, raw).unwrap();
        assert_eq!(got.from, Mailbox { name: Some("Ana Ruiz".into()), address: "ana@example.com".into() });
        assert_eq!(got.reply_to[0].address, "r@example.com");
        assert!(got.html.unwrap().contains("<b>Ana</b>"));
        assert!(got.text.unwrap().contains("Hola"));
        assert!(got.sent_at.is_none() && got.message_id.is_empty());
    }

    #[test]
    fn validation_blocks_injection() {
        let ok = Outgoing { to: vec!["a@b.c".into()], subject: "s".into(), ..Outgoing::default() };
        assert!(validate(&ok).is_ok());
        for bad in [
            Outgoing { to: vec![], ..ok.clone() },
            Outgoing { to: vec!["no-at".into()], ..ok.clone() },
            Outgoing { to: vec!["a@b.c>\r\nBcc: x@y.z".into()], ..ok.clone() },
            Outgoing { subject: "s\r\nBcc: x@y.z".into(), ..ok.clone() },
            Outgoing { in_reply_to: Some("x\ny".into()), ..ok.clone() },
        ] {
            assert!(validate(&bad).is_err(), "{bad:?}");
        }
        assert_eq!(encode_header("plain"), "plain");
        assert!(encode_header("ñ").starts_with("=?UTF-8?B?"));
        let id = new_message_id("bot@kapalua.es").unwrap();
        assert!(id.ends_with("@kapalua.es") && id != new_message_id("bot@kapalua.es").unwrap());
    }
}
