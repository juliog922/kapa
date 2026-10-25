//! `core`: runs the Kapa core service, or `core --healthcheck` / `core --status` for probes.

use kapa_core::{Result, config, gmail::Gmail, log, now_ms, sample, sqlserver, status, store::Store};
use std::{path::PathBuf, process::ExitCode, time::Duration};

/// Environment variable `key`, or `default`.
fn env(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_owned())
}

/// Reads a secrets file; a missing file means "not configured".
fn read_optional(path: &str) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// `core --check-sqlserver`: connects to the Sage SQL Server and checks the schema core uses.
async fn check_sqlserver() -> ExitCode {
    let path = env("SQLSERVER_FILE", "/run/secrets/core/sqlserver.env");
    let result = async {
        let text = read_optional(&path)?.ok_or_else(|| kapa_core::Error::Invalid(format!("{path} not found")))?;
        Box::pin(sqlserver::check(&sqlserver::parse_config(&text)?)).await
    };
    match result.await {
        Ok((report, ok)) => {
            print!("{report}");
            ExitCode::from(u8::from(!ok))
        }
        Err(e) => {
            println!("✘ SQL Server check failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `core --sample-sqlserver`: prints the Sage schema and a coherent sample as JSON on stdout
/// (summary on stderr). `SAMPLE_ORDERS` (default 10, max 200) orders; `SAMPLE_RAW=1` disables
/// the masking of personal data.
async fn sample_sqlserver() -> ExitCode {
    let path = env("SQLSERVER_FILE", "/run/secrets/core/sqlserver.env");
    let orders = env("SAMPLE_ORDERS", "10").parse::<i32>().unwrap_or(10).clamp(1, 200);
    let mask = env("SAMPLE_RAW", "0") != "1";
    let result = async {
        let text = read_optional(&path)?.ok_or_else(|| kapa_core::Error::Invalid(format!("{path} not found")))?;
        Box::pin(sample::export(&sqlserver::parse_config(&text)?, orders, mask)).await
    };
    match result.await {
        Ok((json, summary)) => {
            print!("{json}");
            eprintln!("sample: {summary} · personal data {}", if mask { "masked" } else { "RAW (not masked)" });
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("✘ SQL Server sample failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Starts every module and rewrites the status file until SIGTERM or Ctrl+C.
async fn run(status_path: PathBuf) -> Result<()> {
    let accounts_file = env("ACCOUNTS_FILE", "/run/secrets/core/accounts.env");
    let accounts = read_optional(&accounts_file)?.map_or_else(|| config::parse_accounts(""), |t| config::parse_accounts(&t))?;
    let admin = read_optional(&env("ADMIN_FILE", "/run/secrets/core/admin.env"))?.map(|t| config::parse_admin(&t)).transpose()?;
    let store = Store::connect(std::env::var("DATABASE_URL").ok().as_deref()).await?;
    let started = now_ms();
    log(&format!(
        "core starting: {} gmail account(s) from {accounts_file}, admin {}",
        accounts.accounts.len(),
        admin.as_ref().map_or("not configured", |a| a.address.as_str())
    ));
    let gmail = Gmail::start(store, accounts).await?;
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let (report, code) = status::report(&gmail.snapshot(), admin.as_ref().map(|a| a.address.as_str()), started);
                status::write(&status_path, &report, code)?;
            }
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    log("core stopped");
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let status_path = PathBuf::from(env("STATUS_FILE", "/tmp/core.status"));
    match std::env::args().nth(1).as_deref() {
        Some("--healthcheck") => return ExitCode::from(u8::from(status::read(&status_path, status::MAX_AGE).1 == 1)),
        Some("--check-sqlserver") => return check_sqlserver().await,
        Some("--sample-sqlserver") => return sample_sqlserver().await,
        Some("--status") => {
            let (report, code) = status::read(&status_path, status::MAX_AGE);
            print!("{report}");
            return ExitCode::from(code);
        }
        _ => {}
    }
    match run(status_path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log(&format!("fatal: {e}"));
            ExitCode::FAILURE
        }
    }
}
