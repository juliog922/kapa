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
