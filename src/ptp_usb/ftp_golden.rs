//! Equivalence tests for the FTP-profile layout spec (finding b7d3#45).
//!
//! The encoder, the strict decoder, the secret-span function and the `as_encodable` gate used to be
//! three hand-written copies of the blob layout; they are now driven by one spec
//! (`ftp_layout::FTP_LAYOUT`). `tests/fixtures/ftp_layout_golden.json` records what the hand-written
//! code answered, and [`the_live_code_reproduces_the_golden_report`] holds the spec-driven code to it.
//!
//! The report ([`build_report`]) is what an implementation does with a fixed, deterministic set of
//! inputs: the six real blobs of `tests/fixtures/ftp_profile_vectors.json` with its named edits and
//! every truncation; a few thousand pseudo-random profiles (every field at, below and above its
//! limit, non-ASCII, control characters, random camera networks) for the encoder; and mutated blobs
//! (every single-byte edit of the reference capture, nine edits at every position of the others,
//! multi-byte UTF-8 written into them, truncations, insertions, extensions, random garbage) for the
//! decoder, each with the exact error `(field, offset, detail)`, the secret spans, the masked copy and
//! the gate's answer. Small sets are in the golden file line by line, the large ones as a count and a
//! SHA-256 of their lines.
//!
//! The golden file was produced from the hand-written code itself (a verbatim, renamed copy kept in
//! the tree while the two were compared on every input, then deleted); [`write_the_golden_file`]
//! rewrites it from the LIVE code, which is only right after a deliberate behaviour change:
//! `FTP_GOLDEN_WRITE=1 cargo test -p nikon-fleet --lib ftp_golden::write_the_golden_file -- --ignored`,
//! and the commit must say why the old answers were wrong.

use super::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ops::Range;

// ── the code under test ──────────────────────────────────────────────────────────────────────

/// Spans as `(field key, content range)` in wire order.
type Spans = Vec<(&'static str, Range<usize>)>;

fn enc(profile: &FtpProfile, network: &FtpCameraNetwork) -> Result<Vec<u8>, PtpUsbError> {
    encode_ftp_profile_with(profile, network)
}

fn dec(blob: &[u8]) -> Result<(DecodedFtpProfile, Spans), PtpUsbError> {
    decode_ftp_profile_spans(blob).map(|(d, s)| (d, s.spans.into_iter().map(|s| (s.field, s.range)).collect()))
}

// ── deterministic inputs ─────────────────────────────────────────────────────────────────────

/// splitmix64: a few lines, no crate, the same sequence on every machine.
pub(super) struct Rng(pub(super) u64);

impl Rng {
    pub(super) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
}

const ASCII_ALNUM: &[char] = &['a', 'b', 'Z', '0', '9', 'x', 'q', 'M'];
const ASCII_PUNCT: &[char] = &[' ', '-', '_', '.', ':', '/', '!', '~', '"', '\\', '\'', '*'];
const NON_ASCII: &[char] = &['é', 'ü', 'ß', 'あ', '漢', '\u{FFFD}', '😀', '𝄞'];
const CONTROLS: &[char] = &['\0', '\t', '\n', '\r', '\u{1}', '\u{7f}', '\u{85}', '\u{9f}'];

/// A string for a field whose limit is `limit` (bytes or UTF-16 units, per the field). With `stress`
/// off it is valid for either kind of field (ASCII letters, digits and punctuation, at most `limit`
/// long, with the limit itself and zero over-represented); with it on, lengths at, just below and
/// just above the limit are over-represented, and so are non-ASCII characters (BMP and astral) and
/// control characters.
fn gen_string(rng: &mut Rng, limit: usize, stress: bool) -> String {
    let target = if stress {
        match rng.below(12) {
            0 => 0,
            1 => 1,
            2 => limit.saturating_sub(1),
            3 | 4 => limit,
            5 => limit + 1,
            6 => limit + 2,
            7 => 250 + rng.below(8),
            8 => limit + 40,
            _ => rng.below(limit + 3),
        }
    } else {
        match rng.below(8) {
            0 => 0,
            1 => limit,
            _ => rng.below(limit + 1),
        }
    };
    let flavour = if stress { rng.below(20) } else { rng.below(12) };
    let mut s = String::new();
    // Grow by characters until the UTF-8 length reaches the target (for the byte-limited fields this is
    // the quantity that matters; for the UTF-16 ones the char/unit mix still straddles the limit).
    while s.len() < target {
        let pool: &[char] = match flavour {
            0..=8 => ASCII_ALNUM,
            9..=11 => if rng.below(3) == 0 { ASCII_PUNCT } else { ASCII_ALNUM },
            12..=14 => if rng.below(4) == 0 { NON_ASCII } else { ASCII_ALNUM },
            15..=17 => if rng.below(8) == 0 { CONTROLS } else { ASCII_ALNUM },
            _ => match rng.below(3) {
                0 => NON_ASCII,
                1 => CONTROLS,
                _ => ASCII_PUNCT,
            },
        };
        s.push(pool[rng.below(pool.len())]);
    }
    // A valid string never exceeds the limit even when the last character was multi-byte (none is).
    s
}

/// One random profile: three in ten are stress profiles (any field may be out of range), the rest are
/// valid except that one in four has exactly one stressed field.
pub(super) fn gen_profile(rng: &mut Rng) -> (FtpProfile, FtpCameraNetwork) {
    let all_stress = rng.below(10) < 3;
    let one_stress = if !all_stress && rng.below(4) == 0 { Some(rng.below(6)) } else { None };
    let stress = |idx: usize| all_stress || one_stress == Some(idx);
    let profile = FtpProfile {
        profile_name: gen_string(rng, 253, stress(0)),
        ssid_24ghz: gen_string(rng, 32, stress(1)),
        ssid_5ghz: gen_string(rng, 32, stress(2)),
        host: gen_string(rng, 199, stress(3)),
        port: match rng.below(6) {
            0 => 0,
            1 => 21,
            2 => 8080,
            3 => 65535,
            _ => rng.next() as u16,
        },
        username: gen_string(rng, 64, stress(4)),
        password: gen_string(rng, 64, stress(5)),
    };
    let network = match rng.below(5) {
        0 => FtpCameraNetwork { ipv4: [rng.byte(), rng.byte(), rng.byte(), rng.byte()], prefix_len: rng.byte() },
        1 => FtpCameraNetwork { ipv4: [0, 0, 0, 0], prefix_len: 0 },
        _ => FtpCameraNetwork::CAPTURED,
    };
    (profile, network)
}

/// Profiles that sit exactly on each field's boundary: every field at 0, 1, limit-1, limit, limit+1
/// units, in ASCII, in two-byte characters and in surrogate pairs, alone in an otherwise valid profile.
fn boundary_profiles() -> Vec<FtpProfile> {
    let base = FtpProfile {
        profile_name: "TEST".into(),
        ssid_24ghz: "test-ssid".into(),
        ssid_5ghz: "test-ssid-5g".into(),
        host: "192.168.1.1".into(),
        port: 21,
        username: "user".into(),
        password: "pass".into(),
    };
    let limits = [("profile_name", 253usize), ("ssid_24ghz", 32), ("ssid_5ghz", 32), ("host", 199), ("username", 64), ("password", 64)];
    let mut out = Vec::new();
    for (key, limit) in limits {
        for len in [0, 1, limit - 1, limit, limit + 1, limit + 2, 254, 255, 256] {
            for unit in ["a", "é", "😀", "\t"] {
                // `len` characters of `unit`, and a string of `len` bytes/units made of `unit` where it fits.
                let value: String = unit.repeat(len);
                let mut p = base.clone();
                match key {
                    "profile_name" => p.profile_name = value,
                    "ssid_24ghz" => p.ssid_24ghz = value,
                    "ssid_5ghz" => p.ssid_5ghz = value,
                    "host" => p.host = value,
                    "username" => p.username = value,
                    _ => p.password = value,
                }
                out.push(p);
            }
        }
    }
    for port in [0u16, 1, 21, 255, 256, 8080, 32768, 65535] {
        out.push(FtpProfile { port, ..base.clone() });
    }
    out
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("hex digit")).collect()
}

fn vectors() -> Value {
    serde_json::from_str(include_str!("../../tests/fixtures/ftp_profile_vectors.json")).expect("fixture is JSON")
}

fn apply_edit(base: &[u8], edit: &Value) -> Vec<u8> {
    let mut b = base.to_vec();
    for set in edit["set"].as_array().map(Vec::as_slice).unwrap_or_default() {
        let off = set[0].as_u64().expect("offset") as usize;
        let raw = unhex(set[1].as_str().expect("hex"));
        b[off..off + raw.len()].copy_from_slice(&raw);
    }
    if let Some(a) = edit["append"].as_str() {
        b.extend(unhex(a));
    }
    if let Some(t) = edit["truncate"].as_u64() {
        b.truncate(t as usize);
    }
    b
}

// ── rendering ────────────────────────────────────────────────────────────────────────────────

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn short(bytes: &[u8]) -> String {
    sha_hex(bytes)[..12].to_string()
}

fn render_encode(r: &Result<Vec<u8>, PtpUsbError>) -> String {
    match r {
        Ok(b) => format!("ok:{}:{}", b.len(), short(b)),
        Err(e) => format!("err:{e:?}"),
    }
}

/// Every field of a decoded profile in full (secrets included: this is a comparison of two
/// implementations, not output a user sees) and the answers derived from it.
fn render_decode(r: &Result<(DecodedFtpProfile, Spans), PtpUsbError>) -> String {
    match r {
        Ok((d, spans)) => {
            let enc = d.as_encodable().map(|p| (p.profile_name, p.ssid_24ghz, p.ssid_5ghz, p.host, p.port, p.username, p.password));
            let m = d.with_secrets_masked();
            let full = format!(
                "{:?}|{:?}|{:?}|{:?}|{}|{}|{:?}|{}|{:?}|{:?}|{:?}|{:?}|{}|{}|{}|{:?}\nspans={spans:?}\nenc={enc:?}\nmasked={:?}|{:?}\nsecrets={:?}",
                d.profile_name, d.timestamps, d.camera_network, d.ssid_24ghz, d.ssid24_kind, d.ssid24_lead, d.ssid_5ghz,
                d.ssid5_kind, d.host, d.folder, d.username, d.password, d.ftp_port, d.option_flags, d.second_port, d.ipv6_fields,
                m.password, m.ssid_5ghz, d.secret_texts(),
            );
            format!("ok:{}", short(full.as_bytes()))
        }
        Err(PtpUsbError::FtpBlobMalformed { what, offset, detail }) => {
            format!("err:{what}@{offset}:{}", short(detail.as_bytes()))
        }
        Err(other) => format!("other:{other:?}"),
    }
}

// ── the report ───────────────────────────────────────────────────────────────────────────────

/// Small sets are kept line by line (`named`); the big ones as every line, hashed into a digest at
/// write time (`groups`).
#[derive(Default)]
struct Report {
    named: BTreeMap<&'static str, Vec<String>>,
    groups: BTreeMap<&'static str, Vec<String>>,
}

/// The report, computed once (the tests share it).
fn report() -> &'static Report {
    static REPORT: std::sync::OnceLock<Report> = std::sync::OnceLock::new();
    REPORT.get_or_init(build_report)
}

/// The six real blobs plus encoder output for the first random profiles that encode: bases for mutation.
fn build_report() -> Report {
    let mut rep = Report::default();
    let v = vectors();
    let profiles = v["profiles"].as_array().unwrap();
    assert_eq!(profiles.len(), 6, "the fixture changed: the report would no longer be the golden one");
    let fixtures: Vec<(String, Vec<u8>)> =
        profiles.iter().map(|p| (p["name"].as_str().unwrap().to_string(), unhex(p["hex"].as_str().unwrap()))).collect();
    let reference = fixtures.iter().find(|(n, _)| n == "reference").expect("reference").1.clone();

    // 1. The six real blobs: decode, gate, re-encode when the gate says it can.
    let mut lines = Vec::new();
    for (name, blob) in &fixtures {
        let r = dec(blob);
        let mut line = format!("{name}: {}", render_decode(&r));
        if let Ok((d, _)) = &r {
            if let Some(p) = d.as_encodable() {
                line.push_str(&format!(" reencode={}", render_encode(&enc(&p, &d.camera_network))));
                line.push_str(&format!(" same_bytes={}", enc(&p, &d.camera_network).ok().as_deref() == Some(blob.as_slice())));
            }
        }
        lines.push(line);
    }
    rep.named.insert("fixture_blobs", lines);

    // 2. The fixture's named edits, and every truncation of the reference blob.
    rep.named.insert(
        "fixture_rejects",
        v["rejects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| format!("{}: {}", r["name"].as_str().unwrap(), render_decode(&dec(&apply_edit(&reference, &r["edit"])))))
            .collect(),
    );
    rep.named.insert(
        "reference_truncations",
        (0..=reference.len()).map(|cut| format!("{cut}: {}", render_decode(&dec(&reference[..cut])))).collect(),
    );

    // 3. Encoder: the boundary profiles, then thousands of random ones (random camera networks too).
    let mut rng = Rng(0x00F7_5EED_0045);
    rep.named.insert(
        "boundary_encode",
        boundary_profiles()
            .iter()
            .map(|p| render_encode(&enc(p, &FtpCameraNetwork::CAPTURED)))
            .collect(),
    );
    let mut random_lines = Vec::new();
    let mut bases: Vec<Vec<u8>> = fixtures.iter().map(|(_, b)| b.clone()).collect();
    let mut roundtrip = Vec::new();
    for n in 0..6000 {
        let (p, net) = gen_profile(&mut rng);
        let r = enc(&p, &net);
        random_lines.push(render_encode(&r));
        if let Ok(blob) = &r {
            // What the decoder, the gate and the encoder make of the encoder's own output.
            let d = dec(blob);
            roundtrip.push(render_decode(&d));
            if bases.len() < 6 + 12 && n % 61 == 0 {
                bases.push(blob.clone());
            }
        }
    }
    rep.groups.insert("random_encode", random_lines);
    rep.groups.insert("random_encode_then_decode", roundtrip);

    // 4. Decoder over mutated blobs.
    let values: [fn(u8) -> u8; 9] = [
        |_| 0x00, |_| 0x01, |_| 0x03, |_| 0x05, |_| 0x40, |_| 0x81, |_| 0xff, |o| o ^ 0x80, |o| o.wrapping_add(1),
    ];
    let mut edits = Vec::new();
    let mut truncs = Vec::new();
    let mut inserts = Vec::new();
    let mut extends = Vec::new();
    // Valid multi-byte UTF-8 written over two or three bytes: the u32-length text fields decode it (the
    // decoder reads UTF-8) while the encoder cannot write it, which a single-byte edit never reaches.
    let mut utf8 = Vec::new();
    let multibyte: [&[u8]; 3] = ["é".as_bytes(), "中".as_bytes(), "😀".as_bytes()];
    for (bi, base) in bases.iter().enumerate() {
        let exhaustive = bi < 6 && fixtures[bi].0 == "reference";
        for pos in 0..base.len() {
            let orig = base[pos];
            let mut b = base.clone();
            if exhaustive {
                for val in 0..=255u8 {
                    b[pos] = val;
                    edits.push(render_decode(&dec(&b)));
                }
            } else {
                for f in values {
                    b[pos] = f(orig);
                    edits.push(render_decode(&dec(&b)));
                }
            }
            for ins in [0x00u8, 0x01] {
                let mut b = base.clone();
                b.insert(pos, ins);
                inserts.push(render_decode(&dec(&b)));
            }
        }
        for pos in 0..base.len() {
            for bytes in multibyte {
                if pos + bytes.len() <= base.len() {
                    let mut b = base.clone();
                    b[pos..pos + bytes.len()].copy_from_slice(bytes);
                    utf8.push(render_decode(&dec(&b)));
                }
            }
        }
        for cut in 0..=base.len() {
            truncs.push(render_decode(&dec(&base[..cut])));
        }
        for tail in [&[0u8][..], &[0x81], &[0, 0, 0, 0], &[0xff, 0xff, 0xff, 0xff, 0xff], base.as_slice()] {
            let mut b = base.clone();
            b.extend_from_slice(tail);
            extends.push(render_decode(&dec(&b)));
        }
    }
    rep.groups.insert("decode_single_byte_edits", edits);
    rep.groups.insert("decode_truncations", truncs);
    rep.groups.insert("decode_insertions", inserts);
    rep.groups.insert("decode_extensions", extends);
    rep.groups.insert("decode_multibyte_utf8_edits", utf8);

    // 5. Garbage: random bytes, and a valid prefix of a real blob followed by random bytes.
    let mut garbage = Vec::new();
    for n in 0..3000 {
        let len = rng.below(420);
        let mut b: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
        if n % 2 == 1 {
            let keep = rng.below(reference.len());
            let mut head = reference[..keep].to_vec();
            head.extend_from_slice(&b[..b.len().min(40)]);
            b = head;
        }
        garbage.push(render_decode(&dec(&b)));
    }
    rep.groups.insert("decode_garbage", garbage);
    rep
}

fn report_to_json(rep: &Report) -> Value {
    let named: serde_json::Map<String, Value> = rep.named.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
    let groups: serde_json::Map<String, Value> = rep
        .groups
        .iter()
        .map(|(k, lines)| (k.to_string(), json!({ "count": lines.len(), "sha256": sha_hex(lines.join("\n").as_bytes()) })))
        .collect();
    json!({
        "_about": "Golden report of the FTP-profile encoder, strict decoder, secret spans and as_encodable gate, \
                   produced by the hand-written code that ftp_layout replaced (b7d3#45). Regenerate only for a \
                   deliberate behaviour change: see src/ptp_usb/ftp_golden.rs.",
        "named": named,
        "groups": groups,
    })
}

const GOLDEN_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ftp_layout_golden.json");

#[test]
#[ignore = "writes tests/fixtures/ftp_layout_golden.json; run with FTP_GOLDEN_WRITE=1"]
fn write_the_golden_file() {
    assert_eq!(std::env::var("FTP_GOLDEN_WRITE").as_deref(), Ok("1"), "set FTP_GOLDEN_WRITE=1 to overwrite the golden file");
    let text = serde_json::to_string_pretty(&report_to_json(&build_report())).unwrap();
    std::fs::write(GOLDEN_PATH, text + "\n").unwrap();
}

#[test]
fn the_report_is_not_vacuous() {
    // Both outcomes occur in every big set, so a golden match is a statement about real answers.
    let rep = report();
    let enc = &rep.groups["random_encode"];
    assert!(enc.len() >= 6000);
    assert!(enc.iter().filter(|l| l.starts_with("ok:")).count() > 2000);
    assert!(enc.iter().filter(|l| l.starts_with("err:")).count() > 500);
    let edits = &rep.groups["decode_single_byte_edits"];
    assert!(edits.len() > 100_000);
    let ok = edits.iter().filter(|l| l.starts_with("ok:")).count();
    assert!(ok > 1000 && ok < edits.len(), "edits both survive and are rejected: {ok} of {}", edits.len());
    let utf8_ok = rep.groups["decode_multibyte_utf8_edits"].iter().filter(|l| l.starts_with("ok:")).count();
    assert!(utf8_ok > 100, "multi-byte text reaches the gate: {utf8_ok}");
    assert!(rep.groups["random_encode_then_decode"].iter().all(|l| l.starts_with("ok:")), "the encoder's own output always decodes");
}

#[test]
fn the_live_code_reproduces_the_golden_report() {
    let golden: Value = serde_json::from_str(&std::fs::read_to_string(GOLDEN_PATH).expect("golden file")).expect("golden JSON");
    let got = report_to_json(report());
    for (name, want) in golden["named"].as_object().unwrap() {
        assert_eq!(&got["named"][name], want, "named set {name} differs from the golden file");
    }
    for (name, want) in golden["groups"].as_object().unwrap() {
        assert_eq!(&got["groups"][name], want, "group {name} differs from the golden file (count or SHA-256 of its lines)");
    }
    assert_eq!(
        golden["named"].as_object().unwrap().len() + golden["groups"].as_object().unwrap().len(),
        got["named"].as_object().unwrap().len() + got["groups"].as_object().unwrap().len(),
        "the set of report sections changed"
    );
}
