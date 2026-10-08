//! The hello of a subject.

use crate::connection;
use crate::ed25519::PublicKey;
use crate::name::Name;
use crate::node;
use crate::time::Stamp;

/// A subject's claim, sent first on each program connection and signed with one of
/// its keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    /// The subject that the program acts as.
    pub subject: Name,
    /// The key that signs the hello and each request of the connection.
    pub key: PublicKey,
    /// The node that the program connects to.
    pub via: node::Key,
    /// The connection that the hello and its requests name.
    pub connection: connection::Key,
    /// Bytes from the node that `via` names. Only that node checks them.
    pub nonce: [u8; 16],
    /// The mesh time at which the hello stops being valid.
    pub expires: Stamp,
}

impl Hello {
    /// The bytes of the encoded hello: 89 and the subject's bytes.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        self.subject.as_str().len() + 89
    }

    /// Writes the fields of the hello into `out`, each integer little-endian: the
    /// subject's length (1 byte) and bytes, the key (32), `via` as a `u128` (16), the
    /// connection (16), the nonce (16), and `expires` in nanoseconds (8).
    ///
    /// # Panics
    ///
    /// When `out` is not [`Hello::encoded_len`] bytes.
    pub fn encode(&self, out: &mut [u8]) {
        assert!(
            out.len() == self.encoded_len(),
            "out has {} bytes, and the hello has {}",
            out.len(),
            self.encoded_len()
        );
        let subject = self.subject.as_str().as_bytes();
        let len = u8::try_from(subject.len())
            .expect("invariant: a name is at most 255 bytes");
        let mut rest = out;
        for field in [
            &[len][..],
            subject,
            &self.key.to_bytes(),
            &self.via.as_u128().to_le_bytes(),
            &self.connection.0,
            &self.nonce,
            &self.expires.nanos().to_le_bytes(),
        ] {
            let (head, tail) = rest.split_at_mut(field.len());
            head.copy_from_slice(field);
            rest = tail;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_hello() -> Hello {
        Hello {
            subject: "ops.ana".parse().expect("a name"),
            key: PublicKey::new([9; 32]).expect("a valid key"),
            via: node::Key::from_u128(0x0f0e_0d0c_0b0a_0908_0706_0504_0302_0100),
            connection: connection::Key([0xc0; 16]),
            nonce: [0xa0; 16],
            expires: Stamp::from_nanos(0x0102_0304_0506_0708),
        }
    }

    #[test]
    fn writes_each_field_in_order() {
        let hello = create_hello();
        let mut out = vec![0; hello.encoded_len()];

        hello.encode(&mut out);

        let mut want = vec![7];
        want.extend_from_slice(b"ops.ana");
        want.extend_from_slice(&[9; 32]);
        want.extend_from_slice(&core::array::from_fn::<u8, 16, _>(|i| {
            u8::try_from(i).expect("fits")
        }));
        want.extend_from_slice(&[0xc0; 16]);
        want.extend_from_slice(&[0xa0; 16]);
        want.extend_from_slice(&[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(out.len(), 96);
        assert_eq!(out, want);
    }

    #[test]
    #[should_panic(expected = "out has 95 bytes, and the hello has 96")]
    fn panics_on_an_out_of_another_length() {
        create_hello().encode(&mut [0; 95]);
    }
}
