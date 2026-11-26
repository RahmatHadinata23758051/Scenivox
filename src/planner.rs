use serde::Deserialize;

use crate::{
    cost::{CostScope, ReserveBudgetRequest, WorkerLease},
    domain::{PlanStoryboard, PlannedStoryboard, Revision, SceneInput, SfxCue, StoryboardInput},
    openai_adapter::{
        CancellationToken, DispatchContext, LlmHttpTransport, OpenAiAdapterError,
        OpenAiCompatibleAdapter, OpenAiExecutionOutcome, compute_model_snapshot_sha256,
    },
    provider::{LlmMessage, LlmRequest, ResponseFormat, TaskRole},
    store::{Store, StoreError},
};

pub trait Planner: Send + Sync {
    fn plan(&self, input: &PlanStoryboard) -> Result<PlannedStoryboard, PlannerError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlannerError {
    InvalidOutput,
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterPlannerError {
    #[error(transparent)]
    Adapter(#[from] OpenAiAdapterError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("invalid LLM storyboard output: {0}")]
    InvalidOutput(String),
}

pub struct AdapterStoryboardRequest<'a, T: LlmHttpTransport> {
    pub store: &'a Store,
    pub project_id: &'a str,
    pub idempotency_key: Option<&'a str>,
    pub reservation_idempotency_key: Option<&'a str>,
    pub lease: &'a WorkerLease,
    pub cost_consent_id: &'a str,
    pub adapter: &'a OpenAiCompatibleAdapter<T>,
    pub input: &'a PlanStoryboard,
    pub max_output_tokens: u32,
    pub cancel: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AdapterStoryboardOutcome {
    pub revision: Revision,
    pub planning_outcome: OpenAiExecutionOutcome,
    pub request_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlannedStoryboardJson {
    synopsis: String,
    hook: String,
    scenes: Vec<SceneInput>,
    #[serde(default)]
    skill_id: Option<String>,
    #[serde(default)]
    skill_version: Option<String>,
}

/// Generates and commits an immutable storyboard `Revision` through `OpenAiCompatibleAdapter`,
/// preserving exact resolved model selection, conservative budget reservation, single-use
/// dispatch, and immutable adapter/prompt provenance.
pub async fn plan_storyboard_with_adapter<T: LlmHttpTransport>(
    req: AdapterStoryboardRequest<'_, T>,
) -> Result<AdapterStoryboardOutcome, AdapterPlannerError> {
    if req.input.idea.trim().is_empty() || req.input.target.duration_ms == 0 {
        return Err(AdapterPlannerError::InvalidOutput(
            "idea and target.duration_ms must be positive".into(),
        ));
    }

    let allow_bootstrap = req.adapter.config().allow_authorized_availability_bootstrap
        && req.adapter.config().live_dispatch_enabled
        && req
            .adapter
            .config()
            .capabilities
            .provider_kind
            .is_remote_backed();
    let (effective_selection, planning_model, _scene_model) = req
        .store
        .resolve_effective_selection_for_adapter_with_bootstrap(
            req.project_id,
            &req.input.model_selection,
            allow_bootstrap,
        )
        .await?;

    let user_payload = serde_json::json!({
        "schema_version": "scenivox.storyboard_planning.v1",
        "idea": req.input.idea,
        "target": req.input.target,
        "output_schema": {
            "synopsis": "string",
            "hook": "string",
            "scenes": [{
                "order": "u32 contiguous 0..N-1",
                "duration_ms": "u32 summing exactly to target.duration_ms",
                "visual_direction": "string",
                "camera": "string",
                "action": "string",
                "narration": "string",
                "caption_hint": "string",
                "sfx_cues": []
            }]
        }
    });

    let llm_req = LlmRequest {
        schema_version: "1".into(),
        task_role: TaskRole::StoryboardPlanning,
        provider_id: planning_model.provider_id.clone(),
        model_id: planning_model.model_id.clone(),
        adapter_version: planning_model.adapter_version.clone(),
        parameters: planning_model.parameters.clone(),
        max_output_tokens: req.max_output_tokens,
        response_format: ResponseFormat::JsonObject,
        system_prompt: "You are Scenivox's bounded storyboard planner. Return strictly one JSON object matching the requested storyboard schema with 1..=60 scenes whose durations sum strictly to target.duration_ms.".into(),
        messages: vec![LlmMessage::user(user_payload.to_string())],
        tools: Vec::new(),
    };

    let request_sha256 = llm_req
        .canonical_request_sha256()
        .map_err(OpenAiAdapterError::from)?;
    let estimated_input_tokens = req.adapter.trusted_input_tokens(&llm_req)?;
    let snapshot_sha256 = compute_model_snapshot_sha256(&planning_model, None)?;
    let endpoint_identity = req.adapter.config().endpoint_identity()?;

    let reserve_req = ReserveBudgetRequest {
        cost_consent_id: req.cost_consent_id.to_owned(),
        project_id: Some(req.project_id.to_owned()),
        job_id: None,
        provider_id: planning_model.provider_id.clone(),
        model_id: planning_model.model_id.clone(),
        scope: CostScope::StoryboardPlanning,
        owner_id: req.lease.owner_id.clone(),
        lease_epoch: req.lease.lease_epoch,
        canonical_request_sha256: request_sha256.clone(),
        model_snapshot_sha256: snapshot_sha256,
        endpoint_identity,
        estimated_input_tokens,
        max_output_tokens: i64::from(req.max_output_tokens),
    };
    let dispatch_ctx = DispatchContext {
        store: req.store,
        lease: req.lease,
        project_id: Some(req.project_id),
        job_id: None,
        model: &planning_model,
        snapshot: None,
        cancel: req.cancel,
    };
    let outcome = req
        .adapter
        .execute_with_new_reservation(
            dispatch_ctx,
            reserve_req,
            req.reservation_idempotency_key,
            &llm_req,
        )
        .await?;

    let raw: RawPlannedStoryboardJson =
        serde_json::from_str(&outcome.response.content).map_err(|err| {
            AdapterPlannerError::InvalidOutput(format!("malformed storyboard JSON: {err}"))
        })?;
    if raw.synopsis.trim().is_empty()
        || raw.hook.trim().is_empty()
        || raw.scenes.is_empty()
        || raw.scenes.len() > 60
    {
        return Err(AdapterPlannerError::InvalidOutput(
            "storyboard must include non-empty synopsis, hook, and 1..=60 scenes".into(),
        ));
    }
    let total_ms: u64 = raw
        .scenes
        .iter()
        .enumerate()
        .try_fold(0u64, |acc, (idx, sc)| {
            if sc.order as usize != idx || sc.duration_ms == 0 {
                return Err(AdapterPlannerError::InvalidOutput(
                    "scenes must have contiguous order 0..N-1 and positive duration_ms".into(),
                ));
            }
            acc.checked_add(u64::from(sc.duration_ms))
                .ok_or_else(|| AdapterPlannerError::InvalidOutput("duration sum overflow".into()))
        })?;
    if total_ms != u64::from(req.input.target.duration_ms) {
        return Err(AdapterPlannerError::InvalidOutput(format!(
            "scene duration sum {total_ms}ms does not match target {}ms",
            req.input.target.duration_ms
        )));
    }

    let planned = PlannedStoryboard {
        storyboard: StoryboardInput {
            synopsis: raw.synopsis,
            hook: raw.hook,
            scenes: raw.scenes,
        },
        skill_id: raw.skill_id.unwrap_or_else(|| "stick-figure".into()),
        skill_version: raw.skill_version.unwrap_or_else(|| "audited-v1".into()),
    };

    let mut effective_input = req.input.clone();
    effective_input.model_selection = effective_selection.clone();
    let source = format!(
        "openai_compatible:{}",
        outcome.inference_provenance.as_str()
    );
    let revision = req
        .store
        .commit_planned_storyboard_revision(
            req.project_id,
            req.idempotency_key,
            effective_input,
            effective_selection,
            planned,
            &source,
        )
        .await?;

    Ok(AdapterStoryboardOutcome {
        revision,
        planning_outcome: outcome,
        request_sha256,
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DeterministicMockPlanner;

impl Planner for DeterministicMockPlanner {
    fn plan(&self, input: &PlanStoryboard) -> Result<PlannedStoryboard, PlannerError> {
        if input.idea.trim().is_empty() || input.target.duration_ms == 0 {
            return Err(PlannerError::InvalidOutput);
        }
        let scene_count = input.target.duration_ms.div_ceil(10_000).clamp(1, 6);
        let base = input.target.duration_ms / scene_count;
        let remainder = input.target.duration_ms % scene_count;
        let synopsis = format!("Sebuah cerita singkat tentang {}.", input.idea.trim());
        let hook = format!("Apa yang terjadi ketika {}?", input.idea.trim());
        let scenes = (0..scene_count)
            .map(|index| {
                let duration_ms = base + u32::from(index < remainder);
                SceneInput {
                    id: None,
                    order: index,
                    duration_ms,
                    visual_direction: format!(
                        "Gambarkan momen {} dari {}.",
                        index + 1,
                        input.idea.trim()
                    ),
                    camera: if index == 0 {
                        "Wide establishing shot"
                    } else {
                        "Medium tracking shot"
                    }
                    .into(),
                    action: format!("Tampilkan perkembangan cerita bagian {}.", index + 1),
                    narration: format!("{} Bagian {}.", input.idea.trim(), index + 1),
                    caption_hint: format!("Bagian {}", index + 1),
                    sfx_cues: Vec::<SfxCue>::new(),
                }
            })
            .collect();
        Ok(PlannedStoryboard {
            storyboard: crate::domain::StoryboardInput {
                synopsis,
                hook,
                scenes,
            },
            skill_id: "stick-figure".into(),
            skill_version: "audited-v1".into(),
        })
    }
}
