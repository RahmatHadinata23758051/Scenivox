# Scenivox Delivery Plan — v0.3

**Management:** CEO sets phase scope; PM reviews work and Trello; OMP implements, commits and reports.  
**GitHub:** https://github.com/RahmatHadinata23758051/Scenivox · **Trello:** https://trello.com/b/fG3KW5bF/ai-video-studio-development

## Phase 0 — Foundations (first assignment: P0-01 only)
Read-only audit of existing skill ZIP and static safety/license/feasibility findings. After review, bootstrap **Rust/Axum/Tokio/SQLx+SQLite** (P0-02) and validate schemas/ADRs (P0-03). **Gate:** safe verified skill path, realistic 60s feasibility constraints, reproducible Rust dev environment and stable contracts. No frontend.

## Phase 1 — Core planning backend
Rust API+CLI supports project CRUD, task-specific model registry/selections, validated mock storyboard generation, scene edits/revisions, approvals, guarded job enqueue and persistence. **Gate:** reproducible offline `select models → create project → draft → revise → approve`; API rejects stale/unapproved and invalid models. No renderer requirement yet and **no website**.

## Phase 2 — Real output
Adapter for audited Node/Python skill runtime, TTS/verified Indonesian WAV, audio timing and captions, durable render job worker, FFmpeg mux, MP4 manifest/verification. **Gate:** a *real* 9:16 animated video (~60s target) with narration + subtitles, measured output and CEO manual viewing. Mock render does not pass.

## Phase 2.5 — Stabilize & extend
Retry, cache/partial render, clean failure recovery, capability-based second skill integration if appropriate and fresh-machine local setup notes. **Gate:** reproducible install/smoke workflow with resource measurements and no unexplained background processes.

## Phase 3 — Website Studio (CEO-controlled STOP gate)
No visual decisions, CSS or Next.js scaffolding until CEO provides design references and explicitly approves frontend scope. Then create UX brief and separate backlog. Current API contracts should not bias UX.

## OMP handoff requirements per card
Card ID + goal, files changed, tests and commands (including failed tests), evidence/measurements, security/licenses/risks, commit SHA and recommendation. OMP may use subagents; do not proceed to next card without PM review.
