# WhatsApp gateway

gRPC service that links WhatsApp accounts through [whatsmeow](https://github.com/tulir/whatsmeow),
stores their sessions in PostgreSQL and streams their **one-to-one** messages to the other Kapa
services. Contract: [`proto/kapa/whatsapp/v1/whatsapp.proto`](../../proto/kapa/whatsapp/v1/whatsapp.proto).
Code: one hand-written file, `main.go`; `pb/` is generated (`make proto`).

## API

| RPC | Behaviour |
|-----|-----------|
| `Login(phone)` | Returns a pairing code (`ABCD-EFGH`) to type on the phone: *WhatsApp › Linked devices › Link a device › Link with phone number instead*. A code lives about a minute; while pairing is pending, fresh codes arrive on `Subscribe` (`PAIRING` + `pairing_code`), show the latest. The result arrives as `CONNECTED` or `PAIR_FAILED` (~2.5 min without success). For an existing account it returns its state and no code. `RESOURCE_EXHAUSTED` means WhatsApp rate-limits code requests for that number: wait (usually hours), retrying extends the block. |
| `Logout(phone)` | Unlinks the account from WhatsApp and wipes its session. `FAILED_PRECONDITION` while still pairing. |
| `Delete(phone)` | Removes the account no matter what: tries to unlink, always wipes locally, cancels pending pairings. |
| `Send(account, to, text \| image \| audio)` | Uploads media and sends. `voice_note=true` + `audio/ogg; codecs=opus` for voice notes. |
| `Subscribe(accounts[], peers[])` | Server stream of messages and account events. Empty lists = everything; `peers` filters by contact phone. |
| `Status()` | Accounts, whatsmeow version, WhatsApp protocol version, `outdated` flag. |

Plus the standard `grpc.health.v1.Health` service.

**What is delivered:** text, images and audio (decrypted bytes inline) from one-to-one chats only.
Groups, status updates, broadcast lists, channels, your own outgoing messages and every other
message type are discarded on arrival. Senders hidden behind a WhatsApp LID are resolved to their
phone number when possible, otherwise `from = "lid:<id>"`.

**Delivery semantics:** at most once. Messages arriving while no service is subscribed are not
queued, and a subscriber more than 256 events behind is disconnected with `RESOURCE_EXHAUSTED`
(it must resubscribe) so one slow consumer never stalls the others. Media is only downloaded
when at least one subscriber wants that message.

## Configuration

| Variable | Default | |
|----------|---------|---|
| `GRPC_ADDR` | `:50051` | Listen address |
| `DATABASE_URL` | *(empty)* | lib/pq DSN; empty = standard `PGHOST`, `PGUSER`, `PGPASSWORD`, `PGDATABASE`… (what compose uses) |
| `WA_LOG_LEVEL` | `WARN` | `DEBUG`, `INFO`, `WARN`, `ERROR` |
| `WA_SELF_CHAT` | `0` | `1` also delivers "Message yourself" chat messages (`from_me=true`) |

Binary modes: `/whatsapp -healthcheck` (container health) and `/whatsapp -status`
(accounts and protocol check, used by `make monitor`; exit 0 ok, 1 outdated, 2 update available).

## Development

Only Docker is required: every target runs in the tools image (Go, golangci-lint, buf, lizard, perf).

| Command | |
|---------|---|
| `make deps` | `go mod tidy` (run once after cloning a fresh tree without `go.sum`, then commit it) |
| `make format` | gofumpt + goimports, buf format |
| `make lint` | golangci-lint (16 linters), buf lint, lizard (CCN ≤ 10, ≤ 40 lines, ≤ 6 args), `pb/` up to date |
| `make check-docs` | every Go declaration documented; proto fully documented (buf COMMENTS) |
| `make test` | unit tests + integration tests, race detector on |
| `make bench` | benchmarks under perf → `bench/report.txt` (`BENCH=regex BENCHTIME=1s`) |
| `make update-whatsmeow` | upgrade whatsmeow to upstream main and unit-test it |

> **Doxygen** has no Go parser, so `check-docs` enforces the Go equivalent (godoc comments on
> every func/type/const/var) and buf's COMMENTS rules on the proto. Doxygen will be wired into
> `check-docs` for services written in languages it supports.

## Tests

**Unit** (`main_test.go`, no database, no network): phone validation, chat classification
(groups/status/channels/LIDs/self chat), message conversion, filters, zero-copy fan-out (same
pointer to every subscriber), slow-subscriber drop, no media work without subscribers, every
lifecycle event, pairing failure cleanup, every RPC error path over real gRPC, stream endings,
health/status probes with all exit codes, and a concurrency stress test under `-race`.

**Integration** (`integration_test.go`, build tag `integration`) against a real PostgreSQL:
schema migration, session wipe on `Delete` across restarts, and the **protocol drift check**.
With `WA_TEST_PHONE` set, it also runs **live against WhatsApp with a single phone**:

1. One-time: `cp .env.test.example .env.test`, set `WA_TEST_PHONE`, run **`make pair-test`** and
   type the two codes it prints. The phone is linked twice (devices A and B, max 4 per phone);
   the sessions persist in the `kapa-wa-test-sessions` Docker volume (CI on this machine reuses them).
2. Every `make test`: device A sends text, an image and a voice note to the phone's own
   "Message yourself" chat; WhatsApp fans it out to device B, which must receive the same
   message ID and **byte-identical** decrypted media. With `WA_TEST_PAIRING=1` a pairing code
   is also requested and cancelled (off by default: WhatsApp rate-limits code requests per number).

`make test-reset` forgets the test sessions (also remove the two *Kapa* entries from the
phone's linked devices). Automated use of WhatsApp through unofficial clients can lead to bans;
the tests only message the test phone itself.

## WhatsApp protocol updates

WhatsApp changes its protocol from time to time; whatsmeow follows. Detection is layered:

whatsmeow is **pinned** to a known-good commit (`go.mod`): upgrading blindly can break phone-code
pairing, and every re-validation costs pairing-code quota. So drift is a warning and only a real
break fails:

| Where | Signal |
|-------|--------|
| Runtime | WhatsApp rejects the client (`405 client outdated`): `ClientOutdated` → health `NOT_SERVING` → container unhealthy, deploy fails, `make monitor` shows an **issue**. |
| `make monitor` | `-status` compares our protocol version with whatsmeow upstream main: **UPDATE AVAILABLE** warning. |
| `make test` | the stack smoke test and `TestUpstreamProtocol` print the same **UPDATE AVAILABLE** warning; the live round trips fail if WhatsApp stops accepting the pinned build. |

Upgrade deliberately: `make update-whatsmeow WHATSMEOW=<commit|main>` → `make test` → one pairing
check (`WA_TEST_PAIRING=1 make test`) → commit `go.mod`/`go.sum` → PR → merge deploys it. Going back
to an older commit may also need its matching `go.mau.fi/util` (see that commit's `go.mod`).
