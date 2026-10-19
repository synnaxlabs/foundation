//! `wire::hub::forward::Forward::decode` and `places::decode` never panic, a forward
//! that decodes encodes to its bytes, a refused forward has the refusal that its kind,
//! its length, and its count give, the places of a run decode when each is after the
//! one before it and below the channels, else the first that is not is refused, a run
//! that decodes encodes to its bytes, and each valid forward and run made from the
//! input reads back.
//!
//! Input: the channels of the open (`u32`), then the messages of one stream
//! (`fuzz::messages`). The first is decoded as a forward, and the rest, joined, as the
//! run of places.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use std::collections::BTreeSet;

use libfuzzer_sys::{
    arbitrary::{self, Unstructured},
    fuzz_target,
};
use wire::hub::Error;
use wire::hub::forward::{Forward, places};

fn forward(message: &[u8]) {
    let expected = match message {
        [] => Err(Error::Empty),
        &[kind, ..] if kind != 8 => Err(Error::Kind { kind }),
        _ if message.len() != Forward::LEN => Err(Error::Length { len: message.len() }),
        [_, 0, 0, 0, 0] => Err(Error::Unplaced),
        _ => Ok(()),
    };
    let decoded = Forward::decode(message);
    assert_eq!(decoded.map(|_| ()), expected, "{message:?}");
    if let Ok(forward) = decoded {
        let mut out = [0; Forward::LEN];
        forward.encode(&mut out);
        assert_eq!(out.as_slice(), message, "the forward changed");
    }
}

/// The first place of `places` that is not after the place before it or not below
/// `channels`.
fn first_bad(places: &[u32], channels: u32) -> Option<u32> {
    let mut before = None;
    places.iter().copied().find(|&place| {
        let bad = place >= channels || before.is_some_and(|before| place <= before);
        before = Some(place);
        bad
    })
}

fn run(run: &[u8], channels: u32) {
    let (items, rest) = run.as_chunks::<{ places::LEN }>();
    let expected = if run.is_empty() {
        Err(Error::Empty)
    } else if !rest.is_empty() {
        Err(Error::Length { len: run.len() })
    } else {
        let items: Vec<u32> = items.iter().copied().map(u32::from_le_bytes).collect();
        match first_bad(&items, channels) {
            Some(place) => Err(Error::Place { place }),
            None => Ok(items),
        }
    };
    let decoded = places::decode(run, channels).map(|places| {
        let len = places.len();
        let places: Vec<u32> = places.collect();
        assert_eq!(places.len(), len, "the count of places changed");
        places
    });
    assert_eq!(decoded, expected, "{run:?} of {channels} channels");
    if let Ok(decoded) = decoded {
        let mut out = vec![0; run.len()];
        places::encode(decoded, &mut out);
        assert_eq!(out, run, "the run changed");
    }
}

fn read(bytes: &[u8]) {
    let Some((&channels, bytes)) = bytes.split_first_chunk::<4>() else {
        return;
    };
    let mut messages = fuzz::messages(bytes);
    let Some(first) = messages.next() else {
        return;
    };
    forward(first);
    let rest = messages.collect::<Vec<_>>().concat();
    run(&rest, u32::from_le_bytes(channels));
}

fn write(input: &mut Unstructured<'_>) -> arbitrary::Result<()> {
    let written: BTreeSet<u32> = input.arbitrary()?;
    let Some(&last) = written.last() else {
        return Ok(());
    };
    let Some(channels) = last.checked_add(1) else {
        return Ok(());
    };
    let forward = Forward {
        places: u32::try_from(written.len()).expect("a set of u32 fits a u32"),
    };
    let mut out = [0; Forward::LEN];
    forward.encode(&mut out);
    assert_eq!(Forward::decode(&out), Ok(forward), "the forward changed");

    let mut out = vec![0; written.len() * places::LEN];
    places::encode(written.iter().copied(), &mut out);
    let decoded = places::decode(&out, channels).map(Iterator::collect::<Vec<_>>);
    assert_eq!(
        decoded,
        Ok(written.into_iter().collect()),
        "the places changed"
    );
    Ok(())
}

fuzz_target!(|bytes: &[u8]| {
    read(bytes);
    write(&mut Unstructured::new(bytes)).expect("an input that ends gives zeros");
});
