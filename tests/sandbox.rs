use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use scenivox::sandbox::{
    CommandRunner, CommandSpec, HostPlatform, MediaTool, OciBackendConfig, OciEngine,
    OciSandboxBackend, OciTransport, ProcessOutput, ReadOnlyMount, RunnerError, SandboxBackend,
    SandboxCommand, SandboxError, SandboxPolicy, SandboxRequest, ScriptMountRoot, ScriptRuntime,
    SupervisedRunOutcome, SystemCommandRunner, TrustedStagingRoots, UnavailableSandboxBackend,
    WorkspaceQuotaMode, build_oci_run_args, format_host_mount_source,
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
    }
}

fn utf16le_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 2);
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

fn push_current_platform_podman_probe(runner: &ScriptedCommandRunner) {
    if HostPlatform::current() == HostPlatform::Windows {
        let wsl_utf16 = utf16le_bytes(
            "  NAME              STATE           VERSION\r\n* Ubuntu            Running         2\r\n",
        );
        runner.push_probe(&wsl_utf16, b"", Some(0));
    }
    runner.push_probe(
        &podman_info_json(true, "v2", &["cpu", "memory", "pids"], false),
        b"",
        Some(0),
    );
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

    // Build args directly verifies enforceable kernel tmpfs size + nr_inodes for both /tmp and /workspace.
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
            .any(|a| a == "--tmpfs=/workspace:rw,nosuid,nodev,size=536870912,nr_inodes=8192")
    );
    assert!(
        args.iter()
            .any(|a| a == "--tmpfs=/tmp:rw,noexec,nosuid,nodev,size=268435456,nr_inodes=4096")
    );
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
        push_current_platform_podman_probe(&runner);
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

    // 2. End-to-end deadline caps probe + inspect + supervised run.
    let mut short_policy = default_policy();
    short_policy.limits.timeout_secs = 2;
    let runner_deadline = Arc::new(ScriptedCommandRunner::default());
    push_current_platform_podman_probe(&runner_deadline);
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
    push_current_platform_podman_probe(&runner_uncertain_to);
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
    push_current_platform_podman_probe(&runner_ghost);
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
    push_current_platform_podman_probe(&runner_retained);
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
    push_current_platform_podman_probe(&runner_spawn_err);
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

    // 5. Docker local unix:// rootless cgroup v2 with all controls enforced succeeds detect_runtime,
    //    AND verify_readiness separately verifies local digest-pinned image before reporting isolation_verified.
    let runner_ok = ScriptedCommandRunner::default();
    runner_ok.push_probe_err(RunnerError::NotFound("podman".into()));
    runner_ok.push_probe(
        &docker_context_json("unix:///run/user/1000/docker.sock"),
        b"",
        Some(0),
    );
    runner_ok.push_probe(&docker_info_json(true, "2", true, true), b"", Some(0));
    // Now verify_readiness -> second probe + missing image fails readiness while detect_runtime passed!
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
    assert!(detected.controls.all_mandatory_enforced());
    // Readiness still fails closed when pinned image digest does not match locally!
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
    match backend.detect_runtime() {
        Ok(runtime) => {
            assert!(runtime.rootless_verified);
            assert!(runtime.local_endpoint_verified);
            assert!(runtime.controls.all_mandatory_enforced());
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
