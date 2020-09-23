extern crate sndio_sys;

use std::mem::{self, MaybeUninit};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::{
    BackendSpecificError, BufferSize, BuildStreamError, Data, DefaultStreamConfigError,
    DeviceNameError, DevicesError, HostUnavailable, InputCallbackInfo, OutputCallbackInfo,
    PauseStreamError, PlayStreamError, SampleFormat, SampleRate, StreamConfig, StreamError,
    SupportedBufferSize, SupportedStreamConfig, SupportedStreamConfigRange,
    SupportedStreamConfigsError,
};

use traits::{DeviceTrait, HostTrait, StreamTrait};

mod endian;

pub type SupportedInputConfigs = ::std::vec::IntoIter<SupportedStreamConfigRange>;
pub type SupportedOutputConfigs = ::std::vec::IntoIter<SupportedStreamConfigRange>;

pub struct Devices {
    returned: bool,
}

impl Iterator for Devices {
    type Item = Device;
    fn next(&mut self) -> Option<Device> {
        if self.returned {
            None
        } else {
            self.returned = true;
            Some(Device::new())
        }
    }
}

impl Devices {
    fn new() -> Devices {
        Devices { returned: false }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BufferXrunBehavior {
    Ignore, // SIO_IGNORE
    Sync,   // SIO_SYNC
    Error,  // SIO_ERROR
}

/// The shared state between Device and Stream. Responsible for closing handle when dropped.
#[derive(Debug)]
struct InnerState {
    /// If device has been open with sio_open, contains a handle.
    hdl: Option<*mut sndio_sys::sio_hdl>,

    /// If the device was open and configured, contains the configuration.
    config: Option<StreamConfig>,

    /// Indicates if the read/write thread is started, shutting down, or stopped.
    status: Status,
}

#[derive(Debug)]
enum Status {
    /// Initial state. No thread running.
    Stopped,

    /// Thread is running.
    Running(JoinHandle<()>),

    /// A shutdown was requested. The thread was started and may still be running.
    ShuttingDown(JoinHandle<()>),
}

impl InnerState {
    fn new() -> Self {
        InnerState {
            hdl: None,
            status: Status::Stopped,
        }
    }
    fn open(&mut self) -> Result<(), DefaultStreamConfigError> {
        if self.hdl.is_some() {
            // Already open
            return Ok(());
        }

        let hdl = unsafe {
            let devany_ptr = mem::transmute::<_, *const i8>(sndio_sys::SIO_DEVANY as *const _);
            let nonblocking = true as i32;
            sndio_sys::sio_open(devany_ptr, sndio_sys::SIO_PLAY, nonblocking) // TODO: | sndio_sys::SIO_REC
        };
        if hdl.is_null() {
            return Err(DefaultStreamConfigError::DeviceNotAvailable);
        }
        self.hdl = Some(hdl);
        Ok(())
    }
}

impl Drop for InnerState {
    fn drop(&mut self) {
        if let Some(hdl) = self.hdl.take() {
            unsafe {
                sndio_sys::sio_close(hdl);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Device {
    inner_state: Arc<Mutex<InnerState>>,
    behavior: BufferXrunBehavior,
}

impl Device {
    pub fn new() -> Self {
        Device {
            inner_state: Arc::new(Mutex::new(InnerState::new())),
            behavior: BufferXrunBehavior::Ignore, // probably a good default for some cases?
        }
    }

    pub fn set_xrun_behavior(&mut self, behavior: BufferXrunBehavior) {
        self.behavior = behavior;
    }
}

impl DeviceTrait for Device {
    type SupportedInputConfigs = SupportedInputConfigs;
    type SupportedOutputConfigs = SupportedOutputConfigs;
    type Stream = Stream;

    #[inline]
    fn name(&self) -> Result<String, DeviceNameError> {
        Ok("sndio default device".to_owned())
    }

    #[inline]
    fn supported_input_configs(
        &self,
    ) -> Result<Self::SupportedInputConfigs, SupportedStreamConfigsError> {
        unimplemented!("DeviceTrait supported_input_configs")
    }

    #[inline]
    fn supported_output_configs(
        &self,
    ) -> Result<Self::SupportedOutputConfigs, SupportedStreamConfigsError> {
        unimplemented!("DeviceTrait supported_output_configs")
    }

    #[inline]
    fn default_input_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        unimplemented!("DeviceTrait default_input_config")
    }

    #[inline]
    fn default_output_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        let mut par = get_init_par();

        // Use I16 at 48KHz; stereo playback; mono record
        par.bits = 16;
        par.sig = 1;
        par.le = match endian::get_endianness() {
            endian::Endian::BE => 0,
            endian::Endian::LE => 1,
        };
        par.rchan = 1; // mono record
        par.pchan = 2; // stereo playback
        par.rate = 48000;
        par.xrun = match self.behavior {
            BufferXrunBehavior::Ignore => 0,
            BufferXrunBehavior::Sync => 1,
            BufferXrunBehavior::Error => 2,
        };

        let mut inner_state = self.inner_state.lock().unwrap();
        inner_state.open()?;

        // What follows is the suggested parameter negotiation from the man pages.
        // Following unwraps OK because we opened the device.
        let status = unsafe {
            // Request the device use our parameters
            sndio_sys::sio_setpar(inner_state.hdl.unwrap(), &mut par as *mut _)
        };
        if status != 1 {
            return Err(backend_specific_error(
                "failed to request parameters with sio_setpar",
            ));
        }

        let status = unsafe {
            // Retrieve the actual parameters of the device.
            sndio_sys::sio_getpar(inner_state.hdl.unwrap(), &mut par as *mut _)
        };
        if status != 1 {
            return Err(backend_specific_error(
                "failed to get device-supported parameters with sio_getpar",
            ));
        }

        if par.bits != 16 || par.bps != 2 {
            // We have to check both because of the possibility of padding (usually an issue with
            // 24 bits not 16 though).
            return Err(backend_specific_error(format!("unexpected sample size: bits/sample: {}, bytes/sample: {})", par.bits, par.bps)));
        }

        let sample_format = if par.sig == 1 {
            SampleFormat::I16
        } else {
            SampleFormat::U16
        };

        // NOTE: these parameters are set on the device now!
        // Save the parameters for future use

        Ok(SupportedStreamConfig {
            channels: par.pchan as u16,
            sample_rate: SampleRate(par.rate),
            buffer_size: SupportedBufferSize::Range {
                min: par.appbufsz,
                max: par.appbufsz,
            },
            sample_format,
        })
    }

    fn build_input_stream_raw<D, E>(
        &self,
        _config: &StreamConfig,
        _sample_format: SampleFormat,
        _data_callback: D,
        _error_callback: E,
    ) -> Result<Self::Stream, BuildStreamError>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        unimplemented!("DeviceTrait build_input_stream_raw")
    }

    /// Create an output stream.
    fn build_output_stream_raw<D, E>(
        &self,
        config: &StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        _error_callback: E,
    ) -> Result<Self::Stream, BuildStreamError>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        // TODO: do something better with the StreamConfig
        //XXX
        unimplemented!("DeviceTrait build_output_stream_raw")
    }
}

fn get_init_par() -> sndio_sys::sio_par {
    let mut par = MaybeUninit::<sndio_sys::sio_par>::uninit();
    unsafe {
        sndio_sys::sio_initpar(par.as_mut_ptr());
        par.assume_init()
    }
}

fn backend_specific_error(desc: impl Into<String>) -> DefaultStreamConfigError {
    DefaultStreamConfigError::BackendSpecific {
        err: BackendSpecificError {
            description: desc.into(),
        },
    }
}

pub struct Host;

impl Host {
    pub fn new() -> Result<Host, HostUnavailable> {
        Ok(Host)
    }

    pub fn default_output_device() -> Option<Device> {
        Some(Device::new())
    }
}

impl HostTrait for Host {
    type Devices = Devices;
    type Device = Device;

    fn is_available() -> bool {
        // Assume this host is always available on sndio.
        true
    }

    fn devices(&self) -> Result<Self::Devices, DevicesError> {
        Ok(Devices::new())
    }

    fn default_input_device(&self) -> Option<Self::Device> {
        Some(Device::new())
    }

    fn default_output_device(&self) -> Option<Self::Device> {
        Some(Device::new())
    }
}

pub struct Stream;

impl StreamTrait for Stream {
    fn play(&self) -> Result<(), PlayStreamError> {
        unimplemented!("StreamTrait play")
    }

    fn pause(&self) -> Result<(), PauseStreamError> {
        unimplemented!("StreamTrait pause")
    }
}
