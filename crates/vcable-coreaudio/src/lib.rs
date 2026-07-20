//! macOS Core Audio integration.
//!
//! The non-macOS implementation deliberately returns `UnsupportedPlatform`;
//! it is an explicit error, not a silent backend substitution.

use std::error::Error;
use std::fmt;

mod pcm;

pub use pcm::{
    PcmCaptureStream, PcmFormat, PcmPlaybackStream, PcmReader, PcmStreamConfig, PcmStreamDirection,
    PcmStreamError, PcmStreamMetrics, PcmWriter,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioDevice {
    pub object_id: u32,
    pub uid: String,
    pub name: String,
    pub input_channels: u32,
    pub output_channels: u32,
    pub sample_rate: u32,
    pub is_vcable: bool,
    pub is_virtual: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioRoute {
    pub source_device_id: u32,
    pub sink_device_id: u32,
    pub source_channels: u32,
    pub sink_channels: u32,
    pub source_sample_rate: u32,
    pub sink_sample_rate: u32,
    pub matrix: Vec<f32>,
    pub gain: f32,
}

#[derive(Debug)]
pub enum CoreAudioError {
    UnsupportedPlatform,
    OsStatus(i32),
    InvalidProperty(&'static str),
    InvalidUtf8,
    DriverNotInstalled,
    DriverRejected(String),
    DeviceChangeTimeout { uid: String, expected_present: bool },
    InvalidRoute(String),
}

impl fmt::Display for CoreAudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => write!(f, "Core Audio is available only on macOS"),
            Self::OsStatus(status) => write!(f, "Core Audio returned OSStatus {status}"),
            Self::InvalidProperty(name) => write!(f, "invalid Core Audio property: {name}"),
            Self::InvalidUtf8 => write!(f, "Core Audio returned a non-UTF-8 string"),
            Self::DriverNotInstalled => write!(f, "VCable HAL driver is not installed"),
            Self::DriverRejected(message) => {
                write!(f, "VCable driver rejected the request: {message}")
            }
            Self::DeviceChangeTimeout {
                uid,
                expected_present,
            } => write!(
                f,
                "timed out waiting for Core Audio device {uid} to become {}",
                if *expected_present {
                    "visible"
                } else {
                    "absent"
                }
            ),
            Self::InvalidRoute(message) => write!(f, "invalid audio route: {message}"),
        }
    }
}

impl Error for CoreAudioError {}

#[cfg(target_os = "macos")]
mod platform;

#[cfg(target_os = "macos")]
pub use platform::{
    AudioRouter, RouterMetrics, create_virtual_device, delete_virtual_device, list_devices,
};

#[cfg(not(target_os = "macos"))]
pub fn list_devices() -> Result<Vec<AudioDevice>, CoreAudioError> {
    Err(CoreAudioError::UnsupportedPlatform)
}

#[cfg(not(target_os = "macos"))]
pub fn create_virtual_device(
    _id: &str,
    _name: &str,
    _input_channels: u32,
    _output_channels: u32,
    _sample_rate: u32,
) -> Result<(), CoreAudioError> {
    Err(CoreAudioError::UnsupportedPlatform)
}

#[cfg(not(target_os = "macos"))]
pub fn delete_virtual_device(_id: &str) -> Result<(), CoreAudioError> {
    Err(CoreAudioError::UnsupportedPlatform)
}

#[cfg(not(target_os = "macos"))]
pub struct AudioRouter;

#[cfg(not(target_os = "macos"))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RouterMetrics {
    pub underruns: u64,
    pub overruns: u64,
    pub format_errors: u64,
}

#[cfg(not(target_os = "macos"))]
impl AudioRouter {
    pub fn start(_routes: &[AudioRoute]) -> Result<Self, CoreAudioError> {
        Err(CoreAudioError::UnsupportedPlatform)
    }

    pub fn metrics(&self) -> RouterMetrics {
        RouterMetrics::default()
    }
}
