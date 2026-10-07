//! `wire::hub` never panics on a decode, what it reads encodes to the same bytes, and
//! each valid message it writes reads back.

#![no_main]

use libfuzzer_sys::{
    arbitrary::{self, Unstructured},
    fuzz_target,
};
use types::{
    channel,
    frame::{Path, Range},
};
use wire::hub::{Credit, Head, Mode, Open, Reply, ends, keys};

/// The most keys or ends of a written run.
const RUN_MAX: usize = 4;

/// Each message that `bytes` decodes to must encode to the same bytes.
fn read(bytes: &[u8]) {
    if let Ok(open) = Open::decode(bytes) {
        let mut out = vec![0; open.encoded_len()];
        open.encode(&mut out);
        assert_eq!(out, bytes, "the open changed");
    }
    if let Ok(credit) = Credit::decode(bytes) {
        let mut out = [0; Credit::LEN];
        credit.encode(&mut out);
        assert_eq!(out, bytes, "the credit changed");
    }
    if let Ok(reply) = Reply::decode(bytes) {
        let mut out = vec![0; reply.encoded_len()];
        reply.encode(&mut out);
        assert_eq!(out, bytes, "the reply changed");
    }
    if let Ok(decoded) = keys::decode(bytes) {
        let decoded: Vec<_> = decoded.collect();
        let mut out = vec![0; bytes.len()];
        keys::encode(&decoded, &mut out);
        assert_eq!(out, bytes, "the keys changed");
    }
    if let Ok(decoded) = ends::decode(bytes) {
        let mut out = vec![0; bytes.len()];
        ends::encode(decoded, &mut out);
        assert_eq!(out, bytes, "the ends changed");
    }
}

/// Each valid message made from `input` must decode to itself. A count of 0 is not
/// valid, so it becomes 1: an input that ends early writes the smallest messages.
fn write(input: &mut Unstructured) -> arbitrary::Result<()> {
    let limit_bytes = input.arbitrary()?;
    let channels = input.arbitrary::<u32>()?.max(1);
    for mode in [Mode::Latest, Mode::Complete { limit_bytes }] {
        let open = Open { mode, channels };
        let mut out = vec![0; open.encoded_len()];
        open.encode(&mut out);
        assert_eq!(Open::decode(&out), Ok(open), "an open did not read back");
    }

    let credit = Credit { limit_bytes };
    let mut out = [0; Credit::LEN];
    credit.encode(&mut out);
    assert_eq!(
        Credit::decode(&out),
        Ok(credit),
        "a credit did not read back"
    );

    let range = Range {
        seq: input.arbitrary()?,
        count: input.arbitrary()?,
    };
    let series = input.arbitrary::<u32>()?.max(1);
    let heads = [Path::Live, Path::Backfill].map(|path| {
        Reply::Head(Head {
            path,
            range,
            series,
        })
    });
    for reply in [Reply::Opened].into_iter().chain(heads) {
        let mut out = vec![0; reply.encoded_len()];
        reply.encode(&mut out);
        assert_eq!(Reply::decode(&out), Ok(reply), "a reply did not read back");
    }

    let count = input.int_in_range(1..=RUN_MAX)?;
    let written = (0..count)
        .map(|_| input.arbitrary().map(channel::Key::from_u128))
        .collect::<arbitrary::Result<Vec<_>>>()?;
    let mut out = vec![0; count * keys::LEN];
    keys::encode(&written, &mut out);
    let decoded = keys::decode(&out).map(Iterator::collect::<Vec<_>>);
    assert_eq!(decoded, Ok(written), "the keys did not read back");

    let count = input.int_in_range(1..=RUN_MAX)?;
    let written = (0..count)
        .map(|_| input.arbitrary::<(u32, u32)>())
        .collect::<arbitrary::Result<Vec<_>>>()?;
    let mut out = vec![0; count * ends::LEN];
    ends::encode(written.iter().copied(), &mut out);
    let decoded = ends::decode(&out).map(Iterator::collect::<Vec<_>>);
    assert_eq!(decoded, Ok(written), "the ends did not read back");
    Ok(())
}

fuzz_target!(|bytes: &[u8]| {
    read(bytes);
    write(&mut Unstructured::new(bytes)).expect("an input that ends gives zeros");
});
