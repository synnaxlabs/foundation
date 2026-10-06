#![expect(clippy::arithmetic_side_effects, reason = "a test may panic")]

use proptest::prelude::*;

use super::*;

const CAP: usize = 1024;

/// Header fields by name and value.
type Fields<'a> = &'a [(&'a str, &'a [u8])];

/// Reads `bytes` with the stream open and a cap of 1 KiB.
fn open(bytes: &[u8]) -> Result<Option<Response<'_>>, Error> {
    read(bytes, false, CAP)
}

/// The framing of a response with `status` and `headers`.
fn framing(status: u16, headers: Fields<'_>) -> Result<Framing, Error> {
    let headers: Vec<Header<'_>> = headers
        .iter()
        .map(|&(name, value)| Header { name, value })
        .collect();
    Framing::new(status, &headers)
}

#[test]
fn reads_the_204_influx_gives_a_write() {
    let head = b"HTTP/1.1 204 No Content\r\n\
        X-Influxdb-Build: OSS\r\n\
        X-Influxdb-Version: v2.7.11\r\n\
        Date: Tue, 06 Oct 2026 17:00:00 GMT\r\n\r\n";
    let mut bytes = head.to_vec();
    bytes.extend_from_slice(b"HTTP/1.1 204");
    let want = Response {
        status: 204,
        body: Cow::Borrowed(b""),
        used: head.len(),
        last: false,
    };
    assert_eq!(open(&bytes), Ok(Some(want)));
}

#[test]
fn skips_an_interim_response() {
    let bytes =
        b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early Hints\r\nLink: x\r\n\r\n\
        HTTP/1.1 204 No Content\r\n\r\n";
    let got = open(bytes).unwrap().unwrap();
    assert_eq!((got.status, got.used), (204, bytes.len()));
    assert_eq!(open(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 2"), Ok(None));
}

#[test]
fn reads_a_known_chunked_body() {
    let response = b"HTTP/1.1 400 Bad Request\r\n\
        Transfer-Encoding: Chunked\r\n\
        Connection: keep-alive, Close\r\n\r\n\
        5;ext=\"a\"\r\nhello\r\n6\r\n world\r\n0\r\nX-Trailer: t\r\n\r\n";
    let mut bytes = response.to_vec();
    bytes.extend_from_slice(b"extra");
    let want = Response {
        status: 400,
        body: Cow::Owned(b"hello world".to_vec()),
        used: response.len(),
        last: true,
    };
    assert_eq!(open(&bytes), Ok(Some(want)));
}

#[test]
fn reads_a_body_that_ends_at_the_close() {
    let bytes = b"HTTP/1.1 500 Internal Server Error\r\n\r\nfailed";
    assert_eq!(open(bytes), Ok(None));
    let got = read(bytes, true, CAP).unwrap().unwrap();
    assert_eq!(
        (&got.body[..], got.used, got.last),
        (&b"failed"[..], bytes.len(), true)
    );
}

#[test]
fn reads_a_body_that_ends_at_the_close_only_under_the_cap() {
    let bytes = b"HTTP/1.1 500 Internal Server Error\r\n\r\nfailed";
    let cap = bytes.len();
    assert!(read(bytes, true, cap).unwrap().is_some());
    assert_eq!(read(bytes, false, cap), Ok(None));
    assert_eq!(
        read(bytes, false, cap - 1),
        Err(Error::Oversize { cap: cap - 1 })
    );
    let head = b"HTTP/1.1 204 No";
    assert_eq!(read(head, false, head.len()), Ok(None));
}

#[test]
fn refuses_a_chunked_body_that_is_not_valid() {
    let head = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
    for body in [
        &b"x\r\n"[..],
        b"\r\n",
        b";a\r\n0\r\n\r\n",
        b"5\r\nhelloXX",
        b"5\r\nhello\r\n0\r\nbad trailer\r\n\r\n",
        b"10000000000000000\r\n",
    ] {
        let mut bytes = head.to_vec();
        bytes.extend_from_slice(body);
        assert_eq!(open(&bytes), Err(Error::Chunk), "{body:?}");
    }
    assert_eq!(Error::Chunk.to_string(), "a chunked body that is not valid");
}

#[test]
fn refuses_a_head_that_is_not_http_1_1() {
    let many = format!("HTTP/1.1 200 OK\r\n{}\r\n", "A: b\r\n".repeat(33));
    let cases = [
        (
            &b"HTTP/1.0 200 OK\r\n\r\n"[..],
            Error::Version,
            "a response that is not HTTP/1.1",
        ),
        (
            b"HTTP/1.1 2x0 OK\r\n\r\n",
            Error::Head(httparse::Error::Status),
            "a response head that is not valid: invalid response status",
        ),
        (
            many.as_bytes(),
            Error::Head(httparse::Error::TooManyHeaders),
            "a response head that is not valid: too many headers",
        ),
    ];
    for (bytes, error, message) in cases {
        assert_eq!(open(bytes), Err(error.clone()));
        assert_eq!(error.to_string(), message);
    }
}

#[test]
fn refuses_a_stream_that_closes_inside_a_response() {
    for bytes in [
        &b""[..],
        b"HTTP/1.1 204 No",
        b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nab",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nab",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n",
    ] {
        assert_eq!(open(bytes), Ok(None), "{bytes:?}");
        assert_eq!(read(bytes, true, CAP), Err(Error::Truncated), "{bytes:?}");
    }
    assert_eq!(
        Error::Truncated.to_string(),
        "the stream closed before the response ended"
    );
}

#[test]
fn counts_every_byte_against_the_cap() {
    let head = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
    let mut bytes = head.to_vec();
    bytes.extend_from_slice(b"1\r\na\r\n1\r\nb\r\n0\r\n\r\n");
    let cap = bytes.len();
    assert!(read(&bytes, false, cap).unwrap().is_some());
    assert_eq!(
        read(&bytes, false, cap - 1),
        Err(Error::Oversize { cap: cap - 1 })
    );

    let length = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n";
    let cap = length.len() + 3;
    assert_eq!(read(length, false, cap), Err(Error::Oversize { cap }));
    assert_eq!(
        read(b"HTTP/1.1 200 OK\r\nA: b", false, 20),
        Err(Error::Oversize { cap: 20 })
    );
    let interim = b"HTTP/1.1 100 Continue\r\n\r\n";
    assert_eq!(
        read(interim, false, interim.len() - 1),
        Err(Error::Oversize {
            cap: interim.len() - 1
        })
    );
    assert_eq!(
        Error::Oversize { cap: 3 }.to_string(),
        "a response over the cap of 3 bytes"
    );
}

#[test]
fn gives_no_body_to_a_status_that_has_none() {
    let length: Fields<'_> = &[("Content-Length", b"5")];
    let encoding: Fields<'_> = &[("Transfer-Encoding", b"gzip")];
    for status in [100, 102, 103, 199] {
        assert_eq!(framing(status, length), Ok(Framing::Interim));
        assert_eq!(framing(status, encoding), Ok(Framing::Interim));
    }
    for status in [204, 304] {
        assert_eq!(framing(status, length), Ok(Framing::Empty));
        assert_eq!(framing(status, encoding), Ok(Framing::Empty));
    }
    assert_eq!(framing(200, length), Ok(Framing::Length(5)));
    assert_eq!(framing(205, length), Ok(Framing::Length(5)));
    assert_eq!(framing(400, &[]), Ok(Framing::Close));
    let bytes = b"HTTP/1.1 304 Not Modified\r\nContent-Length: 5\r\n\r\n";
    assert_eq!(open(bytes).unwrap().unwrap().used, bytes.len());
}

#[test]
fn refuses_a_switch_of_protocols() {
    assert_eq!(framing(101, &[]), Err(Error::Switch));
    assert_eq!(
        Error::Switch.to_string(),
        "a 101 response to a client that asked for no switch"
    );
}

#[test]
fn refuses_both_framings_on_any_status() {
    let both: Fields<'_> = &[("Content-Length", b"0"), ("Transfer-Encoding", b"x")];
    for status in [100, 101, 200, 204, 304] {
        assert_eq!(framing(status, both), Err(Error::Ambiguous));
    }
    assert_eq!(
        Error::Ambiguous.to_string(),
        "a response with both Content-Length and Transfer-Encoding"
    );
}

#[test]
fn refuses_framing_that_is_not_one_length_or_chunked() {
    let cases: [(Fields<'_>, Error, &str); 9] = [
        (
            &[("Content-Length", b"1"), ("Content-Length", b"1")],
            Error::Length("1, 1".into()),
            "the Content-Length \"1, 1\" is not one decimal number",
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
            &[("Transfer-Encoding", b"gzip, chunked")],
            Error::Encoding("gzip, chunked".into()),
            "the Transfer-Encoding \"gzip, chunked\" is not supported",
        ),
        (
            &[
                ("Transfer-Encoding", b"chunked"),
                ("transfer-encoding", b"chunked"),
            ],
            Error::Encoding("chunked, chunked".into()),
            "the Transfer-Encoding \"chunked, chunked\" is not supported",
        ),
        (
            &[("Transfer-Encoding", b"identity")],
            Error::Encoding("identity".into()),
            "the Transfer-Encoding \"identity\" is not supported",
        ),
    ];
    for (headers, error, message) in cases {
        assert_eq!(framing(200, headers), Err(error.clone()));
        assert_eq!(error.to_string(), message);
    }
    assert_eq!(
        framing(200, &[("transfer-encoding", b"CHUNKED")]),
        Ok(Framing::Chunked)
    );
}

/// `body` cut at each of `cuts` into chunks, with a final chunk and no trailers.
fn encode(body: &[u8], cuts: &[prop::sample::Index]) -> Vec<u8> {
    let mut ends: Vec<usize> =
        cuts.iter().map(|cut| cut.index(body.len() + 1)).collect();
    ends.push(body.len());
    ends.sort_unstable();
    let mut out = Vec::new();
    let mut start = 0;
    for end in ends {
        if end > start {
            out.extend_from_slice(format!("{:x}\r\n", end - start).as_bytes());
            out.extend_from_slice(&body[start..end]);
            out.extend_from_slice(b"\r\n");
            start = end;
        }
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

proptest! {
    #[test]
    fn reads_back_any_chunked_body(
        body in proptest::collection::vec(any::<u8>(), 0..256),
        cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..8),
    ) {
        let mut bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"
            .to_vec();
        bytes.extend_from_slice(&encode(&body, &cuts));
        let got = open(&bytes).unwrap().unwrap();
        prop_assert_eq!(&got.body[..], &body[..]);
        prop_assert_eq!(got.used, bytes.len());
    }

    #[test]
    fn waits_for_the_rest_of_any_response(
        body in proptest::collection::vec(any::<u8>(), 0..64),
        cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..4),
        chunked in any::<bool>(),
        cut in any::<prop::sample::Index>(),
    ) {
        let mut bytes = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\n".to_vec();
        if chunked {
            bytes.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
            bytes.extend_from_slice(&encode(&body, &cuts));
        } else {
            let length = format!("Content-Length: {}\r\n\r\n", body.len());
            bytes.extend_from_slice(length.as_bytes());
            bytes.extend_from_slice(&body);
        }
        let whole = open(&bytes).unwrap().unwrap();
        prop_assert_eq!(&whole.body[..], &body[..]);
        prop_assert_eq!(whole.used, bytes.len());
        let cut = cut.index(bytes.len());
        prop_assert_eq!(open(&bytes[..cut]), Ok(None));
    }
}
