//! Offline tests on the Sage fixture `testdata/sage.local.json` (gitignored), exported from
//! the server with `make deploy-sync-sqlserver` (the SQL Server is only reachable there).
//! Skipped loudly when the file is absent (CI, fresh clones). These tests guard what core
//! assumes about Sage: the schema it relies on, coherent mock data, and masked personal data.

use kapa_core::sqlserver::{Found, SAGE_TABLES, compare};
use serde_json::Value;
use std::collections::HashSet;

/// Path of the fixture.
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/sage.local.json");

/// The parsed fixture, or `None` (with a notice) when it has not been exported.
fn fixture() -> Option<Value> {
    let Ok(text) = std::fs::read_to_string(FIXTURE) else {
        eprintln!("SAGE FIXTURE SKIPPED: {FIXTURE} not found (export it with `make deploy-sync-sqlserver`)");
        return None;
    };
    Some(serde_json::from_str(&text).unwrap_or_else(|e| panic!("{FIXTURE} is not valid JSON: {e}")))
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
    for field in ["generated_at", "server", "database", "masked", "orders", "schema", "sample"] {
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
