//! The node's key and private key, in the file `node.key` of the data directory.
//!
//! The file is 68 bytes: the tag, the node key (big-endian), the Ed25519 private key,
//! and the CRC32C of those 64 bytes (little-endian), in one sector ([`crate::sector`]).
//! 68 zero bytes are a key that a crash kept from being written.

use std::path::Path;

use env::files::Files;
use types::ed25519::PrivateKey;
use types::time::Stamp;

use crate::Error;
use crate::sector::{self, Held};

/// The name of the file.
pub(crate) const FILE: &str = "node.key";
/// The first bytes of the file; a new form gets a new tag.
const TAG: &[u8; 16] = b"foundation/key/1";
/// The length of the tag, the node key, and the private key.
pub(crate) const BODY: usize = 64;
/// The length of the file: the body and its CRC32C.
pub(crate) const LEN: usize = BODY + 4;

/// The node's key and the private key that its transport proves.
#[derive(Clone, Debug)]
pub(crate) struct Identity {
    pub(crate) key: types::node::Key,
    pub(crate) private_key: PrivateKey,
}

/// The identity in `node.key` of `files`. When the file is not there, has no bytes,
/// or holds 68 zero bytes, makes a new one at mesh time from `clock`, once it has
/// mesh time, and from `entropy`. Writes the identity back and makes it durable
/// before it returns, also one it read: a failed sync of an earlier start can leave
/// a key that a read sees but a crash loses. Never writes another key over a file
/// that holds one.
///
/// # Errors
///
/// [`Error::Key`] for a file of another length that is not 0, or of another tag or
/// checksum, and [`Error::Directory`] for a file call that fails.
pub(crate) async fn load(
    files: &Files,
    clock: &clock::Reader,
    entropy: &env::entropy::Entropy,
) -> Result<Identity, Error> {
    let opened = sector::open(files, Path::new(FILE), TAG).await;
    let (file, held) = opened.map_err(|error| match error {
        env::files::Error::Length { .. } => Error::Key,
        error => Error::Directory(error),
    })?;
    let identity = match held {
        Held::Nothing => {
            clock.reach(Stamp::from_nanos(i64::MIN)).await;
            let now = match clock.status() {
                clock::Status::Synced(now) | clock::Status::Holdover(now, _) => {
                    now.time()
                }
                clock::Status::Unsynced(_) => {
                    unreachable!("invariant: mesh time has come")
                }
            };
            create(now, entropy)
        }
        Held::Written(bytes) => fields(&bytes),
        Held::Foreign => return Err(Error::Key),
    };
    sector::write(files, &file, &encode(&identity))
        .await
        .map_err(Error::Directory)?;
    Ok(identity)
}

/// Writes `identity` to `node.key` in `files` when the file is not there, has no
/// bytes, or holds 68 zero bytes, and makes it durable.
///
/// # Errors
///
/// [`Error::Directory`] with [`env::files::Error::Exists`] when the file holds 68
/// bytes that are not all zero, with [`env::files::Error::Length`] when it has another
/// length that is not 0, and with the error of each other file call that fails.
#[cfg(feature = "sim")]
pub(crate) async fn store(files: &Files, identity: &Identity) -> Result<(), Error> {
    let (file, held) = sector::open::<LEN>(files, Path::new(FILE), TAG)
        .await
        .map_err(Error::Directory)?;
    if held != Held::Nothing {
        let path = std::path::PathBuf::from(FILE);
        return Err(Error::Directory(env::files::Error::Exists { path }));
    }
    sector::write(files, &file, &encode(identity))
        .await
        .map_err(Error::Directory)
}

/// A new identity: a UUIDv7 key at `now`, and a random private key.
fn create(now: Stamp, entropy: &env::entropy::Entropy) -> Identity {
    let mut random = [0; 16];
    entropy.fill(&mut random);
    let mut private_key = [0; 32];
    entropy.fill(&mut private_key);
    // A clock before 1970 still gives a key; its time only orders keys.
    let time = now.max(Stamp::EPOCH);
    Identity {
        key: types::node::Key::v7(time, u128::from_le_bytes(random)),
        private_key: PrivateKey(private_key),
    }
}

/// The bytes of the file that holds `identity`.
pub(crate) fn encode(identity: &Identity) -> [u8; LEN] {
    let mut bytes = [0; LEN];
    bytes[..16].copy_from_slice(TAG);
    bytes[16..32].copy_from_slice(&identity.key.as_u128().to_be_bytes());
    bytes[32..BODY].copy_from_slice(&identity.private_key.0);
    sector::checksum(&mut bytes);
    bytes
}

/// The identity in `bytes`, or `None` for another tag or checksum.
#[cfg(any(test, feature = "sim"))]
fn decode(bytes: &[u8; LEN]) -> Option<Identity> {
    sector::written(bytes, TAG).map(|bytes| fields(&bytes))
}

/// The key and the private key in `bytes`, whatever its tag and checksum.
fn fields(bytes: &[u8; LEN]) -> Identity {
    Identity {
        key: types::node::Key::from_u128(u128::from_be_bytes(
            bytes[16..32].try_into().expect("invariant: 16 bytes"),
        )),
        private_key: PrivateKey(
            bytes[32..BODY].try_into().expect("invariant: 32 bytes"),
        ),
    }
}

/// Checks that `decode` gives an identity exactly for the bytes that `encode` writes,
/// and that the identity encodes to `bytes`.
///
/// # Panics
///
/// When a check fails.
#[cfg(any(test, feature = "sim"))]
pub(crate) fn check(bytes: &[u8; LEN]) {
    let valid = encode(&fields(bytes)) == *bytes;
    match decode(bytes) {
        Some(identity) => {
            assert!(valid, "decodes {bytes:02x?}");
            assert_eq!(encode(&identity), *bytes, "encodes {bytes:02x?}");
        }
        None => assert!(!valid, "refuses {bytes:02x?}"),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn identity(key: u128, private_key: [u8; 32]) -> Identity {
        Identity {
            key: types::node::Key::from_u128(key),
            private_key: PrivateKey(private_key),
        }
    }

    #[test]
    fn writes_the_tag_the_key_the_private_key_and_the_checksum() {
        let bytes = encode(&identity(0x0102 << 112 | 0x0f, [7; 32]));
        assert_eq!(&bytes[..16], b"foundation/key/1");
        assert_eq!(
            bytes[16..32],
            [1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0f]
        );
        assert_eq!(bytes[32..64], [7; 32]);
        assert_eq!(
            bytes[64..],
            crc32c::crc32c(&bytes[..64]).to_le_bytes(),
            "little-endian"
        );
    }

    #[test]
    fn refuses_zero_bytes() {
        assert!(decode(&[0; LEN]).is_none());
    }

    proptest! {
        #[test]
        fn decodes_what_it_encodes(
            key in any::<u128>(),
            private_key in any::<[u8; 32]>(),
        ) {
            let bytes = encode(&identity(key, private_key));
            let decoded = decode(&bytes).expect("decodes");
            prop_assert_eq!(decoded.key.as_u128(), key);
            prop_assert_eq!(decoded.private_key.0, private_key);
        }

        #[test]
        fn refuses_each_one_bit_change(
            key in any::<u128>(),
            private_key in any::<[u8; 32]>(),
            bit in 0..LEN * 8,
        ) {
            let mut bytes = encode(&identity(key, private_key));
            bytes[bit / 8] ^= 1 << (bit % 8);
            prop_assert!(decode(&bytes).is_none());
        }

        #[test]
        fn refuses_each_one_bit_change_of_the_tag_with_its_checksum(
            key in any::<u128>(),
            private_key in any::<[u8; 32]>(),
            bit in 0..TAG.len() * 8,
        ) {
            let mut bytes = encode(&identity(key, private_key));
            check(&bytes);
            bytes[bit / 8] ^= 1 << (bit % 8);
            sector::checksum(&mut bytes);
            prop_assert!(decode(&bytes).is_none());
        }

        #[test]
        fn checks_any_bytes(bytes in any::<[u8; LEN]>()) {
            check(&bytes);
        }

        #[test]
        fn checks_any_body_with_its_checksum(mut bytes in any::<[u8; LEN]>()) {
            sector::checksum(&mut bytes);
            check(&bytes);
        }
    }
}
