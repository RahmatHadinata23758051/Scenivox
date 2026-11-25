//! Content-addressed local asset registry storage, bounded WAV validation, integrity verification, and read-only recovery diagnostics (ADR-02 / P2-02B).
//!
//! # Local Filesystem Trust Assumptions & Boundaries
//! - Asset staging and storage reuse the single-user local host trust model documented in `skill.rs`:
//!   canonical containment, segment-by-segment symlink/reparse rejection, opened-handle regular-file
//!   and bounded reads (`<= MAX_WAV_BYTES`), and `sync_all` staging reduce risk but do not eliminate
//!   kernel-level TOCTOU races if an untrusted local user has write access inside source paths.
//! - HTTP imports (`POST /v1/assets`) are **default-deny** unless `SCENIVOX_ASSET_IMPORT_ROOTS`
//!   (`trusted_asset_import_roots`) is explicitly configured; CLI (`scenivox asset register --path`)
//!   retains explicit local operator access under all link, size, and PCM WAV validation bounds.
//! - `reconcile_asset_store` (`scenivox asset reconcile`) is strictly a **read-only diagnostic** that
//!   reports staging directories, unregistered blobs, and missing registered assets without deleting
//!   or adopting anything.

use std::{
    collections::HashSet,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    skill::{
        MAX_ENTRIES_PER_DIR, SkillError, authorize_import_source, ensure_source_dir_safe,
        is_reparse_or_symlink, is_sha256_hex, is_special_or_invalid_file, paths_overlap,
        read_bounded_sorted_entries, sha256_hex, validate_segment_name, verify_no_links_between,
    },
    voice::{
        AudioProvenance, LocalNarration, MAX_WAV_BYTES, VoiceError, validate_local_narration,
        validate_wav,
    },
};

pub const ASSET_SCHEMA_VERSION: &str = "1";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    NarrationWav,
    SfxWav,
}

impl AssetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NarrationWav => "narration_wav",
            Self::SfxWav => "sfx_wav",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "narration_wav" => Some(Self::NarrationWav),
            "sfx_wav" => Some(Self::SfxWav),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterAsset {
    pub schema_version: String,
    pub source_path: String,
    pub kind: AssetKind,
    #[serde(default)]
    pub provenance: Option<AudioProvenance>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetRecord {
    pub schema_version: String,
    pub id: String,
    pub sha256: String,
    pub kind: AssetKind,
    pub provenance: AudioProvenance,
    pub byte_size: u64,
    pub storage_rel_path: String,
    pub metadata: LocalNarration,
    pub created_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetIntegrityReport {
    pub schema_version: String,
    pub id: String,
    pub sha256: String,
    pub kind: AssetKind,
    pub verified: bool,
    pub byte_size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetStoreRecoveryReport {
    pub schema_version: String,
    pub read_only: bool,
    pub staging_dirs: Vec<String>,
    pub unregistered_assets: Vec<String>,
    pub missing_registered_assets: Vec<String>,
    pub registered_assets_present: usize,
}

#[derive(Debug, Error)]
pub enum AssetError {
    #[error("unsafe asset path: {0}")]
    UnsafePath(String),
    #[error("asset import path not authorized: {0}")]
    ImportNotAuthorized(String),
    #[error("audio exceeds {MAX_WAV_BYTES} bytes")]
    AudioTooLarge,
    #[error("unsupported audio format: {0}")]
    UnsupportedAudioFormat(String),
    #[error("audio exceeds supported bounds: {0}")]
    AudioLimitsExceeded(String),
    #[error("invalid audio input: {0}")]
    InvalidAudio(String),
    #[error("asset integrity verification failed: {0}")]
    IntegrityFailed(String),
    #[error("asset store conflict: {0}")]
    InterruptedPublication(String),
    #[error("asset io error: {0}")]
    Io(String),
}

impl From<VoiceError> for AssetError {
    fn from(err: VoiceError) -> Self {
        match err {
            VoiceError::AudioTooLarge => Self::AudioTooLarge,
            VoiceError::UnsupportedFormat => Self::UnsupportedAudioFormat(err.to_string()),
            VoiceError::AudioLimits => Self::AudioLimitsExceeded(err.to_string()),
            VoiceError::ReadFailure => Self::Io(err.to_string()),
            _ => Self::InvalidAudio(err.to_string()),
        }
    }
}

/// Validates that an HTTP-supplied asset `source_path` resides strictly under `trusted_roots`
/// (default-deny when `trusted_roots` is empty).
pub fn authorize_asset_import_source(
    source_path: &Path,
    trusted_roots: &[PathBuf],
) -> Result<(), AssetError> {
    if trusted_roots.is_empty() {
        return Err(AssetError::ImportNotAuthorized(
            "HTTP asset registration is disabled until SCENIVOX_ASSET_IMPORT_ROOTS is configured"
                .into(),
        ));
    }
    authorize_import_source(source_path, trusted_roots).map_err(|err| match err {
        SkillError::ImportNotAuthorized(msg)
        | SkillError::UnsafeBundle(msg)
        | SkillError::Io(msg) => AssetError::ImportNotAuthorized(msg),
        other => AssetError::ImportNotAuthorized(other.to_string()),
    })
}

/// Staged validated WAV asset awaiting atomic SQLite registration.
/// Automatically cleans up staging/published artifacts if dropped before `disarm()`.
pub struct StagedAsset {
    pub sha256: String,
    pub byte_size: u64,
    pub storage_rel_path: String,
    pub metadata: LocalNarration,
    staging_dir: Option<PathBuf>,
    staged_file: PathBuf,
    published_file: Option<PathBuf>,
    target_file: PathBuf,
}

impl StagedAsset {
    pub fn ensure_target_available(&self) -> Result<(), AssetError> {
        if self.target_file.exists() {
            return Err(AssetError::InterruptedPublication(
                "unregistered content-addressed asset file already exists; inspect store before re-registering"
                    .into(),
            ));
        }
        Ok(())
    }

    pub fn publish_file(&mut self) -> Result<(), AssetError> {
        let _staging = self
            .staging_dir
            .as_ref()
            .ok_or_else(|| AssetError::Io("asset staging directory already consumed".into()))?;
        if let Some(parent) = self.target_file.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                AssetError::Io(format!("failed to create asset store subdir: {err}"))
            })?;
        }
        if self.target_file.exists() {
            return Err(AssetError::InterruptedPublication(
                "unregistered content-addressed asset file already exists".into(),
            ));
        }
        fs::rename(&self.staged_file, &self.target_file)
            .map_err(|err| AssetError::Io(format!("failed to publish staged asset file: {err}")))?;
        if let Some(staging_dir) = self.staging_dir.take() {
            let _ = fs::remove_dir_all(staging_dir);
        }
        self.published_file = Some(self.target_file.clone());
        Ok(())
    }

    pub fn disarm(mut self) {
        if let Some(staging_dir) = self.staging_dir.take() {
            let _ = fs::remove_dir_all(staging_dir);
        }
        self.published_file = None;
    }
}

impl Drop for StagedAsset {
    fn drop(&mut self) {
        if let Some(dir) = self.staging_dir.take() {
            let _ = fs::remove_dir_all(dir);
        }
        if let Some(file) = self.published_file.take() {
            let _ = fs::remove_file(&file);
            if let Some(prefix_dir) = file.parent() {
                let _ = fs::remove_dir(prefix_dir);
            }
        }
    }
}

/// Reads and validates a bounded regular WAV file, copying it into `<asset_store_dir>/.staging-<uuid>/asset.wav`.
pub fn stage_local_asset(
    asset_store_dir: &Path,
    source_path: &Path,
    provenance: AudioProvenance,
) -> Result<StagedAsset, AssetError> {
    if source_path.as_os_str().is_empty() {
        return Err(AssetError::UnsafePath(
            "asset source_path must not be empty".into(),
        ));
    }
    let raw_str = source_path
        .to_str()
        .ok_or_else(|| AssetError::UnsafePath("source_path is not valid UTF-8".into()))?;
    if raw_str.chars().any(|c| c == '\0' || c == '\r' || c == '\n') {
        return Err(AssetError::UnsafePath(
            "source_path contains forbidden control characters".into(),
        ));
    }

    let source_meta = fs::symlink_metadata(source_path).map_err(|err| {
        AssetError::Io(format!(
            "asset source '{}' unavailable: {err}",
            source_path.display()
        ))
    })?;
    if is_reparse_or_symlink(&source_meta) {
        return Err(AssetError::UnsafePath(
            "asset source_path must not be a symlink or reparse point".into(),
        ));
    }
    if is_special_or_invalid_file(&source_meta) || !source_meta.is_file() {
        return Err(AssetError::UnsafePath(
            "asset source_path must be a regular file".into(),
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if source_meta.nlink() > 1 {
            return Err(AssetError::UnsafePath(format!(
                "hardlinked asset source is prohibited (nlink={})",
                source_meta.nlink()
            )));
        }
    }

    let file_name = source_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| AssetError::UnsafePath("asset filename must be valid UTF-8".into()))?;
    validate_segment_name(file_name).map_err(|err| AssetError::UnsafePath(err.to_string()))?;

    let source_canon = fs::canonicalize(source_path).map_err(|err| {
        AssetError::Io(format!(
            "failed to canonicalize asset source '{}': {err}",
            source_path.display()
        ))
    })?;
    let source_parent = source_canon.parent().ok_or_else(|| {
        AssetError::UnsafePath("asset source must reside inside a valid parent directory".into())
    })?;
    ensure_source_dir_safe(source_parent).map_err(|err| AssetError::UnsafePath(err.to_string()))?;

    let store_canon = canonicalize_or_init_asset_store_dir(asset_store_dir)?;
    if paths_overlap(&source_canon, &store_canon) {
        return Err(AssetError::UnsafePath(
            "asset source_path must not reside inside asset_store_dir".into(),
        ));
    }

    let bytes = read_bounded_wav_bytes(&source_canon)?;
    let byte_size = bytes.len() as u64;
    let mut narration = validate_wav(&bytes)?;
    narration.provenance = provenance;
    validate_local_narration(&narration)?;
    let sha256 = narration.audio.sha256.clone();

    let staging_dir = store_canon.join(format!(".staging-{}", Uuid::new_v4()));
    fs::create_dir_all(&staging_dir)
        .map_err(|err| AssetError::Io(format!("failed creating asset staging dir: {err}")))?;
    let staged_file = staging_dir.join("asset.wav");

    let write_res = (|| -> std::io::Result<()> {
        let mut f = File::create(&staged_file)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_res {
        let _ = fs::remove_dir_all(&staging_dir);
        return Err(AssetError::Io(format!(
            "failed staging asset payload: {err}"
        )));
    }

    let storage_rel_path = expected_rel_path(&sha256);
    let target_file = store_canon.join(&sha256[..2]).join(format!("{sha256}.wav"));

    Ok(StagedAsset {
        sha256,
        byte_size,
        storage_rel_path,
        metadata: narration,
        staging_dir: Some(staging_dir),
        staged_file,
        published_file: None,
        target_file,
    })
}

/// Read-only integrity verification of a stored content-addressed asset against its SQLite record.
pub fn verify_stored_asset(
    asset_store_dir: &Path,
    record: &AssetRecord,
) -> Result<AssetIntegrityReport, AssetError> {
    if !is_sha256_hex(&record.sha256)
        || record.storage_rel_path != expected_rel_path(&record.sha256)
    {
        return Err(AssetError::IntegrityFailed(
            "stored asset record contains invalid digest or storage_rel_path".into(),
        ));
    }

    let store_meta = fs::symlink_metadata(asset_store_dir).map_err(|err| {
        AssetError::IntegrityFailed(format!(
            "asset store '{}' missing or unreadable: {err}",
            asset_store_dir.display()
        ))
    })?;
    if is_reparse_or_symlink(&store_meta) || !store_meta.is_dir() {
        return Err(AssetError::IntegrityFailed(
            "asset_store_dir is not a real non-symlink directory".into(),
        ));
    }
    let store_canon = fs::canonicalize(asset_store_dir).map_err(|err| {
        AssetError::IntegrityFailed(format!("failed to canonicalize asset_store_dir: {err}"))
    })?;

    let stored_file = store_canon
        .join(&record.sha256[..2])
        .join(format!("{}.wav", record.sha256));
    verify_no_links_between(&store_canon, &stored_file).map_err(|err| {
        AssetError::IntegrityFailed(format!("stored asset link violation: {err}"))
    })?;

    let meta = fs::symlink_metadata(&stored_file).map_err(|err| {
        AssetError::IntegrityFailed(format!(
            "stored asset '{}' missing: {err}",
            record.storage_rel_path
        ))
    })?;
    if is_reparse_or_symlink(&meta) || is_special_or_invalid_file(&meta) || !meta.is_file() {
        return Err(AssetError::IntegrityFailed(
            "stored asset must be a regular non-symlink file".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() > 1 {
            return Err(AssetError::IntegrityFailed(
                "stored asset has forbidden hardlinks".into(),
            ));
        }
    }

    let bytes = read_bounded_wav_bytes(&stored_file).map_err(|err| {
        AssetError::IntegrityFailed(format!("stored asset failed bounded read: {err}"))
    })?;
    let observed_size = bytes.len() as u64;
    let observed_sha = sha256_hex(&bytes);
    let observed_wav = validate_wav(&bytes).map_err(|err| {
        AssetError::IntegrityFailed(format!("stored asset failed WAV validation: {err}"))
    })?;

    if observed_sha != record.sha256
        || observed_size != record.byte_size
        || observed_wav.audio != record.metadata.audio
    {
        return Err(AssetError::IntegrityFailed(format!(
            "stored asset integrity mismatch (expected sha={}, bytes={}; observed sha={}, bytes={})",
            record.sha256, record.byte_size, observed_sha, observed_size
        )));
    }

    Ok(AssetIntegrityReport {
        schema_version: ASSET_SCHEMA_VERSION.into(),
        id: record.id.clone(),
        sha256: observed_sha,
        kind: record.kind,
        verified: true,
        byte_size: observed_size,
    })
}

/// Read-only diagnostic inspection comparing `asset_store_dir` against registered `assets.storage_rel_path` rows.
/// Never deletes files or adopts unregistered payloads.
pub fn inspect_asset_store_artifacts(
    asset_store_dir: &Path,
    registered_rel_paths: &HashSet<String>,
) -> Result<AssetStoreRecoveryReport, AssetError> {
    if !asset_store_dir.exists() {
        let mut missing: Vec<String> = registered_rel_paths.iter().cloned().collect();
        missing.sort();
        return Ok(AssetStoreRecoveryReport {
            schema_version: ASSET_SCHEMA_VERSION.into(),
            read_only: true,
            staging_dirs: Vec::new(),
            unregistered_assets: Vec::new(),
            missing_registered_assets: missing,
            registered_assets_present: 0,
        });
    }

    let meta = fs::symlink_metadata(asset_store_dir)
        .map_err(|err| AssetError::Io(format!("asset_store_dir unavailable: {err}")))?;
    if is_reparse_or_symlink(&meta) || !meta.is_dir() {
        return Err(AssetError::UnsafePath(
            "asset_store_dir must be a real directory".into(),
        ));
    }

    let mut staging_dirs = Vec::new();
    let mut unregistered_assets = Vec::new();
    let mut seen_registered = HashSet::new();

    for (name, path) in read_bounded_sorted_entries(asset_store_dir, MAX_ENTRIES_PER_DIR)
        .map_err(|err| AssetError::Io(err.to_string()))?
    {
        let entry_meta = fs::symlink_metadata(&path)
            .map_err(|err| AssetError::Io(format!("metadata failed inspecting store: {err}")))?;
        if name.starts_with(".staging-") {
            if entry_meta.is_dir() && !is_reparse_or_symlink(&entry_meta) {
                staging_dirs.push(name);
            }
            continue;
        }
        if !entry_meta.is_dir() || is_reparse_or_symlink(&entry_meta) {
            continue;
        }

        for (file_name, _file_path) in read_bounded_sorted_entries(&path, MAX_ENTRIES_PER_DIR)
            .map_err(|err| AssetError::Io(err.to_string()))?
        {
            let rel = format!("{name}/{file_name}");
            if registered_rel_paths.contains(&rel) {
                seen_registered.insert(rel);
            } else {
                unregistered_assets.push(rel);
            }
        }
    }

    let mut missing: Vec<String> = registered_rel_paths
        .difference(&seen_registered)
        .cloned()
        .collect();
    staging_dirs.sort();
    unregistered_assets.sort();
    missing.sort();

    Ok(AssetStoreRecoveryReport {
        schema_version: ASSET_SCHEMA_VERSION.into(),
        read_only: true,
        staging_dirs,
        unregistered_assets,
        missing_registered_assets: missing,
        registered_assets_present: seen_registered.len(),
    })
}

pub fn expected_rel_path(sha256: &str) -> String {
    format!("{}/{}.wav", &sha256[..2], sha256)
}

fn canonicalize_or_init_asset_store_dir(asset_store_dir: &Path) -> Result<PathBuf, AssetError> {
    if asset_store_dir.as_os_str().is_empty() {
        return Err(AssetError::UnsafePath(
            "asset_store_dir must not be empty".into(),
        ));
    }
    if let Ok(meta) = fs::symlink_metadata(asset_store_dir) {
        if is_reparse_or_symlink(&meta) || !meta.is_dir() {
            return Err(AssetError::UnsafePath(
                "asset_store_dir must be a real non-symlink directory".into(),
            ));
        }
    } else {
        fs::create_dir_all(asset_store_dir).map_err(|err| {
            AssetError::Io(format!(
                "failed to initialize asset_store_dir '{}': {err}",
                asset_store_dir.display()
            ))
        })?;
    }
    fs::canonicalize(asset_store_dir).map_err(|err| {
        AssetError::Io(format!(
            "failed to canonicalize asset_store_dir '{}': {err}",
            asset_store_dir.display()
        ))
    })
}

fn read_bounded_wav_bytes(path: &Path) -> Result<Vec<u8>, AssetError> {
    let file = File::open(path)
        .map_err(|err| AssetError::Io(format!("failed to open '{}': {err}", path.display())))?;
    let opened_meta = file.metadata().map_err(|err| {
        AssetError::UnsafePath(format!(
            "opened metadata unavailable for '{}': {err}",
            path.display()
        ))
    })?;
    if !opened_meta.is_file() {
        return Err(AssetError::UnsafePath(
            "opened asset target is not a regular file".into(),
        ));
    }
    if opened_meta.len() > MAX_WAV_BYTES as u64 {
        return Err(AssetError::AudioTooLarge);
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(opened_meta.len().min(MAX_WAV_BYTES as u64)).unwrap_or(0),
    );
    file.take((MAX_WAV_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|err| AssetError::Io(format!("failed reading '{}': {err}", path.display())))?;
    if bytes.len() > MAX_WAV_BYTES {
        return Err(AssetError::AudioTooLarge);
    }
    Ok(bytes)
}
