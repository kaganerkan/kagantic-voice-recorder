//! Opus encoder wrapper + RFC 7845 header packet builders.

use ::opus::{Application, Bitrate, Channels, Encoder, SampleRate};
use anyhow::{anyhow, Result};

use crate::audio::CaptureFormat;

/// Build the OpusHead identification packet (RFC 7845 §5.1).
pub fn build_opus_head(fmt: &CaptureFormat, pre_skip: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(19);
    v.extend_from_slice(b"OpusHead");
    v.push(1); // version
    v.push(fmt.channels); // channel_count
    v.extend_from_slice(&pre_skip.to_le_bytes()); // pre-skip (uint16 LE)
    v.extend_from_slice(&fmt.sample_rate.to_le_bytes()); // input_sample_rate (informational)
    v.extend_from_slice(&0i16.to_le_bytes()); // output_gain (Q7.8, 0 = unity)
    v.push(0); // mapping_family (0 = single stream, no mapping table)
    v
}

/// Build the OpusTags packet (RFC 7845 §5.2).
pub fn build_opus_tags(vendor: &str, comment: Option<&str>) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"OpusTags");
    v.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    v.extend_from_slice(vendor.as_bytes());
    let count = if comment.is_some() { 1u32 } else { 0u32 };
    v.extend_from_slice(&count.to_le_bytes());
    if let Some(c) = comment {
        v.extend_from_slice(&(c.len() as u32).to_le_bytes());
        v.extend_from_slice(c.as_bytes());
    }
    v
}

/// Stateful Opus encoder producing one Ogg packet per `encode_float`.
pub struct OpusStreamEncoder {
    enc: Encoder,
    channels: u8,
    sample_rate: u32,
    frame_size: usize, // samples per channel per Opus frame (e.g. 960 = 20ms @ 48k)
}

impl OpusStreamEncoder {
    pub fn new(fmt: &CaptureFormat, bitrate_bps: i32) -> Result<Self> {
        let channels = match fmt.channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            n => return Err(anyhow!("unsupported channel count: {n}")),
        };
        let rate = SampleRate::Hz48000;
        let mut enc = Encoder::new(channels, rate, Application::Audio)
            .map_err(|e| anyhow!("opus encoder init: {e:?}"))?;
        enc.set_bitrate(Bitrate::from(bitrate_bps))
            .map_err(|e| anyhow!("opus set_bitrate: {e:?}"))?;
        Ok(Self {
            enc,
            channels: fmt.channels,
            sample_rate: 48_000,
            frame_size: 960,
        })
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub fn frame_size(&self) -> usize {
        self.frame_size
    }

    /// Encode exactly `frame_size` samples per channel of `f32` interleaved audio.
    pub fn encode_f32(&mut self, interleaved: &[f32]) -> Result<Vec<u8>> {
        let expected = self.frame_size * self.channels as usize;
        if interleaved.len() != expected {
            return Err(anyhow!(
                "frame length mismatch: got {}, expected {}",
                interleaved.len(),
                expected
            ));
        }
        // opusic-c exposes encode_float_to_slice writing into &mut [u8].
        let mut out = vec![0u8; 4000];
        let n = self
            .enc
            .encode_float_to_slice(interleaved, &mut out)
            .map_err(|e| anyhow!("opus encode: {e:?}"))?;
        out.truncate(n);
        Ok(out)
    }
}

/// Encoder lookahead for the fixed 48 kHz audio mode, in granule samples.
pub fn pre_skip_samples() -> u16 {
    312
}

pub fn write_opus_head(fmt: &CaptureFormat) -> Vec<u8> {
    build_opus_head(fmt, pre_skip_samples())
}

pub fn vendor_string() -> String {
    format!("kvr/{}", env!("CARGO_PKG_VERSION"))
}

pub fn build_tags() -> Vec<u8> {
    build_opus_tags(&vendor_string(), Some("RECORDED_BY=kvr"))
}
