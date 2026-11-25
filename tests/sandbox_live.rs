use std::{
    fs,
    process::Command,
    time::{Duration, Instant},
};

use scenivox::sandbox::{
    OciBackendConfig, OciSandboxBackend, SandboxBackend, SandboxError, SandboxPolicy,
    TrustedStagingRoots,
};

fn live_image_ref() -> Option<String> {
    std::env::var("SCENIVOX_LIVE_SANDBOX_IMAGE")
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
    assert!(detect_err.to_string().contains("nr_inodes"));

    let readiness_err = backend.verify_readiness(&policy).unwrap_err();
    assert!(matches!(readiness_err, SandboxError::SandboxUnavailable(_)));
    assert_eq!(readiness_err.code(), "sandbox_unavailable");
    assert!(readiness_err.to_string().contains("nr_inodes"));
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
