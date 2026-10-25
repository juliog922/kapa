//! Sage sample export (`core --sample-sqlserver`): the full schema of the tables core reads
//! and a small, coherent batch of rows (the latest orders, their lines, their clients and the
//! articles they reference, plus a few sellable catalog articles) as JSON, for tests and
//! development. Read-only like everything else (every query passes the guard) and gentle on
//! the ERP: `NOLOCK` reads never block Sage. Personal data is masked by default,
//! deterministically, so the same value always masks the same way and joins keep working.

use crate::sqlserver::{self, ReadOnlySession, SAGE_TABLES};
use crate::{Result, mail};
use ring::digest::{SHA256, digest};
use std::fmt::Write as _;
use tiberius::time::{Date, DateTime, DateTime2, SmallDateTime, Time};
use tiberius::{ColumnData, Row};

/// The `@P1` latest orders, as a derived table (shared by the sample queries).
macro_rules! latest_orders {
    () => {
        "(SELECT TOP (@P1) CodigoEmpresa, EjercicioPedido, SeriePedido, NumeroPedido, CodigoCliente \
         FROM CabeceraPedidoCliente WITH (NOLOCK) ORDER BY FechaPedido DESC, NumeroPedido DESC)"
    };
}

/// Full column metadata of one table.
const SCHEMA_COLUMNS: &str = concat!(
    "SELECT COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH, NUMERIC_PRECISION, NUMERIC_SCALE, IS_NULLABLE ",
    "FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_NAME = @P1 ORDER BY ORDINAL_POSITION",
);
/// The latest order headers (columns core maps).
const SAMPLE_HEADERS: &str = concat!(
    "SELECT TOP (@P1) CodigoEmpresa, EjercicioPedido, SeriePedido, NumeroPedido, CodigoCliente, FechaPedido, ",
    "NumeroLineas, RazonSocial, ImporteLiquido FROM CabeceraPedidoCliente WITH (NOLOCK) ",
    "ORDER BY FechaPedido DESC, NumeroPedido DESC",
);
/// The lines of the latest orders.
const SAMPLE_LINES: &str = concat!(
    "SELECT l.CodigoEmpresa, l.EjercicioPedido, l.SeriePedido, l.NumeroPedido, l.Orden, l.CodigoArticulo, ",
    "l.DescripcionArticulo, l.Descripcion2Articulo, l.CodigodelCliente, l.UnidadesPedidas, l.Precio, l.ImporteNeto ",
    "FROM LineasPedidoCliente l WITH (NOLOCK) JOIN ",
    latest_orders!(),
    " t ON l.CodigoEmpresa = t.CodigoEmpresa AND l.EjercicioPedido = t.EjercicioPedido ",
    "AND l.SeriePedido = t.SeriePedido AND l.NumeroPedido = t.NumeroPedido ",
    "ORDER BY l.EjercicioPedido, l.SeriePedido, l.NumeroPedido, l.Orden",
);
/// The clients of the latest orders.
const SAMPLE_CLIENTS: &str = concat!(
    "SELECT c.CodigoCliente, c.CodigoEmpresa, c.Nombre, c.Telefono, c.Telefono2, c.Telefono3, c.EMail1, c.EMail2, ",
    "c.Domicilio, c.CodigoPostal, c.Municipio, c.Provincia, c.CifDni FROM Clientes c WITH (NOLOCK) ",
    "WHERE EXISTS (SELECT 1 FROM ",
    latest_orders!(),
    " t WHERE t.CodigoEmpresa = c.CodigoEmpresa AND t.CodigoCliente = c.CodigoCliente) ORDER BY c.CodigoCliente",
);
/// Columns core maps from `Articulos`.
macro_rules! article_columns {
    () => {
        "a.CodigoArticulo, a.CodigoEmpresa, a.DescripcionArticulo, a.Descripcion2Articulo, a.DescripcionLinea, \
         a.ComentarioArticulo, a.MarcaProducto, a.K_BOT, a.CodigoAreaCompetenciaLc, a.CodigoAlternativo, a.FechaAlta, \
         a.PrecioVenta, a.StockMinimo, a.StockMaximo"
    };
}
/// The articles referenced by the lines of the latest orders.
const SAMPLE_ARTICLES: &str = concat!(
    "SELECT ",
    article_columns!(),
    " FROM Articulos a WITH (NOLOCK) WHERE EXISTS (SELECT 1 FROM LineasPedidoCliente l WITH (NOLOCK) JOIN ",
    latest_orders!(),
    " t ON l.CodigoEmpresa = t.CodigoEmpresa AND l.EjercicioPedido = t.EjercicioPedido ",
    "AND l.SeriePedido = t.SeriePedido AND l.NumeroPedido = t.NumeroPedido ",
    "WHERE l.CodigoEmpresa = a.CodigoEmpresa AND l.CodigoArticulo = a.CodigoArticulo) ORDER BY a.CodigoArticulo",
);
/// The newest sellable catalog articles (`K_BOT = -1`).
const SAMPLE_CATALOG: &str =
    concat!("SELECT TOP (@P1) ", article_columns!(), " FROM Articulos a WITH (NOLOCK) WHERE a.K_BOT = -1 ORDER BY a.FechaAlta DESC");

/// Version of the exported JSON layout. Bump it whenever the layout changes, so tests skip
/// (asking for a re-export) fixtures written by an older core instead of failing on them.
/// 1: schema + sample · 2: + stats.
pub const FORMAT: u32 = 2;

/// Columns holding personal data (masked unless the export is raw).
pub const PERSONAL: [&str; 9] = ["Nombre", "Telefono", "Telefono2", "Telefono3", "EMail1", "EMail2", "Domicilio", "CifDni", "RazonSocial"];

/// Days from 1900-01-01 (SQL Server `datetime` epoch) to 1970-01-01.
const DAYS_1900: i64 = -25_567;
/// Days from 0001-01-01 (`date` / `datetime2` epoch) to 1970-01-01.
const DAYS_0001: i64 = -719_162;

/// A JSON string literal.
#[must_use]
pub fn json_str(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A fixed-point decimal as an exact string (`value / 10^scale`).
#[must_use]
pub fn decimal(value: i128, scale: u8) -> String {
    let digits = format!("{:0>width$}", value.unsigned_abs(), width = usize::from(scale) + 1);
    let (int, frac) = digits.split_at(digits.len() - usize::from(scale));
    let sign = if value < 0 { "-" } else { "" };
    if frac.is_empty() { format!("{sign}{int}") } else { format!("{sign}{int}.{frac}") }
}

/// ISO 8601 timestamp (UTC, millisecond precision) from days since 1970 and nanoseconds of day.
#[must_use]
pub fn iso(epoch_days: i64, nanos: u64) -> String {
    let (y, m, d) = mail::civil(epoch_days);
    let (secs, millis) = (nanos / 1_000_000_000, nanos / 1_000_000 % 1000);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}", secs / 3600, secs % 3600 / 60, secs % 60)
}

/// Nanoseconds since midnight of a SQL Server `time` value.
fn time_nanos(t: Time) -> u64 {
    t.increments() * 10u64.pow(u32::from(9u8.saturating_sub(t.scale())))
}

/// `datetime`: days since 1900, 1/300 s fragments.
fn datetime(v: DateTime) -> String {
    iso(DAYS_1900 + i64::from(v.days()), u64::from(v.seconds_fragments()) * 10_000_000 / 3)
}

/// `smalldatetime`: days since 1900, minutes.
fn smalldatetime(v: SmallDateTime) -> String {
    iso(DAYS_1900 + i64::from(v.days()), u64::from(v.seconds_fragments()) * 60_000_000_000)
}

/// `datetime2`: date since year 1 plus time of day.
fn datetime2(v: DateTime2) -> String {
    iso(DAYS_0001 + i64::from(v.date().days()), time_nanos(v.time()))
}

/// A `date` value.
fn date(v: Date) -> String {
    iso(DAYS_0001 + i64::from(v.days()), 0)[..10].to_owned()
}

/// JSON for numeric column values; `None` when `data` is not numeric.
fn number_json(data: &ColumnData<'_>) -> Option<String> {
    let text = |v: Option<String>| v.unwrap_or_else(|| "null".to_owned());
    Some(match data {
        ColumnData::U8(v) => text(v.map(|n| n.to_string())),
        ColumnData::I16(v) => text(v.map(|n| n.to_string())),
        ColumnData::I32(v) => text(v.map(|n| n.to_string())),
        ColumnData::I64(v) => text(v.map(|n| n.to_string())),
        ColumnData::F32(v) => text(v.filter(|f| f.is_finite()).map(|f| f.to_string())),
        ColumnData::F64(v) => text(v.filter(|f| f.is_finite()).map(|f| f.to_string())),
        ColumnData::Numeric(v) => text(v.map(|n| json_str(&decimal(n.value(), n.scale())))),
        _ => return None,
    })
}

/// JSON for date/time column values (ISO 8601 strings); `None` when `data` is not temporal.
fn time_json(data: &ColumnData<'_>) -> Option<String> {
    let text = |v: Option<String>| v.map_or_else(|| "null".to_owned(), |s| json_str(&s));
    Some(match data {
        ColumnData::DateTime(v) => text(v.map(datetime)),
        ColumnData::SmallDateTime(v) => text(v.map(smalldatetime)),
        ColumnData::DateTime2(v) => text(v.map(datetime2)),
        ColumnData::Date(v) => text(v.map(date)),
        ColumnData::Time(v) => text(v.map(|t| iso(0, time_nanos(t))[11..].to_owned())),
        ColumnData::DateTimeOffset(v) => text(v.map(|t| format!("{}Z", datetime2(t.datetime2())))),
        _ => return None,
    })
}

/// JSON for any column value; text is masked when `mask` is set.
#[must_use]
pub fn cell_json(data: &ColumnData<'_>, mask: bool) -> String {
    if let Some(json) = number_json(data).or_else(|| time_json(data)) {
        return json;
    }
    match data {
        ColumnData::Bit(Some(b)) => b.to_string(),
        ColumnData::String(Some(s)) => json_str(&if mask { mask_text(s) } else { s.to_string() }),
        ColumnData::Guid(Some(g)) => json_str(&g.to_string()),
        ColumnData::Binary(Some(b)) => json_str(&format!("base64:{}", mail::base64(b))),
        ColumnData::Xml(Some(_)) => json_str("<xml>"),
        _ => "null".to_owned(),
    }
}

/// Masks personal text deterministically, keeping its shape: digits stay digits, letters stay
/// letters (same case), the rest is kept; e-mail domains become `example.com`.
#[must_use]
pub fn mask_text(value: &str) -> String {
    if let Some((local, _)) = value.split_once('@') {
        return format!("{}@example.com", mask_text(local));
    }
    let hash = digest(&SHA256, format!("kapa-mask:{value}").as_bytes());
    let bytes = hash.as_ref();
    value
        .chars()
        .zip(bytes.iter().cycle())
        .map(|(c, &h)| match c {
            '0'..='9' => char::from(b'0' + h % 10),
            c if c.is_lowercase() => char::from(b'a' + h % 26),
            c if c.is_alphabetic() => char::from(b'A' + h % 26),
            c => c,
        })
        .collect()
}

/// One row as a JSON object; personal columns are masked when `mask` is set.
fn row_json(row: &Row, mask: bool) -> String {
    let cells: Vec<String> = row
        .cells()
        .map(|(column, data)| {
            let personal = mask && PERSONAL.iter().any(|p| p.eq_ignore_ascii_case(column.name()));
            format!("{}: {}", json_str(column.name()), cell_json(data, personal))
        })
        .collect();
    format!("{{{}}}", cells.join(", "))
}

/// Runs one sample query and renders its rows as a JSON array (one row per line).
async fn rows(session: &mut ReadOnlySession, sql: &'static str, limit: Option<i32>, mask: bool) -> Result<(String, usize)> {
    let found = match limit {
        Some(n) => session.select(sql, &[&n]).await?,
        None => session.select(sql, &[]).await?,
    };
    let lines: Vec<String> = found.iter().map(|r| format!("      {}", row_json(r, mask))).collect();
    Ok((format!("[\n{}\n    ]", lines.join(",\n")), found.len()))
}

/// The `"schema"` entry of one table: approximate rows and every column.
async fn table_schema(session: &mut ReadOnlySession, table: &str) -> Result<String> {
    let check = sqlserver::check_table(session, table).await?;
    let columns = session.select(SCHEMA_COLUMNS, &[&table]).await?;
    let lines: Vec<String> = columns.iter().map(|r| format!("        {}", row_json(r, false))).collect();
    let rows = check.rows.map_or_else(|| "null".to_owned(), |n| n.to_string());
    let schema = check.schema.as_deref().map_or_else(|| "null".to_owned(), json_str);
    Ok(format!("{{\"schema\": {schema}, \"rows_approx\": {rows}, \"columns\": [\n{}\n      ]}}", lines.join(",\n")))
}

/// The `"schema"` and `"stats"` entries of every Sage table core reads.
async fn schema_and_stats(session: &mut ReadOnlySession) -> Result<(String, String)> {
    let (mut schema, mut stats) = (Vec::new(), Vec::new());
    for table in &SAGE_TABLES {
        schema.push(format!("    {}: {}", json_str(table.name), table_schema(session, table.name).await?));
        let found = sqlserver::check_table(session, table.name).await?.columns;
        stats.push(format!("    {}: {}", json_str(table.name), crate::stats::table_stats(session, table, &found).await?));
    }
    Ok((schema.join(",\n"), stats.join(",\n")))
}

/// Exports the schema of every Sage table core reads and a coherent sample of `orders`
/// latest orders as JSON. Returns the JSON and a one-line summary.
///
/// # Errors
/// Connection, permission (non read-only login) or query failure.
pub async fn export(cfg: &sqlserver::Config, orders: i32, mask: bool) -> Result<(String, String)> {
    let mut session = Box::pin(sqlserver::connect(cfg)).await?;
    let (version, database, login) = sqlserver::server_info(&mut session).await?;
    let mut out = format!(
        "{{\n  \"format\": {FORMAT},\n  \"generated_at\": {},\n  \"server\": {},\n  \"database\": {},\n  \"login\": {},\n  \"masked\": {mask},\n  \"orders\": {orders},\n  \"schema\": {{\n",
        json_str(&mail::rfc3339(crate::now_ms() / 1000)),
        json_str(&version),
        json_str(&database),
        json_str(&login)
    );
    let (schema, stats) = schema_and_stats(&mut session).await?;
    let _ = write!(out, "{schema}\n  }},\n  \"stats\": {{\n{stats}\n  }},\n  \"sample\": {{\n");
    let sections: [(&str, &'static str); 5] = [
        ("CabeceraPedidoCliente", SAMPLE_HEADERS),
        ("LineasPedidoCliente", SAMPLE_LINES),
        ("Clientes", SAMPLE_CLIENTS),
        ("Articulos", SAMPLE_ARTICLES),
        ("ArticulosCatalogo", SAMPLE_CATALOG),
    ];
    let (mut parts, mut summary) = (Vec::new(), Vec::new());
    for (name, sql) in sections {
        let (json, count) = rows(&mut session, sql, Some(orders), mask).await?;
        parts.push(format!("    {}: {json}", json_str(name)));
        summary.push(format!("{name} {count}"));
    }
    let _ = write!(out, "{}\n  }}\n}}\n", parts.join(",\n"));
    Ok((out, summary.join(" · ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;
    use tiberius::numeric::Numeric;

    #[test]
    fn every_sample_query_passes_the_read_only_guard() {
        for sql in [SCHEMA_COLUMNS, SAMPLE_HEADERS, SAMPLE_LINES, SAMPLE_CLIENTS, SAMPLE_ARTICLES, SAMPLE_CATALOG] {
            sqlserver::read_only(sql).unwrap_or_else(|e| panic!("{e}: {sql}"));
        }
    }

    #[test]
    fn json_and_decimals() {
        assert_eq!(json_str("a\"b\\c\nd\u{1}"), "\"a\\\"b\\\\c\\nd\\u0001\"");
        assert_eq!(decimal(12345, 2), "123.45");
        assert_eq!(decimal(-5, 3), "-0.005");
        assert_eq!(decimal(42, 0), "42");
    }

    #[test]
    fn sql_server_dates_match_reference_values() {
        // Reference values computed independently with Python's datetime.
        assert_eq!(datetime(DateTime::new(45_000, 300 * 3661 + 150)), "2023-03-17T01:01:01.500");
        assert_eq!(date(Date::new(738_000)), "2021-07-30");
        assert_eq!(smalldatetime(SmallDateTime::new(45_000, 61)), "2023-03-17T01:01:00.000");
        assert_eq!(datetime2(DateTime2::new(Date::new(738_000), Time::new(36_615_000_000, 7))), "2021-07-30T01:01:01.500");
    }

    #[test]
    fn cells_render_as_json() {
        assert_eq!(cell_json(&ColumnData::I16(Some(-1)), false), "-1");
        assert_eq!(cell_json(&ColumnData::I32(None), false), "null");
        assert_eq!(cell_json(&ColumnData::Numeric(Some(Numeric::new_with_scale(1999, 2))), false), "\"19.99\"");
        assert_eq!(cell_json(&ColumnData::Bit(Some(true)), false), "true");
        assert_eq!(cell_json(&ColumnData::String(Some(Cow::Borrowed("Ana"))), false), "\"Ana\"");
        assert_ne!(cell_json(&ColumnData::String(Some(Cow::Borrowed("Ana"))), true), "\"Ana\"");
        assert_eq!(cell_json(&ColumnData::F64(Some(f64::NAN)), false), "null");
    }

    #[test]
    fn masking_is_deterministic_and_keeps_shape() {
        let phone = mask_text("+34 600 111 222");
        assert_eq!(phone, mask_text("+34 600 111 222"), "deterministic");
        assert_ne!(phone, "+34 600 111 222");
        assert!(phone.starts_with('+') && phone.chars().filter(char::is_ascii_digit).count() == 11);
        let email = mask_text("Juan.Perez@empresa.es");
        assert!(email.ends_with("@example.com") && !email.contains("Juan"), "{email}");
        let name = mask_text("Ferretería López S.L.");
        assert_eq!(name.chars().count(), "Ferretería López S.L.".chars().count());
        assert!(name.contains(' ') && name.contains('.'));
    }
}
