//! First real AI-generated stick-figure video workflow (CEO-authorized, bounded).
//!
//! Two explicitly separated stages so generated JS/composition code is ALWAYS statically validated
//! AND reviewed on disk BEFORE executing inside the strict `scenivox-rootless` Docker sandbox:
//! Stage 1 (`stage1_prepare_and_generate_coffee_composition`): reuses the frozen coffee storyboard,
//! imports and validates the actual `stick-figure` skill bundle, and sends Request #1 (max 4
//! requests, <= 4096 output tokens per request, <= 65536 total tokens) to 9Router `CODING`,
//! writing `shots.js` + `words.js` without executing any generated code.
//!
//! Stage 2 (`stage2_render_repair_and_qa_coffee_video`): requires explicit
//! `SCENIVOX_REVIEWED_GENERATED_SOURCE=1`, verifies empirical `scenivox-rootless` Docker isolation
//! (`chromiumSandbox: true` + custom seccomp + `nr_inodes`), renders 1080x1920 @ 30 FPS 6s MP4,
//! repairs within the remaining request budget on failure, and runs isolated `ffprobe` + 1s/3s/5s
//! PNG frame extraction.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use scenivox::{
    config::LiveLlmConfig,
    cost::{
        CostConsent, CostScope, CreateCostConsent, InferenceProvenance, ReserveBudgetRequest,
        SetModelPricing, WorkerLease,
    },
    db,
    domain::{
        Approval, CreateProject, CreateRevision, ModelRecord, ModelSelection, ModelSnapshot,
        ModelSnapshots, Parameters, RegisterModel, ReviewCommand, Revision, RevisionInput,
        SceneInput, Selection, StoryboardInput, Target,
    },
    openai_adapter::{
        CancellationToken, DispatchContext, HttpTransportSecurityPolicy, OpenAiCompatibleAdapter,
        OpenAiExecutionOutcome, ReqwestHttpTransport, compute_model_snapshot_sha256,
    },
    provider::{
        LlmMessage, LlmRequest, PROVIDER_SCHEMA_VERSION, ProviderAvailability,
        ProviderCapabilities, ProviderKind, ResponseFormat, TaskRole,
    },
    sandbox::{
        ArtifactTransferRequest, ExpectedArtifact, MediaTool, OciBackendConfig, OciSandboxBackend,
        PinnedDockerRuntime, ReadOnlyMount, SandboxCommand, SandboxPolicy, SandboxRequest,
        ScriptMountRoot, ScriptRuntime, TrustedStagingRoots, VerifiedIsolation,
    },
    skill::{SkillRecord, verify_stored_bundle},
    store::Store,
    voice::{AudioProvenance, SampleFormat, validate_wav},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const RUN_DIR_ENV: &str = "SCENIVOX_LIVE_VIDEO_DIR";
const AUTH_ENV: &str = "SCENIVOX_CEO_QUOTA_AUTHORIZED";
const ENABLED_ENV: &str = "SCENIVOX_LLM_ENABLED";
const DOCKER_IMAGE_ENV: &str = "SCENIVOX_LIVE_DOCKER_IMAGE";
const WSL_DISTRO_ENV: &str = "SCENIVOX_LIVE_WSL_DISTRO";
const REVIEWED_SOURCE_ENV: &str = "SCENIVOX_REVIEWED_GENERATED_SOURCE";
const STICK_FIGURE_REL_SOURCE: &str =
    r"..\Studio-Video-AI\stick-figure-windows\skills\stick-figure";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TurnAuditEntry {
    request_index: u32,
    purpose: String,
    reservation_id: String,
    requested_combo: String,
    actual_upstream_model: Option<String>,
    inference_provenance: InferenceProvenance,
    latency_ms: u64,
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    request_sha256: String,
    response_sha256: String,
    settlement_status: String,
    actual_micros: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LiveVideoRunState {
    project_id: String,
    revision: Revision,
    approval: Approval,
    model: ModelRecord,
    skill: SkillRecord,
    consent_id: String,
    worker_lease: WorkerLease,
    job_id: String,
    db_path: PathBuf,
    skills_root: PathBuf,
    jobs_root: PathBuf,
    job_inputs_dir: PathBuf,
    job_outputs_dir: PathBuf,
    job_previews_dir: PathBuf,
    narration_wav_sha256: String,
    narration_wav_bytes: usize,
    shots_js_sha256: String,
    words_js_sha256: String,
    turns: Vec<TurnAuditEntry>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn require_live_authorization() {
    assert_eq!(
        std::env::var(AUTH_ENV).as_deref(),
        Ok("1"),
        "{AUTH_ENV}=1 is required after explicit CEO quota approval"
    );
    assert_eq!(
        std::env::var(ENABLED_ENV).as_deref(),
        Ok("true"),
        "{ENABLED_ENV}=true is required"
    );
}

fn resolve_run_dir() -> PathBuf {
    match std::env::var(RUN_DIR_ENV) {
        Ok(raw) if !raw.trim().is_empty() => PathBuf::from(raw.trim()),
        _ => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("live_video_run"),
    }
}

fn resolve_actual_stick_figure_source() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let p = manifest_dir.join(STICK_FIGURE_REL_SOURCE);
    assert!(
        p.join("SKILL.md").is_file(),
        "actual stick-figure skill not found at {}",
        p.display()
    );
    p
}

fn combo_caps_unknown(model_id: &str) -> ProviderCapabilities {
    ProviderCapabilities {
        schema_version: PROVIDER_SCHEMA_VERSION.into(),
        provider_kind: ProviderKind::LocalGatewayRemoteUpstream,
        provider_id: "ninerouter".into(),
        model_id: model_id.into(),
        adapter_version: "1.0.0".into(),
        task_roles: vec![TaskRole::StoryboardPlanning, TaskRole::SceneCodeGeneration],
        supports_tool_calling: true,
        supports_structured_json: true,
        supports_seed: true,
        min_temperature: 0.0,
        max_temperature: 2.0,
        max_context_tokens: 272_000,
        max_output_tokens: 128_000,
        enabled: true,
        configured: true,
        availability: ProviderAvailability::Unknown,
        reason: "Configured 9Router CODING combo gateway".into(),
        remote: true,
    }
}

fn combo_selection(model_id: &str) -> ModelSelection {
    let sel = Some(Selection {
        provider_id: "ninerouter".into(),
        model_id: model_id.into(),
    });
    ModelSelection {
        storyboard_planning: sel.clone(),
        scene_code_generation: sel,
    }
}

fn build_9router_adapter() -> (LiveLlmConfig, OpenAiCompatibleAdapter<ReqwestHttpTransport>) {
    let cred_env_var = if std::env::var("SCENIVOX_NINEROUTER_API_KEY")
        .ok()
        .is_some_and(|s| !s.trim().is_empty())
    {
        "SCENIVOX_NINEROUTER_API_KEY".to_owned()
    } else {
        String::new()
    };

    let live_cfg = LiveLlmConfig {
        enabled: true,
        provider_id: "ninerouter".into(),
        model_id: "CODING".into(),
        adapter_version: "1.0.0".into(),
        provider_kind: ProviderKind::LocalGatewayRemoteUpstream,
        base_url: "http://127.0.0.1:20128/v1".into(),
        endpoint_allowlist: vec!["http://127.0.0.1:20128/v1".into()],
        gateway_combo_routing: true,
        tokenization_encoding: Some("o200k_base".into()),
        credential_env_var: cred_env_var,
        request_timeout_ms: 90_000,
        connect_timeout_ms: 5_000,
        max_request_bytes: 256 * 1024,
        max_response_bytes: 512 * 1024,
        max_input_tokens: 32_768,
        max_output_tokens: 4_096,
        max_cost_micros: 0,
    };
    let adapter_cfg = live_cfg
        .to_adapter_config(combo_caps_unknown("CODING"), None)
        .unwrap();
    let sec_policy = HttpTransportSecurityPolicy::hardened(
        Duration::from_millis(live_cfg.connect_timeout_ms),
        Duration::from_millis(live_cfg.request_timeout_ms),
        live_cfg.max_response_bytes,
    )
    .unwrap();
    let adapter = OpenAiCompatibleAdapter::new(
        adapter_cfg.clone(),
        ReqwestHttpTransport::new(&adapter_cfg, sec_policy).unwrap(),
    )
    .unwrap();
    (live_cfg, adapter)
}

fn build_explicit_synthetic_wav_bytes() -> Vec<u8> {
    // Explicitly synthetic 6.000s mono 44.1kHz PCM s16le four-beat chime matching the 4 scenes.
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
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());

    for i in 0..num_samples {
        let t = (i as f64) / (sample_rate as f64);
        let beat_t = t % 1.5;
        let base_freq = if t < 1.5 {
            261.63 // Scene 0: C4
        } else if t < 3.0 {
            293.66 // Scene 1: D4
        } else if t < 4.5 {
            329.63 // Scene 2: E4
        } else {
            392.00 // Scene 3: G4
        };
        let env = (-beat_t * 2.5).exp() * 0.45 + 0.08;
        let sample_f = (2.0 * std::f64::consts::PI * base_freq * t).sin() * env
            + (2.0 * std::f64::consts::PI * (base_freq * 2.0) * t).sin() * (env * 0.2);
        let sample_i16 = (sample_f.clamp(-0.95, 0.95) * (i16::MAX as f64)) as i16;
        wav.extend_from_slice(&sample_i16.to_le_bytes());
    }
    wav
}

fn copy_dir_recursive_regular(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let file_type = entry.file_type().unwrap();
        assert!(
            !file_type.is_symlink(),
            "refusing symlink in skill source: {}",
            entry.path().display()
        );
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive_regular(&entry.path(), &target);
        } else if file_type.is_file() {
            fs::copy(entry.path(), target).unwrap();
        } else {
            panic!("unexpected special file in {}", entry.path().display());
        }
    }
}

const SCENIVOX_RENDER_RUNNER_JS: &str = r###"// Reviewed Scenivox fixed harness executing stick-figure composition inside strict rootless Docker.
const { chromium } = require("playwright");
const { spawn } = require("child_process");
const fs = require("fs");
const path = require("path");

const SKILL_WEB = "/skill/engine/template/web";
const GEN_INPUTS = "/inputs/job";
const WORK_WEB = "/workspace/web";

function copyDir(src, dst) {
  fs.mkdirSync(dst, { recursive: true });
  for (const ent of fs.readdirSync(src, { withFileTypes: true })) {
    const s = path.join(src, ent.name);
    const d = path.join(dst, ent.name);
    if (ent.isDirectory()) copyDir(s, d);
    else if (ent.isFile()) fs.copyFileSync(s, d);
  }
}

(async () => {
  const t0 = Date.now();
  fs.rmSync(WORK_WEB, { recursive: true, force: true });
  copyDir(SKILL_WEB, WORK_WEB);
  // Reset reference 37s warp table and overlay generated 6s shots.js + words.js.
  fs.writeFileSync(path.join(WORK_WEB, "warp.js"), "window.WARP = null;\n");
  fs.copyFileSync(path.join(GEN_INPUTS, "shots.js"), path.join(WORK_WEB, "shots.js"));
  fs.copyFileSync(path.join(GEN_INPUTS, "words.js"), path.join(WORK_WEB, "words.js"));

  process.chdir("/workspace");

  // Strict Chromium internal sandbox enabled (seccomp-chromium.json + multi-depth PID/user ns).
  const browser = await chromium.launch({
    headless: true,
    chromiumSandbox: true,
    args: ["--allow-file-access-from-files"],
  });
  const page = await browser.newPage({ viewport: { width: 1080, height: 1920 } });
  const pageErrors = [];
  const consoleErrors = [];
  const consoleLogs = [];
  page.on("pageerror", (err) => pageErrors.push(String(err && err.stack ? err.stack : err)));
  page.on("console", (msg) => {
    const line = `[${msg.type()}] ${msg.text()}`;
    consoleLogs.push(line);
    if (msg.type() === "error") consoleErrors.push(line);
  });

  await page.goto("file:///workspace/web/index.html", { waitUntil: "load" });
  await page.evaluate(async () => {
    await document.fonts.load("700 40px Poppins");
    await document.fonts.load("700 40px Lora");
    await document.fonts.load("600 40px Lora");
  });

  if (pageErrors.length > 0 || consoleErrors.length > 0) {
    throw new Error(
      "JS page load errors: " + JSON.stringify({ pageErrors, consoleErrors }, null, 2)
    );
  }

  const info = await page.evaluate(() => ({
    dur: window.DUR,
    totalFrames: window.TOTAL_FRAMES,
    shotsCount: typeof SHOTS !== "undefined" ? SHOTS.length : -1,
    wordsCount: typeof WORDS !== "undefined" ? WORDS.length : -1,
  }));
  if (
    !Number.isFinite(info.dur) ||
    Math.abs(info.dur - 6.0) > 0.1 ||
    info.totalFrames !== 180 ||
    info.shotsCount < 1 ||
    info.wordsCount < 1
  ) {
    throw new Error("Invalid timeline bounds after loading generated shots.js/words.js: " + JSON.stringify(info));
  }

  const ff = spawn(
    "ffmpeg",
    [
      "-y",
      "-v", "error",
      "-f", "image2pipe",
      "-framerate", "30",
      "-c:v", "mjpeg",
      "-i", "-",
      "-i", "/inputs/job/narration.wav",
      "-c:v", "libx264",
      "-preset", "fast",
      "-threads", "2",
      "-pix_fmt", "yuv420p",
      "-r", "30",
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
      else reject(new Error("ffmpeg exited " + code + ": " + ffStderr.slice(-800)));
    });
  });

  for (let f = 0; f < info.totalFrames; f++) {
    const dataUrl = await page.evaluate((idx) => {
      renderFrame(idx);
      return grab(0.95);
    }, f);
    if (pageErrors.length > 0 || consoleErrors.length > 0) {
      throw new Error(
        `JS runtime error at frame ${f}: ` + JSON.stringify({ pageErrors, consoleErrors })
      );
    }
    const commaIdx = dataUrl.indexOf(",");
    if (commaIdx < 0) throw new Error(`Invalid frame data URL at frame ${f}`);
    const jpgBuf = Buffer.from(dataUrl.slice(commaIdx + 1), "base64");
    if (!ff.stdin.write(jpgBuf)) {
      await new Promise((r) => ff.stdin.once("drain", r));
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
    timeline: info,
    page_errors: pageErrors,
    console_errors: consoleErrors,
    console_logs_count: consoleLogs.length,
    memory_peak_bytes: readCgroup("/sys/fs/cgroup/memory.peak"),
    cpu_stat: readCgroup("/sys/fs/cgroup/cpu.stat"),
  };
  fs.writeFileSync("/workspace/render-metrics.json", JSON.stringify(metrics, null, 2));
  console.log("SCENIVOX_STICK_FIGURE_RENDER_OK");
})().catch((err) => {
  console.error("SCENIVOX_STICK_FIGURE_RENDER_ERR:", err && err.stack ? err.stack : err);
  process.exit(1);
});
"###;

fn build_staged_skill_dir(run_dir: &Path, actual_skill_src: &Path) -> PathBuf {
    let staged = run_dir.join("stick-figure-src");
    if staged.exists() {
        fs::remove_dir_all(&staged).unwrap();
    }
    copy_dir_recursive_regular(actual_skill_src, &staged);

    let manifest = serde_json::json!({
        "schema_version": "1",
        "id": "stick-figure",
        "version": "v2-windows",
        "kind": "unified_composition",
        "composition_mode": "unified_timeline",
        "rendering_strategy": "canvas_playwright_ffmpeg",
        "supported_aspect_ratios": ["9:16"],
        "max_duration_ms": 60000,
        "supports_independent_scene_render": false,
        "code_generation_policy": {
            "languages": ["javascript"],
            "writable_paths": ["web/shots.js", "web/words.js"],
            "requires_sandbox": true
        },
        "entrypoints": {
            "render": {
                "runtime": "node",
                "script": "scenivox_render.js",
                "args_template": ["final.mp4"],
                "timeout_sec": 180
            }
        },
        "requires": ["node", "playwright", "ffmpeg"],
        "accepted_inputs": ["revision.json", "narration.wav"],
        "produced_outputs": ["final.mp4", "render-metrics.json"]
    });
    fs::write(
        staged.join("skill.manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(staged.join("scenivox_render.js"), SCENIVOX_RENDER_RUNNER_JS).unwrap();
    staged
}

fn frozen_coffee_storyboard() -> StoryboardInput {
    StoryboardInput {
        synopsis: "Stick figure yang mengantuk menyeduh kopi pagi. Cangkirnya hampir terguling, tetapi ia berhasil menyelamatkannya, menyesap kopi, lalu bersemangat memulai hari.".into(),
        hook: "Pagi dimulai dengan aroma kopi—dan sedikit kekacauan!".into(),
        scenes: vec![
            SceneInput {
                id: None,
                order: 0,
                duration_ms: 1_500,
                visual_direction: "Gambar stick figure sederhana di dapur mungil. Cahaya matahari masuk dari jendela; cangkir dan alat kopi tampak jelas.".into(),
                camera: "Medium shot vertikal, sedikit dolly-in.".into(),
                action: "Stick figure memasuki dapur dengan langkah mengantuk, lalu mengambil cangkir dan menyiapkan kopi.".into(),
                narration: "Pagi-pagi, ia menyiapkan kopi andalan.".into(),
                caption_hint: "Pagi yang baru dimulai".into(),
                sfx_cues: vec![],
            },
            SceneInput {
                id: None,
                order: 1,
                duration_ms: 1_500,
                visual_direction: "Cairan bergoyang di dekat bibir cangkir. Stick figure membelalak dan buru-buru menahan cangkir; tanpa tumpahan besar.".into(),
                camera: "Close-up pada cangkir dan tangan, lalu hentakan kecil mengikuti gerakannya.".into(),
                action: "Ia menuangkan kopi, tetapi cangkirnya bergeser dan cairan nyaris tumpah.".into(),
                narration: "Ups—cangkirnya hampir tumpah!".into(),
                caption_hint: "Waduh, hampir tumpah!".into(),
                sfx_cues: vec![],
            },
            SceneInput {
                id: None,
                order: 2,
                duration_ms: 1_500,
                visual_direction: "Cangkir kembali tegak. Ekspresi panik berubah menjadi lega; uap tipis mengepul dari kopi.".into(),
                camera: "Medium close-up, kamera kembali tenang.".into(),
                action: "Stick figure menstabilkan cangkir, menghela napas lega, lalu menyesap kopi.".into(),
                narration: "Untung, kopinya berhasil diselamatkan.".into(),
                caption_hint: "Aman... lega!".into(),
                sfx_cues: vec![],
            },
            SceneInput {
                id: None,
                order: 3,
                duration_ms: 1_500,
                visual_direction: "Stick figure tampak segar dan bersemangat, beranjak keluar membawa tas. Akhiri dengan gestur optimistis dan cahaya pagi yang cerah.".into(),
                camera: "Medium shot vertikal, pan mengikuti langkah menuju pintu.".into(),
                action: "Setelah menyesap kopi, ia tersenyum penuh semangat, mengambil tas, dan melangkah keluar dengan mantap.".into(),
                narration: "Satu teguk kemudian, ia siap menghadapi hari.".into(),
                caption_hint: "Siap memulai hari!".into(),
                sfx_cues: vec![],
            },
        ],
    }
}

fn static_review_generated_js(shots_js: &str, words_js: &str) -> Result<(), String> {
    if shots_js.trim().is_empty() || words_js.trim().is_empty() {
        return Err("generated shots_js and words_js must be non-empty".into());
    }
    if shots_js.len() > 128 * 1024 || words_js.len() > 32 * 1024 {
        return Err("generated JS exceeded 128 KiB safety cap".into());
    }
    if shots_js.contains('\0') || words_js.contains('\0') {
        return Err("generated JS contains NUL bytes".into());
    }
    if !shots_js.contains("SHOTS") || !shots_js.contains("SHOTS.push") {
        return Err("shots.js missing SHOTS.push(...)".into());
    }
    if !words_js.contains("WORDS") {
        return Err("words.js missing WORDS declaration".into());
    }
    let forbidden = [
        "require(",
        "import(",
        "import ",
        "export ",
        "child_process",
        "process.",
        "fs.",
        "eval(",
        "Function(",
        "fetch(",
        "XMLHttpRequest",
        "WebSocket",
        "EventSource",
        "sendBeacon",
        "window.open",
        "document.write",
        "<script",
        "</script",
        "http://",
        "https://",
        "file://",
        "__proto__",
        "constructor[",
    ];
    for (label, text) in [("shots.js", shots_js), ("words.js", words_js)] {
        for needle in forbidden {
            if text.contains(needle) {
                return Err(format!(
                    "generated {label} contains forbidden construct '{needle}'"
                ));
            }
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct GeneratedCompositionJson {
    shots_js: String,
    words_js: String,
}

fn build_composition_system_prompt(skill_md: &str) -> String {
    format!(
        "You are Scenivox's code-driven stick-figure animator using the stick-figure skill.\n\
         Return STRICTLY ONE valid JSON object with exactly two string fields:\n\
         {{\"shots_js\": \"...\", \"words_js\": \"...\"}}\n\n\
         CRITICAL RUNTIME ENVIRONMENT & APIs ALREADY LOADED BEFORE shots.js:\n\
         - HTML loads in order: lib.js, chars.js, env.js, words.js, warp.js (`window.WARP = null`), shots.js, main.js.\n\
         - Available lib.js constants/functions:\n\
           * W=1080, H=1920, FPS=30, INK='#221c22', TAU=Math.PI*2\n\
           * clamp(v,a=0,b=1), lerp(a,b,t), inv(a,b,v), ease(t), easeOut(t), easeIn(t), backOut(t), elastic(t)\n\
           * kf(t, [[t0,v0],[t1,v1,easeFn],...], ease), rng(seed), shake(t,amp,freq=1,seed=0)\n\
           * path(ctx,pts,close=true), rrect(ctx,x,y,w,h,r), fillStroke(ctx,fill,lw=6,stroke=INK)\n\
           * circle(ctx,x,y,r), ellipse(ctx,x,y,rx,ry,rot=0), line(ctx,x1,y1,x2,y2,lw,col=INK)\n\
           * shadeShape(ctx,drawPath,side=1,amt=.16,w=.45,bbox), drawCloud(ctx,x,y,s,seed=1,alpha=1)\n\
           * puff(ctx,x,y,r,t,col,seed,alpha), burst(ctx,x,y,r,n=11,fill='#ffd23a',seed=5,inner=.55), star5(ctx,x,y,r,rot,fill), sparkle(ctx,x,y,r,a=1), sweat(ctx,x,y,s,a=1), text(ctx,str,x,y,size,fill,stroke,sw,font='Poppins',weight=700,align='center')\n\
         - Available chars.js constants/functions:\n\
           * PAL = {{red, blue, yellow, green, navy, purple}}\n\
           * drawStick(ctx, {{ x, y, s, flip=1, cols=PAL.blue, hat:'none', vest, belt, bob=0, lean=0, squash=1, headTilt=0, blush=false, legs:{{L:[a1,a2],R:[a1,a2]}}, arms:{{L:[a1,a2],R:[a1,a2]}}, armLen=1, prop(ctx,k,handPt,ang,side), face:{{eyes:'normal'|'wide'|'happy'|'squint'|'half'|'smug'|'spiral'|'x', blink, look:[lx,ly], mouth:'smile'|'grin'|'o'|'frown'|'smirk'|'flat', browUp, browAng}} }})\n\
           * ALWAYS pass hat:'none' unless specifically wanting a pirate hat (otherwise default bandana draws!).\n\
         - Available env.js constants/functions:\n\
           * applyCam(ctx, {{x, y, z, r}}, depth=1): IMPORTANT — cam.x and cam.y are WORLD CENTER coordinates (default neutral center is x:540, y:960 — NEVER pass x:0, y:0 or 75% of the screen shifts off-canvas!). For subtle pan, use e.g. x:540+lerp(12,0,lt/1.5), y:960. Note shake(t,amp,freq,seed) returns a 2-element array [dx,dy], so index s[0], s[1] if passing to x/y or r.\n\
           * drawSky(ctx, top, bot, y0, y1), mix(a, b, t), drawSun(ctx, x, y, r, t, power)\n\
         - CRITICAL REQUIREMENT FOR shots_js:\n\
           * Define `const SHOTS = [];`\n\
           * Push 4 contiguous shots covering EXACTLY 0.0 to 6.0 seconds: start:0..1.5, 1.5..3.0, 3.0..4.5, 4.5..6.0 (tr: 'whip' or 'cut').\n\
           * Every shot's `draw(ctx, lt, t)` MUST animate smoothly in real time (`lt` inside shot, `t` global) with bright cartoon kitchen visuals (sunlit window, counter/table, coffee maker/kettle, mug with spilling/steaming liquid, splatter drops, bag/door exit), expressive `drawStick`, handheld cam push-ins, whip transitions, and safe framing ABOVE y=1220 so the main.js captions at y=1345 remain unobstructed.\n\
           * Note main.js uses globals `SHOTS` and `WORDS`. `words_js` MUST define `const WORDS = [ ... ];` with Indonesian words `{{\"w\":\"word\",\"s\":start_sec,\"e\":end_sec,\"line\":scene_idx}}` covering 0.1..5.7s matching each scene's Indonesian narration.\n\
           * FORBIDDEN: require, import, export, fetch, XMLHttpRequest, WebSocket, process, eval, Function, external URLs.\n\
         \n\
         --- ACTUAL SKILL.md ---\n{skill_md}"
    )
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_9router_call(
    adapter: &OpenAiCompatibleAdapter<ReqwestHttpTransport>,
    store: &Store,
    lease: &WorkerLease,
    model: &ModelRecord,
    snapshot: &ModelSnapshot,
    consent_id: &str,
    project_id: &str,
    job_id: &str,
    request_index: u32,
    purpose: &str,
    system_prompt: String,
    user_prompt: String,
) -> (OpenAiExecutionOutcome, TurnAuditEntry) {
    let llm_req = LlmRequest {
        schema_version: PROVIDER_SCHEMA_VERSION.into(),
        task_role: TaskRole::SceneCodeGeneration,
        provider_id: "ninerouter".into(),
        model_id: "CODING".into(),
        adapter_version: "1.0.0".into(),
        parameters: model.parameters.clone(),
        max_output_tokens: 4_096,
        response_format: ResponseFormat::JsonObject,
        system_prompt,
        messages: vec![LlmMessage::user(user_prompt)],
        tools: Vec::new(),
    };
    let req_sha = llm_req.canonical_request_sha256().unwrap();
    let est_in = adapter.trusted_input_tokens(&llm_req).unwrap();
    let snap_sha = compute_model_snapshot_sha256(model, Some(snapshot)).unwrap();
    let endpoint_identity = adapter.config().endpoint_identity().unwrap();
    let cancel = CancellationToken::new();

    let idem_key = uuid::Uuid::new_v4().to_string();
    let outcome = adapter
        .execute_with_new_reservation(
            DispatchContext {
                store,
                lease,
                project_id: Some(project_id),
                job_id: Some(job_id),
                model,
                snapshot: Some(snapshot),
                cancel: &cancel,
            },
            ReserveBudgetRequest {
                cost_consent_id: consent_id.to_owned(),
                project_id: Some(project_id.to_owned()),
                job_id: Some(job_id.to_owned()),
                provider_id: "ninerouter".into(),
                model_id: "CODING".into(),
                scope: CostScope::SceneCodeGeneration,
                owner_id: lease.owner_id.clone(),
                lease_epoch: lease.lease_epoch,
                canonical_request_sha256: req_sha.clone(),
                model_snapshot_sha256: snap_sha,
                endpoint_identity,
                estimated_input_tokens: est_in,
                max_output_tokens: 4_096,
            },
            Some(&idem_key),
            &llm_req,
        )
        .await
        .expect("bounded 9Router CODING dispatch failed");

    let entry = TurnAuditEntry {
        request_index,
        purpose: purpose.to_owned(),
        reservation_id: outcome.reservation.id.clone(),
        requested_combo: outcome.requested_model_id.clone(),
        actual_upstream_model: outcome.actual_upstream_model.clone(),
        inference_provenance: outcome.inference_provenance,
        latency_ms: outcome.latency_ms,
        input_tokens: outcome.response.usage.input_tokens,
        output_tokens: outcome.response.usage.output_tokens,
        total_tokens: outcome.response.usage.total_tokens,
        request_sha256: req_sha,
        response_sha256: outcome.response.response_sha256.clone(),
        settlement_status: format!("{:?}", outcome.reservation.status),
        actual_micros: outcome.reservation.actual_micros,
    };
    (outcome, entry)
}

fn parse_and_review_generated(content: &str) -> Result<(String, String), String> {
    let parsed: GeneratedCompositionJson = serde_json::from_str(content)
        .map_err(|err| format!("response is not valid {{shots_js, words_js}} JSON: {err}"))?;
    static_review_generated_js(&parsed.shots_js, &parsed.words_js)?;
    Ok((parsed.shots_js, parsed.words_js))
}

#[tokio::test]
#[ignore = "CEO-authorized Stage 1 live generation (SCENIVOX_CEO_QUOTA_AUTHORIZED=1 and SCENIVOX_LLM_ENABLED=true)"]
async fn stage1_prepare_and_generate_coffee_stick_figure_composition() {
    require_live_authorization();
    let run_dir = resolve_run_dir();
    if run_dir.exists() {
        fs::remove_dir_all(&run_dir).unwrap();
    }
    fs::create_dir_all(&run_dir).unwrap();

    let skills_root = run_dir.join("skills");
    let jobs_root = run_dir.join("jobs");
    let job_id = uuid::Uuid::new_v4().to_string();
    let job_inputs_dir = jobs_root.join(&job_id).join("inputs");
    let job_outputs_dir = jobs_root.join(&job_id).join("outputs");
    let job_previews_dir = jobs_root.join(&job_id).join("previews");
    fs::create_dir_all(&skills_root).unwrap();
    fs::create_dir_all(&job_inputs_dir).unwrap();
    fs::create_dir_all(&job_outputs_dir).unwrap();
    fs::create_dir_all(&job_previews_dir).unwrap();

    let db_path = run_dir.join("scenivox_live_video.db");
    let db_url = format!(
        "sqlite://{}?mode=rwc",
        db_path.to_string_lossy().replace('\\', "/")
    );
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool);

    // 1. Register ninerouter/CODING (UNPRICED, 4 requests max, 65536 tokens max).
    store
        .register_model(RegisterModel {
            schema_version: PROVIDER_SCHEMA_VERSION.into(),
            provider_id: "ninerouter".into(),
            model_id: "CODING".into(),
            adapter_version: "1.0.0".into(),
            task_roles: vec!["storyboard_planning".into(), "scene_code_generation".into()],
            parameters: Parameters {
                temperature: 0.0,
                seed: Some(42),
            },
            enabled: true,
            configured: true,
            availability: "unknown".into(),
            reason: "9Router CODING combo awaiting live composition response".into(),
            remote: true,
        })
        .await
        .unwrap();
    store
        .set_model_pricing(SetModelPricing::unknown_upstream_quota(
            "ninerouter",
            "CODING",
            "2026-11-9router-unpriced-video",
        ))
        .await
        .unwrap();
    let (consent, _) = store
        .create_cost_consent_with_request_limit(
            None,
            CreateCostConsent {
                schema_version: PROVIDER_SCHEMA_VERSION.into(),
                project_id: None,
                provider_id: "ninerouter".into(),
                model_id: "CODING".into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 0,
                max_total_tokens: Some(65_536),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
            Some(4),
        )
        .await
        .unwrap();
    let lease = store
        .acquire_worker_lease(&uuid::Uuid::new_v4().to_string(), 600)
        .await
        .unwrap();
    let model = store
        .list_models()
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.provider_id == "ninerouter" && m.model_id == "CODING")
        .unwrap();

    // 2. Import and validate the ACTUAL stick-figure skill bundle.
    let actual_src = resolve_actual_stick_figure_source();
    let staged_src = build_staged_skill_dir(&run_dir, &actual_src);
    let (skill, _) = store
        .import_skill(&skills_root, None, &staged_src)
        .await
        .unwrap();
    let integrity = verify_stored_bundle(&skills_root, &skill).unwrap();
    assert!(integrity.verified);

    // 3. Reuse the approved coffee storyboard WITHOUT regenerating it.
    let target = Target {
        duration_ms: 6_000,
        aspect_ratio: "9:16".into(),
        fps: 30,
        language: "id-ID".into(),
        style_id: "stick-figure".into(),
    };
    let project = store
        .create_project(CreateProject {
            schema_version: "1".into(),
            title: "Kopi Pagi Stick Figure (Live AI Video)".into(),
            idea: "Stick figure menyeduh kopi pagi, nyaris menumpahkan cangkir, lalu bersemangat memulai hari.".into(),
            target: target.clone(),
            model_selection: combo_selection("CODING"),
        })
        .await
        .unwrap();

    let storyboard = frozen_coffee_storyboard();
    let model_snapshot = ModelSnapshot {
        provider_id: "ninerouter".into(),
        model_id: "CODING".into(),
        adapter_version: "1.0.0".into(),
        prompt_sha256: "0".repeat(64),
        parameters: model.parameters.clone(),
    };
    let revision = store
        .create_revision(
            &project.id,
            CreateRevision {
                schema_version: "1".into(),
                expected_revision_id: None,
                content: RevisionInput {
                    schema_version: "1".into(),
                    idea: project.idea.clone(),
                    target,
                    storyboard,
                    models: ModelSnapshots {
                        storyboard_planning: model_snapshot.clone(),
                        scene_code_generation: model_snapshot.clone(),
                    },
                    asset_hashes: Vec::new(),
                    skill_id: skill.skill_id.clone(),
                    skill_version: skill.skill_version.clone(),
                    planning_provenance: None,
                },
            },
        )
        .await
        .unwrap();
    store
        .review_revision(
            &project.id,
            &revision.id,
            None,
            ReviewCommand {
                schema_version: "1".into(),
                content_sha256: revision.content_sha256.clone(),
            },
        )
        .await
        .unwrap();
    let (approval, _, _) = store
        .approve_revision(
            &project.id,
            &revision.id,
            None,
            ReviewCommand {
                schema_version: "1".into(),
                content_sha256: revision.content_sha256.clone(),
            },
        )
        .await
        .unwrap();

    // Write explicitly labeled synthetic 6.000s PCM WAV fixture + verify via validate_wav.
    let wav_bytes = build_explicit_synthetic_wav_bytes();
    let wav_meta = validate_wav(&wav_bytes).unwrap();
    assert_eq!(wav_meta.audio.format, SampleFormat::PcmSigned16);
    assert_eq!(wav_meta.audio.duration_ms, 6_000);
    assert_eq!(wav_meta.provenance, AudioProvenance::UserSupplied);
    fs::write(job_inputs_dir.join("narration.wav"), &wav_bytes).unwrap();
    fs::write(
        job_inputs_dir.join("revision.json"),
        serde_json::to_vec_pretty(&revision).unwrap(),
    )
    .unwrap();

    // 4. Read actual imported SKILL.md and call 9Router CODING (Request #1) for composition code.
    let stored_skill_md =
        fs::read_to_string(skills_root.join(&skill.storage_rel_dir).join("SKILL.md")).unwrap();
    let (_, adapter) = build_9router_adapter();

    let system_prompt = build_composition_system_prompt(&stored_skill_md);
    let user_prompt = serde_json::json!({
        "task": "Generate the complete 6-second (0.0s to 6.0s) Indonesian stick-figure animation composition (shots_js + words_js) matching the approved storyboard.",
        "revision_id": revision.id,
        "content_sha256": revision.content_sha256,
        "storyboard": revision.content.storyboard,
        "canvas": { "width": 1080, "height": 1920, "fps": 30, "total_duration_sec": 6.0 },
        "caption_safe_zone_note": "main.js draws 1-2 word lowercase captions from WORDS at y=1345; frame character heads and mugs within y=380..1200 and counter tops around y=1180..1240."
    })
    .to_string();

    let (outcome, turn1) = dispatch_9router_call(
        &adapter,
        &store,
        &lease,
        &model,
        &model_snapshot,
        &consent.id,
        &project.id,
        &job_id,
        1,
        "initial_generation",
        system_prompt,
        user_prompt,
    )
    .await;

    let updated_model = store
        .list_models()
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.provider_id == "ninerouter" && m.model_id == "CODING")
        .unwrap();

    let (shots_js, words_js) = parse_and_review_generated(&outcome.response.content)
        .unwrap_or_else(|e| {
            // Save unexecuted raw content even if static gate rejected, so state shows budget used!
            fs::write(
                run_dir.join("raw_response_turn1.txt"),
                &outcome.response.content,
            )
            .unwrap();
            panic!("Generated composition failed pre-execution static review: {e}");
        });

    let shots_path = job_inputs_dir.join("shots.js");
    let words_path = job_inputs_dir.join("words.js");
    fs::write(&shots_path, shots_js.as_bytes()).unwrap();
    fs::write(&words_path, words_js.as_bytes()).unwrap();

    let state = LiveVideoRunState {
        project_id: project.id,
        revision,
        approval,
        model: updated_model,
        skill,
        consent_id: consent.id,
        worker_lease: lease,
        job_id,
        db_path,
        skills_root,
        jobs_root,
        job_inputs_dir,
        job_outputs_dir,
        job_previews_dir,
        narration_wav_sha256: sha256_hex(&wav_bytes),
        narration_wav_bytes: wav_bytes.len(),
        shots_js_sha256: sha256_hex(shots_js.as_bytes()),
        words_js_sha256: sha256_hex(words_js.as_bytes()),
        turns: vec![turn1],
    };
    fs::write(
        run_dir.join("run_state.json"),
        serde_json::to_vec_pretty(&state).unwrap(),
    )
    .unwrap();

    println!(
        "STAGE1_COMPOSITION_READY shots_path={} words_path={} shots_bytes={} words_bytes={} turns_used=1",
        shots_path.display(),
        words_path.display(),
        shots_js.len(),
        words_js.len()
    );
}

fn validate_ffprobe_media(ffprobe_json: &serde_json::Value) -> Result<(), String> {
    let streams = ffprobe_json["streams"]
        .as_array()
        .ok_or_else(|| "ffprobe missing streams array".to_owned())?;
    let v_stream = streams
        .iter()
        .find(|s| s["codec_type"] == "video")
        .ok_or_else(|| "ffprobe missing video stream".to_owned())?;
    let a_stream = streams
        .iter()
        .find(|s| s["codec_type"] == "audio")
        .ok_or_else(|| "ffprobe missing audio stream".to_owned())?;

    if v_stream["codec_name"] != "h264" {
        return Err(format!("expected h264, got {}", v_stream["codec_name"]));
    }
    if v_stream["pix_fmt"] != "yuv420p" {
        return Err(format!("expected yuv420p, got {}", v_stream["pix_fmt"]));
    }
    if v_stream["width"] != 1080 || v_stream["height"] != 1920 {
        return Err(format!(
            "expected 1080x1920, got {}x{}",
            v_stream["width"], v_stream["height"]
        ));
    }
    if v_stream["r_frame_rate"] != "30/1" || v_stream["avg_frame_rate"] != "30/1" {
        return Err(format!(
            "expected 30/1 FPS, got r={} avg={}",
            v_stream["r_frame_rate"], v_stream["avg_frame_rate"]
        ));
    }
    if v_stream["nb_read_frames"] != "180" {
        return Err(format!(
            "expected 180 video frames, got {}",
            v_stream["nb_read_frames"]
        ));
    }
    if a_stream["codec_name"] != "aac" {
        return Err(format!(
            "expected aac audio, got {}",
            a_stream["codec_name"]
        ));
    }
    let dur: f64 = ffprobe_json["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "ffprobe missing numeric format.duration".to_owned())?;
    if !(5.90..=6.10).contains(&dur) {
        return Err(format!("expected ~6.0s duration, got {dur}"));
    }
    Ok(())
}

fn ensure_fresh_isolation(
    backend: &OciSandboxBackend<scenivox::sandbox::SystemCommandRunner>,
    policy: &SandboxPolicy,
    iso: &mut VerifiedIsolation,
) {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    if iso
        .empirical_expires_at_unix_secs
        .is_none_or(|exp| now_secs + 60 >= exp)
    {
        let (fresh, _) = backend.verify_empirical_isolation(policy, true).unwrap();
        *iso = fresh;
    }
}

#[tokio::test]
#[ignore = "CEO-authorized Stage 2 strict rootless Docker render, repair, and isolated MediaQA (SCENIVOX_REVIEWED_GENERATED_SOURCE=1)"]
async fn stage2_render_repair_and_qa_coffee_video() {
    require_live_authorization();
    assert_eq!(
        std::env::var(REVIEWED_SOURCE_ENV).as_deref(),
        Ok("1"),
        "{REVIEWED_SOURCE_ENV}=1 required after human/agent code review of shots.js and words.js"
    );
    let image_ref = std::env::var(DOCKER_IMAGE_ENV)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .expect("SCENIVOX_LIVE_DOCKER_IMAGE must be set");
    let distro = std::env::var(WSL_DISTRO_ENV)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Ubuntu".into());

    let run_dir = resolve_run_dir();
    let mut state: LiveVideoRunState =
        serde_json::from_slice(&fs::read(run_dir.join("run_state.json")).unwrap()).unwrap();

    let db_url = format!(
        "sqlite://{}?mode=rwc",
        state.db_path.to_string_lossy().replace('\\', "/")
    );
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool);

    // Re-verify immutable skill bundle integrity before executing in container.
    let integrity = verify_stored_bundle(&state.skills_root, &state.skill).unwrap();
    assert!(integrity.verified);
    let skill_bundle_dir = state.skills_root.join(&state.skill.storage_rel_dir);

    let mut policy = SandboxPolicy::hardened_default(&image_ref).unwrap();
    policy.limits.timeout_secs = 180;
    policy.limits.memory_mb = 2_048;
    let expected_digest = policy.image.digest().unwrap();
    let pin = PinnedDockerRuntime::scenivox_rootless(&expected_digest).unwrap();

    let backend = OciSandboxBackend::system(OciBackendConfig {
        preferred_wsl_distro: Some(distro),
        trusted_roots: TrustedStagingRoots {
            jobs_root: Some(state.jobs_root.clone()),
            skills_root: Some(state.skills_root.clone()),
        },
    })
    .with_docker_pin(pin)
    .with_strict_empirical_gate(true);

    let (mut empirical_iso, probe) = backend.verify_empirical_isolation(&policy, true).unwrap();
    assert!(empirical_iso.empirically_verified);
    assert!(probe.inode_exhaustion_enforced);
    assert!(probe.chromium_no_sandbox_flag_absent);
    assert!(probe.chromium_zygote_active);
    assert!(probe.chromium_renderer_pid_ns_depth >= 2);
    assert_eq!(probe.chromium_renderer_seccomp_mode, 2);

    let stored_skill_md = fs::read_to_string(skill_bundle_dir.join("SKILL.md")).unwrap();
    let (_, adapter) = build_9router_adapter();

    struct RenderedQaSuccess {
        render_elapsed_ms: u64,
        ffprobe_val: serde_json::Value,
        metrics_val: serde_json::Value,
        frame_hashes: BTreeMap<String, (u64, String)>,
        mp4_sha256: String,
        mp4_bytes: u64,
    }

    let mut attempt: u32 = 0;
    let max_requests: u32 = 4;

    let delivered = loop {
        attempt += 1;
        let shots_js = fs::read_to_string(state.job_inputs_dir.join("shots.js")).unwrap();
        let words_js = fs::read_to_string(state.job_inputs_dir.join("words.js")).unwrap();
        static_review_generated_js(&shots_js, &words_js).unwrap();
        state.shots_js_sha256 = sha256_hex(shots_js.as_bytes());
        state.words_js_sha256 = sha256_hex(words_js.as_bytes());

        ensure_fresh_isolation(&backend, &policy, &mut empirical_iso);

        let attempt_outputs_dir = state.job_outputs_dir.join(format!("attempt-{attempt}"));
        if attempt_outputs_dir.exists() {
            fs::remove_dir_all(&attempt_outputs_dir).unwrap();
        }
        fs::create_dir_all(&attempt_outputs_dir).unwrap();

        let render_req = SandboxRequest {
            job_id: state.job_id.clone(),
            invocation_id: format!("live-render-{attempt}"),
            host_workspace_dir: None,
            read_only_mounts: vec![
                ReadOnlyMount {
                    host_path: skill_bundle_dir.clone(),
                    container_path: "/skill".into(),
                },
                ReadOnlyMount {
                    host_path: state.job_inputs_dir.clone(),
                    container_path: "/inputs/job".into(),
                },
            ],
            command: SandboxCommand::Script {
                runtime: ScriptRuntime::Node,
                root: ScriptMountRoot::Skill,
                script_rel_path: "scenivox_render.js".into(),
                args: vec!["final.mp4".into()],
            },
            env: BTreeMap::new(),
            artifact_transfer: Some(ArtifactTransferRequest {
                destination_dir: attempt_outputs_dir.clone(),
                artifacts: vec![
                    ExpectedArtifact {
                        relative_path: "final.mp4".into(),
                        max_bytes: 32 * 1024 * 1024,
                    },
                    ExpectedArtifact {
                        relative_path: "render-metrics.json".into(),
                        max_bytes: 262_144,
                    },
                ],
                max_total_bytes: 33 * 1024 * 1024,
            }),
        };

        let started = Instant::now();
        let attempt_res: Result<RenderedQaSuccess, String> = (|| {
            let res = backend
                .execute_with_verified_isolation(&policy, &empirical_iso, &render_req)
                .map_err(|err| match err {
                    scenivox::sandbox::SandboxError::InvalidMount(msg)
                    | scenivox::sandbox::SandboxError::InvalidPolicy(msg)
                    | scenivox::sandbox::SandboxError::InvalidInvocation(msg)
                    | scenivox::sandbox::SandboxError::SandboxUnavailable(msg) => {
                        format!("NON_REPAIRABLE_HOST_SANDBOX_ERROR: {msg}")
                    }
                    scenivox::sandbox::SandboxError::CleanupFailed { context, detail } => {
                        format!("NON_REPAIRABLE_HOST_SANDBOX_ERROR: cleanup failed ({context}): {detail}")
                    }
                    other => format!("Sandbox render execution error: {other}"),
                })?;
            assert!(res.cleanup_verified);
            let host_ms = started.elapsed().as_millis() as u64;
            let mp4_art = res
                .transferred_artifacts
                .iter()
                .find(|a| a.relative_path == "final.mp4")
                .ok_or_else(|| "missing transferred final.mp4".to_owned())?;
            let met_art = res
                .transferred_artifacts
                .iter()
                .find(|a| a.relative_path == "render-metrics.json")
                .ok_or_else(|| "missing transferred render-metrics.json".to_owned())?;
            fs::copy(&mp4_art.host_path, state.job_outputs_dir.join("final.mp4"))
                .map_err(|e| e.to_string())?;
            fs::copy(
                &met_art.host_path,
                state.job_outputs_dir.join("render-metrics.json"),
            )
            .map_err(|e| e.to_string())?;
            let met_json: serde_json::Value =
                serde_json::from_slice(&fs::read(&met_art.host_path).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;

            // Isolated MediaQA #1: ffprobe inside strict rootless Docker.
            ensure_fresh_isolation(&backend, &policy, &mut empirical_iso);
            let probe_req = SandboxRequest {
                job_id: state.job_id.clone(),
                invocation_id: format!("live-ffprobe-{attempt}"),
                host_workspace_dir: None,
                read_only_mounts: vec![ReadOnlyMount {
                    host_path: attempt_outputs_dir.clone(),
                    container_path: "/inputs/job".into(),
                }],
                command: SandboxCommand::MediaTool {
                    tool: MediaTool::Ffprobe,
                    args: vec![
                        "-v".into(),
                        "error".into(),
                        "-show_format".into(),
                        "-show_streams".into(),
                        "-count_frames".into(),
                        "-of".into(),
                        "json".into(),
                        "/inputs/job/final.mp4".into(),
                    ],
                },
                env: BTreeMap::new(),
                artifact_transfer: None,
            };
            let probe_res = backend
                .execute_with_verified_isolation(&policy, &empirical_iso, &probe_req)
                .map_err(|err| format!("MediaQA ffprobe error: {err}"))?;
            let parsed_ffprobe: serde_json::Value = serde_json::from_str(&probe_res.stdout)
                .map_err(|err| format!("MediaQA ffprobe JSON parse error: {err}"))?;
            fs::write(
                state.job_outputs_dir.join("ffprobe.json"),
                serde_json::to_vec_pretty(&parsed_ffprobe).unwrap(),
            )
            .unwrap();

            validate_ffprobe_media(&parsed_ffprobe)
                .map_err(|err| format!("MediaQA ffprobe failure: {err}"))?;

            // Isolated MediaQA #2: extract 1s, 3s, 5s preview PNGs inside rootless Docker.
            let mut hashes = BTreeMap::new();
            for (fname, ts) in [
                ("preview_1s.png", "1.0"),
                ("preview_3s.png", "3.0"),
                ("preview_5s.png", "5.0"),
            ] {
                ensure_fresh_isolation(&backend, &policy, &mut empirical_iso);
                let snap_dest_dir = state
                    .job_previews_dir
                    .join(format!("attempt-{attempt}-{}", fname.replace('.', "_")));
                if snap_dest_dir.exists() {
                    fs::remove_dir_all(&snap_dest_dir).unwrap();
                }
                fs::create_dir_all(&snap_dest_dir).unwrap();
                let snap_req = SandboxRequest {
                    job_id: state.job_id.clone(),
                    invocation_id: format!("live-snap-{attempt}-{}", fname.replace('.', "-")),
                    host_workspace_dir: None,
                    read_only_mounts: vec![ReadOnlyMount {
                        host_path: attempt_outputs_dir.clone(),
                        container_path: "/inputs/job".into(),
                    }],
                    command: SandboxCommand::MediaTool {
                        tool: MediaTool::Ffmpeg,
                        args: vec![
                            "-y".into(),
                            "-ss".into(),
                            ts.into(),
                            "-i".into(),
                            "/inputs/job/final.mp4".into(),
                            "-frames:v".into(),
                            "1".into(),
                            format!("/workspace/{fname}"),
                        ],
                    },
                    env: BTreeMap::new(),
                    artifact_transfer: Some(ArtifactTransferRequest {
                        destination_dir: snap_dest_dir,
                        artifacts: vec![ExpectedArtifact {
                            relative_path: fname.into(),
                            max_bytes: 8 * 1024 * 1024,
                        }],
                        max_total_bytes: 8 * 1024 * 1024,
                    }),
                };
                let snap_res = backend
                    .execute_with_verified_isolation(&policy, &empirical_iso, &snap_req)
                    .map_err(|err| format!("MediaQA frame {fname} extraction error: {err}"))?;
                let snap_art = &snap_res.transferred_artifacts[0];
                let png_bytes = fs::read(&snap_art.host_path).unwrap();
                if !png_bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                    return Err(format!("MediaQA frame {fname} is not valid PNG"));
                }
                fs::copy(&snap_art.host_path, state.job_previews_dir.join(fname))
                    .map_err(|e| e.to_string())?;
                hashes.insert(
                    fname.to_owned(),
                    (snap_art.size_bytes, snap_art.sha256.clone()),
                );
            }

            if hashes["preview_1s.png"].1 == hashes["preview_3s.png"].1
                || hashes["preview_3s.png"].1 == hashes["preview_5s.png"].1
                || hashes["preview_1s.png"].1 == hashes["preview_5s.png"].1
            {
                return Err(
                    "MediaQA failure: extracted frames at 1s, 3s, 5s are identical (no motion detected)"
                        .into(),
                );
            }

            Ok(RenderedQaSuccess {
                render_elapsed_ms: host_ms,
                ffprobe_val: parsed_ffprobe,
                metrics_val: met_json,
                frame_hashes: hashes,
                mp4_sha256: mp4_art.sha256.clone(),
                mp4_bytes: mp4_art.size_bytes,
            })
        })();

        let err_detail = match attempt_res {
            Ok(ok) => break ok,
            Err(e) => e,
        };
        assert!(
            !err_detail.starts_with("NON_REPAIRABLE_HOST_SANDBOX_ERROR:"),
            "{err_detail}"
        );

        let consent_now: CostConsent = store.get_cost_consent(&state.consent_id).await.unwrap();
        let used = consent_now.spent_requests as u32;
        if used >= max_requests {
            panic!(
                "Authorized request budget ({max_requests}) exhausted during repair: {err_detail}"
            );
        }

        let next_req_num = used + 1;
        let repair_prompt = serde_json::json!({
            "repair_attempt": next_req_num - 1,
            "error_detail": err_detail,
            "current_shots_js": shots_js,
            "current_words_js": words_js,
            "storyboard": state.revision.content.storyboard,
            "instruction": "Fix the error completely while preserving rich animated 6.0s 1080x1920 stick-figure coffee visuals and Indonesian captions. Return STRICTLY ONE JSON object {\"shots_js\": \"...\", \"words_js\": \"...\"}."
        })
        .to_string();

        let (repair_outcome, repair_turn) = dispatch_9router_call(
            &adapter,
            &store,
            &state.worker_lease,
            &state.model,
            &state.revision.content.models.scene_code_generation,
            &state.consent_id,
            &state.project_id,
            &state.job_id,
            next_req_num,
            "repair",
            build_composition_system_prompt(&stored_skill_md),
            repair_prompt,
        )
        .await;
        state.turns.push(repair_turn);

        let (new_shots, new_words) = parse_and_review_generated(&repair_outcome.response.content)
            .expect("Repaired JS failed static security review before container execution");
        fs::write(state.job_inputs_dir.join("shots.js"), new_shots.as_bytes()).unwrap();
        fs::write(state.job_inputs_dir.join("words.js"), new_words.as_bytes()).unwrap();
        fs::write(
            run_dir.join("run_state.json"),
            serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();
    };

    let final_consent = store.get_cost_consent(&state.consent_id).await.unwrap();
    let total_in: u64 = state.turns.iter().map(|t| t.input_tokens).sum();
    let total_out: u64 = state.turns.iter().map(|t| t.output_tokens).sum();
    let total_tok: u64 = state.turns.iter().map(|t| t.total_tokens).sum();

    let final_report = serde_json::json!({
        "status": "DELIVERED_VERIFIED",
        "job_id": state.job_id,
        "revision_id": state.revision.id,
        "revision_content_sha256": state.revision.content_sha256,
        "skill": {
            "id": state.skill.skill_id,
            "version": state.skill.skill_version,
            "bundle_sha256": state.skill.bundle_sha256,
        },
        "sandbox": {
            "docker_context": empirical_iso.docker_context_name,
            "image_digest": empirical_iso.image_digest,
            "seccomp_profile_sha256": empirical_iso.seccomp_profile_sha256,
            "empirically_verified": empirical_iso.empirically_verified,
            "inode_exhaustion_at": probe.inode_exhaustion_at,
            "workspace_inodes_reported": probe.workspace_inodes_reported,
            "chromium_renderer_pid_ns_depth": probe.chromium_renderer_pid_ns_depth,
            "chromium_renderer_seccomp_mode": probe.chromium_renderer_seccomp_mode,
        },
        "audio": {
            "mode": "SYNTHETIC_FIXTURE_WAV",
            "bytes": state.narration_wav_bytes,
            "sha256": state.narration_wav_sha256,
        },
        "llm_usage": {
            "requests_used": final_consent.spent_requests,
            "max_requests": final_consent.max_requests,
            "total_input_tokens": total_in,
            "total_output_tokens": total_out,
            "total_tokens": total_tok,
            "max_total_tokens": final_consent.max_total_tokens,
            "turns": state.turns,
        },
        "render": {
            "host_elapsed_ms": delivered.render_elapsed_ms,
            "container_metrics": delivered.metrics_val,
        },
        "artifacts": {
            "final_mp4": {
                "path": state.job_outputs_dir.join("final.mp4"),
                "bytes": delivered.mp4_bytes,
                "sha256": delivered.mp4_sha256,
            },
            "shots_js": {
                "path": state.job_inputs_dir.join("shots.js"),
                "sha256": state.shots_js_sha256,
            },
            "words_js": {
                "path": state.job_inputs_dir.join("words.js"),
                "sha256": state.words_js_sha256,
            },
            "previews": {
                "preview_1s": {
                    "path": state.job_previews_dir.join("preview_1s.png"),
                    "bytes": delivered.frame_hashes["preview_1s.png"].0,
                    "sha256": delivered.frame_hashes["preview_1s.png"].1,
                },
                "preview_3s": {
                    "path": state.job_previews_dir.join("preview_3s.png"),
                    "bytes": delivered.frame_hashes["preview_3s.png"].0,
                    "sha256": delivered.frame_hashes["preview_3s.png"].1,
                },
                "preview_5s": {
                    "path": state.job_previews_dir.join("preview_5s.png"),
                    "bytes": delivered.frame_hashes["preview_5s.png"].0,
                    "sha256": delivered.frame_hashes["preview_5s.png"].1,
                },
            },
        },
        "ffprobe": delivered.ffprobe_val,
    });
    fs::write(
        run_dir.join("final_report.json"),
        serde_json::to_vec_pretty(&final_report).unwrap(),
    )
    .unwrap();
    fs::write(
        run_dir.join("run_state.json"),
        serde_json::to_vec_pretty(&state).unwrap(),
    )
    .unwrap();

    println!(
        "LIVE_AI_VIDEO_REPORT={}",
        serde_json::to_string_pretty(&final_report).unwrap()
    );
}

#[tokio::test]
#[ignore = "Isolated strict rootless Docker MediaQA + 1s/3s/5s preview extraction on existing rendered final.mp4 (0 LLM requests)"]
async fn extract_existing_live_video_mediaqa() {
    let image_ref = std::env::var(DOCKER_IMAGE_ENV)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .expect("SCENIVOX_LIVE_DOCKER_IMAGE must be set");
    let distro = std::env::var(WSL_DISTRO_ENV)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Ubuntu".into());

    let run_dir = resolve_run_dir();
    let state: LiveVideoRunState =
        serde_json::from_slice(&fs::read(run_dir.join("run_state.json")).unwrap()).unwrap();

    let mut policy = SandboxPolicy::hardened_default(&image_ref).unwrap();
    policy.limits.timeout_secs = 180;
    policy.limits.memory_mb = 2_048;
    let expected_digest = policy.image.digest().unwrap();
    let pin = PinnedDockerRuntime::scenivox_rootless(&expected_digest).unwrap();

    let backend = OciSandboxBackend::system(OciBackendConfig {
        preferred_wsl_distro: Some(distro),
        trusted_roots: TrustedStagingRoots {
            jobs_root: Some(state.jobs_root.clone()),
            skills_root: Some(state.skills_root.clone()),
        },
    })
    .with_docker_pin(pin)
    .with_strict_empirical_gate(true);

    let (mut empirical_iso, _) = backend.verify_empirical_isolation(&policy, true).unwrap();

    let probe_req = SandboxRequest {
        job_id: state.job_id.clone(),
        invocation_id: "existing-ffprobe-qa".into(),
        host_workspace_dir: None,
        read_only_mounts: vec![ReadOnlyMount {
            host_path: state.job_outputs_dir.clone(),
            container_path: "/inputs/job".into(),
        }],
        command: SandboxCommand::MediaTool {
            tool: MediaTool::Ffprobe,
            args: vec![
                "-v".into(),
                "error".into(),
                "-show_format".into(),
                "-show_streams".into(),
                "-count_frames".into(),
                "-of".into(),
                "json".into(),
                "/inputs/job/final.mp4".into(),
            ],
        },
        env: BTreeMap::new(),
        artifact_transfer: None,
    };
    let probe_res = backend
        .execute_with_verified_isolation(&policy, &empirical_iso, &probe_req)
        .unwrap();
    let parsed_ffprobe: serde_json::Value = serde_json::from_str(&probe_res.stdout).unwrap();
    fs::write(
        state.job_outputs_dir.join("ffprobe.json"),
        serde_json::to_vec_pretty(&parsed_ffprobe).unwrap(),
    )
    .unwrap();
    validate_ffprobe_media(&parsed_ffprobe).unwrap();

    let mut hashes = BTreeMap::new();
    for (fname, ts) in [
        ("preview_1s.png", "1.0"),
        ("preview_3s.png", "3.0"),
        ("preview_5s.png", "5.0"),
    ] {
        ensure_fresh_isolation(&backend, &policy, &mut empirical_iso);
        let snap_dest_dir = state
            .job_previews_dir
            .join(format!("qa-{}", fname.replace('.', "_")));
        if snap_dest_dir.exists() {
            fs::remove_dir_all(&snap_dest_dir).unwrap();
        }
        fs::create_dir_all(&snap_dest_dir).unwrap();
        let snap_req = SandboxRequest {
            job_id: state.job_id.clone(),
            invocation_id: format!("existing-snap-{}", fname.replace('.', "-")),
            host_workspace_dir: None,
            read_only_mounts: vec![ReadOnlyMount {
                host_path: state.job_outputs_dir.clone(),
                container_path: "/inputs/job".into(),
            }],
            command: SandboxCommand::MediaTool {
                tool: MediaTool::Ffmpeg,
                args: vec![
                    "-y".into(),
                    "-ss".into(),
                    ts.into(),
                    "-i".into(),
                    "/inputs/job/final.mp4".into(),
                    "-frames:v".into(),
                    "1".into(),
                    format!("/workspace/{fname}"),
                ],
            },
            env: BTreeMap::new(),
            artifact_transfer: Some(ArtifactTransferRequest {
                destination_dir: snap_dest_dir,
                artifacts: vec![ExpectedArtifact {
                    relative_path: fname.into(),
                    max_bytes: 8 * 1024 * 1024,
                }],
                max_total_bytes: 8 * 1024 * 1024,
            }),
        };
        let snap_res = backend
            .execute_with_verified_isolation(&policy, &empirical_iso, &snap_req)
            .unwrap();
        let snap_art = &snap_res.transferred_artifacts[0];
        let png_bytes = fs::read(&snap_art.host_path).unwrap();
        assert!(png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        fs::copy(&snap_art.host_path, state.job_previews_dir.join(fname)).unwrap();
        hashes.insert(
            fname.to_owned(),
            (snap_art.size_bytes, snap_art.sha256.clone()),
        );
    }

    assert_ne!(hashes["preview_1s.png"].1, hashes["preview_3s.png"].1);
    assert_ne!(hashes["preview_3s.png"].1, hashes["preview_5s.png"].1);
    assert_ne!(hashes["preview_1s.png"].1, hashes["preview_5s.png"].1);

    let mp4_bytes = fs::read(state.job_outputs_dir.join("final.mp4")).unwrap();
    let mp4_sha = sha256_hex(&mp4_bytes);
    println!(
        "EXISTING_MEDIAQA_OK mp4_bytes={} mp4_sha256={mp4_sha} previews={}",
        mp4_bytes.len(),
        serde_json::to_string(&hashes).unwrap()
    );
}
