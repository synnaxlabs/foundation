//! `config::openssh::private_key` never panics on the text of a key file. A key that it
//! reads writes back with `key_file` and reads again as itself, and CRLF line ends do
//! not change what it gives.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use config::openssh;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let read = openssh::private_key(text).map(|key| key.0);
    if let Ok(bytes) = read {
        let written = openssh::key_file(&types::ed25519::PrivateKey(bytes), "");
        let again = openssh::private_key(&written).map(|key| key.0);
        assert_eq!(again, Ok(bytes), "the written file does not read back");
    }
    if !text.contains('\r') {
        let crlf = openssh::private_key(&text.replace('\n', "\r\n")).map(|key| key.0);
        assert_eq!(crlf, read, "CRLF line ends changed it");
    }
});
