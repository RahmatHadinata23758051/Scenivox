//! Shared local persistence. No providers, asset execution, or approval workflow.
use crate::domain::*;
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqliteConnection, SqlitePool, sqlite::SqliteRow};
use std::collections::HashSet;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not found")]
    NotFound,
    #[error("stale revision")]
    StaleRevision,
    #[error("idempotency conflict")]
    IdempotencyConflict,
    #[error("validation: {code} at {path}")]
    Validation { code: &'static str, path: String },
    #[error("database unavailable")]
    Database(#[from] sqlx::Error),
    #[error("internal error")]
    Internal,
}
impl StoreError {
    pub fn code(&self) -> &str {
        match self {
            Self::NotFound => "not_found",
            Self::StaleRevision => "stale_revision",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::Validation { code, .. } => code,
            Self::Database(_) => "database_unavailable",
            Self::Internal => "internal_error",
        }
    }
}
type Result<T, E = StoreError> = std::result::Result<T, E>;
fn invalid(code: &'static str, path: &str) -> StoreError {
    StoreError::Validation {
        code,
        path: path.into(),
    }
}
fn check(ok: bool, code: &'static str, path: &str) -> Result<()> {
    if ok { Ok(()) } else { Err(invalid(code, path)) }
}
fn version(v: &str) -> Result<()> {
    check(v == "1", "unsupported_schema_version", "/schema_version")
}
fn text(v: &str, max: usize, path: &str) -> Result<()> {
    check(
        !v.trim().is_empty() && v.chars().count() <= max,
        "invalid_schema",
        path,
    )
}
fn name(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 128
        && v.bytes()
            .enumerate()
            .all(|(i, c)| c.is_ascii_alphanumeric() || (i > 0 && matches!(c, b'.' | b'_' | b'-')))
}
fn hash(v: &str) -> bool {
    v.len() == 64
        && v.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn id(v: &str) -> bool {
    Uuid::parse_str(v).is_ok_and(|u| u.to_string() == v)
}
fn path_id(v: &str) -> Result<()> {
    check(id(v), "invalid_schema", "/id")
}
fn target(t: &Target) -> Result<()> {
    check(
        (1..=60000).contains(&t.duration_ms)
            && t.aspect_ratio == "9:16"
            && t.fps == 30
            && t.language == "id-ID"
            && name(&t.style_id),
        "invalid_schema",
        "/target",
    )
}
fn selection(s: &ModelSelection) -> Result<()> {
    for s in [&s.storyboard_planning, &s.scene_code_generation]
        .into_iter()
        .flatten()
    {
        check(
            name(&s.provider_id) && name(&s.model_id),
            "invalid_model_selection",
            "/model_selection",
        )?;
    }
    Ok(())
}

fn model_parameters(p: &Parameters) -> Result<()> {
    check(
        p.temperature.is_finite()
            && (0.0..=2.0).contains(&p.temperature)
            && p.seed.is_none_or(|seed| seed <= 2_147_483_647),
        "unsupported_parameter",
        "/parameters",
    )
}

fn validate_model(reg: &ModelRecord, role: &str) -> Result<()> {
    check(
        name(&reg.provider_id) && name(&reg.model_id) && name(&reg.adapter_version),
        "invalid_model_selection",
        "/model_selection",
    )?;
    check(
        reg.task_roles.iter().any(|cap| cap == role),
        "unsupported_capability",
        "/model_selection",
    )?;
    check(
        reg.enabled && reg.configured && reg.availability == "available" && !reg.remote,
        "provider_unavailable",
        "/model_selection",
    )
}

fn model_pair<'a>(selection: &'a ModelSelection, role: &str) -> Option<&'a Selection> {
    match role {
        "storyboard_planning" => selection.storyboard_planning.as_ref(),
        "scene_code_generation" => selection.scene_code_generation.as_ref(),
        _ => None,
    }
}

fn set_model_pair(selection: &mut ModelSelection, role: &str, value: Selection) {
    match role {
        "storyboard_planning" => selection.storyboard_planning = Some(value),
        "scene_code_generation" => selection.scene_code_generation = Some(value),
        _ => unreachable!(),
    }
}
fn json<T: Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|_| StoreError::Internal)
}
fn id_independent_storyboard(storyboard: &StoryboardInput) -> StoryboardInput {
    let mut storyboard = storyboard.clone();
    for scene in &mut storyboard.scenes {
        scene.id = None;
    }
    storyboard
}

fn scene_prompt_input(
    idea: &str,
    target: &Target,
    storyboard: &StoryboardInput,
    model: &ModelRecord,
) -> ScenePromptInput {
    ScenePromptInput {
        schema_version: "scenivox.scene_generation.v1".into(),
        provider_id: model.provider_id.clone(),
        model_id: model.model_id.clone(),
        adapter_version: model.adapter_version.clone(),
        idea: idea.into(),
        target: target.clone(),
        storyboard: id_independent_storyboard(storyboard),
    }
}
fn decode<T: DeserializeOwned>(v: &str) -> Result<T> {
    serde_json::from_str(v).map_err(|_| StoreError::Internal)
}
fn canonical<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    serde_jcs::to_vec(v).map_err(|_| StoreError::Internal)
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn project(row: SqliteRow) -> Result<Project> {
    Ok(Project {
        schema_version: "1".into(),
        id: row.try_get("id")?,
        title: row.try_get("title")?,
        idea: row.try_get("idea")?,
        target: decode(row.try_get("target_json")?)?,
        model_selection: decode(row.try_get("model_selection_json")?)?,
        current_revision_id: row.try_get("current_revision_id")?,
        approved_revision_id: row.try_get("approved_revision_id")?,
    })
}
fn revision(row: SqliteRow) -> Result<Revision> {
    Ok(Revision {
        schema_version: "1".into(),
        id: row.try_get("id")?,
        project_id: row.try_get("project_id")?,
        parent_revision_id: row.try_get("parent_revision_id")?,
        version: row.try_get("version")?,
        created_at: row.try_get("created_at")?,
        content_sha256: row.try_get("content_sha256")?,
        content: decode(row.try_get("content_json")?)?,
    })
}
async fn now(c: &mut SqliteConnection) -> Result<String> {
    Ok(
        sqlx::query_scalar("SELECT strftime('%Y-%m-%dT%H:%M:%SZ','now')")
            .fetch_one(c)
            .await?,
    )
}
async fn read_project(c: &mut SqliteConnection, pid: &str) -> Result<Project> {
    project(
        sqlx::query("SELECT * FROM projects WHERE id=? AND deleted_at IS NULL")
            .bind(pid)
            .fetch_optional(c)
            .await?
            .ok_or(StoreError::NotFound)?,
    )
}
async fn read_revision(c: &mut SqliteConnection, pid: &str, rid: &str) -> Result<Revision> {
    revision(sqlx::query("SELECT r.* FROM revisions r JOIN projects p ON p.id=r.project_id WHERE r.project_id=? AND r.id=? AND p.deleted_at IS NULL").bind(pid).bind(rid).fetch_optional(c).await?.ok_or(StoreError::NotFound)?)
}
#[derive(Clone, Debug)]
pub struct Store {
    pool: SqlitePool,
}
impl Store {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
    pub async fn create_project(&self, command: CreateProject) -> Result<Project> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let value = insert_project(&mut tx, command).await?;
        tx.commit().await?;
        Ok(value)
    }
    pub async fn create_project_idempotent(
        &self,
        key: &str,
        command: CreateProject,
    ) -> Result<(Project, bool)> {
        path_id(key)?;
        let request = digest(&canonical(&command)?);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(value) = replay(&mut tx, "create_project", "", key, &request).await? {
            tx.commit().await?;
            return Ok((value, true));
        }
        let value = insert_project(&mut tx, command).await?;
        remember(
            &mut tx,
            "create_project",
            "",
            key,
            &value.id,
            &request,
            &value,
        )
        .await?;
        tx.commit().await?;
        Ok((value, false))
    }
    pub async fn get_project(&self, pid: &str) -> Result<Project> {
        path_id(pid)?;
        read_project(&mut *self.pool.acquire().await?, pid).await
    }
    pub async fn update_project(&self, pid: &str, command: UpdateProject) -> Result<Project> {
        version(&command.schema_version)?;
        path_id(pid)?;
        text(&command.title, 200, "/title")?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if sqlx::query("UPDATE projects SET title=? WHERE id=? AND deleted_at IS NULL")
            .bind(command.title)
            .bind(pid)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 0
        {
            return Err(StoreError::NotFound);
        }
        let value = read_project(&mut tx, pid).await?;
        tx.commit().await?;
        Ok(value)
    }
    pub async fn delete_project(&self, pid: &str) -> Result<()> {
        path_id(pid)?;
        if sqlx::query("UPDATE projects SET deleted_at=strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id=? AND deleted_at IS NULL").bind(pid).execute(&self.pool).await?.rows_affected()==0 { return Err(StoreError::NotFound); }
        Ok(())
    }
    pub async fn create_revision(&self, pid: &str, command: CreateRevision) -> Result<Revision> {
        path_id(pid)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let value = insert_revision(&mut tx, pid, command).await?;
        tx.commit().await?;
        Ok(value)
    }
    pub async fn create_revision_idempotent(
        &self,
        pid: &str,
        key: &str,
        command: CreateRevision,
    ) -> Result<(Revision, bool)> {
        path_id(pid)?;
        path_id(key)?;
        let request = digest(&canonical(&command)?);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        read_project(&mut tx, pid).await?;
        if let Some(value) = replay(&mut tx, "create_revision", pid, key, &request).await? {
            tx.commit().await?;
            return Ok((value, true));
        }
        let value = insert_revision(&mut tx, pid, command).await?;
        remember(&mut tx, "create_revision", pid, key, pid, &request, &value).await?;
        tx.commit().await?;
        Ok((value, false))
    }
    pub async fn get_revision(&self, pid: &str, rid: &str) -> Result<Revision> {
        path_id(pid)?;
        path_id(rid)?;
        read_revision(&mut *self.pool.acquire().await?, pid, rid).await
    }
    pub async fn list_projects(&self, q: ListQuery) -> Result<Page<Project>> {
        let limit = list_limit(&q)?;
        let mut c = self.pool.acquire().await?;
        let anchor = cursor(&mut c, None, q.cursor.as_deref()).await?;
        let rows=sqlx::query("SELECT * FROM projects WHERE deleted_at IS NULL AND (? IS NULL OR (created_at,id)>(?,?)) ORDER BY created_at,id LIMIT ?").bind(q.cursor.as_deref()).bind(&anchor).bind(q.cursor.as_deref()).bind(limit+1).fetch_all(&mut *c).await?;
        let mut items = rows.into_iter().map(project).collect::<Result<Vec<_>>>()?;
        let next = if items.len() > limit as usize {
            items.pop();
            items.last().map(|p| p.id.clone())
        } else {
            None
        };
        Ok(Page {
            schema_version: "1".into(),
            items,
            next_cursor: next,
        })
    }
    pub async fn list_revisions(&self, pid: &str, q: ListQuery) -> Result<Page<Revision>> {
        path_id(pid)?;
        let limit = list_limit(&q)?;
        let mut c = self.pool.acquire().await?;
        read_project(&mut c, pid).await?;
        let anchor = cursor(&mut c, Some(pid), q.cursor.as_deref()).await?;
        let rows=sqlx::query("SELECT * FROM revisions WHERE project_id=? AND (? IS NULL OR (created_at,id)>(?,?)) ORDER BY created_at,id LIMIT ?").bind(pid).bind(q.cursor.as_deref()).bind(&anchor).bind(q.cursor.as_deref()).bind(limit+1).fetch_all(&mut *c).await?;
        let mut items = rows.into_iter().map(revision).collect::<Result<Vec<_>>>()?;
        let next = if items.len() > limit as usize {
            items.pop();
            items.last().map(|r| r.id.clone())
        } else {
            None
        };
        Ok(Page {
            schema_version: "1".into(),
            items,
            next_cursor: next,
        })
    }

    pub async fn list_models(&self) -> Result<Vec<ModelRecord>> {
        let rows = sqlx::query("SELECT * FROM model_registry ORDER BY provider_id,model_id")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ModelRecord {
                    schema_version: "1".into(),
                    provider_id: row.try_get("provider_id")?,
                    model_id: row.try_get("model_id")?,
                    adapter_version: row.try_get("adapter_version")?,
                    task_roles: decode(row.try_get("task_roles_json")?)?,
                    parameters: decode(row.try_get("parameters_json")?)?,
                    enabled: row.try_get::<i64, _>("enabled")? != 0,
                    configured: row.try_get::<i64, _>("configured")? != 0,
                    availability: row.try_get("availability")?,
                    reason: row.try_get("reason")?,
                    remote: row.try_get::<i64, _>("remote")? != 0,
                })
            })
            .collect()
    }

    pub async fn register_model(&self, input: RegisterModel) -> Result<ModelRecord> {
        version(&input.schema_version)?;
        check(
            name(&input.provider_id) && name(&input.model_id) && name(&input.adapter_version),
            "invalid_model_selection",
            "/model_selection",
        )?;
        model_parameters(&input.parameters)?;
        check(
            !input.task_roles.is_empty()
                && input
                    .task_roles
                    .iter()
                    .all(|r| matches!(r.as_str(), "storyboard_planning" | "scene_code_generation"))
                && input
                    .task_roles
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    == input.task_roles.len(),
            "unsupported_capability",
            "/task_roles",
        )?;
        check(
            matches!(
                input.availability.as_str(),
                "unknown" | "available" | "unavailable"
            ),
            "invalid_model_selection",
            "/availability",
        )?;
        check(
            !input.remote || input.availability != "available",
            "invalid_model_selection",
            "/availability",
        )?;
        text(&input.reason, 500, "/reason").or_else(|e| {
            if input.reason.is_empty() {
                Ok(())
            } else {
                Err(e)
            }
        })?;
        let model = ModelRecord {
            schema_version: "1".into(),
            provider_id: input.provider_id,
            model_id: input.model_id,
            adapter_version: input.adapter_version,
            task_roles: input.task_roles,
            parameters: input.parameters,
            enabled: input.enabled,
            configured: input.configured,
            availability: input.availability,
            reason: input.reason,
            remote: input.remote,
        };
        sqlx::query("INSERT INTO model_registry(provider_id,model_id,adapter_version,task_roles_json,parameters_json,enabled,configured,availability,reason,remote) VALUES(?,?,?,?,?,?,?,?,?,?) ON CONFLICT(provider_id,model_id) DO UPDATE SET adapter_version=excluded.adapter_version,task_roles_json=excluded.task_roles_json,parameters_json=excluded.parameters_json,enabled=excluded.enabled,configured=excluded.configured,availability=excluded.availability,reason=excluded.reason,remote=excluded.remote")
            .bind(&model.provider_id).bind(&model.model_id).bind(&model.adapter_version).bind(json(&model.task_roles)?).bind(json(&model.parameters)?).bind(model.enabled as i64).bind(model.configured as i64).bind(&model.availability).bind(&model.reason).bind(model.remote as i64).execute(&self.pool).await?;
        Ok(model)
    }

    pub async fn get_model_settings(&self) -> Result<ModelSelection> {
        let raw: String = sqlx::query_scalar("SELECT defaults_json FROM model_settings WHERE id=1")
            .fetch_one(&self.pool)
            .await?;
        decode(&raw)
    }

    pub async fn set_model_settings(&self, input: SetModelSettings) -> Result<ModelSelection> {
        version(&input.schema_version)?;
        selection(&input.model_selection)?;
        let defaults = complete_selection(input.model_selection)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        validate_selection(&mut tx, &defaults).await?;
        sqlx::query("UPDATE model_settings SET defaults_json=? WHERE id=1")
            .bind(json(&defaults)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(defaults)
    }

    pub async fn set_project_model_selection(
        &self,
        pid: &str,
        input: SetModelSettings,
    ) -> Result<Project> {
        version(&input.schema_version)?;
        path_id(pid)?;
        selection(&input.model_selection)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        read_project(&mut tx, pid).await?;
        validate_selection(&mut tx, &input.model_selection).await?;
        sqlx::query("UPDATE projects SET model_selection_json=? WHERE id=? AND deleted_at IS NULL")
            .bind(json(&input.model_selection)?)
            .bind(pid)
            .execute(&mut *tx)
            .await?;
        let project = read_project(&mut tx, pid).await?;
        tx.commit().await?;
        Ok(project)
    }

    pub async fn resolve_model_selection(
        &self,
        input: ResolveModelSelection,
    ) -> Result<EffectiveModelSelection> {
        version(&input.schema_version)?;
        selection(&input.model_selection)?;
        let mut tx = self.pool.begin().await?;
        let effective =
            resolve_selection_in_tx(&mut tx, input.project_id.as_deref(), &input.model_selection)
                .await?;
        tx.commit().await?;
        Ok(EffectiveModelSelection {
            schema_version: "1".into(),
            model_selection: effective,
        })
    }

    pub async fn plan_storyboard(
        &self,
        project_id: &str,
        idempotency_key: Option<&str>,
        input: PlanStoryboard,
        planner: &dyn crate::planner::Planner,
    ) -> Result<Revision> {
        path_id(project_id)?;
        if let Some(key) = idempotency_key {
            path_id(key)?;
        }
        let request_sha256 = digest(&canonical(&input)?);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(key) = idempotency_key {
            if let Some(revision) = replay(
                &mut tx,
                "generate_storyboard",
                project_id,
                key,
                &request_sha256,
            )
            .await?
            {
                tx.commit().await?;
                return Ok(revision);
            }
        }
        version(&input.schema_version)?;
        text(&input.idea, 10000, "/idea")?;
        target(&input.target)?;
        selection(&input.model_selection)?;
        if let Some(expected) = input.expected_revision_id.as_deref() {
            path_id(expected)?;
        }
        let project = read_project(&mut tx, project_id).await?;
        if project.current_revision_id != input.expected_revision_id {
            return Err(StoreError::StaleRevision);
        }
        let effective_selection =
            resolve_selection_in_tx(&mut tx, Some(project_id), &input.model_selection).await?;
        let planning_pair = effective_selection
            .storyboard_planning
            .as_ref()
            .ok_or_else(|| {
                invalid(
                    "invalid_model_selection",
                    "/model_selection/storyboard_planning",
                )
            })?;
        let scene_pair = effective_selection
            .scene_code_generation
            .as_ref()
            .ok_or_else(|| {
                invalid(
                    "invalid_model_selection",
                    "/model_selection/scene_code_generation",
                )
            })?;
        let planning_model = read_model(&mut tx, planning_pair).await?;
        let scene_model = read_model(&mut tx, scene_pair).await?;
        let prompt_input = PlanningPromptInput {
            schema_version: "scenivox.storyboard_planning.v1".into(),
            provider_id: planning_model.provider_id.clone(),
            model_id: planning_model.model_id.clone(),
            adapter_version: planning_model.adapter_version.clone(),
            idea: input.idea.clone(),
            target: input.target.clone(),
        };
        let prompt_sha256 = digest(&canonical(&prompt_input)?);
        let mut planner_input = input.clone();
        planner_input.model_selection = effective_selection;
        let planned = planner
            .plan(&planner_input)
            .map_err(|_| invalid("invalid_storyboard", "/storyboard"))?;
        let scene_prompt = scene_prompt_input(
            &input.idea,
            &input.target,
            &planned.storyboard,
            &scene_model,
        );
        let scene_prompt_sha256 = digest(&canonical(&scene_prompt)?);
        let snapshot = |model: &ModelRecord, prompt_sha256: String| ModelSnapshot {
            provider_id: model.provider_id.clone(),
            model_id: model.model_id.clone(),
            adapter_version: model.adapter_version.clone(),
            parameters: model.parameters.clone(),
            prompt_sha256,
        };
        let command = CreateRevision {
            schema_version: "1".into(),
            expected_revision_id: input.expected_revision_id,
            content: RevisionInput {
                schema_version: "1".into(),
                idea: input.idea,
                target: input.target,
                storyboard: planned.storyboard,
                models: ModelSnapshots {
                    storyboard_planning: snapshot(&planning_model, prompt_sha256.clone()),
                    scene_code_generation: snapshot(&scene_model, scene_prompt_sha256.clone()),
                },
                asset_hashes: Vec::new(),
                skill_id: planned.skill_id,
                skill_version: planned.skill_version,
                planning_provenance: Some(PlanningProvenance {
                    source: "deterministic_mock".into(),
                    prompt_input,
                    prompt_sha256,
                    scene_prompt_input: scene_prompt,
                    scene_prompt_sha256,
                    scene_id_mapping: None,
                }),
            },
        };
        let revision = insert_revision(&mut tx, project_id, command).await?;
        if let Some(key) = idempotency_key {
            remember(
                &mut tx,
                "generate_storyboard",
                project_id,
                key,
                project_id,
                &request_sha256,
                &revision,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(revision)
    }
}
async fn resolve_selection_in_tx(
    c: &mut SqliteConnection,
    project_id: Option<&str>,
    run_selection: &ModelSelection,
) -> Result<ModelSelection> {
    let raw: String = sqlx::query_scalar("SELECT defaults_json FROM model_settings WHERE id=1")
        .fetch_one(&mut *c)
        .await?;
    let defaults: ModelSelection = decode(&raw)?;
    let project_selection = if let Some(project_id) = project_id {
        path_id(project_id)?;
        Some(read_project(c, project_id).await?.model_selection)
    } else {
        None
    };
    let mut effective = ModelSelection::default();
    for role in ["storyboard_planning", "scene_code_generation"] {
        let pair = model_pair(run_selection, role)
            .or_else(|| project_selection.as_ref().and_then(|s| model_pair(s, role)))
            .or_else(|| model_pair(&defaults, role))
            .ok_or_else(|| invalid("invalid_model_selection", "/model_selection"))?;
        validate_pair(c, role, pair).await?;
        set_model_pair(&mut effective, role, pair.clone());
    }
    Ok(effective)
}

async fn read_model(c: &mut SqliteConnection, selection: &Selection) -> Result<ModelRecord> {
    let row = sqlx::query("SELECT * FROM model_registry WHERE provider_id=? AND model_id=?")
        .bind(&selection.provider_id)
        .bind(&selection.model_id)
        .fetch_optional(&mut *c)
        .await?
        .ok_or_else(|| invalid("invalid_model_selection", "/model_selection"))?;
    Ok(ModelRecord {
        schema_version: "1".into(),
        provider_id: row.try_get("provider_id")?,
        model_id: row.try_get("model_id")?,
        adapter_version: row.try_get("adapter_version")?,
        task_roles: decode(row.try_get("task_roles_json")?)?,
        parameters: decode(row.try_get("parameters_json")?)?,
        enabled: row.try_get::<i64, _>("enabled")? != 0,
        configured: row.try_get::<i64, _>("configured")? != 0,
        availability: row.try_get("availability")?,
        reason: row.try_get("reason")?,
        remote: row.try_get::<i64, _>("remote")? != 0,
    })
}
fn complete_selection(selection: ModelSelection) -> Result<ModelSelection> {
    check(
        selection.storyboard_planning.is_some() && selection.scene_code_generation.is_some(),
        "invalid_model_selection",
        "/model_selection",
    )?;
    Ok(selection)
}

async fn validate_pair(c: &mut SqliteConnection, role: &str, selection: &Selection) -> Result<()> {
    let model = read_model(c, selection).await?;
    validate_model(&model, role)?;
    check(!model.remote, "provider_unavailable", "/model_selection")
}

async fn validate_selection(c: &mut SqliteConnection, selection: &ModelSelection) -> Result<()> {
    for role in ["storyboard_planning", "scene_code_generation"] {
        if let Some(pair) = model_pair(selection, role) {
            validate_pair(c, role, pair).await?;
        }
    }
    Ok(())
}
fn list_limit(q: &ListQuery) -> Result<u32> {
    let limit = q.limit.unwrap_or(20);
    check((1..=100).contains(&limit), "invalid_schema", "/limit")?;
    Ok(limit)
}
async fn cursor(
    c: &mut SqliteConnection,
    pid: Option<&str>,
    rid: Option<&str>,
) -> Result<Option<String>> {
    let Some(rid) = rid else { return Ok(None) };
    check(id(rid), "invalid_cursor", "/cursor")?;
    let value: Option<String> = if let Some(pid) = pid {
        sqlx::query_scalar("SELECT created_at FROM revisions WHERE id=? AND project_id=?")
            .bind(rid)
            .bind(pid)
            .fetch_optional(c)
            .await?
    } else {
        sqlx::query_scalar("SELECT created_at FROM projects WHERE id=? AND deleted_at IS NULL")
            .bind(rid)
            .fetch_optional(c)
            .await?
    };
    value
        .map(Some)
        .ok_or_else(|| invalid("invalid_cursor", "/cursor"))
}
async fn insert_project(c: &mut SqliteConnection, command: CreateProject) -> Result<Project> {
    version(&command.schema_version)?;
    text(&command.title, 200, "/title")?;
    text(&command.idea, 10000, "/idea")?;
    target(&command.target)?;
    selection(&command.model_selection)?;
    let value = Project {
        schema_version: "1".into(),
        id: Uuid::new_v4().to_string(),
        title: command.title,
        idea: command.idea,
        target: command.target,
        model_selection: command.model_selection,
        current_revision_id: None,
        approved_revision_id: None,
    };
    let timestamp = now(c).await?;
    sqlx::query("INSERT INTO projects(id,title,idea,target_json,model_selection_json,created_at) VALUES(?,?,?,?,?,?)").bind(&value.id).bind(&value.title).bind(&value.idea).bind(json(&value.target)?).bind(json(&value.model_selection)?).bind(timestamp).execute(c).await?;
    Ok(value)
}
async fn replay<T: DeserializeOwned>(
    c: &mut SqliteConnection,
    operation: &str,
    scope: &str,
    key: &str,
    request: &str,
) -> Result<Option<T>> {
    let row=sqlx::query("SELECT i.request_sha256,i.response_json,p.deleted_at FROM idempotency_records i JOIN projects p ON p.id=i.project_id WHERE operation=? AND scope=? AND key=?").bind(operation).bind(scope).bind(key).fetch_optional(c).await?;
    let Some(row) = row else { return Ok(None) };
    if row.try_get::<Option<String>, _>("deleted_at")?.is_some() {
        return Err(StoreError::NotFound);
    }
    if row.try_get::<String, _>("request_sha256")? != request {
        return Err(StoreError::IdempotencyConflict);
    }
    Ok(Some(decode(row.try_get("response_json")?)?))
}
async fn remember<T: Serialize>(
    c: &mut SqliteConnection,
    operation: &str,
    scope: &str,
    key: &str,
    pid: &str,
    request: &str,
    value: &T,
) -> Result<()> {
    sqlx::query("INSERT INTO idempotency_records(operation,scope,key,project_id,request_sha256,response_json) VALUES(?,?,?,?,?,?)").bind(operation).bind(scope).bind(key).bind(pid).bind(request).bind(json(value)?).execute(c).await?;
    Ok(())
}
async fn insert_revision(
    c: &mut SqliteConnection,
    pid: &str,
    command: CreateRevision,
) -> Result<Revision> {
    version(&command.schema_version)?;
    version(&command.content.schema_version)?;
    let project = read_project(c, pid).await?;
    if project.current_revision_id != command.expected_revision_id {
        return Err(StoreError::StaleRevision);
    }
    let parent = if let Some(rid) = &project.current_revision_id {
        Some(read_revision(c, pid, rid).await?)
    } else {
        None
    };
    let mut input = command.content;
    validate_content(&input)?;
    let retained: HashSet<&str> = parent
        .as_ref()
        .map(|r| {
            r.content
                .storyboard
                .scenes
                .iter()
                .map(|s| s.id.as_str())
                .collect()
        })
        .unwrap_or_default();
    let mut seen = HashSet::new();
    let mut scenes = Vec::with_capacity(input.storyboard.scenes.len());
    for scene in input.storyboard.scenes {
        let sid = if let Some(sid) = scene.id {
            check(
                id(&sid) && retained.contains(sid.as_str()),
                "invalid_storyboard",
                "/storyboard/scenes/id",
            )?;
            sid
        } else {
            Uuid::new_v4().to_string()
        };
        check(
            seen.insert(sid.clone()),
            "invalid_storyboard",
            "/storyboard/scenes/id",
        )?;
        scenes.push(Scene {
            id: sid,
            order: scene.order,
            duration_ms: scene.duration_ms,
            visual_direction: scene.visual_direction,
            camera: scene.camera,
            action: scene.action,
            narration: scene.narration,
            caption_hint: scene.caption_hint,
            sfx_cues: scene.sfx_cues,
        });
    }
    if let Some(provenance) = input.planning_provenance.as_mut() {
        provenance.scene_id_mapping = Some(
            scenes
                .iter()
                .map(|scene| SceneIdMapping {
                    order: scene.order,
                    scene_id: scene.id.clone(),
                })
                .collect(),
        );
    }
    input.asset_hashes.sort();
    let content = RevisionContent {
        schema_version: input.schema_version,
        idea: input.idea,
        target: input.target,
        storyboard: Storyboard {
            synopsis: input.storyboard.synopsis,
            hook: input.storyboard.hook,
            scenes,
        },
        models: input.models,
        asset_hashes: input.asset_hashes,
        skill_id: input.skill_id,
        skill_version: input.skill_version,
        planning_provenance: input.planning_provenance,
    };
    let bytes = canonical(&content)?;
    let value = Revision {
        schema_version: "1".into(),
        id: Uuid::new_v4().to_string(),
        project_id: pid.into(),
        parent_revision_id: project.current_revision_id,
        version: parent.as_ref().map_or(Ok(1), |r| {
            r.version.checked_add(1).ok_or(StoreError::Internal)
        })?,
        created_at: now(c).await?,
        content_sha256: digest(&bytes),
        content,
    };
    let source = String::from_utf8(bytes).map_err(|_| StoreError::Internal)?;
    sqlx::query("INSERT INTO revisions(id,project_id,parent_revision_id,version,created_at,content_sha256,content_json) VALUES(?,?,?,?,?,?,?)").bind(&value.id).bind(pid).bind(&value.parent_revision_id).bind(value.version).bind(&value.created_at).bind(&value.content_sha256).bind(source).execute(&mut *c).await?;
    sqlx::query("INSERT INTO storyboards(revision_id,project_id,synopsis,hook) VALUES(?,?,?,?)")
        .bind(&value.id)
        .bind(pid)
        .bind(&value.content.storyboard.synopsis)
        .bind(&value.content.storyboard.hook)
        .execute(&mut *c)
        .await?;
    for scene in &value.content.storyboard.scenes {
        sqlx::query("INSERT INTO scenes(project_id,revision_id,id,scene_order,duration_ms) VALUES(?,?,?,?,?)").bind(pid).bind(&value.id).bind(&scene.id).bind(scene.order).bind(scene.duration_ms).execute(&mut *c).await?;
    }
    sqlx::query("UPDATE projects SET current_revision_id=?,approved_revision_id=NULL,idea=?,target_json=? WHERE id=?").bind(&value.id).bind(&value.content.idea).bind(json(&value.content.target)?).bind(pid).execute(c).await?;
    Ok(value)
}
fn validate_content(input: &RevisionInput) -> Result<()> {
    text(&input.idea, 10000, "/idea")?;
    target(&input.target)?;
    check(
        name(&input.skill_id) && name(&input.skill_version),
        "invalid_schema",
        "/skill_id",
    )?;
    for (role, model) in [
        ("storyboard_planning", &input.models.storyboard_planning),
        ("scene_code_generation", &input.models.scene_code_generation),
    ] {
        check(
            name(&model.provider_id)
                && name(&model.model_id)
                && name(&model.adapter_version)
                && hash(&model.prompt_sha256)
                && model.parameters.temperature.is_finite()
                && (0.0..=2.0).contains(&model.parameters.temperature)
                && model.parameters.seed.is_none_or(|s| s <= 2147483647),
            "invalid_model_selection",
            &format!("/models/{role}"),
        )?;
    }
    if let Some(provenance) = &input.planning_provenance {
        let snapshot = &input.models.storyboard_planning;
        check(
            provenance.source == "deterministic_mock"
                && provenance.prompt_input.schema_version == "scenivox.storyboard_planning.v1"
                && name(&provenance.prompt_input.provider_id)
                && name(&provenance.prompt_input.model_id)
                && name(&provenance.prompt_input.adapter_version)
                && provenance.prompt_input.provider_id == snapshot.provider_id
                && provenance.prompt_input.model_id == snapshot.model_id
                && provenance.prompt_input.adapter_version == snapshot.adapter_version
                && provenance.prompt_input.idea == input.idea
                && provenance.prompt_input.target == input.target
                && hash(&provenance.prompt_sha256)
                && provenance.prompt_sha256 == digest(&canonical(&provenance.prompt_input)?)
                && provenance.scene_prompt_input.schema_version == "scenivox.scene_generation.v1"
                && provenance.scene_prompt_input.idea == input.idea
                && provenance.scene_prompt_input.target == input.target
                && id_independent_storyboard(&provenance.scene_prompt_input.storyboard)
                    == id_independent_storyboard(&input.storyboard)
                && provenance.scene_id_mapping.as_ref().is_none_or(|mapping| {
                    mapping.len() == input.storyboard.scenes.len()
                        && mapping
                            .iter()
                            .zip(&input.storyboard.scenes)
                            .all(|(entry, scene)| {
                                entry.order == scene.order
                                    && scene.id.as_deref() == Some(entry.scene_id.as_str())
                            })
                })
                && provenance.scene_prompt_input.provider_id
                    == input.models.scene_code_generation.provider_id
                && provenance.scene_prompt_input.model_id
                    == input.models.scene_code_generation.model_id
                && provenance.scene_prompt_input.adapter_version
                    == input.models.scene_code_generation.adapter_version
                && hash(&provenance.scene_prompt_sha256)
                && provenance.scene_prompt_sha256
                    == digest(&canonical(&provenance.scene_prompt_input)?),
            "invalid_storyboard",
            "/planning_provenance",
        )?;
    }
    check(
        input.asset_hashes.len() <= 1000
            && input.asset_hashes.iter().all(|s| hash(s))
            && input.asset_hashes.iter().collect::<HashSet<_>>().len() == input.asset_hashes.len(),
        "invalid_schema",
        "/asset_hashes",
    )?;
    // Hashes are immutable metadata only; this store does not resolve or execute assets.
    text(&input.storyboard.synopsis, 10000, "/storyboard/synopsis")?;
    text(&input.storyboard.hook, 10000, "/storyboard/hook")?;
    check(
        (1..=60).contains(&input.storyboard.scenes.len()),
        "invalid_storyboard",
        "/storyboard/scenes",
    )?;
    let mut sum = 0u64;
    for (i, s) in input.storyboard.scenes.iter().enumerate() {
        let path = format!("/storyboard/scenes/{i}");
        check(
            s.order == i as u32,
            "invalid_storyboard",
            &format!("{path}/order"),
        )?;
        check(
            (1..=60000).contains(&s.duration_ms),
            "invalid_storyboard",
            &format!("{path}/duration_ms"),
        )?;
        for (field, value) in [
            ("visual_direction", &s.visual_direction),
            ("camera", &s.camera),
            ("action", &s.action),
            ("narration", &s.narration),
        ] {
            text(value, 10000, &format!("{path}/{field}"))?;
        }
        check(
            s.caption_hint.chars().count() <= 10000,
            "invalid_schema",
            &format!("{path}/caption_hint"),
        )?;
        check(
            s.sfx_cues.len() <= 100,
            "invalid_storyboard",
            &format!("{path}/sfx_cues"),
        )?;
        for cue in &s.sfx_cues {
            check(
                id(&cue.asset_id) && cue.at_ms < s.duration_ms,
                "invalid_storyboard",
                &format!("{path}/sfx_cues"),
            )?;
        }
        // P1-01 has no asset registry: a well-shaped reference is still unresolved.
        check(
            s.sfx_cues.is_empty(),
            "invalid_asset_reference",
            &format!("{path}/sfx_cues"),
        )?;
        sum += u64::from(s.duration_ms);
    }
    check(
        sum == u64::from(input.target.duration_ms),
        "invalid_storyboard",
        "/storyboard/scenes",
    )
}
