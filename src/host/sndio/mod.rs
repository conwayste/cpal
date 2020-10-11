extern crate libc;
extern crate sndio_sys;

use std::convert::From;
use std::mem::{self, MaybeUninit};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::{
    BackendSpecificError, BufferSize, BuildStreamError, Data, DefaultStreamConfigError,
    DeviceNameError, DevicesError, HostUnavailable, InputCallbackInfo, OutputCallbackInfo,
    OutputStreamTimestamp, PauseStreamError, PlayStreamError, SampleFormat, SampleRate,
    StreamConfig, StreamError, StreamInstant, SupportedBufferSize, SupportedStreamConfig,
    SupportedStreamConfigRange, SupportedStreamConfigsError,
};

use traits::{DeviceTrait, HostTrait, StreamTrait};

mod endian;

pub type SupportedInputConfigs = ::std::vec::IntoIter<SupportedStreamConfigRange>;
pub type SupportedOutputConfigs = ::std::vec::IntoIter<SupportedStreamConfigRange>;

/// Default multiple of the round field of a sio_par struct to use for the buffer size.
const DEFAULT_ROUND_MULTIPLE: usize = 2;

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
struct InnerState {
    /// If device has been open with sio_open, contains a handle. Note that even though this is a
    /// pointer type and so doesn't follow Rust's borrowing rules, we should be careful not to copy
    /// it out because that may render Mutex<InnerState> ineffective in enforcing exclusive access.
    hdl: Option<*mut sndio_sys::sio_hdl>,

    /// If the device was open and configured, contains the configuration.
    config: Option<SupportedStreamConfig>,

    /// If a buffer size was chosen, contains that value.
    buffer_size: Option<usize>,

    /// Also store sndio configured parameters.
    par: Option<sndio_sys::sio_par>,

    /// Indicates if the read/write thread is started, shutting down, or stopped.
    status: Status,

    /// Each input Stream that has not been dropped has its callbacks in an element of this Vec.
    /// The last element is guaranteed to not be None.
    input_callbacks: Vec<Option<InputCallbacks>>,

    /// Each output Stream that has not been dropped has its callbacks in an element of this Vec.
    /// The last element is guaranteed to not be None.
    output_callbacks: Vec<Option<OutputCallbacks>>,

    /// Channel of capacity 1 used for signalling that the runner thread should wakeup because
    /// there is now a Stream. This will only be None if there is no runner thread.
    wakeup_sender: Option<mpsc::Sender<()>>,
}

struct InputCallbacks {
    data_callback: Box<dyn FnMut(&Data, &InputCallbackInfo) + Send + 'static>,
    error_callback: Box<dyn FnMut(StreamError) + Send + 'static>,
}

struct OutputCallbacks {
    data_callback: Box<dyn FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static>,
    error_callback: Box<dyn FnMut(StreamError) + Send + 'static>,
}

unsafe impl Send for InnerState {}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum Status {
    /// Initial state. No thread running. Device/Stream methods will start thread and change this
    /// to Running.
    Stopped,

    /// Thread is running (unless it encountered an error).
    Running,
}

impl InnerState {
    fn new() -> Self {
        InnerState {
            hdl: None,
            par: None,
            config: None,
            buffer_size: None,
            status: Status::Stopped,
            input_callbacks: vec![],
            output_callbacks: vec![],
            wakeup_sender: None,
        }
    }

    fn open(&mut self) -> Result<(), SndioError> {
        if self.hdl.is_some() {
            // Already open
            return Ok(());
        }

        let hdl = unsafe {
            let devany_ptr = mem::transmute::<_, *const i8>(sndio_sys::SIO_DEVANY as *const _);
            let nonblocking = true as i32;
            sndio_sys::sio_open(
                devany_ptr,
                sndio_sys::SIO_PLAY | sndio_sys::SIO_REC,
                nonblocking,
            )
        };
        if hdl.is_null() {
            return Err(SndioError::DeviceNotAvailable);
        }
        self.hdl = Some(hdl);
        Ok(())
    }

    fn start(&mut self) -> Result<(), SndioError> {
        let status = unsafe {
            // "The sio_start() function puts the device in a waiting state: the device
            // will wait for playback data to be provided (using the sio_write()
            // function).  Once enough data is queued to ensure that play buffers will
            // not underrun, actual playback is started automatically."
            sndio_sys::sio_start(self.hdl.unwrap()) // Unwrap OK because of open call above
        };
        if status != 1 {
            return Err(backend_specific_error("failed to start stream"));
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SndioError> {
        let status = unsafe {
            // The sio_stop() function puts the audio subsystem in the same state as before
            // sio_start() is called.  It stops recording, drains the play buffer and then stops
            // playback.  If samples to play are queued but playback hasn't started yet then
            // playback is forced immediately; playback will actually stop once the buffer is
            // drained.  In no case are samples in the play buffer discarded.
            sndio_sys::sio_stop(self.hdl.unwrap())
        };
        if status != 1 {
            return Err(backend_specific_error("error calling sio_stop")); // To get more detailed info, need to use errno
        }
        Ok(())
    }

    // TODO: make these 4 methods generic (new CallbackSet<T> where T is either InputCallbacks or OutputCallbacks)
    /// Puts the supplied callbacks into the vector in the first free position, or at the end. The
    /// index of insertion is returned.
    fn add_output_callbacks(&mut self, callbacks: OutputCallbacks) -> usize {
        for (i, cbs) in self.output_callbacks.iter_mut().enumerate() {
            if cbs.is_none() {
                *cbs = Some(callbacks);
                return i;
            }
        }
        // If there were previously no callbacks, wakeup the runner thread.
        if self.input_callbacks.len() == 0 && self.output_callbacks.len() == 0 {
            if let Some(sender) = self.wakeup_sender {
                let _ = sender.send(());
            }
        }
        self.output_callbacks.push(Some(callbacks));
        self.output_callbacks.len() - 1
    }

    /// Removes the callbacks at specified index, returning them. Panics if the index is invalid
    /// (out of range or there is a None element at that position).
    fn remove_output_callbacks(&mut self, index: usize) -> OutputCallbacks {
        let cbs = self.output_callbacks[index].take().unwrap();
        while self.output_callbacks.len() > 0
            && self.output_callbacks[self.output_callbacks.len() - 1].is_none()
        {
            self.output_callbacks.pop();
        }
        cbs
    }

    /// Puts the supplied callbacks into the vector in the first free position, or at the end. The
    /// index of insertion is returned.
    fn add_input_callbacks(&mut self, callbacks: InputCallbacks) -> usize {
        for (i, cbs) in self.input_callbacks.iter_mut().enumerate() {
            if cbs.is_none() {
                *cbs = Some(callbacks);
                return i;
            }
        }
        // If there were previously no callbacks, wakeup the runner thread.
        if self.input_callbacks.len() == 0 && self.output_callbacks.len() == 0 {
            if let Some(sender) = self.wakeup_sender {
                let _ = sender.send(());
            }
        }
        self.input_callbacks.push(Some(callbacks));
        self.input_callbacks.len() - 1
    }

    /// Removes the callbacks at specified index, returning them. Panics if the index is invalid
    /// (out of range or there is a None element at that position).
    fn remove_input_callbacks(&mut self, index: usize) -> InputCallbacks {
        let cbs = self.input_callbacks[index].take().unwrap();
        while self.input_callbacks.len() > 0
            && self.input_callbacks[self.input_callbacks.len() - 1].is_none()
        {
            self.input_callbacks.pop();
        }
        cbs
    }

    /// Send an error to all input and output error callbacks.
    fn error(&mut self, e: impl Into<StreamError>) {
        let e = e.into();
        for cbs in &self.input_callbacks {
            if let Some(cbs) = cbs {
                (cbs.error_callback)(e.clone());
            }
        }
        for cbs in &self.output_callbacks {
            if let Some(cbs) = cbs {
                (cbs.error_callback)(e.clone());
            }
        }
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

#[derive(Clone)]
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
        println!("DEBUG: start of default_output_config"); //XXX
        {
            let inner_state = self.inner_state.lock().unwrap();
            if inner_state.config.is_some() && inner_state.par.is_some() {
                let config = inner_state.config.as_ref().unwrap();
                let par = inner_state.par.as_ref().unwrap();
                return Ok(SupportedStreamConfig {
                    channels: config.channels,
                    sample_rate: config.sample_rate,
                    buffer_size: SupportedBufferSize::Range {
                        min: par.appbufsz,
                        max: par.appbufsz,
                    },
                    sample_format: SampleFormat::I16,
                });
            }
        }

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
            // Request the device using our parameters
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
                min: par.round,
                max: par.appbufsz, // There isn't really a max but in practice, this value can act as one because it has very high latency.
                                   // Also note that min and max hold frame counts not sample counts. This would
                                   // matter if stereo was supported.
            },
            sample_format,
        };
        // NOTE: these parameters are set on the device now!
        // Save the parameters for future use
        inner_state.config = Some(config.clone());
        inner_state.par = Some(par);
        println!("DEBUG: returning default_output_config"); //XXX

        Ok(config)
    }

    fn build_input_stream_raw<D, E>(
        &self,
        config: &StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
    ) -> Result<Self::Stream, BuildStreamError>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        //XXX
        Ok(())
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
        println!("DEBUG: start build_output_stream_raw"); //XXX
        let inner_state_arc = self.inner_state.clone();

        let mut inner_state = self.inner_state.lock().unwrap();
        if inner_state.config.is_none() {
            return Err(backend_specific_error("device not configured").into());
        }

        // Note that the configuration of the device actually happens during the default_*_config
        // steps.
        let supported_config = inner_state.config.as_ref().unwrap();
        if supported_config.channels != config.channels
            || supported_config.sample_rate != config.sample_rate
        {
            return Err(backend_specific_error("configs don't match").into());
        }
        // Round up the buffer size the user selected to the next multiple of par.round. If there
        // was already a stream created with a different buffer size, return an error (sorry).
        // Note: if we want stereo support, this will need to change.
        let round = inner_state.par.unwrap().round as usize;
        inner_state.buffer_size = match config.buffer_size {
            BufferSize::Fixed(requested) => {
                let rounded_frame_count = if requested > 0 {
                    requested as usize + round - ((requested - 1) as usize % round) - 1
                } else {
                    round
                };
                if inner_state.buffer_size.is_some()
                    && inner_state.buffer_size != Some(rounded_frame_count)
                {
                    return Err(backend_specific_error("buffer sizes don't match").into());
                }
                Some(rounded_frame_count)
            }
            BufferSize::Default => inner_state
                .buffer_size
                .or(Some(DEFAULT_ROUND_MULTIPLE * round)),
        };

        if sample_format != SampleFormat::I16 {
            return Err(backend_specific_error(format!(
                "unexpected sample format {:?}, expected I16",
                sample_format
            ))
            .into());
        }

        let idx = inner_state.add_output_callbacks(OutputCallbacks {
            data_callback: Box::new(data_callback),
            error_callback: Box::new(error_callback),
        });

        if inner_state.status != Status::Running {
            thread::spawn(move || runner(inner_state_arc));
            inner_state.status = Status::Running;
        }

        drop(inner_state); // Unlock
        Ok(Stream {
            inner_state: self.inner_state.clone(),
            is_output: true,
            index: idx,
        })
    }
}

/// The runner thread handles playing and/or recording
fn runner(inner_state_arc: Arc<Mutex<InnerState>>) {
    let buffer_size: usize;
    let start_time: Instant;
    let (wakeup_sender, wakeup_receiver) = mpsc::channel();
    {
        println!("DEBUG: runner thread START"); //XXX
        let mut inner_state = inner_state_arc.lock().unwrap();
        inner_state.wakeup_sender = Some(wakeup_sender);

        buffer_size = inner_state.buffer_size.unwrap(); // Unwrap OK because it's always picked before Stream is created
        if buffer_size == 0 {
            // Probably unreachable
            inner_state.error(backend_specific_error("could not determine buffer size"));
            return;
        }

        if let Err(err) = inner_state.open() {
            inner_state.error(err);
            return;
        }
        if let Err(err) = inner_state.start() {
            inner_state.error(err.into());
            return;
        }
        start_time = Instant::now();
    }

    let mut buf = [0i16].repeat(buffer_size); // Allocate buffer of correct size
    let mut data =
        unsafe { Data::from_parts(buf.as_mut_ptr() as *mut (), buf.len(), SampleFormat::I16) };
    let data_byte_size = data.len * data.sample_format.sample_size();

    let mut offset_bytes_into_buf: u64 = 0; // Byte offset in buf to sio_write
    let mut paused = false;
    loop {
        // See if shutdown requested in inner_state.status; if so, break
        let mut nfds;
        let mut pollfds: Vec<libc::pollfd>;
        {
            let inner_state = inner_state_arc.lock().unwrap();
            // If there's nothing to do, wait until that's no longer the case.
            if inner_state.input_callbacks.len() == 0 && inner_state.output_callbacks.len() == 0 {
                if !paused {
                    if let Err(err) = inner_state.stop() {
                        // No callbacks to error with
                        break;
                    }
                }
                paused = true;
                while let Ok(_) = wakeup_receiver.try_recv() {} // While the lock is still held, drain the channel.
                drop(inner_state); // Unlock to prevent deadlock
                wakeup_receiver.recv().unwrap(); // Block until a callback has been added
            }
        }

        // If there no Streams and no Device then there is nothing to do -- exit. Note: this is
        // only correct if there are no Weak references to this InnerState anywhere.
        if Arc::strong_count(&inner_state_arc) == 1 {
            break;
        }

        {
            let inner_state = inner_state_arc.lock().unwrap();
            if paused {
                if inner_state.input_callbacks.len() == 0 && inner_state.output_callbacks.len() == 0
                {
                    // Spurious wakeup
                    continue;
                }

                if let Err(err) = inner_state.start() {
                    inner_state.error(backend_specific_error(format!(
                        "failed to unpause after new Stream created: {:?}",
                        err
                    )));
                    break;
                }
                paused = false;
            }
            nfds = unsafe {
                sndio_sys::sio_nfds(inner_state.hdl.unwrap()) // Unwrap OK because of open call above
            };
            if nfds <= 0 {
                inner_state.error(backend_specific_error(format!(
                    "cannot allocate {} pollfd structs",
                    nfds
                )));
                break;
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
                    (libc::POLLOUT | libc::POLLIN) as i32,
                )
            };
            if nfds <= 0 || nfds > pollfds.len() as i32 {
                inner_state.error(backend_specific_error(format!(
                    "invalid pollfd count from sio_pollfd: {}",
                    nfds
                )));
                break;
            }
        }

        // Poll (block until ready to write)
        let status = unsafe { libc::poll(pollfds.as_mut_ptr(), nfds as u32, -1) };
        if status < 0 {
            let inner_state = inner_state_arc.lock().unwrap();
            inner_state.error(backend_specific_error(format!(
                "poll failed: returned {}",
                status
            )));
            break;
        }

        {
            let inner_state = inner_state_arc.lock().unwrap();
            let revents =
                unsafe { sndio_sys::sio_revents(inner_state.hdl.unwrap(), pollfds.as_mut_ptr()) }
                    as i16;
            if revents & libc::POLLHUP != 0 {
                inner_state.error(backend_specific_error("device disappeared"));
                break;
            }
            if revents & (libc::POLLOUT | libc::POLLIN) == 0 {
                continue;
            }
        }

        if libc::POLLOUT != 0 {
            // At this point we know data can be written
            let elapsed = Instant::now().duration_since(start_time);
            let mut output_callback_info = OutputCallbackInfo {
                timestamp: OutputStreamTimestamp {
                    callback: StreamInstant::new(
                        elapsed.as_secs() as i64,
                        elapsed.as_nanos() as u32,
                    ),
                    playback: StreamInstant::new(0, 0), // XXX .callback.add(latency); calc latency from par.bufsz and rate.
                },
            };
            output_callback_info.timestamp.playback = output_callback_info.timestamp.callback; //XXX remove this (see above)

            if offset_bytes_into_buf == 0 {
                //XXX loop over each output data callback
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
                    inner_state.error(backend_specific_error("no bytes written; EOF?"));
                    break;
                }

                offset_bytes_into_buf += bytes_written;
                if offset_bytes_into_buf as usize > data_byte_size {
                    inner_state.error(backend_specific_error("too many bytes written!"));
                    break;
                }

                if offset_bytes_into_buf as usize == data_byte_size {
                    // Everything written; need to call data callback again.
                    offset_bytes_into_buf = 0;
                };
            }
        } 

        if libc::POLLIN != 0 {
            // At this point, we know data can be read
            //XXX loop over input_callbacks
        }
    }

    {
        let inner_state = inner_state_arc.lock().unwrap();
        inner_state.wakeup_sender = None;
        //XXX should we be stopping here?
        let _ = inner_state.stop(); // Can't do anything with error since no error callbacks left
        inner_state.status = Status::Stopped;
    }
    println!("DEBUG: runner thread STOP"); //XXX
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

    /// True if this is output; false if this is input.
    is_output: bool,

    /// Index into input_callbacks or output_callbacks
    index: usize,
}

impl StreamTrait for Stream {
    fn play(&self) -> Result<(), PlayStreamError> {
        // No-op since the stream was already started by build_output_stream_raw
        Ok(())
    }

    // sndio doesn't support pausing.
    fn pause(&self) -> Result<(), PauseStreamError> {
        Err(backend_specific_error("pausing is not implemented").into())
    }
}

impl Drop for Stream {
    /// Requests a shutdown from the callback (runner) thread and waits for it to finish shutting down.
    /// If the thread is already stopped, nothing happens.
    fn drop(&mut self) {
        let mut inner_state = self.inner_state.lock().unwrap();
        if self.is_output {
            inner_state.remove_output_callbacks(self.index);
        } else {
            inner_state.remove_input_callbacks(self.index);
        }

        if inner_state.input_callbacks.len() == 0
            && inner_state.output_callbacks.len() == 0
            && inner_state.status == Status::Running
        {
            if let Some(sender) = inner_state.wakeup_sender {
                let _ = sender.send(());
            }
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let mut inner_state = self.inner_state.lock().unwrap();
        if inner_state.input_callbacks.len() == 0
            && inner_state.output_callbacks.len() == 0
            && inner_state.status == Status::Running
        {
            // Attempt to wakeup runner thread
            if let Some(sender) = inner_state.wakeup_sender {
                let _ = sender.send(());
            }
        }
    }
}
