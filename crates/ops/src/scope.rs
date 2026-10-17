//! The files that `plan` and `apply` reach, and the limits of `foundation mcp`.

use std::path::{Path, PathBuf};

use env::paths::{self, Found, Kind, Paths};
use wire::hub::client::BODY_BYTES_MAX;

use crate::error::{Error, Unread, Unusable, Unwritten};

/// The files that a run reaches: each one for a command line, and for `foundation mcp`
/// only those under its folders, with its data directory hidden.
pub(crate) struct Scope {
    paths: Paths,
    limits: Option<Limits>,
}

/// The resolved folders of `foundation mcp`, its resolved data directory, and the
/// resolved working directory, where a relative path starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) folders: Vec<PathBuf>,
    pub(crate) data: PathBuf,
    pub(crate) working: PathBuf,
}

impl Scope {
    /// Each file of `paths`, for a command line.
    pub(crate) fn free(paths: &Paths) -> Self {
        Self {
            paths: paths.clone(),
            limits: None,
        }
    }

    /// The files of `foundation mcp` in `paths`: those under the folders `files`, else
    /// under the working directory, and none under the data directory `data`, which it
    /// makes after it checks the folders, when it is not there.
    ///
    /// # Errors
    ///
    /// - [`Error::WideFolder`] with no `files`, when the working directory is `/` or
    ///   the home folder.
    /// - [`Error::Folder`] for the first folder of `files` that does not resolve, does
    ///   not read, or is a file.
    /// - [`Error::Data`] when `data` is not made, or does not resolve.
    pub(crate) fn mcp(
        paths: &Paths,
        files: &[PathBuf],
        data: &Path,
    ) -> Result<Self, Error> {
        let unmade = |error| Error::Data {
            path: data.to_owned(),
            error,
        };
        let folder = |path: &Path, cause| Error::Folder {
            path: path.to_owned(),
            cause,
        };
        let resolve = |path: &Path| {
            let failed = |error| folder(path, Unusable::Failed(error));
            paths.resolve(path).map_err(failed)
        };
        let working = resolve(Path::new("."))?;
        let folders = if files.is_empty() {
            if working.parent().is_none() || paths.home().as_ref() == Some(&working) {
                return Err(Error::WideFolder { working });
            }
            vec![working.clone()]
        } else {
            let mut folders = Vec::with_capacity(files.len());
            for file in files {
                let resolved = resolve(file)?;
                match paths.read(&resolved, 0) {
                    Ok(Found::Directory(_) | Found::Large(Kind::Directory)) => {
                        folders.push(resolved);
                    }
                    Ok(Found::File(_) | Found::Large(Kind::File)) => {
                        return Err(folder(file, Unusable::File));
                    }
                    Err(error) => return Err(folder(file, Unusable::Failed(error))),
                }
            }
            folders
        };
        paths.make_dir(data).map_err(unmade)?;
        let data = paths.resolve(data).map_err(unmade)?;
        let within = paths
            .within(&folders, std::slice::from_ref(&data))
            .map_err(unmade)?;
        Ok(Self {
            paths: within,
            limits: Some(Limits {
                folders,
                data,
                working,
            }),
        })
    }

    /// The error of a call at `path` that its limits refused with `error`, `Outside`,
    /// `Hidden`, `Vacant`, or `Replaced`.
    fn refused(&self, path: &Path, error: paths::Error) -> Error {
        let path = path.to_owned();
        let limits = self
            .limits
            .clone()
            .expect("invariant: only the paths of a scope with limits refuse a path");
        match error {
            paths::Error::Hidden => Error::Hidden {
                path,
                data: limits.data,
            },
            paths::Error::Vacant => Error::NoData {
                path,
                data: limits.data,
            },
            paths::Error::Replaced => Error::FolderChanged {
                path,
                folders: limits.folders,
            },
            _ => Error::Outside { path, limits },
        }
    }

    /// The error of a resolve or a write of the plan file `out` that failed.
    fn unwritten(&self, out: &Path, error: paths::Error) -> Error {
        let cause = match error {
            paths::Error::Outside
            | paths::Error::Hidden
            | paths::Error::Vacant
            | paths::Error::Replaced => {
                return self.refused(out, error);
            }
            paths::Error::NotFound => Unwritten::NotFound,
            paths::Error::Io { code } => Unwritten::Io { code },
        };
        Error::Write {
            path: out.to_owned(),
            cause,
        }
    }

    /// Checks that the plan file `out` resolves to a name that ends in `.plan`, for a
    /// scope with limits.
    ///
    /// # Errors
    ///
    /// - [`Error::PlanName`] when it does not.
    /// - [`Error::Outside`], [`Error::Hidden`], [`Error::NoData`],
    ///   [`Error::FolderChanged`], or [`Error::Write`] when `out` does not resolve.
    pub(crate) fn out(&self, out: &Path) -> Result<(), Error> {
        if self.limits.is_none() {
            return Ok(());
        }
        let resolved = self
            .paths
            .resolve(out)
            .map_err(|error| self.unwritten(out, error))?;
        if resolved.extension() != Some("plan".as_ref()) {
            return Err(Error::PlanName {
                path: out.to_owned(),
                resolved,
            });
        }
        Ok(())
    }

    /// The bytes of the file at `path`, of at most the bytes of one request.
    ///
    /// # Errors
    ///
    /// - [`Error::LargeRequest`] when it has more.
    /// - [`Error::Read`] when it is a directory, is not there, or does not read.
    /// - [`Error::Outside`], [`Error::Hidden`], or [`Error::FolderChanged`] when the
    ///   limits refuse it.
    pub(crate) fn read(&self, path: &Path) -> Result<Vec<u8>, Error> {
        let unread = |cause| Error::Read {
            path: path.to_owned(),
            cause,
        };
        match self.paths.read(path, most()) {
            Ok(Found::File(bytes)) => Ok(bytes),
            Ok(Found::Large(Kind::File)) => Err(Error::LargeRequest),
            Ok(Found::Directory(_) | Found::Large(Kind::Directory)) => {
                Err(unread(Unread::Directory))
            }
            Err(paths::Error::NotFound) => Err(unread(Unread::NotFound)),
            Err(paths::Error::Io { code }) => Err(unread(Unread::Io { code })),
            Err(
                error @ (paths::Error::Outside
                | paths::Error::Hidden
                | paths::Error::Replaced),
            ) => Err(self.refused(path, error)),
            Err(paths::Error::Vacant) => {
                unreachable!("invariant: a read is never vacant")
            }
        }
    }

    /// Writes `bytes` to the plan file `out`.
    ///
    /// # Errors
    ///
    /// [`Error::Write`], [`Error::Outside`], [`Error::Hidden`], [`Error::NoData`], or
    /// [`Error::FolderChanged`] when the write fails.
    pub(crate) fn write(&self, out: &Path, bytes: &[u8]) -> Result<(), Error> {
        self.paths
            .write(out, bytes)
            .map_err(|error| self.unwritten(out, error))
    }
}

/// The most bytes that one request holds.
pub(crate) fn most() -> usize {
    usize::try_from(BODY_BYTES_MAX)
        .expect("invariant: the bound of a body fits a usize")
}
