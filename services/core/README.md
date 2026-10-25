# Core service

The platform's main process, written in Rust. It exposes **no network port**: features run
inside it and talk to the other services as clients. Today it runs the **Gmail module**;
the Kapa **admin** mailbox is loaded apart, and its duties come later.

## Gmail module

Every account in `secrets/accounts.*.env` works like a WhatsApp account:

- **Receives**: one watcher per account (IMAP + IDLE push, reconnect with backoff 5 s → 5 min,
  cursor saved in PostgreSQL so nothing is lost or processed twice across restarts).
- **Filters**: per-account sender allowlist, deny by default (`gmail_allowed_senders`, cached in
  memory, ~40 ns per check). Filtered mail is counted, never stored or published.
- **Publishes**: allowed mail is stored (`gmail_messages`, `gmail_attachments`) and published on
  an in-process bus (`Gmail::subscribe`) with its full envelope: account, IMAP UID, Gmail
  message and thread IDs, Message-ID, from/to/cc/reply-to, subject, In-Reply-To/References,
  `sent_at` (Date header), `internal_at` (server time), `received_at`, text, HTML, attachments
  (name, type, bytes, SHA-256).
- **Sends**: `Gmail::send(from_account, Outgoing)` from any configured account (SMTP STARTTLS,
  text + attachments, replies thread via In-Reply-To), returning the Message-ID.

## SQL Server (Sage ERP, read-only)

Core reaches the Sage database with `tiberius` (SQL Server's TDS protocol in pure Rust, TLS via
the same rustls): **no ODBC driver** (`msodbcsql17` is not needed).

**Sage is never written.** Three independent layers, any one of which blocks a write:

1. **Types**: the SQL Server client is private to `sqlserver.rs`; the rest of core only gets a
   `ReadOnlySession` whose single operation is `select`.
2. **Statement guard**: every statement is checked before it is sent: exactly one `SELECT`,
   no `;`, no comments, no write/DDL/exec/escape keyword (`INSERT`, `UPDATE`, `DELETE`, `MERGE`,
   `INTO`, `EXEC`, `OPENROWSET`, `xp_`/`sp_`…). Unit tests prove every statement core sends
   passes and 18 write attempts are refused.
3. **Login audit**: on connect core asks SQL Server what the login may do (database
   permissions, `db_owner`/`db_datawriter`/`db_ddladmin`, `sysadmin`) and **refuses** a login
   that can write, alter or execute. Create one with `sql/readonly-login.sql` (run by a DBA).

Settings in `secrets/sqlserver.{dev,prod}.env` (template `sqlserver.example.env`):
`SQLSERVER_HOST`, `_PORT` (1433), `_DB`, `_USER`, `_PASSWORD` (quotes allowed),
`_ENCRYPT` = `required` (default) | `login` | `none`, `_TRUST_CERT` (default `true`).

The SQL Server is only reachable from prike, so its **live** checks run there, inside the
deployed core container (the same binary that will query Sage), driven from OMEN16:

```bash
make deploy-check-sqlserver     # live: read-only login audit + schema check (exit 0 = all good)
make deploy-sync-sqlserver      # live check, then export schema + masked sample (fixture)
```

Everything else is tested **offline** on that fixture (`tests/sage_fixture.rs`, part of
`make -C services/core test`): every column core relies on exists with a compatible type, the
sample is coherent (every line's order, every order's client and every referenced article are
present), and personal data is masked. Without the fixture (CI, fresh clone) they skip loudly.

The check connects, prints the server version, database and login, then verifies every table
and column core relies on (`Clientes`, `Articulos`, `CabeceraPedidoCliente`,
`LineasPedidoCliente`): presence, type family, text lengths (shorter = truncation warning),
approximate rows (from `sys.partitions`, no table scan), and whether the login can SELECT and
could write. Exit 0 when everything core needs is there.

If the server only speaks TLS 1.0/1.1 (old SQL Server versions), the login fails with a TLS
error: set `SQLSERVER_ENCRYPT=none` (no TLS; acceptable only inside the LAN/VPN).

## Sage models (`src/sage.rs`)

Read-only models of the four Sage tables: `Client`, `Article`, `OrderHeader`, `OrderLine`,
keyed by `ClientKey`, `ArticleKey` and `OrderKey`. Shaped by the real data:

- every key carries the company (`CodigoEmpresa`): Sage keeps several companies (1, 2, 212,
  7777…) in the same tables;
- text is trimmed and optional fields map `''` to `None` (columns are `NOT NULL` but often
  empty or padded);
- `decimal(28,10)` amounts are exact `Decimal`s (never floats; negative on returns and credits);
  dates are `Date`s (Sage stores them at midnight);
- `Article::is_sellable()` is `es_vendible` from the chatlink models: `K_BOT = -1`, not
  `MERCH`, a description free of non-product words. `K_BOT` alone is not enough (in the real
  sample, 4 of 10 `K_BOT = -1` articles are sellable); `Article::ean()` returns the EAN only when
  `CodigoAlternativo` really is one.

Models decode from any `Record`: the SQL Server row in production, the fixture's JSON in tests,
so `make -C services/core test` decodes every real sample row through the same code. A missing
column, a `NULL` in a `NOT NULL` column or an unexpected type is an error, never a silent default.

## Sage sample data (tests and development)

```bash
make deploy-sync-sqlserver                       # 10 latest orders (SAMPLE_ORDERS=50 for more)
```

Runs `core --sample-sqlserver` on prike and saves `services/core/testdata/sage.local.json`
plus a readable `sage-summary.local.md` (both **gitignored**; `make sage-summary` rebuilds the
summary). The JSON contains:

- `schema`: every column of `Clientes`, `Articulos`, `CabeceraPedidoCliente` and
  `LineasPedidoCliente` (type, length, precision, nullability) and approximate row counts;
- `stats`: computed on the server over the **full** tables (one `NOLOCK` scan per table) for
  every column core uses: nulls, empties, zeros, distinct values, min/max, text lengths, and the
  top 15 values of columns with at most 50 distinct values. Personal columns only get counts,
  never values;
- `sample`: a **coherent** batch: the latest orders, their lines, their clients and the articles
  they reference, plus the newest sellable catalog articles (`K_BOT = -1`). Only the columns core
  maps are exported, so unrelated sensitive data (bank details…) never leaves Sage.

Personal data (`Nombre`, phones, e-mails, `Domicilio`, `CifDni`, `RazonSocial`) is **masked by
default**, deterministically (same input, same mask: joins still work; digits stay digits,
e-mail domains become `example.com`). `SAMPLE_RAW=1` keeps it real and asks for confirmation.
Reads use `NOLOCK`, so sampling never blocks Sage users; every query passes the read-only guard.

## Secrets

`secrets/accounts.{dev,prod}.env` and `secrets/admin.{dev,prod}.env`, gitignored; only the
`*.example.env` templates are committed. In the container they are read from
`ACCOUNTS_FILE` / `ADMIN_FILE` (default `/run/secrets/core/{accounts,admin}.env`); a missing
file means "not configured". Passwords never appear in logs or `Debug` output.

## Production

Core runs in `docker-compose.yml` with `services/core/secrets` mounted read-only at
`/run/secrets/core`; `CORE_SECRETS` (default `prod`) picks `accounts.prod.env` and
`admin.prod.env`. The container user is uid 65532, so secrets must be owned by it with mode 0400:

```bash
make deploy-secrets     # uploads services/*/secrets/*.prod.env to prike (sudo there), restarts core
make monitor            # "Service checks" shows core: accounts, counters, last delivery delay
```

Without secrets core still starts (no accounts, admin not configured), so a deploy never
fails because of them. For a local `make run` on OMEN16: `make -C services/core secrets-perms`.

## Health (no TCP)

Core rewrites a status file every 5 s. `core --healthcheck` (container health) checks it is
fresh; `core --status` prints it for `make monitor`: counters and every account's state and
allowlist size, and the last delivery delay (Gmail filed the message → core had it); exit 2
(warning) when an account is not watching.

## Development

Only Docker is required. Dependencies, kept on purpose to the minimum: `tokio`, `sqlx`
(PostgreSQL), `tokio-rustls` + `webpki-roots` (Gmail requires TLS), `ring` (already pulled by
rustls; SHA-256 and randomness) and `mail-parser` (MIME parsing, zero dependencies).
IMAP, SMTP, base64 and dates are small hand-written subsets.

| Command | |
|---------|---|
| `make format` | rustfmt |
| `make lint` | rustfmt check, clippy pedantic with warnings as errors, lizard (CCN ≤ 10, ≤ 40 lines, ≤ 6 args) |
| `make check-docs` | `missing_docs` + clippy `missing_docs_in_private_items` + rustdoc `-D warnings` (Rust's Doxygen) |
| `make test` | unit tests (fake IMAP/SMTP servers) + integration (PostgreSQL, live Gmail) |
| `make bench` | hot paths under perf → `bench/report.txt` |

**Live test** (runs when `secrets/accounts.dev.env` has an account): the first account sends
itself a message with a non-ASCII subject and an attachment; core must receive, store and
publish it with the full envelope (byte-identical attachment), then, with its own address
revoked, count the next one as filtered and never publish it.
