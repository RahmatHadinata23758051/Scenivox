use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use scenivox::{
    agent::{
        AgentConfig, AgentExecutionMode, AgentLedgerContext, AgentLimits, AgentRuntime,
        AgentTaskInput, ScriptedSimulatedBackend, SimulatedArtifactOutput,
        SimulatedMediaProbeResult, SimulatedStepOutcome,
    },
    config::LiveLlmConfig,
    cost::{
        CostConsentStatus, CostScope, CreateCostConsent, InferenceProvenance, ReservationStatus,
        ReserveBudgetRequest, SetModelPricing,
    },
    db,
    domain::{
        CreateProject, ModelRecord, ModelSelection, Parameters, PlanStoryboard, RegisterModel,
        ReviewCommand, Selection, Target,
    },
    openai_adapter::{
        CancellationToken, DispatchContext, HttpTransportSecurityPolicy, ModelTokenizationProfile,
        OpenAiAdapterError, OpenAiCompatibleAdapter, ReqwestHttpTransport,
        compute_model_snapshot_sha256, compute_trusted_input_tokens_for_profile,
    },
    planner::{AdapterStoryboardRequest, plan_storyboard_with_adapter},
    provider::{
        LlmMessage, LlmRequest, PROVIDER_SCHEMA_VERSION, ProviderAvailability,
        ProviderCapabilities, ProviderKind, ResponseFormat, TaskRole,
    },
    sandbox::{
        ControlEvidenceSource, DetectedRuntime, EffectiveResourceControls, OciEngine, OciTransport,
        PinnedDockerRuntime, SandboxPolicy, VerifiedIsolation,
    },
    store::Store,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use uuid::Uuid;

fn next_db_url(dir: &Path) -> String {
    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);
    let idx = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
    format!(
        "sqlite://{}?mode=rwc",
        dir.join(format!("ninerouter-prep-{idx}.db"))
            .to_string_lossy()
            .replace('\\', "/")
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn combo_caps_with_availability(
    model_id: &str,
    availability: ProviderAvailability,
) -> ProviderCapabilities {
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
        availability,
        reason: "Configured 9Router CODING combo gateway".into(),
        remote: true,
    }
}

fn combo_caps(model_id: &str) -> ProviderCapabilities {
    combo_caps_with_availability(model_id, ProviderAvailability::Available)
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

async fn register_unpriced_combo_with_request_limit(
    store: &Store,
    model_id: &str,
    consent_scopes: Vec<CostScope>,
    max_total_tokens: i64,
    max_requests: Option<i64>,
) -> (
    ModelRecord,
    scenivox::cost::WorkerLease,
    scenivox::cost::CostConsent,
) {
    store
        .register_model(RegisterModel {
            schema_version: PROVIDER_SCHEMA_VERSION.into(),
            provider_id: "ninerouter".into(),
            model_id: model_id.into(),
            adapter_version: "1.0.0".into(),
            task_roles: vec!["storyboard_planning".into(), "scene_code_generation".into()],
            parameters: Parameters {
                temperature: 0.0,
                seed: Some(42),
            },
            enabled: true,
            configured: true,
            availability: "unknown".into(),
            reason: "Configured 9Router combo awaiting genuine authorized response".into(),
            remote: true,
        })
        .await
        .unwrap();

    store
        .set_model_pricing(SetModelPricing::unknown_upstream_quota(
            "ninerouter",
            model_id,
            "2026-11-9router-unpriced",
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
                model_id: model_id.into(),
                scopes: consent_scopes,
                max_cost_micros: 0,
                max_total_tokens: Some(max_total_tokens),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
            max_requests,
        )
        .await
        .unwrap();

    let models = store.list_models().await.unwrap();
    let model = models
        .into_iter()
        .find(|m| {
            m.provider_id == "ninerouter" && m.model_id == model_id && m.adapter_version == "1.0.0"
        })
        .unwrap();
    let lease = store
        .acquire_worker_lease(&Uuid::new_v4().to_string(), 120)
        .await
        .unwrap();
    (model, lease, consent)
}

fn create_stick_figure_skill_fixture(root: &Path) -> PathBuf {
    let dir = root.join("src-stick-figure");
    fs::create_dir_all(dir.join("references")).unwrap();
    fs::create_dir_all(dir.join("scripts")).unwrap();
    let manifest = json!({
        "schema_version": "1",
        "id": "stick-figure",
        "version": "audited-v1",
        "kind": "unified_composition",
        "composition_mode": "unified_timeline",
        "rendering_strategy": "canvas_playwright_ffmpeg",
        "supported_aspect_ratios": ["9:16"],
        "max_duration_ms": null,
        "supports_independent_scene_render": false,
        "code_generation_policy": {
            "languages": ["javascript", "python"],
            "writable_paths": ["web/shots.js", "audio.py", "script.json"],
            "requires_sandbox": true
        },
        "entrypoints": {
            "render": {
                "runtime": "node",
                "script": "scripts/render.js",
                "args_template": ["video", "{out}"],
                "timeout_sec": 300
            }
        },
        "requires": ["node", "playwright", "ffmpeg", "python"],
        "accepted_inputs": ["revision.json"],
        "produced_outputs": ["final.mp4", "stills/*.jpg"]
    });
    fs::write(
        dir.join("skill.manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(
        dir.join("SKILL.md"),
        "# Stick Figure Unified Skill\n\n\
         Hasilkan animasi stick-figure 9:16 30fps berbahasa Indonesia di web/shots.js.\n\
         Gunakan workspace_write / workspace_patch -> skill_run_step -> probe_media -> complete_composition.\n",
    )
    .unwrap();
    fs::write(
        dir.join("references/scene-guide.md"),
        "Canvas 1080x1920 @ 30fps, durasi total 6000ms, gerakan karakter terlihat jelas.\n",
    )
    .unwrap();
    fs::write(
        dir.join("scripts/render.js"),
        "// Test fixture only; simulated agent test never runs host JS.\n",
    )
    .unwrap();
    dir
}

struct MockFixtureHttpReply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
    delay: Duration,
}

async fn read_http_request_raw(stream: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 4096];
    let mut content_len: Option<usize> = None;
    let mut header_end: Option<usize> = None;
    loop {
        let n = stream.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if header_end.is_none() {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let end = pos + 4;
                header_end = Some(end);
                let hdr_text = String::from_utf8_lossy(&buf[..pos]);
                for line in hdr_text.lines() {
                    if let Some((k, v)) = line.split_once(':') {
                        if k.trim().eq_ignore_ascii_case("content-length") {
                            content_len = v.trim().parse::<usize>().ok();
                        }
                    }
                }
            }
        }
        if let Some(end) = header_end {
            let expected = end + content_len.unwrap_or(0);
            if buf.len() >= expected {
                break;
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

async fn spawn_fixture_gateway(
    replies: Vec<MockFixtureHttpReply>,
) -> (
    std::net::SocketAddr,
    Arc<tokio::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let captured_ref = captured.clone();

    let handle = tokio::spawn(async move {
        for reply in replies {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let raw = read_http_request_raw(&mut socket).await;
            captured_ref.lock().await.push(raw);
            if !reply.delay.is_zero() {
                tokio::time::sleep(reply.delay).await;
            }
            let reason = match reply.status {
                200 => "OK",
                401 => "Unauthorized",
                503 => "Service Unavailable",
                _ => "Status",
            };
            let mut extra = String::new();
            for (k, v) in reply.headers {
                extra.push_str(&format!("{k}: {v}\r\n"));
            }
            let resp = format!(
                "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{}",
                reply.status,
                reply.body.len(),
                reply.body
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    (addr, captured, handle)
}

#[tokio::test]
async fn gateway_config_gates_and_loopback_policy_fail_closed() {
    let mut cfg = LiveLlmConfig {
        enabled: false,
        provider_id: "ninerouter".into(),
        model_id: "CODING".into(),
        adapter_version: "1.0.0".into(),
        provider_kind: ProviderKind::LocalGatewayRemoteUpstream,
        base_url: "http://127.0.0.1:20128/v1".into(),
        endpoint_allowlist: vec!["http://127.0.0.1:20128/v1".into()],
        gateway_combo_routing: true,
        tokenization_encoding: Some("o200k_base".into()),
        credential_env_var: String::new(),
        request_timeout_ms: 15_000,
        connect_timeout_ms: 3_000,
        max_request_bytes: 256 * 1024,
        max_response_bytes: 512 * 1024,
        max_input_tokens: 16_384,
        max_output_tokens: 8_192,
        max_cost_micros: 500_000,
    };
    cfg.validate().unwrap();
    // Must stay disabled by default without explicit approval flag.
    assert!(cfg.ensure_live_dispatch_enabled().is_err());

    // Local gateway distinguishes remote-backed inference and forbids non-loopback HTTP.
    assert!(cfg.provider_kind.is_remote_backed());
    let mut bad_ip_cfg = cfg.clone();
    bad_ip_cfg.base_url = "http://192.168.1.25:20128/v1".into();
    bad_ip_cfg.endpoint_allowlist = vec![bad_ip_cfg.base_url.clone()];
    assert!(bad_ip_cfg.validate().is_err());

    // Without verified tokenization encoding or known model family, combo routing fails closed.
    let mut no_enc = cfg.clone();
    no_enc.tokenization_encoding = None;
    assert!(
        no_enc
            .to_adapter_config(combo_caps("CODING"), None)
            .is_err()
    );

    // RemoteApi still forbids CredentialSource::None, while LocalGatewayRemoteUpstream permits it.
    let mut remote_no_auth = cfg.to_adapter_config(combo_caps("CODING"), None).unwrap();
    remote_no_auth.capabilities.provider_kind = ProviderKind::RemoteApi;
    remote_no_auth.base_url = "https://api.example.com/v1".into();
    remote_no_auth.endpoint_allowlist = vec!["https://api.example.com/v1".into()];
    remote_no_auth.gateway_combo_routing = false;
    assert!(remote_no_auth.validate().is_err());

    // Build real HTTP transport for LocalGatewayRemoteUpstream and verify strict gate checks.
    let adapter_cfg = cfg.to_adapter_config(combo_caps("CODING"), None).unwrap();
    let policy = HttpTransportSecurityPolicy::hardened(
        Duration::from_millis(cfg.connect_timeout_ms),
        Duration::from_millis(cfg.request_timeout_ms),
        cfg.max_response_bytes,
    )
    .unwrap();
    let transport = ReqwestHttpTransport::new(&adapter_cfg, policy).unwrap();
    let adapter = OpenAiCompatibleAdapter::new(adapter_cfg, transport).unwrap();

    // Live dispatch gate rejects disabled live flag and rejects metadata-only sandbox isolation.
    let disabled_err = adapter.ensure_live_dispatch_gate(false, None).unwrap_err();
    assert_eq!(disabled_err.code(), "cost_reconciliation_failed");
    assert!(matches!(
        disabled_err,
        OpenAiAdapterError::DuplicateDispatchBlocked(_)
    ));

    let img_digest = "sha256:821bb3ffc1b53460a2e77431b2fd903254c63f60216ff5cd22d512763a2e05dc";
    let img_ref = format!("localhost/scenivox-runtime@{img_digest}");
    let sandbox_policy = SandboxPolicy::hardened_default(&img_ref).unwrap();
    let pin = PinnedDockerRuntime::scenivox_rootless(img_digest).unwrap();
    let mut isolation = VerifiedIsolation {
        runtime: DetectedRuntime {
            engine: OciEngine::Docker,
            transport: OciTransport::NativeLinux,
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
        image_reference: img_ref,
        image_digest: img_digest.into(),
        local_image_id: img_digest.into(),
        runtime_policy_sha256: sandbox_policy.policy_sha256().unwrap(),
        docker_context_name: Some(pin.context_name.clone()),
        unix_socket: Some(pin.unix_socket.clone()),
        seccomp_profile_sha256: Some(pin.seccomp_profile_sha256.clone()),
        isolation_verified: true,
        mandatory_controls_verified: true,
        empirically_verified: false,
        controls_evidence_source: ControlEvidenceSource::EngineMetadataOnly,
        empirical_verified_at_unix_secs: None,
        empirical_expires_at_unix_secs: None,
        empirical_probe_sha256: None,
    };
    let unverified_err = adapter
        .ensure_live_dispatch_gate(true, Some((&sandbox_policy, &pin, &isolation)))
        .unwrap_err();
    assert_eq!(unverified_err.code(), "cost_reconciliation_failed");
    assert!(matches!(
        unverified_err,
        OpenAiAdapterError::DuplicateDispatchBlocked(_)
    ));

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    isolation.empirically_verified = true;
    isolation.controls_evidence_source = ControlEvidenceSource::EmpiricallyVerified;
    isolation.runtime.metadata_source = ControlEvidenceSource::EmpiricallyVerified;
    isolation.empirical_verified_at_unix_secs = Some(now_secs);
    isolation.empirical_expires_at_unix_secs = Some(now_secs + 300);
    isolation.empirical_probe_sha256 = Some(sha256_hex(b"live-empirical-probe"));
    cfg.enabled = true;
    cfg.ensure_live_dispatch_enabled().unwrap();
    adapter
        .ensure_live_dispatch_gate(cfg.enabled, Some((&sandbox_policy, &pin, &isolation)))
        .unwrap();

    // Expired empirical evidence is rejected cleanly.
    let mut expired_iso = isolation.clone();
    expired_iso.empirical_verified_at_unix_secs = Some(now_secs.saturating_sub(400));
    expired_iso.empirical_expires_at_unix_secs = Some(now_secs.saturating_sub(1));
    assert!(
        adapter
            .ensure_live_dispatch_gate(cfg.enabled, Some((&sandbox_policy, &pin, &expired_iso)))
            .is_err()
    );
}

#[tokio::test]
async fn ninerouter_combo_coding_end_to_end_storyboard_agent_repair_over_http1() {
    let temp = tempfile::tempdir().unwrap();
    let pool = db::connect(&next_db_url(temp.path())).await.unwrap();
    let store = Store::new(pool.clone());

    let storyboard_json = json!({
        "synopsis": "Tokoh stick figure menyiapkan kopi pagi di dapur kecil sebelum berangkat kerja.",
        "hook": "Satu detik mengantuk bisa membuat cangkir kopi melayang.",
        "scenes": [
            {
                "order": 0,
                "duration_ms": 2000,
                "visual_direction": "Stick figure menguap sambil menyalakan ketel air di meja dapur.",
                "camera": "Medium vertical shot",
                "action": "Mengangkat cangkir dan menyalakan kompor",
                "narration": "Pagi dimulai dengan mata setengah terpejam.",
                "caption_hint": "Pagi baru dimulai...",
                "sfx_cues": []
            },
            {
                "order": 1,
                "duration_ms": 2000,
                "visual_direction": "Stick figure hampir menjatuhkan cangkir lalu menangkapnya dengan sigap.",
                "camera": "Close-up vertical shot",
                "action": "Menangkap cangkir yang terpeleset di udara",
                "narration": "Hampir saja kopinya tumpah ke lantai.",
                "caption_hint": "Nyaris tumpah!",
                "sfx_cues": []
            },
            {
                "order": 2,
                "duration_ms": 2000,
                "visual_direction": "Stick figure menyeruput kopi hangat dan melompat bersemangat.",
                "camera": "Wide 9:16 vertical shot",
                "action": "Menyeruput kopi dan mengangkat kedua tangan",
                "narration": "Satu teguk kopi langsung menyalakan semangat.",
                "caption_hint": "Siap beraksi!",
                "sfx_cues": []
            }
        ]
    });

    let replies = vec![
        // 1. Storyboard planning: Gateway responds with combo model + x-9router-model header.
        MockFixtureHttpReply {
            status: 200,
            headers: vec![("x-9router-model".into(), "cx/gpt-6-luna".into())],
            body: json!({
                "id": "chatcmpl-9r-plan",
                "model": "CODING",
                "choices": [{
                    "finish_reason": "stop",
                    "message": { "role": "assistant", "content": storyboard_json.to_string() }
                }],
                "usage": { "prompt_tokens": 180, "completion_tokens": 210, "total_tokens": 390 }
            })
            .to_string(),
            delay: Duration::ZERO,
        },
        // 2. Agent turn 0: write bug + try render; gateway exposes upstream in JSON `model`.
        MockFixtureHttpReply {
            status: 200,
            headers: Vec::new(),
            body: json!({
                "id": "chatcmpl-9r-turn0",
                "model": "cx/gpt-6-luna",
                "choices": [{
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [
                            {
                                "id": "call_write_0",
                                "type": "function",
                                "function": {
                                    "name": "workspace_write",
                                    "arguments": json!({
                                        "rel_path": "web/shots.js",
                                        "content": "export const POSITIVE_FPS = 0;\n"
                                    }).to_string()
                                }
                            },
                            {
                                "id": "call_run_0",
                                "type": "function",
                                "function": {
                                    "name": "skill_run_step",
                                    "arguments": json!({ "entrypoint_id": "render", "args": { "out": "final.mp4" } }).to_string()
                                }
                            }
                        ]
                    }
                }],
                "usage": { "prompt_tokens": 520, "completion_tokens": 60, "total_tokens": 580 }
            })
            .to_string(),
            delay: Duration::ZERO,
        },
        // 3. Agent turn 1: repair + re-run + probe_media; gateway omits upstream ("CODING" only).
        MockFixtureHttpReply {
            status: 200,
            headers: Vec::new(),
            body: json!({
                "id": "chatcmpl-9r-turn1",
                "model": "CODING",
                "choices": [{
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [
                            {
                                "id": "call_patch_1",
                                "type": "function",
                                "function": {
                                    "name": "workspace_patch",
                                    "arguments": "{\"rel_path\":\"web/shots.js\",\"edits\":[{\"old_text\":\"POSITIVE_FPS = 0\",\"new_text\":\"POSITIVE_FPS = 30\"}]}"
                                }
                            },
                            {
                                "id": "call_run_1",
                                "type": "function",
                                "function": {
                                    "name": "skill_run_step",
                                    "arguments": "{\"entrypoint_id\":\"render\",\"args\":{\"out\":\"final.mp4\"}}"
                                }
                            },
                            {
                                "id": "call_probe_1",
                                "type": "function",
                                "function": {
                                    "name": "probe_media",
                                    "arguments": "{\"rel_path\":\"final.mp4\"}"
                                }
                            }
                        ]
                    }
                }],
                "usage": { "prompt_tokens": 620, "completion_tokens": 90, "total_tokens": 710 }
            })
            .to_string(),
            delay: Duration::ZERO,
        },
        // 4. Agent turn 2: complete_composition via x-upstream-model header.
        MockFixtureHttpReply {
            status: 200,
            headers: vec![("x-upstream-model".into(), "cx/gpt-6-luna".into())],
            body: json!({
                "id": "chatcmpl-9r-turn2",
                "model": "CODING",
                "choices": [{
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_done_2",
                            "type": "function",
                            "function": {
                                "name": "complete_composition",
                                "arguments": json!({
                                    "output_rel_path": "final.mp4",
                                    "summary": "Animasi stick figure kopi pagi 6 detik selesai setelah perbaikan FPS."
                                }).to_string()
                            }
                        }]
                    }
                }],
                "usage": { "prompt_tokens": 710, "completion_tokens": 50, "total_tokens": 760 }
            })
            .to_string(),
            delay: Duration::ZERO,
        },
    ];

    let (addr, captured_http, srv) = spawn_fixture_gateway(replies).await;
    let base_url = format!("http://127.0.0.1:{}/v1", addr.port());

    let live_cfg = LiveLlmConfig {
        enabled: true,
        provider_id: "ninerouter".into(),
        model_id: "CODING".into(),
        adapter_version: "1.0.0".into(),
        provider_kind: ProviderKind::LocalGatewayRemoteUpstream,
        base_url: base_url.clone(),
        endpoint_allowlist: vec![base_url.clone()],
        gateway_combo_routing: true,
        tokenization_encoding: Some("o200k_base".into()),
        credential_env_var: String::new(),
        request_timeout_ms: 5_000,
        connect_timeout_ms: 1_000,
        max_request_bytes: 256 * 1024,
        max_response_bytes: 512 * 1024,
        max_input_tokens: 32_768,
        max_output_tokens: 8_192,
        max_cost_micros: 1_000_000,
    };
    let adapter_cfg = live_cfg
        .to_adapter_config(
            combo_caps_with_availability("CODING", ProviderAvailability::Unknown),
            None,
        )
        .unwrap();
    let sec_policy = HttpTransportSecurityPolicy::hardened(
        Duration::from_millis(1_000),
        Duration::from_millis(5_000),
        512 * 1024,
    )
    .unwrap();
    let transport = ReqwestHttpTransport::new(&adapter_cfg, sec_policy).unwrap();
    let adapter = OpenAiCompatibleAdapter::new(adapter_cfg, transport).unwrap();

    let (_model, lease, consent) = register_unpriced_combo_with_request_limit(
        &store,
        "CODING",
        vec![
            CostScope::StoryboardPlanning,
            CostScope::SceneCodeGeneration,
        ],
        64_000,
        Some(4),
    )
    .await;

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
            title: "Kopi Pagi Stick Figure".into(),
            idea: "Stick figure membuat kopi pagi di dapur kecil".into(),
            target: target.clone(),
            model_selection: combo_selection("CODING"),
        })
        .await
        .unwrap();

    let cancel = CancellationToken::new();
    let planned = plan_storyboard_with_adapter(AdapterStoryboardRequest {
        store: &store,
        project_id: &project.id,
        idempotency_key: Some("00000000-0000-4000-8000-000000000201"),
        reservation_idempotency_key: Some("00000000-0000-4000-8000-000000000202"),
        lease: &lease,
        cost_consent_id: &consent.id,
        adapter: &adapter,
        input: &PlanStoryboard {
            schema_version: "1".into(),
            idea: project.idea.clone(),
            target,
            expected_revision_id: None,
            model_selection: combo_selection("CODING"),
        },
        max_output_tokens: 2_048,
        cancel: &cancel,
    })
    .await
    .unwrap();

    assert_eq!(
        planned.revision.content.models.storyboard_planning.model_id,
        "CODING"
    );
    assert_eq!(planned.planning_outcome.requested_model_id, "CODING");
    assert_eq!(
        planned.planning_outcome.actual_upstream_model.as_deref(),
        Some("cx/gpt-6-luna")
    );
    assert_eq!(
        planned.planning_outcome.inference_provenance,
        InferenceProvenance::LocalGatewayRemoteInference
    );
    assert!(!planned.planning_outcome.response.simulated);
    assert_eq!(
        planned
            .planning_outcome
            .reservation
            .requested_model_id
            .as_deref(),
        Some("CODING")
    );
    assert_eq!(
        planned
            .planning_outcome
            .reservation
            .actual_upstream_model
            .as_deref(),
        Some("cx/gpt-6-luna")
    );
    assert_eq!(planned.planning_outcome.reservation.actual_micros, Some(0));
    let derived = planned
        .planning_outcome
        .derived_availability_evidence
        .as_ref()
        .expect("genuine response must derive availability evidence");
    assert!(derived.derived_from_authorized_response);
    assert_eq!(
        derived.source_reservation_id.as_deref(),
        Some(planned.planning_outcome.reservation.id.as_str())
    );
    let updated_model = store
        .list_models()
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.provider_id == "ninerouter" && m.model_id == "CODING")
        .unwrap();
    assert_eq!(updated_model.availability, "available");

    store
        .review_revision(
            &project.id,
            &planned.revision.id,
            None,
            ReviewCommand {
                schema_version: "1".into(),
                content_sha256: planned.revision.content_sha256.clone(),
            },
        )
        .await
        .unwrap();
    let (approval, _, _) = store
        .approve_revision(
            &project.id,
            &planned.revision.id,
            None,
            ReviewCommand {
                schema_version: "1".into(),
                content_sha256: planned.revision.content_sha256.clone(),
            },
        )
        .await
        .unwrap();

    let skill_store_dir = temp.path().join("skills");
    let skill_src = create_stick_figure_skill_fixture(temp.path());
    let (skill, _) = store
        .import_skill(&skill_store_dir, None, &skill_src)
        .await
        .unwrap();

    let job_id = Uuid::new_v4().to_string();
    let staging_root = temp.path().join("staging");
    let workspace_dir = staging_root.join(&job_id);
    fs::create_dir_all(&workspace_dir).unwrap();

    let backend = ScriptedSimulatedBackend::new(
        |req| {
            let shots = req
                .workspace_files
                .get("web/shots.js")
                .and_then(|f| f.utf8_text.as_deref())
                .unwrap_or("");
            if shots.contains("POSITIVE_FPS = 0") {
                Ok(SimulatedStepOutcome {
                    simulated: true,
                    succeeded: false,
                    exit_code: 2,
                    error_code: Some("simulated_render_error".into()),
                    stdout: String::new(),
                    stderr: "SIMULATED FAILURE: POSITIVE_FPS must be > 0".into(),
                    simulated_artifacts: Vec::new(),
                })
            } else {
                Ok(SimulatedStepOutcome {
                    simulated: true,
                    succeeded: true,
                    exit_code: 0,
                    error_code: None,
                    stdout: "SIMULATED RENDER OK".into(),
                    stderr: String::new(),
                    simulated_artifacts: vec![SimulatedArtifactOutput {
                        rel_path: "final.mp4".into(),
                        content_bytes: b"SIMULATED_MP4_PLACEHOLDER".to_vec(),
                        simulated: true,
                        description: "Simulated 6s stick figure composition".into(),
                    }],
                })
            }
        },
        |req| {
            Ok(SimulatedMediaProbeResult {
                simulated: true,
                real_mp4_rendered: false,
                rel_path: req.rel_path.clone(),
                sha256: req.sha256.clone(),
                byte_size: req.byte_size,
                width: 1080,
                height: 1920,
                fps: 30,
                duration_ms: 6_000,
                has_audio: false,
                motion_detected: true,
                notice: "SIMULATED_MEDIA_PROBE".into(),
            })
        },
    );

    let agent_out = AgentRuntime::run_with_openai_adapter(
        &AgentConfig {
            execution_mode: AgentExecutionMode::OpenAiCompatibleAdapter,
            require_smoke_verified_skill: false,
            limits: AgentLimits {
                max_turns: 8,
                max_tool_calls: 24,
                max_repairs: 4,
                max_llm_output_tokens_per_turn: 8_192,
                max_total_llm_tokens: 100_000,
                ..AgentLimits::default()
            },
        },
        &adapter,
        AgentLedgerContext {
            store: &store,
            lease: &lease,
            model: &updated_model,
            consent_id: &consent.id,
            cancel: &cancel,
            idempotency_prefix: "9r-agent-1",
        },
        &AgentTaskInput {
            job_id,
            revision: planned.revision.clone(),
            approval,
            skill,
            skill_store_dir,
            workspace_dir,
            staging_root: Some(staging_root),
            narration: None,
            timeline: None,
        },
        &backend,
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(
        planned
            .revision
            .content
            .models
            .scene_code_generation
            .model_id,
        "CODING"
    );
    assert_eq!(agent_out.run.provenance.repair_attempts.len(), 1);
    assert_eq!(agent_out.turn_cost_traces.len(), 3);
    assert_eq!(
        agent_out.inference_provenance,
        InferenceProvenance::LocalGatewayRemoteInference
    );
    assert_eq!(
        agent_out.turn_cost_traces[0]
            .actual_upstream_model
            .as_deref(),
        Some("cx/gpt-6-luna")
    );
    assert_eq!(agent_out.turn_cost_traces[1].actual_upstream_model, None);
    assert_eq!(
        agent_out.turn_cost_traces[2]
            .actual_upstream_model
            .as_deref(),
        Some("cx/gpt-6-luna")
    );
    for trace in &agent_out.turn_cost_traces {
        assert_eq!(trace.requested_model_id, "CODING");
        let saved = store
            .get_budget_reservation(&trace.reservation_id)
            .await
            .unwrap();
        assert_eq!(saved.requested_model_id.as_deref(), Some("CODING"));
        assert_eq!(saved.actual_upstream_model, trace.actual_upstream_model);
        assert!(saved.latency_ms.is_some());
        assert_eq!(
            saved.inference_provenance,
            Some(InferenceProvenance::LocalGatewayRemoteInference)
        );
    }

    let captured = captured_http.lock().await;
    assert_eq!(captured.len(), 4);
    for raw in captured.iter() {
        assert!(
            raw.contains("\"model\":\"CODING\""),
            "raw wire missing CODING: {raw}"
        );
    }
    assert!(captured[0].contains("\"json_object\""));
    assert!(captured[1].contains("\"tools\""));

    srv.await.unwrap();
}

#[tokio::test]
async fn gateway_cancellation_and_output_overage_enforce_ledger_and_preserve_upstream_telemetry() {
    let temp = tempfile::tempdir().unwrap();
    let pool = db::connect(&next_db_url(temp.path())).await.unwrap();
    let store = Store::new(pool.clone());

    let replies = vec![
        MockFixtureHttpReply {
            status: 200,
            headers: vec![("x-9router-model".into(), "cx/gpt-6-luna".into())],
            body: json!({
                "id": "chatcmpl-slow",
                "model": "CODING",
                "choices": [{ "finish_reason": "stop", "message": { "role": "assistant", "content": "{}" } }],
                "usage": { "prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110 }
            })
            .to_string(),
            delay: Duration::from_millis(600),
        },
        MockFixtureHttpReply {
            status: 200,
            headers: vec![("x-9router-model".into(), "cx/gpt-6-luna".into())],
            body: json!({
                "id": "chatcmpl-overage",
                "model": "CODING",
                "choices": [{
                    "finish_reason": "length",
                    "message": { "role": "assistant", "content": "{\"synopsis\":\"truncated\"}" }
                }],
                "usage": { "prompt_tokens": 150, "completion_tokens": 2500, "total_tokens": 2650 }
            })
            .to_string(),
            delay: Duration::ZERO,
        },
    ];
    let (addr, _, srv) = spawn_fixture_gateway(replies).await;
    let base_url = format!("http://127.0.0.1:{}/v1", addr.port());

    let live_cfg = LiveLlmConfig {
        enabled: true,
        provider_id: "ninerouter".into(),
        model_id: "CODING".into(),
        adapter_version: "1.0.0".into(),
        provider_kind: ProviderKind::LocalGatewayRemoteUpstream,
        base_url: base_url.clone(),
        endpoint_allowlist: vec![base_url.clone()],
        gateway_combo_routing: true,
        tokenization_encoding: Some("o200k_base".into()),
        credential_env_var: String::new(),
        request_timeout_ms: 5_000,
        connect_timeout_ms: 1_000,
        max_request_bytes: 256 * 1024,
        max_response_bytes: 512 * 1024,
        max_input_tokens: 32_768,
        max_output_tokens: 8_192,
        max_cost_micros: 2_000_000,
    };
    let adapter_cfg = live_cfg
        .to_adapter_config(
            combo_caps_with_availability("CODING", ProviderAvailability::Unknown),
            None,
        )
        .unwrap();
    let sec_policy = HttpTransportSecurityPolicy::hardened(
        Duration::from_millis(1_000),
        Duration::from_millis(5_000),
        512 * 1024,
    )
    .unwrap();
    let transport = ReqwestHttpTransport::new(&adapter_cfg, sec_policy).unwrap();
    let adapter = OpenAiCompatibleAdapter::new(adapter_cfg, transport).unwrap();

    let (model, lease, consent) = register_unpriced_combo_with_request_limit(
        &store,
        "CODING",
        vec![CostScope::StoryboardPlanning],
        64_000,
        Some(4),
    )
    .await;

    let req = LlmRequest {
        schema_version: PROVIDER_SCHEMA_VERSION.into(),
        task_role: TaskRole::StoryboardPlanning,
        provider_id: "ninerouter".into(),
        model_id: "CODING".into(),
        adapter_version: "1.0.0".into(),
        parameters: model.parameters.clone(),
        max_output_tokens: 2_048,
        response_format: ResponseFormat::JsonObject,
        system_prompt: "Plan.".into(),
        messages: vec![LlmMessage::user("Buat storyboard 6 detik.")],
        tools: Vec::new(),
    };
    let snapshot_sha = compute_model_snapshot_sha256(&model, None).unwrap();
    let req_sha = req.canonical_request_sha256().unwrap();
    let est_in = adapter.trusted_input_tokens(&req).unwrap();

    // 1. Mid-flight cancellation
    let cancel = CancellationToken::new();
    let cancel_bg = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        cancel_bg.cancel();
    });
    let cancel_idem = "00000000-0000-4000-8000-000000000301";
    let err = adapter
        .execute_with_new_reservation(
            DispatchContext {
                store: &store,
                lease: &lease,
                project_id: None,
                job_id: None,
                model: &model,
                snapshot: None,
                cancel: &cancel,
            },
            ReserveBudgetRequest {
                cost_consent_id: consent.id.clone(),
                project_id: None,
                job_id: None,
                provider_id: "ninerouter".into(),
                model_id: "CODING".into(),
                scope: CostScope::StoryboardPlanning,
                owner_id: lease.owner_id.clone(),
                lease_epoch: lease.lease_epoch,
                canonical_request_sha256: req_sha.clone(),
                model_snapshot_sha256: snapshot_sha.clone(),
                endpoint_identity: adapter.config().endpoint_identity().unwrap(),
                estimated_input_tokens: est_in,
                max_output_tokens: 2_048,
            },
            Some(cancel_idem),
            &req,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, OpenAiAdapterError::Cancelled));
    assert_eq!(
        store.get_cost_consent(&consent.id).await.unwrap().status,
        CostConsentStatus::Halted
    );

    // 2. Output token overage reconciles to SettledOverage on a fresh consent, preserves upstream
    //    telemetry, and halts the consent.
    let (overage_consent, _) = store
        .create_cost_consent_with_request_limit(
            None,
            CreateCostConsent {
                schema_version: PROVIDER_SCHEMA_VERSION.into(),
                project_id: None,
                provider_id: "ninerouter".into(),
                model_id: "CODING".into(),
                scopes: vec![CostScope::StoryboardPlanning],
                max_cost_micros: 0,
                max_total_tokens: Some(64_000),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
            Some(1),
        )
        .await
        .unwrap();
    let active_cancel = CancellationToken::new();
    let overage_reserve_req = ReserveBudgetRequest {
        cost_consent_id: overage_consent.id.clone(),
        project_id: None,
        job_id: None,
        provider_id: "ninerouter".into(),
        model_id: "CODING".into(),
        scope: CostScope::StoryboardPlanning,
        owner_id: lease.owner_id.clone(),
        lease_epoch: lease.lease_epoch,
        canonical_request_sha256: req_sha,
        model_snapshot_sha256: snapshot_sha,
        endpoint_identity: adapter.config().endpoint_identity().unwrap(),
        estimated_input_tokens: est_in,
        max_output_tokens: 2_048,
    };
    let (overage_res, _) = store
        .reserve_budget(
            Some("00000000-0000-4000-8000-000000000302"),
            overage_reserve_req,
        )
        .await
        .unwrap();
    let err = adapter
        .execute_for_reservation(
            DispatchContext {
                store: &store,
                lease: &lease,
                project_id: None,
                job_id: None,
                model: &model,
                snapshot: None,
                cancel: &active_cancel,
            },
            &overage_res.id,
            &req,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, OpenAiAdapterError::CostLimitExceeded(_)));

    let saved = store.get_budget_reservation(&overage_res.id).await.unwrap();
    assert_eq!(saved.status, ReservationStatus::SettledOverage);
    assert_eq!(saved.actual_output_tokens, Some(2500));
    assert_eq!(saved.requested_model_id.as_deref(), Some("CODING"));
    assert_eq!(
        saved.actual_upstream_model.as_deref(),
        Some("cx/gpt-6-luna")
    );
    assert_eq!(
        store
            .get_cost_consent(&overage_consent.id)
            .await
            .unwrap()
            .status,
        CostConsentStatus::Halted
    );

    srv.await.unwrap();
}

#[tokio::test]
async fn single_request_storyboard_smoke_enforces_boundary_gates_and_unpriced_ledger() {
    let temp = tempfile::tempdir().unwrap();
    let pool = db::connect(&next_db_url(temp.path())).await.unwrap();
    let store = Store::new(pool.clone());

    let storyboard_json = json!({
        "synopsis": "Tokoh stick figure menyiapkan kopi pagi di dapur kecil sebelum berangkat kerja.",
        "hook": "Satu detik mengantuk bisa membuat cangkir kopi melayang.",
        "scenes": [
            {
                "order": 0,
                "duration_ms": 3000,
                "visual_direction": "Stick figure menyalakan kompor dan mengangkat cangkir kopi.",
                "camera": "Medium vertical shot",
                "action": "Menyalakan ketel air di meja dapur",
                "narration": "Pagi dimulai dengan mata setengah terpejam.",
                "caption_hint": "Pagi baru dimulai...",
                "sfx_cues": []
            },
            {
                "order": 1,
                "duration_ms": 3000,
                "visual_direction": "Stick figure menyeruput kopi hangat dan melompat bersemangat.",
                "camera": "Wide 9:16 vertical shot",
                "action": "Menyeruput kopi dan mengangkat kedua tangan",
                "narration": "Satu teguk kopi langsung menyalakan semangat.",
                "caption_hint": "Siap beraksi!",
                "sfx_cues": []
            }
        ]
    });

    let replies = vec![MockFixtureHttpReply {
        status: 200,
        headers: vec![("x-9router-model".into(), "cx/gpt-6-luna".into())],
        body: json!({
            "id": "chatcmpl-smoke-1",
            "model": "CODING",
            "choices": [{
                "finish_reason": "stop",
                "message": { "role": "assistant", "content": storyboard_json.to_string() }
            }],
            "usage": { "prompt_tokens": 170, "completion_tokens": 220, "total_tokens": 390 }
        })
        .to_string(),
        delay: Duration::from_millis(5),
    }];
    let (addr, captured_http, srv) = spawn_fixture_gateway(replies).await;
    let base_url = format!("http://127.0.0.1:{}/v1", addr.port());

    let (model, lease, consent) = register_unpriced_combo_with_request_limit(
        &store,
        "CODING",
        vec![CostScope::StoryboardPlanning],
        2_048,
        Some(1),
    )
    .await;
    assert_eq!(model.availability, "unknown");

    // Unpriced upstream quota forbids omitting max_total_tokens on consent.
    let no_tok_cap_err = store
        .create_cost_consent_with_request_limit(
            None,
            CreateCostConsent {
                schema_version: PROVIDER_SCHEMA_VERSION.into(),
                project_id: None,
                provider_id: "ninerouter".into(),
                model_id: "CODING".into(),
                scopes: vec![CostScope::StoryboardPlanning],
                max_cost_micros: 0,
                max_total_tokens: None,
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
            Some(1),
        )
        .await
        .unwrap_err();
    assert_eq!(no_tok_cap_err.code(), "invalid_schema");

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
            title: "Smoke Storyboard 6s".into(),
            idea: "Stick figure membuat kopi pagi di dapur kecil".into(),
            target: target.clone(),
            model_selection: combo_selection("CODING"),
        })
        .await
        .unwrap();

    // 1. Disabled live flag MUST block execution before dispatching HTTP.
    let disabled_live_cfg = LiveLlmConfig {
        enabled: false,
        provider_id: "ninerouter".into(),
        model_id: "CODING".into(),
        adapter_version: "1.0.0".into(),
        provider_kind: ProviderKind::LocalGatewayRemoteUpstream,
        base_url: base_url.clone(),
        endpoint_allowlist: vec![base_url.clone()],
        gateway_combo_routing: true,
        tokenization_encoding: Some("o200k_base".into()),
        credential_env_var: String::new(),
        request_timeout_ms: 5_000,
        connect_timeout_ms: 1_000,
        max_request_bytes: 256 * 1024,
        max_response_bytes: 512 * 1024,
        max_input_tokens: 1_024,
        max_output_tokens: 1_024,
        max_cost_micros: 0,
    };
    let disabled_adapter_cfg = disabled_live_cfg
        .to_adapter_config(combo_caps("CODING"), None)
        .unwrap();
    let sec_policy = HttpTransportSecurityPolicy::hardened(
        Duration::from_millis(1_000),
        Duration::from_millis(5_000),
        512 * 1024,
    )
    .unwrap();
    let disabled_adapter = OpenAiCompatibleAdapter::new(
        disabled_adapter_cfg.clone(),
        ReqwestHttpTransport::new(&disabled_adapter_cfg, sec_policy.clone()).unwrap(),
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let disabled_err = plan_storyboard_with_adapter(AdapterStoryboardRequest {
        store: &store,
        project_id: &project.id,
        idempotency_key: Some("00000000-0000-4000-8000-000000000401"),
        reservation_idempotency_key: Some("00000000-0000-4000-8000-000000000402"),
        lease: &lease,
        cost_consent_id: &consent.id,
        adapter: &disabled_adapter,
        input: &PlanStoryboard {
            schema_version: "1".into(),
            idea: project.idea.clone(),
            target: target.clone(),
            expected_revision_id: None,
            model_selection: combo_selection("CODING"),
        },
        max_output_tokens: 1_024,
        cancel: &cancel,
    })
    .await
    .unwrap_err();
    assert!(
        disabled_err.to_string().contains("unavailable")
            || disabled_err
                .to_string()
                .contains("remote-backed LLM dispatch is disabled"),
        "unexpected disabled error: {disabled_err}"
    );
    assert!(
        disabled_adapter
            .ensure_live_dispatch_gate(false, None)
            .unwrap_err()
            .to_string()
            .contains("remote-backed LLM dispatch is disabled")
    );
    assert!(captured_http.lock().await.is_empty());

    // 2. Authorized live single request executes once, records telemetry & settlement, and derives availability.
    let mut enabled_live_cfg = disabled_live_cfg;
    enabled_live_cfg.enabled = true;
    let enabled_adapter_cfg = enabled_live_cfg
        .to_adapter_config(
            combo_caps_with_availability("CODING", ProviderAvailability::Unknown),
            None,
        )
        .unwrap();
    let adapter = OpenAiCompatibleAdapter::new(
        enabled_adapter_cfg.clone(),
        ReqwestHttpTransport::new(&enabled_adapter_cfg, sec_policy).unwrap(),
    )
    .unwrap();

    let planned = plan_storyboard_with_adapter(AdapterStoryboardRequest {
        store: &store,
        project_id: &project.id,
        idempotency_key: Some("00000000-0000-4000-8000-000000000403"),
        reservation_idempotency_key: Some("00000000-0000-4000-8000-000000000404"),
        lease: &lease,
        cost_consent_id: &consent.id,
        adapter: &adapter,
        input: &PlanStoryboard {
            schema_version: "1".into(),
            idea: project.idea.clone(),
            target: target.clone(),
            expected_revision_id: None,
            model_selection: combo_selection("CODING"),
        },
        max_output_tokens: 1_024,
        cancel: &cancel,
    })
    .await
    .unwrap();

    assert_eq!(planned.planning_outcome.requested_model_id, "CODING");
    assert_eq!(
        planned.planning_outcome.actual_upstream_model.as_deref(),
        Some("cx/gpt-6-luna")
    );
    assert_eq!(
        planned.planning_outcome.inference_provenance,
        InferenceProvenance::LocalGatewayRemoteInference
    );
    assert_eq!(
        planned.planning_outcome.reservation.status,
        ReservationStatus::Settled
    );
    assert_eq!(planned.planning_outcome.reservation.actual_micros, Some(0));
    assert!(planned.planning_outcome.reservation.latency_ms.is_some());
    assert_eq!(
        planned.planning_outcome.reservation.actual_input_tokens,
        Some(170)
    );
    assert_eq!(
        planned.planning_outcome.reservation.actual_output_tokens,
        Some(220)
    );
    let derived = planned
        .planning_outcome
        .derived_availability_evidence
        .as_ref()
        .unwrap();
    assert!(derived.derived_from_authorized_response);
    assert_eq!(
        derived.source_reservation_id.as_deref(),
        Some(planned.planning_outcome.reservation.id.as_str())
    );
    assert_eq!(captured_http.lock().await.len(), 1);

    let consent_after = store.get_cost_consent(&consent.id).await.unwrap();
    assert_eq!(consent_after.max_requests, Some(1));
    assert_eq!(consent_after.spent_requests, 1);
    assert_eq!(consent_after.reserved_requests, 0);

    // 3. Second request on max_requests=1 consent MUST be blocked.
    let second_err = store
        .reserve_budget(
            Some("00000000-0000-4000-8000-000000000405"),
            ReserveBudgetRequest {
                cost_consent_id: consent.id.clone(),
                project_id: Some(project.id.clone()),
                job_id: None,
                provider_id: "ninerouter".into(),
                model_id: "CODING".into(),
                scope: CostScope::StoryboardPlanning,
                owner_id: lease.owner_id.clone(),
                lease_epoch: lease.lease_epoch,
                canonical_request_sha256: planned
                    .planning_outcome
                    .reservation
                    .canonical_request_sha256
                    .clone(),
                model_snapshot_sha256: planned
                    .planning_outcome
                    .reservation
                    .model_snapshot_sha256
                    .clone(),
                endpoint_identity: base_url,
                estimated_input_tokens: 170,
                max_output_tokens: 1_024,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(second_err.code(), "cost_limit_exceeded");

    srv.await.unwrap();
}

#[test]
fn prepare_indonesian_six_second_smoke_test_limits_without_remote_execution() {
    let live_cfg = LiveLlmConfig {
        enabled: false,
        provider_id: "ninerouter".into(),
        model_id: "CODING".into(),
        adapter_version: "1.0.0".into(),
        provider_kind: ProviderKind::LocalGatewayRemoteUpstream,
        base_url: "http://127.0.0.1:20128/v1".into(),
        endpoint_allowlist: vec!["http://127.0.0.1:20128/v1".into()],
        gateway_combo_routing: true,
        tokenization_encoding: Some("o200k_base".into()),
        credential_env_var: String::new(),
        request_timeout_ms: 45_000,
        connect_timeout_ms: 3_000,
        max_request_bytes: 256 * 1024,
        max_response_bytes: 512 * 1024,
        max_input_tokens: 2_048,
        max_output_tokens: 1_024,
        max_cost_micros: 0,
    };
    let profile =
        ModelTokenizationProfile::for_verified_encoding("CODING", "o200k_base", true).unwrap();
    let adapter_cfg = live_cfg
        .to_adapter_config(
            combo_caps_with_availability("CODING", ProviderAvailability::Unknown),
            Some(profile.clone()),
        )
        .unwrap();

    let storyboard_req = LlmRequest {
        schema_version: PROVIDER_SCHEMA_VERSION.into(),
        task_role: TaskRole::StoryboardPlanning,
        provider_id: "ninerouter".into(),
        model_id: "CODING".into(),
        adapter_version: "1.0.0".into(),
        parameters: Parameters {
            temperature: 0.0,
            seed: Some(42),
        },
        max_output_tokens: 1_024,
        response_format: ResponseFormat::JsonObject,
        system_prompt: "You are Scenivox's bounded storyboard planner. Return strictly one JSON object matching the requested storyboard schema with 1..=60 scenes whose durations sum strictly to target.duration_ms.".into(),
        messages: vec![LlmMessage::user(
            json!({
                "idea": "Stick figure menyeduh kopi pagi, nyaris menumpahkan cangkir, lalu bersemangat memulai hari.",
                "target": {
                    "aspect_ratio": "9:16",
                    "duration_ms": 6000,
                    "language": "id-ID",
                    "style": "stick-figure"
                },
                "required_schema": {
                    "synopsis": "string (id-ID)",
                    "hook": "string (id-ID)",
                    "scenes": [{
                        "order": 0,
                        "duration_ms": "integer > 0 summing to 6000",
                        "visual_direction": "string",
                        "camera": "string",
                        "action": "string",
                        "narration": "string (id-ID)",
                        "caption_hint": "string (id-ID)",
                        "sfx_cues": []
                    }]
                }
            })
            .to_string(),
        )],
        tools: Vec::new(),
    };
    let plan_in_tokens =
        compute_trusted_input_tokens_for_profile(&storyboard_req, &profile).unwrap() as u64;

    let max_storyboard_requests: u64 = 1;
    let max_composition_turns: u64 = 0;
    let max_total_requests = max_storyboard_requests + max_composition_turns;
    let max_storyboard_output: u64 = 1_024;
    let max_aggregate_tokens_total = plan_in_tokens + max_storyboard_output;

    assert_eq!(max_total_requests, 1);
    assert_eq!(max_storyboard_output, 1_024);
    assert!(
        max_aggregate_tokens_total <= u64::from(adapter_cfg.capabilities.max_context_tokens),
        "single smoke request {max_aggregate_tokens_total} must stay <= 272000"
    );

    let proposal_sha = sha256_hex(
        json!({
            "combo": "CODING",
            "base_url": live_cfg.base_url,
            "max_requests": max_total_requests,
            "max_output_tokens": max_storyboard_output,
            "composition_and_repair_disabled": true
        })
        .to_string()
        .as_bytes(),
    );
    let plan_summary = json!({
        "gateway_base_url": live_cfg.base_url,
        "combo_model_id": live_cfg.model_id,
        "gateway_combo_routing": live_cfg.gateway_combo_routing,
        "pricing_mode": "UNPRICED",
        "tokenization_encoding": live_cfg.tokenization_encoding,
        "safety_margin_bps": profile.safety_margin_bps,
        "advertised_context_window": adapter_cfg.capabilities.max_context_tokens,
        "advertised_max_output": adapter_cfg.capabilities.max_output_tokens,
        "composition_and_repair_enabled": false,
        "automatic_retries_enabled": false,
        "max_requests": {
            "storyboard_planning": max_storyboard_requests,
            "composition_and_repair_turns": max_composition_turns,
            "total": max_total_requests
        },
        "token_ceilings": {
            "storyboard_initial_input_conservative": plan_in_tokens,
            "storyboard_max_output": max_storyboard_output,
            "aggregate_input_ceiling": plan_in_tokens,
            "aggregate_output_ceiling": max_storyboard_output,
            "aggregate_total_ceiling": max_aggregate_tokens_total
        },
        "target": {
            "language": "id-ID",
            "duration_ms": 6000,
            "aspect_ratio": "9:16"
        },
        "proposal_sha256": proposal_sha,
        "remote_inference_executed": false
    });
    println!("PROPOSED_NINEROUTER_SMOKE_PLAN={plan_summary}");
}

#[tokio::test]
#[ignore = "requires explicit CEO quota authorization (SCENIVOX_CEO_QUOTA_AUTHORIZED=1 and SCENIVOX_LLM_ENABLED=true)"]
async fn execute_first_live_ninerouter_storyboard_smoke() {
    assert_eq!(
        std::env::var("SCENIVOX_CEO_QUOTA_AUTHORIZED").as_deref(),
        Ok("1"),
        "CEO quota authorization env var SCENIVOX_CEO_QUOTA_AUTHORIZED=1 is required"
    );
    assert_eq!(
        std::env::var("SCENIVOX_LLM_ENABLED").as_deref(),
        Ok("true"),
        "live LLM flag SCENIVOX_LLM_ENABLED=true is required"
    );

    let temp = tempfile::tempdir().unwrap();
    let pool = db::connect(&next_db_url(temp.path())).await.unwrap();
    let store = Store::new(pool.clone());

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
        request_timeout_ms: 45_000,
        connect_timeout_ms: 3_000,
        max_request_bytes: 256 * 1024,
        max_response_bytes: 512 * 1024,
        max_input_tokens: 2_048,
        max_output_tokens: 1_024,
        max_cost_micros: 0,
    };
    let adapter_cfg = live_cfg
        .to_adapter_config(
            combo_caps_with_availability("CODING", ProviderAvailability::Unknown),
            None,
        )
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

    let (model, lease, consent) = register_unpriced_combo_with_request_limit(
        &store,
        "CODING",
        vec![CostScope::StoryboardPlanning],
        2_048,
        Some(1),
    )
    .await;
    assert_eq!(model.availability, "unknown");

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
            title: "First Live 9Router Smoke (6s Indonesian)".into(),
            idea: "Stick figure menyeduh kopi pagi, nyaris menumpahkan cangkir, lalu bersemangat memulai hari.".into(),
            target: target.clone(),
            model_selection: combo_selection("CODING"),
        })
        .await
        .unwrap();

    let cancel = CancellationToken::new();
    let planned_res = plan_storyboard_with_adapter(AdapterStoryboardRequest {
        store: &store,
        project_id: &project.id,
        idempotency_key: Some("00000000-0000-4000-8000-000000000901"),
        reservation_idempotency_key: Some("00000000-0000-4000-8000-000000000902"),
        lease: &lease,
        cost_consent_id: &consent.id,
        adapter: &adapter,
        input: &PlanStoryboard {
            schema_version: "1".into(),
            idea: project.idea.clone(),
            target,
            expected_revision_id: None,
            model_selection: combo_selection("CODING"),
        },
        max_output_tokens: 1_024,
        cancel: &cancel,
    })
    .await;

    let planned = match planned_res {
        Ok(ok) => ok,
        Err(err) => {
            let consent_state = store.get_cost_consent(&consent.id).await.ok();
            let res_rows = sqlx::query_as::<
                _,
                (
                    String,
                    Option<String>,
                    Option<String>,
                    Option<i64>,
                    Option<i64>,
                    Option<i64>,
                    Option<i64>,
                    String,
                ),
            >(
                "SELECT id, requested_model_id, actual_upstream_model, latency_ms, actual_input_tokens, actual_output_tokens, actual_micros, status FROM budget_reservations WHERE cost_consent_id=?",
            )
            .bind(&consent.id)
            .fetch_all(&pool)
            .await
            .unwrap_or_default();
            let ev_rows = sqlx::query_as::<
                _,
                (String, String, String, String, String, i64, Option<String>),
            >(
                "SELECT provider_id, model_id, availability, probe_provenance, echoed_model_id, derived_from_authorized_response, source_reservation_id FROM model_availability_evidence",
            )
            .fetch_all(&pool)
            .await
            .unwrap_or_default();
            println!(
                "LIVE_NINEROUTER_STORYBOARD_SMOKE_ERR={}",
                json!({
                    "error": err.to_string(),
                    "consent": consent_state,
                    "reservations": res_rows,
                    "availability_evidence": ev_rows,
                })
            );
            panic!("live smoke failed: {err}");
        }
    };

    let report = json!({
        "storyboard": planned.revision.content.storyboard,
        "sanitized_metadata": {
            "requested_combo": planned.planning_outcome.requested_model_id,
            "actual_upstream_model": planned.planning_outcome.actual_upstream_model,
            "inference_provenance": planned.planning_outcome.inference_provenance,
            "latency_ms": planned.planning_outcome.latency_ms,
            "response_sha256": planned.planning_outcome.response.response_sha256,
            "usage": {
                "input_tokens": planned.planning_outcome.reservation.actual_input_tokens,
                "output_tokens": planned.planning_outcome.reservation.actual_output_tokens,
                "total_tokens": planned.planning_outcome.response.usage.total_tokens,
            }
        },
        "ledger": {
            "consent_id": consent.id,
            "reservation_id": planned.planning_outcome.reservation.id,
            "status": planned.planning_outcome.reservation.status,
            "pricing_currency": planned.planning_outcome.reservation.pricing.currency,
            "actual_micros": planned.planning_outcome.reservation.actual_micros,
            "derived_availability": planned.planning_outcome.derived_availability_evidence,
        }
    });
    println!("LIVE_NINEROUTER_STORYBOARD_SMOKE={}", report);
}
