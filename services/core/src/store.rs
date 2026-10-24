//! PostgreSQL persistence (sqlx, no compile-time macros): allowlists, IMAP cursors and the
//! received emails with their attachments. Tables are prefixed `gmail_`.

use crate::{Error, Result, mail::Email};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use std::time::Duration;

/// Schema, created idempotently at startup.
const SCHEMA: [&str; 4] = [
    "CREATE TABLE IF NOT EXISTS gmail_allowed_senders (
        account TEXT NOT NULL, sender TEXT NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
        PRIMARY KEY (account, sender))",
    "CREATE TABLE IF NOT EXISTS gmail_cursors (
        account TEXT PRIMARY KEY, uid_validity BIGINT NOT NULL, last_uid BIGINT NOT NULL,
        updated_at TIMESTAMPTZ NOT NULL DEFAULT now())",
    "CREATE TABLE IF NOT EXISTS gmail_messages (
        account TEXT NOT NULL, gmail_id BIGINT NOT NULL, thread_id BIGINT NOT NULL, uid BIGINT NOT NULL,
        message_id TEXT NOT NULL, from_address TEXT NOT NULL, from_name TEXT,
        to_addresses TEXT[] NOT NULL, cc_addresses TEXT[] NOT NULL, reply_to TEXT[] NOT NULL,
        subject TEXT NOT NULL, in_reply_to TEXT[] NOT NULL, refs TEXT[] NOT NULL,
        sent_at TIMESTAMPTZ, internal_at TIMESTAMPTZ NOT NULL, received_at TIMESTAMPTZ NOT NULL,
        text_body TEXT, html_body TEXT, size BIGINT NOT NULL,
        PRIMARY KEY (account, gmail_id))",
    "CREATE TABLE IF NOT EXISTS gmail_attachments (
        account TEXT NOT NULL, gmail_id BIGINT NOT NULL, idx INT NOT NULL,
        filename TEXT, mime TEXT NOT NULL, size BIGINT NOT NULL, sha256 BYTEA NOT NULL, data BYTEA NOT NULL,
        PRIMARY KEY (account, gmail_id, idx),
        FOREIGN KEY (account, gmail_id) REFERENCES gmail_messages ON DELETE CASCADE)",
];

/// Advisory lock key serializing schema migrations of core (ASCII "kapacore").
const MIGRATION_LOCK: i64 = 0x6b61_7061_636f_7265;

/// Connection pool to the core tables.
#[derive(Clone)]
pub struct Store {
    /// Shared pool.
    pool: PgPool,
}

/// Converts an unsigned id to PostgreSQL's BIGINT.
fn big(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Invalid(format!("id {value} exceeds BIGINT")))
}

/// Addresses of a mailbox list.
fn addresses(list: &[crate::mail::Mailbox]) -> Vec<String> {
    list.iter().map(|m| m.address.clone()).collect()
}

impl Store {
    /// Connects using `url`, or the standard `PG*` environment variables when `None`.
    ///
    /// # Errors
    /// Invalid URL or database unreachable.
    pub async fn connect(url: Option<&str>) -> Result<Self> {
        let options = match url {
            Some(url) => url.parse::<PgConnectOptions>()?,
            None => PgConnectOptions::new(),
        };
        let pool = PgPoolOptions::new().max_connections(8).acquire_timeout(Duration::from_secs(15)).connect_with(options).await?;
        Ok(Self { pool })
    }

    /// The underlying pool (other core modules and tests).
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Creates the tables if they do not exist. `CREATE ... IF NOT EXISTS` is not safe when
    /// run concurrently (two sessions both create, one fails on the catalog), so the whole
    /// schema runs in one transaction serialized by a transaction-scoped advisory lock.
    ///
    /// # Errors
    /// Database failure.
    pub async fn migrate(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(MIGRATION_LOCK).execute(&mut *tx).await?;
        for statement in SCHEMA {
            sqlx::query(statement).execute(&mut *tx).await?;
        }
        Ok(tx.commit().await?)
    }

    /// Every `(account, sender)` allowlist entry.
    ///
    /// # Errors
    /// Database failure.
    pub async fn allowlist(&self) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query_as("SELECT account, sender FROM gmail_allowed_senders").fetch_all(&self.pool).await?)
    }

    /// Adds an allowlist entry (idempotent).
    ///
    /// # Errors
    /// Database failure.
    pub async fn allow(&self, account: &str, sender: &str) -> Result<()> {
        sqlx::query("INSERT INTO gmail_allowed_senders (account, sender) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(account)
            .bind(sender)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Removes an allowlist entry (idempotent).
    ///
    /// # Errors
    /// Database failure.
    pub async fn revoke(&self, account: &str, sender: &str) -> Result<()> {
        sqlx::query("DELETE FROM gmail_allowed_senders WHERE account = $1 AND sender = $2")
            .bind(account)
            .bind(sender)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The saved IMAP position `(UIDVALIDITY, last UID)` of an account.
    ///
    /// # Errors
    /// Database failure.
    pub async fn cursor(&self, account: &str) -> Result<Option<(u32, u32)>> {
        let row: Option<(i64, i64)> = sqlx::query_as("SELECT uid_validity, last_uid FROM gmail_cursors WHERE account = $1")
            .bind(account)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|(v, u)| Some((u32::try_from(v).ok()?, u32::try_from(u).ok()?))))
    }

    /// Saves the IMAP position of an account.
    ///
    /// # Errors
    /// Database failure.
    pub async fn set_cursor(&self, account: &str, validity: u32, last_uid: u32) -> Result<()> {
        sqlx::query(
            "INSERT INTO gmail_cursors (account, uid_validity, last_uid) VALUES ($1, $2, $3)
             ON CONFLICT (account) DO UPDATE SET uid_validity = $2, last_uid = $3, updated_at = now()",
        )
        .bind(account)
        .bind(i64::from(validity))
        .bind(i64::from(last_uid))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Stores an email and its attachments in one transaction. Returns `false` when it was
    /// already stored (a replay after a reconnect), so it is not published twice.
    ///
    /// # Errors
    /// Database failure.
    pub async fn save(&self, e: &Email) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let inserted = insert_message(&mut tx, e).await?;
        if inserted {
            insert_attachments(&mut tx, e).await?;
        }
        tx.commit().await?;
        Ok(inserted)
    }
}

/// Inserts the message row; `false` when it already exists.
async fn insert_message(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, e: &Email) -> Result<bool> {
    Ok(sqlx::query(
        "INSERT INTO gmail_messages (account, gmail_id, thread_id, uid, message_id, from_address, from_name,
                to_addresses, cc_addresses, reply_to, subject, in_reply_to, refs, sent_at, internal_at, received_at,
                text_body, html_body, size)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, to_timestamp($14), to_timestamp($15),
                to_timestamp($16 / 1000.0), $17, $18, $19)
             ON CONFLICT DO NOTHING",
    )
    .bind(&e.account)
    .bind(big(e.gmail_id)?)
    .bind(big(e.thread_id)?)
    .bind(i64::from(e.uid))
    .bind(&e.message_id)
    .bind(&e.from.address)
    .bind(&e.from.name)
    .bind(addresses(&e.to))
    .bind(addresses(&e.cc))
    .bind(addresses(&e.reply_to))
    .bind(&e.subject)
    .bind(&e.in_reply_to)
    .bind(&e.references)
    .bind(e.sent_at)
    .bind(e.internal_at)
    .bind(e.received_at_ms)
    .bind(&e.text)
    .bind(&e.html)
    .bind(i64::try_from(e.size).unwrap_or(i64::MAX))
    .execute(&mut **tx)
    .await?
    .rows_affected()
        == 1)
}

/// Inserts the attachments of a new message.
async fn insert_attachments(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, e: &Email) -> Result<()> {
    for (idx, a) in (0i32..).zip(&e.attachments) {
        sqlx::query("INSERT INTO gmail_attachments (account, gmail_id, idx, filename, mime, size, sha256, data) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)")
            .bind(&e.account)
            .bind(big(e.gmail_id)?)
            .bind(idx)
            .bind(&a.filename)
            .bind(&a.mime)
            .bind(i64::try_from(a.data.len()).unwrap_or(i64::MAX))
            .bind(a.sha256.as_slice())
            .bind(&a.data)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}
