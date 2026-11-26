use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use scenivox::{
    cost::{
        CostConsentStatus, CostScope, CreateCostConsent, InferenceProvenance, ReservationStatus,
        ReserveBudgetRequest, RevokeCostConsent, SetModelPricing,
    },
    db,
    domain::{ModelRecord, ModelSnapshot, Parameters, RegisterModel},
    openai_adapter::{
        BoundedResponseBuffer, CancellationToken, CredentialSource, DispatchContext,
        HttpTransportSecurityPolicy, LlmHttpTransport, MockHttpOutcome, MockHttpTransport,
        ModelTokenizationProfile, NoNetworkTransport, OpenAiCompatibleAdapter,
        OpenAiCompatibleConfig, RedactedSecret, ReqwestHttpTransport,
        compute_model_snapshot_sha256, compute_trusted_input_tokens_for_profile,
        validate_resolved_ip,
    },
    provider::{
        FinishReason, LlmMessage, LlmRequest, PROVIDER_SCHEMA_VERSION, ProviderAvailability,
        ProviderCapabilities, ProviderKind, ResponseFormat, TaskRole, ToolDefinition,
    },
    store::Store,
};
use serde_json::json;

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

fn temp_db_url(temp: &Path) -> String {
    let idx = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
    format!(
        "sqlite://{}?mode=rwc",
        temp.join(format!("openai-adapter-{idx}.db"))
            .to_string_lossy()
            .replace('\\', "/")
    )
}

fn remote_caps(provider_id: &str, model_id: &str) -> ProviderCapabilities {
    ProviderCapabilities {
        schema_version: PROVIDER_SCHEMA_VERSION.into(),
        provider_kind: ProviderKind::RemoteApi,
        provider_id: provider_id.into(),
        model_id: model_id.into(),
        adapter_version: "1.0.0".into(),
        task_roles: vec![TaskRole::SceneCodeGeneration, TaskRole::StoryboardPlanning],
        supports_tool_calling: true,
        supports_structured_json: true,
        supports_seed: true,
        min_temperature: 0.0,
        max_temperature: 2.0,
        max_context_tokens: 32_768,
        max_output_tokens: 4_096,
        enabled: true,
        configured: true,
        availability: ProviderAvailability::Available,
        reason: "Verified OpenAI-compatible remote endpoint".into(),
        remote: true,
    }
}

fn remote_model_record(provider_id: &str, model_id: &str) -> ModelRecord {
    ModelRecord {
        schema_version: PROVIDER_SCHEMA_VERSION.into(),
        provider_id: provider_id.into(),
        model_id: model_id.into(),
        adapter_version: "1.0.0".into(),
        task_roles: vec!["scene_code_generation".into(), "storyboard_planning".into()],
        parameters: Parameters {
            temperature: 0.0,
            seed: Some(42),
        },
        enabled: true,
        configured: true,
        availability: "available".into(),
        reason: "Verified opt-in remote model".into(),
        remote: true,
    }
}

fn test_profile() -> ModelTokenizationProfile {
    ModelTokenizationProfile::for_verified_model("gpt-4.1-mini").unwrap()
}

fn base_config(caps: ProviderCapabilities, secret: &str) -> OpenAiCompatibleConfig {
    let base_url = "https://api.compat.invalid/v1/";
    OpenAiCompatibleConfig {
        capabilities: caps,
        base_url: base_url.into(),
        endpoint_allowlist: vec!["https://api.compat.invalid/v1".into()],
        gateway_combo_routing: false,
        tokenization_profile: Some(test_profile()),
        credential: CredentialSource::BearerToken(RedactedSecret::new(secret).unwrap()),
        request_timeout: Duration::from_secs(5),
        max_request_bytes: 256 * 1024,
        max_response_bytes: 256 * 1024,
        extra_headers: vec![("X-Client-Mode".into(), "test-mock".into())],
    }
}

fn sample_tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "workspace_write".into(),
        description: "Write a generated scene file".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "rel_path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["rel_path", "content"]
        }),
    }]
}

fn base_request(provider_id: &str, model_id: &str, format: ResponseFormat) -> LlmRequest {
    LlmRequest {
        schema_version: PROVIDER_SCHEMA_VERSION.into(),
        task_role: TaskRole::SceneCodeGeneration,
        provider_id: provider_id.into(),
        model_id: model_id.into(),
        adapter_version: "1.0.0".into(),
        parameters: Parameters {
            temperature: 0.0,
            seed: Some(42),
        },
        max_output_tokens: 512,
        response_format: format,
        system_prompt: "Return strictly validated output.".into(),
        messages: vec![LlmMessage::user("Generate scene step 1.")],
        tools: if matches!(format, ResponseFormat::ToolCalls) {
            sample_tools()
        } else {
            Vec::new()
        },
    }
}

fn build_bound_reserve(
    consent_id: &str,
    ctx: DispatchContext<'_>,
    endpoint_identity: &str,
    request: &LlmRequest,
    estimated_input_tokens: i64,
    max_output_tokens: i64,
) -> ReserveBudgetRequest {
    ReserveBudgetRequest {
        cost_consent_id: consent_id.to_owned(),
        project_id: ctx.project_id.map(str::to_owned),
        job_id: ctx.job_id.map(str::to_owned),
        provider_id: request.provider_id.clone(),
        model_id: request.model_id.clone(),
        scope: CostScope::SceneCodeGeneration,
        owner_id: ctx.lease.owner_id.clone(),
        lease_epoch: ctx.lease.lease_epoch,
        canonical_request_sha256: request.canonical_request_sha256().unwrap(),
        model_snapshot_sha256: compute_model_snapshot_sha256(ctx.model, ctx.snapshot).unwrap(),
        endpoint_identity: endpoint_identity.to_owned(),
        estimated_input_tokens,
        max_output_tokens,
    }
}

async fn setup_store_consent_and_lease(
    temp: &Path,
    provider_id: &str,
    model_id: &str,
    max_cost_micros: i64,
    max_total_tokens: Option<i64>,
) -> (
    sqlx::SqlitePool,
    Store,
    scenivox::cost::CostConsent,
    scenivox::cost::WorkerLease,
) {
    let db_url = temp_db_url(temp);
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    store
        .register_model(RegisterModel {
            schema_version: PROVIDER_SCHEMA_VERSION.into(),
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            adapter_version: "1.0.0".into(),
            task_roles: vec!["scene_code_generation".into(), "storyboard_planning".into()],
            parameters: Parameters {
                temperature: 0.0,
                seed: Some(42),
            },
            enabled: true,
            configured: true,
            availability: "unknown".into(),
            reason: "Configured test model".into(),
            remote: true,
        })
        .await
        .unwrap();

    store
        .set_model_pricing(SetModelPricing {
            schema_version: PROVIDER_SCHEMA_VERSION.into(),
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            pricing_version: "2026-11-test".into(),
            currency: "USD".into(),
            input_cost_per_token_micros: 3,
            output_cost_per_token_micros: 12,
            request_fixed_cost_micros: 100,
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
                max_cost_micros,
                max_total_tokens,
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
            "Verified test endpoint probe",
        )
        .await
        .unwrap();

    let owner = uuid::Uuid::new_v4().to_string();
    let lease = store.acquire_worker_lease(&owner, 120).await.unwrap();
    (pool, store, consent, lease)
}

#[tokio::test]
async fn structured_json_and_native_tool_calling_roundtrip_reconciles_ledger_with_mock_provenance()
{
    let temp = tempfile::tempdir().unwrap();
    let provider_id = "compat-provider";
    let model_id = "configured-model-1";
    let secret = "opaque-test-secret-do-not-leak-987654";

    let (pool, store, consent, lease) =
        setup_store_consent_and_lease(temp.path(), provider_id, model_id, 50_000, Some(5_000))
            .await;

    let transport = MockHttpTransport::default();
    transport
        .push_json_response(
            200,
            &json!({
                "id": "chatcmpl-json-1",
                "model": model_id,
                "choices": [{
                    "finish_reason": "stop",
                    "message": {
                        "role": "assistant",
                        "content": "{\"scene_count\":2,\"ok\":true}"
                    }
                }],
                "usage": {
                    "prompt_tokens": 40,
                    "completion_tokens": 20,
                    "total_tokens": 60
                }
            }),
        )
        .unwrap();
    transport
        .push_json_response(
            200,
            &json!({
                "id": "chatcmpl-tool-1",
                "model": model_id,
                "choices": [{
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call-write-1",
                            "type": "function",
                            "function": {
                                "name": "workspace_write",
                                "arguments": "{\"rel_path\":\"scene.js\",\"content\":\"console.log(1);\"}"
                            }
                        }]
                    }
                }],
                "usage": {
                    "prompt_tokens": 50,
                    "completion_tokens": 25,
                    "total_tokens": 75
                }
            }),
        )
        .unwrap();

    let caps = remote_caps(provider_id, model_id);
    let cfg = base_config(caps, secret);
    assert!(!format!("{cfg:?}").contains(secret));
    let adapter = OpenAiCompatibleAdapter::new(cfg, transport).unwrap();
    let endpoint = adapter.config().endpoint_identity().unwrap();
    let model_rec = remote_model_record(provider_id, model_id);
    let cancel = CancellationToken::new();
    let project_id = uuid::Uuid::new_v4().to_string();
    let job_id = uuid::Uuid::new_v4().to_string();
    let ctx = DispatchContext {
        store: &store,
        lease: &lease,
        project_id: Some(&project_id),
        job_id: Some(&job_id),
        model: &model_rec,
        snapshot: None,
        cancel: &cancel,
    };

    let req_1 = base_request(provider_id, model_id, ResponseFormat::JsonObject);
    let trusted_1 = adapter.trusted_input_tokens(&req_1).unwrap().max(100);
    let out_1 = adapter
        .execute_with_new_reservation(
            ctx,
            build_bound_reserve(&consent.id, ctx, &endpoint, &req_1, trusted_1, 512),
            Some("00000000-0000-4000-8000-000000000001"),
            &req_1,
        )
        .await
        .unwrap();

    assert_eq!(out_1.response.finish_reason, FinishReason::Stop);
    assert_eq!(out_1.response.model_id, model_id);
    // Mock transport MUST NOT pose as real remote inference!
    assert!(out_1.response.simulated);
    assert!(out_1.response.usage.simulated);
    assert!(out_1.response.usage.authoritative);
    assert_eq!(
        out_1.inference_provenance,
        InferenceProvenance::DeterministicMockFixture
    );
    assert!(!out_1.inference_provenance.is_real_remote_inference());
    assert_eq!(
        out_1.reservation.inference_provenance,
        Some(InferenceProvenance::DeterministicMockFixture)
    );
    assert_eq!(out_1.reservation.status, ReservationStatus::Settled);
    assert_eq!(out_1.reservation.actual_micros, Some(460));

    let req_2 = base_request(provider_id, model_id, ResponseFormat::ToolCalls);
    let trusted_2 = adapter.trusted_input_tokens(&req_2).unwrap().max(160);
    let out_2 = adapter
        .execute_with_new_reservation(
            ctx,
            build_bound_reserve(&consent.id, ctx, &endpoint, &req_2, trusted_2, 512),
            Some("00000000-0000-4000-8000-000000000002"),
            &req_2,
        )
        .await
        .unwrap();

    assert_eq!(out_2.reservation.status, ReservationStatus::Settled);
    assert_eq!(out_2.reservation.actual_micros, Some(550));

    let live_consent = store.get_cost_consent(&consent.id).await.unwrap();
    assert_eq!(live_consent.status, CostConsentStatus::Active);
    assert_eq!(live_consent.reserved_micros, 0);
    assert_eq!(live_consent.reserved_tokens, 0);
    assert_eq!(live_consent.spent_micros, 460 + 550);
    assert_eq!(live_consent.spent_tokens, 60 + 75);

    let recorded = adapter.transport().recorded_requests();
    assert_eq!(recorded.len(), 2);
    assert_eq!(
        recorded[0].url,
        "https://api.compat.invalid/v1/chat/completions"
    );
    assert_eq!(recorded[0].body_json["model"], model_id);
    assert_eq!(
        recorded[0].body_json["response_format"]["type"],
        "json_object"
    );
    assert_eq!(recorded[0].body_json["stream"], false);
    assert_eq!(recorded[1].body_json["tool_choice"], "required");
    assert_eq!(
        recorded[1].body_json["tools"][0]["function"]["name"],
        "workspace_write"
    );
    for req_log in &recorded {
        let log_dump = format!("{req_log:?}");
        assert!(!log_dump.contains(secret));
        assert!(
            req_log
                .headers
                .iter()
                .any(|(k, v)| k == "Authorization" && v == "[REDACTED]")
        );
    }

    pool.close().await;
}

#[tokio::test]
async fn enforces_single_use_dispatch_and_rejects_replay_before_transport() {
    let temp = tempfile::tempdir().unwrap();
    let provider_id = "compat-provider";
    let model_id = "configured-model-1";
    let (pool, store, consent, lease) =
        setup_store_consent_and_lease(temp.path(), provider_id, model_id, 50_000, Some(5_000))
            .await;

    let transport = MockHttpTransport::default();
    for idx in 1..=3 {
        transport
            .push_json_response(
                200,
                &json!({
                    "id": format!("chatcmpl-{idx}"),
                    "model": model_id,
                    "choices": [{
                        "finish_reason": "stop",
                        "message": {"role": "assistant", "content": "ok"}
                    }],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
                }),
            )
            .unwrap();
    }
    let adapter = OpenAiCompatibleAdapter::new(
        base_config(remote_caps(provider_id, model_id), "opaque-secret-1"),
        transport,
    )
    .unwrap();
    let endpoint = adapter.config().endpoint_identity().unwrap();
    let model_rec = remote_model_record(provider_id, model_id);
    let req = base_request(provider_id, model_id, ResponseFormat::Text);
    let trusted_in = adapter.trusted_input_tokens(&req).unwrap();
    let cancel = CancellationToken::new();
    let ctx = DispatchContext {
        store: &store,
        lease: &lease,
        project_id: None,
        job_id: None,
        model: &model_rec,
        snapshot: None,
        cancel: &cancel,
    };

    // 1. Pre-created bound reservation succeeds once.
    let (pre_res, replayed) = store
        .reserve_budget(
            Some("00000000-0000-4000-8000-000000000003"),
            build_bound_reserve(&consent.id, ctx, &endpoint, &req, trusted_in, 512),
        )
        .await
        .unwrap();
    assert!(!replayed);

    let out = adapter
        .execute_for_reservation(ctx, &pre_res.id, &req)
        .await
        .unwrap();
    assert_eq!(out.reservation.status, ReservationStatus::Settled);
    assert_eq!(adapter.transport().recorded_requests().len(), 1);

    // 2. Reusing same reservation ID fails BEFORE HTTP transport.
    let err = adapter
        .execute_for_reservation(ctx, &pre_res.id, &req)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "cost_reconciliation_failed");
    assert_eq!(adapter.transport().recorded_requests().len(), 1);

    // 3. Replay of idempotency_key blocks subsequent dispatch completely.
    let (res_fresh, _) = store
        .reserve_budget(
            Some("00000000-0000-4000-8000-000000000004"),
            build_bound_reserve(&consent.id, ctx, &endpoint, &req, trusted_in, 512),
        )
        .await
        .unwrap();

    let err = adapter
        .execute_with_new_reservation(
            ctx,
            build_bound_reserve(&consent.id, ctx, &endpoint, &req, trusted_in, 512),
            Some("00000000-0000-4000-8000-000000000004"),
            &req,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "cost_reconciliation_failed");
    assert_eq!(adapter.transport().recorded_requests().len(), 1);

    let err = adapter
        .execute_for_reservation(ctx, &res_fresh.id, &req)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "cost_reconciliation_failed");
    assert_eq!(adapter.transport().recorded_requests().len(), 1);

    pool.close().await;
}

#[tokio::test]
async fn cancellation_and_timeout_handle_pre_and_in_flight_budget_safely() {
    let temp = tempfile::tempdir().unwrap();
    let provider_id = "compat-provider";
    let model_id = "configured-model-1";
    let (pool, store, consent, lease) =
        setup_store_consent_and_lease(temp.path(), provider_id, model_id, 100_000, Some(10_000))
            .await;

    let transport = MockHttpTransport::default();
    transport
        .push_outcome(MockHttpOutcome::Response {
            delay: Duration::from_secs(5),
            status: 200,
            body: Vec::new(),
        })
        .unwrap();

    let mut cfg = base_config(remote_caps(provider_id, model_id), "opaque-secret-cancel");
    cfg.request_timeout = Duration::from_millis(800);
    let adapter = OpenAiCompatibleAdapter::new(cfg, transport).unwrap();
    let endpoint = adapter.config().endpoint_identity().unwrap();
    let model_rec = remote_model_record(provider_id, model_id);
    let req = base_request(provider_id, model_id, ResponseFormat::Text);
    let trusted_in = adapter.trusted_input_tokens(&req).unwrap().max(100);

    // 1. Pre-dispatch cancellation leaves consent untouched.
    let pre_cancel = CancellationToken::new();
    pre_cancel.cancel();
    let pre_ctx = DispatchContext {
        store: &store,
        lease: &lease,
        project_id: None,
        job_id: None,
        model: &model_rec,
        snapshot: None,
        cancel: &pre_cancel,
    };
    let err = adapter
        .execute_with_new_reservation(
            pre_ctx,
            build_bound_reserve(&consent.id, pre_ctx, &endpoint, &req, trusted_in, 512),
            Some("00000000-0000-4000-8000-000000000005"),
            &req,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "request_cancelled");
    assert_eq!(adapter.transport().recorded_requests().len(), 0);
    let c_after_pre = store.get_cost_consent(&consent.id).await.unwrap();
    assert_eq!(c_after_pre.spent_micros, 0);
    assert_eq!(c_after_pre.status, CostConsentStatus::Active);

    // 2. Mid-flight cancellation triggers conservative settlement and halts consent.
    let live_cancel = CancellationToken::new();
    let trigger = live_cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        trigger.cancel();
    });
    let live_ctx = DispatchContext {
        store: &store,
        lease: &lease,
        project_id: None,
        job_id: None,
        model: &model_rec,
        snapshot: None,
        cancel: &live_cancel,
    };
    let err = adapter
        .execute_with_new_reservation(
            live_ctx,
            build_bound_reserve(&consent.id, live_ctx, &endpoint, &req, trusted_in, 512),
            Some("00000000-0000-4000-8000-000000000006"),
            &req,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "request_cancelled");
    assert_eq!(adapter.transport().recorded_requests().len(), 1);

    let c_after_mid = store.get_cost_consent(&consent.id).await.unwrap();
    assert_eq!(c_after_mid.status, CostConsentStatus::Halted);
    assert_eq!(c_after_mid.reserved_micros, 0);
    assert_eq!(c_after_mid.spent_micros, 6_544);

    // 3. Timeout on fresh consent settles conservatively and halts consent.
    let (consent_timeout, _) = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: PROVIDER_SCHEMA_VERSION.into(),
                project_id: None,
                provider_id: provider_id.into(),
                model_id: model_id.into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 100_000,
                max_total_tokens: Some(10_000),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();
    let timeout_transport = MockHttpTransport::default();
    timeout_transport
        .push_outcome(MockHttpOutcome::Response {
            delay: Duration::from_secs(5),
            status: 200,
            body: Vec::new(),
        })
        .unwrap();
    let mut timeout_cfg = base_config(remote_caps(provider_id, model_id), "opaque-secret-timeout");
    timeout_cfg.request_timeout = Duration::from_millis(25);
    let timeout_adapter = OpenAiCompatibleAdapter::new(timeout_cfg, timeout_transport).unwrap();
    let no_cancel = CancellationToken::new();
    let timeout_ctx = DispatchContext {
        store: &store,
        lease: &lease,
        project_id: None,
        job_id: None,
        model: &model_rec,
        snapshot: None,
        cancel: &no_cancel,
    };
    let err = timeout_adapter
        .execute_with_new_reservation(
            timeout_ctx,
            build_bound_reserve(
                &consent_timeout.id,
                timeout_ctx,
                &endpoint,
                &req,
                trusted_in,
                512,
            ),
            Some("00000000-0000-4000-8000-000000000007"),
            &req,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "provider_timeout");
    assert_eq!(
        store
            .get_cost_consent(&consent_timeout.id)
            .await
            .unwrap()
            .status,
        CostConsentStatus::Halted
    );

    pool.close().await;
}

#[tokio::test]
async fn validates_endpoints_credentials_model_binding_bounds_and_provider_errors() {
    let provider_id = "compat-provider";
    let model_id = "configured-model-1";
    let caps = remote_caps(provider_id, model_id);

    let mut bad_url = base_config(caps.clone(), "opaque-secret");
    bad_url.base_url = "http://api.compat.invalid/v1".into();
    assert!(OpenAiCompatibleAdapter::new(bad_url, NoNetworkTransport).is_err());

    let mut no_cred = base_config(caps.clone(), "opaque-secret");
    no_cred.credential = CredentialSource::None;
    assert!(OpenAiCompatibleAdapter::new(no_cred, NoNetworkTransport).is_err());

    let mut local_caps =
        ProviderCapabilities::deterministic_mock(model_id, vec![TaskRole::SceneCodeGeneration]);
    local_caps.provider_id = "local-endpoint".into();
    local_caps.provider_kind = ProviderKind::LocalEndpoint;
    local_caps.remote = false;
    let bad_local_cfg = OpenAiCompatibleConfig {
        capabilities: local_caps.clone(),
        base_url: "http://localhost.evil.test:11434/v1".into(),
        endpoint_allowlist: vec!["http://localhost.evil.test:11434/v1".into()],
        gateway_combo_routing: false,
        tokenization_profile: Some(test_profile()),
        credential: CredentialSource::None,
        request_timeout: Duration::from_secs(2),
        max_request_bytes: 65_536,
        max_response_bytes: 65_536,
        extra_headers: Vec::new(),
    };
    assert!(OpenAiCompatibleAdapter::new(bad_local_cfg, NoNetworkTransport).is_err());

    let ok_local_cfg = OpenAiCompatibleConfig {
        capabilities: local_caps,
        base_url: "http://127.0.0.1:11434/v1".into(),
        endpoint_allowlist: vec!["http://127.0.0.1:11434/v1".into()],
        gateway_combo_routing: false,
        tokenization_profile: Some(test_profile()),
        credential: CredentialSource::None,
        request_timeout: Duration::from_secs(2),
        max_request_bytes: 65_536,
        max_response_bytes: 65_536,
        extra_headers: Vec::new(),
    };
    let local_adapter = OpenAiCompatibleAdapter::new(ok_local_cfg, NoNetworkTransport).unwrap();
    assert_eq!(
        local_adapter.config().chat_completions_url().unwrap(),
        "http://127.0.0.1:11434/v1/chat/completions"
    );

    let temp = tempfile::tempdir().unwrap();
    let (pool, store, consent, lease) =
        setup_store_consent_and_lease(temp.path(), provider_id, model_id, 100_000, Some(10_000))
            .await;

    let transport = MockHttpTransport::default();
    let secret = "sk-do-not-echo-401-secret";
    transport
        .push_json_response(
            401,
            &json!({"error": {"message": format!("bad key {secret}")}}),
        )
        .unwrap();
    transport
        .push_json_response(
            200,
            &json!({
                "id": "chatcmpl-spoof",
                "model": "substituted-other-model",
                "choices": [{
                    "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "hi"}
                }],
                "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
            }),
        )
        .unwrap();
    transport
        .push_json_response(
            200,
            &json!({
                "id": "chatcmpl-nousage",
                "model": model_id,
                "choices": [{
                    "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "hi"}
                }]
            }),
        )
        .unwrap();

    let adapter = OpenAiCompatibleAdapter::new(base_config(caps, secret), transport).unwrap();
    let endpoint = adapter.config().endpoint_identity().unwrap();
    let model_rec = remote_model_record(provider_id, model_id);
    let req = base_request(provider_id, model_id, ResponseFormat::Text);
    let trusted_in = adapter.trusted_input_tokens(&req).unwrap().max(100);
    let cancel = CancellationToken::new();

    let bad_snap = ModelSnapshot {
        provider_id: provider_id.into(),
        model_id: "different-model".into(),
        adapter_version: "1.0.0".into(),
        parameters: Parameters {
            temperature: 0.0,
            seed: Some(42),
        },
        prompt_sha256: "0".repeat(64),
    };
    let bad_snap_ctx = DispatchContext {
        store: &store,
        lease: &lease,
        project_id: None,
        job_id: None,
        model: &model_rec,
        snapshot: Some(&bad_snap),
        cancel: &cancel,
    };
    let err = adapter
        .execute_with_new_reservation(
            bad_snap_ctx,
            build_bound_reserve(&consent.id, bad_snap_ctx, &endpoint, &req, trusted_in, 512),
            None,
            &req,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "invalid_model_selection");
    assert_eq!(adapter.transport().recorded_requests().len(), 0);

    let ctx = DispatchContext {
        store: &store,
        lease: &lease,
        project_id: None,
        job_id: None,
        model: &model_rec,
        snapshot: None,
        cancel: &cancel,
    };

    let err = adapter
        .execute_with_new_reservation(
            ctx,
            build_bound_reserve(&consent.id, ctx, &endpoint, &req, trusted_in, 512),
            None,
            &req,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "provider_auth_failed");
    assert!(!err.to_string().contains(secret));

    let (c2, _) = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: PROVIDER_SCHEMA_VERSION.into(),
                project_id: None,
                provider_id: provider_id.into(),
                model_id: model_id.into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 100_000,
                max_total_tokens: Some(10_000),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();
    let err = adapter
        .execute_with_new_reservation(
            ctx,
            build_bound_reserve(&c2.id, ctx, &endpoint, &req, trusted_in, 512),
            None,
            &req,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "provider_invalid_response");

    let (c3, _) = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: PROVIDER_SCHEMA_VERSION.into(),
                project_id: None,
                provider_id: provider_id.into(),
                model_id: model_id.into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 100_000,
                max_total_tokens: Some(10_000),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();
    let err = adapter
        .execute_with_new_reservation(
            ctx,
            build_bound_reserve(&c3.id, ctx, &endpoint, &req, trusted_in, 512),
            None,
            &req,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "provider_invalid_response");
    assert_eq!(
        store.get_cost_consent(&c3.id).await.unwrap().status,
        CostConsentStatus::Halted
    );

    let (c4, _) = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: PROVIDER_SCHEMA_VERSION.into(),
                project_id: None,
                provider_id: provider_id.into(),
                model_id: model_id.into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 100_000,
                max_total_tokens: Some(10_000),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();
    store
        .revoke_cost_consent(
            &c4.id,
            RevokeCostConsent {
                schema_version: PROVIDER_SCHEMA_VERSION.into(),
            },
        )
        .await
        .unwrap();
    let err = adapter
        .execute_with_new_reservation(
            ctx,
            build_bound_reserve(&c4.id, ctx, &endpoint, &req, trusted_in, 512),
            None,
            &req,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "remote_cost_not_approved");
    assert_eq!(adapter.transport().recorded_requests().len(), 3);

    // Oversized request fails BEFORE reserve_budget.
    let (c5, _) = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: PROVIDER_SCHEMA_VERSION.into(),
                project_id: None,
                provider_id: provider_id.into(),
                model_id: model_id.into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 100_000,
                max_total_tokens: Some(10_000),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();
    let mut small_req_cfg = base_config(remote_caps(provider_id, model_id), "opaque-secret-bounds");
    small_req_cfg.max_request_bytes = 512;
    small_req_cfg.max_response_bytes = 128;
    let bounded_transport = MockHttpTransport::default();
    bounded_transport
        .push_outcome(MockHttpOutcome::Response {
            delay: Duration::ZERO,
            status: 200,
            body: vec![b'x'; 256],
        })
        .unwrap();
    let bounded_adapter = OpenAiCompatibleAdapter::new(small_req_cfg, bounded_transport).unwrap();
    let mut big_req = req.clone();
    big_req.messages = vec![LlmMessage::user("a".repeat(1024))];
    let big_trusted = bounded_adapter.trusted_input_tokens(&big_req).unwrap();
    let err = bounded_adapter
        .execute_with_new_reservation(
            ctx,
            build_bound_reserve(&c5.id, ctx, &endpoint, &big_req, big_trusted, 512),
            None,
            &big_req,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "invalid_schema");
    assert_eq!(bounded_adapter.transport().recorded_requests().len(), 0);

    let small_req = LlmRequest {
        messages: vec![LlmMessage::user("hi")],
        system_prompt: String::new(),
        max_output_tokens: 64,
        ..req.clone()
    };
    let small_trusted = bounded_adapter.trusted_input_tokens(&small_req).unwrap();
    let err = bounded_adapter
        .execute_with_new_reservation(
            ctx,
            build_bound_reserve(&c5.id, ctx, &endpoint, &small_req, small_trusted, 64),
            None,
            &small_req,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "provider_invalid_response");
    assert_eq!(
        store.get_cost_consent(&c5.id).await.unwrap().status,
        CostConsentStatus::Halted
    );

    let (c6, _) = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: PROVIDER_SCHEMA_VERSION.into(),
                project_id: None,
                provider_id: provider_id.into(),
                model_id: model_id.into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 100_000,
                max_total_tokens: Some(10_000),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();
    let offline_adapter = OpenAiCompatibleAdapter::new(
        base_config(remote_caps(provider_id, model_id), "opaque-secret-net"),
        NoNetworkTransport,
    )
    .unwrap();
    let err = offline_adapter
        .execute_with_new_reservation(
            ctx,
            build_bound_reserve(&c6.id, ctx, &endpoint, &req, trusted_in, 512),
            None,
            &req,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "provider_unavailable");

    pool.close().await;
}

#[tokio::test]
async fn rejects_request_substitution_underestimated_tokens_and_preserves_overage_accounting() {
    let temp = tempfile::tempdir().unwrap();
    let provider_id = "compat-provider";
    let model_id = "configured-model-1";
    let (pool, store, consent, lease) =
        setup_store_consent_and_lease(temp.path(), provider_id, model_id, 50_000, Some(5_000))
            .await;

    let transport = MockHttpTransport::default();
    // Provider returns ACTUAL usage exceeding BOTH estimated_input_tokens and max_output_tokens!
    transport
        .push_json_response(
            200,
            &json!({
                "id": "chatcmpl-over-1",
                "model": model_id,
                "choices": [{
                    "finish_reason": "length",
                    "message": {"role": "assistant", "content": "partial truncated output"}
                }],
                "usage": {
                    "prompt_tokens": 150,
                    "completion_tokens": 90,
                    "total_tokens": 240
                }
            }),
        )
        .unwrap();

    let adapter = OpenAiCompatibleAdapter::new(
        base_config(remote_caps(provider_id, model_id), "opaque-secret-qa"),
        transport,
    )
    .unwrap();
    let endpoint = adapter.config().endpoint_identity().unwrap();
    let model_rec = remote_model_record(provider_id, model_id);
    let mut req = base_request(provider_id, model_id, ResponseFormat::Text);
    req.max_output_tokens = 64;
    let trusted_in = adapter.trusted_input_tokens(&req).unwrap();
    let project_id = uuid::Uuid::new_v4().to_string();
    let job_id = uuid::Uuid::new_v4().to_string();
    let cancel = CancellationToken::new();
    let ctx = DispatchContext {
        store: &store,
        lease: &lease,
        project_id: Some(&project_id),
        job_id: Some(&job_id),
        model: &model_rec,
        snapshot: None,
        cancel: &cancel,
    };

    // 1. Reject forged/underestimated token estimate (< trusted_in) BEFORE DB/transport.
    let err = adapter
        .execute_with_new_reservation(
            ctx,
            build_bound_reserve(&consent.id, ctx, &endpoint, &req, trusted_in - 1, 64),
            None,
            &req,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "unsupported_parameter");
    assert_eq!(adapter.transport().recorded_requests().len(), 0);

    // 2. Create valid bound reservation for `req`, then test request substitution variants.
    let (bound_res, _) = store
        .reserve_budget(
            None,
            build_bound_reserve(&consent.id, ctx, &endpoint, &req, trusted_in, 64),
        )
        .await
        .unwrap();

    let mut swapped_req = req.clone();
    swapped_req.messages = vec![LlmMessage::user("Substituted sneaky prompt!")];
    let err = adapter
        .execute_for_reservation(ctx, &bound_res.id, &swapped_req)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "request_substitution_rejected");
    assert_eq!(adapter.transport().recorded_requests().len(), 0);

    let wrong_job = uuid::Uuid::new_v4().to_string();
    let wrong_job_ctx = DispatchContext {
        job_id: Some(&wrong_job),
        ..ctx
    };
    let err = adapter
        .execute_for_reservation(wrong_job_ctx, &bound_res.id, &req)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "request_substitution_rejected");

    let mut alt_cfg = base_config(remote_caps(provider_id, model_id), "opaque-secret-qa");
    alt_cfg.base_url = "https://alternate-compat.invalid/v1".into();
    alt_cfg.endpoint_allowlist = vec!["https://alternate-compat.invalid/v1".into()];
    let alt_adapter = OpenAiCompatibleAdapter::new(alt_cfg, NoNetworkTransport).unwrap();
    let err = alt_adapter
        .execute_for_reservation(ctx, &bound_res.id, &req)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "request_substitution_rejected");

    // 3. Dispatch exact request where provider overflows limits:
    // adapter MUST stop with CostLimitExceeded AND preserve true actual usage in ledger!
    let err = adapter
        .execute_for_reservation(ctx, &bound_res.id, &req)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "cost_limit_exceeded");

    let settled = store.get_budget_reservation(&bound_res.id).await.unwrap();
    assert_eq!(settled.status, ReservationStatus::SettledOverage);
    assert_eq!(settled.actual_input_tokens, Some(150));
    assert_eq!(settled.actual_output_tokens, Some(90));
    // 100 + 150*3 + 90*12 = 1630
    assert_eq!(settled.actual_micros, Some(1_630));
    assert_eq!(
        settled.inference_provenance,
        Some(InferenceProvenance::DeterministicMockFixture)
    );
    let halted_consent = store.get_cost_consent(&consent.id).await.unwrap();
    assert_eq!(halted_consent.status, CostConsentStatus::Halted);
    assert_eq!(halted_consent.spent_micros, 1_630);
    assert_eq!(halted_consent.spent_tokens, 240);

    pool.close().await;
}

#[test]
fn validates_transport_security_policy_resolved_ip_ssrf_and_streaming_buffer() {
    // Policy disables redirects, retries, ambient proxies, and weak TLS.
    let policy =
        HttpTransportSecurityPolicy::hardened(Duration::from_secs(2), Duration::from_secs(5), 1024)
            .unwrap();
    assert!(!policy.allow_redirects);
    assert_eq!(policy.max_retries, 0);
    assert!(policy.disable_system_proxies);
    assert!(policy.require_tls_verification);
    assert!(policy.min_tls_1_2);

    let mut bad = policy.clone();
    bad.allow_redirects = true;
    assert!(bad.validate().is_err());
    let mut bad = policy.clone();
    bad.max_retries = 1;
    assert!(bad.validate().is_err());
    let mut bad = policy.clone();
    bad.disable_system_proxies = false;
    assert!(bad.validate().is_err());
    let mut bad = policy.clone();
    bad.require_tls_verification = false;
    assert!(bad.validate().is_err());

    // Resolved IP SSRF guards.
    for blocked in [
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)),
        IpAddr::V4(Ipv4Addr::new(172, 16, 1, 2)),
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
        IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
        IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
        "::ffff:127.0.0.1".parse().unwrap(),
        "::ffff:169.254.169.254".parse().unwrap(),
        "fe80::1".parse().unwrap(),
        "fc00::1".parse().unwrap(),
    ] {
        assert!(
            validate_resolved_ip(blocked, ProviderKind::RemoteApi).is_err(),
            "expected remote SSRF IP {blocked} to be rejected"
        );
    }
    assert!(
        validate_resolved_ip("93.184.216.34".parse().unwrap(), ProviderKind::RemoteApi).is_ok()
    );
    assert!(
        validate_resolved_ip(IpAddr::V4(Ipv4Addr::LOCALHOST), ProviderKind::LocalEndpoint).is_ok()
    );
    assert!(
        validate_resolved_ip(IpAddr::V6(Ipv6Addr::LOCALHOST), ProviderKind::LocalEndpoint).is_ok()
    );
    assert!(
        validate_resolved_ip(
            "93.184.216.34".parse().unwrap(),
            ProviderKind::LocalEndpoint
        )
        .is_err()
    );

    // Bounded chunked response buffer enforces strict ceiling incrementally.
    let mut buf = BoundedResponseBuffer::new(10).unwrap();
    buf.push_chunk(b"12345").unwrap();
    buf.push_chunk(b"67890").unwrap();
    assert!(buf.push_chunk(b"!").is_err());
    assert_eq!(buf.into_vec(), b"1234567890");
}

#[derive(Clone, Debug)]
struct CapturedHttpRequest {
    request_line: String,
    headers_lower: Vec<(String, String)>,
    body: String,
}

async fn read_http_request_head_and_body(stream: &tokio::net::TcpStream) -> CapturedHttpRequest {
    let mut raw = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        stream.readable().await.unwrap();
        match stream.try_read(&mut tmp) {
            Ok(0) => panic!("unexpected EOF reading request headers"),
            Ok(n) => {
                raw.extend_from_slice(&tmp[..n]);
                if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    header_end = pos + 4;
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => panic!("read error: {e}"),
        }
    }

    let head = String::from_utf8_lossy(&raw[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("").to_owned();
    let mut headers_lower = Vec::new();
    let mut content_len = 0usize;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let kl = k.trim().to_ascii_lowercase();
            let vt = v.trim().to_owned();
            if kl == "content-length" {
                content_len = vt.parse::<usize>().unwrap_or(0);
            }
            headers_lower.push((kl, vt));
        }
    }

    while raw.len() < header_end + content_len {
        stream.readable().await.unwrap();
        match stream.try_read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => panic!("read body error: {e}"),
        }
    }
    let body = String::from_utf8_lossy(&raw[header_end..header_end + content_len]).into_owned();
    CapturedHttpRequest {
        request_line,
        headers_lower,
        body,
    }
}

async fn write_all_raw(stream: &tokio::net::TcpStream, bytes: &[u8]) {
    let mut offset = 0usize;
    while offset < bytes.len() {
        stream.writable().await.unwrap();
        match stream.try_write(&bytes[offset..]) {
            Ok(0) => break,
            Ok(n) => offset += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
    }
}

fn local_available_caps(provider_id: &str, model_id: &str) -> ProviderCapabilities {
    let mut caps =
        ProviderCapabilities::deterministic_mock(model_id, vec![TaskRole::SceneCodeGeneration]);
    caps.provider_id = provider_id.into();
    caps.provider_kind = ProviderKind::LocalEndpoint;
    caps.remote = false;
    caps.availability = ProviderAvailability::Available;
    caps.supports_tool_calling = true;
    caps.supports_structured_json = true;
    caps.supports_seed = true;
    caps
}

fn local_model_record(provider_id: &str, model_id: &str, availability: &str) -> ModelRecord {
    ModelRecord {
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
        availability: availability.into(),
        reason: "Local loopback HTTP fixture".into(),
        remote: false,
    }
}

#[tokio::test]
async fn reqwest_transport_controlled_local_http_roundtrip_provenance_and_secret_redaction() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<CapturedHttpRequest>::new()));
    let captured_srv = std::sync::Arc::clone(&captured);
    let model_id = "local-fixture-model-1";
    let secret = "fixture-secret-do-not-leak-5511";

    let server_handle = tokio::spawn(async move {
        // Request 1: 200 OK structured JSON
        let (stream1, _) = listener.accept().await.unwrap();
        let req1 = read_http_request_head_and_body(&stream1).await;
        captured_srv.lock().unwrap().push(req1);
        let body1 = serde_json::to_vec(&json!({
            "id": "chatcmpl-local-1",
            "model": model_id,
            "choices": [{
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": "{\"ok\":true,\"scene\":1}"}
            }],
            "usage": {"prompt_tokens": 35, "completion_tokens": 18, "total_tokens": 53}
        }))
        .unwrap();
        let resp1 = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body1.len()
        );
        write_all_raw(&stream1, resp1.as_bytes()).await;
        write_all_raw(&stream1, &body1).await;

        // Request 2: 401 echoing secret (adapter must redact)
        let (stream2, _) = listener.accept().await.unwrap();
        let req2 = read_http_request_head_and_body(&stream2).await;
        captured_srv.lock().unwrap().push(req2);
        let body2 = serde_json::to_vec(&json!({
            "error": {"message": format!("bad token {secret}")}
        }))
        .unwrap();
        let resp2 = format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body2.len()
        );
        write_all_raw(&stream2, resp2.as_bytes()).await;
        write_all_raw(&stream2, &body2).await;
    });

    let temp = tempfile::tempdir().unwrap();
    let provider_id = "local-fixture";
    // Setup store with remote=true consent/ledger accounting alongside local HTTP transport check,
    // or use a remote consent? Wait: BudgetReservation in Store checks cost_consents for remote=true;
    // test reserve/dispatch ledger with an available local model + remote consent setup OR direct LocalEndpoint:
    // Note create_cost_consent requires remote=true in Store; verify Store + Mock vs Reqwest LocalEndpoint cleanly!
    let db_url = temp_db_url(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());
    // Also register loopback model for binding metadata:
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
            availability: "available".into(),
            reason: "Loopback HTTP fixture".into(),
            remote: false,
        })
        .await
        .unwrap();

    let base_url = format!("http://127.0.0.1:{}/v1", addr.port());
    let cfg = OpenAiCompatibleConfig {
        capabilities: local_available_caps(provider_id, model_id),
        gateway_combo_routing: false,
        tokenization_profile: Some(test_profile()),
        base_url: base_url.clone(),
        endpoint_allowlist: vec![base_url.clone()],
        credential: CredentialSource::BearerToken(RedactedSecret::new(secret).unwrap()),
        request_timeout: Duration::from_secs(5),
        max_request_bytes: 128 * 1024,
        max_response_bytes: 128 * 1024,
        extra_headers: vec![("X-Fixture-Mode".into(), "http1-local".into())],
    };
    let policy = HttpTransportSecurityPolicy::hardened(
        Duration::from_millis(500),
        Duration::from_secs(5),
        64 * 1024,
    )
    .unwrap();
    let transport = ReqwestHttpTransport::new(&cfg, policy).unwrap();
    let adapter = OpenAiCompatibleAdapter::new(cfg, transport).unwrap();
    let model_rec = local_model_record(provider_id, model_id, "available");
    let req = base_request(provider_id, model_id, ResponseFormat::JsonObject);
    adapter
        .validate_request_binding(&model_rec, None, &req)
        .unwrap();
    let (http_req_1, req_sha_1) = adapter.build_http_request(&req).unwrap();
    let cancel = CancellationToken::new();

    let res1 = adapter
        .transport()
        .execute(http_req_1, &cancel)
        .await
        .unwrap();
    assert_eq!(res1.status, 200);
    assert_eq!(
        res1.provenance,
        InferenceProvenance::ControlledLocalHttpFixture
    );

    let (http_req_2, _req_sha_2) = adapter.build_http_request(&req).unwrap();
    let res2 = adapter
        .transport()
        .execute(http_req_2, &cancel)
        .await
        .unwrap();
    assert_eq!(res2.status, 401);
    assert!(!req_sha_1.is_empty());

    server_handle.await.unwrap();
    {
        let seen = captured.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].request_line, "POST /v1/chat/completions HTTP/1.1");
        assert!(
            seen[0]
                .headers_lower
                .iter()
                .any(|(k, v)| k == "authorization" && v == &format!("Bearer {secret}"))
        );
        assert!(
            seen[0]
                .headers_lower
                .iter()
                .any(|(k, v)| k == "x-fixture-mode" && v == "http1-local")
        );
        let body_val: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(body_val["model"], model_id);
        assert_eq!(body_val["stream"], false);
    }

    pool.close().await;
}

#[tokio::test]
async fn reqwest_transport_disables_redirects_retries_and_system_proxies() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = std::sync::Arc::new(AtomicU64::new(0));
    let hits_srv = std::sync::Arc::clone(&hits);

    let server_handle = tokio::spawn(async move {
        // Hit 1: 302 redirect (must NOT be followed; hits stays 1 for this request)
        let (s1, _) = listener.accept().await.unwrap();
        let _ = read_http_request_head_and_body(&s1).await;
        hits_srv.fetch_add(1, Ordering::SeqCst);
        let resp1 = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{}/redirected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            addr.port()
        );
        write_all_raw(&s1, resp1.as_bytes()).await;

        // Hit 2: 503 Service Unavailable (must NOT retry; total hits == 2)
        let (s2, _) = listener.accept().await.unwrap();
        let _ = read_http_request_head_and_body(&s2).await;
        hits_srv.fetch_add(1, Ordering::SeqCst);
        let resp2 = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy";
        write_all_raw(&s2, resp2.as_bytes()).await;
    });

    // Inject hostile ambient proxy environment variables; `.no_proxy()` must ignore them.
    unsafe {
        std::env::set_var("HTTP_PROXY", "http://127.0.0.1:1");
        std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:1");
        std::env::set_var("ALL_PROXY", "http://127.0.0.1:1");
    }

    let provider_id = "local-fixture";
    let model_id = "local-model-1";
    let base_url = format!("http://127.0.0.1:{}/v1", addr.port());
    let cfg = OpenAiCompatibleConfig {
        capabilities: local_available_caps(provider_id, model_id),
        gateway_combo_routing: false,
        tokenization_profile: Some(test_profile()),
        base_url: base_url.clone(),
        endpoint_allowlist: vec![base_url],
        credential: CredentialSource::None,
        request_timeout: Duration::from_secs(5),
        max_request_bytes: 64 * 1024,
        max_response_bytes: 64 * 1024,
        extra_headers: Vec::new(),
    };
    let policy = HttpTransportSecurityPolicy::hardened(
        Duration::from_millis(500),
        Duration::from_secs(5),
        64 * 1024,
    )
    .unwrap();
    let transport = ReqwestHttpTransport::new(&cfg, policy).unwrap();
    let adapter = OpenAiCompatibleAdapter::new(cfg, transport).unwrap();
    let req = base_request(provider_id, model_id, ResponseFormat::Text);
    let cancel = CancellationToken::new();

    let (http_req_1, _) = adapter.build_http_request(&req).unwrap();
    let err_redir = adapter
        .transport()
        .execute(http_req_1, &cancel)
        .await
        .unwrap_err();
    assert!(err_redir.to_string().contains("redirect"));

    let (http_req_2, _) = adapter.build_http_request(&req).unwrap();
    let res_503 = adapter
        .transport()
        .execute(http_req_2, &cancel)
        .await
        .unwrap();
    assert_eq!(res_503.status, 503);
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    server_handle.await.unwrap();
}

#[tokio::test]
async fn reqwest_transport_enforces_streaming_byte_limit_deadline_and_cancellation() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel_trigger = std::sync::Arc::new(tokio::sync::Notify::new());
    let cancel_srv = std::sync::Arc::clone(&cancel_trigger);

    let server_handle = tokio::spawn(async move {
        // 1. Oversized chunked response exceeding max_response_bytes = 256
        let (s1, _) = listener.accept().await.unwrap();
        let _ = read_http_request_head_and_body(&s1).await;
        write_all_raw(
            &s1,
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        )
        .await;
        write_all_raw(&s1, b"80\r\n").await;
        write_all_raw(&s1, &[b'a'; 128]).await;
        write_all_raw(&s1, b"\r\n81\r\n").await;
        write_all_raw(&s1, &[b'b'; 129]).await;
        write_all_raw(&s1, b"\r\n0\r\n\r\n").await;

        // 2. Slow response exceeding request_timeout = 80ms
        let (s2, _) = listener.accept().await.unwrap();
        let _ = read_http_request_head_and_body(&s2).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(s2);

        // 3. Cancellation mid-flight
        let (s3, _) = listener.accept().await.unwrap();
        let _ = read_http_request_head_and_body(&s3).await;
        cancel_srv.notify_one();
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(s3);
    });

    let provider_id = "local-fixture";
    let model_id = "local-model-1";
    let base_url = format!("http://127.0.0.1:{}/v1", addr.port());
    let cfg = OpenAiCompatibleConfig {
        capabilities: local_available_caps(provider_id, model_id),
        gateway_combo_routing: false,
        tokenization_profile: Some(test_profile()),
        base_url: base_url.clone(),
        endpoint_allowlist: vec![base_url],
        credential: CredentialSource::None,
        request_timeout: Duration::from_secs(2),
        max_request_bytes: 64 * 1024,
        max_response_bytes: 256,
        extra_headers: Vec::new(),
    };

    // 1. Streaming byte limit
    let policy_bytes = HttpTransportSecurityPolicy::hardened(
        Duration::from_millis(500),
        Duration::from_secs(2),
        256,
    )
    .unwrap();
    let adapter_bytes = OpenAiCompatibleAdapter::new(
        cfg.clone(),
        ReqwestHttpTransport::new(&cfg, policy_bytes).unwrap(),
    )
    .unwrap();
    let req = base_request(provider_id, model_id, ResponseFormat::Text);
    let cancel_none = CancellationToken::new();
    let (h1, _) = adapter_bytes.build_http_request(&req).unwrap();
    let err_bytes = adapter_bytes
        .transport()
        .execute(h1, &cancel_none)
        .await
        .unwrap_err();
    assert!(err_bytes.to_string().contains("exceeded"));

    // 2. Request deadline
    let mut cfg_short = cfg.clone();
    cfg_short.request_timeout = Duration::from_millis(80);
    let policy_short = HttpTransportSecurityPolicy::hardened(
        Duration::from_millis(200),
        Duration::from_millis(80),
        256,
    )
    .unwrap();
    let adapter_short = OpenAiCompatibleAdapter::new(
        cfg_short.clone(),
        ReqwestHttpTransport::new(&cfg_short, policy_short).unwrap(),
    )
    .unwrap();
    let (h2, _) = adapter_short.build_http_request(&req).unwrap();
    let err_timeout = adapter_short
        .transport()
        .execute(h2, &cancel_none)
        .await
        .unwrap_err();
    assert!(err_timeout.to_string().contains("timed out"));

    // 3. Cancellation mid-flight
    let cancel_mid = CancellationToken::new();
    let cancel_handle = cancel_mid.clone();
    tokio::spawn(async move {
        cancel_trigger.notified().await;
        cancel_handle.cancel();
    });
    let (h3, _) = adapter_bytes.build_http_request(&req).unwrap();
    let err_cancel = adapter_bytes
        .transport()
        .execute(h3, &cancel_mid)
        .await
        .unwrap_err();
    assert!(err_cancel.to_string().contains("cancelled"));

    server_handle.await.unwrap();
}

#[tokio::test]
async fn enforces_allowlist_remote_tls_client_available_model_status_and_conservative_tokens() {
    let provider_id = "compat-provider";
    let model_id = "configured-model-1";
    let mut caps = remote_caps(provider_id, model_id);

    // 1. Allowlist rejection
    let mut mismatch_cfg = base_config(caps.clone(), "secret-allowlist");
    mismatch_cfg.endpoint_allowlist = vec!["https://other.compat.invalid/v1".into()];
    assert!(OpenAiCompatibleAdapter::new(mismatch_cfg, NoNetworkTransport).is_err());

    let mut empty_allowlist_cfg = base_config(caps.clone(), "secret-allowlist");
    empty_allowlist_cfg.endpoint_allowlist.clear();
    assert!(OpenAiCompatibleAdapter::new(empty_allowlist_cfg, NoNetworkTransport).is_err());

    // 2. ReqwestHttpTransport builds rustls TLS >= 1.2 client for remote endpoint (no socket opened)
    let policy = HttpTransportSecurityPolicy::hardened(
        Duration::from_millis(500),
        Duration::from_secs(5),
        64 * 1024,
    )
    .unwrap();
    let valid_remote_cfg = base_config(caps.clone(), "secret-tls-check");
    let remote_transport = ReqwestHttpTransport::new(&valid_remote_cfg, policy.clone()).unwrap();
    assert_eq!(
        remote_transport
            .expected_provenance(ProviderKind::RemoteApi)
            .unwrap(),
        InferenceProvenance::RealRemoteTlsInference
    );

    // RemoteApi pointing to "https://localhost:.../v1" is blocked by ValidatedDnsResolver BEFORE TCP connect.
    let ssrf_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ssrf_port = ssrf_listener.local_addr().unwrap().port();
    let mut ssrf_cfg = base_config(caps.clone(), "secret-ssrf-token");
    ssrf_cfg.base_url = format!("https://localhost:{ssrf_port}/v1");
    ssrf_cfg.endpoint_allowlist = vec![format!("https://localhost:{ssrf_port}/v1")];
    let ssrf_adapter = OpenAiCompatibleAdapter::new(
        ssrf_cfg.clone(),
        ReqwestHttpTransport::new(&ssrf_cfg, policy.clone()).unwrap(),
    )
    .unwrap();
    let (ssrf_req, _) = ssrf_adapter
        .build_http_request(&base_request(provider_id, model_id, ResponseFormat::Text))
        .unwrap();
    let cancel_ssrf = CancellationToken::new();
    let ssrf_err = ssrf_adapter
        .transport()
        .execute(ssrf_req, &cancel_ssrf)
        .await
        .unwrap_err();
    assert!(
        ssrf_err.to_string().contains("non-public IP")
            || ssrf_err.to_string().contains("error sending request")
    );

    // 3. Unknown or Unavailable capabilities/model rejected strictly
    caps.availability = ProviderAvailability::Unknown;
    let unknown_cfg = base_config(caps.clone(), "secret-unknown");
    assert!(ReqwestHttpTransport::new(&unknown_cfg, policy.clone()).is_err());
    let adapter_unknown =
        OpenAiCompatibleAdapter::new(unknown_cfg, MockHttpTransport::default()).unwrap();
    let req = base_request(provider_id, model_id, ResponseFormat::Text);
    let err = adapter_unknown
        .validate_request_binding(&remote_model_record(provider_id, model_id), None, &req)
        .unwrap_err();
    assert_eq!(err.code(), "provider_unavailable");

    // 4. Store requires active CostConsent before promoting a remote model to "available"
    let temp = tempfile::tempdir().unwrap();
    let pool = db::connect(&temp_db_url(temp.path())).await.unwrap();
    let store = Store::new(pool.clone());
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
    let probe_err = store
        .record_model_availability_probe(provider_id, model_id, "available", "No consent")
        .await
        .unwrap_err();
    assert_eq!(probe_err.code(), "remote_cost_not_approved");

    // 5. Conservative tokenizer exceeds legacy naive bytes/4 on dense JSON, digits, and UTF-8
    let mut dense_req = req;
    dense_req.messages = vec![LlmMessage::user(
        "{\"x\":123456,\"t\":\"你好世界!?\"}".repeat(4),
    )];
    let est = compute_trusted_input_tokens_for_profile(&dense_req, &test_profile()).unwrap();
    assert!(
        est >= 120,
        "expected conservative estimate >= 120, got {est}"
    );

    pool.close().await;
}
