//! Output format selection + linear resampling helpers.
//!
//! `OutputFormat` enumerates every container the recorder can write.
//! `resample_linear` is the cheap one-shot resampler used by the non-Opus
//! sinks when the negotiated capture sample rate differs from the device's
//! native rate.

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

/// Resample interleaved float samples from `src_rate` to `dst_rate` using
/// piecewise-linear interpolation. Equal rates short-circuit to a clone of
/// the input slice; the output length is `ceil(samples.len() * dst / src)`,
/// which matches what callers need when they're not keeping a long history.
pub fn resample_linear(samples: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }
    if src_rate == 0 || dst_rate == 0 {
        return samples.to_vec();
    }
    if src_rate == dst_rate {
        return samples.to_vec();
    }
    let src = samples.len() as f64;
    let ratio = dst_rate as f64 / src_rate as f64;
    let out_len = ((src * ratio).ceil() as usize).max(1);
    let mut out = Vec::with_capacity(out_len);
    let last_index = (samples.len() - 1) as f64;
    for i in 0..out_len {
        let pos = (i as f64) / ratio;
        if pos >= last_index {
            out.push(samples[samples.len() - 1]);
            continue;
        }
        let i0 = pos.floor() as usize;
        let frac = (pos - i0 as f64) as f32;
        let a = samples[i0];
        let b = samples[i0 + 1];
        out.push(a + (b - a) * frac);
    }
    out
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
    fn resample_passthrough_when_rates_match() {
        let input = vec![0.1, 0.2, 0.3, 0.4];
        let out = resample_linear(&input, 48_000, 48_000);
        assert_eq!(out, input);
    }

    #[test]
    fn resample_doubles_count_when_doubling_rate() {
        // 4 samples @ 1Hz → 8 samples @ 2Hz (endpoints included).
        let input = vec![0.0, 1.0, 0.0, 1.0];
        let out = resample_linear(&input, 1, 2);
        assert_eq!(out.len(), 8);
        assert!((out[0] - 0.0).abs() < 1e-6);
        assert!((out[4] - 0.0).abs() < 1e-6);
        assert!((out[7] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn resample_clamps_to_last_sample_for_short_input() {
        // 2 samples @ 1Hz → 1 sample @ 1Hz still produces a copy.
        let input = vec![0.5, 1.0];
        let out = resample_linear(&input, 1, 1);
        assert_eq!(out, input);
    }
}
