# Scenivox Backlog Snapshot — v0.3

**Authoritative live status:** https://trello.com/b/fG3KW5bF/ai-video-studio-development  
This file captures requirements, not current Kanban position. Do not mark complete from documentation alone.

| Task | Phase | Priority | Deliverable / test gate |
|---|---|---|---|
| P0-01 | 0 | P0 | Static security/license/architecture audit of creator-provided skill ZIP; verified inventory, blockers and safe smoke proposal |
| P0-02 | 0 | P0 | **Rust** workspace, Axum/Tokio/SQLx(SQLite) bootstrap, CLI skeleton, migrations and offline test/CI baseline; **no web UI** |
| P0-03 | 0 | P0 | Resolve schema, model capability contract, approval state machine and documented ADRs; PM/CEO signoff |
| P1-01 | 1 | P0 | SQLite project, storyboard, immutable revision persistence and tests |
| P1-05 | 1 | P0 | Backend model registry/selection (global/project/run; planning vs code), capabilities, config status and secure API/CLI |
| P1-02 | 1 | P0 | Typed model-neutral planner and deterministic mock JSON storyboard; reject invalid responses |
| P1-03 | 1 | P0 | Scene/narration edits create revision; approval binds exact hash and immutability |
| P1-04 | 1 | P0 | API/CLI guard: reject unapproved/stale render requests and unapproved costs |
| P2-01 | 2 | P0 | Audited stick-figure skill adapter, safe Rust process executor, real animated test scene |
| P2-02 | 2 | P0 | Indonesian audio path / TTS adapter + timing validation |
| P2-03 | 2 | P0 | Durable local job worker and stage progress with interruption/retry tests |
| P2-04 | 2 | P0 | Caption timing, audio mix, real MP4 via FFmpeg with ffprobe/media QA |
| P2-05 | 2 | P0 | End-to-end approved 60s mystery animation and manifest; CEO creative QA |
| P25-01 | 2.5 | P1 | Safe cache/partial-scene rerender, cancellation and failure recovery when supported |
| P25-02 | 2.5 | P1 | Second skill feasibility and registry compatibility after first engine works |
| P3-01 | 3 | **HOLD** | UI design discovery only after CEO shares references; no frontend code yet |

## Task ordering
Phase 0 audit first. P0-02 scaffold uses Rust baseline. In Phase 1, implement P1-05 before or in parallel with P1-02 so AI calls can select independent models, but keep domain state tests deterministic. No coding task proceeds past assigned PM-approved scope.
