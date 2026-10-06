//! Miri runs the tests that use [`linked`], so it catches a task the driver never
//! clears as a leak.

#![expect(clippy::float_cmp, reason = "each value compared is exact")]

use std::path::PathBuf;

use daqmx_stub as stub;

use super::analog::{Input, Output};
use super::*;

const SECOND: Span = Span::from_nanos(1_000_000_000);

/// The stub's functions, linked in with no library to load, so that Miri runs them.
fn linked() -> Library {
    let functions = Functions {
        create_task: stub::DAQmxCreateTask,
        start_task: stub::DAQmxStartTask,
        stop_task: stub::DAQmxStopTask,
        clear_task: stub::DAQmxClearTask,
        channels: stub::DAQmxGetTaskNumChans,
        analog_in: stub::DAQmxCreateAIVoltageChan,
        analog_out: stub::DAQmxCreateAOVoltageChan,
        clock: stub::DAQmxCfgSampClkTiming,
        read_analog: stub::DAQmxReadAnalogF64,
        write_analog: stub::DAQmxWriteAnalogF64,
        error: stub::DAQmxGetExtendedErrorInfo,
    };
    Library(Arc::new(Loaded {
        functions,
        _library: None,
    }))
}

fn refused(code: i32) -> Error {
    Error::Daqmx {
        code,
        message: "the stub refused the call".into(),
    }
}

fn input(library: &Library, physical: &str) -> Input {
    let mut task = Input::create(library, "input").unwrap();
    task.add(physical, -10.0, 10.0).unwrap();
    task.clock(1000.0, 100).unwrap();
    task.start().unwrap();
    task
}

fn output(library: &Library, physical: &str) -> Output {
    let mut task = Output::create(library, "output").unwrap();
    task.add(physical, 0.0, 5.0).unwrap();
    task.start().unwrap();
    task
}

fn reads_a_ramp(library: &Library) {
    let mut task = Input::create(library, "ramp").unwrap();
    task.add("Dev1/ai0:1", -10.0, 10.0).unwrap();
    task.add("Dev1/ai2", -10.0, 10.0).unwrap();
    task.clock(1000.0, 100).unwrap();
    task.start().unwrap();
    let mut out = [0.0; 6];
    assert_eq!(task.read(&mut out, SECOND), Ok(6));
    assert_eq!(out, [0.0, 1000.0, 2000.0, 1.0, 1001.0, 2001.0]);
    assert_eq!(task.read(&mut out[..3], SECOND), Ok(3));
    assert_eq!(out[..3], [2.0, 1002.0, 2002.0]);
}

#[test]
fn reads_each_channel_by_scan() {
    reads_a_ramp(&linked());
}

#[test]
fn gives_the_values_of_a_short_read() {
    let mut task = input(&linked(), "short/ai0:1");
    let mut out = [0.0; 4];
    assert_eq!(task.read(&mut out, SECOND), Ok(2));
    assert_eq!(out[..2], [0.0, 1000.0]);
}

#[test]
fn reads_through_a_warning() {
    let mut task = input(&linked(), "warn/ai0");
    let mut out = [0.0; 2];
    assert_eq!(task.read(&mut out, SECOND), Ok(2));
    assert_eq!(out, [0.0, 1.0]);
}

#[test]
fn counts_the_channels_a_failed_add_left() {
    let library = linked();
    let mut task = Input::create(&library, "partial").unwrap();
    task.add("Dev1/ai0", -10.0, 10.0).unwrap();
    assert_eq!(task.add("fail/ai1", -10.0, 10.0), Err(refused(stub::FAIL)));
    task.start().unwrap();
    let mut out = [0.0; 2];
    assert_eq!(task.read(&mut out, SECOND), Ok(2));
    assert_eq!(out, [0.0, 1.0]);
}

#[test]
fn refuses_a_read_on_a_stopped_task() {
    let mut task = input(&linked(), "Dev1/ai0");
    task.stop().unwrap();
    let mut out = [7.0];
    assert_eq!(task.read(&mut out, SECOND), Err(refused(stub::STOPPED)));
    assert_eq!(out, [7.0]);
}

#[test]
fn gives_the_driver_error_for_a_channel() {
    let library = linked();
    let mut task = Input::create(&library, "fail").unwrap();
    let error = task.add("fail/ai0", -10.0, 10.0).unwrap_err();
    assert_eq!(error, refused(stub::FAIL));
    assert_eq!(
        error.to_string(),
        "NI-DAQmx error -201000: the stub refused the call"
    );
}

#[test]
fn refuses_a_clock_the_device_cannot_do() {
    let library = linked();
    let mut task = Input::create(&library, "clock").unwrap();
    task.add("Dev1/ai0", -10.0, 10.0).unwrap();
    assert_eq!(task.clock(0.0, 100), Err(refused(stub::ARGUMENT)));
}

#[test]
fn writes_each_channel_within_its_range() {
    let mut task = output(&linked(), "Dev1/ao0:1");
    task.write(&[1.0, 2.0, 3.0, 4.0], SECOND).unwrap();
    assert_eq!(task.write(&[1.0, 5.5], SECOND), Err(refused(stub::RANGE)));
}

#[test]
#[should_panic(expected = "3 values do not fill 2 channels")]
fn panics_on_values_that_do_not_fill_the_channels() {
    let mut task = output(&linked(), "Dev1/ao0:1");
    drop(task.write(&[1.0, 2.0, 3.0], SECOND));
}

#[test]
#[should_panic(expected = "1 values do not fill 2 channels")]
fn panics_on_a_buffer_that_does_not_fill_the_channels() {
    let mut task = input(&linked(), "Dev1/ai0:1");
    drop(task.read(&mut [0.0], SECOND));
}

#[test]
#[should_panic(expected = "at most i32::MAX samples of each channel")]
fn panics_past_i32_max_samples_of_each_channel() {
    scans(1 << 31, 1);
}

#[test]
fn counts_the_samples_of_each_channel() {
    assert_eq!(scans(6, 3), 2);
    assert_eq!(scans(0, 0), 0);
    assert_eq!(scans(0, 2), 0);
}

#[test]
fn a_task_outlives_its_library() {
    let library = linked();
    let mut task = input(&library, "Dev1/ai0");
    drop(library);
    let mut out = [0.0];
    assert_eq!(task.read(&mut out, SECOND), Ok(1));
}

const _: () = {
    const fn send<T: Send>() {}
    send::<Input>();
    send::<Output>();
    send::<Library>();
};

#[test]
fn converts_a_timeout_to_whole_milliseconds() {
    assert_eq!(seconds(Span::from_nanos(1_500_999_999)), 1.5);
    assert_eq!(seconds(Span::from_nanos(-1)), 0.0);
    assert_eq!(
        seconds(Span::from_nanos(i64::MAX)),
        f64::from(u32::MAX) / 1000.0
    );
}

#[test]
#[should_panic(expected = "a name holds no NUL byte")]
fn panics_on_a_name_with_a_nul() {
    drop(Input::create(&linked(), "a\0b"));
}

/// The stub built as a shared library, which Cargo puts next to the test binary.
fn stub_path() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.with_file_name(format!(
        "{}daqmx_stub{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ))
}

#[test]
#[cfg_attr(miri, ignore = "Miri cannot load a library")]
fn reads_through_the_loaded_driver() {
    // SAFETY: the stub has NI's functions and signatures.
    let library = unsafe { Library::open(Some(&stub_path())) }.unwrap();
    reads_a_ramp(&library);
}

#[test]
#[cfg_attr(miri, ignore = "Miri cannot load a library")]
#[cfg_attr(not(target_os = "linux"), ignore = "the message is glibc's")]
fn names_a_library_that_does_not_load() {
    // SAFETY: no library loads.
    let error = unsafe { Library::open(Some(Path::new("/none/libnidaqmx.so"))) };
    assert_eq!(
        error.unwrap_err(),
        Error::Load(
            "/none/libnidaqmx.so: cannot open shared object file: No such file or \
             directory"
                .into()
        )
    );
}

#[test]
#[cfg_attr(miri, ignore = "Miri cannot load a library")]
#[cfg_attr(not(target_os = "linux"), ignore = "the library is glibc's")]
fn names_a_function_the_library_lacks() {
    // SAFETY: the C library loads with no harm, and `find` fails before it uses a
    // symbol.
    let error = unsafe { Library::open(Some(Path::new("libc.so.6"))) };
    assert_eq!(error.unwrap_err(), Error::Missing("DAQmxCreateTask"));
}
