//! The handoff entry of an index. Its body names the writer that holds control of the
//! index from the entry's place in the index log.

use std::num::NonZeroU8;
use std::str;

use buffer::{Buffer, Layout};
use control::{Handoff, Writer};
use types::authority::Authority;
use types::channel::Slot;
use types::frame::Path;
use types::name::Name;
use types::node;

use crate::state;

/// The buffer tag of a handoff entry.
const TAG: NonZeroU8 = NonZeroU8::MIN;

/// The bytes of the node key in the body of a handoff.
const VIA: usize = size_of::<u128>();

/// The most bytes in the body of a handoff: the authority, the node key, and the
/// longest subject.
pub(crate) const MAX_BYTES: usize = 1 + VIA + Name::MAX_BYTES;
const _: () = assert!(
    MAX_BYTES <= Layout::ENTRY_MAX_MIN,
    "a handoff fits one entry of any ring"
);

/// Appends the entry that records `handoff`, alone, with `head`, and returns what
/// [`state::append`] does.
pub(crate) fn append(buffer: &Buffer, handoff: Handoff<'_>, head: state::Head) -> bool {
    state::append(buffer, TAG, body(handoff, &mut [0; MAX_BYTES]), head)
}

/// The body of the entry that records `handoff`, written at the start of `out`, or
/// `None` when no writer holds control.
fn body<'a>(handoff: Handoff<'_>, out: &'a mut [u8; MAX_BYTES]) -> Option<&'a [u8]> {
    let holder = handoff.to?;
    let subject = holder.subject.as_str().as_bytes();
    let (authority, rest) = out.split_at_mut(1);
    let (via, rest) = rest.split_at_mut(VIA);
    authority[0] = holder.authority.0;
    via.copy_from_slice(&holder.via.as_u128().to_le_bytes());
    rest[..subject.len()].copy_from_slice(subject);
    Some(&out[..1 + VIA + subject.len()])
}

/// The holder that the newest durable handoff of each index in `buffer` names, with
/// the index's slot, in slot order. An index whose newest handoff names no holder
/// is absent.
///
/// # Panics
///
/// When a handoff does not decode, as [`read`] says.
pub(crate) fn newest(buffer: &Buffer) -> Vec<(Slot, Writer)> {
    let found = buffer.newest(Path::Live, TAG);
    let holders = found
        .into_iter()
        .filter_map(|(slot, bytes)| Some((slot, read(&bytes)?)));
    holders.collect()
}

/// The holder that `body`, the bytes of a handoff record, names, or `None` when no
/// writer holds control.
///
/// # Panics
///
/// If the body ends before its subject, or the subject is not a valid name. Bytes
/// from another node must be checked before they reach `read`.
fn read(body: &[u8]) -> Option<Writer> {
    let (&authority, rest) = body.split_first()?;
    let (via, subject) = rest.split_first_chunk::<VIA>().unwrap_or_else(|| {
        panic!("the handoff body of {} bytes has no node", body.len())
    });
    let subject = str::from_utf8(subject)
        .unwrap_or_else(|error| panic!("the handoff subject is not UTF-8: {error}"));
    let subject = subject
        .parse()
        .unwrap_or_else(|error| panic!("the handoff subject is not a name: {error}"));
    Some(Writer {
        subject,
        authority: Authority(authority),
        via: node::Key::from_u128(u128::from_le_bytes(*via)),
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// The node key of each writer of a test, and its bytes in a body.
    const VIA: u128 = 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10;
    const VIA_BYTES: &[u8; 16] =
        b"\x10\x0f\x0e\x0d\x0c\x0b\x0a\x09\x08\x07\x06\x05\x04\x03\x02\x01";

    fn writer(subject: &str, authority: u8) -> Writer {
        Writer {
            subject: subject.parse().expect("a valid name"),
            authority: Authority(authority),
            via: node::Key::from_u128(VIA),
        }
    }

    /// The body of a handoff to `subject` at authority 7.
    fn bytes(subject: &str) -> Vec<u8> {
        [&[7], &VIA_BYTES[..], subject.as_bytes()].concat()
    }

    mod body {
        use super::*;

        #[test]
        fn writes_the_authority_the_node_then_the_subject() {
            let holder = writer("plant.pump-1", 7);
            let mut out = [0; MAX_BYTES];

            let body = body(Handoff { to: Some(&holder) }, &mut out).expect("a holder");

            assert_eq!(body, bytes("plant.pump-1"));
        }

        #[test]
        fn writes_the_largest_body_for_the_longest_subject() {
            let holder = writer(&"b".repeat(Name::MAX_BYTES), 7);
            let mut out = [0; MAX_BYTES];

            let body = body(Handoff { to: Some(&holder) }, &mut out).expect("a holder");

            assert_eq!(body.len(), MAX_BYTES);
        }

        #[test]
        fn has_no_body_when_no_writer_holds_control() {
            assert_eq!(body(Handoff { to: None }, &mut [0; MAX_BYTES]), None);
        }
    }

    mod read {
        use super::*;

        #[test]
        fn reads_the_holder() {
            assert_eq!(
                read(&bytes("plant.pump-1")),
                Some(writer("plant.pump-1", 7))
            );
        }

        #[test]
        fn reads_no_holder_from_no_bytes() {
            assert_eq!(read(&[]), None);
        }

        mod when_damaged {
            use super::*;

            #[test]
            #[should_panic(
                expected = "the handoff subject is not UTF-8: invalid utf-8 \
                                       sequence of 1 bytes from index 1"
            )]
            fn panics_on_a_subject_that_is_not_utf8() {
                read(&[&bytes("a")[..], &[0xff]].concat());
            }

            #[test]
            #[should_panic(
                expected = "the handoff subject is not a name: a segment is not \
                                       valid: \"\" in \"a..b\""
            )]
            fn panics_on_a_subject_that_is_not_a_name() {
                read(&bytes("a..b"));
            }

            #[test]
            #[should_panic(expected = "the handoff subject is not a name: a name or \
                                       pattern is empty")]
            fn panics_on_a_node_without_a_subject() {
                read(&bytes("")[..]);
            }
        }
    }

    proptest! {
        #[test]
        fn reads_back_each_handoff(
            holder in proptest::option::of((
                "@?[A-Za-z0-9_-]{1,63}(\\.@?[A-Za-z0-9_-]{1,63}){0,3}"
                    .prop_filter("a name", |subject| subject.len() <= Name::MAX_BYTES),
                any::<u8>(),
                any::<u128>(),
            ))
        ) {
            let holder = holder.map(|(subject, authority, via)| Writer {
                subject: subject.parse().expect("a valid name"),
                authority: Authority(authority),
                via: node::Key::from_u128(via),
            });
            let mut out = [0; MAX_BYTES];
            let bytes = body(Handoff { to: holder.as_ref() }, &mut out).unwrap_or_default();

            prop_assert!(bytes.len() <= MAX_BYTES);
            prop_assert_eq!(read(bytes), holder);
        }
    }
}
