#![expect(clippy::arithmetic_side_effects, reason = "a test may panic")]

use proptest::prelude::*;

use super::*;

/// Response headers, as the head parser gives them.
type Headers<'a> = &'a [(&'a str, &'a [u8])];

#[test]
fn gives_no_body_to_a_status_that_has_none() {
    let length: Headers<'_> = &[("Content-Length", b"5")];
    let encoding: Headers<'_> = &[("Transfer-Encoding", b"chunked")];
    for status in [100, 102, 103, 199] {
        assert_eq!(Framing::new(status, length), Ok(Framing::Interim));
        assert_eq!(Framing::new(status, encoding), Ok(Framing::Interim));
    }
    for status in [204, 304] {
        assert_eq!(Framing::new(status, length), Ok(Framing::Empty));
        assert_eq!(Framing::new(status, encoding), Ok(Framing::Empty));
    }
    assert_eq!(Framing::new(200, length), Ok(Framing::Length(5)));
    assert_eq!(Framing::new(205, length), Ok(Framing::Length(5)));
    assert_eq!(Framing::new(400, &[]), Ok(Framing::Close));
    for framing in [Framing::Interim, Framing::Empty] {
        assert_eq!(
            framing.body(b"HTTP/1.1 200", false, 0),
            Ok(Some(Body::whole(b""))),
            "the bytes after the head belong to the next response"
        );
    }
}

#[test]
fn refuses_a_switch_of_protocols() {
    assert_eq!(Framing::new(101, &[]), Err(Error::Switch));
    assert_eq!(
        Error::Switch.to_string(),
        "a 101 response to a client that asked for no switch"
    );
}

#[test]
fn refuses_both_framings_on_any_status() {
    let both: Headers<'_> = &[("Content-Length", b"0"), ("Transfer-Encoding", b"x")];
    for status in [100, 101, 200, 204, 304] {
        assert_eq!(Framing::new(status, both), Err(Error::Ambiguous));
    }
}

#[test]
fn refuses_framing_that_is_ambiguous() {
    let cases: [(Headers<'_>, Error, &str); 9] = [
        (
            &[("content-length", b"1"), ("Transfer-Encoding", b"chunked")],
            Error::Ambiguous,
            "a response with both Content-Length and Transfer-Encoding",
        ),
        (
            &[("Content-Length", b"1"), ("Content-Length", b"1")],
            Error::Length("1".into()),
            "the Content-Length \"1\" is not one decimal number",
        ),
        (
            &[("Content-Length", b"+1")],
            Error::Length("+1".into()),
            "the Content-Length \"+1\" is not one decimal number",
        ),
        (
            &[("Content-Length", b"1, 1")],
            Error::Length("1, 1".into()),
            "the Content-Length \"1, 1\" is not one decimal number",
        ),
        (
            &[("Content-Length", b" 5")],
            Error::Length(" 5".into()),
            "the Content-Length \" 5\" is not one decimal number",
        ),
        (
            &[("Transfer-Encoding", b"gzip, chunked")],
            Error::Encoding("gzip, chunked".into()),
            "the Transfer-Encoding \"gzip, chunked\" is not supported",
        ),
        (
            &[("Content-Length", b"")],
            Error::Length(String::new()),
            "the Content-Length \"\" is not one decimal number",
        ),
        (
            &[("Content-Length", b"18446744073709551616")],
            Error::Length("18446744073709551616".into()),
            "the Content-Length \"18446744073709551616\" is not one decimal number",
        ),
        (
            &[("Transfer-Encoding", b"chunked")],
            Error::Encoding("chunked".into()),
            "the Transfer-Encoding \"chunked\" is not supported",
        ),
    ];
    for (headers, error, message) in cases {
        assert_eq!(Framing::new(200, headers), Err(error.clone()));
        assert_eq!(error.to_string(), message);
    }
}

#[test]
fn reads_a_body_that_ends_at_the_close_only_under_the_cap() {
    assert_eq!(Framing::Close.body(b"abc", false, 3), Ok(None));
    assert_eq!(
        Framing::Close.body(b"abc", true, 3),
        Ok(Some(Body::whole(b"abc")))
    );
    assert_eq!(
        Framing::Close.body(b"abcd", false, 3),
        Err(Error::Oversize { cap: 3 })
    );
    assert_eq!(
        Error::Oversize { cap: 3 }.to_string(),
        "a response body over the cap of 3 bytes"
    );
}

#[test]
fn refuses_a_length_over_the_cap_before_it_arrives() {
    assert_eq!(
        Framing::Length(4).body(b"", false, 3),
        Err(Error::Oversize { cap: 3 })
    );
    assert_eq!(
        Framing::Length(4).body(b"abcde", false, 4),
        Ok(Some(Body::whole(b"abcd")))
    );
}

#[test]
fn refuses_a_stream_that_closes_inside_a_body() {
    assert_eq!(
        Framing::Length(3).body(b"ab", true, 9),
        Err(Error::Truncated { want: 3, got: 2 })
    );
    assert_eq!(
        Error::Truncated { want: 3, got: 2 }.to_string(),
        "the stream closed after 2 of 3 body bytes"
    );
}

proptest! {
    #[test]
    fn reads_a_length_body_once_it_is_whole(
        body in proptest::collection::vec(any::<u8>(), 0..64),
        after in proptest::collection::vec(any::<u8>(), 0..8),
        cut in any::<prop::sample::Index>(),
    ) {
        let framing = Framing::Length(u64::try_from(body.len()).unwrap());
        let mut bytes = body.clone();
        bytes.extend_from_slice(&after);
        let cut = cut.index(bytes.len() + 1);
        let (head, _) = bytes.split_at(cut);
        let want = (cut >= body.len()).then(|| Body::whole(&body));
        prop_assert_eq!(framing.body(head, false, 64), Ok(want));
    }
}
