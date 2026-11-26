use scenivox::{
    db,
    domain::{CreateProject, CreateRevision, ListQuery, Revision, RevisionContent, UpdateProject},
    store::{Store, StoreError},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tempfile::TempDir;
use uuid::Uuid;

const PROJECT: &str = include_str!("fixtures/p1-project.json");
const REVISION: &str = include_str!("fixtures/p1-revision.json");
const ABSENT: &str = "00000000-0000-4000-8000-000000000099";

struct Database {
    // Keep the directory alive until all pooled SQLite connections have closed.
    directory: TempDir,
    pool: SqlitePool,
}

impl Database {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory
                .path()
                .join("persistence.db")
                .to_string_lossy()
                .replace('\\', "/")
        );
        let pool = db::connect(&url).await.unwrap();
        Self { directory, pool }
    }

    fn store(&self) -> Store {
        Store::new(self.pool.clone())
    }

    async fn close(self) {
        self.pool.close().await;
        drop(self.directory);
    }
}

fn project_input() -> CreateProject {
    serde_json::from_str(PROJECT).unwrap()
}

fn revision_input(parent: Option<&str>) -> CreateRevision {
    let mut input: CreateRevision = serde_json::from_str(REVISION).unwrap();
    input.expected_revision_id = parent.map(str::to_owned);
    input
}

fn edit_input(parent: &Revision) -> CreateRevision {
    // This models a client editing its last snapshot, not a server-side clone API.
    let content = serde_json::to_value(&parent.content).unwrap();
    CreateRevision {
        schema_version: "1".into(),
        expected_revision_id: Some(parent.id.clone()),
        content: serde_json::from_value(content).unwrap(),
    }
}

fn assert_id(id: &str) {
    let parsed = Uuid::parse_str(id).expect("server IDs must be UUIDs");
    assert_eq!(
        parsed.to_string(),
        id,
        "IDs must be canonical lowercase UUIDs"
    );
    assert_eq!(parsed.get_version_num(), 4);
}

fn assert_validation(error: StoreError, expected_code: &str) {
    match error {
        StoreError::Validation { code, path } => {
            assert_eq!(code, expected_code);
            assert!(
                path.starts_with('/'),
                "validation must identify a JSON path: {path}"
            );
        }
        other => panic!("expected validation {expected_code}, got {other:?}"),
    }
}

#[tokio::test]
async fn project_crud_and_cursor_pages_are_persisted() {
    let database = Database::new().await;
    let store = database.store();
    assert!(
        store
            .list_projects(ListQuery::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let first = store.create_project(project_input()).await.unwrap();
    assert_id(&first.id);
    assert_eq!(first.schema_version, "1");
    assert_eq!(first.title, "Misteri bawah tanah");
    assert_eq!(first.idea, "Rahasia laboratorium di bawah kota");
    assert_eq!(first.target.duration_ms, 60000);
    assert_eq!(first.current_revision_id, None);
    assert_eq!(first.approved_revision_id, None);
    assert_eq!(store.get_project(&first.id).await.unwrap(), first);
    let second = store.create_project(project_input()).await.unwrap();
    let all = store.list_projects(ListQuery::default()).await.unwrap();
    assert_eq!(all.schema_version, "1");
    assert_eq!(all.items.len(), 2);
    assert_eq!(all.next_cursor, None);
    let page = store
        .list_projects(ListQuery {
            limit: Some(1),
            cursor: None,
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    let next = store
        .list_projects(ListQuery {
            limit: Some(1),
            cursor: page.next_cursor.clone(),
        })
        .await
        .unwrap();
    assert!(page.next_cursor.is_some());
    assert_eq!(next.items.len(), 1);
    assert_eq!(next.next_cursor, None);
    assert_ne!(page.items[0].id, next.items[0].id);
    assert_eq!(
        vec![page.items[0].clone(), next.items[0].clone()],
        all.items
    );
    let updated = store
        .update_project(
            &first.id,
            UpdateProject {
                schema_version: "1".into(),
                title: "Judul baru".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.title, "Judul baru");
    let mut expected = first.clone();
    expected.title = "Judul baru".into();
    assert_eq!(updated, expected);
    assert_eq!(store.get_project(&first.id).await.unwrap(), expected);
    store.delete_project(&first.id).await.unwrap();
    assert!(matches!(
        store.get_project(&first.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.delete_project(&first.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store
            .update_project(
                ABSENT,
                UpdateProject {
                    schema_version: "1".into(),
                    title: "Missing".into(),
                }
            )
            .await,
        Err(StoreError::NotFound)
    ));
    assert_eq!(
        store
            .list_projects(ListQuery::default())
            .await
            .unwrap()
            .items,
        vec![second]
    );
    database.close().await;
}

#[tokio::test]
async fn migrations_reopen_and_metadata_edits_preserve_historical_revisions() {
    let database = Database::new().await;
    let store = database.store();
    let project = store.create_project(project_input()).await.unwrap();
    let first = store
        .create_revision(&project.id, revision_input(None))
        .await
        .unwrap();
    assert_id(&first.id);
    assert_eq!(first.version, 1);
    assert_eq!(first.parent_revision_id, None);
    assert_eq!(first.project_id, project.id);
    assert_eq!(first.content.storyboard.scenes.len(), 2);
    for scene in &first.content.storyboard.scenes {
        assert_id(&scene.id);
    }
    assert_ne!(
        first.content.storyboard.scenes[0].id,
        first.content.storyboard.scenes[1].id
    );
    let mut edit = edit_input(&first);
    edit.content.storyboard.scenes[0].narration = "Catatan itu mengubah segalanya.".into();
    let second = store.create_revision(&project.id, edit).await.unwrap();
    assert_eq!(second.version, 2);
    assert_eq!(
        second.parent_revision_id.as_deref(),
        Some(first.id.as_str())
    );
    assert_ne!(second.content_sha256, first.content_sha256);
    store
        .update_project(
            &project.id,
            UpdateProject {
                schema_version: "1".into(),
                title: "Hanya judul".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.get_revision(&project.id, &first.id).await.unwrap(),
        first
    );
    assert_eq!(
        store.get_revision(&project.id, &second.id).await.unwrap(),
        second
    );
    let current = store.get_project(&project.id).await.unwrap();
    assert_eq!(
        current.current_revision_id.as_deref(),
        Some(second.id.as_str())
    );
    assert_eq!(current.approved_revision_id, None);
    let revisions = store
        .list_revisions(&project.id, ListQuery::default())
        .await
        .unwrap();
    let mut expected_revisions = vec![first.clone(), second.clone()];
    expected_revisions.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    assert_eq!(revisions.items, expected_revisions);
    let page = store
        .list_revisions(
            &project.id,
            ListQuery {
                limit: Some(1),
                cursor: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items, vec![expected_revisions[0].clone()]);
    let next = store
        .list_revisions(
            &project.id,
            ListQuery {
                limit: Some(1),
                cursor: page.next_cursor,
            },
        )
        .await
        .unwrap();
    assert_eq!(next.items, vec![expected_revisions[1].clone()]);
    assert_eq!(next.next_cursor, None);
    database.pool.close().await;
    let url = format!(
        "sqlite://{}?mode=rw",
        database
            .directory
            .path()
            .join("persistence.db")
            .to_string_lossy()
            .replace('\\', "/")
    );
    let reopened = db::connect(&url).await.unwrap();
    let migration_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations WHERE success = 1")
            .fetch_one(&reopened)
            .await
            .unwrap();
    assert_eq!(migration_count, 12);
    let store = Store::new(reopened.clone());
    assert_eq!(store.get_project(&project.id).await.unwrap(), current);
    assert_eq!(
        store.get_revision(&project.id, &first.id).await.unwrap(),
        first
    );
    assert_eq!(
        store.get_revision(&project.id, &second.id).await.unwrap(),
        second
    );
    store.delete_project(&project.id).await.unwrap();
    assert!(matches!(
        store.get_revision(&project.id, &first.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store
            .list_revisions(&project.id, ListQuery::default())
            .await,
        Err(StoreError::NotFound)
    ));
    drop(store);
    reopened.close().await;
    database.close().await;
}

#[tokio::test]
async fn populated_pre_0004_database_preserves_projects_revisions_and_hashes() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory
            .path()
            .join("upgrade.db")
            .to_string_lossy()
            .replace('\\', "/")
    );
    let pool = db::connect(&url).await.unwrap();
    let legacy_store = Store::new(pool.clone());
    let project = legacy_store.create_project(project_input()).await.unwrap();
    let revision = legacy_store
        .create_revision(&project.id, revision_input(None))
        .await
        .unwrap();
    let project_before = legacy_store.get_project(&project.id).await.unwrap();
    let hash_before: String = sqlx::query_scalar("SELECT content_sha256 FROM revisions WHERE id=?")
        .bind(&revision.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(hash_before, revision.content_sha256);

    let migrator = sqlx::migrate!("./migrations");
    sqlx::raw_sql("DROP TRIGGER approvals_insert_requires_current_review; DROP TRIGGER revision_review_transition_guard; DROP TRIGGER approvals_no_update; DROP TRIGGER approvals_no_delete; DROP TABLE approvals; DROP TABLE revision_reviews; DELETE FROM _sqlx_migrations WHERE version=4;")
        .execute(&pool).await.unwrap();
    migrator.run(&pool).await.unwrap();
    let review_state: String = sqlx::query_scalar(
        "SELECT review_state FROM revision_reviews WHERE project_id=? AND revision_id=?",
    )
    .bind(&project.id)
    .bind(&revision.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(review_state, "draft");
    let upgraded_store = Store::new(pool.clone());
    assert_eq!(
        upgraded_store.get_project(&project.id).await.unwrap(),
        project_before
    );
    assert_eq!(
        upgraded_store
            .get_revision(&project.id, &revision.id)
            .await
            .unwrap(),
        revision
    );
    let hash_after: String = sqlx::query_scalar("SELECT content_sha256 FROM revisions WHERE id=?")
        .bind(&revision.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(hash_after, hash_before);
    assert_eq!(upgraded_store.list_models().await.unwrap().len(), 2);
    let defaults = upgraded_store.get_model_settings().await.unwrap();
    assert_eq!(defaults.storyboard_planning.unwrap().model_id, "planner-v1");
    assert_eq!(defaults.scene_code_generation.unwrap().model_id, "coder-v1");
    drop(upgraded_store);
    pool.close().await;
    drop(directory);
}

#[tokio::test]
async fn concurrent_writers_with_the_same_parent_have_exactly_one_winner() {
    let database = Database::new().await;
    let store = database.store();
    let project = store.create_project(project_input()).await.unwrap();
    let first = store
        .create_revision(&project.id, revision_input(None))
        .await
        .unwrap();
    let mut left = edit_input(&first);
    let mut right = edit_input(&first);
    left.content.storyboard.hook = "Pilihan kiri".into();
    right.content.storyboard.hook = "Pilihan kanan".into();
    let other = database.store();
    let (left, right) = tokio::join!(
        store.create_revision(&project.id, left),
        other.create_revision(&project.id, right)
    );
    let winner = match (left, right) {
        (Ok(winner), Err(StoreError::StaleRevision))
        | (Err(StoreError::StaleRevision), Ok(winner)) => winner,
        results => panic!("exactly one write must win and one must be stale: {results:?}"),
    };
    assert_eq!(winner.version, 2);
    assert_eq!(
        store
            .get_project(&project.id)
            .await
            .unwrap()
            .current_revision_id,
        Some(winner.id)
    );
    assert_eq!(
        store
            .list_revisions(&project.id, ListQuery::default())
            .await
            .unwrap()
            .items
            .len(),
        2
    );
    assert!(matches!(
        store
            .create_revision(&project.id, revision_input(None))
            .await,
        Err(StoreError::StaleRevision)
    ));
    assert_eq!(
        store.get_revision(&project.id, &first.id).await.unwrap(),
        first
    );
    drop(store);
    database.close().await;
}

#[tokio::test]
async fn invalid_revision_inputs_roll_back_every_observable_write() {
    let database = Database::new().await;
    let store = database.store();
    let project = store.create_project(project_input()).await.unwrap();
    let first = store
        .create_revision(&project.id, revision_input(None))
        .await
        .unwrap();
    let foreign_project = store.create_project(project_input()).await.unwrap();
    let foreign_revision = store
        .create_revision(&foreign_project.id, revision_input(None))
        .await
        .unwrap();
    let original_project = store.get_project(&project.id).await.unwrap();
    let base = edit_input(&first);
    let mut invalid = Vec::new();
    let mut order = base.clone();
    order.content.storyboard.scenes[1].order = 0;
    invalid.push(order);
    let mut gap = base.clone();
    gap.content.storyboard.scenes[1].order = 2;
    invalid.push(gap);
    let mut duration = base.clone();
    duration.content.storyboard.scenes[0].duration_ms -= 1;
    invalid.push(duration);
    let mut duplicate = base.clone();
    duplicate.content.storyboard.scenes[1].id = duplicate.content.storyboard.scenes[0].id.clone();
    invalid.push(duplicate);
    let mut malformed = base.clone();
    malformed.content.storyboard.scenes[0].id = Some("not-a-uuid".into());
    invalid.push(malformed);
    let mut foreign_scene = base.clone();
    foreign_scene.content.storyboard.scenes[0].id =
        Some(foreign_revision.content.storyboard.scenes[0].id.clone());
    invalid.push(foreign_scene);
    let mut invented = base.clone();
    invented.content.storyboard.scenes[0].id = Some(ABSENT.into());
    invalid.push(invented);
    for input in invalid {
        assert_validation(
            store.create_revision(&project.id, input).await.unwrap_err(),
            "invalid_storyboard",
        );
        assert_eq!(
            store.get_project(&project.id).await.unwrap(),
            original_project
        );
        assert_eq!(
            store
                .list_revisions(&project.id, ListQuery::default())
                .await
                .unwrap()
                .items,
            vec![first.clone()]
        );
    }
    let mut cross_parent = base.clone();
    cross_parent.expected_revision_id = Some(foreign_revision.id.clone());
    assert!(matches!(
        store.create_revision(&project.id, cross_parent).await,
        Err(StoreError::StaleRevision)
    ));
    assert!(matches!(
        store.get_revision(&project.id, &foreign_revision.id).await,
        Err(StoreError::NotFound)
    ));
    assert_eq!(
        store.get_project(&project.id).await.unwrap(),
        original_project
    );
    let second = store.create_revision(&project.id, base).await.unwrap();
    assert_eq!(
        second.version, 2,
        "rejected writes must not consume a version"
    );
    assert_eq!(
        store.get_revision(&project.id, &first.id).await.unwrap(),
        first
    );
    drop(store);
    database.close().await;
}

#[tokio::test]
async fn scene_ids_are_retained_allocated_and_never_resurrected() {
    let database = Database::new().await;
    let store = database.store();
    let project = store.create_project(project_input()).await.unwrap();
    let first = store
        .create_revision(&project.id, revision_input(None))
        .await
        .unwrap();
    let retained = first.content.storyboard.scenes[0].id.clone();
    let removed = first.content.storyboard.scenes[1].id.clone();
    let mut edit = edit_input(&first);
    edit.content.storyboard.scenes[1].id = None;
    let second = store.create_revision(&project.id, edit).await.unwrap();
    assert_eq!(second.content.storyboard.scenes[0].id, retained);
    let allocated = &second.content.storyboard.scenes[1].id;
    assert_id(allocated);
    assert_ne!(allocated, &removed);
    assert_ne!(allocated, &retained);
    let mut resurrect = edit_input(&second);
    resurrect.content.storyboard.scenes[1].id = Some(removed);
    assert_validation(
        store
            .create_revision(&project.id, resurrect)
            .await
            .unwrap_err(),
        "invalid_storyboard",
    );
    let mut expected_revisions = vec![first, second];
    expected_revisions.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    assert_eq!(
        store
            .list_revisions(&project.id, ListQuery::default())
            .await
            .unwrap()
            .items,
        expected_revisions
    );
    drop(store);
    database.close().await;
}

#[test]
fn canonical_jcs_content_hash_has_an_independent_known_vector() {
    let mut content: Value = serde_json::from_str::<Value>(REVISION).unwrap()["content"].clone();
    content["storyboard"]["scenes"][0]["id"] = json!("00000000-0000-4000-8000-000000000001");
    content["storyboard"]["scenes"][1]["id"] = json!("00000000-0000-4000-8000-000000000002");
    let typed: RevisionContent = serde_json::from_value(content.clone()).unwrap();
    let expected = "6151364c7fa0796e5f7213b77e0543848f175004bd30abd9c83c03066f2f340c";
    // Fixed independently using sorted compact UTF-8 JSON. This fixture uses only
    // integral numbers, so its canonical bytes also have no float-format ambiguity.
    assert_eq!(
        format!("{:x}", Sha256::digest(serde_jcs::to_vec(&content).unwrap())),
        expected
    );
    assert_eq!(
        format!("{:x}", Sha256::digest(serde_jcs::to_vec(&typed).unwrap())),
        expected
    );
}

#[tokio::test]
async fn stored_hash_covers_the_expected_frozen_input_not_record_metadata() {
    let database = Database::new().await;
    let store = database.store();
    let project = store.create_project(project_input()).await.unwrap();
    let first = store
        .create_revision(&project.id, revision_input(None))
        .await
        .unwrap();
    let mut expected: Value = serde_json::from_str::<Value>(REVISION).unwrap()["content"].clone();
    for (index, scene) in first.content.storyboard.scenes.iter().enumerate() {
        expected["storyboard"]["scenes"][index]["id"] = json!(scene.id);
    }
    let expected_content: RevisionContent = serde_json::from_value(expected).unwrap();
    assert_eq!(first.content, expected_content);
    let expected_hash = format!(
        "{:x}",
        Sha256::digest(serde_jcs::to_vec(&expected_content).unwrap())
    );
    assert_eq!(first.content_sha256, expected_hash);
    let second = store
        .create_revision(&project.id, edit_input(&first))
        .await
        .unwrap();
    assert_ne!(second.id, first.id);
    assert_eq!(second.version, 2);
    assert_eq!(
        second.content_sha256, expected_hash,
        "revision metadata is outside the content hash"
    );
    drop(store);
    database.close().await
}

#[tokio::test]
async fn invalid_commands_and_list_cursors_return_structured_errors() {
    let database = Database::new().await;
    let store = database.store();
    let mut invalid = project_input();
    invalid.schema_version = "2".into();
    assert_validation(
        store.create_project(invalid).await.unwrap_err(),
        "unsupported_schema_version",
    );
    assert!(
        store
            .list_projects(ListQuery::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let project = store.create_project(project_input()).await.unwrap();
    assert_validation(
        store
            .list_projects(ListQuery {
                limit: None,
                cursor: Some("not-a-cursor".into()),
            })
            .await
            .unwrap_err(),
        "invalid_cursor",
    );
    assert_validation(
        store
            .list_revisions(
                &project.id,
                ListQuery {
                    limit: None,
                    cursor: Some("not-a-cursor".into()),
                },
            )
            .await
            .unwrap_err(),
        "invalid_cursor",
    );
    let mut input = revision_input(None);
    input.content.schema_version = "2".into();
    assert_validation(
        store.create_revision(&project.id, input).await.unwrap_err(),
        "unsupported_schema_version",
    );
    assert_eq!(
        store
            .get_project(&project.id)
            .await
            .unwrap()
            .current_revision_id,
        None
    );
    assert!(
        store
            .list_revisions(&project.id, ListQuery::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(matches!(
        store.create_revision(ABSENT, revision_input(None)).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.get_revision(&project.id, ABSENT).await,
        Err(StoreError::NotFound)
    ));
    drop(store);
    database.close().await;
}

#[tokio::test]
async fn model_registry_precedence_persists_and_never_rewrites_revisions() {
    use scenivox::domain::{
        ModelSelection, Parameters, RegisterModel, ResolveModelSelection, Selection,
        SetModelSettings,
    };
    let database = Database::new().await;
    let store = database.store();
    let listed = store.list_models().await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(
        listed
            .iter()
            .all(|m| !m.remote && m.availability == "available")
    );
    let original_defaults = store.get_model_settings().await.unwrap();
    let project = store.create_project(project_input()).await.unwrap();
    let revision = store
        .create_revision(&project.id, revision_input(None))
        .await
        .unwrap();

    store
        .register_model(RegisterModel {
            schema_version: "1".into(),
            provider_id: "mock".into(),
            model_id: "planner-alt".into(),
            adapter_version: "1".into(),
            task_roles: vec!["storyboard_planning".into()],
            parameters: Parameters {
                temperature: 0.0,
                seed: Some(0),
            },
            enabled: true,
            configured: true,
            availability: "available".into(),
            reason: "offline test model".into(),
            remote: false,
        })
        .await
        .unwrap();

    let bad_parameters = RegisterModel {
        schema_version: "1".into(),
        provider_id: "mock".into(),
        model_id: "bad-params".into(),
        adapter_version: "1".into(),
        task_roles: vec!["storyboard_planning".into()],
        parameters: Parameters {
            temperature: 2.1,
            seed: None,
        },
        enabled: true,
        configured: true,
        availability: "available".into(),
        reason: String::new(),
        remote: false,
    };
    assert_validation(
        store.register_model(bad_parameters).await.unwrap_err(),
        "unsupported_parameter",
    );
    store
        .register_model(RegisterModel {
            schema_version: "1".into(),
            provider_id: "remote-example".into(),
            model_id: "remote-v1".into(),
            adapter_version: "1".into(),
            task_roles: vec!["storyboard_planning".into()],
            parameters: Parameters {
                temperature: 0.0,
                seed: None,
            },
            enabled: true,
            configured: true,
            availability: "unknown".into(),
            reason: "metadata only; no probe".into(),
            remote: true,
        })
        .await
        .unwrap();
    let remote_rejected = store
        .set_model_settings(SetModelSettings {
            schema_version: "1".into(),
            model_selection: ModelSelection {
                storyboard_planning: Some(Selection {
                    provider_id: "remote-example".into(),
                    model_id: "remote-v1".into(),
                }),
                scene_code_generation: Some(Selection {
                    provider_id: "mock".into(),
                    model_id: "coder-v1".into(),
                }),
            },
        })
        .await
        .unwrap_err();
    assert_validation(remote_rejected, "provider_unavailable");
    let updated_defaults = ModelSelection {
        storyboard_planning: Some(Selection {
            provider_id: "mock".into(),
            model_id: "planner-alt".into(),
        }),
        scene_code_generation: Some(Selection {
            provider_id: "mock".into(),
            model_id: "coder-v1".into(),
        }),
    };
    store
        .set_model_settings(SetModelSettings {
            schema_version: "1".into(),
            model_selection: updated_defaults.clone(),
        })
        .await
        .unwrap();

    let project_override = ModelSelection {
        storyboard_planning: Some(Selection {
            provider_id: "mock".into(),
            model_id: "planner-v1".into(),
        }),
        scene_code_generation: None,
    };
    store
        .set_project_model_selection(
            &project.id,
            SetModelSettings {
                schema_version: "1".into(),
                model_selection: project_override,
            },
        )
        .await
        .unwrap();
    let run_override = ModelSelection {
        storyboard_planning: None,
        scene_code_generation: Some(Selection {
            provider_id: "mock".into(),
            model_id: "coder-v1".into(),
        }),
    };
    let resolved = store
        .resolve_model_selection(ResolveModelSelection {
            schema_version: "1".into(),
            project_id: Some(project.id.clone()),
            model_selection: run_override,
        })
        .await
        .unwrap();
    assert_eq!(
        resolved
            .model_selection
            .storyboard_planning
            .unwrap()
            .model_id,
        "planner-v1"
    );
    assert_eq!(
        resolved
            .model_selection
            .scene_code_generation
            .unwrap()
            .model_id,
        "coder-v1"
    );

    let invalid = store
        .resolve_model_selection(ResolveModelSelection {
            schema_version: "1".into(),
            project_id: Some(project.id.clone()),
            model_selection: ModelSelection {
                storyboard_planning: Some(Selection {
                    provider_id: "mock".into(),
                    model_id: "coder-v1".into(),
                }),
                ..ModelSelection::default()
            },
        })
        .await
        .unwrap_err();
    assert_validation(invalid, "unsupported_capability");

    let url = format!(
        "sqlite://{}?mode=rw",
        database
            .directory
            .path()
            .join("persistence.db")
            .to_string_lossy()
            .replace('\\', "/")
    );
    database.pool.close().await;
    let reopened = db::connect(&url).await.unwrap();
    let reopened_store = Store::new(reopened.clone());
    assert_eq!(
        reopened_store.get_model_settings().await.unwrap(),
        updated_defaults
    );
    assert_eq!(reopened_store.list_models().await.unwrap().len(), 4);
    assert_eq!(
        reopened_store
            .get_revision(&project.id, &revision.id)
            .await
            .unwrap(),
        revision
    );
    assert_ne!(
        original_defaults,
        reopened_store.get_model_settings().await.unwrap()
    );
    drop(reopened_store);
    reopened.close().await;
    database.directory.close().unwrap();
}
