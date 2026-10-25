//! Offline tests on the Sage fixture `testdata/sage.local.json` (gitignored), exported from
//! the server with `make deploy-sync-sqlserver` (the SQL Server is only reachable there).
//! Skipped loudly when the file is absent (CI, fresh clones). These tests guard what core
//! assumes about Sage: the schema it relies on, coherent mock data, and masked personal data.

use kapa_core::sage::{Article, Client, Date, Decimal, OrderHeader, OrderLine, Record};
use kapa_core::sample::FORMAT;
use kapa_core::sqlserver::{Found, SAGE_TABLES, compare};
use kapa_core::stats::personal;
use serde_json::Value;
use std::collections::HashSet;

/// Path of the fixture.
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/sage.local.json");

/// The parsed fixture, or `None` (with a notice) when it is absent or was written by an older
/// core (an older layout cannot be judged by today's expectations: it needs a re-export).
fn fixture() -> Option<Value> {
    let Ok(text) = std::fs::read_to_string(FIXTURE) else {
        eprintln!("SAGE FIXTURE SKIPPED: {FIXTURE} not found (export it with `make deploy-sync-sqlserver`)");
        return None;
    };
    let fx: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{FIXTURE} is not valid JSON: {e}"));
    let format = fx["format"].as_u64().unwrap_or(1);
    if format < u64::from(FORMAT) {
        eprintln!(
            "SAGE FIXTURE SKIPPED: STALE fixture (format {format}, this core expects {FORMAT}): deploy the current core, then `make deploy-sync-sqlserver`"
        );
        return None;
    }
    Some(fx)
}

/// Rows of one sample section.
fn rows<'a>(fx: &'a Value, section: &str) -> &'a [Value] {
    fx["sample"][section].as_array().map_or(&[], Vec::as_slice)
}

/// A composite key made of the given columns of `row`.
fn key(row: &Value, columns: &[&str]) -> String {
    columns.iter().map(|c| row[*c].to_string()).collect::<Vec<_>>().join("|")
}

/// The set of keys of `section`.
fn keys(fx: &Value, section: &str, columns: &[&str]) -> HashSet<String> {
    rows(fx, section).iter().map(|r| key(r, columns)).collect()
}

/// The order key shared by headers and lines.
const ORDER: [&str; 4] = ["CodigoEmpresa", "EjercicioPedido", "SeriePedido", "NumeroPedido"];

#[test]
fn fixture_has_the_exported_shape() {
    let Some(fx) = fixture() else { return };
    for field in ["format", "generated_at", "server", "database", "masked", "orders", "schema", "stats", "sample"] {
        assert!(!fx[field].is_null(), "fixture misses {field}");
    }
    for section in ["CabeceraPedidoCliente", "LineasPedidoCliente", "Clientes", "Articulos", "ArticulosCatalogo"] {
        assert!(fx["sample"][section].is_array(), "sample misses {section}");
    }
    eprintln!(
        "sage fixture: {} · exported {} · {} orders · {} lines · {} clients · {} articles · {} catalog",
        fx["database"].as_str().unwrap_or("?"),
        fx["generated_at"].as_str().unwrap_or("?"),
        rows(&fx, "CabeceraPedidoCliente").len(),
        rows(&fx, "LineasPedidoCliente").len(),
        rows(&fx, "Clientes").len(),
        rows(&fx, "Articulos").len(),
        rows(&fx, "ArticulosCatalogo").len()
    );
}

#[test]
fn schema_has_every_column_core_relies_on() {
    let Some(fx) = fixture() else { return };
    for table in &SAGE_TABLES {
        let columns = fx["schema"][table.name]["columns"].as_array().unwrap_or_else(|| panic!("schema misses {}", table.name));
        let found: Vec<Found> = columns
            .iter()
            .map(|c| {
                let text = |f: &str| c[f].as_str().unwrap_or_default().to_owned();
                let len = c["CHARACTER_MAXIMUM_LENGTH"].as_i64().and_then(|n| i32::try_from(n).ok());
                (text("COLUMN_NAME"), text("DATA_TYPE"), len)
            })
            .collect();
        let diff = compare(table, &found);
        assert!(diff.missing.is_empty() && diff.wrong_type.is_empty(), "{}: {diff:?}", table.name);
        if !diff.shorter.is_empty() {
            eprintln!("sage fixture: {} texts shorter than core assumes: {:?}", table.name, diff.shorter);
        }
    }
}

#[test]
fn statistics_cover_every_column_and_never_expose_personal_values() {
    let Some(fx) = fixture() else { return };
    for table in &SAGE_TABLES {
        let stats = &fx["stats"][table.name];
        assert!(stats["rows"].as_i64().is_some_and(|n| n > 0), "{}: no row count", table.name);
        for &(column, _, _) in table.columns {
            let metrics = &stats["columns"][column];
            assert!(metrics.is_object(), "{}.{column} has no statistics", table.name);
            if personal(column) {
                for value in ["min", "max", "top"] {
                    assert!(metrics[value].is_null(), "{}.{column} exposes {value}", table.name);
                }
            }
        }
    }
}

#[test]
fn sample_is_coherent() {
    let Some(fx) = fixture() else { return };
    let orders = keys(&fx, "CabeceraPedidoCliente", &ORDER);
    assert!(!orders.is_empty(), "the sample has no orders");
    for line in rows(&fx, "LineasPedidoCliente") {
        assert!(orders.contains(&key(line, &ORDER)), "line of an order outside the sample: {line}");
    }
    let clients = keys(&fx, "Clientes", &["CodigoEmpresa", "CodigoCliente"]);
    for header in rows(&fx, "CabeceraPedidoCliente") {
        assert!(clients.contains(&key(header, &["CodigoEmpresa", "CodigoCliente"])), "order without its client: {header}");
    }
    let articles = keys(&fx, "Articulos", &["CodigoEmpresa", "CodigoArticulo"]);
    let unknown: Vec<String> = rows(&fx, "LineasPedidoCliente")
        .iter()
        .filter(|l| l["CodigoArticulo"].as_str().is_some_and(|c| !c.trim().is_empty()))
        .map(|l| key(l, &["CodigoEmpresa", "CodigoArticulo"]))
        .filter(|k| !articles.contains(k))
        .collect();
    assert!(unknown.is_empty(), "lines reference articles missing from the sample: {unknown:?}");
}

#[test]
fn personal_data_is_masked() {
    let Some(fx) = fixture() else { return };
    if fx["masked"] != Value::Bool(true) {
        eprintln!("sage fixture: exported RAW (SAGE personal data unmasked), masking not checked");
        return;
    }
    for client in rows(&fx, "Clientes") {
        for field in ["EMail1", "EMail2"] {
            if let Some(email) = client[field].as_str().filter(|e| e.contains('@')) {
                assert!(email.ends_with("@example.com"), "{field} not masked: {email}");
            }
        }
    }
}

/// A fixture row as a Sage record: decimals are exported as strings, dates as ISO strings.
struct Json<'a>(&'a Value);

impl Json<'_> {
    /// The value of `column`; a missing column is an error, like on the server.
    fn field(&self, column: &str) -> kapa_core::Result<&Value> {
        self.0.get(column).ok_or_else(|| kapa_core::Error::Invalid(format!("fixture column {column} missing")))
    }

    /// A string value parsed with `parse`; `NULL` is `None`.
    fn parsed<T>(&self, column: &str, parse: impl Fn(&str) -> Option<T>) -> kapa_core::Result<Option<T>> {
        match self.field(column)? {
            Value::Null => Ok(None),
            Value::String(s) => parse(s).map(Some).ok_or_else(|| kapa_core::Error::Invalid(format!("{column}: unparsable {s}"))),
            other => Err(kapa_core::Error::Invalid(format!("{column}: unexpected {other}"))),
        }
    }
}

impl Record for Json<'_> {
    fn text(&self, column: &str) -> kapa_core::Result<Option<String>> {
        self.parsed(column, |s| Some(s.to_owned()))
    }

    fn int(&self, column: &str) -> kapa_core::Result<Option<i64>> {
        match self.field(column)? {
            Value::Null => Ok(None),
            v => v.as_i64().map(Some).ok_or_else(|| kapa_core::Error::Invalid(format!("{column}: not an integer: {v}"))),
        }
    }

    fn decimal(&self, column: &str) -> kapa_core::Result<Option<Decimal>> {
        self.parsed(column, Decimal::parse)
    }

    fn date(&self, column: &str) -> kapa_core::Result<Option<Date>> {
        self.parsed(column, Date::parse)
    }
}

/// Decodes every row of a sample section, failing with the row on the first error.
fn decode<T>(fx: &Value, section: &str, from: impl Fn(&Json) -> kapa_core::Result<T>) -> Vec<T> {
    rows(fx, section).iter().map(|r| from(&Json(r)).unwrap_or_else(|e| panic!("{section}: {e}\nrow: {r}"))).collect()
}

#[test]
fn every_sample_row_decodes_into_the_sage_models() {
    let Some(fx) = fixture() else { return };
    let clients = decode(&fx, "Clientes", |r| Client::from_record(r));
    let articles = decode(&fx, "Articulos", |r| Article::from_record(r));
    let catalog = decode(&fx, "ArticulosCatalogo", |r| Article::from_record(r));
    let headers = decode(&fx, "CabeceraPedidoCliente", |r| OrderHeader::from_record(r));
    let lines = decode(&fx, "LineasPedidoCliente", |r| OrderLine::from_record(r));

    let client_keys: HashSet<_> = clients.iter().map(|c| &c.key).collect();
    let order_keys: HashSet<_> = headers.iter().map(|h| &h.key).collect();
    let article_keys: HashSet<_> = articles.iter().map(|a| &a.key).collect();
    assert!(headers.iter().all(|h| client_keys.contains(&h.client)), "every order's client decodes");
    assert!(lines.iter().all(|l| order_keys.contains(&l.order)), "every line's order decodes");
    assert!(lines.iter().filter(|l| !l.is_text_line()).all(|l| article_keys.contains(&l.article)), "every article decodes");
    assert!(catalog.iter().all(|a| a.bot_enabled), "the catalog sample is K_BOT = -1");
    eprintln!(
        "sage models: {} clients · {} articles ({} sellable, {} with EAN) · catalog {} ({} sellable) · {} orders · {} lines ({} text-only)",
        clients.len(),
        articles.len(),
        articles.iter().filter(|a| a.is_sellable()).count(),
        articles.iter().filter(|a| a.ean().is_some()).count(),
        catalog.len(),
        catalog.iter().filter(|a| a.is_sellable()).count(),
        headers.len(),
        lines.len(),
        lines.iter().filter(|l| l.is_text_line()).count()
    );
}
