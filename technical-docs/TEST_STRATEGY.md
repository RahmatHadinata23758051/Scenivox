# Scenivox Test & Acceptance Strategy — ADR-02

## 1. Automated CI Layers (Offline, Deterministic)
- **Rust baseline:** `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`. CI runs without paid accounts, network calls, or large downloads.
- **Phase 1 & P2-02 regressions (implemented):** JCS hash vectors, SQLite migrations/reopen, concurrent stale-write protection, HTTP idempotency, review/approval triggers, fail-closed `render_jobs`, JSON Schema 2020-12 contract tests (`LocalNarration` included), bounded PCM WAV inspection, `1..=60` scene timing, and CLI path confinement.
- **Planned Phase 2 unit & integration layers:**
  - **Skill import:** reject zip bombs, traversal (`..`, `/`, `:`), symlinks, duplicate entries, invalid manifests; verify `bundle_sha256` pinning and read-only discovery.
  - **Three-gate & ledger invariants:** reject enqueue/retry when `Approval`, `CostConsent`, or single-job `ExecutionAuth` is missing, stale, revoked, mismatched, or already bound to another `job_id`; test atomic concurrent budget reservation/reconciliation and fail-closed rejection of unknown pricing/missing usage.
  - **Exclusive worker ownership:** verify ordinary `db::connect()` reads do **not** interrupt active jobs; verify `worker_leases` blocks dual active workers and recovers stale active jobs + orphaned containers/workspaces on exclusive worker startup.
  - **Agent runtime (`DeterministicMockLlm`):** manifest-declared tools/entrypoints only, `writable_paths` containment, iterative repair from simulated render failure, and explicit failure on exhausted repair/token/cost limits.
  - **OCI sandbox & isolated MediaQA:** fail closed (`sandbox_unavailable`) when OCI/WSL2 unavailable; verify `--network=none`, zero host secrets/Docker socket/privileged caps/broad mounts, and bounded isolated `ffmpeg`/`ffprobe` execution.

## 2. Offline Mock-Provider Vertical Slice (`P2-03B`) vs. Real Media (`P2-02C` / `P2-04`)
1. **Offline mock-provider OCI vertical slice (`P2-03B`, runs before remote LLMs and real TTS):**
   - Uses `DeterministicMockLlm` + imported `stick-figure` bundle + user-supplied/fixture PCM WAV (`P2-02`) inside the Linux OCI `--network=none` sandbox and validates output via the isolated MediaQA OCI container.
   - Proves unified composition, agent tool boundaries, OCI isolation, `ExecutionAuth` workspace-evidence binding, and real `ffprobe` + pixel-diff motion checks with **zero** remote API keys or TTS dependencies.
2. **Real Indonesian narration & alignment verification (`P2-02C`, separate from `P2-02B` authorization ledgers):**
   - Evaluates user-supplied `id-ID` WAV + forced alignment and licensed local TTS quality, pronunciation, and caption sync independently of authorization-ledger tests.
3. **Golden 60s Indonesian E2E (`P2-04`):**
   - `Misteri eksperimen rahasia di bawah tanah yang disembunyikan selama puluhan tahun` (`id-ID`, `9:16`, `1080×1920`, `30` fps, `60` s): approved revision + scene 3 edit -> `CostConsent` + `ExecutionAuth` -> unified composition in OCI sandbox -> isolated MediaQA (`ffprobe` + multi-frame pixel-diff motion) -> human audio/caption review, capturing commit SHA, `bundle_sha256`, `workspace_manifest_sha256`, trace, peak RSS/runtime, and zero leaked secrets.
