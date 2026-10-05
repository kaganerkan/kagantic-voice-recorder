//! Simulate a noisy real-time scenario: cpal callback fires in 480-sample
//! chunks at 48 kHz (10 ms each), the worker thread drains and encodes with
//! simulated CPU contention. Verify the resulting Ogg file has strictly
//! monotonic granule positions with no skipped frames.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use kvr_recorder::audio::{
    current_level_blocking, discard_partial_blocking, try_take_frame_blocking, Accum,
};
use kvr_recorder::encoder::{build_tags, write_opus_head, OpusStreamEncoder};
use kvr_recorder::ogg::OggWriter;

#[test]
fn real_time_pipeline_produces_continuous_granules() {
    // Unique, RAII-cleaned working dir: safe under parallel runs and
    // Windows file-sharing rules.
    let tmpdir = tempfile::tempdir().expect("create temp dir");
    let tmp = tmpdir.path().join("kvr-nogaps.opus");

    let fmt = kvr_recorder::audio::CaptureFormat {
        sample_rate: 48_000,
        channels: 1,
    };
    let mut encoder = OpusStreamEncoder::new(&fmt, 64_000).expect("encoder init");

    let file = std::fs::File::create(&tmp).expect("create file");
    let mut w = OggWriter::new(file);
    w.write_bos(&write_opus_head(&fmt)).expect("write BOS");
    w.write_audio(&build_tags(), 0).expect("write OpusTags");

    let accum: Accum = Arc::new(Mutex::new(Vec::with_capacity(8192)));

    // Simulated producer: pushes 480-sample chunks (10 ms @ 48 kHz) every 10 ms.
    let producer_accum = accum.clone();
    let producer = std::thread::spawn(move || {
        let mut t: f32 = 0.0;
        let dt = 1.0 / 48_000.0;
        // 100 chunks * 10 ms = 1 second of audio
        for _ in 0..100 {
            let mut chunk = Vec::with_capacity(480);
            for _ in 0..480 {
                chunk.push(0.3 * (2.0 * std::f64::consts::PI as f32 * 1000.0 * t).sin());
                t += dt;
            }
            {
                let mut b = producer_accum.lock();
                b.extend_from_slice(&chunk);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });

    let mut frames_encoded = 0u64;
    let mut last_granule = 0u64;
    let mut last_level_report = std::time::Instant::now();
    loop {
        while let Some(frame) = try_take_frame_blocking(&accum, &encoder) {
            let packet = encoder.encode_f32(&frame).expect("encode");
            let granule = (frames_encoded + 1) * encoder.frame_size() as u64;
            assert_eq!(
                granule,
                last_granule + encoder.frame_size() as u64,
                "granule position must increment by exactly one frame"
            );
            w.write_audio(&packet, granule).expect("write");
            frames_encoded += 1;
            last_granule = granule;
        }
        if last_level_report.elapsed() >= Duration::from_millis(80) {
            let _ = current_level_blocking(&accum);
            last_level_report = std::time::Instant::now();
        }
        if frames_encoded >= 50 {
            discard_partial_blocking(&accum);
            break;
        }
        // Simulate CPU contention: occasionally sleep 15 ms (>1 frame).
        if frames_encoded.is_multiple_of(10) {
            std::thread::sleep(Duration::from_millis(15));
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    w.write_eos(&[], last_granule).expect("write EOS");
    // Note: the EOS page carries `last_granule` (the position of the final
    // audio frame), so the EOS page itself shares a granule with the last
    // audio page. That's correct per RFC 3533 — the EOS flag tells the
    // decoder it's the final page.
    let file = w.finish().expect("finish");
    let _ = file.sync_all();
    // Close the handle before re-reading the file (Windows file sharing).
    drop(file);

    producer.join().expect("producer join");

    // Validate the file: every granule should increment by exactly 960.
    let bytes = std::fs::read(&tmp).expect("read back");
    let mut off = 0usize;
    let mut prev_granule: u64 = 0;
    let mut audio_pages = 0u32;
    while off + 27 <= bytes.len() {
        assert_eq!(&bytes[off..off + 4], b"OggS");
        let nseg = bytes[off + 26] as usize;
        let seg = &bytes[off + 27..off + 27 + nseg];
        let plen: usize = seg.iter().map(|&s| s as usize).sum();
        let granule = u64::from_le_bytes(bytes[off + 6..off + 14].try_into().unwrap());
        let header_type = bytes[off + 5];
        let is_eos = header_type & 0x04 != 0;
        if granule > 0 {
            // EOS page intentionally shares the final audio granule; skip
            // it from the monotonicity check.
            if !is_eos {
                if audio_pages > 0 {
                    assert_eq!(
                        granule,
                        prev_granule + 960,
                        "audio page {} has gap (prev={} curr={})",
                        audio_pages,
                        prev_granule,
                        granule
                    );
                }
                prev_granule = granule;
            }
            audio_pages += 1;
        }
        off += 27 + nseg + plen;
    }
    assert_eq!(off, bytes.len(), "trailing bytes after last page");
    assert!(
        audio_pages >= 50,
        "expected at least 50 audio pages, got {}",
        audio_pages
    );

    // Sanity: ffprobe accepts the file.
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "default=nw=1",
            tmp.to_str().unwrap(),
        ])
        .output()
        .expect("ffprobe");
    assert!(
        out.status.success(),
        "ffprobe rejected the file: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("codec_name=opus"));
}
