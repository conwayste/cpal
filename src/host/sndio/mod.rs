extern crate sndio_sys;

use crate::{
    BackendSpecificError, BufferSize, BuildStreamError, Data, DefaultStreamConfigError,
    DeviceNameError, DevicesError, InputCallbackInfo, HostUnavailable, OutputCallbackInfo,
    PauseStreamError, PlayStreamError, SampleFormat, SampleRate, StreamConfig, StreamError,
    SupportedBufferSize, SupportedStreamConfig, SupportedStreamConfigRange,
    SupportedStreamConfigsError,
};

use traits::{DeviceTrait, HostTrait, StreamTrait};

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
            Some(Device)
        }
    }
}

impl Devices {
    fn new() -> Devices {
        Devices { returned: false }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device;

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
        unimplemented!("DeviceTrait default_output_config")
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
        unimplemented!("DeviceTrait build_output_stream_raw")
    }
}

pub struct Host;

impl Host {
    pub fn new() -> Result<Host, HostUnavailable> {
        Ok(Host)
    }

    pub fn default_output_device() -> Option<Device> {
        Some(Device)
    }
}

impl HostTrait for Host {
    type Devices = Devices;
    type Device = Device;

    fn is_available() -> bool {
        // Assume this host is always available on webaudio.
        true
    }

    fn devices(&self) -> Result<Self::Devices, DevicesError> {
        Ok(Devices::new())
    }

    fn default_input_device(&self) -> Option<Self::Device> {
        unimplemented!("HostTrait default_input_device")
    }

    fn default_output_device(&self) -> Option<Self::Device> {
        unimplemented!("HostTrait default_output_device")
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
