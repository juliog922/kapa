//! SQL Server access (the Sage ERP) over TDS with `tiberius`: no ODBC driver, same rustls
//! as the rest of core. Sage must NEVER be written, so read-only is enforced in three
//! independent layers, any one of which blocks a write:
//!
//! 1. Types: the raw client is private; the rest of core only gets a [`ReadOnlySession`]
//!    whose single operation is [`ReadOnlySession::select`].
//! 2. Statement guard: [`read_only`] refuses, before anything reaches the server, any text
//!    that is not one `SELECT` (no `;`, no comments, no write/DDL/exec/escape keywords).
//! 3. Login audit: [`connect`] asks the server what the login may do and refuses to hand out
//!    a session if it holds any write, DDL or EXECUTE permission.
//!
//! `core --check-sqlserver` reports all of it plus the Sage schema core relies on.

use crate::{Error, Result};
use std::{fmt, fmt::Write as _, time::Duration};
use tiberius::{AuthMethod, Client, Config as TdsConfig, EncryptionLevel, Row};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

// ---- The only SQL core sends to SQL Server (each one passes `read_only`, see tests) ----

/// Server identity.
const SERVER_INFO: &str = "SELECT @@VERSION, DB_NAME(), SUSER_SNAME()";
/// Column metadata of one table.
const COLUMNS: &str = concat!(
    "SELECT TABLE_SCHEMA, COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH FROM INFORMATION_SCHEMA.COLUMNS ",
    "WHERE TABLE_NAME = @P1 ORDER BY ORDINAL_POSITION",
);
/// Approximate rows and SELECT / write permissions of one table.
const TABLE_STATS: &str = concat!(
    "SELECT (SELECT CAST(SUM(p.rows) AS BIGINT) FROM sys.partitions p ",
    "WHERE p.object_id = OBJECT_ID(@P1) AND p.index_id IN (0, 1)), ",
    "HAS_PERMS_BY_NAME(@P1, 'OBJECT', 'SELECT'), ",
    "HAS_PERMS_BY_NAME(@P1, 'OBJECT', 'INSERT') | HAS_PERMS_BY_NAME(@P1, 'OBJECT', 'UPDATE') | HAS_PERMS_BY_NAME(@P1, 'OBJECT', 'DELETE')",
);
/// Everything the login may do that could change the database, plus write-capable role
/// memberships and sysadmin.
const WRITE_PERMISSIONS: &str = concat!(
    "SELECT permission_name FROM fn_my_permissions(NULL, 'DATABASE') ",
    "WHERE permission_name IN ('INSERT', 'UPDATE', 'DELETE', 'ALTER', 'CONTROL', 'EXECUTE', 'TAKE OWNERSHIP', 'IMPERSONATE', ",
    "'CREATE TABLE', 'CREATE PROCEDURE', 'CREATE FUNCTION', 'CREATE VIEW', 'CREATE SCHEMA', 'ALTER ANY SCHEMA', ",
    "'ALTER ANY USER', 'ALTER ANY ROLE', 'BACKUP DATABASE') ",
    "UNION ALL SELECT 'member of ' + r FROM (VALUES ('db_owner'), ('db_datawriter'), ('db_ddladmin')) v(r) WHERE IS_ROLEMEMBER(r) = 1 ",
    "UNION ALL SELECT 'member of sysadmin' WHERE IS_SRVROLEMEMBER('sysadmin') = 1",
);

/// A SQL Server session that can only run guarded `SELECT`s. The underlying client is
/// private to this module: no other code in core can send anything else.
pub struct ReadOnlySession(Client<Compat<TcpStream>>);

impl ReadOnlySession {
    /// Runs one `SELECT` (checked by [`read_only`] before it is sent) and returns its rows.
    /// Statements may be assembled at run time (e.g. per-column statistics) but only from
    /// core's own constants, never from external input; the guard checks the final text.
    ///
    /// # Errors
    /// The statement is not a single read-only `SELECT` (refused locally), or query failure.
    pub async fn select(&mut self, sql: &str, params: &[&dyn tiberius::ToSql]) -> Result<Vec<Row>> {
        read_only(sql)?;
        Ok(self.0.query(sql, params).await?.into_first_result().await?)
    }
}

/// Whole words that can write, change state, run code or reach outside the query.
const FORBIDDEN: [&str; 37] = [
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "INTO",
    "EXEC",
    "EXECUTE",
    "DROP",
    "ALTER",
    "CREATE",
    "TRUNCATE",
    "GRANT",
    "REVOKE",
    "DENY",
    "BACKUP",
    "RESTORE",
    "DBCC",
    "SHUTDOWN",
    "KILL",
    "OPENROWSET",
    "OPENQUERY",
    "OPENDATASOURCE",
    "OPENXML",
    "BULK",
    "WAITFOR",
    "RECONFIGURE",
    "USE",
    "SET",
    "DECLARE",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "SAVE",
    "TRAN",
    "TRANSACTION",
    "GO",
    "UPDATETEXT",
];

/// The statement with the contents of its string literals blanked (literals cannot run).
fn without_literals(sql: &str) -> String {
    let mut inside = false;
    sql.chars()
        .map(|c| {
            if c == '\'' {
                inside = !inside;
                c
            } else if inside {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Refuses anything that is not exactly one read-only `SELECT` statement.
///
/// # Errors
/// [`Error::Invalid`] naming the reason; nothing is sent to the server.
pub fn read_only(sql: &str) -> Result<()> {
    let code = without_literals(sql).to_ascii_uppercase();
    let refuse = |why: &str| Err(Error::Invalid(format!("read-only guard refused SQL ({why})")));
    if code.split_whitespace().next() != Some("SELECT") {
        return refuse("not a SELECT");
    }
    // "/\x2a" is "/*" (written escaped so complexity tools do not read it as a comment).
    if code.contains(';') || code.contains("--") || code.contains("/\x2a") {
        return refuse("statement separators and comments are not allowed");
    }
    let mut words = code.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'));
    match words.find(|w| FORBIDDEN.contains(w) || w.starts_with("XP_") || w.starts_with("SP_")) {
        Some(word) => refuse(&format!("forbidden keyword {word}")),
        None => Ok(()),
    }
}

/// TLS mode of the connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encrypt {
    /// Encrypt everything; fail if the server cannot (default).
    Required,
    /// Encrypt only the login (old servers whose TLS is too weak for full sessions).
    LoginOnly,
    /// No TLS at all (servers that only speak TLS 1.0/1.1; LAN only).
    None,
}

/// Connection settings from `sqlserver.*.env`. The password never appears in `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    /// Server host or IP.
    pub host: String,
    /// TCP port (default 1433).
    pub port: u16,
    /// SQL login.
    pub user: String,
    /// SQL password.
    pub password: String,
    /// Database (catalog).
    pub database: String,
    /// TLS mode.
    pub encrypt: Encrypt,
    /// Accept the server certificate without validation (typical self-signed ERP servers).
    pub trust_cert: bool,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Config")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("database", &self.database)
            .field("encrypt", &self.encrypt)
            .field("trust_cert", &self.trust_cert)
            .finish()
    }
}

/// Parses `sqlserver.*.env` (`SQLSERVER_HOST`, `_USER`, `_PASSWORD`, `_DB` required;
/// `_PORT` 1433, `_ENCRYPT` required|login|none, `_TRUST_CERT` true|false).
///
/// # Errors
/// Malformed lines, unknown keys, invalid values or missing required keys.
pub fn parse_config(text: &str) -> Result<Config> {
    let mut cfg = Config {
        host: String::new(),
        port: 1433,
        user: String::new(),
        password: String::new(),
        database: String::new(),
        encrypt: Encrypt::Required,
        trust_cert: true,
    };
    for (n, key, value) in crate::config::entries(text)? {
        apply(&mut cfg, key, value).map_err(|what| Error::Invalid(format!("line {n}: {what}")))?;
    }
    if cfg.host.is_empty() || cfg.user.is_empty() || cfg.password.is_empty() || cfg.database.is_empty() {
        return Err(Error::Invalid("sqlserver file needs SQLSERVER_HOST, _USER, _PASSWORD and _DB".into()));
    }
    Ok(cfg)
}

/// Parses `SQLSERVER_ENCRYPT`.
fn encrypt(value: &str) -> std::result::Result<Encrypt, String> {
    match value {
        "required" => Ok(Encrypt::Required),
        "login" => Ok(Encrypt::LoginOnly),
        "none" => Ok(Encrypt::None),
        _ => Err("SQLSERVER_ENCRYPT must be required, login or none".into()),
    }
}

/// The text setting named by `key`, if it is one.
fn text_field<'c>(cfg: &'c mut Config, key: &str) -> Option<&'c mut String> {
    match key {
        "SQLSERVER_HOST" => Some(&mut cfg.host),
        "SQLSERVER_USER" => Some(&mut cfg.user),
        "SQLSERVER_PASSWORD" => Some(&mut cfg.password),
        "SQLSERVER_DB" => Some(&mut cfg.database),
        _ => None,
    }
}

/// Applies one `KEY=VALUE` line of `sqlserver.*.env`.
fn apply(cfg: &mut Config, key: &str, value: &str) -> std::result::Result<(), String> {
    if let Some(field) = text_field(cfg, key) {
        value.clone_into(field);
        return Ok(());
    }
    match key {
        "SQLSERVER_PORT" => cfg.port = value.parse().map_err(|_| "invalid SQLSERVER_PORT")?,
        "SQLSERVER_TRUST_CERT" => cfg.trust_cert = value.parse().map_err(|_| "SQLSERVER_TRUST_CERT must be true or false")?,
        "SQLSERVER_ENCRYPT" => cfg.encrypt = encrypt(value)?,
        other => return Err(format!("unknown key {other}")),
    }
    Ok(())
}

impl From<tiberius::error::Error> for Error {
    fn from(e: tiberius::error::Error) -> Self {
        Self::Db(format!("sql server: {e}"))
    }
}

/// Connects and audits the login: the session is only handed out when the login holds no
/// write, DDL or EXECUTE permission.
///
/// # Errors
/// Network, TLS or login failure, or a login that can write (refused).
pub async fn connect(cfg: &Config) -> Result<ReadOnlySession> {
    let mut session = Box::pin(open(cfg)).await?;
    let writes = write_permissions(&mut session).await?;
    if !writes.is_empty() {
        return Err(Error::Invalid(format!(
            "refusing SQL Server login {}: it can write ({}); core requires a read-only login",
            cfg.user,
            writes.join(", ")
        )));
    }
    Ok(session)
}

/// Write-capable permissions the session's login holds (empty = read-only).
///
/// # Errors
/// Query failure.
pub async fn write_permissions(session: &mut ReadOnlySession) -> Result<Vec<String>> {
    let rows = session.select(WRITE_PERMISSIONS, &[]).await?;
    Ok(rows.iter().filter_map(|r| r.get::<&str, _>(0).map(str::to_owned)).collect())
}

/// Opens a session (30 s timeout) as `cfg.user` on `cfg.database`, without the audit.
async fn open(cfg: &Config) -> Result<ReadOnlySession> {
    crate::imap::install_crypto_provider(); // tiberius uses the process-wide provider
    let mut tds = TdsConfig::new();
    tds.readonly(true); // ApplicationIntent=ReadOnly (routes to a readable secondary, if any)
    tds.host(&cfg.host);
    tds.port(cfg.port);
    tds.database(&cfg.database);
    tds.application_name("kapa-core");
    tds.authentication(AuthMethod::sql_server(&cfg.user, &cfg.password));
    tds.encryption(match cfg.encrypt {
        Encrypt::Required => EncryptionLevel::Required,
        Encrypt::LoginOnly => EncryptionLevel::Off,
        Encrypt::None => EncryptionLevel::NotSupported,
    });
    if cfg.trust_cert {
        tds.trust_cert();
    }
    let tcp = crate::imap::tcp(&cfg.host, cfg.port).await?;
    tcp.set_nodelay(true)?;
    // The TDS login future is ~18 KB: box it instead of carrying it on the task stack.
    let session = tokio::time::timeout(Duration::from_secs(30), Box::pin(Client::connect(tds, tcp.compat_write())))
        .await
        .map_err(|_| Error::Protocol(format!("sql server {}:{}: login timeout", cfg.host, cfg.port)))??;
    Ok(ReadOnlySession(session))
}

/// Family of a SQL type, as far as core cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// char, varchar, nchar, nvarchar, text, ntext.
    Text,
    /// tinyint, smallint, int, bigint.
    Int,
    /// decimal, numeric, money, smallmoney, float, real.
    Decimal,
    /// datetime, datetime2, smalldatetime, date.
    DateTime,
}

/// The family of a SQL Server `DATA_TYPE`, if core handles it.
#[must_use]
pub fn kind_of(data_type: &str) -> Option<Kind> {
    match data_type.to_ascii_lowercase().as_str() {
        "char" | "varchar" | "nchar" | "nvarchar" | "text" | "ntext" => Some(Kind::Text),
        "tinyint" | "smallint" | "int" | "bigint" => Some(Kind::Int),
        "decimal" | "numeric" | "money" | "smallmoney" | "float" | "real" => Some(Kind::Decimal),
        "datetime" | "datetime2" | "smalldatetime" | "date" => Some(Kind::DateTime),
        _ => None,
    }
}

/// A column core reads: name, family and minimum length for text (0 = any).
pub type Expected = (&'static str, Kind, i32);

/// A Sage table core reads, with the columns it relies on (from the chatlink models).
pub struct Table {
    /// Table name.
    pub name: &'static str,
    /// Columns core reads.
    pub columns: &'static [Expected],
}

/// Every Sage table core reads (read-only).
pub const SAGE_TABLES: [Table; 4] = [
    Table {
        name: "Clientes",
        columns: &[
            ("CodigoCliente", Kind::Text, 15),
            ("CodigoEmpresa", Kind::Int, 0),
            ("Nombre", Kind::Text, 35),
            ("Telefono", Kind::Text, 15),
            ("Telefono2", Kind::Text, 15),
            ("Telefono3", Kind::Text, 15),
            ("EMail1", Kind::Text, 250),
            ("EMail2", Kind::Text, 250),
            ("Domicilio", Kind::Text, 40),
            ("CodigoPostal", Kind::Text, 8),
            ("Municipio", Kind::Text, 25),
            ("Provincia", Kind::Text, 20),
            ("CifDni", Kind::Text, 13),
        ],
    },
    Table {
        name: "Articulos",
        columns: &[
            ("CodigoArticulo", Kind::Text, 21),
            ("CodigoEmpresa", Kind::Int, 0),
            ("DescripcionArticulo", Kind::Text, 60),
            ("Descripcion2Articulo", Kind::Text, 40),
            ("DescripcionLinea", Kind::Text, 0),
            ("ComentarioArticulo", Kind::Text, 0),
            ("MarcaProducto", Kind::Text, 50),
            ("K_BOT", Kind::Int, 0),
            ("CodigoAreaCompetenciaLc", Kind::Text, 10),
            ("CodigoAlternativo", Kind::Text, 20),
            ("FechaAlta", Kind::DateTime, 0),
            ("PrecioVenta", Kind::Decimal, 0),
            ("StockMinimo", Kind::Decimal, 0),
            ("StockMaximo", Kind::Decimal, 0),
        ],
    },
    Table {
        name: "CabeceraPedidoCliente",
        columns: &[
            ("CodigoEmpresa", Kind::Int, 0),
            ("EjercicioPedido", Kind::Int, 0),
            ("SeriePedido", Kind::Text, 10),
            ("NumeroPedido", Kind::Int, 0),
            ("CodigoCliente", Kind::Text, 15),
            ("FechaPedido", Kind::DateTime, 0),
            ("NumeroLineas", Kind::Int, 0),
            ("RazonSocial", Kind::Text, 40),
            ("ImporteLiquido", Kind::Decimal, 0),
        ],
    },
    Table {
        name: "LineasPedidoCliente",
        columns: &[
            ("CodigoEmpresa", Kind::Int, 0),
            ("EjercicioPedido", Kind::Int, 0),
            ("SeriePedido", Kind::Text, 10),
            ("NumeroPedido", Kind::Int, 0),
            ("Orden", Kind::Int, 0),
            ("CodigoArticulo", Kind::Text, 21),
            ("DescripcionArticulo", Kind::Text, 60),
            ("Descripcion2Articulo", Kind::Text, 40),
            ("CodigodelCliente", Kind::Text, 20),
            ("UnidadesPedidas", Kind::Decimal, 0),
            ("Precio", Kind::Decimal, 0),
            ("ImporteNeto", Kind::Decimal, 0),
        ],
    },
];

/// A column found on the server: name, `DATA_TYPE`, `CHARACTER_MAXIMUM_LENGTH` (-1 = max).
pub type Found = (String, String, Option<i32>);

/// Problems of one table: missing columns, wrong families, and texts shorter than expected.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Diff {
    /// Expected columns that do not exist.
    pub missing: Vec<&'static str>,
    /// `(column, expected, actual DATA_TYPE)` with an incompatible family.
    pub wrong_type: Vec<(&'static str, Kind, String)>,
    /// `(column, expected length, actual length)` for texts shorter than expected (warning).
    pub shorter: Vec<(&'static str, i32, i32)>,
}

/// Compares the expected columns of `table` with the columns `found` on the server.
#[must_use]
pub fn compare(table: &Table, found: &[Found]) -> Diff {
    let mut diff = Diff::default();
    for &(name, kind, len) in table.columns {
        let Some((_, data_type, max_len)) = found.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(name)) else {
            diff.missing.push(name);
            continue;
        };
        if kind_of(data_type) != Some(kind) {
            diff.wrong_type.push((name, kind, data_type.clone()));
        } else if let Some(actual) = max_len.filter(|&l| l != -1 && l < len) {
            diff.shorter.push((name, len, actual));
        }
    }
    diff
}

/// Server identity: version line, database, login.
///
/// # Errors
/// Query failure.
pub async fn server_info(session: &mut ReadOnlySession) -> Result<(String, String, String)> {
    let rows = session.select(SERVER_INFO, &[]).await?;
    let row = rows.first().ok_or_else(|| Error::Protocol("no server info".into()))?;
    let text = |i| row.get::<&str, _>(i).unwrap_or_default().to_owned();
    Ok((text(0).lines().next().unwrap_or_default().to_owned(), text(1), text(2)))
}

/// What the server says about one table.
#[derive(Debug, Default)]
pub struct TableCheck {
    /// Schema holding the table (`None` = table not found).
    pub schema: Option<String>,
    /// Columns found.
    pub columns: Vec<Found>,
    /// Approximate row count (from `sys.partitions`, no table scan).
    pub rows: Option<i64>,
    /// The login may `SELECT` from it.
    pub can_select: bool,
    /// The login could `INSERT`/`UPDATE`/`DELETE` it (core never does).
    pub can_write: bool,
}

/// Reads the column metadata, approximate size and permissions of `table`.
///
/// # Errors
/// Query failure.
pub async fn check_table(session: &mut ReadOnlySession, table: &str) -> Result<TableCheck> {
    let rows = session.select(COLUMNS, &[&table]).await?;
    let Some(schema) = rows.first().and_then(|r| r.get::<&str, _>(0)).map(str::to_owned) else {
        return Ok(TableCheck::default());
    };
    let columns = rows.iter().map(found).collect();
    let (rows, can_select, can_write) = stats(session, &schema, table).await?;
    Ok(TableCheck { schema: Some(schema), columns, rows, can_select, can_write })
}

/// Converts an `INFORMATION_SCHEMA.COLUMNS` row into a [`Found`] column.
fn found(row: &Row) -> Found {
    let text = |i| row.get::<&str, _>(i).unwrap_or_default().to_owned();
    (text(1), text(2), row.get::<i32, _>(3))
}

/// Approximate rows and SELECT / write permissions of `schema.table`.
async fn stats(session: &mut ReadOnlySession, schema: &str, table: &str) -> Result<(Option<i64>, bool, bool)> {
    let object = format!("[{}].[{}]", schema.replace(']', "]]"), table.replace(']', "]]"));
    let rows = session.select(TABLE_STATS, &[&object.as_str()]).await?;
    let row = rows.first().ok_or_else(|| Error::Protocol("no table stats".into()))?;
    Ok((row.get::<i64, _>(0), row.get::<i32, _>(1) == Some(1), row.get::<i32, _>(2) == Some(1)))
}

/// Lists the column problems of a table under its summary line.
fn render_diff(out: &mut String, diff: &Diff) {
    for name in &diff.missing {
        let _ = writeln!(out, "      missing column {name}");
    }
    for (name, kind, actual) in &diff.wrong_type {
        let _ = writeln!(out, "      {name}: expected {kind:?}, server has {actual}");
    }
    for (name, want, got) in &diff.shorter {
        let _ = writeln!(out, "      {name}: text length {got} < {want} expected (values may be truncated)");
    }
}

/// Renders the check of one table; returns `false` when core cannot rely on it.
fn render_table(out: &mut String, table: &Table, check: &TableCheck) -> bool {
    let Some(schema) = &check.schema else {
        let _ = writeln!(out, "  ✘ {}: table not found (or not visible to this login)", table.name);
        return false;
    };
    let diff = compare(table, &check.columns);
    let ok = diff.missing.is_empty() && diff.wrong_type.is_empty() && check.can_select && !check.can_write;
    let rows = check.rows.map_or_else(|| "?".to_owned(), |r| r.to_string());
    let _ = writeln!(
        out,
        "  {} {schema}.{}: {} columns ({} used by core) · ~{rows} rows · SELECT {}{}",
        if ok { "✔" } else { "✘" },
        table.name,
        check.columns.len(),
        table.columns.len(),
        if check.can_select { "yes" } else { "NO" },
        if check.can_write { " · ✘ login can WRITE this table" } else { " · read-only" }
    );
    render_diff(out, &diff);
    ok
}

/// Connects and checks every table core relies on. Returns the report and whether all is fine.
///
/// # Errors
/// Connection or query failure.
pub async fn check(cfg: &Config) -> Result<(String, bool)> {
    let mut session = Box::pin(open(cfg)).await?;
    let (version, database, login) = server_info(&mut session).await?;
    let mut out = format!("SQL Server {}:{} · database {database} · login {login}\n  {version}\n", cfg.host, cfg.port);
    let writes = write_permissions(&mut session).await?;
    let mut all_ok = render_audit(&mut out, &login, &writes);
    for table in &SAGE_TABLES {
        let check = check_table(&mut session, table.name).await?;
        all_ok &= render_table(&mut out, table, &check);
    }
    out.push_str(if all_ok {
        "✔ read-only login, and every table and column core relies on is present and readable\n"
    } else {
        "✘ SQL Server check FAILED: core will not use this connection\n"
    });
    Ok((out, all_ok))
}

/// Renders the login audit; returns `false` when the login can write.
fn render_audit(out: &mut String, login: &str, writes: &[String]) -> bool {
    if writes.is_empty() {
        let _ = writeln!(out, "  ✔ login {login} is read-only (no write, DDL or EXECUTE permission)");
        return true;
    }
    let _ = writeln!(out, "  ✘ login {login} can WRITE: {}", writes.join(", "));
    let _ = writeln!(out, "    core refuses it: create a read-only login (services/core/sql/readonly-login.sql)");
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config_with_quotes_and_defaults() {
        let cfg =
            parse_config("SQLSERVER_USER=BOT\nSQLSERVER_PASSWORD=\"p@ss#w\"\nSQLSERVER_HOST=192.168.1.249\nSQLSERVER_DB=Sage\n").unwrap();
        assert_eq!((cfg.host.as_str(), cfg.port, cfg.user.as_str(), cfg.password.as_str()), ("192.168.1.249", 1433, "BOT", "p@ss#w"));
        assert_eq!((cfg.database.as_str(), cfg.encrypt, cfg.trust_cert), ("Sage", Encrypt::Required, true));
        let tuned = parse_config("SQLSERVER_HOST=h\nSQLSERVER_USER=u\nSQLSERVER_PASSWORD=p\nSQLSERVER_DB=d\nSQLSERVER_PORT=1500\nSQLSERVER_ENCRYPT=none\nSQLSERVER_TRUST_CERT=false\n").unwrap();
        assert_eq!((tuned.port, tuned.encrypt, tuned.trust_cert), (1500, Encrypt::None, false));
        assert!(!format!("{cfg:?}").contains("p@ss#w"), "password must be redacted");
        for bad in [
            "SQLSERVER_HOST=h",
            "SQLSERVER_HOST=h\nSQLSERVER_USER=u\nSQLSERVER_PASSWORD=p\nSQLSERVER_DB=d\nSQLSERVER_ENCRYPT=maybe",
            "OTHER=1",
        ] {
            assert!(parse_config(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn type_families() {
        assert_eq!(kind_of("NVARCHAR"), Some(Kind::Text));
        assert_eq!(kind_of("smallint"), Some(Kind::Int));
        assert_eq!(kind_of("decimal"), Some(Kind::Decimal));
        assert_eq!(kind_of("datetime2"), Some(Kind::DateTime));
        assert_eq!(kind_of("uniqueidentifier"), None);
    }

    #[test]
    fn compares_expected_and_found_columns() {
        let table =
            Table { name: "T", columns: &[("A", Kind::Text, 20), ("B", Kind::Int, 0), ("C", Kind::Decimal, 0), ("D", Kind::Text, 10)] };
        let found: Vec<Found> =
            vec![("a".into(), "varchar".into(), Some(15)), ("B".into(), "varchar".into(), Some(5)), ("D".into(), "text".into(), Some(-1))];
        let diff = compare(&table, &found);
        assert_eq!(diff.missing, ["C"]);
        assert_eq!(diff.wrong_type, [("B", Kind::Int, "varchar".to_owned())]);
        assert_eq!(diff.shorter, [("A", 20, 15)], "max (-1) never counts as shorter");
    }

    #[test]
    fn renders_failures_and_success() {
        let table = &SAGE_TABLES[0];
        let mut out = String::new();
        assert!(!render_table(&mut out, table, &TableCheck::default()));
        assert!(out.contains("Clientes: table not found"));
        let columns: Vec<Found> = table
            .columns
            .iter()
            .map(|(n, _, len)| ((*n).to_owned(), if *n == "CodigoEmpresa" { "smallint" } else { "varchar" }.to_owned(), Some(*len)))
            .collect();
        let mut check = TableCheck { schema: Some("dbo".into()), columns, rows: Some(42), can_select: true, can_write: false };
        let mut out = String::new();
        assert!(render_table(&mut out, table, &check), "{out}");
        assert!(out.contains("✔ dbo.Clientes: 13 columns (13 used by core) · ~42 rows · SELECT yes · read-only"), "{out}");
        check.can_write = true;
        let mut out = String::new();
        assert!(!render_table(&mut out, table, &check), "a writable table must fail the check");
        assert!(out.contains("✘ login can WRITE this table"), "{out}");
    }

    #[test]
    fn audit_report() {
        let mut out = String::new();
        assert!(render_audit(&mut out, "kapa_ro", &[]));
        assert!(out.contains("✔ login kapa_ro is read-only"));
        let mut out = String::new();
        assert!(!render_audit(&mut out, "BOT", &["INSERT".into(), "member of db_owner".into()]));
        assert!(out.contains("✘ login BOT can WRITE: INSERT, member of db_owner") && out.contains("readonly-login.sql"));
    }

    #[test]
    fn guard_accepts_every_statement_core_sends() {
        for sql in [SERVER_INFO, COLUMNS, TABLE_STATS, WRITE_PERMISSIONS] {
            read_only(sql).unwrap_or_else(|e| panic!("{e}: {sql}"));
        }
        read_only("select Nombre from Clientes where CodigoCliente = @P1").unwrap();
        read_only("SELECT 'DELETE; DROP TABLE x' AS harmless_literal").unwrap();
    }

    #[test]
    fn guard_refuses_anything_that_could_write() {
        for sql in [
            "UPDATE Clientes SET Nombre = 'x'",
            "DELETE FROM Clientes",
            "INSERT INTO Clientes VALUES (1)",
            "MERGE Clientes AS t USING x AS s ON 1 = 1 WHEN MATCHED THEN DELETE",
            "SELECT * INTO Copia FROM Clientes",
            "SELECT 1; DELETE FROM Clientes",
            "SELECT 1 -- comment",
            "SELECT 1 /* c */",
            "EXEC sp_who",
            "SELECT * FROM OPENROWSET('SQLNCLI', 'x', 'SELECT 1')",
            "SELECT * FROM x WHERE 1 = 1 AND EXISTS (SELECT 1) UNION SELECT xp_cmdshell",
            "WITH c AS (SELECT 1 AS a) DELETE FROM Clientes",
            "TRUNCATE TABLE Clientes",
            "DROP TABLE Clientes",
            "  ",
            "SELECTx FROM t",
            "SELECT 1 WAITFOR DELAY '00:00:05'",
            "SET NOCOUNT ON",
        ] {
            assert!(read_only(sql).is_err(), "guard let through: {sql}");
        }
    }
}
