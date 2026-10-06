#![expect(clippy::float_cmp, reason = "the stub gives whole numbers")]

use std::ptr;

use super::*;

#[test]
fn counts_the_channels_a_name_holds() {
    assert_eq!(count(c"Dev1/ai0"), 1);
    assert_eq!(count(c"Dev1/ai0:3"), 4);
    assert_eq!(count(c"Dev1/ai2:2"), 1);
    assert_eq!(count(c"Dev1/ai3:1"), 1);
}

fn create() -> *mut c_void {
    let mut handle = ptr::null_mut();
    // SAFETY: a valid out pointer.
    let code = unsafe { DAQmxCreateTask(c"t".as_ptr(), &raw mut handle) };
    assert_eq!(code, 0);
    handle
}

#[test]
fn reads_a_ramp_on_each_channel() {
    let handle = create();
    let add = |physical: &CStr| {
        // SAFETY: a live handle and NUL-terminated strings.
        unsafe {
            DAQmxCreateAIVoltageChan(
                handle,
                physical.as_ptr(),
                c"".as_ptr(),
                -1,
                -10.0,
                10.0,
                10_348,
                ptr::null(),
            )
        }
    };
    assert_eq!(add(c"Dev1/ai0:1"), 0);
    assert_eq!(add(c"fail/ai2"), FAIL);
    let mut out = [0.0; 4];
    let (mut read, mut reserved) = (0, 0);
    let mut read_into = |out: &mut [f64; 4]| {
        // SAFETY: a live handle, and out pointers valid for their sizes.
        unsafe {
            DAQmxReadAnalogF64(
                handle,
                2,
                1.0,
                0,
                out.as_mut_ptr(),
                4,
                &raw mut read,
                &raw mut reserved,
            )
        }
    };
    assert_eq!(read_into(&mut out), STOPPED);
    // SAFETY: a live handle.
    assert_eq!(unsafe { DAQmxStartTask(handle) }, 0);
    assert_eq!(read_into(&mut out), 0);
    assert_eq!(out, [0.0, 1000.0, 1.0, 1001.0]);
    assert_eq!(read_into(&mut out), 0);
    assert_eq!(out, [2.0, 1002.0, 3.0, 1003.0]);
    assert_eq!(read, 2);
    // SAFETY: a live handle, cleared once.
    assert_eq!(unsafe { DAQmxClearTask(handle) }, 0);
}

#[test]
fn refuses_an_output_outside_its_range() {
    let handle = create();
    // SAFETY: a live handle and NUL-terminated strings.
    let added = unsafe {
        DAQmxCreateAOVoltageChan(
            handle,
            c"Dev1/ao0".as_ptr(),
            c"".as_ptr(),
            0.0,
            5.0,
            10_348,
            ptr::null(),
        )
    };
    assert_eq!(added, 0);
    // SAFETY: a live handle.
    assert_eq!(unsafe { DAQmxStartTask(handle) }, 0);
    let (mut written, mut reserved) = (0, 0);
    let mut write = |values: &[f64]| {
        // SAFETY: a live handle, and `values` holds one sample.
        unsafe {
            DAQmxWriteAnalogF64(
                handle,
                1,
                0,
                1.0,
                0,
                values.as_ptr(),
                &raw mut written,
                &raw mut reserved,
            )
        }
    };
    assert_eq!(write(&[2.5]), 0);
    assert_eq!(write(&[5.5]), RANGE);
    // SAFETY: a live handle, cleared once.
    assert_eq!(unsafe { DAQmxClearTask(handle) }, 0);
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
