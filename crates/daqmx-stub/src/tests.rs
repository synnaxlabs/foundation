#![expect(clippy::float_cmp, reason = "the stub gives whole numbers")]

use std::ptr;

use super::*;

#[test]
fn gives_codes_below_zero_for_errors_as_the_driver_does() {
    let codes = [FAIL, STOPPED, RANGE, SIZE, ARGUMENT, DIRECTION, WARN];
    assert_eq!(codes.map(i32::signum), [-1, -1, -1, -1, -1, -1, 1]);
}

#[test]
fn takes_the_driver_values_of_its_arguments() {
    let handle = create();
    assert_eq!(analog_in(handle, c"Dev1/ai0", -1, 10_348), 0);
    clear(handle);
}

#[test]
fn counts_the_channels_a_name_holds() {
    assert_eq!(count(c"Dev1/ai0"), 1);
    assert_eq!(count(c"Dev1/ai0:3"), 4);
    assert_eq!(count(c"Dev1/ai2:2"), 1);
    assert_eq!(count(c"Dev1/ai3:1"), 3);
    assert_eq!(count(c"Dev1/ai0,Dev1/ai2:3"), 3);
}

fn create() -> *mut c_void {
    let mut handle = ptr::null_mut();
    // SAFETY: a valid out pointer.
    let code = unsafe { DAQmxCreateTask(c"t".as_ptr(), &raw mut handle) };
    assert_eq!(code, 0);
    handle
}

/// Adds analog inputs to the task behind `handle`.
fn analog_in(handle: *mut c_void, physical: &CStr, terminal: i32, units: i32) -> i32 {
    // SAFETY: a live handle and NUL-terminated strings.
    unsafe {
        DAQmxCreateAIVoltageChan(
            handle,
            physical.as_ptr(),
            c"".as_ptr(),
            terminal,
            -10.0,
            10.0,
            units,
            ptr::null(),
        )
    }
}

/// Adds analog outputs from 0 to `max` volts to the task behind `handle`.
fn output(handle: *mut c_void, physical: &CStr, max: f64) -> i32 {
    // SAFETY: a live handle and NUL-terminated strings.
    unsafe {
        DAQmxCreateAOVoltageChan(
            handle,
            physical.as_ptr(),
            c"".as_ptr(),
            0.0,
            max,
            VOLTS,
            ptr::null(),
        )
    }
}

/// Reads 2 samples of 2 channels with `layout`, and gives the code and the count.
fn read_analog(handle: *mut c_void, layout: u32, out: &mut [f64; 4]) -> (i32, i32) {
    let (mut read, mut reserved) = (0, 0);
    // SAFETY: a live handle, and out pointers valid for their sizes.
    let code = unsafe {
        DAQmxReadAnalogF64(
            handle,
            2,
            1.0,
            layout,
            out.as_mut_ptr(),
            4,
            &raw mut read,
            &raw mut reserved,
        )
    };
    (code, read)
}

fn start(handle: *mut c_void) {
    // SAFETY: a live handle.
    assert_eq!(unsafe { DAQmxStartTask(handle) }, 0);
}

fn clear(handle: *mut c_void) {
    // SAFETY: a live handle, cleared once.
    assert_eq!(unsafe { DAQmxClearTask(handle) }, 0);
}

#[test]
fn reads_a_ramp_on_each_channel() {
    let handle = create();
    assert_eq!(analog_in(handle, c"Dev1/ai0:1", DEFAULT, VOLTS), 0);
    assert_eq!(analog_in(handle, c"fail/ai2", DEFAULT, VOLTS), FAIL);
    let mut out = [0.0; 4];
    assert_eq!(read_analog(handle, BY_SCAN, &mut out), (STOPPED, 0));
    start(handle);
    assert_eq!(read_analog(handle, 0, &mut out), (ARGUMENT, 0));
    assert_eq!(read_analog(handle, BY_SCAN, &mut out), (0, 2));
    assert_eq!(out, [0.0, 1000.0, 1.0, 1001.0]);
    assert_eq!(read_analog(handle, BY_SCAN, &mut out), (0, 2));
    assert_eq!(out, [2.0, 1002.0, 3.0, 1003.0]);
    clear(handle);
}

#[test]
fn warns_and_reads_short_on_demand() {
    let handle = create();
    assert_eq!(analog_in(handle, c"warn/ai0", DEFAULT, VOLTS), 0);
    assert_eq!(analog_in(handle, c"short/ai1", DEFAULT, VOLTS), 0);
    start(handle);
    let mut out = [0.0; 4];
    assert_eq!(read_analog(handle, BY_SCAN, &mut out), (WARN, 1));
    assert_eq!(out[..2], [0.0, 1000.0]);
    clear(handle);
}

#[test]
fn refuses_what_it_does_not_model() {
    let handle = create();
    assert_eq!(analog_in(handle, c"Dev1/ai0", 10_083, VOLTS), ARGUMENT);
    assert_eq!(analog_in(handle, c"Dev1/ai0", DEFAULT, 10_000), ARGUMENT);
    assert_eq!(output(handle, c"Dev1/ao0", 0.0), ARGUMENT);
    assert_eq!(analog_in(handle, c"Dev1/ai0", DEFAULT, VOLTS), 0);
    assert_eq!(output(handle, c"Dev1/ao0", 5.0), DIRECTION);
    let clock = |rate, edge, mode| {
        // SAFETY: a live handle and a NUL-terminated string.
        unsafe { DAQmxCfgSampClkTiming(handle, c"".as_ptr(), rate, edge, mode, 100) }
    };
    assert_eq!(clock(1000.0, RISING, CONTINUOUS), 0);
    assert_eq!(clock(0.0, RISING, CONTINUOUS), ARGUMENT);
    assert_eq!(clock(1000.0, 10_171, CONTINUOUS), ARGUMENT);
    assert_eq!(clock(1000.0, RISING, 10_178), ARGUMENT);
    clear(handle);
}

#[test]
fn refuses_an_output_outside_its_range() {
    let handle = create();
    assert_eq!(output(handle, c"Dev1/ao0", 5.0), 0);
    assert_eq!(analog_in(handle, c"Dev1/ai0", DEFAULT, VOLTS), DIRECTION);
    start(handle);
    let mut out = [0.0; 4];
    assert_eq!(read_analog(handle, BY_SCAN, &mut out), (DIRECTION, 0));
    let (mut written, mut reserved) = (0, 0);
    let mut write = |value: f64, layout| {
        // SAFETY: a live handle, and `value` is one sample of one channel.
        unsafe {
            DAQmxWriteAnalogF64(
                handle,
                1,
                0,
                1.0,
                layout,
                &raw const value,
                &raw mut written,
                &raw mut reserved,
            )
        }
    };
    assert_eq!(write(2.5, BY_SCAN), 0);
    assert_eq!(write(2.5, 0), ARGUMENT);
    assert_eq!(write(5.5, BY_SCAN), RANGE);
    clear(handle);
}

#[test]
fn cuts_the_message_to_the_buffer() {
    let mut out: [c_char; 8] = [1; 8];
    // SAFETY: `out` holds 8 bytes.
    assert_eq!(unsafe { DAQmxGetExtendedErrorInfo(out.as_mut_ptr(), 8) }, 0);
    // SAFETY: the call wrote a NUL inside `out`.
    let got = unsafe { CStr::from_ptr(out.as_ptr()) };
    assert_eq!(got, c"the stu");
}

/// Adds digital lines to the task behind `handle`, and gives the code.
fn lines(handle: *mut c_void, physical: &CStr, output: bool, grouping: i32) -> i32 {
    let add = if output {
        DAQmxCreateDOChan
    } else {
        DAQmxCreateDIChan
    };
    // SAFETY: a live handle and NUL-terminated strings.
    unsafe { add(handle, physical.as_ptr(), c"".as_ptr(), grouping) }
}

#[test]
fn reads_and_writes_digital_lines() {
    let input = create();
    assert_eq!(lines(input, c"Dev1/port0/line0:1", false, 1), ARGUMENT);
    assert_eq!(lines(input, c"Dev1/port0/line0:1", false, PER_LINE), 0);
    assert_eq!(lines(input, c"Dev1/port0/line2", true, PER_LINE), DIRECTION);
    assert_eq!(analog_in(input, c"Dev1/ai0", DEFAULT, VOLTS), DIRECTION);
    start(input);
    let mut out = [9_u8; 4];
    let (mut read, mut bytes, mut reserved) = (0, 0, 0);
    // SAFETY: a live handle, and out pointers valid for their sizes.
    let code = unsafe {
        DAQmxReadDigitalLines(
            input,
            2,
            1.0,
            BY_SCAN,
            out.as_mut_ptr(),
            4,
            &raw mut read,
            &raw mut bytes,
            &raw mut reserved,
        )
    };
    assert_eq!((code, read, bytes), (0, 2, 1));
    assert_eq!(out, [0, 1, 1, 0]);
    let mut analog = [0.0; 4];
    assert_eq!(read_analog(input, BY_SCAN, &mut analog), (DIRECTION, 0));
    clear(input);

    let output = create();
    assert_eq!(lines(output, c"Dev1/port0/line0:1", true, PER_LINE), 0);
    start(output);
    let (mut written, mut reserved) = (0, 0);
    // SAFETY: a live handle, and two lines of one sample each.
    let code = unsafe {
        DAQmxWriteDigitalLines(
            output,
            1,
            0,
            1.0,
            BY_SCAN,
            [0, 1].as_ptr(),
            &raw mut written,
            &raw mut reserved,
        )
    };
    assert_eq!((code, written), (0, 1));
    clear(output);
}

#[test]
fn refuses_a_digital_write_of_no_samples_to_inputs() {
    let input = create();
    assert_eq!(lines(input, c"Dev1/port0/line0", false, PER_LINE), 0);
    start(input);
    let (mut written, mut reserved) = (0, 0);
    let values: [u8; 0] = [];
    // SAFETY: a live handle, and zero samples of one line.
    let code = unsafe {
        DAQmxWriteDigitalLines(
            input,
            0,
            0,
            1.0,
            BY_SCAN,
            values.as_ptr(),
            &raw mut written,
            &raw mut reserved,
        )
    };
    assert_eq!(code, DIRECTION);
    clear(input);
}
