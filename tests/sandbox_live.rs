use std::{collections::BTreeMap, fs, process::Command};

use scenivox::sandbox::{
    ArtifactTransferRequest, ExpectedArtifact, MediaTool, OciBackendConfig, OciSandboxBackend,
    OciTransport, ReadOnlyMount, SandboxBackend, SandboxCommand, SandboxError, SandboxPolicy,
    SandboxRequest, ScriptMountRoot, ScriptRuntime, SystemCommandRunner, TrustedStagingRoots,
};

const LIVE_JOB_ID: &str = "22222222-3333-4444-8555-666666666666";

fn live_image_ref() -> Option<String> {
    std::env::var("SCENIVOX_LIVE_SANDBOX_IMAGE")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn live_backend(roots: TrustedStagingRoots) -> OciSandboxBackend<SystemCommandRunner> {
    OciSandboxBackend::system(OciBackendConfig {
        preferred_wsl_distro: std::env::var("SCENIVOX_LIVE_WSL_DISTRO").ok(),
        trusted_roots: roots,
    })
}

fn list_managed_containers(transport: &OciTransport, job_id: &str) -> Vec<String> {
    let args = [
        "ps",
        "-a",
        "--filter",
        "label=scenivox.managed=true",
        "--filter",
        &format!("label=scenivox.job_id={job_id}"),
        "--format",
        "{{.Names}}",
    ];
    let out = match transport {
        OciTransport::NativeLinux => Command::new("podman").args(args).output().unwrap(),
        OciTransport::Wsl2 { distro } => Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--exec")
            .arg("podman")
            .args(args)
            .output()
            .unwrap(),
    };
    assert!(out.status.success(), "podman ps failed: {:?}", out);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
#[ignore = "requires provisioned rootless Podman runtime (set SCENIVOX_LIVE_SANDBOX_IMAGE)"]
fn live_podman_isolation_limits_cleanup_chromium_and_artifact_transfer() {
    let image_ref = live_image_ref().expect("SCENIVOX_LIVE_SANDBOX_IMAGE must be set");
    let temp = tempfile::tempdir().unwrap();
    let jobs_root = temp.path().join("jobs");
    let skills_root = temp.path().join("skills");
    let job_dir = jobs_root.join(LIVE_JOB_ID);
    let skill_dir = skills_root.join("self-test-skill");
    let inputs_dir = job_dir.join("inputs");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::create_dir_all(&inputs_dir).unwrap();
    fs::write(inputs_dir.join("marker.txt"), b"trusted-input-123").unwrap();

    // Script 1: isolation + resource limits + chromium + ffmpeg test media generation into /workspace.
    fs::write(
        skill_dir.join("verify_sandbox.py"),
        r###"
import os, socket, subprocess, sys, json

report = {}

# 1. Network isolation (--network=none)
s = socket.socket()
try:
    s.settimeout(2)
    s.connect(("1.1.1.1", 80))
    raise AssertionError("network unexpectedly reachable")
except OSError as exc:
    report["net_errno"] = exc.errno

# 2. Read-only rootfs and read-only /skill and /inputs/job
for ro_target in ["/etc/scenivox-ro-test", "/skill/ro-test", "/inputs/job/ro-test"]:
    try:
        with open(ro_target, "w") as f:
            f.write("x")
        raise AssertionError(f"unexpected write to {ro_target}")
    except OSError as exc:
        report.setdefault("ro_errnos", []).append(exc.errno)

# 3. Cgroup v2 controls visible inside container
for key, path in [
    ("cpu_max", "/sys/fs/cgroup/cpu.max"),
    ("memory_max", "/sys/fs/cgroup/memory.max"),
    ("swap_max", "/sys/fs/cgroup/memory.swap.max"),
    ("pids_max", "/sys/fs/cgroup/pids.max"),
]:
    with open(path) as f:
        report[key] = f.read().strip()

# 4. Workspace tmpfs byte quota enforcement (ENOSPC)
try:
    with open("/workspace/quota_probe.bin", "wb") as f:
        for _ in range(32):
            f.write(b"Z" * (1024 * 1024))
    raise AssertionError("workspace tmpfs exceeded 8 MiB without ENOSPC")
except OSError as exc:
    report["tmpfs_enospc_errno"] = exc.errno
finally:
    if os.path.exists("/workspace/quota_probe.bin"):
        os.remove("/workspace/quota_probe.bin")

# 5. Inode measurement on /workspace
st_vfs = os.statvfs("/workspace")
report["workspace_f_files"] = st_vfs.f_files

# 6. /tmp noexec enforcement
with open("/tmp/noexec_test.sh", "w") as f:
    f.write("#!/bin/sh\necho NOEXEC_FAIL\n")
os.chmod("/tmp/noexec_test.sh", 0o755)
try:
    subprocess.run(["/tmp/noexec_test.sh"], check=True)
    raise AssertionError("/tmp unexpectedly allowed execution")
except PermissionError as exc:
    report["tmp_noexec_errno"] = exc.errno

# 7. PIDs limit check
children = []
try:
    for _ in range(128):
        children.append(subprocess.Popen(["sleep", "2"]))
    raise AssertionError("pids_limit did not block fork exhaustion")
except OSError as exc:
    report["pids_limit_errno"] = exc.errno
    report["spawned_before_limit"] = len(children)
finally:
    for p in children:
        p.kill()
        p.wait()

with open("/workspace/isolation-report.json", "w") as f:
    json.dump(report, f)

# 8. Deterministic 1s synthetic MP4 via ffmpeg in /workspace
subprocess.run([
    "ffmpeg", "-y", "-f", "lavfi", "-i", "color=c=navy:s=360x640:r=15:d=1",
    "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100:duration=1",
    "-c:v", "libx264", "-preset", "ultrafast", "-threads", "2", "-pix_fmt", "yuv420p",
    "-c:a", "aac", "-shortest",
    "/workspace/probe.mp4"
], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

print("LIVE_ISOLATION_OK")
"###,
    )
    .unwrap();

    // Script 2: Hardened Playwright Chromium test (no --no-sandbox!) saving PNG + JSON into /workspace.
    fs::write(
        skill_dir.join("verify_chromium.js"),
        r###"
const { chromium } = require("playwright");
const fs = require("fs");

(async () => {
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 360, height: 640 } });
  await page.setContent(`
    <html><body style="margin:0;background:#102030;color:#fff;display:flex;align-items:center;justify-content:center;height:100vh;font-family:sans-serif;">
      <div id="badge">SCENIVOX_HARDENED_OK</div>
    </body></html>
  `);
  const badge = await page.textContent("#badge");
  await page.screenshot({ path: "/workspace/frame.png" });
  await browser.close();
  fs.writeFileSync("/workspace/chromium.json", JSON.stringify({ badge }));
  console.log("CHROMIUM_LIVE_OK:" + badge);
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
"###,
    )
    .unwrap();

    // Script 3: Timeout test spawning detached grandchild.
    fs::write(
        skill_dir.join("timeout_test.py"),
        "import subprocess, time\nsubprocess.Popen(['sleep', '60'])\ntime.sleep(60)\n",
    )
    .unwrap();

    // Script 4: Symlink escape trap inside /workspace attempting to exfiltrate /etc/passwd.
    fs::write(
        skill_dir.join("symlink_trap.py"),
        "import os\nos.symlink('/etc/passwd', '/workspace/evil.txt')\n",
    )
    .unwrap();

    let backend = live_backend(TrustedStagingRoots {
        jobs_root: Some(jobs_root.clone()),
        skills_root: Some(skills_root.clone()),
    });

    let mut policy = SandboxPolicy::hardened_default(&image_ref).unwrap();
    policy.limits.cpus = 1.5;
    policy.limits.memory_mb = 512;
    policy.limits.pids_limit = 64;
    policy.limits.workspace_bytes = 8 * 1024 * 1024;
    policy.limits.tmpfs_bytes = 8 * 1024 * 1024;
    policy.limits.timeout_secs = 120;

    let isolation = backend.verify_readiness(&policy).unwrap();
    println!("LIVE_READINESS={:?}", isolation);
    assert!(isolation.rootless_verified_or_check(&isolation));
    assert!(isolation.runtime.rootless_verified);
    assert_eq!(isolation.runtime.cgroup_version, "v2");
    assert_eq!(isolation.runtime.oci_runtime_name, "crun");
    assert!(isolation.mandatory_controls_verified);
    assert!(isolation.runtime.controls.tmpfs_size_enforced);
    assert!(!isolation.runtime.controls.tmpfs_inodes_enforced);

    // Run 1: Isolation + Cgroup Limits + FFmpeg + Bounded Artifact Transfer.
    let out_dir_1 = job_dir.join("outputs-isolation");
    let res1 = backend
        .execute(
            &policy,
            &SandboxRequest {
                job_id: LIVE_JOB_ID.into(),
                invocation_id: "live-iso-1".into(),
                host_workspace_dir: None,
                read_only_mounts: vec![
                    ReadOnlyMount {
                        host_path: skill_dir.clone(),
                        container_path: "/skill".into(),
                    },
                    ReadOnlyMount {
                        host_path: inputs_dir.clone(),
                        container_path: "/inputs/job".into(),
                    },
                ],
                command: SandboxCommand::Script {
                    runtime: ScriptRuntime::Python3,
                    root: ScriptMountRoot::Skill,
                    script_rel_path: "verify_sandbox.py".into(),
                    args: vec![],
                },
                env: BTreeMap::new(),
                artifact_transfer: Some(ArtifactTransferRequest {
                    destination_dir: out_dir_1.clone(),
                    artifacts: vec![
                        ExpectedArtifact {
                            relative_path: "isolation-report.json".into(),
                            max_bytes: 64 * 1024,
                        },
                        ExpectedArtifact {
                            relative_path: "probe.mp4".into(),
                            max_bytes: 4 * 1024 * 1024,
                        },
                    ],
                    max_total_bytes: 4 * 1024 * 1024 + 64 * 1024,
                }),
            },
        )
        .unwrap();

    assert!(res1.cleanup_verified);
    assert!(res1.stdout.contains("LIVE_ISOLATION_OK"));
    let report_json: serde_json::Value =
        serde_json::from_slice(&fs::read(out_dir_1.join("isolation-report.json")).unwrap())
            .unwrap();
    println!("LIVE_ISOLATION_REPORT={report_json}");
    assert_eq!(report_json["cpu_max"], "150000 100000");
    assert_eq!(report_json["memory_max"], "536870912");
    assert_eq!(report_json["swap_max"], "0");
    assert_eq!(report_json["pids_max"], "64");
    assert_eq!(report_json["tmpfs_enospc_errno"], 28);
    assert_eq!(report_json["tmp_noexec_errno"], 13);
    assert_eq!(report_json["pids_limit_errno"], 11);
    assert!(out_dir_1.join("probe.mp4").exists());
    assert!(
        list_managed_containers(&isolation.runtime.transport, LIVE_JOB_ID).is_empty(),
        "no leftover container allowed after Run 1"
    );

    // Run 1b: ffprobe inside hardened container against the extracted MP4 mounted read-only under /inputs/job!
    let staged_mp4 = inputs_dir.join("probe.mp4");
    fs::copy(out_dir_1.join("probe.mp4"), &staged_mp4).unwrap();
    let probe_res = backend
        .execute(
            &policy,
            &SandboxRequest {
                job_id: LIVE_JOB_ID.into(),
                invocation_id: "live-ffprobe".into(),
                host_workspace_dir: None,
                read_only_mounts: vec![ReadOnlyMount {
                    host_path: inputs_dir.clone(),
                    container_path: "/inputs/job".into(),
                }],
                command: SandboxCommand::MediaTool {
                    tool: MediaTool::Ffprobe,
                    args: vec![
                        "-v".into(),
                        "error".into(),
                        "-show_entries".into(),
                        "stream=codec_type,width,height:format=duration".into(),
                        "-of".into(),
                        "json".into(),
                        "/inputs/job/probe.mp4".into(),
                    ],
                },
                env: BTreeMap::new(),
                artifact_transfer: None,
            },
        )
        .unwrap();
    println!("LIVE_FFPROBE={}", probe_res.stdout.trim());
    let ffprobe_val: serde_json::Value = serde_json::from_str(&probe_res.stdout).unwrap();
    assert_eq!(ffprobe_val["streams"][0]["width"], 360);
    assert_eq!(ffprobe_val["streams"][0]["height"], 640);

    // Run 2: Hardened Playwright Chromium (no --no-sandbox!) + bounded artifact transfer.
    let mut chromium_policy = SandboxPolicy::hardened_default(&image_ref).unwrap();
    chromium_policy.limits.cpus = 2.0;
    chromium_policy.limits.memory_mb = 1024;
    chromium_policy.limits.pids_limit = 256;
    chromium_policy.limits.tmpfs_bytes = 128 * 1024 * 1024;
    chromium_policy.limits.workspace_bytes = 128 * 1024 * 1024;
    chromium_policy.limits.timeout_secs = 120;

    let out_dir_2 = job_dir.join("outputs-chromium");
    let res2 = backend
        .execute(
            &chromium_policy,
            &SandboxRequest {
                job_id: LIVE_JOB_ID.into(),
                invocation_id: "live-chromium-1".into(),
                host_workspace_dir: None,
                read_only_mounts: vec![ReadOnlyMount {
                    host_path: skill_dir.clone(),
                    container_path: "/skill".into(),
                }],
                command: SandboxCommand::Script {
                    runtime: ScriptRuntime::Node,
                    root: ScriptMountRoot::Skill,
                    script_rel_path: "verify_chromium.js".into(),
                    args: vec![],
                },
                env: BTreeMap::new(),
                artifact_transfer: Some(ArtifactTransferRequest {
                    destination_dir: out_dir_2.clone(),
                    artifacts: vec![
                        ExpectedArtifact {
                            relative_path: "frame.png".into(),
                            max_bytes: 2 * 1024 * 1024,
                        },
                        ExpectedArtifact {
                            relative_path: "chromium.json".into(),
                            max_bytes: 64 * 1024,
                        },
                    ],
                    max_total_bytes: 3 * 1024 * 1024,
                }),
            },
        )
        .unwrap();
    assert!(
        res2.stdout
            .contains("CHROMIUM_LIVE_OK:SCENIVOX_HARDENED_OK")
    );
    let png_bytes = fs::read(out_dir_2.join("frame.png")).unwrap();
    assert!(png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    println!("LIVE_CHROMIUM_PNG_BYTES={}", png_bytes.len());
    assert!(list_managed_containers(&isolation.runtime.transport, LIVE_JOB_ID).is_empty());

    // Run 3: Symlink trap inside /workspace must be rejected by O_NOFOLLOW extractor!
    let trap_out_dir = job_dir.join("outputs-symlink-trap");
    let trap_err = backend
        .execute(
            &policy,
            &SandboxRequest {
                job_id: LIVE_JOB_ID.into(),
                invocation_id: "live-symlink-trap".into(),
                host_workspace_dir: None,
                read_only_mounts: vec![ReadOnlyMount {
                    host_path: skill_dir.clone(),
                    container_path: "/skill".into(),
                }],
                command: SandboxCommand::Script {
                    runtime: ScriptRuntime::Python3,
                    root: ScriptMountRoot::Skill,
                    script_rel_path: "symlink_trap.py".into(),
                    args: vec![],
                },
                env: BTreeMap::new(),
                artifact_transfer: Some(ArtifactTransferRequest {
                    destination_dir: trap_out_dir.clone(),
                    artifacts: vec![ExpectedArtifact {
                        relative_path: "evil.txt".into(),
                        max_bytes: 64 * 1024,
                    }],
                    max_total_bytes: 64 * 1024,
                }),
            },
        )
        .unwrap_err();
    assert!(matches!(trap_err, SandboxError::ExecutionFailed { .. }));
    assert!(!trap_out_dir.exists());
    assert!(list_managed_containers(&isolation.runtime.transport, LIVE_JOB_ID).is_empty());

    // Run 4: Timeout + descendant cleanup verification!
    let mut timeout_policy = policy.clone();
    timeout_policy.limits.timeout_secs = 12;
    let to_err = backend
        .execute(
            &timeout_policy,
            &SandboxRequest {
                job_id: LIVE_JOB_ID.into(),
                invocation_id: "live-timeout-1".into(),
                host_workspace_dir: None,
                read_only_mounts: vec![ReadOnlyMount {
                    host_path: skill_dir,
                    container_path: "/skill".into(),
                }],
                command: SandboxCommand::Script {
                    runtime: ScriptRuntime::Python3,
                    root: ScriptMountRoot::Skill,
                    script_rel_path: "timeout_test.py".into(),
                    args: vec![],
                },
                env: BTreeMap::new(),
                artifact_transfer: None,
            },
        )
        .unwrap_err();
    assert_eq!(to_err.code(), "sandbox_timeout");
    assert!(list_managed_containers(&isolation.runtime.transport, LIVE_JOB_ID).is_empty());
}

trait IsolationExt {
    fn rootless_verified_or_check(&self, v: &scenivox::sandbox::VerifiedIsolation) -> bool;
}

impl IsolationExt for scenivox::sandbox::VerifiedIsolation {
    fn rootless_verified_or_check(&self, v: &scenivox::sandbox::VerifiedIsolation) -> bool {
        v.isolation_verified && v.runtime.cgroup_version == "v2"
    }
}
