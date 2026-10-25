//! Models of the Sage ERP tables core reads (read-only): clients, articles, order headers
//! and order lines. Shaped by the real data (`make deploy-sync-sqlserver` statistics):
//!
//! - Sage holds several companies in the same tables (`CodigoEmpresa`: 1, 2, 212, 7777…), so
//!   every key carries its company.
//! - Text columns are `NOT NULL` but often empty and padded: values are trimmed, and optional
//!   fields map `''` to `None`.
//! - `decimal(28,10)` amounts are exact fixed-point [`Decimal`]s (never floats; they can be
//!   negative: returns and credits). Dates are stored at midnight: [`Date`].
//!
//! Models decode from any [`Record`]: the SQL Server row in production, the fixture's JSON in
//! tests, so the same decoding is tested against real data.

use crate::sample::{DAYS_0001, DAYS_1900, decimal};
use crate::{Error, Result, mail};
use std::fmt;
use tiberius::{ColumnData, Row};

/// Fixed-point scale of Sage amounts (`decimal(28,10)`).
const SCALE: u32 = 10;

/// An exact Sage amount: an integer count of 10⁻¹⁰ units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Decimal(i128);

impl Decimal {
    /// Zero.
    pub const ZERO: Self = Self(0);

    /// From a mantissa and its scale (`value / 10^scale`); `None` on overflow or when a
    /// finer scale would lose digits.
    #[must_use]
    pub fn from_parts(value: i128, scale: u8) -> Option<Self> {
        let scale = u32::from(scale);
        if scale <= SCALE {
            return value.checked_mul(10i128.pow(SCALE - scale)).map(Self);
        }
        let divisor = 10i128.checked_pow(scale - SCALE)?;
        (value % divisor == 0).then(|| Self(value / divisor))
    }

    /// Parses `"-12.5"`, `"3"` or `"0.0000000000"`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (int, frac) = text.split_once('.').unwrap_or((text, ""));
        let digits = int.trim_start_matches(['-', '+']);
        if frac.len() > SCALE as usize || !frac.bytes().all(|b| b.is_ascii_digit()) || digits.is_empty() {
            return None;
        }
        let value: i128 = format!("{digits}{frac}").parse().ok()?;
        let signed = if int.starts_with('-') { -value } else { value };
        Self::from_parts(signed, u8::try_from(frac.len()).ok()?)
    }

    /// The raw count of 10⁻¹⁰ units.
    #[must_use]
    pub fn units(self) -> i128 {
        self.0
    }

    /// Whether the amount is zero.
    #[must_use]
    pub fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for Decimal {
    /// Exact, without trailing zeros: `19.99`, `36`, `-0.005`.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let full = decimal(self.0, 10);
        let trimmed = full.trim_end_matches('0').trim_end_matches('.');
        f.write_str(trimmed)
    }
}

/// A calendar date (Sage stores dates at midnight; any time of day is dropped).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    /// Days since 1970-01-01.
    days: i64,
}

impl Date {
    /// A valid calendar date, or `None`.
    #[must_use]
    pub fn from_ymd(year: i64, month: i64, day: i64) -> Option<Self> {
        let days = mail::days_from_civil(year, month, day);
        let (y, m, d) = mail::civil(days);
        (y == year && i64::try_from(m).ok()? == month && d == day).then_some(Self { days })
    }

    /// Parses the date part of `"2026-10-23"` or `"2026-10-23T00:00:00.000"`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.get(..10)?.split('-').map(str::parse::<i64>);
        Self::from_ymd(parts.next()?.ok()?, parts.next()?.ok()?, parts.next()?.ok()?)
    }

    /// Days since 1970-01-01.
    #[must_use]
    pub fn epoch_days(self) -> i64 {
        self.days
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let (y, m, d) = mail::civil(self.days);
        write!(f, "{y:04}-{m:02}-{d:02}")
    }
}

/// A source of named column values. `Ok(None)` is SQL `NULL`; a missing column or a value of
/// another type is an error (schema drift must surface, not decode silently).
pub trait Record {
    /// Text column.
    ///
    /// # Errors
    /// Missing column or not text.
    fn text(&self, column: &str) -> Result<Option<String>>;
    /// Integer column.
    ///
    /// # Errors
    /// Missing column or not an integer.
    fn int(&self, column: &str) -> Result<Option<i64>>;
    /// Decimal column.
    ///
    /// # Errors
    /// Missing column, not a decimal, or out of range.
    fn decimal(&self, column: &str) -> Result<Option<Decimal>>;
    /// Date column.
    ///
    /// # Errors
    /// Missing column or not a date.
    fn date(&self, column: &str) -> Result<Option<Date>>;
}

/// The error for a column holding an unexpected type.
fn mismatch(column: &str, expected: &str) -> Error {
    Error::Invalid(format!("Sage column {column}: expected {expected}"))
}

/// The value of `column` in a SQL Server row.
fn cell<'r>(row: &'r Row, column: &str) -> Result<&'r ColumnData<'static>> {
    row.cells()
        .find(|(c, _)| c.name().eq_ignore_ascii_case(column))
        .map(|(_, data)| data)
        .ok_or_else(|| Error::Invalid(format!("Sage column {column} missing")))
}

impl Record for Row {
    fn text(&self, column: &str) -> Result<Option<String>> {
        match cell(self, column)? {
            ColumnData::String(v) => Ok(v.as_ref().map(ToString::to_string)),
            _ => Err(mismatch(column, "text")),
        }
    }

    fn int(&self, column: &str) -> Result<Option<i64>> {
        match cell(self, column)? {
            ColumnData::U8(v) => Ok(v.map(i64::from)),
            ColumnData::I16(v) => Ok(v.map(i64::from)),
            ColumnData::I32(v) => Ok(v.map(i64::from)),
            ColumnData::I64(v) => Ok(*v),
            _ => Err(mismatch(column, "an integer")),
        }
    }

    fn decimal(&self, column: &str) -> Result<Option<Decimal>> {
        match cell(self, column)? {
            ColumnData::Numeric(v) => {
                v.map(|n| Decimal::from_parts(n.value(), n.scale()).ok_or_else(|| mismatch(column, "a decimal in range"))).transpose()
            }
            _ => Err(mismatch(column, "a decimal")),
        }
    }

    fn date(&self, column: &str) -> Result<Option<Date>> {
        let days = match cell(self, column)? {
            ColumnData::DateTime(v) => v.map(|t| DAYS_1900 + i64::from(t.days())),
            ColumnData::SmallDateTime(v) => v.map(|t| DAYS_1900 + i64::from(t.days())),
            ColumnData::DateTime2(v) => v.map(|t| DAYS_0001 + i64::from(t.date().days())),
            ColumnData::Date(v) => v.map(|t| DAYS_0001 + i64::from(t.days())),
            _ => return Err(mismatch(column, "a date")),
        };
        Ok(days.map(|days| Date { days }))
    }
}

/// A `NOT NULL` value that came back `NULL`.
fn required<T>(value: Option<T>, column: &str) -> Result<T> {
    value.ok_or_else(|| Error::Invalid(format!("Sage column {column} is NULL")))
}

/// Trimmed text of a `NOT NULL` column (may be empty).
fn text(r: &impl Record, column: &str) -> Result<String> {
    Ok(required(r.text(column)?, column)?.trim().to_owned())
}

/// Trimmed text, `None` when NULL or empty.
fn optional(r: &impl Record, column: &str) -> Result<Option<String>> {
    Ok(r.text(column)?.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()))
}

/// A `smallint` column.
fn small(r: &impl Record, column: &str) -> Result<i16> {
    i16::try_from(required(r.int(column)?, column)?).map_err(|_| mismatch(column, "a smallint"))
}

/// An `int` column.
fn int(r: &impl Record, column: &str) -> Result<i32> {
    i32::try_from(required(r.int(column)?, column)?).map_err(|_| mismatch(column, "an int"))
}

/// A `NOT NULL` decimal column.
fn amount(r: &impl Record, column: &str) -> Result<Decimal> {
    required(r.decimal(column)?, column)
}

/// Several optional text columns at once, in order.
fn optionals<const N: usize>(r: &impl Record, columns: [&str; N]) -> Result<[Option<String>; N]> {
    let mut values: [Option<String>; N] = std::array::from_fn(|_| None);
    for (value, column) in values.iter_mut().zip(columns) {
        *value = optional(r, column)?;
    }
    Ok(values)
}

/// Several `NOT NULL` decimal columns at once, in order.
fn amounts<const N: usize>(r: &impl Record, columns: [&str; N]) -> Result<[Decimal; N]> {
    let mut values = [Decimal::ZERO; N];
    for (value, column) in values.iter_mut().zip(columns) {
        *value = amount(r, column)?;
    }
    Ok(values)
}

/// The non-empty values of several text columns (phones, e-mails).
fn several(r: &impl Record, columns: &[&str]) -> Result<Vec<String>> {
    columns.iter().filter_map(|c| optional(r, c).transpose()).collect()
}

/// Company code (`CodigoEmpresa`): Sage keeps several companies in the same tables.
pub type Company = i16;

/// Identifies a client: `(CodigoEmpresa, CodigoCliente)`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientKey {
    /// `CodigoEmpresa`.
    pub company: Company,
    /// `CodigoCliente`, e.g. `"000890"`.
    pub code: String,
}

/// Identifies an article: `(CodigoEmpresa, CodigoArticulo)`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArticleKey {
    /// `CodigoEmpresa`.
    pub company: Company,
    /// `CodigoArticulo`, e.g. `"12-00000601"` (empty on text-only order lines).
    pub code: String,
}

/// Identifies an order: `(CodigoEmpresa, EjercicioPedido, SeriePedido, NumeroPedido)`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrderKey {
    /// `CodigoEmpresa`.
    pub company: Company,
    /// `EjercicioPedido` (fiscal year).
    pub year: i16,
    /// `SeriePedido` (`"A"`, `"EDI"`, `"M"`…; empty is a valid series).
    pub series: String,
    /// `NumeroPedido`.
    pub number: i32,
}

/// The key columns of an order, shared by headers and lines.
fn order_key(r: &impl Record) -> Result<OrderKey> {
    Ok(OrderKey {
        company: small(r, "CodigoEmpresa")?,
        year: small(r, "EjercicioPedido")?,
        series: text(r, "SeriePedido")?,
        number: int(r, "NumeroPedido")?,
    })
}

/// A Sage client (`Clientes`). Contains personal data: never log it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Client {
    /// Company and client code.
    pub key: ClientKey,
    /// `Nombre`.
    pub name: String,
    /// Non-empty `Telefono`, `Telefono2`, `Telefono3`, in that order.
    pub phones: Vec<String>,
    /// Non-empty `EMail1`, `EMail2`.
    pub emails: Vec<String>,
    /// `Domicilio`.
    pub address: Option<String>,
    /// `CodigoPostal`.
    pub postal_code: Option<String>,
    /// `Municipio`.
    pub city: Option<String>,
    /// `Provincia`.
    pub province: Option<String>,
    /// `CifDni` (tax id).
    pub tax_id: Option<String>,
}

impl Client {
    /// The columns [`Client::from_record`] reads.
    pub const COLUMNS: &str = "CodigoCliente, CodigoEmpresa, Nombre, Telefono, Telefono2, Telefono3, EMail1, EMail2, Domicilio, CodigoPostal, Municipio, Provincia, CifDni";

    /// Decodes a `Clientes` row.
    ///
    /// # Errors
    /// A missing column, a `NULL` in a `NOT NULL` column, or a value of another type.
    pub fn from_record(r: &impl Record) -> Result<Self> {
        let [address, postal_code, city, province, tax_id] =
            optionals(r, ["Domicilio", "CodigoPostal", "Municipio", "Provincia", "CifDni"])?;
        Ok(Self {
            key: ClientKey { company: small(r, "CodigoEmpresa")?, code: text(r, "CodigoCliente")? },
            name: text(r, "Nombre")?,
            phones: several(r, &["Telefono", "Telefono2", "Telefono3"])?,
            emails: several(r, &["EMail1", "EMail2"])?,
            address,
            postal_code,
            city,
            province,
            tax_id,
        })
    }
}

/// Description words that mark non-product articles (lots, promos, displays…).
const STOPWORDS: [&str; 8] = ["LOTE", "INTRO DEAL", "BONIF", "PROMO", "EXPOSITOR", "DISPLAY", "TESTER", "OBSEQUIO"];

/// A Sage article (`Articulos`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Article {
    /// Company and article code.
    pub key: ArticleKey,
    /// `DescripcionArticulo`.
    pub description: String,
    /// `Descripcion2Articulo` (empty in 94 % of articles).
    pub description2: Option<String>,
    /// `DescripcionLinea` (long text).
    pub line_description: Option<String>,
    /// `ComentarioArticulo` (long text).
    pub comment: Option<String>,
    /// `MarcaProducto`.
    pub brand: Option<String>,
    /// `K_BOT = -1`: enabled for the bot (`0` = disabled).
    pub bot_enabled: bool,
    /// `CodigoAreaCompetenciaLc`: `PRO`, `RET-PRO`, `RET`, `MERCH`, `REC`…
    pub area: Option<String>,
    /// `CodigoAlternativo`: usually the EAN, sometimes free text.
    pub alternative_code: Option<String>,
    /// `FechaAlta` (NULL in a few broken rows).
    pub created: Option<Date>,
    /// `PrecioVenta` (often 0: prices usually come from tariffs).
    pub sale_price: Decimal,
    /// `StockMinimo`.
    pub stock_min: Decimal,
    /// `StockMaximo`.
    pub stock_max: Decimal,
}

impl Article {
    /// The columns [`Article::from_record`] reads.
    pub const COLUMNS: &str = "CodigoArticulo, CodigoEmpresa, DescripcionArticulo, Descripcion2Articulo, DescripcionLinea, \
                               ComentarioArticulo, MarcaProducto, K_BOT, CodigoAreaCompetenciaLc, CodigoAlternativo, FechaAlta, \
                               PrecioVenta, StockMinimo, StockMaximo";

    /// Decodes an `Articulos` row.
    ///
    /// # Errors
    /// A missing column, a `NULL` in a `NOT NULL` column, or a value of another type.
    pub fn from_record(r: &impl Record) -> Result<Self> {
        let [description2, line_description, comment, brand, area, alternative_code] = optionals(
            r,
            [
                "Descripcion2Articulo",
                "DescripcionLinea",
                "ComentarioArticulo",
                "MarcaProducto",
                "CodigoAreaCompetenciaLc",
                "CodigoAlternativo",
            ],
        )?;
        let [sale_price, stock_min, stock_max] = amounts(r, ["PrecioVenta", "StockMinimo", "StockMaximo"])?;
        Ok(Self {
            key: ArticleKey { company: small(r, "CodigoEmpresa")?, code: text(r, "CodigoArticulo")? },
            description: text(r, "DescripcionArticulo")?,
            description2,
            line_description,
            comment,
            brand,
            bot_enabled: small(r, "K_BOT")? == -1,
            area,
            alternative_code,
            created: r.date("FechaAlta")?,
            sale_price,
            stock_min,
            stock_max,
        })
    }

    /// Whether the bot may sell it (`es_vendible`): enabled (`K_BOT = -1`), not ERP
    /// merchandising (`MERCH`), with a description free of non-product words.
    #[must_use]
    pub fn is_sellable(&self) -> bool {
        let description = self.description.to_uppercase();
        self.bot_enabled
            && !self.area.as_deref().is_some_and(|a| a.eq_ignore_ascii_case("MERCH"))
            && !description.is_empty()
            && !STOPWORDS.iter().any(|w| description.contains(w))
    }

    /// The EAN/GTIN in `CodigoAlternativo`, when it is one (8, 12, 13 or 14 digits).
    #[must_use]
    pub fn ean(&self) -> Option<&str> {
        self.alternative_code.as_deref().filter(|c| matches!(c.len(), 8 | 12 | 13 | 14) && c.bytes().all(|b| b.is_ascii_digit()))
    }
}

/// A Sage order header (`CabeceraPedidoCliente`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderHeader {
    /// Company, fiscal year, series and number.
    pub key: OrderKey,
    /// The client (`CodigoEmpresa`, `CodigoCliente`).
    pub client: ClientKey,
    /// `FechaPedido`.
    pub date: Date,
    /// `NumeroLineas`.
    pub line_count: i16,
    /// `RazonSocial`: the client's business name, copied onto the order (personal data).
    pub client_name: String,
    /// `ImporteLiquido`: order total (negative for credits).
    pub total: Decimal,
}

impl OrderHeader {
    /// The columns [`OrderHeader::from_record`] reads.
    pub const COLUMNS: &str =
        "CodigoEmpresa, EjercicioPedido, SeriePedido, NumeroPedido, CodigoCliente, FechaPedido, NumeroLineas, RazonSocial, ImporteLiquido";

    /// Decodes a `CabeceraPedidoCliente` row.
    ///
    /// # Errors
    /// A missing column, a `NULL` in a `NOT NULL` column, or a value of another type.
    pub fn from_record(r: &impl Record) -> Result<Self> {
        let key = order_key(r)?;
        Ok(Self {
            client: ClientKey { company: key.company, code: text(r, "CodigoCliente")? },
            date: required(r.date("FechaPedido")?, "FechaPedido")?,
            line_count: small(r, "NumeroLineas")?,
            client_name: text(r, "RazonSocial")?,
            total: amount(r, "ImporteLiquido")?,
            key,
        })
    }
}

/// A Sage order line (`LineasPedidoCliente`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderLine {
    /// The order it belongs to.
    pub order: OrderKey,
    /// `Orden`: position within the order.
    pub position: i16,
    /// The article (`CodigoEmpresa`, `CodigoArticulo`); empty code on text-only lines.
    pub article: ArticleKey,
    /// `DescripcionArticulo`, frozen at order time (may differ from today's catalog).
    pub description: String,
    /// `Descripcion2Articulo`.
    pub description2: Option<String>,
    /// `CodigodelCliente`: the client's own reference for the article.
    pub client_reference: Option<String>,
    /// `UnidadesPedidas` (negative on returns).
    pub quantity: Decimal,
    /// `Precio` (unit price).
    pub price: Decimal,
    /// `ImporteNeto`: line amount after discounts.
    pub net_amount: Decimal,
}

impl OrderLine {
    /// The columns [`OrderLine::from_record`] reads.
    pub const COLUMNS: &str = "CodigoEmpresa, EjercicioPedido, SeriePedido, NumeroPedido, Orden, CodigoArticulo, \
                               DescripcionArticulo, Descripcion2Articulo, CodigodelCliente, UnidadesPedidas, Precio, ImporteNeto";

    /// Decodes a `LineasPedidoCliente` row.
    ///
    /// # Errors
    /// A missing column, a `NULL` in a `NOT NULL` column, or a value of another type.
    pub fn from_record(r: &impl Record) -> Result<Self> {
        let order = order_key(r)?;
        let [description2, client_reference] = optionals(r, ["Descripcion2Articulo", "CodigodelCliente"])?;
        let [quantity, price, net_amount] = amounts(r, ["UnidadesPedidas", "Precio", "ImporteNeto"])?;
        Ok(Self {
            position: small(r, "Orden")?,
            article: ArticleKey { company: order.company, code: text(r, "CodigoArticulo")? },
            description: text(r, "DescripcionArticulo")?,
            description2,
            client_reference,
            quantity,
            price,
            net_amount,
            order,
        })
    }

    /// A text-only line (no article: comments, notes).
    #[must_use]
    pub fn is_text_line(&self) -> bool {
        self.article.code.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A test value.
    enum V {
        T(&'static str),
        I(i64),
        D(&'static str),
        Null,
    }

    /// An in-memory record.
    struct Map(HashMap<&'static str, V>);

    impl Record for Map {
        fn text(&self, c: &str) -> Result<Option<String>> {
            match self.0.get(c) {
                Some(V::T(t)) => Ok(Some((*t).to_owned())),
                Some(V::Null) => Ok(None),
                _ => Err(mismatch(c, "text")),
            }
        }
        fn int(&self, c: &str) -> Result<Option<i64>> {
            match self.0.get(c) {
                Some(V::I(i)) => Ok(Some(*i)),
                _ => Err(mismatch(c, "an integer")),
            }
        }
        fn decimal(&self, c: &str) -> Result<Option<Decimal>> {
            match self.0.get(c) {
                Some(V::D(d)) => Ok(Decimal::parse(d)),
                _ => Err(mismatch(c, "a decimal")),
            }
        }
        fn date(&self, c: &str) -> Result<Option<Date>> {
            match self.0.get(c) {
                Some(V::T(t)) => Ok(Date::parse(t)),
                Some(V::Null) => Ok(None),
                _ => Err(mismatch(c, "a date")),
            }
        }
    }

    /// An article row, with `overrides` replacing defaults.
    fn article(overrides: Vec<(&'static str, V)>) -> Map {
        let mut m: HashMap<&'static str, V> = HashMap::from([
            ("CodigoArticulo", V::T(" 12-00000601 ")),
            ("CodigoEmpresa", V::I(1)),
            ("DescripcionArticulo", V::T("KATIVA MASCARILLA 300ML")),
            ("Descripcion2Articulo", V::T("")),
            ("DescripcionLinea", V::Null),
            ("ComentarioArticulo", V::T("  ")),
            ("MarcaProducto", V::T("KATIVA")),
            ("K_BOT", V::I(-1)),
            ("CodigoAreaCompetenciaLc", V::T("RET-PRO")),
            ("CodigoAlternativo", V::T("7750075052949")),
            ("FechaAlta", V::T("2022-11-30T00:00:00.000")),
            ("PrecioVenta", V::D("0.0000000000")),
            ("StockMinimo", V::D("1147.0000000000")),
            ("StockMaximo", V::D("2294")),
        ]);
        m.extend(overrides);
        Map(m)
    }

    #[test]
    fn decimals_are_exact() {
        let d = Decimal::parse("3168.5800000000").unwrap();
        assert_eq!(d.to_string(), "3168.58");
        assert_eq!(Decimal::parse("-12172.69").unwrap().to_string(), "-12172.69");
        assert_eq!(Decimal::parse("36").unwrap().to_string(), "36");
        assert_eq!(Decimal::from_parts(1999, 2), Decimal::parse("19.99"));
        assert_eq!(Decimal::from_parts(1_230_000_000_000, 12), Decimal::parse("1.23"), "finer scale, exact");
        assert_eq!(Decimal::from_parts(1, 12), None, "finer scale that would lose digits");
        assert!(Decimal::parse("0.0000000000").unwrap().is_zero());
        for bad in ["", "-", "1.2.3", "1,5", "1.00000000001", "abc"] {
            assert_eq!(Decimal::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn dates_validate_and_print() {
        assert_eq!(Date::parse("2026-10-23T00:00:00.000").unwrap().to_string(), "2026-10-23");
        assert_eq!(Date::from_ymd(2024, 2, 29).map(|d| d.to_string()).as_deref(), Some("2024-02-29"));
        assert_eq!(Date::from_ymd(2025, 2, 29), None);
        assert_eq!(Date::parse("2026-13-01"), None);
        assert!(Date::parse("2012-09-03").unwrap() < Date::parse("2026-10-23").unwrap());
    }

    #[test]
    fn article_decodes_trimmed_with_empty_as_none() {
        let a = Article::from_record(&article(vec![])).unwrap();
        assert_eq!(a.key, ArticleKey { company: 1, code: "12-00000601".into() }, "codes are trimmed");
        assert_eq!((&a.description2, &a.line_description, &a.comment), (&None, &None, &None), "'' and NULL are both None");
        assert_eq!(a.created.unwrap().to_string(), "2022-11-30");
        assert_eq!((a.stock_min.to_string(), a.stock_max.to_string()), ("1147".into(), "2294".into()));
        assert_eq!(a.ean(), Some("7750075052949"));
        assert!(a.is_sellable());
    }

    #[test]
    fn sellable_matches_es_vendible() {
        let sellable = |o| Article::from_record(&article(o)).unwrap().is_sellable();
        assert!(!sellable(vec![("K_BOT", V::I(0))]), "K_BOT 0");
        assert!(!sellable(vec![("CodigoAreaCompetenciaLc", V::T("merch"))]), "MERCH, any case");
        assert!(!sellable(vec![("DescripcionArticulo", V::T("  "))]), "no description");
        assert!(!sellable(vec![("DescripcionArticulo", V::T("Expositor Kativa 2022"))]), "stopword, any case");
        assert!(sellable(vec![("CodigoAreaCompetenciaLc", V::T(""))]), "no area is fine");
        let free_text = Article::from_record(&article(vec![("CodigoAlternativo", V::T("ver 12-00002408"))])).unwrap();
        assert_eq!(free_text.ean(), None, "free text is not an EAN");
    }

    #[test]
    fn schema_drift_is_an_error() {
        let mut row = article(vec![]);
        row.0.remove("K_BOT");
        assert!(Article::from_record(&row).is_err(), "missing column");
        assert!(Article::from_record(&article(vec![("K_BOT", V::I(40_000))])).is_err(), "out of smallint range");
        assert!(Article::from_record(&article(vec![("DescripcionArticulo", V::Null)])).is_err(), "NULL in NOT NULL");
    }
}
