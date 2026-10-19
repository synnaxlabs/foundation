//! What a read and a write stream share: the first message, the places, and the
//! decoder of the places at the program.

use std::sync::Arc;

use types::channel;
use types::frame::key_set::KeySet;
use types::name::Name;
use types::sample::{Scalar, Type};

use super::{Body, Error, READ, fields};
use crate::common::{Writer, rest_of_run};

/// The most bytes of the open of a read or a write: 64 KiB.
pub const OPEN_BYTES_MAX: u64 = 1 << 16;

/// The most places of a read or a write: 65,536.
pub const PLACES_MAX: u32 = 1 << 16;

/// The bytes of the first message of a read or a write stream.
pub(super) const LEN: usize = 73;

/// Writes the first message of a read or a write stream: `kind`, the open's
/// `length`, and the `signature` over the open.
///
/// # Panics
///
/// When `length` is over [`OPEN_BYTES_MAX`], or `out` is not [`LEN`] bytes.
pub(super) fn encode(kind: u8, length: u64, signature: &[u8; 64], out: &mut [u8]) {
    check(length);
    let mut out = Writer::new(out, LEN);
    out.put(&[kind]);
    out.put(&length.to_le_bytes());
    out.put(signature);
}

/// Decodes the first message of a read or a write stream: the open's length and the
/// signature.
///
/// # Errors
///
/// [`Error::Empty`], [`Error::Kind`] for a message that is not `kind`,
/// [`Error::Length`], and [`Error::Oversize`] for an open over [`OPEN_BYTES_MAX`].
pub(super) fn decode(kind: u8, message: &[u8]) -> Result<(u64, [u8; 64]), Error> {
    let mut fields = fields(message, kind)?;
    let length = u64::from_le_bytes(*fields.take()?);
    let signature = *fields.take()?;
    fields.end()?;
    if length > OPEN_BYTES_MAX {
        return Err(Error::Oversize {
            length,
            cap: OPEN_BYTES_MAX,
        });
    }
    Ok((length, signature))
}

/// The open of `length` bytes that follows the first message.
///
/// # Panics
///
/// When `length` is over [`OPEN_BYTES_MAX`].
pub(super) fn body(length: u64) -> Body {
    check(length);
    Body::new(length)
}

fn check(length: u64) {
    assert!(
        length <= OPEN_BYTES_MAX,
        "an open of {length} bytes is over the cap of {OPEN_BYTES_MAX}"
    );
}

/// A decoder of the start of a write stream at the program: the places and their run.
#[derive(Debug, Default)]
pub struct Opening {
    next: Next,
    /// Each place names the index of place 0, as on a read.
    single: bool,
}

/// The message the stream expects next.
#[derive(Debug, Default)]
enum Next {
    #[default]
    Places,
    Run(Run),
    Given,
}

/// The start of a write stream, at the program.
#[derive(Debug)]
pub struct Start {
    /// The places, in place order.
    pub places: Vec<Place>,
    /// The key set of each frame of the write, from [`KeySet::ordered`]: entry `n` is
    /// place `n`, on the index that place `n` names.
    pub set: Arc<KeySet>,
}

impl Opening {
    /// A decoder of the places of a read, where a place also breaks a rule when its
    /// index is not that of place 0: [`Error::Indexes`].
    pub(crate) fn read() -> Self {
        Self {
            next: Next::Places,
            single: true,
        }
    }

    /// Decodes the next message from the node, up to the last place, or `None` when
    /// the node finished the stream. Gives the start with the last place, else
    /// `None`.
    ///
    /// # Errors
    ///
    /// [`Error::Finished`] when `message` is `None`. The first message decodes as
    /// [`Places`] and gives its errors: [`Error::Empty`], [`Error::Kind`] for another
    /// kind, [`Error::Length`], or [`Error::Places`]. A message of the run gives the
    /// error of its bytes ([`Error::Empty`], [`Error::Length`], [`Error::Sample`], or
    /// [`Error::Name`]), then [`Error::Run`] when it holds more places than remain.
    /// The last message then gives, for the first place in place order that breaks a
    /// rule, [`Error::Index`] when its index is not a place whose index is itself, or
    /// [`Error::Unstamped`] when it is an index whose sample type is not `Stamp`.
    /// Last, [`Error::Repeated`] when the places name a channel more than once.
    ///
    /// The stream is then not valid ([`MALFORMED`](crate::header::MALFORMED)), and
    /// the caller stops it. A refused message leaves the decoder as it was.
    ///
    /// # Panics
    ///
    /// When it gave the start before.
    pub fn decode(&mut self, message: Option<&[u8]>) -> Result<Option<Start>, Error> {
        match (&mut self.next, message) {
            (Next::Given, _) => {
                panic!("a stream is decoded after the decoder gave its start")
            }
            (_, None) => Err(Error::Finished),
            (Next::Places, Some(message)) => {
                self.next = Next::Run(Run::new(Places::decode(message)?));
                Ok(None)
            }
            (Next::Run(run), Some(message)) => {
                let Some((places, set)) = run.decode(message, self.single)? else {
                    return Ok(None);
                };
                self.next = Next::Given;
                Ok(Some(Start {
                    places,
                    set: Arc::new(set),
                }))
            }
        }
    }
}

/// The run of a [`Places`], with the places that arrived.
#[derive(Debug)]
struct Run {
    places: Places,
    remain: u32,
    arrived: Vec<Place>,
}

impl Run {
    fn new(places: Places) -> Self {
        Self {
            places,
            remain: places.count,
            arrived: Vec::new(),
        }
    }

    /// Decodes the next message of the run. Gives the places and their key set, from
    /// [`KeySet::ordered`], with the last message, else `None`. Gives the errors of a
    /// message of the run that [`Opening::decode`] states, and a refused message
    /// leaves the run as it was. With `single`, each place names the index of place 0.
    fn decode(
        &mut self,
        message: &[u8],
        single: bool,
    ) -> Result<Option<(Vec<Place>, KeySet)>, Error> {
        let Self {
            places,
            remain,
            arrived: all,
        } = self;
        let run = place::decode(message)?;
        let rest = rest_of_run(*remain, run.len())?;
        let first = places.count.checked_sub(*remain).unwrap_or_else(|| {
            panic!(
                "invariant: remain {remain} is at most count {}",
                places.count
            )
        });
        // Once for the whole run, so `all` never doubles past the cap, and the last
        // message moves into it before the key set is built.
        all.reserve_exact(to_usize(*remain));
        all.extend(run);
        if rest > 0 {
            *remain = rest;
            return Ok(None);
        }
        let grouped = indexes(all, single).and_then(|()| {
            let channels = all
                .iter()
                .map(|place| (place.key, place.data_type, place.index));
            KeySet::ordered(channels)
                .map_err(|repeated| Error::Repeated { key: repeated.key })
        });
        match grouped {
            Ok(set) => Ok(Some((std::mem::take(all), set))),
            Err(error) => {
                all.truncate(to_usize(first));
                Err(error)
            }
        }
    }
}

/// Checks that the index of each of `places` is a place whose index is itself, of
/// type `Stamp`, and with `single` that of place 0, as [`Opening::decode`] states.
fn indexes(places: &[Place], single: bool) -> Result<(), Error> {
    let lead = places.first().map(|place| place.index);
    for (at, place) in places.iter().enumerate() {
        let index = places
            .get(place.index)
            .filter(|index| index.index == place.index);
        let Some(index) = index else {
            return Err(Error::Index {
                place: to_u32(at),
                index: to_u32(place.index),
            });
        };
        if at == place.index && index.data_type != Type::Scalar(Scalar::Stamp) {
            return Err(Error::Unstamped {
                place: to_u32(at),
                sample: index.data_type,
            });
        }
        if single && Some(place.index) != lead {
            return Err(Error::Indexes {
                place: to_u32(at),
                index: to_u32(place.index),
            });
        }
    }
    Ok(())
}

/// The first reply on a read or a write stream, of kind 6 on both. The run of its
/// [`place`]s follows. On a read, the replies of a reader session then follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Places {
    /// The places of the stream, each index too. At least 1, and at most
    /// [`PLACES_MAX`].
    pub count: u32,
}

impl Places {
    /// The bytes of an encoded places.
    pub const LEN: usize = 5;

    /// Writes the places into `out`.
    ///
    /// # Panics
    ///
    /// When `count` is 0 or over [`PLACES_MAX`], or `out` is not [`Places::LEN`]
    /// bytes.
    pub fn encode(&self, out: &mut [u8]) {
        assert!(
            (1..=PLACES_MAX).contains(&self.count),
            "{} places are not from 1 to the cap of {PLACES_MAX}",
            self.count
        );
        let mut out = Writer::new(out, Self::LEN);
        out.put(&[READ]);
        out.put(&self.count.to_le_bytes());
    }

    /// Decodes a places.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`], [`Error::Kind`] for a message that is not kind 6,
    /// [`Error::Length`], and [`Error::Places`] when `count` is 0 or over
    /// [`PLACES_MAX`].
    pub fn decode(message: &[u8]) -> Result<Self, Error> {
        let mut fields = fields(message, READ)?;
        let count = u32::from_le_bytes(*fields.take()?);
        fields.end()?;
        if !(1..=PLACES_MAX).contains(&count) {
            return Err(Error::Places {
                count,
                cap: PLACES_MAX,
            });
        }
        Ok(Self { count })
    }
}

/// One place of a read or a write: a channel, in the place order of the stream's
/// frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    /// The channel's key.
    pub key: channel::Key,
    /// The channel's sample type.
    pub data_type: Type,
    /// The channel's name.
    pub name: Name,
    /// The place of the index of the channel's group: an index's own place. On a
    /// read, each place has the one index.
    pub index: usize,
}

impl Place {
    /// The bytes of the encoded place.
    fn encoded_len(&self) -> usize {
        self.data_type
            .to_string()
            .len()
            .saturating_add(self.name.as_str().len())
            .saturating_add(22)
    }
}

/// The run of places after a [`Places`], in place order. A message of the run holds
/// whole places.
pub mod place {
    use types::channel;

    use super::{Place, to_usize};
    use crate::common::{Fields, Writer};
    use crate::hub::client::Error;
    use crate::hub::client::text;

    /// One message of the run: whole places that fit in the `max` of [`messages`].
    #[derive(Clone, Copy, Debug)]
    pub struct Message<'p> {
        places: &'p [Place],
        len: usize,
    }

    impl Message<'_> {
        /// The bytes of the encoded message.
        #[must_use]
        pub fn encoded_len(&self) -> usize {
            self.len
        }

        /// Writes the message into `out`.
        ///
        /// # Panics
        ///
        /// When `out` is not [`Message::encoded_len`] bytes, or the index of a place
        /// is over `u32::MAX`.
        pub fn encode(&self, out: &mut [u8]) {
            let fit = "the index of a place is at most u32::MAX";
            let mut out = Writer::new(out, self.len);
            for place in self.places {
                let data_type = place.data_type.to_string();
                out.put(&place.key.as_u128().to_le_bytes());
                out.put(&u32::try_from(place.index).expect(fit).to_le_bytes());
                out.put(&[u8::try_from(data_type.len()).expect(
                    "invariant: the text of a sample type is under 256 bytes",
                )]);
                out.put(data_type.as_bytes());
                out.put(&[u8::try_from(place.name.as_str().len())
                    .expect("invariant: a name is at most 255 bytes")]);
                out.put(place.name.as_str().as_bytes());
            }
        }
    }

    /// `places` cut into the messages of the run, in order: each holds as many whole
    /// places as fit in `max` bytes. A place is at most 304 bytes.
    ///
    /// # Panics
    ///
    /// While it iterates, when a place is over `max` bytes. A `max` of at least 304
    /// never panics.
    pub fn messages(places: &[Place], max: usize) -> impl Iterator<Item = Message<'_>> {
        let mut rest = places;
        std::iter::from_fn(move || {
            let mut len = 0_usize;
            let fit = rest
                .iter()
                .take_while(|place| {
                    let bytes = len.saturating_add(place.encoded_len());
                    let fits = bytes <= max;
                    if fits {
                        len = bytes;
                    }
                    fits
                })
                .count();
            if let Some(first) = rest.first() {
                assert!(
                    fit > 0,
                    "a place of {} bytes is over a message of {max}",
                    first.encoded_len()
                );
            }
            let (places, after) = rest.split_at(fit);
            rest = after;
            (!places.is_empty()).then_some(Message { places, len })
        })
    }

    /// The places in `message`, one message of the run.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] when `message` is empty, [`Error::Length`] when it ends inside
    /// a place, [`Error::Sample`] when the sample type of a place does not read, and
    /// [`Error::Name`] when its name does not. The first defect in the order of the
    /// bytes gives the error.
    pub fn decode(message: &[u8]) -> Result<Vec<Place>, Error> {
        if message.is_empty() {
            return Err(Error::Empty);
        }
        let mut fields = Fields::new(message);
        let mut places = Vec::new();
        while !fields.is_empty() {
            let key = u128::from_le_bytes(*fields.take()?);
            let index = u32::from_le_bytes(*fields.take()?);
            let [len] = *fields.take()?;
            let data_type = text::read(fields.take_slice(usize::from(len))?)
                .map_err(Error::Sample)?;
            let [len] = *fields.take()?;
            let name = text::read(fields.take_slice(usize::from(len))?)
                .map_err(Error::Name)?;
            places.push(Place {
                key: channel::Key::from_u128(key),
                data_type,
                name,
                index: to_usize(index),
            });
        }
        Ok(places)
    }
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).expect("invariant: a usize holds a u32")
}

/// A decoded place's position or index, each below `PLACES_MAX` or read from a `u32`.
fn to_u32(n: usize) -> u32 {
    u32::try_from(n).expect("invariant: a decoded place's position and index fit a u32")
}
