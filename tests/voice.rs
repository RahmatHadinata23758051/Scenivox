use scenivox::voice::{
    self, AudioProvenance, LocalNarration, NarrationProvider, SceneAllocation, VoiceCapabilities,
    VoiceError, WordTiming,
};
use std::io::Cursor;

fn wav(frames: u32, sample_rate: u32, channels: u16, bits: u16, encoding: u16) -> Vec<u8> {
    let block_align = channels * (bits / 8);
    let data_len = frames * u32::from(block_align);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&encoding.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
    bytes.extend_from_slice(&block_align.to_le_bytes());
    bytes.extend_from_slice(&bits.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    // Stable zero-valued fixture samples. Synthetic silence is test-only, never speech evidence.
    bytes.resize(bytes.len() + data_len as usize, 0);
    bytes
}

fn synthetic(frames: u32, sample_rate: u32) -> LocalNarration {
    let mut narration = voice::validate_wav(&wav(frames, sample_rate, 1, 16, 1)).unwrap();
    narration.provenance = AudioProvenance::SyntheticTestFixture;
    narration
}

#[test]
fn accepts_pcm_wav_and_rejects_bad_or_unsupported_audio() {
    let accepted = voice::read_wav(Cursor::new(wav(16_000, 16_000, 1, 16, 1))).unwrap();
    assert_eq!(accepted.audio.duration_ms, 1_000);
    assert_eq!(accepted.audio.channels, 1);
    assert_eq!(accepted.audio.format, voice::SampleFormat::PcmSigned16);
    assert_eq!(accepted.provenance, AudioProvenance::UserSupplied);

    assert!(matches!(
        voice::validate_wav(b"not wav"),
        Err(VoiceError::InvalidWav)
    ));
    let mut truncated = wav(100, 16_000, 1, 16, 1);
    truncated.pop();
    assert!(matches!(
        voice::validate_wav(&truncated),
        Err(VoiceError::InvalidWav)
    ));
    assert!(matches!(
        voice::validate_wav(&wav(100, 16_000, 1, 16, 3)),
        Err(VoiceError::UnsupportedFormat)
    ));
    assert!(matches!(
        voice::validate_wav(&wav(100, 16_000, 0, 16, 1)),
        Err(VoiceError::AudioLimits)
    ));
    assert!(matches!(
        voice::validate_wav(&wav(100, 16_000, 1, 12, 1)),
        Err(VoiceError::UnsupportedFormat)
    ));
}

#[test]
fn accepts_each_declared_pcm_width_and_rejects_duplicate_scene_ids() {
    for bits in [8, 16, 24, 32] {
        let audio = voice::validate_wav(&wav(16_000, 16_000, 2, bits, 1)).unwrap();
        assert_eq!(audio.audio.channels, 2);
        assert_eq!(audio.audio.duration_ms, 1_000);
    }
    let duplicated = vec![
        SceneAllocation {
            scene_id: "same".into(),
            duration_ms: 100,
            audio: synthetic(160, 16_000),
            word_timings: vec![],
        },
        SceneAllocation {
            scene_id: "same".into(),
            duration_ms: 100,
            audio: synthetic(160, 16_000),
            word_timings: vec![],
        },
    ];
    assert!(matches!(
        voice::align_scenes(&duplicated),
        Err(VoiceError::InvalidTiming)
    ));
}

#[test]
fn rejects_overlapping_word_spans() {
    let spans = [
        WordTiming {
            text: "satu".into(),
            start_ms: 0,
            end_ms: 200,
        },
        WordTiming {
            text: "dua".into(),
            start_ms: 199,
            end_ms: 300,
        },
    ];
    assert!(matches!(
        voice::validate_word_timings(&spans, 400),
        Err(VoiceError::InvalidTiming)
    ));
}

#[test]
fn rejects_corrupt_lengths_alignment_and_duration() {
    let mut bad_size = wav(100, 16_000, 1, 16, 1);
    bad_size[4..8].copy_from_slice(&1u32.to_le_bytes());
    assert!(matches!(
        voice::validate_wav(&bad_size),
        Err(VoiceError::InvalidWav)
    ));

    let mut bad_rate = wav(100, 16_000, 1, 16, 1);
    bad_rate[28..32].copy_from_slice(&1u32.to_le_bytes());
    assert!(matches!(
        voice::validate_wav(&bad_rate),
        Err(VoiceError::InvalidAudioData)
    ));

    assert_eq!(
        voice::validate_wav(&wav(1, 8_000, 1, 16, 1))
            .unwrap()
            .audio
            .duration_ms,
        1
    );
    assert_eq!(
        voice::validate_wav(&wav(480_000, 8_000, 1, 16, 1))
            .unwrap()
            .audio
            .duration_ms,
        60_000
    );
    assert!(matches!(
        voice::validate_wav(&wav(480_008, 8_000, 1, 16, 1)),
        Err(VoiceError::AudioLimits)
    ));
    assert!(matches!(
        voice::read_wav(Cursor::new(vec![0; voice::MAX_WAV_BYTES + 1])),
        Err(VoiceError::AudioTooLarge)
    ));
}

#[test]
fn timing_maps_stable_scene_ids_and_uses_cumulative_frame_boundaries() {
    let allocations = vec![
        SceneAllocation {
            scene_id: "scene-a".into(),
            duration_ms: 17,
            audio: synthetic(160, 16_000),
            word_timings: vec![WordTiming {
                text: "test".into(),
                start_ms: 0,
                end_ms: 10,
            }],
        },
        SceneAllocation {
            scene_id: "scene-b".into(),
            duration_ms: 17,
            audio: synthetic(160, 16_000),
            word_timings: vec![],
        },
        SceneAllocation {
            scene_id: "scene-c".into(),
            duration_ms: 16,
            audio: synthetic(160, 16_000),
            word_timings: vec![],
        },
    ];
    let timeline = voice::align_scenes(&allocations).unwrap();
    assert_eq!(timeline.fps, 30);
    assert_eq!(timeline.total_duration_ms, 50);
    assert_eq!(timeline.total_frames, 2);
    assert_eq!(
        timeline
            .scenes
            .iter()
            .map(|s| s.scene_id.as_str())
            .collect::<Vec<_>>(),
        ["scene-a", "scene-b", "scene-c"]
    );
    assert_eq!(
        timeline
            .scenes
            .iter()
            .map(|s| (s.start_frame, s.end_frame))
            .collect::<Vec<_>>(),
        [(0, 1), (1, 1), (1, 2)]
    );
    assert_eq!(timeline.scenes[0].captions, allocations[0].word_timings);
}

#[test]
fn exact_allocation_is_accepted_but_overlong_audio_is_never_truncated() {
    let exact = SceneAllocation {
        scene_id: "s1".into(),
        duration_ms: 1_000,
        audio: synthetic(16_000, 16_000),
        word_timings: vec![],
    };
    assert_eq!(
        voice::align_scenes(std::slice::from_ref(&exact))
            .unwrap()
            .scenes[0]
            .narration_duration_ms,
        1_000
    );
    let overlong = SceneAllocation {
        audio: synthetic(16_016, 16_000),
        ..exact
    };
    assert!(matches!(
        voice::align_scenes(&[overlong]),
        Err(VoiceError::NarrationOverlong)
    ));
}

#[test]
fn caption_spans_must_be_measured_ordered_and_inside_audio() {
    assert!(
        voice::validate_word_timings(
            &[WordTiming {
                text: "kata".into(),
                start_ms: 0,
                end_ms: 400
            }],
            400
        )
        .is_ok()
    );
    assert!(matches!(
        voice::validate_word_timings(
            &[WordTiming {
                text: "kata".into(),
                start_ms: 2,
                end_ms: 2
            }],
            400
        ),
        Err(VoiceError::InvalidTiming)
    ));
    assert!(matches!(
        voice::validate_word_timings(
            &[WordTiming {
                text: "kata".into(),
                start_ms: 0,
                end_ms: 401
            }],
            400
        ),
        Err(VoiceError::InvalidTiming)
    ));
}

struct NoVoice;
impl NarrationProvider for NoVoice {
    fn capabilities(&self) -> VoiceCapabilities {
        VoiceCapabilities {
            can_synthesize: false,
            can_align: false,
            audio: None,
            languages: vec![],
            remote: false,
        }
    }
    fn synthesize(&self, _: &voice::VoiceRequest) -> Result<Vec<u8>, VoiceError> {
        Err(VoiceError::UnsupportedCapability)
    }
}

#[test]
fn missing_provider_and_unavailable_audio_are_explicit() {
    assert!(matches!(
        voice::require_synthesis(None, "id-ID"),
        Err(VoiceError::ProviderUnavailable)
    ));
    assert!(matches!(
        voice::require_alignment(None, "id-ID"),
        Err(VoiceError::ProviderUnavailable)
    ));
    assert!(matches!(
        voice::require_synthesis(Some(&NoVoice.capabilities()), "id-ID"),
        Err(VoiceError::UnsupportedCapability)
    ));
    let indonesian = VoiceCapabilities {
        can_synthesize: true,
        can_align: true,
        audio: None,
        languages: vec!["id-ID".into()],
        remote: false,
    };
    assert!(voice::require_synthesis(Some(&indonesian), "id-ID").is_ok());
    assert!(voice::require_alignment(Some(&indonesian), "id-ID").is_ok());
    assert!(matches!(
        voice::require_synthesis(Some(&indonesian), "en-US"),
        Err(VoiceError::UnsupportedCapability)
    ));
    assert!(matches!(
        voice::read_wav(Cursor::new(Vec::<u8>::new())),
        Err(VoiceError::InvalidWav)
    ));
}

#[test]
fn voice_cli_inspects_only_confined_explicit_files_without_database() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("narration.wav"),
        wav(16_000, 16_000, 1, 16, 1),
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args(["voice", "inspect", "--file", "narration.wav"])
        .current_dir(dir.path())
        .env_remove("SCENIVOX_DATABASE_URL")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: LocalNarration = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result.audio.duration_ms, 1_000);
    assert_eq!(result.provenance, AudioProvenance::UserSupplied);
    assert_eq!(result.schema_version, "1");

    let traversal = std::process::Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args(["voice", "inspect", "--file", "../outside.wav"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!traversal.status.success());
    assert!(String::from_utf8_lossy(&traversal.stderr).contains("unsafe_audio_path"));
    let absolute = std::process::Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args([
            "voice",
            "inspect",
            "--file",
            dir.path().join("narration.wav").to_str().unwrap(),
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!absolute.status.success());
    assert!(String::from_utf8_lossy(&absolute.stderr).contains("unsafe_audio_path"));
}

#[test]
fn rejects_timeline_longer_than_contract_limit() {
    let allocations = [SceneAllocation {
        scene_id: "long".into(),
        duration_ms: 60_001,
        audio: synthetic(16_000, 16_000),
        word_timings: vec![],
    }];
    assert!(matches!(
        voice::align_scenes(&allocations),
        Err(VoiceError::AudioLimits)
    ));
}
