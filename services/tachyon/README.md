# tachyon service

[Tachyon](https://github.com/juliog922/tachyon) is the platform's local inference engine: Gemma 4
chat, Laya typed decisions and Granite embeddings on the NVIDIA GPU. `tachyon serve` keeps the
models in VRAM between requests; core (and any other service) talks to it over a Unix socket and a
shared memory of its own.

**Tachyon's source is not part of this repository.** It lives in its own project, with its own
rules; Kapa ships a **prebuilt binary** of it and never changes it.

```
services/tachyon/
├── bin/tachyon          the engine, built from a commit of ~/projects/tachyon (committed)
├── bin/VERSION          its provenance: source commit, how it was built, SHA-256
├── Dockerfile           distroless runtime (uid 65532) around bin/tachyon
├── seccomp.json         Docker's default seccomp profile + io_uring (generated)
├── scripts/seccomp.py   regenerates it: make -C services/tachyon seccomp TAG=v27.3.1
├── eval/                tests + benchmarks of the models Kapa uses (make deploy-tachyon-eval)
└── Makefile             import, verify, build
```

## Updating the engine

```bash
make -C services/tachyon import                       # from ~/projects/tachyon (TACHYON_SRC=path)
git add services/tachyon/bin && git commit -m "tachyon: <commit>"
```

`import` builds in `rust:1.98.1-bookworm`, with the source mounted read-only: Debian 12's glibc is
the runtime image's, so the binary runs there (one built on the host could need a newer glibc). It
refuses a source with uncommitted changes, so `bin/VERSION` always names a real commit
(`ALLOW_DIRTY=1` to override, and VERSION says so), then runs `verify`.

`verify` (also `make test` here and in the root) checks that `bin/tachyon` matches the SHA-256 in
`bin/VERSION` and runs on the runtime base image. Tachyon's own gates (format, clippy, complexity,
tests, GPU tests) are Tachyon's: `./ci.sh` in its project.

## Running in a container

| Need | How |
|---|---|
| The GPU | NVIDIA Container Toolkit on the host; compose reserves one GPU (`deploy.resources`) and the toolkit mounts `libcuda.so.1` |
| `io_uring` (the model loader, no fallback) | `seccomp.json`: Docker's default profile, which denies io_uring since Docker 25, plus exactly `io_uring_setup/enter/register` |
| `O_DIRECT` reads | models in the `tachyon_models` volume (a real filesystem, not the overlay) |
| The same user as its clients | the daemon admits only its uid (`SO_PEERCRED`, socket mode 0600): tachyon and core both run as 65532 |
| A shared socket | `tachyon_run`: a tmpfs volume owned by 65532, mode 0700, mounted in tachyon and (read-only) in core |

Shared memory crosses containers as a `memfd` passed over the socket, woken by futexes: both work
between containers on the same kernel. The service is in compose profile `gpu`, so hosts without
a GPU (the CI stack, laptops without the toolkit) never start it.

## On prike

```bash
make deploy-check-gpu                     # driver, GPU, NVIDIA Container Toolkit, docker runtime
# add COMPOSE_PROFILES=gpu to the production .env, then:
make deploy-env && make deploy            # (or merge to main: CD deploys)
make deploy-tachyon ARGS=doctor           # inside the container: driver, GPU, memory, certificates, Hub
make deploy-tachyon ARGS="pull gemma-4-e4b"
make deploy-tachyon ARGS=ps               # also shown by make monitor (kapa.status)
```

Models persist in the `tachyon_models` volume (with the CUDA kernel cache). `HF_TOKEN` is only
needed for gated models.

## Evaluation: the models Kapa uses

| Use | Model |
|---|---|
| Text, images, audio | `gemma-4-e4b` |
| Decisions on customer messages | `laya-multilingual` (Spanish; `laya-english` is English only) |
| Embeddings | `granite-embedding-311m` (768 dims; `EMBED=granite-embedding-97m` to compare) |

```bash
PULL=1 make deploy-tachyon-eval        # first time: pulls what is missing, then evaluates
make deploy-tachyon-eval               # later: after a new tachyon binary, a driver update...
```

It runs from OMEN16, every call going through the daemon on prike as production will
(`eval/eval.py`, standard library only):

- **Checks** with answers fixed by construction: arithmetic and a capital in Spanish; the color
  of images it generates (red; blue and yellow); the transcription of real speech (LibriSpeech
  clips from `~/projects/tachyon`, `TACHYON_SRC=path`); Laya's intent (`pedido`, `incidencia`,
  `consulta`, `saludo`) and order flag on Kapa-style messages; product retrieval by embeddings,
  across languages too.
- **Goldens**: Gemma decodes greedily, so its answers are deterministic. The first run records
  them in `eval/expected.json`: review it and commit it. Later runs require the same answers;
  `APPROVE=1` accepts deliberate changes (never a failed check).
- **Bench**: load times; prefill and decode tok/s from the engine for text, image and audio;
  decide and embed latency end to end (docker exec included: the round trip is reported too);
  embedding throughput in batches of 64. `SAVE_BASELINE=1` records `eval/baseline.json`; later
  runs fail when 5% slower (engine timings) or 20% (end to end).

The report is saved in `testdata/eval-report.local.md` (gitignored).
