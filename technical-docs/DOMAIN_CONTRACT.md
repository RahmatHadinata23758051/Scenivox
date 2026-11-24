# Scenivox Domain Contract v1 — ADR-02 Rebaseline

**Status:** P1-01 (immutable revisions), P1-02 (offline planner), P1-05 (model registry/selection), P1-03 (review & editorial approval), P1-04 (durable `RenderJob` schema, triggers, and fail-closed enqueue/read/retry/cancel), and P2-02 (WAV inspection & internal `Timeline` sidecar) are **implemented** at `53db65bc72b0cc37162a5bbcac3689f126cdcf97`. Skill registry, cost-consent ledger, execution-authorization ledger, asset registry, AI agent runtime, sandbox runner, and worker execution remain **planned Phase 2**.

See [`schemas/v1/contracts.schema.json`](../schemas/v1/contracts.schema.json) for validated v1 JSON Schema shapes.

---

## 1. Core Records & Ownership

| Record | Ownership & Invariants | Status |
|---|---|---|
| `Project` | Mutable workspace metadata (`id`, `title`, `idea`, `target`, per-role `model_selection`, `current_revision_id`, `approved_revision_id`). Edits to revision pointers never mutate revisions. | Implemented |
| `Revision` | Immutable snapshot (`id`, `project_id`, `parent_revision_id`, `version`, `created_at`, `content`, `content_sha256`). Embedded `Storyboard` + ordered `Scene`s; copy-on-write on edit clears `approved_revision_id`. | Implemented |
| `RevisionReview` | Mutable workflow state per revision (`draft` / `in_review` / `approved`), separated from immutable content. | Implemented |
| `Approval` | Append-only hash-bound editorial evidence (`id`, `project_id`, `revision_id`, `content_sha256`, `approved_at`). | Implemented |
| `RenderJob` | Durable lifecycle envelope copying exact approved `RevisionContent` snapshot (`queued -> preparing -> voicing -> animating -> mixing -> validating -> succeeded | failed | cancelled | interrupted`). | Implemented (fail-closed) |
| `LocalNarration` | Bounded PCM WAV inspection metadata (`schema_version`, `AudioInfo`, `provenance`: `user_supplied` \| `synthetic_test_fixture`). Sidecar only; never mutates revisions. | Implemented |
| `SkillBundle` | Pinned skill record `(skill_id, skill_version, bundle_sha256, manifest_json, audit_status, enabled)`. | Planned Phase 2 |
| `CostConsent` | Explicit budget/scope authorization for remote/paid model or TTS calls with monotonic spend tracking. | Planned Phase 2 |
| `ExecutionAuth` | Explicit authorization to run AI-generated code for a pinned skill inside a verified sandbox. | Planned Phase 2 |
| `RenderJobEvent` | Append-only agent turn, tool-call, repair-iteration, and error trace linked to a `RenderJob` (secrets scrubbed). | Planned Phase 2 |
| `Artifact` | Content-addressed output (`mp4`, `stills`, `qa_manifest`) produced by a job after passing host MediaQA. | Planned Phase 2 |

---

## 2. Unified Composition & P2-02 Sidecar Relationship (ADR-02)

- **Storyboard & Scenes (`RevisionContent`):** V1 validation limits (`id-ID`, `9:16`, `30` fps, `1..=60000` ms total duration, `1..=60` scenes, exact sum of scene durations) remain unchanged. Each scene keeps a stable server-assigned UUID (`scene_id_mapping` binds `order -> scene_id`).
- **P2-02 Narration & Timing Sidecars:**
  - `LocalNarration` validates structural metadata and ceiling-ms duration consistency (`ceil(frame_count * 1000 / sample_rate_hz)`). Structural checks never prove audio authenticity or speech quality.
  - `Timeline` remains an internal sidecar validating `1..=60` scenes, unique nonempty IDs, positive allocations (`<= 60,000` ms total), cumulative 30 fps frame boundaries (`round(ms * 30 / 1000)`), and no narration truncation (`audio.duration_ms <= allocation_ms`).
  - `SceneTiming` preserves `audio_provenance` and `caption_provenance` (`none`, `unverified_external`, `measured_provider_alignment`). `SyntheticTestFixture` can never qualify as verified narration or `measured_provider_alignment`.
- **Unified Composition Input (Planned Phase 2):** At job execution, Rust exports frozen `revision.json` + verified `narration.wav` + `timeline.json` / `words.json` into the sandboxed job workspace. The agent creates one unified composition (`final.mp4`) rather than mandatory per-scene video clips, respecting the skill manifest's internal rendering strategy.

---

## 3. Enqueue, Retry, Recovery & Cleanup Invariants

1. **Atomic Enqueue Gate:** Enqueue must name `revision_id` and `content_sha256` and atomically verify:
   - Project exists & not deleted; `revision_id == project.current_revision_id`; `revision_id == project.approved_revision_id`.
   - Stored `Approval` row matches `(project_id, revision_id, content_sha256)` and `revision_reviews.review_state == 'approved'`.
   - `asset_hashes` and `sfx_cues` resolve in `assets` registry.
   - Pinned `(skill_id, skill_version)` exists in `skill_bundles`, is `enabled`, and passes policy.
   - If any selected model/TTS provider is `remote: true`, a valid unexhausted `CostConsent` covers it.
   - A valid `ExecutionAuth` exists and `SandboxBackend::is_available() == true`.
   - Failure at any gate returns `409` / `422` / `503` and inserts **zero** `render_jobs` rows.
2. **Startup Recovery:** On every `db::connect`, active jobs (`preparing`, `voicing`, `animating`, `mixing`, `validating`) transition transactionally to `interrupted` (`error_code='startup_interrupted'`). Phase 2 adds startup cleanup of orphaned sandbox containers/processes and stale temporary job workspaces.
3. **Retry:** Retrying `failed` / `cancelled` / `interrupted` creates a **new** job ID bound to the original frozen `snapshot`, after re-verifying current revision approval, skill availability, sandbox readiness, `CostConsent`, and `ExecutionAuth`. Changing models, skill, target, or assets requires a new revision and editorial approval.
4. **Success Criteria:** A `RenderJob` transitions to `succeeded` **only** after host `MediaQA` verifies `final.mp4` via `ffprobe` (9:16 `1080×1920`, `30` fps, video + audio streams, duration match), multi-frame pixel-diff motion (not a static image), and persisted `Artifact` records (`json_array_length(artifact_ids_json) > 0`).
