//! Column statistics of the Sage tables core reads, computed on the server over the FULL
//! tables (a 10-order sample says nothing about 1.6 million lines): nulls, empties, distinct
//! values, ranges, text lengths, zeros, and the top values of low-cardinality columns.
//! One scan per table computes every aggregate (`NOLOCK`, through the read-only guard).
//! Personal columns only get counts: never min/max or top values, which would expose them.

use crate::sample::{PERSONAL, cell_json, json_str};
use crate::sqlserver::{Found, Kind, ReadOnlySession, Table, cell};
use crate::{Error, Result};
use tiberius::Row;

/// Columns with at most this many distinct values get their top values listed.
const TOP_DISTINCT: i32 = 50;

/// Whether `name` holds personal data (counts only, never values).
#[must_use]
pub fn personal(name: &str) -> bool {
    PERSONAL.iter().any(|p| p.eq_ignore_ascii_case(name))
}

/// Large objects (`text`, `ntext`, `image`, `xml`, `(max)`) allow no DISTINCT, MIN or MAX.
fn lob(data_type: &str, len: Option<i32>) -> bool {
    matches!(data_type.to_ascii_lowercase().as_str(), "text" | "ntext" | "image" | "xml") || len == Some(-1)
}

/// SQL aggregates of one column, aliased `<column>__<metric>`.
#[must_use]
pub fn metrics(name: &str, kind: Kind, lob: bool, personal: bool) -> Vec<String> {
    let c = format!("[{name}]");
    let mut m = vec![format!("SUM(CASE WHEN {c} IS NULL THEN 1 ELSE 0 END) AS [{name}__nulls]")];
    if kind == Kind::Text {
        let e = format!("CAST({c} AS varchar(max))");
        m.push(format!("SUM(CASE WHEN LTRIM(RTRIM({e})) = '' THEN 1 ELSE 0 END) AS [{name}__empty]"));
        m.push(format!("MIN(LEN({e})) AS [{name}__min_len]"));
        m.push(format!("MAX(LEN({e})) AS [{name}__max_len]"));
    } else if kind != Kind::DateTime {
        m.push(format!("SUM(CASE WHEN {c} = 0 THEN 1 ELSE 0 END) AS [{name}__zeros]"));
    }
    if !lob {
        m.push(format!("COUNT(DISTINCT {c}) AS [{name}__distinct]"));
    }
    if !lob && !personal {
        m.push(format!("MIN({c}) AS [{name}__min]"));
        m.push(format!("MAX({c}) AS [{name}__max]"));
    }
    m
}

/// The one-scan statistics query of `table`, for the columns core uses that exist in `found`.
#[must_use]
pub fn stats_sql(table: &Table, found: &[Found]) -> String {
    let parts: Vec<String> = table
        .columns
        .iter()
        .filter_map(|&(name, kind, _)| {
            let (_, data_type, len) = found.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(name))?;
            Some(metrics(name, kind, lob(data_type, *len), personal(name)).join(", "))
        })
        .collect();
    format!("SELECT COUNT_BIG(*) AS [rows], {} FROM [{}] WITH (NOLOCK)", parts.join(", "), table.name)
}

/// The 15 most frequent values of one column.
#[must_use]
pub fn top_sql(table: &str, column: &str) -> String {
    format!(
        "SELECT TOP 15 [{column}] AS [value], COUNT_BIG(*) AS [n] FROM [{table}] WITH (NOLOCK) GROUP BY [{column}] ORDER BY COUNT_BIG(*) DESC"
    )
}

/// Groups the `<column>__<metric>` cells of a statistics row per column, in order.
fn columns_json(row: &Row) -> Vec<(String, Vec<String>)> {
    let mut columns: Vec<(String, Vec<String>)> = Vec::new();
    for (column, data) in row.cells() {
        let Some((name, metric)) = column.name().rsplit_once("__") else { continue };
        let entry = format!("{}: {}", json_str(metric), cell_json(data, false));
        match columns.iter_mut().find(|(n, _)| n == name) {
            Some((_, metrics)) => metrics.push(entry),
            None => columns.push((name.to_owned(), vec![entry])),
        }
    }
    columns
}

/// The top values of one column as `[[value, count], ...]`.
async fn top_json(session: &mut ReadOnlySession, table: &str, column: &str) -> Result<String> {
    let rows = session.select(&top_sql(table, column), &[]).await?;
    let pairs: Vec<String> = rows
        .iter()
        .map(|r| {
            let value = r.cells().next().map_or_else(|| "null".to_owned(), |(_, d)| cell_json(d, false));
            format!("[{value}, {}]", cell::<i64, _>(r, 1).unwrap_or(0))
        })
        .collect();
    Ok(format!("[{}]", pairs.join(", ")))
}

/// Statistics of `table` as JSON: `{"rows": n, "columns": {"<column>": {metrics…, "top": […]}}}`.
///
/// # Errors
/// Query failure.
pub async fn table_stats(session: &mut ReadOnlySession, table: &Table, found: &[Found]) -> Result<String> {
    let rows = session.select(&stats_sql(table, found), &[]).await?;
    let row = rows.first().ok_or_else(|| Error::Protocol(format!("no statistics for {}", table.name)))?;
    let total = cell::<i64, _>(row, "rows").unwrap_or(0);
    let mut columns = columns_json(row);
    for (name, metrics) in &mut columns {
        // Large objects have no __distinct metric: cell() yields None instead of panicking.
        let distinct = cell::<i32, _>(row, format!("{name}__distinct").as_str());
        if !personal(name) && distinct.is_some_and(|d| (1..=TOP_DISTINCT).contains(&d)) {
            metrics.push(format!("\"top\": {}", top_json(session, table.name, name).await?));
        }
    }
    let lines: Vec<String> = columns.iter().map(|(n, m)| format!("        {}: {{{}}}", json_str(n), m.join(", "))).collect();
    Ok(format!("{{\"rows\": {total}, \"columns\": {{\n{}\n      }}}}", lines.join(",\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlserver::{SAGE_TABLES, read_only};

    /// The columns of `table` as the server would report them (LOBs as `text`).
    fn found(table: &Table) -> Vec<Found> {
        let lobs = ["DescripcionLinea", "ComentarioArticulo"];
        table
            .columns
            .iter()
            .map(|&(n, kind, len)| {
                let data_type = match kind {
                    Kind::Text if lobs.contains(&n) => "text",
                    Kind::Text => "varchar",
                    Kind::Int => "smallint",
                    Kind::Decimal => "decimal",
                    Kind::DateTime => "datetime",
                };
                (n.to_owned(), data_type.to_owned(), (kind == Kind::Text).then_some(if lobs.contains(&n) { 2_147_483_647 } else { len }))
            })
            .collect()
    }

    #[test]
    fn every_statistics_query_passes_the_read_only_guard() {
        for table in &SAGE_TABLES {
            let sql = stats_sql(table, &found(table));
            read_only(&sql).unwrap_or_else(|e| panic!("{e}: {sql}"));
            for &(column, _, _) in table.columns {
                read_only(&top_sql(table.name, column)).unwrap();
            }
        }
    }

    #[test]
    fn personal_columns_never_expose_values() {
        let m = metrics("Nombre", Kind::Text, false, true).join(" ");
        assert!(m.contains("Nombre__distinct") && m.contains("Nombre__max_len"), "{m}");
        assert!(!m.contains("Nombre__min]") && !m.contains("Nombre__max]"), "no min/max of a personal column: {m}");
        assert!(personal("EMail1") && personal("cifdni") && !personal("Provincia"));
    }

    #[test]
    fn large_objects_skip_distinct_and_ranges() {
        let sql = stats_sql(&SAGE_TABLES[1], &found(&SAGE_TABLES[1]));
        assert!(sql.contains("[DescripcionLinea__max_len]") && !sql.contains("[DescripcionLinea__distinct]"), "{sql}");
        assert!(sql.contains("[K_BOT__zeros]") && sql.contains("[K_BOT__distinct]") && sql.contains("[FechaAlta__min]"));
        assert!(sql.ends_with("FROM [Articulos] WITH (NOLOCK)"), "one scan per table");
    }
}
