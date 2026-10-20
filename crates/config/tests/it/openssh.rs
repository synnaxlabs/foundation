//! The OpenSSH private key file of an Ed25519 key, as `openssh` reads and writes it.

use config::openssh::{self, Unreadable};
use proptest::prelude::*;
use ssh_key::private::{Ed25519Keypair, Ed25519PrivateKey, KeypairData};
use ssh_key::public::Ed25519PublicKey;
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

/// `ssh-keygen -t ecdsa -b 256 -N ""`.
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
    let error = openssh::private_key(text).map(|key| key.0).unwrap_err();
    (error, error.to_string())
}

#[test]
fn reads_the_key_that_ssh_keygen_writes() {
    let key = openssh::private_key(KEYGEN).unwrap();
    assert_eq!(openssh::line(key.public()), KEYGEN_PUB);
}

#[test]
fn writes_the_form_that_ssh_keygen_writes() {
    let key = openssh::private_key(KEYGEN).unwrap();
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
        "the file is not an OpenSSH private key".into(),
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
}

proptest! {
    #[test]
    fn reads_back_each_key_that_it_writes(
        bytes in any::<[u8; 32]>(),
        comment in "[ -~]{0,40}",
    ) {
        let text = openssh::key_file(&PrivateKey(bytes), &comment);
        prop_assert_eq!(openssh::private_key(&text).unwrap().0, bytes);
    }
}
