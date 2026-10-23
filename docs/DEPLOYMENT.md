# Kapa · CI/CD and Deployment

## Overview

```
 dev branch ──PR──▶ main ──merge──▶ CD ──ssh/rsync──▶ prike (/opt/kapa)
                │                    │
                ▼                    ▼
        CI: make lint           make deploy
            make test           make monitor  (fails the job if unhealthy)
        "CI result" gates the merge
```

Both workflows run on a **self-hosted runner** on the dev machine (OMEN16) with the label `kapa`.
The runner reaches prike over SSH with a dedicated deploy key.

| Where     | What lives there                                                        |
|-----------|-------------------------------------------------------------------------|
| GitHub    | Code, workflows, deploy SSH key (secret), host/user (variables)         |
| OMEN16    | Self-hosted runner, your local `.env` (source of the production config) |
| prike     | `/opt/kapa` (synced code), `/opt/kapa/.env` (prod secrets, mode 600), `.release` |

The production `.env` is never stored in GitHub. It is uploaded once with `make deploy-env`
and survives every deploy (it is excluded from rsync).

---

## 1 · Provision prike (once)

From OMEN16, where `ssh prike` already works:

```bash
make deploy-setup     # asks for julio's sudo password on prike
ssh prike sudo reboot # only if the script says REBOOT REQUIRED (NVIDIA driver)
make monitor          # GPU section should now show the card via nvidia-smi
```

`deploy-setup` is idempotent. It installs make/rsync, Docker Engine + Compose
(official repo), the recommended NVIDIA driver (`ubuntu-drivers install`), the
NVIDIA Container Toolkit (configured as a Docker runtime), adds `julio` to the
`docker` group and creates `/opt/kapa`.

> If the script reports *"No NVIDIA GPU found on the PCI bus"*, the card is not
> visible to the OS (check `lspci | grep -i nvidia`, BIOS, or passthrough if prike is a VM).
> If Secure Boot is enabled you may need to enroll the MOK key on the console at reboot.

Then upload the production config and do a first manual deploy:

```bash
cp .env.example .env   # set real credentials
make deploy-env
make deploy
make monitor
```

## 2 · Self-hosted runner on OMEN16 (once)

The runner refuses to run as root, so use a dedicated user:

```bash
sudo useradd -m -s /bin/bash github-runner
sudo usermod -aG docker github-runner
sudo apt install -y make rsync openssh-client
```

In GitHub: **Settings → Actions → Runners → New self-hosted runner → Linux x64** and run the
download commands it shows as `github-runner` (`sudo -iu github-runner`). When configuring,
add the `kapa` label:

```bash
./config.sh --url https://github.com/<owner>/kapa --token <TOKEN> \
            --name omen16 --labels kapa --unattended
sudo ./svc.sh install github-runner
sudo ./svc.sh start
```

> ⚠ **Keep the repository private.** On a public repo anyone could open a PR that runs
> code on OMEN16. The CI job also refuses PRs from forks as a second safety net.

The runner must be online for PRs to be checked and merges to be deployed; jobs queue while it is off.

## 3 · Deploy key and GitHub environment (once)

On OMEN16, create a key used only by CD and authorize it on prike:

```bash
ssh-keygen -t ed25519 -f ~/.ssh/kapa_deploy -N "" -C "kapa-deploy"
ssh-copy-id -i ~/.ssh/kapa_deploy.pub prike
ssh -G prike | grep -E '^(hostname|user|port) '     # values for the variables below
ssh-keyscan -p 22 <hostname> 2>/dev/null            # value for PRIKE_KNOWN_HOSTS
```

In GitHub: **Settings → Environments → New environment → `production`**, then add:

| Kind     | Name                | Value                                     |
|----------|---------------------|-------------------------------------------|
| Secret   | `PRIKE_SSH_KEY`     | contents of `~/.ssh/kapa_deploy` (private) |
| Secret   | `PRIKE_KNOWN_HOSTS` | output of `ssh-keyscan`                   |
| Variable | `PRIKE_HOST`        | the `hostname` from `ssh -G prike` (e.g. `10.20.10.100`) |
| Variable | `PRIKE_USER`        | `julio`                                   |
| Variable | `PRIKE_PORT`        | optional, defaults to `22`                |

Delete the local private key copy afterwards if you don't need it.

## 4 · Protect `main` (once)

Apply the ruleset in `.github/rulesets/main.json` (blocks direct pushes, force-pushes and deletion;
requires a PR whose **CI result** check passed on an up-to-date branch):

```bash
gh api --method POST repos/<owner>/kapa/rulesets --input .github/rulesets/main.json
```

Or create it by hand in **Settings → Rules → Rulesets**. Rulesets on private repositories
require GitHub Pro / Team for personal accounts.

## 5 · Daily workflow

```bash
git switch dev
# ... work, commit ...
git push
gh pr create --base main --head dev --fill   # CI runs `make test`
gh pr merge --merge                          # allowed only when "CI result" is green
# merge to main → CD deploys to prike and runs `make monitor`
```

## Operating production

| Command                         | Description                                              |
|---------------------------------|----------------------------------------------------------|
| `make monitor`                  | One-shot dashboard; exits 1 if anything is unhealthy     |
| `make monitor WATCH=5`          | Live dashboard refreshed every 5 s                       |
| `make monitor SERVICE=postgres LOG_LINES=50` | Focus the log section on one service        |
| `make deploy-logs`              | Follow logs of every service (see *Logs* below)          |
| `make deploy-logs-ui`           | Web log viewer through an SSH tunnel                     |
| `make deploy-stop`              | **Emergency stop** of all services (data kept). `FORCE=1` skips the prompt |
| `make deploy-clean`             | **Destroys** containers, images and data volumes (asks you to type the host name) |
| `make deploy`                   | Manual redeploy (also restores after a stop or clean)    |
| `make deploy-env`               | Replace the production `.env` (applies on next deploy)   |

All deployment targets accept `DEPLOY_HOST=` and `DEPLOY_DIR=` overrides; `KAPA_SSH=` changes the
SSH command (CI uses it to point at a temporary SSH config).

## Logs

Two complementary tools, both reading Docker's logs on prike (nothing extra to ship or store):

**Terminal: `make deploy-logs`** for follow, filter and search from the CLI.

| Variable    | Example                         | Effect                                                  |
|-------------|---------------------------------|---------------------------------------------------------|
| `SERVICE`   | `SERVICE=postgres,dozzle`       | Only these services (default: all)                      |
| `LEVEL`     | `LEVEL=error` / `LEVEL=warn`    | Only error-level (or warn and above) lines              |
| `GREP`      | `GREP='timeout\|refused'`       | Case-insensitive regex, matches highlighted             |
| `SINCE`     | `SINCE=30m`, `2h`, `2026-10-23T20:00:00` | Start point (otherwise the last `LOG_LINES`)  |
| `FOLLOW`    | `FOLLOW=0`                      | Print and exit instead of following (search mode)       |
| `LOG_LINES` | `LOG_LINES=500`                 | Backlog size when `SINCE` is not set                    |

```bash
make deploy-logs                                    # live tail, all services
make deploy-logs SERVICE=postgres LEVEL=warn        # live warnings+errors of postgres
make deploy-logs SINCE=24h FOLLOW=0 LEVEL=error     # what failed today?
make deploy-logs SINCE=1h FOLLOW=0 GREP=auth > auth.log
```

**Browser: `make deploy-logs-ui`** for the visual view. It opens an SSH tunnel to
[Dozzle](https://dozzle.dev), which runs on prike as part of the `ops` compose profile
(started by `make run` / `make deploy`, never by `make test`). Browse
`http://localhost:8090` for live tailing, full-text and regex search, per-service views, side-by-side
logs and per-container CPU/memory. Dozzle listens on prike's `127.0.0.1` only and sees only `kapa`
containers, so it is never exposed on the network. Change the local port with `LOGS_UI_PORT=`.

> Pin `DOZZLE_VERSION` in the production `.env` to a specific release once you're happy with it,
> so a deploy never pulls an unexpected new version.

`make monitor` also shows an **ERR/1h** column: error-level lines per service in the last hour.

Retention follows Docker's log driver. Before production traffic grows, cap it in
`/etc/docker/daemon.json` on prike (`"log-opts": {"max-size": "50m", "max-file": "5"}`), or move to a
central stack (Loki + Grafana) once there are several services.