//! The connection IDs this node makes: [`LEN`] bytes. Each one this node issues
//! starts with the shard that owns the connection and ends with a tag from the node
//! key.

use std::time::Duration;

use aws_lc_rs::error::Unspecified;
use aws_lc_rs::{constant_time, hmac};
use env::entropy::Entropy;
use noq_proto::{ConnectionId, ConnectionIdGenerator, InvalidCid};

/// The bytes in every connection ID this node makes.
pub(super) const LEN: usize = 8;

/// The bytes of the tag that ends an issued ID.
const TAG: usize = 3;

/// The group of the destination ID of `datagram` when it has a short header: the
/// ID's first random byte. `None` for a long header.
pub(super) fn group(datagram: &[u8]) -> Option<u8> {
    match *datagram {
        [form, _shard, random, ..] if form & 0x80 == 0 => Some(random),
        _ => None,
    }
}

/// A connection ID of random bytes from `entropy`.
pub(super) fn random(entropy: &Entropy) -> ConnectionId {
    let mut id = [0; LEN];
    entropy.fill(&mut id);
    ConnectionId::new(&id)
}

/// Issues one shard's connection IDs: the shard, random bytes, and a tag over both.
#[derive(Clone)]
pub(super) struct Issuer {
    pub(super) shard: u8,
    /// Signs the tags. Derive it from the node key, so a restarted node knows the
    /// IDs it issued before and resets their connections.
    pub(super) key: hmac::Key,
    pub(super) entropy: Entropy,
}

impl Issuer {
    fn tag(&self, body: &[u8]) -> [u8; TAG] {
        let tag = hmac::sign(&self.key, body);
        let tag = tag.as_ref()[..TAG].try_into();
        tag.expect("invariant: an HMAC tag is longer than TAG")
    }
}

impl ConnectionIdGenerator for Issuer {
    fn generate_cid(&mut self) -> ConnectionId {
        let mut id = [self.shard; LEN];
        let (body, tag) = id.split_at_mut(LEN - TAG);
        self.entropy.fill(&mut body[1..]);
        tag.copy_from_slice(&self.tag(body));
        ConnectionId::new(&id)
    }

    /// Refuses an ID with the wrong tag, so the node sends a stateless reset only for
    /// an ID it issued. A reset carries a random ID, so no reset answers a reset.
    /// noq-proto gives it only IDs of [`LEN`] bytes.
    fn validate(&self, id: ConnectionId) -> Result<(), InvalidCid> {
        let (body, tag) = id.split_at(LEN - TAG);
        constant_time::verify_slices_are_equal(&self.tag(body), tag)
            .map_err(|Unspecified| InvalidCid)
    }

    fn cid_len(&self) -> usize {
        LEN
    }

    fn cid_lifetime(&self) -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use env::entropy::Driver;
    use proptest::prelude::*;

    use super::*;

    /// Gives the bytes of one value, over and over.
    struct Fixed(u8);

    impl Driver for Fixed {
        fn fill(&self, bytes: &mut [u8]) {
            bytes.fill(self.0);
        }
    }

    fn issuer(key: [u8; 32], value: u8) -> Issuer {
        Issuer {
            shard: 5,
            key: hmac::Key::new(hmac::HMAC_SHA256, &key),
            entropy: Entropy::new(Fixed(value)),
        }
    }

    proptest! {
        #[test]
        fn accepts_each_id_one_key_issued(value: u8, other: u8) {
            let id = issuer([1; 32], value).generate_cid();
            prop_assert!(issuer([1; 32], other).validate(id).is_ok());
        }

        #[test]
        fn refuses_an_id_with_one_bit_changed(value: u8, bit in 0..LEN * 8) {
            let mut id = issuer([1; 32], value).generate_cid().to_vec();
            id[bit / 8] ^= 1 << (bit % 8);
            let id = ConnectionId::new(&id);
            prop_assert!(issuer([1; 32], value).validate(id).is_err());
        }
    }

    #[test]
    fn groups_a_short_header_by_the_first_random_byte_of_its_id() {
        let id = issuer([1; 32], 9).generate_cid();
        let short = [[0x40].as_slice(), &id].concat();
        let long = [[0xc0].as_slice(), &id].concat();
        let groups =
            [short.as_slice(), &short[..3], &short[..2], &long, &[]].map(group);
        assert_eq!(groups, [Some(9), Some(9), None, None, None]);
    }

    #[test]
    fn refuses_an_id_another_key_issued() {
        let id = issuer([1; 32], 0).generate_cid();
        assert!(issuer([2; 32], 0).validate(id).is_err());
    }
}
