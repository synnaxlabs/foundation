//! NI's driver, loaded at run time, and its tasks.

use std::ffi::{CStr, CString};
use std::fmt;
use std::path::Path;
use std::ptr;
use std::sync::Arc;

use types::time::Span;

pub mod analog;
mod ffi;

use ffi::{Functions, Handle};

/// The size of the buffer for the driver's error message, in bytes.
const MESSAGE: usize = 2048;

/// The name NI's driver has on the system's library search path.
const NAME: &str = if cfg!(windows) {
    "nicaiu.dll"
} else {
    "libnidaqmx.so"
};

/// NI's driver, loaded. A clone shares it, and it unloads when the last clone and
/// the last task drop.
#[derive(Clone, Debug)]
pub struct Library(Arc<Loaded>);

#[derive(Debug)]
struct Loaded {
    functions: Functions,
    /// Keeps the driver loaded while `functions` points into it. `None` when the
    /// functions are linked in, as in tests.
    _library: Option<libloading::Library>,
}

impl Library {
    /// Loads NI's driver from `path`, or by its usual name (`libnidaqmx.so`, or
    /// `nicaiu.dll` on Windows) from the system's search path when `path` is `None`.
    ///
    /// # Errors
    ///
    /// [`Error::Load`] when the library does not load, and [`Error::Missing`] when it
    /// lacks a function this crate calls.
    ///
    /// # Safety
    ///
    /// The library is NI's driver, or one with the same functions and signatures.
    /// Loading it runs its initialization code.
    pub unsafe fn open(path: Option<&Path>) -> Result<Self, Error> {
        let path = path.unwrap_or(Path::new(NAME));
        // SAFETY: the caller's contract.
        let library = unsafe { libloading::Library::new(path) }.map_err(|error| {
            let reason = std::error::Error::source(&error);
            Error::Load(reason.map_or_else(|| error.to_string(), ToString::to_string))
        })?;
        // SAFETY: the caller's contract, and `Loaded` keeps `library` loaded as long
        // as `functions`.
        let functions = unsafe { Functions::find(&library) }?;
        Ok(Self(Arc::new(Loaded {
            functions,
            _library: Some(library),
        })))
    }

    fn functions(&self) -> &Functions {
        &self.0.functions
    }

    /// The driver's description of the last error on this thread.
    fn message(&self) -> String {
        let mut message = [0_u8; MESSAGE];
        let size = u32::try_from(MESSAGE).expect("the message size fits a u32");
        // SAFETY: `message` holds `size` bytes.
        unsafe { (self.functions().error)(message.as_mut_ptr().cast(), size) };
        CStr::from_bytes_until_nul(&message)
            .map(|message| message.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Gives `Ok` for a code of zero or above (success or a warning), and the
    /// driver's error for a code below zero.
    fn check(&self, code: i32) -> Result<(), Error> {
        if code >= 0 {
            return Ok(());
        }
        let message = self.message();
        Err(Error::Daqmx { code, message })
    }
}

/// A task of the driver, which [`analog`] wraps by direction. Dropping it clears it
/// in the driver.
#[derive(Debug)]
struct Task {
    library: Library,
    handle: Handle,
}

// SAFETY: NI documents its driver as safe to call from any thread, so a handle may
// move between threads. `Task` is not `Sync`: each call takes `&mut self`.
unsafe impl Send for Task {}

impl Task {
    fn create(library: &Library, name: &str) -> Result<Self, Error> {
        let name = text(name);
        let mut handle = ptr::null_mut();
        // SAFETY: `name` ends with NUL and `handle` is valid for one write.
        let code = unsafe {
            (library.functions().create_task)(name.as_ptr(), &raw mut handle)
        };
        library.check(code)?;
        Ok(Self {
            library: library.clone(),
            handle,
        })
    }

    fn functions(&self) -> &Functions {
        self.library.functions()
    }

    fn check(&self, code: i32) -> Result<(), Error> {
        self.library.check(code)
    }

    fn start(&mut self) -> Result<(), Error> {
        // SAFETY: a live handle.
        self.check(unsafe { (self.functions().start_task)(self.handle) })
    }

    fn stop(&mut self) -> Result<(), Error> {
        // SAFETY: a live handle.
        self.check(unsafe { (self.functions().stop_task)(self.handle) })
    }

    /// Gives the number of channels in the task, from the driver, so a failed add
    /// that left some channels behind still counts them.
    fn channels(&self) -> Result<u32, Error> {
        let mut channels = 0;
        // SAFETY: a live handle, and `channels` is valid for one write.
        self.check(unsafe {
            (self.functions().channels)(self.handle, &raw mut channels)
        })?;
        Ok(channels)
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        // SAFETY: a live handle, used no more after this call. A failure leaves
        // nothing to do.
        unsafe { (self.functions().clear_task)(self.handle) };
    }
}

/// The number of samples of each of `channels` channels in `len` values.
///
/// # Panics
///
/// When `len` values do not fill the channels, or fill each with more than
/// `i32::MAX` samples.
fn scans(len: usize, channels: u32) -> i32 {
    let channels = usize::try_from(channels).expect("a u32 fits a usize");
    let scans = len.checked_div(channels).unwrap_or(0);
    assert!(
        scans.checked_mul(channels) == Some(len),
        "{len} values do not fill {channels} channels"
    );
    i32::try_from(scans).expect("at most i32::MAX samples of each channel")
}

/// The number of values in `read` samples of each of `channels` channels.
///
/// # Panics
///
/// When `read` is below zero, which the driver never gives.
fn values(read: i32, channels: u32) -> usize {
    let read = usize::try_from(read).expect("the driver reads zero samples or more");
    let channels = usize::try_from(channels).expect("a u32 fits a usize");
    read.checked_mul(channels)
        .expect("the driver reads at most the buffer")
}

/// The size of a buffer of `len` values, for the driver.
///
/// # Panics
///
/// When `len` is above `u32::MAX`.
fn size(len: usize) -> u32 {
    u32::try_from(len).expect("at most u32::MAX values")
}

/// `text` as a C string.
///
/// # Panics
///
/// When `text` holds a NUL byte.
fn text(text: &str) -> CString {
    CString::new(text).expect("a name holds no NUL byte")
}

/// `span` in seconds, cut to whole milliseconds, from zero to `u32::MAX`
/// milliseconds.
fn seconds(span: Span) -> f64 {
    let millis = span.nanos().max(0).checked_div(1_000_000).unwrap_or(0);
    f64::from(u32::try_from(millis).unwrap_or(u32::MAX)) / 1000.0
}

/// Why a call to NI's driver failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The driver did not load. It holds the system loader's message.
    Load(String),
    /// The driver lacks this function.
    Missing(&'static str),
    /// The driver refused a call.
    Daqmx {
        /// NI's error code, below zero.
        code: i32,
        /// NI's description of the error.
        message: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(message) => write!(f, "NI-DAQmx did not load: {message}"),
            Self::Missing(name) => write!(f, "NI-DAQmx has no function {name}"),
            Self::Daqmx { code, message } => {
                write!(f, "NI-DAQmx error {code}: {message}")
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
