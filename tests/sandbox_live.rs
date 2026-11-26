use std::{
    fs,
    process::Command,
    time::{Duration, Instant},
};

use scenivox::sandbox::{
    ArtifactTransferRequest, ControlEvidenceSource, ExpectedArtifact, OciBackendConfig,
    OciSandboxBackend, PinnedDockerRuntime, ReadOnlyMount, SCENIVOX_CHROMIUM_SECCOMP_SHA256,
    SandboxBackend, SandboxCommand, SandboxError, SandboxPolicy, SandboxRequest, ScriptMountRoot,
    ScriptRuntime, TrustedStagingRoots,
};

fn live_image_ref() -> Option<String> {
    std::env::var("SCENIVOX_LIVE_SANDBOX_IMAGE")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn live_docker_image_ref() -> Option<String> {
    std::env::var("SCENIVOX_LIVE_DOCKER_IMAGE")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn live_wsl_distro() -> String {
    std::env::var("SCENIVOX_LIVE_WSL_DISTRO")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Ubuntu".into())
}

fn podman_run_hardened(
    distro: &str,
    image_ref: &str,
    skill_dir: Option<&std::path::Path>,
    entrypoint: &str,
    args: &[&str],
) -> std::process::Output {
    let mut podman_args = vec![
        "run".to_owned(),
        "--rm".to_owned(),
        "--pull=never".to_owned(),
        "--network=none".to_owned(),
        "--read-only".to_owned(),
        "--cap-drop=ALL".to_owned(),
        "--security-opt=no-new-privileges".to_owned(),
        "--user=65532:65532".to_owned(),
        "--cpus=2".to_owned(),
        "--memory=1024m".to_owned(),
        "--memory-swap=1024m".to_owned(),
        "--pids-limit=256".to_owned(),
        "--tmpfs=/tmp:rw,noexec,nosuid,nodev,size=134217728,mode=1777".to_owned(),
        "--tmpfs=/workspace:rw,nosuid,nodev,size=134217728,mode=1777".to_owned(),
    ];
    if let Some(dir) = skill_dir {
        let raw = dir.to_str().unwrap();
        let trimmed = raw.strip_prefix(r"\\?\").unwrap_or(raw);
        let linux_src = if cfg!(windows) {
            let drive = trimmed.chars().next().unwrap().to_ascii_lowercase();
            let rest = trimmed[3..].replace('\\', "/");
            format!("/mnt/{drive}/{rest}")
        } else {
            trimmed.to_owned()
        };
        podman_args.push(format!(
            "--mount=type=bind,src={linux_src},dst=/skill,readonly"
        ));
    }
    podman_args.push(format!("--entrypoint={entrypoint}"));
    podman_args.push(image_ref.to_owned());
    for a in args {
        podman_args.push((*a).to_owned());
    }

    if cfg!(windows) {
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--exec")
            .arg("podman")
            .args(&podman_args)
            .output()
            .unwrap()
    } else {
        Command::new("podman").args(&podman_args).output().unwrap()
    }
}

#[test]
#[ignore = "requires provisioned rootless Podman runtime (set SCENIVOX_LIVE_SANDBOX_IMAGE)"]
fn step1_verifies_fail_closed_inode_blocker_chromium_sandbox_and_extractor_regressions() {
    let image_ref = live_image_ref().expect("SCENIVOX_LIVE_SANDBOX_IMAGE must be set");
    let distro = live_wsl_distro();

    // 1. Fail-closed readiness check: Podman 4.9.3 does not support tmpfs `nr_inodes`
    //    (and WSL2 Docker is rootful), so detect_runtime & verify_readiness MUST fail closed (`sandbox_unavailable`).
    let backend = OciSandboxBackend::system(OciBackendConfig {
        preferred_wsl_distro: Some(distro.clone()),
        trusted_roots: TrustedStagingRoots::default(),
    });
    let policy = SandboxPolicy::hardened_default(&image_ref).unwrap();
    let detect_err = backend.detect_runtime().unwrap_err();
    assert!(matches!(detect_err, SandboxError::SandboxUnavailable(_)));
    assert_eq!(detect_err.code(), "sandbox_unavailable");
    assert!(
        detect_err.to_string().contains("nr_inodes") || detect_err.to_string().contains("rootless"),
        "unexpected detect_err: {detect_err}"
    );

    let readiness_err = backend.verify_readiness(&policy).unwrap_err();
    assert!(matches!(readiness_err, SandboxError::SandboxUnavailable(_)));
    assert_eq!(readiness_err.code(), "sandbox_unavailable");
    assert!(
        readiness_err.to_string().contains("nr_inodes")
            || readiness_err.to_string().contains("rootless"),
        "unexpected readiness_err: {readiness_err}"
    );
    println!("LIVE_INODE_FAIL_CLOSED={readiness_err}");

    let temp = tempfile::tempdir().unwrap();
    let skill_dir = temp.path().join("skill");
    fs::create_dir_all(&skill_dir).unwrap();

    // 2. Detached-process regression: a child spawns an `os.setsid()` background writer
    //    trying to overwrite `/workspace/out.txt` after the child exits 0.
    //    `artifact_runner.py` must kill all PID > 1 processes in the container PID namespace
    //    before hashing/extracting `/workspace/out.txt`.
    fs::write(
        skill_dir.join("detached_tamper.py"),
        r###"import os, subprocess, sys
with open("/workspace/out.txt", "wb") as f:
    f.write(b"CLEAN")
subprocess.Popen(
    [
        sys.executable,
        "-c",
        "import time\ntime.sleep(0.05)\nwith open('/workspace/out.txt', 'wb') as f:\n    f.write(b'TAMPERED')\n",
    ],
    start_new_session=True,
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)
"###,
    )
    .unwrap();

    let detached_out = podman_run_hardened(
        &distro,
        &image_ref,
        Some(&skill_dir),
        "python3",
        &[
            "/opt/scenivox-runtime/artifact_runner.py",
            "--max-stdout",
            "65536",
            "--max-stderr",
            "65536",
            "--max-total",
            "65536",
            "--artifact",
            "out.txt:1024",
            "--",
            "python3",
            "/skill/detached_tamper.py",
        ],
    );
    assert!(
        detached_out.status.success(),
        "detached_tamper runner failed: stderr={}",
        String::from_utf8_lossy(&detached_out.stderr)
    );
    // Verify frame contains `CLEAN` and NOT `TAMPERED`.
    assert!(detached_out.stdout.starts_with(b"SVXART01"));
    assert!(
        detached_out.stdout.windows(5).any(|w| w == b"CLEAN"),
        "expected CLEAN payload in artifact stream"
    );
    assert!(
        !detached_out.stdout.windows(8).any(|w| w == b"TAMPERED"),
        "detached descendant modified artifact before extraction!"
    );

    // 3. FIFO extraction regression: FIFO at `/workspace/fifo.mp4` must fail immediately
    //    via O_NONBLOCK + fstat(!S_ISREG) instead of hanging open().
    fs::write(
        skill_dir.join("fifo_trap.py"),
        "import os\nos.mkfifo('/workspace/fifo.mp4')\n",
    )
    .unwrap();
    let fifo_started = Instant::now();
    let fifo_out = podman_run_hardened(
        &distro,
        &image_ref,
        Some(&skill_dir),
        "python3",
        &[
            "/opt/scenivox-runtime/artifact_runner.py",
            "--max-stdout",
            "65536",
            "--max-stderr",
            "65536",
            "--max-total",
            "65536",
            "--artifact",
            "fifo.mp4:1024",
            "--",
            "python3",
            "/skill/fifo_trap.py",
        ],
    );
    let fifo_elapsed = fifo_started.elapsed();
    assert!(
        !fifo_out.status.success(),
        "FIFO artifact unexpectedly succeeded"
    );
    assert!(
        fifo_elapsed < Duration::from_secs(25),
        "FIFO open blocked unexpectedly ({fifo_elapsed:?})"
    );
    let fifo_stderr = String::from_utf8_lossy(&fifo_out.stderr);
    assert!(
        fifo_stderr.contains("is not a regular file"),
        "unexpected FIFO stderr: {fifo_stderr}"
    );
    println!("LIVE_FIFO_REJECTED={}", fifo_stderr.trim());

    // 4. Chromium `chromiumSandbox: true` regression: verify `--no-sandbox` is omitted and
    //    record exact failure under `--cap-drop=ALL --security-opt=no-new-privileges`.
    fs::write(
        skill_dir.join("check_chromium_sandbox.js"),
        r###"
const { chromium } = require("playwright");
(async () => {
  try {
    const b = await chromium.launch({ headless: true, chromiumSandbox: true });
    await b.close();
    console.log("CHROMIUM_SANDBOX_TRUE_OK");
  } catch (err) {
    console.log("CHROMIUM_SANDBOX_TRUE_ERR:" + (err.message || err));
  }
})();
"###,
    )
    .unwrap();
    let chrome_out = podman_run_hardened(
        &distro,
        &image_ref,
        Some(&skill_dir),
        "node",
        &["/skill/check_chromium_sandbox.js"],
    );
    assert!(chrome_out.status.success());
    let chrome_stdout = String::from_utf8_lossy(&chrome_out.stdout);
    assert!(chrome_stdout.contains("<launching>"));
    assert!(!chrome_stdout.contains("--no-sandbox"));
    assert!(chrome_stdout.contains("CHROMIUM_SANDBOX_TRUE_ERR:"));
    assert!(chrome_stdout.contains("sys_chroot(\"/proc/self/fdinfo/\")"));
    println!("LIVE_CHROMIUM_SANDBOX_CHECK={}", chrome_stdout.trim());
}

#[test]
#[ignore = "requires provisioned dedicated scenivox-rootless Docker runtime (set SCENIVOX_LIVE_DOCKER_IMAGE)"]
fn strict_rootless_docker_verifies_pinning_controls_chromium_sandbox_timeout_detached_fifo_and_transfer()
 {
    let image_ref = live_docker_image_ref().expect("SCENIVOX_LIVE_DOCKER_IMAGE must be set");
    let distro = live_wsl_distro();
    let policy = SandboxPolicy::hardened_default(&image_ref).unwrap();
    let expected_digest = policy.image.digest().unwrap();
    let pin = PinnedDockerRuntime::scenivox_rootless(&expected_digest).unwrap();

    let temp = tempfile::tempdir().unwrap();
    let jobs_root = temp.path().join("jobs");
    let skills_root = temp.path().join("skills");
    let job_id = "11111111-2222-4333-8444-555555555555";
    let job_inputs = jobs_root.join(job_id).join("inputs");
    let out_dir = jobs_root.join(job_id).join("outputs");
    let skill_dir = skills_root.join("live-docker-skill");
    fs::create_dir_all(&job_inputs).unwrap();
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(job_inputs.join("input.txt"), b"frozen-input-ok").unwrap();

    let backend_cfg = OciBackendConfig {
        preferred_wsl_distro: Some(distro.clone()),
        trusted_roots: TrustedStagingRoots {
            jobs_root: Some(jobs_root.clone()),
            skills_root: Some(skills_root),
        },
    };
    let backend = OciSandboxBackend::system(backend_cfg.clone()).with_docker_pin(pin.clone());

    // 1. Metadata readiness check & explicit pin checks.
    let readiness = backend.verify_readiness(&policy).unwrap();
    assert_eq!(
        readiness.docker_context_name.as_deref(),
        Some("scenivox-rootless")
    );
    assert_eq!(
        readiness.seccomp_profile_sha256.as_deref(),
        Some(SCENIVOX_CHROMIUM_SECCOMP_SHA256)
    );
    assert!(readiness.mandatory_controls_verified);

    // Mismatched pin rejection against live engine:
    let mut wrong_sock_pin = pin.clone();
    wrong_sock_pin.unix_socket = "unix:///var/run/docker.sock".into();
    let wrong_sock_backend =
        OciSandboxBackend::system(backend_cfg.clone()).with_docker_pin(wrong_sock_pin);
    assert!(
        wrong_sock_backend
            .verify_readiness(&policy)
            .unwrap_err()
            .to_string()
            .contains("socket mismatch")
    );

    let mut wrong_seccomp_pin = pin.clone();
    wrong_seccomp_pin.seccomp_profile_sha256 = "0".repeat(64);
    let wrong_seccomp_backend =
        OciSandboxBackend::system(backend_cfg.clone()).with_docker_pin(wrong_seccomp_pin);
    assert!(
        wrong_seccomp_backend
            .verify_readiness(&policy)
            .unwrap_err()
            .to_string()
            .contains("seccomp profile SHA-256 mismatch")
    );

    // 2. Full empirical verification: cgroup v2 limits, network=none, ro rootfs, tmpfs inode exhaustion,
    //    AND Chromium `chromiumSandbox: true` renderer PID/seccomp isolation.
    let (empirical_iso, probe) = backend.verify_empirical_isolation(&policy, true).unwrap();
    assert_eq!(
        empirical_iso.controls_evidence_source,
        ControlEvidenceSource::EmpiricallyVerified
    );
    assert!(empirical_iso.empirically_verified);
    assert!(probe.inode_exhaustion_enforced);
    assert!(probe.chromium_no_sandbox_flag_absent);
    assert!(probe.chromium_zygote_active);
    assert!(probe.chromium_renderer_pid_ns_depth >= 2);
    assert_eq!(probe.chromium_renderer_seccomp_mode, 2);
    println!(
        "LIVE_DOCKER_EMPIRICAL_OK inodes_exhausted_at={}/{} renderer_ns_depth={} screenshot_bytes={}",
        probe.inode_exhaustion_at,
        probe.workspace_inodes_reported,
        probe.chromium_renderer_pid_ns_depth,
        probe.chromium_screenshot_bytes
    );

    // 3. Read-only /skill & /inputs mount + bounded artifact transfer + detached tamper prevention.
    fs::write(
        skill_dir.join("detached_clean.py"),
        r###"import os, subprocess, sys
with open("/inputs/job/input.txt", "rb") as inp:
    data = inp.read()
with open("/workspace/out.txt", "wb") as out:
    out.write(b"VERIFIED:" + data)
subprocess.Popen(
    [
        sys.executable,
        "-c",
        "import time\ntime.sleep(0.05)\nwith open('/workspace/out.txt', 'wb') as f:\n    f.write(b'TAMPERED')\n",
    ],
    start_new_session=True,
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)
"###,
    )
    .unwrap();
    let transfer_req = SandboxRequest {
        job_id: job_id.into(),
        invocation_id: "docker-transfer-1".into(),
        host_workspace_dir: None,
        read_only_mounts: vec![
            ReadOnlyMount {
                host_path: skill_dir.clone(),
                container_path: "/skill".into(),
            },
            ReadOnlyMount {
                host_path: job_inputs.clone(),
                container_path: "/inputs/job".into(),
            },
        ],
        command: SandboxCommand::Script {
            runtime: ScriptRuntime::Python3,
            root: ScriptMountRoot::Skill,
            script_rel_path: "detached_clean.py".into(),
            args: Vec::new(),
        },
        env: std::collections::BTreeMap::new(),
        artifact_transfer: Some(ArtifactTransferRequest {
            destination_dir: out_dir.clone(),
            artifacts: vec![ExpectedArtifact {
                relative_path: "out.txt".into(),
                max_bytes: 1024,
            }],
            max_total_bytes: 1024,
        }),
    };
    let transfer_ungated_err = backend.execute(&policy, &transfer_req).unwrap_err();
    assert!(
        transfer_ungated_err
            .to_string()
            .contains("cannot bypass strict empirical verification"),
        "pinned rootless docker must block un-gated execute(): {transfer_ungated_err}"
    );

    let strict_backend = OciSandboxBackend::system(backend_cfg.clone())
        .with_docker_pin(pin.clone())
        .with_strict_empirical_gate(true);

    // Stale/expired VerifiedIsolation must be rejected cleanly on live gate.
    let mut expired_iso = empirical_iso.clone();
    expired_iso.empirical_verified_at_unix_secs = Some(1);
    expired_iso.empirical_expires_at_unix_secs = Some(2);
    let stale_err = strict_backend
        .execute_with_verified_isolation(&policy, &expired_iso, &transfer_req)
        .unwrap_err();
    assert!(stale_err.to_string().contains("stale empirical"));

    let transfer_res = strict_backend
        .execute_with_verified_isolation(&policy, &empirical_iso, &transfer_req)
        .unwrap();
    assert!(transfer_res.cleanup_verified);
    assert_eq!(transfer_res.transferred_artifacts.len(), 1);
    assert_eq!(
        fs::read(out_dir.join("out.txt")).unwrap(),
        b"VERIFIED:frozen-input-ok"
    );

    // 4. FIFO extraction trap fails immediately without hanging.
    fs::write(
        skill_dir.join("fifo_trap.py"),
        "import os\nos.mkfifo('/workspace/fifo.mp4')\n",
    )
    .unwrap();
    let fifo_req = SandboxRequest {
        job_id: job_id.into(),
        invocation_id: "docker-fifo-1".into(),
        host_workspace_dir: None,
        read_only_mounts: vec![ReadOnlyMount {
            host_path: skill_dir.clone(),
            container_path: "/skill".into(),
        }],
        command: SandboxCommand::Script {
            runtime: ScriptRuntime::Python3,
            root: ScriptMountRoot::Skill,
            script_rel_path: "fifo_trap.py".into(),
            args: Vec::new(),
        },
        env: std::collections::BTreeMap::new(),
        artifact_transfer: Some(ArtifactTransferRequest {
            destination_dir: jobs_root.join(job_id).join("outputs-fifo"),
            artifacts: vec![ExpectedArtifact {
                relative_path: "fifo.mp4".into(),
                max_bytes: 1024,
            }],
            max_total_bytes: 1024,
        }),
    };
    let fifo_err = strict_backend
        .execute_with_verified_isolation(&policy, &empirical_iso, &fifo_req)
        .unwrap_err();
    assert!(matches!(fifo_err, SandboxError::ExecutionFailed { .. }));
    assert!(fifo_err.to_string().contains("is not a regular file"));

    // 5. Policy SHA binding rejects empirical evidence from another timeout policy, then fresh empirical check executes timeout cleanly.
    fs::write(
        skill_dir.join("sleep_long.py"),
        "import time\ntime.sleep(30)\n",
    )
    .unwrap();
    let mut short_policy = policy.clone();
    short_policy.limits.timeout_secs = 8;
    let timeout_req = SandboxRequest {
        job_id: job_id.into(),
        invocation_id: "docker-timeout-1".into(),
        host_workspace_dir: None,
        read_only_mounts: vec![ReadOnlyMount {
            host_path: skill_dir.clone(),
            container_path: "/skill".into(),
        }],
        command: SandboxCommand::Script {
            runtime: ScriptRuntime::Python3,
            root: ScriptMountRoot::Skill,
            script_rel_path: "sleep_long.py".into(),
            args: Vec::new(),
        },
        env: std::collections::BTreeMap::new(),
        artifact_transfer: None,
    };
    let policy_mismatch_err = strict_backend
        .execute_with_verified_isolation(&short_policy, &empirical_iso, &timeout_req)
        .unwrap_err();
    assert!(
        policy_mismatch_err
            .to_string()
            .contains("runtime_policy_sha256")
    );
    let (short_iso, _) = strict_backend
        .verify_empirical_isolation(&short_policy, false)
        .unwrap();
    let timeout_err = strict_backend
        .execute_with_verified_isolation(&short_policy, &short_iso, &timeout_req)
        .unwrap_err();
    assert!(matches!(timeout_err, SandboxError::Timeout { .. }));

    // Confirm zero leftover managed containers in scenivox-rootless AND rootful default untouched.
    let ps_out = Command::new("wsl.exe")
        .args([
            "-d",
            &distro,
            "--exec",
            "docker",
            "--context",
            "scenivox-rootless",
            "ps",
            "-a",
            "--filter",
            &format!("label=scenivox.job_id={job_id}"),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .unwrap();
    assert!(ps_out.status.success());
    assert!(
        String::from_utf8_lossy(&ps_out.stdout).trim().is_empty(),
        "stray rootless container left behind"
    );

    let rootful_out = Command::new("wsl.exe")
        .args([
            "-d",
            &distro,
            "--exec",
            "docker",
            "--context",
            "default",
            "info",
            "--format",
            "{{.DockerRootDir}}",
        ])
        .output()
        .unwrap();
    assert!(rootful_out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&rootful_out.stdout).trim(),
        "/var/lib/docker"
    );
}
