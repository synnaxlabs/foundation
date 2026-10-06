//! The bytes of a handoff record: the writer that holds control of an index from the
//! record's place in the index log.

use std::str;

use block::Block;
use control::{Handoff, Writer};
use types::authority::Authority;
use types::name::Name;

/// The buffer tag of a handoff entry.
pub(crate) const TAG: u8 = 1;

/// The most bytes in the body of a handoff: the authority and the longest subject.
/// One entry of a record holds it alone, since `Layout::entry_max` is at least 4032.
pub(crate) const MAX_BYTES: usize = 1 + Name::MAX_BYTES;
const _: () = assert!(MAX_BYTES <= 4032, "a handoff fits one entry of any ring");

/// The parts of the buffer entry that records `handoff`: a block from `pool`, or `None`
/// when no writer holds control.
///
/// # Errors
///
/// [`block::Error`] when `pool` has no block for the bytes.
pub(crate) fn body(
    pool: &block::Pool,
    handoff: Handoff<'_>,
) -> Result<Option<Block>, block::Error> {
    let Some(holder) = handoff.to else {
        return Ok(None);
    };
    let subject = holder.subject.as_str().as_bytes();
    let mut block = pool.alloc(1 + subject.len())?;
    block[0] = holder.authority.0;
    block[1..].copy_from_slice(subject);
    Ok(Some(block.freeze()))
}

/// The holder that `body`, the bytes of a handoff record, names, or `None` when no
/// writer holds control.
///
/// # Panics
///
/// If the subject is not a valid name. Bytes from another node must be checked before
/// they reach `read`.
pub(crate) fn read(body: &[u8]) -> Option<Writer> {
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
    use crate::common::pool;

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

            let body = body(&pool(4096), Handoff { to: Some(&holder) })
                .expect("room")
                .expect("a holder");

            assert_eq!(&body[..], b"\x07plant.pump-1");
        }

        #[test]
        fn writes_the_largest_body_for_the_longest_subject() {
            let holder = writer(&"b".repeat(Name::MAX_BYTES), 7);

            let body = body(&pool(4096), Handoff { to: Some(&holder) })
                .expect("room")
                .expect("a holder");

            assert_eq!(body.len(), MAX_BYTES);
        }

        #[test]
        fn takes_no_block_when_no_writer_holds_control() {
            let pool = pool(4096);

            let body = body(&pool, Handoff { to: None }).expect("no block");

            assert!(body.is_none(), "a block for no holder: {body:?}");
            assert_eq!(pool.committed(), 0);
        }

        #[test]
        fn returns_the_pool_error_when_the_pool_is_full() {
            let pool = pool(4096);
            let mut held = Vec::new();
            while let Ok(block) = pool.alloc(4) {
                held.push(block);
            }
            let available = 4096 - pool.committed();
            let holder = writer("ops", 1);

            let error = body(&pool, Handoff { to: Some(&holder) }).expect_err("full");

            assert_eq!(
                error,
                block::Error::Exhausted {
                    requested: 4,
                    available,
                }
            );
            assert_eq!(
                error.to_string(),
                format!("pool is full: asked for 4 bytes, {available} bytes free")
            );
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
                expected = "the handoff subject is not a name: \"a..b\" has \
                                       a segment that is not valid: \"\""
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
            let pool = pool(4096);

            let body = body(&pool, Handoff { to: holder.as_ref() }).expect("room");
            let bytes = body.as_deref().unwrap_or_default();

            prop_assert!(bytes.len() <= MAX_BYTES);
            prop_assert_eq!(read(bytes), holder);
        }
    }
}
