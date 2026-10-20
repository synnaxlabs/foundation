//! `config::openssh::private_key` never panics on the bytes of a key file. A key that
//! it reads writes back with `key_file` and reads again as itself, and CRLF line ends
//! do not change what it gives.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use config::openssh;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|file: &[u8]| {
    let read = openssh::private_key(file).map(|key| key.0);
    if let Ok(bytes) = read {
        let written = openssh::key_file(&types::ed25519::PrivateKey(bytes), "");
        let again = openssh::private_key(written.as_bytes()).map(|key| key.0);
        assert_eq!(again, Ok(bytes), "the written file does not read back");
    }
    if !file.contains(&b'\r') {
        let mut crlf = Vec::new();
        for byte in file {
            if *byte == b'\n' {
                crlf.push(b'\r');
            }
            crlf.push(*byte);
        }
        let crlf = openssh::private_key(&crlf).map(|key| key.0);
        assert_eq!(crlf, read, "CRLF line ends changed it");
    }
});
