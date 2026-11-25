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
        CreateProject, CreateRenderJob, CreateRevision, ListQuery, ModelSelection, ReviewCommand,
        RevisionInput, SceneInput, StoryboardInput, Target,
    },
    skill::{
        AuditStatus, MAX_BUNDLE_DIRS, MAX_BUNDLE_FILES, MAX_ENTRIES_PER_DIR, MAX_SINGLE_FILE_BYTES,
        SkillIntegrityReport, SkillRecord, SkillStoreRecoveryReport,
    },
    store::{SkillImportFault, Store, StoreError},
};
use serde_json::{Value, json};
use tower::ServiceExt;

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

fn temp_db_and_store(temp: &Path) -> (String, PathBuf) {
    let idx = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
    let db_path = temp.join(format!("skill-reg-{idx}.db"));
    let store_dir = temp.join(format!("skills-store-{idx}"));
    let url = format!(
        "sqlite://{}?mode=rwc",
        db_path.to_string_lossy().replace('\\', "/")
    );
    (url, store_dir)
}

fn manifest_json(id: &str, version: &str) -> Value {
    json!({
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
            "languages": ["javascript", "python"],
            "writable_paths": ["web/shots.js", "audio.py"],
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
        "accepted_inputs": ["revision.json", "narration.wav", "timeline.json"],
        "produced_outputs": ["final.mp4"]
    })
}

fn create_benign_skill_dir(root: &Path, id: &str, version: &str, script_marker: &str) -> PathBuf {
    let dir = root.join(format!("src-{id}-{version}"));
    fs::create_dir_all(dir.join("references")).unwrap();
    fs::create_dir_all(dir.join("scripts")).unwrap();
    fs::create_dir_all(dir.join("assets")).unwrap();

    fs::write(
        dir.join("skill.manifest.json"),
        serde_json::to_vec_pretty(&manifest_json(id, version)).unwrap(),
    )
    .unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!("# {id}\n\nDeterministic benign fixture skill {version}.\n"),
    )
    .unwrap();
    fs::write(
        dir.join("references/scene-guide.md"),
        "Use unified timeline composition.\n",
    )
    .unwrap();
    // Contains marker and intentional non-executed sentinel (`process.exit(99)` if ever run).
    fs::write(
        dir.join("scripts/render.js"),
        format!(
            "// {script_marker}\nthrow new Error('must never be executed during import/list');\n"
        ),
    )
    .unwrap();
    fs::write(dir.join("assets/palette.json"), br##"{"bg":"#111111"}"##).unwrap();
    dir
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
async fn imports_benign_skill_bundle_as_unaudited_with_deterministic_hash_and_persistence() {
    let temp = tempfile::tempdir().unwrap();
    let (db_url, store_dir) = temp_db_and_store(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    let src_a = create_benign_skill_dir(&temp.path().join("a"), "benign-stick", "0.1.0", "v1");
    let src_b = create_benign_skill_dir(&temp.path().join("b"), "benign-stick", "0.1.0", "v1");

    let (rec_a, replay_a) = store
        .import_skill(&store_dir, None, &src_a)
        .await
        .expect("import succeeds");
    assert!(!replay_a);
    assert_eq!(rec_a.skill_id, "benign-stick");
    assert_eq!(rec_a.skill_version, "0.1.0");
    assert_eq!(rec_a.audit_status, AuditStatus::Unaudited);
    assert!(rec_a.enabled);
    assert_eq!(rec_a.bundle_sha256.len(), 64);
    assert!(rec_a.layout.has_skill_md);
    assert_eq!(rec_a.layout.references_count, 1);
    assert_eq!(rec_a.layout.scripts_count, 1);
    assert_eq!(rec_a.layout.assets_count, 1);
    assert_eq!(rec_a.layout.file_count, 5);
    assert_eq!(count_staging_leftovers(&store_dir), 0);

    // Second independent DB + identical source directory produces exact same bundle_sha256.
    let (db_url_2, store_dir_2) = temp_db_and_store(temp.path());
    let pool_2 = db::connect(&db_url_2).await.unwrap();
    let (rec_b, _) = Store::new(pool_2.clone())
        .import_skill(&store_dir_2, None, &src_b)
        .await
        .unwrap();
    assert_eq!(rec_a.bundle_sha256, rec_b.bundle_sha256);
    pool_2.close().await;

    // DB reopen preserves persisted record, and SQLite triggers forbid mutating identity/deletion.
    pool.close().await;
    let reopened = db::connect(&db_url).await.unwrap();
    let fetched = Store::new(reopened.clone())
        .get_skill("benign-stick", "0.1.0")
        .await
        .unwrap();
    assert_eq!(fetched, rec_a);

    assert!(
        sqlx::query("UPDATE skill_bundles SET bundle_sha256=? WHERE skill_id='benign-stick'")
            .bind("a".repeat(64))
            .execute(&reopened)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM skill_bundles WHERE skill_id='benign-stick'")
            .execute(&reopened)
            .await
            .is_err()
    );
    reopened.close().await;
}

#[tokio::test]
async fn rejects_invalid_manifests_self_declared_audit_status_and_missing_entrypoints_with_cleanup()
{
    let temp = tempfile::tempdir().unwrap();
    let (db_url, store_dir) = temp_db_and_store(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    // 1. Self-declared `audit_status` inside manifest must be rejected cleanly.
    let self_cert = create_benign_skill_dir(temp.path(), "self-cert", "0.1.0", "x");
    let mut bad_manifest = manifest_json("self-cert", "0.1.0");
    bad_manifest["audit_status"] = json!("smoke_verified");
    fs::write(
        self_cert.join("skill.manifest.json"),
        serde_json::to_vec(&bad_manifest).unwrap(),
    )
    .unwrap();
    let err = store
        .import_skill(&store_dir, None, &self_cert)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "invalid_skill_manifest");
    assert_eq!(count_staging_leftovers(&store_dir), 0);
    assert!(
        store
            .list_skills(ListQuery::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );

    // 2. Duplicate JSON keys rejected.
    let dup_key = create_benign_skill_dir(temp.path(), "dup-key", "0.1.0", "x");
    fs::write(
        dup_key.join("skill.manifest.json"),
        br#"{"schema_version":"1","schema_version":"1","id":"dup-key","version":"0.1.0"}"#,
    )
    .unwrap();
    assert_eq!(
        store
            .import_skill(&store_dir, None, &dup_key)
            .await
            .unwrap_err()
            .code(),
        "invalid_skill_manifest"
    );
    assert_eq!(count_staging_leftovers(&store_dir), 0);

    // 3. Missing entrypoint script referenced in manifest rejected.
    let missing_ep = create_benign_skill_dir(temp.path(), "missing-ep", "0.1.0", "x");
    fs::remove_file(missing_ep.join("scripts/render.js")).unwrap();
    assert_eq!(
        store
            .import_skill(&store_dir, None, &missing_ep)
            .await
            .unwrap_err()
            .code(),
        "invalid_skill_manifest"
    );
    assert_eq!(count_staging_leftovers(&store_dir), 0);

    // 4. `requires_sandbox: false` rejected.
    let no_sandbox = create_benign_skill_dir(temp.path(), "no-sbx", "0.1.0", "x");
    let mut m = manifest_json("no-sbx", "0.1.0");
    m["code_generation_policy"]["requires_sandbox"] = json!(false);
    fs::write(
        no_sandbox.join("skill.manifest.json"),
        serde_json::to_vec(&m).unwrap(),
    )
    .unwrap();
    assert_eq!(
        store
            .import_skill(&store_dir, None, &no_sandbox)
            .await
            .unwrap_err()
            .code(),
        "invalid_skill_manifest"
    );

    // 5. Missing SKILL.md rejected.
    let no_md = create_benign_skill_dir(temp.path(), "no-md", "0.1.0", "x");
    fs::remove_file(no_md.join("SKILL.md")).unwrap();
    assert_eq!(
        store
            .import_skill(&store_dir, None, &no_md)
            .await
            .unwrap_err()
            .code(),
        "invalid_skill_manifest"
    );
    assert_eq!(count_staging_leftovers(&store_dir), 0);
    assert!(!store_dir.join("no-md").exists());

    pool.close().await;
}

#[tokio::test]
async fn rejects_zip_symlinks_hardlinks_reserved_names_case_collisions_special_files_and_oversized_bundles()
 {
    let temp = tempfile::tempdir().unwrap();
    let (db_url, store_dir) = temp_db_and_store(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    // 1. ZIP file rejected explicitly (missing approved ZIP/DEFLATE crate).
    let zip_path = temp.path().join("bundle.zip");
    fs::write(&zip_path, b"PK\x03\x04dummy-zip-header").unwrap();
    assert_eq!(
        store
            .import_skill(&store_dir, None, &zip_path)
            .await
            .unwrap_err()
            .code(),
        "unsupported_archive_format"
    );
    let disguised_zip = temp.path().join("bundle.bin");
    fs::write(&disguised_zip, b"PK\x03\x04dummy-zip-header").unwrap();
    assert_eq!(
        store
            .import_skill(&store_dir, None, &disguised_zip)
            .await
            .unwrap_err()
            .code(),
        "unsupported_archive_format"
    );

    // 2. Hidden dotfiles (.env) & reserved filenames (CON.txt) rejected.
    let dotfile_dir = create_benign_skill_dir(temp.path(), "dotfile-bad", "0.1.0", "x");
    fs::write(dotfile_dir.join(".env"), "SECRET=1").unwrap();
    assert_eq!(
        store
            .import_skill(&store_dir, None, &dotfile_dir)
            .await
            .unwrap_err()
            .code(),
        "unsafe_skill_bundle"
    );

    if cfg!(not(windows)) {
        // Windows blocks creating reserved device filename `CON.txt` directly on NTFS; test on Unix.
        let reserved_dir = create_benign_skill_dir(temp.path(), "res-bad", "0.1.0", "x");
        fs::write(reserved_dir.join("CON.txt"), "bad").unwrap();
        assert_eq!(
            store
                .import_skill(&store_dir, None, &reserved_dir)
                .await
                .unwrap_err()
                .code(),
            "unsafe_skill_bundle"
        );
    }

    // 3. Symlink inside bundle rejected.
    let outside = temp.path().join("outside-secret.txt");
    fs::write(&outside, b"secret").unwrap();
    let symlink_dir = create_benign_skill_dir(temp.path(), "symlink-bad", "0.1.0", "x");
    if create_symlink_file(&outside, &symlink_dir.join("assets/leak.txt")) {
        assert_eq!(
            store
                .import_skill(&store_dir, None, &symlink_dir)
                .await
                .unwrap_err()
                .code(),
            "unsafe_skill_bundle"
        );
        assert_eq!(count_staging_leftovers(&store_dir), 0);
    }

    // 4. Unix-specific checks: hardlinks, FIFOs, and same-dir case collisions.
    #[cfg(unix)]
    {
        let hardlink_dir = create_benign_skill_dir(temp.path(), "hardlink-bad", "0.1.0", "x");
        fs::hard_link(&outside, hardlink_dir.join("assets/link.txt")).unwrap();
        assert_eq!(
            store
                .import_skill(&store_dir, None, &hardlink_dir)
                .await
                .unwrap_err()
                .code(),
            "unsafe_skill_bundle"
        );

        let fifo_dir = create_benign_skill_dir(temp.path(), "fifo-bad", "0.1.0", "x");
        let status = Command::new("mkfifo")
            .arg(fifo_dir.join("assets/pipe.fifo"))
            .status()
            .unwrap();
        if status.success() {
            assert_eq!(
                store
                    .import_skill(&store_dir, None, &fifo_dir)
                    .await
                    .unwrap_err()
                    .code(),
                "unsafe_skill_bundle"
            );
        }

        let case_dir = create_benign_skill_dir(temp.path(), "case-bad", "0.1.0", "x");
        fs::write(case_dir.join("assets/Palette.json"), br#"{}"#).unwrap();
        assert_eq!(
            store
                .import_skill(&store_dir, None, &case_dir)
                .await
                .unwrap_err()
                .code(),
            "unsafe_skill_bundle"
        );
    }

    // 5. Oversized single file rejected.
    let big_file_dir = create_benign_skill_dir(temp.path(), "big-file", "0.1.0", "x");
    let big = fs::File::create(big_file_dir.join("assets/huge.bin")).unwrap();
    big.set_len(MAX_SINGLE_FILE_BYTES + 1).unwrap();
    assert_eq!(
        store
            .import_skill(&store_dir, None, &big_file_dir)
            .await
            .unwrap_err()
            .code(),
        "unsafe_skill_bundle"
    );
    assert_eq!(count_staging_leftovers(&store_dir), 0);

    // 6. Oversized file count rejected.
    let many_files_dir = create_benign_skill_dir(temp.path(), "many-files", "0.1.0", "x");
    for i in 0..MAX_BUNDLE_FILES {
        fs::write(many_files_dir.join(format!("assets/f{i}.txt")), b"x").unwrap();
    }
    assert_eq!(
        store
            .import_skill(&store_dir, None, &many_files_dir)
            .await
            .unwrap_err()
            .code(),
        "unsafe_skill_bundle"
    );
    assert_eq!(count_staging_leftovers(&store_dir), 0);

    pool.close().await;
}

#[tokio::test]
async fn handles_version_hash_conflicts_concurrent_imports_and_rollback_cleanly() {
    let temp = tempfile::tempdir().unwrap();
    let (db_url, store_dir) = temp_db_and_store(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    let src_v1 =
        create_benign_skill_dir(&temp.path().join("orig"), "conflict-skill", "1.0.0", "v1");
    let (orig, _) = store.import_skill(&store_dir, None, &src_v1).await.unwrap();

    // Exact same version + identical content -> skill_already_exists (409).
    let same_err = store
        .import_skill(&store_dir, None, &src_v1)
        .await
        .unwrap_err();
    assert_eq!(same_err.code(), "skill_already_exists");

    // Same version + different bundle hash -> skill_version_conflict (409).
    let src_modified = create_benign_skill_dir(
        &temp.path().join("mod"),
        "conflict-skill",
        "1.0.0",
        "tampered",
    );
    let ver_err = store
        .import_skill(&store_dir, None, &src_modified)
        .await
        .unwrap_err();
    assert_eq!(ver_err.code(), "skill_version_conflict");
    assert_eq!(count_staging_leftovers(&store_dir), 0);
    assert_eq!(
        store
            .get_skill("conflict-skill", "1.0.0")
            .await
            .unwrap()
            .bundle_sha256,
        orig.bundle_sha256
    );

    // Different (skill_id, skill_version) with an already-registered bundle_sha256 -> skill_hash_conflict (409).
    let src_v2 = create_benign_skill_dir(&temp.path().join("v2"), "conflict-skill", "2.0.0", "v2");
    let (v2_rec, _) = store.import_skill(&store_dir, None, &src_v2).await.unwrap();
    let src_v3 = create_benign_skill_dir(&temp.path().join("v3"), "conflict-skill", "3.0.0", "v3");
    let src_hash_dup = create_benign_skill_dir(
        &temp.path().join("hashdup"),
        "dup-hash-skill",
        "1.0.0",
        "dup",
    );
    let preview_dup = scenivox::skill::stage_local_skill(&store_dir, &src_hash_dup).unwrap();
    let dup_sha = preview_dup.bundle_sha256.clone();
    drop(preview_dup);
    sqlx::query(
        "INSERT INTO skill_bundles(skill_id,skill_version,bundle_sha256,manifest_json,layout_json,storage_rel_dir,enabled,audit_status,audit_evidence_json,file_count,total_bytes,imported_at) VALUES('seeded-other','9.9.9',?,'{}','{}','seeded-other/9.9.9',1,'unaudited',NULL,1,1,'2026-11-25T00:00:00Z')",
    )
    .bind(&dup_sha)
    .execute(&pool)
    .await
    .unwrap();
    let hash_err = store
        .import_skill(&store_dir, None, &src_hash_dup)
        .await
        .unwrap_err();
    assert_eq!(hash_err.code(), "skill_hash_conflict");
    assert_eq!(count_staging_leftovers(&store_dir), 0);
    assert!(!store_dir.join("dup-hash-skill").exists());

    // Concurrent race on identical bundle yields exactly one winner and zero leftovers.
    let src_race_a =
        create_benign_skill_dir(&temp.path().join("race-a"), "race-skill", "1.0.0", "same");
    let src_race_b =
        create_benign_skill_dir(&temp.path().join("race-b"), "race-skill", "1.0.0", "same");
    let (res_a, res_b) = tokio::join!(
        store.import_skill(&store_dir, None, &src_race_a),
        store.import_skill(&store_dir, None, &src_race_b)
    );
    match (res_a, res_b) {
        (Ok(_), Err(err)) | (Err(err), Ok(_)) => assert_eq!(err.code(), "skill_already_exists"),
        other => panic!("expected exactly one concurrent winner: {other:?}"),
    }
    assert_eq!(count_staging_leftovers(&store_dir), 0);
    assert_eq!(v2_rec.skill_version, "2.0.0");

    // Failed SQLite commit / target collision leaves no stray directory.
    let pre_existing_target = store_dir.join("conflict-skill").join("3.0.0");
    // Pre-create blocking file at target path to simulate rename failure after staging.
    fs::create_dir_all(&pre_existing_target).unwrap();
    let staged_preview =
        scenivox::skill::stage_local_skill(&store_dir, &src_v3).expect("staging works");
    let blocking_dir = store_dir
        .join("conflict-skill")
        .join("3.0.0")
        .join(&staged_preview.bundle_sha256);
    drop(staged_preview); // drop removes staging dir cleanly
    assert_eq!(count_staging_leftovers(&store_dir), 0);
    fs::create_dir_all(&blocking_dir).unwrap();
    let err = store
        .import_skill(&store_dir, None, &src_v3)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "interrupted_skill_publication");
    assert!(matches!(
        store.get_skill("conflict-skill", "3.0.0").await,
        Err(StoreError::NotFound)
    ));
    assert_eq!(count_staging_leftovers(&store_dir), 0);

    pool.close().await;
}

#[tokio::test]
async fn http_api_and_cli_discover_skills_and_preserve_fail_closed_render_jobs() {
    let temp = tempfile::tempdir().unwrap();
    let (db_url, store_dir) = temp_db_and_store(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let trusted_import_root = temp.path().join("trusted-imports");
    fs::create_dir_all(&trusted_import_root).unwrap();
    let app = api::router_with_skill_config(
        pool.clone(),
        store_dir.clone(),
        vec![trusted_import_root.clone()],
    );

    let src_alpha = create_benign_skill_dir(&trusted_import_root, "alpha-skill", "0.1.0", "alpha");
    let src_beta = create_benign_skill_dir(temp.path(), "beta-skill", "0.1.0", "beta");

    // Missing Idempotency-Key returns 400.
    let no_key = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/skills:import")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "schema_version": "1",
                        "source_path": src_alpha
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(no_key.status(), StatusCode::BAD_REQUEST);

    // Import via HTTP with Idempotency-Key.
    let key1 = "11111111-1111-4111-8111-111111111111";
    let body1 = serde_json::to_vec(&json!({
        "schema_version": "1",
        "source_path": src_alpha
    }))
    .unwrap();
    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/skills:import")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", key1)
                .body(Body::from(body1.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    assert_eq!(
        created.headers().get(header::LOCATION).unwrap(),
        "/v1/skills/alpha-skill/0.1.0"
    );
    let rec_bytes = axum::body::to_bytes(created.into_body(), usize::MAX)
        .await
        .unwrap();
    let rec: SkillRecord = serde_json::from_slice(&rec_bytes).unwrap();
    assert_eq!(rec.audit_status, AuditStatus::Unaudited);

    // Replay with same key & body returns same record; same key & different body returns 409.
    let replay = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/skills:import")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", key1)
                .body(Body::from(body1))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::CREATED);
    let src_gamma = create_benign_skill_dir(&trusted_import_root, "gamma-skill", "0.1.0", "gamma");
    let conflict = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/skills:import")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", key1)
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "schema_version": "1",
                        "source_path": src_gamma
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);

    // Import second skill via CLI, then inspect via HTTP pagination & CLI list/get.
    let cli_import = Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args(["skill", "import", "--path"])
        .arg(&src_beta)
        .env("SCENIVOX_DATABASE_URL", &db_url)
        .env("SCENIVOX_SKILL_STORE_DIR", &store_dir)
        .output()
        .unwrap();
    assert!(
        cli_import.status.success(),
        "cli import stderr: {}",
        String::from_utf8_lossy(&cli_import.stderr)
    );
    let cli_rec: SkillRecord = serde_json::from_slice(&cli_import.stdout).unwrap();
    assert_eq!(cli_rec.skill_id, "beta-skill");
    assert_eq!(cli_rec.audit_status, AuditStatus::Unaudited);

    // Paginated read-only HTTP discovery (`limit=1`).
    let page1_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/skills?limit=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page1_res.status(), StatusCode::OK);
    let page1: Value = serde_json::from_slice(
        &axum::body::to_bytes(page1_res.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(page1["items"].as_array().unwrap().len(), 1);
    assert_eq!(page1["items"][0]["skill_id"], "alpha-skill");
    let cursor = page1["next_cursor"].as_str().unwrap();

    let page2_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/skills?limit=1&cursor={cursor}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page2_res.status(), StatusCode::OK);
    let page2: Value = serde_json::from_slice(
        &axum::body::to_bytes(page2_res.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(page2["items"][0]["skill_id"], "beta-skill");
    assert!(page2["next_cursor"].is_null());

    // Invalid cursor fails cleanly.
    let bad_cursor = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/skills?cursor=not-a-hash")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_cursor.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // HTTP GET by id/version & 404 when absent.
    let get_ok = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/skills/beta-skill/0.1.0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_ok.status(), StatusCode::OK);
    let get_404 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/skills/missing-skill/0.1.0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_404.status(), StatusCode::NOT_FOUND);

    // CLI list & get succeed read-only.
    let cli_list = Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args(["skill", "list"])
        .env("SCENIVOX_DATABASE_URL", &db_url)
        .env("SCENIVOX_SKILL_STORE_DIR", &store_dir)
        .output()
        .unwrap();
    assert!(cli_list.status.success());
    let listed: Value = serde_json::from_slice(&cli_list.stdout).unwrap();
    assert_eq!(listed["items"].as_array().unwrap().len(), 2);

    let cli_get = Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args(["skill", "get", "alpha-skill", "0.1.0"])
        .env("SCENIVOX_DATABASE_URL", &db_url)
        .env("SCENIVOX_SKILL_STORE_DIR", &store_dir)
        .output()
        .unwrap();
    assert!(cli_get.status.success());

    // Importing a skill does NOT activate production render-job enqueue!
    let store = Store::new(pool.clone());
    let target = Target {
        duration_ms: 5_000,
        aspect_ratio: "9:16".into(),
        fps: 30,
        language: "id-ID".into(),
        style_id: "alpha-skill".into(),
    };
    let project = store
        .create_project(CreateProject {
            schema_version: "1".into(),
            title: "Fail closed test".into(),
            idea: "Ide video pendek".into(),
            target: target.clone(),
            model_selection: ModelSelection::default(),
        })
        .await
        .unwrap();
    let models = store
        .plan_storyboard(
            &project.id,
            None,
            scenivox::domain::PlanStoryboard {
                schema_version: "1".into(),
                idea: "Ide video pendek".into(),
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
    let rev = store
        .create_revision(
            &project.id,
            CreateRevision {
                schema_version: "1".into(),
                expected_revision_id: project.current_revision_id.or(Some(
                    store
                        .get_project(&project.id)
                        .await
                        .unwrap()
                        .current_revision_id
                        .unwrap(),
                )),
                content: RevisionInput {
                    schema_version: "1".into(),
                    idea: "Ide video pendek".into(),
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
                    models,
                    asset_hashes: vec![],
                    skill_id: "alpha-skill".into(),
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

    pool.close().await;
}

#[tokio::test]
async fn finding1_bounded_traversal_limits_and_non_overlapping_store_dirs() {
    let temp = tempfile::tempdir().unwrap();
    let (db_url, store_dir) = temp_db_and_store(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    // 1. Exceeding MAX_BUNDLE_DIRS rejected cleanly.
    let many_dirs = create_benign_skill_dir(temp.path(), "many-dirs", "0.1.0", "x");
    for i in 0..MAX_BUNDLE_DIRS {
        fs::create_dir_all(many_dirs.join(format!("assets/d{i}"))).unwrap();
    }
    assert_eq!(
        store
            .import_skill(&store_dir, None, &many_dirs)
            .await
            .unwrap_err()
            .code(),
        "unsafe_skill_bundle"
    );
    assert_eq!(count_staging_leftovers(&store_dir), 0);

    // 2. Exceeding MAX_ENTRIES_PER_DIR rejected cleanly.
    let crowded_dir = create_benign_skill_dir(temp.path(), "crowded-dir", "0.1.0", "x");
    for i in 0..MAX_ENTRIES_PER_DIR {
        fs::write(crowded_dir.join(format!("assets/item{i}.txt")), b"a").unwrap();
    }
    assert_eq!(
        store
            .import_skill(&store_dir, None, &crowded_dir)
            .await
            .unwrap_err()
            .code(),
        "unsafe_skill_bundle"
    );
    assert_eq!(count_staging_leftovers(&store_dir), 0);

    // 3. Relative path exceeding MAX_REL_PATH_LEN (512 bytes) rejected cleanly.
    let long_rel = create_benign_skill_dir(temp.path(), "long-rel", "0.1.0", "x");
    let mut nested = long_rel.join("assets");
    for _ in 0..6 {
        nested = nested.join(
            "segment_0123456789_0123456789_0123456789_0123456789_0123456789_0123456789_0123456789",
        );
    }
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("leaf.txt"), b"x").unwrap();
    assert_eq!(
        store
            .import_skill(&store_dir, None, &long_rel)
            .await
            .unwrap_err()
            .code(),
        "unsafe_skill_bundle"
    );
    assert_eq!(count_staging_leftovers(&store_dir), 0);

    // 4. Overlapping source and skill_store_dir rejected (equal, parent, child).
    fs::create_dir_all(&store_dir).unwrap();
    let store_inside_src = temp.path().join("parent-src");
    let nested_store = store_inside_src.join("nested-store");
    let src_parent = create_benign_skill_dir(&store_inside_src, "overlap-parent", "0.1.0", "x");
    // Exact overlap (source == skill_store_dir):
    let valid_bundle = create_benign_skill_dir(temp.path(), "valid-x", "0.1.0", "x");
    assert_eq!(
        store
            .import_skill(&valid_bundle, None, &valid_bundle)
            .await
            .unwrap_err()
            .code(),
        "unsafe_skill_bundle"
    );
    // Source inside skill_store_dir:
    let child_src = create_benign_skill_dir(&store_dir, "child-inside-store", "0.1.0", "x");
    assert_eq!(
        store
            .import_skill(&store_dir, None, &child_src)
            .await
            .unwrap_err()
            .code(),
        "unsafe_skill_bundle"
    );
    // skill_store_dir inside source:
    fs::create_dir_all(src_parent.join("inner-store")).unwrap();
    assert_eq!(
        store
            .import_skill(&src_parent.join("inner-store"), None, &src_parent)
            .await
            .unwrap_err()
            .code(),
        "unsafe_skill_bundle"
    );
    assert!(!nested_store.exists());

    pool.close().await;
}

#[tokio::test]
async fn finding2_storage_integrity_reverification_detects_tamper_missing_extra_and_unsafe_paths() {
    let temp = tempfile::tempdir().unwrap();
    let (db_url, store_dir) = temp_db_and_store(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());
    let app = api::router_with_skill_store(pool.clone(), store_dir.clone());

    let src = create_benign_skill_dir(temp.path(), "verify-skill", "0.1.0", "verify");
    let (rec, _) = store.import_skill(&store_dir, None, &src).await.unwrap();
    let stored_root = store_dir.join(&rec.storage_rel_dir);

    // 1. Clean read-only verification succeeds via Store, HTTP, and CLI.
    let report = store
        .verify_skill(&store_dir, "verify-skill", "0.1.0")
        .await
        .unwrap();
    assert!(report.verified);
    assert_eq!(report.bundle_sha256, rec.bundle_sha256);

    let http_ok = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/skills/verify-skill/0.1.0/integrity")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(http_ok.status(), StatusCode::OK);
    let http_report: SkillIntegrityReport = serde_json::from_slice(
        &axum::body::to_bytes(http_ok.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(http_report.verified);

    let cli_verify = Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args(["skill", "verify", "verify-skill", "0.1.0"])
        .env("SCENIVOX_DATABASE_URL", &db_url)
        .env("SCENIVOX_SKILL_STORE_DIR", &store_dir)
        .output()
        .unwrap();
    assert!(cli_verify.status.success());

    // 2. Modified stored file detected cleanly (409 skill_integrity_failed).
    let palette = stored_root.join("assets/palette.json");
    let orig_palette = fs::read(&palette).unwrap();
    fs::write(&palette, br##"{"bg":"#ffffff"}"##).unwrap();
    assert_eq!(
        store
            .verify_skill(&store_dir, "verify-skill", "0.1.0")
            .await
            .unwrap_err()
            .code(),
        "skill_integrity_failed"
    );
    let http_tampered = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/skills/verify-skill/0.1.0/integrity")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(http_tampered.status(), StatusCode::CONFLICT);
    fs::write(&palette, orig_palette).unwrap();

    // 3. Unexpected extra file detected cleanly.
    let extra_file = stored_root.join("assets/unexpected.js");
    fs::write(&extra_file, b"// injected").unwrap();
    assert_eq!(
        store
            .verify_skill(&store_dir, "verify-skill", "0.1.0")
            .await
            .unwrap_err()
            .code(),
        "skill_integrity_failed"
    );
    fs::remove_file(&extra_file).unwrap();

    // 4. Case alias / unsafe stored file detected cleanly on Unix.
    #[cfg(unix)]
    {
        let alias_file = stored_root.join("assets/Palette.json");
        fs::write(&alias_file, b"{}").unwrap();
        assert_eq!(
            store
                .verify_skill(&store_dir, "verify-skill", "0.1.0")
                .await
                .unwrap_err()
                .code(),
            "skill_integrity_failed"
        );
        fs::remove_file(&alias_file).unwrap();
    }

    // 5. Missing stored file detected cleanly.
    fs::remove_file(stored_root.join("references/scene-guide.md")).unwrap();
    assert_eq!(
        store
            .verify_skill(&store_dir, "verify-skill", "0.1.0")
            .await
            .unwrap_err()
            .code(),
        "skill_integrity_failed"
    );

    pool.close().await;
}

#[tokio::test]
async fn finding3_injected_db_fault_and_interrupted_publication_recovery_never_adopt_or_delete_registered()
 {
    let temp = tempfile::tempdir().unwrap();
    let (db_url, store_dir) = temp_db_and_store(temp.path());
    let pool = db::connect(&db_url).await.unwrap();
    let store = Store::new(pool.clone());

    // Pre-existing valid registered bundle that must NEVER be touched or deleted by faults/recovery.
    let registered_src = create_benign_skill_dir(temp.path(), "registered-keep", "0.1.0", "keep");
    let (kept_rec, _) = store
        .import_skill(&store_dir, None, &registered_src)
        .await
        .unwrap();

    // 1. Injected DB fault BEFORE publish_files cleans staging & rolls back SQLite row.
    let fault_src = create_benign_skill_dir(temp.path(), "fault-skill", "0.1.0", "fault");
    let err_before = store
        .import_skill_with_policy(
            &store_dir,
            None,
            &fault_src,
            None,
            SkillImportFault::BeforePublish,
        )
        .await
        .unwrap_err();
    assert_eq!(err_before.code(), "database_unavailable");
    assert_eq!(count_staging_leftovers(&store_dir), 0);
    assert!(!store_dir.join("fault-skill").exists());
    assert!(matches!(
        store.get_skill("fault-skill", "0.1.0").await,
        Err(StoreError::NotFound)
    ));

    // 2. Injected DB fault AFTER publish_files (before commit) rolls back SQLite row AND deletes newly moved target dir.
    let err_after = store
        .import_skill_with_policy(
            &store_dir,
            None,
            &fault_src,
            None,
            SkillImportFault::AfterPublishBeforeCommit,
        )
        .await
        .unwrap_err();
    assert_eq!(err_after.code(), "database_unavailable");
    assert_eq!(count_staging_leftovers(&store_dir), 0);
    assert!(!store_dir.join("fault-skill").exists());
    assert!(matches!(
        store.get_skill("fault-skill", "0.1.0").await,
        Err(StoreError::NotFound)
    ));

    // 3. Read-only reconcile reports orphaned staging dirs, unregistered bundles, and missing registered bundles
    //    without deleting unrelated dirs, active staging dirs, unregistered bundles, or registered bundles.
    let active_staging = store_dir.join(".staging-active-concurrent");
    fs::create_dir_all(&active_staging).unwrap();
    fs::write(active_staging.join("in_flight.tmp"), b"busy").unwrap();

    let unrelated_dir = store_dir.join("unrelated-user-folder");
    fs::create_dir_all(&unrelated_dir).unwrap();
    fs::write(unrelated_dir.join("notes.txt"), b"keep-me").unwrap();

    let preview = scenivox::skill::stage_local_skill(&store_dir, &fault_src).unwrap();
    let orphan_rel_dir = preview.storage_rel_dir.clone();
    let orphan_pub_dir = store_dir.join(&orphan_rel_dir);
    drop(preview);
    fs::create_dir_all(&orphan_pub_dir).unwrap();
    fs::write(orphan_pub_dir.join("SKILL.md"), b"orphan").unwrap();

    // Unregistered bundle is NEVER silently adopted into SQLite!
    assert!(matches!(
        store.get_skill("fault-skill", "0.1.0").await,
        Err(StoreError::NotFound)
    ));
    assert_eq!(
        store
            .import_skill(&store_dir, None, &fault_src)
            .await
            .unwrap_err()
            .code(),
        "interrupted_skill_publication"
    );

    // Seed a second registered skill whose on-disk folder is removed externally to test missing-registered detection.
    let missing_src = create_benign_skill_dir(temp.path(), "vanished-skill", "0.1.0", "vanish");
    let (vanished_rec, _) = store
        .import_skill(&store_dir, None, &missing_src)
        .await
        .unwrap();
    fs::remove_dir_all(store_dir.join(&vanished_rec.storage_rel_dir)).unwrap();

    let cli_rec = Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args(["skill", "reconcile"])
        .env("SCENIVOX_DATABASE_URL", &db_url)
        .env("SCENIVOX_SKILL_STORE_DIR", &store_dir)
        .output()
        .unwrap();
    assert!(
        cli_rec.status.success(),
        "cli reconcile stderr: {}",
        String::from_utf8_lossy(&cli_rec.stderr)
    );
    let report: SkillStoreRecoveryReport = serde_json::from_slice(&cli_rec.stdout).unwrap();
    assert!(report.read_only);
    assert_eq!(report.staging_dirs, vec![".staging-active-concurrent"]);
    assert_eq!(report.unregistered_bundles, vec![orphan_rel_dir.clone()]);
    assert_eq!(
        report.missing_registered_bundles,
        vec![vanished_rec.storage_rel_dir.clone()]
    );
    assert_eq!(report.registered_bundles_present, 1);

    // Confirm NOTHING was deleted or silently adopted:
    assert!(active_staging.join("in_flight.tmp").is_file());
    assert!(unrelated_dir.join("notes.txt").is_file());
    assert!(orphan_pub_dir.join("SKILL.md").is_file());
    assert!(
        store
            .verify_skill(&store_dir, &kept_rec.skill_id, &kept_rec.skill_version)
            .await
            .unwrap()
            .verified
    );
    assert!(
        store
            .get_skill(&vanished_rec.skill_id, &vanished_rec.skill_version)
            .await
            .is_ok()
    );
    assert!(matches!(
        store.get_skill("fault-skill", "0.1.0").await,
        Err(StoreError::NotFound)
    ));

    // 4. Staging cleanup when target parent directory creation fails:
    // Create a separate store where `<skill_id>` is blocked by a regular file so `create_dir_all(target.parent())` fails.
    let (db_url_blk, store_dir_blk) = temp_db_and_store(temp.path());
    let pool_blk = db::connect(&db_url_blk).await.unwrap();
    let store_blk = Store::new(pool_blk.clone());
    fs::create_dir_all(&store_dir_blk).unwrap();
    let blocking_parent_file = store_dir_blk.join("parent-blocked");
    fs::write(&blocking_parent_file, b"regular-file-blocking-mkdir").unwrap();
    let blocked_src = create_benign_skill_dir(temp.path(), "parent-blocked", "0.1.0", "blk");
    let err_parent = store_blk
        .import_skill(&store_dir_blk, None, &blocked_src)
        .await
        .unwrap_err();
    assert_eq!(err_parent.code(), "input_unavailable");
    assert_eq!(
        count_staging_leftovers(&store_dir_blk),
        0,
        "staging dir leaked when create_dir_all(target_dir.parent()) failed"
    );
    assert!(blocking_parent_file.is_file());

    pool_blk.close().await;
    pool.close().await;
}

#[tokio::test]
async fn finding4_http_import_restricted_to_trusted_roots_while_cli_remains_explicit() {
    let temp = tempfile::tempdir().unwrap();
    let (db_url, store_dir) = temp_db_and_store(temp.path());
    let pool = db::connect(&db_url).await.unwrap();

    let trusted_root = temp.path().join("trusted-root");
    let untrusted_root = temp.path().join("untrusted-root");
    fs::create_dir_all(&trusted_root).unwrap();
    fs::create_dir_all(&untrusted_root).unwrap();
    let src_trusted = create_benign_skill_dir(&trusted_root, "http-allowed", "0.1.0", "ok");
    let src_untrusted = create_benign_skill_dir(&untrusted_root, "http-denied", "0.1.0", "no");

    // 1. HTTP router WITHOUT trusted_skill_import_roots fails closed with 403 default-deny!
    let app_default_deny = api::router_with_skill_store(pool.clone(), store_dir.clone());
    let res_denied_default = app_default_deny
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/skills:import")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", "22222222-2222-4222-8222-222222222222")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "schema_version": "1",
                        "source_path": src_trusted
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_denied_default.status(), StatusCode::FORBIDDEN);

    // 2. HTTP router WITH trusted_root rejects outside path (403) and allows inside path (201).
    let app_scoped =
        api::router_with_skill_config(pool.clone(), store_dir.clone(), vec![trusted_root.clone()]);
    let res_outside = app_scoped
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/skills:import")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", "33333333-3333-4333-8333-333333333333")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "schema_version": "1",
                        "source_path": src_untrusted
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_outside.status(), StatusCode::FORBIDDEN);

    let res_inside = app_scoped
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/skills:import")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", "44444444-4444-4444-8444-444444444444")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "schema_version": "1",
                        "source_path": src_trusted
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_inside.status(), StatusCode::CREATED);

    // 3. Explicit CLI import preserves direct local operator access even for untrusted_root paths.
    let cli_ok = Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args(["skill", "import", "--path"])
        .arg(&src_untrusted)
        .env("SCENIVOX_DATABASE_URL", &db_url)
        .env("SCENIVOX_SKILL_STORE_DIR", &store_dir)
        .output()
        .unwrap();
    assert!(
        cli_ok.status.success(),
        "cli explicit import failed: {}",
        String::from_utf8_lossy(&cli_ok.stderr)
    );

    pool.close().await;
}

fn create_symlink_file(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        match std::os::windows::fs::symlink_file(target, link) {
            Ok(()) => true,
            Err(err)
                if err.kind() == std::io::ErrorKind::PermissionDenied
                    || err.raw_os_error() == Some(1314) =>
            {
                false
            }
            Err(err) => panic!("unexpected symlink error: {err}"),
        }
    }
}
