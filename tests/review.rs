use scenivox::{
    db,
    domain::{CreateProject, CreateRevision, ReviewCommand, ReviewStatus},
    store::{Store, StoreError},
};
use sha2::Digest;
use sqlx::SqlitePool;
use tempfile::TempDir;

const PROJECT: &str = include_str!("fixtures/p1-project.json");
const REVISION: &str = include_str!("fixtures/p1-revision.json");

struct Database {
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
                .join("review.db")
                .to_string_lossy()
                .replace('\\', "/")
        );
        let pool = db::connect(&url).await.unwrap();
        Self { directory, pool }
    }
    fn store(&self) -> Store {
        Store::new(self.pool.clone())
    }
    fn url(&self) -> String {
        format!(
            "sqlite://{}?mode=rw",
            self.directory
                .path()
                .join("review.db")
                .to_string_lossy()
                .replace('\\', "/")
        )
    }
}

fn review(hash: &str) -> ReviewCommand {
    ReviewCommand {
        schema_version: "1".into(),
        content_sha256: hash.into(),
    }
}
fn expect_conflict(error: StoreError, code: &str) {
    assert!(
        matches!(error, StoreError::Conflict { code: actual } if actual == code),
        "{error:?}"
    );
}

async fn project_revision(
    store: &Store,
) -> (scenivox::domain::Project, scenivox::domain::Revision) {
    let project: CreateProject = serde_json::from_str(PROJECT).unwrap();
    let project = store.create_project(project).await.unwrap();
    let command: CreateRevision = serde_json::from_str(REVISION).unwrap();
    let revision = store.create_revision(&project.id, command).await.unwrap();
    (project, revision)
}

#[tokio::test]
async fn review_approval_transitions_conflicts_and_concurrent_replay() {
    let database = Database::new().await;
    let store = database.store();
    let (project, revision) = project_revision(&store).await;
    let command = review(&revision.content_sha256);

    expect_conflict(
        store
            .approve_revision(&project.id, &revision.id, None, command.clone())
            .await
            .unwrap_err(),
        "invalid_transition",
    );
    expect_conflict(
        store
            .review_revision(&project.id, &revision.id, None, review(&"0".repeat(64)))
            .await
            .unwrap_err(),
        "hash_mismatch",
    );
    assert!(matches!(
        store
            .review_revision(
                "00000000-0000-4000-8000-000000000099",
                &revision.id,
                None,
                command.clone()
            )
            .await
            .unwrap_err(),
        StoreError::NotFound
    ));
    assert!(matches!(
        store
            .review_revision(&project.id, &revision.id, None, review(&"0".repeat(64)))
            .await
            .unwrap_err(),
        StoreError::Conflict {
            code: "hash_mismatch"
        }
    ));

    let in_review = store
        .review_revision(&project.id, &revision.id, None, command.clone())
        .await
        .unwrap();
    assert_eq!(in_review.review_state, ReviewStatus::InReview);
    assert_eq!(
        store
            .review_revision(&project.id, &revision.id, None, command.clone())
            .await
            .unwrap(),
        in_review
    );
    let withdrawn = store
        .withdraw_review(&project.id, &revision.id, None, command.clone())
        .await
        .unwrap();
    assert_eq!(withdrawn.review_state, ReviewStatus::Draft);
    assert_eq!(
        store
            .withdraw_review(&project.id, &revision.id, None, command.clone())
            .await
            .unwrap(),
        withdrawn
    );
    expect_conflict(
        store
            .withdraw_review(&project.id, &revision.id, None, review(&"0".repeat(64)))
            .await
            .unwrap_err(),
        "hash_mismatch",
    );
    expect_conflict(
        store
            .approve_revision(&project.id, &revision.id, None, command.clone())
            .await
            .unwrap_err(),
        "invalid_transition",
    );

    store
        .review_revision(&project.id, &revision.id, None, command.clone())
        .await
        .unwrap();
    let (first, second) = tokio::join!(
        store.approve_revision(&project.id, &revision.id, None, command.clone()),
        store.approve_revision(&project.id, &revision.id, None, command.clone()),
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first.0, second.0);
    assert!(!first.1 && !second.1);
    assert_ne!(first.2, second.2);
    assert_eq!(
        store
            .get_project(&project.id)
            .await
            .unwrap()
            .approved_revision_id
            .as_deref(),
        Some(revision.id.as_str())
    );
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM approvals WHERE project_id=? AND revision_id=?")
            .bind(&project.id)
            .bind(&revision.id)
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);

    let immutable_update = sqlx::query("UPDATE revisions SET content_json='{}' WHERE id=?")
        .bind(&revision.id)
        .execute(&database.pool)
        .await;
    assert!(
        immutable_update.is_err(),
        "approval cannot mutate immutable revision content"
    );
    let immutable_approval_update = sqlx::query("UPDATE approvals SET content_sha256=? WHERE id=?")
        .bind("0".repeat(64))
        .bind(&first.0.id)
        .execute(&database.pool)
        .await;
    assert!(immutable_approval_update.is_err());
    let immutable_approval_delete = sqlx::query("DELETE FROM approvals WHERE id=?")
        .bind(&first.0.id)
        .execute(&database.pool)
        .await;
    assert!(immutable_approval_delete.is_err());
    let changed = store
        .create_revision(
            &project.id,
            CreateRevision {
                schema_version: "1".into(),
                expected_revision_id: Some(revision.id.clone()),
                content: serde_json::from_value(serde_json::to_value(&revision.content).unwrap())
                    .unwrap(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        store
            .approve_revision(&project.id, &revision.id, None, command.clone())
            .await
            .unwrap_err(),
        StoreError::StaleRevision
    ));
    assert_eq!(changed.version, 2);
    assert_eq!(
        store
            .get_project(&project.id)
            .await
            .unwrap()
            .approved_revision_id,
        None
    );
    assert_eq!(
        store.get_revision(&project.id, &revision.id).await.unwrap(),
        revision
    );
    assert_eq!(
        store.get_approval(&project.id, &first.0.id).await.unwrap(),
        first.0
    );

    let reopened = db::connect(&database.url()).await.unwrap();
    let reopened_store = Store::new(reopened.clone());
    assert_eq!(
        reopened_store
            .get_project(&project.id)
            .await
            .unwrap()
            .approved_revision_id,
        None
    );
    assert_eq!(
        reopened_store
            .get_approval(&project.id, &first.0.id)
            .await
            .unwrap(),
        first.0
    );
    let state: String = sqlx::query_scalar(
        "SELECT review_state FROM revision_reviews WHERE project_id=? AND revision_id=?",
    )
    .bind(&project.id)
    .bind(&revision.id)
    .fetch_one(&reopened)
    .await
    .unwrap();
    assert_eq!(state, "approved");
    assert_eq!(
        reopened_store
            .get_revision(&project.id, &changed.id)
            .await
            .unwrap(),
        changed
    );
    reopened.close().await;
    database.pool.close().await;
}

#[tokio::test]
async fn scene_edits_refresh_prompt_hash_mapping_and_keep_model_snapshots() {
    use scenivox::{
        domain::{ModelSelection, PlanStoryboard},
        planner::DeterministicMockPlanner,
    };

    let database = Database::new().await;
    let store = database.store();
    let project_input: CreateProject = serde_json::from_str(PROJECT).unwrap();
    let project = store.create_project(project_input).await.unwrap();
    let plan = PlanStoryboard {
        schema_version: "1".into(),
        idea: "A seed grows".into(),
        target: project.target.clone(),
        expected_revision_id: None,
        model_selection: ModelSelection::default(),
    };
    let original = store
        .plan_storyboard(&project.id, None, plan, &DeterministicMockPlanner)
        .await
        .unwrap();
    let mut input: CreateRevision = serde_json::from_value(serde_json::json!({
        "schema_version":"1",
        "expected_revision_id":original.id,
        "content":original.content
    }))
    .unwrap();
    input.content.storyboard.scenes[0]
        .action
        .push_str(" revised");
    let child = store.create_revision(&project.id, input).await.unwrap();
    let provenance = child.content.planning_provenance.as_ref().unwrap();
    let scene_sha = format!(
        "{:x}",
        sha2::Sha256::digest(serde_jcs::to_vec(&provenance.scene_prompt_input).unwrap())
    );
    assert_eq!(provenance.scene_prompt_sha256, scene_sha);
    assert_eq!(
        child.content.models.storyboard_planning,
        original.content.models.storyboard_planning
    );
    assert_eq!(
        child.content.models.scene_code_generation.prompt_sha256,
        scene_sha
    );
    assert_eq!(
        provenance.prompt_input,
        original
            .content
            .planning_provenance
            .as_ref()
            .unwrap()
            .prompt_input
    );
    let mapping = provenance.scene_id_mapping.as_ref().unwrap();
    for (prompt_scene, (scene, map)) in provenance
        .scene_prompt_input
        .storyboard
        .scenes
        .iter()
        .zip(child.content.storyboard.scenes.iter().zip(mapping))
    {
        assert_eq!(prompt_scene.id, None);
        assert_eq!(map.order, scene.order);
        assert_eq!(map.scene_id, scene.id);
    }
    assert_eq!(
        store.get_revision(&project.id, &original.id).await.unwrap(),
        original
    );
    database.pool.close().await;
}
