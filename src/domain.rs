//! Strict v1 commands and immutable snapshot records shared by every transport.
use serde::{Deserialize, Serialize};

fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
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
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub storyboard_planning: Option<Selection>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub scene_code_generation: Option<Selection>,
}
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
record!(RevisionInput { schema_version: String, idea: String, target: Target, storyboard: StoryboardInput, models: ModelSnapshots, asset_hashes: Vec<String>, skill_id: String, skill_version: String });
record!(RevisionContent { schema_version: String, idea: String, target: Target, storyboard: Storyboard, models: ModelSnapshots, asset_hashes: Vec<String>, skill_id: String, skill_version: String });
record!(Project { schema_version: String, id: String, title: String, idea: String, target: Target, model_selection: ModelSelection, current_revision_id: Option<String>, approved_revision_id: Option<String> });
record!(Revision { schema_version: String, id: String, project_id: String, parent_revision_id: Option<String>, version: u32, created_at: String, content_sha256: String, content: RevisionContent });
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
