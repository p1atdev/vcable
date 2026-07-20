use crate::{AudioDevice, AudioRoute, CoreAudioError};
use std::collections::BTreeMap;
use std::ffi::{c_char, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use vcable_core::{Consumer, Producer, spsc_ring_buffer};

type AudioObjectId = u32;
type OsStatus = i32;
type CfStringRef = *const c_void;
type AudioDeviceIoProcId = *mut c_void;

const SYSTEM_OBJECT: AudioObjectId = 1;
const SCOPE_GLOBAL: u32 = fourcc(*b"glob");
const SCOPE_INPUT: u32 = fourcc(*b"inpt");
const SCOPE_OUTPUT: u32 = fourcc(*b"outp");
const ELEMENT_MAIN: u32 = 0;
const HARDWARE_DEVICES: u32 = fourcc(*b"dev#");
const HARDWARE_PLUGIN_LIST: u32 = fourcc(*b"plg#");
const OBJECT_NAME: u32 = fourcc(*b"lnam");
const PLUGIN_BUNDLE_ID: u32 = fourcc(*b"piid");
const DEVICE_UID: u32 = fourcc(*b"uid ");
const DEVICE_SAMPLE_RATE: u32 = fourcc(*b"nsrt");
const DEVICE_TRANSPORT_TYPE: u32 = fourcc(*b"tran");
const DEVICE_STREAM_CONFIGURATION: u32 = fourcc(*b"slay");
const DEVICE_STREAMS: u32 = fourcc(*b"stm#");
const DEVICE_IOPROC_STREAM_USAGE: u32 = fourcc(*b"suse");
const VCABLE_CONTROL: u32 = fourcc(*b"vctl");
const VCABLE_BUNDLE_ID: &str = "dev.vcable.driver";
const VCABLE_UID_PREFIX: &str = "dev.vcable.device.";
const TRANSPORT_TYPE_VIRTUAL: u32 = fourcc(*b"virt");
const UTF8_ENCODING: u32 = 0x0800_0100;
const BAD_OBJECT_STATUS: OsStatus = 560_947_818;
const DEVICE_CHANGE_TIMEOUT: Duration = Duration::from_secs(5);
const DEVICE_CHANGE_POLL_INTERVAL: Duration = Duration::from_millis(10);

const fn fourcc(value: [u8; 4]) -> u32 {
    u32::from_be_bytes(value)
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[repr(C)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferListHeader {
    number_buffers: u32,
    first_buffer: AudioBuffer,
}

#[repr(C)]
struct AudioHardwareIoProcStreamUsageHeader {
    io_proc: *mut c_void,
    number_streams: u32,
    first_stream_is_on: u32,
}

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyDataSize(
        object_id: AudioObjectId,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: u32,
        qualifier_data: *const c_void,
        data_size: *mut u32,
    ) -> OsStatus;
    fn AudioObjectGetPropertyData(
        object_id: AudioObjectId,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: u32,
        qualifier_data: *const c_void,
        data_size: *mut u32,
        data: *mut c_void,
    ) -> OsStatus;
    fn AudioObjectSetPropertyData(
        object_id: AudioObjectId,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: u32,
        qualifier_data: *const c_void,
        data_size: u32,
        data: *const c_void,
    ) -> OsStatus;
    fn AudioDeviceCreateIOProcID(
        device: AudioObjectId,
        procedure: Option<AudioDeviceIoProc>,
        client_data: *mut c_void,
        procedure_id: *mut AudioDeviceIoProcId,
    ) -> OsStatus;
    fn AudioDeviceDestroyIOProcID(
        device: AudioObjectId,
        procedure_id: AudioDeviceIoProcId,
    ) -> OsStatus;
    fn AudioDeviceStart(device: AudioObjectId, procedure_id: AudioDeviceIoProcId) -> OsStatus;
    fn AudioDeviceStop(device: AudioObjectId, procedure_id: AudioDeviceIoProcId) -> OsStatus;
}

type AudioDeviceIoProc = unsafe extern "C" fn(
    AudioObjectId,
    *const c_void,
    *const AudioBufferListHeader,
    *const c_void,
    *mut AudioBufferListHeader,
    *const c_void,
    *mut c_void,
) -> OsStatus;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithBytes(
        allocator: *const c_void,
        bytes: *const u8,
        number_bytes: isize,
        encoding: u32,
        is_external_representation: bool,
    ) -> CfStringRef;
    fn CFStringGetLength(string: CfStringRef) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(
        string: CfStringRef,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> bool;
    fn CFRelease(value: *const c_void);
}

struct OwnedCfString(CfStringRef);

impl OwnedCfString {
    fn create(value: &str) -> Result<Self, CoreAudioError> {
        // Safety: value remains alive for the call and the byte count is exact.
        let string = unsafe {
            CFStringCreateWithBytes(
                ptr::null(),
                value.as_ptr(),
                value.len() as isize,
                UTF8_ENCODING,
                false,
            )
        };
        if string.is_null() {
            return Err(CoreAudioError::InvalidProperty("CFStringCreateWithBytes"));
        }
        Ok(Self(string))
    }
}

impl Drop for OwnedCfString {
    fn drop(&mut self) {
        // Safety: OwnedCfString owns the non-null reference returned at creation.
        unsafe { CFRelease(self.0) };
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RouterMetrics {
    pub underruns: u64,
    pub overruns: u64,
    pub format_errors: u64,
}

#[derive(Default)]
struct AtomicRouterMetrics {
    underruns: AtomicU64,
    overruns: AtomicU64,
    format_errors: AtomicU64,
}

impl AtomicRouterMetrics {
    fn snapshot(&self) -> RouterMetrics {
        RouterMetrics {
            underruns: self.underruns.load(Ordering::Relaxed),
            overruns: self.overruns.load(Ordering::Relaxed),
            format_errors: self.format_errors.load(Ordering::Relaxed),
        }
    }
}

struct OutgoingRoute {
    producer: Producer,
    channels: usize,
}

struct IncomingRoute {
    consumer: Consumer,
    source_channels: usize,
    sink_channels: usize,
    nominal_step: f64,
    gain: f32,
    matrix: Box<[f32]>,
    current: Box<[f32]>,
    next: Box<[f32]>,
    interpolated: Box<[f32]>,
    phase: f64,
    primed: bool,
}

impl IncomingRoute {
    const STARTUP_FRAMES: usize = 512;

    fn read_source_frame(&mut self, next: bool) -> bool {
        if self.consumer.available() < self.source_channels {
            return false;
        }
        let destination = if next {
            &mut self.next
        } else {
            &mut self.current
        };
        for sample in destination.iter_mut() {
            let Some(value) = self.consumer.pop() else {
                return false;
            };
            *sample = value;
        }
        true
    }

    fn prepare_output_frame(&mut self, metrics: &AtomicRouterMetrics) -> bool {
        if !self.primed {
            let available_frames = self.consumer.available() / self.source_channels;
            if available_frames < Self::STARTUP_FRAMES {
                return false;
            }
            if !self.read_source_frame(false) || !self.read_source_frame(true) {
                metrics.underruns.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            self.phase = 0.0;
            self.primed = true;
        }

        let phase = self.phase as f32;
        for channel in 0..self.source_channels {
            self.interpolated[channel] =
                self.current[channel] + (self.next[channel] - self.current[channel]) * phase;
        }

        let available_frames = self.consumer.available() / self.source_channels;
        let target = Self::STARTUP_FRAMES as f64;
        let error = (available_frames as f64 - target) / target;
        let correction = (error * 0.0005).clamp(-0.001, 0.001);
        self.phase += self.nominal_step * (1.0 + correction);
        while self.phase >= 1.0 {
            self.current.copy_from_slice(&self.next);
            if !self.read_source_frame(true) {
                self.primed = false;
                metrics.underruns.fetch_add(1, Ordering::Relaxed);
                break;
            }
            self.phase -= 1.0;
        }
        true
    }

    fn mixed_sample(&self, sink_channel: usize) -> f32 {
        let row = &self.matrix[sink_channel * self.source_channels..][..self.source_channels];
        row.iter()
            .zip(self.interpolated.iter())
            .map(|(coefficient, sample)| coefficient * sample)
            .sum::<f32>()
            * self.gain
    }
}

struct DeviceIoContext {
    device_id: AudioObjectId,
    outgoing: Vec<OutgoingRoute>,
    incoming: Vec<IncomingRoute>,
    procedure_id: AudioDeviceIoProcId,
    running: bool,
    metrics: Arc<AtomicRouterMetrics>,
}

pub struct AudioRouter {
    // Boxes keep callback client-data pointers stable while the vector grows.
    #[allow(clippy::vec_box)]
    contexts: Vec<Box<DeviceIoContext>>,
    metrics: Arc<AtomicRouterMetrics>,
}

impl AudioRouter {
    pub fn start(routes: &[AudioRoute]) -> Result<Self, CoreAudioError> {
        validate_routes(routes)?;
        let metrics = Arc::new(AtomicRouterMetrics::default());
        let mut builders: BTreeMap<AudioObjectId, (Vec<OutgoingRoute>, Vec<IncomingRoute>)> =
            BTreeMap::new();

        for route in routes {
            let sample_capacity = (route.source_channels as usize)
                .checked_mul(16_384)
                .ok_or_else(|| {
                    CoreAudioError::InvalidRoute("ring-buffer size overflow".to_owned())
                })?
                .next_power_of_two();
            let (producer, consumer) = spsc_ring_buffer(sample_capacity)
                .map_err(|error| CoreAudioError::InvalidRoute(error.to_string()))?;
            builders
                .entry(route.source_device_id)
                .or_default()
                .0
                .push(OutgoingRoute {
                    producer,
                    channels: route.source_channels as usize,
                });
            builders
                .entry(route.sink_device_id)
                .or_default()
                .1
                .push(IncomingRoute {
                    consumer,
                    source_channels: route.source_channels as usize,
                    sink_channels: route.sink_channels as usize,
                    nominal_step: f64::from(route.source_sample_rate)
                        / f64::from(route.sink_sample_rate),
                    gain: route.gain,
                    matrix: route.matrix.clone().into_boxed_slice(),
                    current: vec![0.0; route.source_channels as usize].into_boxed_slice(),
                    next: vec![0.0; route.source_channels as usize].into_boxed_slice(),
                    interpolated: vec![0.0; route.source_channels as usize].into_boxed_slice(),
                    phase: 0.0,
                    primed: false,
                });
        }

        let mut router = Self {
            contexts: builders
                .into_iter()
                .map(|(device_id, (outgoing, incoming))| {
                    Box::new(DeviceIoContext {
                        device_id,
                        outgoing,
                        incoming,
                        procedure_id: ptr::null_mut(),
                        running: false,
                        metrics: Arc::clone(&metrics),
                    })
                })
                .collect(),
            metrics,
        };

        for context in &mut router.contexts {
            let context_pointer: *mut DeviceIoContext = &mut **context;
            // Safety: context is boxed and remains at a stable address until
            // its IOProc is destroyed in Drop.
            status(unsafe {
                AudioDeviceCreateIOProcID(
                    context.device_id,
                    Some(audio_device_io_proc),
                    context_pointer.cast(),
                    &mut context.procedure_id,
                )
            })?;
            configure_stream_usage(context, SCOPE_INPUT, !context.outgoing.is_empty())?;
            configure_stream_usage(context, SCOPE_OUTPUT, !context.incoming.is_empty())?;
        }
        for context in &mut router.contexts {
            // Safety: the procedure ID was created for this device above.
            status(unsafe { AudioDeviceStart(context.device_id, context.procedure_id) })?;
            context.running = true;
        }
        Ok(router)
    }

    pub fn metrics(&self) -> RouterMetrics {
        self.metrics.snapshot()
    }
}

impl Drop for AudioRouter {
    fn drop(&mut self) {
        for context in &mut self.contexts {
            if context.running {
                // Safety: the procedure ID belongs to this device and context.
                let _ = unsafe { AudioDeviceStop(context.device_id, context.procedure_id) };
                context.running = false;
            }
            if !context.procedure_id.is_null() {
                // Safety: stopping above ensures the callback no longer uses context.
                let _ =
                    unsafe { AudioDeviceDestroyIOProcID(context.device_id, context.procedure_id) };
                context.procedure_id = ptr::null_mut();
            }
        }
    }
}

fn validate_routes(routes: &[AudioRoute]) -> Result<(), CoreAudioError> {
    let devices = list_devices()?
        .into_iter()
        .map(|device| (device.object_id, device))
        .collect::<BTreeMap<_, _>>();
    for route in routes {
        if route.source_channels == 0 || route.sink_channels == 0 {
            return Err(CoreAudioError::InvalidRoute(
                "channel counts must be greater than zero".to_owned(),
            ));
        }
        if route.matrix.len() != route.source_channels as usize * route.sink_channels as usize
            || route.matrix.iter().any(|value| !value.is_finite())
        {
            return Err(CoreAudioError::InvalidRoute(
                "channel matrix dimensions or values are invalid".to_owned(),
            ));
        }
        if !route.gain.is_finite() || route.source_sample_rate == 0 || route.sink_sample_rate == 0 {
            return Err(CoreAudioError::InvalidRoute(
                "gain and sample rates must be finite and nonzero".to_owned(),
            ));
        }
        let source = devices.get(&route.source_device_id).ok_or_else(|| {
            CoreAudioError::InvalidRoute(format!(
                "source device {} is unavailable",
                route.source_device_id
            ))
        })?;
        let sink = devices.get(&route.sink_device_id).ok_or_else(|| {
            CoreAudioError::InvalidRoute(format!(
                "sink device {} is unavailable",
                route.sink_device_id
            ))
        })?;
        if source.input_channels != route.source_channels
            || source.sample_rate != route.source_sample_rate
        {
            return Err(CoreAudioError::InvalidRoute(format!(
                "source device {} format changed",
                route.source_device_id
            )));
        }
        if sink.output_channels != route.sink_channels || sink.sample_rate != route.sink_sample_rate
        {
            return Err(CoreAudioError::InvalidRoute(format!(
                "sink device {} format changed",
                route.sink_device_id
            )));
        }
    }
    Ok(())
}

unsafe extern "C" fn audio_device_io_proc(
    _device_id: AudioObjectId,
    _now: *const c_void,
    input: *const AudioBufferListHeader,
    _input_time: *const c_void,
    output: *mut AudioBufferListHeader,
    _output_time: *const c_void,
    client_data: *mut c_void,
) -> OsStatus {
    if client_data.is_null() {
        return -1;
    }
    // Safety: AudioRouter passes a stable DeviceIoContext pointer and destroys
    // the IOProc before freeing it.
    let context = unsafe { &mut *client_data.cast::<DeviceIoContext>() };
    if !context.outgoing.is_empty() && !input.is_null() {
        // Safety: Core Audio owns a valid AudioBufferList for this callback.
        let Some(frames) = (unsafe { audio_buffer_list_frames(input) }) else {
            context
                .metrics
                .format_errors
                .fetch_add(1, Ordering::Relaxed);
            return 0;
        };
        for frame in 0..frames {
            for route in &mut context.outgoing {
                if route.producer.available() < route.channels {
                    context.metrics.overruns.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                for channel in 0..route.channels {
                    // Safety: frame/channel are bounded by the validated ABL.
                    let Some(sample) = (unsafe { read_audio_sample(input, frame, channel) }) else {
                        context
                            .metrics
                            .format_errors
                            .fetch_add(1, Ordering::Relaxed);
                        return 0;
                    };
                    let pushed = route.producer.push(sample);
                    debug_assert!(pushed);
                }
            }
        }
    }

    if !context.incoming.is_empty() && !output.is_null() {
        // Safety: Core Audio owns a writable AudioBufferList for this callback.
        let Some(frames) = (unsafe { audio_buffer_list_frames(output) }) else {
            context
                .metrics
                .format_errors
                .fetch_add(1, Ordering::Relaxed);
            return 0;
        };
        // Safety: frame count validation ensures buffer byte sizes are sound.
        unsafe { clear_audio_buffer_list(output) };
        for route in &mut context.incoming {
            for frame in 0..frames {
                if !route.prepare_output_frame(&context.metrics) {
                    continue;
                }
                for channel in 0..route.sink_channels {
                    // Safety: output was validated for this frame count above.
                    if !unsafe {
                        add_audio_sample(output, frame, channel, route.mixed_sample(channel))
                    } {
                        context
                            .metrics
                            .format_errors
                            .fetch_add(1, Ordering::Relaxed);
                        return 0;
                    }
                }
            }
        }
    }
    0
}

unsafe fn audio_buffer_list_frames(list: *const AudioBufferListHeader) -> Option<usize> {
    // Safety: caller guarantees Core Audio provided a valid list pointer.
    let number_buffers = unsafe { (*list).number_buffers as usize };
    if number_buffers == 0 {
        return Some(0);
    }
    let first = unsafe { &raw const (*list).first_buffer };
    let mut frames = None;
    for index in 0..number_buffers {
        // Safety: AudioBufferList contains number_buffers flexible entries.
        let buffer = unsafe { &*first.add(index) };
        if buffer.number_channels == 0 || buffer.data.is_null() {
            return None;
        }
        let bytes_per_frame = buffer.number_channels as usize * size_of::<f32>();
        if !(buffer.data_byte_size as usize).is_multiple_of(bytes_per_frame) {
            return None;
        }
        let current = buffer.data_byte_size as usize / bytes_per_frame;
        if frames.is_some_and(|value| value != current) {
            return None;
        }
        frames = Some(current);
    }
    frames
}

unsafe fn read_audio_sample(
    list: *const AudioBufferListHeader,
    frame: usize,
    channel: usize,
) -> Option<f32> {
    let mut channel_offset = 0_usize;
    // Safety: caller guarantees a valid ABL.
    let number_buffers = unsafe { (*list).number_buffers as usize };
    let first = unsafe { &raw const (*list).first_buffer };
    for index in 0..number_buffers {
        // Safety: flexible-array entry is within number_buffers.
        let buffer = unsafe { &*first.add(index) };
        let channels = buffer.number_channels as usize;
        if channel < channel_offset + channels {
            let local_channel = channel - channel_offset;
            let sample_index = frame.checked_mul(channels)?.checked_add(local_channel)?;
            if sample_index >= buffer.data_byte_size as usize / size_of::<f32>() {
                return None;
            }
            // Safety: index was checked against the buffer's byte size.
            return Some(unsafe { *buffer.data.cast::<f32>().add(sample_index) });
        }
        channel_offset += channels;
    }
    None
}

unsafe fn add_audio_sample(
    list: *mut AudioBufferListHeader,
    frame: usize,
    channel: usize,
    value: f32,
) -> bool {
    let mut channel_offset = 0_usize;
    // Safety: caller guarantees a valid mutable ABL.
    let number_buffers = unsafe { (*list).number_buffers as usize };
    let first = unsafe { &raw mut (*list).first_buffer };
    for index in 0..number_buffers {
        // Safety: flexible-array entry is within number_buffers.
        let buffer = unsafe { &mut *first.add(index) };
        let channels = buffer.number_channels as usize;
        if channel < channel_offset + channels {
            let local_channel = channel - channel_offset;
            let Some(sample_index) = frame
                .checked_mul(channels)
                .and_then(|index| index.checked_add(local_channel))
            else {
                return false;
            };
            if sample_index >= buffer.data_byte_size as usize / size_of::<f32>() {
                return false;
            }
            // Safety: index was checked against the writable buffer size.
            unsafe { *buffer.data.cast::<f32>().add(sample_index) += value };
            return true;
        }
        channel_offset += channels;
    }
    false
}

unsafe fn clear_audio_buffer_list(list: *mut AudioBufferListHeader) {
    // Safety: caller guarantees a writable ABL from Core Audio.
    let number_buffers = unsafe { (*list).number_buffers as usize };
    let first = unsafe { &raw mut (*list).first_buffer };
    for index in 0..number_buffers {
        // Safety: flexible-array entry is within number_buffers.
        let buffer = unsafe { &mut *first.add(index) };
        if !buffer.data.is_null() {
            // Safety: Core Audio declares data_byte_size writable bytes.
            unsafe {
                ptr::write_bytes(buffer.data.cast::<u8>(), 0, buffer.data_byte_size as usize)
            };
        }
    }
}

fn configure_stream_usage(
    context: &DeviceIoContext,
    scope: u32,
    enabled: bool,
) -> Result<(), CoreAudioError> {
    let streams = get_u32_array(context.device_id, DEVICE_STREAMS, scope)?;
    if streams.is_empty() {
        return Ok(());
    }
    let offset = std::mem::offset_of!(AudioHardwareIoProcStreamUsageHeader, first_stream_is_on);
    let size = offset
        .checked_add(streams.len() * size_of::<u32>())
        .ok_or(CoreAudioError::InvalidProperty("IOProc stream usage size"))?;
    let words = size.div_ceil(size_of::<usize>());
    let mut storage = vec![0_usize; words];
    let header = storage
        .as_mut_ptr()
        .cast::<AudioHardwareIoProcStreamUsageHeader>();
    // Safety: storage is suitably aligned and large enough for the flexible structure.
    unsafe {
        (*header).io_proc = context.procedure_id;
        (*header).number_streams = streams.len() as u32;
        let first = &raw mut (*header).first_stream_is_on;
        for index in 0..streams.len() {
            *first.add(index) = u32::from(enabled);
        }
    }
    let property = address(DEVICE_IOPROC_STREAM_USAGE, scope);
    // Safety: storage contains a complete AudioHardwareIOProcStreamUsage value.
    status(unsafe {
        AudioObjectSetPropertyData(
            context.device_id,
            &property,
            0,
            ptr::null(),
            size as u32,
            storage.as_ptr().cast(),
        )
    })
}

pub fn list_devices() -> Result<Vec<AudioDevice>, CoreAudioError> {
    let ids = get_u32_array(SYSTEM_OBJECT, HARDWARE_DEVICES, SCOPE_GLOBAL)?;
    ids.into_iter()
        .map(|object_id| {
            let uid = get_cf_string(object_id, DEVICE_UID, SCOPE_GLOBAL)?;
            Ok(AudioDevice {
                object_id,
                is_vcable: uid.starts_with(VCABLE_UID_PREFIX),
                is_virtual: get_u32(object_id, DEVICE_TRANSPORT_TYPE, SCOPE_GLOBAL)?
                    == TRANSPORT_TYPE_VIRTUAL,
                uid,
                name: get_cf_string(object_id, OBJECT_NAME, SCOPE_GLOBAL)?,
                input_channels: get_channel_count(object_id, SCOPE_INPUT)?,
                output_channels: get_channel_count(object_id, SCOPE_OUTPUT)?,
                sample_rate: get_f64(object_id, DEVICE_SAMPLE_RATE, SCOPE_GLOBAL)?.round() as u32,
            })
        })
        .collect()
}

pub fn create_virtual_device(
    id: &str,
    name: &str,
    input_channels: u32,
    output_channels: u32,
    sample_rate: u32,
) -> Result<(), CoreAudioError> {
    validate_field(id, "device id")?;
    validate_field(name, "device name")?;
    if input_channels == 0 || output_channels == 0 {
        return Err(CoreAudioError::DriverRejected(
            "channel counts must be greater than zero".to_owned(),
        ));
    }
    let command =
        format!("create\t{id}\t{name}\t{input_channels}\t{output_channels}\t{sample_rate}");
    send_driver_command(&command)?;
    wait_for_device_visibility(&format!("{VCABLE_UID_PREFIX}{id}"), true)
}

pub fn delete_virtual_device(id: &str) -> Result<(), CoreAudioError> {
    validate_field(id, "device id")?;
    send_driver_command(&format!("delete\t{id}"))?;
    wait_for_device_visibility(&format!("{VCABLE_UID_PREFIX}{id}"), false)
}

fn wait_for_device_visibility(uid: &str, expected_present: bool) -> Result<(), CoreAudioError> {
    let deadline = Instant::now() + DEVICE_CHANGE_TIMEOUT;
    loop {
        match list_devices() {
            Ok(devices) => {
                let present = devices.iter().any(|device| device.uid == uid);
                if present == expected_present {
                    return Ok(());
                }
            }
            // The HAL can retire an object between fetching the device ID list and reading
            // that object's properties. Retry only this documented transition condition.
            Err(CoreAudioError::OsStatus(BAD_OBJECT_STATUS)) => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Err(CoreAudioError::DeviceChangeTimeout {
                uid: uid.to_owned(),
                expected_present,
            });
        }
        thread::sleep(DEVICE_CHANGE_POLL_INTERVAL);
    }
}

fn validate_field(value: &str, name: &'static str) -> Result<(), CoreAudioError> {
    if value.is_empty() || value.contains(['\t', '\r', '\n', '\0']) {
        return Err(CoreAudioError::InvalidProperty(name));
    }
    Ok(())
}

fn send_driver_command(command: &str) -> Result<(), CoreAudioError> {
    let plugin_id = find_vcable_plugin()?;
    let command = OwnedCfString::create(command)?;
    let address = address(VCABLE_CONTROL, SCOPE_GLOBAL);
    let reference = command.0;
    // Safety: reference points to a valid CFString for the duration of the call.
    status(unsafe {
        AudioObjectSetPropertyData(
            plugin_id,
            &address,
            0,
            ptr::null(),
            size_of::<CfStringRef>() as u32,
            (&raw const reference).cast(),
        )
    })
}

fn find_vcable_plugin() -> Result<AudioObjectId, CoreAudioError> {
    for plugin_id in get_u32_array(SYSTEM_OBJECT, HARDWARE_PLUGIN_LIST, SCOPE_GLOBAL)? {
        if get_cf_string(plugin_id, PLUGIN_BUNDLE_ID, SCOPE_GLOBAL)
            .ok()
            .as_deref()
            == Some(VCABLE_BUNDLE_ID)
        {
            return Ok(plugin_id);
        }
    }
    Err(CoreAudioError::DriverNotInstalled)
}

fn get_u32_array(
    object_id: AudioObjectId,
    selector: u32,
    scope: u32,
) -> Result<Vec<u32>, CoreAudioError> {
    let property = get_property_bytes(object_id, selector, scope)?;
    if property.len() % size_of::<u32>() != 0 {
        return Err(CoreAudioError::InvalidProperty("AudioObjectID array"));
    }
    let mut values = Vec::with_capacity(property.len() / size_of::<u32>());
    for bytes in property.chunks_exact(size_of::<u32>()) {
        values.push(u32::from_ne_bytes(
            bytes.try_into().expect("chunk size is exact"),
        ));
    }
    Ok(values)
}

fn get_f64(object_id: AudioObjectId, selector: u32, scope: u32) -> Result<f64, CoreAudioError> {
    let property = get_property_bytes(object_id, selector, scope)?;
    let bytes: [u8; size_of::<f64>()] = property
        .try_into()
        .map_err(|_| CoreAudioError::InvalidProperty("Float64"))?;
    Ok(f64::from_ne_bytes(bytes))
}

fn get_u32(object_id: AudioObjectId, selector: u32, scope: u32) -> Result<u32, CoreAudioError> {
    let property = get_property_bytes(object_id, selector, scope)?;
    let bytes: [u8; size_of::<u32>()] = property
        .try_into()
        .map_err(|_| CoreAudioError::InvalidProperty("UInt32"))?;
    Ok(u32::from_ne_bytes(bytes))
}

fn get_cf_string(
    object_id: AudioObjectId,
    selector: u32,
    scope: u32,
) -> Result<String, CoreAudioError> {
    let address = address(selector, scope);
    let mut value: CfStringRef = ptr::null();
    let mut size = size_of::<CfStringRef>() as u32;
    // Safety: value has enough space for a CFStringRef and size describes it.
    status(unsafe {
        AudioObjectGetPropertyData(
            object_id,
            &address,
            0,
            ptr::null(),
            &mut size,
            (&raw mut value).cast(),
        )
    })?;
    if value.is_null() {
        return Err(CoreAudioError::InvalidProperty("CFString"));
    }
    let result = cf_string_to_rust(value);
    // Core Audio string properties return retained CF objects.
    // Safety: value is non-null and owned by the caller.
    unsafe { CFRelease(value) };
    result
}

fn cf_string_to_rust(value: CfStringRef) -> Result<String, CoreAudioError> {
    // Safety: value is a valid CFStringRef supplied by Core Audio.
    let length = unsafe { CFStringGetLength(value) };
    // Safety: pure size calculation for the valid string.
    let capacity = unsafe { CFStringGetMaximumSizeForEncoding(length, UTF8_ENCODING) }
        .checked_add(1)
        .ok_or(CoreAudioError::InvalidProperty("CFString length"))?;
    let mut buffer = vec![0_u8; capacity as usize];
    // Safety: buffer is writable for capacity bytes and value is valid.
    if !unsafe { CFStringGetCString(value, buffer.as_mut_ptr().cast(), capacity, UTF8_ENCODING) } {
        return Err(CoreAudioError::InvalidUtf8);
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8(buffer[..end].to_vec()).map_err(|_| CoreAudioError::InvalidUtf8)
}

fn get_channel_count(object_id: AudioObjectId, scope: u32) -> Result<u32, CoreAudioError> {
    let property = get_property_bytes(object_id, DEVICE_STREAM_CONFIGURATION, scope)?;
    if property.len() < size_of::<u32>() {
        return Err(CoreAudioError::InvalidProperty("AudioBufferList"));
    }
    let header = property.as_ptr().cast::<AudioBufferListHeader>();
    // Safety: get_property_bytes returned the exact Core Audio structure and
    // the size was checked above.
    let number_buffers = unsafe { (*header).number_buffers as usize };
    let required = std::mem::offset_of!(AudioBufferListHeader, first_buffer)
        .checked_add(number_buffers.saturating_mul(size_of::<AudioBuffer>()))
        .ok_or(CoreAudioError::InvalidProperty("AudioBufferList size"))?;
    if property.len() < required {
        return Err(CoreAudioError::InvalidProperty("AudioBufferList buffers"));
    }
    // AudioBufferList inserts target-specific alignment padding before mBuffers.
    let first = unsafe {
        property
            .as_ptr()
            .add(std::mem::offset_of!(AudioBufferListHeader, first_buffer))
            .cast::<AudioBuffer>()
    };
    let mut channels = 0_u32;
    for index in 0..number_buffers {
        // Safety: required size above covers every indexed AudioBuffer.
        channels = channels.saturating_add(unsafe { (*first.add(index)).number_channels });
    }
    Ok(channels)
}

fn get_property_bytes(
    object_id: AudioObjectId,
    selector: u32,
    scope: u32,
) -> Result<Vec<u8>, CoreAudioError> {
    let address = address(selector, scope);
    let mut size = 0_u32;
    // Safety: size is a valid output pointer and there is no qualifier.
    status(unsafe {
        AudioObjectGetPropertyDataSize(object_id, &address, 0, ptr::null(), &mut size)
    })?;
    let mut result = vec![0_u8; size as usize];
    // Safety: result is writable for size bytes.
    status(unsafe {
        AudioObjectGetPropertyData(
            object_id,
            &address,
            0,
            ptr::null(),
            &mut size,
            result.as_mut_ptr().cast(),
        )
    })?;
    result.truncate(size as usize);
    Ok(result)
}

const fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        selector,
        scope,
        element: ELEMENT_MAIN,
    }
}

fn status(value: OsStatus) -> Result<(), CoreAudioError> {
    if value == 0 {
        Ok(())
    } else {
        Err(CoreAudioError::OsStatus(value))
    }
}
