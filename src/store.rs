//! Shared local persistence. No providers, asset execution, or approval workflow.
use crate::{
    asset::{
        ASSET_SCHEMA_VERSION, AssetError, AssetIntegrityReport, AssetKind, AssetRecord,
        AssetStoreRecoveryReport, RegisterAsset, authorize_asset_import_source,
        inspect_asset_store_artifacts, stage_local_asset, verify_stored_asset,
    },
    cost::{
        BudgetReservation, COST_SCHEMA_VERSION, CostConsent, CostConsentStatus, CostError,
        CostGateDecision, CostScope, CreateCostConsent, MAX_COST_SCOPES, ModelPricingRecord,
        PricingSnapshot, ProviderUsageReport, ReconcileBudgetRequest, ReservationStatus,
        ReserveBudgetRequest, RevokeCostConsent, SetModelPricing, WorkerLease,
        compute_usage_cost_micros, is_valid_utc_timestamp, validate_pricing_snapshot,
    },
    domain::*,
    execution_auth::{
        ClaimExecutionAuthorization, EXECUTION_AUTH_SCHEMA_VERSION, ExecutionAuthStatus,
        ExecutionAuthorization, ExecutionWorkspaceEvidence, IssueExecutionAuthorization,
        RecordWorkspaceEvidence,
    },
    skill::{
        AuditStatus, ImportSkill, SkillError, SkillIntegrityReport, SkillLayoutSummary,
        SkillManifest, SkillRecord, SkillStoreRecoveryReport, authorize_import_source,
        inspect_skill_store_artifacts, is_safe_skill_id, is_safe_skill_version, stage_local_skill,
        verify_stored_bundle,
    },
    voice::{AudioProvenance, LocalNarration},
};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqliteConnection, SqlitePool, sqlite::SqliteRow};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillImportFault {
    None,
    BeforePublish,
    AfterPublishBeforeCommit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetImportFault {
    None,
    BeforePublish,
    AfterPublishBeforeCommit,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not found")]
    NotFound,
    #[error("stale revision")]
    StaleRevision,
    #[error("idempotency conflict")]
    IdempotencyConflict,
    #[error("forbidden: {code}")]
    Forbidden { code: &'static str },
    #[error("validation: {code} at {path}")]
    Validation { code: &'static str, path: String },
    #[error("database unavailable")]
    Database(#[from] sqlx::Error),
    #[error("conflict: {code}")]
    Conflict { code: &'static str },
    #[error("internal error")]
    Internal,
}
impl StoreError {
    pub fn code(&self) -> &str {
        match self {
            Self::NotFound => "not_found",
            Self::StaleRevision => "stale_revision",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::Forbidden { code } | Self::Validation { code, .. } | Self::Conflict { code } => {
                code
            }
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
fn refresh_scene_prompt_provenance(input: &mut RevisionInput) -> Result<()> {
    if let Some(provenance) = input.planning_provenance.as_mut() {
        provenance.scene_prompt_input.idea = input.idea.clone();
        provenance.scene_prompt_input.target = input.target.clone();
        provenance.scene_prompt_input.storyboard = id_independent_storyboard(&input.storyboard);
        provenance.scene_id_mapping = None;
        let prompt_sha256 = digest(&canonical(&provenance.scene_prompt_input)?);
        provenance.scene_prompt_sha256 = prompt_sha256.clone();
        input.models.scene_code_generation.prompt_sha256 = prompt_sha256;
    }
    Ok(())
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
fn render_job(row: SqliteRow) -> Result<RenderJob> {
    Ok(RenderJob {
        schema_version: "1".into(),
        id: row.try_get("id")?,
        project_id: row.try_get("project_id")?,
        revision_id: row.try_get("revision_id")?,
        approval_id: row.try_get("approval_id")?,
        content_sha256: row.try_get("content_sha256")?,
        snapshot: decode(row.try_get("snapshot_json")?)?,
        status: decode_status(row.try_get("status")?)?,
        retry_of_job_id: row.try_get("retry_of_job_id")?,
        created_at: row.try_get("created_at")?,
        error_code: row.try_get("error_code")?,
        artifact_ids: decode(row.try_get("artifact_ids_json")?)?,
        cost_consent_id: row.try_get("cost_consent_id")?,
    })
}
fn decode_status(raw: String) -> Result<RenderJobStatus> {
    serde_json::from_str(&format!("\"{raw}\"")).map_err(|_| StoreError::Internal)
}

fn map_skill_error(err: SkillError) -> StoreError {
    match err {
        SkillError::InvalidManifest(_) => invalid("invalid_skill_manifest", "/skill.manifest.json"),
        SkillError::UnsafeBundle(_) => invalid("unsafe_skill_bundle", "/source_path"),
        SkillError::UnsupportedArchive(_) => invalid("unsupported_archive_format", "/source_path"),
        SkillError::ImportNotAuthorized(_) => StoreError::Forbidden {
            code: "skill_import_not_authorized",
        },
        SkillError::IntegrityFailed(_) => StoreError::Conflict {
            code: "skill_integrity_failed",
        },
        SkillError::InterruptedPublication(_) => StoreError::Conflict {
            code: "interrupted_skill_publication",
        },
        SkillError::Io(_) => invalid("input_unavailable", "/source_path"),
    }
}

fn skill_row(row: SqliteRow) -> Result<SkillRecord> {
    let audit_raw: String = row.try_get("audit_status")?;
    let audit_status = AuditStatus::parse(&audit_raw).ok_or(StoreError::Internal)?;
    let manifest: SkillManifest = decode(row.try_get("manifest_json")?)?;
    let layout: SkillLayoutSummary = decode(row.try_get("layout_json")?)?;
    Ok(SkillRecord {
        schema_version: "1".into(),
        skill_id: row.try_get("skill_id")?,
        skill_version: row.try_get("skill_version")?,
        bundle_sha256: row.try_get("bundle_sha256")?,
        enabled: row.try_get::<i64, _>("enabled")? != 0,
        audit_status,
        storage_rel_dir: row.try_get("storage_rel_dir")?,
        file_count: u64::try_from(row.try_get::<i64, _>("file_count")?)
            .map_err(|_| StoreError::Internal)?,
        total_bytes: u64::try_from(row.try_get::<i64, _>("total_bytes")?)
            .map_err(|_| StoreError::Internal)?,
        imported_at: row.try_get("imported_at")?,
        layout,
        manifest,
    })
}

fn map_asset_error(err: AssetError) -> StoreError {
    match err {
        AssetError::UnsafePath(_) => invalid("unsafe_asset_path", "/source_path"),
        AssetError::ImportNotAuthorized(_) => StoreError::Forbidden {
            code: "asset_import_not_authorized",
        },
        AssetError::AudioTooLarge => invalid("audio_too_large", "/source_path"),
        AssetError::UnsupportedAudioFormat(_) => {
            invalid("unsupported_audio_format", "/source_path")
        }
        AssetError::AudioLimitsExceeded(_) => invalid("audio_limits_exceeded", "/source_path"),
        AssetError::InvalidAudio(_) => invalid("invalid_audio", "/source_path"),
        AssetError::IntegrityFailed(_) => StoreError::Conflict {
            code: "asset_integrity_failed",
        },
        AssetError::InterruptedPublication(_) => StoreError::Conflict {
            code: "interrupted_asset_publication",
        },
        AssetError::Io(_) => invalid("input_unavailable", "/source_path"),
    }
}

fn asset_row(row: SqliteRow) -> Result<AssetRecord> {
    let kind_raw: String = row.try_get("kind")?;
    let kind = AssetKind::parse(&kind_raw).ok_or(StoreError::Internal)?;
    let prov_raw: String = row.try_get("provenance")?;
    let provenance: AudioProvenance =
        serde_json::from_str(&format!("\"{prov_raw}\"")).map_err(|_| StoreError::Internal)?;
    let metadata: LocalNarration = decode(row.try_get("metadata_json")?)?;
    Ok(AssetRecord {
        schema_version: ASSET_SCHEMA_VERSION.into(),
        id: row.try_get("id")?,
        sha256: row.try_get("sha256")?,
        kind,
        provenance,
        byte_size: u64::try_from(row.try_get::<i64, _>("byte_size")?)
            .map_err(|_| StoreError::Internal)?,
        storage_rel_path: row.try_get("storage_rel_path")?,
        metadata,
        created_at: row.try_get("created_at")?,
    })
}

async fn replay_asset_import(
    c: &mut SqliteConnection,
    key: &str,
    request_sha256: &str,
) -> Result<Option<(AssetRecord, bool)>> {
    let row = sqlx::query(
        "SELECT request_sha256,response_json,created_flag FROM asset_import_idempotency WHERE idempotency_key=?",
    )
    .bind(key)
    .fetch_optional(c)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row.try_get::<String, _>("request_sha256")? != request_sha256 {
        return Err(StoreError::IdempotencyConflict);
    }
    let created_flag: i64 = row.try_get("created_flag")?;
    Ok(Some((
        decode(row.try_get("response_json")?)?,
        created_flag == 0,
    )))
}

async fn replay_skill_import(
    c: &mut SqliteConnection,
    key: &str,
    request_sha256: &str,
) -> Result<Option<SkillRecord>> {
    let row = sqlx::query(
        "SELECT request_sha256,response_json FROM skill_import_idempotency WHERE idempotency_key=?",
    )
    .bind(key)
    .fetch_optional(c)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row.try_get::<String, _>("request_sha256")? != request_sha256 {
        return Err(StoreError::IdempotencyConflict);
    }
    Ok(Some(decode(row.try_get("response_json")?)?))
}
async fn now_plus_seconds(c: &mut SqliteConnection, secs: i64) -> Result<String> {
    Ok(
        sqlx::query_scalar("SELECT strftime('%Y-%m-%dT%H:%M:%SZ','now', ?)")
            .bind(format!("+{secs} seconds"))
            .fetch_one(c)
            .await?,
    )
}

async fn replay_ledger<T: DeserializeOwned>(
    c: &mut SqliteConnection,
    operation: &str,
    key: &str,
    request_sha256: &str,
) -> Result<Option<T>> {
    let row = sqlx::query(
        "SELECT request_sha256,response_json FROM ledger_idempotency WHERE operation=? AND idempotency_key=?",
    )
    .bind(operation)
    .bind(key)
    .fetch_optional(c)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row.try_get::<String, _>("request_sha256")? != request_sha256 {
        return Err(StoreError::IdempotencyConflict);
    }
    Ok(Some(decode(row.try_get("response_json")?)?))
}

async fn remember_ledger<T: Serialize>(
    c: &mut SqliteConnection,
    operation: &str,
    key: &str,
    request_sha256: &str,
    value: &T,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO ledger_idempotency(operation,idempotency_key,request_sha256,response_json) VALUES(?,?,?,?)",
    )
    .bind(operation)
    .bind(key)
    .bind(request_sha256)
    .bind(json(value)?)
    .execute(c)
    .await?;
    Ok(())
}

fn cost_consent_row(row: SqliteRow) -> Result<CostConsent> {
    let scopes_raw: Vec<String> = decode(row.try_get("scopes_json")?)?;
    let scopes = scopes_raw
        .iter()
        .map(|s| CostScope::parse(s).ok_or(StoreError::Internal))
        .collect::<Result<Vec<_>>>()?;
    let status_raw: String = row.try_get("status")?;
    let status = CostConsentStatus::parse(&status_raw).ok_or(StoreError::Internal)?;
    Ok(CostConsent {
        schema_version: COST_SCHEMA_VERSION.into(),
        id: row.try_get("id")?,
        project_id: row.try_get("project_id")?,
        provider_id: row.try_get("provider_id")?,
        model_id: row.try_get("model_id")?,
        scopes,
        max_cost_micros: row.try_get("max_cost_micros")?,
        max_total_tokens: row.try_get("max_total_tokens")?,
        spent_micros: row.try_get("spent_micros")?,
        reserved_micros: row.try_get("reserved_micros")?,
        spent_tokens: row.try_get("spent_tokens")?,
        reserved_tokens: row.try_get("reserved_tokens")?,
        pricing: decode(row.try_get("pricing_snapshot_json")?)?,
        pricing_sha256: row.try_get("pricing_sha256")?,
        status,
        halt_reason: row.try_get("halt_reason")?,
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
        revoked_at: row.try_get("revoked_at")?,
    })
}

fn reservation_row_with_dispatch(
    row: SqliteRow,
    dispatch_authorized: bool,
) -> Result<BudgetReservation> {
    let scope_raw: String = row.try_get("scope")?;
    let scope = CostScope::parse(&scope_raw).ok_or(StoreError::Internal)?;
    let status_raw: String = row.try_get("status")?;
    let status = ReservationStatus::parse(&status_raw).ok_or(StoreError::Internal)?;
    Ok(BudgetReservation {
        schema_version: COST_SCHEMA_VERSION.into(),
        id: row.try_get("id")?,
        cost_consent_id: row.try_get("cost_consent_id")?,
        project_id: row.try_get("project_id")?,
        job_id: row.try_get("job_id")?,
        provider_id: row.try_get("provider_id")?,
        model_id: row.try_get("model_id")?,
        scope,
        owner_id: row.try_get("owner_id")?,
        lease_epoch: row.try_get("lease_epoch")?,
        estimated_input_tokens: row.try_get("estimated_input_tokens")?,
        max_output_tokens: row.try_get("max_output_tokens")?,
        reserved_micros: row.try_get("reserved_micros")?,
        reserved_tokens: row.try_get("reserved_tokens")?,
        actual_input_tokens: row.try_get("actual_input_tokens")?,
        actual_output_tokens: row.try_get("actual_output_tokens")?,
        actual_micros: row.try_get("actual_micros")?,
        overage_micros: row.try_get("overage_micros")?,
        pricing: decode(row.try_get("pricing_snapshot_json")?)?,
        pricing_sha256: row.try_get("pricing_sha256")?,
        status,
        dispatch_claimed: row.try_get::<i64, _>("dispatch_claimed")? != 0,
        dispatch_claimed_at: row.try_get("dispatch_claimed_at")?,
        dispatch_authorized,
        error_code: row.try_get("error_code")?,
        created_at: row.try_get("created_at")?,
        settled_at: row.try_get("settled_at")?,
    })
}

fn reservation_row(row: SqliteRow) -> Result<BudgetReservation> {
    reservation_row_with_dispatch(row, false)
}

fn pricing_row(row: SqliteRow) -> Result<ModelPricingRecord> {
    let pricing = PricingSnapshot {
        pricing_version: row.try_get("pricing_version")?,
        currency: row.try_get("currency")?,
        input_cost_per_token_micros: row.try_get("input_cost_per_token_micros")?,
        output_cost_per_token_micros: row.try_get("output_cost_per_token_micros")?,
        request_fixed_cost_micros: row.try_get("request_fixed_cost_micros")?,
        verified: row.try_get::<i64, _>("verified")? != 0,
    };
    Ok(ModelPricingRecord {
        schema_version: COST_SCHEMA_VERSION.into(),
        provider_id: row.try_get("provider_id")?,
        model_id: row.try_get("model_id")?,
        pricing,
        pricing_sha256: row.try_get("pricing_sha256")?,
        updated_at: row.try_get("updated_at")?,
    })
}

async fn active_worker_lease(
    c: &mut SqliteConnection,
    now_utc: &str,
) -> Result<Option<WorkerLease>> {
    let row = sqlx::query("SELECT * FROM worker_leases WHERE id=1")
        .fetch_optional(c)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let expires_at: String = row.try_get("expires_at")?;
    if now_utc >= expires_at.as_str() {
        return Ok(None);
    }
    Ok(Some(WorkerLease {
        owner_id: row.try_get("owner_id")?,
        lease_epoch: row.try_get("lease_epoch")?,
        acquired_at: row.try_get("acquired_at")?,
        heartbeat_at: row.try_get("heartbeat_at")?,
        expires_at,
    }))
}

async fn require_verified_worker_lease(
    c: &mut SqliteConnection,
    now_utc: &str,
    owner_id: &str,
    lease_epoch: i64,
) -> Result<WorkerLease> {
    let active = active_worker_lease(c, now_utc)
        .await?
        .ok_or(StoreError::Forbidden {
            code: "reservation_ownership_required",
        })?;
    if active.owner_id != owner_id || active.lease_epoch != lease_epoch {
        return Err(StoreError::Forbidden {
            code: "reservation_ownership_required",
        });
    }
    Ok(active)
}

async fn verify_consent_pricing_freshness(
    tx: &mut SqliteConnection,
    consent: &CostConsent,
) -> Result<()> {
    if !consent.pricing.verified || digest(&canonical(&consent.pricing)?) != consent.pricing_sha256
    {
        return Err(StoreError::Conflict {
            code: "provider_pricing_unavailable",
        });
    }
    let current_row = sqlx::query("SELECT * FROM model_pricing WHERE provider_id=? AND model_id=?")
        .bind(&consent.provider_id)
        .bind(&consent.model_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::Conflict {
            code: "provider_pricing_unavailable",
        })?;
    let current = pricing_row(current_row)?;
    if !current.pricing.verified {
        return Err(StoreError::Conflict {
            code: "provider_pricing_unavailable",
        });
    }
    if current.pricing_sha256 != consent.pricing_sha256 || current.pricing != consent.pricing {
        return Err(StoreError::Conflict {
            code: "remote_cost_not_approved",
        });
    }
    Ok(())
}

async fn validate_registered_model_for_snapshot(
    tx: &mut SqliteConnection,
    role: &str,
    snap: &ModelSnapshot,
) -> Result<ModelRecord> {
    let model = read_model(
        tx,
        &Selection {
            provider_id: snap.provider_id.clone(),
            model_id: snap.model_id.clone(),
        },
    )
    .await?;
    check(
        model.task_roles.iter().any(|cap| cap == role),
        "unsupported_capability",
        &format!("/models/{role}"),
    )?;
    check(
        model.adapter_version == snap.adapter_version,
        "invalid_model_selection",
        &format!("/models/{role}/adapter_version"),
    )?;
    if !model.enabled
        || !model.configured
        || model.availability == "unavailable"
        || (!model.remote && model.availability != "available")
    {
        return Err(StoreError::Conflict {
            code: "provider_unavailable",
        });
    }
    Ok(model)
}

async fn verify_cost_consent_for_remote_gate(
    tx: &mut SqliteConnection,
    pid: &str,
    revision: &Revision,
    consent_id: Option<&str>,
    scope: CostScope,
) -> Result<CostGateDecision> {
    let planning_model = validate_registered_model_for_snapshot(
        tx,
        "storyboard_planning",
        &revision.content.models.storyboard_planning,
    )
    .await?;
    let scene_model = validate_registered_model_for_snapshot(
        tx,
        "scene_code_generation",
        &revision.content.models.scene_code_generation,
    )
    .await?;

    let remote_models: Vec<&ModelSnapshot> = [
        (
            &revision.content.models.storyboard_planning,
            planning_model.remote,
        ),
        (
            &revision.content.models.scene_code_generation,
            scene_model.remote,
        ),
    ]
    .into_iter()
    .filter_map(|(snap, remote)| remote.then_some(snap))
    .collect();

    if remote_models.is_empty() {
        if let Some(cid) = consent_id {
            // Reject opaque/invalid consent IDs even on offline revisions.
            if !id(cid)
                || sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM cost_consents WHERE id=? AND status='active'",
                )
                .bind(cid)
                .fetch_one(&mut *tx)
                .await?
                    == 0
            {
                return Err(StoreError::Conflict {
                    code: "remote_cost_not_approved",
                });
            }
        }
        return Ok(CostGateDecision::ZeroCostOffline);
    }

    let Some(cid) = consent_id else {
        return Err(StoreError::Conflict {
            code: "remote_cost_not_approved",
        });
    };
    if !id(cid) {
        return Err(StoreError::Conflict {
            code: "remote_cost_not_approved",
        });
    }
    let row = sqlx::query("SELECT * FROM cost_consents WHERE id=?")
        .bind(cid)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::Conflict {
            code: "remote_cost_not_approved",
        })?;
    let consent = cost_consent_row(row)?;
    let now_utc = now(&mut *tx).await?;
    if consent.status != CostConsentStatus::Active
        || now_utc >= consent.expires_at
        || consent
            .project_id
            .as_deref()
            .is_some_and(|bound| bound != pid)
        || !consent.scopes.contains(&scope)
        || remote_models
            .iter()
            .any(|m| m.provider_id != consent.provider_id || m.model_id != consent.model_id)
    {
        return Err(StoreError::Conflict {
            code: "remote_cost_not_approved",
        });
    }
    verify_consent_pricing_freshness(tx, &consent).await?;
    let committed_micros = consent
        .spent_micros
        .checked_add(consent.reserved_micros)
        .ok_or(StoreError::Conflict {
            code: "cost_limit_exceeded",
        })?;
    if committed_micros >= consent.max_cost_micros {
        return Err(StoreError::Conflict {
            code: "cost_limit_exceeded",
        });
    }
    if let Some(max_tok) = consent.max_total_tokens {
        let committed_tokens = consent
            .spent_tokens
            .checked_add(consent.reserved_tokens)
            .ok_or(StoreError::Conflict {
                code: "cost_limit_exceeded",
            })?;
        if committed_tokens >= max_tok {
            return Err(StoreError::Conflict {
                code: "cost_limit_exceeded",
            });
        }
    }
    Ok(CostGateDecision::RemoteConsentVerified {
        cost_consent_id: consent.id,
    })
}

fn execution_auth_row(row: SqliteRow) -> Result<ExecutionAuthorization> {
    let status_raw: String = row.try_get("status")?;
    let status = ExecutionAuthStatus::parse(&status_raw).ok_or(StoreError::Internal)?;
    Ok(ExecutionAuthorization {
        schema_version: EXECUTION_AUTH_SCHEMA_VERSION.into(),
        id: row.try_get("id")?,
        project_id: row.try_get("project_id")?,
        revision_id: row.try_get("revision_id")?,
        approval_id: row.try_get("approval_id")?,
        content_sha256: row.try_get("content_sha256")?,
        skill_id: row.try_get("skill_id")?,
        skill_version: row.try_get("skill_version")?,
        bundle_sha256: row.try_get("bundle_sha256")?,
        runtime_policy_sha256: row.try_get("runtime_policy_sha256")?,
        status,
        claimed_job_id: row.try_get("claimed_job_id")?,
        issued_at: row.try_get("issued_at")?,
        expires_at: row.try_get("expires_at")?,
        claimed_at: row.try_get("claimed_at")?,
    })
}

fn workspace_evidence_row(row: SqliteRow) -> Result<ExecutionWorkspaceEvidence> {
    Ok(ExecutionWorkspaceEvidence {
        schema_version: EXECUTION_AUTH_SCHEMA_VERSION.into(),
        id: row.try_get("id")?,
        execution_auth_id: row.try_get("execution_auth_id")?,
        project_id: row.try_get("project_id")?,
        revision_id: row.try_get("revision_id")?,
        job_id: row.try_get("job_id")?,
        worker_owner_id: row.try_get("worker_owner_id")?,
        worker_lease_epoch: row.try_get("worker_lease_epoch")?,
        workspace_manifest_sha256: row.try_get("workspace_manifest_sha256")?,
        output_artifact_sha256: row.try_get("output_artifact_sha256")?,
        recorded_at: row.try_get("recorded_at")?,
    })
}

async fn verify_revision_assets_in_tx(
    tx: &mut SqliteConnection,
    revision: &Revision,
    require_production_provenance: bool,
    asset_store_dir: Option<&Path>,
) -> Result<()> {
    let mut approved_hashes = HashSet::with_capacity(revision.content.asset_hashes.len());
    for asset_hash in &revision.content.asset_hashes {
        let row = sqlx::query("SELECT * FROM assets WHERE sha256=?")
            .bind(asset_hash)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| invalid("invalid_asset_reference", "/asset_hashes"))?;
        let record = asset_row(row)?;
        if require_production_provenance && record.provenance != AudioProvenance::UserSupplied {
            return Err(invalid("invalid_asset_reference", "/asset_hashes"));
        }
        if let Some(dir) = asset_store_dir {
            verify_stored_asset(dir, &record).map_err(map_asset_error)?;
        }
        approved_hashes.insert(record.sha256);
    }
    for (idx, scene) in revision.content.storyboard.scenes.iter().enumerate() {
        for cue in &scene.sfx_cues {
            let row = sqlx::query("SELECT * FROM assets WHERE id=? AND kind='sfx_wav'")
                .bind(&cue.asset_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| {
                    invalid(
                        "invalid_asset_reference",
                        &format!("/storyboard/scenes/{idx}/sfx_cues"),
                    )
                })?;
            let record = asset_row(row)?;
            if !approved_hashes.contains(&record.sha256) {
                return Err(invalid(
                    "invalid_asset_reference",
                    &format!("/storyboard/scenes/{idx}/sfx_cues"),
                ));
            }
            if require_production_provenance && record.provenance != AudioProvenance::UserSupplied {
                return Err(invalid(
                    "invalid_asset_reference",
                    &format!("/storyboard/scenes/{idx}/sfx_cues"),
                ));
            }
            if let Some(dir) = asset_store_dir {
                verify_stored_asset(dir, &record).map_err(map_asset_error)?;
            }
        }
    }
    Ok(())
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
async fn read_review_status(
    c: &mut SqliteConnection,
    pid: &str,
    rid: &str,
) -> Result<ReviewStatus> {
    let value: String = sqlx::query_scalar(
        "SELECT review_state FROM revision_reviews WHERE project_id=? AND revision_id=?",
    )
    .bind(pid)
    .bind(rid)
    .fetch_optional(c)
    .await?
    .ok_or(StoreError::Internal)?;
    match value.as_str() {
        "draft" => Ok(ReviewStatus::Draft),
        "in_review" => Ok(ReviewStatus::InReview),
        "approved" => Ok(ReviewStatus::Approved),
        _ => Err(StoreError::Internal),
    }
}
async fn read_approval(
    c: &mut SqliteConnection,
    pid: &str,
    rid: &str,
    content_sha256: &str,
) -> Result<Approval> {
    let row = sqlx::query(
        "SELECT id,project_id,revision_id,content_sha256,approved_at FROM approvals WHERE project_id=? AND revision_id=? AND content_sha256=?",
    )
    .bind(pid)
    .bind(rid)
    .bind(content_sha256)
    .fetch_optional(c)
    .await?
    .ok_or(StoreError::Internal)?;
    Ok(Approval {
        schema_version: "1".into(),
        id: row.try_get("id")?,
        project_id: row.try_get("project_id")?,
        revision_id: row.try_get("revision_id")?,
        content_sha256: row.try_get("content_sha256")?,
        approved_at: row.try_get("approved_at")?,
    })
}
fn validate_approval_hash(value: &str) -> Result<()> {
    check(hash(value), "invalid_schema", "/content_sha256")
}
async fn read_revision(c: &mut SqliteConnection, pid: &str, rid: &str) -> Result<Revision> {
    revision(sqlx::query("SELECT r.* FROM revisions r JOIN projects p ON p.id=r.project_id WHERE r.project_id=? AND r.id=? AND p.deleted_at IS NULL").bind(pid).bind(rid).fetch_optional(c).await?.ok_or(StoreError::NotFound)?)
}
#[derive(Clone, Debug)]
pub struct Store {
    pool: SqlitePool,
}
impl Store {
    pub async fn enqueue_render_job(
        &self,
        pid: &str,
        idempotency_key: Option<&str>,
        input: CreateRenderJob,
    ) -> Result<(RenderJob, bool)> {
        path_id(pid)?;
        if let Some(key) = idempotency_key {
            path_id(key)?;
        }
        let request_sha256 = digest(&canonical(&input)?);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(key) = idempotency_key {
            let existing = sqlx::query("SELECT request_sha256,job_id FROM render_job_idempotency WHERE project_id=? AND idempotency_key=?")
                .bind(pid).bind(key).fetch_optional(&mut *tx).await?;
            if let Some(row) = existing {
                let prior: String = row.try_get("request_sha256")?;
                if prior != request_sha256 {
                    return Err(StoreError::IdempotencyConflict);
                }
                let job_id: String = row.try_get("job_id")?;
                let job = render_job(
                    sqlx::query("SELECT * FROM render_jobs WHERE id=?")
                        .bind(job_id)
                        .fetch_one(&mut *tx)
                        .await?,
                )?;
                tx.commit().await?;
                return Ok((job, true));
            }
        }
        version(&input.schema_version)?;
        path_id(&input.revision_id)?;
        validate_approval_hash(&input.content_sha256)?;
        let project = read_project(&mut tx, pid).await?;
        if project.current_revision_id.as_deref() != Some(&input.revision_id) {
            return Err(StoreError::StaleRevision);
        }
        if project.approved_revision_id.as_deref() != Some(&input.revision_id) {
            return Err(StoreError::Conflict {
                code: "approval_required",
            });
        }
        let revision = read_revision(&mut tx, pid, &input.revision_id).await?;
        if revision.content_sha256 != input.content_sha256 {
            return Err(StoreError::Conflict {
                code: "hash_mismatch",
            });
        }
        let approval =
            read_approval(&mut tx, pid, &input.revision_id, &input.content_sha256).await?;
        if digest(&canonical(&revision.content)?) != revision.content_sha256
            || approval.revision_id != revision.id
            || approval.content_sha256 != revision.content_sha256
        {
            return Err(StoreError::Conflict {
                code: "hash_mismatch",
            });
        }
        verify_revision_assets_in_tx(&mut tx, &revision, false, None).await?;
        // Preserve fail-closed consent rejection when a consent ID is supplied or models require remote consent,
        // and reject execution with `skill_unavailable` until audited execution runtime lands.
        if let Some(cid) = input.cost_consent_id.as_deref() {
            if !id(cid) {
                return Err(StoreError::Conflict {
                    code: "remote_cost_not_approved",
                });
            }
            let _ = verify_cost_consent_for_remote_gate(
                &mut tx,
                pid,
                &revision,
                Some(cid),
                CostScope::SceneCodeGeneration,
            )
            .await?;
        }
        Err(StoreError::Conflict {
            code: "skill_unavailable",
        })
    }

    pub async fn get_render_job(&self, job_id: &str) -> Result<RenderJob> {
        path_id(job_id)?;
        let row = sqlx::query("SELECT * FROM render_jobs WHERE id=?")
            .bind(job_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        render_job(row)
    }

    pub async fn retry_render_job(&self, job_id: &str, input: RetryRenderJob) -> Result<RenderJob> {
        path_id(job_id)?;
        version(&input.schema_version)?;
        let job = self.get_render_job(job_id).await?;
        if !matches!(
            job.status,
            RenderJobStatus::Failed | RenderJobStatus::Cancelled | RenderJobStatus::Interrupted
        ) {
            return Err(StoreError::Conflict {
                code: "invalid_transition",
            });
        }
        // A terminal retry must revalidate consent and audited skill capability.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(cid) = input.cost_consent_id.as_deref() {
            if !id(cid) {
                return Err(StoreError::Conflict {
                    code: "remote_cost_not_approved",
                });
            }
            if let Ok(revision) = read_revision(&mut tx, &job.project_id, &job.revision_id).await {
                let _ = verify_cost_consent_for_remote_gate(
                    &mut tx,
                    &job.project_id,
                    &revision,
                    Some(cid),
                    CostScope::SceneCodeGeneration,
                )
                .await?;
            }
        }
        Err(StoreError::Conflict {
            code: "skill_unavailable",
        })
    }

    pub async fn cancel_render_job(
        &self,
        job_id: &str,
        input: JobCancelCommand,
    ) -> Result<RenderJob> {
        version(&input.schema_version)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query("SELECT * FROM render_jobs WHERE id=?")
            .bind(job_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        let mut job = render_job(row)?;
        match job.status {
            RenderJobStatus::Queued => {
                sqlx::query("UPDATE render_jobs SET status='cancelled' WHERE id=?")
                    .bind(job_id)
                    .execute(&mut *tx)
                    .await?;
                job.status = RenderJobStatus::Cancelled;
            }
            RenderJobStatus::Cancelled => {}
            _ => {
                return Err(StoreError::Conflict {
                    code: "invalid_transition",
                });
            }
        }
        tx.commit().await?;
        Ok(job)
    }

    pub async fn set_model_pricing(&self, input: SetModelPricing) -> Result<ModelPricingRecord> {
        version(&input.schema_version)?;
        check(
            name(&input.provider_id) && name(&input.model_id),
            "invalid_model_selection",
            "/model_selection",
        )?;
        text(&input.pricing_version, 128, "/pricing_version")?;
        check(input.currency == "USD", "invalid_schema", "/currency")?;
        check(
            input.input_cost_per_token_micros >= 0
                && input.output_cost_per_token_micros >= 0
                && input.request_fixed_cost_micros >= 0,
            "invalid_schema",
            "/pricing",
        )?;

        let snapshot = PricingSnapshot {
            pricing_version: input.pricing_version,
            currency: input.currency,
            input_cost_per_token_micros: input.input_cost_per_token_micros,
            output_cost_per_token_micros: input.output_cost_per_token_micros,
            request_fixed_cost_micros: input.request_fixed_cost_micros,
            verified: input.verified,
        };
        check(
            validate_pricing_snapshot(&snapshot),
            "invalid_schema",
            "/pricing",
        )?;
        let pricing_sha256 = digest(&canonical(&snapshot)?);

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let model_exists = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM model_registry WHERE provider_id=? AND model_id=?",
        )
        .bind(&input.provider_id)
        .bind(&input.model_id)
        .fetch_one(&mut *tx)
        .await?;
        if model_exists == 0 {
            return Err(StoreError::NotFound);
        }

        let updated_at = now(&mut tx).await?;
        sqlx::query(
            "INSERT INTO model_pricing(provider_id,model_id,pricing_version,currency,input_cost_per_token_micros,output_cost_per_token_micros,request_fixed_cost_micros,verified,pricing_sha256,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?) ON CONFLICT(provider_id,model_id) DO UPDATE SET pricing_version=excluded.pricing_version,currency=excluded.currency,input_cost_per_token_micros=excluded.input_cost_per_token_micros,output_cost_per_token_micros=excluded.output_cost_per_token_micros,request_fixed_cost_micros=excluded.request_fixed_cost_micros,verified=excluded.verified,pricing_sha256=excluded.pricing_sha256,updated_at=excluded.updated_at",
        )
        .bind(&input.provider_id)
        .bind(&input.model_id)
        .bind(&snapshot.pricing_version)
        .bind(&snapshot.currency)
        .bind(snapshot.input_cost_per_token_micros)
        .bind(snapshot.output_cost_per_token_micros)
        .bind(snapshot.request_fixed_cost_micros)
        .bind(i64::from(snapshot.verified))
        .bind(&pricing_sha256)
        .bind(&updated_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(ModelPricingRecord {
            schema_version: COST_SCHEMA_VERSION.into(),
            provider_id: input.provider_id,
            model_id: input.model_id,
            pricing: snapshot,
            pricing_sha256,
            updated_at,
        })
    }

    pub async fn get_model_pricing(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<ModelPricingRecord> {
        check(
            name(provider_id) && name(model_id),
            "invalid_model_selection",
            "/model_selection",
        )?;
        let row = sqlx::query("SELECT * FROM model_pricing WHERE provider_id=? AND model_id=?")
            .bind(provider_id)
            .bind(model_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        pricing_row(row)
    }

    pub async fn create_cost_consent(
        &self,
        idempotency_key: Option<&str>,
        mut input: CreateCostConsent,
    ) -> Result<(CostConsent, bool)> {
        if let Some(key) = idempotency_key {
            path_id(key)?;
        }
        version(&input.schema_version)?;
        if let Some(pid) = input.project_id.as_deref() {
            path_id(pid)?;
        }
        check(
            name(&input.provider_id) && name(&input.model_id),
            "invalid_model_selection",
            "/model_selection",
        )?;
        check(
            !input.scopes.is_empty() && input.scopes.len() <= MAX_COST_SCOPES,
            "invalid_schema",
            "/scopes",
        )?;
        let mut sorted_scopes = input.scopes.clone();
        sorted_scopes.sort();
        sorted_scopes.dedup();
        check(
            sorted_scopes.len() == input.scopes.len(),
            "invalid_schema",
            "/scopes",
        )?;
        input.scopes = sorted_scopes;
        check(
            input.max_cost_micros > 0,
            "invalid_schema",
            "/max_cost_micros",
        )?;
        check(
            input.max_total_tokens.is_none_or(|t| t > 0),
            "invalid_schema",
            "/max_total_tokens",
        )?;
        check(
            is_valid_utc_timestamp(&input.expires_at),
            "invalid_schema",
            "/expires_at",
        )?;

        let request_sha256 = digest(&canonical(&input)?);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(key) = idempotency_key {
            if let Some(consent) =
                replay_ledger::<CostConsent>(&mut tx, "create_cost_consent", key, &request_sha256)
                    .await?
            {
                tx.commit().await?;
                return Ok((consent, true));
            }
        }

        if let Some(pid) = input.project_id.as_deref() {
            read_project(&mut tx, pid).await?;
        }

        let model_row =
            sqlx::query("SELECT remote FROM model_registry WHERE provider_id=? AND model_id=?")
                .bind(&input.provider_id)
                .bind(&input.model_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| invalid("invalid_model_selection", "/model_selection"))?;
        let is_remote = model_row.try_get::<i64, _>("remote")? != 0;
        check(is_remote, "invalid_model_selection", "/model_selection")?;

        let pricing_rec = pricing_row(
            sqlx::query("SELECT * FROM model_pricing WHERE provider_id=? AND model_id=?")
                .bind(&input.provider_id)
                .bind(&input.model_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(StoreError::Conflict {
                    code: "provider_pricing_unavailable",
                })?,
        )?;
        if !pricing_rec.pricing.verified
            || digest(&canonical(&pricing_rec.pricing)?) != pricing_rec.pricing_sha256
        {
            return Err(StoreError::Conflict {
                code: "provider_pricing_unavailable",
            });
        }

        let created_at = now(&mut tx).await?;
        check(
            created_at < input.expires_at,
            "invalid_schema",
            "/expires_at",
        )?;

        let consent = CostConsent {
            schema_version: COST_SCHEMA_VERSION.into(),
            id: Uuid::new_v4().to_string(),
            project_id: input.project_id,
            provider_id: input.provider_id,
            model_id: input.model_id,
            scopes: input.scopes,
            max_cost_micros: input.max_cost_micros,
            max_total_tokens: input.max_total_tokens,
            spent_micros: 0,
            reserved_micros: 0,
            spent_tokens: 0,
            reserved_tokens: 0,
            pricing: pricing_rec.pricing,
            pricing_sha256: pricing_rec.pricing_sha256,
            status: CostConsentStatus::Active,
            halt_reason: None,
            created_at,
            expires_at: input.expires_at,
            revoked_at: None,
        };
        let scope_strs: Vec<&'static str> = consent.scopes.iter().map(|s| s.as_str()).collect();
        sqlx::query(
            "INSERT INTO cost_consents(id,project_id,provider_id,model_id,scopes_json,max_cost_micros,max_total_tokens,spent_micros,reserved_micros,spent_tokens,reserved_tokens,pricing_snapshot_json,pricing_sha256,status,halt_reason,created_at,expires_at,revoked_at) VALUES(?,?,?,?,?,?,?,0,0,0,0,?,?,'active',NULL,?,?,NULL)",
        )
        .bind(&consent.id)
        .bind(&consent.project_id)
        .bind(&consent.provider_id)
        .bind(&consent.model_id)
        .bind(json(&scope_strs)?)
        .bind(consent.max_cost_micros)
        .bind(consent.max_total_tokens)
        .bind(json(&consent.pricing)?)
        .bind(&consent.pricing_sha256)
        .bind(&consent.created_at)
        .bind(&consent.expires_at)
        .execute(&mut *tx)
        .await?;

        if let Some(key) = idempotency_key {
            remember_ledger(
                &mut tx,
                "create_cost_consent",
                key,
                &request_sha256,
                &consent,
            )
            .await?;
        }
        tx.commit().await?;
        Ok((consent, false))
    }

    pub async fn get_cost_consent(&self, consent_id: &str) -> Result<CostConsent> {
        path_id(consent_id)?;
        let row = sqlx::query("SELECT * FROM cost_consents WHERE id=?")
            .bind(consent_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        cost_consent_row(row)
    }

    pub async fn revoke_cost_consent(
        &self,
        consent_id: &str,
        input: RevokeCostConsent,
    ) -> Result<CostConsent> {
        path_id(consent_id)?;
        version(&input.schema_version)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query("SELECT * FROM cost_consents WHERE id=?")
            .bind(consent_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        let consent = cost_consent_row(row)?;
        match consent.status {
            CostConsentStatus::Active => {
                let revoked_at = now(&mut tx).await?;
                sqlx::query("UPDATE cost_consents SET status='revoked',revoked_at=? WHERE id=?")
                    .bind(&revoked_at)
                    .bind(consent_id)
                    .execute(&mut *tx)
                    .await?;
                let updated = cost_consent_row(
                    sqlx::query("SELECT * FROM cost_consents WHERE id=?")
                        .bind(consent_id)
                        .fetch_one(&mut *tx)
                        .await?,
                )?;
                tx.commit().await?;
                Ok(updated)
            }
            CostConsentStatus::Revoked => {
                tx.commit().await?;
                Ok(consent)
            }
            CostConsentStatus::Halted => Err(StoreError::Conflict {
                code: "invalid_transition",
            }),
        }
    }

    pub async fn acquire_worker_lease(&self, owner_id: &str, ttl_secs: i64) -> Result<WorkerLease> {
        path_id(owner_id)?;
        check(
            (1..=3600).contains(&ttl_secs),
            "invalid_schema",
            "/ttl_secs",
        )?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now_utc = now(&mut tx).await?;
        let existing = sqlx::query("SELECT * FROM worker_leases WHERE id=1")
            .fetch_optional(&mut *tx)
            .await?;
        let next_epoch = if let Some(row) = existing {
            let cur_owner: String = row.try_get("owner_id")?;
            let cur_epoch: i64 = row.try_get("lease_epoch")?;
            let cur_expires: String = row.try_get("expires_at")?;
            if now_utc < cur_expires && cur_owner != owner_id {
                return Err(StoreError::Forbidden {
                    code: "reservation_ownership_required",
                });
            }
            if cur_owner == owner_id && now_utc < cur_expires {
                cur_epoch
            } else {
                cur_epoch.checked_add(1).ok_or(StoreError::Internal)?
            }
        } else {
            1
        };
        let expires_at = now_plus_seconds(&mut tx, ttl_secs).await?;
        sqlx::query(
            "INSERT INTO worker_leases(id,owner_id,lease_epoch,acquired_at,heartbeat_at,expires_at) VALUES(1,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET owner_id=excluded.owner_id,lease_epoch=excluded.lease_epoch,acquired_at=excluded.acquired_at,heartbeat_at=excluded.heartbeat_at,expires_at=excluded.expires_at",
        )
        .bind(owner_id)
        .bind(next_epoch)
        .bind(&now_utc)
        .bind(&now_utc)
        .bind(&expires_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(WorkerLease {
            owner_id: owner_id.to_owned(),
            lease_epoch: next_epoch,
            acquired_at: now_utc.clone(),
            heartbeat_at: now_utc,
            expires_at,
        })
    }

    pub async fn reserve_budget(
        &self,
        idempotency_key: Option<&str>,
        input: ReserveBudgetRequest,
    ) -> Result<(BudgetReservation, bool)> {
        if let Some(key) = idempotency_key {
            path_id(key)?;
        }
        path_id(&input.cost_consent_id)?;
        path_id(&input.owner_id)?;
        if let Some(pid) = input.project_id.as_deref() {
            path_id(pid)?;
        }
        if let Some(jid) = input.job_id.as_deref() {
            path_id(jid)?;
        }
        check(
            name(&input.provider_id) && name(&input.model_id),
            "invalid_model_selection",
            "/model_selection",
        )?;
        check(input.lease_epoch >= 1, "invalid_schema", "/lease_epoch")?;
        check(
            input.estimated_input_tokens >= 0 && input.max_output_tokens >= 0,
            "invalid_schema",
            "/tokens",
        )?;

        let request_sha256 = digest(&canonical(&input)?);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now_utc = now(&mut tx).await?;
        require_verified_worker_lease(&mut tx, &now_utc, &input.owner_id, input.lease_epoch)
            .await?;

        if let Some(key) = idempotency_key {
            let existing_key = sqlx::query(
                "SELECT request_sha256 FROM ledger_idempotency WHERE operation='reserve_budget' AND idempotency_key=?",
            )
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(key_row) = existing_key {
                if key_row.try_get::<String, _>("request_sha256")? != request_sha256 {
                    return Err(StoreError::IdempotencyConflict);
                }
                let live_row =
                    sqlx::query("SELECT * FROM budget_reservations WHERE idempotency_key=?")
                        .bind(key)
                        .fetch_optional(&mut *tx)
                        .await?
                        .ok_or(StoreError::Internal)?;
                let mut live = reservation_row_with_dispatch(live_row, false)?;
                if live.owner_id != input.owner_id || live.lease_epoch != input.lease_epoch {
                    return Err(StoreError::Forbidden {
                        code: "reservation_ownership_required",
                    });
                }
                if live.status == ReservationStatus::Reserved {
                    sqlx::query("UPDATE budget_reservations SET replay_blocked=1 WHERE id=? AND status='reserved'")
                        .bind(&live.id)
                        .execute(&mut *tx)
                        .await?;
                }
                // Replays NEVER authorize another provider dispatch.
                live.dispatch_authorized = false;
                tx.commit().await?;
                return Ok((live, true));
            }
        }

        let row = sqlx::query("SELECT * FROM cost_consents WHERE id=?")
            .bind(&input.cost_consent_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::Conflict {
                code: "remote_cost_not_approved",
            })?;
        let consent = cost_consent_row(row)?;
        if consent.status == CostConsentStatus::Halted {
            let code = match consent.halt_reason.as_deref() {
                Some("cost_limit_exceeded") => "cost_limit_exceeded",
                _ => "cost_reconciliation_failed",
            };
            return Err(StoreError::Conflict { code });
        }
        if consent.status != CostConsentStatus::Active
            || now_utc >= consent.expires_at
            || consent
                .project_id
                .as_deref()
                .is_some_and(|bound| input.project_id.as_deref() != Some(bound))
            || consent.provider_id != input.provider_id
            || consent.model_id != input.model_id
            || !consent.scopes.contains(&input.scope)
        {
            return Err(StoreError::Conflict {
                code: "remote_cost_not_approved",
            });
        }
        verify_consent_pricing_freshness(&mut tx, &consent).await?;

        let in_toks_u64 =
            u64::try_from(input.estimated_input_tokens).map_err(|_| StoreError::Conflict {
                code: "cost_limit_exceeded",
            })?;
        let out_toks_u64 =
            u64::try_from(input.max_output_tokens).map_err(|_| StoreError::Conflict {
                code: "cost_limit_exceeded",
            })?;
        let reserve_micros = compute_usage_cost_micros(&consent.pricing, in_toks_u64, out_toks_u64)
            .map_err(|err| match err {
                CostError::PricingUnavailable => StoreError::Conflict {
                    code: "provider_pricing_unavailable",
                },
                CostError::CostLimitExceeded => StoreError::Conflict {
                    code: "cost_limit_exceeded",
                },
            })?;
        let reserve_tokens = input
            .estimated_input_tokens
            .checked_add(input.max_output_tokens)
            .ok_or(StoreError::Conflict {
                code: "cost_limit_exceeded",
            })?;

        let new_reserved_micros =
            consent
                .reserved_micros
                .checked_add(reserve_micros)
                .ok_or(StoreError::Conflict {
                    code: "cost_limit_exceeded",
                })?;
        let total_committed_micros = consent
            .spent_micros
            .checked_add(new_reserved_micros)
            .ok_or(StoreError::Conflict {
                code: "cost_limit_exceeded",
            })?;
        if total_committed_micros > consent.max_cost_micros {
            return Err(StoreError::Conflict {
                code: "cost_limit_exceeded",
            });
        }

        let new_reserved_tokens =
            consent
                .reserved_tokens
                .checked_add(reserve_tokens)
                .ok_or(StoreError::Conflict {
                    code: "cost_limit_exceeded",
                })?;
        if let Some(max_tok) = consent.max_total_tokens {
            let total_tokens = consent
                .spent_tokens
                .checked_add(new_reserved_tokens)
                .ok_or(StoreError::Conflict {
                    code: "cost_limit_exceeded",
                })?;
            if total_tokens > max_tok {
                return Err(StoreError::Conflict {
                    code: "cost_limit_exceeded",
                });
            }
        }

        let res = BudgetReservation {
            schema_version: COST_SCHEMA_VERSION.into(),
            id: Uuid::new_v4().to_string(),
            cost_consent_id: consent.id.clone(),
            project_id: input.project_id,
            job_id: input.job_id,
            provider_id: input.provider_id,
            model_id: input.model_id,
            scope: input.scope,
            owner_id: input.owner_id,
            lease_epoch: input.lease_epoch,
            estimated_input_tokens: input.estimated_input_tokens,
            max_output_tokens: input.max_output_tokens,
            reserved_micros: reserve_micros,
            reserved_tokens: reserve_tokens,
            actual_input_tokens: None,
            actual_output_tokens: None,
            actual_micros: None,
            overage_micros: 0,
            pricing: consent.pricing,
            pricing_sha256: consent.pricing_sha256,
            status: ReservationStatus::Reserved,
            dispatch_claimed: false,
            dispatch_claimed_at: None,
            dispatch_authorized: true,
            error_code: None,
            created_at: now_utc,
            settled_at: None,
        };

        sqlx::query(
            "INSERT INTO budget_reservations(id,cost_consent_id,project_id,job_id,provider_id,model_id,scope,owner_id,lease_epoch,estimated_input_tokens,max_output_tokens,reserved_micros,reserved_tokens,actual_input_tokens,actual_output_tokens,actual_micros,overage_micros,pricing_snapshot_json,pricing_sha256,status,error_code,created_at,settled_at,idempotency_key,dispatch_claimed,dispatch_claimed_at,replay_blocked) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,NULL,NULL,NULL,0,?,?,'reserved',NULL,?,NULL,?,0,NULL,0)",
        )
        .bind(&res.id)
        .bind(&res.cost_consent_id)
        .bind(&res.project_id)
        .bind(&res.job_id)
        .bind(&res.provider_id)
        .bind(&res.model_id)
        .bind(res.scope.as_str())
        .bind(&res.owner_id)
        .bind(res.lease_epoch)
        .bind(res.estimated_input_tokens)
        .bind(res.max_output_tokens)
        .bind(res.reserved_micros)
        .bind(res.reserved_tokens)
        .bind(json(&res.pricing)?)
        .bind(&res.pricing_sha256)
        .bind(&res.created_at)
        .bind(idempotency_key)
        .execute(&mut *tx)
        .await?;

        sqlx::query("UPDATE cost_consents SET reserved_micros=?,reserved_tokens=? WHERE id=?")
            .bind(new_reserved_micros)
            .bind(new_reserved_tokens)
            .bind(&res.cost_consent_id)
            .execute(&mut *tx)
            .await?;

        if let Some(key) = idempotency_key {
            remember_ledger(&mut tx, "reserve_budget", key, &request_sha256, &res).await?;
        }
        tx.commit().await?;
        Ok((res, false))
    }

    /// Consumes the single-use provider dispatch permit on an active reservation.
    /// Rejects missing/expired/deposed leases, replayed reservations, settled reservations, and stale pricing.
    pub async fn claim_reservation_dispatch(
        &self,
        reservation_id: &str,
        owner_id: &str,
        lease_epoch: i64,
    ) -> Result<BudgetReservation> {
        path_id(reservation_id)?;
        path_id(owner_id)?;
        check(lease_epoch >= 1, "invalid_schema", "/lease_epoch")?;

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now_utc = now(&mut tx).await?;
        require_verified_worker_lease(&mut tx, &now_utc, owner_id, lease_epoch).await?;

        let row = sqlx::query("SELECT * FROM budget_reservations WHERE id=?")
            .bind(reservation_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        let replay_blocked: bool = row.try_get::<i64, _>("replay_blocked")? != 0;
        let res = reservation_row(row)?;
        if res.owner_id != owner_id || res.lease_epoch != lease_epoch {
            return Err(StoreError::Forbidden {
                code: "reservation_ownership_required",
            });
        }
        if res.status != ReservationStatus::Reserved || res.dispatch_claimed || replay_blocked {
            return Err(StoreError::Conflict {
                code: "cost_reconciliation_failed",
            });
        }

        let consent = cost_consent_row(
            sqlx::query("SELECT * FROM cost_consents WHERE id=?")
                .bind(&res.cost_consent_id)
                .fetch_one(&mut *tx)
                .await?,
        )?;
        if consent.status != CostConsentStatus::Active || now_utc >= consent.expires_at {
            return Err(StoreError::Conflict {
                code: "remote_cost_not_approved",
            });
        }
        verify_consent_pricing_freshness(&mut tx, &consent).await?;

        let affected = sqlx::query(
            "UPDATE budget_reservations SET dispatch_claimed=1,dispatch_claimed_at=? WHERE id=? AND status='reserved' AND dispatch_claimed=0 AND replay_blocked=0",
        )
        .bind(&now_utc)
        .bind(reservation_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if affected != 1 {
            return Err(StoreError::Conflict {
                code: "cost_reconciliation_failed",
            });
        }
        let updated_row = sqlx::query("SELECT * FROM budget_reservations WHERE id=?")
            .bind(reservation_id)
            .fetch_one(&mut *tx)
            .await?;
        let updated = reservation_row_with_dispatch(updated_row, true)?;
        tx.commit().await?;
        Ok(updated)
    }

    /// Cancels an active reservation whose single-use dispatch permit has never been claimed
    /// and was never blocked by replay, returning reserved budget/tokens cleanly to the consent.
    pub async fn cancel_unclaimed_reservation(
        &self,
        reservation_id: &str,
        owner_id: &str,
        lease_epoch: i64,
    ) -> Result<BudgetReservation> {
        path_id(reservation_id)?;
        path_id(owner_id)?;
        check(lease_epoch >= 1, "invalid_schema", "/lease_epoch")?;

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now_utc = now(&mut tx).await?;
        require_verified_worker_lease(&mut tx, &now_utc, owner_id, lease_epoch).await?;

        let row = sqlx::query("SELECT * FROM budget_reservations WHERE id=?")
            .bind(reservation_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        let replay_blocked: bool = row.try_get::<i64, _>("replay_blocked")? != 0;
        let res = reservation_row(row)?;
        if res.owner_id != owner_id || res.lease_epoch != lease_epoch {
            return Err(StoreError::Forbidden {
                code: "reservation_ownership_required",
            });
        }
        if res.status != ReservationStatus::Reserved || res.dispatch_claimed || replay_blocked {
            return Err(StoreError::Conflict {
                code: "cost_reconciliation_failed",
            });
        }

        let consent = cost_consent_row(
            sqlx::query("SELECT * FROM cost_consents WHERE id=?")
                .bind(&res.cost_consent_id)
                .fetch_one(&mut *tx)
                .await?,
        )?;
        let released_reserved_micros = consent
            .reserved_micros
            .checked_sub(res.reserved_micros)
            .ok_or(StoreError::Internal)?;
        let released_reserved_tokens = consent
            .reserved_tokens
            .checked_sub(res.reserved_tokens)
            .ok_or(StoreError::Internal)?;

        let affected = sqlx::query(
            "UPDATE budget_reservations SET status='settled',actual_input_tokens=0,actual_output_tokens=0,actual_micros=0,overage_micros=0,error_code='request_cancelled',settled_at=? WHERE id=? AND status='reserved' AND dispatch_claimed=0 AND replay_blocked=0",
        )
        .bind(&now_utc)
        .bind(&res.id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if affected != 1 {
            return Err(StoreError::Conflict {
                code: "cost_reconciliation_failed",
            });
        }

        sqlx::query("UPDATE cost_consents SET reserved_micros=?,reserved_tokens=? WHERE id=?")
            .bind(released_reserved_micros)
            .bind(released_reserved_tokens)
            .bind(&res.cost_consent_id)
            .execute(&mut *tx)
            .await?;

        let updated = reservation_row(
            sqlx::query("SELECT * FROM budget_reservations WHERE id=?")
                .bind(&res.id)
                .fetch_one(&mut *tx)
                .await?,
        )?;
        tx.commit().await?;
        Ok(updated)
    }

    pub async fn reconcile_budget_reservation(
        &self,
        idempotency_key: Option<&str>,
        input: ReconcileBudgetRequest,
    ) -> Result<(BudgetReservation, bool)> {
        if let Some(key) = idempotency_key {
            path_id(key)?;
        }
        path_id(&input.reservation_id)?;
        path_id(&input.owner_id)?;
        check(input.lease_epoch >= 1, "invalid_schema", "/lease_epoch")?;

        let request_sha256 = digest(&canonical(&input)?);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(key) = idempotency_key {
            if let Some(res) = replay_ledger::<BudgetReservation>(
                &mut tx,
                "reconcile_budget",
                key,
                &request_sha256,
            )
            .await?
            {
                tx.commit().await?;
                return Ok((res, true));
            }
        }

        let res = reservation_row(
            sqlx::query("SELECT * FROM budget_reservations WHERE id=?")
                .bind(&input.reservation_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(StoreError::NotFound)?,
        )?;

        let now_utc = now(&mut tx).await?;
        // Strict verified ownership gate: require active, unexpired lease matching exact owner & epoch.
        require_verified_worker_lease(&mut tx, &now_utc, &input.owner_id, input.lease_epoch)
            .await?;
        if res.owner_id != input.owner_id || res.lease_epoch != input.lease_epoch {
            return Err(StoreError::Forbidden {
                code: "reservation_ownership_required",
            });
        }

        // Exactly-once reconciliation gate: already-settled reservations cannot re-settle.
        if res.status != ReservationStatus::Reserved {
            return Err(StoreError::Conflict {
                code: "invalid_transition",
            });
        }

        let consent = cost_consent_row(
            sqlx::query("SELECT * FROM cost_consents WHERE id=?")
                .bind(&res.cost_consent_id)
                .fetch_one(&mut *tx)
                .await?,
        )?;
        let released_reserved_micros = consent
            .reserved_micros
            .checked_sub(res.reserved_micros)
            .ok_or(StoreError::Internal)?;
        let released_reserved_tokens = consent
            .reserved_tokens
            .checked_sub(res.reserved_tokens)
            .ok_or(StoreError::Internal)?;

        let (
            new_status,
            actual_in,
            actual_out,
            actual_micros_val,
            overage_micros,
            error_code,
            new_spent_micros,
            new_spent_tokens,
            halt_reason,
        ) = match input.usage {
            ProviderUsageReport::MissingOrUnverifiable => (
                ReservationStatus::SettledConservative,
                None,
                None,
                Some(res.reserved_micros),
                0i64,
                Some("cost_reconciliation_failed".to_owned()),
                consent.spent_micros.saturating_add(res.reserved_micros),
                consent.spent_tokens.saturating_add(res.reserved_tokens),
                Some("cost_reconciliation_failed"),
            ),
            ProviderUsageReport::Verified {
                input_tokens,
                output_tokens,
            } => {
                let in_tok_i64 = i64::try_from(input_tokens);
                let out_tok_i64 = i64::try_from(output_tokens);
                let actual_tokens = input_tokens
                    .checked_add(output_tokens)
                    .and_then(|t| i64::try_from(t).ok());
                let cost_res = compute_usage_cost_micros(&res.pricing, input_tokens, output_tokens);
                match (cost_res, actual_tokens, in_tok_i64, out_tok_i64) {
                    (Ok(actual_cost), Some(tok_sum), Ok(in_i), Ok(out_i)) => {
                        let spent_plus = consent.spent_micros.checked_add(actual_cost);
                        let tokens_plus = consent.spent_tokens.checked_add(tok_sum);
                        let over_reserved = actual_cost > res.reserved_micros;
                        match (spent_plus, tokens_plus) {
                            (Some(next_spent), Some(next_tokens))
                                if !over_reserved
                                    && next_spent
                                        .checked_add(released_reserved_micros)
                                        .is_some_and(|tot| tot <= consent.max_cost_micros)
                                    && consent.max_total_tokens.is_none_or(|cap| {
                                        next_tokens
                                            .checked_add(released_reserved_tokens)
                                            .is_some_and(|tot| tot <= cap)
                                    }) =>
                            {
                                (
                                    ReservationStatus::Settled,
                                    Some(in_i),
                                    Some(out_i),
                                    Some(actual_cost),
                                    0i64,
                                    None,
                                    next_spent,
                                    next_tokens,
                                    None,
                                )
                            }
                            _ => {
                                let overage =
                                    actual_cost.saturating_sub(res.reserved_micros).max(0);
                                (
                                    ReservationStatus::SettledOverage,
                                    Some(in_i),
                                    Some(out_i),
                                    Some(actual_cost),
                                    overage,
                                    Some("cost_limit_exceeded".to_owned()),
                                    spent_plus.unwrap_or(i64::MAX),
                                    tokens_plus.unwrap_or(i64::MAX),
                                    Some("cost_limit_exceeded"),
                                )
                            }
                        }
                    }
                    _ => {
                        let saturating_tok = input_tokens
                            .saturating_add(output_tokens)
                            .min(i64::MAX as u64)
                            as i64;
                        (
                            ReservationStatus::SettledOverage,
                            Some(input_tokens.min(i64::MAX as u64) as i64),
                            Some(output_tokens.min(i64::MAX as u64) as i64),
                            None,
                            i64::MAX.saturating_sub(res.reserved_micros),
                            Some("cost_limit_exceeded".to_owned()),
                            i64::MAX,
                            consent.spent_tokens.saturating_add(saturating_tok),
                            Some("cost_limit_exceeded"),
                        )
                    }
                }
            }
        };

        sqlx::query(
            "UPDATE budget_reservations SET status=?,actual_input_tokens=?,actual_output_tokens=?,actual_micros=?,overage_micros=?,error_code=?,settled_at=? WHERE id=? AND status='reserved'",
        )
        .bind(new_status.as_str())
        .bind(actual_in)
        .bind(actual_out)
        .bind(actual_micros_val)
        .bind(overage_micros)
        .bind(&error_code)
        .bind(&now_utc)
        .bind(&res.id)
        .execute(&mut *tx)
        .await?;

        if let Some(reason) = halt_reason {
            sqlx::query(
                "UPDATE cost_consents SET reserved_micros=?,reserved_tokens=?,spent_micros=?,spent_tokens=?,status=CASE WHEN status='revoked' THEN 'revoked' ELSE 'halted' END,halt_reason=COALESCE(halt_reason, ?) WHERE id=?",
            )
            .bind(released_reserved_micros)
            .bind(released_reserved_tokens)
            .bind(new_spent_micros)
            .bind(new_spent_tokens)
            .bind(reason)
            .bind(&res.cost_consent_id)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query(
                "UPDATE cost_consents SET reserved_micros=?,reserved_tokens=?,spent_micros=?,spent_tokens=? WHERE id=?",
            )
            .bind(released_reserved_micros)
            .bind(released_reserved_tokens)
            .bind(new_spent_micros)
            .bind(new_spent_tokens)
            .bind(&res.cost_consent_id)
            .execute(&mut *tx)
            .await?;
        }

        let updated = reservation_row(
            sqlx::query("SELECT * FROM budget_reservations WHERE id=?")
                .bind(&res.id)
                .fetch_one(&mut *tx)
                .await?,
        )?;
        if let Some(key) = idempotency_key {
            remember_ledger(&mut tx, "reconcile_budget", key, &request_sha256, &updated).await?;
        }
        tx.commit().await?;
        Ok((updated, false))
    }

    /// Settles ONLY stale/orphaned open reservations (`owner_id != active.owner_id OR lease_epoch < active.lease_epoch`)
    /// conservatively at `reserved_micros`. Fails closed unless caller holds the verified active `worker_leases` lock.
    pub async fn recover_orphaned_reservations(
        &self,
        owner_id: &str,
        lease_epoch: i64,
    ) -> Result<Vec<BudgetReservation>> {
        path_id(owner_id)?;
        check(lease_epoch >= 1, "invalid_schema", "/lease_epoch")?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now_utc = now(&mut tx).await?;
        let active =
            active_worker_lease(&mut tx, &now_utc)
                .await?
                .ok_or(StoreError::Forbidden {
                    code: "reservation_ownership_required",
                })?;
        if active.owner_id != owner_id || active.lease_epoch != lease_epoch {
            return Err(StoreError::Forbidden {
                code: "reservation_ownership_required",
            });
        }

        let rows = sqlx::query(
            "SELECT * FROM budget_reservations WHERE status='reserved' AND (owner_id != ? OR lease_epoch < ?) ORDER BY created_at, id",
        )
        .bind(owner_id)
        .bind(lease_epoch)
        .fetch_all(&mut *tx)
        .await?;

        let mut settled = Vec::with_capacity(rows.len());
        for row in rows {
            let res = reservation_row(row)?;
            let consent = cost_consent_row(
                sqlx::query("SELECT * FROM cost_consents WHERE id=?")
                    .bind(&res.cost_consent_id)
                    .fetch_one(&mut *tx)
                    .await?,
            )?;
            let released_micros = consent
                .reserved_micros
                .checked_sub(res.reserved_micros)
                .ok_or(StoreError::Internal)?;
            let released_tokens = consent
                .reserved_tokens
                .checked_sub(res.reserved_tokens)
                .ok_or(StoreError::Internal)?;
            let new_spent_micros = consent.spent_micros.saturating_add(res.reserved_micros);
            let new_spent_tokens = consent.spent_tokens.saturating_add(res.reserved_tokens);

            sqlx::query(
                "UPDATE budget_reservations SET status='settled_conservative',actual_micros=?,overage_micros=0,error_code='cost_reconciliation_failed',settled_at=? WHERE id=? AND status='reserved'",
            )
            .bind(res.reserved_micros)
            .bind(&now_utc)
            .bind(&res.id)
            .execute(&mut *tx)
            .await?;

            sqlx::query(
                "UPDATE cost_consents SET reserved_micros=?,reserved_tokens=?,spent_micros=?,spent_tokens=?,status=CASE WHEN status='revoked' THEN 'revoked' ELSE 'halted' END,halt_reason=COALESCE(halt_reason, 'cost_reconciliation_failed') WHERE id=?",
            )
            .bind(released_micros)
            .bind(released_tokens)
            .bind(new_spent_micros)
            .bind(new_spent_tokens)
            .bind(&res.cost_consent_id)
            .execute(&mut *tx)
            .await?;

            settled.push(reservation_row(
                sqlx::query("SELECT * FROM budget_reservations WHERE id=?")
                    .bind(&res.id)
                    .fetch_one(&mut *tx)
                    .await?,
            )?);
        }
        tx.commit().await?;
        Ok(settled)
    }

    pub async fn get_budget_reservation(&self, reservation_id: &str) -> Result<BudgetReservation> {
        path_id(reservation_id)?;
        let row = sqlx::query("SELECT * FROM budget_reservations WHERE id=?")
            .bind(reservation_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        reservation_row(row)
    }

    pub async fn check_revision_cost_gate(
        &self,
        project_id: &str,
        revision_id: &str,
        cost_consent_id: Option<&str>,
        scope: CostScope,
    ) -> Result<CostGateDecision> {
        path_id(project_id)?;
        path_id(revision_id)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let revision = read_revision(&mut tx, project_id, revision_id).await?;
        let decision = verify_cost_consent_for_remote_gate(
            &mut tx,
            project_id,
            &revision,
            cost_consent_id,
            scope,
        )
        .await?;
        tx.commit().await?;
        Ok(decision)
    }

    /// Host-owned audit status transition for an already-imported skill bundle.
    pub async fn record_host_skill_audit(
        &self,
        skill_id: &str,
        skill_version: &str,
        bundle_sha256: &str,
        audit_status: AuditStatus,
        audit_evidence_json: Option<&str>,
    ) -> Result<SkillRecord> {
        check(is_safe_skill_id(skill_id), "invalid_schema", "/skill_id")?;
        check(
            is_safe_skill_version(skill_version),
            "invalid_schema",
            "/skill_version",
        )?;
        check(hash(bundle_sha256), "invalid_schema", "/bundle_sha256")?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing =
            sqlx::query("SELECT * FROM skill_bundles WHERE skill_id=? AND skill_version=?")
                .bind(skill_id)
                .bind(skill_version)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(StoreError::NotFound)?;
        let rec = skill_row(existing)?;
        if rec.bundle_sha256 != bundle_sha256 {
            return Err(StoreError::Conflict {
                code: "hash_mismatch",
            });
        }
        sqlx::query(
            "UPDATE skill_bundles SET audit_status=?,audit_evidence_json=? WHERE skill_id=? AND skill_version=?",
        )
        .bind(audit_status.as_str())
        .bind(audit_evidence_json)
        .bind(skill_id)
        .bind(skill_version)
        .execute(&mut *tx)
        .await?;
        let updated = skill_row(
            sqlx::query("SELECT * FROM skill_bundles WHERE skill_id=? AND skill_version=?")
                .bind(skill_id)
                .bind(skill_version)
                .fetch_one(&mut *tx)
                .await?,
        )?;
        tx.commit().await?;
        Ok(updated)
    }

    pub async fn issue_execution_authorization(
        &self,
        skill_store_dir: &Path,
        asset_store_dir: &Path,
        idempotency_key: Option<&str>,
        input: IssueExecutionAuthorization,
    ) -> Result<(ExecutionAuthorization, bool)> {
        if let Some(key) = idempotency_key {
            path_id(key)?;
        }
        version(&input.schema_version)?;
        path_id(&input.project_id)?;
        path_id(&input.revision_id)?;
        check(
            hash(&input.content_sha256),
            "invalid_schema",
            "/content_sha256",
        )?;
        check(
            is_safe_skill_id(&input.skill_id),
            "invalid_schema",
            "/skill_id",
        )?;
        check(
            is_safe_skill_version(&input.skill_version),
            "invalid_schema",
            "/skill_version",
        )?;
        check(
            hash(&input.bundle_sha256),
            "invalid_schema",
            "/bundle_sha256",
        )?;
        check(
            hash(&input.runtime_policy_sha256),
            "invalid_schema",
            "/runtime_policy_sha256",
        )?;
        check(
            is_valid_utc_timestamp(&input.expires_at),
            "invalid_schema",
            "/expires_at",
        )?;

        let request_sha256 = digest(&canonical(&input)?);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(key) = idempotency_key {
            if let Some(auth) = replay_ledger::<ExecutionAuthorization>(
                &mut tx,
                "issue_execution_auth",
                key,
                &request_sha256,
            )
            .await?
            {
                tx.commit().await?;
                return Ok((auth, true));
            }
        }

        let project = read_project(&mut tx, &input.project_id).await?;
        if project.current_revision_id.as_deref() != Some(&input.revision_id) {
            return Err(StoreError::StaleRevision);
        }
        if project.approved_revision_id.as_deref() != Some(&input.revision_id) {
            return Err(StoreError::Conflict {
                code: "approval_required",
            });
        }
        let revision = read_revision(&mut tx, &input.project_id, &input.revision_id).await?;
        if revision.content_sha256 != input.content_sha256 {
            return Err(StoreError::Conflict {
                code: "hash_mismatch",
            });
        }
        let approval = read_approval(
            &mut tx,
            &input.project_id,
            &input.revision_id,
            &input.content_sha256,
        )
        .await?;
        if digest(&canonical(&revision.content)?) != revision.content_sha256
            || approval.revision_id != revision.id
            || approval.content_sha256 != revision.content_sha256
        {
            return Err(StoreError::Conflict {
                code: "hash_mismatch",
            });
        }
        if revision.content.skill_id != input.skill_id
            || revision.content.skill_version != input.skill_version
        {
            return Err(StoreError::Conflict {
                code: "execution_not_authorized",
            });
        }
        validate_registered_model_for_snapshot(
            &mut tx,
            "storyboard_planning",
            &revision.content.models.storyboard_planning,
        )
        .await?;
        validate_registered_model_for_snapshot(
            &mut tx,
            "scene_code_generation",
            &revision.content.models.scene_code_generation,
        )
        .await?;
        // Production authorization rejects synthetic narration, requires exact SFX hash binding,
        // and verifies stored asset bytes on disk before issuance.
        verify_revision_assets_in_tx(&mut tx, &revision, true, Some(asset_store_dir)).await?;

        let skill_row_opt =
            sqlx::query("SELECT * FROM skill_bundles WHERE skill_id=? AND skill_version=?")
                .bind(&input.skill_id)
                .bind(&input.skill_version)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(skill_sql) = skill_row_opt else {
            return Err(StoreError::Conflict {
                code: "skill_unavailable",
            });
        };
        let skill = skill_row(skill_sql)?;
        // Static audit status alone is NEVER treated as verified production execution readiness.
        if !skill.enabled || skill.audit_status != AuditStatus::SmokeVerified {
            return Err(StoreError::Conflict {
                code: "skill_unavailable",
            });
        }
        if skill.bundle_sha256 != input.bundle_sha256 {
            return Err(StoreError::Conflict {
                code: "execution_not_authorized",
            });
        }
        verify_stored_bundle(skill_store_dir, &skill).map_err(map_skill_error)?;

        let issued_at = now(&mut tx).await?;
        check(
            issued_at < input.expires_at,
            "invalid_schema",
            "/expires_at",
        )?;

        let auth = ExecutionAuthorization {
            schema_version: EXECUTION_AUTH_SCHEMA_VERSION.into(),
            id: Uuid::new_v4().to_string(),
            project_id: input.project_id,
            revision_id: input.revision_id,
            approval_id: approval.id,
            content_sha256: input.content_sha256,
            skill_id: input.skill_id,
            skill_version: input.skill_version,
            bundle_sha256: input.bundle_sha256,
            runtime_policy_sha256: input.runtime_policy_sha256,
            status: ExecutionAuthStatus::Issued,
            claimed_job_id: None,
            issued_at: issued_at.clone(),
            expires_at: input.expires_at,
            claimed_at: None,
        };
        sqlx::query(
            "INSERT INTO execution_authorizations(id,project_id,revision_id,approval_id,content_sha256,skill_id,skill_version,bundle_sha256,runtime_policy_sha256,status,claimed_job_id,issued_at,expires_at,claimed_at) VALUES(?,?,?,?,?,?,?,?,?,'issued',NULL,?,?,NULL)",
        )
        .bind(&auth.id)
        .bind(&auth.project_id)
        .bind(&auth.revision_id)
        .bind(&auth.approval_id)
        .bind(&auth.content_sha256)
        .bind(&auth.skill_id)
        .bind(&auth.skill_version)
        .bind(&auth.bundle_sha256)
        .bind(&auth.runtime_policy_sha256)
        .bind(&auth.issued_at)
        .bind(&auth.expires_at)
        .execute(&mut *tx)
        .await?;

        if let Some(key) = idempotency_key {
            remember_ledger(&mut tx, "issue_execution_auth", key, &request_sha256, &auth).await?;
        }
        tx.commit().await?;
        Ok((auth, false))
    }

    pub async fn get_execution_authorization(
        &self,
        auth_id: &str,
    ) -> Result<ExecutionAuthorization> {
        path_id(auth_id)?;
        let row = sqlx::query("SELECT * FROM execution_authorizations WHERE id=?")
            .bind(auth_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        execution_auth_row(row)
    }

    /// Atomically claims a single-use authorization bound to an actual matching `render_jobs` record,
    /// re-verifying stored skill/asset integrity, smoke-verified audit readiness, and approval state.
    pub async fn claim_execution_authorization(
        &self,
        skill_store_dir: &Path,
        asset_store_dir: &Path,
        auth_id: &str,
        input: ClaimExecutionAuthorization,
    ) -> Result<ExecutionAuthorization> {
        path_id(auth_id)?;
        version(&input.schema_version)?;
        path_id(&input.job_id)?;
        path_id(&input.project_id)?;
        path_id(&input.revision_id)?;
        check(
            hash(&input.content_sha256),
            "invalid_schema",
            "/content_sha256",
        )?;
        check(
            is_safe_skill_id(&input.skill_id),
            "invalid_schema",
            "/skill_id",
        )?;
        check(
            is_safe_skill_version(&input.skill_version),
            "invalid_schema",
            "/skill_version",
        )?;
        check(
            hash(&input.bundle_sha256),
            "invalid_schema",
            "/bundle_sha256",
        )?;
        check(
            hash(&input.runtime_policy_sha256),
            "invalid_schema",
            "/runtime_policy_sha256",
        )?;

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let auth = Self::claim_execution_authorization_in_tx(
            &mut tx,
            skill_store_dir,
            asset_store_dir,
            auth_id,
            &input,
        )
        .await?;
        tx.commit().await?;
        Ok(auth)
    }

    pub async fn claim_execution_authorization_in_tx(
        tx: &mut SqliteConnection,
        skill_store_dir: &Path,
        asset_store_dir: &Path,
        auth_id: &str,
        input: &ClaimExecutionAuthorization,
    ) -> Result<ExecutionAuthorization> {
        let now_utc = now(&mut *tx).await?;
        let row = sqlx::query("SELECT * FROM execution_authorizations WHERE id=?")
            .bind(auth_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::Conflict {
                code: "execution_not_authorized",
            })?;
        let auth = execution_auth_row(row)?;
        if auth.status != ExecutionAuthStatus::Issued
            || auth.claimed_job_id.is_some()
            || now_utc >= auth.expires_at
            || auth.project_id != input.project_id
            || auth.revision_id != input.revision_id
            || auth.content_sha256 != input.content_sha256
            || auth.skill_id != input.skill_id
            || auth.skill_version != input.skill_version
            || auth.bundle_sha256 != input.bundle_sha256
            || auth.runtime_policy_sha256 != input.runtime_policy_sha256
        {
            return Err(StoreError::Conflict {
                code: "execution_not_authorized",
            });
        }

        // Prevent cross-job reuse of an already claimed job_id.
        if sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM execution_authorizations WHERE claimed_job_id=?",
        )
        .bind(&input.job_id)
        .fetch_one(&mut *tx)
        .await?
            > 0
        {
            return Err(StoreError::Conflict {
                code: "execution_not_authorized",
            });
        }

        // Future claims must bind atomically to an actual matching render job row.
        let matching_job = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM render_jobs WHERE id=? AND project_id=? AND revision_id=? AND approval_id=? AND content_sha256=?",
        )
        .bind(&input.job_id)
        .bind(&auth.project_id)
        .bind(&auth.revision_id)
        .bind(&auth.approval_id)
        .bind(&auth.content_sha256)
        .fetch_one(&mut *tx)
        .await?;
        if matching_job == 0 {
            return Err(StoreError::Conflict {
                code: "execution_not_authorized",
            });
        }

        // Re-verify project/revision/approval pointer, assets, models, and skill integrity.
        let project = read_project(&mut *tx, &auth.project_id).await?;
        if project.current_revision_id.as_deref() != Some(&auth.revision_id)
            || project.approved_revision_id.as_deref() != Some(&auth.revision_id)
        {
            return Err(StoreError::Conflict {
                code: "execution_not_authorized",
            });
        }
        let revision = read_revision(&mut *tx, &auth.project_id, &auth.revision_id).await?;
        verify_revision_assets_in_tx(&mut *tx, &revision, true, Some(asset_store_dir)).await?;

        let skill_row_opt =
            sqlx::query("SELECT * FROM skill_bundles WHERE skill_id=? AND skill_version=?")
                .bind(&auth.skill_id)
                .bind(&auth.skill_version)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(skill_sql) = skill_row_opt else {
            return Err(StoreError::Conflict {
                code: "skill_unavailable",
            });
        };
        let skill = skill_row(skill_sql)?;
        if !skill.enabled
            || skill.bundle_sha256 != auth.bundle_sha256
            || skill.audit_status != AuditStatus::SmokeVerified
        {
            return Err(StoreError::Conflict {
                code: "skill_unavailable",
            });
        }
        verify_stored_bundle(skill_store_dir, &skill).map_err(map_skill_error)?;

        let affected = sqlx::query(
            "UPDATE execution_authorizations SET status='claimed',claimed_job_id=?,claimed_at=? WHERE id=? AND status='issued' AND claimed_job_id IS NULL",
        )
        .bind(&input.job_id)
        .bind(&now_utc)
        .bind(auth_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if affected != 1 {
            return Err(StoreError::Conflict {
                code: "execution_not_authorized",
            });
        }

        let updated = execution_auth_row(
            sqlx::query("SELECT * FROM execution_authorizations WHERE id=?")
                .bind(auth_id)
                .fetch_one(&mut *tx)
                .await?,
        )?;
        Ok(updated)
    }

    /// Records separated post-execution workspace evidence once for an already-claimed authorization,
    /// requiring an active verified worker lease and non-null SHA-256 artifact hash.
    pub async fn record_execution_workspace_evidence(
        &self,
        auth_id: &str,
        idempotency_key: Option<&str>,
        input: RecordWorkspaceEvidence,
    ) -> Result<(ExecutionWorkspaceEvidence, bool)> {
        path_id(auth_id)?;
        if let Some(key) = idempotency_key {
            path_id(key)?;
        }
        version(&input.schema_version)?;
        path_id(&input.job_id)?;
        path_id(&input.worker_owner_id)?;
        check(
            input.worker_lease_epoch >= 1,
            "invalid_schema",
            "/worker_lease_epoch",
        )?;
        check(
            hash(&input.workspace_manifest_sha256),
            "invalid_schema",
            "/workspace_manifest_sha256",
        )?;
        check(
            hash(&input.output_artifact_sha256),
            "invalid_schema",
            "/output_artifact_sha256",
        )?;

        let request_sha256 = digest(&canonical(&(auth_id, &input))?);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now_utc = now(&mut tx).await?;
        require_verified_worker_lease(
            &mut tx,
            &now_utc,
            &input.worker_owner_id,
            input.worker_lease_epoch,
        )
        .await?;

        if let Some(key) = idempotency_key {
            if let Some(ev) = replay_ledger::<ExecutionWorkspaceEvidence>(
                &mut tx,
                "record_workspace_evidence",
                key,
                &request_sha256,
            )
            .await?
            {
                tx.commit().await?;
                return Ok((ev, true));
            }
        }

        let auth = execution_auth_row(
            sqlx::query("SELECT * FROM execution_authorizations WHERE id=?")
                .bind(auth_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(StoreError::NotFound)?,
        )?;
        match auth.status {
            ExecutionAuthStatus::Issued => {
                return Err(StoreError::Conflict {
                    code: "execution_not_authorized",
                });
            }
            ExecutionAuthStatus::EvidenceRecorded => {
                return Err(StoreError::Conflict {
                    code: "invalid_transition",
                });
            }
            ExecutionAuthStatus::Claimed => {
                if auth.claimed_job_id.as_deref() != Some(&input.job_id) {
                    return Err(StoreError::Conflict {
                        code: "execution_not_authorized",
                    });
                }
            }
        }

        let evidence = ExecutionWorkspaceEvidence {
            schema_version: EXECUTION_AUTH_SCHEMA_VERSION.into(),
            id: Uuid::new_v4().to_string(),
            execution_auth_id: auth.id.clone(),
            project_id: auth.project_id,
            revision_id: auth.revision_id,
            job_id: input.job_id,
            worker_owner_id: input.worker_owner_id,
            worker_lease_epoch: input.worker_lease_epoch,
            workspace_manifest_sha256: input.workspace_manifest_sha256,
            output_artifact_sha256: input.output_artifact_sha256,
            recorded_at: now_utc,
        };
        sqlx::query(
            "INSERT INTO execution_workspace_evidence(id,execution_auth_id,project_id,revision_id,job_id,workspace_manifest_sha256,output_artifact_sha256,recorded_at,worker_owner_id,worker_lease_epoch) VALUES(?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&evidence.id)
        .bind(&evidence.execution_auth_id)
        .bind(&evidence.project_id)
        .bind(&evidence.revision_id)
        .bind(&evidence.job_id)
        .bind(&evidence.workspace_manifest_sha256)
        .bind(&evidence.output_artifact_sha256)
        .bind(&evidence.recorded_at)
        .bind(&evidence.worker_owner_id)
        .bind(evidence.worker_lease_epoch)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "UPDATE execution_authorizations SET status='evidence_recorded' WHERE id=? AND status='claimed'",
        )
        .bind(auth_id)
        .execute(&mut *tx)
        .await?;

        if let Some(key) = idempotency_key {
            remember_ledger(
                &mut tx,
                "record_workspace_evidence",
                key,
                &request_sha256,
                &evidence,
            )
            .await?;
        }
        tx.commit().await?;
        Ok((evidence, false))
    }

    pub async fn get_execution_workspace_evidence(
        &self,
        auth_id: &str,
    ) -> Result<ExecutionWorkspaceEvidence> {
        path_id(auth_id)?;
        let row =
            sqlx::query("SELECT * FROM execution_workspace_evidence WHERE execution_auth_id=?")
                .bind(auth_id)
                .fetch_optional(&self.pool)
                .await?
                .ok_or(StoreError::NotFound)?;
        workspace_evidence_row(row)
    }

    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn register_asset(
        &self,
        asset_store_dir: &Path,
        idempotency_key: Option<&str>,
        input: RegisterAsset,
    ) -> Result<(AssetRecord, bool, bool)> {
        self.register_asset_with_policy(
            asset_store_dir,
            idempotency_key,
            input,
            None,
            AssetImportFault::None,
        )
        .await
    }

    pub async fn register_asset_authorized(
        &self,
        asset_store_dir: &Path,
        idempotency_key: Option<&str>,
        input: RegisterAsset,
        trusted_import_roots: &[PathBuf],
    ) -> Result<(AssetRecord, bool, bool)> {
        self.register_asset_with_policy(
            asset_store_dir,
            idempotency_key,
            input,
            Some(trusted_import_roots),
            AssetImportFault::None,
        )
        .await
    }

    pub async fn register_asset_with_policy(
        &self,
        asset_store_dir: &Path,
        idempotency_key: Option<&str>,
        input: RegisterAsset,
        trusted_import_roots: Option<&[PathBuf]>,
        fault: AssetImportFault,
    ) -> Result<(AssetRecord, bool, bool)> {
        if let Some(key) = idempotency_key {
            path_id(key)?;
        }
        version(&input.schema_version)?;
        check(
            !input.source_path.is_empty(),
            "unsafe_asset_path",
            "/source_path",
        )?;
        let provenance = input.provenance.unwrap_or(AudioProvenance::UserSupplied);
        let provenance_str = match provenance {
            AudioProvenance::UserSupplied => "user_supplied",
            AudioProvenance::SyntheticTestFixture => "synthetic_test_fixture",
        };
        let request_sha256 = digest(&canonical(&input)?);
        let source_path = Path::new(&input.source_path);

        if let Some(roots) = trusted_import_roots {
            authorize_asset_import_source(source_path, roots).map_err(map_asset_error)?;
        }

        if let Some(key) = idempotency_key {
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            if let Some((record, duplicate)) =
                replay_asset_import(&mut tx, key, &request_sha256).await?
            {
                tx.commit().await?;
                return Ok((record, true, duplicate));
            }
            tx.commit().await?;
        }

        let mut staged =
            stage_local_asset(asset_store_dir, source_path, provenance).map_err(map_asset_error)?;

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(key) = idempotency_key {
            if let Some((record, duplicate)) =
                replay_asset_import(&mut tx, key, &request_sha256).await?
            {
                tx.commit().await?;
                return Ok((record, true, duplicate));
            }
        }

        if let Some(row) = sqlx::query("SELECT * FROM assets WHERE sha256=?")
            .bind(&staged.sha256)
            .fetch_optional(&mut *tx)
            .await?
        {
            let existing = asset_row(row)?;
            if existing.kind != input.kind || existing.provenance != provenance {
                return Err(StoreError::Conflict {
                    code: "asset_conflict",
                });
            }
            if let Some(key) = idempotency_key {
                sqlx::query(
                    "INSERT INTO asset_import_idempotency(idempotency_key,request_sha256,asset_id,response_json,created_flag) VALUES(?,?,?,?,0)",
                )
                .bind(key)
                .bind(&request_sha256)
                .bind(&existing.id)
                .bind(json(&existing)?)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            return Ok((existing, false, true));
        }

        staged.ensure_target_available().map_err(map_asset_error)?;

        let created_at = now(&mut tx).await?;
        let record = AssetRecord {
            schema_version: ASSET_SCHEMA_VERSION.into(),
            id: Uuid::new_v4().to_string(),
            sha256: staged.sha256.clone(),
            kind: input.kind,
            provenance,
            byte_size: staged.byte_size,
            storage_rel_path: staged.storage_rel_path.clone(),
            metadata: staged.metadata.clone(),
            created_at: created_at.clone(),
        };

        sqlx::query(
            "INSERT INTO assets(id,sha256,kind,provenance,byte_size,storage_rel_path,metadata_json,created_at) VALUES(?,?,?,?,?,?,?,?)",
        )
        .bind(&record.id)
        .bind(&record.sha256)
        .bind(record.kind.as_str())
        .bind(provenance_str)
        .bind(i64::try_from(record.byte_size).map_err(|_| StoreError::Internal)?)
        .bind(&record.storage_rel_path)
        .bind(json(&record.metadata)?)
        .bind(&created_at)
        .execute(&mut *tx)
        .await?;

        if let Some(key) = idempotency_key {
            sqlx::query(
                "INSERT INTO asset_import_idempotency(idempotency_key,request_sha256,asset_id,response_json,created_flag) VALUES(?,?,?,?,1)",
            )
            .bind(key)
            .bind(&request_sha256)
            .bind(&record.id)
            .bind(json(&record)?)
            .execute(&mut *tx)
            .await?;
        }

        if fault == AssetImportFault::BeforePublish {
            sqlx::query("SELECT * FROM __injected_fault_table")
                .execute(&mut *tx)
                .await?;
        }

        staged.publish_file().map_err(map_asset_error)?;

        if fault == AssetImportFault::AfterPublishBeforeCommit {
            sqlx::query("SELECT * FROM __injected_fault_table")
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;
        staged.disarm();
        Ok((record, false, false))
    }

    pub async fn get_asset(&self, asset_id: &str) -> Result<AssetRecord> {
        path_id(asset_id)?;
        let row = sqlx::query("SELECT * FROM assets WHERE id=?")
            .bind(asset_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        asset_row(row)
    }

    pub async fn get_asset_by_sha256(&self, sha256: &str) -> Result<AssetRecord> {
        check(hash(sha256), "invalid_schema", "/sha256")?;
        let row = sqlx::query("SELECT * FROM assets WHERE sha256=?")
            .bind(sha256)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        asset_row(row)
    }

    pub async fn list_assets(&self, q: ListQuery) -> Result<Page<AssetRecord>> {
        let limit = list_limit(&q)?;
        let mut c = self.pool.acquire().await?;
        let anchor: Option<String> = if let Some(cid) = q.cursor.as_deref() {
            check(id(cid), "invalid_cursor", "/cursor")?;
            let created: Option<String> =
                sqlx::query_scalar("SELECT created_at FROM assets WHERE id=?")
                    .bind(cid)
                    .fetch_optional(&mut *c)
                    .await?;
            Some(created.ok_or_else(|| invalid("invalid_cursor", "/cursor"))?)
        } else {
            None
        };
        let rows = sqlx::query(
            "SELECT * FROM assets WHERE (? IS NULL OR (created_at,id)>(?,?)) ORDER BY created_at,id LIMIT ?",
        )
        .bind(q.cursor.as_deref())
        .bind(&anchor)
        .bind(q.cursor.as_deref())
        .bind(limit + 1)
        .fetch_all(&mut *c)
        .await?;
        let mut items = rows
            .into_iter()
            .map(asset_row)
            .collect::<Result<Vec<_>>>()?;
        let next = if items.len() > limit as usize {
            items.pop();
            items.last().map(|a| a.id.clone())
        } else {
            None
        };
        Ok(Page {
            schema_version: "1".into(),
            items,
            next_cursor: next,
        })
    }

    pub async fn verify_asset(
        &self,
        asset_store_dir: &Path,
        asset_id: &str,
    ) -> Result<AssetIntegrityReport> {
        let record = self.get_asset(asset_id).await?;
        verify_stored_asset(asset_store_dir, &record).map_err(map_asset_error)
    }

    /// Read-only diagnostic report comparing `asset_store_dir` against SQLite `assets`.
    /// Never deletes files or adopts unregistered asset blobs.
    pub async fn reconcile_asset_store(
        &self,
        asset_store_dir: &Path,
    ) -> Result<AssetStoreRecoveryReport> {
        let rel_paths: HashSet<String> =
            sqlx::query_scalar::<_, String>("SELECT storage_rel_path FROM assets")
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .collect();
        inspect_asset_store_artifacts(asset_store_dir, &rel_paths).map_err(map_asset_error)
    }

    pub async fn import_skill(
        &self,
        skill_store_dir: &Path,
        idempotency_key: Option<&str>,
        source_path: &Path,
    ) -> Result<(SkillRecord, bool)> {
        self.import_skill_with_policy(
            skill_store_dir,
            idempotency_key,
            source_path,
            None,
            SkillImportFault::None,
        )
        .await
    }

    pub async fn import_skill_authorized(
        &self,
        skill_store_dir: &Path,
        idempotency_key: Option<&str>,
        source_path: &Path,
        trusted_import_roots: &[PathBuf],
    ) -> Result<(SkillRecord, bool)> {
        self.import_skill_with_policy(
            skill_store_dir,
            idempotency_key,
            source_path,
            Some(trusted_import_roots),
            SkillImportFault::None,
        )
        .await
    }

    pub async fn import_skill_with_policy(
        &self,
        skill_store_dir: &Path,
        idempotency_key: Option<&str>,
        source_path: &Path,
        trusted_import_roots: Option<&[PathBuf]>,
        fault: SkillImportFault,
    ) -> Result<(SkillRecord, bool)> {
        if let Some(key) = idempotency_key {
            path_id(key)?;
        }
        let raw_source = source_path
            .to_str()
            .ok_or_else(|| invalid("unsafe_skill_bundle", "/source_path"))?;
        check(
            !raw_source.is_empty(),
            "unsafe_skill_bundle",
            "/source_path",
        )?;
        let request_sha256 = digest(&canonical(&ImportSkill {
            schema_version: "1".into(),
            source_path: raw_source.to_owned(),
        })?);

        if let Some(roots) = trusted_import_roots {
            authorize_import_source(source_path, roots).map_err(map_skill_error)?;
        }

        if let Some(key) = idempotency_key {
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            if let Some(record) = replay_skill_import(&mut tx, key, &request_sha256).await? {
                tx.commit().await?;
                return Ok((record, true));
            }
            tx.commit().await?;
        }

        let mut staged =
            stage_local_skill(skill_store_dir, source_path).map_err(map_skill_error)?;

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(key) = idempotency_key {
            if let Some(record) = replay_skill_import(&mut tx, key, &request_sha256).await? {
                tx.commit().await?;
                return Ok((record, true));
            }
        }

        if let Some(existing_sha) = sqlx::query_scalar::<_, String>(
            "SELECT bundle_sha256 FROM skill_bundles WHERE skill_id=? AND skill_version=?",
        )
        .bind(&staged.manifest.id)
        .bind(&staged.manifest.version)
        .fetch_optional(&mut *tx)
        .await?
        {
            if existing_sha == staged.bundle_sha256 {
                return Err(StoreError::Conflict {
                    code: "skill_already_exists",
                });
            }
            return Err(StoreError::Conflict {
                code: "skill_version_conflict",
            });
        }

        if sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM skill_bundles WHERE bundle_sha256=?")
            .bind(&staged.bundle_sha256)
            .fetch_one(&mut *tx)
            .await?
            > 0
        {
            return Err(StoreError::Conflict {
                code: "skill_hash_conflict",
            });
        }

        staged.ensure_target_available().map_err(map_skill_error)?;

        let imported_at = now(&mut tx).await?;
        let manifest_json = json(&staged.manifest)?;
        let layout_json = json(&staged.layout)?;
        let record = SkillRecord {
            schema_version: "1".into(),
            skill_id: staged.manifest.id.clone(),
            skill_version: staged.manifest.version.clone(),
            bundle_sha256: staged.bundle_sha256.clone(),
            enabled: true,
            audit_status: AuditStatus::Unaudited,
            storage_rel_dir: staged.storage_rel_dir.clone(),
            file_count: staged.layout.file_count as u64,
            total_bytes: staged.layout.total_bytes,
            imported_at: imported_at.clone(),
            layout: staged.layout.clone(),
            manifest: staged.manifest.clone(),
        };

        sqlx::query(
            "INSERT INTO skill_bundles(skill_id,skill_version,bundle_sha256,manifest_json,layout_json,storage_rel_dir,enabled,audit_status,audit_evidence_json,file_count,total_bytes,imported_at) VALUES(?,?,?,?,?,?,1,'unaudited',NULL,?,?,?)",
        )
        .bind(&record.skill_id)
        .bind(&record.skill_version)
        .bind(&record.bundle_sha256)
        .bind(&manifest_json)
        .bind(&layout_json)
        .bind(&record.storage_rel_dir)
        .bind(record.file_count as i64)
        .bind(record.total_bytes as i64)
        .bind(&imported_at)
        .execute(&mut *tx)
        .await?;

        if let Some(key) = idempotency_key {
            sqlx::query(
                "INSERT INTO skill_import_idempotency(idempotency_key,request_sha256,skill_id,skill_version,response_json) VALUES(?,?,?,?,?)",
            )
            .bind(key)
            .bind(&request_sha256)
            .bind(&record.skill_id)
            .bind(&record.skill_version)
            .bind(json(&record)?)
            .execute(&mut *tx)
            .await?;
        }

        if fault == SkillImportFault::BeforePublish {
            sqlx::query("SELECT * FROM __injected_fault_table")
                .execute(&mut *tx)
                .await?;
        }

        staged.publish_files().map_err(map_skill_error)?;

        if fault == SkillImportFault::AfterPublishBeforeCommit {
            sqlx::query("SELECT * FROM __injected_fault_table")
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;
        staged.disarm();
        Ok((record, false))
    }

    pub async fn verify_skill(
        &self,
        skill_store_dir: &Path,
        skill_id: &str,
        skill_version: &str,
    ) -> Result<SkillIntegrityReport> {
        let record = self.get_skill(skill_id, skill_version).await?;
        verify_stored_bundle(skill_store_dir, &record).map_err(map_skill_error)
    }

    /// Read-only diagnostic report comparing `skill_store_dir` against SQLite `skill_bundles`.
    /// Never deletes files or adopts unregistered bundles.
    pub async fn reconcile_skill_store(
        &self,
        skill_store_dir: &Path,
    ) -> Result<SkillStoreRecoveryReport> {
        let rel_dirs: HashSet<String> =
            sqlx::query_scalar::<_, String>("SELECT storage_rel_dir FROM skill_bundles")
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .collect();
        inspect_skill_store_artifacts(skill_store_dir, &rel_dirs).map_err(map_skill_error)
    }

    pub async fn list_skills(&self, q: ListQuery) -> Result<Page<SkillRecord>> {
        let limit = list_limit(&q)?;
        let mut c = self.pool.acquire().await?;
        let anchor: Option<(String, String)> = if let Some(cursor_sha) = q.cursor.as_deref() {
            check(hash(cursor_sha), "invalid_cursor", "/cursor")?;
            let row = sqlx::query(
                "SELECT skill_id,skill_version FROM skill_bundles WHERE bundle_sha256=?",
            )
            .bind(cursor_sha)
            .fetch_optional(&mut *c)
            .await?
            .ok_or_else(|| invalid("invalid_cursor", "/cursor"))?;
            Some((row.try_get("skill_id")?, row.try_get("skill_version")?))
        } else {
            None
        };

        let rows = if let Some((cursor_id, cursor_ver)) = &anchor {
            sqlx::query(
                "SELECT * FROM skill_bundles WHERE (skill_id > ? OR (skill_id = ? AND skill_version > ?)) ORDER BY skill_id,skill_version LIMIT ?",
            )
            .bind(cursor_id)
            .bind(cursor_id)
            .bind(cursor_ver)
            .bind(limit + 1)
            .fetch_all(&mut *c)
            .await?
        } else {
            sqlx::query("SELECT * FROM skill_bundles ORDER BY skill_id,skill_version LIMIT ?")
                .bind(limit + 1)
                .fetch_all(&mut *c)
                .await?
        };

        let mut items = rows
            .into_iter()
            .map(skill_row)
            .collect::<Result<Vec<_>>>()?;
        let next = if items.len() > limit as usize {
            items.pop();
            items.last().map(|s| s.bundle_sha256.clone())
        } else {
            None
        };
        Ok(Page {
            schema_version: "1".into(),
            items,
            next_cursor: next,
        })
    }

    pub async fn get_skill(&self, skill_id: &str, skill_version: &str) -> Result<SkillRecord> {
        check(is_safe_skill_id(skill_id), "invalid_schema", "/skill_id")?;
        check(
            is_safe_skill_version(skill_version),
            "invalid_schema",
            "/skill_version",
        )?;
        let row = sqlx::query("SELECT * FROM skill_bundles WHERE skill_id=? AND skill_version=?")
            .bind(skill_id)
            .bind(skill_version)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        skill_row(row)
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

    pub async fn get_approval(&self, pid: &str, approval_id: &str) -> Result<Approval> {
        path_id(pid)?;
        path_id(approval_id)?;
        let row = sqlx::query(
            "SELECT a.id,a.project_id,a.revision_id,a.content_sha256,a.approved_at FROM approvals a JOIN projects p ON p.id=a.project_id WHERE a.project_id=? AND a.id=? AND p.deleted_at IS NULL",
        )
        .bind(pid)
        .bind(approval_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound)?;
        Ok(Approval {
            schema_version: "1".into(),
            id: row.try_get("id")?,
            project_id: row.try_get("project_id")?,
            revision_id: row.try_get("revision_id")?,
            content_sha256: row.try_get("content_sha256")?,
            approved_at: row.try_get("approved_at")?,
        })
    }

    pub async fn review_revision(
        &self,
        pid: &str,
        rid: &str,
        idempotency_key: Option<&str>,
        command: ReviewCommand,
    ) -> Result<RevisionReview> {
        path_id(pid)?;
        path_id(rid)?;
        if let Some(key) = idempotency_key {
            path_id(key)?;
        }
        let request = digest(&canonical(&(rid, &command))?);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(key) = idempotency_key {
            if let Some(value) = replay(&mut tx, "review_revision", pid, key, &request).await? {
                tx.commit().await?;
                return Ok(value);
            }
        }
        version(&command.schema_version)?;
        let project = read_project(&mut tx, pid).await?;
        read_revision(&mut tx, pid, rid).await?;
        if project.current_revision_id.as_deref() != Some(rid) {
            return Err(StoreError::StaleRevision);
        }
        validate_approval_hash(&command.content_sha256)?;
        let revision = read_revision(&mut tx, pid, rid).await?;
        if revision.content_sha256 != command.content_sha256 {
            return Err(StoreError::Conflict {
                code: "hash_mismatch",
            });
        }
        let state = read_review_status(&mut tx, pid, rid).await?;
        match state {
            ReviewStatus::Draft => {
                sqlx::query("UPDATE revision_reviews SET review_state='in_review',updated_at=? WHERE project_id=? AND revision_id=?")
                    .bind(now(&mut tx).await?).bind(pid).bind(rid).execute(&mut *tx).await?;
            }
            ReviewStatus::InReview => {}
            ReviewStatus::Approved => {
                return Err(StoreError::Conflict {
                    code: "invalid_transition",
                });
            }
        }
        let value = RevisionReview {
            schema_version: "1".into(),
            revision_id: rid.into(),
            review_state: ReviewStatus::InReview,
        };
        if let Some(key) = idempotency_key {
            remember(&mut tx, "review_revision", pid, key, pid, &request, &value).await?;
        }
        tx.commit().await?;
        Ok(value)
    }

    pub async fn withdraw_review(
        &self,
        pid: &str,
        rid: &str,
        idempotency_key: Option<&str>,
        command: ReviewCommand,
    ) -> Result<RevisionReview> {
        path_id(pid)?;
        path_id(rid)?;
        if let Some(key) = idempotency_key {
            path_id(key)?;
        }
        let request = digest(&canonical(&(rid, &command))?);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(key) = idempotency_key {
            if let Some(value) = replay(&mut tx, "withdraw_review", pid, key, &request).await? {
                tx.commit().await?;
                return Ok(value);
            }
        }
        version(&command.schema_version)?;
        let project = read_project(&mut tx, pid).await?;
        read_revision(&mut tx, pid, rid).await?;
        if project.current_revision_id.as_deref() != Some(rid) {
            return Err(StoreError::Conflict {
                code: "invalid_transition",
            });
        }
        validate_approval_hash(&command.content_sha256)?;
        let revision = read_revision(&mut tx, pid, rid).await?;
        if revision.content_sha256 != command.content_sha256 {
            return Err(StoreError::Conflict {
                code: "hash_mismatch",
            });
        }
        let state = read_review_status(&mut tx, pid, rid).await?;
        match state {
            ReviewStatus::InReview => {
                sqlx::query("UPDATE revision_reviews SET review_state='draft',updated_at=? WHERE project_id=? AND revision_id=?")
                    .bind(now(&mut tx).await?).bind(pid).bind(rid).execute(&mut *tx).await?;
            }
            ReviewStatus::Draft => {}
            ReviewStatus::Approved => {
                return Err(StoreError::Conflict {
                    code: "invalid_transition",
                });
            }
        }
        let value = RevisionReview {
            schema_version: "1".into(),
            revision_id: rid.into(),
            review_state: ReviewStatus::Draft,
        };
        if let Some(key) = idempotency_key {
            remember(&mut tx, "withdraw_review", pid, key, pid, &request, &value).await?;
        }
        tx.commit().await?;
        Ok(value)
    }

    pub async fn approve_revision(
        &self,
        pid: &str,
        rid: &str,
        idempotency_key: Option<&str>,
        command: ReviewCommand,
    ) -> Result<(Approval, bool, bool)> {
        path_id(pid)?;
        path_id(rid)?;
        if let Some(key) = idempotency_key {
            path_id(key)?;
        }
        let request = digest(&canonical(&(rid, &command))?);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(key) = idempotency_key {
            if let Some(value) = replay(&mut tx, "approve_revision", pid, key, &request).await? {
                tx.commit().await?;
                return Ok((value, true, false));
            }
        }
        version(&command.schema_version)?;
        let project = read_project(&mut tx, pid).await?;
        let revision = read_revision(&mut tx, pid, rid).await?;
        if project.current_revision_id.as_deref() != Some(rid) {
            return Err(StoreError::StaleRevision);
        }
        validate_approval_hash(&command.content_sha256)?;
        if revision.content_sha256 != command.content_sha256 {
            return Err(StoreError::Conflict {
                code: "hash_mismatch",
            });
        }
        let state = read_review_status(&mut tx, pid, rid).await?;
        if state == ReviewStatus::Approved {
            let value = read_approval(&mut tx, pid, rid, &command.content_sha256).await?;
            if let Some(key) = idempotency_key {
                remember(&mut tx, "approve_revision", pid, key, pid, &request, &value).await?;
            }
            tx.commit().await?;
            return Ok((value, false, true));
        }
        if state != ReviewStatus::InReview {
            return Err(StoreError::Conflict {
                code: "invalid_transition",
            });
        }
        let value = Approval {
            schema_version: "1".into(),
            id: Uuid::new_v4().to_string(),
            project_id: pid.into(),
            revision_id: rid.into(),
            content_sha256: revision.content_sha256,
            approved_at: now(&mut tx).await?,
        };
        sqlx::query("INSERT INTO approvals(id,project_id,revision_id,content_sha256,approved_at) VALUES(?,?,?,?,?)")
            .bind(&value.id).bind(pid).bind(rid).bind(&value.content_sha256).bind(&value.approved_at).execute(&mut *tx).await?;
        sqlx::query("UPDATE revision_reviews SET review_state='approved',updated_at=? WHERE project_id=? AND revision_id=?")
            .bind(&value.approved_at).bind(pid).bind(rid).execute(&mut *tx).await?;
        sqlx::query(
            "UPDATE projects SET approved_revision_id=? WHERE id=? AND current_revision_id=?",
        )
        .bind(rid)
        .bind(pid)
        .bind(rid)
        .execute(&mut *tx)
        .await?;
        if let Some(key) = idempotency_key {
            remember(&mut tx, "approve_revision", pid, key, pid, &request, &value).await?;
        }
        tx.commit().await?;
        Ok((value, false, false))
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
    refresh_scene_prompt_provenance(&mut input)?;
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
    for (i, scene) in input.storyboard.scenes.into_iter().enumerate() {
        for cue in &scene.sfx_cues {
            let sfx_sha = sqlx::query_scalar::<_, String>(
                "SELECT sha256 FROM assets WHERE id=? AND kind='sfx_wav'",
            )
            .bind(&cue.asset_id)
            .fetch_optional(&mut *c)
            .await?
            .ok_or_else(|| {
                invalid(
                    "invalid_asset_reference",
                    &format!("/storyboard/scenes/{i}/sfx_cues"),
                )
            })?;
            check(
                input.asset_hashes.iter().any(|h| h == &sfx_sha),
                "invalid_asset_reference",
                &format!("/storyboard/scenes/{i}/sfx_cues"),
            )?;
        }
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
    sqlx::query("UPDATE projects SET current_revision_id=?,approved_revision_id=NULL,idea=?,target_json=? WHERE id=?").bind(&value.id).bind(&value.content.idea).bind(json(&value.content.target)?).bind(pid).execute(&mut *c).await?;
    sqlx::query("INSERT INTO revision_reviews(project_id,revision_id,review_state,updated_at) VALUES(?,?,'draft',?)")
        .bind(pid).bind(&value.id).bind(&value.created_at).execute(c).await?;
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
        sum += u64::from(s.duration_ms);
    }
    check(
        sum == u64::from(input.target.duration_ms),
        "invalid_storyboard",
        "/storyboard/scenes",
    )
}
