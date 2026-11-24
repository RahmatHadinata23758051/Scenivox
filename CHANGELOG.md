# Foundation changelog

## Unreleased — ADR-02 architecture specification rebaseline
- Recorded ADR-022–029 in `technical-docs/DECISIONS.md` and updated `technical-docs/*` contracts while preserving baseline v1 API/domain/model/testing invariants from `53db65bc72b0cc37162a5bbcac3689f126cdcf97` and explicitly separating planned Phase 2 extensions and supersessions:
  - Linux OCI container isolation as MVP baseline (including Windows via WSL2), `--pull=never` (no automatic OCI image downloads), isolated `ffmpeg`/`ffprobe` MediaQA, and explicit bans on host secrets, Docker sockets, `--privileged`, broad mounts, and network access during generated-code execution.
  - Manifest-declared tools/entrypoints only (no unrestricted shell); host-owned skill `audit_status`; untrusted skill instructions, model outputs, generated code, and produced media artifacts.
  - Atomic `CostConsent` budget reservation/conservative reconciliation failing closed on unknown pricing/usage (distinct from zero-cost offline execution), and single-job/policy/workspace-evidence-bound `ExecutionAuth`.
  - Exclusive `worker_leases` startup recovery and monotonic lease fencing (superseding ordinary `db::connect()` recovery in Phase 2), separated auth-ledger (`P2-02B`) vs. Indonesian speech (`P2-02C`), and an offline mock-provider OCI vertical slice (`P2-03B`) before remote LLMs/real TTS.
- Specification-only update; no Phase 2 runtime code, dependencies, or frontend added.

## Unreleased — P2-02 voice and timing foundation
- Added provider-neutral narration/alignment capabilities and typed errors, bounded PCM WAV inspection, and measured scene timing sidecars with cumulative 30 fps boundaries; no provider, database persistence, network call or render dispatch.
- Enforced v1 1–60 scene limit, `LocalNarration` structural integrity validation (and `schemas/v1/contracts.schema.json` contract), provenance preservation (`AudioProvenance` and `CaptionProvenance`), non-synthetic guard for measured alignment, and CLI empty-path/opened-handle regular-file checks with consistent error codes.
- Synthetic zero-sample WAVs are marked test-only. PATH checks found no `piper`, `espeak-ng`, `espeak`, or `ffprobe`; this is not an exhaustive system audit. No licensed Indonesian voice was identified or pronunciation-checked; that verification remains pending.

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
