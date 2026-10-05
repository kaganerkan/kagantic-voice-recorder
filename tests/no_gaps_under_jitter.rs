//! Worst-case stress: producer stalls for 200 ms (~10 frames) mid-stream
//! while the worker continues encoding. Verifies the pipeline tolerates
//! long stalls without dropping frames when the producer catches up.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use kvr_recorder::audio::{
    current_level_blocking, discard_partial_blocking, try_take_frame_blocking, Accum,
};
use kvr_recorder::encoder::{build_tags, write_opus_head, OpusStreamEncoder};
use kvr_recorder::ogg::OggWriter;

#[test]
fn stalls_do_not_drop_frames() {
    // Unique, RAII-cleaned working dir: safe under parallel runs and
    // Windows file-sharing rules.
    let tmpdir = tempfile::tempdir().expect("create temp dir");
    let tmp = tmpdir.path().join("kvr-jitter.opus");

    let fmt = kvr_recorder::audio::CaptureFormat {
        sample_rate: 48_000,
        channels: 1,
    };
    let mut encoder = OpusStreamEncoder::new(&fmt, 64_000).expect("encoder init");

    let file = std::fs::File::create(&tmp).expect("create file");
    let mut w = OggWriter::new(file);
    w.write_bos(&write_opus_head(&fmt)).expect("write BOS");
    w.write_audio(&build_tags(), 0).expect("write OpusTags");

    let accum: Accum = Arc::new(Mutex::new(Vec::with_capacity(16384)));

    let producer_accum = accum.clone();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let producer = std::thread::spawn(move || {
        // Phase 1: 50 chunks (500 ms) at 10 ms cadence.
        let mut t: f32 = 0.0;
        let dt = 1.0 / 48_000.0;
        for _ in 0..50 {
            push_chunk(&producer_accum, &mut t, 480, dt);
            std::thread::sleep(Duration::from_millis(10));
        }
        // Phase 2: 200 ms stall (no producer activity).
        std::thread::sleep(Duration::from_millis(200));
        // Phase 3: 50 more chunks (500 ms) — accumulator now has a backlog.
        for _ in 0..50 {
            push_chunk(&producer_accum, &mut t, 480, dt);
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = done_tx.send(());
    });

    let mut frames_encoded = 0u64;
    let mut last_granule = 0u64;
    let timeout = Duration::from_secs(15);
    let deadline = std::time::Instant::now() + timeout;

    loop {
        // Drain all available frames.
        while let Some(frame) = try_take_frame_blocking(&accum, &encoder) {
            let packet = encoder.encode_f32(&frame).expect("encode");
            let granule = (frames_encoded + 1) * encoder.frame_size() as u64;
            w.write_audio(&packet, granule).expect("write");
            frames_encoded += 1;
            last_granule = granule;
            let _ = current_level_blocking(&accum);
        }

        // Done. Stop sending, or wait for a producer tick before breaking.
        if done_rx.try_recv().is_ok() {
            // Producer is done. Drain any final samples before stopping.
            while let Some(frame) = try_take_frame_blocking(&accum, &encoder) {
                let packet = encoder.encode_f32(&frame).expect("encode");
                let granule = (frames_encoded + 1) * encoder.frame_size() as u64;
                w.write_audio(&packet, granule).expect("write");
                frames_encoded += 1;
                last_granule = granule;
            }
            discard_partial_blocking(&accum);
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("test timed out after {timeout:?} (frames_encoded={frames_encoded})");
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    w.write_eos(&[], last_granule).expect("write EOS");
    let file = w.finish().expect("finish");
    let _ = file.sync_all();
    // Close the handle before re-reading the file (Windows file sharing).
    drop(file);

    producer.join().expect("producer join");

    // Verify all audio pages have granule = (frame_index + 1) * 960 strictly
    // increasing, ignoring the EOS page (which intentionally shares the last
    // granule).
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
            if !is_eos {
                if audio_pages > 0 {
                    assert_eq!(
                        granule,
                        prev_granule + 960,
                        "audio page {} gap: prev={} curr={}",
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
    // Producer emitted 100 chunks * 480 = 48_000 samples. With frame_size=960,
    // that's exactly 50 frames worth. Each chunk = 480 samples = half a frame.
    // Two chunks make one frame, so 100 chunks → 50 audio pages + 1 EOS page
    // (EOS is also counted in `audio_pages` because granule > 0).
    assert_eq!(
        audio_pages, 51,
        "expected 50 audio pages + 1 EOS = 51 total, got {}",
        audio_pages
    );

    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=duration,codec_name",
            "-of",
            "default=nw=1",
            tmp.to_str().unwrap(),
        ])
        .output()
        .expect("ffprobe");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("ffprobe: {}", stdout);
    assert!(stdout.contains("codec_name=opus"));
}

fn push_chunk(accum: &Accum, t: &mut f32, n: usize, dt: f32) {
    let mut chunk = Vec::with_capacity(n);
    for _ in 0..n {
        chunk.push(0.3 * (2.0 * std::f64::consts::PI as f32 * 1000.0 * *t).sin());
        *t += dt;
    }
    let mut b = accum.lock();
    b.extend_from_slice(&chunk);
}
