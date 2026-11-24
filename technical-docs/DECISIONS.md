# Scenivox — Architecture Decisions (ADR-02 Rebaseline)

**Status:** Refined for final PM merge review on `adr/02-skill-driven-runtime`. Baseline code verified at `53db65bc72b0cc37162a5bbcac3689f126cdcf97`.

This document records the **ADR-02 AI-native skill-driven architecture rebaseline**, preserving Phase 0–1 foundations (Rust/Axum/Tokio/SQLx+SQLite core, immutable revisions, hash-bound editorial approval, provider-neutral selection, offline mock CI, and the Phase 3 Web UI HOLD gate) while explicitly documenting intentional supersessions.

---

## ADR Summary (ADR-02)

| ID | Decision | Status | Supersession / Relationship |
|---|---|---|---|
| **ADR-022** | In-process Rust AI agent runtime with manifest-declared tools and entrypoints only; no unrestricted shell access; no Claude Code / OMP / Antigravity runtime dependency | Accepted direction (pending merge) | Extends ADR-008, ADR-013 |
| **ADR-023** | Skill bundles imported, hash-pinned, and discovered read-only; `audit_status` is host-owned evidence (never manifest self-certified); instructions/outputs/code/media all untrusted | Accepted direction (pending merge) | Extends ADR-005, ADR-007 |
| **ADR-024** | Unified composition generation and skill-specific internal rendering strategies | Accepted direction (pending merge) | **Intentionally supersedes** Phase 0–1 forced per-scene render assumption |
| **ADR-025** | Linux OCI container isolation as MVP baseline (including Windows via WSL2); isolated MediaQA (`ffmpeg`/`ffprobe`); no auto-pulls, host secrets, Docker socket, `--privileged`, broad mounts, or network | Accepted direction (pending merge) | **Intentionally strengthens** basic host-subprocess safety minimum |
| **ADR-026** | Three independent gates (`Approval`, `CostConsent` with atomic reservation/conservative reconciliation, and single-job/policy/evidence-bound `ExecutionAuth`); distinguishes zero-cost offline runs from remote cost auth | Accepted direction (pending merge) | Extends ADR-017, ADR-020 |
| **ADR-027** | Provider-independent `LlmProvider` abstraction (`mock`, local, opt-in remote; no silent fallback); user-supplied Indonesian WAV first, then licensed local TTS evaluation | Accepted direction (pending merge) | Extends ADR-008, ADR-012 |
| **ADR-028** | Exclusive worker startup and `worker_leases` fencing for job recovery (moved out of ordinary `db::connect`) | Accepted direction (pending merge) | **Intentionally supersedes** P1-04 `db::connect()` recovery once `worker.rs` lands |
| **ADR-029** | Phased Phase 2 sequence separating auth ledgers (`P2-02B`) from Indonesian speech verification (`P2-02C`), with an offline mock-provider OCI vertical slice (`P2-03B`) first; `README` & legacy worktree cleanup deferred | Accepted direction (pending merge) | Revises Phase 2 delivery sequence |

---

## Decision Details

### ADR-022 — In-process Rust AI Agent Runtime & Manifest-Declared Tools
- **Decision:** Scenivox embeds a provider-independent AI agent loop in Rust (`src/agent.rs`) driving `LlmProvider` tool-calling turns against manifest-declared tools and allowlisted entrypoints (`workspace_list`, `workspace_read`, `workspace_write`, `workspace_patch`, `skill_run_step`, `probe_media`, `complete_composition`). Unrestricted shell access (`sh -c` with arbitrary commands) is prohibited.
- **Consequences:** All turns, tool calls, repair iterations, tokens, and wall-clock limits are bounded and logged in `render_job_events`.

### ADR-023 — Untrusted Skill Bundles, Host-Owned Audit Evidence & Untrusted Artifacts
- **Decision:** Skill bundles (`skill.manifest.json`, `SKILL.md`, `references/`, `scripts/`, `assets/`) are imported locally, checked against traversal/zip bombs/symlinks, and hash-pinned (`bundle_sha256`). Skill instructions (`SKILL.md`), model outputs, generated JS/Python code, and produced media artifacts are all treated as **untrusted**. `audit_status` is **host-owned evidence** stored by Scenivox in SQLite (`skill_bundles`), never a trusted field from an imported manifest.
- **Consequences:** Bundle updates require a new version/hash; `ffprobe`/`ffmpeg` inspection of generated media runs inside an isolated, bounded media-validation container.

### ADR-024 — Unified Composition & Skill-Specific Rendering *(Supersedes Per-Scene Assumption)*
- **Decision:** The agent generates a unified composition for the approved storyboard and P2-02 `Timeline` / `LocalNarration` sidecars, while each skill chooses its own internal rendering strategy inside its sandbox.
- **Consequences:** Cross-scene transitions and continuous audio mix work naturally; partial single-scene rerender remains deferred to Phase 2.5 where manifests explicitly declare support.

### ADR-025 — Linux OCI Container Isolation (Including Windows via WSL2) & Isolated MediaQA
- **Decision:** Linux OCI container isolation (`--network=none`, `--pull=never` against pinned local image digests, `--read-only`, `--cap-drop=ALL`, `--security-opt=no-new-privileges`, non-root UID/GID, memory/CPU/PIDs/disk/time caps) is the MVP execution baseline; Windows hosts execute via WSL2 Linux OCI containers. Host secrets, Docker/Podman sockets, `--privileged` containers, unrestricted host mounts, automatic OCI image downloads, and network access from generated-code execution are explicitly prohibited. Media inspection (`ffmpeg` / `ffprobe`) also executes inside an isolated, bounded OCI container.
- **Consequences:** Hosts lacking verified OCI/WSL2 isolation or pinned local images fail closed (`503 sandbox_unavailable`).

### ADR-026 — Three Independent Gates, Conservative Budget Reconciliation & Bound Execution Auth
- **Decision:** Separate:
  1. **Editorial `Approval`:** exact `revision_id` + `content_sha256`.
  2. **`CostConsent`:** required for `remote: true` (or paid) model/TTS calls, with atomic pre-call reservation (`spent_micros + reserved_micros <= max_cost_micros`) and conservative post-call reconciliation inside SQLite `BEGIN IMMEDIATE`. Unknown provider pricing or missing usage fails closed (and settles open reservations conservatively at `reserved_micros`). **Zero-cost offline execution** (`remote: false` models + user-supplied WAV) incurs no external spend and needs no paid `CostConsent`.
  3. **`ExecutionAuth`:** single-job authorization bound to exact `project_id`, `revision_id`, `content_sha256`, `job_id`, `skill_id`, `skill_version`, `bundle_sha256`, `runtime_policy_sha256`, and recorded post-run `workspace_manifest_sha256` evidence.
- **Consequences:** Removing any applicable gate blocks execution before provider calls or sandbox launch.

### ADR-027 — Provider Abstraction & Indonesian Audio Strategy
- **Decision:** Support deterministic `mock`, local (`remote: false`), and opt-in remote (`remote: true`) `LlmProvider` adapters with zero silent fallback. Support **user-supplied Indonesian WAV (`P2-02`) first**, followed by licensed local TTS/alignment evaluation (`P2-02C`).

### ADR-028 — Exclusive Worker Startup & Lease Fencing *(Supersedes P1-04 `db::connect` Recovery)*
- **Decision:** Once `worker.rs` is introduced in Phase 2, move active-job interruption recovery (`preparing`..`validating` -> `interrupted`) **out of ordinary `db::connect()`** into exclusive worker startup guarded by SQLite `worker_leases` with **monotonic lease fencing** (`lease_epoch` checked on every job state transition).
- **Consequences:** Ordinary CLI reads or API connections never interrupt active worker jobs, and deposed/stale workers cannot commit late transitions.

### ADR-029 — Phased Vertical Slice & Deferred Cleanup
- **Decision:** Separate asset/cost/execution authorization implementation (`P2-02B`) from real Indonesian speech verification (`P2-02C`), and run an **offline mock-provider OCI vertical slice (`P2-03B`)** using `stick-figure` + user/fixture WAV before integrating remote LLMs or real TTS. `README.md` roadmap and legacy worktree cleanup remain deferred.
