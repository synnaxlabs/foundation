use std::collections::BTreeMap;

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use proptest::prelude::*;
use spec::definition::{Definition, Kind};
use spec::subject::Subject;
use types::time::Interval;

use super::*;
use crate::Rules;

/// The private key of RFC 8032, section 7.1, test 1.
const TEST_1: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
/// The private key of RFC 8032, section 7.1, test 2.
const TEST_2: &str = "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb";

const PEER: node::Key = node::Key::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
const EXPIRES: Stamp = Stamp::from_nanos(1_791_446_400_000_000_000);

fn hex<const N: usize>(hex: &str) -> [u8; N] {
    let mut bytes = [0; N];
    assert_eq!(hex.len(), 2 * N, "{hex}");
    for (byte, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
    }
    bytes
}

fn pair(private: &str) -> Ed25519KeyPair {
    Ed25519KeyPair::from_seed_unchecked(&hex::<32>(private)).unwrap()
}

fn public(pair: &Ed25519KeyPair) -> PublicKey {
    PublicKey::new(pair.public_key().as_ref().try_into().unwrap()).unwrap()
}

fn sign(pair: &Ed25519KeyPair, message: &[u8]) -> [u8; 64] {
    pair.sign(message).as_ref().try_into().unwrap()
}

fn name(s: &str) -> Name {
    s.parse().unwrap()
}

/// The rules of a root tree that holds each subject of `subjects` with its keys.
fn rules(subjects: &[(&str, &[PublicKey])]) -> Rules {
    let tree: BTreeMap<Name, Definition> = subjects
        .iter()
        .map(|(subject, keys)| {
            let key = Kind::Subject.key(subject).unwrap();
            (
                key,
                Definition::Subject(Subject::new(keys.to_vec()).unwrap()),
            )
        })
        .collect();
    Rules::new([(types::name::Prefix::ROOT, &tree)])
}

/// The hello of the golden test: subject `ops.ana`, signed with the key of test 1.
fn create_hello() -> Hello {
    Hello {
        subject: name("ops.ana"),
        key: public(&pair(TEST_1)),
        via: PEER,
        connection: connection::Key(hex("c0c1c2c3c4c5c6c7c8c9cacbcccdcecf")),
        nonce: hex("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf"),
        expires: EXPIRES,
    }
}

/// A mesh time one second wide that ends at `latest`.
fn at(latest: Stamp) -> Interval {
    Interval {
        earliest: latest - Span::SECOND,
        latest,
    }
}

/// A mesh time one second wide that ends a minute before [`EXPIRES`].
const NOW: Option<Interval> = Some(Interval {
    earliest: Stamp::from_nanos(1_791_446_339_000_000_000),
    latest: Stamp::from_nanos(1_791_446_340_000_000_000),
});

/// Signs `hello` with the key of test 1 and admits it at `now` from `PEER`.
fn admit(
    rules: &Rules,
    now: Option<Interval>,
    hello: Hello,
) -> Result<Admitted, Error> {
    let signature = sign(&pair(TEST_1), &super::hello(&hello));
    rules.admit(now, PEER, hello, &signature)
}

fn listed() -> Rules {
    rules(&[("ops.ana", &[public(&pair(TEST_1))])])
}

fn admitted() -> Admitted {
    admit(&listed(), NOW, create_hello()).unwrap()
}

/// Signs a request of `body` on the connection of the golden hello.
fn signed(body: &[u8]) -> [u8; 64] {
    sign(&pair(TEST_1), &request(create_hello().connection, body))
}

mod golden {
    use super::*;

    #[test]
    fn gives_the_exact_bytes_of_a_hello_and_its_signature_verifies() {
        let bytes = hello(&create_hello());

        assert_eq!(
            bytes,
            hex::<114>(concat!(
                "666f756e646174696f6e2f68656c6c6f2f31",
                "07",
                "6f70732e616e61",
                "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
                "efcdab8967452301efcdab8967452301",
                "c0c1c2c3c4c5c6c7c8c9cacbcccdcecf",
                "a0a1a2a3a4a5a6a7a8a9aaabacadaeaf",
                "00002762027fdc18",
            ))
        );
        let signature = hex(concat!(
            "f7752b95cf02e41d6d79d4474e84cb53ed6a02c2a9725e44888fd853f6f61f78",
            "bc2011c1feb9557346878c032f92f51ed0191f722d86c469839d1d701b0a7f03",
        ));
        assert_eq!(create_hello().key.verify(&bytes, &signature), Ok(()));
    }

    #[test]
    fn gives_the_exact_bytes_of_a_request_and_its_signature_verifies() {
        let bytes = request(create_hello().connection, b"open site_a.pt_1");

        assert_eq!(
            bytes,
            hex::<52>(concat!(
                "666f756e646174696f6e2f726571756573742f31",
                "c0c1c2c3c4c5c6c7c8c9cacbcccdcecf",
                "6f70656e20736974655f612e70745f31",
            ))
        );
        let signature = hex(concat!(
            "60bfa85a16ec42321352a9ab26ad64056eee4bb51f63e07e0e43ab310cfa4634",
            "1bc146b32998921f446113a0d563e5ccd5232654cc8ba4da49d4c499e062ed03",
        ));
        assert_eq!(create_hello().key.verify(&bytes, &signature), Ok(()));
    }

    #[test]
    fn holds_a_subject_of_the_most_bytes_a_name_has() {
        let subject = "a".repeat(Name::MAX_BYTES);
        let hello = Hello {
            subject: name(&subject),
            ..create_hello()
        };

        let bytes = super::hello(&hello);

        assert_eq!(bytes[18], 255);
        assert_eq!(&bytes[19..274], subject.as_bytes());
        assert_eq!(bytes.len(), 18 + 1 + 255 + 88);
    }

    /// A hello whose field `field` differs from the golden one.
    fn changed(field: usize, bit: u8) -> Hello {
        let mut hello = create_hello();
        let flip = 1 << (bit % 8);
        match field {
            0 => hello.subject = name("ops.anb"),
            1 => hello.key = public(&pair(TEST_2)),
            2 => hello.via = node::Key::from_u128(PEER.as_u128() ^ u128::from(flip)),
            3 => hello.connection.0[usize::from(bit % 16)] ^= flip,
            4 => hello.nonce[usize::from(bit % 16)] ^= flip,
            _ => hello.expires = hello.expires + Span::from_nanos(i64::from(flip)),
        }
        hello
    }

    proptest! {
        #[test]
        fn gives_other_bytes_for_a_hello_with_one_field_changed(
            field in 0..6_usize,
            bit: u8,
        ) {
            prop_assert_ne!(hello(&changed(field, bit)), hello(&create_hello()));
        }

        #[test]
        fn never_gives_the_bytes_of_a_request_for_a_hello(
            body in proptest::collection::vec(any::<u8>(), 0..128),
        ) {
            let hello = create_hello();

            prop_assert_ne!(request(hello.connection, &body), super::hello(&hello));
        }
    }
}

mod admit {
    use super::*;

    #[test]
    fn admits_a_hello_signed_with_a_listed_key() {
        let admitted = admit(&listed(), NOW, create_hello()).unwrap();

        assert_eq!(admitted.hello(), &create_hello());
    }

    #[test]
    fn admits_a_hello_signed_with_any_listed_key() {
        let rules =
            rules(&[("ops.ana", &[public(&pair(TEST_2)), public(&pair(TEST_1))])]);

        let admitted = admit(&rules, NOW, create_hello()).unwrap();

        assert_eq!(admitted.hello(), &create_hello());
    }

    #[test]
    fn refuses_a_hello_at_a_node_with_no_mesh_time() {
        let error = admit(&listed(), None, create_hello()).unwrap_err();

        assert_eq!(error, Error::Unsynced);
    }

    #[test]
    fn refuses_a_subject_that_the_spec_does_not_have() {
        let rules = rules(&[("ops.bob", &[public(&pair(TEST_1))])]);

        let error = admit(&rules, NOW, create_hello()).unwrap_err();

        assert_eq!(
            error,
            Error::Unknown {
                subject: name("ops.ana")
            }
        );
    }

    #[test]
    fn refuses_a_key_that_the_spec_does_not_list_for_the_subject() {
        let rules = rules(&[("ops.ana", &[public(&pair(TEST_2))])]);

        let error = admit(&rules, NOW, create_hello()).unwrap_err();

        let key = public(&pair(TEST_1));
        let subject = name("ops.ana");
        assert_eq!(error, Error::Unlisted { subject, key });
    }

    #[test]
    fn refuses_a_hello_signed_with_another_key() {
        let hello = create_hello();
        let signature = sign(&pair(TEST_2), &super::hello(&hello));

        let error = listed().admit(NOW, PEER, hello, &signature).unwrap_err();

        assert_eq!(error, Error::Signature);
    }

    #[test]
    fn refuses_a_hello_signed_with_another_listed_key() {
        let both =
            rules(&[("ops.ana", &[public(&pair(TEST_1)), public(&pair(TEST_2))])]);
        let hello = create_hello();
        let signature = sign(&pair(TEST_2), &super::hello(&hello));

        let error = both.admit(NOW, PEER, hello, &signature).unwrap_err();

        assert_eq!(error, Error::Signature);
    }

    #[test]
    fn refuses_a_hello_changed_after_its_signature() {
        let signature = sign(&pair(TEST_1), &super::hello(&create_hello()));
        let mut hello = create_hello();
        hello.nonce[0] ^= 1;

        let error = listed().admit(NOW, PEER, hello, &signature).unwrap_err();

        assert_eq!(error, Error::Signature);
    }

    #[test]
    fn refuses_a_hello_that_names_another_node() {
        let via = node::Key::from_u128(7);
        let hello = Hello {
            via,
            ..create_hello()
        };

        let error = admit(&listed(), NOW, hello).unwrap_err();

        assert_eq!(error, Error::Via { via, peer: PEER });
    }

    #[test]
    fn refuses_a_hello_once_the_latest_mesh_time_reaches_its_expiry() {
        let before = Some(at(EXPIRES - Span::NANOSECOND));
        admit(&listed(), before, create_hello()).unwrap();

        let error = admit(&listed(), Some(at(EXPIRES)), create_hello()).unwrap_err();

        let now = EXPIRES;
        assert_eq!(
            error,
            Error::Expired {
                expires: EXPIRES,
                now
            }
        );
    }

    #[test]
    fn refuses_a_hello_that_expires_past_the_cap() {
        let earliest = |cap: Stamp| {
            Some(Interval {
                earliest: cap - CAP,
                latest: cap - CAP + Span::SECOND,
            })
        };
        admit(&listed(), earliest(EXPIRES), create_hello()).unwrap();

        let cap = EXPIRES - Span::NANOSECOND;
        let error = admit(&listed(), earliest(cap), create_hello()).unwrap_err();

        assert_eq!(
            error,
            Error::Capped {
                expires: EXPIRES,
                cap
            }
        );
    }

    /// A hello that fails each check, with one failure removed at each step, gives
    /// the refusal of the first check that still fails.
    #[test]
    fn refuses_a_hello_for_the_first_check_that_fails() {
        let other = node::Key::from_u128(7);
        let wide = Some(Interval {
            earliest: EXPIRES - CAP - CAP,
            latest: EXPIRES,
        });
        let hello = Hello {
            via: other,
            ..create_hello()
        };
        let unsigned = [0; 64];
        let signature = sign(&pair(TEST_1), &super::hello(&hello));
        let unlisted = rules(&[("ops.ana", &[public(&pair(TEST_2))])]);
        let refuse = |rules: &Rules, now, hello: &Hello, signature| {
            rules
                .admit(now, PEER, hello.clone(), signature)
                .unwrap_err()
        };

        let subject = name("ops.ana");
        let key = public(&pair(TEST_1));
        assert_eq!(
            refuse(&rules(&[]), None, &hello, &unsigned),
            Error::Unsynced
        );
        assert_eq!(
            refuse(&rules(&[]), wide, &hello, &unsigned),
            Error::Unknown {
                subject: subject.clone()
            }
        );
        assert_eq!(
            refuse(&unlisted, wide, &hello, &unsigned),
            Error::Unlisted { subject, key }
        );
        assert_eq!(refuse(&listed(), wide, &hello, &unsigned), Error::Signature);
        assert_eq!(
            refuse(&listed(), wide, &hello, &signature),
            Error::Via {
                via: other,
                peer: PEER
            }
        );
        let hello = create_hello();
        let signature = sign(&pair(TEST_1), &super::hello(&hello));
        assert_eq!(
            refuse(&listed(), wide, &hello, &signature),
            Error::Expired {
                expires: EXPIRES,
                now: EXPIRES
            }
        );
        let wide = wide.map(|now| Interval {
            latest: now.latest - Span::NANOSECOND,
            ..now
        });
        let cap = EXPIRES - CAP;
        assert_eq!(
            refuse(&listed(), wide, &hello, &signature),
            Error::Capped {
                expires: EXPIRES,
                cap
            }
        );
    }

    #[test]
    fn keeps_each_subject_of_each_region_tree_by_its_tree_key() {
        let key = public(&pair(TEST_1));
        let subject = Definition::Subject(Subject::new(vec![key]).unwrap());
        let region: BTreeMap<Name, Definition> =
            [(Kind::Subject.key("site_a.ana").unwrap(), subject)].into();
        let rules = Rules::new([
            (types::name::Prefix::ROOT, &BTreeMap::new()),
            ("site_a".parse().unwrap(), &region),
        ]);
        let hello = Hello {
            subject: name("site_a.ana"),
            ..create_hello()
        };

        let admitted = admit(&rules, NOW, hello.clone()).unwrap();

        assert_eq!(admitted.hello(), &hello);
    }

    #[test]
    fn admits_a_hello_when_the_cap_is_past_the_last_stamp() {
        let latest = Stamp::from_nanos(i64::MAX - 1);
        let now = Some(Interval {
            earliest: latest - Span::SECOND,
            latest,
        });
        let hello = Hello {
            expires: Stamp::from_nanos(i64::MAX),
            ..create_hello()
        };

        let admitted = admit(&listed(), now, hello.clone()).unwrap();

        assert_eq!(admitted.hello(), &hello);
    }

    #[test]
    fn refuses_a_subject_whose_definition_is_not_at_its_subject_key() {
        let key = public(&pair(TEST_1));
        let subject = Definition::Subject(Subject::new(vec![key]).unwrap());
        let tree: BTreeMap<Name, Definition> = [(name("ops.ana"), subject)].into();
        let rules = Rules::new([(types::name::Prefix::ROOT, &tree)]);

        let error = admit(&rules, NOW, create_hello()).unwrap_err();

        let subject = name("ops.ana");
        assert_eq!(error, Error::Unknown { subject });
    }

    #[test]
    fn refuses_a_subject_that_makes_no_subject_key() {
        let reserved = name("ops.@subject");
        let long = name(&"a".repeat(Name::MAX_BYTES));
        for subject in [reserved, long] {
            let hello = Hello {
                subject: subject.clone(),
                ..create_hello()
            };

            let error = admit(&listed(), NOW, hello).unwrap_err();

            assert_eq!(error, Error::Unknown { subject });
        }
    }
}

mod renew {
    use super::*;

    /// The golden hello with a new nonce and an expiry a minute later.
    fn renewal() -> Hello {
        Hello {
            nonce: [0xb5; 16],
            expires: EXPIRES + Span::MINUTE,
            ..create_hello()
        }
    }

    /// Signs `hello` with `private`, renews [`admitted`] with it at `now`, and gives
    /// the hello that the result holds.
    fn renew(
        rules: &Rules,
        now: Option<Interval>,
        private: &str,
        hello: Hello,
    ) -> Result<Hello, Error> {
        let signature = sign(&pair(private), &super::hello(&hello));
        rules
            .renew(&admitted(), now, hello, &signature)
            .map(|renewed| renewed.hello().clone())
    }

    #[test]
    fn gives_the_renewal_in_place_of_the_hello() {
        assert_eq!(renew(&listed(), NOW, TEST_1, renewal()), Ok(renewal()));
    }

    #[test]
    fn refuses_a_renewal_that_changes_the_subject_key_via_or_connection() {
        let rules = rules(&[
            ("ops.ana", &[public(&pair(TEST_1)), public(&pair(TEST_2))]),
            ("ops.bob", &[public(&pair(TEST_1))]),
        ]);
        let subject = Hello {
            subject: name("ops.bob"),
            ..renewal()
        };
        let connection = Hello {
            connection: connection::Key([0xc5; 16]),
            ..renewal()
        };
        let key = Hello {
            key: public(&pair(TEST_2)),
            ..renewal()
        };
        let via = Hello {
            via: node::Key::from_u128(7),
            ..renewal()
        };

        let changed = |field| Err(Error::Changed { field });
        assert_eq!(renew(&rules, NOW, TEST_1, subject), changed(Field::Subject));
        assert_eq!(renew(&rules, NOW, TEST_2, key), changed(Field::Key));
        assert_eq!(renew(&rules, NOW, TEST_1, via), changed(Field::Via));
        assert_eq!(
            renew(&rules, NOW, TEST_1, connection),
            changed(Field::Connection)
        );
    }

    /// A renewal that changes more than one field names the first in the order of
    /// `Field`.
    #[test]
    fn names_the_first_field_that_changed() {
        let rules =
            rules(&[("ops.ana", &[public(&pair(TEST_1)), public(&pair(TEST_2))])]);
        let subject = Hello {
            subject: name("ops.bob"),
            key: public(&pair(TEST_2)),
            ..renewal()
        };
        let key = Hello {
            key: public(&pair(TEST_2)),
            via: node::Key::from_u128(7),
            connection: connection::Key([0xc5; 16]),
            ..renewal()
        };
        let via = Hello {
            via: node::Key::from_u128(7),
            connection: connection::Key([0xc5; 16]),
            ..renewal()
        };

        let changed = |field| Err(Error::Changed { field });
        assert_eq!(renew(&rules, NOW, TEST_2, subject), changed(Field::Subject));
        assert_eq!(renew(&rules, NOW, TEST_2, key), changed(Field::Key));
        assert_eq!(renew(&rules, NOW, TEST_1, via), changed(Field::Via));
    }

    #[test]
    fn checks_the_change_before_the_mesh_time() {
        let changed = Hello {
            subject: name("ops.bob"),
            ..renewal()
        };

        assert_eq!(
            renew(&listed(), None, TEST_1, changed),
            Err(Error::Changed {
                field: Field::Subject
            })
        );
    }

    #[test]
    fn checks_a_renewal_as_a_first_hello() {
        let late = Some(at(EXPIRES + Span::MINUTE));
        let capped = Some(Interval {
            earliest: EXPIRES - CAP,
            latest: EXPIRES - CAP + Span::SECOND,
        });

        assert_eq!(
            renew(&listed(), None, TEST_1, renewal()),
            Err(Error::Unsynced)
        );
        assert_eq!(
            renew(&listed(), NOW, TEST_2, renewal()),
            Err(Error::Signature)
        );
        assert_eq!(
            renew(&rules(&[]), NOW, TEST_1, renewal()),
            Err(Error::Unknown {
                subject: name("ops.ana")
            })
        );
        assert_eq!(
            renew(&listed(), late, TEST_1, renewal()),
            Err(Error::Expired {
                expires: EXPIRES + Span::MINUTE,
                now: EXPIRES + Span::MINUTE,
            })
        );
        assert_eq!(
            renew(&listed(), capped, TEST_1, renewal()),
            Err(Error::Capped {
                expires: EXPIRES + Span::MINUTE,
                cap: EXPIRES,
            })
        );
    }

    /// A spec that no longer lists the key refuses the renewal of a hello that it
    /// admitted.
    #[test]
    fn refuses_a_renewal_once_the_spec_drops_the_key() {
        let moved = rules(&[("ops.ana", &[public(&pair(TEST_2))])]);

        assert_eq!(
            renew(&moved, NOW, TEST_1, renewal()),
            Err(Error::Unlisted {
                subject: name("ops.ana"),
                key: public(&pair(TEST_1)),
            })
        );
    }
}

mod verify {
    use super::*;

    #[test]
    fn verifies_a_request_signed_with_the_key_of_the_hello() {
        let body = b"open site_a.pt_1";

        let verified = listed().verify(&admitted(), NOW, body, &signed(body));

        assert_eq!(verified, Ok(()));
    }

    #[test]
    fn refuses_a_request_whose_body_changed_after_its_signature() {
        let signature = signed(b"open site_a.pt_1");

        let verified =
            listed().verify(&admitted(), NOW, b"open site_a.pt_2", &signature);

        assert_eq!(verified, Err(Error::Signature));
    }

    #[test]
    fn refuses_a_request_signed_for_another_connection() {
        let body = b"open site_a.pt_1";
        let other = connection::Key([0; 16]);
        let signature = sign(&pair(TEST_1), &request(other, body));

        let verified = listed().verify(&admitted(), NOW, body, &signature);

        assert_eq!(verified, Err(Error::Signature));
    }

    #[test]
    fn refuses_a_request_signed_with_another_listed_key() {
        let body = b"open site_a.pt_1";
        let both =
            rules(&[("ops.ana", &[public(&pair(TEST_1)), public(&pair(TEST_2))])]);
        let admitted = admit(&both, NOW, create_hello()).unwrap();
        let signature = sign(&pair(TEST_2), &request(create_hello().connection, body));

        let verified = both.verify(&admitted, NOW, body, &signature);

        assert_eq!(verified, Err(Error::Signature));
    }

    #[test]
    fn refuses_the_signature_of_the_hello_as_a_request() {
        let bytes = hello(&create_hello());
        let signature = sign(&pair(TEST_1), &bytes);

        let verified = listed().verify(&admitted(), NOW, &bytes, &signature);

        assert_eq!(verified, Err(Error::Signature));
    }

    #[test]
    fn refuses_a_request_once_the_latest_mesh_time_reaches_the_expiry() {
        let body = b"open site_a.pt_1";
        let before = Some(at(EXPIRES - Span::NANOSECOND));
        assert_eq!(
            listed().verify(&admitted(), before, body, &signed(body)),
            Ok(())
        );

        let late = Some(at(EXPIRES));
        let verified = listed().verify(&admitted(), late, body, &signed(body));

        let now = EXPIRES;
        assert_eq!(
            verified,
            Err(Error::Expired {
                expires: EXPIRES,
                now
            })
        );
    }

    #[test]
    fn refuses_a_request_at_a_node_with_no_mesh_time() {
        let body = b"open site_a.pt_1";

        let verified = listed().verify(&admitted(), None, body, &signed(body));

        assert_eq!(verified, Err(Error::Unsynced));
    }

    #[test]
    fn refuses_a_request_once_the_spec_no_longer_lists_the_key() {
        let body = b"open site_a.pt_1";
        let rules = rules(&[("ops.ana", &[public(&pair(TEST_2))])]);

        let verified = rules.verify(&admitted(), NOW, body, &signed(body));

        let key = public(&pair(TEST_1));
        let subject = name("ops.ana");
        assert_eq!(verified, Err(Error::Unlisted { subject, key }));
    }

    #[test]
    fn refuses_a_request_once_the_spec_no_longer_has_the_subject() {
        let body = b"open site_a.pt_1";

        let verified = rules(&[]).verify(&admitted(), NOW, body, &signed(body));

        let subject = name("ops.ana");
        assert_eq!(verified, Err(Error::Unknown { subject }));
    }

    #[test]
    fn refuses_a_request_for_the_first_check_that_fails() {
        let body = b"open site_a.pt_1";
        let unsigned = [0; 64];
        let unlisted = rules(&[("ops.ana", &[public(&pair(TEST_2))])]);
        let late = Some(at(EXPIRES));
        let refuse = |rules: &Rules, now, signature| {
            rules.verify(&admitted(), now, body, signature).unwrap_err()
        };

        let subject = name("ops.ana");
        let key = public(&pair(TEST_1));
        assert_eq!(refuse(&rules(&[]), None, &unsigned), Error::Unsynced);
        assert_eq!(
            refuse(&rules(&[]), late, &unsigned),
            Error::Unknown {
                subject: subject.clone()
            }
        );
        assert_eq!(
            refuse(&unlisted, late, &unsigned),
            Error::Unlisted { subject, key }
        );
        assert_eq!(refuse(&listed(), late, &unsigned), Error::Signature);
        assert_eq!(
            refuse(&listed(), late, &signed(body)),
            Error::Expired {
                expires: EXPIRES,
                now: EXPIRES
            }
        );
    }
}

#[test]
fn names_each_refusal_and_its_fix() {
    let key = public(&pair(TEST_1));
    let subject = name("ops.ana");
    let peer = node::Key::from_u128(7);
    let cases = [
        (
            Error::Unsynced,
            "the node has no mesh time yet",
            "Connect again when the node has synced its clock",
        ),
        (
            Error::Unknown {
                subject: subject.clone(),
            },
            "the spec has no subject ops.ana",
            "Define the subject with the program's public key in the spec",
        ),
        (
            Error::Unlisted { subject, key },
            "the spec does not list key \
             d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a \
             for subject ops.ana",
            "Add the key to the subject in the spec, or sign with a listed key",
        ),
        (
            Error::Signature,
            "the signature is not of the message by the key",
            "Sign the exact bytes with the key of the hello",
        ),
        (
            Error::Via { via: PEER, peer },
            "the hello names node 01234567-89ab-cdef-0123-456789abcdef, but node \
             00000000-0000-0000-0000-000000000007 carried it",
            "Name the node that the program connects to as `via`",
        ),
        (
            Error::Expired {
                expires: Stamp::from_nanos(2_000_000_000),
                now: Stamp::from_nanos(3_000_000_000),
            },
            "the hello expired at 1970-01-01T00:00:02.000000000Z, at or before the \
             mesh time 1970-01-01T00:00:03.000000000Z",
            "Send a new hello with a later expiry",
        ),
        (
            Error::Capped {
                expires: Stamp::from_nanos(2_000_000_000),
                cap: Stamp::from_nanos(1_000_000_000),
            },
            "the hello expires at 1970-01-01T00:00:02.000000000Z, after the cap \
             1970-01-01T00:00:01.000000000Z",
            "Send a hello that expires within 15 minutes",
        ),
    ];
    for (error, message, fix) in cases {
        assert_eq!(error.to_string(), message);
        assert_eq!(error.fix(), fix);
    }
}

#[test]
fn names_each_changed_field() {
    let cases = [
        (Field::Subject, "subject"),
        (Field::Key, "key"),
        (Field::Via, "`via` node"),
        (Field::Connection, "connection"),
    ];
    for (field, text) in cases {
        let error = Error::Changed { field };
        assert_eq!(
            error.to_string(),
            format!("the renewal names another {text} than the hello it renews")
        );
        assert_eq!(
            error.fix(),
            "Renew with the subject, key, `via`, and connection of the hello it renews"
        );
    }
}

#[test]
fn caps_a_hello_at_15_minutes() {
    assert_eq!(CAP.nanos(), 15 * 60 * 1_000_000_000);
}
