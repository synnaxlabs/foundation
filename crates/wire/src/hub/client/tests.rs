use proptest::prelude::*;

use super::*;

const KEY: [u8; 32] = [
    0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64,
    0x07, 0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68,
    0xf7, 0x07, 0x51, 0x1a,
];

fn challenge() -> Challenge {
    Challenge {
        nonce: [0xa0; 16],
        now: Interval {
            earliest: Stamp::from_nanos(0x0102_0304_0506_0708),
            latest: Stamp::from_nanos(-2),
        },
    }
}

fn signed(subject: &str) -> Signed {
    Signed {
        hello: Hello {
            subject: subject.parse().unwrap(),
            key: PublicKey::new(KEY).unwrap(),
            via: node::Key::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
            connection: connection::Key([0xc0; 16]),
            nonce: [0xa1; 16],
            expires: Stamp::from_nanos(0x1122_3344_5566_7788),
        },
        signature: [0x5e; 64],
    }
}

fn request(length: u64) -> Request {
    Request {
        length,
        signature: [0x5e; 64],
    }
}

fn encode_challenge(challenge: Challenge) -> Vec<u8> {
    let mut out = vec![0xaa; Challenge::LEN];
    challenge.encode(&mut out);
    out
}

fn encode_signed(signed: &Signed) -> Vec<u8> {
    let mut out = vec![0xaa; signed.encoded_len()];
    signed.encode(&mut out);
    out
}

fn encode_request(request: Request) -> Vec<u8> {
    let mut out = vec![0xaa; Request::LEN];
    request.encode(&mut out);
    out
}

fn encode_response(response: Response) -> Vec<u8> {
    let mut out = vec![0xaa; Response::LEN];
    response.encode(&mut out);
    out
}

/// `kind`, then `len - 1` zeros.
fn zeros(kind: u8, len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    bytes[0] = kind;
    bytes
}

mod pins {
    use super::*;

    #[test]
    fn the_challenge() {
        let mut bytes = vec![4];
        bytes.extend([0xa0; 16]);
        bytes.extend([8, 7, 6, 5, 4, 3, 2, 1]);
        bytes.extend([0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(encode_challenge(challenge()), bytes);
    }

    #[test]
    fn the_hello() {
        let mut bytes = vec![4, 7];
        bytes.extend(b"ops.ana");
        bytes.extend(KEY);
        bytes.extend([0xef, 0xcd, 0xab, 0x89, 0x67, 0x45, 0x23, 0x01]);
        bytes.extend([0xef, 0xcd, 0xab, 0x89, 0x67, 0x45, 0x23, 0x01]);
        bytes.extend([0xc0; 16]);
        bytes.extend([0xa1; 16]);
        bytes.extend([0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]);
        bytes.extend([0x5e; 64]);
        let signed = signed("ops.ana");
        assert_eq!(signed.encoded_len(), 161);
        assert_eq!(encode_signed(&signed), bytes);
    }

    #[test]
    fn the_request() {
        let mut bytes = vec![5, 0xef, 0xcd, 0xab, 0, 0, 0, 0, 0];
        bytes.extend([0x5e; 64]);
        assert_eq!(encode_request(request(0x00ab_cdef)), bytes);
    }

    #[test]
    fn the_response() {
        assert_eq!(
            encode_response(Response {
                length: 0x0001_0203
            }),
            [5, 3, 2, 1, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn the_constants() {
        assert_eq!(
            [REFUSED, UNSYNCED, STALE, VIA, EXPIRED, CAPPED, CHANGED],
            [20, 21, 22, 23, 24, 25, 26]
        );
        assert_eq!(BODY_BYTES_MAX, 16_777_216);
        assert_eq!((Challenge::LEN, Request::LEN, Response::LEN), (33, 73, 9));
    }
}

mod gateway {
    use super::*;

    #[test]
    fn decodes_a_hello_and_its_renewals() {
        let mut gateway = Gateway::default();
        for subject in ["ops.ana", "ops.ana", "a"] {
            let signed = signed(subject);
            assert_eq!(
                gateway.decode(&encode_signed(&signed)),
                Ok(FromProgram::Signed(signed))
            );
        }
    }

    #[test]
    fn decodes_a_request_and_its_body() {
        let mut gateway = Gateway::default();
        let request = request(5);
        assert_eq!(
            gateway.decode(&encode_request(request)),
            Ok(FromProgram::Request(request))
        );
        assert_eq!(
            gateway.decode(b"he"),
            Ok(FromProgram::Body {
                bytes: b"he",
                last: false
            })
        );
        assert_eq!(
            gateway.decode(b"llo"),
            Ok(FromProgram::Body {
                bytes: b"llo",
                last: true
            })
        );
        assert_eq!(gateway.decode(b"!"), Err(Error::Trailing));
        assert_eq!(gateway.decode(&[]), Err(Error::Trailing));
    }

    #[test]
    fn reads_kind_bytes_in_a_body_as_body() {
        let mut gateway = Gateway::default();
        gateway.decode(&encode_request(request(73))).unwrap();
        let again = encode_request(request(1));
        assert_eq!(
            gateway.decode(&again),
            Ok(FromProgram::Body {
                bytes: &again,
                last: true
            })
        );
    }

    #[test]
    fn takes_no_message_after_a_request_of_no_body() {
        let mut gateway = Gateway::default();
        gateway.decode(&encode_request(request(0))).unwrap();
        assert_eq!(
            gateway.decode(&encode_request(request(0))),
            Err(Error::Trailing)
        );
        assert_eq!(
            gateway.decode(&encode_signed(&signed("a"))),
            Err(Error::Trailing)
        );
    }

    #[test]
    fn refuses_a_request_on_the_hello_stream() {
        let mut gateway = Gateway::default();
        gateway.decode(&encode_signed(&signed("a"))).unwrap();
        assert_eq!(
            gateway.decode(&encode_request(request(1))),
            Err(Error::Mixed { kind: 5 })
        );
    }

    #[test]
    fn refuses_a_body_message_past_the_body() {
        let mut gateway = Gateway::default();
        gateway.decode(&encode_request(request(3))).unwrap();
        assert_eq!(
            gateway.decode(b"four"),
            Err(Error::Body { len: 4, remain: 3 })
        );
    }

    #[test]
    fn refuses_an_empty_message_at_each_step() {
        let mut gateway = Gateway::default();
        assert_eq!(gateway.decode(&[]), Err(Error::Empty));
        gateway.decode(&encode_request(request(1))).unwrap();
        assert_eq!(gateway.decode(&[]), Err(Error::Empty));
        let mut hello = Gateway::default();
        hello.decode(&encode_signed(&signed("a"))).unwrap();
        assert_eq!(hello.decode(&[]), Err(Error::Empty));
    }

    #[test]
    fn refuses_unknown_kinds() {
        for kind in (0..=u8::MAX).filter(|kind| ![4, 5].contains(kind)) {
            assert_eq!(
                Gateway::default().decode(&zeros(kind, 73)),
                Err(Error::Kind { kind })
            );
        }
    }

    #[test]
    fn refuses_a_request_over_the_cap() {
        let mut bytes = encode_request(request(0));
        bytes[1..9].copy_from_slice(&(BODY_BYTES_MAX + 1).to_le_bytes());
        assert_eq!(
            Gateway::default().decode(&bytes),
            Err(Error::Oversize {
                length: BODY_BYTES_MAX + 1
            })
        );
        bytes[1..9].copy_from_slice(&BODY_BYTES_MAX.to_le_bytes());
        assert_eq!(
            Gateway::default().decode(&bytes),
            Ok(FromProgram::Request(request(BODY_BYTES_MAX)))
        );
    }

    #[test]
    fn refuses_each_wrong_length() {
        for len in [1, 9, 72, 74] {
            assert_eq!(
                Gateway::default().decode(&zeros(5, len)),
                Err(Error::Length { len })
            );
        }
        let hello = encode_signed(&signed("a"));
        for len in [1, 2, 3, 154] {
            let mut bytes = hello.clone();
            bytes.resize(len, 0);
            assert_eq!(
                Gateway::default().decode(&bytes),
                Err(Error::Length { len }),
                "{len}"
            );
        }
        let mut long = hello;
        long.push(0);
        assert_eq!(
            Gateway::default().decode(&long),
            Err(Error::Length { len: 156 })
        );
    }

    #[test]
    fn refuses_a_subject_that_is_not_a_name() {
        let mut hello = encode_signed(&signed("a.b"));
        for subject in [*b"a..", *b"a.*", [0xff, b'.', b'b']] {
            hello[2..5].copy_from_slice(&subject);
            assert_eq!(Gateway::default().decode(&hello), Err(Error::Subject));
        }
        let mut empty = encode_signed(&signed("a"));
        empty.remove(2);
        empty[1] = 0;
        assert_eq!(Gateway::default().decode(&empty), Err(Error::Subject));
    }

    #[test]
    fn refuses_a_key_of_small_order() {
        let mut hello = encode_signed(&signed("a"));
        let mut identity = [0; 32];
        identity[0] = 1;
        hello[3..35].copy_from_slice(&identity);
        assert_eq!(Gateway::default().decode(&hello), Err(Error::SmallOrder));
    }

    #[test]
    fn checks_the_subject_before_the_key() {
        let mut hello = encode_signed(&signed("a"));
        hello[2] = b'*';
        hello[3..35].copy_from_slice(&[0; 32]);
        assert_eq!(Gateway::default().decode(&hello), Err(Error::Subject));
    }

    #[test]
    fn decodes_the_message_before_the_order() {
        let mut gateway = Gateway::default();
        gateway.decode(&encode_signed(&signed("a"))).unwrap();
        assert_eq!(gateway.decode(&[5, 0]), Err(Error::Length { len: 2 }));
        assert_eq!(gateway.decode(&[9]), Err(Error::Kind { kind: 9 }));
    }
}

mod program {
    use super::*;

    #[test]
    fn decodes_each_challenge() {
        let mut program = Program::default();
        for _ in 0..3 {
            assert_eq!(
                program.decode(&encode_challenge(challenge())),
                Ok(FromGateway::Challenge(challenge()))
            );
        }
    }

    #[test]
    fn decodes_a_response_and_its_body() {
        let mut program = Program::default();
        let response = Response { length: 2 };
        assert_eq!(
            program.decode(&encode_response(response)),
            Ok(FromGateway::Response(response))
        );
        assert_eq!(
            program.decode(b"ok"),
            Ok(FromGateway::Body {
                bytes: b"ok",
                last: true
            })
        );
        assert_eq!(
            program.decode(&encode_challenge(challenge())),
            Err(Error::Trailing)
        );
    }

    #[test]
    fn refuses_a_response_on_the_hello_stream() {
        let mut program = Program::default();
        program.decode(&encode_challenge(challenge())).unwrap();
        assert_eq!(
            program.decode(&encode_response(Response { length: 0 })),
            Err(Error::Mixed { kind: 5 })
        );
    }

    #[test]
    fn refuses_a_response_over_the_cap() {
        let mut bytes = encode_response(Response { length: 0 });
        bytes[1..9].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(
            Program::default().decode(&bytes),
            Err(Error::Oversize { length: u64::MAX })
        );
    }

    #[test]
    fn refuses_each_wrong_length() {
        for (kind, len) in [(4, 1), (4, 32), (4, 34), (5, 1), (5, 8), (5, 10)] {
            assert_eq!(
                Program::default().decode(&zeros(kind, len)),
                Err(Error::Length { len })
            );
        }
    }
}

#[test]
#[should_panic(expected = "a body of 16777217 bytes is over the cap of 16777216")]
fn panics_on_a_request_over_the_cap() {
    request(BODY_BYTES_MAX + 1).encode(&mut [0; Request::LEN]);
}

#[test]
#[should_panic(expected = "a body of 16777217 bytes is over the cap of 16777216")]
fn panics_on_a_response_over_the_cap() {
    Response {
        length: BODY_BYTES_MAX + 1,
    }
    .encode(&mut [0; Response::LEN]);
}

#[test]
#[should_panic(expected = "out has 160 bytes, and the message has 161")]
fn panics_when_out_has_the_wrong_length() {
    signed("ops.ana").encode(&mut [0; 160]);
}

fn hello() -> impl Strategy<Value = Signed> {
    let subject = "[a-z0-9_-]{1,20}(\\.[a-z0-9_-]{1,20}){0,3}";
    (
        subject,
        any::<[u8; 32]>(),
        any::<u128>(),
        any::<[u8; 16]>(),
        any::<[u8; 16]>(),
        any::<i64>(),
        any::<[u8; 32]>(),
    )
        .prop_filter_map("the key is of small order", |parts| {
            let (subject, key, via, connection, nonce, expires, half) = parts;
            let mut signature = [0; 64];
            signature[..32].copy_from_slice(&half);
            Some(Signed {
                hello: Hello {
                    subject: subject.parse().ok()?,
                    key: PublicKey::new(key).ok()?,
                    via: node::Key::from_u128(via),
                    connection: connection::Key(connection),
                    nonce,
                    expires: Stamp::from_nanos(expires),
                },
                signature,
            })
        })
}

proptest! {
    #[test]
    fn round_trips_a_hello(signed in hello()) {
        let bytes = encode_signed(&signed);
        let decoded = Gateway::default().decode(&bytes);
        prop_assert_eq!(decoded, Ok(FromProgram::Signed(signed)));
    }

    #[test]
    fn round_trips_a_challenge(
        nonce in any::<[u8; 16]>(),
        earliest in any::<i64>(),
        latest in any::<i64>(),
    ) {
        let challenge = Challenge {
            nonce,
            now: Interval {
                earliest: Stamp::from_nanos(earliest),
                latest: Stamp::from_nanos(latest),
            },
        };
        let bytes = encode_challenge(challenge);
        prop_assert_eq!(
            Program::default().decode(&bytes),
            Ok(FromGateway::Challenge(challenge))
        );
    }

    #[test]
    fn decodes_a_body_cut_into_messages_of_any_size(
        body in proptest::collection::vec(any::<u8>(), 0..64),
        sizes in proptest::collection::vec(1..=9_usize, 1..8),
        signature in any::<[u8; 32]>(),
    ) {
        let mut request = request(u64::try_from(body.len()).unwrap());
        request.signature[..32].copy_from_slice(&signature);
        let response = Response { length: request.length };
        let mut gateway = Gateway::default();
        let mut program = Program::default();
        let (sent, answered) = (encode_request(request), encode_response(response));
        prop_assert_eq!(gateway.decode(&sent), Ok(FromProgram::Request(request)));
        prop_assert_eq!(program.decode(&answered), Ok(FromGateway::Response(response)));
        let mut sizes = sizes.into_iter().cycle();
        let mut rest = body.as_slice();
        while !rest.is_empty() {
            let size = sizes.next().unwrap().min(rest.len());
            let (bytes, after) = rest.split_at(size);
            rest = after;
            let last = rest.is_empty();
            let body = FromProgram::Body { bytes, last };
            prop_assert_eq!(gateway.decode(bytes), Ok(body));
            let body = FromGateway::Body { bytes, last };
            prop_assert_eq!(program.decode(bytes), Ok(body));
        }
        prop_assert_eq!(gateway.decode(&[0]), Err(Error::Trailing));
        prop_assert_eq!(program.decode(&[0]), Err(Error::Trailing));
    }
}
