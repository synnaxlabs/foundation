//! The driver functions this crate calls, declared by hand from NI's `NIDAQmx.h`.

use std::ffi::{c_char, c_void};

use super::Error;

/// A task handle of the driver.
pub(super) type Handle = *mut c_void;

/// One pointer to each driver function this crate calls.
#[derive(Debug)]
pub(super) struct Functions {
    pub(super) create_task:
        unsafe extern "system" fn(*const c_char, *mut Handle) -> i32,
    pub(super) start_task: unsafe extern "system" fn(Handle) -> i32,
    pub(super) stop_task: unsafe extern "system" fn(Handle) -> i32,
    pub(super) clear_task: unsafe extern "system" fn(Handle) -> i32,
    pub(super) channels: unsafe extern "system" fn(Handle, *mut u32) -> i32,
    pub(super) analog_in: unsafe extern "system" fn(
        Handle,
        *const c_char,
        *const c_char,
        i32,
        f64,
        f64,
        i32,
        *const c_char,
    ) -> i32,
    pub(super) analog_out: unsafe extern "system" fn(
        Handle,
        *const c_char,
        *const c_char,
        f64,
        f64,
        i32,
        *const c_char,
    ) -> i32,
    pub(super) clock:
        unsafe extern "system" fn(Handle, *const c_char, f64, i32, i32, u64) -> i32,
    pub(super) read_analog: unsafe extern "system" fn(
        Handle,
        i32,
        f64,
        u32,
        *mut f64,
        u32,
        *mut i32,
        *mut u32,
    ) -> i32,
    pub(super) write_analog: unsafe extern "system" fn(
        Handle,
        i32,
        u32,
        f64,
        u32,
        *const f64,
        *mut i32,
        *mut u32,
    ) -> i32,
    pub(super) digital_in:
        unsafe extern "system" fn(Handle, *const c_char, *const c_char, i32) -> i32,
    pub(super) digital_out:
        unsafe extern "system" fn(Handle, *const c_char, *const c_char, i32) -> i32,
    pub(super) read_digital: unsafe extern "system" fn(
        Handle,
        i32,
        f64,
        u32,
        *mut u8,
        u32,
        *mut i32,
        *mut i32,
        *mut u32,
    ) -> i32,
    pub(super) write_digital: unsafe extern "system" fn(
        Handle,
        i32,
        u32,
        f64,
        u32,
        *const u8,
        *mut i32,
        *mut u32,
    ) -> i32,
    pub(super) error: unsafe extern "system" fn(*mut c_char, u32) -> i32,
}

/// `DAQmx_Val_Cfg_Default`.
pub(super) const DEFAULT: i32 = -1;
/// `DAQmx_Val_Volts`.
pub(super) const VOLTS: i32 = 10_348;
/// `DAQmx_Val_Rising`.
pub(super) const RISING: i32 = 10_280;
/// `DAQmx_Val_ContSamps`.
pub(super) const CONTINUOUS: i32 = 10_123;
/// `DAQmx_Val_GroupByScanNumber`.
pub(super) const BY_SCAN: u32 = 1;
/// `DAQmx_Val_ChanPerLine`.
pub(super) const PER_LINE: i32 = 0;

impl Functions {
    /// Finds each function in `library`.
    ///
    /// # Safety
    ///
    /// `library` is NI's driver, or a library with its functions and signatures. The
    /// pointers are valid only while `library` stays loaded.
    pub(super) unsafe fn find(library: &libloading::Library) -> Result<Self, Error> {
        macro_rules! find {
            ($name:literal) => {
                // SAFETY: the caller's contract gives the symbol the field's type.
                *unsafe { library.get($name) }.map_err(|_| Error::Missing($name))?
            };
        }
        Ok(Self {
            create_task: find!("DAQmxCreateTask"),
            start_task: find!("DAQmxStartTask"),
            stop_task: find!("DAQmxStopTask"),
            clear_task: find!("DAQmxClearTask"),
            channels: find!("DAQmxGetTaskNumChans"),
            analog_in: find!("DAQmxCreateAIVoltageChan"),
            analog_out: find!("DAQmxCreateAOVoltageChan"),
            clock: find!("DAQmxCfgSampClkTiming"),
            read_analog: find!("DAQmxReadAnalogF64"),
            write_analog: find!("DAQmxWriteAnalogF64"),
            digital_in: find!("DAQmxCreateDIChan"),
            digital_out: find!("DAQmxCreateDOChan"),
            read_digital: find!("DAQmxReadDigitalLines"),
            write_digital: find!("DAQmxWriteDigitalLines"),
            error: find!("DAQmxGetExtendedErrorInfo"),
        })
    }
}
