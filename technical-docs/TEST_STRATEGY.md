# Scenivox Test & Acceptance Strategy — Baseline + ADR-02

## 1. Automated layers (Baseline)
- **Rust quality:** `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`. Make CI independent of external AI subscriptions.
- **Unit:** scene duration math; revision/approval state transitions; provider capability selection, overrides and budget guard; safe path resolution; child-process timeout behavior; validation of structured generated JSON.
- **Integration:** SQLx SQLite migrations and reopen; Axum API response/status contract; Rust CLI using same services; deterministic mock planner and mock voice; idempotent job enqueue; crash/retry behavior.
- **Real engine (local/release gate):** vetted skill smoke example and full video; use `ffprobe` dimensions/fps/codecs/duration/audio and a sample of temporally separated frames to confirm actual motion; review captions and audio quality manually.

## 2. Required negative cases (Baseline)
1. Rendering a draft/unapproved or stale revision returns `409` and launches **no renderer**.
2. Editing an approved scene creates new draft while retaining original job snapshot; default-model changes do not mutate it.
3. Invalid or unavailable model/provider, unsupported capability, missing credentials and unapproved paid calls fail explicitly; never auto-fallback.
4. Malicious paths, traversal, URLs/scripts in LLM output, unknown skill IDs and excessive payload sizes are rejected.
5. Missing Node/Chromium/FFmpeg/TTS, subprocess timeout, child process cleanup and job interruption produce accurate status/diagnostics.
6. Narration exceeding scene time must be reported or resolved by explicit timing rules, not silently truncated.
7. Job success implies artifacts exist and actually pass media validation, not just that a command returned zero.
8. Any network-exposed backend mode requires explicit security review; loopback default must be tested.

## 3. Golden end-to-end (Phase 2)
Project: `Misteri eksperimen rahasia di bawah tanah yang disembunyikan selama puluhan tahun`; duration 60 seconds, language `id-ID`, portrait `9:16`. Generate multi-scene draft, edit scene 3, approve exact revision, run audited unified composition inside OCI sandbox, produce `final.mp4` passing isolated MediaQA (`ffprobe` + multi-frame pixel-diff motion), and inspect camera/action changes, intelligible Indonesian narration, and synchronized captions. Log model selection, skill `bundle_sha256`, `workspace_manifest_sha256`, provenance, asset hashes, agent trace, and measured runtime/peak resource use.

## 4. Environment / CI
- Mock CI must work offline without large model downloads or service accounts. Use temp SQLite, disposable local files, deterministic fixtures and avoid committing large videos.
- Heavy real-render tests may run only locally initially, with commands, hashes and metadata captured for PM/CEO review. A synthetic generated sample can test mechanics but **cannot** alone pass the creative quality gate.
- Windows/WSL portability must be explicitly measured. Do not claim Windows native compatibility from a WSL pass.

## 5. Model selection matrix
- List configured vs unconfigured/unknown models without secrets or implicit connection probes.
- Select different supported mock models for `storyboard_planning` and `scene_code_generation` independently.
- Verify precedence: `global default < project override < per-run selection`, with no silent fallback.
- Persist selections and validate reruns; ensure existing approved revision/job records retain their snapshot after a settings change.

## 6. P0-03 & P2-02 contract validation boundary
`cargo test --test contracts` compiles the self-contained 2020-12 JSON Schema bundle and validates representative `Project`, `Revision`, `Approval`, `RenderJob`, `Model`, `LocalNarration` and `Error` records, with embedded `Storyboard`/`Scene`/`AudioInfo` values. It rejects wrong schema versions, required-field omission, unknown fields, invalid scene/model/audio parameter values and unknown job/availability/provenance states. Validator dependencies are test-only, with remote reference retrieval disabled.

These are structural checks, not domain implementation. Phase 1 tests [DOMAIN_CONTRACT.md](DOMAIN_CONTRACT.md) invariants: cross-project references, duplicate scene IDs/contiguous order, exact total duration, JCS hash vectors, atomic stale writes, separate review/approval, immutable snapshots after default changes, no render override/fallback, no paid dispatch without consent, legal/illegal transitions, interruption and new-job retry. `/ready` outage semantics require separate future handler tests; P0-03 does not change `/health`.

---

## 7. ADR-02 Future Implementation Acceptance Requirements (Not Yet Implemented)

1. **Host-owned skill `audit_status`:** Reject/ignore self-declared audit fields in imported `skill.manifest.json`; verify imported skills default to `unaudited`, enforce zip-bomb/traversal/symlink rejection, and keep `bundle_sha256` immutable.
2. **Worker lease fencing:** Verify ordinary `db::connect()` reads never interrupt active jobs once `worker.rs` exists; verify `worker_leases` blocks concurrent dual-worker startup, fences stale-epoch writes from deposed workers, and recovers stale active jobs + orphaned OCI containers on exclusive worker startup.
3. **Conservative budget reconciliation:** Verify atomic SQLite `BEGIN IMMEDIATE` pre-call reservation prevents concurrent over-commit, unknown provider pricing or missing usage fails closed (`provider_pricing_unavailable` / `cost_reconciliation_failed`), and crash recovery settles open reservations conservatively at `reserved_micros`.
4. **Zero-cost offline vs. remote cost:** Verify offline `remote: false` jobs (e.g., `P2-03B` `DeterministicMockLlm` + user-supplied WAV vertical slice) run without paid `CostConsent` while still enforcing `Approval`, `ExecutionAuth`, and OCI isolation; verify `remote: true` calls always require valid `CostConsent`.
5. **Safe workspace access & OCI prohibitions:** Verify agent tools reject traversal, symlinks, `.env`/RO-mount writes, and undeclared `writable_paths`; verify OCI runner enforces `--network=none`, `--pull=never` (no automatic OCI image downloads), `--read-only`, `--cap-drop=ALL`, zero host secrets/Docker socket/broad mounts, and isolated `ffmpeg`/`ffprobe` MediaQA container execution.
6. **Separated speech track (`P2-02C`):** Keep asset/cost/execution-auth ledger tests (`P2-02B`) independent from real Indonesian TTS/alignment pronunciation and caption-sync verification (`P2-02C`).
