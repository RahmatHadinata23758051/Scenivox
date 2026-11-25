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
    skill::{AuditStatus, MAX_BUNDLE_FILES, MAX_SINGLE_FILE_BYTES, SkillRecord},
    store::{Store, StoreError},
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
    assert_eq!(err.code(), "unsafe_skill_bundle");
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
    let app = api::router_with_skill_store(pool.clone(), store_dir.clone());

    let src_alpha = create_benign_skill_dir(temp.path(), "alpha-skill", "0.1.0", "alpha");
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
                        "source_path": src_beta
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
