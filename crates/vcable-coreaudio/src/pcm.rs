use crate::CoreAudioError;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use vcable_core::{Consumer, Producer, spsc_ring_buffer};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// The process-facing nominal PCM format. Samples are always interleaved `f32`.
pub struct PcmFormat {
    pub sample_rate: u32,
    pub channels: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Configuration shared by playback and capture streams.
pub struct PcmStreamConfig {
    pub device_uid: String,
    pub client_format: PcmFormat,
    pub capacity_frames: usize,
    /// Desired total buffered frames, including the resampler's `current` and `next` frames.
    ///
    /// Playback and capture wait for `max(target_fill_frames, 2)` total buffered frames before
    /// initially transferring PCM and after recovering from an underrun.
    pub target_fill_frames: usize,
    pub max_drift_ppm: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// The Core Audio channel direction required from the selected device.
pub enum PcmStreamDirection {
    Input,
    Output,
}

impl fmt::Display for PcmStreamDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input => write!(f, "input"),
            Self::Output => write!(f, "output"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Errors produced by process PCM stream creation and non-blocking transfer.
pub enum PcmStreamError {
    UnsupportedPlatform,
    DeviceNotFound {
        uid: String,
    },
    AmbiguousDevice {
        uid: String,
        matches: usize,
    },
    WrongDirection {
        uid: String,
        required: PcmStreamDirection,
    },
    FormatMismatch {
        expected: PcmFormat,
        actual: PcmFormat,
    },
    UnsupportedDevicePcmFormat {
        uid: String,
    },
    InvalidChannelCount,
    InvalidSampleRate,
    InvalidBufferLength,
    InvalidRingCapacity,
    InvalidTargetFill,
    InvalidDriftPpm,
    BufferFull {
        requested_frames: usize,
        available_frames: usize,
    },
    StreamClosed,
    CoreAudioOsStatus(i32),
    CoreAudioProperty(&'static str),
    CoreAudio(String),
}

impl fmt::Display for PcmStreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => write!(f, "PCM Core Audio streams require macOS"),
            Self::DeviceNotFound { uid } => write!(f, "Core Audio device UID not found: {uid}"),
            Self::AmbiguousDevice { uid, matches } => {
                write!(f, "Core Audio device UID {uid} matched {matches} devices")
            }
            Self::WrongDirection { uid, required } => {
                write!(f, "Core Audio device {uid} has no {required} channels")
            }
            Self::FormatMismatch { expected, actual } => write!(
                f,
                "PCM format mismatch: expected {} Hz/{} channels, got {} Hz/{} channels",
                expected.sample_rate, expected.channels, actual.sample_rate, actual.channels
            ),
            Self::UnsupportedDevicePcmFormat { uid } => {
                write!(f, "Core Audio device {uid} does not expose packed f32 PCM")
            }
            Self::InvalidChannelCount => write!(f, "channel count must be greater than zero"),
            Self::InvalidSampleRate => write!(f, "sample rate must be greater than zero"),
            Self::InvalidBufferLength => {
                write!(
                    f,
                    "interleaved buffer length is not a whole number of frames"
                )
            }
            Self::InvalidRingCapacity => write!(f, "invalid PCM ring-buffer capacity"),
            Self::InvalidTargetFill => {
                write!(f, "target fill must be inside the PCM ring buffer")
            }
            Self::InvalidDriftPpm => write!(f, "max drift must be less than 1,000,000 ppm"),
            Self::BufferFull {
                requested_frames,
                available_frames,
            } => write!(
                f,
                "PCM ring buffer is full: requested {requested_frames} frames, {available_frames} available"
            ),
            Self::StreamClosed => write!(f, "PCM stream is closed"),
            Self::CoreAudioOsStatus(status) => {
                write!(f, "Core Audio returned OSStatus {status}")
            }
            Self::CoreAudioProperty(property) => {
                write!(f, "invalid Core Audio property: {property}")
            }
            Self::CoreAudio(message) => write!(f, "Core Audio error: {message}"),
        }
    }
}

impl Error for PcmStreamError {}

impl From<CoreAudioError> for PcmStreamError {
    fn from(value: CoreAudioError) -> Self {
        match value {
            CoreAudioError::UnsupportedPlatform => Self::UnsupportedPlatform,
            CoreAudioError::OsStatus(status) => Self::CoreAudioOsStatus(status),
            CoreAudioError::InvalidProperty(property) => Self::CoreAudioProperty(property),
            other => Self::CoreAudio(other.to_string()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
/// An atomic point-in-time snapshot of a PCM stream.
///
/// `transferred_frames` counts non-silent playback frames delivered to Core Audio or capture
/// frames accepted from Core Audio. `ring_fill_frames` counts only frames still in the ring and
/// excludes the resampler's `current` and `next` frames.
pub struct PcmStreamMetrics {
    pub callbacks: u64,
    pub transferred_frames: u64,
    pub underrun_frames: u64,
    pub overrun_frames: u64,
    pub format_errors: u64,
    pub ring_fill_frames: usize,
    pub ring_capacity_frames: usize,
}

pub(crate) struct AtomicPcmStreamMetrics {
    callbacks: AtomicU64,
    transferred_frames: AtomicU64,
    underrun_frames: AtomicU64,
    overrun_frames: AtomicU64,
    format_errors: AtomicU64,
    ring_fill_frames: AtomicUsize,
    ring_capacity_frames: usize,
    closed: AtomicBool,
}

impl AtomicPcmStreamMetrics {
    fn new(ring_capacity_frames: usize) -> Self {
        Self {
            callbacks: AtomicU64::new(0),
            transferred_frames: AtomicU64::new(0),
            underrun_frames: AtomicU64::new(0),
            overrun_frames: AtomicU64::new(0),
            format_errors: AtomicU64::new(0),
            ring_fill_frames: AtomicUsize::new(0),
            ring_capacity_frames,
            closed: AtomicBool::new(false),
        }
    }

    fn snapshot(&self) -> PcmStreamMetrics {
        PcmStreamMetrics {
            callbacks: self.callbacks.load(Ordering::Relaxed),
            transferred_frames: self.transferred_frames.load(Ordering::Relaxed),
            underrun_frames: self.underrun_frames.load(Ordering::Relaxed),
            overrun_frames: self.overrun_frames.load(Ordering::Relaxed),
            format_errors: self.format_errors.load(Ordering::Relaxed),
            ring_fill_frames: self.ring_fill_frames.load(Ordering::Relaxed),
            ring_capacity_frames: self.ring_capacity_frames,
        }
    }

    pub(crate) fn callback(&self) {
        self.callbacks.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn transferred(&self, frames: usize) {
        self.transferred_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    }

    pub(crate) fn underrun(&self, frames: usize) {
        self.underrun_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    }

    pub(crate) fn overrun(&self, frames: usize) {
        self.overrun_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    }

    pub(crate) fn format_error(&self) {
        self.format_errors.fetch_add(1, Ordering::Relaxed);
    }

    fn add_fill(&self, frames: usize) {
        self.ring_fill_frames.fetch_add(frames, Ordering::Release);
    }

    fn remove_fill(&self, frames: usize) {
        self.ring_fill_frames.fetch_sub(frames, Ordering::Release);
    }

    #[cfg(test)]
    fn fill(&self) -> usize {
        self.ring_fill_frames.load(Ordering::Acquire)
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    fn ensure_open(&self) -> Result<(), PcmStreamError> {
        if self.closed.load(Ordering::Acquire) {
            Err(PcmStreamError::StreamClosed)
        } else {
            Ok(())
        }
    }
}

/// The unique, non-cloneable producer for a playback stream.
pub struct PcmWriter {
    producer: Producer,
    channels: usize,
    logical_capacity_samples: usize,
    metrics: Arc<AtomicPcmStreamMetrics>,
}

impl PcmWriter {
    /// Writes complete interleaved frames without blocking.
    ///
    /// The ring is unchanged and [`PcmStreamError::BufferFull`] is returned unless every frame
    /// fits.
    pub fn try_write_interleaved(&mut self, pcm: &[f32]) -> Result<(), PcmStreamError> {
        self.metrics.ensure_open()?;
        if !pcm.len().is_multiple_of(self.channels) {
            return Err(PcmStreamError::InvalidBufferLength);
        }
        let requested_frames = pcm.len() / self.channels;
        let used_samples = self.producer.capacity() - self.producer.available();
        let available_frames = (self.logical_capacity_samples - used_samples) / self.channels;
        if requested_frames > available_frames {
            return Err(PcmStreamError::BufferFull {
                requested_frames,
                available_frames,
            });
        }
        // Publish the metric before the ring's release-store makes samples visible to
        // the consumer. This prevents a consumer-side decrement from racing ahead.
        self.metrics.add_fill(requested_frames);
        let written = self.producer.push_slice(pcm);
        debug_assert_eq!(written, pcm.len());
        if self.metrics.ensure_open().is_err() {
            return Err(PcmStreamError::StreamClosed);
        }
        Ok(())
    }
}

struct RingResampler {
    consumer: Consumer,
    channels: usize,
    target_fill_frames: usize,
    priming_threshold_frames: usize,
    control_span_frames: usize,
    nominal_step: f64,
    max_drift_ratio: f64,
    current: Box<[f32]>,
    next: Box<[f32]>,
    interpolated: Box<[f32]>,
    phase: f64,
    have_current: bool,
    have_next: bool,
    primed: bool,
    has_primed: bool,
    metrics: Arc<AtomicPcmStreamMetrics>,
}

impl RingResampler {
    fn new(
        consumer: Consumer,
        channels: usize,
        target_fill_frames: usize,
        capacity_frames: usize,
        nominal_step: f64,
        max_drift_ppm: u32,
        metrics: Arc<AtomicPcmStreamMetrics>,
    ) -> Self {
        Self {
            consumer,
            channels,
            target_fill_frames,
            priming_threshold_frames: target_fill_frames.max(2),
            control_span_frames: target_fill_frames.min(capacity_frames - target_fill_frames),
            nominal_step,
            max_drift_ratio: f64::from(max_drift_ppm) / 1_000_000.0,
            current: vec![0.0; channels].into_boxed_slice(),
            next: vec![0.0; channels].into_boxed_slice(),
            interpolated: vec![0.0; channels].into_boxed_slice(),
            phase: 0.0,
            have_current: false,
            have_next: false,
            primed: false,
            has_primed: false,
            metrics,
        }
    }

    fn buffered_frames(&self) -> usize {
        self.consumer.available() / self.channels
            + usize::from(self.have_current)
            + usize::from(self.have_next)
    }

    fn drift_correction(&self) -> f64 {
        drift_correction(
            self.buffered_frames(),
            self.target_fill_frames,
            self.control_span_frames,
            self.max_drift_ratio,
        )
    }

    fn pop_frame(&mut self, next: bool) -> bool {
        if self.consumer.available() < self.channels {
            return false;
        }
        let output = if next {
            &mut self.next
        } else {
            &mut self.current
        };
        let read = self.consumer.pop_slice(output);
        debug_assert_eq!(read, self.channels);
        self.metrics.remove_fill(1);
        true
    }

    fn prepare_next_frame(&mut self) -> bool {
        if !self.primed {
            if self.buffered_frames() < self.priming_threshold_frames {
                return false;
            }
            self.primed = true;
            self.has_primed = true;
        }

        if !self.have_current {
            self.have_current = self.pop_frame(false);
            if !self.have_current {
                self.primed = false;
                return false;
            }
        }
        if !self.have_next {
            self.have_next = self.pop_frame(true);
            if !self.have_next {
                self.primed = false;
                return false;
            }
        }

        let phase = self.phase as f32;
        for channel in 0..self.channels {
            self.interpolated[channel] =
                self.current[channel] + (self.next[channel] - self.current[channel]) * phase;
        }

        let correction = self.drift_correction();
        self.phase += self.nominal_step * (1.0 + correction);
        while self.phase >= 1.0 && self.have_next {
            self.current.copy_from_slice(&self.next);
            self.have_current = true;
            self.have_next = false;
            self.phase -= 1.0;
            self.have_next = self.pop_frame(true);
        }
        if !self.have_next {
            self.primed = false;
        }
        true
    }

    fn next_frame(&mut self) -> Option<&[f32]> {
        if self.prepare_next_frame() {
            Some(&self.interpolated)
        } else {
            None
        }
    }
}

fn drift_correction(
    fill_frames: usize,
    target_fill_frames: usize,
    control_span_frames: usize,
    max_drift_ratio: f64,
) -> f64 {
    let error = fill_frames as f64 - target_fill_frames as f64;
    (error / control_span_frames as f64 * max_drift_ratio).clamp(-max_drift_ratio, max_drift_ratio)
}

/// The unique, non-cloneable consumer for a capture stream.
pub struct PcmReader {
    resampler: RingResampler,
}

impl PcmReader {
    /// Reads as many complete interleaved frames as are currently available without blocking.
    ///
    /// While the stream is initially priming or re-priming, this returns `0` without consuming
    /// buffered PCM. If an already-primed stream runs out partway through this call, the frames
    /// produced before the underrun are returned and re-priming begins with the next call.
    pub fn try_read_interleaved(&mut self, pcm: &mut [f32]) -> Result<usize, PcmStreamError> {
        self.resampler.metrics.ensure_open()?;
        if !pcm.len().is_multiple_of(self.resampler.channels) {
            return Err(PcmStreamError::InvalidBufferLength);
        }

        let requested_frames = pcm.len() / self.resampler.channels;
        let mut read_frames = 0;
        for output in pcm.chunks_exact_mut(self.resampler.channels) {
            let Some(frame) = self.resampler.next_frame() else {
                break;
            };
            output.copy_from_slice(frame);
            read_frames += 1;
        }
        debug_assert!(read_frames <= requested_frames);
        if self.resampler.metrics.ensure_open().is_err() {
            return Err(PcmStreamError::StreamClosed);
        }
        Ok(read_frames)
    }
}

pub(crate) struct PlaybackCallbackState {
    resampler: RingResampler,
}

impl PlaybackCallbackState {
    fn new(
        consumer: Consumer,
        channels: usize,
        config: &PcmStreamConfig,
        device_sample_rate: u32,
        metrics: Arc<AtomicPcmStreamMetrics>,
    ) -> Self {
        Self {
            resampler: RingResampler::new(
                consumer,
                channels,
                config.target_fill_frames,
                config.capacity_frames,
                f64::from(config.client_format.sample_rate) / f64::from(device_sample_rate),
                config.max_drift_ppm,
                metrics,
            ),
        }
    }

    pub(crate) fn channels(&self) -> usize {
        self.resampler.channels
    }

    pub(crate) fn metrics(&self) -> &AtomicPcmStreamMetrics {
        &self.resampler.metrics
    }

    pub(crate) fn next_frame(&mut self) -> Option<&[f32]> {
        let count_as_underrun = self.resampler.has_primed;
        if self.resampler.prepare_next_frame() {
            self.resampler.metrics.transferred(1);
            Some(&self.resampler.interpolated)
        } else {
            if count_as_underrun {
                self.resampler.metrics.underrun(1);
            }
            None
        }
    }
}

pub(crate) struct CaptureCallbackState {
    producer: Producer,
    channels: usize,
    logical_capacity_samples: usize,
    scratch: Box<[f32]>,
    metrics: Arc<AtomicPcmStreamMetrics>,
}

impl CaptureCallbackState {
    fn new(
        producer: Producer,
        channels: usize,
        capacity_frames: usize,
        metrics: Arc<AtomicPcmStreamMetrics>,
    ) -> Self {
        Self {
            producer,
            channels,
            logical_capacity_samples: capacity_frames * channels,
            scratch: vec![0.0; channels].into_boxed_slice(),
            metrics,
        }
    }

    pub(crate) fn channels(&self) -> usize {
        self.channels
    }

    pub(crate) fn metrics(&self) -> &AtomicPcmStreamMetrics {
        &self.metrics
    }

    pub(crate) fn has_capacity_for_frame(&self) -> bool {
        let used_samples = self.producer.capacity() - self.producer.available();
        used_samples + self.channels <= self.logical_capacity_samples
    }

    pub(crate) fn scratch_mut(&mut self) -> &mut [f32] {
        &mut self.scratch
    }

    pub(crate) fn commit_scratch_frame(&mut self) {
        // Keep the fill counter ordered ahead of publication to the reader.
        self.metrics.add_fill(1);
        let written = self.producer.push_slice(&self.scratch);
        debug_assert_eq!(written, self.channels);
        self.metrics.transferred(1);
    }

    #[cfg(test)]
    fn capture_interleaved(&mut self, input: &[f32]) {
        for frame in input.chunks_exact(self.channels) {
            if !self.has_capacity_for_frame() {
                self.metrics.overrun(1);
                continue;
            }
            self.scratch.copy_from_slice(frame);
            self.commit_scratch_frame();
        }
    }
}

/// An active process-to-Core-Audio-output stream that can be moved between threads.
///
/// On a VCable device, PCM written through the associated [`PcmWriter`] is played into the
/// device's output side and becomes available from the device's looped-back input side.
pub struct PcmPlaybackStream {
    metrics: Arc<AtomicPcmStreamMetrics>,
    #[cfg(target_os = "macos")]
    platform: Option<crate::platform::PcmPlatformStream>,
}

impl PcmPlaybackStream {
    /// Validates the UID and format, creates the IOProc, configures stream usage, and starts I/O.
    pub fn start(config: PcmStreamConfig) -> Result<(Self, PcmWriter), PcmStreamError> {
        let validated = validate_config(&config)?;
        #[cfg(not(target_os = "macos"))]
        {
            let _ = validated;
            return Err(PcmStreamError::UnsupportedPlatform);
        }
        #[cfg(target_os = "macos")]
        {
            let device = crate::platform::resolve_pcm_device(&config, PcmStreamDirection::Output)?;
            let metrics = Arc::new(AtomicPcmStreamMetrics::new(config.capacity_frames));
            let (producer, consumer) = create_ring(validated.internal_sample_capacity)?;
            let writer = PcmWriter {
                producer,
                channels: validated.channels,
                logical_capacity_samples: validated.logical_capacity_samples,
                metrics: Arc::clone(&metrics),
            };
            let callback = PlaybackCallbackState::new(
                consumer,
                validated.channels,
                &config,
                device.sample_rate,
                Arc::clone(&metrics),
            );
            let platform = crate::platform::PcmPlatformStream::start_playback(device, callback)?;
            Ok((
                Self {
                    metrics,
                    platform: Some(platform),
                },
                writer,
            ))
        }
    }

    /// Returns a point-in-time snapshot without blocking the real-time callback.
    pub fn metrics(&self) -> PcmStreamMetrics {
        self.metrics.snapshot()
    }
}

impl Drop for PcmPlaybackStream {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        drop(self.platform.take());
        self.metrics.close();
    }
}

/// An active Core-Audio-input-to-process stream that can be moved between threads.
///
/// On a VCable device, the associated [`PcmReader`] captures PCM from the input side, including
/// PCM looped back from a [`PcmPlaybackStream`] using the same device.
pub struct PcmCaptureStream {
    metrics: Arc<AtomicPcmStreamMetrics>,
    #[cfg(target_os = "macos")]
    platform: Option<crate::platform::PcmPlatformStream>,
}

impl PcmCaptureStream {
    /// Validates the UID and format, creates the IOProc, configures stream usage, and starts I/O.
    pub fn start(config: PcmStreamConfig) -> Result<(Self, PcmReader), PcmStreamError> {
        let validated = validate_config(&config)?;
        #[cfg(not(target_os = "macos"))]
        {
            let _ = validated;
            return Err(PcmStreamError::UnsupportedPlatform);
        }
        #[cfg(target_os = "macos")]
        {
            let device = crate::platform::resolve_pcm_device(&config, PcmStreamDirection::Input)?;
            let metrics = Arc::new(AtomicPcmStreamMetrics::new(config.capacity_frames));
            let (producer, consumer) = create_ring(validated.internal_sample_capacity)?;
            let callback = CaptureCallbackState::new(
                producer,
                validated.channels,
                config.capacity_frames,
                Arc::clone(&metrics),
            );
            let reader = PcmReader {
                resampler: RingResampler::new(
                    consumer,
                    validated.channels,
                    config.target_fill_frames,
                    config.capacity_frames,
                    f64::from(device.sample_rate) / f64::from(config.client_format.sample_rate),
                    config.max_drift_ppm,
                    Arc::clone(&metrics),
                ),
            };
            let platform = crate::platform::PcmPlatformStream::start_capture(device, callback)?;
            Ok((
                Self {
                    metrics,
                    platform: Some(platform),
                },
                reader,
            ))
        }
    }

    /// Returns a point-in-time snapshot without blocking the real-time callback.
    pub fn metrics(&self) -> PcmStreamMetrics {
        self.metrics.snapshot()
    }
}

impl Drop for PcmCaptureStream {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        drop(self.platform.take());
        self.metrics.close();
    }
}

struct ValidatedConfig {
    channels: usize,
    logical_capacity_samples: usize,
    internal_sample_capacity: usize,
}

fn validate_config(config: &PcmStreamConfig) -> Result<ValidatedConfig, PcmStreamError> {
    let channels = usize::try_from(config.client_format.channels)
        .ok()
        .filter(|channels| *channels > 0)
        .ok_or(PcmStreamError::InvalidChannelCount)?;
    if config.client_format.sample_rate == 0 {
        return Err(PcmStreamError::InvalidSampleRate);
    }
    if config.capacity_frames < 2 {
        return Err(PcmStreamError::InvalidRingCapacity);
    }
    if config.target_fill_frames == 0 || config.target_fill_frames >= config.capacity_frames {
        return Err(PcmStreamError::InvalidTargetFill);
    }
    if config.max_drift_ppm >= 1_000_000 {
        return Err(PcmStreamError::InvalidDriftPpm);
    }
    let logical_capacity_samples = config
        .capacity_frames
        .checked_mul(channels)
        .ok_or(PcmStreamError::InvalidRingCapacity)?;
    let internal_sample_capacity = logical_capacity_samples
        .checked_next_power_of_two()
        .ok_or(PcmStreamError::InvalidRingCapacity)?;
    Ok(ValidatedConfig {
        channels,
        logical_capacity_samples,
        internal_sample_capacity,
    })
}

fn create_ring(capacity: usize) -> Result<(Producer, Consumer), PcmStreamError> {
    spsc_ring_buffer(capacity).map_err(|_| PcmStreamError::InvalidRingCapacity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(capacity_frames: usize, target_fill_frames: usize) -> PcmStreamConfig {
        PcmStreamConfig {
            device_uid: "test-device".to_owned(),
            client_format: PcmFormat {
                sample_rate: 48_000,
                channels: 2,
            },
            capacity_frames,
            target_fill_frames,
            max_drift_ppm: 1_000,
        }
    }

    fn playback_harness(
        config: &PcmStreamConfig,
    ) -> (
        PcmWriter,
        PlaybackCallbackState,
        Arc<AtomicPcmStreamMetrics>,
    ) {
        let validated = validate_config(config).unwrap();
        let metrics = Arc::new(AtomicPcmStreamMetrics::new(config.capacity_frames));
        let (producer, consumer) = create_ring(validated.internal_sample_capacity).unwrap();
        (
            PcmWriter {
                producer,
                channels: validated.channels,
                logical_capacity_samples: validated.logical_capacity_samples,
                metrics: Arc::clone(&metrics),
            },
            PlaybackCallbackState::new(
                consumer,
                validated.channels,
                config,
                config.client_format.sample_rate,
                Arc::clone(&metrics),
            ),
            metrics,
        )
    }

    fn capture_harness(
        config: &PcmStreamConfig,
    ) -> (CaptureCallbackState, PcmReader, Arc<AtomicPcmStreamMetrics>) {
        let validated = validate_config(config).unwrap();
        let metrics = Arc::new(AtomicPcmStreamMetrics::new(config.capacity_frames));
        let (producer, consumer) = create_ring(validated.internal_sample_capacity).unwrap();
        (
            CaptureCallbackState::new(
                producer,
                validated.channels,
                config.capacity_frames,
                Arc::clone(&metrics),
            ),
            PcmReader {
                resampler: RingResampler::new(
                    consumer,
                    validated.channels,
                    config.target_fill_frames,
                    config.capacity_frames,
                    1.0,
                    config.max_drift_ppm,
                    Arc::clone(&metrics),
                ),
            },
            metrics,
        )
    }

    #[test]
    fn rejects_partial_interleaved_frames() {
        let (mut writer, _, _) = playback_harness(&config(8, 4));
        assert_eq!(
            writer.try_write_interleaved(&[0.0; 3]),
            Err(PcmStreamError::InvalidBufferLength)
        );

        let (_, mut reader, _) = capture_harness(&config(8, 4));
        assert_eq!(
            reader.try_read_interleaved(&mut [0.0; 3]),
            Err(PcmStreamError::InvalidBufferLength)
        );
    }

    #[test]
    fn playback_write_is_all_or_nothing_when_full() {
        let (mut writer, _, metrics) = playback_harness(&config(4, 2));
        writer.try_write_interleaved(&[1.0; 6]).unwrap();
        assert_eq!(
            writer.try_write_interleaved(&[2.0; 4]),
            Err(PcmStreamError::BufferFull {
                requested_frames: 2,
                available_frames: 1,
            })
        );
        assert_eq!(metrics.snapshot().ring_fill_frames, 3);
    }

    #[test]
    fn capture_waits_for_target_without_consuming_then_preserves_order() {
        let (mut callback, mut reader, metrics) = capture_harness(&config(8, 4));
        callback.capture_interleaved(&[1.0, 10.0, 2.0, 20.0, 3.0, 30.0]);

        let mut output = [9.0; 4];
        assert_eq!(reader.try_read_interleaved(&mut output).unwrap(), 0);
        assert_eq!(output, [9.0; 4]);
        assert_eq!(metrics.snapshot().ring_fill_frames, 3);

        callback.capture_interleaved(&[4.0, 40.0]);
        assert_eq!(reader.try_read_interleaved(&mut output).unwrap(), 2);
        assert_eq!(output, [1.0, 10.0, 2.0, 20.0]);
    }

    #[test]
    fn capture_discards_new_frames_on_overrun() {
        let (mut callback, _, metrics) = capture_harness(&config(4, 2));
        callback.capture_interleaved(&[1.0; 12]);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.ring_fill_frames, 4);
        assert_eq!(snapshot.transferred_frames, 4);
        assert_eq!(snapshot.overrun_frames, 2);
    }

    #[test]
    fn playback_waits_for_target_without_consuming_or_counting_underruns() {
        let (mut writer, mut callback, metrics) = playback_harness(&config(8, 4));
        writer
            .try_write_interleaved(&[1.0, 10.0, 2.0, 20.0, 3.0, 30.0])
            .unwrap();

        assert!(callback.next_frame().is_none());
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.transferred_frames, 0);
        assert_eq!(snapshot.underrun_frames, 0);
        assert_eq!(snapshot.ring_fill_frames, 3);

        writer.try_write_interleaved(&[4.0, 40.0]).unwrap();
        assert_eq!(callback.next_frame().unwrap(), [1.0, 10.0]);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.transferred_frames, 1);
        assert_eq!(snapshot.underrun_frames, 0);
        assert_eq!(snapshot.ring_fill_frames, 1);
        assert_eq!(snapshot.ring_capacity_frames, 8);
    }

    #[test]
    fn priming_requires_two_frames_when_target_is_one() {
        let (mut writer, mut callback, metrics) = playback_harness(&config(4, 1));
        writer.try_write_interleaved(&[1.0, 10.0]).unwrap();
        assert!(callback.next_frame().is_none());
        assert_eq!(metrics.snapshot().ring_fill_frames, 1);

        writer.try_write_interleaved(&[2.0, 20.0]).unwrap();
        assert_eq!(callback.next_frame().unwrap(), [1.0, 10.0]);
    }

    #[test]
    fn playback_reprimes_without_consuming_partial_fill_and_preserves_order() {
        let mut test_config = config(8, 4);
        test_config.max_drift_ppm = 0;
        let (mut writer, mut callback, metrics) = playback_harness(&test_config);
        writer
            .try_write_interleaved(&[1.0, 10.0, 2.0, 20.0, 3.0, 30.0, 4.0, 40.0])
            .unwrap();
        for expected in [[1.0, 10.0], [2.0, 20.0], [3.0, 30.0]] {
            assert_eq!(callback.next_frame().unwrap(), expected);
        }
        assert!(!callback.resampler.primed);

        assert!(callback.next_frame().is_none());
        assert_eq!(metrics.snapshot().underrun_frames, 1);

        writer
            .try_write_interleaved(&[5.0, 50.0, 6.0, 60.0])
            .unwrap();
        assert!(callback.next_frame().is_none());
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.ring_fill_frames, 2);
        assert_eq!(snapshot.underrun_frames, 2);

        writer.try_write_interleaved(&[7.0, 70.0]).unwrap();
        for expected in [[4.0, 40.0], [5.0, 50.0], [6.0, 60.0]] {
            assert_eq!(callback.next_frame().unwrap(), expected);
        }
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.transferred_frames, 6);
        assert_eq!(snapshot.underrun_frames, 2);
    }

    #[test]
    fn capture_returns_partial_read_then_reprimes_without_consuming_partial_fill() {
        let mut test_config = config(8, 4);
        test_config.max_drift_ppm = 0;
        let (mut callback, mut reader, metrics) = capture_harness(&test_config);
        callback.capture_interleaved(&[1.0, 10.0, 2.0, 20.0, 3.0, 30.0, 4.0, 40.0]);

        let mut initial = [9.0; 8];
        assert_eq!(reader.try_read_interleaved(&mut initial).unwrap(), 3);
        assert_eq!(&initial[..6], &[1.0, 10.0, 2.0, 20.0, 3.0, 30.0]);
        assert!(!reader.resampler.primed);

        callback.capture_interleaved(&[5.0, 50.0, 6.0, 60.0]);
        let mut output = [9.0; 6];
        assert_eq!(reader.try_read_interleaved(&mut output).unwrap(), 0);
        assert_eq!(output, [9.0; 6]);
        assert_eq!(metrics.snapshot().ring_fill_frames, 2);

        callback.capture_interleaved(&[7.0, 70.0]);
        assert_eq!(reader.try_read_interleaved(&mut output).unwrap(), 3);
        assert_eq!(output, [4.0, 40.0, 5.0, 50.0, 6.0, 60.0]);
    }

    #[test]
    fn drift_control_is_centered_and_clamped_to_one_thousand_ppm() {
        let max = 1_000.0 / 1_000_000.0;
        assert_eq!(drift_correction(960, 960, 960, max), 0.0);
        assert!((drift_correction(0, 960, 960, max) + max).abs() < f64::EPSILON);
        assert!((drift_correction(1_920, 960, 960, max) - max).abs() < f64::EPSILON);
        assert!((drift_correction(8_192, 960, 960, max) - max).abs() < f64::EPSILON);
    }

    #[test]
    fn drift_control_uses_ring_and_cached_frames_as_total_fill() {
        let test_config = config(8, 4);
        let validated = validate_config(&test_config).unwrap();
        let metrics = Arc::new(AtomicPcmStreamMetrics::new(test_config.capacity_frames));
        let (mut producer, consumer) = create_ring(validated.internal_sample_capacity).unwrap();
        let mut resampler = RingResampler::new(
            consumer,
            2,
            test_config.target_fill_frames,
            test_config.capacity_frames,
            0.0,
            test_config.max_drift_ppm,
            Arc::clone(&metrics),
        );
        for _ in 0..test_config.target_fill_frames {
            assert_eq!(producer.push_slice(&[0.25, -0.25]), 2);
            metrics.add_fill(1);
        }

        assert_eq!(resampler.drift_correction(), 0.0);
        assert!(resampler.next_frame().is_some());
        assert_eq!(metrics.snapshot().ring_fill_frames, 2);
        assert_eq!(resampler.buffered_frames(), 4);
        assert_eq!(resampler.drift_correction(), 0.0);
    }

    #[test]
    fn drift_simulation_stays_inside_ring_at_plus_and_minus_one_thousand_ppm() {
        for input_per_output in [1.001_f64, 0.999_f64] {
            let test_config = config(8_192, 2_048);
            let validated = validate_config(&test_config).unwrap();
            let metrics = Arc::new(AtomicPcmStreamMetrics::new(test_config.capacity_frames));
            let (mut producer, consumer) = create_ring(validated.internal_sample_capacity).unwrap();
            let mut resampler = RingResampler::new(
                consumer,
                2,
                test_config.target_fill_frames,
                test_config.capacity_frames,
                1.0,
                1_000,
                Arc::clone(&metrics),
            );
            for _ in 0..test_config.target_fill_frames {
                assert_eq!(producer.push_slice(&[0.25, -0.25]), 2);
                metrics.add_fill(1);
            }
            let mut input_phase = 0.0;
            for _ in 0..250_000 {
                input_phase += input_per_output;
                while input_phase >= 1.0 {
                    if producer.push_slice(&[0.25, -0.25]) == 2 {
                        metrics.add_fill(1);
                    }
                    input_phase -= 1.0;
                }
                assert!(resampler.next_frame().is_some());
            }
            let fill = metrics.fill();
            assert!(fill > 0);
            assert!(fill < test_config.capacity_frames);
        }
    }

    #[test]
    fn reader_and_writer_observe_stream_close() {
        let (mut writer, _, playback_metrics) = playback_harness(&config(8, 4));
        playback_metrics.close();
        assert_eq!(
            writer.try_write_interleaved(&[0.0; 2]),
            Err(PcmStreamError::StreamClosed)
        );

        let (_, mut reader, capture_metrics) = capture_harness(&config(8, 4));
        capture_metrics.close();
        assert_eq!(
            reader.try_read_interleaved(&mut [0.0; 2]),
            Err(PcmStreamError::StreamClosed)
        );
    }

    #[test]
    fn pcm_handles_are_send() {
        fn assert_send<T: Send>() {}
        assert_send::<PcmReader>();
        assert_send::<PcmWriter>();
        assert_send::<PcmPlaybackStream>();
        assert_send::<PcmCaptureStream>();
    }
}
