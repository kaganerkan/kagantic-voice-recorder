//! End-to-end integration test:
//!   synthesize a 1 kHz sine wave → OpusStreamEncoder → OggWriter → .opus file
//!
//! Verifies the produced file:
//!   1. Has a valid OggS capture pattern in the first 4 bytes.
//!   2. Contains the OpusHead identification packet (19 bytes, starts with "OpusHead").
//!   3. Contains the OpusTags comment packet.
//!   4. Page sequence numbers monotonically increase across the file.
//!   5. ffprobe recognises it as Opus (codec_name=opus).

use kvr_recorder::audio::CaptureFormat;
use kvr_recorder::encoder::{build_tags, write_opus_head, OpusStreamEncoder};
use kvr_recorder::ogg::OggWriter;

fn synth_sine(seconds: f64, freq: f64, sr: u32) -> Vec<f32> {
    let n = (seconds * sr as f64) as usize;
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / sr as f64;
        v.push(0.3 * (2.0 * std::f64::consts::PI * freq * t).sin() as f32);
    }
    v
}

#[test]
fn opus_file_is_valid_ogg_opus() {
    // Unique, RAII-cleaned working dir: safe under parallel runs and
    // Windows file-sharing rules.
    let tmpdir = tempfile::tempdir().expect("create temp dir");
    let tmp = tmpdir.path().join("kvr-itest.opus");

    let fmt = CaptureFormat {
        sample_rate: 48_000,
        channels: 1,
    };
    let mut enc = OpusStreamEncoder::new(&fmt, 32_000).expect("encoder init");
    let file = std::fs::File::create(&tmp).expect("create file");
    // Use a single OggWriter across all pages so page_seq increments.
    let mut w = OggWriter::new(file);
    w.write_bos(&write_opus_head(&fmt)).expect("write BOS");
    w.write_audio(&build_tags(), 0).expect("write OpusTags");

    // Synthesize 0.5 s of 1 kHz mono audio → 24 frames × 960 samples.
    let sr = 48_000u32;
    let samples = synth_sine(0.5, 1000.0, sr);
    assert_eq!(samples.len(), (0.5 * sr as f64) as usize);

    let mut frame = vec![0.0f32; 960];
    let mut granule: u64 = 0;
    for (frame_count, chunk) in samples.chunks(960).enumerate() {
        frame[..chunk.len()].copy_from_slice(chunk);
        for s in &mut frame[chunk.len()..] {
            *s = 0.0;
        }
        let packet = enc.encode_f32(&frame).expect("encode");
        granule = (frame_count as u64 + 1) * 960;
        w.write_audio(&packet, granule).expect("write audio page");
    }
    w.write_eos(&[], granule).expect("write EOS");
    let file = w.finish().expect("finish OggWriter");
    let _ = file.sync_all();
    // Close the handle before re-reading / handing the path to ffprobe/ffmpeg
    // (Windows file sharing).
    drop(file);

    let bytes = std::fs::read(&tmp).expect("read back");

    // 1) Magic.
    assert_eq!(&bytes[0..4], b"OggS");

    // 2) OpusHead in first packet.
    let pos = bytes
        .windows(8)
        .position(|w| w == b"OpusHead")
        .expect("OpusHead present");
    assert_eq!(pos, 28, "OpusHead BOS payload starts at byte 28 (27-byte header + 1-byte segment table for an OpusHead-sized packet)");
    assert_eq!(bytes[pos + 8], 1); // version
    assert_eq!(bytes[pos + 9], 1); // channels
    let pre_skip = u16::from_le_bytes(bytes[pos + 10..pos + 12].try_into().unwrap());
    assert_eq!(pre_skip, 312);
    let in_rate = u32::from_le_bytes(bytes[pos + 12..pos + 16].try_into().unwrap());
    assert_eq!(in_rate, 48_000);

    // 3) OpusTags comment packet present.
    assert!(
        bytes.windows(8).any(|w| w == b"OpusTags"),
        "OpusTags comment packet must be present"
    );

    // 4) Page sequence numbers monotonically increase.
    let mut last_seq: Option<u32> = None;
    let mut off = 0usize;
    while off + 27 <= bytes.len() {
        assert_eq!(&bytes[off..off + 4], b"OggS");
        let seq = u32::from_le_bytes(bytes[off + 18..off + 22].try_into().unwrap());
        if let Some(prev) = last_seq {
            assert_eq!(seq, prev + 1, "page seq must increment by 1");
        }
        last_seq = Some(seq);
        let nseg = bytes[off + 26] as usize;
        let seg_table = &bytes[off + 27..off + 27 + nseg];
        let payload_len: usize = seg_table.iter().map(|&s| s as usize).sum();
        off += 27 + nseg + payload_len;
    }
    assert_eq!(off, bytes.len(), "trailing garbage or short read");

    // 5) ffprobe round-trip.
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_name,codec_type,sample_rate,channels",
            "-of",
            "default=nw=1",
            tmp.to_str().unwrap(),
        ])
        .output()
        .expect("ffprobe");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("ffprobe stdout:\n{}", stdout);
    if !out.status.success() {
        eprintln!("ffprobe stderr:\n{}", stderr);
    }
    assert!(
        out.status.success(),
        "ffprobe failed to parse the produced .opus"
    );
    assert!(
        stdout.contains("codec_name=opus"),
        "ffprobe did not recognise Opus: {}",
        stdout
    );
    assert!(stdout.contains("codec_type=audio"));

    // 6) ffmpeg decodes the file to PCM.
    let wav = tmpdir.path().join("kvr-itest.wav");
    let out2 = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-i",
            tmp.to_str().unwrap(),
            "-f",
            "wav",
            wav.to_str().unwrap(),
        ])
        .output()
        .expect("ffmpeg");
    assert!(
        out2.status.success(),
        "ffmpeg decode failed: {}",
        String::from_utf8_lossy(&out2.stderr)
    );
}
