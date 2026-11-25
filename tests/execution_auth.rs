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
    api, db,
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
};
use serde_json::json;
use tower::ServiceExt;

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

fn temp_db_and_store(temp: &Path) -> (String, PathBuf) {
    let idx = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
    let url = format!(
        "sqlite://{}?mode=rwc",
        temp.join(format!("exec-auth-{idx}.db"))
            .to_string_lossy()
            .replace('\\', "/")
    );
    let skill_dir = temp.join(format!("skills-{idx}"));
    (url, skill_dir)
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

async fn setup_approved_project(
    store: &Store,
    skill_id: &str,
    skill_version: &str,
) -> (scenivox::domain::Project, Revision) {
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
                    asset_hashes: vec![],
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
    store
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
    (store.get_project(&project.id).await.unwrap(), rev)
}

async fn setup_audited_skill(
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
            AuditStatus::StaticAudited,
            Some(r#"{"auditor":"host"}"#),
        )
        .await
        .unwrap()
}

fn sample_policy_sha256() -> String {
    let image = format!("ghcr.io/scenivox/runtime@sha256:{}", "a".repeat(64));
    SandboxPolicy::hardened_default(image)
        .unwrap()
        .policy_sha256()
        .unwrap()
}

#[tokio::test]
async fn execution_auth_requires_approved_audited_pinned_inputs_and_atomic_single_use_claim() {
    let temp = tempfile::tempdir().unwrap();
    let (db_url, skill_dir) = temp_db_and_store(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    let policy_sha = sample_policy_sha256();
    let skill_src = write_benign_skill(temp.path(), "exec-skill", "0.1.0");
    let (unaudited, _) = store
        .import_skill(&skill_dir, None, &skill_src)
        .await
        .unwrap();
    let (project, rev) = setup_approved_project(&store, "exec-skill", "0.1.0").await;

    // 1. Unaudited skill CANNOT be issued an ExecutionAuthorization.
    let err_unaudited = store
        .issue_execution_authorization(
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
        .unwrap_err();
    assert_eq!(err_unaudited.code(), "skill_unavailable");

    // Promote skill via host-owned audit evidence.
    let audited = store
        .record_host_skill_audit(
            &unaudited.skill_id,
            &unaudited.skill_version,
            &unaudited.bundle_sha256,
            AuditStatus::StaticAudited,
            Some(r#"{"auditor":"host"}"#),
        )
        .await
        .unwrap();

    // Wrong bundle_sha256 rejected cleanly.
    assert_eq!(
        store
            .issue_execution_authorization(
                None,
                IssueExecutionAuthorization {
                    schema_version: "1".into(),
                    project_id: project.id.clone(),
                    revision_id: rev.id.clone(),
                    content_sha256: rev.content_sha256.clone(),
                    skill_id: audited.skill_id.clone(),
                    skill_version: audited.skill_version.clone(),
                    bundle_sha256: "f".repeat(64),
                    runtime_policy_sha256: policy_sha.clone(),
                    expires_at: "2099-01-01T00:00:00Z".into(),
                },
            )
            .await
            .unwrap_err()
            .code(),
        "execution_not_authorized"
    );

    // Production render enqueue still stays fail-closed even after skill is audited!
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

    // Issue valid authorization bound to exact project, revision, content_sha256, skill, bundle, policy.
    let (auth, _) = store
        .issue_execution_authorization(
            None,
            IssueExecutionAuthorization {
                schema_version: "1".into(),
                project_id: project.id.clone(),
                revision_id: rev.id.clone(),
                content_sha256: rev.content_sha256.clone(),
                skill_id: audited.skill_id.clone(),
                skill_version: audited.skill_version.clone(),
                bundle_sha256: audited.bundle_sha256.clone(),
                runtime_policy_sha256: policy_sha.clone(),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(auth.status, ExecutionAuthStatus::Issued);
    assert!(auth.claimed_job_id.is_none());

    // 2. Concurrent claim race: 6 future jobs race for the single-use authorization -> EXACTLY 1 wins!
    let mut claims = Vec::new();
    for _ in 0..6 {
        let s = store.clone();
        let aid = auth.id.clone();
        let pid = project.id.clone();
        let rid = rev.id.clone();
        let csha = rev.content_sha256.clone();
        let sid = audited.skill_id.clone();
        let sver = audited.skill_version.clone();
        let bsha = audited.bundle_sha256.clone();
        let psha = policy_sha.clone();
        claims.push(tokio::spawn(async move {
            let jid = uuid::Uuid::new_v4().to_string();
            let res = s
                .claim_execution_authorization(
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
    assert_eq!(losers, 5);
    let (winning_job_id, claimed_auth) = winners.pop().unwrap();
    assert_eq!(claimed_auth.status, ExecutionAuthStatus::Claimed);
    assert_eq!(
        claimed_auth.claimed_job_id.as_deref(),
        Some(winning_job_id.as_str())
    );

    // 3. Re-claiming even with the EXACT same winning job_id fails closed!
    assert_eq!(
        store
            .claim_execution_authorization(
                &auth.id,
                ClaimExecutionAuthorization {
                    schema_version: "1".into(),
                    job_id: winning_job_id.clone(),
                    project_id: project.id.clone(),
                    revision_id: rev.id.clone(),
                    content_sha256: rev.content_sha256.clone(),
                    skill_id: audited.skill_id.clone(),
                    skill_version: audited.skill_version.clone(),
                    bundle_sha256: audited.bundle_sha256.clone(),
                    runtime_policy_sha256: policy_sha.clone(),
                },
            )
            .await
            .unwrap_err()
            .code(),
        "execution_not_authorized"
    );

    // 4. Issuing a SECOND authorization and trying to reuse `winning_job_id` (cross-job reuse) fails closed!
    let (auth_second, _) = store
        .issue_execution_authorization(
            None,
            IssueExecutionAuthorization {
                schema_version: "1".into(),
                project_id: project.id.clone(),
                revision_id: rev.id.clone(),
                content_sha256: rev.content_sha256.clone(),
                skill_id: audited.skill_id.clone(),
                skill_version: audited.skill_version.clone(),
                bundle_sha256: audited.bundle_sha256.clone(),
                runtime_policy_sha256: policy_sha.clone(),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .claim_execution_authorization(
                &auth_second.id,
                ClaimExecutionAuthorization {
                    schema_version: "1".into(),
                    job_id: winning_job_id,
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
            .unwrap_err()
            .code(),
        "execution_not_authorized"
    );

    pool.close().await;
}

#[tokio::test]
async fn prevents_cross_project_cross_revision_policy_tamper_and_separates_workspace_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let (db_url, skill_dir) = temp_db_and_store(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    let policy_sha = sample_policy_sha256();
    let audited = setup_audited_skill(&store, &skill_dir, temp.path(), "sep-skill", "0.1.0").await;
    let (project_a, rev_a) = setup_approved_project(&store, "sep-skill", "0.1.0").await;
    let (project_b, rev_b) = setup_approved_project(&store, "sep-skill", "0.1.0").await;

    let (auth_a, _) = store
        .issue_execution_authorization(
            None,
            IssueExecutionAuthorization {
                schema_version: "1".into(),
                project_id: project_a.id.clone(),
                revision_id: rev_a.id.clone(),
                content_sha256: rev_a.content_sha256.clone(),
                skill_id: audited.skill_id.clone(),
                skill_version: audited.skill_version.clone(),
                bundle_sha256: audited.bundle_sha256.clone(),
                runtime_policy_sha256: policy_sha.clone(),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();

    // 1. Cannot record post-execution workspace evidence BEFORE auth is claimed!
    let job_a = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        store
            .record_execution_workspace_evidence(
                &auth_a.id,
                None,
                RecordWorkspaceEvidence {
                    schema_version: "1".into(),
                    job_id: job_a.clone(),
                    workspace_manifest_sha256: "b".repeat(64),
                    output_artifact_sha256: None,
                },
            )
            .await
            .unwrap_err()
            .code(),
        "execution_not_authorized"
    );

    // 2. Cross-project, cross-revision, wrong policy hash, wrong content hash, wrong bundle hash claims MUST fail!
    let alt_policy = "e".repeat(64);
    for (bad_proj, bad_rev, bad_content_sha, bad_bundle_sha, bad_policy_sha) in [
        (
            &project_b.id,
            &rev_b.id,
            &rev_b.content_sha256,
            &audited.bundle_sha256,
            &policy_sha,
        ),
        (
            &project_a.id,
            &rev_b.id,
            &rev_a.content_sha256,
            &audited.bundle_sha256,
            &policy_sha,
        ),
        (
            &project_a.id,
            &rev_a.id,
            &rev_b.content_sha256,
            &audited.bundle_sha256,
            &policy_sha,
        ),
        (
            &project_a.id,
            &rev_a.id,
            &rev_a.content_sha256,
            &alt_policy,
            &policy_sha,
        ),
        (
            &project_a.id,
            &rev_a.id,
            &rev_a.content_sha256,
            &audited.bundle_sha256,
            &alt_policy,
        ),
    ] {
        assert_eq!(
            store
                .claim_execution_authorization(
                    &auth_a.id,
                    ClaimExecutionAuthorization {
                        schema_version: "1".into(),
                        job_id: uuid::Uuid::new_v4().to_string(),
                        project_id: bad_proj.clone(),
                        revision_id: bad_rev.clone(),
                        content_sha256: bad_content_sha.clone(),
                        skill_id: audited.skill_id.clone(),
                        skill_version: audited.skill_version.clone(),
                        bundle_sha256: bad_bundle_sha.clone(),
                        runtime_policy_sha256: bad_policy_sha.clone(),
                    },
                )
                .await
                .unwrap_err()
                .code(),
            "execution_not_authorized"
        );
    }

    // Valid claim succeeds.
    let claimed = store
        .claim_execution_authorization(
            &auth_a.id,
            ClaimExecutionAuthorization {
                schema_version: "1".into(),
                job_id: job_a.clone(),
                project_id: project_a.id.clone(),
                revision_id: rev_a.id.clone(),
                content_sha256: rev_a.content_sha256.clone(),
                skill_id: audited.skill_id.clone(),
                skill_version: audited.skill_version.clone(),
                bundle_sha256: audited.bundle_sha256.clone(),
                runtime_policy_sha256: policy_sha.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(claimed.status, ExecutionAuthStatus::Claimed);

    // 3. Mismatched job_id cannot attach workspace evidence to auth_a!
    let wrong_job = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        store
            .record_execution_workspace_evidence(
                &auth_a.id,
                None,
                RecordWorkspaceEvidence {
                    schema_version: "1".into(),
                    job_id: wrong_job,
                    workspace_manifest_sha256: "c".repeat(64),
                    output_artifact_sha256: Some("d".repeat(64)),
                },
            )
            .await
            .unwrap_err()
            .code(),
        "execution_not_authorized"
    );

    // Valid post-execution workspace evidence succeeds and stays cleanly separated & immutable.
    let (evidence, _) = store
        .record_execution_workspace_evidence(
            &auth_a.id,
            None,
            RecordWorkspaceEvidence {
                schema_version: "1".into(),
                job_id: job_a.clone(),
                workspace_manifest_sha256: "c".repeat(64),
                output_artifact_sha256: Some("d".repeat(64)),
            },
        )
        .await
        .unwrap();
    assert_eq!(evidence.execution_auth_id, auth_a.id);
    assert_eq!(evidence.job_id, job_a);
    let auth_after = store.get_execution_authorization(&auth_a.id).await.unwrap();
    assert_eq!(auth_after.status, ExecutionAuthStatus::EvidenceRecorded);

    // Duplicate evidence recording blocked cleanly.
    assert_eq!(
        store
            .record_execution_workspace_evidence(
                &auth_a.id,
                None,
                RecordWorkspaceEvidence {
                    schema_version: "1".into(),
                    job_id: job_a,
                    workspace_manifest_sha256: "1".repeat(64),
                    output_artifact_sha256: None,
                },
            )
            .await
            .unwrap_err()
            .code(),
        "invalid_transition"
    );

    // SQLite immutability triggers block mutating either table.
    assert!(
        sqlx::query(
            "UPDATE execution_workspace_evidence SET workspace_manifest_sha256=? WHERE id=?"
        )
        .bind("0".repeat(64))
        .bind(&evidence.id)
        .execute(&pool)
        .await
        .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM execution_authorizations WHERE id=?")
            .bind(&auth_a.id)
            .execute(&pool)
            .await
            .is_err()
    );

    // 4. Editing a project invalidates any unclaimed authorization issued for the superseded revision!
    let (auth_stale, _) = store
        .issue_execution_authorization(
            None,
            IssueExecutionAuthorization {
                schema_version: "1".into(),
                project_id: project_b.id.clone(),
                revision_id: rev_b.id.clone(),
                content_sha256: rev_b.content_sha256.clone(),
                skill_id: audited.skill_id.clone(),
                skill_version: audited.skill_version.clone(),
                bundle_sha256: audited.bundle_sha256.clone(),
                runtime_policy_sha256: policy_sha.clone(),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .await
        .unwrap();
    let mut edit_content = serde_json::to_value(&rev_b.content).unwrap();
    edit_content["storyboard"]["scenes"][0]["id"] = serde_json::Value::Null;
    let edit_input: RevisionInput = serde_json::from_value(edit_content).unwrap();
    store
        .create_revision(
            &project_b.id,
            CreateRevision {
                schema_version: "1".into(),
                expected_revision_id: Some(rev_b.id.clone()),
                content: edit_input,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .claim_execution_authorization(
                &auth_stale.id,
                ClaimExecutionAuthorization {
                    schema_version: "1".into(),
                    job_id: uuid::Uuid::new_v4().to_string(),
                    project_id: project_b.id,
                    revision_id: rev_b.id,
                    content_sha256: rev_b.content_sha256,
                    skill_id: audited.skill_id,
                    skill_version: audited.skill_version,
                    bundle_sha256: audited.bundle_sha256,
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
async fn http_and_cli_execution_authorization_and_evidence_workflow() {
    let temp = tempfile::tempdir().unwrap();
    let (db_url, skill_dir) = temp_db_and_store(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    let policy_sha = sample_policy_sha256();
    let audited = setup_audited_skill(&store, &skill_dir, temp.path(), "http-skill", "0.1.0").await;
    let (project, rev) = setup_approved_project(&store, "http-skill", "0.1.0").await;

    let app = api::router(pool.clone());
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

    // Reject smuggling evidence/job fields into issuance!
    let mut bad_issue = issue_body.clone();
    bad_issue["workspace_manifest_sha256"] = json!("a".repeat(64));
    let res_smuggle = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/execution-authorizations")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", "11111111-1111-4111-8111-111111111131")
                .body(Body::from(serde_json::to_vec(&bad_issue).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_smuggle.status(), StatusCode::UNPROCESSABLE_ENTITY);

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

    // Claim and record evidence through CLI against same SQLite DB.
    let future_job_id = uuid::Uuid::new_v4().to_string();
    let claim_file = temp.path().join("claim.json");
    fs::write(
        &claim_file,
        serde_json::to_vec(&json!({
            "schema_version": "1",
            "job_id": future_job_id,
            "project_id": project.id,
            "revision_id": rev.id,
            "content_sha256": rev.content_sha256,
            "skill_id": audited.skill_id,
            "skill_version": audited.skill_version,
            "bundle_sha256": audited.bundle_sha256,
            "runtime_policy_sha256": policy_sha,
        }))
        .unwrap(),
    )
    .unwrap();
    let cli_claim = Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args(["execution-auth", "claim", &issued.id, "--file"])
        .arg(&claim_file)
        .env("SCENIVOX_DATABASE_URL", &db_url)
        .output()
        .unwrap();
    assert!(
        cli_claim.status.success(),
        "cli claim stderr: {}",
        String::from_utf8_lossy(&cli_claim.stderr)
    );

    let ev_file = temp.path().join("evidence.json");
    fs::write(
        &ev_file,
        serde_json::to_vec(&json!({
            "schema_version": "1",
            "job_id": future_job_id,
            "workspace_manifest_sha256": "a".repeat(64),
            "output_artifact_sha256": "b".repeat(64)
        }))
        .unwrap(),
    )
    .unwrap();
    let cli_ev = Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args(["execution-auth", "record-evidence", &issued.id, "--file"])
        .arg(&ev_file)
        .env("SCENIVOX_DATABASE_URL", &db_url)
        .output()
        .unwrap();
    assert!(cli_ev.status.success());

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
    let ev: ExecutionWorkspaceEvidence = serde_json::from_slice(
        &axum::body::to_bytes(res_ev.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(ev.workspace_manifest_sha256, "a".repeat(64));

    pool.close().await;
}
