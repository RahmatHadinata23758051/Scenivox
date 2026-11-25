use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use scenivox::{
    api,
    asset::{AssetKind, RegisterAsset},
    db,
    domain::{
        CreateProject, CreateRenderJob, CreateRevision, ModelSelection, PlanStoryboard,
        ReviewCommand, Revision, RevisionInput, SceneInput, StoryboardInput, Target,
    },
    execution_auth::{
        ClaimExecutionAuthorization, ExecutionAuthStatus, ExecutionAuthorization,
        ExecutionWorkspaceEvidence, IssueExecutionAuthorization, RecordWorkspaceEvidence,
    },
    sandbox::SandboxPolicy,
    skill::{AuditStatus, SkillRecord},
    store::Store,
    voice::AudioProvenance,
};
use serde_json::json;
use tower::ServiceExt;

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

fn temp_db_and_stores(temp: &Path) -> (String, PathBuf, PathBuf) {
    let idx = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
    let url = format!(
        "sqlite://{}?mode=rwc",
        temp.join(format!("exec-auth-{idx}.db"))
            .to_string_lossy()
            .replace('\\', "/")
    );
    let skill_dir = temp.join(format!("skills-{idx}"));
    let asset_dir = temp.join(format!("assets-{idx}"));
    (url, skill_dir, asset_dir)
}

fn pcm16_wav_bytes(sample_rate_hz: u32, duration_secs: u32, tone_byte: u8) -> Vec<u8> {
    let channels: u16 = 1;
    let bits_per_sample: u16 = 16;
    let block_align = channels * (bits_per_sample / 8);
    let byte_rate = sample_rate_hz * u32::from(block_align);
    let frame_count = sample_rate_hz * duration_secs;
    let data_len = frame_count * u32::from(block_align);
    let riff_size = 36 + data_len;

    let mut out = Vec::with_capacity((8 + riff_size) as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate_hz.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.resize(out.len() + data_len as usize, tone_byte);
    out
}

fn write_benign_skill(root: &Path, id: &str, version: &str) -> PathBuf {
    let dir = root.join(format!("skill-{id}-{version}"));
    fs::create_dir_all(dir.join("scripts")).unwrap();
    let manifest = json!({
        "schema_version": "1",
        "id": id,
        "version": version,
        "kind": "unified_composition",
        "composition_mode": "unified_timeline",
        "rendering_strategy": "canvas_playwright_ffmpeg",
        "supported_aspect_ratios": ["9:16"],
        "max_duration_ms": null,
        "supports_independent_scene_render": false,
        "code_generation_policy": {
            "languages": ["javascript"],
            "writable_paths": ["web/shots.js"],
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
        "requires": ["node", "ffmpeg"],
        "accepted_inputs": ["revision.json"],
        "produced_outputs": ["final.mp4"]
    });
    fs::write(
        dir.join("skill.manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(dir.join("SKILL.md"), "# Benign test skill\n").unwrap();
    fs::write(dir.join("scripts/render.js"), "// fixture\n").unwrap();
    dir
}

async fn setup_approved_project_with_assets(
    store: &Store,
    skill_id: &str,
    skill_version: &str,
    asset_hashes: Vec<String>,
) -> (scenivox::domain::Project, Revision, String) {
    let target = Target {
        duration_ms: 5_000,
        aspect_ratio: "9:16".into(),
        fps: 30,
        language: "id-ID".into(),
        style_id: skill_id.into(),
    };
    let project = store
        .create_project(CreateProject {
            schema_version: "1".into(),
            title: "Exec Auth Project".into(),
            idea: "Tes eksekusi".into(),
            target: target.clone(),
            model_selection: ModelSelection::default(),
        })
        .await
        .unwrap();
    let planned = store
        .plan_storyboard(
            &project.id,
            None,
            PlanStoryboard {
                schema_version: "1".into(),
                idea: "Tes eksekusi".into(),
                target: target.clone(),
                expected_revision_id: None,
                model_selection: ModelSelection::default(),
            },
            &scenivox::planner::DeterministicMockPlanner,
        )
        .await
        .unwrap();
    let rev = store
        .create_revision(
            &project.id,
            CreateRevision {
                schema_version: "1".into(),
                expected_revision_id: Some(planned.id),
                content: RevisionInput {
                    schema_version: "1".into(),
                    idea: "Tes eksekusi".into(),
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
                    models: planned.content.models,
                    asset_hashes,
                    skill_id: skill_id.into(),
                    skill_version: skill_version.into(),
                    planning_provenance: None,
                },
            },
        )
        .await
        .unwrap();
    store
        .review_revision(
            &project.id,
            &rev.id,
            None,
            ReviewCommand {
                schema_version: "1".into(),
                content_sha256: rev.content_sha256.clone(),
            },
        )
        .await
        .unwrap();
    let (approval, _, _) = store
        .approve_revision(
            &project.id,
            &rev.id,
            None,
            ReviewCommand {
                schema_version: "1".into(),
                content_sha256: rev.content_sha256.clone(),
            },
        )
        .await
        .unwrap();
    (
        store.get_project(&project.id).await.unwrap(),
        rev,
        approval.id,
    )
}

async fn setup_smoke_verified_skill(
    store: &Store,
    skill_dir: &Path,
    src_root: &Path,
    skill_id: &str,
    skill_version: &str,
) -> SkillRecord {
    let src = write_benign_skill(src_root, skill_id, skill_version);
    let (rec, _) = store.import_skill(skill_dir, None, &src).await.unwrap();
    store
        .record_host_skill_audit(
            &rec.skill_id,
            &rec.skill_version,
            &rec.bundle_sha256,
            AuditStatus::SmokeVerified,
            Some(r#"{"auditor":"host","smoke":"passed"}"#),
        )
        .await
        .unwrap()
}

async fn seed_matching_render_job(
    pool: &sqlx::SqlitePool,
    project: &scenivox::domain::Project,
    rev: &Revision,
    approval_id: &str,
) -> String {
    let job_id = uuid::Uuid::new_v4().to_string();
    let snapshot: String =
        sqlx::query_scalar("SELECT content_json FROM revisions WHERE project_id=? AND id=?")
            .bind(&project.id)
            .bind(&rev.id)
            .fetch_one(pool)
            .await
            .unwrap();
    sqlx::query("INSERT INTO render_jobs(id,project_id,revision_id,approval_id,content_sha256,snapshot_json,status,created_at,error_code,artifact_ids_json,request_sha256) VALUES(?,?,?,?,?,?,'queued',strftime('%Y-%m-%dT%H:%M:%SZ','now'),NULL,'[]',?)")
        .bind(&job_id)
        .bind(&project.id)
        .bind(&rev.id)
        .bind(approval_id)
        .bind(&rev.content_sha256)
        .bind(snapshot)
        .bind("a".repeat(64))
        .execute(pool)
        .await
        .unwrap();
    job_id
}

fn sample_policy_sha256() -> String {
    let image = format!("ghcr.io/scenivox/runtime@sha256:{}", "a".repeat(64));
    SandboxPolicy::hardened_default(image)
        .unwrap()
        .policy_sha256()
        .unwrap()
}

#[tokio::test]
async fn execution_auth_rejects_static_audit_synthetic_audio_tampered_stores_and_enforces_job_bound_atomic_claim()
 {
    let temp = tempfile::tempdir().unwrap();
    let (db_url, skill_dir, asset_dir) = temp_db_and_stores(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    let policy_sha = sample_policy_sha256();
    let skill_src = write_benign_skill(temp.path(), "exec-skill", "0.1.0");
    let (unaudited, _) = store
        .import_skill(&skill_dir, None, &skill_src)
        .await
        .unwrap();

    // Register real UserSupplied WAV + SyntheticTestFixture WAV.
    let real_wav = temp.path().join("real.wav");
    fs::write(&real_wav, pcm16_wav_bytes(16_000, 1, 0x11)).unwrap();
    let (real_asset, _, _) = store
        .register_asset(
            &asset_dir,
            None,
            RegisterAsset {
                schema_version: "1".into(),
                source_path: real_wav.to_string_lossy().into_owned(),
                kind: AssetKind::NarrationWav,
                provenance: Some(AudioProvenance::UserSupplied),
            },
        )
        .await
        .unwrap();

    let synth_wav = temp.path().join("synth.wav");
    fs::write(&synth_wav, pcm16_wav_bytes(16_000, 1, 0x22)).unwrap();
    let (synth_asset, _, _) = store
        .register_asset(
            &asset_dir,
            None,
            RegisterAsset {
                schema_version: "1".into(),
                source_path: synth_wav.to_string_lossy().into_owned(),
                kind: AssetKind::NarrationWav,
                provenance: Some(AudioProvenance::SyntheticTestFixture),
            },
        )
        .await
        .unwrap();

    let (project, rev, approval_id) = setup_approved_project_with_assets(
        &store,
        "exec-skill",
        "0.1.0",
        vec![real_asset.sha256.clone()],
    )
    .await;

    // 1. Unaudited AND StaticAudited skills CANNOT obtain production ExecutionAuthorization!
    assert_eq!(
        store
            .issue_execution_authorization(
                &skill_dir,
                &asset_dir,
                None,
                IssueExecutionAuthorization {
                    schema_version: "1".into(),
                    project_id: project.id.clone(),
                    revision_id: rev.id.clone(),
                    content_sha256: rev.content_sha256.clone(),
                    skill_id: unaudited.skill_id.clone(),
                    skill_version: unaudited.skill_version.clone(),
                    bundle_sha256: unaudited.bundle_sha256.clone(),
                    runtime_policy_sha256: policy_sha.clone(),
                    expires_at: "2099-01-01T00:00:00Z".into(),
                },
            )
            .await
            .unwrap_err()
            .code(),
        "skill_unavailable"
    );

    store
        .record_host_skill_audit(
            &unaudited.skill_id,
            &unaudited.skill_version,
            &unaudited.bundle_sha256,
            AuditStatus::StaticAudited,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .issue_execution_authorization(
                &skill_dir,
                &asset_dir,
                None,
                IssueExecutionAuthorization {
                    schema_version: "1".into(),
                    project_id: project.id.clone(),
                    revision_id: rev.id.clone(),
                    content_sha256: rev.content_sha256.clone(),
                    skill_id: unaudited.skill_id.clone(),
                    skill_version: unaudited.skill_version.clone(),
                    bundle_sha256: unaudited.bundle_sha256.clone(),
                    runtime_policy_sha256: policy_sha.clone(),
                    expires_at: "2099-01-01T00:00:00Z".into(),
                },
            )
            .await
            .unwrap_err()
            .code(),
        "skill_unavailable"
    );

    // Promote skill to SmokeVerified.
    let verified_skill = store
        .record_host_skill_audit(
            &unaudited.skill_id,
            &unaudited.skill_version,
            &unaudited.bundle_sha256,
            AuditStatus::SmokeVerified,
            Some(r#"{"smoke":true}"#),
        )
        .await
        .unwrap();

    // 2. SyntheticTestFixture narration CANNOT obtain production ExecutionAuthorization!
    let (synth_proj, synth_rev, _) = setup_approved_project_with_assets(
        &store,
        "exec-skill",
        "0.1.0",
        vec![synth_asset.sha256.clone()],
    )
    .await;
    assert_eq!(
        store
            .issue_execution_authorization(
                &skill_dir,
                &asset_dir,
                None,
                IssueExecutionAuthorization {
                    schema_version: "1".into(),
                    project_id: synth_proj.id.clone(),
                    revision_id: synth_rev.id.clone(),
                    content_sha256: synth_rev.content_sha256.clone(),
                    skill_id: verified_skill.skill_id.clone(),
                    skill_version: verified_skill.skill_version.clone(),
                    bundle_sha256: verified_skill.bundle_sha256.clone(),
                    runtime_policy_sha256: policy_sha.clone(),
                    expires_at: "2099-01-01T00:00:00Z".into(),
                },
            )
            .await
            .unwrap_err()
            .code(),
        "invalid_asset_reference"
    );

    // 3. On-disk tamper in asset or skill fails closed BEFORE issuance!
    let stored_asset_path = asset_dir.join(&real_asset.storage_rel_path);
    let orig_asset_bytes = fs::read(&stored_asset_path).unwrap();
    let mut bad_bytes = orig_asset_bytes.clone();
    let last = bad_bytes.len() - 1;
    bad_bytes[last] ^= 0xFF;
    fs::write(&stored_asset_path, bad_bytes).unwrap();
    assert_eq!(
        store
            .issue_execution_authorization(
                &skill_dir,
                &asset_dir,
                None,
                IssueExecutionAuthorization {
                    schema_version: "1".into(),
                    project_id: project.id.clone(),
                    revision_id: rev.id.clone(),
                    content_sha256: rev.content_sha256.clone(),
                    skill_id: verified_skill.skill_id.clone(),
                    skill_version: verified_skill.skill_version.clone(),
                    bundle_sha256: verified_skill.bundle_sha256.clone(),
                    runtime_policy_sha256: policy_sha.clone(),
                    expires_at: "2099-01-01T00:00:00Z".into(),
                },
            )
            .await
            .unwrap_err()
            .code(),
        "asset_integrity_failed"
    );
    fs::write(&stored_asset_path, orig_asset_bytes).unwrap();

    // Production enqueue STILL fails closed!
    assert_eq!(
        store
            .enqueue_render_job(
                &project.id,
                None,
                CreateRenderJob {
                    schema_version: "1".into(),
                    revision_id: rev.id.clone(),
                    content_sha256: rev.content_sha256.clone(),
                    cost_consent_id: None,
                },
            )
            .await
            .unwrap_err()
            .code(),
        "skill_unavailable"
    );

    let (auth, _) = store
        .issue_execution_authorization(
            &skill_dir,
            &asset_dir,
            None,
            IssueExecutionAuthorization {
                schema_version: "1".into(),
                project_id: project.id.clone(),
                revision_id: rev.id.clone(),
                content_sha256: rev.content_sha256.clone(),
                skill_id: verified_skill.skill_id.clone(),
                skill_version: verified_skill.skill_version.clone(),
                bundle_sha256: verified_skill.bundle_sha256.clone(),
                runtime_policy_sha256: policy_sha.clone(),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();

    // 4. Claiming against a fabricated/non-existent job_id MUST fail closed (DB trigger + Store gate)!
    let fake_job_id = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        store
            .claim_execution_authorization(
                &skill_dir,
                &asset_dir,
                &auth.id,
                ClaimExecutionAuthorization {
                    schema_version: "1".into(),
                    job_id: fake_job_id.clone(),
                    project_id: project.id.clone(),
                    revision_id: rev.id.clone(),
                    content_sha256: rev.content_sha256.clone(),
                    skill_id: verified_skill.skill_id.clone(),
                    skill_version: verified_skill.skill_version.clone(),
                    bundle_sha256: verified_skill.bundle_sha256.clone(),
                    runtime_policy_sha256: policy_sha.clone(),
                },
            )
            .await
            .unwrap_err()
            .code(),
        "execution_not_authorized"
    );
    assert!(
        sqlx::query("UPDATE execution_authorizations SET status='claimed',claimed_job_id=?,claimed_at='2026-01-01T00:00:00Z' WHERE id=?")
            .bind(&fake_job_id)
            .bind(&auth.id)
            .execute(&pool)
            .await
            .is_err()
    );

    // 5. Concurrent claim race across matching real render_jobs rows -> ONLY 1 wins!
    let mut job_ids = Vec::new();
    for _ in 0..4 {
        job_ids.push(seed_matching_render_job(&pool, &project, &rev, &approval_id).await);
    }
    let mut claims = Vec::new();
    for jid in job_ids {
        let s = store.clone();
        let sd = skill_dir.clone();
        let ad = asset_dir.clone();
        let aid = auth.id.clone();
        let pid = project.id.clone();
        let rid = rev.id.clone();
        let csha = rev.content_sha256.clone();
        let sid = verified_skill.skill_id.clone();
        let sver = verified_skill.skill_version.clone();
        let bsha = verified_skill.bundle_sha256.clone();
        let psha = policy_sha.clone();
        claims.push(tokio::spawn(async move {
            let res = s
                .claim_execution_authorization(
                    &sd,
                    &ad,
                    &aid,
                    ClaimExecutionAuthorization {
                        schema_version: "1".into(),
                        job_id: jid.clone(),
                        project_id: pid,
                        revision_id: rid,
                        content_sha256: csha,
                        skill_id: sid,
                        skill_version: sver,
                        bundle_sha256: bsha,
                        runtime_policy_sha256: psha,
                    },
                )
                .await;
            (jid, res)
        }));
    }

    let mut winners = Vec::new();
    let mut losers = 0usize;
    for handle in claims {
        let (jid, outcome) = handle.await.unwrap();
        match outcome {
            Ok(claimed) => winners.push((jid, claimed)),
            Err(err) => {
                assert_eq!(err.code(), "execution_not_authorized");
                losers += 1;
            }
        }
    }
    assert_eq!(winners.len(), 1);
    assert_eq!(losers, 3);
    let (winning_job_id, _) = winners.pop().unwrap();

    // 6. Second auth CANNOT reuse winning_job_id.
    let (auth_2, _) = store
        .issue_execution_authorization(
            &skill_dir,
            &asset_dir,
            None,
            IssueExecutionAuthorization {
                schema_version: "1".into(),
                project_id: project.id.clone(),
                revision_id: rev.id.clone(),
                content_sha256: rev.content_sha256.clone(),
                skill_id: verified_skill.skill_id.clone(),
                skill_version: verified_skill.skill_version.clone(),
                bundle_sha256: verified_skill.bundle_sha256.clone(),
                runtime_policy_sha256: policy_sha.clone(),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .claim_execution_authorization(
                &skill_dir,
                &asset_dir,
                &auth_2.id,
                ClaimExecutionAuthorization {
                    schema_version: "1".into(),
                    job_id: winning_job_id,
                    project_id: project.id,
                    revision_id: rev.id,
                    content_sha256: rev.content_sha256,
                    skill_id: verified_skill.skill_id,
                    skill_version: verified_skill.skill_version,
                    bundle_sha256: verified_skill.bundle_sha256,
                    runtime_policy_sha256: policy_sha,
                },
            )
            .await
            .unwrap_err()
            .code(),
        "execution_not_authorized"
    );

    pool.close().await;
}

#[tokio::test]
async fn verified_worker_workspace_evidence_and_external_endpoints_disabled() {
    let temp = tempfile::tempdir().unwrap();
    let (db_url, skill_dir, asset_dir) = temp_db_and_stores(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    let policy_sha = sample_policy_sha256();
    let audited =
        setup_smoke_verified_skill(&store, &skill_dir, temp.path(), "http-skill", "0.1.0").await;
    let (project, rev, approval_id) =
        setup_approved_project_with_assets(&store, "http-skill", "0.1.0", vec![]).await;

    let app = api::router_with_stores(
        pool.clone(),
        skill_dir.clone(),
        Vec::new(),
        asset_dir.clone(),
        Vec::new(),
    );
    let issue_body = json!({
        "schema_version": "1",
        "project_id": project.id,
        "revision_id": rev.id,
        "content_sha256": rev.content_sha256,
        "skill_id": audited.skill_id,
        "skill_version": audited.skill_version,
        "bundle_sha256": audited.bundle_sha256,
        "runtime_policy_sha256": policy_sha,
        "expires_at": "2099-01-01T00:00:00Z"
    });

    let res_issue = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/execution-authorizations")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", "11111111-1111-4111-8111-111111111132")
                .body(Body::from(serde_json::to_vec(&issue_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_issue.status(), StatusCode::CREATED);
    let issued: ExecutionAuthorization = serde_json::from_slice(
        &axum::body::to_bytes(res_issue.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();

    // External HTTP claim and workspace-evidence mutation routes are DISABLED!
    let res_claim_http = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/execution-authorizations/{}/claim", issued.id))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b"{}".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_claim_http.status(), StatusCode::NOT_FOUND);

    let res_ev_http = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/v1/execution-authorizations/{}/evidence",
                    issued.id
                ))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b"{}".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_ev_http.status(), StatusCode::METHOD_NOT_ALLOWED);

    // CLI rejects external claim and record-evidence subcommands.
    assert!(
        !Command::new(env!("CARGO_BIN_EXE_scenivox"))
            .args(["execution-auth", "claim", &issued.id])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        !Command::new(env!("CARGO_BIN_EXE_scenivox"))
            .args(["execution-auth", "record-evidence", &issued.id])
            .output()
            .unwrap()
            .status
            .success()
    );

    // Internal claim + verified worker evidence succeeds and becomes visible via read-only discovery.
    let job_id = seed_matching_render_job(&pool, &project, &rev, &approval_id).await;
    let claimed = store
        .claim_execution_authorization(
            &skill_dir,
            &asset_dir,
            &issued.id,
            ClaimExecutionAuthorization {
                schema_version: "1".into(),
                job_id: job_id.clone(),
                project_id: project.id.clone(),
                revision_id: rev.id.clone(),
                content_sha256: rev.content_sha256.clone(),
                skill_id: audited.skill_id,
                skill_version: audited.skill_version,
                bundle_sha256: audited.bundle_sha256,
                runtime_policy_sha256: policy_sha,
            },
        )
        .await
        .unwrap();
    assert_eq!(claimed.status, ExecutionAuthStatus::Claimed);

    let fake_worker = uuid::Uuid::new_v4().to_string();
    // Without active worker lease, evidence recording MUST fail!
    assert_eq!(
        store
            .record_execution_workspace_evidence(
                &issued.id,
                None,
                RecordWorkspaceEvidence {
                    schema_version: "1".into(),
                    job_id: job_id.clone(),
                    worker_owner_id: fake_worker.clone(),
                    worker_lease_epoch: 1,
                    workspace_manifest_sha256: "a".repeat(64),
                    output_artifact_sha256: "b".repeat(64),
                },
            )
            .await
            .unwrap_err()
            .code(),
        "reservation_ownership_required"
    );

    let real_worker = uuid::Uuid::new_v4().to_string();
    let lease = store.acquire_worker_lease(&real_worker, 60).await.unwrap();
    let (ev, _) = store
        .record_execution_workspace_evidence(
            &issued.id,
            None,
            RecordWorkspaceEvidence {
                schema_version: "1".into(),
                job_id,
                worker_owner_id: real_worker.clone(),
                worker_lease_epoch: lease.lease_epoch,
                workspace_manifest_sha256: "a".repeat(64),
                output_artifact_sha256: "b".repeat(64),
            },
        )
        .await
        .unwrap();
    assert_eq!(ev.worker_owner_id, real_worker);
    assert_eq!(ev.output_artifact_sha256, "b".repeat(64));

    // Read-only HTTP & CLI discovery succeed!
    let res_get = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/execution-authorizations/{}", issued.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_get.status(), StatusCode::OK);

    let res_ev = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/v1/execution-authorizations/{}/evidence",
                    issued.id
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_ev.status(), StatusCode::OK);
    let fetched_ev: ExecutionWorkspaceEvidence = serde_json::from_slice(
        &axum::body::to_bytes(res_ev.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(fetched_ev, ev);

    assert!(
        Command::new(env!("CARGO_BIN_EXE_scenivox"))
            .args(["execution-auth", "get-evidence", &issued.id])
            .env("SCENIVOX_DATABASE_URL", &db_url)
            .output()
            .unwrap()
            .status
            .success()
    );

    pool.close().await;
}
