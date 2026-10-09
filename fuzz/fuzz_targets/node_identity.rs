//! `node::identity::decode` gives an identity exactly for the bytes of `node.key` with
//! the tag and the CRC32C, and that identity encodes to the same bytes.
//!
//! Input: the 68 bytes of the file, or its first 64 bytes, to which the target appends
//! their CRC32C.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| node::fuzz::identity(data));
