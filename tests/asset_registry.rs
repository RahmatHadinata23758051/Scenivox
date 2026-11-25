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
    asset::{
        AssetIntegrityReport, AssetKind, AssetRecord, AssetStoreRecoveryReport, RegisterAsset,
    },
    db,
    domain::{
        CreateProject, CreateRenderJob, CreateRevision, ModelSelection, PlanStoryboard,
        ReviewCommand, RevisionInput, SceneInput, SfxCue, StoryboardInput, Target,
    },
    store::{AssetImportFault, Store},
    voice::AudioProvenance,
};
use serde_json::{Value, json};
use tower::ServiceExt;

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

fn temp_db_and_store(temp: &Path) -> (String, PathBuf) {
    let idx = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
    let db_path = temp.join(format!("asset-reg-{idx}.db"));
    let store_dir = temp.join(format!("assets-store-{idx}"));
    let url = format!(
        "sqlite://{}?mode=rwc",
        db_path.to_string_lossy().replace('\\', "/")
    );
    (url, store_dir)
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

fn count_staging_leftovers(store_dir: &Path) -> usize {
    if !store_dir.exists() {
        return 0;
    }
    fs::read_dir(store_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(".staging-"))
        .count()
}

#[tokio::test]
async fn registers_content_addressed_wav_immutably_and_resolves_revisions() {
    let temp = tempfile::tempdir().unwrap();
    let (db_url, store_dir) = temp_db_and_store(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    let src_dir = temp.path().join("src");
    fs::create_dir_all(&src_dir).unwrap();
    let wav_a = src_dir.join("narration.wav");
    fs::write(&wav_a, pcm16_wav_bytes(16_000, 1, 0x11)).unwrap();
    let wav_a_copy = src_dir.join("narration-copy.wav");
    fs::write(&wav_a_copy, pcm16_wav_bytes(16_000, 1, 0x11)).unwrap();
    let wav_sfx = src_dir.join("sfx.wav");
    fs::write(&wav_sfx, pcm16_wav_bytes(16_000, 1, 0x22)).unwrap();

    let (rec, replay, dup) = store
        .register_asset(
            &store_dir,
            None,
            RegisterAsset {
                schema_version: "1".into(),
                source_path: wav_a.to_string_lossy().into_owned(),
                kind: AssetKind::NarrationWav,
                provenance: None,
            },
        )
        .await
        .unwrap();
    assert!(!replay);
    assert!(!dup);
    assert_eq!(rec.kind, AssetKind::NarrationWav);
    assert_eq!(rec.provenance, AudioProvenance::UserSupplied);
    assert_eq!(rec.sha256, rec.metadata.audio.sha256);
    assert_eq!(rec.metadata.audio.duration_ms, 1_000);
    assert!(store_dir.join(&rec.storage_rel_path).is_file());
    assert_eq!(count_staging_leftovers(&store_dir), 0);

    // Content-addressed dedup returns the same immutable record when kind/provenance match.
    let (dup_rec, _, dup_flag) = store
        .register_asset(
            &store_dir,
            None,
            RegisterAsset {
                schema_version: "1".into(),
                source_path: wav_a_copy.to_string_lossy().into_owned(),
                kind: AssetKind::NarrationWav,
                provenance: Some(AudioProvenance::UserSupplied),
            },
        )
        .await
        .unwrap();
    assert!(dup_flag);
    assert_eq!(dup_rec, rec);

    // Different kind or provenance for identical sha256 conflicts cleanly.
    let err_conflict = store
        .register_asset(
            &store_dir,
            None,
            RegisterAsset {
                schema_version: "1".into(),
                source_path: wav_a.to_string_lossy().into_owned(),
                kind: AssetKind::SfxWav,
                provenance: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err_conflict.code(), "asset_conflict");

    // Register SFX asset and bind both asset_hashes and sfx_cues into an approved revision.
    let (sfx_rec, _, _) = store
        .register_asset(
            &store_dir,
            None,
            RegisterAsset {
                schema_version: "1".into(),
                source_path: wav_sfx.to_string_lossy().into_owned(),
                kind: AssetKind::SfxWav,
                provenance: Some(AudioProvenance::SyntheticTestFixture),
            },
        )
        .await
        .unwrap();
    assert_eq!(sfx_rec.provenance, AudioProvenance::SyntheticTestFixture);

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
            title: "Asset Test".into(),
            idea: "Video uji aset".into(),
            target: target.clone(),
            model_selection: ModelSelection::default(),
        })
        .await
        .unwrap();
    let models = store
        .plan_storyboard(
            &project.id,
            None,
            PlanStoryboard {
                schema_version: "1".into(),
                idea: "Video uji aset".into(),
                target: target.clone(),
                expected_revision_id: None,
                model_selection: ModelSelection::default(),
            },
            &scenivox::planner::DeterministicMockPlanner,
        )
        .await
        .unwrap()
        .content
        .models;
    let current = store
        .get_project(&project.id)
        .await
        .unwrap()
        .current_revision_id;
    let rev = store
        .create_revision(
            &project.id,
            CreateRevision {
                schema_version: "1".into(),
                expected_revision_id: current,
                content: RevisionInput {
                    schema_version: "1".into(),
                    idea: "Video uji aset".into(),
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
                            action: "Diam".into(),
                            narration: "Tes".into(),
                            caption_hint: "".into(),
                            sfx_cues: vec![SfxCue {
                                asset_id: sfx_rec.id.clone(),
                                at_ms: 100,
                            }],
                        }],
                    },
                    models,
                    asset_hashes: vec![rec.sha256.clone()],
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

    // Resolved assets pass asset gate and remain fail-closed at skill gate (no worker/renderer).
    let enqueue_err = store
        .enqueue_render_job(
            &project.id,
            None,
            CreateRenderJob {
                schema_version: "1".into(),
                revision_id: rev.id,
                content_sha256: rev.content_sha256,
                cost_consent_id: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(enqueue_err.code(), "skill_unavailable");

    // Reopen persistence + SQLite immutability triggers.
    pool.close().await;
    let reopened = db::connect(&db_url).await.unwrap();
    let reopened_store = Store::new(reopened.clone());
    assert_eq!(reopened_store.get_asset(&rec.id).await.unwrap(), rec);
    assert_eq!(
        reopened_store
            .get_asset_by_sha256(&rec.sha256)
            .await
            .unwrap(),
        rec
    );
    assert!(
        sqlx::query("UPDATE assets SET sha256=? WHERE id=?")
            .bind("a".repeat(64))
            .bind(&rec.id)
            .execute(&reopened)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM assets WHERE id=?")
            .bind(&rec.id)
            .execute(&reopened)
            .await
            .is_err()
    );
    reopened.close().await;
}

#[tokio::test]
async fn http_and_cli_discovery_trusted_roots_idempotency_and_bad_audio_rejection() {
    let temp = tempfile::tempdir().unwrap();
    let (db_url, store_dir) = temp_db_and_store(temp.path());
    let pool = db::connect(&db_url).await.unwrap();

    let trusted = temp.path().join("trusted");
    let untrusted = temp.path().join("untrusted");
    fs::create_dir_all(&trusted).unwrap();
    fs::create_dir_all(&untrusted).unwrap();

    let trusted_wav = trusted.join("clip1.wav");
    fs::write(&trusted_wav, pcm16_wav_bytes(16_000, 1, 0x33)).unwrap();
    let untrusted_wav = untrusted.join("clip2.wav");
    fs::write(&untrusted_wav, pcm16_wav_bytes(16_000, 1, 0x44)).unwrap();
    let bad_wav = trusted.join("bad.wav");
    fs::write(&bad_wav, b"NOT_A_WAV").unwrap();

    // 1. HTTP without trusted roots fails closed (403).
    let app_deny = api::router_with_asset_config(pool.clone(), store_dir.clone(), Vec::new());
    let res_deny = app_deny
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/assets")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", "11111111-1111-4111-8111-111111111111")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "schema_version": "1",
                        "source_path": trusted_wav,
                        "kind": "narration_wav"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_deny.status(), StatusCode::FORBIDDEN);

    // 2. Scoped HTTP allows trusted root, rejects outside root & invalid WAV, handles idempotency.
    let app = api::router_with_asset_config(pool.clone(), store_dir.clone(), vec![trusted.clone()]);
    let res_outside = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/assets")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", "11111111-1111-4111-8111-111111111112")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "schema_version": "1",
                        "source_path": untrusted_wav,
                        "kind": "narration_wav"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_outside.status(), StatusCode::FORBIDDEN);

    let res_bad = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/assets")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", "11111111-1111-4111-8111-111111111113")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "schema_version": "1",
                        "source_path": bad_wav,
                        "kind": "narration_wav"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_bad.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let idem_key = "11111111-1111-4111-8111-111111111114";
    let res_ok = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/assets")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", idem_key)
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "schema_version": "1",
                        "source_path": trusted_wav,
                        "kind": "narration_wav"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_ok.status(), StatusCode::CREATED);
    let created: AssetRecord = serde_json::from_slice(
        &axum::body::to_bytes(res_ok.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();

    // Same key replay returns exact same record.
    let res_replay = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/assets")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", idem_key)
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "schema_version": "1",
                        "source_path": trusted_wav,
                        "kind": "narration_wav"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_replay.status(), StatusCode::CREATED);
    let replayed: AssetRecord = serde_json::from_slice(
        &axum::body::to_bytes(res_replay.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(replayed, created);

    // Same key + different body -> 409 idempotency_conflict.
    let res_conflict = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/assets")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", idem_key)
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "schema_version": "1",
                        "source_path": trusted_wav,
                        "kind": "sfx_wav"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_conflict.status(), StatusCode::CONFLICT);

    // HTTP discovery & integrity endpoints.
    let res_get = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/assets/{}", created.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_get.status(), StatusCode::OK);

    let res_list = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/assets")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_list.status(), StatusCode::OK);
    let list_json: Value = serde_json::from_slice(
        &axum::body::to_bytes(res_list.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(list_json["items"].as_array().unwrap().len(), 1);

    let res_verify = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/assets/{}/integrity", created.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_verify.status(), StatusCode::OK);
    let report: AssetIntegrityReport = serde_json::from_slice(
        &axum::body::to_bytes(res_verify.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(report.verified);

    // Explicit CLI registration works cleanly for operator local paths.
    let cli_reg = Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args([
            "asset",
            "register",
            "--path",
            untrusted_wav.to_str().unwrap(),
            "--kind",
            "sfx_wav",
        ])
        .env("SCENIVOX_DATABASE_URL", &db_url)
        .env("SCENIVOX_ASSET_STORE_DIR", &store_dir)
        .output()
        .unwrap();
    assert!(
        cli_reg.status.success(),
        "cli register stderr: {}",
        String::from_utf8_lossy(&cli_reg.stderr)
    );
    let cli_asset: AssetRecord = serde_json::from_slice(&cli_reg.stdout).unwrap();
    assert_eq!(cli_asset.kind, AssetKind::SfxWav);

    let cli_list = Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args(["asset", "list"])
        .env("SCENIVOX_DATABASE_URL", &db_url)
        .env("SCENIVOX_ASSET_STORE_DIR", &store_dir)
        .output()
        .unwrap();
    assert!(cli_list.status.success());

    let cli_verify = Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args(["asset", "verify", &cli_asset.id])
        .env("SCENIVOX_DATABASE_URL", &db_url)
        .env("SCENIVOX_ASSET_STORE_DIR", &store_dir)
        .output()
        .unwrap();
    assert!(cli_verify.status.success());

    pool.close().await;
}

#[tokio::test]
async fn integrity_tamper_fault_rollback_and_read_only_reconcile() {
    let temp = tempfile::tempdir().unwrap();
    let (db_url, store_dir) = temp_db_and_store(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    let src_dir = temp.path().join("src");
    fs::create_dir_all(&src_dir).unwrap();
    let keep_wav = src_dir.join("keep.wav");
    fs::write(&keep_wav, pcm16_wav_bytes(16_000, 1, 0x55)).unwrap();
    let fault_wav = src_dir.join("fault.wav");
    fs::write(&fault_wav, pcm16_wav_bytes(16_000, 1, 0x66)).unwrap();
    let missing_wav = src_dir.join("missing.wav");
    fs::write(&missing_wav, pcm16_wav_bytes(16_000, 1, 0x77)).unwrap();

    let (kept, _, _) = store
        .register_asset(
            &store_dir,
            None,
            RegisterAsset {
                schema_version: "1".into(),
                source_path: keep_wav.to_string_lossy().into_owned(),
                kind: AssetKind::NarrationWav,
                provenance: None,
            },
        )
        .await
        .unwrap();

    // 1. Overlapping path rejected cleanly.
    let inside_store = store_dir.join("inside.wav");
    fs::write(&inside_store, pcm16_wav_bytes(16_000, 1, 0x88)).unwrap();
    assert_eq!(
        store
            .register_asset(
                &store_dir,
                None,
                RegisterAsset {
                    schema_version: "1".into(),
                    source_path: inside_store.to_string_lossy().into_owned(),
                    kind: AssetKind::NarrationWav,
                    provenance: None,
                },
            )
            .await
            .unwrap_err()
            .code(),
        "unsafe_asset_path"
    );
    fs::remove_file(&inside_store).unwrap();

    // 2. DB fault before and after publish rolls back DB & cleans up staged/published file.
    for fault in [
        AssetImportFault::BeforePublish,
        AssetImportFault::AfterPublishBeforeCommit,
    ] {
        let err = store
            .register_asset_with_policy(
                &store_dir,
                None,
                RegisterAsset {
                    schema_version: "1".into(),
                    source_path: fault_wav.to_string_lossy().into_owned(),
                    kind: AssetKind::NarrationWav,
                    provenance: None,
                },
                None,
                fault,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), "database_unavailable");
        assert_eq!(count_staging_leftovers(&store_dir), 0);
    }

    // 3. Integrity check detects tampered payload, then passes once restored.
    let stored_path = store_dir.join(&kept.storage_rel_path);
    let orig = fs::read(&stored_path).unwrap();
    let mut tampered = orig.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0xFF;
    fs::write(&stored_path, tampered).unwrap();
    assert_eq!(
        store
            .verify_asset(&store_dir, &kept.id)
            .await
            .unwrap_err()
            .code(),
        "asset_integrity_failed"
    );
    fs::write(&stored_path, orig).unwrap();
    assert!(
        store
            .verify_asset(&store_dir, &kept.id)
            .await
            .unwrap()
            .verified
    );

    // 4. Read-only reconcile reports staging dir, unregistered orphan blob, and missing asset without deleting anything.
    let (missing_rec, _, _) = store
        .register_asset(
            &store_dir,
            None,
            RegisterAsset {
                schema_version: "1".into(),
                source_path: missing_wav.to_string_lossy().into_owned(),
                kind: AssetKind::NarrationWav,
                provenance: None,
            },
        )
        .await
        .unwrap();
    fs::remove_file(store_dir.join(&missing_rec.storage_rel_path)).unwrap();

    let staging_active = store_dir.join(".staging-active-concurrent");
    fs::create_dir_all(&staging_active).unwrap();
    fs::write(staging_active.join("asset.wav"), b"busy").unwrap();

    let orphan_preview =
        scenivox::asset::stage_local_asset(&store_dir, &fault_wav, AudioProvenance::UserSupplied)
            .unwrap();
    let orphan_rel = orphan_preview.storage_rel_path.clone();
    drop(orphan_preview);
    let orphan_path = store_dir.join(&orphan_rel);
    fs::create_dir_all(orphan_path.parent().unwrap()).unwrap();
    fs::write(&orphan_path, pcm16_wav_bytes(16_000, 1, 0x66)).unwrap();

    assert_eq!(
        store
            .register_asset(
                &store_dir,
                None,
                RegisterAsset {
                    schema_version: "1".into(),
                    source_path: fault_wav.to_string_lossy().into_owned(),
                    kind: AssetKind::NarrationWav,
                    provenance: None,
                },
            )
            .await
            .unwrap_err()
            .code(),
        "interrupted_asset_publication"
    );

    let cli_rec = Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args(["asset", "reconcile"])
        .env("SCENIVOX_DATABASE_URL", &db_url)
        .env("SCENIVOX_ASSET_STORE_DIR", &store_dir)
        .output()
        .unwrap();
    assert!(cli_rec.status.success());
    let rec_report: AssetStoreRecoveryReport = serde_json::from_slice(&cli_rec.stdout).unwrap();
    assert!(rec_report.read_only);
    assert_eq!(rec_report.staging_dirs, vec![".staging-active-concurrent"]);
    assert_eq!(rec_report.unregistered_assets, vec![orphan_rel.clone()]);
    assert_eq!(
        rec_report.missing_registered_assets,
        vec![missing_rec.storage_rel_path.clone()]
    );
    assert_eq!(rec_report.registered_assets_present, 1);

    // Everything on disk stays untouched by read-only reconcile.
    assert!(staging_active.join("asset.wav").is_file());
    assert!(orphan_path.is_file());
    assert!(
        store
            .verify_asset(&store_dir, &kept.id)
            .await
            .unwrap()
            .verified
    );

    pool.close().await;
}
