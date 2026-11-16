# Foundation changelog

## Unreleased — P0-03 contracts
- Added strict versioned JSON schemas and structural validation tests for domain, approval/job and model contracts.
- Finalized immutable revision/review separation, canonical hashes, job transitions, model snapshots and API error semantics in specifications.
- Recorded ADR-017–021, including legacy health versus future readiness; no new runtime endpoints or Phase 1 features.

## Unreleased — P0-02 acceptance fixes
- Fixed `.env.example` to create SQLite on first startup (`mode=rwc`), matching runtime defaults.
- Added clean-process startup/HTTP health, config validation, migration history and reconnect persistence regressions.

## v0.3 — 2026-11-16
- Renamed project references from **AI Video Studio** to **Scenivox**; recorded official GitHub repository.
- Replaced proposed FastAPI core with CEO-approved **Rust/Axum/Tokio/SQLx+SQLite** direction.
- Kept Python/Node/Playwright/FFmpeg as skill-native processes; OMP remains development agent, not runtime requirement.
- Preserved and detailed model/provider selection per task, per-project and per-run with mocks and strict config/credential boundaries.
- Clarified untrusted generated code / third-party ZIP safety, actual moving-video quality gates, WSL portability and no-UI STOP gate.
- Added `TECH_STACK.md` and `PHASE_0_HANDOFF.md`; reworked phase/backlog contracts and acceptance.

## v0.2 (prior source)
- Introduced backend model/provider selection and updated Trello phase backlog.
