//! Secrets files: `accounts.*.env` (the Gmail accounts) and `admin.*.env` (the Kapa admin
//! mailbox, kept apart). Format: `KEY=VALUE` lines, `#` comments, one `ACCOUNT=` per account.

use crate::{Error, Result};
use std::fmt;

/// Credentials of one mailbox. The password never appears in `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// Email address, lowercase (also the login).
    pub address: String,
    /// Google app password, spaces removed.
    pub password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials").field("address", &self.address).field("password", &"<redacted>").finish()
    }
}

/// A mail server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    /// Host name (also used to verify the TLS certificate).
    pub host: String,
    /// TCP port.
    pub port: u16,
}

/// Contents of `accounts.*.env`.
#[derive(Debug, PartialEq, Eq)]
pub struct Accounts {
    /// Accounts that receive and send.
    pub accounts: Vec<Credentials>,
    /// SMTP server (STARTTLS).
    pub smtp: Server,
    /// IMAP server (implicit TLS).
    pub imap: Server,
}

/// Parses `accounts.*.env`; servers default to Gmail.
///
/// # Errors
/// Malformed lines, unknown keys, invalid ports, accounts without password or listed twice.
pub fn parse_accounts(text: &str) -> Result<Accounts> {
    let mut out = Accounts {
        accounts: Vec::new(),
        smtp: Server { host: "smtp.gmail.com".into(), port: 587 },
        imap: Server { host: "imap.gmail.com".into(), port: 993 },
    };
    for (n, key, value) in entries(text)? {
        apply(&mut out, n, key, value)?;
    }
    Ok(out)
}

/// Applies one `KEY=VALUE` line of `accounts.*.env`.
fn apply(out: &mut Accounts, n: usize, key: &str, value: &str) -> Result<()> {
    let port = |what| value.parse().map_err(|_| invalid(n, what));
    match key {
        "ACCOUNT" => out.accounts.push(account(&out.accounts, n, value)?),
        "SMTP_SERVER" => value.clone_into(&mut out.smtp.host),
        "IMAP_SERVER" => value.clone_into(&mut out.imap.host),
        "SMTP_PORT" => out.smtp.port = port("invalid SMTP_PORT")?,
        "IMAP_PORT" => out.imap.port = port("invalid IMAP_PORT")?,
        other => return Err(invalid(n, &format!("unknown key {other}"))),
    }
    Ok(())
}

/// Parses an `ACCOUNT=<address>:<app password>` value, rejecting duplicates of `known`.
fn account(known: &[Credentials], n: usize, value: &str) -> Result<Credentials> {
    let (address, password) = value.split_once(':').ok_or_else(|| invalid(n, "ACCOUNT=<address>:<app password>"))?;
    let account = credentials(n, address, password)?;
    if known.iter().any(|a| a.address == account.address) {
        return Err(invalid(n, "account listed twice"));
    }
    Ok(account)
}

/// Parses `admin.*.env` (`ADMIN_ADDRESS` and `ADMIN_APP_PASSWORD`, both required).
///
/// # Errors
/// Malformed lines, unknown keys or missing values.
pub fn parse_admin(text: &str) -> Result<Credentials> {
    let (mut address, mut password) = (None, None);
    for (n, key, value) in entries(text)? {
        match key {
            "ADMIN_ADDRESS" => address = Some(value),
            "ADMIN_APP_PASSWORD" => password = Some(value),
            other => return Err(invalid(n, &format!("unknown key {other}"))),
        }
    }
    match (address, password) {
        (Some(a), Some(p)) => credentials(0, a, p),
        _ => Err(Error::Invalid("admin file needs ADMIN_ADDRESS and ADMIN_APP_PASSWORD".into())),
    }
}

/// Splits a file into `(line number, key, value)`, skipping blank lines and comments.
fn entries(text: &str) -> Result<Vec<(usize, &str, &str)>> {
    text.lines()
        .enumerate()
        .map(|(i, line)| (i + 1, line.trim()))
        .filter(|(_, line)| !line.is_empty() && !line.starts_with('#'))
        .map(|(n, line)| line.split_once('=').map(|(k, v)| (n, k.trim(), v.trim())).ok_or_else(|| invalid(n, "expected KEY=VALUE")))
        .collect()
}

/// Validates an address and an app password (whitespace removed).
fn credentials(n: usize, address: &str, password: &str) -> Result<Credentials> {
    let address = address.trim().to_ascii_lowercase();
    let password: String = password.chars().filter(|c| !c.is_whitespace()).collect();
    if !address.contains('@') || address.contains(char::is_whitespace) || password.is_empty() {
        return Err(invalid(n, "expected <address> and <app password>"));
    }
    Ok(Credentials { address, password })
}

/// Builds a configuration error for line `n` (0 = whole file).
fn invalid(n: usize, what: &str) -> Error {
    Error::Invalid(if n == 0 { what.to_owned() } else { format!("line {n}: {what}") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_accounts_and_defaults() {
        let cfg = parse_accounts("# c\n\nACCOUNT=Bot@Kapalua.es:abcd efgh ijkl mnop\nACCOUNT=x@y.z:pw\nIMAP_PORT=1993\n").unwrap();
        assert_eq!(cfg.accounts.len(), 2);
        assert_eq!(cfg.accounts[0].address, "bot@kapalua.es");
        assert_eq!(cfg.accounts[0].password, "abcdefghijklmnop");
        assert_eq!(cfg.smtp, Server { host: "smtp.gmail.com".into(), port: 587 });
        assert_eq!(cfg.imap.port, 1993);
        assert!(parse_accounts("").unwrap().accounts.is_empty());
    }

    #[test]
    fn rejects_bad_accounts() {
        for bad in [
            "ACCOUNT=nopassword@x.y",
            "ACCOUNT=noat:pw",
            "ACCOUNT=a@b.c:",
            "OTHER=1",
            "SMTP_PORT=99999",
            "garbage",
            "ACCOUNT=a@b.c:p\nACCOUNT=A@B.C:q",
        ] {
            assert!(parse_accounts(bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn parses_admin() {
        let admin = parse_admin("ADMIN_ADDRESS=bot@kapalua.es\nADMIN_APP_PASSWORD=ab cd\n").unwrap();
        assert_eq!(admin, Credentials { address: "bot@kapalua.es".into(), password: "abcd".into() });
        assert!(parse_admin("ADMIN_ADDRESS=bot@kapalua.es\n").is_err());
        assert!(parse_admin("ACCOUNT=a@b.c:p\n").is_err());
    }

    #[test]
    fn debug_never_shows_passwords() {
        let creds = Credentials { address: "a@b.c".into(), password: "secretpassword".into() };
        let shown = format!("{creds:?} {:?}", parse_accounts("ACCOUNT=a@b.c:secretpassword").unwrap());
        assert!(!shown.contains("secretpassword") && shown.contains("<redacted>"));
    }
}
