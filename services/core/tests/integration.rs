//! Integration tests. Always: a real PostgreSQL (`TEST_DATABASE_URL`). Live: when
//! `ACCOUNTS_FILE` names a file with at least one account, that Gmail account sends to itself
//! and core must receive, store and publish it (allowed) or drop it (revoked).

use kapa_core::config::{self, Accounts, Server};
use kapa_core::gmail::Gmail;
use kapa_core::mail::{self, Attachment, Email, Outgoing};
use kapa_core::{now_ms, store::Store};
use std::fmt::Write as _;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

/// Connects to the test database, or skips the test when it is not configured.
async fn store() -> Option<Store> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL not set: skipped");
        return None;
    };
    let store = Store::connect(Some(&url)).await.expect("test database reachable");
    store.migrate().await.expect("schema");
    Some(store)
}

/// Accounts configuration whose IMAP server refuses connections (no network involved).
fn offline(addresses: &[&str]) -> Accounts {
    let text = addresses.iter().fold(String::new(), |mut text, a| {
        let _ = writeln!(text, "ACCOUNT={a}:password");
        text
    });
    let mut cfg = config::parse_accounts(&text).expect("valid accounts");
    cfg.imap = Server { host: "127.0.0.1".into(), port: 1 };
    cfg
}

/// A unique id for test data.
fn unique() -> u64 {
    u64::try_from(now_ms()).expect("positive time") * 1000 + u64::from(std::process::id() % 1000)
}

/// Creates an empty database next to the test database and returns its URL.
async fn fresh_database(store: &Store) -> String {
    let name = format!("fresh_{}", unique());
    // The name is generated here (digits only), never user input.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}"))).execute(store.pool()).await.expect("create database");
    let url = std::env::var("TEST_DATABASE_URL").expect("checked by store()");
    format!("{}/{name}", url.rsplit_once('/').expect("URL with a database").0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_migrations_on_fresh_database() {
    let Some(store) = store().await else { return };
    let url = fresh_database(&store).await;
    let tasks: Vec<_> = (0..16)
        .map(|_| {
            let url = url.clone();
            tokio::spawn(async move { Store::connect(Some(&url)).await?.migrate().await })
        })
        .collect();
    for task in tasks {
        task.await.expect("task").expect("concurrent migrations must all succeed");
    }
}

#[tokio::test]
async fn store_round_trip() {
    let Some(store) = store().await else { return };
    store.migrate().await.expect("migrations are idempotent");
    let out = Outgoing {
        to: vec!["ana@example.com".into()],
        subject: "almacén".into(),
        text: "hola".into(),
        attachments: vec![Attachment { filename: "a.bin".into(), mime: "application/octet-stream".into(), data: vec![1, 2, 3] }],
        ..Outgoing::default()
    };
    let id = unique();
    let raw = mail::build("bot@kapalua.es", &out, 1_792_840_002, &format!("{id}@kapalua.es"));
    let email = mail::parse("store-test@kapalua.es", 9, id, id + 1, 1_792_840_003, &raw).expect("parses");
    assert!(store.save(&email).await.expect("save"), "first save inserts");
    assert!(!store.save(&email).await.expect("save again"), "replay is ignored");
    let (subject, sent, files, sha): (String, i64, i64, Vec<u8>) = sqlx::query_as(
        "SELECT m.subject, extract(epoch FROM m.sent_at)::bigint, count(a.idx), (array_agg(a.sha256))[1]
         FROM gmail_messages m JOIN gmail_attachments a USING (account, gmail_id)
         WHERE m.account = $1 AND m.gmail_id = $2 GROUP BY m.subject, m.sent_at",
    )
    .bind("store-test@kapalua.es")
    .bind(i64::try_from(id).expect("bigint"))
    .fetch_one(store.pool())
    .await
    .expect("stored row");
    assert_eq!((subject.as_str(), sent, files, sha.as_slice()), ("almacén", 1_792_840_002, 1, email.attachments[0].sha256.as_slice()));
    store.set_cursor("store-test@kapalua.es", 7, 41).await.expect("cursor");
    assert_eq!(store.cursor("store-test@kapalua.es").await.expect("cursor"), Some((7, 41)));
    assert_eq!(store.cursor("nobody@kapalua.es").await.expect("cursor"), None);
}

#[tokio::test]
async fn allowlist_persists_and_validates() {
    let Some(store) = store().await else { return };
    let account = format!("allow-{}@kapalua.es", unique());
    let gmail = Gmail::start(store.clone(), offline(&[&account])).await.expect("start");
    gmail.allow(&account, "Ana@Example.com ").await.expect("allow");
    gmail.allow(&account, "bob@example.com").await.expect("allow");
    gmail.allow(&account, "bob@example.com").await.expect("idempotent");
    gmail.revoke(&account, "bob@example.com").await.expect("revoke");
    assert!(gmail.allow("unknown@kapalua.es", "ana@example.com").await.is_err(), "unknown account");
    assert!(gmail.allow(&account, "not an address").await.is_err(), "invalid sender");
    let restarted = Gmail::start(store, offline(&[&account])).await.expect("restart");
    assert_eq!(restarted.allowed_senders(&account), ["ana@example.com"]);
    let snapshot = restarted.snapshot();
    assert_eq!(snapshot.accounts.len(), 1);
    assert_eq!(snapshot.accounts[0].2, 1, "allowed senders in the status");
}

#[tokio::test]
async fn sending_validates_before_connecting() {
    let Some(store) = store().await else { return };
    let gmail = Gmail::start(store, offline(&["bot@kapalua.es"])).await.expect("start");
    let ok = Outgoing { to: vec!["a@b.c".into()], subject: "s".into(), ..Outgoing::default() };
    assert!(gmail.send("nobody@kapalua.es", &ok).await.is_err(), "unknown sender account");
    let injected = Outgoing { subject: "s\r\nBcc: x@y.z".into(), ..ok };
    assert!(gmail.send("bot@kapalua.es", &injected).await.is_err(), "header injection");
    assert_eq!(gmail.stats().sent.load(Relaxed) + gmail.stats().send_failed.load(Relaxed), 0, "nothing reached SMTP");
}

/// Loads the live account from `ACCOUNTS_FILE`, or skips.
fn live_accounts() -> Option<Accounts> {
    let Ok(path) = std::env::var("ACCOUNTS_FILE") else {
        eprintln!("LIVE GMAIL SKIPPED: ACCOUNTS_FILE not set (no secrets/accounts.dev.env)");
        return None;
    };
    let mut cfg = config::parse_accounts(&std::fs::read_to_string(&path).ok()?).expect("valid accounts file");
    cfg.accounts.truncate(1);
    if cfg.accounts.is_empty() {
        eprintln!("LIVE GMAIL SKIPPED: {path} has no ACCOUNT");
        return None;
    }
    Some(cfg)
}

/// Waits until the account is watching its inbox.
async fn wait_watching(gmail: &Gmail) {
    for _ in 0..120 {
        if gmail.snapshot().accounts.iter().all(|(_, state, _)| state == "watching") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("account never reached 'watching': {:?}", gmail.snapshot().accounts);
}

/// A message from the account to itself, with a unique subject and attachment.
fn self_message(account: &str, nonce: u64) -> Outgoing {
    Outgoing {
        to: vec![account.to_owned()],
        subject: format!("kapa core test {nonce} — ñandú"),
        text: format!("Prueba {nonce}\r\nlínea 2\r\n"),
        attachments: vec![Attachment {
            filename: format!("prueba-{nonce}.bin"),
            mime: "application/octet-stream".into(),
            data: nonce.to_le_bytes().repeat(256),
        }],
        ..Outgoing::default()
    }
}

/// Prints the evidence that a sent message came back through IMAP.
fn report_received(account: &str, email: &Email, after: Duration) {
    let sha = email.attachments[0].sha256.iter().take(6).fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    });
    eprintln!(
        "live gmail [{account}] IMAP received it after {:.1}s: uid {} · gmail id {} · thread {} · {} bytes · attachment sha256 {sha}… ✔ envelope verified",
        after.as_secs_f64(),
        email.uid,
        email.gmail_id,
        email.thread_id,
        email.size
    );
}

/// Checks the received email carries the full envelope of what was sent.
fn assert_envelope(email: &Email, account: &str, sent: &Outgoing, sent_at_ms: i64) {
    let now = now_ms() / 1000;
    assert_eq!((email.account.as_str(), email.from.address.as_str()), (account, account));
    assert!(email.to.iter().any(|m| m.address == account), "to: {:?}", email.to);
    assert_eq!(email.subject, sent.subject);
    assert!(email.text.as_deref().is_some_and(|t| t.contains(&sent.text[..12])), "text: {:?}", email.text);
    assert!(email.gmail_id > 0 && email.thread_id > 0 && email.uid > 0 && email.size > 0);
    let within = |t: i64| (sent_at_ms / 1000 - 300..=now + 300).contains(&t);
    assert!(email.sent_at.is_some_and(within) && within(email.internal_at), "dates {:?} {}", email.sent_at, email.internal_at);
    assert!(email.received_at_ms >= sent_at_ms, "received_at before send");
    let (got, want) = (&email.attachments[0], &sent.attachments[0]);
    assert_eq!((got.filename.as_deref(), got.mime.as_str(), &got.data), (Some(want.filename.as_str()), want.mime.as_str(), &want.data));
}

#[tokio::test]
async fn live_gmail_self_round_trip() {
    let (Some(store), Some(cfg)) = (store().await, live_accounts()) else { return };
    let account = cfg.accounts[0].address.clone();
    let gmail = Gmail::start(store.clone(), cfg).await.expect("start");
    let t0 = std::time::Instant::now();
    wait_watching(&gmail).await;
    eprintln!("live gmail [{account}] IMAP logged in, INBOX selected, IDLE watching after {:.1}s", t0.elapsed().as_secs_f64());
    gmail.allow(&account, &account).await.expect("allow self");
    let mut bus = gmail.subscribe();

    let out = self_message(&account, unique());
    let started = now_ms();
    let id = gmail.send(&account, &out).await.expect("send");
    let sent = std::time::Instant::now();
    eprintln!("live gmail [{account}] SMTP accepted <{id}> ({} bytes attached) in {}ms", out.attachments[0].data.len(), now_ms() - started);
    let email = tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            let email = bus.recv().await.expect("bus open");
            if email.message_id == id {
                return email;
            }
        }
    })
    .await
    .expect("sent message received within 180s");
    assert_envelope(&email, &account, &out, started);
    report_received(&account, &email, sent.elapsed());
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM gmail_messages WHERE account = $1 AND message_id = $2")
        .bind(&account)
        .bind(&id)
        .fetch_one(store.pool())
        .await
        .expect("query");
    assert_eq!(stored, 1, "published messages are stored");
    eprintln!("live gmail [{account}] stored in gmail_messages ✔");

    assert_filtered(&gmail, &mut bus, &account).await;
}

/// Revokes the account's own address, sends to itself and checks the message is counted as
/// filtered and never published; allows the address again afterwards.
async fn assert_filtered(gmail: &Gmail, bus: &mut tokio::sync::broadcast::Receiver<std::sync::Arc<Email>>, account: &str) {
    gmail.revoke(account, account).await.expect("revoke self");
    let filtered = gmail.stats().filtered.load(Relaxed);
    let blocked = gmail.send(account, &self_message(account, unique())).await.expect("send");
    let sent = std::time::Instant::now();
    eprintln!("live gmail [{account}] sender revoked; SMTP accepted <{blocked}>");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    while gmail.stats().filtered.load(Relaxed) == filtered {
        assert!(tokio::time::Instant::now() < deadline, "revoked message never reached the filter");
        if let Ok(Ok(email)) = tokio::time::timeout(Duration::from_millis(200), bus.recv()).await {
            assert_ne!(email.message_id, blocked, "message from a revoked sender was published");
        }
    }
    eprintln!("live gmail [{account}] IMAP received it after {:.1}s and FILTERED it (never published) ✔", sent.elapsed().as_secs_f64());
    gmail.allow(account, account).await.expect("allow again");
}
