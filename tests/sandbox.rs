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
    SupervisedRunOutcome, SystemCommandRunner, UnavailableSandboxBackend, build_oci_run_args,
    format_host_mount_source,
};

const TEST_IMAGE_DIGEST: &str =
    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const TEST_IMAGE_REF: &str = "ghcr.io/scenivox/sandbox@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const OTHER_IMAGE_DIGEST: &str =
    "sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

#[derive(Default)]
struct ScriptedCommandRunner {
    recorded: Mutex<Vec<CommandSpec>>,
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
        self.recorded.lock().unwrap().clone()
    }
}

impl CommandRunner for ScriptedCommandRunner {
    fn run_probe(
        &self,
        spec: &CommandSpec,
        _timeout: Duration,
    ) -> Result<ProcessOutput, RunnerError> {
        self.recorded.lock().unwrap().push(spec.clone());
        self.probe_responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected probe invocation without scripted response")
    }

    fn run_supervised(
        &self,
        spec: &CommandSpec,
        _timeout: Duration,
        _max_stdout_bytes: usize,
        _max_stderr_bytes: usize,
    ) -> Result<SupervisedRunOutcome, RunnerError> {
        self.recorded.lock().unwrap().push(spec.clone());
        self.supervised_responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected supervised invocation without scripted response")
    }
}

fn default_policy() -> SandboxPolicy {
    SandboxPolicy::hardened_default(TEST_IMAGE_REF).unwrap()
}

fn setup_dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let skill = temp.path().join("skill");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(skill.join("engine/tools")).unwrap();
    fs::write(
        skill.join("engine/tools/render.js"),
        "// render entrypoint\n",
    )
    .unwrap();
    (temp, workspace, skill)
}

fn default_request(workspace: &Path, skill: &Path) -> SandboxRequest {
    SandboxRequest {
        job_id: "11111111-2222-4333-8444-555555555555".into(),
        invocation_id: "step-01".into(),
        workspace_dir: workspace.to_path_buf(),
        read_only_mounts: vec![ReadOnlyMount {
            host_path: skill.to_path_buf(),
            container_path: "/skill".into(),
        }],
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

#[test]
fn policy_validation_hashing_and_strict_schema_reject_weak_or_unknown_fields() {
    let p1 = default_policy();
    let h1 = p1.policy_sha256().unwrap();
    assert_eq!(h1.len(), 64);

    let mut p2 = p1.clone();
    assert_eq!(p2.policy_sha256().unwrap(), h1);
    p2.limits.memory_mb = 1024;
    assert_ne!(p2.policy_sha256().unwrap(), h1);

    let json_val = serde_json::to_value(&p1).unwrap();
    let mut extra_field = json_val.clone();
    extra_field["privileged"] = serde_json::json!(false);
    assert!(serde_json::from_value::<SandboxPolicy>(extra_field).is_err());

    let mut bad_policies = Vec::new();
    let mut p = p1.clone();
    p.schema_version = "2".into();
    bad_policies.push(p);

    let mut p = p1.clone();
    p.image.reference = "ghcr.io/scenivox/sandbox:latest".into();
    bad_policies.push(p);

    let mut p = p1.clone();
    p.image.reference = "-ghcr.io/scenivox/sandbox@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into();
    bad_policies.push(p);

    let mut p = p1.clone();
    p.network_disabled = false;
    bad_policies.push(p);

    let mut p = p1.clone();
    p.pull_never = false;
    bad_policies.push(p);

    let mut p = p1.clone();
    p.read_only_rootfs = false;
    bad_policies.push(p);

    let mut p = p1.clone();
    p.drop_all_capabilities = false;
    bad_policies.push(p);

    let mut p = p1.clone();
    p.no_new_privileges = false;
    bad_policies.push(p);

    let mut p = p1.clone();
    p.require_rootless = false;
    bad_policies.push(p);

    let mut p = p1.clone();
    p.uid = 0;
    bad_policies.push(p);

    let mut p = p1.clone();
    p.gid = 0;
    bad_policies.push(p);

    let mut p = p1.clone();
    p.limits.cpus = f64::NAN;
    bad_policies.push(p);

    let mut p = p1.clone();
    p.limits.memory_mb = 16;
    bad_policies.push(p);

    let mut p = p1.clone();
    p.limits.pids_limit = 2;
    bad_policies.push(p);

    let mut p = p1.clone();
    p.limits.tmpfs_mb = 0;
    bad_policies.push(p);

    let mut p = p1.clone();
    p.limits.timeout_secs = 0;
    bad_policies.push(p);

    let mut p = p1.clone();
    p.limits.max_total_output_bytes = p.limits.workspace_max_bytes + 1;
    bad_policies.push(p);

    for invalid in bad_policies {
        assert!(matches!(
            invalid.validate(),
            Err(SandboxError::InvalidPolicy(_))
        ));
        assert!(matches!(
            invalid.policy_sha256(),
            Err(SandboxError::InvalidPolicy(_))
        ));
    }
}

fn push_current_platform_podman_probe(runner: &ScriptedCommandRunner) {
    if HostPlatform::current() == HostPlatform::Windows {
        let wsl_utf16 = utf16le_bytes(
            "  NAME              STATE           VERSION\r\n* Ubuntu            Running         2\r\n",
        );
        runner.push_probe(&wsl_utf16, b"", Some(0));
    }
    runner.push_probe(b"true|5.4.1\n", b"", Some(0));
}

fn oci_run_args(spec: &CommandSpec) -> &[String] {
    if spec.program == "wsl.exe" && spec.args.len() >= 4 && spec.args[2] == "--" {
        &spec.args[4..]
    } else {
        &spec.args
    }
}

#[test]
fn mock_runner_current_platform_rootless_detection_image_verify_and_hardened_argv() {
    let (_temp, workspace, skill) = setup_dirs();
    let policy = default_policy();
    let request = default_request(&workspace, &skill);

    let runner = Arc::new(ScriptedCommandRunner::default());
    // 1. probe() -> current platform (native Linux or Windows WSL2) rootless podman
    push_current_platform_podman_probe(&runner);
    // 2. image inspect -> matches pinned digest
    runner.push_probe(
        format!("{TEST_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{TEST_IMAGE_DIGEST}\"]\n")
            .as_bytes(),
        b"",
        Some(0),
    );
    // 3. supervised container run -> succeeds
    runner.push_supervised(SupervisedRunOutcome::Exited(ProcessOutput {
        exit_code: Some(0),
        stdout: b"rendered ok\n".to_vec(),
        stderr: Vec::new(),
    }));
    // 4. force cleanup rm -f -> already removed (--rm)
    runner.push_probe(b"", b"no such container\n", Some(1));

    let backend = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig::default(),
        Arc::clone(&runner),
    );
    let result = backend.execute(&policy, &request).unwrap();

    assert_eq!(result.runtime.engine, OciEngine::Podman);
    let expected_transport = if HostPlatform::current() == HostPlatform::Windows {
        OciTransport::Wsl2 {
            distro: "Ubuntu".into(),
        }
    } else {
        OciTransport::NativeLinux
    };
    assert_eq!(result.runtime.transport, expected_transport);
    assert!(result.runtime.rootless_verified);
    assert_eq!(result.runtime.engine_version, "5.4.1");
    assert_eq!(result.image_digest, TEST_IMAGE_DIGEST);
    assert_eq!(
        result.runtime_policy_sha256,
        policy.policy_sha256().unwrap()
    );
    assert_eq!(result.stdout, "rendered ok\n");
    assert!(result.cleaned_up);

    let commands = runner.commands();
    assert!(
        !commands
            .iter()
            .any(|cmd| cmd.args.iter().any(|a| a == "pull")),
        "auto-pull must never be invoked"
    );

    let run_cmd = commands
        .iter()
        .find(|cmd| oci_run_args(cmd).first().map(String::as_str) == Some("run"))
        .expect("run command missing");
    let run_args = oci_run_args(run_cmd);
    if HostPlatform::current() == HostPlatform::Windows {
        assert_eq!(run_cmd.program, "wsl.exe");
        assert_eq!(&run_cmd.args[..4], &["-d", "Ubuntu", "--", "podman"]);
    } else {
        assert_eq!(run_cmd.program, "podman");
    }

    for required_flag in [
        "--rm",
        "--pull=never",
        "--network=none",
        "--read-only",
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
        "--user=65532:65532",
        "--cpus=2",
        "--memory=2048m",
        "--memory-swap=2048m",
        "--pids-limit=128",
        "--tmpfs=/tmp:rw,noexec,nosuid,nodev,size=256m",
        "--workdir=/workspace",
        "--entrypoint=node",
        "scenivox.managed=true",
        "scenivox.job_id=11111111-2222-4333-8444-555555555555",
        TEST_IMAGE_REF,
        "/skill/engine/tools/render.js",
        "video",
        "final.mp4",
    ] {
        assert!(
            run_args.iter().any(|a| a == required_flag),
            "missing flag '{required_flag}' in {:?}",
            run_args
        );
    }
    assert!(
        run_args
            .iter()
            .any(|a| a.starts_with("--mount=type=bind,src=") && a.ends_with(",dst=/workspace"))
    );
    assert!(run_args
        .iter()
        .any(|a| a.starts_with("--mount=type=bind,src=") && a.ends_with(",dst=/skill,readonly")));
    assert!(!run_args.iter().any(|a| a == "--privileged"));
    assert!(!run_args.iter().any(|a| a == "sh" || a == "-c"));
}

#[test]
fn build_oci_run_args_supports_native_linux_posix_mounts() {
    let policy = default_policy();
    let policy_hash = policy.policy_sha256().unwrap();
    let request = default_request(
        Path::new("/tmp/scenivox/ws"),
        Path::new("/tmp/scenivox/skill"),
    );
    let args = build_oci_run_args(
        &policy,
        &request,
        &policy_hash,
        "scenivox-job-step1",
        &OciTransport::NativeLinux,
        Path::new("/tmp/scenivox/ws"),
        &[(PathBuf::from("/tmp/scenivox/skill"), "/skill".into())],
        "node",
        &[
            "/skill/engine/tools/render.js".into(),
            "video".into(),
            "final.mp4".into(),
        ],
    )
    .unwrap();
    assert!(
        args.iter()
            .any(|a| a == "--mount=type=bind,src=/tmp/scenivox/ws,dst=/workspace")
    );
    assert!(
        args.iter()
            .any(|a| a == "--mount=type=bind,src=/tmp/scenivox/skill,dst=/skill,readonly")
    );
}

#[test]
fn mock_runner_rejects_rootful_runtimes_and_accepts_rootless_docker_only() {
    // Both podman (rootless=false) and docker (missing name=rootless) fail closed.
    let runner_rootful = ScriptedCommandRunner::default();
    runner_rootful.push_probe(b"false|5.4.1\n", b"", Some(0));
    runner_rootful.push_probe(b"[\"name=seccomp,profile=builtin\"]|28.3.3\n", b"", Some(0));
    let backend = OciSandboxBackend::with_runner(
        HostPlatform::Linux,
        OciBackendConfig::default(),
        runner_rootful,
    );
    let err = backend.probe().unwrap_err();
    assert_eq!(err.code(), "sandbox_unavailable");

    // Podman absent, Docker reports name=rootless -> verified.
    let runner_docker = ScriptedCommandRunner::default();
    runner_docker.push_probe_err(RunnerError::NotFound("podman".into()));
    runner_docker.push_probe(
        b"[\"name=seccomp,profile=builtin\",\"name=rootless\"]|28.3.3\n",
        b"",
        Some(0),
    );
    let backend = OciSandboxBackend::with_runner(
        HostPlatform::Linux,
        OciBackendConfig::default(),
        runner_docker,
    );
    let rt = backend.probe().unwrap();
    assert_eq!(rt.engine, OciEngine::Docker);
    assert!(rt.rootless_verified);
}

#[test]
fn mock_runner_windows_wsl2_detection_and_path_translation() {
    let wsl_utf16 = utf16le_bytes(
        "  NAME              STATE           VERSION\r\n  docker-desktop    Stopped         2\r\n* Ubuntu            Stopped         2\r\n",
    );
    let runner = Arc::new(ScriptedCommandRunner::default());
    runner.push_probe(&wsl_utf16, b"", Some(0));
    runner.push_probe(b"true|4.9.3\n", b"", Some(0));

    let backend = OciSandboxBackend::with_runner(
        HostPlatform::Windows,
        OciBackendConfig::default(),
        Arc::clone(&runner),
    );
    let rt = backend.probe().unwrap();
    assert_eq!(rt.engine, OciEngine::Podman);
    assert_eq!(
        rt.transport,
        OciTransport::Wsl2 {
            distro: "Ubuntu".into()
        }
    );
    let commands = runner.commands();
    assert_eq!(commands[0].program, "wsl.exe");
    assert_eq!(
        commands[0].env.get("WSL_UTF8").map(String::as_str),
        Some("1")
    );
    assert_eq!(commands[1].program, "wsl.exe");
    assert_eq!(&commands[1].args[..4], &["-d", "Ubuntu", "--", "podman"]);

    // WSL1-only distro fails closed.
    let wsl1_only = utf16le_bytes("  NAME STATE VERSION\n* Legacy Stopped 1\n");
    let runner_wsl1 = ScriptedCommandRunner::default();
    runner_wsl1.push_probe(&wsl1_only, b"", Some(0));
    let backend_wsl1 = OciSandboxBackend::with_runner(
        HostPlatform::Windows,
        OciBackendConfig::default(),
        runner_wsl1,
    );
    assert_eq!(
        backend_wsl1.probe().unwrap_err().code(),
        "sandbox_unavailable"
    );

    // Windows canonical path translation for WSL2 vs UNC rejection.
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
}

#[test]
fn mock_runner_missing_or_mismatched_local_image_and_unavailable_backend_fail_closed() {
    let policy = default_policy();
    let (_temp, workspace, skill) = setup_dirs();
    let req = default_request(&workspace, &skill);

    // Missing image locally.
    let runner_missing = Arc::new(ScriptedCommandRunner::default());
    runner_missing.push_probe(b"true|5.4.1\n", b"", Some(0));
    runner_missing.push_probe(b"", b"Error: no such image\n", Some(125));
    let backend_missing = OciSandboxBackend::with_runner(
        HostPlatform::Linux,
        OciBackendConfig::default(),
        Arc::clone(&runner_missing),
    );
    assert_eq!(
        backend_missing
            .verify_policy_image(&policy)
            .unwrap_err()
            .code(),
        "sandbox_unavailable"
    );
    assert!(
        !runner_missing
            .commands()
            .iter()
            .any(|c| c.args.iter().any(|a| a == "pull"))
    );

    // Digest mismatch locally.
    let runner_mismatch = ScriptedCommandRunner::default();
    runner_mismatch.push_probe(b"true|5.4.1\n", b"", Some(0));
    runner_mismatch.push_probe(
        format!("{OTHER_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{OTHER_IMAGE_DIGEST}\"]\n")
            .as_bytes(),
        b"",
        Some(0),
    );
    let backend_mismatch = OciSandboxBackend::with_runner(
        HostPlatform::Linux,
        OciBackendConfig::default(),
        runner_mismatch,
    );
    assert_eq!(
        backend_mismatch
            .verify_policy_image(&policy)
            .unwrap_err()
            .code(),
        "sandbox_unavailable"
    );

    // Explicit UnavailableSandboxBackend.
    let unavailable = UnavailableSandboxBackend::new("no rootless OCI runtime configured");
    assert_eq!(
        unavailable.probe().unwrap_err().code(),
        "sandbox_unavailable"
    );
    assert_eq!(
        unavailable.verify_policy_image(&policy).unwrap_err().code(),
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

    // Unsupported host OS.
    let backend_unsupported = OciSandboxBackend::with_runner(
        HostPlatform::Unsupported,
        OciBackendConfig::default(),
        ScriptedCommandRunner::default(),
    );
    assert_eq!(
        backend_unsupported.probe().unwrap_err().code(),
        "sandbox_unavailable"
    );
}

#[test]
fn rejects_unsafe_mounts_sensitive_files_and_symlinks() {
    let (_temp, workspace, skill) = setup_dirs();
    let policy = default_policy();
    let backend = OciSandboxBackend::with_runner(
        HostPlatform::Linux,
        OciBackendConfig::default(),
        ScriptedCommandRunner::default(),
    );

    // .env inside workspace is rejected before touching any runtime.
    fs::write(workspace.join(".env"), "OPENAI_API_KEY=secret").unwrap();
    let req = default_request(&workspace, &skill);
    assert_eq!(
        backend.execute(&policy, &req).unwrap_err().code(),
        "invalid_sandbox_mount"
    );
    fs::remove_file(workspace.join(".env")).unwrap();

    // scenivox.db mounted read-only is rejected.
    let temp2 = tempfile::tempdir().unwrap();
    let db_file = temp2.path().join("scenivox.db");
    fs::write(&db_file, b"sqlite").unwrap();
    let mut bad_req = default_request(&workspace, &skill);
    bad_req.read_only_mounts.push(ReadOnlyMount {
        host_path: db_file,
        container_path: "/inputs/db".into(),
    });
    assert_eq!(
        backend.execute(&policy, &bad_req).unwrap_err().code(),
        "invalid_sandbox_mount"
    );

    // .ssh directory or docker.sock path component is rejected.
    let ssh_dir = temp2.path().join(".ssh");
    fs::create_dir(&ssh_dir).unwrap();
    let mut bad_req = default_request(&workspace, &skill);
    bad_req.read_only_mounts.push(ReadOnlyMount {
        host_path: ssh_dir,
        container_path: "/inputs/ssh".into(),
    });
    assert_eq!(
        backend.execute(&policy, &bad_req).unwrap_err().code(),
        "invalid_sandbox_mount"
    );

    let sock_file = temp2.path().join("docker.sock");
    fs::write(&sock_file, b"").unwrap();
    let mut bad_req = default_request(&workspace, &skill);
    bad_req.read_only_mounts.push(ReadOnlyMount {
        host_path: sock_file,
        container_path: "/inputs/sock".into(),
    });
    assert_eq!(
        backend.execute(&policy, &bad_req).unwrap_err().code(),
        "invalid_sandbox_mount"
    );

    // Bad container paths (/etc/passwd, /workspace, traversal, delimiter injection).
    for target in [
        "/etc/passwd",
        "/workspace",
        "/inputs/../etc/passwd",
        "/inputs/a,readonly",
        "/inputs/a:ro",
    ] {
        let mut bad_req = default_request(&workspace, &skill);
        bad_req.read_only_mounts.push(ReadOnlyMount {
            host_path: temp2.path().to_path_buf(),
            container_path: target.into(),
        });
        assert_eq!(
            backend.execute(&policy, &bad_req).unwrap_err().code(),
            "invalid_sandbox_mount",
            "expected rejection for container target '{target}'"
        );
    }

    // Overlapping workspace & read-only host path is rejected.
    let mut overlap_req = default_request(&workspace, &skill);
    overlap_req.read_only_mounts = vec![ReadOnlyMount {
        host_path: workspace.clone(),
        container_path: "/inputs/ws".into(),
    }];
    assert_eq!(
        backend.execute(&policy, &overlap_req).unwrap_err().code(),
        "invalid_sandbox_mount"
    );

    // Symlink inside skill read-only mount escaping to outside file is rejected.
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("host_secret.txt");
    fs::write(&secret, b"top-secret").unwrap();
    let escape_link = skill.join("escape.txt");
    if create_symlink_file(&secret, &escape_link) {
        let req = default_request(&workspace, &skill);
        assert_eq!(
            backend.execute(&policy, &req).unwrap_err().code(),
            "invalid_sandbox_mount"
        );
        let _ = fs::remove_file(&escape_link);
    }
}

#[test]
fn rejects_forbidden_execution_options_shells_and_secret_env() {
    let (_temp, workspace, skill) = setup_dirs();
    let policy = default_policy();
    let backend = OciSandboxBackend::with_runner(
        HostPlatform::Linux,
        OciBackendConfig::default(),
        ScriptedCommandRunner::default(),
    );

    let bad_commands = [
        SandboxCommand::Script {
            runtime: ScriptRuntime::Node,
            root: ScriptMountRoot::Skill,
            script_rel_path: "../outside.js".into(),
            args: vec![],
        },
        SandboxCommand::Script {
            runtime: ScriptRuntime::Node,
            root: ScriptMountRoot::Skill,
            script_rel_path: "engine/tools/render.py".into(),
            args: vec![],
        },
        SandboxCommand::Script {
            runtime: ScriptRuntime::Python3,
            root: ScriptMountRoot::Workspace,
            script_rel_path: "-c".into(),
            args: vec![],
        },
        SandboxCommand::Script {
            runtime: ScriptRuntime::Node,
            root: ScriptMountRoot::Skill,
            script_rel_path: "engine/tools/render.js".into(),
            args: vec!["/bin/sh".into()],
        },
        SandboxCommand::MediaTool {
            tool: MediaTool::Ffprobe,
            args: vec!["--privileged".into()],
        },
        SandboxCommand::MediaTool {
            tool: MediaTool::Ffmpeg,
            args: vec!["--network=host".into()],
        },
        SandboxCommand::MediaTool {
            tool: MediaTool::Ffmpeg,
            args: vec!["--cap-add=SYS_ADMIN".into()],
        },
    ];

    for cmd in bad_commands {
        let mut req = default_request(&workspace, &skill);
        req.command = cmd;
        assert_eq!(
            backend.execute(&policy, &req).unwrap_err().code(),
            "invalid_sandbox_invocation"
        );
    }

    for env_key in [
        "OPENAI_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "AWS_SECRET_ACCESS_KEY",
        "NODE_OPTIONS",
        "LD_PRELOAD",
        "PATH",
    ] {
        let mut req = default_request(&workspace, &skill);
        req.env.insert(env_key.into(), "val".into());
        assert_eq!(
            backend.execute(&policy, &req).unwrap_err().code(),
            "invalid_sandbox_invocation"
        );
    }

    let mut bad_job = default_request(&workspace, &skill);
    bad_job.job_id = "../bad-job".into();
    assert_eq!(
        backend.execute(&policy, &bad_job).unwrap_err().code(),
        "invalid_sandbox_invocation"
    );
}

#[test]
fn mock_runner_enforces_timeout_output_limits_and_cleanup() {
    let (_temp, workspace, skill) = setup_dirs();
    let policy = default_policy();
    let request = default_request(&workspace, &skill);

    // 1. Timeout forces `rm -f` cleanup.
    let runner_to = Arc::new(ScriptedCommandRunner::default());
    push_current_platform_podman_probe(&runner_to);
    runner_to.push_probe(
        format!("{TEST_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{TEST_IMAGE_DIGEST}\"]\n")
            .as_bytes(),
        b"",
        Some(0),
    );
    runner_to.push_supervised(SupervisedRunOutcome::TimedOut);
    runner_to.push_probe(b"cleaned\n", b"", Some(0));
    let backend_to = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig::default(),
        Arc::clone(&runner_to),
    );
    let err = backend_to.execute(&policy, &request).unwrap_err();
    assert!(matches!(
        err,
        SandboxError::Timeout {
            cleaned_up: true,
            ..
        }
    ));
    let cmds_to = runner_to.commands();
    let rm_cmd = cmds_to.last().expect("cleanup rm missing");
    assert_eq!(&oci_run_args(rm_cmd)[..2], &["rm", "-f"]);

    // 2. Output limit exceeded forces `rm -f` cleanup.
    let runner_out = Arc::new(ScriptedCommandRunner::default());
    push_current_platform_podman_probe(&runner_out);
    runner_out.push_probe(
        format!("{TEST_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{TEST_IMAGE_DIGEST}\"]\n")
            .as_bytes(),
        b"",
        Some(0),
    );
    runner_out.push_supervised(SupervisedRunOutcome::OutputLimitExceeded {
        stream: "stdout",
        limit_bytes: policy.limits.max_stdout_bytes,
    });
    runner_out.push_probe(b"cleaned\n", b"", Some(0));
    let backend_out = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig::default(),
        Arc::clone(&runner_out),
    );
    let err = backend_out.execute(&policy, &request).unwrap_err();
    assert!(matches!(
        err,
        SandboxError::OutputLimitExceeded {
            stream: "stdout",
            cleaned_up: true,
            ..
        }
    ));
    let cmds_out = runner_out.commands();
    let rm_cmd_out = cmds_out.last().unwrap();
    assert_eq!(&oci_run_args(rm_cmd_out)[..2], &["rm", "-f"]);

    // 3. Non-zero exit code & spawn failure also clean up and fail closed.
    let runner_exit = Arc::new(ScriptedCommandRunner::default());
    push_current_platform_podman_probe(&runner_exit);
    runner_exit.push_probe(
        format!("{TEST_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{TEST_IMAGE_DIGEST}\"]\n")
            .as_bytes(),
        b"",
        Some(0),
    );
    runner_exit.push_supervised(SupervisedRunOutcome::Exited(ProcessOutput {
        exit_code: Some(42),
        stdout: Vec::new(),
        stderr: b"boom".to_vec(),
    }));
    runner_exit.push_probe(b"", b"", Some(0));
    let backend_exit = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig::default(),
        runner_exit,
    );
    assert!(matches!(
        backend_exit.execute(&policy, &request),
        Err(SandboxError::ExecutionFailed {
            exit_code: Some(42),
            cleaned_up: true,
            ..
        })
    ));

    let runner_spawn_err = Arc::new(ScriptedCommandRunner::default());
    push_current_platform_podman_probe(&runner_spawn_err);
    runner_spawn_err.push_probe(
        format!("{TEST_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{TEST_IMAGE_DIGEST}\"]\n")
            .as_bytes(),
        b"",
        Some(0),
    );
    runner_spawn_err.push_supervised_err(RunnerError::NotFound("podman".into()));
    runner_spawn_err.push_probe(b"", b"no such container", Some(1));
    let backend_spawn_err = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig::default(),
        runner_spawn_err,
    );
    assert_eq!(
        backend_spawn_err
            .execute(&policy, &request)
            .unwrap_err()
            .code(),
        "sandbox_unavailable"
    );

    // 4. cleanup_job sweeps all labeled containers for the job.
    let runner_sweep = ScriptedCommandRunner::default();
    push_current_platform_podman_probe(&runner_sweep);
    runner_sweep.push_probe(b"scenivox-job1-a\nscenivox-job1-b\n", b"", Some(0));
    runner_sweep.push_probe(b"", b"", Some(0));
    runner_sweep.push_probe(b"", b"", Some(0));
    let backend_sweep = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig::default(),
        runner_sweep,
    );
    assert_eq!(backend_sweep.cleanup_job("job1").unwrap(), 2);

    // 5. Post-run workspace file count exceeded rejects result.
    let mut small_policy = default_policy();
    small_policy.limits.max_output_files = 1;
    fs::write(workspace.join("a.txt"), b"1").unwrap();
    fs::write(workspace.join("b.txt"), b"2").unwrap();
    let runner_disk = ScriptedCommandRunner::default();
    push_current_platform_podman_probe(&runner_disk);
    runner_disk.push_probe(
        format!("{TEST_IMAGE_DIGEST}|[\"ghcr.io/scenivox/sandbox@{TEST_IMAGE_DIGEST}\"]\n")
            .as_bytes(),
        b"",
        Some(0),
    );
    runner_disk.push_supervised(SupervisedRunOutcome::Exited(ProcessOutput {
        exit_code: Some(0),
        stdout: Vec::new(),
        stderr: Vec::new(),
    }));
    runner_disk.push_probe(b"", b"", Some(0));
    let backend_disk = OciSandboxBackend::with_runner(
        HostPlatform::current(),
        OciBackendConfig::default(),
        runner_disk,
    );
    assert_eq!(
        backend_disk
            .execute(&small_policy, &request)
            .unwrap_err()
            .code(),
        "sandbox_disk_limit_exceeded"
    );
}

#[test]
fn system_command_runner_real_process_timeout_and_output_limit_cleanup() {
    // Tests host OS process-tree termination & bounded pipe reading using benign OS tools.
    // Note: this verifies OS child process supervision only, NOT real Linux OCI container isolation.
    let runner = SystemCommandRunner;

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

    let start = Instant::now();
    let outcome = runner
        .run_supervised(&sleep_spec, Duration::from_millis(200), 4096, 4096)
        .unwrap();
    assert_eq!(outcome, SupervisedRunOutcome::TimedOut);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "timeout cleanup took too long: {:?}",
        start.elapsed()
    );

    #[cfg(unix)]
    let noisy_spec = CommandSpec {
        program: "head".into(),
        args: vec!["-c".into(), "32768".into(), "/dev/zero".into()],
        env: BTreeMap::new(),
    };
    #[cfg(windows)]
    let noisy_spec = CommandSpec {
        program: "powershell.exe".into(),
        args: vec![
            "-NoProfile".into(),
            "-Command".into(),
            "[Console]::Write('A' * 32768)".into(),
        ],
        env: BTreeMap::new(),
    };

    let outcome = runner
        .run_supervised(&noisy_spec, Duration::from_secs(10), 512, 512)
        .unwrap();
    assert_eq!(
        outcome,
        SupervisedRunOutcome::OutputLimitExceeded {
            stream: "stdout",
            limit_bytes: 512
        }
    );
}

#[test]
fn host_oci_probe_fails_closed_when_unavailable_and_never_falls_back() {
    let backend = OciSandboxBackend::system(OciBackendConfig::default());
    match backend.probe() {
        Ok(runtime) => {
            assert!(
                runtime.rootless_verified,
                "probe must never return an unverified or rootful runtime"
            );
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
