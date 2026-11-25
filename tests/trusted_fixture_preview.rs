//! TRUSTED_FIXTURE_ONLY development preview harness (excluded from production execution).
//!
//! Authorized ONLY for human-reviewed fixed animation source + verified local WAV fixture
//! under outer rootless Podman OCI containment (`--network=none --read-only --cap-drop=ALL
//! --security-opt=no-new-privileges`, UID 65532). Never alters production `SandboxPolicy`
//! or invokes `OciSandboxBackend` production gates.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use scenivox::voice::{AudioProvenance, SampleFormat, validate_wav};
use sha2::{Digest, Sha256};

const PREVIEW_IMAGE_DEFAULT_ENV: &str = "SCENIVOX_LIVE_SANDBOX_IMAGE";
const PREVIEW_ENABLE_ENV: &str = "SCENIVOX_TRUSTED_FIXTURE_PREVIEW";
const PREVIEW_OUT_DIR_ENV: &str = "SCENIVOX_PREVIEW_OUT_DIR";
const PREVIEW_DISTRO_ENV: &str = "SCENIVOX_LIVE_WSL_DISTRO";

const RENDER_SCRIPT_JS: &str = r###"
// TRUSTED_FIXTURE_ONLY deterministic 6-second 1080x1920 30 FPS stick-figure renderer.
const { chromium } = require("playwright");
const { spawn } = require("child_process");
const fs = require("fs");

const WIDTH = 1080;
const HEIGHT = 1920;
const FPS = 30;
const TOTAL_FRAMES = 180;

const SCENE_HTML = `<!doctype html>
<html>
<head>
<meta charset="utf-8" />
<style>
  html, body { margin: 0; padding: 0; width: 1080px; height: 1920px; overflow: hidden; background: #09111f; }
  canvas { display: block; width: 1080px; height: 1920px; }
</style>
</head>
<body>
<canvas id="stage" width="1080" height="1920"></canvas>
<script>
const WIDTH = 1080;
const HEIGHT = 1920;
const canvas = document.getElementById("stage");
const ctx = canvas.getContext("2d");

function clamp(v, lo, hi) { return Math.max(lo, Math.min(hi, v)); }
function lerp(a, b, t) { return a + (b - a) * t; }
function smoothstep(t) { t = clamp(t, 0, 1); return t * t * (3 - 2 * t); }

function drawLimb(ax, ay, bx, by, cx, cy, color, width) {
  ctx.strokeStyle = color;
  ctx.lineWidth = width;
  ctx.lineCap = "round";
  ctx.lineJoin = "round";
  ctx.beginPath();
  ctx.moveTo(ax, ay);
  ctx.lineTo(bx, by);
  ctx.lineTo(cx, cy);
  ctx.stroke();
}

window.renderFrame = function(frameIdx) {
  const t = frameIdx / 30.0;
  const p = frameIdx / 180.0;

  // Background gradient
  const bg = ctx.createLinearGradient(0, 0, 0, HEIGHT);
  bg.addColorStop(0, "#09111f");
  bg.addColorStop(0.55, "#112240");
  bg.addColorStop(1, "#1a2f4b");
  ctx.fillStyle = bg;
  ctx.fillRect(0, 0, WIDTH, HEIGHT);

  // Decorative subtle grid & ambient orbs
  ctx.strokeStyle = "rgba(100, 200, 255, 0.06)";
  ctx.lineWidth = 2;
  for (let gx = 90; gx < WIDTH; gx += 180) {
    ctx.beginPath(); ctx.moveTo(gx, 0); ctx.lineTo(gx, 1460); ctx.stroke();
  }
  for (let gy = 180; gy < 1460; gy += 180) {
    ctx.beginPath(); ctx.moveTo(0, gy); ctx.lineTo(WIDTH, gy); ctx.stroke();
  }

  // Floor plane
  const floorY = 1460;
  const floorGrad = ctx.createLinearGradient(0, floorY, 0, HEIGHT);
  floorGrad.addColorStop(0, "#15253c");
  floorGrad.addColorStop(1, "#070d18");
  ctx.fillStyle = floorGrad;
  ctx.fillRect(0, floorY, WIDTH, HEIGHT - floorY);
  ctx.strokeStyle = "#38bdf8";
  ctx.lineWidth = 5;
  ctx.beginPath(); ctx.moveTo(80, floorY); ctx.lineTo(WIDTH - 80, floorY); ctx.stroke();

  // Header branding + progress pill
  ctx.fillStyle = "rgba(15, 23, 42, 0.85)";
  ctx.beginPath(); ctx.roundRect(72, 88, 936, 130, 28); ctx.fill();
  ctx.strokeStyle = "rgba(56, 189, 248, 0.45)";
  ctx.lineWidth = 3;
  ctx.stroke();

  ctx.fillStyle = "#38bdf8";
  ctx.font = "bold 34px sans-serif";
  ctx.fillText("SCENIVOX  //  TRUSTED OCI SANDBOX PREVIEW", 112, 145);
  ctx.fillStyle = "#cbd5e1";
  ctx.font = "28px monospace";
  ctx.fillText("1080x1920 @ 30 FPS   |   t = " + t.toFixed(2) + "s / 6.00s   |   F" + String(frameIdx + 1).padStart(3, "0"), 112, 192);

  // Choreography parameters across 3 phases (0-2s walk, 2-4s wave & showcase card, 4-6s leap + pulse)
  let rootX = 520;
  let jumpY = 0;
  let lean = 0;
  let stride = 0;
  let rightArmRaise = 0;
  let leftArmPoint = 0;
  let squash = 0;
  let caption = "";
  let badgeColor = "#38bdf8";

  if (t < 2.0) {
    const u = smoothstep(t / 2.0);
    rootX = lerp(220, 520, u);
    const cycle = t * Math.PI * 3.0;
    stride = Math.sin(cycle) * (1.0 - smoothstep((t - 1.7) / 0.3));
    jumpY = -Math.abs(Math.sin(cycle)) * 22 * (1.0 - smoothstep((t - 1.7) / 0.3));
    lean = 14 * (1.0 - u);
    caption = "1. Rootless OCI Sandbox Isolation";
    badgeColor = "#38bdf8";
  } else if (t < 4.0) {
    const u = clamp((t - 2.0) / 2.0, 0, 1);
    rootX = 520;
    rightArmRaise = smoothstep(clamp(u * 3.5, 0, 1)) * (1.0 - smoothstep(clamp((u - 0.85) / 0.15, 0, 1)));
    leftArmPoint = rightArmRaise;
    jumpY = Math.sin(u * Math.PI * 2.0) * 6;
    caption = "2. Bounded Ephemeral Extraction";
    badgeColor = "#34d399";
  } else {
    const u = clamp((t - 4.0) / 2.0, 0, 1);
    rootX = 520;
    if (u < 0.2) {
      squash = smoothstep(u / 0.2) * 32;
    } else if (u < 0.65) {
      const air = (u - 0.2) / 0.45;
      jumpY = -Math.sin(air * Math.PI) * 230;
      rightArmRaise = Math.sin(air * Math.PI) * 1.15;
      leftArmPoint = rightArmRaise;
      stride = Math.sin(air * Math.PI) * 0.7;
    } else {
      const land = (u - 0.65) / 0.35;
      squash = Math.sin(clamp(land * 2.0, 0, 1) * Math.PI) * 22;
      rightArmRaise = 0.25 * (1.0 - land);
    }
    caption = "3. Isolated MediaQA Verified";
    badgeColor = "#f59e0b";
  }

  // Floating 9:16 telemetry card during Phase 2 & 3
  if (t >= 1.8) {
    const cardIn = smoothstep(clamp((t - 1.8) / 0.5, 0, 1));
    const cardX = lerp(1120, 680, cardIn);
    const cardY = 470 + Math.sin(t * 2.5) * 14;
    ctx.save();
    ctx.globalAlpha = cardIn;
    ctx.fillStyle = "rgba(15, 23, 42, 0.92)";
    ctx.beginPath(); ctx.roundRect(cardX, cardY, 320, 250, 26); ctx.fill();
    ctx.strokeStyle = badgeColor;
    ctx.lineWidth = 4;
    ctx.stroke();
    ctx.fillStyle = badgeColor;
    ctx.font = "bold 28px sans-serif";
    ctx.fillText("ARTIFACT SHA-256", cardX + 28, cardY + 52);
    ctx.fillStyle = "#e2e8f0";
    ctx.font = "24px monospace";
    ctx.fillText("net: none", cardX + 28, cardY + 104);
    ctx.fillText("rootfs: ro", cardX + 28, cardY + 144);
    ctx.fillText("uid: 65532", cardX + 28, cardY + 184);
    ctx.fillStyle = "#38bdf8";
    ctx.fillRect(cardX + 28, cardY + 206, 264 * p, 14);
    ctx.restore();
  }

  // Shockwave ripple on final landing
  if (t >= 5.25) {
    const rw = clamp((t - 5.25) / 0.75, 0, 1);
    ctx.save();
    ctx.strokeStyle = "rgba(245, 158, 11, " + (1.0 - rw) * 0.8 + ")";
    ctx.lineWidth = 8 * (1.0 - rw) + 2;
    ctx.beginPath();
    ctx.ellipse(rootX, floorY + 8, 60 + rw * 290, 18 + rw * 46, 0, 0, Math.PI * 2);
    ctx.stroke();
    ctx.restore();
  }

  // Ground shadow
  const shadowScale = clamp(1.0 + jumpY / 420.0, 0.4, 1.15);
  ctx.fillStyle = "rgba(2, 6, 23, 0.55)";
  ctx.beginPath();
  ctx.ellipse(rootX, floorY + 6, 95 * shadowScale, 20 * shadowScale, 0, 0, Math.PI * 2);
  ctx.fill();

  // Articulated Stick Figure Skeleton
  const hipY = floorY - 270 + jumpY + squash;
  const hipX = rootX;
  const shoulderX = hipX + lean;
  const shoulderY = hipY - 210 + squash * 0.35;
  const headR = 64;
  const headX = shoulderX + lean * 0.4;
  const headY = shoulderY - 92;

  // Torso
  ctx.strokeStyle = "#f8fafc";
  ctx.lineWidth = 22;
  ctx.lineCap = "round";
  ctx.beginPath();
  ctx.moveTo(hipX, hipY);
  ctx.lineTo(shoulderX, shoulderY);
  ctx.stroke();

  // Legs
  const leftFootX = hipX - 58 + stride * 125;
  const leftKneeX = (hipX + leftFootX) * 0.5 + 18;
  const leftKneeY = (hipY + floorY) * 0.5 + jumpY * 0.4 - Math.max(0, stride) * 26;
  const leftFootY = floorY + jumpY * 0.85 - Math.max(0, stride) * 38;
  drawLimb(hipX, hipY, leftKneeX, leftKneeY, leftFootX, Math.min(floorY, leftFootY), "#e2e8f0", 20);

  const rightFootX = hipX + 58 - stride * 125;
  const rightKneeX = (hipX + rightFootX) * 0.5 - 12;
  const rightKneeY = (hipY + floorY) * 0.5 + jumpY * 0.4 - Math.max(0, -stride) * 26;
  const rightFootY = floorY + jumpY * 0.85 - Math.max(0, -stride) * 38;
  drawLimb(hipX, hipY, rightKneeX, rightKneeY, rightFootX, Math.min(floorY, rightFootY), "#ffffff", 20);

  // Left arm (swings during walk, points toward telemetry card in phase 2/3)
  let lElbowX = shoulderX - 75 - stride * 70;
  let lElbowY = shoulderY + 85;
  let lHandX = lElbowX - 35 - stride * 45;
  let lHandY = lElbowY + 85;
  if (leftArmPoint > 0.01) {
    lElbowX = lerp(lElbowX, shoulderX + 90, leftArmPoint);
    lElbowY = lerp(lElbowY, shoulderY - 35, leftArmPoint);
    lHandX = lerp(lHandX, shoulderX + 175, leftArmPoint);
    lHandY = lerp(lHandY, shoulderY - 95, leftArmPoint);
  }
  drawLimb(shoulderX, shoulderY, lElbowX, lElbowY, lHandX, lHandY, "#94a3b8", 18);

  // Right arm (swings during walk, waves dynamically upward in phase 2/3)
  let rElbowX = shoulderX + 75 + stride * 70;
  let rElbowY = shoulderY + 85;
  let rHandX = rElbowX + 35 + stride * 45;
  let rHandY = rElbowY + 85;
  if (rightArmRaise > 0.01) {
    const wave = Math.sin(t * 9.0) * 44 * rightArmRaise;
    rElbowX = lerp(rElbowX, shoulderX - 90, rightArmRaise);
    rElbowY = lerp(rElbowY, shoulderY - 55, rightArmRaise);
    rHandX = lerp(rHandX, shoulderX - 155 + wave, rightArmRaise);
    rHandY = lerp(rHandY, shoulderY - 175, rightArmRaise);
  }
  drawLimb(shoulderX, shoulderY, rElbowX, rElbowY, rHandX, rHandY, "#38bdf8", 18);

  // Head + expressive eyes
  ctx.fillStyle = "#0f172a";
  ctx.strokeStyle = "#ffffff";
  ctx.lineWidth = 18;
  ctx.beginPath();
  ctx.arc(headX, headY, headR, 0, Math.PI * 2);
  ctx.fill();
  ctx.stroke();

  const blink = (frameIdx % 75) > 71;
  ctx.fillStyle = badgeColor;
  if (blink) {
    ctx.fillRect(headX + 4, headY - 10, 16, 4);
    ctx.fillRect(headX + 30, headY - 10, 16, 4);
  } else {
    ctx.beginPath(); ctx.arc(headX + 12, headY - 8, 7, 0, Math.PI * 2); ctx.fill();
    ctx.beginPath(); ctx.arc(headX + 36, headY - 8, 7, 0, Math.PI * 2); ctx.fill();
  }

  // Bottom Caption Pill + Timeline Progress Bar
  ctx.fillStyle = "rgba(15, 23, 42, 0.92)";
  ctx.beginPath(); ctx.roundRect(72, 1550, 936, 220, 32); ctx.fill();
  ctx.strokeStyle = badgeColor;
  ctx.lineWidth = 4;
  ctx.stroke();

  ctx.fillStyle = "#ffffff";
  ctx.font = "bold 44px sans-serif";
  ctx.textAlign = "center";
  ctx.fillText(caption, WIDTH / 2, 1645);
  ctx.textAlign = "left";

  ctx.fillStyle = "rgba(148, 163, 184, 0.25)";
  ctx.beginPath(); ctx.roundRect(120, 1700, 840, 22, 11); ctx.fill();
  ctx.fillStyle = badgeColor;
  ctx.beginPath(); ctx.roundRect(120, 1700, Math.max(22, 840 * p), 22, 11); ctx.fill();
};
</script>
</body>
</html>`;

(async () => {
  const t0 = Date.now();
  // TRUSTED_FIXTURE_ONLY exception: Chromium internal sandbox disabled inside outer hardened Podman container.
  const browser = await chromium.launch({ headless: true, chromiumSandbox: false });
  const page = await browser.newPage({ viewport: { width: WIDTH, height: HEIGHT } });
  await page.setContent(SCENE_HTML, { waitUntil: "load" });

  const ff = spawn(
    "ffmpeg",
    [
      "-y",
      "-f", "image2pipe",
      "-vcodec", "png",
      "-r", String(FPS),
      "-i", "-",
      "-i", "/inputs/job/narration.wav",
      "-c:v", "libx264",
      "-preset", "fast",
      "-threads", "2",
      "-pix_fmt", "yuv420p",
      "-r", String(FPS),
      "-c:a", "aac",
      "-b:a", "128k",
      "-t", "6",
      "-shortest",
      "/workspace/final.mp4",
    ],
    { stdio: ["pipe", "ignore", "pipe"] }
  );

  let ffStderr = "";
  ff.stderr.on("data", (d) => { ffStderr += d.toString(); });
  const ffDone = new Promise((resolve, reject) => {
    ff.on("error", reject);
    ff.on("close", (code) => {
      if (code === 0) resolve();
      else reject(new Error("ffmpeg exited " + code + ": " + ffStderr.slice(-600)));
    });
  });

  for (let i = 0; i < TOTAL_FRAMES; i++) {
    await page.evaluate((idx) => window.renderFrame(idx), i);
    const png = await page.screenshot({ type: "png" });
    if (!ff.stdin.write(png)) {
      await new Promise((res) => ff.stdin.once("drain", res));
    }
  }
  ff.stdin.end();
  await ffDone;
  await browser.close();

  const readCgroup = (p) => {
    try { return fs.readFileSync(p, "utf8").trim(); } catch { return null; }
  };
  const metrics = {
    render_ms: Date.now() - t0,
    total_frames: TOTAL_FRAMES,
    fps: FPS,
    width: WIDTH,
    height: HEIGHT,
    memory_peak_bytes: readCgroup("/sys/fs/cgroup/memory.peak"),
    memory_current_bytes: readCgroup("/sys/fs/cgroup/memory.current"),
    cpu_stat: readCgroup("/sys/fs/cgroup/cpu.stat"),
  };
  fs.writeFileSync("/workspace/render-metrics.json", JSON.stringify(metrics, null, 2));
  console.log("TRUSTED_PREVIEW_RENDER_OK");
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
"###;

fn build_trusted_preview_wav_bytes() -> Vec<u8> {
    let sample_rate = 44_100u32;
    let duration_secs = 6u32;
    let num_samples = sample_rate * duration_secs;
    let data_bytes = num_samples * 2;
    let mut wav = Vec::with_capacity(44 + data_bytes as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());

    for i in 0..num_samples {
        let t = (i as f64) / (sample_rate as f64);
        let beat_t = t % 2.0;
        let base_freq = if t < 2.0 {
            261.63 // C4
        } else if t < 4.0 {
            329.63 // E4
        } else {
            392.00 // G4
        };
        let env = (-beat_t * 2.2).exp() * 0.45 + 0.08;
        let sample_f = (2.0 * std::f64::consts::PI * base_freq * t).sin() * env
            + (2.0 * std::f64::consts::PI * (base_freq * 2.0) * t).sin() * (env * 0.25);
        let sample_i16 = (sample_f.clamp(-0.95, 0.95) * (i16::MAX as f64)) as i16;
        wav.extend_from_slice(&sample_i16.to_le_bytes());
    }
    wav
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn format_wsl_mount_path(path: &Path) -> String {
    let raw = path.to_str().expect("UTF-8 path required");
    let trimmed = raw.strip_prefix(r"\\?\").unwrap_or(raw);
    if cfg!(windows) {
        let bytes = trimmed.as_bytes();
        let drive = (bytes[0] as char).to_ascii_lowercase();
        let rest = trimmed[3..].replace('\\', "/");
        format!("/mnt/{drive}/{rest}")
    } else {
        trimmed.to_owned()
    }
}

fn run_podman(distro: &str, args: &[String]) -> std::process::Output {
    if cfg!(windows) {
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--exec")
            .arg("podman")
            .args(args)
            .output()
            .expect("failed to invoke wsl.exe podman")
    } else {
        Command::new("podman")
            .args(args)
            .output()
            .expect("failed to invoke podman")
    }
}

fn decode_svxart_stream(raw: &[u8], expected: &[&str]) -> (String, BTreeMap<String, Vec<u8>>) {
    assert!(raw.starts_with(b"SVXART01"), "missing SVXART01 header");
    let mut pos = 8usize;
    let mut child_stdout = Vec::new();
    let mut files = BTreeMap::new();
    let mut done = false;

    while pos < raw.len() {
        assert!(!done, "unexpected trailing bytes after SVXART01 end frame");
        let tag = raw[pos];
        pos += 1;
        match tag {
            b'O' => {
                let len = u32::from_be_bytes(raw[pos..pos + 4].try_into().unwrap()) as usize;
                pos += 4;
                child_stdout.extend_from_slice(&raw[pos..pos + len]);
                pos += len;
            }
            b'A' => {
                let path_len = u16::from_be_bytes(raw[pos..pos + 2].try_into().unwrap()) as usize;
                pos += 2;
                let rel = std::str::from_utf8(&raw[pos..pos + path_len])
                    .unwrap()
                    .to_owned();
                pos += path_len;
                let claimed: [u8; 32] = raw[pos..pos + 32].try_into().unwrap();
                pos += 32;
                let data_len = u64::from_be_bytes(raw[pos..pos + 8].try_into().unwrap()) as usize;
                pos += 8;
                let payload = raw[pos..pos + data_len].to_vec();
                pos += data_len;
                let actual: [u8; 32] = Sha256::digest(&payload).into();
                assert_eq!(actual, claimed, "SHA-256 mismatch for {rel}");
                assert!(
                    files.insert(rel.clone(), payload).is_none(),
                    "duplicate {rel}"
                );
            }
            b'D' => {
                let marker_len = u32::from_be_bytes(raw[pos..pos + 4].try_into().unwrap());
                assert_eq!(marker_len, 0);
                pos += 4;
                done = true;
            }
            other => panic!("unexpected SVXART01 tag 0x{other:02x}"),
        }
    }

    assert!(done, "truncated SVXART01 stream");
    for name in expected {
        assert!(
            files.contains_key(*name),
            "missing expected artifact {name}"
        );
    }
    assert_eq!(files.len(), expected.len());
    (String::from_utf8_lossy(&child_stdout).into_owned(), files)
}

fn hardened_podman_prefix(
    container_name: &str,
    label_id: &str,
    mounts: &[(&Path, &str)],
    memory_mb: u64,
) -> Vec<String> {
    let mut args = vec![
        "run".into(),
        "--rm".into(),
        "--name".into(),
        container_name.into(),
        "--label".into(),
        "scenivox.trusted_preview=true".into(),
        "--label".into(),
        format!("scenivox.preview_run={label_id}"),
        "--pull=never".into(),
        "--network=none".into(),
        "--read-only".into(),
        "--cap-drop=ALL".into(),
        "--security-opt=no-new-privileges".into(),
        "--user=65532:65532".into(),
        "--cpus=2".into(),
        format!("--memory={memory_mb}m"),
        format!("--memory-swap={memory_mb}m"),
        "--pids-limit=256".into(),
        "--tmpfs=/tmp:rw,noexec,nosuid,nodev,size=268435456,mode=1777".into(),
        "--tmpfs=/workspace:rw,nosuid,nodev,size=268435456,mode=1777".into(),
    ];
    for (host_dir, dst) in mounts {
        let src = format_wsl_mount_path(host_dir);
        args.push(format!("--mount=type=bind,src={src},dst={dst},readonly"));
    }
    args.push("--workdir=/workspace".into());
    args
}

#[test]
#[ignore = "TRUSTED_FIXTURE_ONLY development preview (set SCENIVOX_TRUSTED_FIXTURE_PREVIEW=1 and SCENIVOX_LIVE_SANDBOX_IMAGE)"]
fn trusted_fixture_renders_first_stick_figure_mp4_and_validates_media_qa() {
    assert_eq!(
        std::env::var(PREVIEW_ENABLE_ENV).ok().as_deref(),
        Some("1"),
        "SCENIVOX_TRUSTED_FIXTURE_PREVIEW=1 required"
    );
    let image_ref = std::env::var(PREVIEW_IMAGE_DEFAULT_ENV)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .expect("SCENIVOX_LIVE_SANDBOX_IMAGE must be set");
    let distro = std::env::var(PREVIEW_DISTRO_ENV)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Ubuntu".into());

    let out_dir = match std::env::var(PREVIEW_OUT_DIR_ENV) {
        Ok(raw) if !raw.trim().is_empty() => {
            let p = PathBuf::from(raw.trim());
            fs::create_dir_all(&p).unwrap();
            p
        }
        _ => tempfile::tempdir().unwrap().keep(),
    };

    let stage_temp = tempfile::tempdir().unwrap();
    let skill_dir = stage_temp.path().join("skill");
    let inputs_dir = stage_temp.path().join("inputs");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::create_dir_all(&inputs_dir).unwrap();

    // 1. Prepare fixed human-reviewed render script and verified 6.000s WAV fixture.
    fs::write(skill_dir.join("render_preview.js"), RENDER_SCRIPT_JS).unwrap();
    let wav_bytes = build_trusted_preview_wav_bytes();
    let wav_narration = validate_wav(&wav_bytes).unwrap();
    assert_eq!(wav_narration.audio.format, SampleFormat::PcmSigned16);
    assert_eq!(wav_narration.audio.sample_rate_hz, 44_100);
    assert_eq!(wav_narration.audio.channels, 1);
    assert_eq!(wav_narration.audio.duration_ms, 6_000);
    assert_eq!(wav_narration.provenance, AudioProvenance::UserSupplied);
    assert_eq!(wav_narration.audio.sha256, sha256_hex(&wav_bytes));
    fs::write(inputs_dir.join("narration.wav"), &wav_bytes).unwrap();
    let wav_sha = sha256_hex(&wav_bytes);

    let run_label = format!("preview-{}", std::process::id());

    // 2. Render 1080x1920 30 FPS 6s MP4 inside outer hardened rootless Podman container
    //    and extract via bounded SVXART01 stream before `/workspace` destruction.
    let render_cname = format!("scenivox-trusted-render-{}", std::process::id());
    let mut render_args = hardened_podman_prefix(
        &render_cname,
        &run_label,
        &[(&skill_dir, "/skill"), (&inputs_dir, "/inputs/job")],
        1536,
    );
    render_args.extend([
        "--entrypoint=python3".into(),
        image_ref.clone(),
        "/opt/scenivox-runtime/artifact_runner.py".into(),
        "--max-stdout".into(),
        "65536".into(),
        "--max-stderr".into(),
        "262144".into(),
        "--max-total".into(),
        "16777216".into(),
        "--artifact".into(),
        "final.mp4:16000000".into(),
        "--artifact".into(),
        "render-metrics.json:65536".into(),
        "--".into(),
        "node".into(),
        "/skill/render_preview.js".into(),
    ]);

    let render_started = Instant::now();
    let render_out = run_podman(&distro, &render_args);
    let host_render_ms = render_started.elapsed().as_millis() as u64;
    assert!(
        render_out.status.success(),
        "render failed: stderr={}",
        String::from_utf8_lossy(&render_out.stderr)
    );
    let (render_stdout, render_files) =
        decode_svxart_stream(&render_out.stdout, &["final.mp4", "render-metrics.json"]);
    assert!(render_stdout.contains("TRUSTED_PREVIEW_RENDER_OK"));

    let mp4_bytes = &render_files["final.mp4"];
    let mp4_sha = sha256_hex(mp4_bytes);
    let metrics_bytes = &render_files["render-metrics.json"];
    let metrics_val: serde_json::Value = serde_json::from_slice(metrics_bytes).unwrap();
    fs::write(out_dir.join("final.mp4"), mp4_bytes).unwrap();
    fs::write(out_dir.join("render-metrics.json"), metrics_bytes).unwrap();

    // 3. Isolated MediaQA container #1: ffprobe validation on final.mp4 mounted read-only.
    let probe_cname = format!("scenivox-trusted-ffprobe-{}", std::process::id());
    let mut probe_args =
        hardened_podman_prefix(&probe_cname, &run_label, &[(&out_dir, "/inputs/job")], 512);
    probe_args.extend([
        "--entrypoint=ffprobe".into(),
        image_ref.clone(),
        "-v".into(),
        "error".into(),
        "-show_format".into(),
        "-show_streams".into(),
        "-count_frames".into(),
        "-of".into(),
        "json".into(),
        "/inputs/job/final.mp4".into(),
    ]);
    let probe_out = run_podman(&distro, &probe_args);
    assert!(
        probe_out.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&probe_out.stderr)
    );
    let ffprobe_val: serde_json::Value = serde_json::from_slice(&probe_out.stdout).unwrap();
    fs::write(
        out_dir.join("ffprobe.json"),
        serde_json::to_vec_pretty(&ffprobe_val).unwrap(),
    )
    .unwrap();

    let streams = ffprobe_val["streams"].as_array().unwrap();
    let v_stream = streams
        .iter()
        .find(|s| s["codec_type"] == "video")
        .expect("missing video stream");
    let a_stream = streams
        .iter()
        .find(|s| s["codec_type"] == "audio")
        .expect("missing audio stream");
    assert_eq!(v_stream["codec_name"], "h264");
    assert_eq!(v_stream["pix_fmt"], "yuv420p");
    assert_eq!(v_stream["width"], 1080);
    assert_eq!(v_stream["height"], 1920);
    assert_eq!(v_stream["r_frame_rate"], "30/1");
    assert_eq!(v_stream["avg_frame_rate"], "30/1");
    assert_eq!(v_stream["nb_read_frames"], "180");
    assert_eq!(a_stream["codec_name"], "aac");
    let duration_s: f64 = ffprobe_val["format"]["duration"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (5.90..=6.10).contains(&duration_s),
        "unexpected duration {duration_s}"
    );

    // 4. Isolated MediaQA container #2: extract three representative preview frames (1s, 3s, 5s).
    let preview_timestamps = [
        ("preview_1s.png", "1.0"),
        ("preview_3s.png", "3.0"),
        ("preview_5s.png", "5.0"),
    ];
    let mut frame_hashes = BTreeMap::new();
    for (fname, ts) in preview_timestamps {
        let snap_cname = format!(
            "scenivox-trusted-snap-{}-{}",
            fname.replace('.', "-"),
            std::process::id()
        );
        let mut snap_args =
            hardened_podman_prefix(&snap_cname, &run_label, &[(&out_dir, "/inputs/job")], 512);
        snap_args.extend([
            "--entrypoint=python3".into(),
            image_ref.clone(),
            "/opt/scenivox-runtime/artifact_runner.py".into(),
            "--max-stdout".into(),
            "65536".into(),
            "--max-stderr".into(),
            "65536".into(),
            "--max-total".into(),
            "8388608".into(),
            "--artifact".into(),
            format!("{fname}:8000000"),
            "--".into(),
            "ffmpeg".into(),
            "-y".into(),
            "-ss".into(),
            ts.into(),
            "-i".into(),
            "/inputs/job/final.mp4".into(),
            "-frames:v".into(),
            "1".into(),
            format!("/workspace/{fname}"),
        ]);
        let snap_out = run_podman(&distro, &snap_args);
        assert!(
            snap_out.status.success(),
            "snapshot {fname} failed: {}",
            String::from_utf8_lossy(&snap_out.stderr)
        );
        let (_, snap_files) = decode_svxart_stream(&snap_out.stdout, &[fname]);
        let png = &snap_files[fname];
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        let fsha = sha256_hex(png);
        fs::write(out_dir.join(fname), png).unwrap();
        frame_hashes.insert(fname.to_owned(), (png.len(), fsha));
    }

    assert_ne!(
        frame_hashes["preview_1s.png"].1,
        frame_hashes["preview_3s.png"].1
    );
    assert_ne!(
        frame_hashes["preview_3s.png"].1,
        frame_hashes["preview_5s.png"].1
    );
    assert_ne!(
        frame_hashes["preview_1s.png"].1,
        frame_hashes["preview_5s.png"].1
    );

    // 5. Verify zero leftover containers from this preview run.
    let ps_out = run_podman(
        &distro,
        &[
            "ps".into(),
            "-a".into(),
            "--filter".into(),
            format!("label=scenivox.preview_run={run_label}"),
            "--format".into(),
            "{{.Names}}".into(),
        ],
    );
    assert!(ps_out.status.success());
    assert!(
        String::from_utf8_lossy(&ps_out.stdout).trim().is_empty(),
        "leftover preview containers found!"
    );

    let report = serde_json::json!({
        "mode": "TRUSTED_FIXTURE_ONLY",
        "image_reference": image_ref,
        "out_dir": out_dir,
        "host_render_elapsed_ms": host_render_ms,
        "container_metrics": metrics_val,
        "narration_wav_bytes": wav_bytes.len(),
        "narration_wav_sha256": wav_sha,
        "final_mp4_bytes": mp4_bytes.len(),
        "final_mp4_sha256": mp4_sha,
        "previews": {
            "preview_1s.png": { "bytes": frame_hashes["preview_1s.png"].0, "sha256": frame_hashes["preview_1s.png"].1 },
            "preview_3s.png": { "bytes": frame_hashes["preview_3s.png"].0, "sha256": frame_hashes["preview_3s.png"].1 },
            "preview_5s.png": { "bytes": frame_hashes["preview_5s.png"].0, "sha256": frame_hashes["preview_5s.png"].1 },
        }
    });
    fs::write(
        out_dir.join("preview-report.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    println!(
        "TRUSTED_FIXTURE_PREVIEW_REPORT={}",
        serde_json::to_string_pretty(&report).unwrap()
    );
}
