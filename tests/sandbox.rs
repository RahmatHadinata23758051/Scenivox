use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use scenivox::sandbox::{
    ArtifactTransferRequest, CommandRunner, CommandSpec, ControlEvidenceSource, DetectedRuntime,
    EffectiveResourceControls, EmpiricalControlProbe, ExpectedArtifact, HostPlatform, MediaTool,
    OciBackendConfig, OciEngine, OciSandboxBackend, OciTransport, PinnedDockerRuntime,
    ProcessOutput, ReadOnlyMount, RunnerError, SCENIVOX_CHROMIUM_SECCOMP_SHA256, SandboxBackend,
    SandboxCommand, SandboxError, SandboxPolicy, SandboxRequest, ScriptMountRoot, ScriptRuntime,
    SupervisedRunOutcome, SystemCommandRunner, TrustedStagingRoots, UnavailableSandboxBackend,
    VerifiedIsolation, WorkspaceQuotaMode, build_oci_run_args, format_host_mount_source,
    validate_empirical_probe_against_policy, verify_verified_isolation_for_dispatch,
};

const TEST_IMAGE_DIGEST: &str =
    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const TEST_IMAGE_REF: &str = "ghcr.io/scenivox/sandbox@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const OTHER_IMAGE_DIGEST: &str =
    "sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const JOB_ID: &str = "11111111-2222-4333-8444-555555555555";
const OTHER_JOB_ID: &str = "99999999-8888-4777-8666-555555555555";

#[derive(Default)]
struct ScriptedCommandRunner {
    recorded: Mutex<Vec<(CommandSpec, Duration)>>,
    probe_responses: Mutex<VecDeque<Result<ProcessOutput, RunnerError>>>,
    supervised_responses: Mutex<VecDeque<Result<SupervisedRunOutcome, RunnerError>>>,
}

impl ScriptedCommandRunner {
    fn push_probe(&self, stdout: &[u8], stderr: &[u8], exit_code: Option<i32>) {
        self.probe_responses
            .lock()
            .unwrap()
            .push_back(Ok(ProcessOutput {
                exit_code,
                stdout: stdout.to_vec(),
                stderr: stderr.to_vec(),
            }));
    }

    fn push_probe_err(&self, err: RunnerError) {
        self.probe_responses.lock().unwrap().push_back(Err(err));
    }

    fn push_supervised(&self, outcome: SupervisedRunOutcome) {
        self.supervised_responses
            .lock()
            .unwrap()
            .push_back(Ok(outcome));
    }

    fn push_supervised_err(&self, err: RunnerError) {
        self.supervised_responses
            .lock()
            .unwrap()
            .push_back(Err(err));
    }

    fn commands(&self) -> Vec<CommandSpec> {
        self.recorded
            .lock()
            .unwrap()
            .iter()
            .map(|(spec, _)| spec.clone())
            .collect()
    }

    fn commands_with_timeouts(&self) -> Vec<(CommandSpec, Duration)> {
        self.recorded.lock().unwrap().clone()
    }
}

impl CommandRunner for ScriptedCommandRunner {
    fn run_probe(
        &self,
        spec: &CommandSpec,
        timeout: Duration,
    ) -> Result<ProcessOutput, RunnerError> {
        self.recorded.lock().unwrap().push((spec.clone(), timeout));
        self.probe_responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected probe invocation without scripted response")
    }

    fn run_supervised(
        &self,
        spec: &CommandSpec,
        timeout: Duration,
        _max_stdout_bytes: usize,
        _max_stderr_bytes: usize,
    ) -> Result<SupervisedRunOutcome, RunnerError> {
        self.recorded.lock().unwrap().push((spec.clone(), timeout));
        self.supervised_responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected supervised invocation without scripted response")
    }
}

fn podman_info_json(
    rootless: bool,
    cgroup_version: &str,
    controllers: &[&str],
    remote: bool,
) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "host": {
            "os": "linux",
            "serviceIsRemote": remote,
            "security": { "rootless": rootless },
            "cgroupVersion": cgroup_version,
            "cgroupControllers": controllers,
            "ociRuntime": { "name": "crun" }
        },
        "version": { "Version": "5.4.1" }
    }))
    .unwrap()
}

fn docker_context_json(host: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!([{
        "Name": "rootless",
        "Endpoints": { "docker": { "Host": host } }
    }]))
    .unwrap()
}

fn docker_info_json(
    rootless: bool,
    cgroup_version: &str,
    pids_limit: bool,
    cpu_quota: bool,
) -> Vec<u8> {
    let sec_opts = if rootless {
        vec!["name=seccomp,profile=builtin", "name=rootless"]
    } else {
        vec!["name=seccomp,profile=builtin"]
    };
    serde_json::to_vec(&serde_json::json!({
        "OSType": "linux",
        "ServerVersion": "28.3.3",
        "SecurityOptions": sec_opts,
        "CgroupVersion": cgroup_version,
        "CpuCfsPeriod": true,
        "CpuCfsQuota": cpu_quota,
        "MemoryLimit": true,
        "SwapLimit": true,
        "PidsLimit": pids_limit,
        "DefaultRuntime": "runc"
    }))
    .unwrap()
}

struct TestStaging {
    _temp: tempfile::TempDir,
    roots: TrustedStagingRoots,
    job_inputs: PathBuf,
    other_job_inputs: PathBuf,
    skill_dir: PathBuf,
    unauthorized_dir: PathBuf,
}

fn setup_staging() -> TestStaging {
    let temp = tempfile::tempdir().unwrap();
    let jobs_root = temp.path().join("jobs");
    let skills_root = temp.path().join("skills");
    let job_inputs = jobs_root.join(JOB_ID).join("inputs");
    let other_job_inputs = jobs_root.join(OTHER_JOB_ID).join("inputs");
    let skill_dir = skills_root.join("stick-figure-v1");
    let unauthorized_dir = temp.path().join("arbitrary-host-dir");

    fs::create_dir_all(&job_inputs).unwrap();
    fs::write(job_inputs.join("narration.wav"), b"RIFFdemo").unwrap();
    fs::create_dir_all(&other_job_inputs).unwrap();
    fs::write(other_job_inputs.join("other.wav"), b"RIFFother").unwrap();
    fs::create_dir_all(skill_dir.join("engine/tools")).unwrap();
    fs::write(
        skill_dir.join("engine/tools/render.js"),
        "// render entrypoint\n",
    )
    .unwrap();
    fs::create_dir_all(&unauthorized_dir).unwrap();
    fs::write(unauthorized_dir.join("data.txt"), b"untrusted").unwrap();

    TestStaging {
        _temp: temp,
        roots: TrustedStagingRoots {
            jobs_root: Some(jobs_root),
            skills_root: Some(skills_root),
        },
        job_inputs,
        other_job_inputs,
        skill_dir,
        unauthorized_dir,
    }
}

fn default_policy() -> SandboxPolicy {
    SandboxPolicy::hardened_default(TEST_IMAGE_REF).unwrap()
}

fn default_request(staging: &TestStaging) -> SandboxRequest {
    SandboxRequest {
        job_id: JOB_ID.into(),
        invocation_id: "step-01".into(),
        host_workspace_dir: None,
        read_only_mounts: vec![
            ReadOnlyMount {
                host_path: staging.skill_dir.clone(),
                container_path: "/skill".into(),
            },
            ReadOnlyMount {
                host_path: staging.job_inputs.clone(),
                container_path: "/inputs/job".into(),
            },
        ],
        command: SandboxCommand::Script {
            runtime: ScriptRuntime::Node,
            root: ScriptMountRoot::Skill,
            script_rel_path: "engine/tools/render.js".into(),
            args: vec!["video".into(), "final.mp4".into()],
        },
        env: BTreeMap::from([("SCENIVOX_LOCALE".into(), "id-ID".into())]),
        artifact_transfer: None,
    }
}

fn utf16le_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 2);
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

fn push_current_platform_rootless_docker_probe(runner: &ScriptedCommandRunner) {
    if HostPlatform::current() == HostPlatform::Windows {
        let wsl_utf16 = utf16le_bytes(
            "  NAME              STATE           VERSION\r\n* Ubuntu            Running         2\r\n",
        );
        runner.push_probe(&wsl_utf16, b"", Some(0));
    }
    runner.push_probe_err(RunnerError::NotFound("podman".into()));
    runner.push_probe(
        &docker_context_json("unix:///run/user/1000/docker.sock"),
        b"",
        Some(0),
    );
    runner.push_probe(&docker_info_json(true, "2", true, true), b"", Some(0));
}

fn oci_args(spec: &CommandSpec) -> &[String] {
    if spec.program == "wsl.exe" && spec.args.len() >= 4 && spec.args[2] == "--" {
        &spec.args[4..]
    } else {
        &spec.args
    }
}

#[test]
fn finding1_policy_enforces_workspace_byte_and_inode_quotas_and_rejects_unbounded_bind_mounts() {
    let p1 = default_policy();
    let h1 = p1.policy_sha256().unwrap();
    assert_eq!(h1.len(), 64);

    let mut p2 = p1.clone();
    p2.limits.workspace_inodes = 4_096;
    assert_ne!(p2.policy_sha256().unwrap(), h1);

    // Reject weak limits (byte/inode quotas out of bounds, non-finite CPU, weak flags).
    let mut bad_inodes = p1.clone();
    bad_inodes.limits.workspace_inodes = 10;
    assert_eq!(
        bad_inodes.validate().unwrap_err().code(),
        "invalid_sandbox_policy"
    );

    let mut bad_ws_bytes = p1.clone();
    bad_ws_bytes.limits.workspace_bytes = 1024;
    assert_eq!(
        bad_ws_bytes.validate().unwrap_err().code(),
        "invalid_sandbox_policy"
    );

    let mut bad_tmp_inodes = p1.clone();
    bad_tmp_inodes.limits.tmpfs_inodes = 0;
    assert_eq!(
        bad_tmp_inodes.validate().unwrap_err().code(),
        "invalid_sandbox_policy"
    );

    // Host bind-mount mode fails closed because rootless bind mounts cannot enforce byte/inode quotas.
    let staging = setup_staging();
    let runner = Arc::new(ScriptedCommandRunner::default());
    let backend = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig {
            preferred_wsl_distro: None,
            trusted_roots: staging.roots.clone(),
        },
        Arc::clone(&runner),
    );

    let mut bind_policy = default_policy();
    bind_policy.workspace_mode = WorkspaceQuotaMode::HostBindWithVerifiedQuota;
    let req = default_request(&staging);
    assert_eq!(
        backend.execute(&bind_policy, &req).unwrap_err().code(),
        "sandbox_unavailable"
    );
    assert!(runner.commands().is_empty());

    // Supplying host_workspace_dir under TmpfsQuota is also rejected.
    let mut bad_req = default_request(&staging);
    bad_req.host_workspace_dir = Some(staging.job_inputs.clone());
    assert_eq!(
        backend.execute(&p1, &bad_req).unwrap_err().code(),
        "invalid_sandbox_mount"
    );

    // Build args directly verifies enforceable kernel tmpfs size + nr_inodes + mode=1777 for /tmp and /workspace.
    let args = build_oci_run_args(
        &p1,
        &req,
        &h1,
        "run-123",
        "scenivox-111111111111-0123456789abcdef",
        &OciTransport::NativeLinux,
        &[],
        "node",
        &["/skill/engine/tools/render.js".into()],
    )
    .unwrap();
    assert!(
        args.iter()
            .any(|a| a
                == "--tmpfs=/workspace:rw,nosuid,nodev,size=536870912,nr_inodes=8192,mode=1777")
    );
    assert!(args.iter().any(
        |a| a == "--tmpfs=/tmp:rw,noexec,nosuid,nodev,size=268435456,nr_inodes=4096,mode=1777"
    ));
    assert!(
        !args
            .iter()
            .any(|a| a.contains("dst=/workspace") && a.starts_with("--mount=type=bind"))
    );
}

#[test]
fn finding2_mounts_restricted_to_authorized_job_and_skill_roots_only() {
    let staging = setup_staging();
    let policy = default_policy();

    // 1. Default backend with empty TrustedStagingRoots rejects all host mounts (default deny).
    let empty_backend = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig::default(),
        ScriptedCommandRunner::default(),
    );
    let req = default_request(&staging);
    assert_eq!(
        empty_backend.execute(&policy, &req).unwrap_err().code(),
        "invalid_sandbox_mount"
    );

    let backend = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig {
            preferred_wsl_distro: None,
            trusted_roots: staging.roots.clone(),
        },
        ScriptedCommandRunner::default(),
    );

    // 2. Arbitrary host directory (even without sensitive filenames) is rejected!
    let mut arbitrary_req = default_request(&staging);
    arbitrary_req.read_only_mounts.push(ReadOnlyMount {
        host_path: staging.unauthorized_dir.clone(),
        container_path: "/inputs/arbitrary".into(),
    });
    assert_eq!(
        backend.execute(&policy, &arbitrary_req).unwrap_err().code(),
        "invalid_sandbox_mount"
    );

    // 3. Another job's staging directory is rejected (cross-job isolation).
    let mut cross_job_req = default_request(&staging);
    cross_job_req.read_only_mounts = vec![
        ReadOnlyMount {
            host_path: staging.skill_dir.clone(),
            container_path: "/skill".into(),
        },
        ReadOnlyMount {
            host_path: staging.other_job_inputs.clone(),
            container_path: "/inputs/stolen".into(),
        },
    ];
    assert_eq!(
        backend.execute(&policy, &cross_job_req).unwrap_err().code(),
        "invalid_sandbox_mount"
    );

    // 4. Skill store cannot be mounted under `/inputs/...`.
    let mut misrouted_skill = default_request(&staging);
    misrouted_skill.read_only_mounts = vec![
        ReadOnlyMount {
            host_path: staging.skill_dir.clone(),
            container_path: "/skill".into(),
        },
        ReadOnlyMount {
            host_path: staging.skill_dir.clone(),
            container_path: "/inputs/skill_copy".into(),
        },
    ];
    assert_eq!(
        backend
            .execute(&policy, &misrouted_skill)
            .unwrap_err()
            .code(),
        "invalid_sandbox_mount"
    );

    // 5. Missing `/skill` mount when ScriptMountRoot::Skill is selected is rejected.
    let mut no_skill_req = default_request(&staging);
    no_skill_req.read_only_mounts = vec![ReadOnlyMount {
        host_path: staging.job_inputs.clone(),
        container_path: "/inputs/job".into(),
    }];
    assert_eq!(
        backend.execute(&policy, &no_skill_req).unwrap_err().code(),
        "invalid_sandbox_mount"
    );

    // 6. Symlink inside job inputs escaping outside is rejected.
    let escape_link = staging.job_inputs.join("escape.txt");
    if create_symlink_file(&staging.unauthorized_dir.join("data.txt"), &escape_link) {
        let req = default_request(&staging);
        assert_eq!(
            backend.execute(&policy, &req).unwrap_err().code(),
            "invalid_sandbox_mount"
        );
        let _ = fs::remove_file(&escape_link);
    }

    // 7. Special FIFO file on Unix inside authorized root is rejected.
    #[cfg(unix)]
    {
        let fifo_path = staging.job_inputs.join("pipe.fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .unwrap();
        if status.success() {
            let req = default_request(&staging);
            assert_eq!(
                backend.execute(&policy, &req).unwrap_err().code(),
                "invalid_sandbox_mount"
            );
            let _ = fs::remove_file(&fifo_path);
        }
    }
}

#[test]
fn finding3_prevents_container_name_collisions_enforces_total_deadline_and_fails_on_uncertain_cleanup()
 {
    let staging = setup_staging();
    let policy = default_policy();
    let request = default_request(&staging);

    // 1. Identical requests produce distinct UUIDv4 container names and run_ids.
    let runner = Arc::new(ScriptedCommandRunner::default());
    for _ in 0..2 {
        push_current_platform_rootless_docker_probe(&runner);
        runner.push_probe(
            format!("{TEST_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{TEST_IMAGE_DIGEST}\"]\n")
                .as_bytes(),
            b"",
            Some(0),
        );
        runner.push_supervised(SupervisedRunOutcome::Exited(ProcessOutput {
            exit_code: Some(0),
            stdout: b"ok".to_vec(),
            stderr: Vec::new(),
        }));
        runner.push_probe(b"", b"no such container", Some(1));
        runner.push_probe(b"", b"", Some(0)); // ps -a confirmation
    }

    let backend = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig {
            preferred_wsl_distro: None,
            trusted_roots: staging.roots.clone(),
        },
        Arc::clone(&runner),
    );
    let r1 = backend.execute(&policy, &request).unwrap();
    let r2 = backend.execute(&policy, &request).unwrap();
    assert_ne!(r1.container_name, r2.container_name);
    assert_ne!(r1.run_id, r2.run_id);
    assert!(r1.cleanup_verified && r2.cleanup_verified);
    assert_eq!(
        r1.isolation.controls_evidence_source,
        ControlEvidenceSource::EngineMetadataOnly
    );
    assert!(!r1.isolation.empirically_verified);

    // 2. End-to-end deadline caps probe + inspect + supervised run.
    let mut short_policy = default_policy();
    short_policy.limits.timeout_secs = 2;
    let runner_deadline = Arc::new(ScriptedCommandRunner::default());
    push_current_platform_rootless_docker_probe(&runner_deadline);
    runner_deadline.push_probe(
        format!("{TEST_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{TEST_IMAGE_DIGEST}\"]\n")
            .as_bytes(),
        b"",
        Some(0),
    );
    runner_deadline.push_supervised(SupervisedRunOutcome::Exited(ProcessOutput {
        exit_code: Some(0),
        stdout: Vec::new(),
        stderr: Vec::new(),
    }));
    runner_deadline.push_probe(b"", b"", Some(0));
    let backend_deadline = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig {
            preferred_wsl_distro: None,
            trusted_roots: staging.roots.clone(),
        },
        Arc::clone(&runner_deadline),
    );
    backend_deadline.execute(&short_policy, &request).unwrap();
    let timed_cmds = runner_deadline.commands_with_timeouts();
    let run_budget = timed_cmds
        .iter()
        .find(|(spec, _)| oci_args(spec).first().map(String::as_str) == Some("run"))
        .map(|(_, d)| *d)
        .unwrap();
    assert!(run_budget <= Duration::from_secs(2));

    // Probe timeout during execution maps to Timeout (no container spawned yet).
    let runner_probe_to = Arc::new(ScriptedCommandRunner::default());
    if HostPlatform::current() == HostPlatform::Windows {
        let wsl_utf16 = utf16le_bytes("  NAME STATE VERSION\r\n* Ubuntu Running 2\r\n");
        runner_probe_to.push_probe(&wsl_utf16, b"", Some(0));
    }
    runner_probe_to.push_probe_err(RunnerError::TimedOut(Duration::from_secs(2)));
    let backend_probe_to = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig {
            preferred_wsl_distro: None,
            trusted_roots: staging.roots.clone(),
        },
        runner_probe_to,
    );
    assert_eq!(
        backend_probe_to
            .execute(&short_policy, &request)
            .unwrap_err()
            .code(),
        "sandbox_timeout"
    );

    // 3. Uncertain/failed cleanup is ALWAYS surfaced as CleanupFailed (`sandbox_cleanup_failed`),
    // even when triggered by Timeout or non-zero exit or ps-still-present!
    let runner_uncertain_to = Arc::new(ScriptedCommandRunner::default());
    push_current_platform_rootless_docker_probe(&runner_uncertain_to);
    runner_uncertain_to.push_probe(
        format!("{TEST_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{TEST_IMAGE_DIGEST}\"]\n")
            .as_bytes(),
        b"",
        Some(0),
    );
    runner_uncertain_to.push_supervised(SupervisedRunOutcome::TimedOut);
    runner_uncertain_to.push_probe(b"", b"daemon permission denied", Some(125));
    let backend_uncertain_to = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig {
            preferred_wsl_distro: None,
            trusted_roots: staging.roots.clone(),
        },
        runner_uncertain_to,
    );
    assert_eq!(
        backend_uncertain_to
            .execute(&policy, &request)
            .unwrap_err()
            .code(),
        "sandbox_cleanup_failed"
    );

    // Container claims "no such container" on `rm -f`, but `ps -a` still sees it -> CleanupFailed!
    let runner_ghost = Arc::new(ScriptedCommandRunner::default());
    push_current_platform_rootless_docker_probe(&runner_ghost);
    runner_ghost.push_probe(
        format!("{TEST_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{TEST_IMAGE_DIGEST}\"]\n")
            .as_bytes(),
        b"",
        Some(0),
    );
    runner_ghost.push_supervised(SupervisedRunOutcome::Exited(ProcessOutput {
        exit_code: Some(0),
        stdout: Vec::new(),
        stderr: Vec::new(),
    }));
    runner_ghost.push_probe(b"", b"no such container", Some(1));
    // ps -a confirms absence only when empty; non-empty matching name fails cleanup:
    runner_ghost.push_probe_err(RunnerError::TimedOut(Duration::from_secs(10)));
    let backend_ghost = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig {
            preferred_wsl_distro: None,
            trusted_roots: staging.roots.clone(),
        },
        runner_ghost,
    );
    assert_eq!(
        backend_ghost.execute(&policy, &request).unwrap_err().code(),
        "sandbox_cleanup_failed"
    );

    // 4. Pipe retained after exit cleans up container and fails closed; spawn error fails closed too.
    let runner_retained = Arc::new(ScriptedCommandRunner::default());
    push_current_platform_rootless_docker_probe(&runner_retained);
    runner_retained.push_probe(
        format!("{TEST_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{TEST_IMAGE_DIGEST}\"]\n")
            .as_bytes(),
        b"",
        Some(0),
    );
    runner_retained
        .push_supervised(SupervisedRunOutcome::PipeRetainedAfterExit { stream: "stdout" });
    runner_retained.push_probe(b"", b"", Some(0));
    let backend_retained = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig {
            preferred_wsl_distro: None,
            trusted_roots: staging.roots.clone(),
        },
        runner_retained,
    );
    assert_eq!(
        backend_retained
            .execute(&policy, &request)
            .unwrap_err()
            .code(),
        "sandbox_execution_failed"
    );

    let runner_spawn_err = Arc::new(ScriptedCommandRunner::default());
    push_current_platform_rootless_docker_probe(&runner_spawn_err);
    runner_spawn_err.push_probe(
        format!("{TEST_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{TEST_IMAGE_DIGEST}\"]\n")
            .as_bytes(),
        b"",
        Some(0),
    );
    runner_spawn_err.push_supervised_err(RunnerError::NotFound("podman".into()));
    runner_spawn_err.push_probe(b"", b"", Some(0));
    let backend_spawn_err = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig {
            preferred_wsl_distro: None,
            trusted_roots: staging.roots,
        },
        runner_spawn_err,
    );
    assert_eq!(
        backend_spawn_err
            .execute(&policy, &request)
            .unwrap_err()
            .code(),
        "sandbox_unavailable"
    );
}

#[test]
fn finding3_system_runner_bounded_drain_does_not_hang_when_descendant_retains_pipe() {
    // Spawns a real OS child that exits immediately while leaving a sleeping child holding
    // stdout open. Confirms supervise_child returns cleanly without blocking on unbounded joins.
    let runner = SystemCommandRunner;

    #[cfg(unix)]
    let retained_spec = CommandSpec {
        program: "sh".into(),
        args: vec!["-c".into(), "sleep 5 & echo done".into()],
        env: BTreeMap::new(),
    };
    #[cfg(windows)]
    let retained_spec = CommandSpec {
        program: "powershell.exe".into(),
        args: vec![
            "-NoProfile".into(),
            "-Command".into(),
            "$null = Start-Process -FilePath powershell.exe -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 5' -WindowStyle Hidden; Write-Output done".into(),
        ],
        env: BTreeMap::new(),
    };

    let start = Instant::now();
    let outcome = runner
        .run_supervised(&retained_spec, Duration::from_secs(4), 4096, 4096)
        .unwrap();
    let elapsed = start.elapsed();
    // On Windows/Linux depending on whether grandchild inherits stdout pipe, outcome is either
    // Exited (if OS didn't pass handle) or PipeRetainedAfterExit (bounded within <= 3s, well before 5s sleep!).
    assert!(
        elapsed < Duration::from_secs(4),
        "reader wait blocked on descendant pipe: elapsed={elapsed:?}, outcome={outcome:?}"
    );
    assert!(matches!(
        outcome,
        SupervisedRunOutcome::PipeRetainedAfterExit { .. } | SupervisedRunOutcome::Exited(_)
    ));

    // Also test direct child timeout + output-limit capping with real OS processes.
    #[cfg(unix)]
    let sleep_spec = CommandSpec {
        program: "sleep".into(),
        args: vec!["10".into()],
        env: BTreeMap::new(),
    };
    #[cfg(windows)]
    let sleep_spec = CommandSpec {
        program: "powershell.exe".into(),
        args: vec![
            "-NoProfile".into(),
            "-Command".into(),
            "Start-Sleep -Seconds 10".into(),
        ],
        env: BTreeMap::new(),
    };
    assert_eq!(
        runner
            .run_supervised(&sleep_spec, Duration::from_millis(200), 4096, 4096)
            .unwrap(),
        SupervisedRunOutcome::TimedOut
    );
}

#[test]
fn finding4_runtime_verification_rejects_remote_contexts_cgroup_v1_and_missing_controllers() {
    let policy = default_policy();

    // 1. Remote podman connection is rejected.
    let runner_remote_podman = ScriptedCommandRunner::default();
    runner_remote_podman.push_probe(
        &podman_info_json(true, "v2", &["cpu", "memory", "pids"], true),
        b"",
        Some(0),
    );
    runner_remote_podman.push_probe_err(RunnerError::NotFound("docker".into()));
    let backend = OciSandboxBackend::with_runner(
        HostPlatform::Linux,
        OciBackendConfig::default(),
        runner_remote_podman,
    );
    assert_eq!(
        backend.detect_runtime().unwrap_err().code(),
        "sandbox_unavailable"
    );

    // 2. Rootless podman on cgroup v1 or missing `pids` controller is rejected.
    let runner_missing_ctrl = ScriptedCommandRunner::default();
    runner_missing_ctrl.push_probe(
        &podman_info_json(true, "v2", &["memory"], false),
        b"",
        Some(0),
    );
    runner_missing_ctrl.push_probe_err(RunnerError::NotFound("docker".into()));
    let backend = OciSandboxBackend::with_runner(
        HostPlatform::Linux,
        OciBackendConfig::default(),
        runner_missing_ctrl,
    );
    assert_eq!(
        backend.detect_runtime().unwrap_err().code(),
        "sandbox_unavailable"
    );

    // 3. Docker remote tcp:// context is rejected even if daemon itself would claim rootless.
    let runner_remote_docker = ScriptedCommandRunner::default();
    runner_remote_docker.push_probe_err(RunnerError::NotFound("podman".into()));
    runner_remote_docker.push_probe(&docker_context_json("tcp://10.0.0.5:2376"), b"", Some(0));
    let backend = OciSandboxBackend::with_runner(
        HostPlatform::Linux,
        OciBackendConfig::default(),
        runner_remote_docker,
    );
    assert_eq!(
        backend.detect_runtime().unwrap_err().code(),
        "sandbox_unavailable"
    );

    // 4. Docker local unix:// rootless with PidsLimit=false fails closed.
    let runner_no_pids = ScriptedCommandRunner::default();
    runner_no_pids.push_probe_err(RunnerError::NotFound("podman".into()));
    runner_no_pids.push_probe(
        &docker_context_json("unix:///run/user/1000/docker.sock"),
        b"",
        Some(0),
    );
    runner_no_pids.push_probe(&docker_info_json(true, "2", false, true), b"", Some(0));
    let backend = OciSandboxBackend::with_runner(
        HostPlatform::Linux,
        OciBackendConfig::default(),
        runner_no_pids,
    );
    assert_eq!(
        backend.detect_runtime().unwrap_err().code(),
        "sandbox_unavailable"
    );

    // 6. Podman reports tmpfs_inodes_enforced=false truthfully and fails closed in detect_runtime & verify_readiness!
    let runner_podman_inodes = ScriptedCommandRunner::default();
    runner_podman_inodes.push_probe(
        &podman_info_json(true, "v2", &["cpu", "memory", "pids"], false),
        b"",
        Some(0),
    );
    runner_podman_inodes.push_probe_err(RunnerError::NotFound("docker".into()));
    runner_podman_inodes.push_probe(
        &podman_info_json(true, "v2", &["cpu", "memory", "pids"], false),
        b"",
        Some(0),
    );
    runner_podman_inodes.push_probe_err(RunnerError::NotFound("docker".into()));
    let backend_podman_inodes = OciSandboxBackend::with_runner(
        HostPlatform::Linux,
        OciBackendConfig::default(),
        runner_podman_inodes,
    );
    let detect_err = backend_podman_inodes.detect_runtime().unwrap_err();
    assert_eq!(detect_err.code(), "sandbox_unavailable");
    assert!(detect_err.to_string().contains("nr_inodes"));
    let podman_err = backend_podman_inodes.verify_readiness(&policy).unwrap_err();
    assert_eq!(podman_err.code(), "sandbox_unavailable");
    assert!(podman_err.to_string().contains("nr_inodes"));

    // 7. Docker local unix:// rootless cgroup v2 with all controls enforced succeeds detect_runtime
    //    as EngineMetadataOnly (empirically_verified=false), and fails readiness on wrong image digest.
    let runner_ok = ScriptedCommandRunner::default();
    runner_ok.push_probe_err(RunnerError::NotFound("podman".into()));
    runner_ok.push_probe(
        &docker_context_json("unix:///run/user/1000/docker.sock"),
        b"",
        Some(0),
    );
    runner_ok.push_probe(&docker_info_json(true, "2", true, true), b"", Some(0));
    runner_ok.push_probe_err(RunnerError::NotFound("podman".into()));
    runner_ok.push_probe(
        &docker_context_json("unix:///run/user/1000/docker.sock"),
        b"",
        Some(0),
    );
    runner_ok.push_probe(&docker_info_json(true, "2", true, true), b"", Some(0));
    runner_ok.push_probe(
        format!("{OTHER_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{OTHER_IMAGE_DIGEST}\"]\n")
            .as_bytes(),
        b"",
        Some(0),
    );
    let backend_ok =
        OciSandboxBackend::with_runner(HostPlatform::Linux, OciBackendConfig::default(), runner_ok);
    let detected = backend_ok.detect_runtime().unwrap();
    assert_eq!(detected.engine, OciEngine::Docker);
    assert!(detected.local_endpoint_verified);
    assert_eq!(
        detected.metadata_source,
        ControlEvidenceSource::EngineMetadataOnly
    );
    assert!(detected.controls.all_mandatory_enforced());
    assert_eq!(
        backend_ok.verify_readiness(&policy).unwrap_err().code(),
        "sandbox_unavailable"
    );
}

#[test]
fn wsl2_path_translation_and_unavailable_backend_guards() {
    let staging = setup_staging();
    let policy = default_policy();
    let req = default_request(&staging);

    let wsl_transport = OciTransport::Wsl2 {
        distro: "Ubuntu".into(),
    };
    assert_eq!(
        format_host_mount_source(
            Path::new(r"\\?\C:\Users\user\AppData\Local\Temp\job1"),
            &wsl_transport
        )
        .unwrap(),
        "/mnt/c/Users/user/AppData/Local/Temp/job1"
    );
    assert!(matches!(
        format_host_mount_source(Path::new(r"\\server\share\dir"), &wsl_transport),
        Err(SandboxError::InvalidMount(_))
    ));

    let unavailable = UnavailableSandboxBackend::new("no verified runtime");
    assert_eq!(
        unavailable.detect_runtime().unwrap_err().code(),
        "sandbox_unavailable"
    );
    assert_eq!(
        unavailable.verify_readiness(&policy).unwrap_err().code(),
        "sandbox_unavailable"
    );
    assert_eq!(
        unavailable.execute(&policy, &req).unwrap_err().code(),
        "sandbox_unavailable"
    );
    assert_eq!(
        unavailable.cleanup_job("job-1").unwrap_err().code(),
        "sandbox_unavailable"
    );

    // Forbidden execution options & secret env still rejected.
    let backend = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig {
            preferred_wsl_distro: None,
            trusted_roots: staging.roots.clone(),
        },
        ScriptedCommandRunner::default(),
    );
    let mut bad_cmd = default_request(&staging);
    bad_cmd.command = SandboxCommand::MediaTool {
        tool: MediaTool::Ffprobe,
        args: vec!["--privileged".into()],
    };
    assert_eq!(
        backend.execute(&policy, &bad_cmd).unwrap_err().code(),
        "invalid_sandbox_invocation"
    );
    let mut bad_env = default_request(&staging);
    bad_env
        .env
        .insert("DOCKER_HOST".into(), "tcp://evil:2375".into());
    assert_eq!(
        backend.execute(&policy, &bad_env).unwrap_err().code(),
        "invalid_sandbox_invocation"
    );
}

#[test]
fn host_oci_probe_fails_closed_when_unavailable_and_never_falls_back() {
    let backend = OciSandboxBackend::system(OciBackendConfig::default());
    let policy = default_policy();
    match backend.detect_runtime() {
        Ok(runtime) => {
            assert!(runtime.rootless_verified);
            assert!(runtime.local_endpoint_verified);
            assert_eq!(
                runtime.metadata_source,
                ControlEvidenceSource::EngineMetadataOnly
            );
            if !runtime.controls.all_mandatory_enforced() {
                assert_eq!(
                    backend.verify_readiness(&policy).unwrap_err().code(),
                    "sandbox_unavailable"
                );
            }
        }
        Err(err) => {
            assert_eq!(err.code(), "sandbox_unavailable");
        }
    }
}

fn create_symlink_file(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        match std::os::windows::fs::symlink_file(target, link) {
            Ok(()) => true,
            Err(err)
                if err.kind() == std::io::ErrorKind::PermissionDenied
                    || err.raw_os_error() == Some(1314) =>
            {
                false
            }
            Err(err) => panic!("unexpected symlink error: {err}"),
        }
    }
}

type ScriptedArtifactEntry<'a> = (&'a str, &'a [u8], Option<[u8; 32]>);

fn encode_artifact_stream(
    stdout_chunks: &[&[u8]],
    artifacts: &[ScriptedArtifactEntry<'_>],
    include_done: bool,
) -> Vec<u8> {
    use sha2::{Digest, Sha256};

    let mut out = Vec::from(&b"SVXART01"[..]);
    for chunk in stdout_chunks {
        out.push(b'O');
        out.extend_from_slice(&(chunk.len() as u32).to_be_bytes());
        out.extend_from_slice(chunk);
    }
    for (rel, data, override_hash) in artifacts {
        let digest: [u8; 32] = match override_hash {
            Some(h) => *h,
            None => Sha256::digest(data).into(),
        };
        out.push(b'A');
        out.extend_from_slice(&(rel.len() as u16).to_be_bytes());
        out.extend_from_slice(rel.as_bytes());
        out.extend_from_slice(&digest);
        out.extend_from_slice(&(data.len() as u64).to_be_bytes());
        out.extend_from_slice(data);
    }
    if include_done {
        out.push(b'D');
        out.extend_from_slice(&0u32.to_be_bytes());
    }
    out
}

#[test]
fn bounded_artifact_transfer_validates_protocol_bounds_and_job_destination() {
    use sha2::{Digest, Sha256};

    let staging = setup_staging();
    let policy = default_policy();
    let jobs_root = staging.roots.jobs_root.as_ref().unwrap();
    let out_dir = jobs_root.join(JOB_ID).join("outputs");

    // 1. Happy path: extracts allowlisted files into job-owned destination_dir before container exit.
    let runner = Arc::new(ScriptedCommandRunner::default());
    push_current_platform_rootless_docker_probe(&runner);
    runner.push_probe(
        format!("{TEST_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{TEST_IMAGE_DIGEST}\"]\n")
            .as_bytes(),
        b"",
        Some(0),
    );
    let payload_mp4 = b"\x00\x00\x00\x18ftypisomdemo-mp4";
    let payload_log = b"{\"frames\":30}";
    let stream = encode_artifact_stream(
        &[b"render-ok\n"],
        &[
            ("nested/final.mp4", payload_mp4, None),
            ("report.json", payload_log, None),
        ],
        true,
    );
    runner.push_supervised(SupervisedRunOutcome::Exited(ProcessOutput {
        exit_code: Some(0),
        stdout: stream,
        stderr: b"".to_vec(),
    }));
    runner.push_probe(b"", b"", Some(0)); // rm -f verified

    let backend = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig {
            preferred_wsl_distro: None,
            trusted_roots: staging.roots.clone(),
        },
        Arc::clone(&runner),
    );
    let mut req = default_request(&staging);
    req.artifact_transfer = Some(ArtifactTransferRequest {
        destination_dir: out_dir.clone(),
        artifacts: vec![
            ExpectedArtifact {
                relative_path: "nested/final.mp4".into(),
                max_bytes: 1024,
            },
            ExpectedArtifact {
                relative_path: "report.json".into(),
                max_bytes: 512,
            },
        ],
        max_total_bytes: 1536,
    });

    let res = backend.execute(&policy, &req).unwrap();
    assert!(res.cleanup_verified);
    assert_eq!(res.stdout, "render-ok\n");
    assert_eq!(res.transferred_artifacts.len(), 2);
    assert_eq!(
        fs::read(out_dir.join("nested/final.mp4")).unwrap(),
        payload_mp4
    );
    assert_eq!(fs::read(out_dir.join("report.json")).unwrap(), payload_log);
    let expected_mp4_sha: String = Sha256::digest(payload_mp4)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(res.transferred_artifacts[0].sha256, expected_mp4_sha);

    let cmds = runner.commands();
    let run_spec = cmds
        .iter()
        .find(|spec| oci_args(spec).first().map(String::as_str) == Some("run"))
        .unwrap();
    let args = oci_args(run_spec);
    assert!(args.iter().any(|a| a == "--entrypoint=python3"));
    assert!(
        args.iter()
            .any(|a| a == "/opt/scenivox-runtime/artifact_runner.py")
    );
    assert!(args.iter().any(|a| a == "--network=none"));
    assert!(args.iter().any(|a| a == "--read-only"));
    assert!(args.iter().any(|a| a == "--cap-drop=ALL"));

    // 2. Non-empty destination directory (e.g. reusing out_dir) fails closed before container launch!
    let pre_count = runner.commands().len();
    assert_eq!(
        backend.execute(&policy, &req).unwrap_err().code(),
        "invalid_sandbox_mount"
    );
    assert_eq!(runner.commands().len(), pre_count);

    // 3. Cross-job or unauthorized destination directory fails closed before container launch!
    let mut cross_job = req.clone();
    cross_job
        .artifact_transfer
        .as_mut()
        .unwrap()
        .destination_dir = jobs_root.join(OTHER_JOB_ID).join("outputs");
    assert_eq!(
        backend.execute(&policy, &cross_job).unwrap_err().code(),
        "invalid_sandbox_mount"
    );

    let mut outside_job = req.clone();
    outside_job
        .artifact_transfer
        .as_mut()
        .unwrap()
        .destination_dir = staging.unauthorized_dir.join("outputs");
    assert_eq!(
        backend.execute(&policy, &outside_job).unwrap_err().code(),
        "invalid_sandbox_mount"
    );

    // 4. Tampered SHA-256 in transfer stream fails closed and never writes host destination.
    let bad_out_dir = jobs_root.join(JOB_ID).join("outputs-bad-hash");
    let mut bad_hash_req = req.clone();
    bad_hash_req
        .artifact_transfer
        .as_mut()
        .unwrap()
        .destination_dir = bad_out_dir.clone();
    bad_hash_req.artifact_transfer.as_mut().unwrap().artifacts = vec![ExpectedArtifact {
        relative_path: "report.json".into(),
        max_bytes: 512,
    }];
    push_current_platform_rootless_docker_probe(&runner);
    runner.push_probe(
        format!("{TEST_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{TEST_IMAGE_DIGEST}\"]\n")
            .as_bytes(),
        b"",
        Some(0),
    );
    runner.push_supervised(SupervisedRunOutcome::Exited(ProcessOutput {
        exit_code: Some(0),
        stdout: encode_artifact_stream(&[], &[("report.json", b"real", Some([0xAA; 32]))], true),
        stderr: b"".to_vec(),
    }));
    runner.push_probe(b"", b"", Some(0));
    assert_eq!(
        backend.execute(&policy, &bad_hash_req).unwrap_err().code(),
        "sandbox_artifact_transfer_failed"
    );
    assert!(!bad_out_dir.exists());

    // 5. Missing expected artifact or unexpected unrequested artifact fails closed.
    let missing_out_dir = jobs_root.join(JOB_ID).join("outputs-missing");
    let mut missing_req = bad_hash_req.clone();
    missing_req
        .artifact_transfer
        .as_mut()
        .unwrap()
        .destination_dir = missing_out_dir.clone();
    push_current_platform_rootless_docker_probe(&runner);
    runner.push_probe(
        format!("{TEST_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{TEST_IMAGE_DIGEST}\"]\n")
            .as_bytes(),
        b"",
        Some(0),
    );
    runner.push_supervised(SupervisedRunOutcome::Exited(ProcessOutput {
        exit_code: Some(0),
        stdout: encode_artifact_stream(&[], &[("evil.txt", b"x", None)], true),
        stderr: b"".to_vec(),
    }));
    runner.push_probe(b"", b"", Some(0));
    assert_eq!(
        backend.execute(&policy, &missing_req).unwrap_err().code(),
        "sandbox_artifact_transfer_failed"
    );
    assert!(!missing_out_dir.exists());

    // 6. Non-zero exit from artifact_runner (e.g. FIFO or detached process survival) never writes host destination.
    let fifo_out_dir = jobs_root.join(JOB_ID).join("outputs-fifo-fail");
    let mut fifo_req = bad_hash_req.clone();
    fifo_req.artifact_transfer.as_mut().unwrap().destination_dir = fifo_out_dir.clone();
    push_current_platform_rootless_docker_probe(&runner);
    runner.push_probe(
        format!("{TEST_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{TEST_IMAGE_DIGEST}\"]\n")
            .as_bytes(),
        b"",
        Some(0),
    );
    runner.push_supervised(SupervisedRunOutcome::Exited(ProcessOutput {
        exit_code: Some(125),
        stdout: b"SVXART01".to_vec(),
        stderr: b"artifact transfer failed: artifact '/workspace/fifo.mp4' is not a regular file"
            .to_vec(),
    }));
    runner.push_probe(b"", b"", Some(0));
    let fifo_err = backend.execute(&policy, &fifo_req).unwrap_err();
    assert_eq!(fifo_err.code(), "sandbox_execution_failed");
    assert!(!fifo_out_dir.exists());
}

#[test]
fn pinned_rootless_docker_enforces_context_socket_seccomp_image_and_empirical_validation() {
    let valid_seccomp =
        fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("sandbox/seccomp-chromium.json"))
            .unwrap();
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut hasher, &valid_seccomp);
    let actual_seccomp_sha: String = sha2::Digest::finalize(hasher)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(actual_seccomp_sha, SCENIVOX_CHROMIUM_SECCOMP_SHA256);

    let pin = PinnedDockerRuntime::scenivox_rootless(TEST_IMAGE_DIGEST).unwrap();
    assert!(pin.validate().is_ok());

    let mut bad_ctx = pin.clone();
    bad_ctx.context_name = "default".into();
    assert!(bad_ctx.validate().is_err());

    let mut bad_sock = pin.clone();
    bad_sock.unix_socket = "tcp://127.0.0.1:2375".into();
    assert!(bad_sock.validate().is_err());

    let mut bad_sha = pin.clone();
    bad_sha.seccomp_profile_sha256 = "not-valid-sha256".into();
    assert!(bad_sha.validate().is_err());

    let staging = setup_staging();
    let policy = default_policy();
    let runner = Arc::new(ScriptedCommandRunner::default());
    let backend = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig {
            preferred_wsl_distro: Some("Ubuntu".into()),
            trusted_roots: staging.roots.clone(),
        },
        Arc::clone(&runner),
    )
    .with_docker_pin(pin.clone());

    // Helper closure for WSL UTF-16LE probe when running on Windows host
    let push_wsl = |r: &ScriptedCommandRunner| {
        if HostPlatform::current() == HostPlatform::Windows {
            let wsl_utf16 = utf16le_bytes(
                "  NAME              STATE           VERSION\r\n* Ubuntu            Running         2\r\n",
            );
            r.push_probe(&wsl_utf16, b"", Some(0));
        }
    };

    // 1. Mismatched Docker context name rejected
    push_wsl(&runner);
    runner.push_probe(
        serde_json::to_vec(&serde_json::json!([{
            "Name": "default",
            "Endpoints": { "docker": { "Host": pin.unix_socket } }
        }]))
        .unwrap()
        .as_slice(),
        b"",
        Some(0),
    );
    let err = backend.verify_readiness(&policy).unwrap_err();
    assert!(err.to_string().contains("context mismatch"));

    // 2. Mismatched Unix socket rejected
    push_wsl(&runner);
    runner.push_probe(
        serde_json::to_vec(&serde_json::json!([{
            "Name": pin.context_name,
            "Endpoints": { "docker": { "Host": "unix:///var/run/docker.sock" } }
        }]))
        .unwrap()
        .as_slice(),
        b"",
        Some(0),
    );
    let err = backend.verify_readiness(&policy).unwrap_err();
    assert!(err.to_string().contains("socket mismatch"));

    // 3. Tampered seccomp SHA-256 rejected
    let seccomp_probe_override = std::env::var_os("SCENIVOX_TEST_USE_RUNNER_FOR_SECCOMP");
    unsafe {
        std::env::set_var("SCENIVOX_TEST_USE_RUNNER_FOR_SECCOMP", "1");
    }
    push_wsl(&runner);
    runner.push_probe(
        serde_json::to_vec(&serde_json::json!([{
            "Name": pin.context_name,
            "Endpoints": { "docker": { "Host": pin.unix_socket } }
        }]))
        .unwrap()
        .as_slice(),
        b"",
        Some(0),
    );
    runner.push_probe(
        b"{\"defaultAction\":\"SCMP_ACT_ERRNO\",\"syscalls\":[]}",
        b"",
        Some(0),
    );
    let err = backend.verify_readiness(&policy).unwrap_err();
    assert!(err.to_string().contains("seccomp profile SHA-256 mismatch"));

    // 4. Valid pinned rootless Docker passes readiness & includes --context + --security-opt=seccomp=...
    push_wsl(&runner);
    runner.push_probe(
        serde_json::to_vec(&serde_json::json!([{
            "Name": pin.context_name,
            "Endpoints": { "docker": { "Host": pin.unix_socket } }
        }]))
        .unwrap()
        .as_slice(),
        b"",
        Some(0),
    );
    runner.push_probe(&valid_seccomp, b"", Some(0));
    runner.push_probe(&docker_info_json(true, "2", true, true), b"", Some(0));
    runner.push_probe(
        format!("{TEST_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{TEST_IMAGE_DIGEST}\"]\n")
            .as_bytes(),
        b"",
        Some(0),
    );
    let isolation = backend.verify_readiness(&policy).unwrap();
    assert_eq!(
        isolation.docker_context_name.as_deref(),
        Some("scenivox-rootless")
    );
    assert_eq!(
        isolation.seccomp_profile_sha256.as_deref(),
        Some(SCENIVOX_CHROMIUM_SECCOMP_SHA256)
    );
    assert_eq!(
        isolation.controls_evidence_source,
        ControlEvidenceSource::EngineMetadataOnly
    );

    // 5. Image digest mismatch against pinned runtime rejected
    let mut wrong_policy = policy.clone();
    wrong_policy.image = scenivox::sandbox::DigestPinnedImage::new(format!(
        "ghcr.io/scenivox/sandbox@{OTHER_IMAGE_DIGEST}"
    ))
    .unwrap();
    push_wsl(&runner);
    runner.push_probe(
        serde_json::to_vec(&serde_json::json!([{
            "Name": pin.context_name,
            "Endpoints": { "docker": { "Host": pin.unix_socket } }
        }]))
        .unwrap()
        .as_slice(),
        b"",
        Some(0),
    );
    runner.push_probe(&valid_seccomp, b"", Some(0));
    runner.push_probe(&docker_info_json(true, "2", true, true), b"", Some(0));
    let err = backend.verify_readiness(&wrong_policy).unwrap_err();
    assert!(err.to_string().contains("image digest mismatch"));

    if seccomp_probe_override.is_none() {
        unsafe {
            std::env::remove_var("SCENIVOX_TEST_USE_RUNNER_FOR_SECCOMP");
        }
    }

    // 6. Empirical evidence validator rejects any weak or tampered control telemetry.
    let valid_probe = EmpiricalControlProbe {
        uid: policy.uid,
        gid: policy.gid,
        cap_eff_hex: "0000000000000000".into(),
        no_new_privs: 1,
        seccomp_mode: 2,
        network_interfaces: vec!["lo".into()],
        read_only_rootfs_enforced: true,
        cpu_max: "200000 100000".into(),
        memory_max_bytes: policy.limits.memory_mb * 1024 * 1024,
        memory_swap_max_bytes: 0,
        pids_max: policy.limits.pids_limit,
        tmp_inodes_reported: policy.limits.tmpfs_inodes,
        workspace_inodes_reported: policy.limits.workspace_inodes,
        inode_exhaustion_at: policy.limits.workspace_inodes - 1,
        inode_exhaustion_enforced: true,
        chromium_no_sandbox_flag_absent: true,
        chromium_zygote_active: true,
        chromium_renderer_pid_ns_depth: 3,
        chromium_renderer_seccomp_mode: 2,
        chromium_screenshot_bytes: 824,
    };
    assert!(validate_empirical_probe_against_policy(&valid_probe, &policy, true).is_ok());

    let mut bad_net = valid_probe.clone();
    bad_net.network_interfaces.push("eth0".into());
    assert!(validate_empirical_probe_against_policy(&bad_net, &policy, true).is_err());

    let mut bad_inodes = valid_probe.clone();
    bad_inodes.inode_exhaustion_enforced = false;
    assert!(validate_empirical_probe_against_policy(&bad_inodes, &policy, true).is_err());

    let mut bad_chrome = valid_probe.clone();
    bad_chrome.chromium_no_sandbox_flag_absent = false;
    assert!(validate_empirical_probe_against_policy(&bad_chrome, &policy, true).is_err());

    let mut bad_ns = valid_probe;
    bad_ns.chromium_renderer_pid_ns_depth = 1;
    assert!(validate_empirical_probe_against_policy(&bad_ns, &policy, true).is_err());
}

#[test]
fn strict_verified_isolation_gate_rejects_metadata_and_mismatches_and_executes_when_verified() {
    let staging = setup_staging();
    let policy = default_policy();
    let req = default_request(&staging);
    let pin = PinnedDockerRuntime::scenivox_rootless(TEST_IMAGE_DIGEST).unwrap();

    let platform_transport = if HostPlatform::current() == HostPlatform::Windows {
        OciTransport::Wsl2 {
            distro: "Ubuntu".into(),
        }
    } else {
        OciTransport::NativeLinux
    };

    let runner = ScriptedCommandRunner::default();
    let backend = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig {
            preferred_wsl_distro: Some("Ubuntu".into()),
            trusted_roots: staging.roots.clone(),
        },
        runner,
    )
    .with_docker_pin(pin.clone())
    .with_strict_empirical_gate(true);

    let mut metadata_isolation = VerifiedIsolation {
        runtime: DetectedRuntime {
            engine: OciEngine::Docker,
            transport: platform_transport,
            oci_runtime_name: "runc".into(),
            engine_version: "29.1.3".into(),
            cgroup_version: "2".into(),
            rootless_verified: true,
            local_endpoint_verified: true,
            metadata_source: ControlEvidenceSource::EngineMetadataOnly,
            controls: EffectiveResourceControls {
                cpu_quota_enforced: true,
                memory_limit_enforced: true,
                swap_limit_enforced: true,
                pids_limit_enforced: true,
                tmpfs_size_enforced: true,
                tmpfs_inodes_enforced: true,
            },
            docker_context_name: Some(pin.context_name.clone()),
            unix_socket: Some(pin.unix_socket.clone()),
            seccomp_profile_sha256: Some(pin.seccomp_profile_sha256.clone()),
        },
        image_reference: TEST_IMAGE_REF.into(),
        image_digest: TEST_IMAGE_DIGEST.into(),
        local_image_id: TEST_IMAGE_DIGEST.into(),
        runtime_policy_sha256: policy.policy_sha256().unwrap(),
        docker_context_name: Some(pin.context_name.clone()),
        unix_socket: Some(pin.unix_socket.clone()),
        seccomp_profile_sha256: Some(pin.seccomp_profile_sha256.clone()),
        isolation_verified: true,
        mandatory_controls_verified: true,
        empirically_verified: false,
        controls_evidence_source: ControlEvidenceSource::EngineMetadataOnly,
    };

    // Metadata-only isolation must be rejected by both standalone gate and execute_with_verified_isolation.
    assert!(
        verify_verified_isolation_for_dispatch(&policy, &pin, &metadata_isolation)
            .unwrap_err()
            .to_string()
            .contains("empirically verified")
    );
    assert!(
        backend
            .execute_with_verified_isolation(&policy, &metadata_isolation, &req)
            .unwrap_err()
            .to_string()
            .contains("empirically verified")
    );

    // Promote to empirically verified, then test policy SHA mismatch and pinned context mismatch.
    metadata_isolation.empirically_verified = true;
    metadata_isolation.controls_evidence_source = ControlEvidenceSource::EmpiricallyVerified;
    metadata_isolation.runtime.metadata_source = ControlEvidenceSource::EmpiricallyVerified;
    assert!(verify_verified_isolation_for_dispatch(&policy, &pin, &metadata_isolation).is_ok());

    let mut wrong_policy_iso = metadata_isolation.clone();
    wrong_policy_iso.runtime_policy_sha256 = "0".repeat(64);
    assert!(
        verify_verified_isolation_for_dispatch(&policy, &pin, &wrong_policy_iso)
            .unwrap_err()
            .to_string()
            .contains("runtime_policy_sha256")
    );

    let mut wrong_ctx_iso = metadata_isolation.clone();
    wrong_ctx_iso.docker_context_name = Some("default".into());
    assert!(
        verify_verified_isolation_for_dispatch(&policy, &pin, &wrong_ctx_iso)
            .unwrap_err()
            .to_string()
            .contains("scenivox-rootless")
    );

    // Verified isolation passes strict gate and executes containerized step without re-weakening policy.
    let runner_verified = ScriptedCommandRunner::default();
    runner_verified.push_supervised(SupervisedRunOutcome::Exited(ProcessOutput {
        exit_code: Some(0),
        stdout: b"ok".to_vec(),
        stderr: Vec::new(),
    }));
    runner_verified.push_probe(b"", b"no such container", Some(1));
    runner_verified.push_probe(b"", b"", Some(0));
    let verified_backend = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig {
            preferred_wsl_distro: Some("Ubuntu".into()),
            trusted_roots: staging.roots.clone(),
        },
        runner_verified,
    )
    .with_docker_pin(pin)
    .with_strict_empirical_gate(true);
    let res = verified_backend
        .execute_with_verified_isolation(&policy, &metadata_isolation, &req)
        .unwrap();
    assert_eq!(res.exit_code, 0);
    assert!(res.isolation.empirically_verified);
}
