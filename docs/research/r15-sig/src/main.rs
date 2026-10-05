use std::time::Instant;
use ed25519_dalek::{Signer, SigningKey, Verifier};
use aws_lc_rs::signature::{self, Ed25519KeyPair, KeyPair, UnparsedPublicKey};

fn main() {
    // A token is ~128 bytes: subject key (32), audience node key (32), nonce (16),
    // not_before (8), not_after (8), scope hash (32).
    let msg = [7u8; 128];
    let n = 20_000;

    let sk = SigningKey::generate(&mut rand_core::OsRng);
    let vk = sk.verifying_key();
    let sig = sk.sign(&msg);
    for _ in 0..1000 { vk.verify(&msg, &sig).unwrap(); }
    let t = Instant::now();
    for _ in 0..n { std::hint::black_box(sk.sign(std::hint::black_box(&msg))); }
    let sign_ns = t.elapsed().as_nanos() as f64 / n as f64;
    let t = Instant::now();
    for _ in 0..n { vk.verify(std::hint::black_box(&msg), &sig).unwrap(); }
    let verify_ns = t.elapsed().as_nanos() as f64 / n as f64;
    let t = Instant::now();
    for _ in 0..n { vk.verify_strict(std::hint::black_box(&msg), &sig).unwrap(); }
    let verify_strict_ns = t.elapsed().as_nanos() as f64 / n as f64;
    println!("ed25519-dalek sign {:.1} us, verify {:.1} us, verify_strict {:.1} us", sign_ns/1e3, verify_ns/1e3, verify_strict_ns/1e3);

    let rng = aws_lc_rs::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let pk = UnparsedPublicKey::new(&signature::ED25519, kp.public_key().as_ref().to_vec());
    let s = kp.sign(&msg);
    let t = Instant::now();
    for _ in 0..n { std::hint::black_box(kp.sign(std::hint::black_box(&msg))); }
    let a_sign = t.elapsed().as_nanos() as f64 / n as f64;
    let t = Instant::now();
    for _ in 0..n { pk.verify(std::hint::black_box(&msg), s.as_ref()).unwrap(); }
    let a_verify = t.elapsed().as_nanos() as f64 / n as f64;
    println!("aws-lc-rs sign {:.1} us, verify {:.1} us", a_sign/1e3, a_verify/1e3);

    // Per-frame alternative for comparison: keyed BLAKE3 MAC over a 4 KiB frame.
    let key = [3u8; 32];
    let frame = vec![1u8; 4096];
    let t = Instant::now();
    for _ in 0..200_000 { std::hint::black_box(blake3::keyed_hash(&key, std::hint::black_box(&frame))); }
    let mac = t.elapsed().as_nanos() as f64 / 200_000.0;
    let t = Instant::now();
    for _ in 0..20_000 { std::hint::black_box(sk.sign(std::hint::black_box(&frame))); }
    let sign4k = t.elapsed().as_nanos() as f64 / 20_000.0;
    println!("blake3 keyed MAC over 4 KiB {:.0} ns; ed25519 sign over 4 KiB {:.1} us", mac, sign4k/1e3);
}
