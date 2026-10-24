//! Benchmarks of core's per-message hot paths, without extra crates: each case runs for about
//! a second and prints ns/op. `make bench` runs this under `perf stat`/`perf record`.

use kapa_core::gmail::Allowlist;
use kapa_core::mail::{self, Attachment, Outgoing};
use std::hint::black_box;
use std::time::{Duration, Instant};

/// Runs `f` repeatedly for about `BENCHTIME` (default 1s) and prints its cost per call.
fn bench<F: FnMut()>(name: &str, mut f: F) {
    let budget = std::env::var("BENCHTIME").ok().and_then(|v| v.trim_end_matches(char::is_alphabetic).parse::<f64>().ok()).unwrap_or(1.0);
    let (mut iters, start) = (0u64, Instant::now());
    while start.elapsed() < Duration::from_secs_f64(budget) {
        for _ in 0..64 {
            f();
        }
        iters += 64;
    }
    let ns = start.elapsed().as_nanos() / u128::from(iters);
    println!("{name:<32} {iters:>12} iters {ns:>10} ns/op");
}

/// Sample outgoing message: text plus a 64 KiB attachment.
fn sample() -> Outgoing {
    Outgoing {
        to: vec!["ana@example.com".into()],
        subject: "Factura nº 7".into(),
        text: "Hola Ana,\r\n\r\nadjunto la factura.\r\n".repeat(20),
        attachments: vec![Attachment { filename: "factura.pdf".into(), mime: "application/pdf".into(), data: vec![7; 64 * 1024] }],
        ..Outgoing::default()
    }
}

/// A named benchmark case.
type Case = (&'static str, fn(&Inputs));

/// Shared inputs of the benchmark cases.
struct Inputs {
    /// Outgoing message with a 64 KiB attachment.
    out: Outgoing,
    /// The same message as raw bytes.
    raw: Vec<u8>,
    /// Allowlist with 100 senders.
    allow: Allowlist,
    /// 1 MiB of data.
    mib: Vec<u8>,
}

/// Allowlist lookup on the per-message hot path.
fn allowlist_check(i: &Inputs) {
    black_box(i.allow.contains(black_box("bot@kapalua.es"), black_box("s42@example.com")));
}

/// Parsing a received email with a 64 KiB attachment.
fn parse_email(i: &Inputs) {
    black_box(mail::parse("bot@kapalua.es", 1, 1, 1, 0, black_box(&i.raw)));
}

/// Building an outgoing email with a 64 KiB attachment.
fn build_email(i: &Inputs) {
    black_box(mail::build("bot@kapalua.es", black_box(&i.out), 0, "id@kapalua.es"));
}

/// Base64 of 1 MiB.
fn base64_1mib(i: &Inputs) {
    black_box(mail::base64(black_box(&i.mib)));
}

/// SMTP dot-stuffing of a 64 KiB message.
fn dot_stuff(i: &Inputs) {
    black_box(kapa_core::smtp::dot_stuff(black_box(&i.raw)));
}

fn main() {
    let filter = std::env::args().nth(1).filter(|a| !a.starts_with('-')).unwrap_or_default();
    let out = sample();
    let inputs = Inputs {
        raw: mail::build("bot@kapalua.es", &out, 1_792_840_002, "id@kapalua.es"),
        out,
        allow: Allowlist::default(),
        mib: vec![1; 1 << 20],
    };
    for n in 0..100 {
        inputs.allow.insert("bot@kapalua.es", &format!("s{n}@example.com"));
    }
    let cases: [Case; 5] = [
        ("allowlist_check", allowlist_check),
        ("parse_email_64k_attachment", parse_email),
        ("build_email_64k_attachment", build_email),
        ("base64_1mib", base64_1mib),
        ("dot_stuff_64k", dot_stuff),
    ];
    for (name, case) in cases.into_iter().filter(|(name, _)| name.contains(filter.as_str())) {
        bench(name, || case(&inputs));
    }
}
