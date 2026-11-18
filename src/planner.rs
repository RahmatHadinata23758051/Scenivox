use crate::domain::{PlanStoryboard, PlannedStoryboard, SceneInput, SfxCue};

pub trait Planner: Send + Sync {
    fn plan(&self, input: &PlanStoryboard) -> Result<PlannedStoryboard, PlannerError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlannerError {
    InvalidOutput,
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
