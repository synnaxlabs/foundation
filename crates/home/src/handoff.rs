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

use crate::state;

/// The buffer tag of a handoff entry.
const TAG: NonZeroU8 = NonZeroU8::MIN;

/// The most bytes in the body of a handoff: the authority and the longest subject.
pub(crate) const MAX_BYTES: usize = 1 + Name::MAX_BYTES;
const _: () = assert!(
    MAX_BYTES <= Layout::ENTRY_MAX_MIN,
    "a handoff fits one entry of any ring"
);

/// Appends the entry that records `handoff`, alone, with `head`, and returns what
/// [`state::append`] does.
pub(crate) fn append(buffer: &Buffer, handoff: Handoff<'_>, head: state::Head) -> bool {
    state::append(buffer, TAG, body(handoff).as_deref(), head)
}

/// The body of the entry that records `handoff`, or `None` when no writer holds
/// control.
fn body(handoff: Handoff<'_>) -> Option<Vec<u8>> {
    let holder = handoff.to?;
    let subject = holder.subject.as_str().as_bytes();
    let mut body = Vec::with_capacity(1 + subject.len());
    body.push(holder.authority.0);
    body.extend_from_slice(subject);
    Some(body)
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
/// If the subject is not a valid name. Bytes from another node must be checked before
/// they reach `read`.
fn read(body: &[u8]) -> Option<Writer> {
    let (&authority, subject) = body.split_first()?;
    let subject = str::from_utf8(subject)
        .unwrap_or_else(|error| panic!("the handoff subject is not UTF-8: {error}"));
    let subject = subject
        .parse()
        .unwrap_or_else(|error| panic!("the handoff subject is not a name: {error}"));
    Some(Writer {
        subject,
        authority: Authority(authority),
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn writer(subject: &str, authority: u8) -> Writer {
        Writer {
            subject: subject.parse().expect("a valid name"),
            authority: Authority(authority),
        }
    }

    mod body {
        use super::*;

        #[test]
        fn writes_the_authority_then_the_subject() {
            let holder = writer("plant.pump-1", 7);

            let body = body(Handoff { to: Some(&holder) }).expect("a holder");

            assert_eq!(body, b"\x07plant.pump-1");
        }

        #[test]
        fn writes_the_largest_body_for_the_longest_subject() {
            let holder = writer(&"b".repeat(Name::MAX_BYTES), 7);

            let body = body(Handoff { to: Some(&holder) }).expect("a holder");

            assert_eq!(body.len(), MAX_BYTES);
        }

        #[test]
        fn has_no_body_when_no_writer_holds_control() {
            assert_eq!(body(Handoff { to: None }), None);
        }
    }

    mod read {
        use super::*;

        #[test]
        fn reads_the_holder() {
            assert_eq!(read(b"\x07plant.pump-1"), Some(writer("plant.pump-1", 7)));
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
                read(&[7, b'a', 0xff]);
            }

            #[test]
            #[should_panic(
                expected = "the handoff subject is not a name: a segment is not \
                                       valid: \"\" in \"a..b\""
            )]
            fn panics_on_a_subject_that_is_not_a_name() {
                read(b"\x07a..b");
            }

            #[test]
            #[should_panic(expected = "the handoff subject is not a name: a name or \
                                       pattern is empty")]
            fn panics_on_an_authority_without_a_subject() {
                read(&[7]);
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
            ))
        ) {
            let holder = holder.map(|(subject, authority)| writer(&subject, authority));
            let body = body(Handoff { to: holder.as_ref() });
            let bytes = body.as_deref().unwrap_or_default();

            prop_assert!(bytes.len() <= MAX_BYTES);
            prop_assert_eq!(read(bytes), holder);
        }
    }
}
