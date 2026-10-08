//! `wire::hub::client::Gateway` and `Program` never panic, each message encodes to
//! its bytes and comes in the order of a client stream, each body ends at its length,
//! each refusal is the one that the order gives, and each valid message made from the
//! input reads back.
//!
//! Input: the messages of one stream (`fuzz::messages`), read by both decoders.

#![no_main]

use libfuzzer_sys::{
    arbitrary::{self, Unstructured},
    fuzz_target,
};
use types::connection;
use types::ed25519::PublicKey;
use types::hello::Hello;
use types::node;
use types::time::{Interval, Stamp};
use wire::hub::Error;
use wire::hub::client::{
    BODY_BYTES_MAX, Challenge, FromNode, FromProgram, Gateway, Program, Request,
    Response, Signed,
};

/// What a decoder must take next, kept apart from the decoder.
#[derive(Clone, Copy, Debug)]
enum Next {
    First,
    Hello,
    Body { remain: usize },
    Done,
}

/// One decoded message of either side: its bytes encoded again, and the body length
/// for a request or a response.
enum Event<'m> {
    Fixed {
        encoded: Vec<u8>,
        length: Option<u64>,
    },
    Body {
        bytes: &'m [u8],
        last: bool,
    },
}

fn gateway<'m>(gateway: &mut Gateway, message: &'m [u8]) -> Result<Event<'m>, Error> {
    gateway.decode(message).map(|event| match event {
        FromProgram::Signed(signed) => {
            let mut encoded = vec![0; signed.encoded_len()];
            signed.encode(&mut encoded);
            Event::Fixed {
                encoded,
                length: None,
            }
        }
        FromProgram::Request(request) => {
            let mut encoded = vec![0; Request::LEN];
            request.encode(&mut encoded);
            Event::Fixed {
                encoded,
                length: Some(request.length),
            }
        }
        FromProgram::Body { bytes, last } => Event::Body { bytes, last },
    })
}

fn program<'m>(program: &mut Program, message: &'m [u8]) -> Result<Event<'m>, Error> {
    program.decode(message).map(|event| match event {
        FromNode::Challenge(challenge) => {
            let mut encoded = vec![0; Challenge::LEN];
            challenge.encode(&mut encoded);
            Event::Fixed {
                encoded,
                length: None,
            }
        }
        FromNode::Response(response) => {
            let mut encoded = vec![0; Response::LEN];
            response.encode(&mut encoded);
            Event::Fixed {
                encoded,
                length: Some(response.length),
            }
        }
        FromNode::Body { bytes, last } => Event::Body { bytes, last },
    })
}

/// Whether `error` says only that the bytes of a message do not decode.
fn malformed(error: Error) -> bool {
    matches!(
        error,
        Error::Empty
            | Error::Kind { .. }
            | Error::Length { .. }
            | Error::Subject
            | Error::SmallOrder
            | Error::Oversize { .. }
    )
}

/// Each event of the stream in `bytes` must encode to its message and come in the
/// order of a client stream, and each refusal must be the one that the order gives.
fn read<D>(
    bytes: &[u8],
    mut decoder: D,
    decode: for<'m> fn(&mut D, &'m [u8]) -> Result<Event<'m>, Error>,
    fresh: fn() -> D,
) {
    let mut next = Next::First;
    for message in fuzz::messages(bytes) {
        next = match (next, decode(&mut decoder, message)) {
            (Next::First | Next::Hello, Ok(Event::Fixed { encoded, length })) => {
                assert_eq!(encoded, message, "the message changed");
                match (next, length) {
                    (_, None) => Next::Hello,
                    (Next::First, Some(0)) => Next::Done,
                    (Next::First, Some(length)) => {
                        assert!(length <= BODY_BYTES_MAX, "a body over the cap came");
                        Next::Body {
                            remain: usize::try_from(length).expect("the body fits"),
                        }
                    }
                    (next, Some(_)) => panic!("a request came on {next:?}"),
                }
            }
            (Next::Body { remain }, Ok(Event::Body { bytes, last })) => {
                assert_eq!(bytes, message, "the body bytes changed");
                assert!(!bytes.is_empty(), "an empty body message came");
                let remain = remain
                    .checked_sub(bytes.len())
                    .expect("a body message past the body came");
                assert_eq!(last, remain == 0, "the body ends at another message");
                if last {
                    Next::Done
                } else {
                    Next::Body { remain }
                }
            }
            (next, Err(error)) => {
                let correct = match next {
                    Next::First => malformed(error),
                    Next::Hello => match decode(&mut fresh(), message) {
                        Ok(Event::Fixed {
                            length: Some(_), ..
                        }) => error == Error::Mixed { kind: 5 },
                        Ok(_) => false,
                        Err(alone) => error == alone,
                    },
                    Next::Body { remain } => {
                        let len = message.len();
                        if len == 0 {
                            error == Error::Empty
                        } else {
                            error == Error::Body { len, remain }
                        }
                    }
                    Next::Done => error == Error::Trailing,
                };
                assert!(
                    correct,
                    "{error:?} is not the refusal of {message:?} for {next:?}"
                );
                next
            }
            (next, Ok(_)) => panic!("an event came that {next:?} does not take"),
        };
    }
}

/// Each valid message made from `input` must decode to itself. A key of small order
/// or a subject that is not a name is not valid, so it becomes a fixed one.
fn write(input: &mut Unstructured) -> arbitrary::Result<()> {
    let key = PublicKey::new(input.arbitrary()?)
        .unwrap_or_else(|_small| PublicKey::new([9; 32]).expect("a valid key"));
    let subject = input
        .arbitrary::<&str>()?
        .parse()
        .unwrap_or_else(|_bad| "ops.ana".parse().expect("a name"));
    let signed = Signed {
        hello: Hello {
            subject,
            key,
            via: node::Key::from_u128(input.arbitrary()?),
            connection: connection::Key(input.arbitrary()?),
            nonce: input.arbitrary()?,
            expires: Stamp::from_nanos(input.arbitrary()?),
        },
        signature: input.arbitrary()?,
    };
    let mut out = vec![0; signed.encoded_len()];
    signed.encode(&mut out);
    match Gateway::default().decode(&out) {
        Ok(FromProgram::Signed(decoded)) => {
            assert_eq!(decoded, signed, "the hello changed")
        }
        other => panic!("a hello did not read back: {other:?}"),
    }

    let request = Request {
        length: input.int_in_range(0..=BODY_BYTES_MAX)?,
        signature: input.arbitrary()?,
    };
    let mut out = [0; Request::LEN];
    request.encode(&mut out);
    match Gateway::default().decode(&out) {
        Ok(FromProgram::Request(decoded)) => {
            assert_eq!(decoded, request, "the request changed")
        }
        other => panic!("a request did not read back: {other:?}"),
    }

    let challenge = Challenge {
        nonce: input.arbitrary()?,
        now: Interval {
            earliest: Stamp::from_nanos(input.arbitrary()?),
            latest: Stamp::from_nanos(input.arbitrary()?),
        },
    };
    let mut out = [0; Challenge::LEN];
    challenge.encode(&mut out);
    match Program::default().decode(&out) {
        Ok(FromNode::Challenge(decoded)) => {
            assert_eq!(decoded, challenge, "the challenge changed")
        }
        other => panic!("a challenge did not read back: {other:?}"),
    }

    let response = Response {
        length: input.int_in_range(0..=BODY_BYTES_MAX)?,
    };
    let mut out = [0; Response::LEN];
    response.encode(&mut out);
    match Program::default().decode(&out) {
        Ok(FromNode::Response(decoded)) => {
            assert_eq!(decoded, response, "the response changed")
        }
        other => panic!("a response did not read back: {other:?}"),
    }
    Ok(())
}

fuzz_target!(|bytes: &[u8]| {
    read(bytes, Gateway::default(), gateway, Gateway::default);
    read(bytes, Program::default(), program, Program::default);
    write(&mut Unstructured::new(bytes)).expect("an input that ends gives zeros");
});
