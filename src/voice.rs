//! Provider-neutral narration inputs and evidence-based scene timing.
//!
//! This module validates externally supplied WAV bytes and structural narration metadata.
//! It does not synthesize speech, call an aligner, verify audio authenticity, access paths,
//! persist artifacts, or dispatch render jobs.
use std::collections::HashSet;
use std::io::Read;

use serde::{Deserialize, Serialize};
use sha2::Digest;
use thiserror::Error;

pub const MAX_WAV_BYTES: usize = 64 * 1024 * 1024;
pub const MIN_SAMPLE_RATE_HZ: u32 = 8_000;
pub const MAX_SAMPLE_RATE_HZ: u32 = 192_000;
pub const MAX_AUDIO_DURATION_MS: u64 = 60_000;
pub const MAX_SCENES: usize = 60;
pub const MAX_CAPTION_TEXT_CHARS: usize = 10_000;
pub const FPS: u32 = 30;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleFormat {
    PcmUnsigned8,
    PcmSigned16,
    PcmSigned24,
    PcmSigned32,
}

impl SampleFormat {
    fn bytes_per_sample(self) -> u16 {
        match self {
            Self::PcmUnsigned8 => 1,
            Self::PcmSigned16 => 2,
            Self::PcmSigned24 => 3,
            Self::PcmSigned32 => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    if language.trim().is_empty()
        || !capabilities.can_synthesize
        || !supports_language(capabilities, language)
    {
        return Err(VoiceError::UnsupportedCapability);
    }
    Ok(())
}

pub fn require_alignment(
    capabilities: Option<&VoiceCapabilities>,
    language: &str,
) -> Result<(), VoiceError> {
    let capabilities = capabilities.ok_or(VoiceError::ProviderUnavailable)?;
    if language.trim().is_empty()
        || !capabilities.can_align
        || !supports_language(capabilities, language)
    {
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

impl AudioProvenance {
    pub fn is_synthetic(self) -> bool {
        matches!(self, Self::SyntheticTestFixture)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptionProvenance {
    /// No captions were supplied for the scene.
    None,
    /// Caption spans were supplied externally without measured alignment evidence.
    UnverifiedExternal,
    /// Caption spans were returned by an `AlignmentProvider` on non-synthetic audio.
    MeasuredProviderAlignment,
}

/// Measured caption alignment bound to a non-synthetic audio digest.
///
/// `#[non_exhaustive]` prevents external callers from constructing `MeasuredAlignment` literals;
/// it does not derive `Deserialize`, so external JSON cannot forge measured alignment provenance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MeasuredAlignment {
    pub language: String,
    pub audio_sha256: String,
    pub word_timings: Vec<WordTiming>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneAllocation {
    pub scene_id: String,
    pub duration_ms: u32,
    pub audio: LocalNarration,
    /// Externally supplied timings remain unverified unless produced via `measure_alignment`.
    pub word_timings: Vec<WordTiming>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CaptionInput {
    None,
    UnverifiedExternal(Vec<WordTiming>),
    Measured(MeasuredAlignment),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlignedSceneAllocation {
    pub scene_id: String,
    pub duration_ms: u32,
    pub audio: LocalNarration,
    pub captions: CaptionInput,
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
    pub audio_provenance: AudioProvenance,
    pub caption_provenance: CaptionProvenance,
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

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(bytes))
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Computes ceiling millisecond duration from sample frames and sample rate within v1 bounds.
pub fn duration_ms_from_frames(frame_count: u64, sample_rate_hz: u32) -> Result<u64, VoiceError> {
    if frame_count == 0 || !(MIN_SAMPLE_RATE_HZ..=MAX_SAMPLE_RATE_HZ).contains(&sample_rate_hz) {
        return Err(VoiceError::AudioLimits);
    }
    let duration_ms = frame_count
        .checked_mul(1_000)
        .and_then(|samples| samples.checked_add(u64::from(sample_rate_hz) - 1))
        .ok_or(VoiceError::AudioLimits)?
        / u64::from(sample_rate_hz);
    if duration_ms == 0 || duration_ms > MAX_AUDIO_DURATION_MS {
        return Err(VoiceError::AudioLimits);
    }
    Ok(duration_ms)
}

/// Validates structural consistency of `LocalNarration` metadata.
///
/// Passing this check never proves that matching audio exists, is authentic, or contains speech.
pub fn validate_local_narration(narration: &LocalNarration) -> Result<(), VoiceError> {
    if narration.schema_version != "1" {
        return Err(VoiceError::InvalidTiming);
    }
    let audio = &narration.audio;
    if !is_lower_hex_sha256(&audio.sha256) {
        return Err(VoiceError::InvalidAudioData);
    }
    if audio.channels == 0 || audio.channels > 2 {
        return Err(VoiceError::AudioLimits);
    }
    if audio.frame_count == 0 {
        return Err(VoiceError::InvalidAudioData);
    }
    let expected_duration_ms = duration_ms_from_frames(audio.frame_count, audio.sample_rate_hz)?;
    if audio.duration_ms != expected_duration_ms {
        return Err(VoiceError::InvalidAudioData);
    }
    audio
        .frame_count
        .checked_mul(u64::from(audio.channels))
        .and_then(|samples| samples.checked_mul(u64::from(audio.format.bytes_per_sample())))
        .ok_or(VoiceError::AudioLimits)?;
    Ok(())
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
    if channels == 0
        || channels > 2
        || !(MIN_SAMPLE_RATE_HZ..=MAX_SAMPLE_RATE_HZ).contains(&sample_rate_hz)
    {
        return Err(VoiceError::AudioLimits);
    }
    let sample_format = match bits {
        8 => SampleFormat::PcmUnsigned8,
        16 => SampleFormat::PcmSigned16,
        24 => SampleFormat::PcmSigned24,
        32 => SampleFormat::PcmSigned32,
        _ => return Err(VoiceError::UnsupportedFormat),
    };
    let expected_align = channels
        .checked_mul(sample_format.bytes_per_sample())
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
    let duration_ms = duration_ms_from_frames(frame_count, sample_rate_hz)?;
    Ok(LocalNarration {
        schema_version: "1".into(),
        audio: AudioInfo {
            format: sample_format,
            sample_rate_hz,
            channels,
            frame_count,
            duration_ms,
            sha256: sha256_hex(bytes),
        },
        provenance: AudioProvenance::UserSupplied,
    })
}

/// Runs an alignment provider against non-synthetic user-supplied audio and binds the result.
pub fn measure_alignment(
    provider: &dyn AlignmentProvider,
    request: &AlignmentRequest<'_>,
    narration: &LocalNarration,
) -> Result<MeasuredAlignment, VoiceError> {
    validate_local_narration(narration)?;
    if narration.provenance.is_synthetic() {
        return Err(VoiceError::UnsupportedCapability);
    }
    require_alignment(Some(&provider.capabilities()), request.language)?;
    if request.scene_id.trim().is_empty()
        || request.transcript.trim().is_empty()
        || request.allocation_ms == 0
    {
        return Err(VoiceError::InvalidTiming);
    }
    if request.audio != &narration.audio || sha256_hex(request.wav_bytes) != narration.audio.sha256
    {
        return Err(VoiceError::InvalidAudioData);
    }
    if narration.audio.duration_ms > u64::from(request.allocation_ms) {
        return Err(VoiceError::NarrationOverlong);
    }
    let word_timings = provider.align(request)?;
    validate_word_timings(&word_timings, narration.audio.duration_ms)?;
    Ok(MeasuredAlignment {
        language: request.language.to_owned(),
        audio_sha256: narration.audio.sha256.clone(),
        word_timings,
    })
}

/// Validates alignment spans against audio duration. Empty spans mean no measured captions.
pub fn validate_word_timings(timings: &[WordTiming], duration_ms: u64) -> Result<(), VoiceError> {
    let mut previous_end = 0;
    for timing in timings {
        if timing.text.trim().is_empty()
            || timing.text.chars().count() > MAX_CAPTION_TEXT_CHARS
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

/// Builds a sequential scene timeline from allocations with unverified external captions.
pub fn align_scenes(allocations: &[SceneAllocation]) -> Result<Timeline, VoiceError> {
    let inputs: Vec<AlignedSceneAllocation> = allocations
        .iter()
        .map(|allocation| AlignedSceneAllocation {
            scene_id: allocation.scene_id.clone(),
            duration_ms: allocation.duration_ms,
            audio: allocation.audio.clone(),
            captions: if allocation.word_timings.is_empty() {
                CaptionInput::None
            } else {
                CaptionInput::UnverifiedExternal(allocation.word_timings.clone())
            },
        })
        .collect();
    align_scenes_with_provenance(&inputs)
}

/// Builds a sequential scene timeline using cumulative boundaries rounded once at each edge,
/// preserving audio and caption provenance.
pub fn align_scenes_with_provenance(
    allocations: &[AlignedSceneAllocation],
) -> Result<Timeline, VoiceError> {
    if allocations.is_empty() || allocations.len() > MAX_SCENES {
        return Err(VoiceError::InvalidTiming);
    }
    let mut scenes = Vec::with_capacity(allocations.len());
    let mut elapsed_ms = 0u64;
    let mut scene_ids = HashSet::with_capacity(allocations.len());
    for allocation in allocations {
        if allocation.scene_id.trim().is_empty()
            || !scene_ids.insert(allocation.scene_id.as_str())
            || allocation.duration_ms == 0
        {
            return Err(VoiceError::InvalidTiming);
        }
        validate_local_narration(&allocation.audio)?;
        if allocation.audio.audio.duration_ms > u64::from(allocation.duration_ms) {
            return Err(VoiceError::NarrationOverlong);
        }
        let (caption_provenance, captions) = match &allocation.captions {
            CaptionInput::None => (CaptionProvenance::None, Vec::new()),
            CaptionInput::UnverifiedExternal(timings) => {
                validate_word_timings(timings, allocation.audio.audio.duration_ms)?;
                let provenance = if timings.is_empty() {
                    CaptionProvenance::None
                } else {
                    CaptionProvenance::UnverifiedExternal
                };
                (provenance, timings.clone())
            }
            CaptionInput::Measured(measured) => {
                if allocation.audio.provenance.is_synthetic() {
                    return Err(VoiceError::UnsupportedCapability);
                }
                if measured.audio_sha256 != allocation.audio.audio.sha256 {
                    return Err(VoiceError::InvalidTiming);
                }
                validate_word_timings(&measured.word_timings, allocation.audio.audio.duration_ms)?;
                let provenance = if measured.word_timings.is_empty() {
                    CaptionProvenance::None
                } else {
                    CaptionProvenance::MeasuredProviderAlignment
                };
                (provenance, measured.word_timings.clone())
            }
        };
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
            audio_provenance: allocation.audio.provenance,
            caption_provenance,
            captions,
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
