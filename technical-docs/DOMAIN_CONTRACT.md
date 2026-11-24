# Scenivox Domain Contract v1 — ADR-02 Rebaseline

**Status:** P1-01 (immutable revisions), P1-02 (offline planner), P1-05 (model registry/selection), P1-03 (review & editorial approval), P1-04 (durable `RenderJob` schema, triggers, and fail-closed enqueue/read/retry/cancel), and P2-02 (WAV inspection & internal `Timeline` sidecar) are **implemented** at `53db65bc72b0cc37162a5bbcac3689f126cdcf97`. Skill registry, atomic cost-consent ledger, bound execution-authorization ledger, asset registry, AI agent runtime, OCI sandbox/MediaQA, and exclusive worker remain **planned Phase 2**.

See [`schemas/v1/contracts.schema.json`](../schemas/v1/contracts.schema.json) for validated v1 JSON Schema shapes.

---

## 1. Core Records & Ownership

| Record | Ownership & Invariants | Status |
|---|---|---|
| `Project` | Mutable workspace metadata (`id`, `title`, `idea`, `target`, per-role `model_selection`, `current_revision_id`, `approved_revision_id`). | Implemented |
| `Revision` | Immutable snapshot (`id`, `project_id`, `parent_revision_id`, `version`, `created_at`, `content`, `content_sha256`). Copy-on-write on edit clears `approved_revision_id`. | Implemented |
| `RevisionReview` | Mutable workflow state (`draft` / `in_review` / `approved`) separate from immutable content. | Implemented |
| `Approval` | Append-only hash-bound editorial evidence (`id`, `project_id`, `revision_id`, `content_sha256`, `approved_at`). | Implemented |
| `RenderJob` | Durable lifecycle envelope copying exact approved `RevisionContent` snapshot (`queued -> preparing -> voicing -> animating -> mixing -> validating -> succeeded | failed | cancelled | interrupted`). | Implemented (fail-closed) |
| `LocalNarration` | Bounded PCM WAV inspection metadata (`schema_version`, `AudioInfo`, `provenance`: `user_supplied` \| `synthetic_test_fixture`). Sidecar only. | Implemented |
| `SkillBundle` | Pinned bundle `(skill_id, skill_version, bundle_sha256, manifest_json, audit_status, enabled)`. | Planned Phase 2 |
| `CostConsent` | Scoped remote/paid spend limit with atomic pre-call reservation and post-call reconciliation (`budget_reservations`). | Planned Phase 2 |
| `ExecutionAuth` | Job-bound authorization pinning `(project_id, revision_id, content_sha256, job_id, skill_id, skill_version, bundle_sha256, runtime_policy_sha256)` plus recorded `workspace_manifest_sha256` evidence. | Planned Phase 2 |
| `WorkerLease` | Exclusive worker ownership row (`worker_id`, `pid`, `heartbeat_at`, `lease_expires_at`) governing startup recovery and job execution. | Planned Phase 2 |
| `RenderJobEvent` | Append-only, secret-scrubbed agent turn, tool-call, repair, reservation, and MediaQA trace. | Planned Phase 2 |
| `Artifact` | Content-addressed outputs (`final.mp4`, stills, `workspace_manifest`, `qa_manifest`) linked to a job. | Planned Phase 2 |

---

## 2. Unified Composition & P2-02 Sidecar Relationship (ADR-02)

- **Storyboard & Scenes:** V1 limits (`id-ID`, `9:16`, `30` fps, `1..=60000` ms total, `1..=60` scenes, exact sum) and stable `scene_id_mapping` (`order -> scene_id`) remain unchanged.
- **P2-02 Narration & Timing Sidecars:**
  - `LocalNarration` validates structural metadata and ceiling-ms consistency (`ceil(frame_count * 1000 / sample_rate_hz)`). Structural validation never proves audio authenticity or speech content.
  - `Timeline` remains an internal sidecar validating `1..=60` scenes, unique nonempty IDs, positive allocations (`<= 60,000` ms total), cumulative 30 fps frame boundaries (`round(ms * 30 / 1000)`), and no narration truncation (`audio.duration_ms <= allocation_ms`).
  - `SceneTiming` preserves `audio_provenance` and `caption_provenance` (`none`, `unverified_external`, `measured_provider_alignment`); `SyntheticTestFixture` can never qualify as verified narration or `measured_provider_alignment`.
- **Unified Composition Input:** Rust stages frozen `revision.json` + verified `narration.wav` + `timeline.json` / `words.json` into the OCI sandbox workspace. The agent generates one unified composition (`final.mp4`) using the skill's internal rendering strategy.

---

## 3. Enqueue, Authorization Binding, Exclusive Worker Recovery & MediaQA

1. **Atomic Enqueue Gate:** Enqueue must name `revision_id`, `content_sha256`, and (when Phase 2 gates are wired) `execution_auth_id` / `cost_consent_id`, atomically verifying:
   - Current approved revision + matching `Approval` + `revision_reviews.review_state == 'approved'`.
   - Resolved `asset_hashes` and `sfx_cues` in `assets`.
   - Pinned `(skill_id, skill_version, bundle_sha256)` enabled in `skill_bundles`.
   - Active `CostConsent` with verified provider pricing for any `remote: true` model/TTS.
   - Active, unused `ExecutionAuth` bound to exact `(project_id, revision_id, content_sha256, skill_id, skill_version, bundle_sha256, runtime_policy_sha256)` — atomically bound to the newly created `job_id` so it cannot be replayed across jobs!
   - `SandboxBackend::is_available() == true` (Linux OCI / WSL2).
   - Any failure inserts **zero** `render_jobs` rows.
2. **Exclusive Worker Startup & Ownership Recovery (Revises P1-04):**
   - *Current P1-04 state (`53db65bc72b0cc37162a5bbcac3689f126cdcf97`):* `db::connect()` marks active jobs `interrupted` because no background worker exists yet.
   - *ADR-02 Phase 2 target:* Remove job-state mutation from ordinary `db::connect()` so read-only CLI commands, API requests, and tests never interrupt live jobs. Instead, `worker.rs` acquires an exclusive SQLite `worker_leases` lock inside `BEGIN IMMEDIATE` at startup (failing if another live worker holds an unexpired lease), transitions stale active jobs (`preparing`..`validating`) to `interrupted` (`error_code='startup_interrupted'`), conservatively settles any orphaned `budget_reservations`, and kills/cleans orphaned OCI containers and temp workspaces before polling `queued` jobs.
3. **Retry:** Retrying `failed` / `cancelled` / `interrupted` creates a **new** `job_id` bound to the original frozen `snapshot`, after requiring fresh/valid `ExecutionAuth` (bound to the new `job_id`), `CostConsent`, current revision approval, pinned skill, and sandbox readiness.
4. **Evidence & Isolated MediaQA:** Before marking `succeeded`, the worker computes `workspace_manifest_sha256` (hashes of generated composition files) and binds it to the job/`ExecutionAuth` evidence, then runs `ffmpeg`/`ffprobe` inspection inside an isolated, bounded `--network=none` OCI MediaQA container to verify `1080×1920`, `30` fps, H.264 + audio streams, exact/bounded duration, and non-zero multi-frame pixel-diff motion, persisting `Artifact` rows.
