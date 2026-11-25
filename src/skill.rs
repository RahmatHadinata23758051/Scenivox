//! Local animation skill bundle validation, secure staging, integrity verification, and recovery (ADR-02 / P2-01B).
//!
//! # Local Filesystem Trust Assumptions, Digest Limits & Recovery Boundaries
//! - Skill staging and storage assume a trusted single-user local host filesystem where no
//!   concurrent untrusted local attacker races renames beneath `skill_store_dir` or approved
//!   import roots. Segment-by-segment `symlink_metadata`, opened-handle regular-file verification,
//!   bound-checked streaming reads, and canonical containment checks are defense-in-depth and do
//!   not eliminate kernel-level TOCTOU races if an untrusted party has write access to source dirs.
//! - On Windows, stable `std::os::windows::fs::MetadataExt` does not expose hardlink counts (`nlink`)
//!   without extra crates/FFI; Unix enforces `meta.nlink() == 1`, while all platforms copy bytes
//!   into new staged files rather than linking. On case-insensitive filesystems (NTFS), colliding
//!   cased filenames cannot coexist inside a single source dir, while cross-platform lowercase checks
//!   prevent case-alias collisions.
//! - `bundle_sha256` hashes canonical RFC 8785 JCS metadata over sorted relative POSIX paths, file
//!   byte sizes, and raw file SHA-256 digests; it intentionally does not normalize CRLF/LF line
//!   endings and does not encode file permissions/mode bits or filesystem timestamps.
//! - HTTP imports (`POST /v1/skills:import`) are **default-deny** unless `trusted_skill_import_roots`
//!   is explicitly configured on the server; CLI imports (`scenivox skill import --path`) retain
//!   explicit local operator access under all bundle structural and symlink/link/size guards.
//! - `reconcile_skill_store` (`scenivox skill reconcile`) is strictly a **read-only diagnostic**:
//!   it reports orphaned staging dirs, unregistered bundles, and missing registered bundles without
//!   deleting files or adopting unregistered bundles. Destructive store cleanup remains deferred
//!   until exclusive store locking and explicit operator authorization exist.
//! - ZIP archive extraction is deferred (requires an explicitly approved ZIP/DEFLATE crate); `.zip`
//!   files and ZIP magic headers are rejected deterministically without running external binaries.

use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

pub const SKILL_SCHEMA_VERSION: &str = "1";
pub const MAX_BUNDLE_FILES: usize = 512;
pub const MAX_BUNDLE_DIRS: usize = 128;
pub const MAX_TOTAL_ENTRIES: usize = MAX_BUNDLE_FILES + MAX_BUNDLE_DIRS;
pub const MAX_ENTRIES_PER_DIR: usize = 512;
pub const MAX_REL_PATH_LEN: usize = 512;
pub const MAX_BUNDLE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_SINGLE_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
pub const MAX_SKILL_MD_BYTES: u64 = 1024 * 1024;
pub const MAX_DIR_DEPTH: usize = 12;
pub const MAX_MANIFEST_ITEMS: usize = 64;
pub const MAX_ENTRYPOINTS: usize = 16;

#[derive(Debug, Error)]
pub enum SkillError {
    #[error("invalid skill manifest: {0}")]
    InvalidManifest(String),
    #[error("unsafe skill bundle: {0}")]
    UnsafeBundle(String),
    #[error("zip archive import unavailable without an approved ZIP/DEFLATE crate: {0}")]
    UnsupportedArchive(String),
    #[error("skill import path not authorized: {0}")]
    ImportNotAuthorized(String),
    #[error("skill bundle integrity verification failed: {0}")]
    IntegrityFailed(String),
    #[error("skill store conflict: {0}")]
    InterruptedPublication(String),
    #[error("skill import io error: {0}")]
    Io(String),
}

impl SkillError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidManifest(_) => "invalid_skill_manifest",
            Self::UnsafeBundle(_) => "unsafe_skill_bundle",
            Self::UnsupportedArchive(_) => "unsupported_archive_format",
            Self::ImportNotAuthorized(_) => "skill_import_not_authorized",
            Self::IntegrityFailed(_) => "skill_integrity_failed",
            Self::InterruptedPublication(_) => "interrupted_skill_publication",
            Self::Io(_) => "input_unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditStatus {
    Unaudited,
    StaticAudited,
    SmokeVerified,
    Rejected,
}

impl AuditStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unaudited => "unaudited",
            Self::StaticAudited => "static_audited",
            Self::SmokeVerified => "smoke_verified",
            Self::Rejected => "rejected",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "unaudited" => Some(Self::Unaudited),
            "static_audited" => Some(Self::StaticAudited),
            "smoke_verified" => Some(Self::SmokeVerified),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillKind {
    UnifiedComposition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompositionMode {
    UnifiedTimeline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeLanguage {
    Javascript,
    Python,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntrypointRuntime {
    Node,
    Python,
    Ffmpeg,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeGenerationPolicy {
    pub languages: Vec<CodeLanguage>,
    pub writable_paths: Vec<String>,
    pub requires_sandbox: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillEntrypoint {
    pub runtime: EntrypointRuntime,
    pub script: String,
    pub args_template: Vec<String>,
    pub timeout_sec: u64,
}

/// Strict v1 manifest; `audit_status` is host-owned and forbidden here (`deny_unknown_fields`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillManifest {
    pub schema_version: String,
    pub id: String,
    pub version: String,
    pub kind: SkillKind,
    pub composition_mode: CompositionMode,
    pub rendering_strategy: String,
    pub supported_aspect_ratios: Vec<String>,
    pub max_duration_ms: Option<u32>,
    pub supports_independent_scene_render: bool,
    pub code_generation_policy: CodeGenerationPolicy,
    pub entrypoints: BTreeMap<String, SkillEntrypoint>,
    pub requires: Vec<String>,
    pub accepted_inputs: Vec<String>,
    pub produced_outputs: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillLayoutSummary {
    pub has_skill_md: bool,
    pub references_count: usize,
    pub scripts_count: usize,
    pub assets_count: usize,
    pub file_count: usize,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillRecord {
    pub schema_version: String,
    pub skill_id: String,
    pub skill_version: String,
    pub bundle_sha256: String,
    pub enabled: bool,
    pub audit_status: AuditStatus,
    pub storage_rel_dir: String,
    pub file_count: u64,
    pub total_bytes: u64,
    pub imported_at: String,
    pub layout: SkillLayoutSummary,
    pub manifest: SkillManifest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportSkill {
    pub schema_version: String,
    pub source_path: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillIntegrityReport {
    pub schema_version: String,
    pub skill_id: String,
    pub skill_version: String,
    pub bundle_sha256: String,
    pub verified: bool,
    pub file_count: u64,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillStoreRecoveryReport {
    pub schema_version: String,
    pub read_only: bool,
    pub staging_dirs: Vec<String>,
    pub unregistered_bundles: Vec<String>,
    pub missing_registered_bundles: Vec<String>,
    pub registered_bundles_present: usize,
}

/// Staged bundle awaiting atomic SQLite registration; cleans up automatically unless disarmed.
pub struct StagedSkillBundle {
    pub manifest: SkillManifest,
    pub layout: SkillLayoutSummary,
    pub bundle_sha256: String,
    pub storage_rel_dir: String,
    staging_dir: Option<PathBuf>,
    published_dir: Option<PathBuf>,
    target_dir: PathBuf,
}

impl StagedSkillBundle {
    pub fn ensure_target_available(&self) -> Result<(), SkillError> {
        if self.target_dir.exists() {
            return Err(SkillError::InterruptedPublication(
                "unregistered target skill directory already exists; inspect store before re-importing"
                    .into(),
            ));
        }
        Ok(())
    }

    pub fn publish_files(&mut self) -> Result<(), SkillError> {
        // Keep `self.staging_dir` armed until `fs::rename` completes so `Drop` cleans staging on any error.
        let staging = self
            .staging_dir
            .clone()
            .ok_or_else(|| SkillError::Io("staging directory already consumed".into()))?;
        if let Some(parent) = self.target_dir.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                SkillError::Io(format!("failed to create skill store dir: {err}"))
            })?;
        }
        if self.target_dir.exists() {
            return Err(SkillError::InterruptedPublication(
                "unregistered target skill directory already exists".into(),
            ));
        }
        fs::rename(&staging, &self.target_dir).map_err(|err| {
            SkillError::Io(format!(
                "failed to move staged bundle into skill store: {err}"
            ))
        })?;
        self.staging_dir = None;
        self.published_dir = Some(self.target_dir.clone());
        Ok(())
    }

    pub fn disarm(mut self) {
        self.staging_dir = None;
        self.published_dir = None;
    }
}

impl Drop for StagedSkillBundle {
    fn drop(&mut self) {
        if let Some(dir) = self.staging_dir.take() {
            let _ = fs::remove_dir_all(dir);
        }
        if let Some(dir) = self.published_dir.take() {
            let _ = fs::remove_dir_all(&dir);
            prune_empty_skill_ancestors(&dir);
        }
    }
}

fn prune_empty_skill_ancestors(target_dir: &Path) {
    if let Some(version_dir) = target_dir.parent() {
        let _ = fs::remove_dir(version_dir);
        if let Some(id_dir) = version_dir.parent() {
            let _ = fs::remove_dir(id_dir);
        }
    }
}

#[derive(Serialize)]
struct BundleDigestEntry {
    path: String,
    sha256: String,
    size: u64,
}

/// Validates that an HTTP-supplied `source_path` resides strictly within one of the configured
/// `trusted_roots` (default-deny when `trusted_roots` is empty).
pub fn authorize_import_source(
    source_path: &Path,
    trusted_roots: &[PathBuf],
) -> Result<(), SkillError> {
    if trusted_roots.is_empty() {
        return Err(SkillError::ImportNotAuthorized(
            "HTTP skill import is disabled until SCENIVOX_SKILL_IMPORT_ROOTS is configured".into(),
        ));
    }
    if source_path.as_os_str().is_empty() {
        return Err(SkillError::ImportNotAuthorized(
            "source_path must not be empty".into(),
        ));
    }
    let meta = fs::symlink_metadata(source_path).map_err(|err| {
        SkillError::ImportNotAuthorized(format!(
            "source_path '{}' is unavailable or unauthorized: {err}",
            source_path.display()
        ))
    })?;
    if is_reparse_or_symlink(&meta) {
        return Err(SkillError::ImportNotAuthorized(
            "symlink or reparse source_path is not authorized".into(),
        ));
    }
    let source_canon = fs::canonicalize(source_path).map_err(|err| {
        SkillError::ImportNotAuthorized(format!(
            "source_path '{}' cannot be resolved: {err}",
            source_path.display()
        ))
    })?;

    for root in trusted_roots {
        if root.as_os_str().is_empty() {
            continue;
        }
        let Ok(root_meta) = fs::symlink_metadata(root) else {
            continue;
        };
        if is_reparse_or_symlink(&root_meta) || !root_meta.is_dir() {
            continue;
        }
        let Ok(root_canon) = fs::canonicalize(root) else {
            continue;
        };
        if ensure_source_dir_safe(&root_canon).is_err() {
            continue;
        }
        if source_canon.starts_with(&root_canon)
            && source_canon != root_canon
            && verify_no_links_between(&root_canon, &source_canon).is_ok()
        {
            return Ok(());
        }
    }

    Err(SkillError::ImportNotAuthorized(format!(
        "source_path '{}' is outside configured trusted_skill_import_roots",
        source_path.display()
    )))
}

/// Validates and copies a local skill directory into an isolated `.staging-<uuid>` directory,
/// checking traversal bounds, overlap with `skill_store_dir`, link invariants, and strict manifest rules.
pub fn stage_local_skill(
    skill_store_dir: &Path,
    source_path: &Path,
) -> Result<StagedSkillBundle, SkillError> {
    if source_path.as_os_str().is_empty() {
        return Err(SkillError::UnsafeBundle(
            "skill source_path must not be empty".into(),
        ));
    }
    let raw_str = source_path
        .to_str()
        .ok_or_else(|| SkillError::UnsafeBundle("source_path is not valid UTF-8".into()))?;
    if raw_str.chars().any(|c| c == '\0' || c == '\r' || c == '\n') {
        return Err(SkillError::UnsafeBundle(
            "source_path contains forbidden control characters".into(),
        ));
    }

    let source_meta = fs::symlink_metadata(source_path).map_err(|err| {
        SkillError::Io(format!(
            "skill source '{}' unavailable: {err}",
            source_path.display()
        ))
    })?;
    if is_reparse_or_symlink(&source_meta) {
        return Err(SkillError::UnsafeBundle(
            "skill source root must not be a symlink or reparse point".into(),
        ));
    }

    if source_meta.is_file() {
        if is_zip_candidate(source_path)? {
            return Err(SkillError::UnsupportedArchive(
                "`.zip` bundle import requires an approved ZIP/DEFLATE crate dependency (none installed; import a local directory instead)".into(),
            ));
        }
        return Err(SkillError::UnsafeBundle(
            "skill source must be a directory (or approved ZIP archive once enabled)".into(),
        ));
    }
    if !source_meta.is_dir() {
        return Err(SkillError::UnsafeBundle(
            "skill source must be a regular directory".into(),
        ));
    }

    let source_canon = fs::canonicalize(source_path).map_err(|err| {
        SkillError::Io(format!(
            "failed to canonicalize skill source '{}': {err}",
            source_path.display()
        ))
    })?;
    ensure_source_dir_safe(&source_canon)?;

    let store_canon = canonicalize_or_init_store_dir(skill_store_dir)?;
    if paths_overlap(&source_canon, &store_canon) {
        return Err(SkillError::UnsafeBundle(
            "skill source directory must not equal or overlap skill_store_dir".into(),
        ));
    }

    let staging_dir = store_canon.join(format!(".staging-{}", Uuid::new_v4()));
    fs::create_dir_all(&staging_dir)
        .map_err(|err| SkillError::Io(format!("failed to create staging dir: {err}")))?;

    let mut staged = StagedSkillBundle {
        manifest: placeholder_manifest(),
        layout: SkillLayoutSummary {
            has_skill_md: false,
            references_count: 0,
            scripts_count: 0,
            assets_count: 0,
            file_count: 0,
            total_bytes: 0,
        },
        bundle_sha256: String::new(),
        storage_rel_dir: String::new(),
        staging_dir: Some(staging_dir.clone()),
        published_dir: None,
        target_dir: PathBuf::new(),
    };

    let mut files_meta: BTreeMap<String, (u64, String)> = BTreeMap::new();
    let mut seen_lower: HashSet<String> = HashSet::new();
    let mut dir_count = 0usize;
    let mut total_entries = 0usize;
    let mut total_bytes = 0u64;
    let mut manifest_bytes: Option<Vec<u8>> = None;
    let mut skill_md_bytes: Option<Vec<u8>> = None;

    let mut stack: Vec<(PathBuf, Vec<String>)> = vec![(source_canon.clone(), Vec::new())];
    while let Some((dir, rel_segments)) = stack.pop() {
        if rel_segments.len() > MAX_DIR_DEPTH {
            return Err(SkillError::UnsafeBundle(format!(
                "skill bundle exceeds max directory depth of {MAX_DIR_DEPTH}"
            )));
        }

        let entries = read_bounded_sorted_entries(&dir, MAX_ENTRIES_PER_DIR)?;
        for (name, path) in entries {
            total_entries += 1;
            if total_entries > MAX_TOTAL_ENTRIES {
                return Err(SkillError::UnsafeBundle(format!(
                    "skill bundle exceeds maximum total entry limit of {MAX_TOTAL_ENTRIES}"
                )));
            }
            validate_segment_name(&name)?;

            let mut next_segments = rel_segments.clone();
            next_segments.push(name);
            let rel_posix = next_segments.join("/");
            if rel_posix.len() > MAX_REL_PATH_LEN {
                return Err(SkillError::UnsafeBundle(format!(
                    "relative path '{rel_posix}' exceeds {MAX_REL_PATH_LEN} byte bound"
                )));
            }
            let rel_lower = rel_posix.to_lowercase();
            if !seen_lower.insert(rel_lower) {
                return Err(SkillError::UnsafeBundle(format!(
                    "duplicate or case-colliding path '{rel_posix}' in skill bundle"
                )));
            }

            let meta = fs::symlink_metadata(&path).map_err(|err| {
                SkillError::UnsafeBundle(format!("metadata failed for '{rel_posix}': {err}"))
            })?;
            if is_reparse_or_symlink(&meta) {
                return Err(SkillError::UnsafeBundle(format!(
                    "symlink or reparse point '{rel_posix}' is prohibited"
                )));
            }
            if is_special_or_invalid_file(&meta) {
                return Err(SkillError::UnsafeBundle(format!(
                    "special file '{rel_posix}' is prohibited"
                )));
            }

            if meta.is_dir() {
                dir_count += 1;
                if dir_count > MAX_BUNDLE_DIRS {
                    return Err(SkillError::UnsafeBundle(format!(
                        "skill bundle exceeds maximum directory count of {MAX_BUNDLE_DIRS}"
                    )));
                }
                let staged_subdir = staging_dir.join(rel_to_native(&next_segments));
                fs::create_dir_all(&staged_subdir).map_err(|err| {
                    SkillError::Io(format!(
                        "failed to create staged subdir '{rel_posix}': {err}"
                    ))
                })?;
                stack.push((path, next_segments));
                continue;
            }

            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if meta.nlink() > 1 {
                    return Err(SkillError::UnsafeBundle(format!(
                        "hardlinked file '{rel_posix}' is prohibited (nlink={})",
                        meta.nlink()
                    )));
                }
            }

            if files_meta.len() >= MAX_BUNDLE_FILES {
                return Err(SkillError::UnsafeBundle(format!(
                    "skill bundle exceeds maximum file count of {MAX_BUNDLE_FILES}"
                )));
            }

            let content = read_bounded_regular_file(&path, &rel_posix)?;
            let byte_len = content.len() as u64;

            total_bytes = total_bytes.saturating_add(byte_len);
            if total_bytes > MAX_BUNDLE_BYTES {
                return Err(SkillError::UnsafeBundle(format!(
                    "skill bundle total size exceeds {MAX_BUNDLE_BYTES} bytes"
                )));
            }

            if rel_posix == "skill.manifest.json" {
                if byte_len > MAX_MANIFEST_BYTES {
                    return Err(SkillError::InvalidManifest(
                        "skill.manifest.json exceeds 1 MiB".into(),
                    ));
                }
                manifest_bytes = Some(content.clone());
            } else if rel_posix == "SKILL.md" {
                if byte_len > MAX_SKILL_MD_BYTES {
                    return Err(SkillError::InvalidManifest("SKILL.md exceeds 1 MiB".into()));
                }
                skill_md_bytes = Some(content.clone());
            }

            let file_sha256 = sha256_hex(&content);
            let dst = staging_dir.join(rel_to_native(&next_segments));
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)
                    .map_err(|err| SkillError::Io(format!("staging parent error: {err}")))?;
            }
            fs::write(&dst, &content)
                .map_err(|err| SkillError::Io(format!("failed writing '{rel_posix}': {err}")))?;

            files_meta.insert(rel_posix, (byte_len, file_sha256));
        }
    }

    let raw_manifest = manifest_bytes.ok_or_else(|| {
        SkillError::InvalidManifest("missing required root file 'skill.manifest.json'".into())
    })?;
    let raw_skill_md = skill_md_bytes.ok_or_else(|| {
        SkillError::InvalidManifest("missing required root file 'SKILL.md'".into())
    })?;
    let skill_md_text = std::str::from_utf8(&raw_skill_md)
        .map_err(|_| SkillError::InvalidManifest("SKILL.md must be valid UTF-8".into()))?;
    if skill_md_text.trim().is_empty() {
        return Err(SkillError::InvalidManifest(
            "SKILL.md must not be blank".into(),
        ));
    }

    let manifest: SkillManifest = crate::json::parse(&raw_manifest).map_err(|code| {
        SkillError::InvalidManifest(format!(
            "skill.manifest.json failed strict schema/duplicate-key validation ({code})"
        ))
    })?;
    validate_manifest(&manifest, &files_meta)?;

    let layout = summarize_layout(&files_meta, total_bytes);
    let bundle_sha256 = compute_bundle_sha256(&files_meta)?;
    let storage_rel_dir = format!("{}/{}/{}", manifest.id, manifest.version, bundle_sha256);
    let target_dir = store_canon
        .join(&manifest.id)
        .join(&manifest.version)
        .join(&bundle_sha256);

    staged.manifest = manifest;
    staged.layout = layout;
    staged.bundle_sha256 = bundle_sha256;
    staged.storage_rel_dir = storage_rel_dir;
    staged.target_dir = target_dir;
    Ok(staged)
}

/// Read-only verification of a persisted bundle on disk against its registered SQLite record.
/// Detects missing, modified, or unexpected extra files, unsafe paths, reserved names,
/// case aliases, symlinks/reparse points, special files, and manifest mismatches.
pub fn verify_stored_bundle(
    skill_store_dir: &Path,
    record: &SkillRecord,
) -> Result<SkillIntegrityReport, SkillError> {
    let expected_rel = format!(
        "{}/{}/{}",
        record.skill_id, record.skill_version, record.bundle_sha256
    );
    if record.storage_rel_dir != expected_rel
        || !is_safe_skill_id(&record.skill_id)
        || !is_safe_skill_version(&record.skill_version)
        || !is_sha256_hex(&record.bundle_sha256)
    {
        return Err(SkillError::IntegrityFailed(
            "stored skill record contains unsafe or inconsistent storage_rel_dir".into(),
        ));
    }

    let store_meta = fs::symlink_metadata(skill_store_dir).map_err(|err| {
        SkillError::IntegrityFailed(format!(
            "skill store '{}' missing or unreadable: {err}",
            skill_store_dir.display()
        ))
    })?;
    if is_reparse_or_symlink(&store_meta) || !store_meta.is_dir() {
        return Err(SkillError::IntegrityFailed(
            "skill_store_dir is not a real non-symlink directory".into(),
        ));
    }
    let store_canon = fs::canonicalize(skill_store_dir).map_err(|err| {
        SkillError::IntegrityFailed(format!("failed to canonicalize skill_store_dir: {err}"))
    })?;

    let stored_dir = store_canon
        .join(&record.skill_id)
        .join(&record.skill_version)
        .join(&record.bundle_sha256);
    verify_no_links_between(&store_canon, &stored_dir)
        .map_err(|err| SkillError::IntegrityFailed(format!("stored path link violation: {err}")))?;

    let stored_meta = fs::symlink_metadata(&stored_dir).map_err(|err| {
        SkillError::IntegrityFailed(format!(
            "stored skill bundle directory '{}' missing: {err}",
            record.storage_rel_dir
        ))
    })?;
    if is_reparse_or_symlink(&stored_meta) || !stored_meta.is_dir() {
        return Err(SkillError::IntegrityFailed(
            "stored bundle root must be a regular non-symlink directory".into(),
        ));
    }

    let mut files_meta: BTreeMap<String, (u64, String)> = BTreeMap::new();
    let mut seen_lower: HashSet<String> = HashSet::new();
    let mut dir_count = 0usize;
    let mut total_entries = 0usize;
    let mut total_bytes = 0u64;
    let mut manifest_bytes: Option<Vec<u8>> = None;
    let mut skill_md_present = false;

    let mut stack: Vec<(PathBuf, Vec<String>)> = vec![(stored_dir, Vec::new())];
    while let Some((dir, rel_segments)) = stack.pop() {
        if rel_segments.len() > MAX_DIR_DEPTH {
            return Err(SkillError::IntegrityFailed(
                "stored bundle exceeded max directory depth".into(),
            ));
        }
        let entries = read_bounded_sorted_entries(&dir, MAX_ENTRIES_PER_DIR).map_err(|err| {
            SkillError::IntegrityFailed(format!("stored directory read failure: {err}"))
        })?;
        for (name, path) in entries {
            total_entries += 1;
            if total_entries > MAX_TOTAL_ENTRIES {
                return Err(SkillError::IntegrityFailed(
                    "stored bundle exceeded entry count bound".into(),
                ));
            }
            validate_segment_name(&name).map_err(|err| {
                SkillError::IntegrityFailed(format!("unsafe filename in stored bundle: {err}"))
            })?;

            let mut next_segments = rel_segments.clone();
            next_segments.push(name);
            let rel_posix = next_segments.join("/");
            if rel_posix.len() > MAX_REL_PATH_LEN {
                return Err(SkillError::IntegrityFailed(
                    "stored relative path exceeded max length".into(),
                ));
            }
            if !seen_lower.insert(rel_posix.to_lowercase()) {
                return Err(SkillError::IntegrityFailed(format!(
                    "case-colliding stored path '{rel_posix}'"
                )));
            }

            let meta = fs::symlink_metadata(&path).map_err(|err| {
                SkillError::IntegrityFailed(format!(
                    "stored metadata failed for '{rel_posix}': {err}"
                ))
            })?;
            if is_reparse_or_symlink(&meta) || is_special_or_invalid_file(&meta) {
                return Err(SkillError::IntegrityFailed(format!(
                    "symlink, reparse point, or special file '{rel_posix}' detected in storage"
                )));
            }

            if meta.is_dir() {
                dir_count += 1;
                if dir_count > MAX_BUNDLE_DIRS {
                    return Err(SkillError::IntegrityFailed(
                        "stored bundle directory limit exceeded".into(),
                    ));
                }
                stack.push((path, next_segments));
                continue;
            }

            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if meta.nlink() > 1 {
                    return Err(SkillError::IntegrityFailed(format!(
                        "hardlinked file '{rel_posix}' detected in storage"
                    )));
                }
            }

            if files_meta.len() >= MAX_BUNDLE_FILES {
                return Err(SkillError::IntegrityFailed(
                    "stored bundle file count exceeds maximum limit".into(),
                ));
            }

            let content = read_bounded_regular_file(&path, &rel_posix).map_err(|err| {
                SkillError::IntegrityFailed(format!("failed reading stored '{rel_posix}': {err}"))
            })?;
            let byte_len = content.len() as u64;
            total_bytes = total_bytes.saturating_add(byte_len);
            if total_bytes > MAX_BUNDLE_BYTES {
                return Err(SkillError::IntegrityFailed(
                    "stored bundle byte count exceeds limit".into(),
                ));
            }

            if rel_posix == "skill.manifest.json" {
                manifest_bytes = Some(content.clone());
            } else if rel_posix == "SKILL.md" {
                let text = std::str::from_utf8(&content).map_err(|_| {
                    SkillError::IntegrityFailed("stored SKILL.md is not UTF-8".into())
                })?;
                if text.trim().is_empty() {
                    return Err(SkillError::IntegrityFailed(
                        "stored SKILL.md is blank".into(),
                    ));
                }
                skill_md_present = true;
            }

            let file_sha = sha256_hex(&content);
            files_meta.insert(rel_posix, (byte_len, file_sha));
        }
    }

    if !skill_md_present {
        return Err(SkillError::IntegrityFailed(
            "stored bundle missing SKILL.md".into(),
        ));
    }
    let raw_manifest = manifest_bytes.ok_or_else(|| {
        SkillError::IntegrityFailed("stored bundle missing skill.manifest.json".into())
    })?;
    let on_disk_manifest: SkillManifest = crate::json::parse(&raw_manifest).map_err(|code| {
        SkillError::IntegrityFailed(format!("stored skill.manifest.json invalid ({code})"))
    })?;
    validate_manifest(&on_disk_manifest, &files_meta).map_err(|err| {
        SkillError::IntegrityFailed(format!("stored manifest failed validation: {err}"))
    })?;

    if on_disk_manifest != record.manifest {
        return Err(SkillError::IntegrityFailed(
            "stored manifest differs from registered SQLite manifest".into(),
        ));
    }

    let computed_sha = compute_bundle_sha256(&files_meta)?;
    let observed_files = files_meta.len() as u64;
    if computed_sha != record.bundle_sha256
        || observed_files != record.file_count
        || total_bytes != record.total_bytes
    {
        return Err(SkillError::IntegrityFailed(format!(
            "stored bundle mismatch (expected sha={}, files={}, bytes={}; observed sha={}, files={}, bytes={})",
            record.bundle_sha256,
            record.file_count,
            record.total_bytes,
            computed_sha,
            observed_files,
            total_bytes
        )));
    }

    Ok(SkillIntegrityReport {
        schema_version: "1".into(),
        skill_id: record.skill_id.clone(),
        skill_version: record.skill_version.clone(),
        bundle_sha256: computed_sha,
        verified: true,
        file_count: observed_files,
        total_bytes,
    })
}

/// Read-only diagnostic inspection of `skill_store_dir` against registered SQLite `skill_bundles`.
/// Reports staging directories, unregistered bundle directories, and missing registered bundles
/// without deleting files or adopting unregistered bundles.
pub fn inspect_skill_store_artifacts(
    skill_store_dir: &Path,
    registered_rel_dirs: &HashSet<String>,
) -> Result<SkillStoreRecoveryReport, SkillError> {
    if !skill_store_dir.exists() {
        let mut missing: Vec<String> = registered_rel_dirs.iter().cloned().collect();
        missing.sort();
        return Ok(SkillStoreRecoveryReport {
            schema_version: "1".into(),
            read_only: true,
            staging_dirs: Vec::new(),
            unregistered_bundles: Vec::new(),
            missing_registered_bundles: missing,
            registered_bundles_present: 0,
        });
    }

    let meta = fs::symlink_metadata(skill_store_dir)
        .map_err(|err| SkillError::Io(format!("skill_store_dir unavailable: {err}")))?;
    if is_reparse_or_symlink(&meta) || !meta.is_dir() {
        return Err(SkillError::UnsafeBundle(
            "skill_store_dir must be a real directory".into(),
        ));
    }

    let mut staging_dirs = Vec::new();
    let mut unregistered_bundles = Vec::new();
    let mut seen_registered = HashSet::new();

    for (name, path) in read_bounded_sorted_entries(skill_store_dir, MAX_ENTRIES_PER_DIR)? {
        let entry_meta = fs::symlink_metadata(&path).map_err(|err| {
            SkillError::Io(format!("metadata failed during store inspection: {err}"))
        })?;
        if name.starts_with(".staging-") {
            if entry_meta.is_dir() && !is_reparse_or_symlink(&entry_meta) {
                staging_dirs.push(name);
            }
            continue;
        }
        if !entry_meta.is_dir() || is_reparse_or_symlink(&entry_meta) {
            continue;
        }

        // Level 2: <skill_id>/<skill_version>
        for (ver_name, ver_path) in read_bounded_sorted_entries(&path, MAX_ENTRIES_PER_DIR)? {
            let ver_meta =
                fs::symlink_metadata(&ver_path).map_err(|err| SkillError::Io(err.to_string()))?;
            if !ver_meta.is_dir() || is_reparse_or_symlink(&ver_meta) {
                continue;
            }

            // Level 3: <skill_id>/<skill_version>/<bundle_sha256>
            for (sha_name, _sha_path) in
                read_bounded_sorted_entries(&ver_path, MAX_ENTRIES_PER_DIR)?
            {
                let rel_key = format!("{name}/{ver_name}/{sha_name}");
                if registered_rel_dirs.contains(&rel_key) {
                    seen_registered.insert(rel_key);
                } else {
                    unregistered_bundles.push(rel_key);
                }
            }
        }
    }

    let mut missing_registered: Vec<String> = registered_rel_dirs
        .difference(&seen_registered)
        .cloned()
        .collect();
    staging_dirs.sort();
    unregistered_bundles.sort();
    missing_registered.sort();

    Ok(SkillStoreRecoveryReport {
        schema_version: "1".into(),
        read_only: true,
        staging_dirs,
        unregistered_bundles,
        missing_registered_bundles: missing_registered,
        registered_bundles_present: seen_registered.len(),
    })
}

fn canonicalize_or_init_store_dir(skill_store_dir: &Path) -> Result<PathBuf, SkillError> {
    if skill_store_dir.as_os_str().is_empty() {
        return Err(SkillError::UnsafeBundle(
            "skill_store_dir must not be empty".into(),
        ));
    }
    if let Ok(meta) = fs::symlink_metadata(skill_store_dir) {
        if is_reparse_or_symlink(&meta) || !meta.is_dir() {
            return Err(SkillError::UnsafeBundle(
                "skill_store_dir must be a real non-symlink directory".into(),
            ));
        }
    } else {
        fs::create_dir_all(skill_store_dir).map_err(|err| {
            SkillError::Io(format!(
                "failed to initialize skill store directory '{}': {err}",
                skill_store_dir.display()
            ))
        })?;
    }
    fs::canonicalize(skill_store_dir).map_err(|err| {
        SkillError::Io(format!(
            "failed to canonicalize skill_store_dir '{}': {err}",
            skill_store_dir.display()
        ))
    })
}

pub(crate) fn read_bounded_sorted_entries(
    dir: &Path,
    max_per_dir: usize,
) -> Result<Vec<(String, PathBuf)>, SkillError> {
    let mut entries = Vec::new();
    for entry_res in fs::read_dir(dir)
        .map_err(|err| SkillError::Io(format!("failed to read '{}': {err}", dir.display())))?
    {
        if entries.len() >= max_per_dir {
            return Err(SkillError::UnsafeBundle(format!(
                "directory '{}' exceeds max entries per directory ({max_per_dir})",
                dir.display()
            )));
        }
        let entry = entry_res
            .map_err(|err| SkillError::Io(format!("directory entry read error: {err}")))?;
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| SkillError::UnsafeBundle("non-UTF-8 filename in bundle".into()))?
            .to_owned();
        entries.push((name, entry.path()));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

fn read_bounded_regular_file(path: &Path, rel_posix: &str) -> Result<Vec<u8>, SkillError> {
    let mut file = File::open(path)
        .map_err(|err| SkillError::Io(format!("failed to open '{rel_posix}': {err}")))?;
    let opened_meta = file.metadata().map_err(|err| {
        SkillError::UnsafeBundle(format!("opened metadata failed for '{rel_posix}': {err}"))
    })?;
    if !opened_meta.is_file() {
        return Err(SkillError::UnsafeBundle(format!(
            "'{rel_posix}' is not a regular file"
        )));
    }
    if opened_meta.len() > MAX_SINGLE_FILE_BYTES {
        return Err(SkillError::UnsafeBundle(format!(
            "file '{rel_posix}' exceeds {MAX_SINGLE_FILE_BYTES} bytes"
        )));
    }
    let mut content = Vec::with_capacity(
        usize::try_from(opened_meta.len().min(MAX_SINGLE_FILE_BYTES)).unwrap_or(0),
    );
    file.by_ref()
        .take(MAX_SINGLE_FILE_BYTES + 1)
        .read_to_end(&mut content)
        .map_err(|err| SkillError::Io(format!("failed reading '{rel_posix}': {err}")))?;
    if content.len() as u64 > MAX_SINGLE_FILE_BYTES {
        return Err(SkillError::UnsafeBundle(format!(
            "file '{rel_posix}' exceeds {MAX_SINGLE_FILE_BYTES} bytes"
        )));
    }
    Ok(content)
}

fn compute_bundle_sha256(
    files_meta: &BTreeMap<String, (u64, String)>,
) -> Result<String, SkillError> {
    let digest_entries: Vec<BundleDigestEntry> = files_meta
        .iter()
        .map(|(path, (size, sha256))| BundleDigestEntry {
            path: path.clone(),
            sha256: sha256.clone(),
            size: *size,
        })
        .collect();
    let canonical_digest_payload = serde_jcs::to_vec(&digest_entries)
        .map_err(|err| SkillError::Io(format!("failed canonicalizing bundle digest: {err}")))?;
    Ok(sha256_hex(&canonical_digest_payload))
}

pub(crate) fn verify_no_links_between(base: &Path, target: &Path) -> Result<(), SkillError> {
    let rel = target
        .strip_prefix(base)
        .map_err(|_| SkillError::UnsafeBundle("path is outside expected root directory".into()))?;
    let mut cursor = base.to_path_buf();
    for component in rel.components() {
        cursor.push(component);
        let meta = fs::symlink_metadata(&cursor).map_err(|err| {
            SkillError::Io(format!(
                "metadata unavailable at '{}': {err}",
                cursor.display()
            ))
        })?;
        if is_reparse_or_symlink(&meta) || is_special_or_invalid_file(&meta) {
            return Err(SkillError::UnsafeBundle(format!(
                "symlink, reparse point, or special file along '{}'",
                cursor.display()
            )));
        }
    }
    Ok(())
}

pub fn validate_manifest(
    manifest: &SkillManifest,
    files_meta: &BTreeMap<String, (u64, String)>,
) -> Result<(), SkillError> {
    if manifest.schema_version != SKILL_SCHEMA_VERSION {
        return Err(SkillError::InvalidManifest(
            "unsupported manifest schema_version (expected '1')".into(),
        ));
    }
    if !is_safe_skill_id(&manifest.id) {
        return Err(SkillError::InvalidManifest(
            "manifest.id must be 1..=64 lowercase [a-z0-9._-] starting alphanumeric".into(),
        ));
    }
    if !is_safe_skill_version(&manifest.version) {
        return Err(SkillError::InvalidManifest(
            "manifest.version must be 1..=64 safe [a-zA-Z0-9._+-] starting alphanumeric".into(),
        ));
    }
    if manifest.rendering_strategy.trim().is_empty()
        || manifest.rendering_strategy.len() > 128
        || manifest
            .rendering_strategy
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(SkillError::InvalidManifest(
            "manifest.rendering_strategy is invalid".into(),
        ));
    }
    if manifest.supported_aspect_ratios.is_empty()
        || manifest.supported_aspect_ratios.len() > MAX_MANIFEST_ITEMS
    {
        return Err(SkillError::InvalidManifest(
            "supported_aspect_ratios must contain 1..=64 entries".into(),
        ));
    }
    let mut ratios = HashSet::new();
    for ratio in &manifest.supported_aspect_ratios {
        if !matches!(ratio.as_str(), "9:16" | "16:9" | "1:1" | "4:5") || !ratios.insert(ratio) {
            return Err(SkillError::InvalidManifest(format!(
                "invalid or duplicate supported_aspect_ratio '{ratio}'"
            )));
        }
    }
    if let Some(max_dur) = manifest.max_duration_ms {
        if !(1..=600_000).contains(&max_dur) {
            return Err(SkillError::InvalidManifest(
                "max_duration_ms must be null or within 1..=600000".into(),
            ));
        }
    }

    if !manifest.code_generation_policy.requires_sandbox {
        return Err(SkillError::InvalidManifest(
            "code_generation_policy.requires_sandbox must be true".into(),
        ));
    }
    if manifest.code_generation_policy.languages.is_empty() {
        return Err(SkillError::InvalidManifest(
            "code_generation_policy.languages must not be empty".into(),
        ));
    }
    let mut langs = HashSet::new();
    for lang in &manifest.code_generation_policy.languages {
        if !langs.insert(*lang) {
            return Err(SkillError::InvalidManifest(
                "duplicate language in code_generation_policy.languages".into(),
            ));
        }
    }
    if manifest.code_generation_policy.writable_paths.is_empty()
        || manifest.code_generation_policy.writable_paths.len() > MAX_MANIFEST_ITEMS
    {
        return Err(SkillError::InvalidManifest(
            "code_generation_policy.writable_paths must contain 1..=64 entries".into(),
        ));
    }
    let mut writable_set = HashSet::new();
    for wp in &manifest.code_generation_policy.writable_paths {
        let norm = normalize_rel_posix_path(wp)?;
        if norm == "SKILL.md" || norm == "skill.manifest.json" {
            return Err(SkillError::InvalidManifest(format!(
                "writable_paths cannot target protected manifest path '{norm}'"
            )));
        }
        if !writable_set.insert(norm.to_lowercase()) {
            return Err(SkillError::InvalidManifest(format!(
                "duplicate writable_path '{norm}'"
            )));
        }
    }

    if manifest.entrypoints.is_empty() || manifest.entrypoints.len() > MAX_ENTRYPOINTS {
        return Err(SkillError::InvalidManifest(
            "entrypoints must contain 1..=16 entries".into(),
        ));
    }
    if !manifest.entrypoints.contains_key("render") {
        return Err(SkillError::InvalidManifest(
            "entrypoints must define at least a 'render' entrypoint".into(),
        ));
    }
    for (name, ep) in &manifest.entrypoints {
        if !is_safe_skill_id(name) {
            return Err(SkillError::InvalidManifest(format!(
                "invalid entrypoint key '{name}'"
            )));
        }
        let norm_script = normalize_rel_posix_path(&ep.script)?;
        if writable_set.contains(&norm_script.to_lowercase()) {
            return Err(SkillError::InvalidManifest(format!(
                "entrypoint script '{norm_script}' cannot also be listed in writable_paths"
            )));
        }
        if !files_meta.contains_key(&norm_script) {
            return Err(SkillError::InvalidManifest(format!(
                "entrypoint '{name}' references missing script '{norm_script}'"
            )));
        }
        if !(1..=3_600).contains(&ep.timeout_sec) {
            return Err(SkillError::InvalidManifest(format!(
                "entrypoint '{name}' timeout_sec must be within 1..=3600"
            )));
        }
        if ep.args_template.len() > MAX_MANIFEST_ITEMS {
            return Err(SkillError::InvalidManifest(format!(
                "entrypoint '{name}' args_template exceeds {MAX_MANIFEST_ITEMS} items"
            )));
        }
        for arg in &ep.args_template {
            validate_arg_template(arg)?;
        }
    }

    validate_string_list("requires", &manifest.requires, false)?;
    validate_string_list("accepted_inputs", &manifest.accepted_inputs, false)?;
    validate_string_list("produced_outputs", &manifest.produced_outputs, true)?;

    Ok(())
}

fn summarize_layout(
    files_meta: &BTreeMap<String, (u64, String)>,
    total_bytes: u64,
) -> SkillLayoutSummary {
    let mut references_count = 0usize;
    let mut scripts_count = 0usize;
    let mut assets_count = 0usize;

    for rel in files_meta.keys() {
        if rel.starts_with("references/") {
            references_count += 1;
        } else if rel.starts_with("scripts/")
            || rel.starts_with("engine/")
            || rel.starts_with("template/")
        {
            scripts_count += 1;
        } else if rel.starts_with("assets/") {
            assets_count += 1;
        }
    }

    SkillLayoutSummary {
        has_skill_md: files_meta.contains_key("SKILL.md"),
        references_count,
        scripts_count,
        assets_count,
        file_count: files_meta.len(),
        total_bytes,
    }
}

fn validate_string_list(
    field: &str,
    items: &[String],
    required_nonempty: bool,
) -> Result<(), SkillError> {
    if required_nonempty && items.is_empty() {
        return Err(SkillError::InvalidManifest(format!(
            "{field} must not be empty"
        )));
    }
    if items.len() > MAX_MANIFEST_ITEMS {
        return Err(SkillError::InvalidManifest(format!(
            "{field} exceeds {MAX_MANIFEST_ITEMS} entries"
        )));
    }
    let mut seen = HashSet::new();
    for item in items {
        if item.trim().is_empty()
            || item.len() > 128
            || item.contains("..")
            || item.starts_with('/')
            || item.contains('\\')
            || item.chars().any(|c| c == '\0' || c.is_control())
        {
            return Err(SkillError::InvalidManifest(format!(
                "invalid entry '{item}' in {field}"
            )));
        }
        if !seen.insert(item.to_lowercase()) {
            return Err(SkillError::InvalidManifest(format!(
                "duplicate entry '{item}' in {field}"
            )));
        }
    }
    Ok(())
}

fn validate_arg_template(arg: &str) -> Result<(), SkillError> {
    if arg.is_empty()
        || arg.len() > 128
        || arg
            .chars()
            .any(|c| c == '\0' || c == '\r' || c == '\n' || c.is_control())
    {
        return Err(SkillError::InvalidManifest(
            "args_template item contains invalid length or control character".into(),
        ));
    }
    let lower = arg.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "sh" | "/bin/sh" | "bash" | "/bin/bash" | "cmd" | "cmd.exe" | "powershell" | "pwsh"
    ) || lower == "--privileged"
        || lower.starts_with("--cap-add")
        || lower.starts_with("--network")
    {
        return Err(SkillError::InvalidManifest(format!(
            "forbidden shell or privileged token '{arg}' in args_template"
        )));
    }
    Ok(())
}

fn normalize_rel_posix_path(raw: &str) -> Result<String, SkillError> {
    if raw.is_empty()
        || raw.len() > MAX_REL_PATH_LEN
        || raw.starts_with('/')
        || raw.starts_with('-')
        || raw.contains('\\')
        || raw.contains(':')
        || raw.chars().any(|c| c == '\0' || c.is_control())
    {
        return Err(SkillError::InvalidManifest(format!(
            "invalid relative path '{raw}' in manifest"
        )));
    }
    let mut segments = Vec::new();
    for seg in raw.split('/') {
        validate_segment_name(seg)?;
        segments.push(seg);
    }
    Ok(segments.join("/"))
}

pub(crate) fn validate_segment_name(seg: &str) -> Result<(), SkillError> {
    if seg.is_empty()
        || seg == "."
        || seg == ".."
        || seg.starts_with('.')
        || seg.ends_with(' ')
        || seg.ends_with('.')
        || seg.starts_with(' ')
        || seg.len() > 128
        || seg.bytes().any(|b| {
            matches!(
                b,
                b'\\' | b'/' | b':' | b'*' | b'?' | b'"' | b'<' | b'>' | b'|' | b'\0'
            ) || (b as char).is_control()
        })
    {
        return Err(SkillError::UnsafeBundle(format!(
            "prohibited or unsafe filename '{seg}' in bundle"
        )));
    }

    let stem = seg.split('.').next().unwrap_or(seg).to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "CLOCK$"
            | "COM0"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT0"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        return Err(SkillError::UnsafeBundle(format!(
            "reserved filename '{seg}' in bundle"
        )));
    }

    let lower = seg.to_ascii_lowercase();
    if lower == "scenivox.db"
        || lower.starts_with("scenivox.db-")
        || lower.ends_with(".db")
        || lower.ends_with(".sqlite")
        || lower.ends_with(".sock")
    {
        return Err(SkillError::UnsafeBundle(format!(
            "forbidden database/socket file '{seg}' in bundle"
        )));
    }

    Ok(())
}

pub(crate) fn ensure_source_dir_safe(canonical: &Path) -> Result<(), SkillError> {
    if canonical.parent().is_none() {
        return Err(SkillError::UnsafeBundle(
            "filesystem root cannot be imported as a skill bundle".into(),
        ));
    }
    let normalized = canonical
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('\\', "/")
        .to_ascii_lowercase();
    for forbidden in [
        "/etc",
        "/proc",
        "/sys",
        "/dev",
        "/run",
        "/var/run",
        "c:/windows",
        "c:/program files",
    ] {
        if normalized == forbidden || normalized.starts_with(&format!("{forbidden}/")) {
            return Err(SkillError::UnsafeBundle(
                "system directories cannot be imported as skill bundles".into(),
            ));
        }
    }
    for env_home in ["HOME", "USERPROFILE"] {
        if let Ok(home) = std::env::var(env_home) {
            if let Ok(home_canon) = fs::canonicalize(home) {
                if canonical == home_canon {
                    return Err(SkillError::UnsafeBundle(
                        "home directory cannot be imported directly".into(),
                    ));
                }
            }
        }
    }
    for part in canonical.components() {
        if let Component::Normal(seg) = part {
            let s = seg.to_string_lossy().to_ascii_lowercase();
            if matches!(s.as_str(), ".git" | ".ssh" | ".aws" | ".gnupg" | ".docker") {
                return Err(SkillError::UnsafeBundle(format!(
                    "sensitive directory component '{s}' cannot be imported"
                )));
            }
        }
    }
    Ok(())
}

fn is_zip_candidate(path: &Path) -> Result<bool, SkillError> {
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
    {
        return Ok(true);
    }
    let mut file = File::open(path)
        .map_err(|err| SkillError::Io(format!("failed opening '{}': {err}", path.display())))?;
    let mut magic = [0u8; 4];
    let n = file
        .read(&mut magic)
        .map_err(|err| SkillError::Io(format!("failed reading '{}': {err}", path.display())))?;
    Ok(n >= 4 && (magic == *b"PK\x03\x04" || magic == *b"PK\x05\x06" || magic == *b"PK\x01\x02"))
}

pub fn is_safe_skill_id(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 64
        && v.bytes().enumerate().all(|(i, b)| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || (i > 0 && matches!(b, b'-' | b'_' | b'.'))
        })
        && !v.contains("..")
}

pub fn is_safe_skill_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 64
        && v.bytes().enumerate().all(|(i, b)| {
            b.is_ascii_alphanumeric() || (i > 0 && matches!(b, b'-' | b'_' | b'.' | b'+'))
        })
        && !v.contains("..")
}

pub(crate) fn is_sha256_hex(v: &str) -> bool {
    v.len() == 64
        && v.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

pub(crate) fn paths_overlap(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}

fn rel_to_native(segments: &[String]) -> PathBuf {
    let mut pb = PathBuf::new();
    for seg in segments {
        pb.push(seg);
    }
    pb
}

#[cfg(windows)]
pub(crate) fn is_reparse_or_symlink(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    meta.file_type().is_symlink() || (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
}

#[cfg(not(windows))]
pub(crate) fn is_reparse_or_symlink(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

#[cfg(unix)]
pub(crate) fn is_special_or_invalid_file(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    let ft = meta.file_type();
    !(meta.is_file() || meta.is_dir())
        || ft.is_fifo()
        || ft.is_socket()
        || ft.is_char_device()
        || ft.is_block_device()
}

#[cfg(not(unix))]
pub(crate) fn is_special_or_invalid_file(meta: &fs::Metadata) -> bool {
    !(meta.is_file() || meta.is_dir())
}

fn placeholder_manifest() -> SkillManifest {
    SkillManifest {
        schema_version: SKILL_SCHEMA_VERSION.into(),
        id: String::new(),
        version: String::new(),
        kind: SkillKind::UnifiedComposition,
        composition_mode: CompositionMode::UnifiedTimeline,
        rendering_strategy: String::new(),
        supported_aspect_ratios: Vec::new(),
        max_duration_ms: None,
        supports_independent_scene_render: false,
        code_generation_policy: CodeGenerationPolicy {
            languages: Vec::new(),
            writable_paths: Vec::new(),
            requires_sandbox: true,
        },
        entrypoints: BTreeMap::new(),
        requires: Vec::new(),
        accepted_inputs: Vec::new(),
        produced_outputs: Vec::new(),
    }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
