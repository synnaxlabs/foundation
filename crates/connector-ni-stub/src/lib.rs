//! A stand-in for NI's `libnidaqmx.so`, for tests: the driver functions that
//! `connector-ni` calls, over a fake device. Its error codes are its own.
//!
//! - Analog input `i` of a task reads `1000 i + n` at sample `n`.
//! - Reads and writes lay out samples by scan: each channel of the first sample, then
//!   each channel of the next. The stub ignores the layout argument.
//! - A physical channel `name<a:b>` holds the channels `a` to `b`, as NI's `ai0:3`
//!   does. Any other name holds one channel.
//! - A physical channel that starts with `fail/` fails the call that adds it with
//!   [`FAIL`].
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
/// The message of each failure.
pub const MESSAGE: &CStr = c"the stub refused the call";

#[derive(Debug, Default)]
struct Task {
    running: bool,
    /// The range of each output channel, or `None` for an input.
    channels: Vec<Option<(f64, f64)>>,
    sample: u64,
}

/// The number of channels that `physical` names.
fn count(physical: &CStr) -> usize {
    let name = physical.to_str().unwrap_or_default();
    let range = name.rsplit_once(':').and_then(|(head, last)| {
        let first: usize = head
            .rsplit(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()?;
        let last: usize = last.parse().ok()?;
        last.checked_sub(first)?.checked_add(1)
    });
    range.unwrap_or(1)
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

/// Adds the channels `physical` names, each with `range`.
///
/// # Safety
///
/// As [`task`], and `physical` is a NUL-terminated string.
unsafe fn add(
    handle: *mut c_void,
    physical: *const c_char,
    range: Option<(f64, f64)>,
) -> i32 {
    // SAFETY: the caller's contract.
    let physical = unsafe { CStr::from_ptr(physical) };
    if physical.to_bytes().starts_with(b"fail/") {
        return FAIL;
    }
    // SAFETY: the caller's contract.
    let task = unsafe { task(handle) };
    let n = count(physical);
    task.channels.extend(std::iter::repeat_n(range, n));
    0
}

/// Creates an empty task.
///
/// # Safety
///
/// `out` is valid for one write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn DAQmxCreateTask(
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
pub unsafe extern "C" fn DAQmxStartTask(handle: *mut c_void) -> i32 {
    // SAFETY: the caller's contract.
    unsafe { task(handle) }.running = true;
    0
}

/// Stops the task.
///
/// # Safety
///
/// As [`task`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn DAQmxStopTask(handle: *mut c_void) -> i32 {
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
pub unsafe extern "C" fn DAQmxClearTask(handle: *mut c_void) -> i32 {
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
pub unsafe extern "C" fn DAQmxGetTaskNumChans(
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
pub unsafe extern "C" fn DAQmxCreateAIVoltageChan(
    handle: *mut c_void,
    physical: *const c_char,
    _name: *const c_char,
    _terminal: i32,
    _min: f64,
    _max: f64,
    _units: i32,
    _scale: *const c_char,
) -> i32 {
    // SAFETY: the caller's contract.
    unsafe { add(handle, physical, None) }
}

/// Adds analog output channels with the range `min` to `max`.
///
/// # Safety
///
/// As [`add`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn DAQmxCreateAOVoltageChan(
    handle: *mut c_void,
    physical: *const c_char,
    _name: *const c_char,
    min: f64,
    max: f64,
    _units: i32,
    _scale: *const c_char,
) -> i32 {
    // SAFETY: the caller's contract.
    unsafe { add(handle, physical, Some((min, max))) }
}

/// Accepts any timing.
///
/// # Safety
///
/// None: it reads no argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn DAQmxCfgSampClkTiming(
    _handle: *mut c_void,
    _source: *const c_char,
    _rate: f64,
    _edge: i32,
    _mode: i32,
    _buffer: u64,
) -> i32 {
    0
}

/// Reads `per_channel` samples of each channel into `out`.
///
/// # Safety
///
/// As [`task`]. `out` is valid for `size` writes, and `read` for one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn DAQmxReadAnalogF64(
    handle: *mut c_void,
    per_channel: i32,
    _timeout: f64,
    _layout: u32,
    out: *mut f64,
    size: u32,
    read: *mut i32,
    _reserved: *mut u32,
) -> i32 {
    // SAFETY: the caller's contract.
    let task = unsafe { task(handle) };
    if !task.running {
        return STOPPED;
    }
    let (Ok(n), Ok(size)) = (u64::try_from(per_channel), usize::try_from(size)) else {
        return SIZE;
    };
    let channels = task.channels.len();
    let Some(want) = usize::try_from(n)
        .ok()
        .and_then(|n| n.checked_mul(channels))
    else {
        return SIZE;
    };
    if want > size {
        return SIZE;
    }
    // SAFETY: the caller's contract, and `want` is at most `size`.
    let out = unsafe { std::slice::from_raw_parts_mut(out, want) };
    let channel = (0..channels).cycle();
    let sample = (0..n).flat_map(|offset| std::iter::repeat_n(offset, channels));
    for ((slot, channel), offset) in out.iter_mut().zip(channel).zip(sample) {
        let sample = task.sample.saturating_add(offset);
        *slot = f64::from(u32::try_from(channel).unwrap_or(u32::MAX)) * 1000.0
            + f64::from(u32::try_from(sample).unwrap_or(u32::MAX));
    }
    task.sample = task.sample.saturating_add(n);
    // SAFETY: the caller's contract.
    unsafe { read.write(per_channel) };
    0
}

/// Writes `per_channel` samples of each channel from `values`.
///
/// # Safety
///
/// As [`task`]. `values` is valid for `per_channel` reads per channel, and
/// `written` for one write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn DAQmxWriteAnalogF64(
    handle: *mut c_void,
    per_channel: i32,
    _start: u32,
    _timeout: f64,
    _layout: u32,
    values: *const f64,
    written: *mut i32,
    _reserved: *mut u32,
) -> i32 {
    // SAFETY: the caller's contract.
    let task = unsafe { task(handle) };
    if !task.running {
        return STOPPED;
    }
    let Ok(n) = usize::try_from(per_channel) else {
        return SIZE;
    };
    let Some(len) = n.checked_mul(task.channels.len()) else {
        return SIZE;
    };
    // SAFETY: the caller's contract.
    let values = unsafe { std::slice::from_raw_parts(values, len) };
    let ranges = task.channels.iter().cycle();
    for (value, range) in values.iter().zip(ranges) {
        let Some((min, max)) = range else {
            return RANGE;
        };
        if !(min..=max).contains(&value) {
            return RANGE;
        }
    }
    // SAFETY: the caller's contract.
    unsafe { written.write(per_channel) };
    0
}

/// Copies [`MESSAGE`] into `out`, cut to `size` bytes with its NUL.
///
/// # Safety
///
/// `out` is valid for `size` writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn DAQmxGetExtendedErrorInfo(out: *mut c_char, size: u32) -> i32 {
    let Some(room) = usize::try_from(size)
        .ok()
        .and_then(|size| size.checked_sub(1))
    else {
        return 0;
    };
    let bytes = MESSAGE.to_bytes();
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
