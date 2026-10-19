//! The failed file call at the root of an error of a lower crate. Each match names
//! each variant, so a new variant that wraps a file call cannot go unread.

use env::files;
use mesh::Stopped;

/// The failed file call that caused `error`, or `None`.
pub(crate) fn buffer(error: &buffer::Error) -> Option<&files::Error> {
    match error {
        buffer::Error::Files(error) => Some(error),
        buffer::Error::Full { .. }
        | buffer::Error::Pool(_)
        | buffer::Error::Budget { .. }
        | buffer::Error::Length { .. }
        | buffer::Error::Missing
        | buffer::Error::Damaged
        | buffer::Error::Version(_)
        | buffer::Error::Unfit(_)
        | buffer::Error::Unaligned { .. }
        | buffer::Error::Invalid { .. } => None,
    }
}

/// The failed file call that caused `error`, or `None`.
pub(crate) fn blob(error: &blob::Error) -> Option<&files::Error> {
    match error {
        blob::Error::Files(error) => Some(error),
        blob::Error::Pool(_)
        | blob::Error::Mismatch { .. }
        | blob::Error::Stray { .. }
        | blob::Error::Floor { .. } => None,
    }
}

/// The failed file call that caused `error`, or `None`.
pub(crate) fn mesh(error: &mesh::Error) -> Option<&files::Error> {
    match error {
        mesh::Error::Files(error) => Some(error),
        mesh::Error::Log(error) => log(error),
        mesh::Error::Blob(error) => blob(error),
        mesh::Error::Stopped(error) => stopped(error),
        mesh::Error::Raft(_)
        | mesh::Error::Spoofed { .. }
        | mesh::Error::NotVoter { .. }
        | mesh::Error::Removed { .. }
        | mesh::Error::PeerNotVoter { .. }
        | mesh::Error::Claim(_)
        | mesh::Error::NotMember(_)
        | mesh::Error::NoVote
        | mesh::Error::Contested { .. }
        | mesh::Error::Member(_)
        | mesh::Error::WrongKey
        | mesh::Error::Pool(_)
        | mesh::Error::Malformed
        | mesh::Error::Stream(_)
        | mesh::Error::Stale { .. }
        | mesh::Error::Large { .. }
        | mesh::Error::Problems(_)
        | mesh::Error::Quorum { .. }
        | mesh::Error::Stray { .. }
        | mesh::Error::NotIndex(_)
        | mesh::Error::UnknownNode(_)
        | mesh::Error::Homes { .. }
        | mesh::Error::Voters { .. }
        | mesh::Error::Limit { .. }
        | mesh::Error::Founding { .. }
        | mesh::Error::Unjoinable { .. }
        | mesh::Error::Unfounded { .. }
        | mesh::Error::CardFile { .. }
        | mesh::Error::Soon => None,
    }
}

/// The failed file call that stopped the group, or `None`.
pub(crate) fn stopped(stopped: &Stopped) -> Option<&files::Error> {
    match stopped {
        Stopped::Write(error) => log(error),
        Stopped::Change { .. } | Stopped::Removed { .. } | Stopped::Dropped => None,
    }
}

/// The failed file call that caused `error`, or `None`.
fn log(error: &mesh::log::Error) -> Option<&files::Error> {
    match error {
        mesh::log::Error::Files(error) => Some(error),
        mesh::log::Error::Pool(_)
        | mesh::log::Error::Corrupt { .. }
        | mesh::log::Error::Version { .. }
        | mesh::log::Error::Stray { .. }
        | mesh::log::Error::Poisoned { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use env::files;
    use mesh::{Stopped, log};

    use crate::Error;

    #[test]
    fn files_gives_the_failed_file_call_of_each_path() {
        let failed = files::Error::Io {
            path: PathBuf::from("mesh/log/log-0"),
            operation: files::Operation::Open,
            code: 13,
        };
        let write = || Stopped::Write(log::Error::Files(failed.clone()));
        for error in [
            Error::Directory(failed.clone()),
            Error::Buffer {
                core: 1,
                error: buffer::Error::Files(failed.clone()),
            },
            Error::Blob(blob::Error::Files(failed.clone())),
            Error::Mesh(mesh::Error::Files(failed.clone())),
            Error::Mesh(mesh::Error::Log(log::Error::Files(failed.clone()))),
            Error::Mesh(mesh::Error::Blob(blob::Error::Files(failed.clone()))),
            Error::Mesh(mesh::Error::Stopped(write())),
            Error::Group(write()),
        ] {
            assert_eq!(error.files(), Some(&failed), "{error:?}");
        }
    }

    #[test]
    fn files_gives_none_when_no_file_call_caused_the_error() {
        let corrupt = log::Error::Corrupt {
            path: PathBuf::from("mesh/log/log-0"),
            offset: 64,
        };
        let floor = blob::Error::Floor {
            len: 68,
            free_bytes: (1 << 30) - 1,
            floor_bytes: 1 << 30,
        };
        for error in [
            Error::Mesh(mesh::Error::Log(corrupt)),
            Error::Mesh(mesh::Error::Blob(floor)),
            Error::Group(Stopped::Dropped),
            Error::Key,
        ] {
            assert_eq!(error.files(), None, "{error:?}");
        }
    }
}
