//! Health without a network port: core rewrites a status file every few seconds;
//! `core --healthcheck` checks it is fresh and `core --status` prints it (for `make monitor`).

use crate::gmail::Snapshot;
use std::{fmt::Write as _, path::Path, time::Duration};

/// A status file older than this means core is stuck or dead.
pub const MAX_AGE: Duration = Duration::from_secs(30);

/// Renders the operator report and its exit code: 2 (warning) when an account is not
/// watching, else 0.
#[must_use]
pub fn report(gmail: &Snapshot, admin: Option<&str>, started_ms: i64) -> (String, u8) {
    let mut out = String::new();
    let up = Duration::from_millis(u64::try_from(crate::now_ms() - started_ms).unwrap_or(0)).as_secs();
    let _ = writeln!(out, "core · up {}h{:02}m · admin {}", up / 3600, up % 3600 / 60, admin.unwrap_or("not configured"));
    let [received, filtered, published, sent, failed] = gmail.counters;
    let _ = writeln!(
        out,
        "gmail: received {received} · filtered {filtered} · published {published} · sent {sent} · send failures {failed} · streams {}",
        gmail.subscribers
    );
    let _ = writeln!(out, "{} account(s)", gmail.accounts.len());
    let mut code = 0;
    for (address, state, allowed) in &gmail.accounts {
        let mut note = if *allowed == 0 {
            " · no allowed senders (nothing is published)".to_owned()
        } else {
            format!(" · {allowed} allowed senders")
        };
        if state != "watching" && state != "starting" && state != "connecting" {
            note.push_str("  <- not watching");
            code = 2;
        }
        let _ = writeln!(out, "  {address} {state}{note}");
    }
    (out, code)
}

/// Atomically writes the status file: `updated=<unix ms>`, `code=<n>`, then the report.
///
/// # Errors
/// File system failure.
pub fn write(path: &Path, report: &str, code: u8) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("updated={}\ncode={code}\n{report}", crate::now_ms()))?;
    std::fs::rename(tmp, path)
}

/// Reads the status file: the report and its exit code (1 when missing or stale).
#[must_use]
pub fn read(path: &Path, max_age: Duration) -> (String, u8) {
    let Ok(text) = std::fs::read_to_string(path) else { return ("core status unavailable: not running?\n".into(), 1) };
    let mut lines = text.splitn(3, '\n');
    let updated: i64 = lines.next().and_then(|l| l.strip_prefix("updated=")?.parse().ok()).unwrap_or(0);
    let code: u8 = lines.next().and_then(|l| l.strip_prefix("code=")?.parse().ok()).unwrap_or(1);
    let report = lines.next().unwrap_or_default().to_owned();
    let age = Duration::from_millis(u64::try_from(crate::now_ms() - updated).unwrap_or(u64::MAX));
    if age > max_age {
        return (format!("{report}STALE: status not updated for {}s (core stuck or stopped)\n", age.as_secs()), 1);
    }
    (report, code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_codes_and_contents() {
        let snap = |state: &str, allowed| Snapshot {
            accounts: vec![("bot@kapalua.es".into(), state.into(), allowed)],
            counters: [5, 1, 4, 2, 0],
            subscribers: 3,
        };
        let (text, code) = report(&snap("watching", 2), Some("bot@kapalua.es"), crate::now_ms());
        assert_eq!(code, 0);
        assert!(text.contains("received 5 · filtered 1 · published 4 · sent 2 · send failures 0 · streams 3"), "{text}");
        assert!(text.contains("bot@kapalua.es watching · 2 allowed senders") && text.contains("admin bot@kapalua.es"));
        let (text, code) = report(&snap("error: server: NO", 0), None, crate::now_ms());
        assert_eq!(code, 2);
        assert!(text.contains("not watching") && text.contains("no allowed senders") && text.contains("admin not configured"));
    }

    #[test]
    fn status_file_freshness() {
        let path = std::env::temp_dir().join(format!("core-status-{}", crate::now_ms()));
        assert_eq!(read(&path, MAX_AGE).1, 1, "missing file is unhealthy");
        write(&path, "core · ok\n", 2).unwrap();
        assert_eq!(read(&path, MAX_AGE), ("core · ok\n".into(), 2));
        let (text, code) = read(&path, Duration::ZERO);
        std::thread::sleep(Duration::from_millis(5));
        assert!(code == 1 || text.contains("ok"));
        std::thread::sleep(Duration::from_millis(5));
        let (text, code) = read(&path, Duration::from_millis(1));
        assert_eq!(code, 1);
        assert!(text.contains("STALE"));
        std::fs::remove_file(path).unwrap();
    }
}
