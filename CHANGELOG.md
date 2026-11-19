# Foundation changelog

## Unreleased — P1-03 review and approval
- Added separate per-revision review state and hash-bound append-only approval evidence with SQLite transition/immutability guards.
- Added shared transactional Store workflows, HTTP review/withdraw/approve and evidence lookup routes, and CLI review commands.
- Added regression coverage for conflicts, idempotency, concurrent approval, reopen persistence, immutable evidence, and edited scene prompt provenance.
- Approval is editorial only; it does not authorize rendering, paid access, or skill execution.

## Unreleased — P1-04 foundation QA
- Added idempotent startup recovery for unfinished render jobs and strict cancel-command schema-version validation.
- Added isolated Store/HTTP/CLI authorization, no-enqueue, persistence, constraint, transition, cancellation and recovery regressions. Enqueue/retry stay fail-closed; successful authorization remains deferred pending audited skill, asset and cost evidence.

## Unreleased — DOC-01 public documentation
- Replaced README artwork with the owner's three unchanged WebP images in `img/`.
- Clarified implemented persistence versus planned model, approval and media workflows.
- Relocated public specifications to `technical-docs/`; ignored and untracked local `docs/` planning/audit material without deleting local copies or rewriting history.

## Unreleased — P1-01 persistence
- Added SQLite project CRUD and immutable revision/storyboard/scene persistence shared by HTTP and CLI.
- Added transactional stale-write protection, strict bounded JSON, server-owned scene IDs, canonical JCS SHA-256 hashes and persisted POST idempotency.
- Added reopen, validation rollback, concurrent writer, HTTP and real CLI process regressions. No providers, approval, rendering or UI implemented.

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
