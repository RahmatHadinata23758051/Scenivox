# Scenivox — Backend & Media Architecture (v0.3)

**Decision:** Rust-first backend core is CEO-approved direction; exact internal modules are implementation proposals for OMP to validate. The target is **small, local-first, one-user**, not distributed infrastructure.

## 1. System boundaries
```text
Future website (NOT authorized in Phase 0–2.5)
      │ REST / job polling / safe asset links
      ▼
┌──────────────────────────── Rust Scenivox Core ────────────────────────────┐
│ Axum API + Rust CLI (same domain services)                                  │
│ Project/Storyboard/Revision/Approval  │ Provider Registry & Model Settings  │
│ Planner Adapter / Structured JSON     │ Job Manager / Safe Process Runner   │
└───────┬───────────────────────┬─────────────────────┬──────────────────────┘
        │                       │                     │
   SQLite (SQLx)         local project files     selected supported API
  metadata+jobs          hash/manifest JSON       or deterministic mock
                                │
                         audited Skill Adapter (versioned JSON I/O)
                                │ controlled subprocess / one render default
              ┌─────────────────┼─────────────────────────────────┐
              │                 │                                 │
        Python / TTS       Node + Playwright                      FFmpeg
        speech/timing      Canvas scene frames                   mux/encode
              └─────────────────┼─────────────────────────────────┘
                                ▼
              scene clips + WAV + captions + final MP4 + QA manifest
```

**Crucial:** OMP is a development coding-agent harness only. The studio must function without OMP installed at runtime; its own model adapter handles planner and scene-generation AI tasks.

## 2. Baseline components
- **Core binary/crate workspace:** Rust (stable), Axum REST, Tokio I/O, `serde` contracts, `tracing`/structured logs, SQLx migrations with SQLite. A shared domain layer serves CLI and API to avoid duplicated business logic.
- **Local store:** SQLite DB for projects, revisions, jobs, selected model IDs and artifact metadata. Store media files in configured workspace directories; expose only opaque relative artifact IDs, never arbitrary requested absolute paths.
- **Model registry:** task-specific capabilities (`storyboard_planning`, `scene_code_generation`) and separate default, project, run overrides. Listing is read-only (no credit spend). Explicit connection probe must be opt-in. Mock adapter is the baseline functional implementation; 9router or other providers are future supported adapters only after endpoint/credentials verification.
- **Planner:** typed request → bounded LLM response → validated storyboard schema; reject or safely retry malformed JSON. AI output is data until validated; executable scene code is a separate risky boundary.
- **Render scheduler:** durable `RenderJob` snapshots including approved revision hash, resolved model IDs, skill ID/version, parameters, budget and asset IDs. Default one concurrent render, worker loop in Rust; no Redis/Celery/MQ service for MVP.
- **Skill adapter:** controlled entrypoints per audited capability manifest, versioned JSON files and approved tool arguments. `SKILL.md` alone is not executable or evidence of supported capabilities.
- **Media pipeline:** source narration or verified Indonesian TTS → alignment/caption timing → animated shots via Canvas/Playwright (or compatible tool) → FFmpeg mix/mux → `ffprobe` checks and manual media review.

## 3. Persistence / state integrity
- `Project` → many `StoryboardRevision` → ordered `Scene` records. Each revision is immutable once saved as versioned snapshot; modification is **copy-on-write** to a new draft revision.
- `Project.approved_revision_id` only points to explicitly approved revision, not implicitly latest. Rendering always binds **that ID and its content hash**. When the creator edits the project, new revision is not approved; require a new explicit approval for that revision.
- `ModelSelection`: task role + provider/model IDs; resolution order global default → project override → run override (only when explicitly provided). Snapshot the resolved model + non-secret inference settings in revision/job records; changing defaults has no effect on already approved plans/jobs.
- `RenderJob`: `queued → preparing → voicing → animating → mixing → validating → succeeded`; any running state can transition to `failed`/`cancelled`. Running-job restart behavior must be defined and tested (e.g., mark `interrupted` and safe retry), never mislabel as succeeded.
- Project/storyboard state: `draft → in_review → approved → superseded`, with revision-bound approval. Ensure API rejects mismatched/unapproved revision (HTTP 409) and malformed schema (HTTP 422).

## 4. Rust subprocess safety & scheduling
- Do not execute arbitrary LLM-text as shell. Choose an allowlisted executable and argument vector; workspace-root path validation, restricted CWD/env and output capture.
- Enforce timeouts, single-render concurrency initially, CPU/memory budgets where host supports them, output size limits, and process-tree cleanup. Tokio `kill_on_drop` exists but **does not automatically guarantee cleanup of grandchildren**; design robust termination/reaping, particularly Playwright/Chromium child processes.
- Full isolation of AI-generated code is **not** guaranteed by subprocess alone. Prefer audited skill code/spec validation and further OS/container isolation if arbitrary JS/Python generation is permitted. Fail closed where isolation is unavailable.
- Keep queue and recovery simple: persisted SQLite job table plus background worker loop in the Rust process; no remote worker/streaming infrastructure before proven necessary.

## 5. Skill ZIP baseline
`stick-figure-windows-v2.zip` is local third-party input. Prior inspection suggests JavaScript Canvas, Playwright Chromium, Python TTS/alignment, FFmpeg, portrait frame support. This is a **candidate**, not proof of 60-second Indonesian end-to-end generation. Phase P0-01 must verify origin/license/entrypoints and attempt an explicitly approved safe smoke run only after static audit.

## 6. UI boundary
REST/CLI should expose models, drafts, revision editing, approval, jobs and safe artifact listings. **Do not build, style or scaffold a web interface in phases 0–2.5.** A future UI framework is only a candidate, not selected.

## 7. Scope, ownership, and success
OMP may propose alternate crates/boundaries with testable evidence; core language change requires PM review and CEO approval. End-to-end success is a **measured playable MP4 with actual moving frames, voice and captions**, not a server returning `200 OK` or a purely hypothetical mock renderer.
