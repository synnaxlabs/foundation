//! The node's key and private key, in the file `node.key` of the data directory.
//!
//! The file is 68 bytes: the tag, the node key (big-endian), the Ed25519 private key,
//! and the CRC32C of those 64 bytes (little-endian). It fits one sector, which a crash
//! keeps whole or old, so a write never tears it. 68 zero bytes are a key that a crash
//! kept from being written.

use std::path::Path;

use env::files::{Files, Mode};
use types::ed25519::PrivateKey;
use types::time::Stamp;

use crate::Error;

/// The name of the file.
const FILE: &str = "node.key";
/// The first bytes of the file; a new form gets a new tag.
const TAG: &[u8; 16] = b"foundation/key/1";
/// The length of the file.
const LEN: usize = 68;
/// The pool that the file's two blocks come from. The shard's pool is not used, so
/// the load never fails for the shard's budget.
const POOL: block::Config = block::Config { budget: 4096 };

/// The node's key and the private key that its transport proves.
#[derive(Clone, Debug)]
pub(crate) struct Identity {
    pub(crate) key: types::node::Key,
    pub(crate) private_key: PrivateKey,
}

/// The identity in `node.key` of `files`. When the file is not there, or holds only
/// zero bytes, makes a new one at mesh time from `clock`, once it has mesh time, and
/// from `entropy`, and makes it durable before it returns. Never writes over a file
/// that holds a key.
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
    let file = files
        .open(Path::new(FILE), Mode::Create { len: LEN as u64 })
        .await
        .map_err(|error| match error {
            env::files::Error::Length { .. } => Error::Key,
            error => Error::Directory(error),
        })?;
    let pool = block::Pool::heap(POOL);
    let into = pool.alloc(LEN).expect("invariant: the pool holds a key");
    let read = file.read_at(0, into).await.map_err(Error::Directory)?;
    let bytes: &[u8; LEN] = (&*read).try_into().expect("invariant: a read fills it");
    if *bytes != [0; LEN] {
        return decode(bytes).ok_or(Error::Key);
    }
    clock.reach(Stamp::from_nanos(i64::MIN)).await;
    let identity = create(clock, entropy);
    let block = pool
        .copy(&encode(&identity))
        .expect("invariant: the pool holds a key");
    file.write_at(0, &[block]).await.map_err(Error::Directory)?;
    file.sync().await.map_err(Error::Directory)?;
    files
        .sync_dir(Path::new(""))
        .await
        .map_err(Error::Directory)?;
    Ok(identity)
}

/// A new identity: a UUIDv7 key at mesh time, and a random private key.
fn create(clock: &clock::Reader, entropy: &env::entropy::Entropy) -> Identity {
    let mut random = [0; 16];
    entropy.fill(&mut random);
    let mut private_key = [0; 32];
    entropy.fill(&mut private_key);
    let now = match clock.status() {
        clock::Status::Synced(now) | clock::Status::Holdover(now, _) => now.time(),
        clock::Status::Unsynced(_) => unreachable!("invariant: mesh time has come"),
    };
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
    bytes[32..64].copy_from_slice(&identity.private_key.0);
    let crc = crc32c::crc32c(&bytes[..64]);
    bytes[64..].copy_from_slice(&crc.to_le_bytes());
    bytes
}

/// The identity in `bytes`, or `None` for another tag or checksum.
fn decode(bytes: &[u8; LEN]) -> Option<Identity> {
    let (body, crc) = bytes.split_first_chunk::<64>()?;
    let (tag, rest) = body.split_first_chunk::<16>()?;
    let (key, private_key) = rest.split_first_chunk::<16>()?;
    if tag != TAG || crc32c::crc32c(body).to_le_bytes() != *crc {
        return None;
    }
    Some(Identity {
        key: types::node::Key::from_u128(u128::from_be_bytes(*key)),
        private_key: PrivateKey(private_key.try_into().ok()?),
    })
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
    }
}
