//! Entry points for the crate's benchmark and allocation test, behind the `bench`
//! feature. Not a contract: the public surface of the crate replaces it.

use crate::crc32c::Crc32c;
use crate::record::HEADER_LEN;
use crate::wal::{Cursor, Layout, Position, Step, Window, Writer};

/// The CRC32C of `bytes`.
#[must_use]
pub fn crc(bytes: &[u8]) -> u32 {
    let mut crc = Crc32c::new();
    crc.update(bytes);
    crc.finish()
}

/// A ring in memory with its writer, after the restart record of its first open.
#[derive(Debug)]
pub struct Ring {
    layout: Layout,
    writer: Writer,
    tail: Position,
    area: Vec<u8>,
    records: u64,
}

impl Ring {
    const START: u32 = 0x5EED_0001;
    const CHAIN: u32 = 7;

    /// An empty ring of `area` bytes for bodies of at most `body_max` bytes.
    ///
    /// # Panics
    ///
    /// When the sizes do not make a ring.
    #[must_use]
    pub fn new(area: u64, body_max: usize) -> Self {
        let layout = Layout::new(area, body_max).expect("the sizes make a ring");
        let start = Position::new(0, Self::START).expect("offset 0 is aligned");
        let mut cursor = Cursor::new(layout, start);
        let Window { len, .. } = cursor.window();
        let zeros = vec![0; at(len)];
        let step = cursor.next(&zeros);
        assert_eq!(step, Ok(Step::End), "a zeroed area holds no record");
        let (writer, plan) = cursor
            .writer(start, Self::CHAIN)
            .expect("the ring is empty");
        let mut ring = Self {
            layout,
            writer,
            tail: start,
            area: vec![0; at(area)],
            records: 0,
        };
        ring.put(plan, &Self::CHAIN.to_le_bytes());
        ring
    }

    /// Plans one record of `body` and frees it and every record before it, so the
    /// ring stays empty. Nothing is written to the area.
    ///
    /// # Panics
    ///
    /// When `body` is over the largest body.
    pub fn plan(&mut self, body: &[u8]) {
        let plan = self
            .writer
            .append(&[body])
            .expect("an empty ring takes a record");
        self.writer.release(plan.next);
        self.tail = plan.next;
    }

    /// Writes one record of `body` to the area. Returns `false` when the ring is
    /// full.
    pub fn write(&mut self, body: &[u8]) -> bool {
        let Ok(plan) = self.writer.append(&[body]) else {
            return false;
        };
        self.put(plan, body);
        self.records += 1;
        true
    }

    /// Data records written.
    #[must_use]
    pub fn records(&self) -> u64 {
        self.records
    }

    /// Walks the live records from the tail as recovery does and returns the number
    /// of data records found.
    ///
    /// # Panics
    ///
    /// When a record follows the chain but cannot be read.
    #[must_use]
    pub fn walk(&self) -> u64 {
        let mut cursor = Cursor::new(self.layout, self.tail);
        let mut found = 0;
        loop {
            let Window { place, len } = cursor.window();
            let bytes = &self.area[at(place)..at(place + len)];
            match cursor.next(bytes).expect("the ring is valid") {
                Step::Data(_) => found += 1,
                Step::Moved | Step::More => {}
                Step::End => return found,
            }
        }
    }

    fn put(&mut self, plan: crate::wal::Plan, body: &[u8]) {
        if let Some(wrap) = plan.wrap {
            let place = at(wrap.place);
            self.area[place..place + HEADER_LEN].copy_from_slice(&wrap.header);
        }
        let place = at(plan.record.place);
        self.area[place..place + HEADER_LEN].copy_from_slice(&plan.record.header);
        let body_at = place + HEADER_LEN;
        self.area[body_at..body_at + body.len()].copy_from_slice(body);
    }
}

fn at(place: u64) -> usize {
    usize::try_from(place).expect("a place in the area fits in memory")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_check_vector() {
        assert_eq!(crc(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn a_new_ring_holds_no_data_record() {
        let ring = Ring::new(1 << 16, 4087);
        assert_eq!(ring.records(), 0);
        assert_eq!(ring.walk(), 0);
    }

    #[test]
    fn the_walk_finds_each_record_written_until_the_ring_is_full() {
        let mut ring = Ring::new(1 << 16, 4087);
        let mut written = 0;
        while ring.write(&[0xA5; 4087]) {
            written += 1;
        }
        assert_eq!(written, 15, "16 blocks less the restart record");
        assert_eq!(ring.records(), 15);
        assert_eq!(ring.walk(), 15);
    }

    #[test]
    fn the_walk_crosses_the_end_of_the_area() {
        let mut ring = Ring::new(1 << 16, 8183);
        for _ in 0..10 {
            ring.plan(&[0xA5; 4087]);
        }
        let mut written = 0;
        while ring.write(&[0xA5; 8183]) {
            written += 1;
        }
        assert_eq!(written, 7, "two before the wrap record and five after it");
        assert_eq!(ring.walk(), 7);
    }

    #[test]
    fn a_plan_frees_what_it_planned_and_the_records_before_it() {
        let mut ring = Ring::new(1 << 16, 4087);
        for _ in 0..100 {
            ring.plan(&[0xA5; 4087]);
        }
        let mut written = 0;
        while ring.write(&[0xA5; 4087]) {
            written += 1;
        }
        assert_eq!(written, 16, "the restart record was freed too");
    }
}
