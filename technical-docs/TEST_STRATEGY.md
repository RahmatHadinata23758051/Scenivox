# Scenivox Test & Acceptance Strategy — ADR-02

## 1. Automated CI Layers (Offline, Deterministic)
- **Rust baseline:** `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`. CI runs without paid accounts, network calls, or large downloads.
- **Phase 1 & P2-02 regressions (implemented):** JCS hash vectors, SQLite migrations/reopen, concurrent stale-write protection, HTTP idempotency, review/approval triggers, fail-closed `render_jobs`, JSON Schema 2020-12 contract tests (`LocalNarration` included), bounded PCM WAV inspection, `1..=60` scene timing, and CLI path confinement.
- **Planned Phase 2 unit & integration layers:**
  - **Skill import:** reject zip bombs, traversal (`..`, `/`, `:`), symlinks, duplicate entries, invalid manifests; verify `bundle_sha256` pinning and read-only capability discovery.
  - **Three-gate authorization:** reject enqueue/retry when any of `Approval`, `CostConsent`, or `ExecutionAuth` is missing, stale, revoked, exhausted, or mismatched; reject when `SandboxBackend::is_available() == false`.
  - **Agent runtime (`DeterministicMockLlm`):** bounded turn/tool-call enforcement, manifest `writable_paths` enforcement, iterative repair from simulated render error to passing output, and explicit failure on exhausted repair/token/cost budgets.
  - **Sandbox isolation:** fail closed when sandbox unavailable, verify `--network=none`, verify host secrets/DB/`$HOME` unreadable, verify timeout kills descendant process tree cleanly.

## 2. Required Negative Cases (Phase 2 Additions)
1. Draft/unapproved/stale revision -> `409` and **zero** provider calls or sandbox spawns.
2. Remote `LlmProvider` / TTS without active `CostConsent`, or mid-job budget exhaustion -> `409 remote_cost_not_approved` / `cost_limit_exceeded`; no silent fallback to another model or `mock`.
3. Missing `ExecutionAuth` or unavailable host sandbox -> `409 execution_not_authorized` / `503 sandbox_unavailable`; never run generated JS/Python unsandboxed.
4. Generated JS/Python attempting to read `/etc`, host `.env`, `scenivox.db`, or open outbound HTTP -> blocked by sandbox mounts/network isolation.
5. `ffprobe` failure, static/frozen frames (zero pixel diff across sampled timestamps), missing audio stream, or narration exceeding scene allocation -> job fails explicitly (`MediaQA`).

## 3. Golden End-to-End Acceptance (Phase 2 — `P2-04`)
- **Prompt:** `Misteri eksperimen rahasia di bawah tanah yang disembunyikan selama puluhan tahun` (`id-ID`, `9:16`, `1080×1920`, `30` fps, target `60` s).
- **Flow:** Plan/create multi-scene draft -> edit scene 3 -> approve exact immutable revision -> grant `CostConsent` (if remote) & `ExecutionAuth` -> unified composition generated & repaired by agent inside sandbox -> host `MediaQA` verifies `ffprobe` streams, duration, and temporally separated frame motion -> human review of Indonesian narration intelligibility and caption synchronization.
- **Evidence required:** exact commit SHA, skill `bundle_sha256`, frozen model snapshots, agent turn/cost trace, `ffprobe` JSON, sampled frame timestamps/diffs, peak RSS/wall-clock runtime, and zero leaked secrets.
