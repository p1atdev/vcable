#![cfg(target_os = "macos")]

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use vcable_coreaudio::{
    PcmCaptureStream, PcmFormat, PcmPlaybackStream, PcmReader, PcmStreamConfig, PcmWriter,
    create_virtual_device, delete_virtual_device,
};

const CHANNELS: usize = 2;
const UID_PREFIX: &str = "dev.vcable.device.";
static NEXT_DEVICE_ID: AtomicU64 = AtomicU64::new(0);

struct TemporaryDevice {
    id: String,
    uid: String,
}

fn with_temporary_devices(test_name: &str, count: usize, test: impl FnOnce(&[TemporaryDevice])) {
    let mut devices = Vec::with_capacity(count);
    for index in 0..count {
        let serial = NEXT_DEVICE_ID.fetch_add(1, Ordering::Relaxed);
        let id = format!("pcm-test-{}-{serial}-{index}", std::process::id());
        let name = format!("VCable PCM test {test_name} {index}");
        if let Err(error) = create_virtual_device(&id, &name, 2, 2, 48_000) {
            let cleanup_errors = delete_temporary_devices(&devices);
            panic!(
                "failed to create temporary device {id}: {error}; cleanup errors: {cleanup_errors:?}"
            );
        }
        devices.push(TemporaryDevice {
            uid: format!("{UID_PREFIX}{id}"),
            id,
        });
    }

    let test_result = catch_unwind(AssertUnwindSafe(|| test(&devices)));
    let cleanup_errors = delete_temporary_devices(&devices);
    assert!(
        cleanup_errors.is_empty(),
        "temporary VCable device cleanup failed: {cleanup_errors:?}; test panicked: {}",
        test_result.is_err()
    );
    if let Err(payload) = test_result {
        resume_unwind(payload);
    }
}

fn delete_temporary_devices(devices: &[TemporaryDevice]) -> Vec<String> {
    let mut errors = Vec::new();
    for device in devices.iter().rev() {
        if let Err(error) = delete_virtual_device(&device.id) {
            errors.push(format!("{}: {error}", device.id));
        }
    }
    errors
}

fn config(uid: &str) -> PcmStreamConfig {
    PcmStreamConfig {
        device_uid: uid.to_owned(),
        client_format: PcmFormat {
            sample_rate: 48_000,
            channels: CHANNELS as u32,
        },
        capacity_frames: 16_384,
        target_fill_frames: 960,
        // The exact loopback assertions isolate transport correctness from the
        // separately unit-tested adaptive resampler.
        max_drift_ppm: 0,
    }
}

fn waveform(seed: u32) -> Vec<f32> {
    let mut result = Vec::with_capacity(2_050 * CHANNELS);
    let start = [
        0.8125 - seed as f32 * 0.03125,
        -0.6875 + seed as f32 * 0.03125,
    ];
    result.extend_from_slice(&start);
    for frame in 0..2_048_u32 {
        let left = ((frame.wrapping_mul(37).wrapping_add(seed * 101)) % 997) as f32 / 997.0;
        let right = ((frame.wrapping_mul(83).wrapping_add(seed * 211)) % 991) as f32 / 991.0;
        result.push(left * 1.5 - 0.75);
        result.push(right * 1.25 - 0.625);
    }
    result.extend_from_slice(&[-0.90625 + seed as f32 * 0.015625, 0.84375]);
    result
}

fn write_waveform(writer: &mut PcmWriter, expected: &[f32]) {
    let mut with_resampler_lookahead = Vec::with_capacity(expected.len() + CHANNELS * 2);
    with_resampler_lookahead.extend_from_slice(expected);
    with_resampler_lookahead.extend_from_slice(&[0.0; CHANNELS * 2]);
    writer
        .try_write_interleaved(&with_resampler_lookahead)
        .unwrap();
}

fn read_until_waveform(reader: &mut PcmReader, expected: &[f32]) -> Vec<f32> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut captured = Vec::new();
    let mut chunk = [0.0_f32; 960 * CHANNELS];
    while Instant::now() < deadline {
        let frames = reader.try_read_interleaved(&mut chunk).unwrap();
        captured.extend_from_slice(&chunk[..frames * CHANNELS]);
        if let Some(start) = find_stereo_frame(&captured, &expected[..CHANNELS])
            && captured.len() >= start + expected.len()
        {
            return captured[start..start + expected.len()].to_vec();
        }
        thread::sleep(Duration::from_millis(2));
    }
    panic!("timed out waiting for the looped-back waveform");
}

fn find_stereo_frame(haystack: &[f32], marker: &[f32]) -> Option<usize> {
    haystack
        .chunks_exact(CHANNELS)
        .position(|frame| frame == marker)
        .map(|frame| frame * CHANNELS)
}

fn assert_waveform(actual: &[f32], expected: &[f32]) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "loopback sample count changed"
    );
    for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        assert!(
            (actual - expected).abs() <= 1.0e-6,
            "sample {index} changed: expected {expected}, got {actual}"
        );
    }
}

#[test]
#[ignore = "requires the installed VCable HAL driver"]
fn playback_to_capture_preserves_waveform_channels_and_sample_count() {
    with_temporary_devices("loopback", 1, |devices| {
        let uid = &devices[0].uid;
        let (capture, mut reader) = PcmCaptureStream::start(config(uid)).unwrap();
        let (playback, mut writer) = PcmPlaybackStream::start(config(uid)).unwrap();
        let expected = waveform(0);

        write_waveform(&mut writer, &expected);
        let actual = read_until_waveform(&mut reader, &expected);
        assert_waveform(&actual, &expected);

        assert!(playback.metrics().callbacks > 0);
        assert!(capture.metrics().callbacks > 0);
    });
}

#[test]
#[ignore = "requires the installed VCable HAL driver"]
fn two_simultaneous_bidirectional_devices_do_not_interfere() {
    with_temporary_devices("isolation", 2, |devices| {
        let first_uid = &devices[0].uid;
        let second_uid = &devices[1].uid;
        let (first_capture, mut first_reader) = PcmCaptureStream::start(config(first_uid)).unwrap();
        let (second_capture, mut second_reader) =
            PcmCaptureStream::start(config(second_uid)).unwrap();
        let (first_playback, mut first_writer) =
            PcmPlaybackStream::start(config(first_uid)).unwrap();
        let (second_playback, mut second_writer) =
            PcmPlaybackStream::start(config(second_uid)).unwrap();
        let first_expected = waveform(1);
        let second_expected = waveform(2);

        write_waveform(&mut first_writer, &first_expected);
        write_waveform(&mut second_writer, &second_expected);
        let first_actual = read_until_waveform(&mut first_reader, &first_expected);
        let second_actual = read_until_waveform(&mut second_reader, &second_expected);
        assert_waveform(&first_actual, &first_expected);
        assert_waveform(&second_actual, &second_expected);
        assert_ne!(first_actual, second_actual);

        for metrics in [
            first_capture.metrics(),
            second_capture.metrics(),
            first_playback.metrics(),
            second_playback.metrics(),
        ] {
            assert_eq!(metrics.format_errors, 0);
        }
    });
}
