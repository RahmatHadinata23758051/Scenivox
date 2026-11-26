use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
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
        CostScope, CreateCostConsent, InferenceProvenance, PricingSnapshot,
        RecordModelAvailabilityProbe, ReservationStatus, ReserveBudgetRequest, SetModelPricing,
        compute_usage_cost_micros,
    },
    db,
    domain::{
        CreateProject, ModelRecord, ModelSelection, Parameters, PlanStoryboard, RegisterModel,
        ReviewCommand, Selection, Target,
    },
    openai_adapter::{
        CancellationToken, CredentialSource, DispatchContext, HttpTransportSecurityPolicy,
        LlmHttpTransport, MAX_ERROR_EXCERPT_BYTES, MockHttpTransport, ModelTokenizationProfile,
        OpenAiCompatibleAdapter, OpenAiCompatibleConfig, RedactedSecret, ReqwestHttpTransport,
        compute_model_snapshot_sha256, compute_trusted_input_tokens,
    },
    planner::{AdapterStoryboardRequest, plan_storyboard_with_adapter},
    provider::{
        LlmMessage, LlmRequest, PROVIDER_SCHEMA_VERSION, ProviderAvailability,
        ProviderCapabilities, ProviderKind, ResponseFormat, TaskRole,
    },
    store::Store,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::{io::AsyncWriteExt, net::TcpListener};
use uuid::Uuid;

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

fn next_db_url(temp: &Path) -> String {
    let idx = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
    format!(
        "sqlite://{}?mode=rwc",
        temp.join(format!("live-llm-prep-{idx}.db"))
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

fn remote_caps(provider_id: &str, model_id: &str) -> ProviderCapabilities {
    ProviderCapabilities {
        schema_version: PROVIDER_SCHEMA_VERSION.into(),
        provider_kind: ProviderKind::RemoteApi,
        provider_id: provider_id.into(),
        model_id: model_id.into(),
        adapter_version: "1.0.0".into(),
        task_roles: vec![TaskRole::StoryboardPlanning, TaskRole::SceneCodeGeneration],
        supports_tool_calling: true,
        supports_structured_json: true,
        supports_seed: true,
        min_temperature: 0.0,
        max_temperature: 2.0,
        max_context_tokens: 128_000,
        max_output_tokens: 4_096,
        enabled: true,
        configured: true,
        availability: ProviderAvailability::Available,
        reason: "Verified OpenAI-compatible remote provider".into(),
        remote: true,
    }
}

fn remote_model_record(provider_id: &str, model_id: &str) -> ModelRecord {
    ModelRecord {
        schema_version: PROVIDER_SCHEMA_VERSION.into(),
        provider_id: provider_id.into(),
        model_id: model_id.into(),
        adapter_version: "1.0.0".into(),
        task_roles: vec!["storyboard_planning".into(), "scene_code_generation".into()],
        parameters: Parameters {
            temperature: 0.0,
            seed: Some(42),
        },
        enabled: true,
        configured: true,
        availability: "available".into(),
        reason: "Verified remote model".into(),
        remote: true,
    }
}

fn remote_cfg(caps: ProviderCapabilities, secret: &str) -> OpenAiCompatibleConfig {
    OpenAiCompatibleConfig {
        capabilities: caps,
        base_url: "https://api.compat.invalid/v1".into(),
        endpoint_allowlist: vec!["https://api.compat.invalid/v1".into()],
        gateway_combo_routing: false,
        tokenization_profile: None,
        credential: CredentialSource::BearerToken(RedactedSecret::new(secret).unwrap()),
        request_timeout: Duration::from_secs(10),
        max_request_bytes: 256 * 1024,
        max_response_bytes: 256 * 1024,
        extra_headers: Vec::new(),
    }
}

fn create_benign_stick_figure_skill(root: &Path) -> PathBuf {
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
         Write web/shots.js with POSITIVE_FPS=30, execute render, probe media, and complete.\n",
    )
    .unwrap();
    fs::write(
        dir.join("references/scene-guide.md"),
        "Canvas must define POSITIVE_FPS=30 and match 1080x1920 9:16 timeline.\n",
    )
    .unwrap();
    fs::write(
        dir.join("scripts/render.js"),
        "throw new Error('simulated agent test must never execute raw JS on host');\n",
    )
    .unwrap();
    dir
}

#[tokio::test]
async fn redacts_credentials_before_truncation_even_across_excerpt_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let pool = db::connect(&next_db_url(temp.path())).await.unwrap();
    let store = Store::new(pool.clone());

    let provider_id = "compat-provider";
    let model_id = "gpt-4.1-mini";
    let secret = "sk-secret-boundary-token-99887766554433221100";

    store
        .register_model(RegisterModel {
            schema_version: PROVIDER_SCHEMA_VERSION.into(),
            provider_id: provider_id.into(),
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
            reason: "Initial".into(),
            remote: true,
        })
        .await
        .unwrap();
    store
        .set_model_pricing(SetModelPricing {
            schema_version: PROVIDER_SCHEMA_VERSION.into(),
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            pricing_version: "2026-11".into(),
            currency: "USD".into(),
            input_cost_per_token_micros: 1,
            output_cost_per_token_micros: 2,
            request_fixed_cost_micros: 50,
            verified: true,
        })
        .await
        .unwrap();
    let (consent, _) = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: PROVIDER_SCHEMA_VERSION.into(),
                project_id: None,
                provider_id: provider_id.into(),
                model_id: model_id.into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 50_000,
                max_total_tokens: Some(10_000),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();
    store
        .record_model_availability_probe(provider_id, model_id, "available", "Verified")
        .await
        .unwrap();

    let transport = MockHttpTransport::default();
    // Place secret BOTH before MAX_ERROR_EXCERPT_BYTES (512) AND straddling byte 512 so truncation
    // cannot cut a secret into a non-matching prefix that bypasses post-truncation redaction!
    let pad = "E".repeat(MAX_ERROR_EXCERPT_BYTES - 16);
    let long_err = format!("{pad}{secret} trailing-detail");
    transport
        .push_json_response(429, &json!({ "error": { "message": long_err } }))
        .unwrap();

    let adapter = OpenAiCompatibleAdapter::new(
        remote_cfg(remote_caps(provider_id, model_id), secret),
        transport,
    )
    .unwrap();
    let lease = store
        .acquire_worker_lease(&Uuid::new_v4().to_string(), 120)
        .await
        .unwrap();
    let model = remote_model_record(provider_id, model_id);
    let cancel = CancellationToken::new();
    let req = LlmRequest {
        schema_version: PROVIDER_SCHEMA_VERSION.into(),
        task_role: TaskRole::SceneCodeGeneration,
        provider_id: provider_id.into(),
        model_id: model_id.into(),
        adapter_version: "1.0.0".into(),
        parameters: Parameters {
            temperature: 0.0,
            seed: Some(42),
        },
        max_output_tokens: 128,
        response_format: ResponseFormat::Text,
        system_prompt: "test".into(),
        messages: vec![LlmMessage::user("hello")],
        tools: Vec::new(),
    };
    let trusted_in = adapter.trusted_input_tokens(&req).unwrap();
    let ctx = DispatchContext {
        store: &store,
        lease: &lease,
        project_id: None,
        job_id: None,
        model: &model,
        snapshot: None,
        cancel: &cancel,
    };
    let reserve = ReserveBudgetRequest {
        cost_consent_id: consent.id.clone(),
        project_id: None,
        job_id: None,
        provider_id: provider_id.into(),
        model_id: model_id.into(),
        scope: CostScope::SceneCodeGeneration,
        owner_id: lease.owner_id.clone(),
        lease_epoch: lease.lease_epoch,
        canonical_request_sha256: req.canonical_request_sha256().unwrap(),
        model_snapshot_sha256: compute_model_snapshot_sha256(&model, None).unwrap(),
        endpoint_identity: adapter.config().endpoint_identity().unwrap(),
        estimated_input_tokens: trusted_in,
        max_output_tokens: 128,
    };
    let err = adapter
        .execute_with_new_reservation(ctx, reserve, None, &req)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "provider_rate_limited");
    let rendered = err.to_string();
    assert!(!rendered.contains("sk-secret-boundary"));
    assert!(!rendered.contains(secret));
    assert!(rendered.contains("...") || rendered.contains("[REDACTED]"));

    pool.close().await;
}

#[test]
fn tokenization_profile_resolves_verified_families_and_fails_closed_for_unknown() {
    for known in [
        "gpt-4.1-mini",
        "gpt-4o-mini",
        "gpt-5-mini",
        "o3-mini",
        "gpt-4-turbo",
        "gpt-3.5-turbo",
    ] {
        assert!(
            ModelTokenizationProfile::for_verified_model(known).is_some(),
            "expected verified profile for {known}"
        );
    }

    let req_unknown = LlmRequest {
        schema_version: PROVIDER_SCHEMA_VERSION.into(),
        task_role: TaskRole::StoryboardPlanning,
        provider_id: "compat-provider".into(),
        model_id: "unregistered-mystery-model-x".into(),
        adapter_version: "1.0.0".into(),
        parameters: Parameters {
            temperature: 0.0,
            seed: Some(1),
        },
        max_output_tokens: 128,
        response_format: ResponseFormat::JsonObject,
        system_prompt: "Plan scenes.".into(),
        messages: vec![LlmMessage::user("{\"id\":12345,\"text\":\"你好世界\"}")],
        tools: Vec::new(),
    };
    let err = compute_trusted_input_tokens(&req_unknown).unwrap_err();
    assert_eq!(err.code(), "invalid_schema");

    // Unrecognized model without explicit profile cannot construct adapter config.
    let mut cfg_unknown = remote_cfg(
        remote_caps("compat-provider", "unregistered-mystery-model-x"),
        "opaque-secret",
    );
    assert!(cfg_unknown.validate().is_err());

    // Mismatched custom profile encoding against built-in family fails closed.
    let mut cfg_mismatch = remote_cfg(
        remote_caps("compat-provider", "gpt-4.1-mini"),
        "opaque-secret",
    );
    cfg_mismatch.tokenization_profile =
        ModelTokenizationProfile::for_verified_model("gpt-3.5-turbo");
    assert!(cfg_mismatch.validate().is_err());

    // Explicit verified custom profile for unknown model works deterministically.
    cfg_unknown.tokenization_profile = ModelTokenizationProfile::for_verified_model("gpt-4.1-mini");
    assert!(cfg_unknown.validate().is_ok());

    let mut req_known = req_unknown;
    req_known.model_id = "gpt-4.1-mini".into();
    let conservative = compute_trusted_input_tokens(&req_known).unwrap();
    assert!(conservative >= 40, "got {conservative}");
}

#[tokio::test]
async fn availability_evidence_gate_rejects_untrusted_mock_expired_or_wrong_endpoint() {
    let temp = tempfile::tempdir().unwrap();
    let pool = db::connect(&next_db_url(temp.path())).await.unwrap();
    let store = Store::new(pool.clone());

    let provider_id = "compat-provider";
    let model_id = "gpt-4.1-mini";
    store
        .register_model(RegisterModel {
            schema_version: PROVIDER_SCHEMA_VERSION.into(),
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            adapter_version: "1.0.0".into(),
            task_roles: vec!["scene_code_generation".into()],
            parameters: Parameters {
                temperature: 0.0,
                seed: Some(42),
            },
            enabled: true,
            configured: true,
            availability: "unknown".into(),
            reason: "Initial".into(),
            remote: true,
        })
        .await
        .unwrap();
    store
        .set_model_pricing(SetModelPricing {
            schema_version: PROVIDER_SCHEMA_VERSION.into(),
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            pricing_version: "2026-11".into(),
            currency: "USD".into(),
            input_cost_per_token_micros: 2,
            output_cost_per_token_micros: 8,
            request_fixed_cost_micros: 50,
            verified: true,
        })
        .await
        .unwrap();
    let (consent, _) = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: PROVIDER_SCHEMA_VERSION.into(),
                project_id: None,
                provider_id: provider_id.into(),
                model_id: model_id.into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 50_000,
                max_total_tokens: Some(10_000),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();

    // 1. Bare mock probe satisfies DeterministicMockFixture ONLY, NEVER RealRemoteTlsInference.
    store
        .record_model_availability_probe(provider_id, model_id, "available", "Mock probe")
        .await
        .unwrap();
    assert!(
        store
            .verify_model_availability_evidence(
                provider_id,
                model_id,
                "1.0.0",
                "https://api.compat.invalid/v1",
                InferenceProvenance::DeterministicMockFixture,
            )
            .await
            .is_ok()
    );
    assert!(
        store
            .verify_model_availability_evidence(
                provider_id,
                model_id,
                "1.0.0",
                "https://api.compat.invalid/v1",
                InferenceProvenance::RealRemoteTlsInference,
            )
            .await
            .is_err()
    );

    // 2. Pre-expired probe cannot be recorded, wrong endpoint is rejected, and lapsed TTL is rejected.
    assert!(
        store
            .record_model_availability_evidence(RecordModelAvailabilityProbe {
                provider_id: provider_id.into(),
                model_id: model_id.into(),
                adapter_version: "1.0.0".into(),
                endpoint_identity: "https://api.compat.invalid/v1".into(),
                availability: "available".into(),
                probe_provenance: InferenceProvenance::RealRemoteTlsInference,
                probe_sha256: sha256_hex(b"live-tls-probe-expired"),
                echoed_model_id: model_id.into(),
                expires_at: "2020-01-01T00:00:00Z".into(),
                reason: "Expired TLS probe".into(),
            })
            .await
            .is_err()
    );
    store
        .record_model_availability_evidence(RecordModelAvailabilityProbe {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            adapter_version: "1.0.0".into(),
            endpoint_identity: "https://other.compat.invalid/v1".into(),
            availability: "available".into(),
            probe_provenance: InferenceProvenance::RealRemoteTlsInference,
            probe_sha256: sha256_hex(b"live-tls-probe-other-endpoint"),
            echoed_model_id: model_id.into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            reason: "Other endpoint TLS probe".into(),
        })
        .await
        .unwrap();
    assert!(
        store
            .verify_model_availability_evidence(
                provider_id,
                model_id,
                "1.0.0",
                "https://api.compat.invalid/v1",
                InferenceProvenance::RealRemoteTlsInference,
            )
            .await
            .is_err()
    );
    store
        .record_model_availability_evidence(RecordModelAvailabilityProbe {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            adapter_version: "1.0.0".into(),
            endpoint_identity: "https://api.compat.invalid/v1".into(),
            availability: "available".into(),
            probe_provenance: InferenceProvenance::RealRemoteTlsInference,
            probe_sha256: sha256_hex(b"live-tls-probe-valid"),
            echoed_model_id: model_id.into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            reason: "Valid TLS probe".into(),
        })
        .await
        .unwrap();
    assert!(
        store
            .verify_model_availability_evidence(
                provider_id,
                model_id,
                "1.0.0",
                "https://api.compat.invalid/v1",
                InferenceProvenance::RealRemoteTlsInference,
            )
            .await
            .is_ok()
    );
    sqlx::query("UPDATE model_availability_evidence SET expires_at = '2020-01-01T00:00:00Z'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        store
            .verify_model_availability_evidence(
                provider_id,
                model_id,
                "1.0.0",
                "https://api.compat.invalid/v1",
                InferenceProvenance::RealRemoteTlsInference,
            )
            .await
            .is_err()
    );
    assert!(consent.max_cost_micros > 0);
    pool.close().await;
}

#[tokio::test]
async fn reqwest_transport_executes_single_attempt_and_controlled_local_http_fixture() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_srv = Arc::clone(&hits);

    let srv = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
        hits_srv.fetch_add(1, Ordering::SeqCst);
        stream
            .write_all(
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy",
            )
            .await
            .unwrap();
    });

    let base_url = format!("http://127.0.0.1:{}/v1", addr.port());
    let mut caps = remote_caps("local-fixture", "gpt-4.1-mini");
    caps.provider_kind = ProviderKind::LocalEndpoint;
    caps.remote = false;
    let cfg = OpenAiCompatibleConfig {
        capabilities: caps,
        base_url: base_url.clone(),
        endpoint_allowlist: vec![base_url],
        gateway_combo_routing: false,
        tokenization_profile: None,
        credential: CredentialSource::None,
        request_timeout: Duration::from_secs(3),
        max_request_bytes: 64 * 1024,
        max_response_bytes: 64 * 1024,
        extra_headers: Vec::new(),
    };
    let policy = HttpTransportSecurityPolicy::hardened(
        Duration::from_millis(500),
        Duration::from_secs(3),
        64 * 1024,
    )
    .unwrap();
    let transport = ReqwestHttpTransport::new(&cfg, policy).unwrap();
    let adapter = OpenAiCompatibleAdapter::new(cfg, transport).unwrap();
    let req = LlmRequest {
        schema_version: PROVIDER_SCHEMA_VERSION.into(),
        task_role: TaskRole::StoryboardPlanning,
        provider_id: "local-fixture".into(),
        model_id: "gpt-4.1-mini".into(),
        adapter_version: "1.0.0".into(),
        parameters: Parameters {
            temperature: 0.0,
            seed: Some(42),
        },
        max_output_tokens: 128,
        response_format: ResponseFormat::JsonObject,
        system_prompt: "Plan.".into(),
        messages: vec![LlmMessage::user("hi")],
        tools: Vec::new(),
    };
    let (http_req, _) = adapter.build_http_request(&req).unwrap();
    let cancel = CancellationToken::new();
    let res = adapter
        .transport()
        .execute(http_req, &cancel)
        .await
        .unwrap();
    assert_eq!(res.status, 503);
    assert_eq!(
        res.provenance,
        InferenceProvenance::ControlledLocalHttpFixture
    );
    // `.retry(reqwest::retry::never())` guarantees strictly single HTTP request.
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    srv.await.unwrap();
}

#[tokio::test]
async fn bounded_storyboard_and_agent_composition_run_end_to_end_with_ledger_and_proposal_check() {
    let temp = tempfile::tempdir().unwrap();
    let pool = db::connect(&next_db_url(temp.path())).await.unwrap();
    let store = Store::new(pool.clone());

    let skill_store_dir = temp.path().join("skills");
    let skill_src = create_benign_stick_figure_skill(temp.path());
    let (skill, _) = store
        .import_skill(&skill_store_dir, None, &skill_src)
        .await
        .unwrap();

    let provider_id = "openai-compat-prep";
    let model_id = "gpt-4.1-mini";
    store
        .register_model(RegisterModel {
            schema_version: PROVIDER_SCHEMA_VERSION.into(),
            provider_id: provider_id.into(),
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
            reason: "Configured prep model".into(),
            remote: true,
        })
        .await
        .unwrap();
    let pricing = PricingSnapshot {
        pricing_version: "2026-11-prep".into(),
        currency: "USD".into(),
        input_cost_per_token_micros: 1,
        output_cost_per_token_micros: 4,
        request_fixed_cost_micros: 100,
        verified: true,
    };
    store
        .set_model_pricing(SetModelPricing {
            schema_version: PROVIDER_SCHEMA_VERSION.into(),
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            pricing_version: pricing.pricing_version.clone(),
            currency: pricing.currency.clone(),
            input_cost_per_token_micros: pricing.input_cost_per_token_micros,
            output_cost_per_token_micros: pricing.output_cost_per_token_micros,
            request_fixed_cost_micros: pricing.request_fixed_cost_micros,
            verified: true,
        })
        .await
        .unwrap();
    let (consent, _) = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: PROVIDER_SCHEMA_VERSION.into(),
                project_id: None,
                provider_id: provider_id.into(),
                model_id: model_id.into(),
                scopes: vec![
                    CostScope::StoryboardPlanning,
                    CostScope::SceneCodeGeneration,
                ],
                max_cost_micros: 50_000,
                max_total_tokens: Some(25_000),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();
    store
        .record_model_availability_probe(
            provider_id,
            model_id,
            "available",
            "Verified mock endpoint availability",
        )
        .await
        .unwrap();

    // Validate LiveLlmConfig ceiling & proposed-call preflight calculation.
    let live_cfg = LiveLlmConfig {
        enabled: false,
        provider_id: provider_id.into(),
        model_id: model_id.into(),
        adapter_version: "1.0.0".into(),
        provider_kind: ProviderKind::RemoteApi,
        base_url: "https://api.compat.invalid/v1".into(),
        endpoint_allowlist: vec!["https://api.compat.invalid/v1".into()],
        gateway_combo_routing: false,
        tokenization_encoding: None,
        credential_env_var: "SCENIVOX_TEST_UNUSED_VAR".into(),
        request_timeout_ms: 15_000,
        connect_timeout_ms: 5_000,
        max_request_bytes: 128 * 1024,
        max_response_bytes: 256 * 1024,
        max_input_tokens: 4_096,
        max_output_tokens: 1_024,
        max_cost_micros: 50_000,
    };
    let summary = live_cfg
        .summarize_proposed_call(&sha256_hex(b"req"), 300, 600, &pricing)
        .unwrap();
    assert_eq!(
        summary.estimated_max_cost_micros,
        compute_usage_cost_micros(&pricing, 300, 600).unwrap()
    );
    assert!(
        live_cfg
            .summarize_proposed_call(&sha256_hex(b"too-big"), 5_000, 600, &pricing)
            .is_err()
    );

    let transport = MockHttpTransport::default();
    // 1. Storyboard planning JSON response
    transport
        .push_json_response(
            200,
            &json!({
                "id": "chatcmpl-plan-1",
                "model": model_id,
                "choices": [{
                    "finish_reason": "stop",
                    "message": {
                        "role": "assistant",
                        "content": serde_json::to_string(&json!({
                            "synopsis": "Stick figure discovers a glowing relay module.",
                            "hook": "Why did the laboratory alarm just blink green?",
                            "scenes": [
                                {
                                    "order": 0,
                                    "duration_ms": 3000,
                                    "visual_direction": "Stick figure enters dimmed server hallway.",
                                    "camera": "Wide 9:16 vertical shot",
                                    "action": "Walks left to right and spots console.",
                                    "narration": "A faint pulse echoed across the quiet corridor.",
                                    "caption_hint": "Quiet corridor",
                                    "sfx_cues": []
                                },
                                {
                                    "order": 1,
                                    "duration_ms": 3000,
                                    "visual_direction": "Close frame on stick figure pressing switch.",
                                    "camera": "Medium vertical shot",
                                    "action": "Raises arm and activates panel.",
                                    "narration": "One tap brought the entire console back to life.",
                                    "caption_hint": "Back to life",
                                    "sfx_cues": []
                                }
                            ]
                        })).unwrap()
                    }
                }],
                "usage": {"prompt_tokens": 120, "completion_tokens": 180, "total_tokens": 300}
            }),
        )
        .unwrap();

    // 2. Agent turn 0: write bug + try render
    transport
        .push_json_response(
            200,
            &json!({
                "id": "chatcmpl-agent-0",
                "model": model_id,
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
                                    "arguments": "{\"rel_path\":\"web/shots.js\",\"content\":\"export const POSITIVE_FPS = 0;\\n\"}"
                                }
                            },
                            {
                                "id": "call_run_0",
                                "type": "function",
                                "function": {
                                    "name": "skill_run_step",
                                    "arguments": "{\"entrypoint_id\":\"render\",\"args\":{\"out\":\"final.mp4\"}}"
                                }
                            }
                        ]
                    }
                }],
                "usage": {"prompt_tokens": 600, "completion_tokens": 80, "total_tokens": 680}
            }),
        )
        .unwrap();

    // 3. Agent turn 1: bounded repair + re-render + probe media
    transport
        .push_json_response(
            200,
            &json!({
                "id": "chatcmpl-agent-1",
                "model": model_id,
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
                "usage": {"prompt_tokens": 750, "completion_tokens": 110, "total_tokens": 860}
            }),
        )
        .unwrap();

    // 4. Agent turn 2: complete_composition
    transport
        .push_json_response(
            200,
            &json!({
                "id": "chatcmpl-agent-2",
                "model": model_id,
                "choices": [{
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [
                            {
                                "id": "call_done_2",
                                "type": "function",
                                "function": {
                                    "name": "complete_composition",
                                    "arguments": "{\"output_rel_path\":\"final.mp4\",\"summary\":\"Completed 6s stick-figure composition after POSITIVE_FPS repair.\"}"
                                }
                            }
                        ]
                    }
                }],
                "usage": {"prompt_tokens": 850, "completion_tokens": 60, "total_tokens": 910}
            }),
        )
        .unwrap();

    let adapter = OpenAiCompatibleAdapter::new(
        remote_cfg(remote_caps(provider_id, model_id), "sk-prep-test-secret"),
        transport,
    )
    .unwrap();
    let lease = store
        .acquire_worker_lease(&Uuid::new_v4().to_string(), 120)
        .await
        .unwrap();
    let selection = ModelSelection {
        storyboard_planning: Some(Selection {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
        }),
        scene_code_generation: Some(Selection {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
        }),
    };
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
            title: "P2-03C Bounded Adapter Flow".into(),
            idea: "A stick figure discovers a glowing server relay.".into(),
            target: target.clone(),
            model_selection: selection.clone(),
        })
        .await
        .unwrap();

    let cancel = CancellationToken::new();
    let plan_outcome = plan_storyboard_with_adapter(AdapterStoryboardRequest {
        store: &store,
        project_id: &project.id,
        idempotency_key: Some("00000000-0000-4000-8000-000000000101"),
        reservation_idempotency_key: Some("00000000-0000-4000-8000-000000000102"),
        lease: &lease,
        cost_consent_id: &consent.id,
        adapter: &adapter,
        input: &PlanStoryboard {
            schema_version: "1".into(),
            idea: project.idea.clone(),
            target,
            expected_revision_id: None,
            model_selection: selection.clone(),
        },
        max_output_tokens: 600,
        cancel: &cancel,
    })
    .await
    .unwrap();

    assert_eq!(
        plan_outcome
            .revision
            .content
            .models
            .storyboard_planning
            .model_id,
        model_id
    );
    assert_eq!(
        plan_outcome
            .revision
            .content
            .models
            .scene_code_generation
            .model_id,
        model_id
    );
    assert_eq!(
        plan_outcome.planning_outcome.reservation.status,
        ReservationStatus::Settled
    );
    assert_eq!(
        plan_outcome
            .revision
            .content
            .planning_provenance
            .as_ref()
            .unwrap()
            .source,
        "openai_compatible:deterministic_mock_fixture"
    );

    let proposed_first_call = live_cfg
        .summarize_proposed_call(
            &plan_outcome.request_sha256,
            u64::try_from(
                plan_outcome
                    .planning_outcome
                    .reservation
                    .estimated_input_tokens,
            )
            .unwrap(),
            600,
            &pricing,
        )
        .unwrap();
    assert_eq!(
        proposed_first_call.estimated_max_cost_micros,
        plan_outcome.planning_outcome.reservation.reserved_micros
    );
    println!(
        "PROPOSED_FIRST_LIVE_CALL={}",
        serde_json::to_string(&proposed_first_call).unwrap()
    );

    store
        .review_revision(
            &project.id,
            &plan_outcome.revision.id,
            None,
            ReviewCommand {
                schema_version: "1".into(),
                content_sha256: plan_outcome.revision.content_sha256.clone(),
            },
        )
        .await
        .unwrap();
    let (approval, _, _) = store
        .approve_revision(
            &project.id,
            &plan_outcome.revision.id,
            None,
            ReviewCommand {
                schema_version: "1".into(),
                content_sha256: plan_outcome.revision.content_sha256.clone(),
            },
        )
        .await
        .unwrap();

    let job_id = Uuid::new_v4().to_string();
    let staging_root = temp.path().join("staging");
    let workspace_dir = staging_root.join(&job_id);
    fs::create_dir_all(&workspace_dir).unwrap();

    let task_input = AgentTaskInput {
        job_id: job_id.clone(),
        revision: plan_outcome.revision.clone(),
        approval,
        skill,
        skill_store_dir,
        workspace_dir,
        staging_root: Some(staging_root),
        narration: None,
        timeline: None,
    };

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
                        content_bytes: b"SIMULATED_ADAPTER_PLACEHOLDER_NOT_REAL_MP4".to_vec(),
                        simulated: true,
                        description: "Simulated composition placeholder".into(),
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

    let model_rec = remote_model_record(provider_id, model_id);
    let agent_out = AgentRuntime::run_with_openai_adapter(
        &AgentConfig {
            execution_mode: AgentExecutionMode::OpenAiCompatibleAdapter,
            require_smoke_verified_skill: false,
            limits: AgentLimits {
                max_turns: 4,
                max_llm_output_tokens_per_turn: 600,
                max_total_llm_tokens: 10_000,
                ..AgentLimits::default()
            },
        },
        &adapter,
        AgentLedgerContext {
            store: &store,
            lease: &lease,
            model: &model_rec,
            consent_id: &consent.id,
            cancel: &cancel,
            idempotency_prefix: "prep-agent",
        },
        &task_input,
        &backend,
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(agent_out.turn_cost_traces.len(), 3);
    assert_eq!(
        agent_out.run.provenance.execution_mode,
        AgentExecutionMode::OpenAiCompatibleAdapter
    );
    assert!(
        agent_out
            .run
            .provenance
            .all_execution_and_media_outputs_simulated
    );
    assert!(!agent_out.run.provenance.real_mp4_rendered);
    assert_eq!(agent_out.run.provenance.repair_attempts.len(), 1);
    assert_eq!(
        agent_out.inference_provenance,
        InferenceProvenance::DeterministicMockFixture
    );

    let planning_cost = plan_outcome
        .planning_outcome
        .reservation
        .actual_micros
        .unwrap();
    let expected_total_cost = planning_cost + agent_out.total_cost_micros;
    let updated_consent = store.get_cost_consent(&consent.id).await.unwrap();
    assert_eq!(updated_consent.spent_micros, expected_total_cost);
    assert_eq!(updated_consent.spent_tokens, 300 + 680 + 860 + 910);
    assert_eq!(
        adapter.transport().recorded_requests().len(),
        4,
        "1 storyboard request + 3 agent turn requests"
    );

    pool.close().await;
}
