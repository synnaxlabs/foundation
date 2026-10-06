//! Tasks that read or write voltages. A task holds inputs or outputs, never both, as
//! NI's driver requires.

use std::ptr;

use types::time::Span;

use super::ffi::{BY_SCAN, CONTINUOUS, DEFAULT, RISING, VOLTS};
use super::{Error, Library, Task, scans, seconds, size, text, values};

/// A task that reads voltages. Dropping it clears it in the driver.
#[derive(Debug)]
pub struct Input(Task);

impl Input {
    /// Creates an empty task named `name`.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses, as for a name already in use.
    ///
    /// # Panics
    ///
    /// When `name` holds a NUL byte.
    pub fn create(library: &Library, name: &str) -> Result<Self, Error> {
        Task::create(library, name).map(Self)
    }

    /// Adds the channels `physical` names (`Dev1/ai0`, or `Dev1/ai0:3` for four), which
    /// measure from `min` to `max` volts on the device's default terminal setup.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses, as for an unknown channel. A refused
    /// call can still leave some of the channels in the task.
    ///
    /// # Panics
    ///
    /// When `physical` holds a NUL byte.
    pub fn add(&mut self, physical: &str, min: f64, max: f64) -> Result<(), Error> {
        let task = &self.0;
        let physical = text(physical);
        // SAFETY: a live handle, NUL-terminated strings, and a null scale for none.
        let code = unsafe {
            (task.functions().analog_in)(
                task.handle,
                physical.as_ptr(),
                c"".as_ptr(),
                DEFAULT,
                min,
                max,
                VOLTS,
                ptr::null(),
            )
        };
        task.check(code)
    }

    /// Samples each channel `rate` times a second without end, on the device's own
    /// clock, into a driver buffer that holds `buffer` samples of each channel.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses, as for a rate the device cannot do.
    pub fn clock(&mut self, rate: f64, buffer: u64) -> Result<(), Error> {
        let task = &self.0;
        // SAFETY: a live handle and a NUL-terminated string.
        let code = unsafe {
            (task.functions().clock)(
                task.handle,
                c"".as_ptr(),
                rate,
                RISING,
                CONTINUOUS,
                buffer,
            )
        };
        task.check(code)
    }

    /// Starts sampling.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses.
    pub fn start(&mut self) -> Result<(), Error> {
        self.0.start()
    }

    /// Stops sampling. The task can start again.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses.
    pub fn stop(&mut self) -> Result<(), Error> {
        self.0.stop()
    }

    /// Reads into `out` by scan (each channel of the first sample, then each channel of
    /// the next), and gives the number of values read. It waits up to `timeout`, cut to
    /// whole milliseconds, for `out` to fill. A driver warning reads as success.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses, as when the task is not running or
    /// `timeout` passes before `out` fills.
    ///
    /// # Panics
    ///
    /// When `out` does not hold a whole number of samples of each channel, more than
    /// `i32::MAX` samples of each channel, or more than `u32::MAX` values.
    pub fn read(&mut self, out: &mut [f64], timeout: Span) -> Result<usize, Error> {
        let task = &self.0;
        let channels = task.channels()?;
        let per_channel = scans(out.len(), channels);
        let size = size(out.len());
        let (mut read, mut reserved) = (0, 0);
        // SAFETY: a live handle, `out` holds `size` values, and `read` and `reserved`
        // are valid for one write each.
        let code = unsafe {
            (task.functions().read_analog)(
                task.handle,
                per_channel,
                seconds(timeout),
                BY_SCAN,
                out.as_mut_ptr(),
                size,
                &raw mut read,
                &raw mut reserved,
            )
        };
        task.check(code)?;
        Ok(values(read, channels))
    }
}

/// A task that writes voltages. Dropping it clears it in the driver.
#[derive(Debug)]
pub struct Output(Task);

impl Output {
    /// Creates an empty task named `name`.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses, as for a name already in use.
    ///
    /// # Panics
    ///
    /// When `name` holds a NUL byte.
    pub fn create(library: &Library, name: &str) -> Result<Self, Error> {
        Task::create(library, name).map(Self)
    }

    /// Adds the channels `physical` names (`Dev1/ao0`, or `Dev1/ao0:1` for two), which
    /// write from `min` to `max` volts.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses, as for an unknown channel. A refused
    /// call can still leave some of the channels in the task.
    ///
    /// # Panics
    ///
    /// When `physical` holds a NUL byte.
    pub fn add(&mut self, physical: &str, min: f64, max: f64) -> Result<(), Error> {
        let task = &self.0;
        let physical = text(physical);
        // SAFETY: a live handle, NUL-terminated strings, and a null scale for none.
        let code = unsafe {
            (task.functions().analog_out)(
                task.handle,
                physical.as_ptr(),
                c"".as_ptr(),
                min,
                max,
                VOLTS,
                ptr::null(),
            )
        };
        task.check(code)
    }

    /// Starts the task, so writes reach the device.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses.
    pub fn start(&mut self) -> Result<(), Error> {
        self.0.start()
    }

    /// Stops the task. The task can start again.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses.
    pub fn stop(&mut self) -> Result<(), Error> {
        self.0.stop()
    }

    /// Writes `values` by scan (each channel of the first sample, then each channel of
    /// the next). It waits up to `timeout`, cut to whole milliseconds, for room in the
    /// driver's buffer. A driver warning reads as success.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses, as when the task is not running or a
    /// value is outside its channel's range.
    ///
    /// # Panics
    ///
    /// When `values` does not hold a whole number of samples of each channel, or holds
    /// more than `i32::MAX` samples of each channel.
    pub fn write(&mut self, values: &[f64], timeout: Span) -> Result<(), Error> {
        let task = &self.0;
        let per_channel = scans(values.len(), task.channels()?);
        let (mut written, mut reserved) = (0, 0);
        // SAFETY: a live handle, `values` holds `per_channel` samples of each channel,
        // and `written` and `reserved` are valid for one write each.
        let code = unsafe {
            (task.functions().write_analog)(
                task.handle,
                per_channel,
                0,
                seconds(timeout),
                BY_SCAN,
                values.as_ptr(),
                &raw mut written,
                &raw mut reserved,
            )
        };
        task.check(code)?;
        assert_eq!(written, per_channel, "the driver wrote every sample");
        Ok(())
    }
}
