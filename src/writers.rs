//! Output sinks used by `run_recorder`. Each sink implements [`FrameSink`]
//! so the recorder loop can stay format-agnostic: it pulls a Vec<f32>
//! frame from the accumulator and hands it to whichever sink the
//! configuration asked for.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};

use anyhow::{anyhow, Result};

use crate::audio::CaptureFormat;
use crate::encoder::OpusStreamEncoder;
use crate::ogg::OggWriter;
// `OggWriter` is concrete over `File`, so `OggOpusSink` is too. Generic
// variants would require reshaping `OggWriter` itself, which is out of
// scope for this slice.

/// Container sink: takes interleaved float frames from the recorder and
/// writes them into whatever container is configured.
pub trait FrameSink: Send {
    /// Called once with the negotiated capture format before any frames.
    fn write_header(&mut self, fmt: &CaptureFormat) -> Result<()>;
    /// Called for each complete interleaved frame.
    fn write_frames(&mut self, frames: &[f32], fmt: &CaptureFormat) -> Result<()>;
    /// Called once after the recorder stops; flushes and (if applicable)
    /// back-patches container sizes.
    fn finalize(&mut self) -> Result<()>;
    /// Total file bytes, including container overhead; header back-patches do
    /// not increase this count. Finalization may append additional bytes.
    fn bytes_written(&self) -> u64;
}

// -- OggOpusSink ------------------------------------------------------------

/// Ogg/Opus sink: encodes each frame with `OpusStreamEncoder` and writes
/// the resulting packets through an [`OggWriter`] using the BOS + OpusTags
/// + EOS layout from RFC 7845.
pub struct OggOpusSink {
    pub ogg: OggWriter,
    pub encoder: OpusStreamEncoder,
    pub fmt: CaptureFormat,
    frames_emitted: u64,
    header_written: bool,
    resampler: crate::output::OpusResampler,
    pending_samples: Vec<f32>,
    pending_packet: Option<Vec<u8>>,
    finalized: bool,
}

impl OggOpusSink {
    /// Convenience constructor that opens `path` and prepares an encoder
    /// for the negotiated format at `bitrate_bps`.
    pub fn new_file(path: &std::path::Path, fmt: &CaptureFormat, bitrate_bps: i32) -> Result<Self> {
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| anyhow!("create ogg output {}: {e}", path.display()))?;
        Self::new(file, fmt, bitrate_bps)
    }

    pub fn new(file: File, fmt: &CaptureFormat, bitrate_bps: i32) -> Result<Self> {
        let encoder = OpusStreamEncoder::new(fmt, bitrate_bps)?;
        Ok(Self {
            ogg: OggWriter::new(file),
            encoder,
            fmt: fmt.clone(),
            frames_emitted: 0,
            header_written: false,
            resampler: crate::output::OpusResampler::new(fmt.sample_rate, fmt.channels)?,
            pending_samples: Vec::with_capacity(960 * fmt.channels as usize),
            pending_packet: None,
            finalized: false,
        })
    }

    fn encode_pending(&mut self) -> Result<()> {
        let need = self.encoder.frame_size() * self.fmt.channels as usize;
        let complete = self.pending_samples.len() / need;
        for index in 0..complete {
            let packet = self
                .encoder
                .encode_f32(&self.pending_samples[index * need..(index + 1) * need])?;
            if let Some(previous) = self.pending_packet.replace(packet) {
                self.ogg.write_audio(&previous, self.frames_emitted * 960)?;
            }
            self.frames_emitted += 1;
        }
        self.pending_samples.drain(..complete * need);
        Ok(())
    }
}

impl FrameSink for OggOpusSink {
    fn write_header(&mut self, fmt: &CaptureFormat) -> Result<()> {
        let head = crate::encoder::write_opus_head(fmt);
        let tags = crate::encoder::build_tags();
        self.ogg.write_bos(&head)?;
        self.ogg.write_audio(&tags, 0)?;
        self.header_written = true;
        Ok(())
    }

    fn write_frames(&mut self, frames: &[f32], _fmt: &CaptureFormat) -> Result<()> {
        if self.finalized {
            return Err(anyhow!("Opus sink already finalized"));
        }
        self.resampler
            .convert(frames, &mut self.pending_samples, false)?;
        self.encode_pending()
    }

    fn finalize(&mut self) -> Result<()> {
        if self.finalized {
            return Ok(());
        }
        if !self.header_written {
            self.write_header(&self.fmt.clone())?;
        }
        self.resampler
            .convert(&[], &mut self.pending_samples, true)?;
        // Encode the tail and encoder delay; the final audio page's granule
        // trims padding while pre-skip removes the initial lookahead.
        let final_granule =
            self.resampler.output_frames() + u64::from(crate::encoder::pre_skip_samples());
        let channels = self.fmt.channels as usize;
        self.pending_samples
            .resize(self.pending_samples.len() + 312 * channels, 0.0);
        let need = self.encoder.frame_size() * channels;
        self.pending_samples
            .resize(self.pending_samples.len().div_ceil(need) * need, 0.0);
        self.encode_pending()?;
        if let Some(packet) = self.pending_packet.take() {
            self.ogg.write_eos(&packet, final_granule)?;
        }
        self.ogg.flush()?;
        self.finalized = true;
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.ogg.bytes_written()
    }
}

// -- WavSink ----------------------------------------------------------------

/// WAVE sink for interleaved signed 16-bit little-endian PCM at the
/// negotiated capture sample rate. The 44-byte RIFF header carries
/// `fmt.sample_rate` / `fmt.channels`; `finalize` seeks to offset 4 and
/// 40 to back-patch the RIFF size and data chunk size, then releases
/// the writer so subsequent `File::open` calls don't fail on Windows
/// (which enforces exclusive write sharing).
pub struct WavSink<W: Write + Seek> {
    pub writer: Option<W>,
    pub fmt: CaptureFormat,
    pub bytes_written: u64,
    header_written: bool,
}

impl WavSink<File> {
    pub fn new_file(path: &std::path::Path, fmt: &CaptureFormat) -> Result<Self> {
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| anyhow!("create wav output {}: {e}", path.display()))?;
        Ok(Self::new(file, fmt.clone()))
    }
}

impl<W: Write + Seek> WavSink<W> {
    pub fn new(writer: W, fmt: CaptureFormat) -> Self {
        Self {
            writer: Some(writer),
            fmt,
            bytes_written: 0,
            header_written: false,
        }
    }

    /// Build the 44-byte WAVE header for the negotiated sample format.
    pub fn build_header(fmt: &CaptureFormat) -> [u8; 44] {
        let mut h = [0u8; 44];
        h[0..4].copy_from_slice(b"RIFF");
        // ChunkSize = 36 + data_size; back-patched in finalize.
        h[4..8].copy_from_slice(&0u32.to_le_bytes());
        h[8..12].copy_from_slice(b"WAVE");
        h[12..16].copy_from_slice(b"fmt ");
        h[16..20].copy_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
        h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM format tag
        h[22..24].copy_from_slice(&(fmt.channels as u16).to_le_bytes());
        h[24..28].copy_from_slice(&fmt.sample_rate.to_le_bytes());
        let byte_rate = fmt.sample_rate * fmt.channels as u32 * 2;
        h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
        let block_align = (fmt.channels as u16) * 2;
        h[32..34].copy_from_slice(&block_align.to_le_bytes());
        h[34..36].copy_from_slice(&16u16.to_le_bytes()); // bits per sample
        h[36..40].copy_from_slice(b"data");
        // Subchunk2Size = data_size; back-patched in finalize.
        h[40..44].copy_from_slice(&0u32.to_le_bytes());
        h
    }

    /// Float → i16 PCM conversion helper, exposed for the integration tests.
    pub fn encode_i16(sample: f32) -> i16 {
        // i16 ranges from -32768 to 32767; multiplying by i16::MAX (= 32767)
        // makes positive full-scale round-trip, while negatives clamp to
        // -32767 (i16::MIN+1) and -1.0 maps cleanly to -32767. We therefore
        // use (i16::MAX + 1) (= 32768) so the negative end reaches i16::MIN.
        let v = sample.clamp(-1.0, 1.0) * ((i16::MAX as i32) + 1) as f32;
        v as i16
    }
}

impl<W: Write + Seek + Send> FrameSink for WavSink<W> {
    fn write_header(&mut self, fmt: &CaptureFormat) -> Result<()> {
        self.fmt = fmt.clone();
        let header = Self::build_header(fmt);
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| anyhow!("WAV sink writer already released"))?;
        writer.write_all(&header)?;
        writer.flush()?;
        self.header_written = true;
        Ok(())
    }

    fn write_frames(&mut self, frames: &[f32], _fmt: &CaptureFormat) -> Result<()> {
        if self
            .bytes_written
            .checked_add(frames.len() as u64 * 2)
            .is_none_or(|size| size > u64::from(u32::MAX) - 36)
        {
            return Err(anyhow!("WAV recording exceeds the classic RIFF size limit; stop before 4 GiB or use Opus/raw"));
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| anyhow!("WAV sink writer already released"))?;
        for sample in frames {
            let encoded = Self::encode_i16(*sample).to_le_bytes();
            writer.write_all(&encoded)?;
            self.bytes_written += encoded.len() as u64;
        }
        Ok(())
    }

    fn finalize(&mut self) -> Result<()> {
        // Take the writer out so subsequent calls (e.g. `File::open` on the
        // same path on Windows) don't collide with a still-open handle.
        let mut writer = match self.writer.take() {
            Some(w) => w,
            None => return Ok(()),
        };
        if !self.header_written {
            // Empty recording: still a valid (silent) WAV header.
            writer.seek(SeekFrom::Start(0))?;
            let mut header = Self::build_header(&self.fmt);
            header[4..8].copy_from_slice(&36u32.to_le_bytes());
            writer.write_all(&header)?;
            writer.flush()?;
        } else {
            writer.flush()?;
            // RIFF ChunkSize = 36 + Subchunk2Size
            writer.seek(SeekFrom::Start(4))?;
            let riff_size = u32::try_from(self.bytes_written + 36)?;
            writer.write_all(&riff_size.to_le_bytes())?;
            // Subchunk2Size = data_size
            writer.seek(SeekFrom::Start(40))?;
            writer.write_all(&u32::try_from(self.bytes_written)?.to_le_bytes())?;
            writer.flush()?;
        }
        self.header_written = true;
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.bytes_written + if self.header_written { 44 } else { 0 }
    }
}

// -- RawF32Sink -------------------------------------------------------------

/// Raw little-endian f32 sink. No header bytes are emitted; the negotiated
/// sample rate / channel count are carried solely in `CaptureFormat` and
/// surfaced to ffprobe via CLI flags by the integration tests.
pub struct RawF32Sink<W: Write> {
    pub writer: Option<W>,
    pub fmt: CaptureFormat,
    header_written: bool,
    bytes_written: u64,
}

impl RawF32Sink<File> {
    pub fn new_file(path: &std::path::Path, fmt: &CaptureFormat) -> Result<Self> {
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| anyhow!("create raw output {}: {e}", path.display()))?;
        Ok(Self::new(file, fmt.clone()))
    }
}

impl<W: Write> RawF32Sink<W> {
    pub fn new(writer: W, fmt: CaptureFormat) -> Self {
        Self {
            writer: Some(writer),
            fmt,
            header_written: false,
            bytes_written: 0,
        }
    }
}

impl<W: Write + Send> FrameSink for RawF32Sink<W> {
    fn write_header(&mut self, _fmt: &CaptureFormat) -> Result<()> {
        // Intentionally writes zero bytes — sample rate / channel count are
        // out-of-band metadata that the caller is responsible for carrying.
        self.header_written = true;
        Ok(())
    }

    fn write_frames(&mut self, frames: &[f32], _fmt: &CaptureFormat) -> Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| anyhow!("raw sink writer already released"))?;
        for sample in frames {
            writer.write_all(&sample.to_le_bytes())?;
            self.bytes_written += 4;
        }
        Ok(())
    }

    fn finalize(&mut self) -> Result<()> {
        // Take the writer out so the file handle is closed before downstream
        // tools (e.g. ffmpeg) try to read the raw stream on Windows.
        if let Some(mut writer) = self.writer.take() {
            writer.flush()?;
        }
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

/// Open a no-overwrite sink at the negotiated input clock. Only Opus resamples.
pub fn open_sink(
    path: &std::path::Path,
    fmt: &CaptureFormat,
    format: crate::output::OutputFormat,
    bitrate: i32,
) -> Result<Box<dyn FrameSink>> {
    let mut sink: Box<dyn FrameSink> = match format {
        crate::output::OutputFormat::Opus => Box::new(OggOpusSink::new_file(path, fmt, bitrate)?),
        crate::output::OutputFormat::WavPcm16le => Box::new(WavSink::new_file(path, fmt)?),
        crate::output::OutputFormat::RawF32le => Box::new(RawF32Sink::new_file(path, fmt)?),
    };
    sink.write_header(fmt)?;
    Ok(sink)
}

/// Drain bounded batches outside the callback lock, retaining a reusable buffer.
/// Returns interleaved sample count and sum of squares for duration/RMS.
pub fn drain_capture(
    accum: &crate::audio::Accum,
    sink: &mut dyn FrameSink,
    fmt: &CaptureFormat,
    finish: bool,
    buffer: &mut Vec<f32>,
) -> Result<(u64, f64)> {
    let channels = usize::from(fmt.channels);
    let block = (fmt.sample_rate as usize / 100).max(1) * channels;
    let mut samples = 0;
    let mut squares = 0.0;
    loop {
        buffer.clear();
        {
            let mut input = accum.lock();
            let available = input.len() / channels * channels;
            if available < block && !finish {
                break;
            }
            let count = available.min(block);
            if count == 0 {
                break;
            }
            buffer.extend(input.drain(..count));
        }
        squares += buffer.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
        if let Err(error) = sink.write_frames(buffer, fmt) {
            return match sink.finalize() {
                Ok(()) => Err(error.context("write recording; partial file finalized")),
                Err(finalization) => Err(error.context(format!(
                    "write recording; partial-file finalization also failed: {finalization:#}"
                ))),
            };
        }
        samples += buffer.len() as u64;
    }
    Ok((samples, squares))
}

// -- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn empty_wav_finalization_writes_valid_sizes_without_a_prior_header() {
        let mut bytes = Vec::new();
        let mut sink = WavSink::new(Cursor::new(&mut bytes), test_fmt());
        sink.finalize().unwrap();
        assert_eq!(sink.bytes_written(), 44);
        assert_eq!(bytes.len(), 44);
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 36);
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 0);
    }

    #[test]
    fn wav_riff_limit_is_rejected_before_writing_or_wrapping() {
        #[derive(Default)]
        struct SparseWriter {
            position: u64,
            length: u64,
        }
        impl Write for SparseWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.position += bytes.len() as u64;
                self.length = self.length.max(self.position);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl Seek for SparseWriter {
            fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
                self.position = match position {
                    SeekFrom::Start(position) => position,
                    SeekFrom::End(offset) => self.length.checked_add_signed(offset).unwrap(),
                    SeekFrom::Current(offset) => self.position.checked_add_signed(offset).unwrap(),
                };
                Ok(self.position)
            }
        }
        let mut sink = WavSink::new(SparseWriter::default(), test_fmt());
        sink.write_header(&test_fmt()).unwrap();
        sink.bytes_written = u64::from(u32::MAX) - 39;
        let writer = sink.writer.as_mut().unwrap();
        writer.length = sink.bytes_written + 44;
        writer.position = writer.length;
        sink.write_frames(&[0.25], &test_fmt()).unwrap();
        let before = sink.bytes_written();
        assert!(sink
            .write_frames(&[0.25], &test_fmt())
            .unwrap_err()
            .to_string()
            .contains("RIFF size limit"));
        assert_eq!(sink.bytes_written(), before);
        assert_eq!(sink.writer.as_ref().unwrap().length, before);
        sink.finalize().unwrap();
        assert_eq!(sink.bytes_written(), before);
    }

    fn test_fmt() -> CaptureFormat {
        CaptureFormat {
            sample_rate: 48_000,
            channels: 1,
        }
    }

    #[test]
    fn wav_header_is_44_bytes_with_riff_wave_fmt_magic() {
        let fmt = test_fmt();
        let header = WavSink::<File>::build_header(&fmt);
        assert_eq!(header.len(), 44);
        assert_eq!(&header[0..4], b"RIFF");
        assert_eq!(&header[8..12], b"WAVE");
        assert_eq!(&header[12..16], b"fmt ");
        assert_eq!(&header[16..20], &[16, 0, 0, 0]); // fmt chunk size
        assert_eq!(&header[20..22], &[1, 0]); // PCM format tag
        assert_eq!(u16::from_le_bytes([header[22], header[23]]), 1);
        assert_eq!(
            u32::from_le_bytes([header[24], header[25], header[26], header[27]]),
            48_000
        );
        assert_eq!(
            u32::from_le_bytes([header[28], header[29], header[30], header[31]]),
            48_000 * 2
        );
        assert_eq!(&header[36..40], b"data");
        // Sizes zero before finalize; finalize back-patches them.
        assert_eq!(
            u32::from_le_bytes([header[4], header[5], header[6], header[7]]),
            0
        );
        assert_eq!(
            u32::from_le_bytes([header[40], header[41], header[42], header[43]]),
            0
        );
    }

    #[test]
    fn wav_sink_writes_integer_pcm_and_patches_sizes_on_finalize() {
        let mut bytes = Vec::<u8>::new();
        {
            let mut sink = WavSink::new(Cursor::new(&mut bytes), test_fmt());
            sink.write_header(&test_fmt()).unwrap();
            let frames: [f32; 4] = [0.0, 0.5, -0.5, 1.0];
            sink.write_frames(&frames, &test_fmt()).unwrap();
            sink.finalize().unwrap();
        }
        // Header + 4 samples × 2 bytes = 44 + 8.
        assert_eq!(bytes.len(), 52);
        // First sample is silent.
        assert_eq!(&bytes[44..46], &[0, 0]);
        // 0.5 * i16::MAX ≈ 16383.
        let half = i16::from_le_bytes([bytes[46], bytes[47]]);
        assert!((half - 16383).abs() <= 1);
        // -0.5 → -16384.
        let neg_half = i16::from_le_bytes([bytes[48], bytes[49]]);
        assert!((neg_half + 16384).abs() <= 1);
        // 1.0 → i16::MAX = 32767.
        let full = i16::from_le_bytes([bytes[50], bytes[51]]);
        assert_eq!(full, i16::MAX);
        // RIFF ChunkSize = 36 + 8 = 44.
        let riff_size = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        assert_eq!(riff_size, 44);
        // data chunk Subchunk2Size = 8.
        let data_size = u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]);
        assert_eq!(data_size, 8);
    }

    #[test]
    fn wav_sink_clamps_out_of_range_samples() {
        let mut bytes = Vec::<u8>::new();
        {
            let mut sink = WavSink::new(Cursor::new(&mut bytes), test_fmt());
            sink.write_header(&test_fmt()).unwrap();
            sink.write_frames(&[2.0, -2.0], &test_fmt()).unwrap();
        }
        let hi = i16::from_le_bytes([bytes[44], bytes[45]]);
        let lo = i16::from_le_bytes([bytes[46], bytes[47]]);
        assert_eq!(hi, i16::MAX);
        assert_eq!(lo, i16::MIN);
    }

    #[test]
    fn raw_sink_writes_four_bytes_per_sample() {
        let mut bytes = Vec::<u8>::new();
        {
            let mut sink = RawF32Sink::new(Cursor::new(&mut bytes), test_fmt());
            sink.write_header(&test_fmt()).unwrap();
            let frames: [f32; 5] = [0.0, 1.0, -1.0, 0.25, 0.75];
            sink.write_frames(&frames, &test_fmt()).unwrap();
            sink.finalize().unwrap();
        }
        // Header writes zero bytes, so total = 5 * 4 = 20.
        assert_eq!(bytes.len(), 20);
        for (i, expected) in [0.0f32, 1.0, -1.0, 0.25, 0.75].iter().enumerate() {
            let got = f32::from_le_bytes([
                bytes[i * 4],
                bytes[i * 4 + 1],
                bytes[i * 4 + 2],
                bytes[i * 4 + 3],
            ]);
            assert_eq!(got, *expected);
        }
    }

    #[test]
    fn ogg_opus_sink_writes_valid_bos_with_opus_head_at_byte_28() {
        // Write to a temp path so we can re-read the bytes as &[u8] and check
        // the OggS capture pattern + OpusHead payload position required by
        // the integration tests.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.opus");
        let fmt = test_fmt();
        {
            let mut sink = OggOpusSink::new_file(&path, &fmt, 96_000).unwrap();
            sink.write_header(&fmt).unwrap();
            // 960 samples × 1 channel = 1 frame; encode_f32 enforces that.
            let frame = vec![0.0f32; 960];
            sink.write_frames(&frame, &fmt).unwrap();
            sink.finalize().unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        // First page starts with OggS capture pattern.
        assert_eq!(&bytes[0..4], b"OggS");
        // BOS page (page_seq=0): segment_table byte count at offset 26.
        assert_eq!(bytes[26], 1, "single segment table for OpusHead");
        // First segment length byte at offset 27 = 19 (size of OpusHead packet).
        assert_eq!(bytes[27], 19);
        // OpusHead packet starts at byte 28.
        assert_eq!(&bytes[28..36], b"OpusHead");
        // Channel count and pre-skip follow.
        assert_eq!(bytes[36], 1); // version
        assert_eq!(bytes[37], 1); // channels
    }
}
