//! Hardware-free production drain/sink checks. Requires ffmpeg/ffprobe on PATH.
//! Nonzero stereo at 44.1 kHz, irregular callback batches, and a partial final
//! block exercise resampling, channel separation, exact duration, and EOS.
use kvr_recorder::audio::CaptureFormat;
use kvr_recorder::output::OutputFormat;
use kvr_recorder::writers::{drain_capture, open_sink};
use std::process::Command;
use std::sync::Arc;

fn samples(frames: usize) -> Vec<f32> {
    (0..frames)
        .flat_map(|index| {
            let phase = index as f32 * std::f32::consts::TAU / 44_100.0;
            [0.25 * (phase * 440.0).sin(), 0.25 * (phase * 880.0).sin()]
        })
        .collect()
}

fn correlation(decoded: &[f32], channel: usize, frequency: f64, rate: u32) -> f64 {
    let mut sine = 0.0;
    let mut cosine = 0.0;
    let count = decoded.len() / 2;
    for (index, frame) in decoded.as_chunks::<2>().0.iter().enumerate() {
        let phase = index as f64 * std::f64::consts::TAU * frequency / rate as f64;
        sine += frame[channel] as f64 * phase.sin();
        cosine += frame[channel] as f64 * phase.cos();
    }
    2.0 * sine.hypot(cosine) / count as f64
}

#[test]
fn production_drain_sinks_preserve_signal_clock_channels_bytes_and_final_tail() {
    let frames = 22_067usize; // deliberately not a complete 10 ms / 20 ms frame
    let input = samples(frames);
    for format in [
        OutputFormat::Opus,
        OutputFormat::WavPcm16le,
        OutputFormat::RawF32le,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory
            .path()
            .join(format!("take.{}", format.default_ext()));
        let fmt = CaptureFormat {
            sample_rate: 44_100,
            channels: 2,
        };
        let mut sink = open_sink(&path, &fmt, format, 96_000).unwrap();
        let accum = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut buffer = Vec::new();
        let mut consumed = 0;
        for callback in input.chunks(254) {
            accum.lock().extend_from_slice(callback);
            consumed += drain_capture(&accum, sink.as_mut(), &fmt, false, &mut buffer)
                .unwrap()
                .0;
            assert_eq!(
                sink.bytes_written(),
                std::fs::metadata(&path).unwrap().len()
            );
        }
        consumed += drain_capture(&accum, sink.as_mut(), &fmt, true, &mut buffer)
            .unwrap()
            .0;
        assert_eq!(consumed, input.len() as u64);
        sink.finalize().unwrap();
        let final_bytes = sink.bytes_written();
        assert_eq!(final_bytes, std::fs::metadata(&path).unwrap().len());
        sink.finalize().unwrap();
        assert_eq!(
            sink.bytes_written(),
            final_bytes,
            "finalize must not append a second EOS"
        );
        drop(sink);

        let mut decode = Command::new("ffmpeg");
        decode.args(["-v", "error"]);
        if format == OutputFormat::RawF32le {
            decode.args(["-f", "f32le", "-ar", "44100", "-ac", "2"]);
        }
        let result = decode
            .arg("-i")
            .arg(&path)
            .args(["-f", "f32le", "-acodec", "pcm_f32le", "pipe:1"])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            result.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let decoded: Vec<f32> = result
            .stdout
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| f32::from_le_bytes(*bytes))
            .collect();
        let (rate, expected_frames) = if format == OutputFormat::Opus {
            (48_000, (frames as u64 * 48_000).div_ceil(44_100) as usize)
        } else {
            (44_100, frames)
        };
        assert_eq!(
            decoded.len(),
            expected_frames * 2,
            "decoded duration differs for {format}"
        );
        for (channel, expected, unwanted) in [(0, 440.0, 880.0), (1, 880.0, 440.0)] {
            assert!(
                correlation(&decoded, channel, expected, rate) > 0.20,
                "signal missing on {format} channel {channel}"
            );
            assert!(
                correlation(&decoded, channel, unwanted, rate) < 0.02,
                "channels mixed on {format} channel {channel}"
            );
        }
        if format == OutputFormat::RawF32le {
            assert_eq!(
                decoded, input,
                "raw capture must be bit-exact at the native clock"
            );
        } else {
            let probe = Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "stream=codec_name,sample_rate,channels:format=duration",
                    "-of",
                    "json",
                ])
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                probe.status.success(),
                "{}",
                String::from_utf8_lossy(&probe.stderr)
            );
            let metadata: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
            let stream = &metadata["streams"][0];
            assert_eq!(stream["channels"], 2);
            assert_eq!(stream["sample_rate"], rate.to_string());
            let expected_codec = if format == OutputFormat::Opus {
                "opus"
            } else {
                "pcm_s16le"
            };
            assert_eq!(stream["codec_name"], expected_codec);
            let reported: f64 = metadata["format"]["duration"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap();
            let pre_skip = if format == OutputFormat::Opus { 312 } else { 0 };
            let container_duration = (expected_frames + pre_skip) as f64 / rate as f64;
            assert!((reported - container_duration).abs() < 0.00001);
            eprintln!("{format}: bytes={final_bytes}, decoded_frames={expected_frames}, rate={rate}, channels=2, container_duration={reported:.6}");
            if format == OutputFormat::Opus {
                check_ogg(&std::fs::read(&path).unwrap(), expected_frames as u64 + 312);
            }
        }
    }
}

fn check_ogg(bytes: &[u8], final_granule: u64) {
    let mut offset = 0;
    let mut sequence = 0;
    while offset < bytes.len() {
        assert_eq!(&bytes[offset..offset + 4], b"OggS");
        let segments = bytes[offset + 26] as usize;
        let payload: usize = bytes[offset + 27..offset + 27 + segments]
            .iter()
            .map(|&byte| byte as usize)
            .sum();
        let end = offset + 27 + segments + payload;
        let expected_crc = u32::from_le_bytes(bytes[offset + 22..offset + 26].try_into().unwrap());
        let mut crc = 0u32;
        for (index, &byte) in bytes[offset..end].iter().enumerate() {
            crc ^= (if (22..26).contains(&index) { 0 } else { byte } as u32) << 24;
            for _ in 0..8 {
                crc = (crc << 1)
                    ^ if crc & 0x8000_0000 != 0 {
                        0x04C1_1DB7
                    } else {
                        0
                    };
            }
        }
        assert_eq!(crc, expected_crc, "Ogg CRC mismatch");
        assert_eq!(
            u32::from_le_bytes(bytes[offset + 18..offset + 22].try_into().unwrap()),
            sequence
        );
        if end == bytes.len() {
            assert_ne!(bytes[offset + 5] & 4, 0, "final page must have EOS");
            assert_eq!(
                u64::from_le_bytes(bytes[offset + 6..offset + 14].try_into().unwrap()),
                final_granule
            );
            assert!(
                payload > 0,
                "EOS must carry the final audio packet for end trimming"
            );
        }
        sequence += 1;
        offset = end;
    }
    assert_eq!(offset, bytes.len());
}

#[test]
fn sink_creation_never_truncates_an_existing_take() {
    for format in [
        OutputFormat::Opus,
        OutputFormat::WavPcm16le,
        OutputFormat::RawF32le,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("existing");
        std::fs::write(&path, b"preserve this recording").unwrap();
        assert!(open_sink(
            &path,
            &CaptureFormat {
                sample_rate: 44_100,
                channels: 1
            },
            format,
            96_000
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"preserve this recording");
    }
}
