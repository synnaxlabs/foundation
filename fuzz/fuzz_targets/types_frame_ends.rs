//! `Layout::from_ends` never panics on the ends of another node, it refuses exactly
//! the ends that break one of its rules, and a frame drafted from the others has those
//! ends. `frame::check` refuses exactly the ends that do not fit a body.

#![no_main]

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use types::{
    channel,
    frame::{
        self, BadEnd, Error, Form, Layout, Path,
        key_set::{Group, Interner, KeySet},
    },
    sample::{Scalar, Type},
};

/// The bytes of one end of an input: a `u32` entry, then a `u64` end.
const END: usize = 12;

/// The largest block that the target drafts, and the largest body that it checks.
const DRAFT_MAX: usize = 1 << 16;

/// The groups of the wide key set: more than the 16 whose alternating data a layout
/// checks in linear time.
const GROUPS: u32 = 18;

const F64: Type = Type::Scalar(Scalar::F64);

fn key(n: u32) -> channel::Key {
    channel::Key::from_u128(u128::from(n))
}

/// One group: the index at entry 0, then two data channels.
fn narrow(interner: &mut Interner) -> Arc<KeySet> {
    interner.intern(&[Group {
        index: key(0),
        data: &[(key(1), F64), (key(2), Type::Scalar(Scalar::U8))],
    }])
}

/// The group of `narrow` with its first data channel at entry 0, before its index.
fn late(interner: &mut Interner) -> Arc<KeySet> {
    interner.slots().assign(key(1));
    narrow(interner)
}

/// `GROUPS` groups of two data channels. The indexes are the first entries. The first
/// data channel of each group follows, then the second of each, so the data of the
/// groups alternate.
fn wide(interner: &mut Interner) -> Arc<KeySet> {
    for n in 0..3 * GROUPS {
        interner.slots().assign(key(n));
    }
    let data: Vec<_> = (0..GROUPS)
        .map(|group| [(key(GROUPS + group), F64), (key(2 * GROUPS + group), F64)])
        .collect();
    let groups: Vec<_> = (0..GROUPS)
        .zip(&data)
        .map(|(group, data)| Group {
            index: key(group),
            data,
        })
        .collect();
    interner.intern(&groups)
}

/// Where a series starts when the series bytes before it end at `last`.
fn start(last: usize) -> usize {
    last.checked_next_multiple_of(8).unwrap_or(usize::MAX)
}

/// Each rule of `Layout::from_ends` that `ends` break, as the error that names it. Of
/// the ends below the start of their series, it gives only the first.
fn broken(set: &KeySet, ends: &[(usize, usize)]) -> Vec<Error> {
    let entries = set.entries().len();
    let present = |entry| ends.iter().any(|&(other, _)| other == entry);
    let (mut broken, mut short) = (Vec::new(), false);
    let (mut last, mut bytes) = (None, 0);
    for &(entry, end) in ends {
        if entry >= entries {
            broken.push(Error::OutOfRange { entry, entries });
        } else {
            let index = set.index(entry);
            if !present(index) {
                broken.push(Error::IndexAbsent { entry, index });
            }
        }
        if let Some(last) = last
            && entry <= last
        {
            broken.push(Error::Unordered { entry, last });
        }
        let start = start(bytes);
        if end < start && !short {
            broken.push(Error::End(BadEnd::Before { end, start }));
            short = true;
        }
        (last, bytes) = (Some(entry), end);
    }
    broken
}

/// Each rule of `frame::check` that the first bad end of `ends` breaks in a body of
/// `len` bytes.
fn unfit(len: usize, ends: &[(usize, usize)]) -> Vec<BadEnd> {
    let mut last = 0;
    for &(_, end) in ends {
        let (start, mut unfit) = (start(last), Vec::new());
        if end > len {
            unfit.push(BadEnd::Past { end, len });
        }
        if end < start {
            unfit.push(BadEnd::Before { end, start });
        }
        if !unfit.is_empty() {
            return unfit;
        }
        last = end;
    }
    if last == len {
        Vec::new()
    } else {
        vec![BadEnd::Short { last, len }]
    }
}

/// Bytes that differ between near places, so that a series cut at a wrong place
/// differs.
fn body(len: usize) -> Vec<u8> {
    (0..=250).cycle().take(len).collect()
}

/// `frame::check` must refuse a body of `len` bytes exactly when `ends` do not fit
/// it, and `split` must cut a body that fits at them.
fn fit(len: usize, ends: &[(usize, usize)]) {
    let (body, unfit) = (body(len), unfit(len, ends));
    if let Err(error) = frame::check(&body, ends.iter().copied()) {
        assert!(unfit.contains(&error), "{error:?} is not in {unfit:?}");
        return;
    }
    assert_eq!(unfit, [], "the check took ends that do not fit");
    let mut last = 0;
    let cut = ends.iter().map(|&(entry, end)| {
        let series = &body[start(last)..end];
        last = end;
        (entry, series)
    });
    let split = frame::split(&body, ends.iter().copied());
    assert!(split.eq(cut), "the split gave other series");
}

/// A frame drafted from `layout`, the layout of `ends`, must have them, and its series
/// must be the ones that `ends` cut from its body.
fn draft(set: &KeySet, layout: Layout, ends: &[(usize, usize)]) {
    let body_len = layout.body_len();
    let config = block::Config { budget: 1 << 21 };
    let pool = block::Pool::new(config.clone(), block::Heap::new(config.reservation()));
    let mut draft = layout
        .draft(&pool, Form::Raw)
        .expect("the pool holds the block");
    let drafted = draft.body_mut();
    assert_eq!(drafted.len(), body_len, "the draft has another body");
    drafted.copy_from_slice(&body(body_len));
    let frame = draft.freeze(Path::Live);
    let ends = || ends.iter().copied();
    assert!(frame.ends().eq(ends()), "the frame has other ends");
    let body = frame.body();
    assert_eq!(frame::check(&body, ends()), Ok(()), "the ends do not fit");
    let series = ends().map(|(entry, _)| Some((entry, frame.series(entry)?)));
    let split = frame::split(&body, ends()).map(Some);
    assert!(split.eq(series), "the split gave other series");
    let indexes = ends().filter(|(entry, _)| set.groups().contains(entry));
    if indexes.count() == 1 {
        let charge = frame::charge(ends().count(), body_len);
        assert_eq!(frame.charge(), charge, "the frame has another charge");
    }
}

fuzz_target!(|bytes: &[u8]| {
    let Some((&selector, bytes)) = bytes.split_first() else {
        return;
    };
    let mut interner = Interner::new();
    let set = match selector % 3 {
        0 => narrow(&mut interner),
        1 => wide(&mut interner),
        _ => late(&mut interner),
    };
    let (ends, rest) = bytes.as_chunks::<END>();
    let ends: Vec<_> = ends
        .iter()
        .map(|&[e0, e1, e2, e3, end @ ..]| {
            let entry = u32::from_le_bytes([e0, e1, e2, e3]);
            let end = u64::from_le_bytes(end);
            (
                usize::try_from(entry).unwrap_or(usize::MAX),
                usize::try_from(end).unwrap_or(usize::MAX),
            )
        })
        .collect();
    // The bytes after the last whole end give the length of the body to check.
    let len = rest
        .iter()
        .take(3)
        .rev()
        .fold(0, |len, &byte| len << 8 | usize::from(byte));
    fit(len.min(DRAFT_MAX), &ends);

    let broken = broken(&set, &ends);
    let layout = match Layout::from_ends(&set, &ends) {
        Ok(layout) => layout,
        Err(error) => {
            assert!(broken.contains(&error), "{error:?} is not in {broken:?}");
            return;
        }
    };
    assert_eq!(broken, [], "a layout took ends that break a rule");

    let mut last = 0;
    let lens: Vec<_> = ends
        .iter()
        .map(|&(entry, end)| {
            let len = end - start(last);
            last = end;
            (entry, len)
        })
        .collect();
    let sizes = |layout: &Layout| (layout.body_len(), layout.block_len());
    assert_eq!(
        Layout::new(&set, &lens).as_ref().map(sizes),
        Ok(sizes(&layout)),
        "the ends and their lengths differ"
    );
    assert!(
        frame::ends(lens.iter().copied()).eq(ends.iter().copied()),
        "the lengths give other ends"
    );
    if layout.block_len() <= DRAFT_MAX {
        draft(&set, layout, &ends);
    }
});
