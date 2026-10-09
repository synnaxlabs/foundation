//! The node's name, in the file `name` of the data directory.
//!
//! The file is 277 bytes: the tag, the length of the name in one byte, the name with
//! zero bytes after it up to [`Name::MAX_BYTES`], and the CRC32C of those 273 bytes
//! (little-endian), in one sector ([`crate::sector`]). A file with no bytes, or with
//! 277 zero bytes, is a name that a crash kept from being written.

use std::path::Path;

use env::files::{Files, Mode};
use types::name::Name;

use crate::{Error, sector};

/// The name of the file.
pub(crate) const FILE: &str = "name";
/// The first bytes of the file; a new form gets a new tag.
const TAG: &[u8; 17] = b"foundation/name/1";
/// The length of the tag, the length of the name, and the name with its padding.
const BODY: usize = TAG.len() + 1 + Name::MAX_BYTES;
/// The length of the file: the body and its CRC32C.
pub(crate) const LEN: usize = BODY + 4;

/// The name in the file `name` of `files`, or `None` when no node wrote one. Opens
/// the file to read only, so it makes nothing and waits for no lock.
///
/// # Errors
///
/// [`Error::Name`] for a file that a node did not write, and [`Error::Directory`] for
/// a file call that fails.
pub(crate) async fn read(files: &Files) -> Result<Option<Name>, Error> {
    let file = match files.open(Path::new(FILE), Mode::Read).await {
        Ok(file) => file,
        Err(env::files::Error::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(Error::Directory(error)),
    };
    match file.len() {
        0 => Ok(None),
        len if len == LEN as u64 => {
            decode(&sector::read(&file).await.map_err(Error::Directory)?)
        }
        _ => Err(Error::Name),
    }
}

/// Writes `name` to the file `name` of `files` and makes it durable, when the file
/// holds no name or holds `name`: a failed sync of an earlier start can leave a name
/// that a read sees but a crash loses. Call it under the lock of the data directory.
///
/// # Errors
///
/// [`Error::Renamed`] when the file holds another name, [`Error::Name`] for a file
/// that a node did not write, and [`Error::Directory`] for a file call that fails.
/// Writes nothing over a name.
pub(crate) async fn keep(files: &Files, name: &Name) -> Result<(), Error> {
    let opened = sector::open(files, Path::new(FILE)).await;
    let (file, bytes) = opened.map_err(|error| match error {
        env::files::Error::Length { .. } => Error::Name,
        error => Error::Directory(error),
    })?;
    if let Some(stored) = decode(&bytes)?
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
    sector::seal(&mut bytes);
    bytes
}

/// The name in `bytes`, `None` for zero bytes, or [`Error::Name`] for bytes that
/// [`encode`] does not give.
fn decode(bytes: &[u8; LEN]) -> Result<Option<Name>, Error> {
    if *bytes == [0; LEN] {
        return Ok(None);
    }
    let len = usize::from(bytes[TAG.len()]);
    let name = &bytes[TAG.len() + 1..][..len.min(Name::MAX_BYTES)];
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
        assert_eq!(decode(&bytes).unwrap(), Some(name));
    }

    #[test]
    fn zero_bytes_are_no_name() {
        assert_eq!(decode(&[0; LEN]).unwrap(), None);
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
            prop_assert_eq!(decode(&bytes).unwrap(), Some(name.parse().unwrap()));
            bytes[at] ^= flip;
            prop_assert!(matches!(decode(&bytes), Err(Error::Name)));
        }

        /// Bytes that no encode gives are not a name.
        #[test]
        fn other_bytes_are_not_a_name(bytes in prop::array::uniform32(any::<u8>())) {
            let mut file = [0; LEN];
            file[..32].copy_from_slice(&bytes);
            prop_assume!(file != [0; LEN]);
            prop_assert!(matches!(decode(&file), Err(Error::Name)));
        }
    }
}
