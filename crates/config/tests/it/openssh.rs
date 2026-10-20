//! The OpenSSH private key file of an Ed25519 key, as `openssh` reads and writes it.

use config::openssh::{self, Unreadable};
use proptest::prelude::*;
use ssh_key::private::{Ed25519Keypair, Ed25519PrivateKey, KeypairData, OpaqueKeypair};
use ssh_key::public::{Ed25519PublicKey, OpaquePublicKey};
use types::ed25519::PrivateKey;

/// `ssh-keygen -t ed25519 -N "" -C alice@laptop`, and the line of its `.pub` file.
const KEYGEN: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACDyYqNHsEEJBY9+1DrLFvXJtoCq/ZtO+VB3MK2ID2vV6AAAAJBSIDjtUiA4
7QAAAAtzc2gtZWQyNTUxOQAAACDyYqNHsEEJBY9+1DrLFvXJtoCq/ZtO+VB3MK2ID2vV6A
AAAEDgNDcccof+k8sjQg0LWIxV+LadcnBbg1uyWMLuFK1kz/Jio0ewQQkFj37UOssW9cm2
gKr9m075UHcwrYgPa9XoAAAADGFsaWNlQGxhcHRvcAE=
-----END OPENSSH PRIVATE KEY-----
";
const KEYGEN_PUB: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPJio0ewQQkFj37UOssW9cm2gKr9m075UHcwrYgPa9Xo";

/// `ssh-keygen -t ed25519 -N pass`.
const PASSPHRASE: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABBRYR5pwW
bFCkXFkNGtK43SAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAID3c6PhZelvs0Kxs
CNlOf4UmEqtOZalvTtE9sIJQPMPYAAAAkLBQd/W84o/JHO/N8J8dWSL8ermZ7OwmpmjoM+
4/g9kcpaas3MxRxvPAT/fuaFjf14Vh1Zw/P04KuJ16jBiQgcTY7izhi9NA0BUsYl6ntZX+
rNzPHH96Megmp/yYypFO0I9EBv7zKm42m2M1GtSgVHxIn2hMC3GDE4WyI/WunONIVUUPOW
apyEWMirL6NgKgnQ==
-----END OPENSSH PRIVATE KEY-----
";

/// `ssh-keygen -t rsa -b 1024 -N ""`.
const RSA: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAlwAAAAdzc2gtcn
NhAAAAAwEAAQAAAIEAv3Su37OsRS+tk7zwm9dxJasHJhoWYvCGH8OXR7GWlZdkwzeQQx5/
Acw+nZqKPG13WEZXt/NPk0tv1cBMiJjuTc8jScrZzWYMmQj95U87By+c/Go2qiGlOc7+hN
/8JFdx6CTUEWI83FKGC1wSZmBgim15ka0ap4ksl1gsA8Y7qz0AAAH4ITNiXCEzYlwAAAAH
c3NoLXJzYQAAAIEAv3Su37OsRS+tk7zwm9dxJasHJhoWYvCGH8OXR7GWlZdkwzeQQx5/Ac
w+nZqKPG13WEZXt/NPk0tv1cBMiJjuTc8jScrZzWYMmQj95U87By+c/Go2qiGlOc7+hN/8
JFdx6CTUEWI83FKGC1wSZmBgim15ka0ap4ksl1gsA8Y7qz0AAAADAQABAAAAgC+vXTrkYl
zdEiuA3KDEGS9bmK05xCeiZpWUvqIhrjLWAUfKGAlPmCyXdWUFr7Z7QUVgR8rONByoplMj
0SwYgwm5RGcmyBpA14PJZW9OCCzhktmUIodvo0/gmZbZsIDFmalwBQteB3kpKlFee9I4Xd
6mpQq5qYTJLHqzfuYDkbDhAAAAQQCdv1m/NAXytLTfKgvSsst0vvX/dtMrYI5jBogxWmEn
1Tlo/qxLL2v10E4bjZAibd1Xr58BjvaDA6fUdIwa9sGPAAAAQQD26Cbxd0wV8jEB/lTOJY
9fSfChXzPg3PiJ99T64XEsJRVdfzcsLFANP4nYuCClADT9ZguAejc0uf/8ucIFu1rVAAAA
QQDGgbx3D8ZSGTxlxQHjSSVqy1nXTM/Rey2+xNtZaCMhmdgJnhp7BupUXvqzp4ySJKyBAO
LuN/r50yflT0JbNPLJAAAAAAEC
-----END OPENSSH PRIVATE KEY-----
";
/// `ssh-keygen -t ecdsa -b 256 -N ""`.
const ECDSA: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAaAAAABNlY2RzYS
1zaGEyLW5pc3RwMjU2AAAACG5pc3RwMjU2AAAAQQRoMDjdYYenV2PI4+S3E381Kevu6Ay+
w06FCLCggKDankT4pu5kuoOQFHoSVDIW/1sy7IslJxI02TFRSWb9o59MAAAAoJvzPiab8z
4mAAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBGgwON1hh6dXY8jj
5LcTfzUp6+7oDL7DToUIsKCAoNqeRPim7mS6g5AUehJUMhb/WzLsiyUnEjTZMVFJZv2jn0
wAAAAhALtETUKZA++mtWola1ac1GiHc2vbUymBIa1LE65UqDb+AAAAAAECAwQFBgc=
-----END OPENSSH PRIVATE KEY-----
";

/// Why `text` is not read, with its message.
fn unreadable(text: &str) -> (Unreadable, String) {
    let error = openssh::private_key(text.as_bytes())
        .map(|key| key.0)
        .unwrap_err();
    (error, error.to_string())
}

#[test]
fn reads_the_key_that_ssh_keygen_writes() {
    let key = openssh::private_key(KEYGEN.as_bytes()).unwrap();
    assert_eq!(openssh::line(key.public()), KEYGEN_PUB);
}

#[test]
fn writes_the_form_that_ssh_keygen_writes() {
    let key = openssh::private_key(KEYGEN.as_bytes()).unwrap();
    let text = openssh::key_file(&key, "alice@laptop");
    let ours = ssh_key::PrivateKey::from_openssh(&text).unwrap();
    let theirs = ssh_key::PrivateKey::from_openssh(KEYGEN).unwrap();
    assert_eq!(ours.key_data(), theirs.key_data());
    assert_eq!(ours.comment(), "alice@laptop");
    assert!(!ours.is_encrypted());
    assert!(
        text.ends_with("-----END OPENSSH PRIVATE KEY-----\n"),
        "{text}"
    );
}

#[test]
fn refuses_a_text_that_is_not_a_private_key_file() {
    let expected = (
        Unreadable::Format,
        "the file is not an OpenSSH private key as `ssh-keygen` writes it".into(),
    );
    assert_eq!(unreadable(""), expected);
    assert_eq!(unreadable(KEYGEN_PUB), expected);
}

#[test]
fn refuses_a_key_of_another_algorithm_by_its_name() {
    assert_eq!(
        unreadable(RSA),
        (
            Unreadable::Algorithm("ssh-rsa"),
            "the private key is \"ssh-rsa\", and a subject takes only `ssh-ed25519`"
                .into()
        )
    );
}

/// An algorithm that is not on the list of known names is not named, since a corrupt
/// file can hold any text there.
#[test]
fn refuses_a_key_of_an_unknown_algorithm_as_no_key_file() {
    let algorithm = ssh_key::Algorithm::new("a@b.example").unwrap();
    let public = OpaquePublicKey::new(vec![1; 32], algorithm);
    let pair = OpaqueKeypair::new(vec![2; 32], public);
    let file = ssh_key::PrivateKey::new(KeypairData::Other(pair), "").unwrap();
    let text = file.to_openssh(ssh_key::LineEnding::LF).unwrap();
    assert_eq!(
        unreadable(&text),
        (
            Unreadable::Format,
            "the file is not an OpenSSH private key as `ssh-keygen` writes it".into()
        )
    );
}

/// A key file with CRLF line ends, as an editor on Windows saves it, with blank lines
/// or other text after the end mark, or with no line end after the end mark reads as
/// its key. OpenSSH 9.6p1 reads the second and third, and refuses the first and last.
#[test]
fn reads_a_key_file_with_each_line_end_and_text_after_the_end_mark() {
    let crlf = KEYGEN.replace('\n', "\r\n");
    let blank = format!("{KEYGEN}\n\r\n");
    let after = format!("{KEYGEN}any text\n");
    for text in [crlf.as_str(), &blank, &after, KEYGEN.trim_end()] {
        let key = openssh::private_key(text.as_bytes()).unwrap();
        assert_eq!(openssh::line(key.public()), KEYGEN_PUB, "{text:?}");
    }
}

/// OpenSSH skips the bytes after the end mark, so they need not be UTF-8. A byte
/// before it that is not UTF-8 makes the file no key file.
#[test]
fn reads_any_bytes_after_the_end_mark_and_refuses_others_before_it() {
    let after = [KEYGEN.as_bytes(), b"caf\xe9 notes\n"].concat();
    let key = openssh::private_key(&after).unwrap();
    assert_eq!(openssh::line(key.public()), KEYGEN_PUB);
    let before = [b"\xe9".as_slice(), KEYGEN.as_bytes()].concat();
    let error = openssh::private_key(&before).map(|key| key.0).unwrap_err();
    assert_eq!(
        (error, error.to_string()),
        (
            Unreadable::Format,
            "the file is not an OpenSSH private key".into()
        )
    );
}

#[test]
fn reads_a_key_file_with_mixed_line_ends_and_space_at_the_end() {
    let mixed: String = KEYGEN
        .lines()
        .enumerate()
        .flat_map(|(i, line)| [line, if i % 2 == 0 { "\r\n" } else { "\n" }])
        .collect();
    let crlf_blanks = format!("{}\r\n\r\n  \r\n", KEYGEN.replace('\n', "\r\n"));
    let trailing_tab = format!("{KEYGEN}\t\n");
    for text in [mixed, crlf_blanks, trailing_tab] {
        let key = openssh::private_key(text.as_bytes()).unwrap();
        assert_eq!(openssh::line(key.public()), KEYGEN_PUB, "{text:?}");
    }
}

/// Go's `ssh.MarshalPrivateKey` wraps the body at 64 columns, and OpenSSH reads a body
/// at any width.
#[test]
fn reads_a_key_file_whose_body_has_any_width() {
    let body: String = KEYGEN
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    for width in [64, 76, body.len()] {
        let mut text = String::from("-----BEGIN OPENSSH PRIVATE KEY-----\n");
        for line in body.as_bytes().chunks(width) {
            text.push_str(std::str::from_utf8(line).unwrap());
            text.push('\n');
        }
        text.push_str("-----END OPENSSH PRIVATE KEY-----\n");
        let key = openssh::private_key(text.as_bytes()).unwrap();
        assert_eq!(openssh::line(key.public()), KEYGEN_PUB, "{text:?}");
    }
}

/// `ssh-keygen -C` in a Latin-1 locale writes a comment that is not UTF-8, which OpenSSH
/// reads and `ssh-key` 0.6.7 refuses. The error says so, not that the file is no key.
#[test]
fn refuses_a_key_file_whose_comment_is_not_utf8() {
    use base64ct::{Base64, Encoding};
    let body: String = KEYGEN
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let mut binary = Base64::decode_vec(&body).unwrap();
    let at = binary
        .windows(12)
        .position(|bytes| bytes == b"alice@laptop")
        .unwrap();
    binary[at + 2] = 0xe9;
    let text = format!(
        "-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n",
        Base64::encode_string(&binary)
    );
    let expected = (
        Unreadable::Comment,
        "the key file holds a text that is not UTF-8, such as its comment".into(),
    );
    assert_eq!(unreadable(&text), expected);
}

/// OpenSSH skips white space in the body: space and tab around each line, and a
/// vertical tab or a form feed in a line.
#[test]
fn reads_a_key_file_with_white_space_in_its_body() {
    let mut lines: Vec<String> = KEYGEN.lines().map(str::to_owned).collect();
    let last = lines.len() - 1;
    for (i, line) in lines[1..last].iter_mut().enumerate() {
        let mid = line.len() / 2;
        let c = if i % 2 == 0 { '\x0b' } else { '\x0c' };
        *line = format!("  {}{c}{}\t ", &line[..mid], &line[mid..]);
    }
    let text = lines.join("\n") + "\n";
    let key = openssh::private_key(text.as_bytes()).unwrap();
    assert_eq!(openssh::line(key.public()), KEYGEN_PUB, "{text:?}");
}

#[test]
fn refuses_an_ecdsa_key_by_its_family() {
    assert_eq!(
        unreadable(ECDSA),
        (
            Unreadable::Algorithm("ECDSA"),
            "the private key is \"ECDSA\", and a subject takes only `ssh-ed25519`"
                .into()
        )
    );
}

/// `ssh-key` 0.6.7 gives the same error for a `none` KDF with options as for an ECDSA
/// key. OpenSSH reads such a file, which no known writer makes.
#[test]
fn refuses_an_ed25519_key_whose_none_kdf_has_options_as_no_key_file() {
    use base64ct::{Base64, Encoding};
    let body: String = KEYGEN
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let mut binary = Base64::decode_vec(&body).unwrap();
    let options = b"\0\0\0\x04none\0\0\0\x04none\0\0\0\0";
    let at = binary
        .windows(20)
        .position(|bytes| bytes == options)
        .unwrap()
        + 19;
    binary.splice(at..=at, [4, 0, 0, 0, 0]);
    let text = format!(
        "-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n",
        Base64::encode_string(&binary)
    );
    let expected = (
        Unreadable::Format,
        "the file is not an OpenSSH private key as `ssh-keygen` writes it".into(),
    );
    assert_eq!(unreadable(&text), expected);
}

#[test]
fn refuses_a_key_with_a_passphrase() {
    assert_eq!(
        unreadable(PASSPHRASE),
        (
            Unreadable::Passphrase,
            "the private key has a passphrase".into()
        )
    );
}

#[test]
fn refuses_a_file_whose_public_key_is_not_its_private_key() {
    let pair = Ed25519Keypair {
        public: Ed25519PublicKey(PrivateKey([2; 32]).public().to_bytes()),
        private: Ed25519PrivateKey::from_bytes(&[1; 32]),
    };
    let file = ssh_key::PrivateKey::new(KeypairData::Ed25519(pair), "").unwrap();
    let text = file.to_openssh(ssh_key::LineEnding::LF).unwrap();
    assert_eq!(
        unreadable(&text),
        (
            Unreadable::Mismatch,
            "the public key of the file is not the key of its private key".into()
        )
    );
}

#[test]
fn gives_the_code_and_the_fix_of_each_error() {
    let new_key = "Make a new key with `ssh-keygen -t ed25519 -N \"\"`, and add its \
                   `.pub` line to `keys` of the `subject` block";
    let parts = |error: Unreadable| (error.code().as_str(), error.fix());
    assert_eq!(
        parts(Unreadable::Format),
        (
            "config.key-format",
            "Use the private key file that `ssh-keygen -t ed25519 -N \"\"` writes, not \
             its `.pub` file"
        )
    );
    assert_eq!(
        parts(Unreadable::Algorithm("ssh-rsa")),
        ("config.key-algorithm", new_key)
    );
    assert_eq!(
        parts(Unreadable::Passphrase),
        (
            "config.key-passphrase",
            "Use a key file with no passphrase, as `ssh-keygen -t ed25519 -N \"\"` \
             writes it"
        )
    );
    assert_eq!(
        parts(Unreadable::Mismatch),
        ("config.key-mismatch", new_key)
    );
    assert_eq!(
        parts(Unreadable::Comment),
        (
            "config.key-comment",
            "Give the key a comment in UTF-8 with `ssh-keygen -c -C <comment> -f <file>`"
        )
    );
}

proptest! {
    #[test]
    fn reads_back_each_key_that_it_writes(
        bytes in any::<[u8; 32]>(),
        comment in "[ -~]{0,40}",
    ) {
        let text = openssh::key_file(&PrivateKey(bytes), &comment);
        prop_assert_eq!(openssh::private_key(text.as_bytes()).unwrap().0, bytes);
    }
}
