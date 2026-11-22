//! Provider-neutral narration inputs and evidence-based scene timing.
//!
//! This module validates externally supplied WAV bytes. It does not synthesize speech,
//! call an aligner, access paths, persist artifacts, or dispatch render jobs.
use std::io::Read;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_WAV_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_SAMPLE_RATE_HZ: u32 = 192_000;
pub const MAX_AUDIO_DURATION_MS: u64 = 60_000;
pub const FPS: u32 = 30;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleFormat {
    PcmUnsigned8,
    PcmSigned16,
    PcmSigned24,
    PcmSigned32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AudioCapabilities {
    pub format: SampleFormat,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub max_duration_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceCapabilities {
    pub can_synthesize: bool,
    pub can_align: bool,
    pub audio: Option<AudioCapabilities>,
    pub languages: Vec<String>,
    pub remote: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceRequest {
    pub scene_id: String,
    pub language: String,
    pub text: String,
    pub allocation_ms: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WordTiming {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Clone, Debug)]
pub struct AlignmentRequest<'a> {
    pub scene_id: &'a str,
    pub language: &'a str,
    pub transcript: &'a str,
    pub audio: &'a AudioInfo,
    pub wav_bytes: &'a [u8],
    pub allocation_ms: u32,
}

/// A future provider adapter must declare capabilities and return explicit failures.
/// Calling either method may have effects; the core module never supplies an adapter.
pub trait NarrationProvider: Send + Sync {
    fn capabilities(&self) -> VoiceCapabilities;
    fn synthesize(&self, request: &VoiceRequest) -> Result<Vec<u8>, VoiceError>;
}

pub trait AlignmentProvider: Send + Sync {
    fn capabilities(&self) -> VoiceCapabilities;
    fn align(&self, request: &AlignmentRequest<'_>) -> Result<Vec<WordTiming>, VoiceError>;
}

pub fn require_synthesis(
    capabilities: Option<&VoiceCapabilities>,
    language: &str,
) -> Result<(), VoiceError> {
    let capabilities = capabilities.ok_or(VoiceError::ProviderUnavailable)?;
    if !capabilities.can_synthesize || !supports_language(capabilities, language) {
        return Err(VoiceError::UnsupportedCapability);
    }
    Ok(())
}

pub fn require_alignment(
    capabilities: Option<&VoiceCapabilities>,
    language: &str,
) -> Result<(), VoiceError> {
    let capabilities = capabilities.ok_or(VoiceError::ProviderUnavailable)?;
    if !capabilities.can_align || !supports_language(capabilities, language) {
        return Err(VoiceError::UnsupportedCapability);
    }
    Ok(())
}

fn supports_language(capabilities: &VoiceCapabilities, language: &str) -> bool {
    capabilities
        .languages
        .iter()
        .any(|supported| supported.eq_ignore_ascii_case(language))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioInfo {
    pub format: SampleFormat,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub frame_count: u64,
    pub duration_ms: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalNarration {
    pub schema_version: String,
    pub audio: AudioInfo,
    pub provenance: AudioProvenance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioProvenance {
    UserSupplied,
    SyntheticTestFixture,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneAllocation {
    pub scene_id: String,
    pub duration_ms: u32,
    pub audio: LocalNarration,
    /// Only measured provider alignments create captions; no synthetic fallback.
    pub word_timings: Vec<WordTiming>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneTiming {
    pub scene_id: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub start_frame: u64,
    pub end_frame: u64,
    pub narration_duration_ms: u64,
    pub captions: Vec<WordTiming>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timeline {
    pub fps: u32,
    pub scenes: Vec<SceneTiming>,
    pub total_duration_ms: u64,
    pub total_frames: u64,
}

#[derive(Debug, Error)]
pub enum VoiceError {
    #[error("WAV data exceeds the configured size limit")]
    AudioTooLarge,
    #[error("invalid or truncated RIFF/WAVE container")]
    InvalidWav,
    #[error("WAV encoding is unsupported; expected little-endian integer PCM")]
    UnsupportedFormat,
    #[error("WAV sample rate, channel count, or duration is outside supported limits")]
    AudioLimits,
    #[error("WAV data chunk has invalid block alignment or integrity")]
    InvalidAudioData,
    #[error("narration exceeds its scene allocation")]
    NarrationOverlong,
    #[error("scene timing or caption metadata is inconsistent")]
    InvalidTiming,
    #[error("required narration provider is unavailable")]
    ProviderUnavailable,
    #[error("provider does not support the requested capability")]
    UnsupportedCapability,
    #[error("provider failed to process narration")]
    ProviderFailure,
    #[error("audio read failed")]
    ReadFailure,
}

/// Reads at most `MAX_WAV_BYTES + 1`, rejecting oversized inputs before parsing.
pub fn read_wav<R: Read>(mut reader: R) -> Result<LocalNarration, VoiceError> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take((MAX_WAV_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| VoiceError::ReadFailure)?;
    validate_wav(&bytes)
}

/// Accepts only canonical single-RIFF, integer-PCM WAV with one `fmt ` and one `data` chunk.
pub fn validate_wav(bytes: &[u8]) -> Result<LocalNarration, VoiceError> {
    if bytes.len() > MAX_WAV_BYTES {
        return Err(VoiceError::AudioTooLarge);
    }
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(VoiceError::InvalidWav);
    }
    let declared_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    if declared_size.checked_add(8) != Some(bytes.len()) {
        return Err(VoiceError::InvalidWav);
    }

    let mut offset = 12usize;
    let mut format = None;
    let mut data = None;
    while offset < bytes.len() {
        let header_end = offset.checked_add(8).ok_or(VoiceError::InvalidWav)?;
        if header_end > bytes.len() {
            return Err(VoiceError::InvalidWav);
        }
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..header_end].try_into().unwrap()) as usize;
        let start = header_end;
        let end = start.checked_add(size).ok_or(VoiceError::InvalidWav)?;
        let padded_end = end.checked_add(size & 1).ok_or(VoiceError::InvalidWav)?;
        if end > bytes.len() || padded_end > bytes.len() {
            return Err(VoiceError::InvalidWav);
        }
        match id {
            b"fmt " if format.is_none() => format = Some(&bytes[start..end]),
            b"data" if data.is_none() => data = Some(&bytes[start..end]),
            b"fmt " | b"data" => return Err(VoiceError::InvalidWav),
            _ => {}
        }
        offset = padded_end;
    }
    if offset != bytes.len() {
        return Err(VoiceError::InvalidWav);
    }
    let fmt = format.ok_or(VoiceError::InvalidWav)?;
    let data = data.ok_or(VoiceError::InvalidWav)?;
    if fmt.len() < 16 {
        return Err(VoiceError::InvalidWav);
    }
    if fmt.len() != 16 {
        return Err(VoiceError::UnsupportedFormat);
    }
    let encoding = u16::from_le_bytes(fmt[0..2].try_into().unwrap());
    let channels = u16::from_le_bytes(fmt[2..4].try_into().unwrap());
    let sample_rate_hz = u32::from_le_bytes(fmt[4..8].try_into().unwrap());
    let byte_rate = u32::from_le_bytes(fmt[8..12].try_into().unwrap());
    let block_align = u16::from_le_bytes(fmt[12..14].try_into().unwrap());
    let bits = u16::from_le_bytes(fmt[14..16].try_into().unwrap());
    if encoding != 1 {
        return Err(VoiceError::UnsupportedFormat);
    }
    if channels == 0 || channels > 2 || !(8_000..=MAX_SAMPLE_RATE_HZ).contains(&sample_rate_hz) {
        return Err(VoiceError::AudioLimits);
    }
    let (sample_format, bytes_per_sample) = match bits {
        8 => (SampleFormat::PcmUnsigned8, 1u16),
        16 => (SampleFormat::PcmSigned16, 2),
        24 => (SampleFormat::PcmSigned24, 3),
        32 => (SampleFormat::PcmSigned32, 4),
        _ => return Err(VoiceError::UnsupportedFormat),
    };
    let expected_align = channels
        .checked_mul(bytes_per_sample)
        .ok_or(VoiceError::InvalidAudioData)?;
    let expected_rate = sample_rate_hz
        .checked_mul(u32::from(expected_align))
        .ok_or(VoiceError::InvalidAudioData)?;
    if block_align != expected_align
        || byte_rate != expected_rate
        || data.is_empty()
        || data.len() % usize::from(block_align) != 0
    {
        return Err(VoiceError::InvalidAudioData);
    }
    let frame_count = (data.len() / usize::from(block_align)) as u64;
    let duration_ms = frame_count
        .checked_mul(1_000)
        .and_then(|samples| samples.checked_add(u64::from(sample_rate_hz) - 1))
        .ok_or(VoiceError::AudioLimits)?
        / u64::from(sample_rate_hz);
    if duration_ms == 0 || duration_ms > MAX_AUDIO_DURATION_MS {
        return Err(VoiceError::AudioLimits);
    }
    use sha2::Digest;
    let sha256 = format!("{:x}", sha2::Sha256::digest(bytes));
    Ok(LocalNarration {
        schema_version: "1".into(),
        audio: AudioInfo {
            format: sample_format,
            sample_rate_hz,
            channels,
            frame_count,
            duration_ms,
            sha256,
        },
        provenance: AudioProvenance::UserSupplied,
    })
}

/// Validates alignment spans against audio duration. Empty spans mean no measured captions.
pub fn validate_word_timings(timings: &[WordTiming], duration_ms: u64) -> Result<(), VoiceError> {
    let mut previous_end = 0;
    for timing in timings {
        if timing.text.trim().is_empty()
            || timing.start_ms >= timing.end_ms
            || timing.end_ms > duration_ms
            || timing.start_ms < previous_end
        {
            return Err(VoiceError::InvalidTiming);
        }
        previous_end = timing.end_ms;
    }
    Ok(())
}

/// Builds a sequential scene timeline using cumulative boundaries rounded once at each edge.
pub fn align_scenes(allocations: &[SceneAllocation]) -> Result<Timeline, VoiceError> {
    if allocations.is_empty() {
        return Err(VoiceError::InvalidTiming);
    }
    let mut scenes = Vec::with_capacity(allocations.len());
    let mut elapsed_ms = 0u64;
    let mut scene_ids = std::collections::HashSet::with_capacity(allocations.len());
    for allocation in allocations {
        if allocation.scene_id.trim().is_empty()
            || !scene_ids.insert(allocation.scene_id.as_str())
            || allocation.duration_ms == 0
        {
            return Err(VoiceError::InvalidTiming);
        }
        if allocation.audio.audio.duration_ms > u64::from(allocation.duration_ms) {
            return Err(VoiceError::NarrationOverlong);
        }
        validate_word_timings(&allocation.word_timings, allocation.audio.audio.duration_ms)?;
        let start_ms = elapsed_ms;
        elapsed_ms = elapsed_ms
            .checked_add(u64::from(allocation.duration_ms))
            .ok_or(VoiceError::InvalidTiming)?;
        if elapsed_ms > MAX_AUDIO_DURATION_MS {
            return Err(VoiceError::AudioLimits);
        }
        scenes.push(SceneTiming {
            scene_id: allocation.scene_id.clone(),
            start_ms,
            end_ms: elapsed_ms,
            start_frame: frame_boundary(start_ms)?,
            end_frame: frame_boundary(elapsed_ms)?,
            narration_duration_ms: allocation.audio.audio.duration_ms,
            captions: allocation.word_timings.clone(),
        });
    }
    Ok(Timeline {
        fps: FPS,
        scenes,
        total_duration_ms: elapsed_ms,
        total_frames: frame_boundary(elapsed_ms)?,
    })
}

fn frame_boundary(ms: u64) -> Result<u64, VoiceError> {
    ms.checked_mul(u64::from(FPS))
        .and_then(|ticks| ticks.checked_add(500))
        .map(|ticks| ticks / 1_000)
        .ok_or(VoiceError::InvalidTiming)
}
