//! Tasks that read or write digital lines, one channel for each line. A task holds
//! inputs or outputs, never both, as NI's driver requires.

use types::time::Span;

use super::ffi::BY_SCAN;
use super::{Error, Library, Read, Task, Written, scans, seconds, size, values};

/// A task that reads digital lines. Dropping it clears it in the driver.
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

    /// Adds the lines `lines` names (`Dev1/port0/line0`, or `Dev1/port0/line0:7` for
    /// eight), one channel for each line.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses, as for an unknown line. A refused
    /// call can still leave some of the lines in the task.
    ///
    /// # Panics
    ///
    /// When `lines` holds a NUL byte.
    pub fn add(&mut self, lines: &str) -> Result<(), Error> {
        self.0.lines(self.0.functions().digital_in, lines)
    }

    /// Samples each line `rate` times a second without end, on the device's own
    /// clock, into a driver buffer that holds `buffer` samples of each line. Without a
    /// clock, a read samples the lines when called.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses, as for a device with no clock for
    /// its lines.
    pub fn clock(&mut self, rate: f64, buffer: u64) -> Result<(), Error> {
        self.0.clock(rate, buffer)
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

    /// Reads into `out` by scan (each line of the first sample, then each line of the
    /// next), 0 for low and 1 for high, and gives the number of values read and the
    /// driver's warning. It waits up to `timeout`, cut to whole milliseconds, for `out`
    /// to fill.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses, as when the task is not running or
    /// `timeout` passes before `out` fills.
    ///
    /// # Panics
    ///
    /// When `out` does not hold a whole number of samples of each line, more than
    /// `i32::MAX` samples of each line, or more than `u32::MAX` values.
    pub fn read(&mut self, out: &mut [u8], timeout: Span) -> Result<Read, Error> {
        let task = &self.0;
        let channels = task.channels()?;
        let per_channel = scans(out.len(), channels);
        let size = size(out.len());
        let (mut read, mut bytes, mut reserved) = (0, 0, 0);
        // SAFETY: a live handle, `out` holds `size` bytes, and `read`, `bytes`, and
        // `reserved` are valid for one write each.
        let code = unsafe {
            (task.functions().read_digital)(
                task.handle,
                per_channel,
                seconds(timeout),
                BY_SCAN,
                out.as_mut_ptr(),
                size,
                &raw mut read,
                &raw mut bytes,
                &raw mut reserved,
            )
        };
        let warning = task.library.outcome(code)?;
        Ok(Read {
            values: values(read, channels),
            warning,
        })
    }
}

/// A task that writes digital lines. Dropping it clears it in the driver.
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

    /// Adds the lines `lines` names (`Dev1/port0/line0`, or `Dev1/port0/line0:7` for
    /// eight), one channel for each line.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses, as for an unknown line. A refused
    /// call can still leave some of the lines in the task.
    ///
    /// # Panics
    ///
    /// When `lines` holds a NUL byte.
    pub fn add(&mut self, lines: &str) -> Result<(), Error> {
        self.0.lines(self.0.functions().digital_out, lines)
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

    /// Writes `values` by scan (each line of the first sample, then each line of the
    /// next): `true` sets a line high. It gives the driver's warning. It waits up to
    /// `timeout`, cut to whole milliseconds, for room in the driver's buffer.
    ///
    /// # Errors
    ///
    /// [`Error::Daqmx`] when the driver refuses, as when the task is not running.
    ///
    /// # Panics
    ///
    /// When `values` does not hold a whole number of samples of each line, or holds
    /// more than `i32::MAX` samples of each line.
    pub fn write(&mut self, values: &[bool], timeout: Span) -> Result<Written, Error> {
        let task = &self.0;
        let per_channel = scans(values.len(), task.channels()?);
        let (mut written, mut reserved) = (0, 0);
        // SAFETY: a live handle, `values` holds `per_channel` samples of each line as
        // bytes of 0 or 1, and `written` and `reserved` are valid for one write each.
        let code = unsafe {
            (task.functions().write_digital)(
                task.handle,
                per_channel,
                0,
                seconds(timeout),
                BY_SCAN,
                values.as_ptr().cast::<u8>(),
                &raw mut written,
                &raw mut reserved,
            )
        };
        let warning = task.library.outcome(code)?;
        assert_eq!(written, per_channel, "the driver wrote every sample");
        Ok(Written { warning })
    }
}
