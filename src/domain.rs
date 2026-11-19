//! Strict v1 commands and immutable snapshot records shared by every transport.
use serde::{Deserialize, Serialize};

fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn selection_present<'de, D>(deserializer: D) -> Result<Option<Selection>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Selection>::deserialize(deserializer)?
        .map(Some)
        .ok_or_else(|| serde::de::Error::custom("explicit null is not allowed"))
}

macro_rules! record {
    ($name:ident { $($field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
        #[serde(deny_unknown_fields)]
        pub struct $name { $(pub $field: $ty),* }
    };
}
record!(Selection {
    provider_id: String,
    model_id: String
});
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelSelection {
    #[serde(
        default,
        deserialize_with = "selection_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub storyboard_planning: Option<Selection>,
    #[serde(
        default,
        deserialize_with = "selection_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub scene_code_generation: Option<Selection>,
}

record!(ModelRecord {
    schema_version: String,
    provider_id: String,
    model_id: String,
    adapter_version: String,
    task_roles: Vec<String>,
    parameters: Parameters,
    enabled: bool,
    configured: bool,
    availability: String,
    reason: String,
    remote: bool
});
record!(SetModelSettings {
    schema_version: String,
    model_selection: ModelSelection
});
record!(ResolveModelSelection {
    schema_version: String,
    project_id: Option<String>,
    model_selection: ModelSelection
});
record!(EffectiveModelSelection {
    schema_version: String,
    model_selection: ModelSelection
});

record!(RegisterModel {
    schema_version: String,
    provider_id: String,
    model_id: String,
    adapter_version: String,
    task_roles: Vec<String>,
    parameters: Parameters,
    enabled: bool,
    configured: bool,
    availability: String,
    reason: String,
    remote: bool
});
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Parameters {
    pub temperature: f64,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub seed: Option<u32>,
}
record!(ModelSnapshot {
    provider_id: String,
    model_id: String,
    adapter_version: String,
    parameters: Parameters,
    prompt_sha256: String
});
record!(ModelSnapshots {
    storyboard_planning: ModelSnapshot,
    scene_code_generation: ModelSnapshot
});
record!(Target {
    duration_ms: u32,
    aspect_ratio: String,
    fps: u32,
    language: String,
    style_id: String
});
record!(SfxCue {
    asset_id: String,
    at_ms: u32
});
record!(SceneInput { id: Option<String>, order: u32, duration_ms: u32, visual_direction: String, camera: String, action: String, narration: String, caption_hint: String, sfx_cues: Vec<SfxCue> });
record!(Scene { id: String, order: u32, duration_ms: u32, visual_direction: String, camera: String, action: String, narration: String, caption_hint: String, sfx_cues: Vec<SfxCue> });
record!(StoryboardInput { synopsis: String, hook: String, scenes: Vec<SceneInput> });
record!(Storyboard { synopsis: String, hook: String, scenes: Vec<Scene> });

record!(PlanningPromptInput {
    schema_version: String,
    provider_id: String,
    model_id: String,
    adapter_version: String,
    idea: String,
    target: Target
});
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlanningProvenance {
    pub source: String,
    pub prompt_input: PlanningPromptInput,
    pub prompt_sha256: String,
    pub scene_prompt_input: ScenePromptInput,
    pub scene_prompt_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene_id_mapping: Option<Vec<SceneIdMapping>>,
}
record!(SceneIdMapping {
    order: u32,
    scene_id: String
});
record!(ScenePromptInput {
    schema_version: String,
    provider_id: String,
    model_id: String,
    adapter_version: String,
    idea: String,
    target: Target,
    storyboard: StoryboardInput
});
record!(PlanStoryboard {
    schema_version: String,
    idea: String,
    target: Target,
    expected_revision_id: Option<String>,
    model_selection: ModelSelection
});
record!(PlannedStoryboard {
    storyboard: StoryboardInput,
    skill_id: String,
    skill_version: String
});
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RevisionInput {
    pub schema_version: String,
    pub idea: String,
    pub target: Target,
    pub storyboard: StoryboardInput,
    pub models: ModelSnapshots,
    pub asset_hashes: Vec<String>,
    pub skill_id: String,
    pub skill_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planning_provenance: Option<PlanningProvenance>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RevisionContent {
    pub schema_version: String,
    pub idea: String,
    pub target: Target,
    pub storyboard: Storyboard,
    pub models: ModelSnapshots,
    pub asset_hashes: Vec<String>,
    pub skill_id: String,
    pub skill_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planning_provenance: Option<PlanningProvenance>,
}
record!(Project { schema_version: String, id: String, title: String, idea: String, target: Target, model_selection: ModelSelection, current_revision_id: Option<String>, approved_revision_id: Option<String> });
record!(Revision { schema_version: String, id: String, project_id: String, parent_revision_id: Option<String>, version: u32, created_at: String, content_sha256: String, content: RevisionContent });

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Draft,
    InReview,
    Approved,
}
record!(RevisionReview {
    schema_version: String,
    revision_id: String,
    review_state: ReviewStatus
});
record!(ReviewCommand {
    schema_version: String,
    content_sha256: String
});
record!(Approval {
    schema_version: String,
    id: String,
    project_id: String,
    revision_id: String,
    content_sha256: String,
    approved_at: String
});
record!(CreateRenderJob {
    schema_version: String,
    revision_id: String,
    content_sha256: String,
    cost_consent_id: Option<String>
});
record!(RetryRenderJob {
    schema_version: String,
    cost_consent_id: Option<String>
});
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RenderJobStatus {
    Queued,
    Preparing,
    Voicing,
    Animating,
    Mixing,
    Validating,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}
record!(RenderJob {
    schema_version: String,
    id: String,
    project_id: String,
    revision_id: String,
    approval_id: String,
    content_sha256: String,
    snapshot: RevisionContent,
    status: RenderJobStatus,
    retry_of_job_id: Option<String>,
    created_at: String,
    error_code: Option<String>,
    artifact_ids: Vec<String>,
    cost_consent_id: Option<String>
});
record!(JobCancelCommand {
    schema_version: String
});
record!(CreateProject {
    schema_version: String,
    title: String,
    idea: String,
    target: Target,
    model_selection: ModelSelection
});
record!(UpdateProject {
    schema_version: String,
    title: String
});
record!(CreateRevision { schema_version: String, expected_revision_id: Option<String>, content: RevisionInput });
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Page<T> {
    pub schema_version: String,
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}
