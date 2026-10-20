use std::ffi::CString;
use std::future;
use std::net::SocketAddr;
use std::ptr;

use aws_lc_sys::{
    CBS, CBS_ASN1_CONSTRUCTED, CBS_ASN1_CONTEXT_SPECIFIC, CBS_ASN1_INTEGER,
    CBS_ASN1_OBJECT, CBS_ASN1_OCTETSTRING, CBS_ASN1_SEQUENCE, CBS_ASN1_SET,
    CBS_ASN1_TAG,
};
use types::time::{Span, Stamp};

use super::{Client, PORT, VALUE, fits, fitting, long, run, serve_on};
use crate::client::{Credential, Credentials, Error, Large, Mode, Refusal};
use crate::event;
use crate::ffi::{self, Status};
use crate::secure::{
    ANONYMOUS, BROKEN_TRUST, CLIENT, CLIENT_ONLY_SERVER, CLIENT_PEM, EC, EMPTY_URI,
    END, HEADED_TRUST, HIGH_PEM, Identity, LASTING, LINE_TRUST, LONG_BLOCK_TRUST,
    LONG_KEY, ONE_BLOCK_TRUST, OTHER, Policy, REJECTED_SERVER, SERVER, START, TRUST,
    TWO_URIS, UNNAMED,
};
use crate::server::Limits;
use crate::value::{Fault, Value};

/// The test server, which trusts [`CLIENT`].
const SERVED: Credentials<'_> = with(SERVER, CLIENT.certificate);

/// The test client, which trusts [`SERVER`].
const CLIENT_SIDE: Credentials<'_> = with(CLIENT, SERVER.certificate);

const MODES: [Mode; 2] = [Mode::Sign, Mode::SignAndEncrypt];

/// The credentials of `own` that trust `trust`.
const fn with(own: Identity, trust: &'static [u8]) -> Credentials<'static> {
    Credentials {
        certificate: own.certificate,
        key: own.key,
        trust,
    }
}

/// What a read of [`VALUE`] through a secure channel gives.
#[derive(Debug, PartialEq)]
struct Read {
    /// The value of [`VALUE`] on the server.
    value: Value,
    /// The answer of the read.
    answer: Result<Value, Fault>,
    /// The policy URI and the mode of the channel.
    channel: (String, ffi::Mode),
}

/// The policy URI and the mode of the channel of `client`, read from C: no call of
/// `Client` and no server log gives them, and a read alone also passes over None,
/// which the server offers too.
fn channel(client: &Client) -> (String, ffi::Mode) {
    let mut mode = ffi::Mode(0);
    // SAFETY: the client lives, and `mode` is a place to write.
    let policy =
        unsafe { ffi::test::shim_client_channel(client.raw.as_ptr(), &raw mut mode) };
    // SAFETY: open62541 gives a live string of the config of the client, or null.
    let policy = unsafe { policy.as_ref() }.expect("the client has a channel");
    // SAFETY: as above.
    (unsafe { policy.lossy() }, mode)
}

/// The connect of a client whose loop starts at `client_wall` with `client` that
/// trusts `trust`, and its read of [`VALUE`], to a server whose loop starts at
/// `server_wall` with `server`, which offers `policies` in that order, in `mode`, in a
/// run of `seed`, and the digest of the run.
fn connect(
    seed: u64,
    (server, server_wall): (Credentials<'static>, Stamp),
    (client, trust, client_wall): (Identity, impl AsRef<[u8]> + Send + 'static, Stamp),
    policies: &[Policy],
    mode: Mode,
) -> (Result<Read, Error>, u64) {
    let policies = policies.to_vec();
    run(seed, async move |node| {
        let client = Credentials {
            certificate: client.certificate,
            key: client.key,
            trust: trust.as_ref(),
        };
        let time = event::Time::since(node.clock(), server_wall);
        serve_on(
            &node,
            4,
            time,
            Some((&server, &policies)),
            async |mut peer| {
                let mut connected = peer.secure(mode, &client, client_wall).await?;
                let read = Read {
                    value: peer.value,
                    answer: connected.read_one(&VALUE).await,
                    channel: channel(&connected),
                };
                connected.close().await;
                Ok(read)
            },
        )
        .await
    })
}

/// A read from a server that offers `policies`, in `mode` at [`START`], in a run of
/// `seed`.
fn read(seed: u64, policies: &[Policy], mode: Mode) -> (Result<Read, Error>, u64) {
    connect(
        seed,
        (SERVED, START),
        (CLIENT, SERVER.certificate, START),
        policies,
        mode,
    )
}

#[test]
fn a_client_reads_with_each_policy_in_each_mode() {
    for policy in Policy::ALL {
        for mode in MODES {
            let (read, _) = read(0, &[policy], mode);
            let read = read.expect("the client connects");
            assert_eq!(read.answer, Ok(read.value), "{policy:?} in {mode:?}");
            assert_eq!(read.channel, (uri(policy), mode.into()));
        }
    }
}

/// The URI of `policy`.
fn uri(policy: Policy) -> String {
    policy.uri().to_str().expect("the URI is UTF-8").to_owned()
}

#[test]
fn a_client_takes_the_policy_of_the_highest_level() {
    for policies in [
        Policy::ALL.as_slice(),
        &[Policy::Aes256Sha256RsaPss, Policy::Basic256Sha256],
    ] {
        for mode in MODES {
            let (read, _) = read(0, policies, mode);
            let read = read.expect("the client connects");
            assert_eq!(read.answer, Ok(read.value), "{policies:?} in {mode:?}");
            let channel = (uri(Policy::Aes256Sha256RsaPss), mode.into());
            assert_eq!(read.channel, channel, "{policies:?}");
        }
    }
}

/// The connect of a client with [`CLIENT`] that trusts `trust`, in `Sign`.
fn trusting(trust: impl AsRef<[u8]> + Send + 'static) -> Result<Read, Error> {
    let (read, _) = connect(
        0,
        (SERVED, START),
        (CLIENT, trust, START),
        &Policy::ALL,
        Mode::Sign,
    );
    read
}

#[test]
fn a_client_that_trusts_a_pem_of_two_certificates_reads() {
    let read = trusting(TRUST).expect("the client connects");
    assert_eq!(read.answer, Ok(read.value));
}

#[test]
fn a_client_that_trusts_a_pem_after_a_header_reads() {
    let read = trusting(HEADED_TRUST).expect("the client connects");
    assert_eq!(read.answer, Ok(read.value));
}

#[test]
fn a_client_that_trusts_a_der_bundle_whose_text_holds_a_pem_line_reads() {
    let read = trusting(LINE_TRUST).expect("the client connects");
    assert_eq!(read.answer, Ok(read.value));
}

/// The two certificates of [`TRUST`], [`OTHER`] then [`SERVER`], each as its own PEM.
fn blocks() -> (String, String) {
    let trust = std::str::from_utf8(TRUST).expect("PEM is text");
    let at = trust[1..].find("-----BEGIN").expect("two blocks") + 1;
    (trust[..at].to_owned(), trust[at..].to_owned())
}

#[test]
fn a_client_that_trusts_a_pem_with_text_between_its_certificates_reads() {
    let (other, server) = blocks();
    let trust = format!("{other}Bag Attributes\n    friendlyName: server\n{server}");
    let read = trusting(trust).expect("the client connects");
    assert_eq!(read.answer, Ok(read.value));
}

#[test]
fn a_client_that_trusts_a_second_x509_certificate_block_reads() {
    let (other, server) = blocks();
    let server = server.replace(" CERTIFICATE-----", " X509 CERTIFICATE-----");
    let trust = format!("{other}{server}");
    let read = trusting(trust).expect("the client connects");
    assert_eq!(read.answer, Ok(read.value));
}

#[test]
fn a_client_that_trusts_a_second_trusted_certificate_block_reads() {
    let (other, server) = blocks();
    let server = server.replace(" CERTIFICATE-----", " TRUSTED CERTIFICATE-----");
    let trust = format!("{other}{server}");
    let read = trusting(trust).expect("the client connects");
    assert_eq!(read.answer, Ok(read.value));
}

#[test]
fn a_client_that_trusts_a_pem_past_4_gib_reads() {
    let (other, server) = blocks();
    // The group copies the trust, so the test writes all 4 GiB. A cut to 32 bits keeps
    // only `other`.
    let mut trust = vec![0; (1 << 32) + other.len()];
    trust[..other.len()].copy_from_slice(other.as_bytes());
    let at = trust.len() - server.len();
    trust[at - 1] = b'\n';
    trust[at..].copy_from_slice(server.as_bytes());
    let read = trusting(trust).expect("the client connects");
    assert_eq!(read.answer, Ok(read.value));
}

#[test]
fn a_client_that_trusts_a_pem_after_text_that_starts_with_a_zero_reads() {
    let trust = [b"0 1 2\n", TRUST].concat();
    let read = trusting(trust).expect("the client connects");
    assert_eq!(read.answer, Ok(read.value));
}

#[test]
fn a_client_with_a_pem_certificate_and_key_reads() {
    let (read, _) = connect(
        0,
        (SERVED, START),
        (CLIENT_PEM, SERVER.certificate, START),
        &Policy::ALL,
        Mode::Sign,
    );
    let read = read.expect("the client connects");
    assert_eq!(read.answer, Ok(read.value));
}

#[test]
fn a_der_key_past_100_kib_reads() {
    let (read, _) = connect(
        0,
        (SERVED, START),
        (LONG_KEY, SERVER.certificate, START),
        &Policy::ALL,
        Mode::Sign,
    );
    let read = read.expect("the client connects");
    assert_eq!(read.answer, Ok(read.value));
}

/// The contents of the DER element of `tag` at the start of `der`, which it takes.
fn element(der: &mut CBS, tag: i64) -> CBS {
    let tag = CBS_ASN1_TAG::try_from(tag).expect("a tag of aws-lc");
    let mut out = CBS {
        data: ptr::null(),
        len: 0,
    };
    // SAFETY: `der` points to live bytes, and `out` to a place to write.
    let found = unsafe { aws_lc_sys::CBS_get_asn1(der, &raw mut out, tag) };
    assert_eq!(found, 1, "an element of tag {tag:#x} that fits its parent");
    out
}

/// The key read skips what the attributes hold, so only a strict read sees the lengths
/// inside them.
#[test]
fn the_long_key_nests_its_pad_in_one_attribute() {
    let mut der = CBS {
        data: LONG_KEY.key.as_ptr(),
        len: LONG_KEY.key.len(),
    };
    let mut info = element(&mut der, CBS_ASN1_SEQUENCE.into());
    for tag in [CBS_ASN1_INTEGER, CBS_ASN1_SEQUENCE, CBS_ASN1_OCTETSTRING] {
        element(&mut info, tag.into());
    }
    let attributes = i64::from(CBS_ASN1_CONSTRUCTED) | CBS_ASN1_CONTEXT_SPECIFIC;
    let mut attributes = element(&mut info, attributes);
    let mut attribute = element(&mut attributes, CBS_ASN1_SEQUENCE.into());
    element(&mut attribute, CBS_ASN1_OBJECT.into());
    let mut values = element(&mut attribute, CBS_ASN1_SET.into());
    let pad = element(&mut values, CBS_ASN1_OCTETSTRING.into());
    assert!(pad.len >= 100 << 10, "a pad of {} bytes", pad.len);
    for rest in [der, info, attributes, attribute, values] {
        assert_eq!(rest.len, 0);
    }
}

/// The refusal of a trust whose certificate `number` does not parse.
fn invalid(number: usize) -> Error {
    Error::Refused(Refusal::Invalid { number })
}

/// The refusal of a trust with a PEM block that does not read after `after`
/// certificates.
fn damaged(after: usize) -> Error {
    Error::Refused(Refusal::Damaged { after })
}

/// The refusal of a trust whose block of certificate `number` holds bytes after it.
fn trailing(number: usize) -> Error {
    Error::Refused(Refusal::Trailing { number })
}

#[test]
fn a_trust_whose_second_certificate_does_not_parse_ends_the_connect() {
    assert_eq!(trusting(BROKEN_TRUST), Err(invalid(2)));
}

#[test]
fn a_trust_whose_second_block_has_no_end_line_ends_the_connect() {
    let (other, _) = blocks();
    let trust = format!("{other}-----BEGIN CERTIFICATE-----\nMIIB\n");
    assert_eq!(trusting(trust), Err(damaged(1)));
}

#[test]
fn a_trust_with_a_damaged_or_empty_block_of_another_label_ends_the_connect() {
    let (other, _) = blocks();
    for body in ["!!!!\n", ""] {
        let trust = format!("{other}-----BEGIN NOTE-----\n{body}-----END NOTE-----\n");
        assert_eq!(trusting(trust), Err(damaged(1)), "{body:?}");
    }
}

#[test]
fn a_damaged_block_between_two_certificates_names_the_certificate_before_it() {
    let (other, server) = blocks();
    let key = "-----BEGIN PRIVATE KEY-----\n!!!!\n-----END PRIVATE KEY-----\n";
    assert_eq!(trusting(format!("{other}{key}{server}")), Err(damaged(1)));
}

#[test]
fn a_damaged_block_before_each_certificate_names_none() {
    let (other, _) = blocks();
    let note = "-----BEGIN NOTE-----\n!!!!\n-----END NOTE-----\n";
    assert_eq!(trusting(format!("{note}{other}")), Err(damaged(0)));
}

#[test]
fn a_trust_whose_begin_line_starts_no_block_drops_it() {
    let (other, server) = blocks();
    for begin in [
        "----BEGIN CERTIFICATE-----",
        "-----BEGIN CERTIFICATE----",
        "-----BEGIN CERTIFICATE-----\u{A0}x",
    ] {
        let server = server.replacen("-----BEGIN CERTIFICATE-----", begin, 1);
        assert_eq!(
            trusting(format!("{other}{server}")),
            Err(Error::Failed(Status::BAD_CERTIFICATE_UNTRUSTED)),
            "{begin}"
        );
    }
}

#[test]
fn a_begin_line_at_a_piece_of_253_bytes_of_a_long_line_starts_a_block() {
    let (other, server) = blocks();
    let untrusted = trusting(format!("{other}{}{server}", "x".repeat(252)));
    assert_eq!(
        untrusted.map(|_| ()),
        Err(Error::Failed(Status::BAD_CERTIFICATE_UNTRUSTED))
    );
    for text in [253, 506] {
        let read = trusting(format!("{other}{}{server}", "x".repeat(text)))
            .expect("the client connects");
        assert_eq!(read.answer, Ok(read.value), "{text}");
    }
}

#[test]
fn a_begin_line_with_white_space_after_its_dashes_starts_a_block() {
    let (other, server) = blocks();
    for tail in [" ", "\t", "\r"] {
        let begin = format!("-----BEGIN CERTIFICATE-----{tail}");
        let server = server.replacen("-----BEGIN CERTIFICATE-----", &begin, 1);
        let read = trusting(format!("{other}{server}")).expect("the client connects");
        assert_eq!(read.answer, Ok(read.value), "{tail:?}");
    }
}

#[test]
fn a_trust_whose_end_line_names_another_label_ends_the_connect() {
    let (other, server) = blocks();
    for (line, with) in [
        (
            "-----BEGIN CERTIFICATE-----",
            "-----BEGIN CERTIFICATE------",
        ),
        (
            "-----BEGIN CERTIFICATE-----",
            "-----BEGIN  CERTIFICATE-----",
        ),
        (
            "-----END CERTIFICATE-----",
            "-----END CERTIFICATE-----\u{A0}x",
        ),
    ] {
        let server = server.replacen(line, with, 1);
        assert_eq!(
            trusting(format!("{other}{server}")),
            Err(damaged(1)),
            "{with}"
        );
    }
}

/// The `Line` refusal of line `number` of the trust.
fn line(number: usize) -> Error {
    Error::Refused(Refusal::Line {
        credential: Credential::Trust,
        number,
    })
}

/// [`TRUST`] with `tail` before the `\n` of line `at`, from 0, of its second block, and
/// the number from 1 of that line in the trust.
fn with_tail(at: usize, tail: &[u8]) -> (Vec<u8>, usize) {
    let (other, server) = blocks();
    let number = other.matches('\n').count() + at + 1;
    let mut trust = other.into_bytes();
    for (index, line) in server.split_inclusive('\n').enumerate() {
        let line = line.as_bytes();
        if index == at {
            trust.extend_from_slice(&line[..line.len() - 1]);
            trust.extend_from_slice(tail);
            trust.push(b'\n');
        } else {
            trust.extend_from_slice(line);
        }
    }
    (trust, number)
}

#[test]
fn a_line_of_a_block_with_a_high_byte_after_its_text_ends_the_connect() {
    let last = blocks().1.lines().count() - 1;
    for at in [0, 1, 2, last] {
        for tail in [b"\xA0".as_slice(), b" \xFF\r", b"\x80 "] {
            let (trust, number) = with_tail(at, tail);
            assert_eq!(trusting(trust), Err(line(number)), "{at} {tail:?}");
        }
    }
}

#[test]
fn a_line_of_a_block_of_only_a_high_byte_ends_the_connect() {
    let (trust, number) = with_tail(0, b"\n\xA0");
    assert_eq!(trusting(trust), Err(line(number + 1)));
}

#[test]
fn a_trust_whose_last_byte_is_high_ends_the_connect() {
    let (mut trust, number) = with_tail(blocks().1.lines().count() - 1, b"\xA0");
    assert_eq!(trust.pop(), Some(b'\n'));
    assert_eq!(trusting(trust), Err(line(number)));
}

#[test]
fn a_begin_line_with_a_high_byte_at_a_piece_of_a_long_line_ends_the_connect() {
    let (trust, number) = with_tail(0, b"\xA0");
    let at = blocks().0.len();
    let text = "x".repeat(253);
    let trust = [&trust[..at], text.as_bytes(), &trust[at..]].concat();
    assert_eq!(trusting(trust), Err(line(number)));
}

#[test]
fn a_begin_line_whose_label_holds_a_high_byte_starts_a_block() {
    let (other, server) = blocks();
    let begin = "-----BEGIN CERTIFICATE-----\u{A0}-----";
    let server = server.replacen("-----BEGIN CERTIFICATE-----", begin, 1);
    assert_eq!(trusting(format!("{other}{server}")), Err(damaged(1)));
    let end = "-----END CERTIFICATE-----\u{A0}-----";
    let server = server.replacen("-----END CERTIFICATE-----", end, 1);
    assert_eq!(
        trusting(format!("{other}{server}")).map(|_| ()),
        Err(Error::Failed(Status::BAD_CERTIFICATE_UNTRUSTED))
    );
}

#[test]
fn a_client_that_trusts_a_pem_with_utf_8_text_outside_its_blocks_reads() {
    let (other, server) = blocks();
    let long = "é".repeat(200);
    for trust in [
        format!("{other}Certigna é\n{server}"),
        format!("{other}-----BEGIN A NOTE é\na note that ends in -----é\n{server}"),
        format!("{long}\n{other}{long}\n{server}{long}"),
    ] {
        let read = trusting(trust).expect("the client connects");
        assert_eq!(read.answer, Ok(read.value));
    }
}

#[test]
fn a_begin_line_with_a_nul_before_its_dashes_is_text() {
    let trust = [b"-----BEGIN X\0-----\xA0\n", TRUST].concat();
    let read = trusting(trust).expect("the client connects");
    assert_eq!(read.answer, Ok(read.value));
}

#[test]
fn a_begin_line_with_no_label_and_a_high_byte_ends_the_connect() {
    let trust = [b"-----BEGIN -----\xA0\n", TRUST].concat();
    assert_eq!(trusting(trust), Err(line(1)));
}

#[test]
fn a_begin_line_with_no_space_or_four_dashes_starts_no_block() {
    for begin in [
        b"-----BEGINX-----".as_slice(),
        b"-----BEGIN X----",
        b"-----BEGIN X----Y",
    ] {
        let trust = [begin, b"\nnote \xA0\n", TRUST].concat();
        let read = trusting(trust).expect("the client connects");
        assert_eq!(read.answer, Ok(read.value), "{begin:?}");
    }
}

#[test]
fn an_end_line_of_10_bytes_ends_a_block() {
    let trust = [b"-----BEGIN X-----\n-----END X\nnote \xA0\n", TRUST].concat();
    assert_eq!(trusting(trust), Err(damaged(0)));
}

#[test]
fn an_end_line_with_no_space_ends_no_block() {
    let block = b"-----BEGIN X-----\n-----ENDXYZ\nnote \xA0\n-----END X-----\n";
    assert_eq!(trusting([block, TRUST].concat()), Err(line(3)));
}

#[test]
fn a_trust_that_does_not_parse_ends_the_connect() {
    assert_eq!(
        trusting(b"no certificate"),
        Err(Error::Refused(Refusal::Empty))
    );
}

#[test]
fn a_trust_with_a_byte_after_its_der_certificate_ends_the_connect() {
    let trust = [SERVER.certificate, b"\n"].concat();
    assert_eq!(trusting(trust), Err(invalid(2)));
}

#[test]
fn a_trust_of_der_then_pem_ends_the_connect() {
    let (_, server) = blocks();
    let trust = [OTHER.certificate, server.as_bytes()].concat();
    assert_eq!(trusting(trust), Err(invalid(2)));
}

#[test]
fn a_trust_of_a_damaged_der_then_pem_drops_the_der() {
    let (other, _) = blocks();
    let mut damaged = SERVER.certificate.to_vec();
    assert_eq!(damaged[4], 0x30, "the to-be-signed part is a sequence");
    damaged[4] = 0x31;
    let trust = [&damaged, b"\n".as_slice(), other.as_bytes()].concat();
    assert_eq!(
        trusting(trust),
        Err(Error::Failed(Status::BAD_CERTIFICATE_UNTRUSTED))
    );
}

#[test]
fn a_trust_of_pem_then_der_drops_the_der() {
    let (other, _) = blocks();
    let trust = [other.as_bytes(), SERVER.certificate].concat();
    assert_eq!(
        trusting(trust),
        Err(Error::Failed(Status::BAD_CERTIFICATE_UNTRUSTED))
    );
}

#[test]
fn a_trust_of_two_certificates_in_one_block_ends_the_connect() {
    assert_eq!(trusting(ONE_BLOCK_TRUST), Err(trailing(1)));
}

#[test]
fn a_trust_of_a_block_with_a_byte_after_its_certificate_ends_the_connect() {
    let (other, _) = blocks();
    let trust = [LONG_BLOCK_TRUST, other.as_bytes()].concat();
    for trust in [LONG_BLOCK_TRUST.to_vec(), trust] {
        assert_eq!(trusting(trust), Err(trailing(1)));
    }
}

#[test]
fn a_trusted_certificate_block_with_trust_settings_ends_the_connect() {
    for settings in [REJECTED_SERVER, CLIENT_ONLY_SERVER] {
        let (other, _) = blocks();
        let trust = [other.as_bytes(), settings].concat();
        assert_eq!(trusting(trust), Err(trailing(2)));
    }
}

#[test]
fn an_empty_trust_ends_the_connect() {
    assert_eq!(trusting(b""), Err(Error::Refused(Refusal::Empty)));
}

/// A server loads its trust through the group alone, which refuses each trust that
/// the client refuses before the dial.
#[test]
fn a_group_whose_trust_does_not_load_is_refused() {
    let trusts = [
        b"".as_slice(),
        b"no certificate",
        BROKEN_TRUST,
        ONE_BLOCK_TRUST,
        &with_tail(0, b"\xA0").0,
    ];
    for trust in trusts {
        let status = crate::validity::verify(SERVER.certificate, Some(trust), START);
        assert_eq!(status.name(), "BadInternalError", "{trust:?}");
    }
}

/// The connect of a client with `own` that trusts [`SERVER`], in `Sign`, to a port
/// where no server listens, so that a refusal before the dial is the only error that
/// is not one of the dial.
fn refused(own: Identity) -> Result<(), Error> {
    refused_at(own, SERVER.certificate, START)
}

/// [`refused`] with a trust of `trust`, on a loop whose wall time starts at `wall`.
fn refused_at(own: Identity, trust: &'static [u8], wall: Stamp) -> Result<(), Error> {
    let (connect, _) = run(0, async move |node| {
        let client = with(own, trust);
        let mut rng = node.entropy().rng();
        let url = format!("opc.tcp://{}", SocketAddr::new(node.addresses()[0], PORT));
        let url = CString::new(url).expect("the URL holds no NUL");
        let connect = Client::connect(
            event::Time::since(node.clock(), wall),
            tracing::Span::none(),
            node.net(),
            &mut rng,
            &url,
            Some((&client, Mode::Sign)),
            future::pending(),
        );
        connect.await.map(|_| ())
    });
    connect
}

#[test]
fn a_certificate_is_valid_only_inside_its_window_on_the_loop() {
    let dials = Err(Error::Failed(Status::BAD_CONNECTION_REJECTED));
    let refused = Err(Error::Refused(Refusal::Time));
    for (wall, connect) in [
        (START - Span::NANOSECOND, &refused),
        (START, &dials),
        (END - Span::NANOSECOND, &dials),
        (END, &refused),
    ] {
        let connect_at = refused_at(CLIENT, SERVER.certificate, wall);
        assert_eq!(&connect_at, connect, "{wall:?}");
    }
}

#[test]
fn a_refusal_names_the_part_that_fails() {
    for (refusal, text) in [
        (
            Refusal::Certificate,
            "the certificate does not parse or holds no RSA key",
        ),
        (Refusal::Key, "the key does not parse"),
        (
            Refusal::Mismatch,
            "the key is not the key of the certificate",
        ),
        (
            Refusal::Time,
            "the time is outside the validity window of the certificate",
        ),
        (
            Refusal::Empty,
            "the trust holds no certificate in DER or PEM",
        ),
        (
            Refusal::Invalid { number: 2 },
            "certificate 2 of the trust does not parse",
        ),
        (
            Refusal::Damaged { after: 1 },
            "a PEM block after certificate 1 of the trust is damaged",
        ),
        (
            Refusal::Damaged { after: 0 },
            "a PEM block of the trust before its first certificate is damaged",
        ),
        (
            Refusal::Trailing { number: 3 },
            "the PEM block of certificate 3 of the trust holds bytes after it",
        ),
        (
            Refusal::Line {
                credential: Credential::Trust,
                number: 4,
            },
            "line 4 of the trust, in a PEM block, holds a byte of 0x80 to 0xFF, which \
             PEM does not allow",
        ),
        (
            Refusal::Line {
                credential: Credential::Certificate,
                number: 2,
            },
            "line 2 of the certificate, in a PEM block, holds a byte of 0x80 to 0xFF, \
             which PEM does not allow",
        ),
        (
            Refusal::Line {
                credential: Credential::Key,
                number: 3,
            },
            "line 3 of the key, in a PEM block, holds a byte of 0x80 to 0xFF, which \
             PEM does not allow",
        ),
        (
            Refusal::Uri,
            "the certificate holds no URI in its subject alternative name",
        ),
    ] {
        assert_eq!(refusal.to_string(), text);
    }
}

#[test]
fn a_certificate_that_does_not_parse_ends_the_connect_before_the_dial() {
    let own = Identity {
        certificate: b"no certificate",
        ..CLIENT
    };
    assert_eq!(refused(own), Err(Error::Refused(Refusal::Certificate)));
}

/// The `Line` refusal of line 2 of `credential`.
fn second_line(credential: Credential) -> Result<(), Error> {
    Err(Error::Refused(Refusal::Line {
        credential,
        number: 2,
    }))
}

#[test]
fn a_pem_certificate_with_a_high_byte_ends_the_connect_before_the_dial() {
    let own = Identity {
        certificate: HIGH_PEM.certificate,
        ..CLIENT_PEM
    };
    assert_eq!(refused(own), second_line(Credential::Certificate));
}

#[test]
fn a_pem_key_with_a_high_byte_ends_the_connect_before_the_dial() {
    let own = Identity {
        key: HIGH_PEM.key,
        ..CLIENT_PEM
    };
    assert_eq!(refused(own), second_line(Credential::Key));
}

#[test]
fn a_certificate_with_no_rsa_key_ends_the_connect_before_the_dial() {
    assert_eq!(refused(EC), Err(Error::Refused(Refusal::Certificate)));
}

#[test]
fn a_key_that_does_not_parse_ends_the_connect_before_the_dial() {
    let own = Identity {
        key: b"no key",
        ..CLIENT
    };
    assert_eq!(refused(own), Err(Error::Refused(Refusal::Key)));
}

#[test]
fn a_key_of_another_certificate_ends_the_connect_before_the_dial() {
    let own = Identity {
        key: OTHER.key,
        ..CLIENT
    };
    assert_eq!(refused(own), Err(Error::Refused(Refusal::Mismatch)));
}

#[test]
fn a_certificate_with_no_application_uri_ends_the_connect_before_the_dial() {
    assert_eq!(refused(ANONYMOUS), Err(Error::Refused(Refusal::Uri)));
}

#[test]
fn a_certificate_with_no_alternative_name_ends_the_connect_before_the_dial() {
    assert_eq!(refused(UNNAMED), Err(Error::Refused(Refusal::Uri)));
}

#[test]
fn a_certificate_whose_first_uri_is_empty_ends_the_connect_before_the_dial() {
    assert_eq!(refused(EMPTY_URI), Err(Error::Refused(Refusal::Uri)));
}

#[test]
fn a_client_sends_the_first_uri_of_its_certificate() {
    let server = with(SERVER, TWO_URIS.certificate);
    let client = with(TWO_URIS, SERVER.certificate);
    let (uri, _) = run(0, async move |node| {
        let time = event::Time::since(node.clock(), START);
        serve_on(
            &node,
            4,
            time,
            Some((&server, Policy::ALL.as_slice())),
            async |mut peer| {
                let connected = peer.secure(Mode::Sign, &client, START).await;
                let connected = connected.expect("the client connects");
                // SAFETY: the client lives.
                let uri = unsafe { ffi::test::shim_client_uri(connected.raw.as_ptr()) };
                // SAFETY: open62541 gives a live string of the config of the client.
                let uri = unsafe { uri.as_ref() }.expect("the config holds a URI");
                // SAFETY: as above.
                let uri = unsafe { uri.lossy() };
                connected.close().await;
                uri
            },
        )
        .await
    });
    assert_eq!(uri, "urn:foundation:test:client");
}

#[test]
fn a_client_with_a_valid_certificate_dials() {
    assert_eq!(
        refused(CLIENT),
        Err(Error::Failed(Status::BAD_CONNECTION_REJECTED))
    );
}

#[test]
fn a_read_in_sign_and_encrypt_replays() {
    let spike = |seed| read(seed, &[Policy::Aes256Sha256RsaPss], Mode::SignAndEncrypt);
    let (first, digest) = spike(7);
    let first = first.expect("the client connects");
    let value = first.value;
    assert_eq!(first.answer, Ok(value));
    assert_eq!(spike(7), (Ok(first), digest));
    let (other, _) = spike(8);
    let other = other.expect("the client connects");
    assert_eq!(other.answer, Ok(other.value));
    assert_ne!(other.value, value);
}

#[test]
fn a_server_certificate_that_the_client_does_not_trust_ends_the_connect() {
    let other = with(OTHER, CLIENT.certificate);
    let (read, _) = connect(
        0,
        (other, START),
        (CLIENT, SERVER.certificate, START),
        &Policy::ALL,
        Mode::Sign,
    );
    assert_eq!(read, Err(Error::Failed(Status::BAD_CERTIFICATE_UNTRUSTED)));
}

#[test]
fn a_client_past_the_end_of_the_server_certificate_ends_the_connect() {
    let (read, _) = connect(
        0,
        (with(SERVER, LASTING.certificate), START),
        (LASTING, SERVER.certificate, END + Span::SECOND),
        &Policy::ALL,
        Mode::Sign,
    );
    assert_eq!(
        read,
        Err(Error::Failed(Status::BAD_CERTIFICATE_TIME_INVALID))
    );
}

#[test]
fn a_client_one_second_before_the_end_of_the_server_certificate_reads() {
    let (read, _) = connect(
        0,
        (with(SERVER, LASTING.certificate), START),
        (LASTING, SERVER.certificate, END - Span::SECOND),
        &Policy::ALL,
        Mode::Sign,
    );
    let read = read.expect("the client connects");
    assert_eq!(read.answer, Ok(read.value));
}

#[test]
fn a_client_one_second_before_the_start_of_the_server_certificate_ends_the_connect() {
    let (read, _) = connect(
        0,
        (with(SERVER, LASTING.certificate), START),
        (LASTING, SERVER.certificate, START - Span::SECOND),
        &Policy::ALL,
        Mode::Sign,
    );
    assert_eq!(
        read,
        Err(Error::Failed(Status::BAD_CERTIFICATE_TIME_INVALID))
    );
}

#[test]
fn a_server_past_the_end_of_the_client_certificate_refuses_the_channel() {
    let (read, _) = connect(
        0,
        (SERVED, END + Span::SECOND),
        (CLIENT, SERVER.certificate, START),
        &Policy::ALL,
        Mode::Sign,
    );
    // The server sends the status of its check in the ERR message.
    assert_eq!(
        read,
        Err(Error::Failed(Status::BAD_CERTIFICATE_TIME_INVALID))
    );
}

/// What a read of the largest request after [`VALUE`] in one message gives.
#[derive(Debug)]
struct Largest {
    /// The answer of the read.
    read: Result<Vec<Result<Value, Fault>>, Status>,
    /// The [`Large`] of a request whose last node is 1 byte longer.
    past: Vec<Large>,
    /// The value of [`VALUE`] on the server.
    value: Value,
}

/// The read of the largest request after [`VALUE`] that a client with `policy` in
/// `mode` puts in one message, to a server with a buffer of `buffer` bytes (0 for the
/// default).
fn largest_request(policy: Policy, mode: Mode, buffer: u32) -> Largest {
    let (reads, _) = run(0, async move |node| {
        let time = event::Time::since(node.clock(), START);
        serve_on(
            &node,
            4,
            time,
            Some((&SERVED, [policy].as_slice())),
            async |mut peer| {
                peer.server.set_limits(Limits {
                    buffer,
                    ..Limits::default()
                });
                let mut client = peer
                    .secure(mode, &CLIENT_SIDE, START)
                    .await
                    .expect("the client connects");
                let length = fitting(&client, &[], 1 << 20);
                peer.server.add_scalar(&long(length), Value::F64(2.5));
                let fit = fits(client.nodes(&[VALUE, long(length)]));
                let read = client.read(&fit).await.map(<[_]>::to_vec);
                let past = client.nodes(&[VALUE, long(length + 1)]).large().to_vec();
                client.close().await;
                Largest {
                    read,
                    past,
                    value: peer.value,
                }
            },
        )
        .await
    });
    reads
}

#[test]
fn a_request_that_fits_by_the_client_reads_in_sign() {
    let Largest { read, value, .. } =
        largest_request(Policy::Basic256Sha256, Mode::Sign, 0);
    assert_eq!(read, Ok(vec![Ok(value), Ok(Value::F64(2.5))]));
}

#[test]
fn a_request_that_fits_by_the_client_reads_in_sign_and_encrypt() {
    let Largest { read, value, .. } =
        largest_request(Policy::Aes256Sha256RsaPss, Mode::SignAndEncrypt, 0);
    assert_eq!(read, Ok(vec![Ok(value), Ok(Value::F64(2.5))]));
}

/// A chunk of 65536 bytes holds 24 bytes of headers and a signature of 32 bytes, and in
/// `SignAndEncrypt` 1 byte of padding, for each policy.
#[test]
fn a_request_takes_the_chunks_of_one_message_with_each_policy_in_each_mode() {
    for policy in Policy::ALL {
        for (mode, payload) in [(Mode::Sign, 65_480), (Mode::SignAndEncrypt, 65_479)] {
            let Largest { read, past, value } = largest_request(policy, mode, 0);
            let case = format!("{policy:?} in {mode:?}");
            assert_eq!(read, Ok(vec![Ok(value), Ok(Value::F64(2.5))]), "{case}");
            let most = 8 * payload - 7 * 7;
            let large = Large {
                index: 1,
                bytes: most + 1,
                most,
            };
            assert_eq!(past, [large], "{case}");
        }
    }
}

/// A chunk of 8200 bytes, 8 past a block of 16 bytes, also leaves those 8 bytes empty.
#[test]
fn a_chunk_leaves_its_bytes_past_a_block_empty() {
    let Largest { read, past, value } =
        largest_request(Policy::Basic256Sha256, Mode::Sign, 8200);
    assert_eq!(read, Ok(vec![Ok(value), Ok(Value::F64(2.5))]));
    let most = 8 * (8200 - 24 - 32 - 8) - 7 * 7;
    let large = Large {
        index: 1,
        bytes: most + 1,
        most,
    };
    assert_eq!(past, [large]);
}

/// A server with a buffer of 8192 bytes and 4 chunks sends an answer of at most 4
/// chunks of 8136 bytes less 21 in `Sign`, 955 Doubles of 34 bytes after 36, and of
/// 8120 in `SignAndEncrypt`, 953 Doubles.
#[test]
fn an_answer_takes_the_chunks_of_the_server_in_each_mode() {
    for (mode, split) in [(Mode::Sign, [955, 1]), (Mode::SignAndEncrypt, [953, 3])] {
        let (reads, _) = run(0, async move |node| {
            let time = event::Time::since(node.clock(), START);
            serve_on(
                &node,
                4,
                time,
                Some((&SERVED, [Policy::Basic256Sha256].as_slice())),
                async |mut peer| {
                    peer.server.set_limits(Limits {
                        buffer: 8192,
                        chunks: 4,
                        ..Limits::default()
                    });
                    let mut client = peer
                        .secure(mode, &CLIENT_SIDE, START)
                        .await
                        .expect("the client connects");
                    let nodes = fits(client.nodes(&vec![VALUE; 956]));
                    let values = client.read(&nodes).await.map(<[_]>::to_vec);
                    client.close().await;
                    (nodes.counts(), values, peer.value)
                },
            )
            .await
        });
        let (counts, values, value) = reads;
        let read = (counts, values);
        assert_eq!(read, (split.to_vec(), Ok(vec![Ok(value); 956])), "{mode:?}");
    }
}

/// In `SignAndEncrypt`, open62541 gives a chunk of 8135 bytes from a buffer of 8193, 1
/// less than Part 6, and one of 8199 from 8264, 1 less than Part 6 too. An answer of 4
/// chunks then holds 955 and 962 Doubles.
#[test]
fn an_answer_takes_the_payload_of_open62541_when_it_is_the_smaller() {
    for (buffer, split) in [(8193, [955, 45]), (8264, [962, 38])] {
        let (reads, _) = run(0, async move |node| {
            let time = event::Time::since(node.clock(), START);
            serve_on(
                &node,
                4,
                time,
                Some((&SERVED, [Policy::Basic256Sha256].as_slice())),
                async |mut peer| {
                    peer.server.set_limits(Limits {
                        buffer,
                        chunks: 4,
                        ..Limits::default()
                    });
                    let mut client = peer
                        .secure(Mode::SignAndEncrypt, &CLIENT_SIDE, START)
                        .await
                        .expect("the client connects");
                    let nodes = fits(client.nodes(&vec![VALUE; 1000]));
                    let values = client.read(&nodes).await.map(<[_]>::to_vec);
                    client.close().await;
                    (nodes.counts(), values, peer.value)
                },
            )
            .await
        });
        let (counts, values, value) = reads;
        let read = (counts, values);
        assert_eq!(
            read,
            (split.to_vec(), Ok(vec![Ok(value); 1000])),
            "{buffer}"
        );
    }
}

/// A received chunk holds the smaller of the payload of a sent one and the
/// `MaxBodySize` of Part 6. In `SignAndEncrypt`, the second is 508 blocks of 16 bytes
/// less 8 in a chunk of 8192 bytes, and 509 blocks less 8 in one of 8200, 1 byte past
/// the first.
#[test]
fn a_received_chunk_holds_the_smaller_of_two_payloads() {
    for (mode, buffer, payloads) in [
        (Mode::Sign, 8192, [8136, 8136]),
        (Mode::SignAndEncrypt, 8192, [8135, 8120]),
        (Mode::Sign, 8200, [8136, 8136]),
        (Mode::SignAndEncrypt, 8200, [8135, 8135]),
    ] {
        let (limits, _) = run(0, async move |node| {
            let time = event::Time::since(node.clock(), START);
            serve_on(
                &node,
                4,
                time,
                Some((&SERVED, [Policy::Basic256Sha256].as_slice())),
                async |mut peer| {
                    peer.server.set_limits(Limits {
                        buffer,
                        ..Limits::default()
                    });
                    let client = peer
                        .secure(mode, &CLIENT_SIDE, START)
                        .await
                        .expect("the client connects");
                    let mut limits = [0; 5];
                    // SAFETY: the client lives, and its channel is open.
                    let status = Status(unsafe {
                        ffi::shim_client_limits(client.raw.as_ptr(), &raw mut limits)
                    });
                    client.close().await;
                    (status, [limits[0], limits[1]])
                },
            )
            .await
        });
        assert_eq!(limits, (Status::GOOD, payloads), "{mode:?} {buffer}");
    }
}

#[test]
fn the_certificate_line_is_checked_before_the_key_line() {
    assert_eq!(refused(HIGH_PEM), second_line(Credential::Certificate));
}

#[test]
fn the_key_line_is_checked_before_the_certificate() {
    let own = Identity {
        certificate: b"no certificate",
        key: HIGH_PEM.key,
    };
    assert_eq!(refused(own), second_line(Credential::Key));
}

#[test]
fn the_window_is_checked_before_the_trust() {
    assert_eq!(
        refused_at(CLIENT, BROKEN_TRUST, END),
        Err(Error::Refused(Refusal::Time))
    );
}

#[test]
fn the_trust_is_checked_before_the_uri() {
    assert_eq!(refused_at(ANONYMOUS, BROKEN_TRUST, START), Err(invalid(2)));
}

#[test]
fn the_key_pair_is_checked_before_the_window() {
    let own = Identity {
        key: OTHER.key,
        ..CLIENT
    };
    assert_eq!(
        refused_at(own, SERVER.certificate, END),
        Err(Error::Refused(Refusal::Mismatch))
    );
}
