//! Fail-closed rootless Linux OCI sandbox execution foundation (ADR-02 / P2-01A).
//!
//! This module provides typed sandbox policies, canonical immutable policy hashing,
//! verified rootless Linux OCI runtime detection (native Linux and Windows via WSL2),
//! digest-pinned local image verification (`--pull=never`), restricted mount/invocation
//! validation, bounded supervision with container/process-tree cleanup, and explicit
//! `sandbox_unavailable` failures without host fallback.

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
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
pub const MAX_TMPFS_DISK_MB: u64 = 4_096;
pub const MAX_WORKSPACE_DISK_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const MAX_TIMEOUT_SECS: u64 = 3_600;
pub const MAX_STREAM_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_OUTPUT_FILES: usize = 2_048;
pub const MAX_TOTAL_OUTPUT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_EXEC_ARGS: usize = 64;
pub const MAX_EXEC_ARG_LEN: usize = 1_024;
pub const MAX_ENV_VARS: usize = 16;
pub const MAX_ENV_VALUE_LEN: usize = 1_024;
pub const MAX_READ_ONLY_MOUNTS: usize = 16;
const MAX_INSPECTED_DIR_ENTRIES: usize = 4_096;
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);

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
    #[error("sandbox timed out after {timeout_secs}s (cleaned_up={cleaned_up})")]
    Timeout { timeout_secs: u64, cleaned_up: bool },
    #[error("sandbox {stream} exceeded {limit_bytes} bytes (cleaned_up={cleaned_up})")]
    OutputLimitExceeded {
        stream: &'static str,
        limit_bytes: usize,
        cleaned_up: bool,
    },
    #[error("sandbox workspace disk limit exceeded: {0}")]
    WorkspaceLimitExceeded(String),
    #[error(
        "sandbox execution failed (exit_code={exit_code:?}, cleaned_up={cleaned_up}): {stderr_excerpt}"
    )]
    ExecutionFailed {
        exit_code: Option<i32>,
        cleaned_up: bool,
        stderr_excerpt: String,
    },
    #[error("sandbox cleanup failed: {0}")]
    CleanupFailed(String),
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
            Self::WorkspaceLimitExceeded(_) => "sandbox_disk_limit_exceeded",
            Self::ExecutionFailed { .. } => "sandbox_execution_failed",
            Self::CleanupFailed(_) => "sandbox_cleanup_failed",
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedRuntime {
    pub engine: OciEngine,
    pub transport: OciTransport,
    pub rootless_verified: bool,
    pub engine_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageVerification {
    pub reference: String,
    pub digest: String,
    pub local_image_id: String,
    pub runtime: VerifiedRuntime,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxLimits {
    pub cpus: f64,
    pub memory_mb: u64,
    pub pids_limit: u32,
    pub tmpfs_mb: u64,
    pub workspace_max_bytes: u64,
    pub timeout_secs: u64,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
    pub max_output_files: usize,
    pub max_total_output_bytes: u64,
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
        if !(8..=MAX_TMPFS_DISK_MB).contains(&self.tmpfs_mb) {
            return Err(SandboxError::InvalidPolicy(
                "tmpfs_mb must be between 8 and 4096".into(),
            ));
        }
        if !(1_024..=MAX_WORKSPACE_DISK_BYTES).contains(&self.workspace_max_bytes) {
            return Err(SandboxError::InvalidPolicy(
                "workspace_max_bytes must be between 1024 and 8 GiB".into(),
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
        if !(1..=MAX_OUTPUT_FILES).contains(&self.max_output_files) {
            return Err(SandboxError::InvalidPolicy(
                "max_output_files must be between 1 and 2048".into(),
            ));
        }
        if !(1_024..=MAX_TOTAL_OUTPUT_BYTES).contains(&self.max_total_output_bytes)
            || self.max_total_output_bytes > self.workspace_max_bytes
        {
            return Err(SandboxError::InvalidPolicy(
                "max_total_output_bytes must be within [1024, 2 GiB] and <= workspace_max_bytes"
                    .into(),
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
                tmpfs_mb: 256,
                workspace_max_bytes: 512 * 1024 * 1024,
                timeout_secs: 120,
                max_stdout_bytes: 256 * 1024,
                max_stderr_bytes: 256 * 1024,
                max_output_files: 512,
                max_total_output_bytes: 512 * 1024 * 1024,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxRequest {
    pub job_id: String,
    pub invocation_id: String,
    pub workspace_dir: PathBuf,
    pub read_only_mounts: Vec<ReadOnlyMount>,
    pub command: SandboxCommand,
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxExecutionResult {
    pub container_name: String,
    pub runtime: VerifiedRuntime,
    pub runtime_policy_sha256: String,
    pub image_digest: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub workspace_files: usize,
    pub workspace_bytes: u64,
    pub cleaned_up: bool,
}

/// Fail-closed interface for isolated sandbox detection, local image pinning, and execution.
pub trait SandboxBackend: Send + Sync {
    fn probe(&self) -> Result<VerifiedRuntime, SandboxError>;
    fn verify_policy_image(
        &self,
        policy: &SandboxPolicy,
    ) -> Result<ImageVerification, SandboxError>;
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
    fn probe(&self) -> Result<VerifiedRuntime, SandboxError> {
        Err(SandboxError::SandboxUnavailable(self.reason.clone()))
    }

    fn verify_policy_image(
        &self,
        policy: &SandboxPolicy,
    ) -> Result<ImageVerification, SandboxError> {
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
}

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("program not found: {0}")]
    NotFound(String),
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

/// Real OS command runner with strict host environment allowlisting and bounded pipe reads.
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
            SupervisedRunOutcome::TimedOut => Err(RunnerError::Failed(format!(
                "probe '{}' timed out after {}s",
                spec.program,
                timeout.as_secs()
            ))),
            SupervisedRunOutcome::OutputLimitExceeded {
                stream,
                limit_bytes,
            } => Err(RunnerError::Failed(format!(
                "probe '{}' {stream} exceeded {limit_bytes} bytes",
                spec.program
            ))),
        }
    }

    fn run_supervised(
        &self,
        spec: &CommandSpec,
        timeout: Duration,
        max_stdout_bytes: usize,
        max_stderr_bytes: usize,
    ) -> Result<SupervisedRunOutcome, RunnerError> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();

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

        supervise_child(&mut child, timeout, max_stdout_bytes, max_stderr_bytes)
    }
}

pub fn supervise_child(
    child: &mut Child,
    timeout: Duration,
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

    let out_handle = spawn_bounded_reader(
        stdout,
        max_stdout_bytes,
        "stdout",
        Arc::clone(&exceeded),
        Arc::clone(&exceeded_stream),
    );
    let err_handle = spawn_bounded_reader(
        stderr,
        max_stderr_bytes,
        "stderr",
        Arc::clone(&exceeded),
        Arc::clone(&exceeded_stream),
    );

    let start = Instant::now();
    let status: Option<ExitStatus> = loop {
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

        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(SupervisedRunOutcome::TimedOut);
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunnerError::Failed(err.to_string()));
            }
        }
    };

    let stdout_bytes = out_handle
        .join()
        .map_err(|_| RunnerError::Failed("stdout reader thread panicked".into()))?
        .map_err(RunnerError::Failed)?;
    let stderr_bytes = err_handle
        .join()
        .map_err(|_| RunnerError::Failed("stderr reader thread panicked".into()))?
        .map_err(RunnerError::Failed)?;

    if exceeded.load(Ordering::Acquire) {
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

    Ok(SupervisedRunOutcome::Exited(ProcessOutput {
        exit_code: status.and_then(|s| s.code()),
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    }))
}

fn spawn_bounded_reader<R: Read + Send + 'static>(
    mut reader: R,
    max_bytes: usize,
    stream_name: &'static str,
    exceeded: Arc<AtomicBool>,
    exceeded_stream: Arc<std::sync::Mutex<Option<&'static str>>>,
) -> thread::JoinHandle<Result<Vec<u8>, String>> {
    thread::spawn(move || {
        let mut collected = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = reader.read(&mut buf).map_err(|err| err.to_string())?;
            if n == 0 {
                break;
            }
            if collected.len().saturating_add(n) > max_bytes {
                if let Ok(mut guard) = exceeded_stream.lock() {
                    if guard.is_none() {
                        *guard = Some(stream_name);
                    }
                }
                exceeded.store(true, Ordering::Release);
                break;
            }
            collected.extend_from_slice(&buf[..n]);
        }
        Ok(collected)
    })
}

/// Linux OCI sandbox backend supporting native Linux and Windows via WSL2.
#[derive(Clone, Debug)]
pub struct OciSandboxBackend<R: CommandRunner> {
    platform: HostPlatform,
    config: OciBackendConfig,
    runner: R,
}

impl OciSandboxBackend<SystemCommandRunner> {
    pub fn system(config: OciBackendConfig) -> Self {
        Self {
            platform: HostPlatform::current(),
            config,
            runner: SystemCommandRunner,
        }
    }
}

impl<R: CommandRunner> OciSandboxBackend<R> {
    pub fn with_runner(platform: HostPlatform, config: OciBackendConfig, runner: R) -> Self {
        Self {
            platform,
            config,
            runner,
        }
    }

    fn detect_transport(&self) -> Result<OciTransport, SandboxError> {
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
                let output = self.runner.run_probe(&spec, PROBE_TIMEOUT).map_err(|err| {
                    SandboxError::SandboxUnavailable(format!("wsl2 unavailable: {err}"))
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

    fn probe_engine(
        &self,
        transport: &OciTransport,
        engine: OciEngine,
    ) -> Result<VerifiedRuntime, String> {
        let info_format = match engine {
            OciEngine::Podman => "{{.Host.Security.Rootless}}|{{.Version.Version}}",
            OciEngine::Docker => "{{json .SecurityOptions}}|{{.ServerVersion}}",
        };
        let spec = wrap_command(
            transport,
            engine.binary(),
            vec!["info".into(), "--format".into(), info_format.into()],
            BTreeMap::new(),
        );
        let output = self
            .runner
            .run_probe(&spec, PROBE_TIMEOUT)
            .map_err(|err| format!("{}: {err}", engine.binary()))?;
        if output.exit_code != Some(0) {
            let err_text = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(format!(
                "{} info exited with {:?}: {}",
                engine.binary(),
                output.exit_code,
                truncate_excerpt(&err_text, 200)
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let (left, right) = stdout
            .rsplit_once('|')
            .ok_or_else(|| format!("{} returned malformed info payload", engine.binary()))?;
        let version = right.trim();
        if version.is_empty() {
            return Err(format!("{} reported empty version", engine.binary()));
        }

        let rootless_verified = match engine {
            OciEngine::Podman => left.trim() == "true",
            OciEngine::Docker => parse_docker_rootless(left.trim()),
        };

        if !rootless_verified {
            return Err(format!(
                "{} is running without verified rootless security mode",
                engine.binary()
            ));
        }

        Ok(VerifiedRuntime {
            engine,
            transport: transport.clone(),
            rootless_verified: true,
            engine_version: version.to_owned(),
        })
    }

    fn force_remove_container(
        &self,
        runtime: &VerifiedRuntime,
        container_name: &str,
    ) -> Result<(), SandboxError> {
        let spec = wrap_command(
            &runtime.transport,
            runtime.engine.binary(),
            vec!["rm".into(), "-f".into(), container_name.into()],
            BTreeMap::new(),
        );
        let output = self
            .runner
            .run_probe(&spec, CLEANUP_TIMEOUT)
            .map_err(|err| SandboxError::CleanupFailed(err.to_string()))?;
        if output.exit_code == Some(0) {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        if stderr.contains("no such container") || stderr.contains("not found") {
            return Ok(());
        }
        Err(SandboxError::CleanupFailed(format!(
            "failed to remove container {container_name}: {}",
            truncate_excerpt(&stderr, 200)
        )))
    }
}

impl<R: CommandRunner> SandboxBackend for OciSandboxBackend<R> {
    fn probe(&self) -> Result<VerifiedRuntime, SandboxError> {
        let transport = self.detect_transport()?;
        let mut diagnostics = Vec::new();
        for engine in [OciEngine::Podman, OciEngine::Docker] {
            match self.probe_engine(&transport, engine) {
                Ok(runtime) => return Ok(runtime),
                Err(reason) => diagnostics.push(reason),
            }
        }
        Err(SandboxError::SandboxUnavailable(format!(
            "no verified rootless Linux OCI runtime found ({})",
            diagnostics.join("; ")
        )))
    }

    fn verify_policy_image(
        &self,
        policy: &SandboxPolicy,
    ) -> Result<ImageVerification, SandboxError> {
        policy.validate()?;
        let runtime = self.probe()?;
        let expected_digest = policy.image.digest()?;
        let spec = wrap_command(
            &runtime.transport,
            runtime.engine.binary(),
            vec![
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Id}}|{{json .RepoDigests}}".into(),
                policy.image.reference.clone(),
            ],
            BTreeMap::new(),
        );
        let output = self.runner.run_probe(&spec, PROBE_TIMEOUT).map_err(|err| {
            SandboxError::SandboxUnavailable(format!("local image inspect failed: {err}"))
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

        Ok(ImageVerification {
            reference: policy.image.reference.clone(),
            digest: expected_digest,
            local_image_id: local_id.to_owned(),
            runtime,
        })
    }

    fn execute(
        &self,
        policy: &SandboxPolicy,
        request: &SandboxRequest,
    ) -> Result<SandboxExecutionResult, SandboxError> {
        let policy_hash = policy.policy_sha256()?;
        validate_identifier("job_id", &request.job_id)?;
        validate_identifier("invocation_id", &request.invocation_id)?;
        validate_env(&request.env)?;
        let (entrypoint, container_cmd_args) = request.command.validate()?;

        let workspace_canonical = validate_workspace_dir(&request.workspace_dir)?;
        let (_, pre_run_bytes) = scan_directory_tree(
            &workspace_canonical,
            policy.limits.max_output_files,
            policy.limits.workspace_max_bytes,
            false,
        )?;
        if pre_run_bytes > policy.limits.workspace_max_bytes {
            return Err(SandboxError::WorkspaceLimitExceeded(
                "workspace exceeds limit before execution".into(),
            ));
        }

        if request.read_only_mounts.len() > MAX_READ_ONLY_MOUNTS {
            return Err(SandboxError::InvalidMount(
                "too many read_only_mounts requested".into(),
            ));
        }
        let validated_mounts =
            validate_read_only_mounts(&request.read_only_mounts, &workspace_canonical)?;

        let verification = self.verify_policy_image(policy)?;
        let runtime = verification.runtime;
        let container_name = format!(
            "scenivox-{}-{}",
            sanitize_container_fragment(&request.job_id, 18),
            sanitize_container_fragment(&request.invocation_id, 18)
        );

        let oci_args = build_oci_run_args(
            policy,
            request,
            &policy_hash,
            &container_name,
            &runtime.transport,
            &workspace_canonical,
            &validated_mounts,
            &entrypoint,
            &container_cmd_args,
        )?;

        let spec = wrap_command(
            &runtime.transport,
            runtime.engine.binary(),
            oci_args,
            BTreeMap::new(),
        );

        let started = Instant::now();
        let timeout = Duration::from_secs(policy.limits.timeout_secs);
        let outcome = self
            .runner
            .run_supervised(
                &spec,
                timeout,
                policy.limits.max_stdout_bytes,
                policy.limits.max_stderr_bytes,
            )
            .map_err(|err| {
                let _ = self.force_remove_container(&runtime, &container_name);
                SandboxError::SandboxUnavailable(format!("failed to spawn OCI runtime: {err}"))
            })?;

        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        match outcome {
            SupervisedRunOutcome::TimedOut => {
                let cleaned_up = self
                    .force_remove_container(&runtime, &container_name)
                    .is_ok();
                Err(SandboxError::Timeout {
                    timeout_secs: policy.limits.timeout_secs,
                    cleaned_up,
                })
            }
            SupervisedRunOutcome::OutputLimitExceeded {
                stream,
                limit_bytes,
            } => {
                let cleaned_up = self
                    .force_remove_container(&runtime, &container_name)
                    .is_ok();
                Err(SandboxError::OutputLimitExceeded {
                    stream,
                    limit_bytes,
                    cleaned_up,
                })
            }
            SupervisedRunOutcome::Exited(output) => {
                let cleanup_result = self.force_remove_container(&runtime, &container_name);
                let cleaned_up = cleanup_result.is_ok();
                if output.exit_code != Some(0) {
                    let stderr_excerpt =
                        truncate_excerpt(&String::from_utf8_lossy(&output.stderr), 512);
                    return Err(SandboxError::ExecutionFailed {
                        exit_code: output.exit_code,
                        cleaned_up,
                        stderr_excerpt,
                    });
                }
                cleanup_result?;

                let (workspace_files, workspace_bytes) = scan_directory_tree(
                    &workspace_canonical,
                    policy.limits.max_output_files,
                    policy.limits.max_total_output_bytes,
                    false,
                )?;

                Ok(SandboxExecutionResult {
                    container_name,
                    runtime,
                    runtime_policy_sha256: policy_hash,
                    image_digest: verification.digest,
                    exit_code: 0,
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    duration_ms,
                    workspace_files,
                    workspace_bytes,
                    cleaned_up: true,
                })
            }
        }
    }

    fn cleanup_job(&self, job_id: &str) -> Result<usize, SandboxError> {
        validate_identifier("job_id", job_id)?;
        let runtime = self.probe()?;
        let ps_spec = wrap_command(
            &runtime.transport,
            runtime.engine.binary(),
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
            .map_err(|err| SandboxError::CleanupFailed(err.to_string()))?;
        if output.exit_code != Some(0) {
            return Err(SandboxError::CleanupFailed(format!(
                "container list failed with {:?}",
                output.exit_code
            )));
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
                return Err(SandboxError::CleanupFailed(format!(
                    "unexpected container name '{name}'"
                )));
            }
            self.force_remove_container(&runtime, name)?;
            removed += 1;
        }
        Ok(removed)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_oci_run_args(
    policy: &SandboxPolicy,
    request: &SandboxRequest,
    policy_hash: &str,
    container_name: &str,
    transport: &OciTransport,
    workspace_canonical: &Path,
    validated_mounts: &[(PathBuf, String)],
    entrypoint: &str,
    command_args: &[String],
) -> Result<Vec<String>, SandboxError> {
    policy.validate()?;
    if !is_safe_container_name(container_name) {
        return Err(SandboxError::InvalidInvocation(
            "invalid container name".into(),
        ));
    }

    let workspace_src = format_host_mount_source(workspace_canonical, transport)?;
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
        format!("scenivox.policy_sha256={policy_hash}"),
        "--pull=never".into(),
        "--network=none".into(),
        "--read-only".into(),
        "--cap-drop=ALL".into(),
        "--security-opt=no-new-privileges".into(),
        format!("--user={}:{}", policy.uid, policy.gid),
        format!("--cpus={}", policy.limits.cpus),
        format!("--memory={}m", policy.limits.memory_mb),
        format!("--memory-swap={}m", policy.limits.memory_mb),
        format!("--pids-limit={}", policy.limits.pids_limit),
        format!(
            "--tmpfs=/tmp:rw,noexec,nosuid,nodev,size={}m",
            policy.limits.tmpfs_mb
        ),
        format!("--mount=type=bind,src={workspace_src},dst=/workspace"),
    ];

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
    args.push(format!("--entrypoint={entrypoint}"));
    args.push(policy.image.reference.clone());
    args.extend(command_args.iter().cloned());
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
    // Reference guaranteed to have `@sha256:<64hex>`; return the full `sha256:<64hex>` slice.
    let at_index = reference.len() - 64 - "sha256:".len();
    Ok(&reference[at_index..])
}

fn parse_docker_rootless(security_options_json: &str) -> bool {
    let Ok(options) = serde_json::from_str::<Vec<String>>(security_options_json) else {
        return false;
    };
    options
        .iter()
        .any(|opt| opt == "rootless" || opt == "name=rootless" || opt.starts_with("name=rootless,"))
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
            | "CONTAINER_HOST"
    ) || key.contains("KEY")
        || key.contains("TOKEN")
        || key.contains("SECRET")
        || key.contains("PASSWORD")
        || key.contains("CREDENTIAL")
        || key.contains("AUTH")
}

fn validate_workspace_dir(path: &Path) -> Result<PathBuf, SandboxError> {
    if path.as_os_str().is_empty() {
        return Err(SandboxError::InvalidMount(
            "workspace_dir must not be empty".into(),
        ));
    }
    let meta = fs::symlink_metadata(path)
        .map_err(|err| SandboxError::InvalidMount(format!("workspace_dir unavailable: {err}")))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(SandboxError::InvalidMount(
            "workspace_dir must be a real non-symlink directory".into(),
        ));
    }
    let canonical = fs::canonicalize(path).map_err(|err| {
        SandboxError::InvalidMount(format!("failed to canonicalize workspace_dir: {err}"))
    })?;
    ensure_path_not_sensitive(&canonical)?;
    Ok(canonical)
}

fn validate_read_only_mounts(
    mounts: &[ReadOnlyMount],
    workspace_canonical: &Path,
) -> Result<Vec<(PathBuf, String)>, SandboxError> {
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
        if meta.file_type().is_symlink() {
            return Err(SandboxError::InvalidMount(format!(
                "symlink mount host_path '{}' is prohibited",
                mount.host_path.display()
            )));
        }
        if !(meta.is_file() || meta.is_dir()) {
            return Err(SandboxError::InvalidMount(format!(
                "non-regular/non-directory host_path '{}' is prohibited",
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
        if paths_overlap(&canonical, workspace_canonical) {
            return Err(SandboxError::InvalidMount(
                "read-only mount must not overlap workspace_dir".into(),
            ));
        }
        if meta.is_dir() {
            let _ = scan_directory_tree(&canonical, MAX_INSPECTED_DIR_ENTRIES, u64::MAX, true)?;
        }
        validated.push((canonical, target));
    }

    Ok(validated)
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
                    ".ssh"
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

fn scan_directory_tree(
    root: &Path,
    max_files: usize,
    max_bytes: u64,
    check_sensitive: bool,
) -> Result<(usize, u64), SandboxError> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = 0usize;
    let mut total_bytes = 0u64;

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
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).map_err(|err| {
                SandboxError::InvalidMount(format!(
                    "metadata failed for '{}': {err}",
                    path.display()
                ))
            })?;
            if meta.file_type().is_symlink() {
                return Err(SandboxError::InvalidMount(format!(
                    "symlink '{}' inside sandbox directory is prohibited",
                    path.display()
                )));
            }
            if check_sensitive {
                ensure_path_not_sensitive(&path)?;
            } else {
                let file_name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                if file_name == ".env"
                    || file_name.starts_with(".env.")
                    || file_name == "scenivox.db"
                    || file_name.ends_with(".sock")
                {
                    return Err(SandboxError::InvalidMount(format!(
                        "forbidden file '{file_name}' inside workspace"
                    )));
                }
            }
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                files += 1;
                if files > max_files {
                    return Err(SandboxError::WorkspaceLimitExceeded(format!(
                        "file count exceeded limit of {max_files}"
                    )));
                }
                total_bytes = total_bytes.saturating_add(meta.len());
                if total_bytes > max_bytes {
                    return Err(SandboxError::WorkspaceLimitExceeded(format!(
                        "total bytes exceeded limit of {max_bytes}"
                    )));
                }
            } else {
                return Err(SandboxError::InvalidMount(format!(
                    "special file '{}' is prohibited",
                    path.display()
                )));
            }
        }
    }

    Ok((files, total_bytes))
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
