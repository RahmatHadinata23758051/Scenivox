use scenivox::{
    db,
    domain::{
        CreateProject, ListQuery, ModelSelection, Parameters, PlanStoryboard, PlannedStoryboard,
        RegisterModel, Selection,
    },
    planner::{DeterministicMockPlanner, Planner, PlannerError},
    store::{Store, StoreError},
};
use sqlx::SqlitePool;
use tempfile::TempDir;

const PROJECT: &str = include_str!("fixtures/p1-project.json");

struct Database {
    _directory: TempDir,
    pool: SqlitePool,
}

impl Database {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory
                .path()
                .join("planner.db")
                .to_string_lossy()
                .replace('\\', "/")
        );
        let pool = db::connect(&url).await.unwrap();
        Self {
            _directory: directory,
            pool,
        }
    }

    fn store(&self) -> Store {
        Store::new(self.pool.clone())
    }
}

struct InvalidPlanner;
impl Planner for InvalidPlanner {
    fn plan(&self, input: &PlanStoryboard) -> Result<PlannedStoryboard, PlannerError> {
        let mut planned = DeterministicMockPlanner.plan(input)?;
        planned.storyboard.scenes[0].duration_ms += 1;
        Ok(planned)
    }
}

#[tokio::test]
async fn planning_freezes_run_selection_and_is_stale_safe_and_atomic() {
    let database = Database::new().await;
    let store = database.store();
    let project: CreateProject = serde_json::from_str(PROJECT).unwrap();
    let project = store.create_project(project).await.unwrap();
    store
        .register_model(RegisterModel {
            schema_version: "1".into(),
            provider_id: "mock".into(),
            model_id: "planner-run-override".into(),
            adapter_version: "1".into(),
            task_roles: vec!["storyboard_planning".into()],
            parameters: Parameters {
                temperature: 0.0,
                seed: Some(7),
            },
            enabled: true,
            configured: true,
            availability: "available".into(),
            reason: "offline acceptance test".into(),
            remote: false,
        })
        .await
        .unwrap();
    let input = PlanStoryboard {
        schema_version: "1".into(),
        idea: "A small seed becomes a garden".into(),
        target: project.target.clone(),
        expected_revision_id: None,
        model_selection: ModelSelection {
            storyboard_planning: Some(Selection {
                provider_id: "mock".into(),
                model_id: "planner-run-override".into(),
            }),
            scene_code_generation: None,
        },
    };
    let revision = store
        .plan_storyboard(&project.id, None, input.clone(), &DeterministicMockPlanner)
        .await
        .unwrap();
    assert_eq!(
        revision
            .content
            .storyboard
            .scenes
            .iter()
            .map(|scene| scene.duration_ms as u64)
            .sum::<u64>(),
        revision.content.target.duration_ms as u64,
    );
    let provenance = revision.content.planning_provenance.as_ref().unwrap();
    assert_eq!(provenance.source, "deterministic_mock");
    assert_eq!(
        revision.content.models.storyboard_planning.model_id,
        "planner-run-override"
    );
    assert_eq!(provenance.prompt_input.model_id, "planner-run-override");
    assert_eq!(provenance.prompt_input.idea, revision.content.idea);
    assert_eq!(provenance.prompt_input.target, revision.content.target);
    assert_eq!(provenance.scene_prompt_input.idea, revision.content.idea);
    assert_eq!(
        provenance.scene_prompt_sha256,
        revision.content.models.scene_code_generation.prompt_sha256
    );
    for (planned, stored) in provenance
        .scene_prompt_input
        .storyboard
        .scenes
        .iter()
        .zip(&revision.content.storyboard.scenes)
    {
        assert_eq!(planned.id, None);
        assert_eq!(planned.order, stored.order);
        assert_eq!(planned.duration_ms, stored.duration_ms);
        assert_eq!(planned.visual_direction, stored.visual_direction);
        assert_eq!(planned.camera, stored.camera);
        assert_eq!(planned.action, stored.action);
        assert_eq!(planned.narration, stored.narration);
        assert_eq!(planned.caption_hint, stored.caption_hint);
        assert_eq!(planned.sfx_cues, stored.sfx_cues);
    }
    assert_eq!(
        store.get_revision(&project.id, &revision.id).await.unwrap(),
        revision
    );

    let stale = store
        .plan_storyboard(&project.id, None, input.clone(), &DeterministicMockPlanner)
        .await
        .unwrap_err();
    assert!(matches!(stale, StoreError::StaleRevision));

    let invalid = store
        .plan_storyboard(
            &project.id,
            None,
            PlanStoryboard {
                expected_revision_id: Some(revision.id.clone()),
                ..input
            },
            &InvalidPlanner,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        invalid,
        StoreError::Validation {
            code: "invalid_storyboard",
            ..
        }
    ));
    assert_eq!(
        store
            .get_project(&project.id)
            .await
            .unwrap()
            .current_revision_id
            .as_deref(),
        Some(revision.id.as_str())
    );
    assert_eq!(
        store
            .list_revisions(&project.id, ListQuery::default())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    database.pool.close().await;
}
