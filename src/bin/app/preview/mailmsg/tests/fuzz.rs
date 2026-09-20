#![cfg(test)]

//! The mutation fuzz over the .eml and .msg seeds.

use super::*;

pub(super) struct Rng(u64);

impl Rng {
    // Named `next_u64`, not `next`: an inherent `next` on a non-iterator trips
    // clippy::should_implement_trait, and `-D warnings` makes that a build failure.
    pub(super) fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub(super) fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
}

/// One mutation of `seed`: flip a byte, splice a run, truncate, or grow.
pub(super) fn mutate(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut b = seed.to_vec();
    if b.is_empty() {
        return b;
    }
    match rng.below(6) {
        0 => {
            let i = rng.below(b.len());
            b[i] = (rng.next_u64() & 0xFF) as u8;
        }
        1 => {
            let i = rng.below(b.len());
            b[i] ^= 1 << rng.below(8);
        }
        2 => {
            let i = rng.below(b.len());
            let v = (rng.next_u64() & 0xFF) as u8;
            let n = rng.below(32).min(b.len() - i);
            b[i..i + n].fill(v);
        }
        3 => {
            let cut = rng.below(b.len());
            b.truncate(cut);
        }
        4 => {
            let i = rng.below(b.len());
            let n = rng.below(64);
            let filler: Vec<u8> = (0..n).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
            b.splice(i..i, filler);
        }
        _ => {
            // Interesting integers where a length or a sector number might live.
            let i = rng.below(b.len().saturating_sub(4).max(1));
            let v: u32 = *[0u32, 1, 0x7FFF_FFFF, 0xFFFF_FFFE, 0xFFFF_FFFF]
                .get(rng.below(5))
                .unwrap_or(&0);
            if i + 4 <= b.len() {
                b[i..i + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
    }
    b
}

/// The two mail parsers under mutation. A panic here is fatal in the shipped build —
/// the preview app is `panic = "abort"`, so this is a crash, not a failed preview.
#[test]
fn mutation_fuzz_over_eml_and_msg_never_panics() {
    let eml_seed = b"From: Ada <ada@example.com>\r\n\
         To: Alan <alan@example.com>\r\n\
         Subject: =?utf-8?B?SGVsbG8gd29ybGQ=?=\r\n\
         Date: Mon, 1 Jan 2024 12:00:00 +0000\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"outer\"\r\n\
         \r\n\
         --outer\r\n\
         Content-Type: multipart/alternative; boundary=\"inner\"\r\n\
         \r\n\
         --inner\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Transfer-Encoding: quoted-printable\r\n\
         \r\n\
         Hello =E2=80=94 there.\r\n\
         --inner\r\n\
         Content-Type: text/html; charset=iso-8859-1\r\n\
         \r\n\
         <html><body><p>Hello</p></body></html>\r\n\
         --inner--\r\n\
         --outer\r\n\
         Content-Type: application/pdf; name=\"report.pdf\"\r\n\
         Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
         Content-Transfer-Encoding: base64\r\n\
         \r\n\
         JVBERi0xLjQKJcTl8uXrp/Og0MTGCg==\r\n\
         --outer--\r\n"
        .to_vec();
    let msg_seed = build_msg(&[
        ("__substg1.0_0037001F", "Quarterly numbers"),
        ("__substg1.0_1000001F", "Figures attached, ask if unclear."),
        ("__substg1.0_0C1A001F", "Ada Lovelace"),
        ("__substg1.0_5D01001F", "ada@example.com"),
        ("__substg1.0_3707001F", "report.pdf"),
        ("__substg1.0_3707001F", "photo.jpg"),
        ("__properties_version1.0", "not really a property table"),
    ]);

    let targets: [FuzzTarget; 2] = [
        ("eml_to_markdown", |b| drop(eml_to_markdown(b)), &eml_seed),
        ("msg_to_markdown", |b| drop(msg_to_markdown(b)), &msg_seed),
    ];

    let mut rng = Rng(0x5EED_1234_ABCD_9876);
    for (name, f, seed) in targets {
        // Every prefix first — truncation is the most productive single class for the
        // short-read panics these two parsers are exposed to.
        for cut in 0..seed.len().min(2_048) {
            let input = &seed[..cut];
            if let Err(e) = std::panic::catch_unwind(|| f(input)) {
                panic!("PANIC in {name} on the {cut}-byte prefix: {}", pmsg(&*e));
            }
        }
        for it in 0..20_000u32 {
            let input = mutate(&mut rng, seed);
            let head: String = input
                .iter()
                .take(32)
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            if let Err(e) = std::panic::catch_unwind(|| f(&input)) {
                panic!(
                    "PANIC in {name} at iteration {it} ({} bytes): {}\n  head: {head}",
                    input.len(),
                    pmsg(&*e)
                );
            }
        }
    }
}

/// A panic payload as a printable string.
pub(super) fn pmsg(e: &(dyn std::any::Any + Send)) -> String {
    e.downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| e.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".into())
}
