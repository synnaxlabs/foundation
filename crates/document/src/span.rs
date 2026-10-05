/// The key of the file that a document came from. The caller keeps the table from key
/// to file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Source(pub u32);

/// A place in a file. Lines and columns count from 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Position {
    /// Bytes from the start of the file.
    pub offset: u32,
    /// Lines before this position.
    pub line: u32,
    /// Unicode scalar values between the start of the line and this position.
    pub column: u32,
}

/// The part of a file that holds an item, from `start` up to `end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Span {
    source: Source,
    start: Position,
    end: Position,
}

impl Span {
    /// Makes a span, or returns `None` when `end` is before `start` by offset or by
    /// line and column.
    #[must_use]
    pub fn new(source: Source, start: Position, end: Position) -> Option<Self> {
        let ordered = start.offset <= end.offset
            && (start.line, start.column) <= (end.line, end.column);
        ordered.then_some(Self { source, start, end })
    }

    /// The file that holds the item.
    #[must_use]
    pub const fn source(self) -> Source {
        self.source
    }

    /// Where the item starts.
    #[must_use]
    pub const fn start(self) -> Position {
        self.start
    }

    /// Where the item ends: the position after its last character.
    #[must_use]
    pub const fn end(self) -> Position {
        self.end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(offset: u32) -> Position {
        Position {
            offset,
            line: 0,
            column: offset,
        }
    }

    mod new {
        use super::*;

        #[test]
        fn keeps_the_source_and_both_ends() {
            let span = Span::new(Source(3), at(2), at(9)).unwrap();
            assert_eq!(span.source(), Source(3));
            assert_eq!(span.start(), at(2));
            assert_eq!(span.end(), at(9));
        }

        #[test]
        fn allows_an_empty_span() {
            assert_eq!(Span::new(Source(0), at(4), at(4)).unwrap().end(), at(4));
        }

        #[test]
        fn refuses_an_end_before_the_start() {
            assert_eq!(Span::new(Source(0), at(5), at(4)), None);
        }

        #[test]
        fn refuses_an_end_line_before_the_start_line() {
            let start = Position {
                offset: 0,
                line: 5,
                column: 7,
            };
            let end = Position {
                offset: 3,
                line: 0,
                column: 0,
            };
            assert_eq!(Span::new(Source(0), start, end), None);
        }
    }
}
