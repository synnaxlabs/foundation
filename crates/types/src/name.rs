//! Names in the one name tree, and the one matcher every selector uses.
//!
//! Channels, nodes, connectors, subjects, and secrets share one tree of dot-separated
//! names. Policies, readers, connectors, and access select names with a
//! [`Selector`]. No other crate matches names.

use std::cmp::Reverse;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::{FromStr, Split};

/// A name: dot-separated segments of letters, digits, `_`, and `-`, at most
/// [`Name::MAX_BYTES`] long. Names are case-sensitive. A segment that starts with `@`
/// is reserved for Foundation.
///
/// Letters and digits are ASCII, so `str::eq_ignore_ascii_case` finds two names that
/// differ only in case.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(Box<str>);

impl Name {
    /// The most bytes a name or pattern holds.
    pub const MAX_BYTES: usize = 255;

    /// The name as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The segments in order.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('.')
    }

    /// Reports whether `prefix` is this name or one of its ancestors, by whole
    /// segments: `site_a` is a prefix of `site_a.pt_1`, but `site` is not.
    #[must_use]
    pub fn starts_with(&self, prefix: &Name) -> bool {
        self.0
            .strip_prefix(&*prefix.0)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    }

    /// Reports whether any segment is reserved for Foundation.
    #[must_use]
    pub fn reserved(&self) -> bool {
        self.segments().any(|segment| segment.starts_with('@'))
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Name {
    type Err = Error;

    /// Reads and checks a name. Reserved segments are allowed here; `spec` decides
    /// who may use them.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for segment in split(s)? {
            check_literal(s, segment)?;
        }
        Ok(Self(s.into()))
    }
}

/// One pattern over names: `*` matches one segment and `**` matches any number of
/// segments, including none.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Pattern {
    segments: Box<[Segment]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Segment {
    Literal(Box<str>),
    One,
    Any,
}

impl Pattern {
    /// Reports whether the pattern matches `name`.
    #[must_use]
    pub fn matches(&self, name: &Name) -> bool {
        let pattern = &*self.segments;
        let mut p = 0;
        let mut rest = name.0.split('.');
        // The latest `**` and the name segments after the ones it has taken. Only the
        // latest one needs to take more on a mismatch.
        let mut any: Option<(usize, Split<'_, char>)> = None;
        loop {
            let mut after = rest.clone();
            let Some(segment) = after.next() else {
                return pattern[p..].iter().all(|s| *s == Segment::Any);
            };
            match pattern.get(p) {
                Some(Segment::Any) => {
                    any = Some((p, rest.clone()));
                    p += 1;
                    continue;
                }
                Some(Segment::One) => {
                    rest = after;
                    p += 1;
                    continue;
                }
                Some(Segment::Literal(literal)) if **literal == *segment => {
                    rest = after;
                    p += 1;
                    continue;
                }
                _ => {}
            }
            let Some((at, taken)) = &mut any else {
                return false;
            };
            taken.next();
            rest = taken.clone();
            p = *at + 1;
        }
    }

    /// How specific the pattern is. When several policies match one name, the most
    /// specific pattern wins.
    #[must_use]
    pub fn specificity(&self) -> Specificity {
        let mut specificity = Specificity {
            literals: 0,
            anys: Reverse(0),
            ones: 0,
        };
        for segment in &self.segments {
            match segment {
                Segment::Literal(_) => specificity.literals += 1,
                Segment::One => specificity.ones += 1,
                Segment::Any => specificity.anys.0 += 1,
            }
        }
        specificity
    }

    /// Reports whether every name the pattern matches starts with `prefix`: the
    /// pattern starts with the segments of `prefix`, as literals.
    fn within(&self, prefix: &Name) -> bool {
        let mut segments = self.segments.iter();
        prefix.segments().all(|want| match segments.next() {
            Some(Segment::Literal(literal)) => **literal == *want,
            _ => false,
        })
    }

    /// Reads `body`, reporting errors against `input`, the text the user wrote.
    ///
    /// Each run of wildcards becomes its `*`s and then at most one `**`, so patterns
    /// that match the same names are equal and have the same specificity.
    fn read(input: &str, body: &str) -> Result<Self, Error> {
        let mut segments = Vec::new();
        for segment in split(body)? {
            let next = match segment {
                "*" => Segment::One,
                "**" => Segment::Any,
                _ => {
                    check_literal(input, segment)?;
                    Segment::Literal(segment.into())
                }
            };
            match (segments.last(), &next) {
                (Some(Segment::Any), Segment::Any) => {}
                (Some(Segment::Any), Segment::One) => {
                    segments.insert(segments.len() - 1, next);
                }
                _ => segments.push(next),
            }
        }
        Ok(Self {
            segments: segments.into(),
        })
    }
}

impl FromStr for Pattern {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::read(s, s)
    }
}

/// An ordering of patterns by how specific they are. A greater value is more specific.
///
/// Patterns compare by more literal segments, then fewer `**`, then more `*`: `a.b`
/// is greater than `a.*`, which is greater than `a.*.**`, `a.**`, and `**` in turn.
/// A run of wildcards counts as its `*`s and one `**`, so `a.**.*.**` counts as
/// `a.*.**`. Two different patterns may be equal, such as `a.*` and `*.a`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Specificity {
    literals: usize,
    anys: Reverse<usize>,
    ones: usize,
}

/// A list of patterns, as written. A name matches when an include pattern matches it
/// and no exclusion does. An exclusion is a pattern written with a leading `!`.
/// Equality and hashing compare the texts in order.
#[derive(Clone)]
pub struct Selector {
    texts: Box<[Box<str>]>,
    // The patterns `texts` read as. Each include has its position in `texts`.
    include: Vec<(usize, Pattern)>,
    exclude: Vec<Pattern>,
}

/// A pattern of a selector as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Written<'a> {
    /// A pattern that includes names.
    Include(&'a str),
    /// A pattern that excludes names: the text after the `!`.
    Exclude(&'a str),
}

impl Selector {
    /// Reads a selector from its patterns.
    ///
    /// # Errors
    ///
    /// The first pattern that does not read, or [`Error::NoInclude`] when no pattern
    /// includes names.
    pub fn new<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Result<Self, Error> {
        let texts = patterns
            .into_iter()
            .map(Box::from)
            .collect::<Box<[Box<str>]>>();
        let mut include = Vec::new();
        let mut exclude = Vec::new();
        for (position, text) in texts.iter().enumerate() {
            let text = &**text;
            match written(text) {
                Written::Exclude("") => {
                    return Err(Error::Segment {
                        input: text.into(),
                        segment: String::new(),
                    });
                }
                Written::Exclude(body) => exclude.push(Pattern::read(text, body)?),
                Written::Include(body) => {
                    include.push((position, Pattern::read(text, body)?));
                }
            }
        }
        if include.is_empty() {
            return Err(Error::NoInclude);
        }
        Ok(Self {
            texts,
            include,
            exclude,
        })
    }

    /// The patterns as written, in order, each by what it does.
    #[must_use]
    pub fn written(&self) -> impl ExactSizeIterator<Item = Written<'_>> {
        self.texts.iter().map(|text| written(text))
    }

    /// The specificity of the most specific include pattern that matches `name`, or
    /// `None` when the selector does not match it.
    #[must_use]
    pub fn matches(&self, name: &Name) -> Option<Specificity> {
        if self.exclude.iter().any(|p| p.matches(name)) {
            return None;
        }
        self.include
            .iter()
            .filter(|(_, p)| p.matches(name))
            .map(|(_, p)| p.specificity())
            .max()
    }

    /// The positions, in [`Selector::written`], of the include patterns that can match
    /// a name that does not start with `prefix`, by whole segments, as
    /// [`Name::starts_with`] reads it. The selector stays within `prefix` when this
    /// gives no position. Exclusions are not read, so an include that only its
    /// exclusions keep inside `prefix` still gives its position.
    pub fn outside(&self, prefix: &Name) -> impl Iterator<Item = usize> {
        self.include
            .iter()
            .filter(|(_, p)| !p.within(prefix))
            .map(|(position, _)| *position)
    }
}

impl PartialEq for Selector {
    fn eq(&self, other: &Self) -> bool {
        self.texts == other.texts
    }
}

impl Eq for Selector {}

impl Hash for Selector {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.texts.hash(state);
    }
}

impl fmt::Debug for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.texts.iter()).finish()
    }
}

/// Reads a pattern as written: a leading `!` makes it an exclusion.
fn written(text: &str) -> Written<'_> {
    text.strip_prefix('!')
        .map_or(Written::Include(text), Written::Exclude)
}

fn split(s: &str) -> Result<Split<'_, char>, Error> {
    if s.is_empty() {
        return Err(Error::Empty);
    }
    if s.len() > Name::MAX_BYTES {
        return Err(Error::Long { bytes: s.len() });
    }
    Ok(s.split('.'))
}

/// Checks a segment that is not a wildcard, reporting errors against `input`.
fn check_literal(input: &str, segment: &str) -> Result<(), Error> {
    if segment.contains('*') {
        return Err(Error::Wildcard {
            input: input.into(),
        });
    }
    let body = segment.strip_prefix('@').unwrap_or(segment);
    let valid = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
    if body.is_empty() || !body.bytes().all(valid) {
        return Err(Error::Segment {
            input: input.into(),
            segment: segment.into(),
        });
    }
    Ok(())
}

/// A name, pattern, or selector that is not valid. `Display` gives the message: a
/// lower-case clause with no final period. [`Error::fix`] gives what to do instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The text of a name or pattern is empty.
    Empty,
    /// The text of a name or pattern is longer than [`Name::MAX_BYTES`].
    Long {
        /// The length of the text.
        bytes: usize,
    },
    /// A selector has no pattern that includes names: every pattern is an exclusion.
    NoInclude,
    /// A segment is empty or holds a character other than letters, digits, `_`, `-`,
    /// or a leading `@`.
    Segment {
        /// The whole text.
        input: String,
        /// The segment that is not valid.
        segment: String,
    },
    /// A wildcard appears in a name, or is part of a longer segment in a pattern.
    Wildcard {
        /// The whole text.
        input: String,
    },
}

impl Error {
    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub fn fix(&self) -> &'static str {
        match self {
            Self::Empty => "Write at least one segment",
            Self::Long { .. } => "Use fewer or shorter segments",
            Self::NoInclude => "Add a pattern without a leading `!`",
            Self::Segment { .. } => {
                "Use one or more ASCII letters, digits, `_`, and `-` in that segment, \
                 and no other character"
            }
            Self::Wildcard { .. } => {
                "Use `*` and `**` only as whole segments of a pattern, never in a name"
            }
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("a name or pattern is empty"),
            Self::Long { bytes } => write!(
                f,
                "a name or pattern is {bytes} bytes long, more than the limit of \
                 {} bytes",
                Name::MAX_BYTES
            ),
            Self::NoInclude => f.write_str("a selector includes no names"),
            Self::Segment { input, segment } => {
                write!(f, "{input:?} has a segment that is not valid: {segment:?}")
            }
            Self::Wildcard { input } => {
                write!(f, "{input:?} uses a wildcard where it cannot")
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn name(s: &str) -> Name {
        s.parse().unwrap()
    }

    fn pattern(s: &str) -> Pattern {
        s.parse().unwrap()
    }

    fn segment_error(input: &str, segment: &str) -> Error {
        Error::Segment {
            input: input.into(),
            segment: segment.into(),
        }
    }

    fn wildcard_error(input: &str) -> Error {
        Error::Wildcard {
            input: input.into(),
        }
    }

    mod name {
        use super::*;

        #[test]
        fn reads_letters_digits_underscores_and_dashes() {
            let n = name("site_a.PT-1.temp_2");
            assert_eq!(n.as_str(), "site_a.PT-1.temp_2");
            assert_eq!(
                n.segments().collect::<Vec<_>>(),
                ["site_a", "PT-1", "temp_2"]
            );
        }

        #[test]
        fn is_case_sensitive() {
            assert_ne!(name("site.pt"), name("Site.pt"));
        }

        #[test]
        fn rejects_empty_text() {
            assert_eq!("".parse::<Name>(), Err(Error::Empty));
            assert_eq!(Error::Empty.to_string(), "a name or pattern is empty");
            assert_eq!(Error::Empty.fix(), "Write at least one segment");
        }

        #[test]
        fn holds_up_to_the_limit() {
            let text = format!("a.{}", "b".repeat(Name::MAX_BYTES - 2));
            assert_eq!(name(&text).as_str().len(), 255);
            assert_eq!(
                format!("{text}c").parse::<Name>(),
                Err(Error::Long { bytes: 256 })
            );
            let error = Error::Long { bytes: 256 };
            assert_eq!(
                error.to_string(),
                "a name or pattern is 256 bytes long, more than the limit of 255 bytes"
            );
            assert_eq!(error.fix(), "Use fewer or shorter segments");
        }

        #[test]
        fn rejects_empty_segments() {
            for input in ["a..b", ".a", "a.", "."] {
                assert_eq!(input.parse::<Name>(), Err(segment_error(input, "")));
            }
        }

        #[test]
        fn rejects_other_characters() {
            for (input, segment) in [
                ("a.b c", "b c"),
                ("a.é", "é"),
                ("a/b", "a/b"),
                ("a.!b", "!b"),
                ("a.b$", "b$"),
            ] {
                assert_eq!(input.parse::<Name>(), Err(segment_error(input, segment)));
            }
        }

        #[test]
        fn shows_the_segment_and_the_fix() {
            let error = "a.b c".parse::<Name>().unwrap_err();
            assert_eq!(
                error.to_string(),
                "\"a.b c\" has a segment that is not valid: \"b c\""
            );
            assert_eq!(
                error.fix(),
                "Use one or more ASCII letters, digits, `_`, and `-` in that segment, \
                 and no other character"
            );
        }

        #[test]
        fn escapes_the_text_in_the_message() {
            assert_eq!(
                "a.\"b".parse::<Name>().unwrap_err().to_string(),
                r#""a.\"b" has a segment that is not valid: "\"b""#
            );
        }

        #[test]
        fn rejects_wildcards() {
            for input in ["a.*", "a.**", "a*", "*", "a.b*c"] {
                assert_eq!(input.parse::<Name>(), Err(wildcard_error(input)));
            }
            let error = wildcard_error("a.*");
            assert_eq!(error.to_string(), "\"a.*\" uses a wildcard where it cannot");
            assert_eq!(
                error.fix(),
                "Use `*` and `**` only as whole segments of a pattern, never in a name"
            );
        }

        mod reserved {
            use super::*;

            #[test]
            fn allows_a_leading_at_sign() {
                assert!(name("@node.status").reserved());
                assert!(name("site.@changes").reserved());
                assert!(!name("site.pt").reserved());
            }

            #[test]
            fn rejects_an_at_sign_elsewhere() {
                for (input, segment) in
                    [("@", "@"), ("a.@", "@"), ("a@b", "a@b"), ("@@a", "@@a")]
                {
                    assert_eq!(
                        input.parse::<Name>(),
                        Err(segment_error(input, segment))
                    );
                }
            }
        }

        mod starts_with {
            use super::*;

            #[test]
            fn matches_whole_segments() {
                assert!(name("site_a.pt_1").starts_with(&name("site_a")));
                assert!(name("site_a.pt_1").starts_with(&name("site_a.pt_1")));
                assert!(!name("site_a.pt_1").starts_with(&name("site")));
                assert!(!name("site_a").starts_with(&name("site_a.pt_1")));
                assert!(!name("site_a.pt_1").starts_with(&name("pt_1")));
            }
        }
    }

    mod pattern {
        use super::*;

        #[test]
        fn rejects_partial_wildcards() {
            for input in ["a*", "a.***", "a.**b", "*a.b"] {
                assert_eq!(input.parse::<Pattern>(), Err(wildcard_error(input)));
            }
            assert_eq!(
                "a*".parse::<Pattern>().unwrap_err().fix(),
                "Use `*` and `**` only as whole segments of a pattern, never in a name"
            );
        }

        #[test]
        fn shows_only_the_bad_segment() {
            let error = "site.*.t c".parse::<Pattern>().unwrap_err();
            assert_eq!(error, segment_error("site.*.t c", "t c"));
            assert_eq!(
                error.to_string(),
                "\"site.*.t c\" has a segment that is not valid: \"t c\""
            );
        }

        #[test]
        fn rejects_bad_segments_and_empty_text() {
            assert_eq!("".parse::<Pattern>(), Err(Error::Empty));
            assert_eq!("a..*".parse::<Pattern>(), Err(segment_error("a..*", "")));
            assert_eq!("!a".parse::<Pattern>(), Err(segment_error("!a", "!a")));
        }

        #[test]
        fn one_wildcard_matches_exactly_one_segment() {
            let p = pattern("site.*");
            assert!(p.matches(&name("site.pt")));
            assert!(!p.matches(&name("site")));
            assert!(!p.matches(&name("site.pt.temp")));
            assert!(!p.matches(&name("other.pt")));
        }

        #[test]
        fn any_wildcard_matches_any_depth_including_none() {
            let p = pattern("site.**");
            assert!(p.matches(&name("site")));
            assert!(p.matches(&name("site.pt")));
            assert!(p.matches(&name("site.pt.temp.raw")));
            assert!(!p.matches(&name("other.pt")));
            assert!(pattern("**").matches(&name("x")));
        }

        #[test]
        fn any_wildcard_in_the_middle_backtracks() {
            let p = pattern("a.**.c");
            assert!(p.matches(&name("a.c")));
            assert!(p.matches(&name("a.b.c")));
            assert!(p.matches(&name("a.c.b.c")));
            assert!(!p.matches(&name("a.b.d")));
            assert!(!p.matches(&name("a.c.d")));
            assert!(pattern("**.*.b").matches(&name("x.b")));
            assert!(!pattern("**.*.b").matches(&name("b")));
        }

        #[test]
        fn literals_are_case_sensitive() {
            assert!(!pattern("Site.*").matches(&name("site.pt")));
        }

        proptest! {
            #[test]
            fn matches_like_the_definition(p in patterns(), n in names()) {
                let matched = pattern(&p.join(".")).matches(&name(&n.join(".")));
                prop_assert_eq!(matched, reference(&p, &n));
            }

            #[test]
            fn a_name_matches_itself_with_any_segment_wildcarded(
                n in names(),
                wild in wildcards(),
            ) {
                let p: Vec<_> =
                    n.iter().zip(&wild).map(|(s, w)| w.unwrap_or(s)).collect();
                prop_assert!(pattern(&p.join(".")).matches(&name(&n.join("."))));
            }
        }

        mod specificity {
            use super::*;

            #[test]
            fn orders_literals_then_fewer_any_then_more_one() {
                let order = ["**", "a.**", "a.*.**", "a.*", "a.*.*", "a.b"];
                let specificities: Vec<_> =
                    order.iter().map(|p| pattern(p).specificity()).collect();
                assert!(
                    specificities.is_sorted_by(|a, b| a < b),
                    "{order:?} is not increasing"
                );
            }

            #[test]
            fn ranks_literals_above_fewer_any() {
                assert!(
                    pattern("a.b.**").specificity() > pattern("a.*.*").specificity()
                );
            }

            #[test]
            fn ranks_fewer_any_above_more_one() {
                assert!(
                    pattern("**.a").specificity() > pattern("*.**.a.**").specificity()
                );
            }

            #[test]
            fn counts_a_wildcard_run_as_its_ones_and_one_any() {
                for (written, reduced) in [
                    ("site.**.**", "site.**"),
                    ("site.**.*.**", "site.*.**"),
                    ("**.*", "*.**"),
                    ("a.*.**.*.b", "a.*.*.**.b"),
                ] {
                    assert_eq!(pattern(written), pattern(reduced), "{written}");
                    assert_eq!(
                        pattern(written).specificity(),
                        pattern(reduced).specificity(),
                        "{written}"
                    );
                }
                assert!(
                    pattern("site.**.**").specificity()
                        > pattern("**.site.**").specificity()
                );
            }

            proptest! {
                #[test]
                fn the_exact_pattern_is_the_most_specific_match(
                    p in patterns(),
                    n in names(),
                ) {
                    let exact = pattern(&n.join("."));
                    let other = pattern(&p.join("."));
                    if other != exact && other.matches(&name(&n.join("."))) {
                        prop_assert!(other.specificity() < exact.specificity());
                    }
                }
            }

            #[test]
            fn is_equal_for_different_patterns_of_the_same_shape() {
                assert_eq!(pattern("a.*").specificity(), pattern("*.a").specificity());
                assert_eq!(
                    pattern("a.**.b").specificity(),
                    pattern("**.a.b").specificity()
                );
            }
        }
    }

    mod selector {
        use super::*;

        #[test]
        fn returns_the_most_specific_include() {
            let s = Selector::new(["site.pt", "**", "site.*"]).unwrap();
            assert_eq!(
                s.matches(&name("site.pt")),
                Some(pattern("site.pt").specificity())
            );
            assert_eq!(
                s.matches(&name("site.x")),
                Some(pattern("site.*").specificity())
            );
            assert_eq!(s.matches(&name("other")), Some(pattern("**").specificity()));
        }

        #[test]
        fn exclusions_remove_matches() {
            let s = Selector::new(["site.**", "!site.debug.**"]).unwrap();
            assert!(s.matches(&name("site.pt")).is_some());
            assert_eq!(s.matches(&name("site.debug")), None);
            assert_eq!(s.matches(&name("site.debug.raw")), None);
            assert_eq!(s.matches(&name("other")), None);
        }

        #[test]
        fn rejects_no_includes() {
            assert_eq!(Selector::new([]), Err(Error::NoInclude));
            assert_eq!(Selector::new(["!a", "!b.**"]), Err(Error::NoInclude));
            assert_eq!(Error::NoInclude.to_string(), "a selector includes no names");
            assert_eq!(
                Error::NoInclude.fix(),
                "Add a pattern without a leading `!`"
            );
        }

        #[test]
        fn reports_errors_against_the_written_pattern() {
            assert_eq!(
                Selector::new(["a", "!b..c"]),
                Err(segment_error("!b..c", ""))
            );
            assert_eq!(Selector::new(["a", "!b*"]), Err(wildcard_error("!b*")));
            assert_eq!(Selector::new(["a", "!"]), Err(segment_error("!", "")));
        }

        #[test]
        fn counts_only_the_pattern_after_the_exclamation_mark() {
            let body = "b".repeat(Name::MAX_BYTES);
            let s = Selector::new(["**", &format!("!{body}")]).unwrap();
            assert_eq!(s.matches(&name(&body)), None);
            assert_eq!(
                Selector::new(["a", &format!("!b{body}")]),
                Err(Error::Long { bytes: 256 })
            );
            assert_eq!(
                Selector::new(["a", &format!("!{}", "b".repeat(300))]),
                Err(Error::Long { bytes: 300 })
            );
            assert_eq!(
                Selector::new(["a", &format!("!{}", "é".repeat(128))]),
                Err(Error::Long { bytes: 256 })
            );
        }

        #[test]
        fn keeps_the_patterns_as_written() {
            let s = Selector::new(["a.**.**", "!a.b"]).unwrap();
            assert_eq!(format!("{s:?}"), r#"["a.**.**", "!a.b"]"#);
            assert_eq!(
                s.written().collect::<Vec<_>>(),
                [Written::Include("a.**.**"), Written::Exclude("a.b")]
            );
        }

        #[test]
        fn compares_the_texts_in_order() {
            let read = |texts: &[&str]| Selector::new(texts.iter().copied()).unwrap();
            for (a, b) in [
                (read(&["a.**.**"]), read(&["a.**"])),
                (read(&["a", "b"]), read(&["b", "a"])),
            ] {
                for n in ["a", "a.b", "b", "c"] {
                    assert_eq!(a.matches(&name(n)), b.matches(&name(n)), "{n}");
                }
                assert_ne!(a, b);
            }
            let hash = |s: &Selector| {
                let mut hasher = std::hash::DefaultHasher::new();
                s.hash(&mut hasher);
                hasher.finish()
            };
            let a = read(&["a", "!a.b"]);
            assert_eq!(a, read(&["a", "!a.b"]));
            assert_eq!(hash(&a), hash(&read(&["a", "!a.b"])));
            assert_ne!(hash(&read(&["a", "b"])), hash(&read(&["b", "a"])));
            assert_ne!(hash(&read(&["a.**.**"])), hash(&read(&["a.**"])));
        }

        mod outside {
            use super::*;

            fn selector(patterns: &[&str]) -> Selector {
                Selector::new(patterns.iter().copied()).unwrap()
            }

            fn outside(patterns: &[&str], prefix: &str) -> Vec<usize> {
                selector(patterns).outside(&name(prefix)).collect()
            }

            #[test]
            fn holds_patterns_that_start_with_the_prefix() {
                for pattern in
                    ["site_a", "site_a.*", "site_a.**", "site_a.**.*", "site_a.b"]
                {
                    assert_eq!(outside(&[pattern], "site_a"), [], "{pattern}");
                }
            }

            #[test]
            fn refuses_patterns_that_reach_past_the_prefix() {
                for (pattern, prefix) in [
                    ("**", "site_a"),
                    ("*.gw", "site_a"),
                    ("site_a_b.*", "site_a"),
                    ("site.*", "site_a"),
                    ("**.site_a", "site_a"),
                    ("site_a", "site_a.b"),
                    ("site_a.**", "site_a.b"),
                ] {
                    assert_eq!(outside(&[pattern], prefix), [0], "{pattern} {prefix}");
                }
            }

            #[test]
            fn gives_each_include_that_reaches_out_by_its_position() {
                let patterns = ["a.b", "!b.**", "b", "a.**", "*.c", "!a"];
                assert_eq!(outside(&patterns, "a"), [2, 4]);
                assert_eq!(outside(&["b", "a.b"], "a"), [0]);
            }

            #[test]
            fn reads_no_exclusion() {
                assert_eq!(outside(&["a.**", "!b.**"], "a"), []);
                assert_eq!(outside(&["**", "!b.**"], "a"), [0]);
            }

            proptest! {
                #[test]
                fn every_match_starts_with_the_prefix(
                    p in patterns(),
                    prefix in prefixes(),
                    fill in fills(),
                ) {
                    let segments: Vec<&str> = p
                        .iter()
                        .zip(&fill)
                        .flat_map(|(segment, fill)| match *segment {
                            "*" => &fill[..1],
                            "**" => &fill[1..],
                            _ => std::slice::from_ref(segment),
                        })
                        .copied()
                        .collect();
                    prop_assume!(!segments.is_empty());
                    let selector = selector(&[&p.join(".")]);
                    let n = name(&segments.join("."));
                    let prefix = name(&prefix.join("."));
                    prop_assert!(selector.matches(&n).is_some());
                    if selector.outside(&prefix).next().is_none() {
                        prop_assert!(n.starts_with(&prefix), "{n} outside {prefix}");
                    }
                }

                #[test]
                fn a_pattern_outside_matches_a_name_outside(
                    p in patterns(),
                    prefix in prefixes(),
                ) {
                    let selector = selector(&[&p.join(".")]);
                    let prefix = name(&prefix.join("."));
                    // `c` is never a segment of `prefix`.
                    let outside: Vec<_> = p
                        .iter()
                        .map(|s| if s.starts_with('*') { "c" } else { s })
                        .collect();
                    let outside = name(&outside.join("."));
                    if selector.outside(&prefix).next().is_some() {
                        prop_assert!(selector.matches(&outside).is_some());
                        prop_assert!(!outside.starts_with(&prefix), "{outside}");
                    }
                }
            }

            fn prefixes() -> impl Strategy<Value = Vec<&'static str>> {
                prop::collection::vec(prop::sample::select(vec!["a", "b"]), 1..4)
            }

            /// For each pattern segment, the one segment a `*` takes and then the
            /// segments a `**` takes.
            fn fills() -> impl Strategy<Value = Vec<Vec<&'static str>>> {
                let segment = prop::sample::select(vec!["a", "b", "c"]);
                prop::collection::vec(prop::collection::vec(segment, 1..4), 7)
            }
        }
    }

    proptest! {
        #[test]
        fn every_error_is_a_clause_and_a_sentence_with_no_final_period(
            text in r#"[a.*!@ "\\]{0,8}|a{256}"#,
        ) {
            let errors = [
                text.parse::<Name>().err(),
                text.parse::<Pattern>().err(),
                Selector::new([text.as_str()]).err(),
            ];
            for error in errors.into_iter().flatten() {
                let (message, fix) = (error.to_string(), error.fix());
                prop_assert!(!message.starts_with(char::is_uppercase), "{message}");
                prop_assert!(fix.starts_with(char::is_uppercase), "{fix}");
                prop_assert!(!message.ends_with('.') && !fix.ends_with('.'));
            }
        }
    }

    /// Matches by trying every split, as the definition reads.
    fn reference(pattern: &[&str], name: &[&str]) -> bool {
        match pattern.split_first() {
            None => name.is_empty(),
            Some((&"**", rest)) => {
                (0..=name.len()).any(|i| reference(rest, &name[i..]))
            }
            Some((&"*", rest)) => !name.is_empty() && reference(rest, &name[1..]),
            Some((literal, rest)) => {
                name.first() == Some(literal) && reference(rest, &name[1..])
            }
        }
    }

    fn names() -> impl Strategy<Value = Vec<&'static str>> {
        prop::collection::vec(prop::sample::select(vec!["a", "b", "c"]), 1..7)
    }

    fn patterns() -> impl Strategy<Value = Vec<&'static str>> {
        prop::collection::vec(prop::sample::select(vec!["a", "b", "*", "**"]), 1..7)
    }

    fn wildcards() -> impl Strategy<Value = Vec<Option<&'static str>>> {
        let choice = prop::sample::select(vec![None, Some("*"), Some("**")]);
        prop::collection::vec(choice, 7)
    }
}
