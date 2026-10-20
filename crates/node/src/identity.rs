//! The node's key, private key, and seal key, in the file `node.key` of the data
//! directory.
//!
//! The file is 100 bytes: the tag, the node key (big-endian), the Ed25519 private key,
//! the X25519 seal private key, and the CRC32C of those 96 bytes (little-endian), in
//! one sector ([`sector`]).

use std::path::Path;

use env::files::Files;
use secret::seal::Opener;
use types::ed25519::{PrivateKey, PublicKey};
use types::time::Stamp;

use crate::{Error, directory};
use sector::Held;

/// The name of the file.
pub(crate) const FILE: &str = "node.key";
/// The first bytes of the file; a new form gets a new tag.
const TAG: &[u8; 16] = b"foundation/key/2";
/// The length of the tag, the node key, the private key, and the seal key.
const BODY: usize = 96;
/// The length of the file: the body and its CRC32C.
pub(crate) const LEN: usize = BODY + 4;

/// The node's key, the private key that its transport proves, and the private key
/// that opens what callers seal to its card.
#[derive(Debug)]
pub(crate) struct Identity {
    pub(crate) key: types::node::Key,
    pub(crate) private_key: PrivateKey,
    pub(crate) opener: Opener,
}

/// The node key in `node.key` of `files`, or `None` when the file is not there. Opens
/// the file to read only, so it makes nothing and waits for no lock.
///
/// # Errors
///
/// [`Error::Key`] for a file that [`load`] did not write, and [`Error::Directory`]
/// for a file call that fails.
pub(crate) async fn key(files: &Files) -> Result<Option<types::node::Key>, Error> {
    Ok(read(files).await?.map(|bytes| node_key(&bytes)))
}

/// The public key of the private key in `node.key` of `files`, or `None` when the
/// file is not there. Opens the file as [`key`] does.
///
/// # Errors
///
/// As [`key`].
pub(crate) async fn public_key(files: &Files) -> Result<Option<PublicKey>, Error> {
    Ok(read(files)
        .await?
        .map(|bytes| fields(&bytes).private_key.public()))
}

/// [`Error::NoCard`] when the file is not there and the data directory has the
/// directory of a mesh: a first start makes the file durable before the mesh makes
/// that directory, so the key is lost. A file that a node did not write is there, and
/// its readers give [`Error::Key`]. Call it under the lock of the data directory,
/// before the start writes anything other than `lock` and `shards-<n>`.
///
/// # Errors
///
/// [`Error::NoCard`] as above, and [`Error::Directory`] for a file call that fails.
pub(crate) async fn claim(files: &Files) -> Result<(), Error> {
    let held = sector::read::<LEN>(files, Path::new(FILE), TAG).await;
    if matches!(held.map_err(Error::Directory)?, Held::Nothing)
        && directory::meshed(files).await?
    {
        return Err(Error::NoCard);
    }
    Ok(())
}

/// The identity in `node.key` of `files`. [`claim`] refuses a data directory with the
/// directory of a mesh and no file, so a new key is a key of a new node. When the file
/// is not there, makes a new one at mesh time from `clock`, once it has mesh time, and
/// from `entropy`, and publishes it. Call it under the lock of the data directory,
/// after the claim has made the directory durable, so that a key that `publish` made
/// is durable when it reads it.
///
/// # Errors
///
/// [`Error::Key`] for a file of another length, tag, or checksum, and
/// [`Error::Directory`] for a file call that fails.
pub(crate) async fn load(
    files: &Files,
    clock: &clock::Reader,
    entropy: &env::entropy::Entropy,
) -> Result<Identity, Error> {
    match read(files).await? {
        None => {
            clock.reach(Stamp::from_nanos(i64::MIN)).await;
            let now = match clock.status() {
                clock::Status::Synced(now) | clock::Status::Holdover(now, _) => {
                    now.time()
                }
                clock::Status::Unsynced(_) => {
                    unreachable!("invariant: mesh time has come")
                }
            };
            let identity = create(now, entropy);
            store(files, &identity).await?;
            Ok(identity)
        }
        Some(bytes) => Ok(fields(&bytes)),
    }
}

/// The bytes of `node.key` in `files`, or `None` when the file is not there.
///
/// # Errors
///
/// As [`load`].
async fn read(files: &Files) -> Result<Option<[u8; LEN]>, Error> {
    let held = sector::read(files, Path::new(FILE), TAG).await;
    match held.map_err(Error::Directory)? {
        Held::Nothing => Ok(None),
        Held::Written(bytes) => Ok(Some(bytes)),
        Held::Version { .. } | Held::Foreign => Err(Error::Key),
    }
}

/// Publishes `identity` as `node.key` in `files`, where no `node.key` is. Call it as
/// [`sector::publish`] says.
///
/// # Errors
///
/// [`Error::Directory`] with the error of the first file call that fails.
async fn store(files: &Files, identity: &Identity) -> Result<(), Error> {
    sector::publish(files, Path::new(FILE), &encode(identity))
        .await
        .map_err(Error::Directory)
}

/// Publishes `identity` as `node.key` in `files`, a data directory that no node runs
/// on, when no `node.key` is there.
///
/// # Errors
///
/// [`Error::Directory`] with [`env::files::Error::Exists`] when a file is there, before
/// it writes anything. [`Error::Directory`] with the error of each other file call that
/// fails.
#[cfg(feature = "sim")]
pub(crate) async fn write_new(files: &Files, identity: &Identity) -> Result<(), Error> {
    match sector::read::<LEN>(files, Path::new(FILE), TAG).await {
        Ok(Held::Nothing) => store(files, identity).await,
        Ok(Held::Written(_) | Held::Version { .. } | Held::Foreign) => {
            Err(Error::Directory(env::files::Error::Exists {
                path: FILE.into(),
            }))
        }
        Err(error) => Err(Error::Directory(error)),
    }
}

/// A new identity: a UUIDv7 key at `now`, and a random private key and seal key.
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
        opener: Opener::generate(entropy),
    }
}

/// The bytes of the file that holds `identity`.
pub(crate) fn encode(identity: &Identity) -> [u8; LEN] {
    let mut bytes = [0; LEN];
    bytes[..16].copy_from_slice(TAG);
    bytes[16..32].copy_from_slice(&identity.key.as_u128().to_be_bytes());
    bytes[32..64].copy_from_slice(&identity.private_key.0);
    bytes[64..BODY].copy_from_slice(identity.opener.expose());
    sector::checksum(&mut bytes);
    bytes
}

/// The node key in `bytes`, whatever its tag and checksum.
fn node_key(bytes: &[u8; LEN]) -> types::node::Key {
    let key = bytes[16..32].try_into().expect("invariant: 16 bytes");
    types::node::Key::from_u128(u128::from_be_bytes(key))
}

/// The keys in `bytes`, whatever its tag and checksum.
fn fields(bytes: &[u8; LEN]) -> Identity {
    Identity {
        key: node_key(bytes),
        private_key: PrivateKey(bytes[32..64].try_into().expect("invariant: 32 bytes")),
        opener: Opener::from_bytes(
            bytes[64..BODY].try_into().expect("invariant: 32 bytes"),
        ),
    }
}

/// Checks that `sector::written` takes `bytes` exactly when `encode` of their fields
/// gives them back.
///
/// # Panics
///
/// When the check fails.
#[cfg(any(test, feature = "sim"))]
pub(crate) fn check(bytes: &[u8; LEN]) {
    sector::check(bytes, TAG, |bytes| Some(encode(&fields(bytes))));
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn identity(key: u128, private_key: [u8; 32], opener: [u8; 32]) -> Identity {
        Identity {
            key: types::node::Key::from_u128(key),
            private_key: PrivateKey(private_key),
            opener: Opener::from_bytes(opener),
        }
    }

    #[test]
    fn writes_the_tag_the_keys_and_the_checksum() {
        let bytes = encode(&identity(0x0102 << 112 | 0x0f, [7; 32], [5; 32]));
        assert_eq!(&bytes[..16], b"foundation/key/2");
        assert_eq!(
            bytes[16..32],
            [1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0f]
        );
        assert_eq!(bytes[32..64], [7; 32]);
        assert_eq!(bytes[64..96], [5; 32]);
        assert_eq!(
            bytes[96..],
            crc32c::crc32c(&bytes[..96]).to_le_bytes(),
            "little-endian"
        );
    }

    #[test]
    fn refuses_zero_bytes() {
        assert!(!sector::written(&[0; LEN], TAG));
    }

    proptest! {
        #[test]
        fn decodes_what_it_encodes(
            key in any::<u128>(),
            private_key in any::<[u8; 32]>(),
            opener in any::<[u8; 32]>(),
        ) {
            let bytes = encode(&identity(key, private_key, opener));
            prop_assert!(sector::written(&bytes, TAG));
            let decoded = fields(&bytes);
            prop_assert_eq!(decoded.key.as_u128(), key);
            prop_assert_eq!(decoded.private_key.0, private_key);
            prop_assert_eq!(decoded.opener.expose(), &opener);
        }

        #[test]
        fn refuses_each_one_bit_change(
            key in any::<u128>(),
            private_key in any::<[u8; 32]>(),
            opener in any::<[u8; 32]>(),
            bit in 0..LEN * 8,
        ) {
            let mut bytes = encode(&identity(key, private_key, opener));
            bytes[bit / 8] ^= 1 << (bit % 8);
            prop_assert!(!sector::written(&bytes, TAG));
        }

        #[test]
        fn refuses_each_one_bit_change_of_the_tag_with_its_checksum(
            key in any::<u128>(),
            private_key in any::<[u8; 32]>(),
            opener in any::<[u8; 32]>(),
            bit in 0..TAG.len() * 8,
        ) {
            let mut bytes = encode(&identity(key, private_key, opener));
            check(&bytes);
            bytes[bit / 8] ^= 1 << (bit % 8);
            sector::checksum(&mut bytes);
            prop_assert!(!sector::written(&bytes, TAG));
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
