//! The driver functions this crate calls, declared by hand from NI's `NIDAQmx.h`.

use std::ffi::{CStr, c_char, c_void};

use crate::Error;

/// A task handle of the driver.
pub(crate) type Handle = *mut c_void;

/// One pointer to each driver function this crate calls.
#[derive(Debug)]
pub(crate) struct Functions {
    pub(crate) create_task: unsafe extern "C" fn(*const c_char, *mut Handle) -> i32,
    pub(crate) start_task: unsafe extern "C" fn(Handle) -> i32,
    pub(crate) stop_task: unsafe extern "C" fn(Handle) -> i32,
    pub(crate) clear_task: unsafe extern "C" fn(Handle) -> i32,
    pub(crate) channels: unsafe extern "C" fn(Handle, *mut u32) -> i32,
    pub(crate) analog_in: unsafe extern "C" fn(
        Handle,
        *const c_char,
        *const c_char,
        i32,
        f64,
        f64,
        i32,
        *const c_char,
    ) -> i32,
    pub(crate) analog_out: unsafe extern "C" fn(
        Handle,
        *const c_char,
        *const c_char,
        f64,
        f64,
        i32,
        *const c_char,
    ) -> i32,
    pub(crate) clock:
        unsafe extern "C" fn(Handle, *const c_char, f64, i32, i32, u64) -> i32,
    pub(crate) read_analog: unsafe extern "C" fn(
        Handle,
        i32,
        f64,
        u32,
        *mut f64,
        u32,
        *mut i32,
        *mut u32,
    ) -> i32,
    pub(crate) write_analog: unsafe extern "C" fn(
        Handle,
        i32,
        u32,
        f64,
        u32,
        *const f64,
        *mut i32,
        *mut u32,
    ) -> i32,
    pub(crate) error: unsafe extern "C" fn(*mut c_char, u32) -> i32,
}

/// `DAQmx_Val_Cfg_Default`.
pub(crate) const DEFAULT: i32 = -1;
/// `DAQmx_Val_Volts`.
pub(crate) const VOLTS: i32 = 10_348;
/// `DAQmx_Val_Rising`.
pub(crate) const RISING: i32 = 10_280;
/// `DAQmx_Val_ContSamps`.
pub(crate) const CONTINUOUS: i32 = 10_123;
/// `DAQmx_Val_GroupByScanNumber`.
pub(crate) const BY_SCAN: u32 = 1;

impl Functions {
    /// Finds each function in `library`.
    ///
    /// # Safety
    ///
    /// `library` is NI's driver, or a library with its functions and signatures. The
    /// pointers are valid only while `library` stays loaded.
    pub(crate) unsafe fn find(library: &libloading::Library) -> Result<Self, Error> {
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
            error: find!("DAQmxGetExtendedErrorInfo"),
        })
    }

    /// Gives `Ok` for a code of zero or above (success or a warning), and the
    /// driver's error for a code below zero.
    pub(crate) fn check(&self, code: i32) -> Result<(), Error> {
        if code >= 0 {
            return Ok(());
        }
        let mut message = [0_u8; 2048];
        // SAFETY: `message` holds 2048 bytes.
        unsafe { (self.error)(message.as_mut_ptr().cast(), 2048) };
        let message = CStr::from_bytes_until_nul(&message)
            .map(|message| message.to_string_lossy().into_owned())
            .unwrap_or_default();
        Err(Error::Daqmx { code, message })
    }
}
