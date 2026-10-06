//! A stand-in for NI's `libnidaqmx.so`, for the tests of `connector-ni`: the driver
//! functions it calls, over a fake device. Its error codes are its own.
//!
//! - Analog input `i` of a task reads `1000 i + n` at sample `n`, and digital line `i`
//!   reads `(i + n) % 2`.
//! - Reads and writes lay out samples by scan: each channel of the first sample, then
//!   each channel of the next.
//! - A physical channel `Dev1/ai0:3` holds the channels 0 to 3, `Dev1/ai3:0` holds
//!   them in the other order, and `Dev1/ai0,Dev1/ai2` holds the two named.
//! - A physical channel that starts with `fail/` fails the call that adds it with
//!   [`FAIL`]. One that starts with `warn/` makes each start, read, and write give
//!   [`WARN`], which [`WARNING`] describes. One that starts with `short/` makes each
//!   read and write take half the samples asked for.
//! - A digital channel names lines. A port with no line (`Dev1/port0`) counts as one
//!   line, not as each line of the port.
//! - A task holds one kind of channel (analog or digital, input or output), and reads
//!   or writes only that kind: [`DIRECTION`].
//! - An argument value the stub does not model fails with [`ARGUMENT`]: a terminal
//!   other than the default, units other than volts, a range with `min >= max`, a
//!   clock other than continuous on the rising edge with a rate above zero, digital
//!   lines grouped other than one channel for each line, and a layout other than by
//!   scan.
//! - A read or write on a task that is not running fails with [`STOPPED`].
//! - A written value outside its channel's range fails with [`RANGE`].
//! - A buffer smaller than the request fails with [`SIZE`].

#![expect(unsafe_code, reason = "the functions are a C library's")]
#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions
)]

use std::ffi::{CStr, c_char, c_void};

/// The code of a channel whose name starts with `fail/`.
pub const FAIL: i32 = -201_000;
/// The code of a read or write on a task that is not running.
pub const STOPPED: i32 = -201_001;
/// The code of a written value outside its channel's range.
pub const RANGE: i32 = -201_002;
/// The code of a buffer smaller than the request.
pub const SIZE: i32 = -201_003;
/// The code of an argument value the stub does not model.
pub const ARGUMENT: i32 = -201_004;
/// The code of a channel added to a task of another kind, or a read or write of
/// another kind.
pub const DIRECTION: i32 = -201_005;
/// The warning code of a start, read, or write on a task with a `warn/` channel.
pub const WARN: i32 = 201_000;
/// The message of each failure.
pub const MESSAGE: &CStr = c"the stub refused the call";
/// The description of [`WARN`].
pub const WARNING: &CStr = c"the stub warns";

const DEFAULT: i32 = -1;
const VOLTS: i32 = 10_348;
const RISING: i32 = 10_280;
const CONTINUOUS: i32 = 10_123;
const BY_SCAN: u32 = 1;
const PER_LINE: i32 = 0;

/// A kind of channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    AnalogIn,
    AnalogOut,
    DigitalIn,
    DigitalOut,
}

#[derive(Debug, Default)]
struct Task {
    running: bool,
    channels: Vec<Kind>,
    /// The range of each channel of an analog output task.
    ranges: Vec<(f64, f64)>,
    sample: u64,
    warn: bool,
    short: bool,
}

/// The number of channels that `physical` names.
fn count(physical: &CStr) -> usize {
    let name = physical.to_str().unwrap_or_default();
    let one = |name: &str| {
        let range = name.rsplit_once(':').and_then(|(head, last)| {
            let first = head.rsplit(|c: char| !c.is_ascii_digit()).next()?;
            let (first, last): (usize, usize) =
                (first.parse().ok()?, last.parse().ok()?);
            first.abs_diff(last).checked_add(1)
        });
        range.unwrap_or(1)
    };
    name.split(',').map(one).fold(0, usize::saturating_add)
}

/// Gives the task behind `handle`.
///
/// # Safety
///
/// `handle` came from [`DAQmxCreateTask`] and was not cleared.
unsafe fn task<'a>(handle: *mut c_void) -> &'a mut Task {
    // SAFETY: the caller's contract.
    unsafe { &mut *handle.cast::<Task>() }
}

/// Adds the channels `physical` names, each of `kind` and with `range` if it has one.
///
/// # Safety
///
/// As [`task`], and `physical` is a NUL-terminated string.
unsafe fn add(
    handle: *mut c_void,
    physical: *const c_char,
    kind: Kind,
    range: Option<(f64, f64)>,
) -> i32 {
    // SAFETY: the caller's contract.
    let physical = unsafe { CStr::from_ptr(physical) };
    let name = physical.to_bytes();
    if name.starts_with(b"fail/") {
        return FAIL;
    }
    if range.is_some_and(|(min, max)| min >= max) {
        return ARGUMENT;
    }
    // SAFETY: the caller's contract.
    let task = unsafe { task(handle) };
    if task.channels.first().is_some_and(|first| *first != kind) {
        return DIRECTION;
    }
    task.warn |= name.starts_with(b"warn/");
    task.short |= name.starts_with(b"short/");
    let n = count(physical);
    task.channels.extend(std::iter::repeat_n(kind, n));
    task.ranges.extend(
        range
            .into_iter()
            .flat_map(|range| std::iter::repeat_n(range, n)),
    );
    0
}

/// Reads `per_channel` samples of each channel of `kind` into `out`, with `value`
/// giving channel `i` at sample `n`.
///
/// # Safety
///
/// As [`task`]. `out` is valid for `size` writes, and `read` for one.
#[expect(clippy::too_many_arguments, reason = "the driver's arguments")]
unsafe fn read<T>(
    handle: *mut c_void,
    kind: Kind,
    per_channel: i32,
    layout: u32,
    out: *mut T,
    size: u32,
    read: *mut i32,
    value: impl Fn(u32, u32) -> T,
) -> i32 {
    // SAFETY: the caller's contract.
    let task = unsafe { task(handle) };
    if !task.running {
        return STOPPED;
    }
    if layout != BY_SCAN {
        return ARGUMENT;
    }
    if task.channels.iter().any(|channel| *channel != kind) {
        return DIRECTION;
    }
    let (Ok(asked), Ok(size)) = (usize::try_from(per_channel), usize::try_from(size))
    else {
        return SIZE;
    };
    let channels = task.channels.len();
    if asked.checked_mul(channels).is_none_or(|want| want > size) {
        return SIZE;
    }
    let n = if task.short { asked / 2 } else { asked };
    let want = n.saturating_mul(channels);
    // SAFETY: the caller's contract, and `want` is at most `size`.
    let out = unsafe { std::slice::from_raw_parts_mut(out, want) };
    let channel = (0..channels).cycle();
    let sample = (0..n).flat_map(|offset| std::iter::repeat_n(offset, channels));
    for ((slot, channel), offset) in out.iter_mut().zip(channel).zip(sample) {
        let sample = task
            .sample
            .saturating_add(u64::try_from(offset).unwrap_or(u64::MAX));
        let channel = u32::try_from(channel).unwrap_or(u32::MAX);
        *slot = value(channel, u32::try_from(sample).unwrap_or(u32::MAX));
    }
    task.sample = task
        .sample
        .saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
    // SAFETY: the caller's contract.
    unsafe { read.write(i32::try_from(n).unwrap_or(i32::MAX)) };
    if task.warn { WARN } else { 0 }
}

/// Writes `per_channel` samples of each channel of `kind` from `values`, with
/// `check` giving the code of values the task refuses.
///
/// # Safety
///
/// As [`task`]. `values` is valid for `per_channel` reads per channel, and
/// `written` for one write.
unsafe fn write<T>(
    handle: *mut c_void,
    kind: Kind,
    per_channel: i32,
    layout: u32,
    values: *const T,
    written: *mut i32,
    check: impl Fn(&Task, &[T]) -> Option<i32>,
) -> i32 {
    // SAFETY: the caller's contract.
    let task = unsafe { task(handle) };
    if !task.running {
        return STOPPED;
    }
    if layout != BY_SCAN {
        return ARGUMENT;
    }
    if task.channels.iter().any(|channel| *channel != kind) {
        return DIRECTION;
    }
    let Ok(n) = usize::try_from(per_channel) else {
        return SIZE;
    };
    let Some(len) = n.checked_mul(task.channels.len()) else {
        return SIZE;
    };
    // SAFETY: the caller's contract.
    let values = unsafe { std::slice::from_raw_parts(values, len) };
    if let Some(code) = check(task, values) {
        return code;
    }
    let n = if task.short {
        per_channel / 2
    } else {
        per_channel
    };
    // SAFETY: the caller's contract.
    unsafe { written.write(n) };
    if task.warn { WARN } else { 0 }
}

/// Creates an empty task.
///
/// # Safety
///
/// `out` is valid for one write.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DAQmxCreateTask(
    _name: *const c_char,
    out: *mut *mut c_void,
) -> i32 {
    let task = Box::into_raw(Box::<Task>::default());
    // SAFETY: the caller's contract.
    unsafe { out.write(task.cast()) };
    0
}

/// Starts the task.
///
/// # Safety
///
/// As [`task`].
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DAQmxStartTask(handle: *mut c_void) -> i32 {
    // SAFETY: the caller's contract.
    let task = unsafe { task(handle) };
    task.running = true;
    if task.warn { WARN } else { 0 }
}

/// Stops the task.
///
/// # Safety
///
/// As [`task`].
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DAQmxStopTask(handle: *mut c_void) -> i32 {
    // SAFETY: the caller's contract.
    unsafe { task(handle) }.running = false;
    0
}

/// Frees the task.
///
/// # Safety
///
/// As [`task`]. The handle is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DAQmxClearTask(handle: *mut c_void) -> i32 {
    // SAFETY: the caller's contract; `DAQmxCreateTask` boxed the task.
    drop(unsafe { Box::from_raw(handle.cast::<Task>()) });
    0
}

/// Gives the number of channels in the task.
///
/// # Safety
///
/// As [`task`], and `out` is valid for one write.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DAQmxGetTaskNumChans(
    handle: *mut c_void,
    out: *mut u32,
) -> i32 {
    // SAFETY: the caller's contract.
    let n = unsafe { task(handle) }.channels.len();
    // SAFETY: the caller's contract.
    unsafe { out.write(u32::try_from(n).unwrap_or(u32::MAX)) };
    0
}

/// Adds analog input channels.
///
/// # Safety
///
/// As [`add`].
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DAQmxCreateAIVoltageChan(
    handle: *mut c_void,
    physical: *const c_char,
    _name: *const c_char,
    terminal: i32,
    min: f64,
    max: f64,
    units: i32,
    _scale: *const c_char,
) -> i32 {
    if terminal != DEFAULT || units != VOLTS || min >= max {
        return ARGUMENT;
    }
    // SAFETY: the caller's contract.
    unsafe { add(handle, physical, Kind::AnalogIn, None) }
}

/// Adds analog output channels with the range `min` to `max`.
///
/// # Safety
///
/// As [`add`].
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DAQmxCreateAOVoltageChan(
    handle: *mut c_void,
    physical: *const c_char,
    _name: *const c_char,
    min: f64,
    max: f64,
    units: i32,
    _scale: *const c_char,
) -> i32 {
    if units != VOLTS {
        return ARGUMENT;
    }
    // SAFETY: the caller's contract.
    unsafe { add(handle, physical, Kind::AnalogOut, Some((min, max))) }
}

/// Accepts a continuous clock on the rising edge with a rate above zero.
///
/// # Safety
///
/// None: it reads no pointer.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DAQmxCfgSampClkTiming(
    _handle: *mut c_void,
    _source: *const c_char,
    rate: f64,
    edge: i32,
    mode: i32,
    _buffer: u64,
) -> i32 {
    if rate > 0.0 && edge == RISING && mode == CONTINUOUS {
        0
    } else {
        ARGUMENT
    }
}

/// Reads `per_channel` samples of each channel into `out`.
///
/// # Safety
///
/// As [`task`]. `out` is valid for `size` writes, and `read` for one.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DAQmxReadAnalogF64(
    handle: *mut c_void,
    per_channel: i32,
    _timeout: f64,
    layout: u32,
    out: *mut f64,
    size: u32,
    samples: *mut i32,
    _reserved: *mut u32,
) -> i32 {
    let value = |channel, sample| f64::from(channel) * 1000.0 + f64::from(sample);
    // SAFETY: the caller's contract.
    unsafe {
        read(
            handle,
            Kind::AnalogIn,
            per_channel,
            layout,
            out,
            size,
            samples,
            value,
        )
    }
}

/// Writes `per_channel` samples of each channel from `values`.
///
/// # Safety
///
/// As [`task`]. `values` is valid for `per_channel` reads per channel, and
/// `written` for one write.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DAQmxWriteAnalogF64(
    handle: *mut c_void,
    per_channel: i32,
    _start: u32,
    _timeout: f64,
    layout: u32,
    values: *const f64,
    written: *mut i32,
    _reserved: *mut u32,
) -> i32 {
    let check = |task: &Task, values: &[f64]| {
        let ranges = task.ranges.iter().cycle();
        let outside =
            |(value, (min, max)): (&f64, &(f64, f64))| !(min..=max).contains(&value);
        values.iter().zip(ranges).any(outside).then_some(RANGE)
    };
    // SAFETY: the caller's contract.
    unsafe {
        write(
            handle,
            Kind::AnalogOut,
            per_channel,
            layout,
            values,
            written,
            check,
        )
    }
}

/// Adds digital input lines, one channel for each line.
///
/// # Safety
///
/// As [`add`].
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DAQmxCreateDIChan(
    handle: *mut c_void,
    lines: *const c_char,
    _name: *const c_char,
    grouping: i32,
) -> i32 {
    if grouping != PER_LINE {
        return ARGUMENT;
    }
    // SAFETY: the caller's contract.
    unsafe { add(handle, lines, Kind::DigitalIn, None) }
}

/// Adds digital output lines, one channel for each line.
///
/// # Safety
///
/// As [`add`].
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DAQmxCreateDOChan(
    handle: *mut c_void,
    lines: *const c_char,
    _name: *const c_char,
    grouping: i32,
) -> i32 {
    if grouping != PER_LINE {
        return ARGUMENT;
    }
    // SAFETY: the caller's contract.
    unsafe { add(handle, lines, Kind::DigitalOut, None) }
}

/// Reads `per_channel` samples of each line into `out`, one byte for each.
///
/// # Safety
///
/// As [`task`]. `out` is valid for `size` writes, and `samples` and `bytes` for one
/// each.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DAQmxReadDigitalLines(
    handle: *mut c_void,
    per_channel: i32,
    _timeout: f64,
    layout: u32,
    out: *mut u8,
    size: u32,
    samples: *mut i32,
    bytes: *mut i32,
    _reserved: *mut u32,
) -> i32 {
    let value = |channel: u32, sample: u32| u8::from(channel % 2 != sample % 2);
    // SAFETY: the caller's contract.
    let code = unsafe {
        read(
            handle,
            Kind::DigitalIn,
            per_channel,
            layout,
            out,
            size,
            samples,
            value,
        )
    };
    // SAFETY: the caller's contract.
    unsafe { bytes.write(1) };
    code
}

/// Writes `per_channel` samples of each line from `values`, one byte for each.
///
/// # Safety
///
/// As [`task`]. `values` is valid for `per_channel` reads per line, and `written`
/// for one write.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DAQmxWriteDigitalLines(
    handle: *mut c_void,
    per_channel: i32,
    _start: u32,
    _timeout: f64,
    layout: u32,
    values: *const u8,
    written: *mut i32,
    _reserved: *mut u32,
) -> i32 {
    let check = |_: &Task, _: &[u8]| None;
    // SAFETY: the caller's contract.
    unsafe {
        write(
            handle,
            Kind::DigitalOut,
            per_channel,
            layout,
            values,
            written,
            check,
        )
    }
}

/// Copies [`MESSAGE`] into `out`, cut to `size` bytes with its NUL.
///
/// # Safety
///
/// `out` is valid for `size` writes.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DAQmxGetExtendedErrorInfo(
    out: *mut c_char,
    size: u32,
) -> i32 {
    // SAFETY: the caller's contract.
    unsafe { copy(MESSAGE, out, size) }
}

/// Copies [`WARNING`] into `out` for [`WARN`], and an empty text for any other
/// `code`, cut to `size` bytes with its NUL.
///
/// # Safety
///
/// `out` is valid for `size` writes.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DAQmxGetErrorString(
    code: i32,
    out: *mut c_char,
    size: u32,
) -> i32 {
    let text = if code == WARN { WARNING } else { c"" };
    // SAFETY: the caller's contract.
    unsafe { copy(text, out, size) }
}

/// Copies `text` into `out`, cut to `size` bytes with its NUL.
///
/// # Safety
///
/// `out` is valid for `size` writes.
unsafe fn copy(text: &CStr, out: *mut c_char, size: u32) -> i32 {
    let Some(room) = usize::try_from(size)
        .ok()
        .and_then(|size| size.checked_sub(1))
    else {
        return 0;
    };
    let bytes = text.to_bytes();
    let len = bytes.len().min(room);
    // SAFETY: the caller's contract, and `len` is less than `size`.
    let out = unsafe {
        std::slice::from_raw_parts_mut(out.cast::<u8>(), len.saturating_add(1))
    };
    for (slot, byte) in out.iter_mut().zip(bytes.iter().take(len).chain([&0])) {
        *slot = *byte;
    }
    0
}

#[cfg(test)]
mod tests;
