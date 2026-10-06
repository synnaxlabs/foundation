use super::*;

const TOKEN: &str = "Token s3cret";

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
fn takes_each_form_of_host() {
    for host in [
        "influx",
        "influx:8086",
        "10.0.0.1:8086",
        "[::1]:8086",
        "a-b.c_d",
    ] {
        let request = Request::new(host, "/", &[("X-A", "1"), ("X-B", "1")]);
        assert!(request.is_ok(), "{host}");
    }
}

#[test]
fn shows_no_header_value_in_debug() {
    let request = Request::new("h", "/w", &[("Authorization", TOKEN)]).unwrap();
    let debug = format!("{request:?}");
    assert_eq!(debug, "Request { target: \"/w\", .. }");
}

#[test]
fn refuses_a_target_that_is_not_origin_form() {
    for target in ["w", "/a b", "/a\r\nb", "/a#b", "/a\x7f", "/a\u{85}"] {
        let got = Request::new("h", target, &[]).err();
        assert_eq!(got, Some(Error::Target(target.into())));
    }
    assert_eq!(
        Error::Target("/a\r\nb".into()).to_string(),
        "the target \"/a\\r\\nb\" is not an origin-form target"
    );
}

#[test]
fn refuses_a_host_that_is_not_a_host_and_port() {
    for host in ["", "h\n", "a b", "u@h", "h/p", "caf\u{e9}"] {
        let got = Request::new(host, "/", &[]).err();
        assert_eq!(got, Some(Error::Host(host.into())));
    }
    assert_eq!(
        Error::Host("h\n".into()).to_string(),
        "the host \"h\\n\" is not a host with an optional port"
    );
}

#[test]
fn refuses_a_header_http_cannot_carry() {
    let cases = [
        (
            Request::new("h", "/", &[("Authorization", "Token a\r\nX: y")]),
            Error::Value("Authorization".into()),
            "the value of the header \"Authorization\" holds a byte that is not \
             visible ASCII, a space, or a tab",
        ),
        (
            Request::new("h", "/", &[("X", "a\x7f")]),
            Error::Value("X".into()),
            "the value of the header \"X\" holds a byte that is not visible ASCII, a \
             space, or a tab",
        ),
        (
            Request::new("h", "/", &[("X", "caf\u{e9}")]),
            Error::Value("X".into()),
            "the value of the header \"X\" holds a byte that is not visible ASCII, a \
             space, or a tab",
        ),
        (
            Request::new("h", "/", &[("Authorization", "a"), ("authorization", "b")]),
            Error::Duplicate("authorization".into()),
            "the header \"authorization\" is given twice",
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
