//! Local animation skill bundle validation, secure staging, and registry contracts (ADR-02 / P2-01B).
//!
//! This module validates and stages local skill directories containing `skill.manifest.json`,
//! `SKILL.md`, `references/`, `scripts/`, and `assets/` without executing skill code, pulling
//! OCI images, calling remote models, or introducing unapproved archive dependencies.
//! `audit_status` is strictly host-owned and always initializes to `unaudited` on import.

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
    #[error("skill import io error: {0}")]
    Io(String),
}

impl SkillError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidManifest(_) => "invalid_skill_manifest",
            Self::UnsafeBundle(_) => "unsafe_skill_bundle",
            Self::UnsupportedArchive(_) => "unsupported_archive_format",
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
    pub fn publish_files(&mut self) -> Result<(), SkillError> {
        let Some(staging) = self.staging_dir.take() else {
            return Err(SkillError::Io("staging directory already consumed".into()));
        };
        if let Some(parent) = self.target_dir.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                SkillError::Io(format!("failed to create skill store dir: {err}"))
            })?;
        }
        if self.target_dir.exists() {
            self.staging_dir = Some(staging);
            return Err(SkillError::UnsafeBundle(
                "target skill storage directory already exists".into(),
            ));
        }
        fs::rename(&staging, &self.target_dir).map_err(|err| {
            let _ = fs::remove_dir_all(&staging);
            SkillError::Io(format!(
                "failed to move staged bundle into skill store: {err}"
            ))
        })?;
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
            if let Some(version_dir) = dir.parent() {
                let _ = fs::remove_dir(version_dir);
                if let Some(id_dir) = version_dir.parent() {
                    let _ = fs::remove_dir(id_dir);
                }
            }
        }
    }
}

#[derive(Serialize)]
struct BundleDigestEntry {
    path: String,
    sha256: String,
    size: u64,
}

/// Validates and copies a local skill directory into an isolated `.staging-<uuid>` directory,
/// checking every path, link invariant, manifest field, and byte limit without executing any file.
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

    let store_staging_root = skill_store_dir;
    fs::create_dir_all(store_staging_root).map_err(|err| {
        SkillError::Io(format!(
            "failed to initialize skill store directory '{}': {err}",
            store_staging_root.display()
        ))
    })?;
    let staging_dir = store_staging_root.join(format!(".staging-{}", Uuid::new_v4()));
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
        let mut entries: Vec<_> = fs::read_dir(&dir)
            .map_err(|err| SkillError::Io(format!("failed to read '{}': {err}", dir.display())))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| SkillError::Io(format!("directory entry read error: {err}")))?;
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let os_name = entry.file_name();
            let name = os_name
                .to_str()
                .ok_or_else(|| SkillError::UnsafeBundle("non-UTF-8 filename in bundle".into()))?
                .to_owned();
            validate_segment_name(&name)?;

            let mut next_segments = rel_segments.clone();
            next_segments.push(name.clone());
            let rel_posix = next_segments.join("/");
            let rel_lower = rel_posix.to_ascii_lowercase();
            if !seen_lower.insert(rel_lower) {
                return Err(SkillError::UnsafeBundle(format!(
                    "duplicate or case-colliding path '{rel_posix}' in skill bundle"
                )));
            }

            let path = entry.path();
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

            let mut file = File::open(&path)
                .map_err(|err| SkillError::Io(format!("failed to open '{rel_posix}': {err}")))?;
            let opened_meta = file.metadata().map_err(|err| {
                SkillError::UnsafeBundle(format!("opened metadata failed for '{rel_posix}': {err}"))
            })?;
            if !opened_meta.is_file() {
                return Err(SkillError::UnsafeBundle(format!(
                    "'{rel_posix}' changed to non-regular file during import"
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
            let byte_len = content.len() as u64;
            if byte_len > MAX_SINGLE_FILE_BYTES {
                return Err(SkillError::UnsafeBundle(format!(
                    "file '{rel_posix}' exceeds {MAX_SINGLE_FILE_BYTES} bytes"
                )));
            }

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
    let bundle_sha256 = sha256_hex(&canonical_digest_payload);

    let storage_rel_dir = format!("{}/{}/{}", manifest.id, manifest.version, bundle_sha256);
    let target_dir = skill_store_dir
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
        if !writable_set.insert(norm.to_ascii_lowercase()) {
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
        if writable_set.contains(&norm_script.to_ascii_lowercase()) {
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
        if !seen.insert(item.to_ascii_lowercase()) {
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
        || raw.len() > 256
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

fn validate_segment_name(seg: &str) -> Result<(), SkillError> {
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
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
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

fn ensure_source_dir_safe(canonical: &Path) -> Result<(), SkillError> {
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

fn rel_to_native(segments: &[String]) -> PathBuf {
    let mut pb = PathBuf::new();
    for seg in segments {
        pb.push(seg);
    }
    pb
}

#[cfg(windows)]
fn is_reparse_or_symlink(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    meta.file_type().is_symlink() || (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
}

#[cfg(not(windows))]
fn is_reparse_or_symlink(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

#[cfg(unix)]
fn is_special_or_invalid_file(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    let ft = meta.file_type();
    !(meta.is_file() || meta.is_dir())
        || ft.is_fifo()
        || ft.is_socket()
        || ft.is_char_device()
        || ft.is_block_device()
}

#[cfg(not(unix))]
fn is_special_or_invalid_file(meta: &fs::Metadata) -> bool {
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

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
