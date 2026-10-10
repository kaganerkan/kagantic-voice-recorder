//! Output format selection + linear resampling helpers.
//!
//! `OutputFormat` enumerates every container the recorder can write.
//! PCM retains the capture clock. Opus uses a channel-aware streaming linear
//! resampler with integer phase, so callback boundaries do not change duration.

use std::fmt;
use std::str::FromStr;

use anyhow::{anyhow as anyhow_err, Error, Result};

/// Container written by the recorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// RFC 7845 Ogg/Opus (default).
    #[default]
    Opus,
    /// WAVE / RIFF carrying interleaved signed 16-bit little-endian PCM at
    /// the negotiated capture sample rate.
    WavPcm16le,
    /// Raw little-endian f32 samples at the negotiated capture sample rate.
    RawF32le,
}

impl OutputFormat {
    /// Default file extension (without leading dot).
    pub fn default_ext(&self) -> &'static str {
        match self {
            OutputFormat::Opus => "opus",
            OutputFormat::WavPcm16le => "wav",
            OutputFormat::RawF32le => "f32le",
        }
    }
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            OutputFormat::Opus => "opus",
            OutputFormat::WavPcm16le => "wav",
            OutputFormat::RawF32le => "raw",
        };
        f.write_str(s)
    }
}

impl FromStr for OutputFormat {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "opus" | "ogg" => Ok(OutputFormat::Opus),
            "wav" | "wave" | "pcm_s16le" => Ok(OutputFormat::WavPcm16le),
            "raw" | "f32le" | "pcm_f32le" => Ok(OutputFormat::RawF32le),
            other => Err(anyhow_err!("unknown output format: {other}")),
        }
    }
}

impl clap::ValueEnum for OutputFormat {
    fn value_variants<'a>() -> &'a [Self] {
        &[Self::Opus, Self::WavPcm16le, Self::RawF32le]
    }
    fn from_str(input: &str, _ignore_case: bool) -> std::result::Result<Self, String> {
        // clap's `value_enum!` already routes through `FromStr`; we keep this
        // exhaustive so unrecognised strings surface a friendly error rather
        // than panicking.
        <Self as std::str::FromStr>::from_str(input).map_err(|e| e.to_string())
    }
    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(match self {
            OutputFormat::Opus => {
                clap::builder::PossibleValue::new("opus").help("Ogg/Opus (RFC 7845) — default")
            }
            OutputFormat::WavPcm16le => clap::builder::PossibleValue::new("wav")
                .help("WAVE / RIFF carrying interleaved signed 16-bit PCM"),
            OutputFormat::RawF32le => {
                clap::builder::PossibleValue::new("raw").help("Raw little-endian f32 samples")
            }
        })
    }
}

/// Streaming interleaved resampling; retains the interpolation boundary only.
pub(crate) struct OpusResampler {
    samples: std::collections::VecDeque<f32>,
    src_rate: u32,
    channels: usize,
    input_frames: u64,
    base_frame: u64,
    output_frames: u64,
}

impl OpusResampler {
    pub(crate) fn new(src_rate: u32, channels: u8) -> Result<Self> {
        if src_rate == 0 || !(1..=2).contains(&channels) {
            return Err(anyhow_err!(
                "invalid capture format: {channels} channels at {src_rate} Hz"
            ));
        }
        Ok(Self {
            samples: std::collections::VecDeque::new(),
            src_rate,
            channels: channels as usize,
            input_frames: 0,
            base_frame: 0,
            output_frames: 0,
        })
    }

    pub(crate) fn convert(
        &mut self,
        input: &[f32],
        output: &mut Vec<f32>,
        finish: bool,
    ) -> Result<()> {
        if !input.len().is_multiple_of(self.channels) {
            return Err(anyhow_err!("incomplete interleaved capture frame"));
        }
        if self.src_rate == 48_000 {
            output.extend_from_slice(input);
            self.input_frames += (input.len() / self.channels) as u64;
            self.output_frames = self.input_frames;
            return Ok(());
        }
        self.samples.extend(input.iter().copied());
        self.input_frames += (input.len() / self.channels) as u64;
        let target = (self.input_frames * 48_000).div_ceil(u64::from(self.src_rate));
        while self.output_frames < target {
            let position = self.output_frames * u64::from(self.src_rate);
            let left = position / 48_000;
            let fraction = (position % 48_000) as f32 / 48_000.0;
            if !finish && fraction != 0.0 && left + 1 >= self.input_frames {
                break;
            }
            let right = (left + 1).min(self.input_frames - 1);
            for channel in 0..self.channels {
                let a = self.samples[(left - self.base_frame) as usize * self.channels + channel];
                let b = self.samples[(right - self.base_frame) as usize * self.channels + channel];
                output.push(a + (b - a) * fraction);
            }
            self.output_frames += 1;
        }
        let next = (self.output_frames * u64::from(self.src_rate) / 48_000).min(self.input_frames);
        let discard = (next - self.base_frame) as usize * self.channels;
        self.samples.drain(..discard);
        self.base_frame = next;
        Ok(())
    }

    pub(crate) fn output_frames(&self) -> u64 {
        self.output_frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ext_mapping_is_stable() {
        assert_eq!(OutputFormat::Opus.default_ext(), "opus");
        assert_eq!(OutputFormat::WavPcm16le.default_ext(), "wav");
        assert_eq!(OutputFormat::RawF32le.default_ext(), "f32le");
    }

    #[test]
    fn from_str_accepts_synonyms_case_insensitively() {
        assert_eq!("opus".parse::<OutputFormat>().unwrap(), OutputFormat::Opus);
        assert_eq!("OPUS".parse::<OutputFormat>().unwrap(), OutputFormat::Opus);
        assert_eq!("ogg".parse::<OutputFormat>().unwrap(), OutputFormat::Opus);
        assert_eq!(
            "WAV".parse::<OutputFormat>().unwrap(),
            OutputFormat::WavPcm16le
        );
        assert_eq!(
            "raw".parse::<OutputFormat>().unwrap(),
            OutputFormat::RawF32le
        );
        assert_eq!(
            "f32le".parse::<OutputFormat>().unwrap(),
            OutputFormat::RawF32le
        );
        assert!("docx".parse::<OutputFormat>().is_err());
    }

    #[test]
    fn display_matches_lowercase_token() {
        assert_eq!(OutputFormat::Opus.to_string(), "opus");
        assert_eq!(OutputFormat::WavPcm16le.to_string(), "wav");
        assert_eq!(OutputFormat::RawF32le.to_string(), "raw");
    }

    #[test]
    fn streaming_resampling_is_chunk_invariant_and_does_not_mix_channels() {
        let input: Vec<f32> = (0..44_101)
            .flat_map(|index| [index as f32 / 44_101.0, -0.5])
            .collect();
        let mut whole = OpusResampler::new(44_100, 2).unwrap();
        let mut expected = Vec::new();
        whole.convert(&input, &mut expected, true).unwrap();
        let mut chunked = OpusResampler::new(44_100, 2).unwrap();
        let mut actual = Vec::new();
        for chunk in input.chunks(254) {
            chunked.convert(chunk, &mut actual, false).unwrap();
        }
        chunked.convert(&[], &mut actual, true).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(
            actual.len(),
            (44_101u64 * 48_000).div_ceil(44_100) as usize * 2
        );
        for frame in actual.as_chunks::<2>().0 {
            assert_eq!(frame[1], -0.5);
            assert!((0.0..1.0).contains(&frame[0]));
        }
    }
}
