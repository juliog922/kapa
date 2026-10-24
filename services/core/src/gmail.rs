//! Gmail accounts: one watcher task per account (IMAP IDLE, reconnect with backoff) that
//! publishes allowed messages on an in-process bus and stores them; sending from any account.
//! Allowlists are deny by default, stored in PostgreSQL and checked in memory.

use crate::config::{Accounts, Credentials, Server};
use crate::imap::{self, IDLE_RENEW, Imap};
use crate::mail::{self, Email, Outgoing};
use crate::{Error, Result, log, store::Store};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio_rustls::client::TlsStream;

/// Messages a subscriber may lag behind before it starts losing them.
const BUS_CAPACITY: usize = 1024;

/// Per-account allowlists (deny by default), shared by every task.
#[derive(Default)]
pub struct Allowlist(RwLock<HashMap<String, HashSet<String>>>);

impl Allowlist {
    /// Allows `sender` for `account`.
    pub fn insert(&self, account: &str, sender: &str) {
        self.0.write().unwrap_or_else(std::sync::PoisonError::into_inner).entry(account.to_owned()).or_default().insert(sender.to_owned());
    }

    /// Revokes `sender` for `account`.
    pub fn remove(&self, account: &str, sender: &str) {
        if let Some(set) = self.0.write().unwrap_or_else(std::sync::PoisonError::into_inner).get_mut(account) {
            set.remove(sender);
        }
    }

    /// Whether messages from `sender` to `account` are published.
    #[must_use]
    pub fn contains(&self, account: &str, sender: &str) -> bool {
        self.0.read().unwrap_or_else(std::sync::PoisonError::into_inner).get(account).is_some_and(|s| s.contains(sender))
    }

    /// Allowed senders of `account`, sorted.
    #[must_use]
    pub fn list(&self, account: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(account)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        out.sort_unstable();
        out
    }
}

/// Traffic counters (lock-free).
#[derive(Default)]
pub struct Stats {
    /// Messages fetched from the inboxes.
    pub received: AtomicU64,
    /// Messages dropped because the sender is not allowed.
    pub filtered: AtomicU64,
    /// Messages stored and published.
    pub published: AtomicU64,
    /// Messages sent.
    pub sent: AtomicU64,
    /// Failed sends.
    pub send_failed: AtomicU64,
}

/// Point-in-time view of the module for the status report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// `(address, state, allowed senders)` per account, sorted.
    pub accounts: Vec<(String, String, usize)>,
    /// Counters: received, filtered, published, sent, failed sends.
    pub counters: [u64; 5],
    /// Open bus subscriptions.
    pub subscribers: usize,
}

/// Shared state of the module.
struct Inner {
    /// PostgreSQL.
    store: Store,
    /// Configured accounts by address.
    accounts: HashMap<String, Credentials>,
    /// SMTP server.
    smtp: Server,
    /// IMAP server.
    imap: Server,
    /// Allowlists.
    allow: Allowlist,
    /// In-process publication of allowed messages.
    bus: broadcast::Sender<Arc<Email>>,
    /// Counters.
    stats: Stats,
    /// Connection state per account.
    states: Mutex<BTreeMap<String, String>>,
}

/// Handle to the Gmail module (cheap to clone).
#[derive(Clone)]
pub struct Gmail {
    /// Shared state.
    inner: Arc<Inner>,
}

impl Inner {
    /// Records the connection state of an account.
    fn set_state(&self, account: &str, state: String) {
        self.states.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(account.to_owned(), state);
    }

    /// Handles one fetched message: filter, store, publish.
    async fn process(&self, account: &str, f: imap::Fetched) -> Result<()> {
        self.stats.received.fetch_add(1, Relaxed);
        let Some(email) = mail::parse(account, f.uid, f.gmail_id, f.thread_id, f.internal_at, &f.raw) else {
            log(&format!("gmail {account}: uid {} is not a message, skipped", f.uid));
            return Ok(());
        };
        if !self.allow.contains(account, &email.from.address) {
            self.stats.filtered.fetch_add(1, Relaxed);
            return Ok(());
        }
        if self.store.save(&email).await? {
            self.stats.published.fetch_add(1, Relaxed);
            let _ = self.bus.send(Arc::new(email)); // no subscriber is fine: it is stored
        }
        Ok(())
    }

    /// Opens an authenticated session on INBOX; returns it with `(UIDVALIDITY, last seen UID)`.
    async fn open(&self, creds: &Credentials) -> Result<(Imap<TlsStream<TcpStream>>, u32, u32)> {
        let tcp = imap::tcp(&self.imap.host, self.imap.port).await?;
        let mut session = Imap::connect(imap::tls(tcp, &self.imap.host).await?).await?;
        session.login(&creds.address, &creds.password).await?;
        let (validity, next) = session.select("INBOX").await?;
        let last = match self.store.cursor(&creds.address).await? {
            Some((v, uid)) if v == validity => uid,
            _ => next.saturating_sub(1), // first run or mailbox reset: only new mail
        };
        self.store.set_cursor(&creds.address, validity, last).await?;
        Ok((session, validity, last))
    }

    /// One IMAP session: catch up from the saved cursor, then IDLE forever.
    async fn watch_once(&self, creds: &Credentials) -> Result<()> {
        self.set_state(&creds.address, "connecting".into());
        let (mut session, validity, mut last) = self.open(creds).await?;
        self.set_state(&creds.address, "watching".into());
        loop {
            for f in session.fetch_since(last + 1).await? {
                last = last.max(f.uid);
                self.process(&creds.address, f).await?;
            }
            self.store.set_cursor(&creds.address, validity, last).await?;
            session.idle(IDLE_RENEW).await?;
        }
    }
}

/// Keeps an account watched forever, reconnecting with exponential backoff (5 s to 5 min).
async fn watch(inner: Arc<Inner>, creds: Credentials) {
    let mut backoff = Duration::from_secs(5);
    loop {
        let started = Instant::now();
        let err = inner.watch_once(&creds).await.err().map_or_else(|| "session ended".to_owned(), |e| e.to_string());
        if started.elapsed() > Duration::from_secs(300) {
            backoff = Duration::from_secs(5);
        }
        inner.set_state(&creds.address, format!("error: {err}"));
        log(&format!("gmail {}: {err}; retrying in {}s", creds.address, backoff.as_secs()));
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(300));
    }
}

impl Gmail {
    /// Creates the tables, loads the allowlists and starts one watcher per account.
    ///
    /// # Errors
    /// Database failure.
    pub async fn start(store: Store, cfg: Accounts) -> Result<Self> {
        store.migrate().await?;
        let allow = Allowlist::default();
        for (account, sender) in store.allowlist().await? {
            allow.insert(&account, &sender);
        }
        let inner = Arc::new(Inner {
            accounts: cfg.accounts.iter().map(|c| (c.address.clone(), c.clone())).collect(),
            states: Mutex::new(cfg.accounts.iter().map(|c| (c.address.clone(), "starting".to_owned())).collect()),
            store,
            smtp: cfg.smtp,
            imap: cfg.imap,
            allow,
            bus: broadcast::channel(BUS_CAPACITY).0,
            stats: Stats::default(),
        });
        for creds in cfg.accounts {
            tokio::spawn(watch(Arc::clone(&inner), creds));
        }
        Ok(Self { inner })
    }

    /// Subscribes to allowed incoming messages (a slow subscriber loses old messages, never
    /// blocks the others; everything is also stored in `gmail_messages`).
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Email>> {
        self.inner.bus.subscribe()
    }

    /// Sends `out` from the configured account `from`; returns the new `Message-ID`.
    ///
    /// # Errors
    /// Unknown account, invalid message, or delivery failure.
    pub async fn send(&self, from: &str, out: &Outgoing) -> Result<String> {
        let creds = self.inner.accounts.get(&from.to_ascii_lowercase()).ok_or_else(|| Error::Invalid(format!("unknown account {from}")))?;
        mail::validate(out)?;
        let id = mail::new_message_id(&creds.address)?;
        let data = mail::build(&creds.address, out, crate::now_ms() / 1000, &id);
        let rcpts: Vec<String> = out.to.iter().chain(&out.cc).cloned().collect();
        let result = crate::smtp::send(&self.inner.smtp, creds, &rcpts, &data).await;
        let counter = if result.is_ok() { &self.inner.stats.sent } else { &self.inner.stats.send_failed };
        counter.fetch_add(1, Relaxed);
        result.map(|()| id)
    }

    /// Normalizes and checks an `(account, sender)` pair.
    fn pair(&self, account: &str, sender: &str) -> Result<(String, String)> {
        let (account, sender) = (account.trim().to_ascii_lowercase(), sender.trim().to_ascii_lowercase());
        if !self.inner.accounts.contains_key(&account) {
            return Err(Error::Invalid(format!("unknown account {account}")));
        }
        if !sender.contains('@') || sender.contains(char::is_whitespace) {
            return Err(Error::Invalid(format!("invalid sender {sender:?}")));
        }
        Ok((account, sender))
    }

    /// Allows `sender` to reach `account` (PostgreSQL first, then memory).
    ///
    /// # Errors
    /// Unknown account, invalid sender or database failure.
    pub async fn allow(&self, account: &str, sender: &str) -> Result<()> {
        let (account, sender) = self.pair(account, sender)?;
        self.inner.store.allow(&account, &sender).await?;
        self.inner.allow.insert(&account, &sender);
        Ok(())
    }

    /// Revokes `sender` for `account` (PostgreSQL first, then memory).
    ///
    /// # Errors
    /// Unknown account, invalid sender or database failure.
    pub async fn revoke(&self, account: &str, sender: &str) -> Result<()> {
        let (account, sender) = self.pair(account, sender)?;
        self.inner.store.revoke(&account, &sender).await?;
        self.inner.allow.remove(&account, &sender);
        Ok(())
    }

    /// Allowed senders of `account`, sorted.
    #[must_use]
    pub fn allowed_senders(&self, account: &str) -> Vec<String> {
        self.inner.allow.list(&account.to_ascii_lowercase())
    }

    /// Counters, for tests and the status report.
    #[must_use]
    pub fn stats(&self) -> &Stats {
        &self.inner.stats
    }

    /// Point-in-time view for the status report.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let states = self.inner.states.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        let s = &self.inner.stats;
        Snapshot {
            accounts: states
                .into_iter()
                .map(|(a, st)| {
                    let n = self.inner.allow.list(&a).len();
                    (a, st, n)
                })
                .collect(),
            counters: [
                s.received.load(Relaxed),
                s.filtered.load(Relaxed),
                s.published.load(Relaxed),
                s.sent.load(Relaxed),
                s.send_failed.load(Relaxed),
            ],
            subscribers: self.inner.bus.receiver_count(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_is_deny_by_default_and_per_account() {
        let allow = Allowlist::default();
        assert!(!allow.contains("bot@kapalua.es", "ana@x.y"));
        allow.insert("bot@kapalua.es", "ana@x.y");
        allow.insert("bot@kapalua.es", "bob@x.y");
        assert!(allow.contains("bot@kapalua.es", "ana@x.y"));
        assert!(!allow.contains("other@kapalua.es", "ana@x.y"));
        assert_eq!(allow.list("bot@kapalua.es"), ["ana@x.y", "bob@x.y"]);
        allow.remove("bot@kapalua.es", "ana@x.y");
        allow.remove("nobody@x.y", "ana@x.y");
        assert!(!allow.contains("bot@kapalua.es", "ana@x.y"));
    }

    #[test]
    fn allowlist_concurrent_access() {
        let allow = Arc::new(Allowlist::default());
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let allow = Arc::clone(&allow);
                std::thread::spawn(move || {
                    for j in 0..1000 {
                        let sender = format!("s{}@x.y", j % 10);
                        allow.insert("a@b.c", &sender);
                        let _ = allow.contains("a@b.c", &sender);
                        if i % 2 == 0 {
                            allow.remove("a@b.c", &sender);
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert!(allow.list("a@b.c").len() <= 10);
    }
}
