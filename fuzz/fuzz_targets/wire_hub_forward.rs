//! `wire::hub::forward::Forward::decode` and `places::Run::decode` never panic, a
//! forward that decodes encodes to its bytes, a refused forward has the refusal that its
//! kind, its length, and its count give, each message of the run gives what a model of
//! the run gives, a message that decodes encodes to its bytes, and each valid forward
//! and run made from the input reads back in messages.
//!
//! Input: the channels of the open (`u32`), then the messages of one stream
//! (`fuzz::messages`). The first is decoded as a forward, and each other as the next
//! message of its run of places, until one is refused.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use std::collections::BTreeSet;

use libfuzzer_sys::{
    arbitrary::{self, Unstructured},
    fuzz_target,
};
use wire::hub::Error;
use wire::hub::forward::{Forward, places};

fn forward(message: &[u8]) -> Option<Forward> {
    let expected = match message {
        [] => Err(Error::Empty),
        &[kind, ..] if kind != 8 => Err(Error::Kind { kind }),
        _ if message.len() != Forward::LEN => Err(Error::Length { len: message.len() }),
        [_, 0, 0, 0, 0] => Err(Error::Unplaced),
        _ => Ok(()),
    };
    let decoded = Forward::decode(message);
    assert_eq!(decoded.map(|_| ()), expected, "{message:?}");
    let forward = decoded.ok()?;
    let mut out = [0; Forward::LEN];
    forward.encode(&mut out);
    assert_eq!(out.as_slice(), message, "the forward changed");
    Some(forward)
}

/// A model of the run: the places that remain and the place before.
struct Model {
    remain: u32,
    channels: u32,
    before: Option<u32>,
}

impl Model {
    fn decode(&mut self, message: &[u8]) -> Result<Vec<u32>, Error> {
        let (items, rest) = message.as_chunks::<{ places::LEN }>();
        if message.is_empty() {
            return Err(Error::Empty);
        }
        if !rest.is_empty() {
            return Err(Error::Length { len: message.len() });
        }
        let remain = u32::try_from(items.len())
            .ok()
            .and_then(|count| self.remain.checked_sub(count))
            .ok_or(Error::Run {
                items: items.len(),
                remain: self.remain,
            })?;
        let places: Vec<u32> = items.iter().copied().map(u32::from_le_bytes).collect();
        let mut before = self.before;
        for &place in &places {
            if place >= self.channels || before.is_some_and(|before| place <= before) {
                return Err(Error::Place { place });
            }
            before = Some(place);
        }
        self.remain = remain;
        self.before = before;
        Ok(places)
    }
}

fn read(bytes: &[u8]) {
    let Some((&channels, bytes)) = bytes.split_first_chunk::<4>() else {
        return;
    };
    let channels = u32::from_le_bytes(channels);
    let mut messages = fuzz::messages(bytes);
    let Some(forward) = messages.next().and_then(forward) else {
        return;
    };
    let mut run = places::Run::new(forward, channels);
    let mut model = Model {
        remain: forward.places,
        channels,
        before: None,
    };
    for message in messages {
        let expected = model.decode(message);
        let decoded = run.decode(message).map(|places| {
            let len = places.len();
            let places: Vec<u32> = places.collect();
            assert_eq!(places.len(), len, "the count of places changed");
            places
        });
        assert_eq!(decoded, expected, "{message:?} of {channels} channels");
        let Ok(decoded) = decoded else {
            return;
        };
        assert_eq!(run.ended(), model.remain == 0, "the end of the run changed");
        let mut out = vec![0; message.len()];
        places::encode(decoded, &mut out);
        assert_eq!(out, message, "the message changed");
    }
}

fn write(input: &mut Unstructured<'_>) -> arbitrary::Result<()> {
    let written: BTreeSet<u32> = input.arbitrary()?;
    let per = usize::from(input.arbitrary::<u8>()?).max(1);
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

    let written: Vec<u32> = written.into_iter().collect();
    let mut run = places::Run::new(forward, channels);
    let mut places = written.iter().copied();
    for chunk in written.chunks(per) {
        assert!(!run.ended(), "the run ended early");
        let mut out = vec![0; chunk.len() * places::LEN];
        places::encode(places.by_ref(), &mut out);
        let decoded = run.decode(&out).map(Iterator::collect::<Vec<_>>);
        assert_eq!(decoded.as_deref(), Ok(chunk), "the places changed");
    }
    assert!(run.ended(), "the run did not end");
    assert_eq!(places.next(), None, "a place was not written");
    Ok(())
}

fuzz_target!(|bytes: &[u8]| {
    read(bytes);
    write(&mut Unstructured::new(bytes)).expect("an input that ends gives zeros");
});
