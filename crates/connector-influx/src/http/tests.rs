#![expect(clippy::arithmetic_side_effects, reason = "a test may panic")]

use proptest::prelude::*;

use super::*;

const TOKEN: &str = "Token s3cret";

/// Response headers, as the head parser gives them.
type Headers<'a> = &'a [(&'a str, &'a [u8])];

#[test]
fn writes_one_known_request() {
    let request = Request::new(
        "influx:8086",
        "/api/v2/write?bucket=b&precision=ns",
        &[("Authorization", TOKEN), ("Content-Type", "text/plain\tx")],
    )
    .unwrap();
    let mut out = b"x".to_vec();
    request.write(&mut out, b"m f=1i 0\n");
    assert_eq!(
        std::str::from_utf8(&out).unwrap(),
        "xPOST /api/v2/write?bucket=b&precision=ns HTTP/1.1\r\n\
         Host: influx:8086\r\n\
         Authorization: Token s3cret\r\n\
         Content-Type: text/plain\tx\r\n\
         Content-Length: 9\r\n\
         \r\n\
         m f=1i 0\n"
    );
}

#[test]
fn shows_no_header_value_in_debug() {
    let request = Request::new("h", "/w", &[("Authorization", TOKEN)]).unwrap();
    let debug = format!("{request:?}");
    assert_eq!(debug, "Request { target: \"/w\", .. }");
}

#[test]
fn refuses_a_request_http_cannot_carry() {
    let cases = [
        (
            Request::new("h", "w", &[]),
            Error::Target("w".into()),
            "the target \"w\" does not start with '/' or holds a space or a control \
             character",
        ),
        (
            Request::new("h", "/a b", &[]),
            Error::Target("/a b".into()),
            "the target \"/a b\" does not start with '/' or holds a space or a control \
             character",
        ),
        (
            Request::new("h", "/a\r\nb", &[]),
            Error::Target("/a\r\nb".into()),
            "the target \"/a\\r\\nb\" does not start with '/' or holds a space or a \
             control character",
        ),
        (
            Request::new("h\n", "/", &[]),
            Error::Value("Host".into()),
            "the value of the header \"Host\" holds a control character",
        ),
        (
            Request::new("h", "/", &[("Authorization", "Token a\r\nX: y")]),
            Error::Value("Authorization".into()),
            "the value of the header \"Authorization\" holds a control character",
        ),
        (
            Request::new("h", "/", &[("A B", "v")]),
            Error::Name("A B".into()),
            "the header name \"A B\" is not a token",
        ),
        (
            Request::new("h", "/", &[("", "v")]),
            Error::Name(String::new()),
            "the header name \"\" is not a token",
        ),
    ];
    for (got, error, message) in cases {
        assert_eq!(got.err(), Some(error.clone()));
        assert_eq!(error.to_string(), message);
        assert!(!message.contains("Token a"), "no message holds a value");
    }
}

#[test]
fn refuses_a_header_the_client_writes_or_never_sends() {
    for name in ["host", "Content-Length", "TRANSFER-ENCODING", "Expect"] {
        let got = Request::new("h", "/", &[(name, "v")]).err();
        assert_eq!(got, Some(Error::Reserved(name.into())));
    }
    assert_eq!(
        Error::Reserved("Expect".into()).to_string(),
        "the client does not take the header \"Expect\""
    );
}

#[test]
fn gives_no_body_to_a_status_that_has_none() {
    let length: Headers<'_> = &[("Content-Length", b"5")];
    for status in [100, 101, 199, 204, 304] {
        assert_eq!(Framing::new(status, length), Ok(Framing::Empty));
    }
    assert_eq!(Framing::new(200, length), Ok(Framing::Length(5)));
    assert_eq!(Framing::new(205, length), Ok(Framing::Length(5)));
    assert_eq!(Framing::new(400, &[]), Ok(Framing::Close));
    assert_eq!(
        Framing::Empty.body(b"HTTP/1.1 200", false, 0),
        Ok(Some(&b""[..])),
        "the bytes after a 204 belong to the next response"
    );
}

#[test]
fn refuses_framing_that_is_ambiguous() {
    let cases: [(Headers<'_>, Error, &str); 7] = [
        (
            &[("content-length", b"1"), ("Transfer-Encoding", b"chunked")],
            Error::Both,
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
    assert_eq!(Framing::Close.body(b"abc", true, 3), Ok(Some(&b"abc"[..])));
    assert_eq!(
        Framing::Close.body(b"abcd", false, 3),
        Err(Error::Large { cap: 3 })
    );
    assert_eq!(
        Error::Large { cap: 3 }.to_string(),
        "a response body over the cap of 3 bytes"
    );
}

#[test]
fn refuses_a_length_over_the_cap_before_it_arrives() {
    assert_eq!(
        Framing::Length(4).body(b"", false, 3),
        Err(Error::Large { cap: 3 })
    );
    assert_eq!(
        Framing::Length(4).body(b"abcde", false, 4),
        Ok(Some(&b"abcd"[..]))
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
        let want = if cut < body.len() { None } else { Some(body.as_slice()) };
        prop_assert_eq!(framing.body(head, false, 64), Ok(want));
    }
}
