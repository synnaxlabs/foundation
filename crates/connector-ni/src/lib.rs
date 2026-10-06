//! Reads and writes NI data acquisition devices through NI's driver, loaded at run
//! time.

#![expect(unsafe_code, reason = "NI's driver is a C library")]
#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

use std::ffi::{CStr, CString};
use std::fmt;
use std::path::Path;
use std::ptr;

use types::time::Span;

mod ffi;

use ffi::{Functions, Handle};

/// The name NI's driver has on the system's library search path.
const NAME: &str = if cfg!(windows) {
    "nicaiu.dll"
} else {
    "libnidaqmx.so"
};

/// NI's driver, loaded.
#[derive(Debug)]
pub struct Library {
    functions: Functions,
    /// Keeps the driver loaded while `functions` points into it.
    _library: libloading::Library,
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
        // SAFETY: the caller's contract, and `library` stays loaded as long as
        // `functions`.
        let functions = unsafe { Functions::find(&library) }?;
        Ok(Self {
            functions,
            _library: library,
        })
    }

    /// Creates an empty task named `name`. Dropping it clears it in the driver.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses, for example for a name in use.
    pub fn task(&self, name: &CStr) -> Result<Task<'_>, Error> {
        Task::create(&self.functions, name)
    }
}

/// A task of NI's driver: channels on one device that start, stop, and sample together.
#[derive(Debug)]
pub struct Task<'a> {
    functions: &'a Functions,
    handle: Handle,
    channels: u32,
}

/// Channels that a task adds, named by their NI physical channel, such as
/// `Dev1/ai0` or `Dev1/ai0:3` for four channels.
#[derive(Clone, Debug, PartialEq)]
pub enum Channel {
    /// Analog voltage inputs, in volts. The device sets its gain to measure from `min`
    /// to `max`.
    AnalogIn {
        /// The NI physical channel.
        physical: CString,
        /// The least voltage to measure.
        min: f64,
        /// The greatest voltage to measure.
        max: f64,
    },
    /// Analog voltage outputs, in volts, from `min` to `max`.
    AnalogOut {
        /// The NI physical channel.
        physical: CString,
        /// The least voltage to write.
        min: f64,
        /// The greatest voltage to write.
        max: f64,
    },
}

impl<'a> Task<'a> {
    fn create(functions: &'a Functions, name: &CStr) -> Result<Self, Error> {
        let mut handle = ptr::null_mut();
        // SAFETY: `name` ends with NUL and `handle` is valid for one write.
        functions.check(unsafe {
            (functions.create_task)(name.as_ptr(), &raw mut handle)
        })?;
        Ok(Self {
            functions,
            handle,
            channels: 0,
        })
    }

    /// Adds `channel` to the task.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses the channel, for example a physical
    /// channel that does not exist or a range the device cannot measure.
    pub fn add(&mut self, channel: &Channel) -> Result<(), Error> {
        let code = match channel {
            // SAFETY: a live handle and NUL-terminated strings.
            Channel::AnalogIn { physical, min, max } => unsafe {
                (self.functions.analog_in)(
                    self.handle,
                    physical.as_ptr(),
                    c"".as_ptr(),
                    ffi::DEFAULT,
                    *min,
                    *max,
                    ffi::VOLTS,
                    ptr::null(),
                )
            },
            // SAFETY: a live handle and NUL-terminated strings.
            Channel::AnalogOut { physical, min, max } => unsafe {
                (self.functions.analog_out)(
                    self.handle,
                    physical.as_ptr(),
                    c"".as_ptr(),
                    *min,
                    *max,
                    ffi::VOLTS,
                    ptr::null(),
                )
            },
        };
        self.functions.check(code)?;
        // SAFETY: a live handle, and `self.channels` is valid for one write.
        let code =
            unsafe { (self.functions.channels)(self.handle, &raw mut self.channels) };
        self.functions.check(code)
    }

    /// Samples every channel `rate` times a second on the device's own clock, without
    /// end, and keeps up to `buffer` samples of each channel until a read takes them.
    /// Without a clock, each read or write takes one sample at once.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the device cannot sample at `rate`.
    pub fn clock(&mut self, rate: f64, buffer: u64) -> Result<(), Error> {
        // SAFETY: a live handle and a NUL-terminated string.
        let code = unsafe {
            (self.functions.clock)(
                self.handle,
                c"".as_ptr(),
                rate,
                ffi::RISING,
                ffi::CONTINUOUS,
                buffer,
            )
        };
        self.functions.check(code)
    }

    /// Starts the task.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the device cannot start it, for example when another
    /// task holds its channels.
    pub fn start(&mut self) -> Result<(), Error> {
        // SAFETY: a live handle.
        self.functions
            .check(unsafe { (self.functions.start_task)(self.handle) })
    }

    /// Stops the task. A started task can start again.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses.
    pub fn stop(&mut self) -> Result<(), Error> {
        // SAFETY: a live handle.
        self.functions
            .check(unsafe { (self.functions.stop_task)(self.handle) })
    }

    /// Reads `per_channel` samples of each channel into `out`, by scan: each channel
    /// of the first sample, then each channel of the next. Blocks the thread for up to
    /// `timeout`, which is cut to whole milliseconds; below zero acts as zero.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the task is not running, or when the samples do not come
    /// within `timeout`. `out` is then empty.
    pub fn read_analog(
        &mut self,
        per_channel: u32,
        out: &mut Vec<f64>,
        timeout: Span,
    ) -> Result<(), Error> {
        let size = per_channel.saturating_mul(self.channels);
        out.clear();
        out.resize(usize::try_from(size).unwrap_or(usize::MAX), 0.0);
        let mut read = 0;
        // SAFETY: a live handle, `out` holds `size` values, and `read` is valid for
        // one write.
        let code = unsafe {
            (self.functions.read_analog)(
                self.handle,
                i32::try_from(per_channel).unwrap_or(i32::MAX),
                seconds(timeout),
                ffi::BY_SCAN,
                out.as_mut_ptr(),
                size,
                &raw mut read,
                ptr::null_mut(),
            )
        };
        let read = u32::try_from(read)
            .unwrap_or(0)
            .saturating_mul(self.channels);
        out.truncate(usize::try_from(read).unwrap_or(usize::MAX));
        self.functions.check(code).inspect_err(|_| out.clear())
    }

    /// Writes `values` to the task's outputs, by scan as [`Task::read_analog`] reads.
    /// Blocks the thread for up to `timeout`, as [`Task::read_analog`] does.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the task is not running, when a value is outside its
    /// channel's range, or when the device does not take the values within
    /// `timeout`.
    ///
    /// # Panics
    ///
    /// When the length of `values` is not a multiple of the number of channels.
    pub fn write_analog(&mut self, values: &[f64], timeout: Span) -> Result<(), Error> {
        let channels = usize::try_from(self.channels).unwrap_or(usize::MAX);
        let per_channel = values.len().checked_div(channels).unwrap_or(0);
        assert!(
            per_channel.checked_mul(channels) == Some(values.len()),
            "{} values do not fill {channels} channels",
            values.len()
        );
        let mut written = 0;
        // SAFETY: a live handle, `values` holds `per_channel` values of each channel,
        // and `written` is valid for one write.
        let code = unsafe {
            (self.functions.write_analog)(
                self.handle,
                i32::try_from(per_channel).unwrap_or(i32::MAX),
                0,
                seconds(timeout),
                ffi::BY_SCAN,
                values.as_ptr(),
                &raw mut written,
                ptr::null_mut(),
            )
        };
        self.functions.check(code)
    }
}

impl Drop for Task<'_> {
    fn drop(&mut self) {
        // SAFETY: a live handle, used no more after this call. A failure leaves
        // nothing to do.
        unsafe { (self.functions.clear_task)(self.handle) };
    }
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
