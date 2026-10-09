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
            [REFUSED, UNSYNCED, STALE, VIA, EXPIRED, CHANGED],
            [20, 21, 22, 23, 24, 26]
        );
        assert_eq!(BODY_BYTES_MAX, 16_777_216);
        assert_eq!((Challenge::LEN, Request::LEN, Response::LEN), (33, 73, 9));
    }
}

mod challenge {
    use super::*;

    #[test]
    fn refuses_each_other_kind() {
        for kind in (0..=u8::MAX).filter(|&kind| kind != 4) {
            assert_eq!(
                Challenge::decode(&zeros(kind, Challenge::LEN)),
                Err(Error::Kind { kind })
            );
        }
    }

    #[test]
    fn refuses_an_empty_message() {
        assert_eq!(Challenge::decode(&[]), Err(Error::Empty));
    }

    #[test]
    fn refuses_each_wrong_length() {
        for len in [1, 32, 34] {
            assert_eq!(
                Challenge::decode(&zeros(4, len)),
                Err(Error::Length { len })
            );
        }
    }
}

mod signed {
    use super::*;

    #[test]
    fn decodes_a_hello_and_its_renewals() {
        for subject in ["ops.ana", "a"] {
            let signed = signed(subject);
            assert_eq!(Signed::decode(&encode_signed(&signed)), Ok(signed));
        }
    }

    #[test]
    fn refuses_each_other_kind() {
        let hello = encode_signed(&signed("a"));
        for kind in (0..=u8::MAX).filter(|&kind| kind != 4) {
            let mut bytes = hello.clone();
            bytes[0] = kind;
            assert_eq!(Signed::decode(&bytes), Err(Error::Kind { kind }));
        }
        assert_eq!(
            Signed::decode(&encode_request(request(0))),
            Err(Error::Kind { kind: 5 })
        );
    }

    #[test]
    fn refuses_an_empty_message() {
        assert_eq!(Signed::decode(&[]), Err(Error::Empty));
    }

    #[test]
    fn refuses_each_wrong_length() {
        let hello = encode_signed(&signed("a"));
        for len in [1, 2, 3, 154] {
            let mut bytes = hello.clone();
            bytes.resize(len, 0);
            assert_eq!(Signed::decode(&bytes), Err(Error::Length { len }), "{len}");
        }
        let mut long = hello;
        long.push(0);
        assert_eq!(Signed::decode(&long), Err(Error::Length { len: 156 }));
    }

    #[test]
    fn refuses_a_subject_that_is_not_a_name() {
        let mut hello = encode_signed(&signed("a.b"));
        for subject in [*b"a..", *b"a.*", [0xff, b'.', b'b']] {
            hello[2..5].copy_from_slice(&subject);
            assert_eq!(Signed::decode(&hello), Err(Error::Subject));
        }
        let mut empty = encode_signed(&signed("a"));
        empty.remove(2);
        empty[1] = 0;
        assert_eq!(Signed::decode(&empty), Err(Error::Subject));
    }

    #[test]
    fn refuses_a_key_of_small_order() {
        let mut hello = encode_signed(&signed("a"));
        let mut identity = [0; 32];
        identity[0] = 1;
        hello[3..35].copy_from_slice(&identity);
        assert_eq!(Signed::decode(&hello), Err(Error::SmallOrder));
    }

    #[test]
    fn checks_the_subject_before_the_key() {
        let mut hello = encode_signed(&signed("a"));
        hello[2] = b'*';
        hello[3..35].copy_from_slice(&[0; 32]);
        assert_eq!(Signed::decode(&hello), Err(Error::Subject));
    }
}

mod request {
    use super::*;

    #[test]
    fn refuses_each_other_kind() {
        for kind in (0..=u8::MAX).filter(|&kind| kind != 5) {
            assert_eq!(
                Request::decode(&zeros(kind, Request::LEN)),
                Err(Error::Kind { kind })
            );
        }
        assert_eq!(
            Request::decode(&encode_signed(&signed("a"))),
            Err(Error::Kind { kind: 4 })
        );
    }

    #[test]
    fn refuses_an_empty_message() {
        assert_eq!(Request::decode(&[]), Err(Error::Empty));
    }

    #[test]
    fn refuses_each_wrong_length() {
        for len in [1, 9, 72, 74] {
            assert_eq!(Request::decode(&zeros(5, len)), Err(Error::Length { len }));
        }
    }

    #[test]
    fn checks_the_length_before_the_cap() {
        let mut short = zeros(5, Response::LEN);
        short[1..9].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(Request::decode(&short), Err(Error::Length { len: 9 }));
        let mut long = zeros(5, Request::LEN);
        long[1..9].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(Response::decode(&long), Err(Error::Length { len: 73 }));
    }

    #[test]
    fn takes_a_body_at_the_cap_and_refuses_one_over_it() {
        let at = encode_request(request(BODY_BYTES_MAX));
        assert_eq!(Request::decode(&at), Ok(request(BODY_BYTES_MAX)));
        let mut over = at;
        over[1..9].copy_from_slice(&(BODY_BYTES_MAX + 1).to_le_bytes());
        assert_eq!(
            Request::decode(&over),
            Err(Error::Oversize {
                length: BODY_BYTES_MAX + 1
            })
        );
    }

    #[test]
    fn gives_its_body() {
        assert_eq!(request(5).body().remain(), 5);
        assert_eq!(request(BODY_BYTES_MAX).body().remain(), 16 << 20);
    }
}

mod response {
    use super::*;

    #[test]
    fn decodes_a_response() {
        let response = Response { length: 2 };
        assert_eq!(Response::decode(&encode_response(response)), Ok(response));
    }

    #[test]
    fn refuses_each_other_kind() {
        for kind in (0..=u8::MAX).filter(|&kind| kind != 5) {
            assert_eq!(
                Response::decode(&zeros(kind, Response::LEN)),
                Err(Error::Kind { kind })
            );
        }
        assert_eq!(
            Response::decode(&encode_challenge(challenge())),
            Err(Error::Kind { kind: 4 })
        );
    }

    #[test]
    fn refuses_an_empty_message() {
        assert_eq!(Response::decode(&[]), Err(Error::Empty));
    }

    #[test]
    fn refuses_each_wrong_length() {
        for len in [1, 8, 10] {
            assert_eq!(Response::decode(&zeros(5, len)), Err(Error::Length { len }));
        }
    }

    #[test]
    fn takes_a_body_at_the_cap_and_refuses_one_over_it() {
        let at = Response {
            length: BODY_BYTES_MAX,
        };
        assert_eq!(Response::decode(&encode_response(at)), Ok(at));
        let mut over = encode_response(at);
        over[1..9].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(
            Response::decode(&over),
            Err(Error::Oversize { length: u64::MAX })
        );
    }

    #[test]
    fn gives_its_body() {
        assert_eq!(Response { length: 7 }.body().remain(), 7);
    }
}

mod body {
    use super::*;

    #[test]
    fn takes_each_message_until_it_ends() {
        let mut body = request(5).body();
        assert_eq!(body.take(b"he"), Ok(&b"he"[..]));
        assert_eq!(body.remain(), 3);
        assert_eq!(body.take(b"llo"), Ok(&b"llo"[..]));
        assert_eq!(body.remain(), 0);
    }

    #[test]
    fn takes_kind_bytes_as_body() {
        let mut body = request(73).body();
        let again = encode_request(request(1));
        assert_eq!(body.take(&again), Ok(again.as_slice()));
    }

    #[test]
    fn refuses_a_message_after_the_end() {
        let mut body = request(1).body();
        body.take(b"!").unwrap();
        assert_eq!(body.take(b"!"), Err(Error::Trailing));
        assert_eq!(body.take(&[]), Err(Error::Trailing));
        let mut none = Response { length: 0 }.body();
        assert_eq!(
            none.take(&encode_response(Response { length: 0 })),
            Err(Error::Trailing)
        );
    }

    #[test]
    fn refuses_a_message_past_the_rest() {
        let mut body = request(3).body();
        assert_eq!(body.take(b"four"), Err(Error::Body { len: 4, remain: 3 }));
        assert_eq!(body.remain(), 3);
    }

    #[test]
    fn ends_only_once_no_byte_remains() {
        let mut body = request(3).body();
        assert_eq!(body.end(), Err(Error::Unfinished { remain: 3 }));
        body.take(b"ab").unwrap();
        assert_eq!(body.end(), Err(Error::Unfinished { remain: 1 }));
        body.take(b"c").unwrap();
        assert_eq!(body.end(), Ok(()));
        assert_eq!(Response { length: 0 }.body().end(), Ok(()));
    }

    #[test]
    fn refuses_an_empty_message() {
        let mut body = request(1).body();
        assert_eq!(body.take(&[]), Err(Error::Empty));
        assert_eq!(body.remain(), 1);
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
#[should_panic(expected = "a body of 16777217 bytes is over the cap of 16777216")]
fn panics_on_the_body_of_a_request_over_the_cap() {
    let _body = request(BODY_BYTES_MAX + 1).body();
}

#[test]
#[should_panic(
    expected = "a body of 18446744073709551615 bytes is over the cap of 16777216"
)]
fn panics_on_the_body_of_a_response_over_the_cap() {
    let _body = Response { length: u64::MAX }.body();
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
        prop_assert_eq!(Signed::decode(&encode_signed(&signed)), Ok(signed));
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
        prop_assert_eq!(Challenge::decode(&encode_challenge(challenge)), Ok(challenge));
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
        let decoded = Request::decode(&encode_request(request));
        prop_assert_eq!(decoded, Ok(request));
        let decoded = Response::decode(&encode_response(response));
        prop_assert_eq!(decoded, Ok(response));
        let (mut sent, mut answered) = (request.body(), response.body());
        let mut sizes = sizes.into_iter().cycle();
        let mut rest = body.as_slice();
        while !rest.is_empty() {
            let size = sizes.next().unwrap().min(rest.len());
            let (bytes, after) = rest.split_at(size);
            rest = after;
            prop_assert_eq!(sent.take(bytes), Ok(bytes));
            prop_assert_eq!(answered.take(bytes), Ok(bytes));
            prop_assert_eq!(sent.remain(), rest.len());
            prop_assert_eq!(answered.remain(), rest.len());
        }
        prop_assert_eq!(sent.end(), Ok(()));
        prop_assert_eq!(answered.end(), Ok(()));
        prop_assert_eq!(sent.take(&[0]), Err(Error::Trailing));
        prop_assert_eq!(answered.take(&[0]), Err(Error::Trailing));
    }
}

#[test]
fn gives_the_code_and_the_text_of_each_refusal() {
    let cases = [
        (
            Refusal::Malformed,
            2,
            "a message of the program broke the client wire",
        ),
        (Refusal::Busy, 19, "the node had no memory for a response"),
        (
            Refusal::Refused,
            20,
            "the spec has no such subject, does not list the key for it, or the \
             signature is not valid",
        ),
        (Refusal::Unsynced, 21, "the node has no mesh time yet"),
        (
            Refusal::Stale,
            22,
            "the hello does not echo the nonce of the node's last challenge",
        ),
        (Refusal::Via, 23, "the hello names another node as via"),
        (Refusal::Expired, 24, "the hello expired"),
        (
            Refusal::Changed,
            26,
            "a renewal names another subject, key, via, or connection than the hello \
             it renews",
        ),
    ];
    for (refusal, code, text) in cases {
        assert_eq!(refusal.code(), code);
        assert_eq!(Refusal::from_code(code), Some(refusal));
        assert_eq!(refusal.to_string(), text);
    }
}

#[test]
fn gives_no_refusal_for_0_or_a_code_outside_the_set() {
    for code in [0, 1, 3, 16, 17, 18, 25, 27, u32::MAX] {
        assert_eq!(Refusal::from_code(code), None, "{code}");
    }
}
