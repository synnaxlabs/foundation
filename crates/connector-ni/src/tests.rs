#![expect(clippy::float_cmp, reason = "each value compared is exact")]

use std::path::PathBuf;

use connector_ni_stub as stub;

use super::*;

const SECOND: Span = Span::from_nanos(1_000_000_000);

/// The stub's functions, called without loading a library, so that Miri runs them.
fn functions() -> Functions {
    Functions {
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
    }
}

fn analog_in(physical: &CStr) -> Channel {
    Channel::AnalogIn {
        physical: physical.into(),
        min: -10.0,
        max: 10.0,
    }
}

fn refused(code: i32) -> Error {
    Error::Daqmx {
        code,
        message: "the stub refused the call".into(),
    }
}

fn reads_a_ramp(task: &mut Task<'_>) {
    task.add(&analog_in(c"Dev1/ai0:1")).unwrap();
    task.add(&analog_in(c"Dev1/ai2")).unwrap();
    task.clock(1000.0, 100).unwrap();
    task.start().unwrap();
    let mut out = Vec::new();
    task.read_analog(2, &mut out, SECOND).unwrap();
    assert_eq!(out, [0.0, 1000.0, 2000.0, 1.0, 1001.0, 2001.0]);
    task.read_analog(1, &mut out, SECOND).unwrap();
    assert_eq!(out, [2.0, 1002.0, 2002.0]);
}

#[test]
fn reads_each_channel_by_scan() {
    let functions = functions();
    reads_a_ramp(&mut Task::create(&functions, c"read").unwrap());
}

#[test]
fn refuses_a_read_on_a_stopped_task() {
    let functions = functions();
    let mut task = Task::create(&functions, c"stopped").unwrap();
    task.add(&analog_in(c"Dev1/ai0")).unwrap();
    task.start().unwrap();
    task.stop().unwrap();
    let mut out = vec![1.0];
    assert_eq!(
        task.read_analog(1, &mut out, SECOND),
        Err(refused(stub::STOPPED))
    );
    assert_eq!(out, []);
}

#[test]
fn gives_the_driver_error_for_a_channel() {
    let functions = functions();
    let mut task = Task::create(&functions, c"fail").unwrap();
    let error = task.add(&analog_in(c"fail/ai0")).unwrap_err();
    assert_eq!(error, refused(stub::FAIL));
    assert_eq!(
        error.to_string(),
        "NI-DAQmx error -201000: the stub refused the call"
    );
}

#[test]
fn writes_each_channel_within_its_range() {
    let functions = functions();
    let mut task = Task::create(&functions, c"write").unwrap();
    let out = Channel::AnalogOut {
        physical: c"Dev1/ao0:1".into(),
        min: 0.0,
        max: 5.0,
    };
    task.add(&out).unwrap();
    task.start().unwrap();
    task.write_analog(&[1.0, 2.0, 3.0, 4.0], SECOND).unwrap();
    assert_eq!(
        task.write_analog(&[1.0, 5.5], SECOND),
        Err(refused(stub::RANGE))
    );
}

#[test]
#[should_panic(expected = "3 values do not fill 2 channels")]
fn panics_on_values_that_do_not_fill_the_channels() {
    let functions = functions();
    let mut task = Task::create(&functions, c"panic").unwrap();
    let out = Channel::AnalogOut {
        physical: c"Dev1/ao0:1".into(),
        min: 0.0,
        max: 5.0,
    };
    task.add(&out).unwrap();
    task.start().unwrap();
    drop(task.write_analog(&[1.0, 2.0, 3.0], SECOND));
}

#[test]
fn converts_a_timeout_to_whole_milliseconds() {
    assert_eq!(seconds(Span::from_nanos(1_500_999_999)), 1.5);
    assert_eq!(seconds(Span::from_nanos(-1)), 0.0);
    assert_eq!(
        seconds(Span::from_nanos(i64::MAX)),
        f64::from(u32::MAX) / 1000.0
    );
}

/// The stub built as a shared library, which Cargo puts next to the test binary.
fn stub_path() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.with_file_name(format!(
        "{}connector_ni_stub{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ))
}

#[test]
#[cfg_attr(miri, ignore = "Miri cannot load a library")]
fn reads_through_the_loaded_driver() {
    // SAFETY: the stub has NI's functions and signatures.
    let library = unsafe { Library::open(Some(&stub_path())) }.unwrap();
    reads_a_ramp(&mut library.task(c"loaded").unwrap());
}

#[test]
#[cfg_attr(miri, ignore = "Miri cannot load a library")]
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
fn names_a_function_the_library_lacks() {
    // SAFETY: the C library loads with no harm, and `find` fails before it uses a
    // symbol.
    let error = unsafe { Library::open(Some(Path::new("libc.so.6"))) };
    assert_eq!(error.unwrap_err(), Error::Missing("DAQmxCreateTask"));
}
