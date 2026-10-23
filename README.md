# Kapa

Monorepo for the Kapa platform: independent services (any language) talking gRPC through the
shared contracts in `proto/`. Everything is operated from the root `Makefile` (`make help`), which
delegates to each service's own Makefile.

| Service | Language | Description |
|---------|----------|-------------|
| [`whatsapp`](services/whatsapp/README.md) | Go | WhatsApp gateway (whatsmeow): accounts, messages, event stream — gRPC `:50051` |
| `postgres` | — | PostgreSQL 18 shared database |
| `dozzle` | — | Web log viewer (production `ops` profile) |

## Requirements

- Docker with Compose v2.24+ (`docker compose`) — all toolchains run in containers
- GNU Make, bash, rsync, ssh

## Getting started

```bash
cp .env.example .env   # then set real production credentials
make deps              # first time only: resolves go.sum of the Go services (commit it)
make run
```

## Commands

| Command             | Description                                                          |
|---------------------|----------------------------------------------------------------------|
| `make run`          | Build and start services with production credentials (`.env`)        |
| `make stop`         | Stop and remove running containers (data is kept)                    |
| `make clean`        | Remove containers, volumes and built images — **destroys data**      |
| `make format`       | Format every service and the proto contracts                          |
| `make lint`         | Linters, complexity (lizard), proto lint, generated code freshness   |
| `make check-docs`   | Documentation check of every service and proto contract              |
| `make test`         | Stack smoke test + unit and integration tests of every service       |
| `make bench`        | Benchmarks under perf (context switches, page faults, allocations)   |
| `make update-whatsmeow` | Upgrade whatsmeow when WhatsApp changes its protocol             |
| `make pair-test`    | One-time: link the WhatsApp test phone for live tests                |
| `make deploy`       | Sync the repo to prike and (re)start services                        |
| `make monitor`      | Production dashboard: services, CPU/RAM/GPU/disk, logs (`WATCH=5`)   |
| `make deploy-logs`  | Follow / filter / search production logs (`SERVICE` `LEVEL` `GREP` `SINCE`) |
| `make deploy-logs-ui` | Web log viewer (Dozzle) through an SSH tunnel                      |
| `make deploy-stop`  | **Emergency** stop of production services                            |
| `make deploy-clean` | **Destroy** production containers, images and data                  |
| `make deploy-env`   | Upload the production `.env` to prike                                |
| `make runner-start` / `runner-stop` | Start / stop the on-demand GitHub Actions runner (`runner-status`, `runner-logs`) |
| `make deploy-setup` | One-time provisioning of prike (Docker, NVIDIA driver + toolkit)     |

CI/CD, runner setup and production operations: see [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md).

## Branching

`dev` → pull request → `main`. PRs must pass **CI result** (`make test` on the self-hosted runner);
every merge to `main` is deployed to prike automatically.

## Environments

| File           | Purpose             | Compose project | Host port | Committed |
|----------------|---------------------|-----------------|-----------|-----------|
| `.env`         | Production          | `kapa`          | 127.0.0.1:5432, Dozzle 127.0.0.1:8080 | No |
| `.env.dev`     | Development / test  | `kapa-test`     | none      | Yes       |
| `.env.example` | Template for `.env` | —               | —         | Yes       |

## Structure

```
kapa/
├── Makefile                    # single entry point for the whole repo
├── docker-compose.yml
├── docker-compose.test.yml     # test-only overrides (no host ports)
├── .deployignore               # paths never synced to prike
├── .github/
│   ├── workflows/ci.yml        # PR → make test (self-hosted)
│   ├── workflows/cd.yml        # main → deploy to prike (self-hosted)
│   └── rulesets/main.json      # branch protection for main
├── scripts/
│   ├── deploy*.sh, monitor.sh  # run locally / in CI
│   ├── lib/common.sh
│   └── remote/                 # run on prike (status report, provisioning)
├── proto/                      # shared gRPC contracts (buf lint + breaking rules)
│   └── kapa/whatsapp/v1/whatsapp.proto
├── services/
│   ├── postgres/
│   │   ├── Dockerfile          # postgres:<version>-alpine
│   │   └── initdb/             # SQL/sh run once on first boot
│   └── whatsapp/               # Go gateway: main.go + tests, Makefile, Dockerfile (tools/runtime)
└── docs/DEPLOYMENT.md
```

## License

MIT — see [LICENSE](LICENSE).
