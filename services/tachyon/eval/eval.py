#!/usr/bin/env python3
"""Kapa - test and benchmark the Tachyon models the platform uses, on the deployment host.

    chat     gemma-4-e4b             text, images, audio
    decide   laya-multilingual       typed decisions on (Spanish) customer messages
    embed    granite-embedding-311m  embeddings (EMBED=granite-embedding-97m to compare)

Every call is `docker exec <tachyon> /tachyon ...`, through the daemon exactly as production will:
on the deployment host over SSH (make deploy-tachyon-eval), or here (EVAL_LOCAL=1: make test-gpu).
Results are per machine (results/<machine>/): GPUs differ in speed, and greedy answers may differ
in floating point. EVAL_BENCH=0 runs the checks only. Standard library only.

Checks:   Gemma's answers fixed by construction (arithmetic, generated images, real speech) must be
          right. Laya and the embeddings are classifiers: they are scored on labelled sets
          (DECIDE_MIN, EMBED_MIN), each miss listed. A new order is intent `pedido` (precision and
          recall); catalog text is embedded normalized (both measured on OMEN16, see the notes).
Goldens:  Gemma decodes greedily, so its answers are deterministic: the first run records them in
          expected.json for review; later runs require the same answers (APPROVE=1 accepts changes).
Bench:    the engine's own prefill/decode timings for chat; end-to-end latency for the rest, as
          measured (docker exec + socket + request), next to the cost of an empty round trip. SAVE_BASELINE=1 records baseline.json; later
          runs fail when slower than it (5% for engine timings, 20% end to end).
"""
import io
import json
import math
import os
import re
import shlex
import socket
import statistics
import struct
import subprocess
import sys
import tarfile
import zlib
from pathlib import Path

HERE = Path(__file__).resolve().parent
LOCAL = os.environ.get("EVAL_LOCAL") == "1"
MACHINE = os.environ.get("EVAL_MACHINE") or socket.gethostname()
RESULTS = HERE / "results" / MACHINE
EXPECTED, BASELINE = RESULTS / "expected.json", RESULTS / "baseline.json"
REPORT = HERE.parent / "testdata" / f"eval-{MACHINE}.local.md"
BENCH = os.environ.get("EVAL_BENCH", "1") != "0"
CHAT, DECIDE = "gemma-4-e4b", "laya-multilingual"
EMBED = os.environ.get("EMBED") or "granite-embedding-311m"
DIMS = {"granite-embedding-311m": 768, "granite-embedding-97m": 384}
MEDIA = Path(os.environ.get("TACHYON_SRC") or "~/projects/tachyon").expanduser() / "crates/tachyon/tests/fixtures/media"
BOX = "/tmp/kapa-eval"


# ---- the host -----------------------------------------------------------------------------------

class Call:
    """One tachyon command's outcome; `secs` is measured on the host (no SSH in it)."""

    def __init__(self, rc, out, err, secs):
        self.rc, self.out, self.err, self.secs = rc, out, err, secs


def shell(script, **kwargs):
    """Runs a shell script on the GPU host: here (EVAL_LOCAL=1) or over SSH."""
    remote = shlex.split(os.environ.get("KAPA_SSH", "ssh")) + [os.environ.get("DEPLOY_HOST", "prike")]
    return subprocess.run(["sh", "-c", script] if LOCAL else remote + [script], **kwargs)


def tachyon(*args):
    """Runs /tachyon in the container; times it on the GPU host, around docker exec."""
    inner = " ".join(shlex.quote(a) for a in ["docker", "exec", os.environ["TACHYON_CONTAINER"], "/tachyon", *args])
    script = f's=$(date +%s%N); {inner}; rc=$?; e=$(date +%s%N); echo "__kapa_ns $((e-s))" >&2; exit $rc'
    p = shell(script, capture_output=True, text=True)
    lines = p.stderr.splitlines()
    ns = next((int(x.split()[1]) for x in reversed(lines) if x.startswith("__kapa_ns ")), 0)
    err = "\n".join(x for x in lines if not x.startswith("__kapa_ns "))
    return Call(p.returncode, p.stdout.strip(), err.strip(), ns / 1e9)


def upload(files):
    """Copies {name: bytes} into the container's BOX (a tar through `docker cp -`), owned by tachyon."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as tar:
        for name, data in [("kapa-eval/", None), *((f"kapa-eval/{n}", d) for n, d in files.items())]:
            info = tarfile.TarInfo(name.rstrip("/"))
            info.uid = info.gid = 65532
            info.type, info.mode = (tarfile.DIRTYPE, 0o755) if data is None else (tarfile.REGTYPE, 0o644)
            info.size = 0 if data is None else len(data)
            tar.addfile(info, None if data is None else io.BytesIO(data))
    shell(f"docker cp - {shlex.quote(os.environ['TACHYON_CONTAINER'])}:/tmp", input=buf.getvalue(), check=True, capture_output=True)


# ---- assets -----------------------------------------------------------------------------------

def png(width, height, color):
    """A PNG whose pixel (x, y) is color(x, y): RGB."""
    rows = b"".join(b"\0" + b"".join(bytes(color(x, y)) for x in range(width)) for y in range(height))

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")


def assets():
    """The files the cases use: generated images, and LibriSpeech clips from Tachyon's fixtures."""
    files = {
        "red.png": png(96, 96, lambda x, y: (220, 20, 20)),
        "blue-yellow.png": png(192, 96, lambda x, y: (20, 60, 220) if x < 96 else (240, 210, 20)),
    }
    for clip in ("clip-0.wav", "clip-1.wav"):
        if (MEDIA / clip).is_file():
            files[clip] = (MEDIA / clip).read_bytes()
    return files


# ---- results ----------------------------------------------------------------------------------

class Report:
    """Checks, goldens and metrics of one run."""

    def __init__(self):
        self.lines, self.failed, self.metrics = [], 0, {}
        self.expected = json.loads(EXPECTED.read_text()) if EXPECTED.is_file() else {}
        self.recorded, self.approve = {}, os.environ.get("APPROVE") == "1"

    def check(self, name, ok, detail=""):
        self.failed += 0 if ok else 1
        self.say(f"{'✔' if ok else '✘'} {name}" + (f": {detail}" if detail else ""))

    def golden(self, key, value):
        """Same answer as the reviewed one; new keys are recorded for review."""
        self.recorded[key] = value
        if key not in self.expected:
            self.say(f"• {key}: recorded for review: {json.dumps(value, ensure_ascii=False)[:160]}")
        elif self.expected[key] == value:
            self.say(f"✔ {key}: same answer as reviewed")
        elif self.approve:
            self.say(f"• {key}: CHANGED, accepted (APPROVE=1)")
        else:
            self.check(key, False, f"answer changed\n    was: {self.expected[key]}\n    now: {value}")

    def metric(self, name, value, unit, higher_better, engine):
        self.metrics[name] = {"value": round(value, 4), "unit": unit, "higher_better": higher_better, "engine": engine}

    def say(self, line):
        print(line, flush=True)
        self.lines.append(line)


DURATION = re.compile(r"([\d.]+)(ns|µs|us|ms|s)")
STATS = re.compile(r"(\d+) prompt tokens in (\S+) \(([\d.]+) tok/s\); (\d+) tokens in (\S+) \(([\d.]+) tok/s\)")


def seconds(text):
    """A Rust Duration printed with {:.2?}: 12.34ms, 1.20s, 950.00µs."""
    m = DURATION.fullmatch(text)
    return float(m[1]) * {"ns": 1e-9, "µs": 1e-6, "us": 1e-6, "ms": 1e-3, "s": 1.0}[m[2]] if m else math.nan


def chat(report, *args):
    """Runs a chat request; returns (answer, stats) or (None, None) after reporting the error."""
    c = tachyon("run", CHAT, *args)
    m = STATS.search(c.err)
    if c.rc != 0 or not m:
        report.check(f"chat {args[0][:40]!r}", False, c.err[-300:] or f"exit {c.rc}")
        return None, None
    stats = {"prompt": int(m[1]), "prefill_s": seconds(m[2]), "prefill_tps": float(m[3]), "tokens": int(m[4]),
             "decode_s": seconds(m[5]), "decode_tps": float(m[6])}
    return c.out, stats


# ---- the cases --------------------------------------------------------------------------------

def text_cases(report):
    """Facts and arithmetic with one right answer, in Spanish; the answers become goldens."""
    cases = [("text.arithmetic", "¿Cuánto es 17 + 25? Responde solo con el número.", ["42"]),
             ("text.capital", "¿Cuál es la capital de Francia? Responde con una sola palabra.", ["parís", "paris"]),
             ("text.spanish", "Explica en una frase para qué sirve un champú.", [" el ", " la ", " para ", " de "])]
    for key, prompt, any_of in cases:
        answer, _ = chat(report, prompt)
        if answer is not None:
            report.check(key, any(a in f" {answer.lower()} " for a in any_of), repr(answer[:120]))
            report.golden(key, answer)


def media_cases(report, have):
    """Images generated here (their colors are known) and real speech (LibriSpeech clips)."""
    images = [("image.red", "red.png", "¿De qué color es esta imagen? Responde con una palabra en español.", [["rojo", "roja"]]),
              ("image.two-colors", "blue-yellow.png", "¿Qué dos colores aparecen en esta imagen? Responde en español.",
               [["azul"], ["amarillo"]])]
    for key, file, prompt, groups in images:
        answer, _ = chat(report, prompt, f"--image={BOX}/{file}")
        if answer is not None:
            report.check(key, all(any(w in answer.lower() for w in g) for g in groups), repr(answer[:120]))
            report.golden(key, answer)
    for clip in ("clip-0.wav", "clip-1.wav"):
        if clip not in have:
            report.say(f"- audio.{clip}: skipped ({MEDIA / clip} not found; TACHYON_SRC=path)")
            continue
        c = tachyon("transcribe", CHAT, f"{BOX}/{clip}")
        words = re.findall(r"[A-Za-z']+", c.out)
        report.check(f"audio.{clip}", c.rc == 0 and len(words) >= 8, repr(c.out[:120]) if c.rc == 0 else c.err[-200:])
        if c.rc == 0:
            report.golden(f"audio.{clip}", c.out)


INTENT = {"type": "choice", "instructions": "¿Qué quiere el cliente con este mensaje?",
          "criteria": {"pedido": "pide productos o cantidades para comprar", "incidencia": "queja, rotura, error, devolución o abono",
                       "consulta": "pregunta por stock, precio, plazos o información", "saludo": "saluda, agradece o se despide sin pedir nada"}}
# A new order is intent `pedido`. A separate yes/no question ("noul") was measured on OMEN16 and
# rejected: 12/16, e.g. "Perfecto, recibido. Un saludo" scored 0.99, while the intent caught every
# order with no false one. One question per message is also one computation less.
# Labelled B2B messages (intent, is a new order), 4 per intent, hard cases included.
MESSAGES = [("pedido", True, "Hola, ponme 6 cajas de champú Kativa de keratina y 2 de mascarilla de karité para el jueves."),
            ("pedido", True, "Buenas, necesito 24 adhesivos de pestañas Ardell oscuro y 12 del transparente, gracias"),
            ("pedido", True, "Mándame lo mismo que la última vez pero el doble de esmaltes rojos"),
            ("pedido", True, "Buenos días! Para esta semana: 3 kits de alisado y 10 champús de rizos"),
            ("incidencia", False, "Me ha llegado el pedido con dos botes rotos, quiero que me lo abonéis."),
            ("incidencia", False, "En la factura de ayer me habéis cobrado 12 mascarillas y solo pedí 6"),
            ("incidencia", False, "El champú que me enviasteis está caducado, ¿cómo lo devuelvo?"),
            ("incidencia", False, "Todavía no me ha llegado el pedido del lunes"),
            ("consulta", False, "Buenos días, ¿tenéis stock del kit de alisado de keratina? ¿A qué precio me sale?"),
            ("consulta", False, "¿Cuándo os llega otra vez el adhesivo transparente de Ardell?"),
            ("consulta", False, "¿Qué diferencia hay entre la mascarilla de karité y la de argán?"),
            ("consulta", False, "¿Hacéis envíos a Canarias? ¿Cuánto tardan?"),
            ("saludo", False, "Muchas gracias, que tengáis buen fin de semana"),
            ("saludo", False, "Perfecto, recibido. Un saludo"),
            ("saludo", False, "Hola Laura, ¿qué tal todo por ahí?"),
            ("saludo", False, "Gracias por avisar 👍")]
# Minimum accuracy over MESSAGES: a product requirement (raise it as the questions improve).
DECIDE_MIN = 0.85


def decide_one(i):
    """The intent Laya chooses for MESSAGES[i], or None after the error text."""
    c = tachyon("decide", DECIDE, f"{BOX}/laya-{i}.json")
    return json.loads(c.out)["intent"]["choice"] if c.rc == 0 else None


def order_scores(pairs):
    """Precision and recall of `pedido` (a new order) over (expected, got) intents."""
    hits = sum(1 for want, got in pairs if want == got == "pedido")
    predicted = sum(1 for _, got in pairs if got == "pedido")
    actual = sum(1 for want, _ in pairs if want == "pedido")
    return hits, predicted, actual


def decide_cases(report):
    """Laya on labelled customer messages: intent accuracy, and pedido precision and recall."""
    upload({f"laya-{i}.json": json.dumps({"state": text, "questions": {"intent": INTENT}}).encode()
            for i, (_, _, text) in enumerate(MESSAGES)})
    pairs = []
    for i, (intent, _, text) in enumerate(MESSAGES):
        got = decide_one(i)
        if got is None:
            report.check(f"decide.{i}", False, "Laya gave no answer")
        elif got != intent:
            report.say(f"  miss: {text[:60]!r}: {got}, expected {intent}")
        pairs.append((intent, got))
        report.golden(f"decide.{i}", got)
    n, right = len(MESSAGES), sum(1 for want, got in pairs if want == got)
    hits, predicted, actual = order_scores(pairs)
    report.check("decide intent accuracy", right / n >= DECIDE_MIN, f"{right}/{n} (minimum {DECIDE_MIN:.0%})")
    report.check("new order (pedido) precision", predicted and hits / predicted >= DECIDE_MIN, f"{hits}/{predicted} orders were orders")
    report.check("new order (pedido) recall", actual and hits / actual >= DECIDE_MIN, f"{hits}/{actual} orders caught")


# Product descriptions as Sage stores them (upper case), hard negatives included.
PRODUCTS = ["KATIVA KARITE COCO MARULA MASCARILLA 300ML", "KERATIN ANTIFRIZZ XTRA SHINE ALISADO SIN PLANCHA",
            "ARDELL ADHESIVO PESTAÑAS LASHTITE OSCURO 22ML", "CHAMPU RIZOS DEFINIDOS SIN SULFATOS 300ML",
            "ESMALTE DE UÑAS ROJO PASION 15ML", "ACONDICIONADOR REPARADOR ARGAN 250ML", "TINTE CAPILAR RUBIO CENIZA 9.1",
            "QUITAESMALTE SIN ACETONA 125ML", "PESTAÑAS POSTIZAS NATURALES TIRA COMPLETA", "PROTECTOR TERMICO SPRAY PARA PLANCHA 200ML"]
QUERIES = [("mascarilla nutritiva de manteca de karité", 0), ("alisado de keratina sin plancha", 1),
           ("pegamento para pestañas postizas", 2), ("champú para pelo rizado", 3), ("pintauñas rojo", 4),
           ("acondicionador con aceite de argán", 5), ("tinte rubio ceniza", 6), ("quitaesmalte sin acetona", 7),
           ("pestañas postizas naturales", 8), ("spray protector de calor para la plancha", 9),
           ("shampoo for curly hair", 3), ("nail polish remover", 7)]
# Minimum top-1 accuracy of retrieval over QUERIES (normalized text, as core will embed it).
EMBED_MIN = 0.80


def normalize(text):
    """How core will embed catalog text: lower case, single spaces (Sage descriptions are upper case)."""
    return " ".join(text.lower().split())


def embed(texts, dims=None):
    """Embeddings of texts (one request), or raises with the error."""
    c = tachyon("embed", EMBED, *([f"--dims={dims}"] if dims else []), *texts)
    if c.rc != 0:
        raise RuntimeError(c.err[-300:])
    return [json.loads(line) for line in c.out.splitlines()], c.secs


def cosine(a, b):
    return sum(x * y for x, y in zip(a, b)) / (math.sqrt(sum(x * x for x in a)) * math.sqrt(sum(y * y for y in b)))


def retrieval(products, queries):
    """Top-1 hits and misses of each query against the products (cosine)."""
    hits, misses = 0, []
    for (query, want), q in zip(QUERIES, queries):
        scores = [cosine(q, p) for p in products]
        best = max(range(len(products)), key=scores.__getitem__)
        hits += best == want
        if best != want:
            misses.append(f"{query!r} → {PRODUCTS[best]} ({scores[best]:.3f}), expected {PRODUCTS[want]} ({scores[want]:.3f})")
    return hits, misses


def embed_cases(report):
    """Retrieval accuracy on the raw Sage descriptions and on normalized text, misses listed."""
    raw, _ = embed(PRODUCTS + [q for q, _ in QUERIES])
    norm, _ = embed([normalize(p) for p in PRODUCTS])
    report.check("embed.dims", all(len(v) == DIMS.get(EMBED, len(v)) and all(map(math.isfinite, v)) for v in raw),
                 f"{len(raw[0])} dims")
    queries, n = raw[len(PRODUCTS):], len(QUERIES)
    raw_hits, _ = retrieval(raw[: len(PRODUCTS)], queries)
    hits, misses = retrieval(norm, queries)
    report.say(f"  retrieval top-1: raw upper-case descriptions {raw_hits}/{n} · normalized {hits}/{n}")
    for miss in misses:
        report.say(f"  miss: {miss}")
    report.check("embed retrieval accuracy (normalized)", hits / n >= EMBED_MIN, f"{hits}/{n} (minimum {EMBED_MIN:.0%})")


# ---- the bench --------------------------------------------------------------------------------

def overhead():
    """The cost of an empty round trip (docker exec + socket), shown next to end-to-end times."""
    return statistics.median(tachyon("ps").secs for _ in range(5))


def bench_loads(report):
    """Each model's load from disk into VRAM, end to end."""
    for model in (CHAT, DECIDE, EMBED):
        tachyon("unload", model)
        report.metric(f"load.{model}_s", tachyon("load", model).secs, "s", False, False)


def bench_chat(report, have):
    """Prefill and decode speed for text, image and audio, as the engine reports them (median of 5)."""
    cases = [("text", ["Escribe un párrafo de unas 120 palabras sobre el cuidado del cabello rizado."]),
             ("image", ["Describe esta imagen en una frase.", f"--image={BOX}/red.png"])]
    if "clip-0.wav" in have:
        cases.append(("audio", ["Transcribe this audio.", f"--audio={BOX}/clip-0.wav"]))
    for name, args in cases:
        runs = [stats for _, stats in (chat(report, *args) for _ in range(5)) if stats]
        for key, unit in (("prefill_s", "s"), ("prefill_tps", "tok/s"), ("decode_tps", "tok/s")):
            if runs:
                report.metric(f"chat.{name}.{key}", statistics.median(r[key] for r in runs), unit, key != "prefill_s", True)


def bench_encoders(report):
    """Decide and embed latency (median of 10), and embedding throughput in batches of 64."""
    decide = [tachyon("decide", DECIDE, f"{BOX}/laya-0.json").secs for _ in range(10)]
    report.metric("decide.latency_s", statistics.median(decide), "s", False, False)
    single = [embed(["champú para pelo rizado"])[1] for _ in range(10)]
    report.metric("embed.latency_s", statistics.median(single), "s", False, False)
    batch = [f"{p} {i}" for i in range(13) for p in PRODUCTS][:64]
    report.metric("embed.batch64_texts_per_s", 64 / statistics.median(embed(batch)[1] for _ in range(3)), "texts/s", True, False)


def bench(report, have):
    """The round trip, load times, chat throughput, encoder latency and throughput."""
    report.metric("roundtrip_s", overhead(), "s", False, False)
    bench_loads(report)
    bench_chat(report, have)
    bench_encoders(report)


def compare(report):
    """Against baseline.json: engine timings within 5%, end-to-end within 20%."""
    if os.environ.get("SAVE_BASELINE") == "1":
        RESULTS.mkdir(parents=True, exist_ok=True)
        BASELINE.write_text(json.dumps(report.metrics, indent=1) + "\n")
        report.say(f"• baseline saved: {BASELINE.name}")
        return
    if not BASELINE.is_file():
        report.say("• no baseline yet: SAVE_BASELINE=1 records this run's numbers")
        return
    saved = json.loads(BASELINE.read_text())
    for name, m in report.metrics.items():
        if name in saved:
            old, new, tol = saved[name]["value"], m["value"], 0.05 if m["engine"] else 0.20
            worse = new < old * (1 - tol) if m["higher_better"] else new > old * (1 + tol)
            report.check(f"bench {name}", not worse, f"{new:g} {m['unit']} (baseline {old:g}, ±{tol:.0%})")


def run_bench(report, have):
    """The bench section: numbers, then the comparison with the baseline."""
    report.say("\n## Bench\n")
    bench(report, have)
    for name, m in report.metrics.items():
        report.say(f"  {name:<34} {m['value']:>10g} {m['unit']:<8}{'(engine)' if m['engine'] else '(end to end)'}")
    compare(report)


def save(report):
    """Records new or approved answers for review, and writes the report."""
    if report.recorded and (report.approve or set(report.recorded) ^ set(report.expected)):
        RESULTS.mkdir(parents=True, exist_ok=True)
        kept = {k: v if report.approve else report.expected.get(k, v) for k, v in report.recorded.items()}
        EXPECTED.write_text(json.dumps(kept, indent=1, ensure_ascii=False) + "\n")
        report.say(f"\n• {EXPECTED.relative_to(HERE)} updated: review the answers, then commit it")
    REPORT.parent.mkdir(exist_ok=True)
    REPORT.write_text("\n".join(report.lines) + "\n")


def main():
    report = Report()
    have = assets()
    upload(have)
    gpu = tachyon("gpu").out.splitlines()
    report.say(f"# Tachyon evaluation · {MACHINE} · {gpu[0] if gpu else 'GPU unknown'}\n{CHAT}, {DECIDE}, {EMBED}\n")
    for section in (text_cases, lambda r: media_cases(r, have), decide_cases, embed_cases):
        section(report)
    if BENCH:
        run_bench(report, have)
    save(report)
    report.say(f"\n{'✔ all checks passed' if not report.failed else f'✘ {report.failed} check(s) failed'} · report: {REPORT}")
    return 1 if report.failed else 0


if __name__ == "__main__":
    sys.exit(main())
