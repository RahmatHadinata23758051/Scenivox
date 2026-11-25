use std::{
    fs,
    path::Path,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use scenivox::{
    api,
    cost::{
        ClaimDispatchBinding, CostConsent, CostConsentStatus, CostGateDecision, CostScope,
        CreateCostConsent, InferenceProvenance, ModelPricingRecord, PricingSnapshot,
        ProviderUsageReport, ReconcileBudgetRequest, ReservationStatus, ReserveBudgetRequest,
        RevokeCostConsent, SetModelPricing, compute_usage_cost_micros,
    },
    db,
    domain::{
        CreateProject, CreateRevision, ModelSelection, Parameters, RegisterModel, ReviewCommand,
        RevisionInput, SceneInput, StoryboardInput, Target,
    },
    store::Store,
};
use serde_json::{Value, json};
use tower::ServiceExt;

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

fn temp_db_url(temp: &Path) -> String {
    let idx = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
    format!(
        "sqlite://{}?mode=rwc",
        temp.join(format!("cost-ledger-{idx}.db"))
            .to_string_lossy()
            .replace('\\', "/")
    )
}

async fn seed_remote_model(store: &Store, provider: &str, model: &str) {
    store
        .register_model(RegisterModel {
            schema_version: "1".into(),
            provider_id: provider.into(),
            model_id: model.into(),
            adapter_version: "1.0.0".into(),
            task_roles: vec!["scene_code_generation".into(), "storyboard_planning".into()],
            parameters: Parameters {
                temperature: 0.0,
                seed: Some(1),
            },
            enabled: true,
            configured: true,
            availability: "unknown".into(),
            reason: "Opt-in remote model".into(),
            remote: true,
        })
        .await
        .unwrap();
}

async fn seed_verified_pricing(store: &Store, provider: &str, model: &str) -> ModelPricingRecord {
    store
        .set_model_pricing(SetModelPricing {
            schema_version: "1".into(),
            provider_id: provider.into(),
            model_id: model.into(),
            pricing_version: "2026-11-v1".into(),
            currency: "USD".into(),
            input_cost_per_token_micros: 2,
            output_cost_per_token_micros: 10,
            request_fixed_cost_micros: 100,
            verified: true,
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn cost_consent_scopes_model_validation_token_ceiling_stale_pricing_and_expiry() {
    let temp = tempfile::tempdir().unwrap();
    let db_url = temp_db_url(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    seed_remote_model(&store, "remote-ai", "gen-v1").await;

    let target = Target {
        duration_ms: 5_000,
        aspect_ratio: "9:16".into(),
        fps: 30,
        language: "id-ID".into(),
        style_id: "stick-figure".into(),
    };
    let project = store
        .create_project(CreateProject {
            schema_version: "1".into(),
            title: "Cost Scope Test".into(),
            idea: "Tes biaya".into(),
            target: target.clone(),
            model_selection: ModelSelection::default(),
        })
        .await
        .unwrap();
    let offline_rev = store
        .plan_storyboard(
            &project.id,
            None,
            scenivox::domain::PlanStoryboard {
                schema_version: "1".into(),
                idea: "Tes biaya".into(),
                target: target.clone(),
                expected_revision_id: None,
                model_selection: ModelSelection::default(),
            },
            &scenivox::planner::DeterministicMockPlanner,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .check_revision_cost_gate(
                &project.id,
                &offline_rev.id,
                None,
                CostScope::SceneCodeGeneration,
            )
            .await
            .unwrap(),
        CostGateDecision::ZeroCostOffline
    );

    // 1. Missing or invalid model metadata is NEVER treated as zero-cost execution!
    let mut missing_model_rev = serde_json::to_value(&offline_rev.content).unwrap();
    missing_model_rev
        .as_object_mut()
        .unwrap()
        .remove("planning_provenance");
    missing_model_rev["storyboard"]["scenes"][0]["id"] = Value::Null;
    missing_model_rev["models"]["scene_code_generation"]["model_id"] = json!("missing-model");
    let bad_model_rev = store
        .create_revision(
            &project.id,
            CreateRevision {
                schema_version: "1".into(),
                expected_revision_id: Some(offline_rev.id.clone()),
                content: serde_json::from_value(missing_model_rev).unwrap(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .check_revision_cost_gate(
                &project.id,
                &bad_model_rev.id,
                None,
                CostScope::SceneCodeGeneration,
            )
            .await
            .unwrap_err()
            .code(),
        "invalid_model_selection"
    );

    // Unavailable/disabled model rejected explicitly instead of assuming zero-cost.
    store
        .register_model(RegisterModel {
            schema_version: "1".into(),
            provider_id: "mock".into(),
            model_id: "disabled-local".into(),
            adapter_version: "1".into(),
            task_roles: vec!["scene_code_generation".into()],
            parameters: Parameters {
                temperature: 0.0,
                seed: Some(0),
            },
            enabled: false,
            configured: true,
            availability: "unavailable".into(),
            reason: "Disabled in test".into(),
            remote: false,
        })
        .await
        .unwrap();
    let mut dis_rev_json = serde_json::to_value(&bad_model_rev.content).unwrap();
    dis_rev_json
        .as_object_mut()
        .unwrap()
        .remove("planning_provenance");
    dis_rev_json["storyboard"]["scenes"][0]["id"] = Value::Null;
    dis_rev_json["models"]["scene_code_generation"]["model_id"] = json!("disabled-local");
    let dis_rev = store
        .create_revision(
            &project.id,
            CreateRevision {
                schema_version: "1".into(),
                expected_revision_id: Some(bad_model_rev.id.clone()),
                content: serde_json::from_value(dis_rev_json).unwrap(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .check_revision_cost_gate(
                &project.id,
                &dis_rev.id,
                None,
                CostScope::SceneCodeGeneration,
            )
            .await
            .unwrap_err()
            .code(),
        "provider_unavailable"
    );

    // 2. Offline model cannot take paid CostConsent.
    let err_offline_consent = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: "1".into(),
                project_id: Some(project.id.clone()),
                provider_id: "mock".into(),
                model_id: "coder-v1".into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 1_000,
                max_total_tokens: None,
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err_offline_consent.code(), "invalid_model_selection");

    // 3. Missing/unverified pricing fails closed with 503 provider_pricing_unavailable.
    let no_price_err = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: "1".into(),
                project_id: Some(project.id.clone()),
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 10_000,
                max_total_tokens: Some(30),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(no_price_err.code(), "provider_pricing_unavailable");

    // 4. Verified pricing allows scoped consent; enforce strict lease & monetary/token ceilings.
    let pricing = seed_verified_pricing(&store, "remote-ai", "gen-v1").await;
    let (consent, _) = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: "1".into(),
                project_id: Some(project.id.clone()),
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 10_000,
                max_total_tokens: Some(25),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(consent.pricing_sha256, pricing.pricing_sha256);

    let owner_id = uuid::Uuid::new_v4().to_string();
    // Missing lease MUST be rejected (strict worker ownership)!
    assert_eq!(
        store
            .reserve_budget(
                None,
                ReserveBudgetRequest {
                    cost_consent_id: consent.id.clone(),
                    project_id: Some(project.id.clone()),
                    job_id: None,
                    provider_id: "remote-ai".into(),
                    model_id: "gen-v1".into(),
                    scope: CostScope::SceneCodeGeneration,
                    owner_id: owner_id.clone(),
                    lease_epoch: 1,
                    canonical_request_sha256: "1".repeat(64),
                    model_snapshot_sha256: "2".repeat(64),
                    endpoint_identity: "https://remote-ai.invalid/v1".into(),
                    estimated_input_tokens: 10,
                    max_output_tokens: 10,
                },
            )
            .await
            .unwrap_err()
            .code(),
        "reservation_ownership_required"
    );

    let lease = store.acquire_worker_lease(&owner_id, 60).await.unwrap();

    // Enforce token ceiling even when monetary cost is far below max_cost_micros!
    // Max tokens = 25; reserve 10+15 = 25 fits, then next 1+0 overflows token ceiling.
    let (tok_res, _) = store
        .reserve_budget(
            None,
            ReserveBudgetRequest {
                cost_consent_id: consent.id.clone(),
                project_id: Some(project.id.clone()),
                job_id: None,
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scope: CostScope::SceneCodeGeneration,
                owner_id: owner_id.clone(),
                lease_epoch: lease.lease_epoch,
                canonical_request_sha256: "1".repeat(64),
                model_snapshot_sha256: "2".repeat(64),
                endpoint_identity: "https://remote-ai.invalid/v1".into(),
                estimated_input_tokens: 10,
                max_output_tokens: 15,
            },
        )
        .await
        .unwrap();
    assert_eq!(tok_res.reserved_tokens, 25);
    assert_eq!(
        store
            .reserve_budget(
                None,
                ReserveBudgetRequest {
                    cost_consent_id: consent.id.clone(),
                    project_id: Some(project.id.clone()),
                    job_id: None,
                    provider_id: "remote-ai".into(),
                    model_id: "gen-v1".into(),
                    scope: CostScope::SceneCodeGeneration,
                    owner_id: owner_id.clone(),
                    lease_epoch: lease.lease_epoch,
                    canonical_request_sha256: "1".repeat(64),
                    model_snapshot_sha256: "2".repeat(64),
                    endpoint_identity: "https://remote-ai.invalid/v1".into(),
                    estimated_input_tokens: 1,
                    max_output_tokens: 0,
                },
            )
            .await
            .unwrap_err()
            .code(),
        "cost_limit_exceeded"
    );

    // Remote revision gate also enforces BOTH monetary & token headroom cleanly.
    let mut remote_models = offline_rev.content.models.clone();
    remote_models.scene_code_generation.provider_id = "remote-ai".into();
    remote_models.scene_code_generation.model_id = "gen-v1".into();
    remote_models.scene_code_generation.adapter_version = "1.0.0".into();
    let remote_rev = store
        .create_revision(
            &project.id,
            CreateRevision {
                schema_version: "1".into(),
                expected_revision_id: Some(dis_rev.id.clone()),
                content: RevisionInput {
                    schema_version: "1".into(),
                    idea: "Tes biaya".into(),
                    target,
                    storyboard: StoryboardInput {
                        synopsis: "Sinopsis".into(),
                        hook: "Hook".into(),
                        scenes: vec![SceneInput {
                            id: None,
                            order: 0,
                            duration_ms: 5_000,
                            visual_direction: "Adegan".into(),
                            camera: "Wide".into(),
                            action: "Jalan".into(),
                            narration: "Narasi".into(),
                            caption_hint: "".into(),
                            sfx_cues: vec![],
                        }],
                    },
                    models: remote_models,
                    asset_hashes: vec![],
                    skill_id: "stick-figure".into(),
                    skill_version: "0.1.0".into(),
                    planning_provenance: None,
                },
            },
        )
        .await
        .unwrap();
    store
        .review_revision(
            &project.id,
            &remote_rev.id,
            None,
            ReviewCommand {
                schema_version: "1".into(),
                content_sha256: remote_rev.content_sha256.clone(),
            },
        )
        .await
        .unwrap();
    store
        .approve_revision(
            &project.id,
            &remote_rev.id,
            None,
            ReviewCommand {
                schema_version: "1".into(),
                content_sha256: remote_rev.content_sha256.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .check_revision_cost_gate(
                &project.id,
                &remote_rev.id,
                Some(&consent.id),
                CostScope::SceneCodeGeneration,
            )
            .await
            .unwrap_err()
            .code(),
        "cost_limit_exceeded"
    );

    // 5. Repricing model_pricing after consent creation makes the old consent stale without silently repricing!
    let (consent_b, _) = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: "1".into(),
                project_id: Some(project.id.clone()),
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 10_000,
                max_total_tokens: Some(500),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();
    store
        .set_model_pricing(SetModelPricing {
            schema_version: "1".into(),
            provider_id: "remote-ai".into(),
            model_id: "gen-v1".into(),
            pricing_version: "2026-12-v2".into(),
            currency: "USD".into(),
            input_cost_per_token_micros: 5,
            output_cost_per_token_micros: 20,
            request_fixed_cost_micros: 200,
            verified: true,
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .reserve_budget(
                None,
                ReserveBudgetRequest {
                    cost_consent_id: consent_b.id.clone(),
                    project_id: Some(project.id.clone()),
                    job_id: None,
                    provider_id: "remote-ai".into(),
                    model_id: "gen-v1".into(),
                    scope: CostScope::SceneCodeGeneration,
                    owner_id: owner_id.clone(),
                    lease_epoch: lease.lease_epoch,
                    canonical_request_sha256: "1".repeat(64),
                    model_snapshot_sha256: "2".repeat(64),
                    endpoint_identity: "https://remote-ai.invalid/v1".into(),
                    estimated_input_tokens: 10,
                    max_output_tokens: 10,
                },
            )
            .await
            .unwrap_err()
            .code(),
        "remote_cost_not_approved"
    );
    // Consent snapshot itself remained unchanged!
    assert_eq!(
        store
            .get_cost_consent(&consent_b.id)
            .await
            .unwrap()
            .pricing_sha256,
        pricing.pricing_sha256
    );

    pool.close().await;
}

#[tokio::test]
async fn concurrent_reservations_idempotent_replay_and_integer_overflow_never_double_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let db_url = temp_db_url(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    seed_remote_model(&store, "remote-ai", "gen-v1").await;
    seed_verified_pricing(&store, "remote-ai", "gen-v1").await;
    let owner = uuid::Uuid::new_v4().to_string();
    let lease = store.acquire_worker_lease(&owner, 60).await.unwrap();

    // Fixed=100, in=2, out=10 -> 10 in + 10 out = 220 micros. Max budget 880 = 4 calls.
    let (consent, _) = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: "1".into(),
                project_id: None,
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 880,
                max_total_tokens: None,
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();

    // Idempotent reservation replay MUST NOT authorize duplicate provider dispatch!
    let replay_key = uuid::Uuid::new_v4().to_string();
    let req = ReserveBudgetRequest {
        cost_consent_id: consent.id.clone(),
        project_id: None,
        job_id: None,
        provider_id: "remote-ai".into(),
        model_id: "gen-v1".into(),
        scope: CostScope::SceneCodeGeneration,
        owner_id: owner.clone(),
        lease_epoch: lease.lease_epoch,
        canonical_request_sha256: "1".repeat(64),
        model_snapshot_sha256: "2".repeat(64),
        endpoint_identity: "https://remote-ai.invalid/v1".into(),
        estimated_input_tokens: 10,
        max_output_tokens: 10,
    };
    let binding = ClaimDispatchBinding {
        project_id: None,
        job_id: None,
        provider_id: "remote-ai".into(),
        model_id: "gen-v1".into(),
        scope: CostScope::SceneCodeGeneration,
        canonical_request_sha256: "1".repeat(64),
        model_snapshot_sha256: "2".repeat(64),
        endpoint_identity: "https://remote-ai.invalid/v1".into(),
        max_output_tokens: 10,
    };
    let (first_res, replayed_1) = store
        .reserve_budget(Some(&replay_key), req.clone())
        .await
        .unwrap();
    assert!(!replayed_1);
    assert!(first_res.dispatch_authorized);

    // Replay returns live reservation with dispatch_authorized=false and blocks subsequent dispatch claim.
    let (second_res, replayed_2) = store.reserve_budget(Some(&replay_key), req).await.unwrap();
    assert!(replayed_2);
    assert_eq!(second_res.id, first_res.id);
    assert!(!second_res.dispatch_authorized);
    assert_eq!(
        store
            .claim_reservation_dispatch(&first_res.id, &owner, lease.lease_epoch, &binding)
            .await
            .unwrap_err()
            .code(),
        "cost_reconciliation_failed"
    );

    // Also test normal single-use claim_reservation_dispatch on a non-replayed reservation.
    let (clean_res, _) = store
        .reserve_budget(
            Some(&uuid::Uuid::new_v4().to_string()),
            ReserveBudgetRequest {
                cost_consent_id: consent.id.clone(),
                project_id: None,
                job_id: None,
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scope: CostScope::SceneCodeGeneration,
                owner_id: owner.clone(),
                lease_epoch: lease.lease_epoch,
                canonical_request_sha256: "1".repeat(64),
                model_snapshot_sha256: "2".repeat(64),
                endpoint_identity: "https://remote-ai.invalid/v1".into(),
                estimated_input_tokens: 10,
                max_output_tokens: 10,
            },
        )
        .await
        .unwrap();
    let dispatched = store
        .claim_reservation_dispatch(&clean_res.id, &owner, lease.lease_epoch, &binding)
        .await
        .unwrap();
    assert!(dispatched.dispatch_claimed);
    assert_eq!(
        store
            .claim_reservation_dispatch(&clean_res.id, &owner, lease.lease_epoch, &binding)
            .await
            .unwrap_err()
            .code(),
        "cost_reconciliation_failed"
    );

    // Remaining budget: 880 - 440 = 440 -> 2 concurrent slots out of 6 callers.
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let s = store.clone();
        let cid = consent.id.clone();
        let own = owner.clone();
        let ep = lease.lease_epoch;
        tasks.push(tokio::spawn(async move {
            s.reserve_budget(
                None,
                ReserveBudgetRequest {
                    cost_consent_id: cid,
                    project_id: None,
                    job_id: None,
                    provider_id: "remote-ai".into(),
                    model_id: "gen-v1".into(),
                    scope: CostScope::SceneCodeGeneration,
                    owner_id: own,
                    lease_epoch: ep,
                    canonical_request_sha256: "1".repeat(64),
                    model_snapshot_sha256: "2".repeat(64),
                    endpoint_identity: "https://remote-ai.invalid/v1".into(),
                    estimated_input_tokens: 10,
                    max_output_tokens: 10,
                },
            )
            .await
        }));
    }
    let mut succeeded = 0usize;
    let mut rejected = 0usize;
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => succeeded += 1,
            Err(err) => {
                assert_eq!(err.code(), "cost_limit_exceeded");
                rejected += 1;
            }
        }
    }
    assert_eq!(succeeded, 2);
    assert_eq!(rejected, 4);

    let overflow_pricing = PricingSnapshot {
        pricing_version: "max".into(),
        currency: "USD".into(),
        input_cost_per_token_micros: i64::MAX,
        output_cost_per_token_micros: i64::MAX,
        request_fixed_cost_micros: 1,
        verified: true,
    };
    assert!(compute_usage_cost_micros(&overflow_pricing, 2, 0).is_err());

    pool.close().await;
}

#[tokio::test]
async fn strict_ownership_orphan_recovery_halts_active_and_preserves_revocation_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let db_url = temp_db_url(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    seed_remote_model(&store, "remote-ai", "gen-v1").await;
    seed_verified_pricing(&store, "remote-ai", "gen-v1").await;
    let (consent_active, _) = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: "1".into(),
                project_id: None,
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 1_000,
                max_total_tokens: Some(100),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();
    let (consent_revoked, _) = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: "1".into(),
                project_id: None,
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 1_000,
                max_total_tokens: Some(100),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();

    let worker_a = uuid::Uuid::new_v4().to_string();
    let lease_a = store.acquire_worker_lease(&worker_a, 60).await.unwrap();
    let (res_active, _) = store
        .reserve_budget(
            None,
            ReserveBudgetRequest {
                cost_consent_id: consent_active.id.clone(),
                project_id: None,
                job_id: None,
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scope: CostScope::SceneCodeGeneration,
                owner_id: worker_a.clone(),
                lease_epoch: lease_a.lease_epoch,
                canonical_request_sha256: "1".repeat(64),
                model_snapshot_sha256: "2".repeat(64),
                endpoint_identity: "https://remote-ai.invalid/v1".into(),
                estimated_input_tokens: 10,
                max_output_tokens: 10,
            },
        )
        .await
        .unwrap();
    let (res_for_revoked, _) = store
        .reserve_budget(
            None,
            ReserveBudgetRequest {
                cost_consent_id: consent_revoked.id.clone(),
                project_id: None,
                job_id: None,
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scope: CostScope::SceneCodeGeneration,
                owner_id: worker_a.clone(),
                lease_epoch: lease_a.lease_epoch,
                canonical_request_sha256: "1".repeat(64),
                model_snapshot_sha256: "2".repeat(64),
                endpoint_identity: "https://remote-ai.invalid/v1".into(),
                estimated_input_tokens: 10,
                max_output_tokens: 10,
            },
        )
        .await
        .unwrap();

    // Operator revokes consent_revoked BEFORE crash/recovery happens.
    let revoked_before = store
        .revoke_cost_consent(
            &consent_revoked.id,
            RevokeCostConsent {
                schema_version: "1".into(),
            },
        )
        .await
        .unwrap();
    let orig_revoked_at = revoked_before.revoked_at.clone().unwrap();

    // Expired lease MUST reject reconciliation even by same owner_id!
    sqlx::query("UPDATE worker_leases SET expires_at='2020-01-01T00:00:00Z' WHERE id=1")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        store
            .reconcile_budget_reservation(
                None,
                ReconcileBudgetRequest {
                    reservation_id: res_active.id.clone(),
                    owner_id: worker_a.clone(),
                    lease_epoch: lease_a.lease_epoch,
                    inference_provenance: Some(InferenceProvenance::DeterministicMockFixture),
                    usage: ProviderUsageReport::Verified {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                },
            )
            .await
            .unwrap_err()
            .code(),
        "reservation_ownership_required"
    );

    // Worker B acquires new active lease epoch 2 and runs orphan recovery.
    let worker_b = uuid::Uuid::new_v4().to_string();
    let lease_b = store.acquire_worker_lease(&worker_b, 60).await.unwrap();
    assert_eq!(lease_b.lease_epoch, 2);

    // Deposed Worker A cannot reserve OR reconcile!
    assert_eq!(
        store
            .reconcile_budget_reservation(
                None,
                ReconcileBudgetRequest {
                    reservation_id: res_active.id.clone(),
                    owner_id: worker_a.clone(),
                    lease_epoch: lease_a.lease_epoch,
                    inference_provenance: Some(InferenceProvenance::DeterministicMockFixture),
                    usage: ProviderUsageReport::Verified {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                },
            )
            .await
            .unwrap_err()
            .code(),
        "reservation_ownership_required"
    );

    let recovered = store
        .recover_orphaned_reservations(&worker_b, lease_b.lease_epoch)
        .await
        .unwrap();
    assert_eq!(recovered.len(), 2);
    assert!(
        recovered
            .iter()
            .all(|r| r.status == ReservationStatus::SettledConservative)
    );
    let mut recovered_ids: Vec<&str> = recovered.iter().map(|r| r.id.as_str()).collect();
    let mut expected_ids = vec![res_active.id.as_str(), res_for_revoked.id.as_str()];
    recovered_ids.sort_unstable();
    expected_ids.sort_unstable();
    assert_eq!(recovered_ids, expected_ids);

    // Active consent MUST be halted after conservative orphan recovery!
    let after_active = store.get_cost_consent(&consent_active.id).await.unwrap();
    assert_eq!(after_active.status, CostConsentStatus::Halted);
    assert_eq!(
        after_active.halt_reason.as_deref(),
        Some("cost_reconciliation_failed")
    );
    assert_eq!(after_active.spent_micros, 220);
    assert_eq!(after_active.reserved_micros, 0);

    // Previously revoked consent MUST preserve status=Revoked and exact revoked_at evidence!
    let after_revoked = store.get_cost_consent(&consent_revoked.id).await.unwrap();
    assert_eq!(after_revoked.status, CostConsentStatus::Revoked);
    assert_eq!(
        after_revoked.revoked_at.as_deref(),
        Some(orig_revoked_at.as_str())
    );
    assert_eq!(
        after_revoked.halt_reason.as_deref(),
        Some("cost_reconciliation_failed")
    );
    assert_eq!(after_revoked.spent_micros, 220);

    // DB trigger blocks clearing or altering revoked status/timestamp.
    assert!(
        sqlx::query("UPDATE cost_consents SET status='halted' WHERE id=?")
            .bind(&consent_revoked.id)
            .execute(&pool)
            .await
            .is_err()
    );

    pool.close().await;
}

#[tokio::test]
async fn http_and_cli_cost_consent_and_pricing_surfaces() {
    let temp = tempfile::tempdir().unwrap();
    let db_url = temp_db_url(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());
    seed_remote_model(&store, "remote-ai", "gen-v1").await;

    let app = api::router(pool.clone());
    let put_pricing = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/models/remote-ai/gen-v1/pricing")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "schema_version": "1",
                        "provider_id": "remote-ai",
                        "model_id": "gen-v1",
                        "pricing_version": "2026-11-v1",
                        "currency": "USD",
                        "input_cost_per_token_micros": 2,
                        "output_cost_per_token_micros": 10,
                        "request_fixed_cost_micros": 100,
                        "verified": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put_pricing.status(), StatusCode::OK);

    let idem_key = "11111111-1111-4111-8111-111111111121";
    let create_req = json!({
        "schema_version": "1",
        "project_id": null,
        "provider_id": "remote-ai",
        "model_id": "gen-v1",
        "scopes": ["scene_code_generation"],
        "max_cost_micros": 5000,
        "expires_at": "2099-01-01T00:00:00Z"
    });
    let res_create = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/cost-consents")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", idem_key)
                .body(Body::from(serde_json::to_vec(&create_req).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_create.status(), StatusCode::CREATED);
    let created: CostConsent = serde_json::from_slice(
        &axum::body::to_bytes(res_create.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();

    let res_revoke = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/cost-consents/{}/revoke", created.id))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(br#"{"schema_version":"1"}"#.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_revoke.status(), StatusCode::OK);
    let revoked: CostConsent = serde_json::from_slice(
        &axum::body::to_bytes(res_revoke.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(revoked.status, CostConsentStatus::Revoked);

    let consent_file = temp.path().join("consent.json");
    fs::write(&consent_file, serde_json::to_vec(&create_req).unwrap()).unwrap();
    let cli_create = Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args(["cost", "consent-create", "--file"])
        .arg(&consent_file)
        .env("SCENIVOX_DATABASE_URL", &db_url)
        .output()
        .unwrap();
    assert!(cli_create.status.success());

    pool.close().await;
}

#[tokio::test]
async fn p203c_final_ledger_qa_bindings_individual_token_limits_and_provenance() {
    let temp = tempfile::tempdir().unwrap();
    let db_url = temp_db_url(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    seed_remote_model(&store, "remote-ai", "gen-v1").await;
    // Note: input_cost=100, output_cost=1 so input overage can stay under total micros, testing individual limits!
    store
        .set_model_pricing(SetModelPricing {
            schema_version: "1".into(),
            provider_id: "remote-ai".into(),
            model_id: "gen-v1".into(),
            pricing_version: "2026-11-indiv".into(),
            currency: "USD".into(),
            input_cost_per_token_micros: 100,
            output_cost_per_token_micros: 1,
            request_fixed_cost_micros: 0,
            verified: true,
        })
        .await
        .unwrap();

    let (consent, _) = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: "1".into(),
                project_id: None,
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 50_000,
                max_total_tokens: Some(500),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();

    let owner = uuid::Uuid::new_v4().to_string();
    let lease = store.acquire_worker_lease(&owner, 120).await.unwrap();
    let project_id = uuid::Uuid::new_v4().to_string();
    let job_id = uuid::Uuid::new_v4().to_string();
    let req_hash = "a".repeat(64);
    let snap_hash = "b".repeat(64);
    let endpoint = "https://remote-ai.invalid/v1";

    // 1. Bindings enforced on claim_reservation_dispatch (reject request/project/job/snapshot/endpoint substitution).
    let (res_sub, _) = store
        .reserve_budget(
            None,
            ReserveBudgetRequest {
                cost_consent_id: consent.id.clone(),
                project_id: Some(project_id.clone()),
                job_id: Some(job_id.clone()),
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scope: CostScope::SceneCodeGeneration,
                owner_id: owner.clone(),
                lease_epoch: lease.lease_epoch,
                canonical_request_sha256: req_hash.clone(),
                model_snapshot_sha256: snap_hash.clone(),
                endpoint_identity: endpoint.into(),
                estimated_input_tokens: 10,
                max_output_tokens: 100,
            },
        )
        .await
        .unwrap();

    let base_binding = ClaimDispatchBinding {
        project_id: Some(project_id.clone()),
        job_id: Some(job_id.clone()),
        provider_id: "remote-ai".into(),
        model_id: "gen-v1".into(),
        scope: CostScope::SceneCodeGeneration,
        canonical_request_sha256: req_hash.clone(),
        model_snapshot_sha256: snap_hash.clone(),
        endpoint_identity: endpoint.into(),
        max_output_tokens: 100,
    };

    let mut bad_req = base_binding.clone();
    bad_req.canonical_request_sha256 = "f".repeat(64);
    assert_eq!(
        store
            .claim_reservation_dispatch(&res_sub.id, &owner, lease.lease_epoch, &bad_req)
            .await
            .unwrap_err()
            .code(),
        "request_substitution_rejected"
    );

    let mut bad_proj = base_binding.clone();
    bad_proj.project_id = Some(uuid::Uuid::new_v4().to_string());
    assert_eq!(
        store
            .claim_reservation_dispatch(&res_sub.id, &owner, lease.lease_epoch, &bad_proj)
            .await
            .unwrap_err()
            .code(),
        "request_substitution_rejected"
    );

    let mut bad_snap = base_binding.clone();
    bad_snap.model_snapshot_sha256 = "e".repeat(64);
    assert_eq!(
        store
            .claim_reservation_dispatch(&res_sub.id, &owner, lease.lease_epoch, &bad_snap)
            .await
            .unwrap_err()
            .code(),
        "request_substitution_rejected"
    );

    let mut bad_ep = base_binding.clone();
    bad_ep.endpoint_identity = "https://other.invalid/v1".into();
    assert_eq!(
        store
            .claim_reservation_dispatch(&res_sub.id, &owner, lease.lease_epoch, &bad_ep)
            .await
            .unwrap_err()
            .code(),
        "request_substitution_rejected"
    );

    // Exact match succeeds!
    store
        .claim_reservation_dispatch(&res_sub.id, &owner, lease.lease_epoch, &base_binding)
        .await
        .unwrap();

    // 2. Output token individual limit violation (in=1 <= 10, out=120 > 100), EVEN THOUGH
    // actual_cost (1*100 + 120*1 = 220) <= reserved_micros (10*100 + 100*1 = 1100)
    // and total_tokens (121) <= max_total_tokens (500)!
    // Verified reconciliation without provenance is rejected first.
    assert_eq!(
        store
            .reconcile_budget_reservation(
                None,
                ReconcileBudgetRequest {
                    reservation_id: res_sub.id.clone(),
                    owner_id: owner.clone(),
                    lease_epoch: lease.lease_epoch,
                    inference_provenance: None,
                    usage: ProviderUsageReport::Verified {
                        input_tokens: 1,
                        output_tokens: 120,
                    },
                },
            )
            .await
            .unwrap_err()
            .code(),
        "cost_reconciliation_failed"
    );

    let (settled_out_over, _) = store
        .reconcile_budget_reservation(
            None,
            ReconcileBudgetRequest {
                reservation_id: res_sub.id.clone(),
                owner_id: owner.clone(),
                lease_epoch: lease.lease_epoch,
                inference_provenance: Some(InferenceProvenance::DeterministicMockFixture),
                usage: ProviderUsageReport::Verified {
                    input_tokens: 1,
                    output_tokens: 120,
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(settled_out_over.status, ReservationStatus::SettledOverage);
    assert_eq!(settled_out_over.actual_input_tokens, Some(1));
    assert_eq!(settled_out_over.actual_output_tokens, Some(120));
    assert_eq!(settled_out_over.actual_micros, Some(220));
    assert_eq!(
        settled_out_over.inference_provenance,
        Some(InferenceProvenance::DeterministicMockFixture)
    );
    let halted = store.get_cost_consent(&consent.id).await.unwrap();
    assert_eq!(halted.status, CostConsentStatus::Halted);
    assert_eq!(halted.spent_micros, 220);
    assert_eq!(halted.spent_tokens, 121);

    // 3. Individual input limit violation on fresh consent triggers SettledOverage + preserves exact usage.
    let (consent_in, _) = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: "1".into(),
                project_id: None,
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 50_000,
                max_total_tokens: Some(500),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();
    let (res_in, _) = store
        .reserve_budget(
            None,
            ReserveBudgetRequest {
                cost_consent_id: consent_in.id.clone(),
                project_id: None,
                job_id: None,
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scope: CostScope::SceneCodeGeneration,
                owner_id: owner.clone(),
                lease_epoch: lease.lease_epoch,
                canonical_request_sha256: req_hash.clone(),
                model_snapshot_sha256: snap_hash.clone(),
                endpoint_identity: endpoint.into(),
                estimated_input_tokens: 10,
                max_output_tokens: 100,
            },
        )
        .await
        .unwrap();
    let (settled_in_over, _) = store
        .reconcile_budget_reservation(
            None,
            ReconcileBudgetRequest {
                reservation_id: res_in.id.clone(),
                owner_id: owner.clone(),
                lease_epoch: lease.lease_epoch,
                inference_provenance: Some(InferenceProvenance::ControlledLocalHttpFixture),
                usage: ProviderUsageReport::Verified {
                    input_tokens: 15,
                    output_tokens: 5,
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(settled_in_over.status, ReservationStatus::SettledOverage);
    assert_eq!(settled_in_over.actual_input_tokens, Some(15));
    assert_eq!(settled_in_over.actual_output_tokens, Some(5));
    assert_eq!(settled_in_over.actual_micros, Some(1_505));
    assert_eq!(
        store.get_cost_consent(&consent_in.id).await.unwrap().status,
        CostConsentStatus::Halted
    );

    // 4. DB triggers enforce immutability of request/snapshot/endpoint bindings and provenance.
    assert!(
        sqlx::query("UPDATE budget_reservations SET canonical_request_sha256=? WHERE id=?")
            .bind("c".repeat(64))
            .bind(&res_in.id)
            .execute(&pool)
            .await
            .is_err()
    );

    pool.close().await;
}
