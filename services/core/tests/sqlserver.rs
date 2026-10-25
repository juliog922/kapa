//! Live SQL Server test: runs when `SQLSERVER_FILE` names a readable settings file (the
//! Sage server is not reachable from CI). Checks every table and column core relies on.

use kapa_core::sqlserver::{self, SAGE_TABLES};

#[tokio::test]
async fn live_sage_schema() {
    let Some(text) = std::env::var("SQLSERVER_FILE").ok().and_then(|p| std::fs::read_to_string(p).ok()) else {
        eprintln!("LIVE SQL SERVER SKIPPED: SQLSERVER_FILE not set or unreadable (no secrets/sqlserver.dev.env)");
        return;
    };
    let cfg = sqlserver::parse_config(&text).expect("valid sqlserver file");
    // connect() audits the login and refuses one that can write: passing it proves read-only.
    let mut session = Box::pin(sqlserver::connect(&cfg)).await.expect("connect with a READ-ONLY login");
    eprintln!("live sql server: login {} is read-only ✔", cfg.user);
    let (version, database, login) = sqlserver::server_info(&mut session).await.expect("server info");
    eprintln!("live sql server: {version} · database {database} · login {login}");
    assert!(database.eq_ignore_ascii_case(&cfg.database), "connected to {database}");
    for table in &SAGE_TABLES {
        let check = sqlserver::check_table(&mut session, table.name).await.expect("table metadata");
        let schema = check.schema.as_deref().unwrap_or_else(|| panic!("table {} not found", table.name));
        let diff = sqlserver::compare(table, &check.columns);
        assert!(diff.missing.is_empty() && diff.wrong_type.is_empty(), "{schema}.{}: {diff:?}", table.name);
        assert!(check.can_select, "login cannot SELECT {schema}.{}", table.name);
        eprintln!(
            "live sql server: {schema}.{} ✔ {} columns · ~{:?} rows · shorter {:?}",
            table.name,
            check.columns.len(),
            check.rows,
            diff.shorter
        );
    }
}
