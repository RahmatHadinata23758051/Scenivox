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
        HttpTransportSecurityPolicy, MockHttpOutcome, MockHttpTransport, NoNetworkTransport,
        OpenAiCompatibleAdapter, OpenAiCompatibleConfig, RedactedSecret,
        compute_model_snapshot_sha256, compute_trusted_input_tokens, validate_resolved_ip,
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
        availability: ProviderAvailability::Unknown,
        reason: "Configured OpenAI-compatible remote endpoint".into(),
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
        availability: "unknown".into(),
        reason: "Opt-in remote model".into(),
        remote: true,
    }
}

fn base_config(caps: ProviderCapabilities, secret: &str) -> OpenAiCompatibleConfig {
    OpenAiCompatibleConfig {
        capabilities: caps,
        base_url: "https://api.compat.invalid/v1/".into(),
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
    let trusted_1 = compute_trusted_input_tokens(&req_1).unwrap().max(100);
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
    let trusted_2 = compute_trusted_input_tokens(&req_2).unwrap().max(160);
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
    let trusted_in = compute_trusted_input_tokens(&req).unwrap();
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
    let trusted_in = compute_trusted_input_tokens(&req).unwrap().max(100);

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
        tokio::time::sleep(Duration::from_millis(25)).await;
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
    let trusted_in = compute_trusted_input_tokens(&req).unwrap().max(100);
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
    let big_trusted = compute_trusted_input_tokens(&big_req).unwrap();
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
    let small_trusted = compute_trusted_input_tokens(&small_req).unwrap();
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
    let trusted_in = compute_trusted_input_tokens(&req).unwrap();
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
