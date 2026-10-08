//! open62541 decodes any input as each of its built-in types with no memory fault or
//! leak, and the encoding of each value that it decodes decodes to a value with the
//! same encoding. Until #435 is fixed, the encoding decodes with as many zeros after
//! it as its length.
//!
//! Input: two bytes that pick the type, then the encoded value
//! (`connector_opcua::fuzz::decode`). A new open62541 copy can change the type of an
//! input.
//!
//! The C code gets no coverage or AddressSanitizer flags from cargo-fuzz. Build with
//! `CC=clang CFLAGS="-fsanitize=fuzzer-no-link,address"`.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| connector_opcua::fuzz::decode(data));
