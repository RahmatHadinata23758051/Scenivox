//! Fail-closed rootless Linux OCI sandbox execution foundation (ADR-02 / P2-01A).
//!
//! Security invariants enforced here:
//! 1. Runtime workspace and `/tmp` byte + inode quotas enforced in-kernel via bounded tmpfs
//!    (host bind workspaces fail closed without verified OS quota enforcement).
//! 2. Mounts are allowlisted strictly to explicitly configured, job-owned staging roots and
//!    authorized skill store roots, with segment-by-segment symlink/reparse/special-file rejection.
//! 3. Host child supervision and pipe draining share a single end-to-end deadline with bounded
//!    channel waits (never unbounded thread joins); container names are UUIDv4-scoped per run;
//!    uncertain or failed cleanup always returns an explicit `CleanupFailed` error.
//! 4. Runtime verification distinguishes rootless local engine detection from full verified
//!    isolation readiness (rejecting remote contexts, cgroup v1, missing CPU/memory/PIDs
//!    controllers, missing trusted OCI runtimes, and missing/mismatched local pinned images).

use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::{ErrorKind, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

pub const SANDBOX_POLICY_SCHEMA_VERSION: &str = "1";
pub const DEFAULT_SANDBOX_UID: u32 = 65_532;
pub const DEFAULT_SANDBOX_GID: u32 = 65_532;
pub const MAX_CPUS: f64 = 16.0;
pub const MAX_MEMORY_MB: u64 = 16_384;
pub const MAX_PIDS_LIMIT: u32 = 4_096;
pub const MIN_TMPFS_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_TMPFS_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const MIN_WORKSPACE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_WORKSPACE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const MIN_INODES: u64 = 64;
pub const MAX_INODES: u64 = 262_144;
pub const MAX_TIMEOUT_SECS: u64 = 3_600;
pub const MAX_STREAM_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_EXEC_ARGS: usize = 64;
pub const MAX_EXEC_ARG_LEN: usize = 1_024;
pub const MAX_ENV_VARS: usize = 16;
pub const MAX_ENV_VALUE_LEN: usize = 1_024;
pub const MAX_READ_ONLY_MOUNTS: usize = 16;
pub const MAX_EXPECTED_ARTIFACTS: usize = 16;
pub const MAX_ARTIFACT_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_TOTAL_ARTIFACT_BYTES: u64 = 128 * 1024 * 1024;
const ARTIFACT_RUNNER_CONTAINER_PATH: &str = "/opt/scenivox-runtime/artifact_runner.py";
const ARTIFACT_PROTOCOL_MAGIC: &[u8; 8] = b"SVXART01";
const ARTIFACT_PROTOCOL_OVERHEAD_BYTES: usize = 64 * 1024;
const MAX_INSPECTED_DIR_ENTRIES: usize = 4_096;
const PROBE_TIMEOUT_CAP: Duration = Duration::from_secs(25);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(20);
const POST_EXIT_PIPE_DRAIN_CAP: Duration = Duration::from_millis(500);

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("sandbox unavailable: {0}")]
    SandboxUnavailable(String),
    #[error("invalid sandbox policy: {0}")]
    InvalidPolicy(String),
    #[error("invalid sandbox mount: {0}")]
    InvalidMount(String),
    #[error("invalid sandbox invocation: {0}")]
    InvalidInvocation(String),
    #[error("sandbox timed out after {timeout_secs}s (container cleanup verified)")]
    Timeout { timeout_secs: u64 },
    #[error("sandbox {stream} exceeded {limit_bytes} bytes (container cleanup verified)")]
    OutputLimitExceeded {
        stream: &'static str,
        limit_bytes: usize,
    },
    #[error(
        "sandbox execution failed (exit_code={exit_code:?}, cleanup verified): {stderr_excerpt}"
    )]
    ExecutionFailed {
        exit_code: Option<i32>,
        stderr_excerpt: String,
    },
    #[error("sandbox cleanup failed or uncertain during {context}: {detail}")]
    CleanupFailed { context: String, detail: String },
    #[error("sandbox bounded artifact transfer failed: {0}")]
    ArtifactTransferFailed(String),
}

impl SandboxError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::SandboxUnavailable(_) => "sandbox_unavailable",
            Self::InvalidPolicy(_) => "invalid_sandbox_policy",
            Self::InvalidMount(_) => "invalid_sandbox_mount",
            Self::InvalidInvocation(_) => "invalid_sandbox_invocation",
            Self::Timeout { .. } => "sandbox_timeout",
            Self::OutputLimitExceeded { .. } => "sandbox_output_limit_exceeded",
            Self::ExecutionFailed { .. } => "sandbox_execution_failed",
            Self::CleanupFailed { .. } => "sandbox_cleanup_failed",
            Self::ArtifactTransferFailed(_) => "sandbox_artifact_transfer_failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostPlatform {
    Linux,
    Windows,
    Unsupported,
}

impl HostPlatform {
    pub fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Unsupported
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OciEngine {
    Podman,
    Docker,
}

impl OciEngine {
    pub fn binary(self) -> &'static str {
        match self {
            Self::Podman => "podman",
            Self::Docker => "docker",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OciTransport {
    NativeLinux,
    Wsl2 { distro: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlEvidenceSource {
    /// Derived only from engine `info` / metadata JSON without live container probing.
    EngineMetadataOnly,
    /// Measured from actual execution inside the target container runtime.
    EmpiricallyVerified,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveResourceControls {
    pub cpu_quota_enforced: bool,
    pub memory_limit_enforced: bool,
    pub swap_limit_enforced: bool,
    pub pids_limit_enforced: bool,
    pub tmpfs_size_enforced: bool,
    pub tmpfs_inodes_enforced: bool,
}

impl EffectiveResourceControls {
    pub fn all_mandatory_enforced(self) -> bool {
        self.cpu_quota_enforced
            && self.memory_limit_enforced
            && self.swap_limit_enforced
            && self.pids_limit_enforced
            && self.tmpfs_size_enforced
            && self.tmpfs_inodes_enforced
    }
}

pub const SCENIVOX_ROOTLESS_DOCKER_CONTEXT: &str = "scenivox-rootless";
pub const SCENIVOX_ROOTLESS_DOCKER_SOCKET: &str =
    "unix:///run/user/1000/scenivox-docker/docker.sock";
pub const SCENIVOX_DEFAULT_SECCOMP_PROFILE_PATH: &str =
    "/home/nata/.config/scenivox-docker/seccomp-chromium.json";
pub const SCENIVOX_CHROMIUM_SECCOMP_SHA256: &str =
    "9487aeb312d26c6844c09b0bf6782ac54b5434100de95b189b5ee5a85e9d4a93";
const MAX_SECCOMP_PROFILE_BYTES: usize = 512 * 1024;

/// Cryptographically pinned dedicated rootless Docker runtime specification.
/// Rejects rootful `default` contexts, remote sockets, mismatched image digests,
/// and tampered or unreviewed seccomp profiles.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedDockerRuntime {
    pub context_name: String,
    pub unix_socket: String,
    pub image_digest: String,
    pub seccomp_profile_path: String,
    pub seccomp_profile_sha256: String,
}

impl PinnedDockerRuntime {
    pub fn scenivox_rootless(image_digest: impl Into<String>) -> Result<Self, SandboxError> {
        let pin = Self {
            context_name: SCENIVOX_ROOTLESS_DOCKER_CONTEXT.into(),
            unix_socket: SCENIVOX_ROOTLESS_DOCKER_SOCKET.into(),
            image_digest: image_digest.into(),
            seccomp_profile_path: SCENIVOX_DEFAULT_SECCOMP_PROFILE_PATH.into(),
            seccomp_profile_sha256: SCENIVOX_CHROMIUM_SECCOMP_SHA256.into(),
        };
        pin.validate()?;
        Ok(pin)
    }

    pub fn validate(&self) -> Result<(), SandboxError> {
        let ctx = self.context_name.trim();
        if ctx.is_empty()
            || ctx.len() > 64
            || matches!(ctx, "default" | "desktop-linux")
            || !ctx
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(SandboxError::InvalidPolicy(
                "pinned docker context_name must be a non-default safe local context identifier"
                    .into(),
            ));
        }
        if !self.unix_socket.starts_with("unix:///")
            || self.unix_socket.len() > 256
            || self.unix_socket.contains("..")
            || self.unix_socket.bytes().any(|b| {
                b.is_ascii_whitespace() || b.is_ascii_control() || matches!(b, b',' | b'=')
            })
        {
            return Err(SandboxError::InvalidPolicy(
                "pinned docker unix_socket must be a local unix:/// socket path".into(),
            ));
        }
        if !is_sha256_digest_prefixed(&self.image_digest) {
            return Err(SandboxError::InvalidPolicy(
                "pinned docker image_digest must be sha256:<64 lowercase hex>".into(),
            ));
        }
        if !self.seccomp_profile_path.starts_with('/')
            || self.seccomp_profile_path.len() > 256
            || self.seccomp_profile_path.contains("..")
            || self.seccomp_profile_path.bytes().any(|b| {
                b.is_ascii_whitespace()
                    || b.is_ascii_control()
                    || matches!(b, b',' | b'=' | b':' | b'\'' | b'"')
            })
        {
            return Err(SandboxError::InvalidPolicy(
                "pinned seccomp_profile_path must be a clean absolute POSIX file path".into(),
            ));
        }
        if !is_sha256_hex(&self.seccomp_profile_sha256) {
            return Err(SandboxError::InvalidPolicy(
                "pinned seccomp_profile_sha256 must be 64 lowercase hex chars".into(),
            ));
        }
        Ok(())
    }
}

/// Measured in-container control and Chromium renderer sandbox telemetry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmpiricalControlProbe {
    pub uid: u32,
    pub gid: u32,
    pub cap_eff_hex: String,
    pub no_new_privs: u8,
    pub seccomp_mode: u8,
    pub network_interfaces: Vec<String>,
    pub read_only_rootfs_enforced: bool,
    pub cpu_max: String,
    pub memory_max_bytes: u64,
    pub memory_swap_max_bytes: u64,
    pub pids_max: u32,
    pub tmp_inodes_reported: u64,
    pub workspace_inodes_reported: u64,
    pub inode_exhaustion_at: u64,
    pub inode_exhaustion_enforced: bool,
    pub chromium_no_sandbox_flag_absent: bool,
    pub chromium_zygote_active: bool,
    pub chromium_renderer_pid_ns_depth: usize,
    pub chromium_renderer_seccomp_mode: u8,
    pub chromium_screenshot_bytes: usize,
}

/// Validates measured empirical container telemetry against the requested `SandboxPolicy`.
pub fn validate_empirical_probe_against_policy(
    probe: &EmpiricalControlProbe,
    policy: &SandboxPolicy,
    require_chromium: bool,
) -> Result<(), SandboxError> {
    policy.validate()?;
    if probe.uid != policy.uid || probe.gid != policy.gid {
        return Err(SandboxError::SandboxUnavailable(format!(
            "empirical UID/GID mismatch: expected {}:{}, got {}:{}",
            policy.uid, policy.gid, probe.uid, probe.gid
        )));
    }
    if probe.cap_eff_hex != "0000000000000000" {
        return Err(SandboxError::SandboxUnavailable(format!(
            "empirical container CapEff '{}' is non-zero",
            probe.cap_eff_hex
        )));
    }
    if probe.no_new_privs != 1 || probe.seccomp_mode != 2 {
        return Err(SandboxError::SandboxUnavailable(format!(
            "empirical container missing no_new_privs/seccomp enforcement (NoNewPrivs={}, Seccomp={})",
            probe.no_new_privs, probe.seccomp_mode
        )));
    }
    if probe.network_interfaces != ["lo"] {
        return Err(SandboxError::SandboxUnavailable(format!(
            "empirical container network isolation mismatch: {:?}",
            probe.network_interfaces
        )));
    }
    if !probe.read_only_rootfs_enforced {
        return Err(SandboxError::SandboxUnavailable(
            "empirical rootfs is not read-only".into(),
        ));
    }

    let expected_cpu_quota = (policy.limits.cpus * 100_000.0).round() as u64;
    let expected_cpu_max = format!("{expected_cpu_quota} 100000");
    if probe.cpu_max.trim() != expected_cpu_max {
        return Err(SandboxError::SandboxUnavailable(format!(
            "empirical cgroup cpu.max '{}' != expected '{expected_cpu_max}'",
            probe.cpu_max
        )));
    }
    let expected_mem_bytes = policy.limits.memory_mb.saturating_mul(1024 * 1024);
    if probe.memory_max_bytes != expected_mem_bytes || probe.memory_swap_max_bytes != 0 {
        return Err(SandboxError::SandboxUnavailable(format!(
            "empirical cgroup memory/swap mismatch: mem={} swap={} (expected mem={expected_mem_bytes}, swap=0)",
            probe.memory_max_bytes, probe.memory_swap_max_bytes
        )));
    }
    if probe.pids_max != policy.limits.pids_limit {
        return Err(SandboxError::SandboxUnavailable(format!(
            "empirical cgroup pids.max {} != policy {}",
            probe.pids_max, policy.limits.pids_limit
        )));
    }
    if probe.tmp_inodes_reported != policy.limits.tmpfs_inodes
        || probe.workspace_inodes_reported != policy.limits.workspace_inodes
    {
        return Err(SandboxError::SandboxUnavailable(format!(
            "empirical tmpfs inode reporting mismatch: /tmp={} /workspace={} (expected {} / {})",
            probe.tmp_inodes_reported,
            probe.workspace_inodes_reported,
            policy.limits.tmpfs_inodes,
            policy.limits.workspace_inodes
        )));
    }
    if !probe.inode_exhaustion_enforced
        || probe.inode_exhaustion_at == 0
        || probe.inode_exhaustion_at > policy.limits.workspace_inodes
    {
        return Err(SandboxError::SandboxUnavailable(format!(
            "empirical tmpfs inode exhaustion failed (enforced={}, at={})",
            probe.inode_exhaustion_enforced, probe.inode_exhaustion_at
        )));
    }
    if require_chromium
        && (!probe.chromium_no_sandbox_flag_absent
            || !probe.chromium_zygote_active
            || probe.chromium_renderer_pid_ns_depth < 2
            || probe.chromium_renderer_seccomp_mode != 2
            || probe.chromium_screenshot_bytes == 0)
    {
        return Err(SandboxError::SandboxUnavailable(format!(
            "empirical Chromium renderer sandbox verification failed (no_sandbox_absent={}, zygote={}, renderer_pid_ns_depth={}, renderer_seccomp={}, bytes={})",
            probe.chromium_no_sandbox_flag_absent,
            probe.chromium_zygote_active,
            probe.chromium_renderer_pid_ns_depth,
            probe.chromium_renderer_seccomp_mode,
            probe.chromium_screenshot_bytes
        )));
    }
    Ok(())
}

/// Verified rootless, local, cgroups-v2 OCI engine detection (metadata-only readiness).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectedRuntime {
    pub engine: OciEngine,
    pub transport: OciTransport,
    pub oci_runtime_name: String,
    pub engine_version: String,
    pub cgroup_version: String,
    pub rootless_verified: bool,
    pub local_endpoint_verified: bool,
    pub metadata_source: ControlEvidenceSource,
    pub controls: EffectiveResourceControls,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_context_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unix_socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seccomp_profile_sha256: Option<String>,
}

/// Full isolation readiness proof: verified runtime + policy + local digest-pinned image.
/// Distinguishes engine metadata guarantees from empirical container measurements.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedIsolation {
    pub runtime: DetectedRuntime,
    pub image_reference: String,
    pub image_digest: String,
    pub local_image_id: String,
    pub runtime_policy_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_context_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unix_socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seccomp_profile_sha256: Option<String>,
    pub controls_evidence_source: ControlEvidenceSource,
    pub empirically_verified: bool,
    pub mandatory_controls_verified: bool,
    pub isolation_verified: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestPinnedImage {
    pub reference: String,
}

impl DigestPinnedImage {
    pub fn new(reference: impl Into<String>) -> Result<Self, SandboxError> {
        let image = Self {
            reference: reference.into(),
        };
        image.validate()?;
        Ok(image)
    }

    pub fn validate(&self) -> Result<(), SandboxError> {
        validate_digest_pinned_image_ref(&self.reference).map(|_| ())
    }

    pub fn digest(&self) -> Result<String, SandboxError> {
        validate_digest_pinned_image_ref(&self.reference).map(str::to_owned)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceQuotaMode {
    /// Runtime byte and inode limits enforced in-kernel on `/workspace` via tmpfs.
    TmpfsQuota,
    /// Host bind-mount directory with verified OS quota (fails closed when unsupported).
    HostBindWithVerifiedQuota,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxLimits {
    pub cpus: f64,
    pub memory_mb: u64,
    pub pids_limit: u32,
    pub tmpfs_bytes: u64,
    pub tmpfs_inodes: u64,
    pub workspace_bytes: u64,
    pub workspace_inodes: u64,
    pub timeout_secs: u64,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}

impl SandboxLimits {
    pub fn validate(&self) -> Result<(), SandboxError> {
        if !self.cpus.is_finite() || self.cpus <= 0.0 || self.cpus > MAX_CPUS {
            return Err(SandboxError::InvalidPolicy(
                "cpus must be finite and within (0, 16]".into(),
            ));
        }
        if !(64..=MAX_MEMORY_MB).contains(&self.memory_mb) {
            return Err(SandboxError::InvalidPolicy(
                "memory_mb must be between 64 and 16384".into(),
            ));
        }
        if !(8..=MAX_PIDS_LIMIT).contains(&self.pids_limit) {
            return Err(SandboxError::InvalidPolicy(
                "pids_limit must be between 8 and 4096".into(),
            ));
        }
        if !(MIN_TMPFS_BYTES..=MAX_TMPFS_BYTES).contains(&self.tmpfs_bytes) {
            return Err(SandboxError::InvalidPolicy(
                "tmpfs_bytes must be between 8 MiB and 4 GiB".into(),
            ));
        }
        if !(MIN_INODES..=MAX_INODES).contains(&self.tmpfs_inodes) {
            return Err(SandboxError::InvalidPolicy(
                "tmpfs_inodes must be between 64 and 262144".into(),
            ));
        }
        if !(MIN_WORKSPACE_BYTES..=MAX_WORKSPACE_BYTES).contains(&self.workspace_bytes) {
            return Err(SandboxError::InvalidPolicy(
                "workspace_bytes must be between 8 MiB and 8 GiB".into(),
            ));
        }
        if !(MIN_INODES..=MAX_INODES).contains(&self.workspace_inodes) {
            return Err(SandboxError::InvalidPolicy(
                "workspace_inodes must be between 64 and 262144".into(),
            ));
        }
        if !(1..=MAX_TIMEOUT_SECS).contains(&self.timeout_secs) {
            return Err(SandboxError::InvalidPolicy(
                "timeout_secs must be between 1 and 3600".into(),
            ));
        }
        if !(1..=MAX_STREAM_OUTPUT_BYTES).contains(&self.max_stdout_bytes) {
            return Err(SandboxError::InvalidPolicy(
                "max_stdout_bytes must be between 1 and 4 MiB".into(),
            ));
        }
        if !(1..=MAX_STREAM_OUTPUT_BYTES).contains(&self.max_stderr_bytes) {
            return Err(SandboxError::InvalidPolicy(
                "max_stderr_bytes must be between 1 and 4 MiB".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxPolicy {
    pub schema_version: String,
    pub image: DigestPinnedImage,
    pub workspace_mode: WorkspaceQuotaMode,
    pub network_disabled: bool,
    pub pull_never: bool,
    pub read_only_rootfs: bool,
    pub drop_all_capabilities: bool,
    pub no_new_privileges: bool,
    pub require_rootless: bool,
    pub uid: u32,
    pub gid: u32,
    pub limits: SandboxLimits,
}

impl SandboxPolicy {
    pub fn hardened_default(image_reference: impl Into<String>) -> Result<Self, SandboxError> {
        let policy = Self {
            schema_version: SANDBOX_POLICY_SCHEMA_VERSION.into(),
            image: DigestPinnedImage::new(image_reference)?,
            workspace_mode: WorkspaceQuotaMode::TmpfsQuota,
            network_disabled: true,
            pull_never: true,
            read_only_rootfs: true,
            drop_all_capabilities: true,
            no_new_privileges: true,
            require_rootless: true,
            uid: DEFAULT_SANDBOX_UID,
            gid: DEFAULT_SANDBOX_GID,
            limits: SandboxLimits {
                cpus: 2.0,
                memory_mb: 2_048,
                pids_limit: 128,
                tmpfs_bytes: 256 * 1024 * 1024,
                tmpfs_inodes: 4_096,
                workspace_bytes: 512 * 1024 * 1024,
                workspace_inodes: 8_192,
                timeout_secs: 120,
                max_stdout_bytes: 256 * 1024,
                max_stderr_bytes: 256 * 1024,
            },
        };
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<(), SandboxError> {
        if self.schema_version != SANDBOX_POLICY_SCHEMA_VERSION {
            return Err(SandboxError::InvalidPolicy(
                "unsupported sandbox policy schema_version".into(),
            ));
        }
        self.image.validate()?;
        if !self.network_disabled {
            return Err(SandboxError::InvalidPolicy(
                "network_disabled must be true".into(),
            ));
        }
        if !self.pull_never {
            return Err(SandboxError::InvalidPolicy(
                "pull_never must be true".into(),
            ));
        }
        if !self.read_only_rootfs {
            return Err(SandboxError::InvalidPolicy(
                "read_only_rootfs must be true".into(),
            ));
        }
        if !self.drop_all_capabilities {
            return Err(SandboxError::InvalidPolicy(
                "drop_all_capabilities must be true".into(),
            ));
        }
        if !self.no_new_privileges {
            return Err(SandboxError::InvalidPolicy(
                "no_new_privileges must be true".into(),
            ));
        }
        if !self.require_rootless {
            return Err(SandboxError::InvalidPolicy(
                "require_rootless must be true".into(),
            ));
        }
        if self.uid == 0 || self.gid == 0 {
            return Err(SandboxError::InvalidPolicy(
                "non-root uid and gid (> 0) are required".into(),
            ));
        }
        self.limits.validate()
    }

    /// Computes canonical RFC 8785 JCS SHA-256 over the validated policy.
    pub fn policy_sha256(&self) -> Result<String, SandboxError> {
        self.validate()?;
        let bytes = serde_jcs::to_vec(self)
            .map_err(|err| SandboxError::InvalidPolicy(format!("canonicalization error: {err}")))?;
        Ok(sha256_hex(&bytes))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptRuntime {
    Node,
    Python3,
}

impl ScriptRuntime {
    pub fn binary(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Python3 => "python3",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptMountRoot {
    Skill,
    Workspace,
}

impl ScriptMountRoot {
    pub fn container_dir(self) -> &'static str {
        match self {
            Self::Skill => "/skill",
            Self::Workspace => "/workspace",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaTool {
    Ffmpeg,
    Ffprobe,
}

impl MediaTool {
    pub fn binary(self) -> &'static str {
        match self {
            Self::Ffmpeg => "ffmpeg",
            Self::Ffprobe => "ffprobe",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SandboxCommand {
    Script {
        runtime: ScriptRuntime,
        root: ScriptMountRoot,
        script_rel_path: String,
        args: Vec<String>,
    },
    MediaTool {
        tool: MediaTool,
        args: Vec<String>,
    },
}

impl SandboxCommand {
    pub fn validate(&self) -> Result<(String, Vec<String>), SandboxError> {
        match self {
            Self::Script {
                runtime,
                root,
                script_rel_path,
                args,
            } => {
                let rel = normalize_relative_posix_path(script_rel_path)?;
                let valid_ext = match runtime {
                    ScriptRuntime::Node => {
                        rel.ends_with(".js") || rel.ends_with(".mjs") || rel.ends_with(".cjs")
                    }
                    ScriptRuntime::Python3 => rel.ends_with(".py"),
                };
                if !valid_ext {
                    return Err(SandboxError::InvalidInvocation(format!(
                        "script extension is incompatible with {}",
                        runtime.binary()
                    )));
                }
                validate_args(args, false)?;
                let mut argv = Vec::with_capacity(args.len() + 1);
                argv.push(format!("{}/{}", root.container_dir(), rel));
                argv.extend(args.iter().cloned());
                Ok((runtime.binary().to_owned(), argv))
            }
            Self::MediaTool { tool, args } => {
                validate_args(args, true)?;
                Ok((tool.binary().to_owned(), args.clone()))
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadOnlyMount {
    pub host_path: PathBuf,
    pub container_path: String,
}

/// Explicitly authorized host staging directories permitted as read-only container inputs.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TrustedStagingRoots {
    /// Root containing strictly job-owned directories `<jobs_root>/<job_id>/...`.
    pub jobs_root: Option<PathBuf>,
    /// Root containing immutable audited read-only skill bundles (`/skill`).
    pub skills_root: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedArtifact {
    pub relative_path: String,
    pub max_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactTransferRequest {
    pub destination_dir: PathBuf,
    pub artifacts: Vec<ExpectedArtifact>,
    pub max_total_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferredArtifact {
    pub relative_path: String,
    pub host_path: PathBuf,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxRequest {
    pub job_id: String,
    pub invocation_id: String,
    pub host_workspace_dir: Option<PathBuf>,
    pub read_only_mounts: Vec<ReadOnlyMount>,
    pub command: SandboxCommand,
    pub env: BTreeMap<String, String>,
    pub artifact_transfer: Option<ArtifactTransferRequest>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxExecutionResult {
    pub container_name: String,
    pub run_id: String,
    pub isolation: VerifiedIsolation,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub cleanup_verified: bool,
    pub transferred_artifacts: Vec<TransferredArtifact>,
}

/// Fail-closed interface for rootless engine detection, readiness proof, and sandboxed runs.
pub trait SandboxBackend: Send + Sync {
    fn detect_runtime(&self) -> Result<DetectedRuntime, SandboxError>;
    fn verify_readiness(&self, policy: &SandboxPolicy) -> Result<VerifiedIsolation, SandboxError>;
    fn execute(
        &self,
        policy: &SandboxPolicy,
        request: &SandboxRequest,
    ) -> Result<SandboxExecutionResult, SandboxError>;
    fn cleanup_job(&self, job_id: &str) -> Result<usize, SandboxError>;
}

/// Explicit unavailable backend failing closed with `sandbox_unavailable` without fallback.
#[derive(Clone, Debug)]
pub struct UnavailableSandboxBackend {
    pub reason: String,
}

impl UnavailableSandboxBackend {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl SandboxBackend for UnavailableSandboxBackend {
    fn detect_runtime(&self) -> Result<DetectedRuntime, SandboxError> {
        Err(SandboxError::SandboxUnavailable(self.reason.clone()))
    }

    fn verify_readiness(&self, policy: &SandboxPolicy) -> Result<VerifiedIsolation, SandboxError> {
        policy.validate()?;
        Err(SandboxError::SandboxUnavailable(self.reason.clone()))
    }

    fn execute(
        &self,
        policy: &SandboxPolicy,
        request: &SandboxRequest,
    ) -> Result<SandboxExecutionResult, SandboxError> {
        policy.validate()?;
        validate_identifier("job_id", &request.job_id)?;
        validate_identifier("invocation_id", &request.invocation_id)?;
        Err(SandboxError::SandboxUnavailable(self.reason.clone()))
    }

    fn cleanup_job(&self, job_id: &str) -> Result<usize, SandboxError> {
        validate_identifier("job_id", job_id)?;
        Err(SandboxError::SandboxUnavailable(self.reason.clone()))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OciBackendConfig {
    pub preferred_wsl_distro: Option<String>,
    pub trusted_roots: TrustedStagingRoots,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SupervisedRunOutcome {
    Exited(ProcessOutput),
    TimedOut,
    OutputLimitExceeded {
        stream: &'static str,
        limit_bytes: usize,
    },
    PipeRetainedAfterExit {
        stream: &'static str,
    },
}

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("program not found: {0}")]
    NotFound(String),
    #[error("runner timed out after {0:?}")]
    TimedOut(Duration),
    #[error("runner execution failed: {0}")]
    Failed(String),
}

/// Abstracts host process execution for deterministic tests vs real OS process supervision.
pub trait CommandRunner: Send + Sync {
    fn run_probe(
        &self,
        spec: &CommandSpec,
        timeout: Duration,
    ) -> Result<ProcessOutput, RunnerError>;

    fn run_supervised(
        &self,
        spec: &CommandSpec,
        timeout: Duration,
        max_stdout_bytes: usize,
        max_stderr_bytes: usize,
    ) -> Result<SupervisedRunOutcome, RunnerError>;
}

impl<R: CommandRunner + ?Sized> CommandRunner for Arc<R> {
    fn run_probe(
        &self,
        spec: &CommandSpec,
        timeout: Duration,
    ) -> Result<ProcessOutput, RunnerError> {
        (**self).run_probe(spec, timeout)
    }

    fn run_supervised(
        &self,
        spec: &CommandSpec,
        timeout: Duration,
        max_stdout_bytes: usize,
        max_stderr_bytes: usize,
    ) -> Result<SupervisedRunOutcome, RunnerError> {
        (**self).run_supervised(spec, timeout, max_stdout_bytes, max_stderr_bytes)
    }
}

/// Real OS command runner with strict host environment allowlisting and deadline supervision.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run_probe(
        &self,
        spec: &CommandSpec,
        timeout: Duration,
    ) -> Result<ProcessOutput, RunnerError> {
        match self.run_supervised(spec, timeout, 512 * 1024, 512 * 1024)? {
            SupervisedRunOutcome::Exited(output) => Ok(output),
            SupervisedRunOutcome::TimedOut => Err(RunnerError::TimedOut(timeout)),
            SupervisedRunOutcome::OutputLimitExceeded {
                stream,
                limit_bytes,
            } => Err(RunnerError::Failed(format!(
                "probe '{}' {stream} exceeded {limit_bytes} bytes",
                spec.program
            ))),
            SupervisedRunOutcome::PipeRetainedAfterExit { stream } => Err(RunnerError::Failed(
                format!("probe '{}' {stream} pipe retained after exit", spec.program),
            )),
        }
    }

    fn run_supervised(
        &self,
        spec: &CommandSpec,
        timeout: Duration,
        max_stdout_bytes: usize,
        max_stderr_bytes: usize,
    ) -> Result<SupervisedRunOutcome, RunnerError> {
        let deadline = Instant::now() + timeout;
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();

        // Strictly allowlist minimal host invocation env vars; never forward DOCKER_HOST,
        // DOCKER_CONTEXT, CONTAINER_HOST, or API credentials.
        for var in [
            "PATH",
            "HOME",
            "USER",
            "LOGNAME",
            "USERPROFILE",
            "SYSTEMROOT",
            "WINDIR",
            "TEMP",
            "TMP",
            "XDG_RUNTIME_DIR",
            "XDG_DATA_HOME",
            "XDG_CONFIG_HOME",
            "DBUS_SESSION_BUS_ADDRESS",
        ] {
            if let Ok(value) = std::env::var(var) {
                cmd.env(var, value);
            }
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                RunnerError::NotFound(spec.program.clone())
            } else {
                RunnerError::Failed(err.to_string())
            }
        })?;

        supervise_child(&mut child, deadline, max_stdout_bytes, max_stderr_bytes)
    }
}

pub fn supervise_child(
    child: &mut Child,
    deadline: Instant,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
) -> Result<SupervisedRunOutcome, RunnerError> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RunnerError::Failed("missing child stdout pipe".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| RunnerError::Failed("missing child stderr pipe".into()))?;

    let exceeded = Arc::new(AtomicBool::new(false));
    let exceeded_stream: Arc<std::sync::Mutex<Option<&'static str>>> =
        Arc::new(std::sync::Mutex::new(None));

    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();

    spawn_bounded_reader(
        stdout,
        max_stdout_bytes,
        "stdout",
        Arc::clone(&exceeded),
        Arc::clone(&exceeded_stream),
        out_tx,
    );
    spawn_bounded_reader(
        stderr,
        max_stderr_bytes,
        "stderr",
        Arc::clone(&exceeded),
        Arc::clone(&exceeded_stream),
        err_tx,
    );

    let status: ExitStatus = loop {
        if exceeded.load(Ordering::Acquire) {
            let _ = child.kill();
            let _ = child.wait();
            let stream = exceeded_stream
                .lock()
                .ok()
                .and_then(|guard| *guard)
                .unwrap_or("stdout");
            let limit_bytes = if stream == "stderr" {
                max_stderr_bytes
            } else {
                max_stdout_bytes
            };
            return Ok(SupervisedRunOutcome::OutputLimitExceeded {
                stream,
                limit_bytes,
            });
        }

        let now = Instant::now();
        if now >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(SupervisedRunOutcome::TimedOut);
        }

        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(now);
                thread::sleep(remaining.min(Duration::from_millis(10)));
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunnerError::Failed(err.to_string()));
            }
        }
    };

    let stdout_bytes = receive_stream(&out_rx, "stdout", deadline, &exceeded, max_stdout_bytes)?;
    let stdout_bytes = match stdout_bytes {
        Ok(bytes) => bytes,
        Err(early) => return Ok(early),
    };
    let stderr_bytes = receive_stream(&err_rx, "stderr", deadline, &exceeded, max_stderr_bytes)?;
    let stderr_bytes = match stderr_bytes {
        Ok(bytes) => bytes,
        Err(early) => return Ok(early),
    };

    Ok(SupervisedRunOutcome::Exited(ProcessOutput {
        exit_code: status.code(),
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    }))
}

fn receive_stream(
    rx: &mpsc::Receiver<Result<Vec<u8>, &'static str>>,
    stream_name: &'static str,
    deadline: Instant,
    exceeded: &AtomicBool,
    limit_bytes: usize,
) -> Result<Result<Vec<u8>, SupervisedRunOutcome>, RunnerError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Ok(Err(SupervisedRunOutcome::TimedOut));
    }
    let wait_budget = remaining.min(POST_EXIT_PIPE_DRAIN_CAP);
    match rx.recv_timeout(wait_budget) {
        Ok(Ok(bytes)) => {
            if exceeded.load(Ordering::Acquire) {
                Ok(Err(SupervisedRunOutcome::OutputLimitExceeded {
                    stream: stream_name,
                    limit_bytes,
                }))
            } else {
                Ok(Ok(bytes))
            }
        }
        Ok(Err("exceeded")) => Ok(Err(SupervisedRunOutcome::OutputLimitExceeded {
            stream: stream_name,
            limit_bytes,
        })),
        Ok(Err(err)) => Err(RunnerError::Failed(format!(
            "{stream_name} read error: {err}"
        ))),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if Instant::now() >= deadline {
                Ok(Err(SupervisedRunOutcome::TimedOut))
            } else {
                Ok(Err(SupervisedRunOutcome::PipeRetainedAfterExit {
                    stream: stream_name,
                }))
            }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(RunnerError::Failed(format!(
            "{stream_name} reader channel closed unexpectedly"
        ))),
    }
}

fn spawn_bounded_reader<R: Read + Send + 'static>(
    mut reader: R,
    max_bytes: usize,
    stream_name: &'static str,
    exceeded: Arc<AtomicBool>,
    exceeded_stream: Arc<std::sync::Mutex<Option<&'static str>>>,
    tx: mpsc::Sender<Result<Vec<u8>, &'static str>>,
) {
    thread::spawn(move || {
        let mut collected = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(Ok(collected));
                    break;
                }
                Ok(n) => {
                    if collected.len().saturating_add(n) > max_bytes {
                        if let Ok(mut guard) = exceeded_stream.lock() {
                            if guard.is_none() {
                                *guard = Some(stream_name);
                            }
                        }
                        exceeded.store(true, Ordering::Release);
                        let _ = tx.send(Err("exceeded"));
                        break;
                    }
                    collected.extend_from_slice(&buf[..n]);
                }
                Err(_) => {
                    let _ = tx.send(Err("io_error"));
                    break;
                }
            }
        }
    });
}

/// Linux OCI sandbox backend supporting native Linux and Windows via WSL2.
#[derive(Clone, Debug)]
pub struct OciSandboxBackend<R: CommandRunner> {
    platform: HostPlatform,
    config: OciBackendConfig,
    docker_pin: Option<PinnedDockerRuntime>,
    strict_empirical_gate: Option<bool>,
    runner: R,
}

impl OciSandboxBackend<SystemCommandRunner> {
    pub fn system(config: OciBackendConfig) -> Self {
        Self {
            platform: HostPlatform::current(),
            config,
            docker_pin: None,
            strict_empirical_gate: None,
            runner: SystemCommandRunner,
        }
    }
}

impl<R: CommandRunner> OciSandboxBackend<R> {
    pub fn with_runner(platform: HostPlatform, config: OciBackendConfig, runner: R) -> Self {
        Self {
            platform,
            config,
            docker_pin: None,
            strict_empirical_gate: None,
            runner,
        }
    }

    pub fn with_docker_pin(mut self, pin: PinnedDockerRuntime) -> Self {
        self.docker_pin = Some(pin);
        self
    }

    /// Requires pinned rootless Docker AND live empirical verification of mandatory controls
    /// (including `chromiumSandbox: true` when `verify_chromium` is true) before any execution.
    pub fn with_strict_empirical_gate(mut self, verify_chromium: bool) -> Self {
        self.strict_empirical_gate = Some(verify_chromium);
        self
    }

    pub fn docker_pin(&self) -> Option<&PinnedDockerRuntime> {
        self.docker_pin.as_ref()
    }

    fn probe_budget(
        deadline: Option<Instant>,
        timeout_secs: u64,
    ) -> Result<Duration, SandboxError> {
        match deadline {
            None => Ok(PROBE_TIMEOUT_CAP),
            Some(dl) => {
                let rem = dl.saturating_duration_since(Instant::now());
                if rem.is_zero() {
                    Err(SandboxError::Timeout { timeout_secs })
                } else {
                    Ok(rem.min(PROBE_TIMEOUT_CAP))
                }
            }
        }
    }

    fn detect_transport(
        &self,
        deadline: Option<Instant>,
        timeout_secs: u64,
    ) -> Result<OciTransport, SandboxError> {
        match self.platform {
            HostPlatform::Linux => Ok(OciTransport::NativeLinux),
            HostPlatform::Windows => {
                let mut env = BTreeMap::new();
                env.insert("WSL_UTF8".into(), "1".into());
                let spec = CommandSpec {
                    program: "wsl.exe".into(),
                    args: vec!["--list".into(), "--verbose".into()],
                    env,
                };
                let budget = Self::probe_budget(deadline, timeout_secs)?;
                let output = self
                    .runner
                    .run_probe(&spec, budget)
                    .map_err(|err| match err {
                        RunnerError::TimedOut(_) if deadline.is_some() => {
                            SandboxError::Timeout { timeout_secs }
                        }
                        RunnerError::TimedOut(d) => SandboxError::SandboxUnavailable(format!(
                            "wsl2 probe timed out after {d:?}"
                        )),
                        other => {
                            SandboxError::SandboxUnavailable(format!("wsl2 unavailable: {other}"))
                        }
                    })?;
                if output.exit_code != Some(0) {
                    return Err(SandboxError::SandboxUnavailable(
                        "wsl.exe --list --verbose failed".into(),
                    ));
                }
                let text = decode_wsl_output(&output.stdout);
                let distro =
                    select_wsl2_distro(&text, self.config.preferred_wsl_distro.as_deref())?;
                Ok(OciTransport::Wsl2 { distro })
            }
            HostPlatform::Unsupported => Err(SandboxError::SandboxUnavailable(
                "host OS unsupported; rootless Linux OCI or Windows WSL2 is required".into(),
            )),
        }
    }

    fn detect_runtime_with_deadline(
        &self,
        deadline: Option<Instant>,
        timeout_secs: u64,
    ) -> Result<DetectedRuntime, SandboxError> {
        if let Some(pin) = &self.docker_pin {
            pin.validate()?;
        }
        let transport = self.detect_transport(deadline, timeout_secs)?;
        let mut diagnostics = Vec::new();
        let engines: &[OciEngine] = if self.docker_pin.is_some() {
            &[OciEngine::Docker]
        } else {
            &[OciEngine::Podman, OciEngine::Docker]
        };
        for &engine in engines {
            match self.probe_engine(&transport, engine, deadline, timeout_secs) {
                Ok(runtime) => return Ok(runtime),
                Err(ProbeFailure::Timeout) => return Err(SandboxError::Timeout { timeout_secs }),
                Err(ProbeFailure::Unavailable(reason)) => diagnostics.push(reason),
            }
        }
        Err(SandboxError::SandboxUnavailable(format!(
            "no verified local rootless Linux OCI runtime with mandatory cgroup v2 controls found ({})",
            diagnostics.join("; ")
        )))
    }

    fn probe_engine(
        &self,
        transport: &OciTransport,
        engine: OciEngine,
        deadline: Option<Instant>,
        timeout_secs: u64,
    ) -> Result<DetectedRuntime, ProbeFailure> {
        let mut verified_context_name = None;
        let mut verified_unix_socket = None;
        let mut verified_seccomp_sha = None;

        if matches!(engine, OciEngine::Docker) {
            let (ctx_name, socket_host, seccomp_sha) =
                self.verify_docker_local_context(transport, deadline, timeout_secs)?;
            verified_context_name = ctx_name;
            verified_unix_socket = Some(socket_host);
            verified_seccomp_sha = seccomp_sha;
        }

        let docker_ctx = self.docker_pin.as_ref().map(|p| p.context_name.as_str());
        let spec = wrap_engine_command(
            transport,
            engine,
            docker_ctx,
            vec!["info".into(), "--format".into(), "json".into()],
            BTreeMap::new(),
        );
        let budget =
            Self::probe_budget(deadline, timeout_secs).map_err(|_| ProbeFailure::Timeout)?;
        let output = self
            .runner
            .run_probe(&spec, budget)
            .map_err(|err| match err {
                RunnerError::TimedOut(_) if deadline.is_some() => ProbeFailure::Timeout,
                RunnerError::TimedOut(d) => ProbeFailure::Unavailable(format!(
                    "{} info timed out after {d:?}",
                    engine.binary()
                )),
                other => ProbeFailure::Unavailable(format!("{}: {other}", engine.binary())),
            })?;
        if output.exit_code != Some(0) {
            let err_text = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(ProbeFailure::Unavailable(format!(
                "{} info exited with {:?}: {}",
                engine.binary(),
                output.exit_code,
                truncate_excerpt(&err_text, 200)
            )));
        }

        let payload: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|_| {
            ProbeFailure::Unavailable(format!("{} info returned malformed JSON", engine.binary()))
        })?;

        let mut runtime = match engine {
            OciEngine::Podman => parse_podman_info(&payload, transport)?,
            OciEngine::Docker => parse_docker_info(&payload, transport)?,
        };
        if matches!(engine, OciEngine::Docker) {
            runtime.docker_context_name = verified_context_name;
            runtime.unix_socket = verified_unix_socket;
            runtime.seccomp_profile_sha256 = verified_seccomp_sha;
        }

        if !runtime.controls.tmpfs_inodes_enforced {
            return Err(ProbeFailure::Unavailable(format!(
                "{} does not support mandatory tmpfs inode quota enforcement (nr_inodes)",
                engine.binary()
            )));
        }
        if !runtime.rootless_verified
            || !runtime.local_endpoint_verified
            || !runtime.controls.all_mandatory_enforced()
        {
            return Err(ProbeFailure::Unavailable(format!(
                "{} lacks verified rootless, local endpoint, or mandatory cgroup v2 controls",
                engine.binary()
            )));
        }

        Ok(runtime)
    }

    fn verify_docker_local_context(
        &self,
        transport: &OciTransport,
        deadline: Option<Instant>,
        timeout_secs: u64,
    ) -> Result<(Option<String>, String, Option<String>), ProbeFailure> {
        let mut args = vec![
            "context".into(),
            "inspect".into(),
            "--format".into(),
            "json".into(),
        ];
        if let Some(pin) = &self.docker_pin {
            args.push(pin.context_name.clone());
        }
        let spec = wrap_command(transport, "docker", args, BTreeMap::new());
        let budget =
            Self::probe_budget(deadline, timeout_secs).map_err(|_| ProbeFailure::Timeout)?;
        let output = self
            .runner
            .run_probe(&spec, budget)
            .map_err(|err| match err {
                RunnerError::TimedOut(_) if deadline.is_some() => ProbeFailure::Timeout,
                RunnerError::TimedOut(d) => ProbeFailure::Unavailable(format!(
                    "docker context inspect timed out after {d:?}"
                )),
                other => ProbeFailure::Unavailable(format!("docker context inspect: {other}")),
            })?;
        if output.exit_code != Some(0) {
            return Err(ProbeFailure::Unavailable(
                "docker context inspect failed".into(),
            ));
        }
        let contexts: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|_| {
            ProbeFailure::Unavailable("docker context inspect returned malformed JSON".into())
        })?;
        let Some(first) = contexts.as_array().and_then(|arr| arr.first()) else {
            return Err(ProbeFailure::Unavailable(
                "docker context inspect returned no active context".into(),
            ));
        };
        let actual_name = first
            .get("Name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        let host = first
            .get("Endpoints")
            .and_then(|e| e.get("docker"))
            .and_then(|d| d.get("Host"))
            .and_then(|h| h.as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        if !host.starts_with("unix://") {
            return Err(ProbeFailure::Unavailable(format!(
                "docker context endpoint '{host}' is remote or non-Unix socket (rejected)"
            )));
        }

        if let Some(pin) = &self.docker_pin {
            if actual_name != pin.context_name {
                return Err(ProbeFailure::Unavailable(format!(
                    "docker context mismatch: expected '{}', got '{actual_name}'",
                    pin.context_name
                )));
            }
            if host != pin.unix_socket {
                return Err(ProbeFailure::Unavailable(format!(
                    "docker socket mismatch for '{}': expected '{}', got '{host}'",
                    pin.context_name, pin.unix_socket
                )));
            }
            let seccomp_bytes = self.read_host_seccomp_profile(
                transport,
                &pin.seccomp_profile_path,
                deadline,
                timeout_secs,
            )?;
            let actual_sha = sha256_hex(&seccomp_bytes);
            if actual_sha != pin.seccomp_profile_sha256 {
                return Err(ProbeFailure::Unavailable(format!(
                    "seccomp profile SHA-256 mismatch at '{}': expected {}, got {actual_sha}",
                    pin.seccomp_profile_path, pin.seccomp_profile_sha256
                )));
            }
            validate_seccomp_profile_bytes(&seccomp_bytes).map_err(ProbeFailure::Unavailable)?;
            return Ok((Some(actual_name), host, Some(actual_sha)));
        }

        Ok(((!actual_name.is_empty()).then_some(actual_name), host, None))
    }

    fn read_host_seccomp_profile(
        &self,
        transport: &OciTransport,
        path: &str,
        deadline: Option<Instant>,
        timeout_secs: u64,
    ) -> Result<Vec<u8>, ProbeFailure> {
        let bytes = match transport {
            OciTransport::NativeLinux
                if std::env::var_os("SCENIVOX_TEST_USE_RUNNER_FOR_SECCOMP").is_none() =>
            {
                // Keep tests deterministic via runner when WSL2 or mocked tests run; on native Linux
                // also try direct fs::read if file exists on host, else fall back to runner probe.
                if let Ok(meta) = fs::symlink_metadata(path) {
                    if !meta.file_type().is_file() || meta.len() > MAX_SECCOMP_PROFILE_BYTES as u64
                    {
                        return Err(ProbeFailure::Unavailable(format!(
                            "seccomp profile '{}' is not a bounded regular file",
                            path
                        )));
                    }
                    fs::read(path).map_err(|err| {
                        ProbeFailure::Unavailable(format!(
                            "failed reading seccomp profile '{path}': {err}"
                        ))
                    })?
                } else {
                    self.read_seccomp_via_runner(transport, path, deadline, timeout_secs)?
                }
            }
            _ => self.read_seccomp_via_runner(transport, path, deadline, timeout_secs)?,
        };
        if bytes.is_empty() || bytes.len() > MAX_SECCOMP_PROFILE_BYTES {
            return Err(ProbeFailure::Unavailable(format!(
                "seccomp profile '{}' exceeded valid size bounds (1..={MAX_SECCOMP_PROFILE_BYTES})",
                path
            )));
        }
        Ok(bytes)
    }

    fn read_seccomp_via_runner(
        &self,
        transport: &OciTransport,
        path: &str,
        deadline: Option<Instant>,
        timeout_secs: u64,
    ) -> Result<Vec<u8>, ProbeFailure> {
        let spec = wrap_command(transport, "cat", vec![path.to_owned()], BTreeMap::new());
        let budget =
            Self::probe_budget(deadline, timeout_secs).map_err(|_| ProbeFailure::Timeout)?;
        let out = self
            .runner
            .run_probe(&spec, budget)
            .map_err(|err| match err {
                RunnerError::TimedOut(_) if deadline.is_some() => ProbeFailure::Timeout,
                other => ProbeFailure::Unavailable(format!(
                    "failed reading seccomp profile '{path}': {other}"
                )),
            })?;
        if out.exit_code != Some(0) {
            return Err(ProbeFailure::Unavailable(format!(
                "seccomp profile '{path}' unreadable (exit {:?}): {}",
                out.exit_code,
                truncate_excerpt(&String::from_utf8_lossy(&out.stderr), 160)
            )));
        }
        Ok(out.stdout)
    }

    fn verify_readiness_with_deadline(
        &self,
        policy: &SandboxPolicy,
        deadline: Option<Instant>,
    ) -> Result<VerifiedIsolation, SandboxError> {
        let policy_hash = policy.policy_sha256()?;
        let runtime = self.detect_runtime_with_deadline(deadline, policy.limits.timeout_secs)?;
        let expected_digest = policy.image.digest()?;

        if let Some(pin) = &self.docker_pin {
            if expected_digest != pin.image_digest {
                return Err(SandboxError::SandboxUnavailable(format!(
                    "pinned docker image digest mismatch: policy={}, pinned={}",
                    expected_digest, pin.image_digest
                )));
            }
            if runtime.engine != OciEngine::Docker
                || runtime.docker_context_name.as_deref() != Some(pin.context_name.as_str())
                || runtime.unix_socket.as_deref() != Some(pin.unix_socket.as_str())
                || runtime.seccomp_profile_sha256.as_deref()
                    != Some(pin.seccomp_profile_sha256.as_str())
            {
                return Err(SandboxError::SandboxUnavailable(
                    "detected runtime does not match pinned rootless Docker configuration".into(),
                ));
            }
        }

        if policy.workspace_mode != WorkspaceQuotaMode::TmpfsQuota {
            return Err(SandboxError::SandboxUnavailable(
                "host bind-mount workspaces cannot guarantee enforceable runtime byte/inode quotas"
                    .into(),
            ));
        }

        let docker_ctx = self.docker_pin.as_ref().map(|p| p.context_name.as_str());
        let spec = wrap_engine_command(
            &runtime.transport,
            runtime.engine,
            docker_ctx,
            vec![
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Id}}|{{json .RepoDigests}}".into(),
                policy.image.reference.clone(),
            ],
            BTreeMap::new(),
        );
        let budget = Self::probe_budget(deadline, policy.limits.timeout_secs)?;
        let output = self
            .runner
            .run_probe(&spec, budget)
            .map_err(|err| match err {
                RunnerError::TimedOut(_) if deadline.is_some() => SandboxError::Timeout {
                    timeout_secs: policy.limits.timeout_secs,
                },
                other => {
                    SandboxError::SandboxUnavailable(format!("local image inspect failed: {other}"))
                }
            })?;
        if output.exit_code != Some(0) {
            let err_text = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(SandboxError::SandboxUnavailable(format!(
                "digest-pinned image '{}' is not locally available (no auto-pull): {}",
                policy.image.reference,
                truncate_excerpt(&err_text, 200)
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let (local_id, repo_digests_json) = stdout.split_once('|').ok_or_else(|| {
            SandboxError::SandboxUnavailable("malformed image inspect output".into())
        })?;
        let local_id = local_id.trim();
        let repo_digests: Vec<String> =
            serde_json::from_str(repo_digests_json.trim()).map_err(|_| {
                SandboxError::SandboxUnavailable(
                    "malformed RepoDigests array from image inspect".into(),
                )
            })?;

        let matches_digest = local_id == expected_digest
            || repo_digests
                .iter()
                .any(|entry| entry.ends_with(&format!("@{expected_digest}")));
        if !matches_digest {
            return Err(SandboxError::SandboxUnavailable(format!(
                "local image digest mismatch for '{}'; expected {}",
                policy.image.reference, expected_digest
            )));
        }

        if !runtime.controls.tmpfs_inodes_enforced {
            return Err(SandboxError::SandboxUnavailable(format!(
                "mandatory OCI tmpfs inode quota enforcement (nr_inodes) is unsupported by {} engine metadata",
                runtime.engine.binary()
            )));
        }

        let mandatory_controls_verified = runtime.controls.all_mandatory_enforced();
        if !mandatory_controls_verified {
            return Err(SandboxError::SandboxUnavailable(
                "mandatory OCI resource controls are not verified".into(),
            ));
        }

        let metadata_source = runtime.metadata_source;
        let docker_context_name = runtime.docker_context_name.clone();
        let unix_socket = runtime.unix_socket.clone();
        let seccomp_profile_sha256 = runtime.seccomp_profile_sha256.clone();
        Ok(VerifiedIsolation {
            runtime,
            image_reference: policy.image.reference.clone(),
            image_digest: expected_digest,
            local_image_id: local_id.to_owned(),
            runtime_policy_sha256: policy_hash,
            docker_context_name,
            unix_socket,
            seccomp_profile_sha256,
            controls_evidence_source: metadata_source,
            empirically_verified: matches!(
                metadata_source,
                ControlEvidenceSource::EmpiricallyVerified
            ),
            mandatory_controls_verified: true,
            isolation_verified: true,
        })
    }

    /// Empirically verifies cgroups v2, network isolation, read-only rootfs, tmpfs inode exhaustion,
    /// and (when enabled) Chromium `chromiumSandbox: true` renderer isolation inside the pinned container.
    pub fn verify_empirical_isolation(
        &self,
        policy: &SandboxPolicy,
        verify_chromium: bool,
    ) -> Result<(VerifiedIsolation, EmpiricalControlProbe), SandboxError> {
        let started = Instant::now();
        let deadline = started + Duration::from_secs(policy.limits.timeout_secs);
        let mut isolation = self.verify_readiness_with_deadline(policy, Some(deadline))?;

        let run_uuid = Uuid::new_v4();
        let run_id = run_uuid.to_string();
        let run_hex = run_uuid.simple().to_string();
        let container_name = format!("scenivox-probe-{}", &run_hex[..16]);
        let synthetic_req = SandboxRequest {
            job_id: "00000000-0000-4000-8000-000000000001".into(),
            invocation_id: "empirical-probe".into(),
            host_workspace_dir: None,
            read_only_mounts: Vec::new(),
            command: SandboxCommand::MediaTool {
                tool: MediaTool::Ffmpeg,
                args: vec!["-version".into()],
            },
            env: BTreeMap::new(),
            artifact_transfer: None,
        };

        let probe_args = vec![
            "-c".into(),
            EMPIRICAL_PROBE_PYTHON.into(),
            policy.limits.workspace_inodes.to_string(),
            if verify_chromium { "1" } else { "0" }.into(),
        ];
        let seccomp_path = self
            .docker_pin
            .as_ref()
            .map(|p| p.seccomp_profile_path.as_str());
        let oci_args = build_oci_run_args_with_seccomp(
            policy,
            &synthetic_req,
            &isolation.runtime_policy_sha256,
            &run_id,
            &container_name,
            &isolation.runtime.transport,
            seccomp_path,
            &[],
            ScriptRuntime::Python3.binary(),
            &probe_args,
        )?;
        let docker_ctx = self.docker_pin.as_ref().map(|p| p.context_name.as_str());
        let spec = wrap_engine_command(
            &isolation.runtime.transport,
            isolation.runtime.engine,
            docker_ctx,
            oci_args,
            BTreeMap::new(),
        );
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(SandboxError::Timeout {
                timeout_secs: policy.limits.timeout_secs,
            });
        }

        let outcome = self
            .runner
            .run_supervised(
                &spec,
                remaining,
                policy.limits.max_stdout_bytes,
                policy.limits.max_stderr_bytes,
            )
            .map_err(|err| {
                if let Err(cleanup_err) = self.verify_and_remove_container(
                    &isolation.runtime,
                    &container_name,
                    "empirical_probe_spawn_error",
                ) {
                    return cleanup_err;
                }
                match err {
                    RunnerError::TimedOut(_) => SandboxError::Timeout {
                        timeout_secs: policy.limits.timeout_secs,
                    },
                    other => SandboxError::SandboxUnavailable(format!(
                        "failed spawning empirical probe container: {other}"
                    )),
                }
            })?;

        let output = match outcome {
            SupervisedRunOutcome::TimedOut => {
                self.verify_and_remove_container(
                    &isolation.runtime,
                    &container_name,
                    "empirical_probe_timeout",
                )?;
                return Err(SandboxError::Timeout {
                    timeout_secs: policy.limits.timeout_secs,
                });
            }
            SupervisedRunOutcome::OutputLimitExceeded {
                stream,
                limit_bytes,
            } => {
                self.verify_and_remove_container(
                    &isolation.runtime,
                    &container_name,
                    "empirical_probe_output",
                )?;
                return Err(SandboxError::OutputLimitExceeded {
                    stream,
                    limit_bytes,
                });
            }
            SupervisedRunOutcome::PipeRetainedAfterExit { stream } => {
                self.verify_and_remove_container(
                    &isolation.runtime,
                    &container_name,
                    "empirical_probe_pipe",
                )?;
                return Err(SandboxError::ExecutionFailed {
                    exit_code: None,
                    stderr_excerpt: format!("empirical probe retained {stream} pipe"),
                });
            }
            SupervisedRunOutcome::Exited(out) => {
                self.verify_and_remove_container(
                    &isolation.runtime,
                    &container_name,
                    "empirical_probe_post_exit",
                )?;
                out
            }
        };

        if output.exit_code != Some(0) {
            return Err(SandboxError::SandboxUnavailable(format!(
                "empirical sandbox probe exited {:?}: {}",
                output.exit_code,
                truncate_excerpt(&String::from_utf8_lossy(&output.stderr), 300)
            )));
        }
        let probe: EmpiricalControlProbe =
            serde_json::from_slice(&output.stdout).map_err(|err| {
                SandboxError::SandboxUnavailable(format!(
                    "malformed empirical probe JSON output: {err}"
                ))
            })?;
        validate_empirical_probe_against_policy(&probe, policy, verify_chromium)?;

        isolation.runtime.metadata_source = ControlEvidenceSource::EmpiricallyVerified;
        isolation.controls_evidence_source = ControlEvidenceSource::EmpiricallyVerified;
        isolation.empirically_verified = true;
        Ok((isolation, probe))
    }

    fn verify_and_remove_container(
        &self,
        runtime: &DetectedRuntime,
        container_name: &str,
        context: &str,
    ) -> Result<(), SandboxError> {
        let mut rm_args = vec!["rm".into(), "-f".into()];
        if runtime.engine == OciEngine::Podman {
            // Podman defaults to waiting 10s after SIGTERM before SIGKILL; force immediate kill during cleanup.
            rm_args.push("-t".into());
            rm_args.push("0".into());
        }
        rm_args.push(container_name.into());
        let docker_ctx = self.docker_pin.as_ref().map(|p| p.context_name.as_str());
        let rm_spec = wrap_engine_command(
            &runtime.transport,
            runtime.engine,
            docker_ctx,
            rm_args,
            BTreeMap::new(),
        );
        let rm_out = self
            .runner
            .run_probe(&rm_spec, CLEANUP_TIMEOUT)
            .map_err(|err| SandboxError::CleanupFailed {
                context: context.into(),
                detail: format!("rm -f failed for {container_name}: {err}"),
            })?;

        if rm_out.exit_code == Some(0) {
            return Ok(());
        }

        let stderr_lower = String::from_utf8_lossy(&rm_out.stderr).to_ascii_lowercase();
        let claims_missing = stderr_lower.contains("no such container")
            || stderr_lower.contains("no container with name")
            || stderr_lower.contains("not found");
        if !claims_missing {
            return Err(SandboxError::CleanupFailed {
                context: context.into(),
                detail: format!(
                    "rm -f {container_name} failed ({:?}): {}",
                    rm_out.exit_code,
                    truncate_excerpt(&stderr_lower, 200)
                ),
            });
        }

        // Confirm container is truly absent rather than trusting a text heuristic alone.
        let ps_spec = wrap_engine_command(
            &runtime.transport,
            runtime.engine,
            docker_ctx,
            vec![
                "ps".into(),
                "-a".into(),
                "--filter".into(),
                format!("name=^{container_name}$"),
                "--format".into(),
                "{{.Names}}".into(),
            ],
            BTreeMap::new(),
        );
        let ps_out = self
            .runner
            .run_probe(&ps_spec, CLEANUP_TIMEOUT)
            .map_err(|err| SandboxError::CleanupFailed {
                context: context.into(),
                detail: format!("container absence check failed for {container_name}: {err}"),
            })?;
        if ps_out.exit_code != Some(0) {
            return Err(SandboxError::CleanupFailed {
                context: context.into(),
                detail: format!(
                    "container absence check for {container_name} exited with {:?}",
                    ps_out.exit_code
                ),
            });
        }
        let still_present = String::from_utf8_lossy(&ps_out.stdout)
            .lines()
            .any(|line| line.trim() == container_name);
        if still_present {
            return Err(SandboxError::CleanupFailed {
                context: context.into(),
                detail: format!("container {container_name} still present after cleanup"),
            });
        }
        Ok(())
    }
}

enum ProbeFailure {
    Timeout,
    Unavailable(String),
}

/// Verifies that a previously recorded `VerifiedIsolation` proves strict empirical
/// isolation against the pinned rootless Docker runtime and requested `SandboxPolicy`.
pub fn verify_verified_isolation_for_dispatch(
    policy: &SandboxPolicy,
    pin: &PinnedDockerRuntime,
    isolation: &VerifiedIsolation,
) -> Result<(), SandboxError> {
    policy.validate()?;
    pin.validate()?;
    let expected_policy_sha = policy.policy_sha256()?;
    let expected_digest = policy.image.digest()?;
    if expected_digest != pin.image_digest || isolation.image_digest != pin.image_digest {
        return Err(SandboxError::SandboxUnavailable(
            "verified isolation image_digest does not match pinned rootless Docker digest".into(),
        ));
    }
    if isolation.runtime_policy_sha256 != expected_policy_sha {
        return Err(SandboxError::SandboxUnavailable(
            "verified isolation runtime_policy_sha256 does not match SandboxPolicy".into(),
        ));
    }
    if isolation.runtime.engine != OciEngine::Docker
        || isolation.docker_context_name.as_deref() != Some(pin.context_name.as_str())
        || isolation.unix_socket.as_deref() != Some(pin.unix_socket.as_str())
        || isolation.seccomp_profile_sha256.as_deref() != Some(pin.seccomp_profile_sha256.as_str())
    {
        return Err(SandboxError::SandboxUnavailable(
            "verified isolation does not match pinned scenivox-rootless Docker context/socket/seccomp"
                .into(),
        ));
    }
    if !isolation.isolation_verified
        || !isolation.mandatory_controls_verified
        || !isolation.empirically_verified
        || isolation.controls_evidence_source != ControlEvidenceSource::EmpiricallyVerified
        || !isolation.runtime.controls.all_mandatory_enforced()
    {
        return Err(SandboxError::SandboxUnavailable(
            "strict sandbox dispatch requires empirically verified isolation (metadata-only readiness rejected)"
                .into(),
        ));
    }
    Ok(())
}

struct PreparedSandboxExecution {
    entrypoint: String,
    container_cmd_args: Vec<String>,
    validated_mounts: Vec<(PathBuf, String)>,
    validated_transfer: Option<ValidatedArtifactTransfer>,
}

impl<R: CommandRunner> OciSandboxBackend<R> {
    fn prepare_sandbox_execution(
        &self,
        policy: &SandboxPolicy,
        request: &SandboxRequest,
    ) -> Result<PreparedSandboxExecution, SandboxError> {
        policy.validate()?;
        validate_identifier("job_id", &request.job_id)?;
        validate_identifier("invocation_id", &request.invocation_id)?;
        validate_env(&request.env)?;
        let (entrypoint, container_cmd_args) = request.command.validate()?;

        match policy.workspace_mode {
            WorkspaceQuotaMode::TmpfsQuota => {
                if request.host_workspace_dir.is_some() {
                    return Err(SandboxError::InvalidMount(
                        "host_workspace_dir must be None when using tmpfs_quota workspace mode"
                            .into(),
                    ));
                }
            }
            WorkspaceQuotaMode::HostBindWithVerifiedQuota => {
                return Err(SandboxError::SandboxUnavailable(
                    "host bind-mount workspaces cannot enforce runtime byte and inode quotas"
                        .into(),
                ));
            }
        }

        if request.read_only_mounts.len() > MAX_READ_ONLY_MOUNTS {
            return Err(SandboxError::InvalidMount(
                "too many read_only_mounts requested".into(),
            ));
        }
        let validated_mounts = self
            .config
            .trusted_roots
            .validate_mounts(&request.job_id, &request.read_only_mounts)?;

        let validated_transfer = request
            .artifact_transfer
            .as_ref()
            .map(|transfer| {
                validate_artifact_transfer(
                    &self.config.trusted_roots,
                    &request.job_id,
                    policy,
                    &validated_mounts,
                    transfer,
                )
            })
            .transpose()?;

        if matches!(
            request.command,
            SandboxCommand::Script {
                root: ScriptMountRoot::Skill,
                ..
            }
        ) && !validated_mounts
            .iter()
            .any(|(_, dst)| dst == "/skill" || dst.starts_with("/skill/"))
        {
            return Err(SandboxError::InvalidMount(
                "script targeting /skill requires an authorized /skill read-only mount".into(),
            ));
        }

        Ok(PreparedSandboxExecution {
            entrypoint,
            container_cmd_args,
            validated_mounts,
            validated_transfer,
        })
    }

    /// Executes a bounded container request ONLY after validating that the supplied
    /// `VerifiedIsolation` matches the pinned rootless Docker runtime, seccomp profile,
    /// policy SHA-256, and empirical verification evidence.
    pub fn execute_with_verified_isolation(
        &self,
        policy: &SandboxPolicy,
        isolation: &VerifiedIsolation,
        request: &SandboxRequest,
    ) -> Result<SandboxExecutionResult, SandboxError> {
        let pin = self.docker_pin.as_ref().ok_or_else(|| {
            SandboxError::SandboxUnavailable(
                "strict sandbox dispatch requires PinnedDockerRuntime configuration".into(),
            )
        })?;
        verify_verified_isolation_for_dispatch(policy, pin, isolation)?;
        let prepared = self.prepare_sandbox_execution(policy, request)?;
        let started = Instant::now();
        let deadline = started + Duration::from_secs(policy.limits.timeout_secs);
        self.execute_prepared_with_isolation(
            policy,
            request,
            prepared,
            isolation.clone(),
            started,
            deadline,
        )
    }

    fn execute_prepared_with_isolation(
        &self,
        policy: &SandboxPolicy,
        request: &SandboxRequest,
        prepared: PreparedSandboxExecution,
        isolation: VerifiedIsolation,
        started: Instant,
        deadline: Instant,
    ) -> Result<SandboxExecutionResult, SandboxError> {
        let PreparedSandboxExecution {
            entrypoint,
            container_cmd_args,
            validated_mounts,
            validated_transfer,
        } = prepared;

        let run_uuid = Uuid::new_v4();
        let run_id = run_uuid.to_string();
        let run_hex = run_uuid.simple().to_string();
        let container_name = format!(
            "scenivox-{}-{}",
            sanitize_container_fragment(&request.job_id, 12),
            &run_hex[..16]
        );

        let seccomp_path = self
            .docker_pin
            .as_ref()
            .map(|p| p.seccomp_profile_path.as_str());
        let docker_ctx = self.docker_pin.as_ref().map(|p| p.context_name.as_str());

        let oci_args = match &validated_transfer {
            Some(transfer) => build_oci_artifact_run_args(
                policy,
                request,
                &isolation.runtime_policy_sha256,
                &run_id,
                &container_name,
                &isolation.runtime.transport,
                seccomp_path,
                &validated_mounts,
                &entrypoint,
                &container_cmd_args,
                transfer,
            )?,
            None => build_oci_run_args_with_seccomp(
                policy,
                request,
                &isolation.runtime_policy_sha256,
                &run_id,
                &container_name,
                &isolation.runtime.transport,
                seccomp_path,
                &validated_mounts,
                &entrypoint,
                &container_cmd_args,
            )?,
        };

        let supervised_stdout_limit = match &validated_transfer {
            Some(transfer) => {
                let total_bytes = usize::try_from(transfer.max_total_bytes).map_err(|_| {
                    SandboxError::ArtifactTransferFailed(
                        "artifact max_total_bytes overflows host usize".into(),
                    )
                })?;
                policy
                    .limits
                    .max_stdout_bytes
                    .checked_add(total_bytes)
                    .and_then(|v| v.checked_add(ARTIFACT_PROTOCOL_OVERHEAD_BYTES))
                    .ok_or_else(|| {
                        SandboxError::ArtifactTransferFailed(
                            "supervised stdout buffer limit overflow".into(),
                        )
                    })?
            }
            None => policy.limits.max_stdout_bytes,
        };

        let spec = wrap_engine_command(
            &isolation.runtime.transport,
            isolation.runtime.engine,
            docker_ctx,
            oci_args,
            BTreeMap::new(),
        );

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(SandboxError::Timeout {
                timeout_secs: policy.limits.timeout_secs,
            });
        }

        let outcome = self
            .runner
            .run_supervised(
                &spec,
                remaining,
                supervised_stdout_limit,
                policy.limits.max_stderr_bytes,
            )
            .map_err(|err| {
                if let Err(cleanup_err) = self.verify_and_remove_container(
                    &isolation.runtime,
                    &container_name,
                    "spawn_error",
                ) {
                    return cleanup_err;
                }
                match err {
                    RunnerError::TimedOut(_) => SandboxError::Timeout {
                        timeout_secs: policy.limits.timeout_secs,
                    },
                    other => SandboxError::SandboxUnavailable(format!(
                        "failed to spawn OCI runtime: {other}"
                    )),
                }
            })?;

        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        match outcome {
            SupervisedRunOutcome::TimedOut => {
                self.verify_and_remove_container(&isolation.runtime, &container_name, "timeout")?;
                Err(SandboxError::Timeout {
                    timeout_secs: policy.limits.timeout_secs,
                })
            }
            SupervisedRunOutcome::OutputLimitExceeded {
                stream,
                limit_bytes,
            } => {
                self.verify_and_remove_container(
                    &isolation.runtime,
                    &container_name,
                    "output_limit_exceeded",
                )?;
                Err(SandboxError::OutputLimitExceeded {
                    stream,
                    limit_bytes,
                })
            }
            SupervisedRunOutcome::PipeRetainedAfterExit { stream } => {
                self.verify_and_remove_container(
                    &isolation.runtime,
                    &container_name,
                    "pipe_retained_after_exit",
                )?;
                Err(SandboxError::ExecutionFailed {
                    exit_code: None,
                    stderr_excerpt: format!(
                        "descendant process retained {stream} pipe after container process exited"
                    ),
                })
            }
            SupervisedRunOutcome::Exited(output) => {
                self.verify_and_remove_container(&isolation.runtime, &container_name, "post_exit")?;
                if output.exit_code != Some(0) {
                    let stderr_excerpt =
                        truncate_excerpt(&String::from_utf8_lossy(&output.stderr), 512);
                    return Err(SandboxError::ExecutionFailed {
                        exit_code: output.exit_code,
                        stderr_excerpt,
                    });
                }

                let (stdout, transferred_artifacts) = match validated_transfer {
                    Some(transfer) => {
                        let decoded = decode_framed_artifact_stream(
                            &output.stdout,
                            policy.limits.max_stdout_bytes,
                            &transfer,
                        )?;
                        let transferred = write_transferred_artifacts_safely(
                            &transfer.destination,
                            &decoded.artifacts,
                        )?;
                        (
                            String::from_utf8_lossy(&decoded.child_stdout).into_owned(),
                            transferred,
                        )
                    }
                    None => (
                        String::from_utf8_lossy(&output.stdout).into_owned(),
                        Vec::new(),
                    ),
                };

                Ok(SandboxExecutionResult {
                    container_name,
                    run_id,
                    isolation,
                    exit_code: 0,
                    stdout,
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    duration_ms,
                    cleanup_verified: true,
                    transferred_artifacts,
                })
            }
        }
    }
}

impl<R: CommandRunner> SandboxBackend for OciSandboxBackend<R> {
    fn detect_runtime(&self) -> Result<DetectedRuntime, SandboxError> {
        self.detect_runtime_with_deadline(None, 10)
    }

    fn verify_readiness(&self, policy: &SandboxPolicy) -> Result<VerifiedIsolation, SandboxError> {
        if let Some(verify_chromium) = self.strict_empirical_gate {
            let pin = self.docker_pin.as_ref().ok_or_else(|| {
                SandboxError::SandboxUnavailable(
                    "strict sandbox gate requires PinnedDockerRuntime configuration".into(),
                )
            })?;
            let (isolation, _) = self.verify_empirical_isolation(policy, verify_chromium)?;
            verify_verified_isolation_for_dispatch(policy, pin, &isolation)?;
            Ok(isolation)
        } else {
            self.verify_readiness_with_deadline(policy, None)
        }
    }

    fn execute(
        &self,
        policy: &SandboxPolicy,
        request: &SandboxRequest,
    ) -> Result<SandboxExecutionResult, SandboxError> {
        let prepared = self.prepare_sandbox_execution(policy, request)?;
        let started = Instant::now();
        let deadline = started + Duration::from_secs(policy.limits.timeout_secs);
        let isolation = if let Some(verify_chromium) = self.strict_empirical_gate {
            let pin = self.docker_pin.as_ref().ok_or_else(|| {
                SandboxError::SandboxUnavailable(
                    "strict sandbox gate requires PinnedDockerRuntime configuration".into(),
                )
            })?;
            let (iso, _) = self.verify_empirical_isolation(policy, verify_chromium)?;
            verify_verified_isolation_for_dispatch(policy, pin, &iso)?;
            iso
        } else {
            self.verify_readiness_with_deadline(policy, Some(deadline))?
        };
        self.execute_prepared_with_isolation(
            policy, request, prepared, isolation, started, deadline,
        )
    }

    fn cleanup_job(&self, job_id: &str) -> Result<usize, SandboxError> {
        validate_identifier("job_id", job_id)?;
        let runtime = self.detect_runtime()?;
        let docker_ctx = self.docker_pin.as_ref().map(|p| p.context_name.as_str());
        let ps_spec = wrap_engine_command(
            &runtime.transport,
            runtime.engine,
            docker_ctx,
            vec![
                "ps".into(),
                "-a".into(),
                "--filter".into(),
                "label=scenivox.managed=true".into(),
                "--filter".into(),
                format!("label=scenivox.job_id={job_id}"),
                "--format".into(),
                "{{.Names}}".into(),
            ],
            BTreeMap::new(),
        );
        let output = self
            .runner
            .run_probe(&ps_spec, CLEANUP_TIMEOUT)
            .map_err(|err| SandboxError::CleanupFailed {
                context: "cleanup_job_list".into(),
                detail: err.to_string(),
            })?;
        if output.exit_code != Some(0) {
            return Err(SandboxError::CleanupFailed {
                context: "cleanup_job_list".into(),
                detail: format!("container list failed with {:?}", output.exit_code),
            });
        }

        let names: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        let mut removed = 0usize;
        for name in &names {
            if !is_safe_container_name(name) {
                return Err(SandboxError::CleanupFailed {
                    context: "cleanup_job".into(),
                    detail: format!("unexpected container name '{name}'"),
                });
            }
            self.verify_and_remove_container(&runtime, name, "cleanup_job")?;
            removed += 1;
        }
        Ok(removed)
    }
}

fn parse_podman_info(
    payload: &serde_json::Value,
    transport: &OciTransport,
) -> Result<DetectedRuntime, ProbeFailure> {
    let host = payload
        .get("host")
        .ok_or_else(|| ProbeFailure::Unavailable("podman info missing 'host' object".into()))?;

    if host
        .get("serviceIsRemote")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Err(ProbeFailure::Unavailable(
            "remote podman connection is prohibited".into(),
        ));
    }

    if let Some(os) = host.get("os").and_then(|v| v.as_str()) {
        if !os.eq_ignore_ascii_case("linux") {
            return Err(ProbeFailure::Unavailable(format!(
                "podman host OS '{os}' is not linux"
            )));
        }
    }

    let rootless = host
        .get("security")
        .and_then(|s| s.get("rootless"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !rootless {
        return Err(ProbeFailure::Unavailable(
            "podman is not running in rootless mode".into(),
        ));
    }

    let cgroup_version = host
        .get("cgroupVersion")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if cgroup_version != "v2" {
        return Err(ProbeFailure::Unavailable(format!(
            "podman requires cgroups v2 for rootless resource enforcement (got '{cgroup_version}')"
        )));
    }

    let controllers: Vec<&str> = host
        .get("cgroupControllers")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let cpu_ok = controllers.contains(&"cpu");
    let mem_ok = controllers.contains(&"memory");
    let pids_ok = controllers.contains(&"pids");

    let oci_runtime = host
        .get("ociRuntime")
        .and_then(|o| o.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !matches!(oci_runtime, "crun" | "runc") {
        return Err(ProbeFailure::Unavailable(format!(
            "podman ociRuntime '{oci_runtime}' is not trusted (expected crun or runc)"
        )));
    }

    let version = payload
        .get("version")
        .and_then(|v| v.get("Version"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if version.is_empty() {
        return Err(ProbeFailure::Unavailable(
            "podman reported empty version".into(),
        ));
    }

    let controls = EffectiveResourceControls {
        cpu_quota_enforced: cpu_ok,
        memory_limit_enforced: mem_ok,
        swap_limit_enforced: mem_ok,
        pids_limit_enforced: pids_ok,
        tmpfs_size_enforced: true,
        tmpfs_inodes_enforced: false,
    };

    Ok(DetectedRuntime {
        engine: OciEngine::Podman,
        transport: transport.clone(),
        oci_runtime_name: oci_runtime.to_owned(),
        engine_version: version.to_owned(),
        cgroup_version: cgroup_version.to_owned(),
        rootless_verified: true,
        local_endpoint_verified: true,
        metadata_source: ControlEvidenceSource::EngineMetadataOnly,
        controls,
        docker_context_name: None,
        unix_socket: None,
        seccomp_profile_sha256: None,
    })
}

fn parse_docker_info(
    payload: &serde_json::Value,
    transport: &OciTransport,
) -> Result<DetectedRuntime, ProbeFailure> {
    if let Some(os_type) = payload.get("OSType").and_then(|v| v.as_str()) {
        if !os_type.eq_ignore_ascii_case("linux") {
            return Err(ProbeFailure::Unavailable(format!(
                "docker OSType '{os_type}' is not linux"
            )));
        }
    }

    let rootless = payload
        .get("SecurityOptions")
        .and_then(|v| v.as_array())
        .is_some_and(|opts| {
            opts.iter().filter_map(|v| v.as_str()).any(|opt| {
                opt == "rootless" || opt == "name=rootless" || opt.starts_with("name=rootless,")
            })
        });
    if !rootless {
        return Err(ProbeFailure::Unavailable(
            "docker is running without verified rootless security option".into(),
        ));
    }

    let cgroup_version = payload
        .get("CgroupVersion")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if cgroup_version != "2" {
        return Err(ProbeFailure::Unavailable(format!(
            "docker requires CgroupVersion '2' for rootless limits (got '{cgroup_version}')"
        )));
    }

    let bool_flag = |key: &str| payload.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
    let cpu_ok = bool_flag("CpuCfsPeriod") && bool_flag("CpuCfsQuota");
    let mem_ok = bool_flag("MemoryLimit");
    let swap_ok = bool_flag("SwapLimit");
    let pids_ok = bool_flag("PidsLimit");

    let oci_runtime = payload
        .get("DefaultRuntime")
        .and_then(|v| v.as_str())
        .unwrap_or("runc");
    if !matches!(oci_runtime, "runc" | "crun" | "io.containerd.runc.v2") {
        return Err(ProbeFailure::Unavailable(format!(
            "docker runtime '{oci_runtime}' is not a trusted Linux OCI runtime"
        )));
    }

    let version = payload
        .get("ServerVersion")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if version.is_empty() {
        return Err(ProbeFailure::Unavailable(
            "docker reported empty ServerVersion".into(),
        ));
    }

    let controls = EffectiveResourceControls {
        cpu_quota_enforced: cpu_ok,
        memory_limit_enforced: mem_ok,
        swap_limit_enforced: swap_ok,
        pids_limit_enforced: pids_ok,
        tmpfs_size_enforced: true,
        tmpfs_inodes_enforced: true,
    };

    Ok(DetectedRuntime {
        engine: OciEngine::Docker,
        transport: transport.clone(),
        oci_runtime_name: oci_runtime.to_owned(),
        engine_version: version.to_owned(),
        cgroup_version: cgroup_version.to_owned(),
        rootless_verified: true,
        local_endpoint_verified: true,
        metadata_source: ControlEvidenceSource::EngineMetadataOnly,
        controls,
        docker_context_name: None,
        unix_socket: None,
        seccomp_profile_sha256: None,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn build_oci_run_args(
    policy: &SandboxPolicy,
    request: &SandboxRequest,
    policy_hash: &str,
    run_id: &str,
    container_name: &str,
    transport: &OciTransport,
    validated_mounts: &[(PathBuf, String)],
    entrypoint: &str,
    command_args: &[String],
) -> Result<Vec<String>, SandboxError> {
    build_oci_run_args_with_seccomp(
        policy,
        request,
        policy_hash,
        run_id,
        container_name,
        transport,
        None,
        validated_mounts,
        entrypoint,
        command_args,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn build_oci_run_args_with_seccomp(
    policy: &SandboxPolicy,
    request: &SandboxRequest,
    policy_hash: &str,
    run_id: &str,
    container_name: &str,
    transport: &OciTransport,
    seccomp_profile_path: Option<&str>,
    validated_mounts: &[(PathBuf, String)],
    entrypoint: &str,
    command_args: &[String],
) -> Result<Vec<String>, SandboxError> {
    let mut args = build_oci_hardened_prefix(
        policy,
        request,
        policy_hash,
        run_id,
        container_name,
        transport,
        seccomp_profile_path,
        validated_mounts,
    )?;
    args.push(format!("--entrypoint={entrypoint}"));
    args.push(policy.image.reference.clone());
    args.extend(command_args.iter().cloned());
    Ok(args)
}

#[allow(clippy::too_many_arguments)]
fn build_oci_artifact_run_args(
    policy: &SandboxPolicy,
    request: &SandboxRequest,
    policy_hash: &str,
    run_id: &str,
    container_name: &str,
    transport: &OciTransport,
    seccomp_profile_path: Option<&str>,
    validated_mounts: &[(PathBuf, String)],
    entrypoint: &str,
    command_args: &[String],
    transfer: &ValidatedArtifactTransfer,
) -> Result<Vec<String>, SandboxError> {
    let mut args = build_oci_hardened_prefix(
        policy,
        request,
        policy_hash,
        run_id,
        container_name,
        transport,
        seccomp_profile_path,
        validated_mounts,
    )?;
    args.push(format!("--entrypoint={}", ScriptRuntime::Python3.binary()));
    args.push(policy.image.reference.clone());
    args.push(ARTIFACT_RUNNER_CONTAINER_PATH.into());
    args.push("--max-stdout".into());
    args.push(policy.limits.max_stdout_bytes.to_string());
    args.push("--max-stderr".into());
    args.push(policy.limits.max_stderr_bytes.to_string());
    args.push("--max-total".into());
    args.push(transfer.max_total_bytes.to_string());
    for expected in &transfer.artifacts {
        args.push("--artifact".into());
        args.push(format!("{}:{}", expected.relative_path, expected.max_bytes));
    }
    args.push("--".into());
    args.push(entrypoint.to_owned());
    args.extend(command_args.iter().cloned());
    Ok(args)
}

#[allow(clippy::too_many_arguments)]
fn build_oci_hardened_prefix(
    policy: &SandboxPolicy,
    request: &SandboxRequest,
    policy_hash: &str,
    run_id: &str,
    container_name: &str,
    transport: &OciTransport,
    seccomp_profile_path: Option<&str>,
    validated_mounts: &[(PathBuf, String)],
) -> Result<Vec<String>, SandboxError> {
    policy.validate()?;
    if policy.workspace_mode != WorkspaceQuotaMode::TmpfsQuota {
        return Err(SandboxError::SandboxUnavailable(
            "host bind-mount workspaces cannot enforce runtime byte/inode quotas".into(),
        ));
    }
    if !is_safe_container_name(container_name) {
        return Err(SandboxError::InvalidInvocation(
            "invalid container name".into(),
        ));
    }

    let mut args = vec![
        "run".into(),
        "--rm".into(),
        "--name".into(),
        container_name.into(),
        "--label".into(),
        "scenivox.managed=true".into(),
        "--label".into(),
        format!("scenivox.job_id={}", request.job_id),
        "--label".into(),
        format!("scenivox.invocation_id={}", request.invocation_id),
        "--label".into(),
        format!("scenivox.run_id={run_id}"),
        "--label".into(),
        format!("scenivox.policy_sha256={policy_hash}"),
        "--pull=never".into(),
        "--network=none".into(),
        "--read-only".into(),
        "--cap-drop=ALL".into(),
        "--security-opt=no-new-privileges".into(),
    ];
    if let Some(seccomp_path) = seccomp_profile_path {
        args.push(format!("--security-opt=seccomp={seccomp_path}"));
    }
    args.extend([
        format!("--user={}:{}", policy.uid, policy.gid),
        format!("--cpus={}", policy.limits.cpus),
        format!("--memory={}m", policy.limits.memory_mb),
        format!("--memory-swap={}m", policy.limits.memory_mb),
        format!("--pids-limit={}", policy.limits.pids_limit),
        format!(
            "--tmpfs=/tmp:rw,noexec,nosuid,nodev,size={},nr_inodes={},mode=1777",
            policy.limits.tmpfs_bytes, policy.limits.tmpfs_inodes
        ),
        format!(
            "--tmpfs=/workspace:rw,nosuid,nodev,size={},nr_inodes={},mode=1777",
            policy.limits.workspace_bytes, policy.limits.workspace_inodes
        ),
    ]);

    for (host_canonical, container_dst) in validated_mounts {
        let src = format_host_mount_source(host_canonical, transport)?;
        args.push(format!(
            "--mount=type=bind,src={src},dst={container_dst},readonly"
        ));
    }

    args.push("--workdir=/workspace".into());
    for (k, v) in &request.env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    Ok(args)
}

pub fn format_host_mount_source(
    canonical_host_path: &Path,
    transport: &OciTransport,
) -> Result<String, SandboxError> {
    let raw = canonical_host_path
        .to_str()
        .ok_or_else(|| SandboxError::InvalidMount("host path is not valid UTF-8".into()))?;
    if raw.contains(',')
        || raw.contains('=')
        || raw.contains('\0')
        || raw.contains('\n')
        || raw.contains('\r')
    {
        return Err(SandboxError::InvalidMount(
            "host path contains forbidden mount delimiter characters".into(),
        ));
    }

    match transport {
        OciTransport::NativeLinux => {
            if !raw.starts_with('/') {
                return Err(SandboxError::InvalidMount(
                    "native Linux mount path must be absolute POSIX path".into(),
                ));
            }
            Ok(raw.to_owned())
        }
        OciTransport::Wsl2 { .. } => {
            let trimmed = raw.strip_prefix(r"\\?\").unwrap_or(raw);
            if trimmed.starts_with(r"\\") || trimmed.starts_with("UNC\\") {
                return Err(SandboxError::InvalidMount(
                    "network UNC host paths are prohibited".into(),
                ));
            }
            let bytes = trimmed.as_bytes();
            if bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && (bytes[2] == b'\\' || bytes[2] == b'/')
            {
                let drive = (bytes[0] as char).to_ascii_lowercase();
                let rest = trimmed[3..].replace('\\', "/");
                if rest.contains(':') {
                    return Err(SandboxError::InvalidMount(
                        "unexpected ':' in Windows mount path".into(),
                    ));
                }
                Ok(if rest.is_empty() {
                    format!("/mnt/{drive}")
                } else {
                    format!("/mnt/{drive}/{rest}")
                })
            } else if trimmed.starts_with('/') {
                Ok(trimmed.to_owned())
            } else {
                Err(SandboxError::InvalidMount(
                    "WSL2 mount source must be a canonical local drive path".into(),
                ))
            }
        }
    }
}

impl TrustedStagingRoots {
    pub fn validate_mounts(
        &self,
        job_id: &str,
        mounts: &[ReadOnlyMount],
    ) -> Result<Vec<(PathBuf, String)>, SandboxError> {
        let jobs_canon = self
            .jobs_root
            .as_deref()
            .map(canonicalize_trusted_dir_root)
            .transpose()?;
        let skills_canon = self
            .skills_root
            .as_deref()
            .map(canonicalize_trusted_dir_root)
            .transpose()?;

        let mut seen_targets = HashSet::with_capacity(mounts.len());
        let mut validated = Vec::with_capacity(mounts.len());

        for mount in mounts {
            if mount.host_path.as_os_str().is_empty() {
                return Err(SandboxError::InvalidMount(
                    "mount host_path must not be empty".into(),
                ));
            }
            let target = normalize_container_mount_target(&mount.container_path)?;
            for existing in &seen_targets {
                if paths_overlap(Path::new(existing), Path::new(&target)) {
                    return Err(SandboxError::InvalidMount(format!(
                        "overlapping container mount target '{target}'"
                    )));
                }
            }
            seen_targets.insert(target.clone());

            let meta = fs::symlink_metadata(&mount.host_path).map_err(|err| {
                SandboxError::InvalidMount(format!(
                    "mount host_path '{}' unavailable: {err}",
                    mount.host_path.display()
                ))
            })?;
            if is_reparse_or_symlink(&meta) {
                return Err(SandboxError::InvalidMount(format!(
                    "symlink or reparse mount host_path '{}' is prohibited",
                    mount.host_path.display()
                )));
            }
            if is_special_or_invalid_file(&meta) {
                return Err(SandboxError::InvalidMount(format!(
                    "special/non-regular file '{}' is prohibited",
                    mount.host_path.display()
                )));
            }

            let canonical = fs::canonicalize(&mount.host_path).map_err(|err| {
                SandboxError::InvalidMount(format!(
                    "failed to canonicalize mount '{}': {err}",
                    mount.host_path.display()
                ))
            })?;

            ensure_path_not_sensitive(&canonical)?;

            let is_skill_target = target == "/skill" || target.starts_with("/skill/");
            let authorized_base = verify_authorized_containment(
                &canonical,
                job_id,
                is_skill_target,
                jobs_canon.as_deref(),
                skills_canon.as_deref(),
            )?;

            verify_no_links_between(&authorized_base, &canonical)?;

            if meta.is_dir() {
                scan_directory_tree_readonly(&canonical, MAX_INSPECTED_DIR_ENTRIES)?;
            }

            validated.push((canonical, target));
        }

        Ok(validated)
    }
}

fn canonicalize_trusted_dir_root(root: &Path) -> Result<PathBuf, SandboxError> {
    if root.as_os_str().is_empty() {
        return Err(SandboxError::InvalidMount(
            "trusted root path must not be empty".into(),
        ));
    }
    let meta = fs::symlink_metadata(root).map_err(|err| {
        SandboxError::InvalidMount(format!(
            "trusted staging root '{}' unavailable: {err}",
            root.display()
        ))
    })?;
    if is_reparse_or_symlink(&meta) || !meta.is_dir() {
        return Err(SandboxError::InvalidMount(format!(
            "trusted staging root '{}' must be a real non-symlink directory",
            root.display()
        )));
    }
    let canon = fs::canonicalize(root).map_err(|err| {
        SandboxError::InvalidMount(format!(
            "failed to canonicalize trusted root '{}': {err}",
            root.display()
        ))
    })?;
    ensure_path_not_sensitive(&canon)?;
    Ok(canon)
}

fn verify_authorized_containment(
    canonical: &Path,
    job_id: &str,
    is_skill_target: bool,
    jobs_root: Option<&Path>,
    skills_root: Option<&Path>,
) -> Result<PathBuf, SandboxError> {
    if let Some(j_root) = jobs_root {
        let job_owned_dir = j_root.join(job_id);
        if canonical.starts_with(&job_owned_dir) && canonical != job_owned_dir {
            return Ok(j_root.to_path_buf());
        }
    }

    if is_skill_target {
        if let Some(s_root) = skills_root {
            if canonical.starts_with(s_root) && canonical != s_root {
                return Ok(s_root.to_path_buf());
            }
        }
    }

    Err(SandboxError::InvalidMount(format!(
        "unauthorized mount '{}': must reside inside configured job-owned staging root or authorized skill store",
        canonical.display()
    )))
}

fn verify_no_links_between(base: &Path, target: &Path) -> Result<(), SandboxError> {
    let rel = target.strip_prefix(base).map_err(|_| {
        SandboxError::InvalidMount("mount path escaped trusted staging root".into())
    })?;
    let mut cursor = base.to_path_buf();
    for component in rel.components() {
        cursor.push(component);
        let meta = fs::symlink_metadata(&cursor).map_err(|err| {
            SandboxError::InvalidMount(format!(
                "metadata check failed along '{}': {err}",
                cursor.display()
            ))
        })?;
        if is_reparse_or_symlink(&meta) {
            return Err(SandboxError::InvalidMount(format!(
                "symlink or reparse point '{}' along mount path is prohibited",
                cursor.display()
            )));
        }
        if is_special_or_invalid_file(&meta) {
            return Err(SandboxError::InvalidMount(format!(
                "special file '{}' along mount path is prohibited",
                cursor.display()
            )));
        }
    }
    Ok(())
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

fn scan_directory_tree_readonly(root: &Path, max_entries: usize) -> Result<(), SandboxError> {
    let mut stack = vec![root.to_path_buf()];
    let mut seen = 0usize;

    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).map_err(|err| {
            SandboxError::InvalidMount(format!(
                "failed to read directory '{}': {err}",
                dir.display()
            ))
        })?;
        for entry_res in entries {
            let entry = entry_res.map_err(|err| {
                SandboxError::InvalidMount(format!("directory entry read error: {err}"))
            })?;
            seen += 1;
            if seen > max_entries {
                return Err(SandboxError::InvalidMount(format!(
                    "read-only mount entry count exceeded safety bound ({max_entries})"
                )));
            }
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).map_err(|err| {
                SandboxError::InvalidMount(format!(
                    "metadata failed for '{}': {err}",
                    path.display()
                ))
            })?;
            if is_reparse_or_symlink(&meta) || is_special_or_invalid_file(&meta) {
                return Err(SandboxError::InvalidMount(format!(
                    "symlink, reparse point, or special file '{}' inside mount is prohibited",
                    path.display()
                )));
            }
            ensure_path_not_sensitive(&path)?;
            if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    Ok(())
}

const EMPIRICAL_PROBE_PYTHON: &str = r###"import errno, json, os, subprocess, sys
ws_limit = int(sys.argv[1])
check_chromium = sys.argv[2] == "1"

status_map = {}
with open("/proc/self/status", "r", encoding="utf-8") as f:
    for line in f:
        if ":" in line:
            k, v = line.split(":", 1)
            status_map[k.strip()] = v.strip()

ifaces = []
with open("/proc/net/dev", "r", encoding="utf-8") as f:
    for line in f.readlines()[2:]:
        if ":" in line:
            ifaces.append(line.split(":", 1)[0].strip())

ro_enforced = False
try:
    with open("/_scenivox_ro_probe", "wb") as f:
        f.write(b"x")
except OSError:
    ro_enforced = True

def read_txt(p):
    with open(p, "r", encoding="utf-8") as f:
        return f.read().strip()

cpu_max = read_txt("/sys/fs/cgroup/cpu.max")
mem_max = int(read_txt("/sys/fs/cgroup/memory.max"))
swap_raw = read_txt("/sys/fs/cgroup/memory.swap.max")
swap_max = 0 if swap_raw == "0" else int(swap_raw)
pids_max = int(read_txt("/sys/fs/cgroup/pids.max"))
tmp_inodes = os.statvfs("/tmp").f_files
ws_inodes = os.statvfs("/workspace").f_files

created = []
exhausted = False
exhausted_at = 0
for i in range(1, min(ws_limit, 16384) + 16):
    p = f"/workspace/_inode_probe_{i}"
    try:
        fd = os.open(p, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        os.close(fd)
        created.append(p)
    except OSError as e:
        if e.errno in (errno.ENOSPC, errno.EDQUOT):
            exhausted = True
            exhausted_at = len(created)
        break
for p in created:
    try:
        os.unlink(p)
    except OSError:
        pass

chr_no_sandbox_absent = False
chr_zygote = False
chr_renderer_ns = 0
chr_renderer_seccomp = 0
chr_shot_bytes = 0
if check_chromium:
    env = os.environ.copy()
    env["HOME"] = "/tmp"
    node_js = r'''
const fs = require("fs");
let chromium;
try { ({ chromium } = require("playwright")); }
catch { ({ chromium } = require("/opt/scenivox-runtime/node_modules/playwright")); }
(async () => {
  const b = await chromium.launch({ headless: true, chromiumSandbox: true, args: ["--disable-dev-shm-usage"] });
  const p = await b.newPage({ viewport: { width: 320, height: 240 } });
  await p.setContent("<html><body style='background:#0f172a'><canvas id='c' width='120' height='120'></canvas><script>const x=document.getElementById('c').getContext('2d');x.fillStyle='#38bdf8';x.fillRect(10,10,100,100);</script></body></html>");
  let noSandboxAbsent = true, zygote = false, rDepth = 0, rSeccomp = 0;
  for (const pid of fs.readdirSync("/proc")) {
    if (!/^\d+$/.test(pid)) continue;
    try {
      const cmd = fs.readFileSync("/proc/" + pid + "/cmdline", "utf8").replace(/\0/g, " ");
      const exe = cmd.split(" ")[0] || "";
      if (!(exe.includes("headless_shell") || exe.includes("chrome"))) continue;
      if (cmd.includes("--no-sandbox")) noSandboxAbsent = false;
      if (cmd.includes("--type=zygote")) zygote = true;
      if (cmd.includes("--type=renderer")) {
        const st = fs.readFileSync("/proc/" + pid + "/status", "utf8");
        for (const l of st.split("\n")) {
          if (l.startsWith("NSpid:")) {
            const parts = l.split(":")[1].trim().split(/\s+/);
            if (parts.length > rDepth) rDepth = parts.length;
          }
          if (l.startsWith("Seccomp:")) rSeccomp = parseInt(l.split(":")[1].trim(), 10);
        }
      }
    } catch {}
  }
  const shot = await p.screenshot();
  await b.close();
  console.log(JSON.stringify({noSandboxAbsent, zygote, rDepth, rSeccomp, bytes: shot.length}));
})();
'''
    res = subprocess.run(["node", "-e", node_js], env=env, capture_output=True, text=True, check=True)
    info = json.loads(res.stdout.strip().splitlines()[-1])
    chr_no_sandbox_absent = bool(info["noSandboxAbsent"])
    chr_zygote = bool(info["zygote"])
    chr_renderer_ns = int(info["rDepth"])
    chr_renderer_seccomp = int(info["rSeccomp"])
    chr_shot_bytes = int(info["bytes"])

print(json.dumps({
    "uid": os.getuid(),
    "gid": os.getgid(),
    "cap_eff_hex": status_map.get("CapEff", ""),
    "no_new_privs": int(status_map.get("NoNewPrivs", "0")),
    "seccomp_mode": int(status_map.get("Seccomp", "0")),
    "network_interfaces": ifaces,
    "read_only_rootfs_enforced": ro_enforced,
    "cpu_max": cpu_max,
    "memory_max_bytes": mem_max,
    "memory_swap_max_bytes": swap_max,
    "pids_max": pids_max,
    "tmp_inodes_reported": tmp_inodes,
    "workspace_inodes_reported": ws_inodes,
    "inode_exhaustion_at": exhausted_at,
    "inode_exhaustion_enforced": exhausted,
    "chromium_no_sandbox_flag_absent": chr_no_sandbox_absent,
    "chromium_zygote_active": chr_zygote,
    "chromium_renderer_pid_ns_depth": chr_renderer_ns,
    "chromium_renderer_seccomp_mode": chr_renderer_seccomp,
    "chromium_screenshot_bytes": chr_shot_bytes,
}))
"###;

fn validate_seccomp_profile_bytes(bytes: &[u8]) -> Result<(), String> {
    let val: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|err| format!("seccomp profile is not valid JSON: {err}"))?;
    let default_action = val
        .get("defaultAction")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if default_action != "SCMP_ACT_ERRNO" {
        return Err(format!(
            "seccomp profile defaultAction must be SCMP_ACT_ERRNO (got '{default_action}')"
        ));
    }
    let Some(rules) = val.get("syscalls").and_then(|v| v.as_array()) else {
        return Err("seccomp profile missing syscalls array".into());
    };
    const FORBIDDEN_ALLOWED: &[&str] = &[
        "mount",
        "umount",
        "umount2",
        "pivot_root",
        "mount_setattr",
        "ptrace",
        "setns",
        "keyctl",
        "add_key",
        "request_key",
        "open_by_handle_at",
        "name_to_handle_at",
        "reboot",
        "kexec_load",
        "kexec_file_load",
        "init_module",
        "finit_module",
        "delete_module",
        "bpf",
        "perf_event_open",
        "userfaultfd",
        "io_uring_setup",
    ];
    for rule in rules {
        if rule.get("includes").and_then(|i| i.get("caps")).is_some() {
            return Err("seccomp profile must not contain capability-gated allow rules".into());
        }
        let action = rule.get("action").and_then(|v| v.as_str()).unwrap_or("");
        if action != "SCMP_ACT_ALLOW" {
            continue;
        }
        if let Some(names) = rule.get("names").and_then(|v| v.as_array()) {
            for name in names.iter().filter_map(|v| v.as_str()) {
                if FORBIDDEN_ALLOWED.contains(&name) {
                    return Err(format!(
                        "seccomp profile permits forbidden syscall '{name}'"
                    ));
                }
            }
        }
    }
    Ok(())
}

fn is_sha256_hex(raw: &str) -> bool {
    raw.len() == 64
        && raw
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_sha256_digest_prefixed(raw: &str) -> bool {
    raw.strip_prefix("sha256:").is_some_and(is_sha256_hex)
}

fn wrap_engine_command(
    transport: &OciTransport,
    engine: OciEngine,
    docker_context: Option<&str>,
    args: Vec<String>,
    env: BTreeMap<String, String>,
) -> CommandSpec {
    let final_args = match (engine, docker_context) {
        (OciEngine::Docker, Some(ctx)) => {
            let mut full = Vec::with_capacity(args.len() + 2);
            full.push("--context".into());
            full.push(ctx.to_owned());
            full.extend(args);
            full
        }
        _ => args,
    };
    wrap_command(transport, engine.binary(), final_args, env)
}

fn wrap_command(
    transport: &OciTransport,
    program: &str,
    args: Vec<String>,
    mut env: BTreeMap<String, String>,
) -> CommandSpec {
    match transport {
        OciTransport::NativeLinux => CommandSpec {
            program: program.to_owned(),
            args,
            env,
        },
        OciTransport::Wsl2 { distro } => {
            env.insert("WSL_UTF8".into(), "1".into());
            let mut full_args = Vec::with_capacity(args.len() + 4);
            full_args.push("-d".into());
            full_args.push(distro.clone());
            full_args.push("--".into());
            full_args.push(program.to_owned());
            full_args.extend(args);
            CommandSpec {
                program: "wsl.exe".into(),
                args: full_args,
                env,
            }
        }
    }
}

fn validate_digest_pinned_image_ref(reference: &str) -> Result<&str, SandboxError> {
    if reference.is_empty()
        || reference.len() > 256
        || reference.starts_with('-')
        || reference.contains("://")
        || reference.bytes().any(|b| {
            !b.is_ascii_graphic()
                || matches!(b, b'\\' | b'\'' | b'"' | b'$' | b'`' | b';' | b'|' | b'&')
        })
    {
        return Err(SandboxError::InvalidPolicy(
            "image reference contains invalid or unsafe characters".into(),
        ));
    }
    let Some((repo, digest)) = reference.split_once("@sha256:") else {
        return Err(SandboxError::InvalidPolicy(
            "image reference must be pinned with @sha256:<64-hex>".into(),
        ));
    };
    if repo.is_empty() || repo.contains('@') {
        return Err(SandboxError::InvalidPolicy(
            "image repository part before @sha256: is invalid".into(),
        ));
    }
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(SandboxError::InvalidPolicy(
            "image @sha256 digest must be 64 lowercase hex chars".into(),
        ));
    }
    let at_index = reference.len() - 64 - "sha256:".len();
    Ok(&reference[at_index..])
}

fn decode_wsl_output(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes.len() % 2 == 0 && bytes.iter().skip(1).step_by(2).any(|&b| b == 0)
    {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).replace('\0', "")
    }
}

fn select_wsl2_distro(
    wsl_list_output: &str,
    preferred: Option<&str>,
) -> Result<String, SandboxError> {
    #[derive(Debug)]
    struct WslDistro {
        is_default: bool,
        name: String,
        version: u32,
    }

    let mut distros = Vec::new();
    for line in wsl_list_output.lines() {
        let cleaned = line.trim().trim_start_matches('\u{feff}');
        if cleaned.is_empty() || cleaned.starts_with("NAME") {
            continue;
        }
        let (is_default, rest) = if let Some(after) = cleaned.strip_prefix('*') {
            (true, after.trim())
        } else {
            (false, cleaned)
        };
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let Ok(version) = parts[parts.len() - 1].parse::<u32>() else {
            continue;
        };
        let name = parts[..parts.len() - 2].join(" ");
        distros.push(WslDistro {
            is_default,
            name,
            version,
        });
    }

    let is_utility_distro = |name: &str| {
        name.eq_ignore_ascii_case("docker-desktop")
            || name.eq_ignore_ascii_case("docker-desktop-data")
    };

    if let Some(target) = preferred {
        let matched = distros.iter().find(|d| d.name == target).ok_or_else(|| {
            SandboxError::SandboxUnavailable(format!("configured WSL2 distro '{target}' not found"))
        })?;
        if matched.version != 2 {
            return Err(SandboxError::SandboxUnavailable(format!(
                "WSL distro '{}' uses WSL{} instead of WSL2",
                matched.name, matched.version
            )));
        }
        return Ok(matched.name.clone());
    }

    if let Some(default_v2) = distros
        .iter()
        .find(|d| d.is_default && d.version == 2 && !is_utility_distro(&d.name))
    {
        return Ok(default_v2.name.clone());
    }

    if let Some(first_v2) = distros
        .iter()
        .find(|d| d.version == 2 && !is_utility_distro(&d.name))
    {
        return Ok(first_v2.name.clone());
    }

    Err(SandboxError::SandboxUnavailable(
        "no WSL2 Linux distribution available".into(),
    ))
}

fn validate_identifier(label: &str, value: &str) -> Result<(), SandboxError> {
    if Uuid::parse_str(value).is_ok_and(|u| u.to_string() == value) {
        return Ok(());
    }
    let valid = !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_alphanumeric() || (i > 0 && matches!(b, b'-' | b'_')));
    if !valid {
        return Err(SandboxError::InvalidInvocation(format!(
            "invalid {label}: must be UUID or safe [a-zA-Z0-9_-] token"
        )));
    }
    Ok(())
}

fn sanitize_container_fragment(value: &str, max_len: usize) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(max_len)
        .collect()
}

fn is_safe_container_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 96
        && name
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_alphanumeric() || (i > 0 && matches!(b, b'-' | b'_' | b'.')))
}

fn validate_args(args: &[String], check_flags: bool) -> Result<(), SandboxError> {
    if args.len() > MAX_EXEC_ARGS {
        return Err(SandboxError::InvalidInvocation(
            "too many sandbox execution arguments".into(),
        ));
    }
    for (idx, arg) in args.iter().enumerate() {
        if arg.len() > MAX_EXEC_ARG_LEN
            || arg
                .chars()
                .any(|c| c == '\0' || c == '\r' || c == '\n' || c.is_control())
        {
            return Err(SandboxError::InvalidInvocation(format!(
                "argument {idx} contains disallowed length or control characters"
            )));
        }
        let lower = arg.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "sh" | "/bin/sh"
                | "bash"
                | "/bin/bash"
                | "dash"
                | "ash"
                | "zsh"
                | "cmd"
                | "cmd.exe"
                | "powershell"
                | "pwsh"
        ) {
            return Err(SandboxError::InvalidInvocation(
                "shell executables are forbidden in sandbox arguments".into(),
            ));
        }
        if check_flags
            && (lower == "-e"
                || lower == "--eval"
                || lower == "-c"
                || lower == "--privileged"
                || lower.starts_with("--cap-add")
                || lower.starts_with("--network"))
        {
            return Err(SandboxError::InvalidInvocation(format!(
                "forbidden option '{arg}' in media tool invocation"
            )));
        }
    }
    Ok(())
}

fn normalize_relative_posix_path(raw: &str) -> Result<String, SandboxError> {
    if raw.is_empty()
        || raw.len() > 256
        || raw.starts_with('/')
        || raw.starts_with('-')
        || raw.contains('\\')
        || raw.contains(':')
        || raw.chars().any(|c| c == '\0' || c.is_control())
    {
        return Err(SandboxError::InvalidInvocation(
            "script path must be a clean relative POSIX path".into(),
        ));
    }
    let mut parts = Vec::new();
    for segment in raw.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(SandboxError::InvalidInvocation(
                "script path must not contain empty, '.', or '..' segments".into(),
            ));
        }
        parts.push(segment);
    }
    Ok(parts.join("/"))
}

fn normalize_container_mount_target(raw: &str) -> Result<String, SandboxError> {
    if !raw.starts_with('/')
        || raw.contains('\\')
        || raw.contains(':')
        || raw.contains(',')
        || raw.contains('=')
        || raw
            .chars()
            .any(|c| c == '\0' || c.is_control() || c.is_whitespace())
    {
        return Err(SandboxError::InvalidMount(
            "container mount path contains unsafe or forbidden characters".into(),
        ));
    }
    let mut segments = Vec::new();
    for seg in raw.split('/').skip(1) {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(SandboxError::InvalidMount(
                "container mount path must be normalized without '.', '..', or '//'".into(),
            ));
        }
        segments.push(seg);
    }
    let normalized = format!("/{}", segments.join("/"));
    let allowed = normalized == "/skill"
        || normalized.starts_with("/skill/")
        || normalized.starts_with("/inputs/");
    if !allowed {
        return Err(SandboxError::InvalidMount(
            "read-only container mount targets must be '/skill' or under '/skill/' or '/inputs/'"
                .into(),
        ));
    }
    Ok(normalized)
}

fn validate_env(env: &BTreeMap<String, String>) -> Result<(), SandboxError> {
    if env.len() > MAX_ENV_VARS {
        return Err(SandboxError::InvalidInvocation(
            "too many environment variables supplied".into(),
        ));
    }
    for (k, v) in env {
        let valid_key = !k.is_empty()
            && k.len() <= 64
            && k.bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
        if !valid_key {
            return Err(SandboxError::InvalidInvocation(format!(
                "invalid environment variable key '{k}'"
            )));
        }
        if is_forbidden_env_key(k) {
            return Err(SandboxError::InvalidInvocation(format!(
                "forbidden secret or loader environment variable '{k}'"
            )));
        }
        if v.len() > MAX_ENV_VALUE_LEN || v.chars().any(|c| c == '\0' || c == '\r' || c == '\n') {
            return Err(SandboxError::InvalidInvocation(format!(
                "invalid value for environment variable '{k}'"
            )));
        }
    }
    Ok(())
}

fn is_forbidden_env_key(key: &str) -> bool {
    matches!(
        key,
        "PATH"
            | "HOME"
            | "LD_PRELOAD"
            | "LD_LIBRARY_PATH"
            | "NODE_OPTIONS"
            | "PYTHONSTARTUP"
            | "PYTHONINSPECT"
            | "PYTHONPATH"
            | "DOCKER_HOST"
            | "DOCKER_CONTEXT"
            | "CONTAINER_HOST"
            | "CONTAINER_CONNECTION"
    ) || key.contains("KEY")
        || key.contains("TOKEN")
        || key.contains("SECRET")
        || key.contains("PASSWORD")
        || key.contains("CREDENTIAL")
        || key.contains("AUTH")
}

fn ensure_path_not_sensitive(canonical: &Path) -> Result<(), SandboxError> {
    if canonical.parent().is_none() {
        return Err(SandboxError::InvalidMount(
            "filesystem root mount is prohibited".into(),
        ));
    }

    let normalized_slash = canonical
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('\\', "/")
        .to_ascii_lowercase();

    for forbidden_root in [
        "/etc",
        "/proc",
        "/sys",
        "/dev",
        "/run",
        "/var/run",
        "/boot",
        "/root",
        "c:/windows",
        "c:/program files",
    ] {
        if normalized_slash == forbidden_root
            || normalized_slash.starts_with(&format!("{forbidden_root}/"))
        {
            return Err(SandboxError::InvalidMount(format!(
                "system path '{}' is prohibited",
                canonical.display()
            )));
        }
    }

    for env_home in ["HOME", "USERPROFILE"] {
        if let Ok(home_raw) = std::env::var(env_home) {
            if let Ok(home_canon) = fs::canonicalize(home_raw) {
                if canonical == home_canon {
                    return Err(SandboxError::InvalidMount(
                        "mounting home directory directly is prohibited".into(),
                    ));
                }
            }
        }
    }

    for component in canonical.components() {
        if let Component::Normal(part) = component {
            let name = part.to_string_lossy().to_ascii_lowercase();
            if name == ".env"
                || name.starts_with(".env.")
                || name == "scenivox.db"
                || name.starts_with("scenivox.db-")
                || matches!(
                    name.as_str(),
                    ".git"
                        | ".ssh"
                        | ".aws"
                        | ".gnupg"
                        | ".kube"
                        | ".docker"
                        | "docker.sock"
                        | "podman.sock"
                )
                || name.ends_with(".sock")
            {
                return Err(SandboxError::InvalidMount(format!(
                    "sensitive path component '{name}' is prohibited"
                )));
            }
        }
    }

    Ok(())
}

fn paths_overlap(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}

fn truncate_excerpt(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_owned()
    } else {
        let prefix: String = trimmed.chars().take(max_chars).collect();
        format!("{prefix}...")
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Debug)]
struct ValidatedDestination {
    canonical_jobs_root: PathBuf,
    target_dir: PathBuf,
    pre_existing: bool,
}

#[derive(Clone, Debug)]
struct ValidatedArtifactTransfer {
    destination: ValidatedDestination,
    artifacts: Vec<ExpectedArtifact>,
    max_total_bytes: u64,
}

struct DecodedArtifact {
    relative_path: String,
    sha256: String,
    bytes: Vec<u8>,
}

struct DecodedArtifactStream {
    child_stdout: Vec<u8>,
    artifacts: Vec<DecodedArtifact>,
}

fn validate_artifact_transfer(
    trusted_roots: &TrustedStagingRoots,
    job_id: &str,
    policy: &SandboxPolicy,
    validated_mounts: &[(PathBuf, String)],
    transfer: &ArtifactTransferRequest,
) -> Result<ValidatedArtifactTransfer, SandboxError> {
    if transfer.artifacts.is_empty() {
        return Err(SandboxError::InvalidInvocation(
            "artifact_transfer.artifacts must not be empty".into(),
        ));
    }
    if transfer.artifacts.len() > MAX_EXPECTED_ARTIFACTS {
        return Err(SandboxError::InvalidInvocation(format!(
            "artifact_transfer.artifacts exceeds max count ({MAX_EXPECTED_ARTIFACTS})"
        )));
    }
    if transfer.max_total_bytes == 0
        || transfer.max_total_bytes > MAX_TOTAL_ARTIFACT_BYTES
        || transfer.max_total_bytes > policy.limits.workspace_bytes
    {
        return Err(SandboxError::InvalidInvocation(
            "artifact_transfer.max_total_bytes must be within 1..=min(MAX_TOTAL_ARTIFACT_BYTES, workspace_bytes)".into(),
        ));
    }

    let mut normalized = Vec::with_capacity(transfer.artifacts.len());
    for item in &transfer.artifacts {
        let rel = normalize_relative_posix_path(&item.relative_path)?;
        if item.max_bytes == 0
            || item.max_bytes > MAX_ARTIFACT_FILE_BYTES
            || item.max_bytes > transfer.max_total_bytes
        {
            return Err(SandboxError::InvalidInvocation(format!(
                "artifact '{rel}' max_bytes must be within 1..=min(MAX_ARTIFACT_FILE_BYTES, max_total_bytes)"
            )));
        }
        for existing in &normalized {
            let existing: &ExpectedArtifact = existing;
            if paths_overlap(Path::new(&existing.relative_path), Path::new(&rel)) {
                return Err(SandboxError::InvalidInvocation(format!(
                    "duplicate or overlapping expected artifact path '{rel}'"
                )));
            }
        }
        normalized.push(ExpectedArtifact {
            relative_path: rel,
            max_bytes: item.max_bytes,
        });
    }

    let jobs_root_raw = trusted_roots.jobs_root.as_deref().ok_or_else(|| {
        SandboxError::InvalidMount(
            "artifact transfer requires configured trusted_roots.jobs_root".into(),
        )
    })?;
    let jobs_canon = canonicalize_trusted_dir_root(jobs_root_raw)?;
    let job_owned_dir = jobs_canon.join(job_id);
    let job_owned_canon = canonicalize_trusted_dir_root(&job_owned_dir)?;
    verify_no_links_between(&jobs_canon, &job_owned_canon)?;

    if transfer.destination_dir.as_os_str().is_empty()
        || transfer
            .destination_dir
            .components()
            .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        return Err(SandboxError::InvalidMount(
            "artifact destination_dir must be a clean non-empty path without '.' or '..'".into(),
        ));
    }

    let destination = match fs::symlink_metadata(&transfer.destination_dir) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) || !meta.is_dir() {
                return Err(SandboxError::InvalidMount(format!(
                    "artifact destination '{}' must be a real non-symlink directory",
                    transfer.destination_dir.display()
                )));
            }
            let dest_canon = fs::canonicalize(&transfer.destination_dir).map_err(|err| {
                SandboxError::InvalidMount(format!(
                    "failed to canonicalize artifact destination '{}': {err}",
                    transfer.destination_dir.display()
                ))
            })?;
            if !dest_canon.starts_with(&job_owned_canon) || dest_canon == job_owned_canon {
                return Err(SandboxError::InvalidMount(format!(
                    "artifact destination '{}' must reside in a subdirectory of job directory '{}'",
                    dest_canon.display(),
                    job_owned_canon.display()
                )));
            }
            verify_no_links_between(&jobs_canon, &dest_canon)?;
            ensure_path_not_sensitive(&dest_canon)?;
            ensure_dir_empty(&dest_canon)?;
            ValidatedDestination {
                canonical_jobs_root: jobs_canon,
                target_dir: dest_canon,
                pre_existing: true,
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let parent = transfer.destination_dir.parent().ok_or_else(|| {
                SandboxError::InvalidMount(
                    "artifact destination must have a parent directory".into(),
                )
            })?;
            let leaf = transfer.destination_dir.file_name().ok_or_else(|| {
                SandboxError::InvalidMount("artifact destination must name a subdirectory".into())
            })?;
            let parent_meta = fs::symlink_metadata(parent).map_err(|parent_err| {
                SandboxError::InvalidMount(format!(
                    "artifact destination parent '{}' unavailable: {parent_err}",
                    parent.display()
                ))
            })?;
            if is_reparse_or_symlink(&parent_meta) || !parent_meta.is_dir() {
                return Err(SandboxError::InvalidMount(format!(
                    "artifact destination parent '{}' must be a real non-symlink directory",
                    parent.display()
                )));
            }
            let parent_canon = fs::canonicalize(parent).map_err(|canon_err| {
                SandboxError::InvalidMount(format!(
                    "failed to canonicalize artifact destination parent '{}': {canon_err}",
                    parent.display()
                ))
            })?;
            if !parent_canon.starts_with(&job_owned_canon) {
                return Err(SandboxError::InvalidMount(format!(
                    "artifact destination '{}' must reside under job directory '{}'",
                    transfer.destination_dir.display(),
                    job_owned_canon.display()
                )));
            }
            verify_no_links_between(&jobs_canon, &parent_canon)?;
            let target_dir = parent_canon.join(leaf);
            ensure_path_not_sensitive(&target_dir)?;
            ValidatedDestination {
                canonical_jobs_root: jobs_canon,
                target_dir,
                pre_existing: false,
            }
        }
        Err(err) => {
            return Err(SandboxError::InvalidMount(format!(
                "failed to inspect artifact destination '{}': {err}",
                transfer.destination_dir.display()
            )));
        }
    };

    for (mount_host, _) in validated_mounts {
        if paths_overlap(mount_host, &destination.target_dir) {
            return Err(SandboxError::InvalidMount(format!(
                "artifact destination '{}' overlaps read-only mount '{}'",
                destination.target_dir.display(),
                mount_host.display()
            )));
        }
    }

    Ok(ValidatedArtifactTransfer {
        destination,
        artifacts: normalized,
        max_total_bytes: transfer.max_total_bytes,
    })
}

fn ensure_dir_empty(dir: &Path) -> Result<(), SandboxError> {
    let mut entries = fs::read_dir(dir).map_err(|err| {
        SandboxError::InvalidMount(format!(
            "failed to read destination directory '{}': {err}",
            dir.display()
        ))
    })?;
    if entries
        .next()
        .transpose()
        .map_err(|err| {
            SandboxError::InvalidMount(format!(
                "failed to scan destination directory '{}': {err}",
                dir.display()
            ))
        })?
        .is_some()
    {
        return Err(SandboxError::InvalidMount(format!(
            "artifact destination '{}' must be empty",
            dir.display()
        )));
    }
    Ok(())
}

fn decode_framed_artifact_stream(
    raw: &[u8],
    max_child_stdout: usize,
    transfer: &ValidatedArtifactTransfer,
) -> Result<DecodedArtifactStream, SandboxError> {
    if !raw.starts_with(ARTIFACT_PROTOCOL_MAGIC) {
        return Err(SandboxError::ArtifactTransferFailed(
            "missing framed artifact transfer header".into(),
        ));
    }

    let expected_map: BTreeMap<&str, u64> = transfer
        .artifacts
        .iter()
        .map(|item| (item.relative_path.as_str(), item.max_bytes))
        .collect();

    let mut pos = ARTIFACT_PROTOCOL_MAGIC.len();
    let mut child_stdout = Vec::new();
    let mut artifacts = Vec::with_capacity(transfer.artifacts.len());
    let mut seen_paths = HashSet::with_capacity(transfer.artifacts.len());
    let mut total_artifact_bytes = 0u64;
    let mut done_seen = false;

    while pos < raw.len() {
        if done_seen {
            return Err(SandboxError::ArtifactTransferFailed(
                "unexpected trailing bytes after artifact end frame".into(),
            ));
        }
        let tag = raw[pos];
        pos += 1;

        match tag {
            b'O' => {
                let len = read_be_u32(raw, &mut pos)? as usize;
                let chunk = read_slice(raw, &mut pos, len)?;
                if child_stdout.len().saturating_add(chunk.len()) > max_child_stdout {
                    return Err(SandboxError::OutputLimitExceeded {
                        stream: "stdout",
                        limit_bytes: max_child_stdout,
                    });
                }
                child_stdout.extend_from_slice(chunk);
            }
            b'A' => {
                let path_len = read_be_u16(raw, &mut pos)? as usize;
                let path_bytes = read_slice(raw, &mut pos, path_len)?;
                let rel_raw = std::str::from_utf8(path_bytes).map_err(|_| {
                    SandboxError::ArtifactTransferFailed(
                        "artifact frame path is not valid UTF-8".into(),
                    )
                })?;
                let rel = normalize_relative_posix_path(rel_raw)?;
                let max_for_file = *expected_map.get(rel.as_str()).ok_or_else(|| {
                    SandboxError::ArtifactTransferFailed(format!(
                        "unexpected unrequested artifact '{rel}'"
                    ))
                })?;
                if !seen_paths.insert(rel.clone()) {
                    return Err(SandboxError::ArtifactTransferFailed(format!(
                        "duplicate artifact '{rel}' in transfer stream"
                    )));
                }
                let claimed_hash = read_slice(raw, &mut pos, 32)?;
                let data_len_u64 = read_be_u64(raw, &mut pos)?;
                if data_len_u64 > max_for_file {
                    return Err(SandboxError::ArtifactTransferFailed(format!(
                        "artifact '{rel}' ({data_len_u64} bytes) exceeds per-file limit ({max_for_file} bytes)"
                    )));
                }
                total_artifact_bytes =
                    total_artifact_bytes
                        .checked_add(data_len_u64)
                        .ok_or_else(|| {
                            SandboxError::ArtifactTransferFailed(
                                "total transferred artifact bytes overflowed".into(),
                            )
                        })?;
                if total_artifact_bytes > transfer.max_total_bytes {
                    return Err(SandboxError::ArtifactTransferFailed(format!(
                        "total transferred artifact bytes exceeded limit ({})",
                        transfer.max_total_bytes
                    )));
                }
                let data_len = usize::try_from(data_len_u64).map_err(|_| {
                    SandboxError::ArtifactTransferFailed(
                        "artifact byte length overflows host usize".into(),
                    )
                })?;
                let payload = read_slice(raw, &mut pos, data_len)?;
                let actual_hash = Sha256::digest(payload);
                if actual_hash.as_slice() != claimed_hash {
                    return Err(SandboxError::ArtifactTransferFailed(format!(
                        "SHA-256 mismatch during artifact transfer for '{rel}'"
                    )));
                }
                let sha_hex: String = actual_hash.iter().map(|b| format!("{b:02x}")).collect();
                artifacts.push(DecodedArtifact {
                    relative_path: rel,
                    sha256: sha_hex,
                    bytes: payload.to_vec(),
                });
            }
            b'D' => {
                let marker_len = read_be_u32(raw, &mut pos)?;
                if marker_len != 0 {
                    return Err(SandboxError::ArtifactTransferFailed(
                        "malformed artifact end frame length".into(),
                    ));
                }
                done_seen = true;
            }
            other => {
                return Err(SandboxError::ArtifactTransferFailed(format!(
                    "unknown artifact protocol frame tag 0x{other:02x}"
                )));
            }
        }
    }

    if !done_seen {
        return Err(SandboxError::ArtifactTransferFailed(
            "truncated artifact transfer stream missing end frame".into(),
        ));
    }
    if artifacts.len() != expected_map.len() {
        return Err(SandboxError::ArtifactTransferFailed(format!(
            "missing expected artifacts: received {} of {}",
            artifacts.len(),
            expected_map.len()
        )));
    }

    Ok(DecodedArtifactStream {
        child_stdout,
        artifacts,
    })
}

fn read_be_u16(raw: &[u8], pos: &mut usize) -> Result<u16, SandboxError> {
    let bytes = read_slice(raw, pos, 2)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_be_u32(raw: &[u8], pos: &mut usize) -> Result<u32, SandboxError> {
    let bytes = read_slice(raw, pos, 4)?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_be_u64(raw: &[u8], pos: &mut usize) -> Result<u64, SandboxError> {
    let bytes = read_slice(raw, pos, 8)?;
    Ok(u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn read_slice<'a>(raw: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8], SandboxError> {
    let end = pos.checked_add(len).ok_or_else(|| {
        SandboxError::ArtifactTransferFailed("artifact frame length overflow".into())
    })?;
    if end > raw.len() {
        return Err(SandboxError::ArtifactTransferFailed(
            "truncated artifact frame payload".into(),
        ));
    }
    let slice = &raw[*pos..end];
    *pos = end;
    Ok(slice)
}

fn write_transferred_artifacts_safely(
    dest: &ValidatedDestination,
    artifacts: &[DecodedArtifact],
) -> Result<Vec<TransferredArtifact>, SandboxError> {
    if !dest.pre_existing {
        fs::create_dir(&dest.target_dir).map_err(|err| {
            SandboxError::ArtifactTransferFailed(format!(
                "failed to create artifact destination '{}': {err}",
                dest.target_dir.display()
            ))
        })?;
    }
    let meta = fs::symlink_metadata(&dest.target_dir).map_err(|err| {
        SandboxError::ArtifactTransferFailed(format!(
            "artifact destination '{}' unavailable: {err}",
            dest.target_dir.display()
        ))
    })?;
    if is_reparse_or_symlink(&meta) || !meta.is_dir() {
        return Err(SandboxError::ArtifactTransferFailed(format!(
            "artifact destination '{}' is not a real non-symlink directory",
            dest.target_dir.display()
        )));
    }
    verify_no_links_between(&dest.canonical_jobs_root, &dest.target_dir)?;
    ensure_dir_empty(&dest.target_dir)?;

    let mut written = Vec::with_capacity(artifacts.len());
    for artifact in artifacts {
        let mut cursor = dest.target_dir.clone();
        let segments: Vec<&str> = artifact.relative_path.split('/').collect();
        for seg in &segments[..segments.len() - 1] {
            cursor.push(seg);
            match fs::symlink_metadata(&cursor) {
                Ok(seg_meta) => {
                    if is_reparse_or_symlink(&seg_meta) || !seg_meta.is_dir() {
                        return Err(SandboxError::ArtifactTransferFailed(format!(
                            "unsafe destination parent '{}' during artifact transfer",
                            cursor.display()
                        )));
                    }
                }
                Err(err) if err.kind() == ErrorKind::NotFound => {
                    fs::create_dir(&cursor).map_err(|mkdir_err| {
                        SandboxError::ArtifactTransferFailed(format!(
                            "failed to create artifact subdir '{}': {mkdir_err}",
                            cursor.display()
                        ))
                    })?;
                    let created_meta = fs::symlink_metadata(&cursor).map_err(|meta_err| {
                        SandboxError::ArtifactTransferFailed(format!(
                            "failed to verify created subdir '{}': {meta_err}",
                            cursor.display()
                        ))
                    })?;
                    if is_reparse_or_symlink(&created_meta) || !created_meta.is_dir() {
                        return Err(SandboxError::ArtifactTransferFailed(format!(
                            "unsafe destination subdir '{}' created during transfer",
                            cursor.display()
                        )));
                    }
                }
                Err(err) => {
                    return Err(SandboxError::ArtifactTransferFailed(format!(
                        "failed to inspect destination subdir '{}': {err}",
                        cursor.display()
                    )));
                }
            }
            ensure_path_not_sensitive(&cursor)?;
        }

        let leaf_path = cursor.join(segments[segments.len() - 1]);
        ensure_path_not_sensitive(&leaf_path)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&leaf_path)
            .map_err(|err| {
                SandboxError::ArtifactTransferFailed(format!(
                    "failed to write transferred artifact '{}': {err}",
                    leaf_path.display()
                ))
            })?;
        file.write_all(&artifact.bytes).map_err(|err| {
            SandboxError::ArtifactTransferFailed(format!(
                "failed to write artifact bytes '{}': {err}",
                leaf_path.display()
            ))
        })?;
        file.flush().map_err(|err| {
            SandboxError::ArtifactTransferFailed(format!(
                "failed to flush transferred artifact '{}': {err}",
                leaf_path.display()
            ))
        })?;
        written.push(TransferredArtifact {
            relative_path: artifact.relative_path.clone(),
            host_path: leaf_path,
            size_bytes: artifact.bytes.len() as u64,
            sha256: artifact.sha256.clone(),
        });
    }

    Ok(written)
}
