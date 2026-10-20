//! What a program on the node's host reads to reach the node that runs on a data
//! directory: the files [`crate::address`] and [`crate::admin`].

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use env::files::Files;
use types::ed25519::{PrivateKey, PublicKey};

use crate::{Held, address, admin};

/// The node that runs on a data directory, and the key of its first admin.
#[derive(Clone, Debug)]
pub struct Local {
    /// The public key that the node's transport proves.
    pub public_key: PublicKey,
    /// Where a program on the node's host dials the node: the address where its port
    /// is bound, with the loopback IP of its family in place of an unspecified IP.
    pub address: SocketAddr,
    /// The private key of the first admin of the root region, or `None` when the
    /// directory holds no `admin.key` that a node wrote, as on a node that joined.
    pub admin: Option<PrivateKey>,
}

/// Why [`read`] gave nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// No node runs on the directory: its file `address` holds no address of a node.
    NotRunning,
    /// The file `file` holds the format of version `found`, and this program reads
    /// version `expected`: a node of another version wrote it.
    Version {
        /// The name of the file.
        file: &'static str,
        /// The version byte that the file holds: `1` to `9` or `a` to `z`.
        found: u8,
        /// The version byte of the format that this program reads.
        expected: u8,
    },
    /// A file call on the directory failed.
    Files(env::files::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRunning => f.write_str("no node runs on the data directory"),
            Self::Version {
                file,
                found,
                expected,
            } => write!(
                f,
                "{file} holds format version {}, and this program reads version {}",
                char::from(*found),
                char::from(*expected),
            ),
            Self::Files(error) => write!(f, "{error}"),
        }
    }
}

/// The node that runs on the data directory of `files`, and its first admin. Opens
/// each file to read only, so it makes nothing and waits for no lock.
///
/// # Errors
///
/// In this order: [`Error::Version`] when a file that it reads holds the format of
/// another version, [`Error::Files`] for a file call that fails, other than
/// [`env::files::Error::NotFound`], and [`Error::NotRunning`] when `address` holds no
/// address of a node.
pub async fn read(files: &Files) -> Result<Local, Error> {
    let address = address::read(files).await;
    let admin = admin::read(files).await;
    match (
        address.map(|held| known(held, address::FILE)),
        admin.map(|held| known(held, admin::FILE)),
    ) {
        (Ok(Err(version)), _) | (_, Ok(Err(version))) => Err(version),
        (Err(error), _) | (_, Err(error)) => Err(Error::Files(error)),
        (Ok(Ok(address)), Ok(Ok(admin))) => {
            let address = address.ok_or(Error::NotRunning)?;
            Ok(Local {
                public_key: address.public_key,
                address: dialed(address.bound),
                admin,
            })
        }
    }
}

/// The value that a node wrote to the file `file`, from what `held` says it holds, or
/// `None` for no file or a foreign one.
fn known<T>(held: Held<T>, file: &'static str) -> Result<Option<T>, Error> {
    match held {
        Held::Written(value) => Ok(Some(value)),
        Held::Version { found, expected } => Err(Error::Version {
            file,
            found,
            expected,
        }),
        Held::Nothing | Held::Foreign => Ok(None),
    }
}

/// Where a program on the node's host dials `bound`: the loopback IP of its family in
/// place of the unspecified IP.
fn dialed(mut bound: SocketAddr) -> SocketAddr {
    match bound.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            bound.set_ip(Ipv4Addr::LOCALHOST.into());
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            bound.set_ip(Ipv6Addr::LOCALHOST.into());
        }
        IpAddr::V4(_) | IpAddr::V6(_) => {}
    }
    bound
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use env::files::Mode;
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn dialed_keeps_each_specified_ip(
            ip in any::<IpAddr>().prop_filter("specified", |ip| !ip.is_unspecified()),
            port: u16,
        ) {
            let bound = SocketAddr::new(ip, port);
            prop_assert_eq!(dialed(bound), bound);
        }

        #[test]
        fn dialed_gives_the_loopback_ip_of_the_family_for_an_unspecified_ip(port: u16) {
            let v4 = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port);
            let v6 = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port);
            let loopback = |ip: IpAddr| SocketAddr::new(ip, port);
            prop_assert_eq!(dialed(v4), loopback(Ipv4Addr::LOCALHOST.into()));
            prop_assert_eq!(dialed(v6), loopback(Ipv6Addr::LOCALHOST.into()));
        }
    }

    #[test]
    fn version_prints_each_byte_as_an_ascii_char() {
        let version = |found| Error::Version {
            file: "address",
            found,
            expected: b'1',
        };
        assert_eq!(
            version(b'2').to_string(),
            "address holds format version 2, and this program reads version 1"
        );
        assert_eq!(
            version(b'z').to_string(),
            "address holds format version z, and this program reads version 1"
        );
    }

    #[test]
    fn the_texts_of_the_other_errors() {
        assert_eq!(
            Error::NotRunning.to_string(),
            "no node runs on the data directory"
        );
        let error = env::files::Error::NotFound {
            path: PathBuf::from("address"),
        };
        assert_eq!(Error::Files(error.clone()).to_string(), error.to_string());
    }

    /// `body` with its CRC32C appended.
    fn sealed(body: &[u8]) -> Vec<u8> {
        let mut bytes = body.to_vec();
        bytes.extend_from_slice(&crc32c::crc32c(body).to_le_bytes());
        bytes
    }

    /// The bytes of `address` of a node at `bound`.
    fn address_file(bound: SocketAddr) -> [u8; address::LEN] {
        address::encode(&address::Address {
            key: types::node::Key::from_u128(7),
            public_key: PrivateKey([7; 32]).public(),
            bound,
        })
    }

    /// What [`read`] gives on a host whose data directory holds `files`, each a name
    /// and its bytes, with a fault on each operation of `faults`, once the run ends.
    fn local_over_with(
        files: Vec<(&'static str, Vec<u8>)>,
        faults: Vec<(&'static str, env::files::Operation)>,
    ) -> Result<Result<Local, Error>, sim::Error> {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = sim.node(sim::node::Config::default());
        sim.run_on(&host, |host, _| async move {
            let directory = host.files();
            let pool = block::Pool::heap(crate::POOL);
            for (name, bytes) in files {
                let len = bytes.len() as u64;
                let path = Path::new(name);
                let file = directory.open(path, Mode::Create { len }).await;
                let file = file.expect("makes the file");
                let block = pool.copy(&bytes).expect("a block");
                file.write_at(0, &[block]).await.expect("writes");
            }
            for (name, operation) in faults {
                host.fail_file(Path::new(name), operation);
            }
            read(&directory).await
        })
    }

    /// What [`read`] gives on a host whose data directory holds `files`, each a name
    /// and its bytes, once the run ends.
    fn local_over(
        files: Vec<(&'static str, Vec<u8>)>,
    ) -> Result<Result<Local, Error>, sim::Error> {
        local_over_with(files, Vec::new())
    }

    #[test]
    fn read_gives_the_node_and_the_admin_that_a_node_wrote() {
        let bound = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 7400));
        let local = local_over(vec![
            (address::FILE, address_file(bound).to_vec()),
            (admin::FILE, admin::encode(&PrivateKey([3; 32])).to_vec()),
        ])
        .expect("the run ends")
        .expect("a node runs");
        assert_eq!(local.public_key, PrivateKey([7; 32]).public());
        assert_eq!(local.address, SocketAddr::from((Ipv4Addr::LOCALHOST, 7400)));
        assert_eq!(local.admin.map(|admin| admin.0), Some([3; 32]));
    }

    #[test]
    fn read_on_no_admin_key_or_a_foreign_one_gives_no_admin() {
        let bound = SocketAddr::from((Ipv4Addr::LOCALHOST, 7400));
        let address = (address::FILE, address_file(bound).to_vec());
        for files in [
            vec![address.clone()],
            vec![address.clone(), (admin::FILE, vec![0; admin::LEN])],
            vec![address, (admin::FILE, b"foundation/admin/".to_vec())],
        ] {
            let local = local_over(files).expect("the run ends");
            let local = local.expect("a node runs");
            assert_eq!(local.address, bound);
            assert!(local.admin.is_none(), "{local:?}");
        }
    }

    /// The [`Error::Version`] of `file` that holds version `2`.
    fn version_2(file: &'static str) -> Error {
        Error::Version {
            file,
            found: b'2',
            expected: b'1',
        }
    }

    /// What [`read`] gives, with each `Ok` dropped, on an `address` of `bytes`.
    fn local_over_address(bytes: Vec<u8>) -> Result<Result<(), Error>, sim::Error> {
        local_over(vec![(address::FILE, bytes)]).map(|read| read.map(drop))
    }

    /// `bytes` with `len` bytes, `9` after them.
    fn padded(bytes: &[u8], len: usize) -> Vec<u8> {
        let mut padded = bytes.to_vec();
        padded.resize(len, 9);
        padded
    }

    #[test]
    fn read_on_an_address_of_another_version_and_length_gives_version() {
        let bytes = sealed(&padded(b"foundation/address/2", 140));
        assert_eq!(local_over_address(bytes), Ok(Err(version_2("address"))));
    }

    #[test]
    fn read_on_an_address_of_another_version_and_its_length_gives_version() {
        let bytes = padded(b"foundation/address/2", address::LEN);
        assert_eq!(local_over_address(bytes), Ok(Err(version_2("address"))));
    }

    #[test]
    fn read_on_an_address_of_only_the_tag_of_another_version_gives_version() {
        let bytes = b"foundation/address/2".to_vec();
        assert_eq!(local_over_address(bytes), Ok(Err(version_2("address"))));
    }

    /// A file past a sector is read only up to the length of the tag.
    #[test]
    fn read_on_an_address_of_another_version_past_a_sector_gives_version() {
        let bytes = padded(b"foundation/address/2", env::files::SECTOR + 100);
        assert_eq!(local_over_address(bytes), Ok(Err(version_2("address"))));
    }

    #[test]
    fn read_on_an_admin_key_of_another_version_gives_version() {
        let bound = SocketAddr::from((Ipv4Addr::LOCALHOST, 7400));
        let error = local_over(vec![
            (address::FILE, address_file(bound).to_vec()),
            (admin::FILE, padded(b"foundation/admin/2", 60)),
        ]);
        let error = error.map(|read| read.map(drop));
        assert_eq!(error, Ok(Err(version_2("admin.key"))));
    }

    #[test]
    fn read_on_a_cleared_address_and_an_admin_key_of_another_version_gives_version() {
        let error = local_over(vec![
            (address::FILE, vec![0; address::LEN]),
            (admin::FILE, padded(b"foundation/admin/2", admin::LEN)),
        ]);
        let error = error.map(|read| read.map(drop));
        assert_eq!(error, Ok(Err(version_2("admin.key"))));
    }

    #[test]
    fn read_on_no_address_and_an_admin_key_of_another_version_gives_version() {
        let error = local_over(vec![(admin::FILE, padded(b"foundation/admin/2", 60))]);
        let error = error.map(|read| read.map(drop));
        assert_eq!(error, Ok(Err(version_2("admin.key"))));
    }

    #[test]
    fn read_on_an_address_with_another_prefix_gives_not_running() {
        let bytes = sealed(&padded(b"foundation/other/2", 140));
        assert_eq!(local_over_address(bytes), Ok(Err(Error::NotRunning)));
        let bytes = padded(b"foundation/addresx2", address::LEN);
        assert_eq!(local_over_address(bytes), Ok(Err(Error::NotRunning)));
    }

    #[test]
    fn read_on_an_address_with_its_tag_and_checksum_and_no_address_gives_not_running() {
        let bound = SocketAddr::from((Ipv4Addr::LOCALHOST, 7400));
        let mut bytes = address_file(bound);
        bytes[68] = 5;
        crate::checksum(&mut bytes);
        assert_eq!(
            local_over_address(bytes.to_vec()),
            Ok(Err(Error::NotRunning))
        );
    }

    #[test]
    fn read_on_an_address_shorter_than_its_tag_gives_not_running() {
        let bytes = b"foundation/address/".to_vec();
        assert_eq!(local_over_address(bytes), Ok(Err(Error::NotRunning)));
    }

    #[test]
    fn read_on_an_address_of_version_1_and_another_length_gives_not_running() {
        let bytes = sealed(&padded(b"foundation/address/1", 140));
        assert_eq!(local_over_address(bytes), Ok(Err(Error::NotRunning)));
    }

    #[test]
    fn read_on_an_address_of_version_1_with_a_bad_checksum_gives_not_running() {
        let bound = SocketAddr::from((Ipv4Addr::LOCALHOST, 7400));
        let mut bytes = address_file(bound).to_vec();
        bytes[30] ^= 1;
        assert_eq!(local_over_address(bytes), Ok(Err(Error::NotRunning)));
    }

    #[test]
    fn read_of_the_tag_of_an_address_of_another_length_that_fails_gives_files() {
        let bytes = sealed(&padded(b"foundation/address/2", 140));
        let read = local_over_with(
            vec![(address::FILE, bytes)],
            vec![(address::FILE, env::files::Operation::ReadAt)],
        );
        let error = env::files::Error::Io {
            path: PathBuf::from(address::FILE),
            operation: env::files::Operation::ReadAt,
            code: 5,
        };
        assert_eq!(
            read.map(|read| read.map(drop)),
            Ok(Err(Error::Files(error)))
        );
    }

    /// A newer node can give `admin.key` a form that this program cannot open, such
    /// as a directory. Its `address` of another version still names the cause.
    #[test]
    fn read_on_an_address_of_another_version_and_an_admin_key_directory_gives_version()
    {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = sim.node(sim::node::Config::default());
        let read = sim.run_on(&host, |host, _| async move {
            let directory = host.files();
            let bytes = sealed(&padded(b"foundation/address/2", 140));
            let len = bytes.len() as u64;
            let file = directory
                .open(Path::new(address::FILE), Mode::Create { len })
                .await
                .expect("makes the file");
            let pool = block::Pool::heap(crate::POOL);
            let block = pool.copy(&bytes).expect("a block");
            file.write_at(0, &[block]).await.expect("writes");
            directory
                .create_dir(Path::new(admin::FILE))
                .await
                .expect("makes the directory");
            read(&directory).await.map(drop)
        });
        assert_eq!(read, Ok(Err(version_2("address"))));
    }

    #[test]
    fn read_on_an_address_of_another_version_and_a_failed_admin_read_gives_version() {
        let read = local_over_with(
            vec![
                (address::FILE, sealed(&padded(b"foundation/address/2", 140))),
                (admin::FILE, admin::encode(&PrivateKey([3; 32])).to_vec()),
            ],
            vec![(admin::FILE, env::files::Operation::ReadAt)],
        );
        assert_eq!(
            read.map(|read| read.map(drop)),
            Ok(Err(version_2("address")))
        );
    }

    /// The failed read can hide an `admin.key` of another version, which
    /// [`Error::Version`] names over [`Error::NotRunning`].
    #[test]
    fn read_on_no_address_and_a_failed_read_of_the_admin_key_gives_files() {
        let read = local_over_with(
            vec![(admin::FILE, admin::encode(&PrivateKey([3; 32])).to_vec())],
            vec![(admin::FILE, env::files::Operation::ReadAt)],
        );
        let error = env::files::Error::Io {
            path: PathBuf::from(admin::FILE),
            operation: env::files::Operation::ReadAt,
            code: 5,
        };
        assert_eq!(
            read.map(|read| read.map(drop)),
            Ok(Err(Error::Files(error)))
        );
    }

    #[test]
    fn read_on_an_address_of_a_version_byte_outside_the_set_gives_not_running() {
        for version in [b'0', b'A', b'/', 0xff] {
            let mut tag = b"foundation/address/".to_vec();
            tag.push(version);
            let bytes = sealed(&padded(&tag, 140));
            assert_eq!(local_over_address(bytes), Ok(Err(Error::NotRunning)));
            let bytes = padded(&tag, address::LEN);
            assert_eq!(local_over_address(bytes), Ok(Err(Error::NotRunning)));
        }
    }

    #[test]
    fn read_on_an_address_of_version_z_gives_version() {
        let bytes = sealed(&padded(b"foundation/address/z", 140));
        let version = Error::Version {
            file: "address",
            found: b'z',
            expected: b'1',
        };
        assert_eq!(local_over_address(bytes), Ok(Err(version)));
    }
}
