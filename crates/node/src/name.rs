//! The node's name, in the file `name` of the data directory.
//!
//! The file is 277 bytes: the tag, the length of the name in one byte, the name with
//! zero bytes after it up to [`Name::MAX_BYTES`], and the CRC32C of those 273 bytes
//! (little-endian), in one sector ([`crate::sector`]). A file with no bytes, or with
//! 277 zero bytes, is a name that a crash kept from being written.

use std::path::Path;

use env::files::Files;
use types::name::Name;

use crate::Error;
use crate::sector::{self, Held};

/// The name of the file.
pub(crate) const FILE: &str = "name";
/// The first bytes of the file; a new form gets a new tag.
const TAG: &[u8; 17] = b"foundation/name/1";
/// The length of the tag, the length of the name, and the name with its padding.
const BODY: usize = TAG.len() + 1 + Name::MAX_BYTES;
/// The length of the file: the body and its CRC32C.
pub(crate) const LEN: usize = BODY + 4;
const _: () = assert!(
    Name::MAX_BYTES <= u8::MAX as usize,
    "one byte holds the length"
);

/// The name in the file `name` of `files`, or `None` when no node wrote one. Opens
/// the file to read only, so it makes nothing and waits for no lock.
///
/// # Errors
///
/// [`Error::Name`] for a file that a node did not write, and [`Error::Directory`] for
/// a file call that fails.
pub(crate) async fn read(files: &Files) -> Result<Option<Name>, Error> {
    let held = sector::read(files, Path::new(FILE), TAG).await;
    decode(&held.map_err(Error::Directory)?)
}

/// Writes `name` to the file `name` of `files` and makes it durable, when the file
/// holds no name or holds `name`: a failed sync of an earlier start can leave a name
/// that a read sees but a crash loses. Call it under the lock of the data directory.
///
/// # Errors
///
/// [`Error::Renamed`] when the file holds another name, [`Error::Name`] for a file
/// that a node did not write, and [`Error::Directory`] for a file call that fails.
/// Writes no other name over a name.
pub(crate) async fn keep(files: &Files, name: &Name) -> Result<(), Error> {
    let opened = sector::open(files, Path::new(FILE), TAG).await;
    let (file, held) = opened.map_err(|error| match error {
        env::files::Error::Length { .. } => Error::Name,
        error => Error::Directory(error),
    })?;
    if let Some(stored) = decode(&held)?
        && stored != *name
    {
        return Err(Error::Renamed {
            stored,
            given: name.clone(),
        });
    }
    sector::write(files, &file, &encode(name))
        .await
        .map_err(Error::Directory)
}

/// The bytes of the file that holds `name`.
fn encode(name: &Name) -> [u8; LEN] {
    let name = name.as_str().as_bytes();
    let mut bytes = [0; LEN];
    bytes[..TAG.len()].copy_from_slice(TAG);
    bytes[TAG.len()] = u8::try_from(name.len()).expect("invariant: a name fits a u8");
    bytes[TAG.len() + 1..][..name.len()].copy_from_slice(name);
    sector::checksum(&mut bytes);
    bytes
}

/// The name that `held` holds, `None` for nothing, or [`Error::Name`] for bytes that
/// [`encode`] does not give.
fn decode(held: &Held<LEN>) -> Result<Option<Name>, Error> {
    let bytes = match held {
        Held::Nothing => return Ok(None),
        Held::Written(bytes) => bytes,
        Held::Foreign => return Err(Error::Name),
    };
    let len = usize::from(bytes[TAG.len()]);
    let name = &bytes[TAG.len() + 1..][..len];
    let name = std::str::from_utf8(name)
        .ok()
        .and_then(|name| name.parse::<Name>().ok())
        .ok_or(Error::Name)?;
    // Also refuses padding that is not zero, so each name has one form.
    if encode(&name) == *bytes {
        Ok(Some(name))
    } else {
        Err(Error::Name)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// The name in the file of `bytes`.
    fn read(bytes: &[u8; LEN]) -> Result<Option<Name>, Error> {
        decode(&sector::held(bytes, TAG))
    }

    #[test]
    fn writes_the_tag_the_length_the_name_and_the_checksum() {
        let bytes = encode(&"site_a.edge".parse().unwrap());
        assert_eq!(&bytes[..17], b"foundation/name/1");
        assert_eq!(bytes[17], 11);
        assert_eq!(&bytes[18..29], b"site_a.edge");
        assert_eq!(bytes[29..273], [0; 244]);
        assert_eq!(
            bytes[273..],
            crc32c::crc32c(&bytes[..273]).to_le_bytes(),
            "little-endian"
        );
    }

    #[test]
    fn a_name_of_the_most_bytes_fills_the_body() {
        let name: Name = "a".repeat(Name::MAX_BYTES).parse().unwrap();
        let bytes = encode(&name);
        assert_eq!(bytes[17], 255);
        assert_eq!(read(&bytes).unwrap(), Some(name));
    }

    #[test]
    fn zero_bytes_are_no_name() {
        assert_eq!(read(&[0; LEN]).unwrap(), None);
    }

    proptest! {
        /// Each byte that changes in the file of a name makes the file one that a
        /// node did not write.
        #[test]
        fn a_changed_byte_is_not_a_name(
            name in "[a-z][a-z0-9_]{0,20}(\\.[a-z0-9_-]{1,20}){0,3}",
            at in 0..LEN,
            flip in 1..=u8::MAX,
        ) {
            let mut bytes = encode(&name.parse().unwrap());
            prop_assert_eq!(read(&bytes).unwrap(), Some(name.parse().unwrap()));
            bytes[at] ^= flip;
            prop_assert!(matches!(read(&bytes), Err(Error::Name)));
        }

        /// Bytes that no encode gives are not a name.
        #[test]
        fn other_bytes_are_not_a_name(bytes in prop::array::uniform32(any::<u8>())) {
            let mut file = [0; LEN];
            file[..32].copy_from_slice(&bytes);
            prop_assume!(file != [0; LEN]);
            prop_assert!(matches!(read(&file), Err(Error::Name)));
        }
    }
}
