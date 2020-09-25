extern crate libc;
extern crate sndio_sys;

use std::convert::From;
use std::mem::{self, MaybeUninit};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::{
    BackendSpecificError, BuildStreamError, Data, DefaultStreamConfigError, DeviceNameError,
    DevicesError, HostUnavailable, InputCallbackInfo, OutputCallbackInfo, OutputStreamTimestamp,
    PauseStreamError, PlayStreamError, SampleFormat, SampleRate, StreamConfig, StreamError,
    StreamInstant, SupportedBufferSize, SupportedStreamConfig, SupportedStreamConfigRange,
    SupportedStreamConfigsError,
};

use traits::{DeviceTrait, HostTrait, StreamTrait};

mod endian;

pub type SupportedInputConfigs = ::std::vec::IntoIter<SupportedStreamConfigRange>;
pub type SupportedOutputConfigs = ::std::vec::IntoIter<SupportedStreamConfigRange>;

#[derive(Clone, Debug, Error)]
pub enum SndioError {
    #[error("The requested device is no longer available. For example, it has been unplugged.")]
    DeviceNotAvailable,

    #[error("{0}")]
    BackendSpecific(BackendSpecificError),
}

impl From<SndioError> for BuildStreamError {
    fn from(e: SndioError) -> BuildStreamError {
        match e {
            SndioError::DeviceNotAvailable => BuildStreamError::DeviceNotAvailable,
            SndioError::BackendSpecific(bse) => BuildStreamError::BackendSpecific { err: bse },
        }
    }
}

impl From<SndioError> for DefaultStreamConfigError {
    fn from(e: SndioError) -> DefaultStreamConfigError {
        match e {
            SndioError::DeviceNotAvailable => DefaultStreamConfigError::DeviceNotAvailable,
            SndioError::BackendSpecific(bse) => {
                DefaultStreamConfigError::BackendSpecific { err: bse }
            }
        }
    }
}

impl From<SndioError> for PauseStreamError {
    fn from(e: SndioError) -> PauseStreamError {
        match e {
            SndioError::DeviceNotAvailable => PauseStreamError::DeviceNotAvailable,
            SndioError::BackendSpecific(bse) => PauseStreamError::BackendSpecific { err: bse },
        }
    }
}

impl From<SndioError> for StreamError {
    fn from(e: SndioError) -> StreamError {
        match e {
            SndioError::DeviceNotAvailable => StreamError::DeviceNotAvailable,
            SndioError::BackendSpecific(bse) => StreamError::BackendSpecific { err: bse },
        }
    }
}

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

/// The shared state between Device and Stream. Responsible for closing handle when dropped.
#[derive(Debug)]
struct InnerState {
    /// If device has been open with sio_open, contains a handle.
    hdl: Option<*mut sndio_sys::sio_hdl>,

    /// If the device was open and configured, contains the configuration.
    config: Option<StreamConfig>,

    /// Also store sndio configured parameters.
    par: Option<sndio_sys::sio_par>,

    /// Indicates if the read/write thread is started, shutting down, or stopped.
    status: Status,
}

unsafe impl Send for InnerState {}

type ThreadResult = Result<(), SndioError>;

#[derive(Debug)]
enum Status {
    /// Initial state. No thread running. Device/Stream methods will start thread and change this
    /// to Running.
    Stopped,

    /// Thread is running (unless it encountered an error). Dropping a Stream will change this to
    /// ShuttingDown and also join the thread with this JoinHandle.
    Running(JoinHandle<ThreadResult>),

    /// A shutdown was requested. The thread was started and may still be running.
    ShuttingDown,
}

impl InnerState {
    fn new() -> Self {
        InnerState {
            hdl: None,
            par: None,
            config: None,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BufferXrunBehavior {
    Ignore, // SIO_IGNORE
    Sync,   // SIO_SYNC
    Error,  // SIO_ERROR
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

        // Use I16 at 48KHz; mono playback & record
        par.bits = 16;
        par.sig = 1;
        par.le = match endian::get_endianness() {
            endian::Endian::BE => 0,
            endian::Endian::LE => 1,
        };
        par.rchan = 1; // mono record
        par.pchan = 1; // mono playback
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
            return Err(
                backend_specific_error("failed to request parameters with sio_setpar").into(),
            );
        }

        let status = unsafe {
            // Retrieve the actual parameters of the device.
            sndio_sys::sio_getpar(inner_state.hdl.unwrap(), &mut par as *mut _)
        };
        if status != 1 {
            return Err(backend_specific_error(
                "failed to get device-supported parameters with sio_getpar",
            )
            .into());
        }

        if par.bits != 16 || par.bps != 2 {
            // We have to check both because of the possibility of padding (usually an issue with
            // 24 bits not 16 though).
            return Err(backend_specific_error(format!(
                "unexpected sample size (not 16bit): bits/sample: {}, bytes/sample: {})",
                par.bits, par.bps
            ))
            .into());
        }

        /*
        let sample_format = if par.sig == 1 {
            SampleFormat::I16
        } else {
            SampleFormat::U16
        };
        */
        // TODO: this is inflexible; see code above
        if par.sig != 1 {
            return Err(backend_specific_error(
                "sndio device does not support I16 but we need it to",
            )
            .into());
        }
        let sample_format = SampleFormat::I16;

        let config = SupportedStreamConfig {
            channels: par.pchan as u16,
            sample_rate: SampleRate(par.rate),
            buffer_size: SupportedBufferSize::Range {
                min: par.appbufsz,
                max: par.appbufsz,
            },
            sample_format,
        };
        // NOTE: these parameters are set on the device now!
        // Save the parameters for future use
        inner_state.config = Some(config.clone().into());
        inner_state.par = Some(par);

        Ok(config)
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
        Err(backend_specific_error("recording not implemented").into())
    }

    /// Create an output stream.
    fn build_output_stream_raw<D, E>(
        &self,
        config: &StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
    ) -> Result<Self::Stream, BuildStreamError>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        let inner_state_arc = self.inner_state.clone();

        let mut inner_state = self.inner_state.lock().unwrap();
        match inner_state.status {
            Status::Running(_) | Status::ShuttingDown => {
                return Err(backend_specific_error("stream already started").into());
            }
            _ => {}
        }

        if inner_state.config.is_none() {
            return Err(backend_specific_error("device not configured").into());
        }

        // TODO: try to reconfigure the device rather than returning this lame error if they don't
        // match.
        if inner_state.config.as_ref().unwrap() != config {
            return Err(backend_specific_error("configs don't match").into());
        }

        if sample_format != SampleFormat::I16 {
            return Err(backend_specific_error(format!(
                "unexpected sample format {:?}, expected I16",
                sample_format
            ))
            .into());
        }

        let join_handle =
            thread::spawn(move || writer(inner_state_arc, data_callback, error_callback));

        inner_state.status = Status::Running(join_handle);

        drop(inner_state); // Unlock
        Ok(Stream {
            inner_state: self.inner_state.clone(),
        })
    }
}

/// The writer thread
fn writer(
    inner_state_arc: Arc<Mutex<InnerState>>,
    mut data_callback: impl FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
    mut error_callback: impl FnMut(StreamError) + Send + 'static,
) -> ThreadResult {
    let buffer_size: usize;
    let start_time: Instant;
    {
        let mut inner_state = inner_state_arc.lock().unwrap();
        buffer_size = inner_state.par.unwrap().appbufsz as usize;
        if buffer_size == 0 {
            // Probably unreachable
            let err = backend_specific_error("could not determine buffer size");
            error_callback(err.clone().into());
            return Err(err);
        }

        inner_state.open().unwrap(); // TODO: panic!
        let status = unsafe {
            // "The sio_start() function puts the device in a waiting state: the device
            // will wait for playback data to be provided (using the sio_write()
            // function).  Once enough data is queued to ensure that play buffers will
            // not underrun, actual playback is started automatically."
            sndio_sys::sio_start(inner_state.hdl.unwrap()) // Unwrap OK because of open call above
        };
        start_time = Instant::now();

        if status != 1 {
            let err = backend_specific_error("failed to start stream");
            error_callback(err.clone().into());
            return Err(err);
        }
    }

    let mut buf = [0i16].repeat(buffer_size); // Allocate buffer of correct size
    let mut data =
        unsafe { Data::from_parts(buf.as_mut_ptr() as *mut (), buf.len(), SampleFormat::I16) };
    let data_byte_size = data.len * data.sample_format.sample_size();

    let mut offset_bytes_into_buf: u64 = 0; // Byte offset in buf to sio_write
    loop {
        // See if shutdown requested in inner_state.status; if so, break
        let mut nfds;
        let mut pollfds: Vec<libc::pollfd>;
        {
            let inner_state = inner_state_arc.lock().unwrap();
            match inner_state.status {
                Status::ShuttingDown => break,
                _ => {}
            };

            nfds = unsafe {
                sndio_sys::sio_nfds(inner_state.hdl.unwrap()) // Unwrap OK because of open call above
            };
            if nfds <= 0 {
                let err =
                    backend_specific_error(format!("cannot allocate {} pollfd structs", nfds));
                drop(inner_state);
                error_callback(err.clone().into());
                return Err(err);
            }
            pollfds = [libc::pollfd {
                fd: 0,
                events: 0,
                revents: 0,
            }]
            .repeat(nfds as usize);

            // Populate pollfd structs with sndio_sys::sio_pollfd
            nfds = unsafe {
                sndio_sys::sio_pollfd(
                    inner_state.hdl.unwrap(),
                    pollfds.as_mut_ptr(),
                    libc::POLLOUT as i32,
                ) // TODO: POLLIN
            };
            if nfds <= 0 || nfds > pollfds.len() as i32 {
                let err = backend_specific_error(format!(
                    "invalid pollfd count from sio_pollfd: {}",
                    nfds
                ));
                drop(inner_state);
                error_callback(err.clone().into());
                return Err(err);
            }
        }

        // Poll (block until ready to write)
        let status = unsafe { libc::poll(pollfds.as_mut_ptr(), nfds as u32, -1) };
        if status < 0 {
            let err = backend_specific_error(format!("poll failed: returned {}", status));
            error_callback(err.clone().into());
            return Err(err);
        }

        {
            let inner_state = inner_state_arc.lock().unwrap();
            match inner_state.status {
                Status::ShuttingDown => break,
                _ => {}
            };
            let revents =
                unsafe { sndio_sys::sio_revents(inner_state.hdl.unwrap(), pollfds.as_mut_ptr()) }
                    as i16;
            if revents & libc::POLLHUP != 0 {
                let err = backend_specific_error("device disappeared");
                drop(inner_state);
                error_callback(err.clone().into());
                return Err(err);
            }
            if revents & libc::POLLOUT != 0 {
                // TODO: POLLIN
                continue;
            }
        }

        // At this point we know data can be written
        let elapsed = Instant::now().duration_since(start_time);
        let mut output_callback_info = OutputCallbackInfo {
            timestamp: OutputStreamTimestamp {
                callback: StreamInstant::new(elapsed.as_secs() as i64, elapsed.as_nanos() as u32),
                playback: StreamInstant::new(0, 0), // XXX .callback.add(latency); calc latency from par.bufsz and rate.
            },
        };
        output_callback_info.timestamp.playback = output_callback_info.timestamp.callback; //XXX remove this (see above)

        if offset_bytes_into_buf == 0 {
            data_callback(&mut data, &output_callback_info);
        }

        {
            let inner_state = inner_state_arc.lock().unwrap();
            let bytes_written = unsafe {
                sndio_sys::sio_write(
                    inner_state.hdl.unwrap(),
                    (data.data as *const u8).add(offset_bytes_into_buf as usize) as *const _,
                    data_byte_size as u64 - offset_bytes_into_buf,
                )
            };

            if bytes_written <= 0 {
                let err = backend_specific_error("no bytes written; EOF?");
                drop(inner_state);
                error_callback(err.clone().into());
                return Err(err);
            }

            offset_bytes_into_buf += bytes_written;
            if offset_bytes_into_buf as usize > data_byte_size {
                let err = backend_specific_error("too many bytes written!");
                drop(inner_state);
                error_callback(err.clone().into());
                return Err(err);
            }

            if offset_bytes_into_buf as usize == data_byte_size {
                // Everything written; need to call data callback again.
                offset_bytes_into_buf = 0;
            };
        }
    }

    println!("Thread exiting!"); //XXX DEBUG

    Ok(())
}

fn get_init_par() -> sndio_sys::sio_par {
    let mut par = MaybeUninit::<sndio_sys::sio_par>::uninit();
    unsafe {
        sndio_sys::sio_initpar(par.as_mut_ptr());
        par.assume_init()
    }
}

fn backend_specific_error(desc: impl Into<String>) -> SndioError {
    SndioError::BackendSpecific(BackendSpecificError {
        description: desc.into(),
    })
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

pub struct Stream {
    inner_state: Arc<Mutex<InnerState>>,
}

impl StreamTrait for Stream {
    fn play(&self) -> Result<(), PlayStreamError> {
        Ok(()) //XXX start the stream here instead?
    }

    // sndio doesn't support pausing.
    fn pause(&self) -> Result<(), PauseStreamError> {
        Err(backend_specific_error("pausing is not implemented").into())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let mut inner_state = self.inner_state.lock().unwrap();
        let mut status = Status::ShuttingDown;
        match inner_state.status {
            Status::Stopped => {
                return;
            }
            Status::Running(_) => {
                mem::swap(&mut status, &mut inner_state.status);
            }
            Status::ShuttingDown => {
                // We should join but there's no way to.
                inner_state.status = Status::Stopped;
                return;
            }
        }
        if let Status::Running(join_handle) = status {
            drop(inner_state); // Unlock
            let thread_result = join_handle.join();
            match thread_result {
                Ok(_) => {}
                Err(err) => {
                    // TODO: maybe we can remove this because of error callback?
                    println!("Error from callback thread: {:?}", err);
                }
            }
            let mut inner_state = self.inner_state.lock().unwrap();
            inner_state.status = Status::Stopped;
        }
    }
}
