//! The named entries of an index. Their bodies hold the record of each named reader of
//! the index, open or held, from the entry's place in the index log, in up to
//! [`PARTS`] parts. Part `p` is the entry of tag `2 + p`, and the newest entry of a
//! part replaces the ones before it.

use std::num::NonZeroU8;
use std::str;

use buffer::{Buffer, Layout};
use delivery::{Position, Record};
use types::channel::Slot;
use types::frame::Path;
use types::hash;
use types::name::Name;
use types::time::{Span, Stamp};

use crate::state;

/// The most parts of the named records of one index.
const PARTS: u8 = 64;

/// The buffer tag of part 0.
const TAG: u8 = 2;

/// The version of the body that this build writes and reads.
const VERSION: u8 = 1;

/// The bytes of the body before the records: the version and the count.
const HEAD: usize = 1 + 4;

/// The bytes of a record apart from its subject and name.
const FIXED: usize = 1 + 1 + 8 + 1 + 8 + 8 + 1 + 8;

/// The most bytes of one record: the longest subject and name.
const RECORD_MAX: usize = FIXED + 2 * Name::MAX_BYTES;

/// The most bytes of the body of one part: one entry of any ring, in one block of any
/// pool.
const PART_MAX: usize = Layout::ENTRY_MAX_MIN;
const _: () = assert!(HEAD + 7 * RECORD_MAX <= PART_MAX, "7 records fit one part");

/// The most bytes of the records of the named readers of one index.
pub(crate) const LIMIT: usize = PARTS as usize * (PART_MAX - HEAD);

/// The bytes of the record of `reader` in a body.
fn record_len(reader: &delivery::named::Key) -> usize {
    FIXED + reader.subject.as_str().len() + reader.name.as_str().len()
}

/// The buffer tag of `part`.
fn tag(part: u8) -> NonZeroU8 {
    NonZeroU8::new(TAG + part).expect("invariant: a tag of a part is not 0")
}

/// No part has room for a new record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Full;

/// How far a [`write`](Parts::write) went: it appended each part before `until`
/// whose records changed, and none from `until` on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Appended {
    until: u8,
}

/// The named records of one index in the buffer: the part of each record, and what
/// the last entry of its part keeps of it.
#[derive(Debug, Default)]
pub(crate) struct Parts {
    /// The part of each record that the last entry of its part holds or that a write
    /// adds. Sorted by key, with each key once.
    keys: Vec<(delivery::named::Key, Placed)>,
}

/// The place of the record of one key.
#[derive(Clone, Copy, Debug)]
struct Placed {
    part: u8,
    state: Kept,
}

/// What the last entry of its part keeps of the record of one key. It changes only
/// when its part appends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kept {
    /// No entry of its part holds it yet.
    Added,
    /// The last entry of its part holds it, with these fields.
    Held(Fields),
}

/// The fields of a record apart from its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fields {
    position: Position,
    hold: Span,
    closed: Option<Stamp>,
}

impl From<&Record> for Fields {
    fn from(record: &Record) -> Self {
        Self {
            position: record.position,
            hold: record.hold,
            closed: record.closed,
        }
    }
}

impl Parts {
    /// The parts that the newest durable named entries of each index in `buffer` hold,
    /// with the records of each, by the index's slot. An index with no named entry is
    /// absent. It reads the tags of the parts from part 0 up, to the first tag that no
    /// index has an entry of.
    ///
    /// # Panics
    ///
    /// When a named entry does not decode, as [`read`] says, or a key is in two parts.
    pub(crate) fn newest(buffer: &Buffer) -> hash::Map<Slot, (Self, Vec<Record>)> {
        let mut found: hash::Map<Slot, (Self, Vec<Record>)> = hash::Map::default();
        for part in 0..PARTS {
            let newest = buffer.newest(Path::Live, tag(part));
            if newest.is_empty() {
                break;
            }
            for (slot, bytes) in newest {
                let (parts, records) = found.entry(slot).or_default();
                for record in read(&bytes) {
                    let state = Kept::Held(Fields::from(&record));
                    parts
                        .keys
                        .push((record.reader.clone(), Placed { part, state }));
                    records.push(record);
                }
                parts.sort();
            }
        }
        found
    }

    /// Appends the parts that `records` change, as [`append`](Self::append) does,
    /// for a named open of `reader`, whose record `records` hold. Returns the count of
    /// [`Buffer::commits`] at which the entry that holds its record is durable, or
    /// `None` when the part of its record, or a part before it, found no block or no
    /// room.
    ///
    /// # Errors
    ///
    /// [`Full`] when no part has room for a new record of `records`. It then appends
    /// nothing.
    ///
    /// # Panics
    ///
    /// If `records` hold a key twice.
    pub(crate) fn open(
        &mut self,
        buffer: &Buffer,
        records: &[Record],
        reader: &delivery::named::Key,
        head: state::Head,
    ) -> Result<Option<u64>, Full> {
        let appended = self.write(buffer, records, head)?;
        let recorded = self.recorded(reader, appended);
        Ok(recorded.then(|| buffer.durable_at()))
    }

    /// Appends each part whose records differ from the ones that its last entry holds,
    /// so that the parts hold `records` and no other record, with `head`. Each part
    /// goes alone, in part order, and the append stops at the first part that finds no
    /// block or no room. A record keeps its part. A removed record keeps its room until
    /// the entry of its part that drops it appends, and goes back in its part when it
    /// returns before that. Returns whether it appended each part whose records
    /// changed.
    ///
    /// # Panics
    ///
    /// If `records` hold a key twice, or a new record: only an
    /// [`open`](Self::open) adds one.
    pub(crate) fn append(
        &mut self,
        buffer: &Buffer,
        records: &[Record],
        head: state::Head,
    ) -> bool {
        let appended = self.write(buffer, records, head);
        let appended = appended.expect("invariant: only an open adds a record");
        appended.until == PARTS
    }

    /// Appends the parts as [`append`](Self::append) does, and places each new record
    /// of `records` in the lowest part with room for it.
    ///
    /// # Errors
    ///
    /// [`Full`] when no part has room for a new record. It then appends nothing.
    fn write(
        &mut self,
        buffer: &Buffer,
        records: &[Record],
        head: state::Head,
    ) -> Result<Appended, Full> {
        self.place(records)?;
        let listed = Listed::new(records);
        for (part, body) in self.changed(records, &listed) {
            if !state::append(buffer, tag(part), Some(&body), head) {
                self.keys.retain(|(_, placed)| placed.state != Kept::Added);
                return Ok(Appended { until: part });
            }
            self.appended(part, &listed);
        }
        Ok(Appended { until: PARTS })
    }

    /// Places each new record of `records` in a part.
    ///
    /// # Errors
    ///
    /// [`Full`] when no part has room for a new record. It then adds no key.
    ///
    /// # Panics
    ///
    /// If `records` hold a key twice.
    fn place(&mut self, records: &[Record]) -> Result<(), Full> {
        let mut used = [HEAD; PARTS as usize];
        for (key, placed) in &self.keys {
            used[usize::from(placed.part)] += record_len(key);
        }
        let mut added = Vec::new();
        for key in records.iter().map(|record| &record.reader) {
            if self.placed(key).is_some() {
                continue;
            }
            let len = record_len(key);
            let room = |part: &u8| used[usize::from(*part)] + len <= PART_MAX;
            let part = (0..PARTS).find(room).ok_or(Full)?;
            used[usize::from(part)] += len;
            let state = Kept::Added;
            added.push((key.clone(), Placed { part, state }));
        }
        self.keys.append(&mut added);
        self.sort();
        Ok(())
    }

    /// Sorts `keys` by key. Linear when they are two sorted runs.
    ///
    /// # Panics
    ///
    /// If a key is in `keys` twice.
    fn sort(&mut self) {
        self.keys.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some([(key, a), (_, b)]) =
            self.keys.array_windows().find(|[a, b]| a.0 == b.0)
        {
            panic!(
                "the named reader {key:?} is twice in parts, {} and {}",
                a.part, b.part
            );
        }
    }

    /// The body of each part, in part order, whose records differ from the ones that
    /// its last entry holds, when the parts hold `records`, which
    /// [`place`](Self::place) placed. `listed` holds `records` by key.
    fn changed(&self, records: &[Record], listed: &Listed<'_>) -> Vec<(u8, Vec<u8>)> {
        let mut changed = [false; PARTS as usize];
        for (key, placed) in &self.keys {
            let now = listed
                .get(key)
                .map(|record| Kept::Held(Fields::from(record)));
            if now.is_none_or(|now| now != placed.state) {
                changed[usize::from(placed.part)] = true;
            }
        }
        let mut sorted: Vec<(u8, &Record)> = records
            .iter()
            .map(|record| (self.part(&record.reader), record))
            .filter(|&(part, _)| changed[usize::from(part)])
            .collect();
        sorted.sort_unstable_by(|a, b| (a.0, &a.1.reader).cmp(&(b.0, &b.1.reader)));
        let mut rest = &sorted[..];
        let mut bodies = Vec::new();
        for part in (0..PARTS).filter(|&part| changed[usize::from(part)]) {
            let (now, after) =
                rest.split_at(rest.partition_point(|&(at, _)| at == part));
            rest = after;
            let mut body = vec![0; len(now.iter().map(|&(_, record)| record))];
            write(&mut body, now.iter().map(|&(_, record)| record));
            bodies.push((part, body));
        }
        bodies
    }

    /// Notes that `part` appended an entry that holds the records of `listed` in
    /// `part`. A key of `part` that `listed` lacks goes, and its room is free: a
    /// restart reads a prefix of the appends, so it never reads an add of that key in
    /// another part without this entry.
    fn appended(&mut self, part: u8, listed: &Listed<'_>) {
        self.keys.retain_mut(|(key, placed)| {
            if placed.part != part {
                return true;
            }
            let record = listed.get(key);
            if let Some(record) = record {
                placed.state = Kept::Held(Fields::from(record));
            }
            record.is_some()
        });
    }

    /// Whether the record of `reader` is in a part before the first that `appended`
    /// did not append.
    fn recorded(&self, reader: &delivery::named::Key, appended: Appended) -> bool {
        let placed = self.placed(reader);
        placed.is_some_and(|placed| placed.part < appended.until)
    }

    /// Whether a part holds a record.
    pub(crate) fn held(&self) -> bool {
        !self.keys.is_empty()
    }

    /// The place of the record of `key`, or `None` when no part holds room for it.
    fn placed(&self, key: &delivery::named::Key) -> Option<&Placed> {
        let at = self
            .keys
            .binary_search_by(|(other, _)| other.cmp(key))
            .ok()?;
        Some(&self.keys[at].1)
    }

    /// The part of the record of `key`.
    ///
    /// # Panics
    ///
    /// If no part holds room for it.
    fn part(&self, key: &delivery::named::Key) -> u8 {
        let placed = self.placed(key);
        placed.expect("invariant: each record has a part").part
    }
}

/// Records sorted by key, for a lookup by binary search.
struct Listed<'a>(Vec<&'a Record>);

impl<'a> Listed<'a> {
    fn new(records: &'a [Record]) -> Self {
        let mut listed: Vec<_> = records.iter().collect();
        listed.sort_unstable_by(|a, b| a.reader.cmp(&b.reader));
        Self(listed)
    }

    /// The record of `key`, if any.
    fn get(&self, key: &delivery::named::Key) -> Option<&'a Record> {
        let at = self.0.binary_search_by(|record| record.reader.cmp(key));
        at.ok().map(|at| self.0[at])
    }
}

/// The bytes of the body that holds `records`.
fn len<'a>(records: impl IntoIterator<Item = &'a Record>) -> usize {
    let records = records.into_iter().map(|record| record_len(&record.reader));
    records.sum::<usize>() + HEAD
}

/// Writes `records` to `out`, which has [`len`] bytes.
fn write<'a>(out: &mut [u8], records: impl ExactSizeIterator<Item = &'a Record>) {
    let count = u32::try_from(records.len()).expect("at most u32::MAX records");
    let mut at = 0;
    let mut put = |bytes: &[u8]| {
        out[at..at + bytes.len()].copy_from_slice(bytes);
        at += bytes.len();
    };
    put(&[VERSION]);
    put(&count.to_le_bytes());
    for record in records {
        for name in [&record.reader.subject, &record.reader.name] {
            let name = name.as_str().as_bytes();
            put(&[u8::try_from(name.len()).expect("a name fits 255 bytes")]);
            put(name);
        }
        put(&record.position.live.to_le_bytes());
        put(&[u8::from(record.position.backfill.is_some())]);
        put(&record.position.backfill.unwrap_or(0).to_le_bytes());
        put(&record.hold.nanos().to_le_bytes());
        put(&[u8::from(record.closed.is_some())]);
        put(&record.closed.map_or(0, Stamp::nanos).to_le_bytes());
    }
}

/// The records that `body`, the bytes of a named entry, holds.
///
/// # Panics
///
/// If the version is not 1, or the bytes are not a body of that version. Bytes from
/// another node must be checked before they reach `read`.
fn read(body: &[u8]) -> Vec<Record> {
    let mut rest = body;
    let version = take::<1>(&mut rest)[0];
    assert!(
        version == VERSION,
        "the named entry has version {version}; this build reads version {VERSION}"
    );
    let count = u32::from_le_bytes(take(&mut rest));
    let count = usize::try_from(count).expect("invariant: a usize holds a u32");
    // A count from disk sizes the vector only when the bytes can hold it.
    assert!(
        count <= rest.len() / FIXED,
        "the named entry ends inside a record"
    );
    let records = (0..count)
        .map(|_| {
            let subject = name(&mut rest);
            let name = name(&mut rest);
            let live = u64::from_le_bytes(take(&mut rest));
            let backfill = flagged(&mut rest).map(u64::from_le_bytes);
            let hold = Span::from_nanos(i64::from_le_bytes(take(&mut rest)));
            let closed = flagged(&mut rest)
                .map(|bytes| Stamp::from_nanos(i64::from_le_bytes(bytes)));
            Record {
                reader: delivery::named::Key { subject, name },
                position: Position { live, backfill },
                hold,
                closed,
            }
        })
        .collect();
    assert!(
        rest.is_empty(),
        "the named entry has {} bytes past its records",
        rest.len()
    );
    records
}

/// The next `N` bytes of `rest`.
///
/// # Panics
///
/// If `rest` holds fewer than `N` bytes.
fn take<const N: usize>(rest: &mut &[u8]) -> [u8; N] {
    let Some((bytes, after)) = rest.split_first_chunk() else {
        panic!("the named entry ends inside a record");
    };
    *rest = after;
    *bytes
}

/// The next value of `rest` after its flag: `None` for a flag of 0.
///
/// # Panics
///
/// If the flag is not 0 or 1, or `rest` ends first.
fn flagged(rest: &mut &[u8]) -> Option<[u8; 8]> {
    let flag = take::<1>(rest)[0];
    let value = take(rest);
    match flag {
        0 => None,
        1 => Some(value),
        flag => panic!("the named entry has the flag {flag}"),
    }
}

/// The next name of `rest`, after its length.
///
/// # Panics
///
/// If `rest` ends first, or the bytes are not a name.
fn name(rest: &mut &[u8]) -> Name {
    let len = usize::from(take::<1>(rest)[0]);
    let Some((bytes, after)) = rest.split_at_checked(len) else {
        panic!("the named entry ends inside a record");
    };
    *rest = after;
    str::from_utf8(bytes)
        .unwrap_or_else(|error| panic!("a named entry name is not UTF-8: {error}"))
        .parse()
        .unwrap_or_else(|error| panic!("a named entry name is not a name: {error}"))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn record(
        subject: &str,
        name: &str,
        backfill: Option<u64>,
        closed: Option<i64>,
    ) -> Record {
        Record {
            reader: delivery::named::Key {
                subject: subject.parse().expect("a valid name"),
                name: name.parse().expect("a valid name"),
            },
            position: Position {
                live: 0x0102,
                backfill,
            },
            hold: Span::from_nanos(0x0304),
            closed: closed.map(Stamp::from_nanos),
        }
    }

    fn encoded(records: &[Record]) -> Vec<u8> {
        let mut body = vec![0; len(records)];
        write(&mut body, records.iter());
        body
    }

    #[test]
    fn gives_part_p_the_tag_2_plus_p() {
        assert_eq!((tag(0).get(), tag(PARTS - 1).get()), (2, 65));
    }

    mod write {
        use super::*;

        #[test]
        fn writes_the_version_the_count_then_each_record() {
            let bytes = encoded(&[
                record("ops", "r", Some(5), Some(7)),
                record("a", "b", None, None),
            ]);

            let mut expected = vec![1, 2, 0, 0, 0];
            expected.extend(b"\x03ops\x01r");
            expected.extend([2, 1, 0, 0, 0, 0, 0, 0]);
            expected.extend([1, 5, 0, 0, 0, 0, 0, 0, 0]);
            expected.extend([4, 3, 0, 0, 0, 0, 0, 0]);
            expected.extend([1, 7, 0, 0, 0, 0, 0, 0, 0]);
            expected.extend(b"\x01a\x01b");
            expected.extend([2, 1, 0, 0, 0, 0, 0, 0]);
            expected.extend([0; 9]);
            expected.extend([4, 3, 0, 0, 0, 0, 0, 0]);
            expected.extend([0; 9]);
            assert_eq!(bytes, expected);
        }

        /// The only pin of `RECORD_MAX`: just a compile-time assert reads it.
        #[test]
        fn writes_the_largest_record_for_the_longest_names() {
            let longest = "b".repeat(Name::MAX_BYTES);
            let records = [record(&longest, &longest, Some(1), Some(1))];

            assert_eq!(encoded(&records).len(), HEAD + RECORD_MAX);
            assert_eq!((RECORD_MAX, len(&records), LIMIT), (546, 551, 257_728));
        }
    }

    /// A record of the longest subject and name, with a subject that starts with `n`.
    fn longest(n: usize) -> Record {
        let n = n.to_string();
        let subject = format!("{n}{}", "s".repeat(Name::MAX_BYTES - n.len()));
        record(&subject, &"r".repeat(Name::MAX_BYTES), None, None)
    }

    fn part(parts: &Parts, record: &Record) -> u8 {
        parts.part(&record.reader)
    }

    /// The records of `records` by key.
    fn listed(records: &[Record]) -> Listed<'_> {
        Listed::new(records)
    }

    /// The bodies of the parts that `records` change.
    fn changed(parts: &Parts, records: &[Record]) -> Vec<(u8, Vec<u8>)> {
        parts.changed(records, &listed(records))
    }

    /// The bytes of the records that each part keeps room for, with the head.
    fn used(parts: &Parts) -> Vec<usize> {
        let mut used = vec![HEAD; usize::from(PARTS)];
        for (key, placed) in &parts.keys {
            used[usize::from(placed.part)] += record_len(key);
        }
        used
    }

    /// Places `records`, and appends each part that changes.
    fn append(parts: &mut Parts, records: &[Record]) {
        parts.place(records).expect("room");
        for (part, _) in changed(parts, records) {
            parts.appended(part, &listed(records));
        }
    }

    /// These tests call the private `place`, `changed`, and `appended` with no
    /// buffer: they pin first fit and the key states over hundreds of records, which
    /// a shard test cannot set up cheaply. The `shard` tests of `records` pin each
    /// state through `open` and `append`. Only these tests pin the room that `place`
    /// counts for a second record that one call adds, as a shard call adds at most
    /// one. `shard` checks `held` through its own map, as only memory shows it.
    mod parts {
        use super::*;

        #[test]
        fn places_seven_of_the_longest_records_in_each_part() {
            let mut records: Vec<_> = (0..448).map(longest).collect();
            let mut parts = Parts::default();

            parts.place(&records).expect("room");
            assert_eq!(parts.keys.len(), 448);

            for (n, record) in records.iter().enumerate() {
                assert_eq!(usize::from(part(&parts, record)), n / 7, "record {n}");
            }
            records.push(longest(448));
            assert_eq!(parts.place(&records), Err(Full));
            assert!(parts.placed(&longest(448).reader).is_none());
        }

        #[test]
        fn adds_no_key_when_a_later_record_has_no_room() {
            let mut records: Vec<_> = (0..448).map(longest).collect();
            let mut parts = Parts::default();
            parts.place(&records).expect("room");
            let short = record("a", "b", None, None);
            records.extend([short.clone(), longest(448)]);

            assert_eq!(parts.place(&records), Err(Full));

            assert!(parts.placed(&short.reader).is_none());
            assert_eq!(parts.keys.len(), 448);
        }

        #[test]
        #[should_panic(expected = "is twice in parts, 0 and 0")]
        fn panics_on_records_that_hold_a_key_twice() {
            let records = [longest(0), longest(0)];
            Parts::default().place(&records).expect("room");
        }

        #[test]
        fn frees_the_room_of_a_removed_record_when_its_part_appends() {
            let records: Vec<_> = (0..7).map(longest).collect();
            let mut parts = Parts::default();
            append(&mut parts, &records);
            append(&mut parts, &records[1..]);
            let mut added = records[1..].to_vec();
            added.push(longest(7));

            parts.place(&added).expect("room");

            assert_eq!(part(&parts, &longest(7)), 0);
            assert!(parts.placed(&longest(0).reader).is_none());
        }

        #[test]
        fn keeps_the_room_of_a_record_whose_removal_found_no_append() {
            let records: Vec<_> = (0..7).map(longest).collect();
            let mut parts = Parts::default();
            append(&mut parts, &records);
            parts.place(&records[1..]).expect("room");
            let mut added = records[1..].to_vec();
            added.push(longest(7));

            parts.place(&added).expect("room");

            assert_eq!(part(&parts, &longest(7)), 1);
        }

        #[test]
        fn changes_only_the_parts_whose_records_change() {
            let mut records: Vec<_> = (0..14).map(longest).collect();
            let mut parts = Parts::default();
            parts.place(&records).expect("room");
            let changed_now = changed(&parts, &records);
            let parts_of: Vec<_> = changed_now.iter().map(|(part, _)| *part).collect();
            assert_eq!(parts_of, [0, 1]);
            for (part, _) in changed_now {
                parts.appended(part, &listed(&records));
            }
            records[8].position.live = 9;

            parts.place(&records).expect("room");
            let changed = changed(&parts, &records);

            let mut expected = records[7..].to_vec();
            expected.sort_by(|a, b| a.reader.cmp(&b.reader));
            let read: Vec<_> = changed
                .iter()
                .map(|(part, body)| (*part, read(body)))
                .collect();
            assert_eq!(read, [(1, expected)]);
        }

        #[test]
        fn empties_a_part_whose_records_all_go() {
            let records: Vec<_> = (0..8).map(longest).collect();
            let mut parts = Parts::default();
            append(&mut parts, &records);
            parts.place(&records[..7]).expect("room");

            assert_eq!(changed(&parts, &records[..7]), [(1, encoded(&[]))]);

            append(&mut parts, &records[..7]);
            parts.place(&records[..7]).expect("room");
            assert_eq!(changed(&parts, &records[..7]), []);
            assert_eq!(parts.keys.len(), 7, "7 went when its part appended");
        }

        #[test]
        fn appends_no_part_that_never_held_a_record() {
            let mut parts = Parts::default();
            parts.place(&[]).expect("room");
            assert_eq!(changed(&parts, &[]), []);
        }

        #[test]
        fn records_a_reader_in_a_part_before_the_first_unappended() {
            let records: Vec<_> = (0..8).map(longest).collect();
            let mut parts = Parts::default();
            parts.place(&records).expect("room");
            let appended = Appended { until: 1 };

            assert!(parts.recorded(&records[6].reader, appended));
            assert!(!parts.recorded(&records[7].reader, appended));
            assert!(!parts.recorded(&longest(8).reader, appended));
        }

        #[test]
        fn says_whether_a_part_holds_a_record() {
            let mut parts = Parts::default();
            append(&mut parts, &[longest(0)]);
            assert!(parts.held());

            append(&mut parts, &[]);

            assert!(!parts.held());
        }
    }

    mod read {
        use super::*;

        #[test]
        fn reads_no_record_from_an_empty_body() {
            assert_eq!(read(&encoded(&[])), []);
        }

        mod when_damaged {
            use super::*;

            #[test]
            #[should_panic(
                expected = "the named entry has version 2; this build reads version 1"
            )]
            fn panics_on_another_version() {
                read(&[2, 0, 0, 0, 0]);
            }

            #[test]
            #[should_panic(expected = "the named entry ends inside a record")]
            fn panics_on_a_body_that_ends_inside_a_record() {
                let bytes = encoded(&[record("ops", "r", None, None)]);
                read(&bytes[..bytes.len() - 1]);
            }

            #[test]
            #[should_panic(expected = "the named entry ends inside a record")]
            fn panics_on_a_name_past_the_end() {
                let mut bytes = vec![1, 1, 0, 0, 0, 100];
                bytes.extend([b'o'; 40]);
                read(&bytes);
            }

            #[test]
            #[should_panic(expected = "the named entry ends inside a record")]
            fn panics_on_a_count_past_the_end_with_no_allocation() {
                read(&[1, 0xff, 0xff, 0xff, 0xff]);
            }

            #[test]
            #[should_panic(expected = "the named entry has 1 bytes past its records")]
            fn panics_on_bytes_past_the_records() {
                read(&[1, 0, 0, 0, 0, 9]);
            }

            #[test]
            #[should_panic(expected = "the named entry has the flag 2")]
            fn panics_on_a_flag_that_is_not_0_or_1() {
                let mut bytes = encoded(&[record("ops", "r", None, None)]);
                bytes[HEAD + 6 + 8] = 2;
                read(&bytes);
            }

            #[test]
            #[should_panic(
                expected = "a named entry name is not UTF-8: invalid utf-8 \
                                       sequence of 1 bytes from index 0"
            )]
            fn panics_on_a_name_that_is_not_utf8() {
                let mut bytes = vec![1, 1, 0, 0, 0, 1, 0xff];
                bytes.extend([0; 40]);
                read(&bytes);
            }

            #[test]
            #[should_panic(expected = "a named entry name is not a name: a name or \
                                       pattern is empty")]
            fn panics_on_an_empty_name() {
                let mut bytes = vec![1, 1, 0, 0, 0, 0];
                bytes.extend([0; 40]);
                read(&bytes);
            }
        }
    }

    fn any_name() -> impl Strategy<Value = String> {
        "@?[A-Za-z0-9_-]{1,63}(\\.@?[A-Za-z0-9_-]{1,63}){0,3}"
            .prop_filter("a name", |name| name.len() <= Name::MAX_BYTES)
    }

    proptest! {
        #[test]
        fn reads_back_each_body(
            records in prop::collection::vec(
                (
                    any_name(),
                    any_name(),
                    any::<u64>(),
                    any::<Option<u64>>(),
                    any::<i64>(),
                    any::<Option<i64>>(),
                ),
                0..8,
            )
        ) {
            let records: Vec<_> = records
                .into_iter()
                .map(|(subject, name, live, backfill, hold, closed)| Record {
                    reader: delivery::named::Key {
                        subject: subject.parse().expect("a valid name"),
                        name: name.parse().expect("a valid name"),
                    },
                    position: Position { live, backfill },
                    hold: Span::from_nanos(hold),
                    closed: closed.map(Stamp::from_nanos),
                })
                .collect();

            let bytes = encoded(&records);

            prop_assert_eq!(bytes.len(), len(&records));
            prop_assert_eq!(read(&bytes), records);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn places_each_new_record_in_the_lowest_part_with_room(
            lens in prop::collection::vec(
                (4..=Name::MAX_BYTES, 1..=Name::MAX_BYTES),
                1..1200,
            )
        ) {
            let mut parts = Parts::default();
            let mut records = Vec::new();
            let mut full = 0;
            for (n, (subject, name)) in lens.into_iter().enumerate() {
                let subject = format!("{n:04}{}", "s".repeat(subject - 4));
                let new = record(&subject, &"r".repeat(name), None, None);
                let len = record_len(&new.reader);
                let used = used(&parts);
                let before: Vec<_> =
                    records.iter().map(|record| part(&parts, record)).collect();
                records.push(new.clone());
                let keys = parts.keys.len();
                if parts.place(&records).is_ok() {
                    prop_assert_eq!(parts.keys.len(), keys + 1);
                    let at = usize::from(part(&parts, &new));
                    prop_assert!(used[at] + len <= PART_MAX);
                    let before = &used[..at];
                    prop_assert!(before.iter().all(|&used| used + len > PART_MAX));
                } else {
                    full += 1;
                    prop_assert!(used.iter().all(|&used| used + len > PART_MAX));
                    prop_assert!(parts.placed(&new.reader).is_none());
                    records.pop();
                }
                let after: Vec<_> = records[..before.len()]
                    .iter()
                    .map(|record| part(&parts, record))
                    .collect();
                prop_assert_eq!(after, before, "each record keeps its part");
            }
            prop_assert!(full == 0 || records.len() >= 448);
        }
    }
}
