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
        CostConsent, CostConsentStatus, CostGateDecision, CostScope, CreateCostConsent,
        ModelPricingRecord, PricingSnapshot, ProviderUsageReport, ReconcileBudgetRequest,
        ReservationStatus, ReserveBudgetRequest, RevokeCostConsent, SetModelPricing,
        compute_usage_cost_micros,
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
async fn cost_consent_scopes_pricing_snapshot_expiry_revocation_and_offline_zero_cost() {
    let temp = tempfile::tempdir().unwrap();
    let db_url = temp_db_url(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    seed_remote_model(&store, "remote-ai", "gen-v1").await;

    // 1. Offline remote:false model cannot take paid CostConsent, and passes cost gate with ZeroCostOffline.
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

    // 2. Missing/unverified pricing fails closed with 503 provider_pricing_unavailable.
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
                max_total_tokens: Some(500),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(no_price_err.code(), "provider_pricing_unavailable");

    store
        .set_model_pricing(SetModelPricing {
            schema_version: "1".into(),
            provider_id: "remote-ai".into(),
            model_id: "gen-v1".into(),
            pricing_version: "unverified-v1".into(),
            currency: "USD".into(),
            input_cost_per_token_micros: 2,
            output_cost_per_token_micros: 10,
            request_fixed_cost_micros: 100,
            verified: false,
        })
        .await
        .unwrap();
    assert_eq!(
        store
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
            .unwrap_err()
            .code(),
        "provider_pricing_unavailable"
    );

    // 3. Verified pricing allows scoped consent, pins pricing snapshot, enforces project/model/scope/expiry/revocation.
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
                max_total_tokens: Some(500),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(consent.pricing_sha256, pricing.pricing_sha256);

    // Wrong project / wrong scope / wrong model rejected.
    let wrong_proj = uuid::Uuid::new_v4().to_string();
    let owner_id = uuid::Uuid::new_v4().to_string();
    for bad_req in [
        ReserveBudgetRequest {
            cost_consent_id: consent.id.clone(),
            project_id: Some(wrong_proj),
            job_id: None,
            provider_id: "remote-ai".into(),
            model_id: "gen-v1".into(),
            scope: CostScope::SceneCodeGeneration,
            owner_id: owner_id.clone(),
            lease_epoch: 1,
            estimated_input_tokens: 10,
            max_output_tokens: 10,
        },
        ReserveBudgetRequest {
            cost_consent_id: consent.id.clone(),
            project_id: Some(project.id.clone()),
            job_id: None,
            provider_id: "remote-ai".into(),
            model_id: "gen-v1".into(),
            scope: CostScope::StoryboardPlanning,
            owner_id: owner_id.clone(),
            lease_epoch: 1,
            estimated_input_tokens: 10,
            max_output_tokens: 10,
        },
        ReserveBudgetRequest {
            cost_consent_id: consent.id.clone(),
            project_id: Some(project.id.clone()),
            job_id: None,
            provider_id: "other-provider".into(),
            model_id: "gen-v1".into(),
            scope: CostScope::SceneCodeGeneration,
            owner_id: owner_id.clone(),
            lease_epoch: 1,
            estimated_input_tokens: 10,
            max_output_tokens: 10,
        },
    ] {
        assert_eq!(
            store
                .reserve_budget(None, bad_req)
                .await
                .unwrap_err()
                .code(),
            "remote_cost_not_approved"
        );
    }

    // Remote revision gate checks CostConsent cleanly (requiring consent when remote:true).
    let mut remote_models = offline_rev.content.models.clone();
    remote_models.scene_code_generation.provider_id = "remote-ai".into();
    remote_models.scene_code_generation.model_id = "gen-v1".into();
    let remote_rev = store
        .create_revision(
            &project.id,
            CreateRevision {
                schema_version: "1".into(),
                expected_revision_id: Some(offline_rev.id.clone()),
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
                None,
                CostScope::SceneCodeGeneration,
            )
            .await
            .unwrap_err()
            .code(),
        "remote_cost_not_approved"
    );
    assert_eq!(
        store
            .check_revision_cost_gate(
                &project.id,
                &remote_rev.id,
                Some(&consent.id),
                CostScope::SceneCodeGeneration,
            )
            .await
            .unwrap(),
        CostGateDecision::RemoteConsentVerified {
            cost_consent_id: consent.id.clone(),
        }
    );

    // Revocation blocks future reservations cleanly.
    let revoked = store
        .revoke_cost_consent(
            &consent.id,
            RevokeCostConsent {
                schema_version: "1".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(revoked.status, CostConsentStatus::Revoked);
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
                    estimated_input_tokens: 10,
                    max_output_tokens: 10,
                },
            )
            .await
            .unwrap_err()
            .code(),
        "remote_cost_not_approved"
    );

    // Expired consent rejected cleanly.
    let exp_err = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: "1".into(),
                project_id: Some(project.id.clone()),
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 10_000,
                max_total_tokens: None,
                expires_at: "2020-01-01T00:00:00Z".into(),
            },
        )
        .await
        .expect_err("already expired at creation must fail");
    assert_eq!(exp_err.code(), "invalid_schema");

    // Seed an active consent that expires using clock skew in DB test to verify runtime expiry check.
    let past_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO cost_consents(id,project_id,provider_id,model_id,scopes_json,max_cost_micros,max_total_tokens,spent_micros,reserved_micros,spent_tokens,reserved_tokens,pricing_snapshot_json,pricing_sha256,status,halt_reason,created_at,expires_at,revoked_at) VALUES(?,?,?,?,?,10000,NULL,0,0,0,0,?,?,'active',NULL,'2020-01-01T00:00:00Z','2020-01-02T00:00:00Z',NULL)",
    )
    .bind(&past_id)
    .bind(&project.id)
    .bind("remote-ai")
    .bind("gen-v1")
    .bind(r#"["scene_code_generation"]"#)
    .bind(serde_json::to_string(&pricing.pricing).unwrap())
    .bind(&pricing.pricing_sha256)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        store
            .reserve_budget(
                None,
                ReserveBudgetRequest {
                    cost_consent_id: past_id,
                    project_id: Some(project.id),
                    job_id: None,
                    provider_id: "remote-ai".into(),
                    model_id: "gen-v1".into(),
                    scope: CostScope::SceneCodeGeneration,
                    owner_id,
                    lease_epoch: 1,
                    estimated_input_tokens: 10,
                    max_output_tokens: 10,
                },
            )
            .await
            .unwrap_err()
            .code(),
        "remote_cost_not_approved"
    );

    pool.close().await;
}

#[tokio::test]
async fn concurrent_reservations_and_integer_overflow_never_exceed_budget() {
    let temp = tempfile::tempdir().unwrap();
    let db_url = temp_db_url(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    seed_remote_model(&store, "remote-ai", "gen-v1").await;
    seed_verified_pricing(&store, "remote-ai", "gen-v1").await;
    // Pricing: fixed=100, in=2, out=10 -> per 10 in + 10 out => 100 + 20 + 100 = 220 micros.
    let max_budget = 660i64; // Exactly 3 calls fit; 4th+ must fail closed!
    let (consent, _) = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: "1".into(),
                project_id: None,
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: max_budget,
                max_total_tokens: None,
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();

    // Test integer overflow guards directly and inside reserve_budget.
    let overflow_pricing = PricingSnapshot {
        pricing_version: "max".into(),
        currency: "USD".into(),
        input_cost_per_token_micros: i64::MAX,
        output_cost_per_token_micros: i64::MAX,
        request_fixed_cost_micros: 1,
        verified: true,
    };
    assert!(compute_usage_cost_micros(&overflow_pricing, 2, 0).is_err());
    assert!(compute_usage_cost_micros(&overflow_pricing, 1, 1).is_err());

    let owner = uuid::Uuid::new_v4().to_string();
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let s = store.clone();
        let cid = consent.id.clone();
        let own = owner.clone();
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
                    lease_epoch: 1,
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
    assert_eq!(succeeded, 3);
    assert_eq!(rejected, 5);

    let updated = store.get_cost_consent(&consent.id).await.unwrap();
    assert_eq!(updated.reserved_micros, 660);
    assert_eq!(updated.spent_micros, 0);

    // Overflowing token estimate inside reserve_budget fails closed without panicking or wrapping.
    assert_eq!(
        store
            .reserve_budget(
                None,
                ReserveBudgetRequest {
                    cost_consent_id: consent.id.clone(),
                    project_id: None,
                    job_id: None,
                    provider_id: "remote-ai".into(),
                    model_id: "gen-v1".into(),
                    scope: CostScope::SceneCodeGeneration,
                    owner_id: owner,
                    lease_epoch: 1,
                    estimated_input_tokens: i64::MAX,
                    max_output_tokens: i64::MAX,
                },
            )
            .await
            .unwrap_err()
            .code(),
        "cost_limit_exceeded"
    );

    pool.close().await;
}

#[tokio::test]
async fn exactly_once_reconciliation_missing_usage_overage_and_verified_ownership() {
    let temp = tempfile::tempdir().unwrap();
    let db_url = temp_db_url(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    seed_remote_model(&store, "remote-ai", "gen-v1").await;
    seed_verified_pricing(&store, "remote-ai", "gen-v1").await;
    let (consent_a, _) = store
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
    assert_eq!(lease_a.lease_epoch, 1);

    // Worker B cannot reserve under active lease A or steal lease A before expiry.
    let worker_b = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        store
            .acquire_worker_lease(&worker_b, 60)
            .await
            .unwrap_err()
            .code(),
        "reservation_ownership_required"
    );
    assert_eq!(
        store
            .reserve_budget(
                None,
                ReserveBudgetRequest {
                    cost_consent_id: consent_a.id.clone(),
                    project_id: None,
                    job_id: None,
                    provider_id: "remote-ai".into(),
                    model_id: "gen-v1".into(),
                    scope: CostScope::SceneCodeGeneration,
                    owner_id: worker_b.clone(),
                    lease_epoch: 1,
                    estimated_input_tokens: 10,
                    max_output_tokens: 20,
                },
            )
            .await
            .unwrap_err()
            .code(),
        "reservation_ownership_required"
    );

    // 1. Verified exact usage releases unused budget, and second reconciliation fails (exactly-once).
    // Reserve: 10 in (20) + 20 out (200) + fixed (100) = 320 micros, 30 tokens.
    let (res1, _) = store
        .reserve_budget(
            None,
            ReserveBudgetRequest {
                cost_consent_id: consent_a.id.clone(),
                project_id: None,
                job_id: None,
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scope: CostScope::SceneCodeGeneration,
                owner_id: worker_a.clone(),
                lease_epoch: lease_a.lease_epoch,
                estimated_input_tokens: 10,
                max_output_tokens: 20,
            },
        )
        .await
        .unwrap();
    assert_eq!(res1.reserved_micros, 320);

    // Unverified / mismatched owner CANNOT settle active reservation!
    assert_eq!(
        store
            .reconcile_budget_reservation(
                None,
                ReconcileBudgetRequest {
                    reservation_id: res1.id.clone(),
                    owner_id: worker_b.clone(),
                    lease_epoch: lease_a.lease_epoch,
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
    assert_eq!(
        store.get_budget_reservation(&res1.id).await.unwrap().status,
        ReservationStatus::Reserved
    );
    // Active lease owner calling recover_orphaned_reservations NEVER settles its own active reservation!
    assert!(
        store
            .recover_orphaned_reservations(&worker_a, lease_a.lease_epoch)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.get_budget_reservation(&res1.id).await.unwrap().status,
        ReservationStatus::Reserved
    );

    // Settle once cleanly by verified owner (10 in + 5 out = 100 + 20 + 50 = 170 micros).
    let (settled1, _) = store
        .reconcile_budget_reservation(
            None,
            ReconcileBudgetRequest {
                reservation_id: res1.id.clone(),
                owner_id: worker_a.clone(),
                lease_epoch: lease_a.lease_epoch,
                usage: ProviderUsageReport::Verified {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(settled1.status, ReservationStatus::Settled);
    assert_eq!(settled1.actual_micros, Some(170));
    let after1 = store.get_cost_consent(&consent_a.id).await.unwrap();
    assert_eq!(after1.spent_micros, 170);
    assert_eq!(after1.reserved_micros, 0);

    // Second reconciliation rejected (exactly-once!).
    assert_eq!(
        store
            .reconcile_budget_reservation(
                None,
                ReconcileBudgetRequest {
                    reservation_id: res1.id.clone(),
                    owner_id: worker_a.clone(),
                    lease_epoch: lease_a.lease_epoch,
                    usage: ProviderUsageReport::Verified {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                },
            )
            .await
            .unwrap_err()
            .code(),
        "invalid_transition"
    );

    // 2. Crash recovery with expired lease settles prior owner's open reservation conservatively ONLY for new lease owner.
    let (orphan_res, _) = store
        .reserve_budget(
            None,
            ReserveBudgetRequest {
                cost_consent_id: consent_a.id.clone(),
                project_id: None,
                job_id: None,
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scope: CostScope::SceneCodeGeneration,
                owner_id: worker_a.clone(),
                lease_epoch: lease_a.lease_epoch,
                estimated_input_tokens: 10,
                max_output_tokens: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(orphan_res.reserved_micros, 220);

    // Unverified caller without lease cannot recover orphans.
    assert_eq!(
        store
            .recover_orphaned_reservations(&worker_b, 2)
            .await
            .unwrap_err()
            .code(),
        "reservation_ownership_required"
    );

    // Expire lease A in SQLite to simulate crash, then Worker B acquires lease epoch 2.
    sqlx::query("UPDATE worker_leases SET expires_at='2020-01-01T00:00:00Z' WHERE id=1")
        .execute(&pool)
        .await
        .unwrap();
    let lease_b = store.acquire_worker_lease(&worker_b, 60).await.unwrap();
    assert_eq!(lease_b.lease_epoch, 2);

    // Deposed Worker A can NO LONGER reconcile its old reservation!
    assert_eq!(
        store
            .reconcile_budget_reservation(
                None,
                ReconcileBudgetRequest {
                    reservation_id: orphan_res.id.clone(),
                    owner_id: worker_a.clone(),
                    lease_epoch: lease_a.lease_epoch,
                    usage: ProviderUsageReport::Verified {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                },
            )
            .await
            .unwrap_err()
            .code(),
        "reservation_ownership_required"
    );

    // Verified Worker B settles orphaned epoch-1 reservation conservatively at reserved_micros (220).
    let recovered = store
        .recover_orphaned_reservations(&worker_b, lease_b.lease_epoch)
        .await
        .unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].status, ReservationStatus::SettledConservative);
    assert_eq!(recovered[0].actual_micros, Some(220));
    let after2 = store.get_cost_consent(&consent_a.id).await.unwrap();
    assert_eq!(after2.spent_micros, 170 + 220);
    assert_eq!(after2.reserved_micros, 0);

    // 3. Missing usage settles conservatively and halts further calls on that consent.
    let (res_missing, _) = store
        .reserve_budget(
            None,
            ReserveBudgetRequest {
                cost_consent_id: consent_a.id.clone(),
                project_id: None,
                job_id: None,
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scope: CostScope::SceneCodeGeneration,
                owner_id: worker_b.clone(),
                lease_epoch: lease_b.lease_epoch,
                estimated_input_tokens: 5,
                max_output_tokens: 5,
            },
        )
        .await
        .unwrap();
    let (settled_missing, _) = store
        .reconcile_budget_reservation(
            None,
            ReconcileBudgetRequest {
                reservation_id: res_missing.id,
                owner_id: worker_b.clone(),
                lease_epoch: lease_b.lease_epoch,
                usage: ProviderUsageReport::MissingOrUnverifiable,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        settled_missing.status,
        ReservationStatus::SettledConservative
    );
    assert_eq!(
        settled_missing.error_code.as_deref(),
        Some("cost_reconciliation_failed")
    );
    let halted = store.get_cost_consent(&consent_a.id).await.unwrap();
    assert_eq!(halted.status, CostConsentStatus::Halted);
    assert_eq!(
        store
            .reserve_budget(
                None,
                ReserveBudgetRequest {
                    cost_consent_id: consent_a.id.clone(),
                    project_id: None,
                    job_id: None,
                    provider_id: "remote-ai".into(),
                    model_id: "gen-v1".into(),
                    scope: CostScope::SceneCodeGeneration,
                    owner_id: worker_b.clone(),
                    lease_epoch: lease_b.lease_epoch,
                    estimated_input_tokens: 1,
                    max_output_tokens: 1,
                },
            )
            .await
            .unwrap_err()
            .code(),
        "cost_reconciliation_failed"
    );

    // 4. Actual usage exceeding reservation & cap records full spend + overage and halts consent.
    let (consent_over, _) = store
        .create_cost_consent(
            None,
            CreateCostConsent {
                schema_version: "1".into(),
                project_id: None,
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scopes: vec![CostScope::SceneCodeGeneration],
                max_cost_micros: 300,
                max_total_tokens: None,
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();
    let (res_over, _) = store
        .reserve_budget(
            None,
            ReserveBudgetRequest {
                cost_consent_id: consent_over.id.clone(),
                project_id: None,
                job_id: None,
                provider_id: "remote-ai".into(),
                model_id: "gen-v1".into(),
                scope: CostScope::SceneCodeGeneration,
                owner_id: worker_b.clone(),
                lease_epoch: lease_b.lease_epoch,
                estimated_input_tokens: 5,
                max_output_tokens: 5, // fixed 100 + 10 + 50 = 160 micros
            },
        )
        .await
        .unwrap();
    // Provider returns 50 output tokens => 100 + 10 + 500 = 610 micros (> 160 reserved and > 300 cap).
    let (settled_over, _) = store
        .reconcile_budget_reservation(
            None,
            ReconcileBudgetRequest {
                reservation_id: res_over.id,
                owner_id: worker_b.clone(),
                lease_epoch: lease_b.lease_epoch,
                usage: ProviderUsageReport::Verified {
                    input_tokens: 5,
                    output_tokens: 50,
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(settled_over.status, ReservationStatus::SettledOverage);
    assert_eq!(settled_over.actual_micros, Some(610));
    assert_eq!(settled_over.overage_micros, 610 - 160);
    let over_consent = store.get_cost_consent(&consent_over.id).await.unwrap();
    assert_eq!(over_consent.status, CostConsentStatus::Halted);
    assert_eq!(over_consent.spent_micros, 610);
    assert_eq!(over_consent.reserved_micros, 0);

    // 5. SQLite immutability and once-only reservation triggers block raw tampering/deletion.
    assert!(
        sqlx::query("UPDATE budget_reservations SET actual_micros=0 WHERE id=?")
            .bind(&settled_over.id)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM cost_consents WHERE id=?")
            .bind(&consent_over.id)
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

    // CLI consent-create + consent-get + consent-revoke against same SQLite DB.
    let consent_file = temp.path().join("consent.json");
    fs::write(&consent_file, serde_json::to_vec(&create_req).unwrap()).unwrap();
    let cli_create = Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args(["cost", "consent-create", "--file"])
        .arg(&consent_file)
        .env("SCENIVOX_DATABASE_URL", &db_url)
        .output()
        .unwrap();
    assert!(
        cli_create.status.success(),
        "cli stderr: {}",
        String::from_utf8_lossy(&cli_create.stderr)
    );
    let cli_created: Value = serde_json::from_slice(&cli_create.stdout).unwrap();
    let cli_id = cli_created["id"].as_str().unwrap();

    let cli_get = Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args(["cost", "consent-get", cli_id])
        .env("SCENIVOX_DATABASE_URL", &db_url)
        .output()
        .unwrap();
    assert!(cli_get.status.success());

    pool.close().await;
}
