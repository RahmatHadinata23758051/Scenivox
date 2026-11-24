# Scenivox — Architecture Decisions (ADR-02 Rebaseline)

**Status:** PM Conditional Approval refinements applied; awaiting final PM architecture review (no Phase 2 runtime code implemented yet). Baseline code verified at `53db65bc72b0cc37162a5bbcac3689f126cdcf97`.

This document records the **ADR-02 AI-native skill-driven architecture rebaseline** authorized by the CEO and refined per PM conditional approval, preserving Phase 0–1 foundations (Rust/Axum/Tokio/SQLx+SQLite core, immutable revisions, hash-bound editorial approval, provider-neutral selection, offline mock CI, and the Phase 3 Web UI HOLD gate).

---

## ADR Summary (ADR-02)

| ID | Decision | Status | Replaces / Extends |
|---|---|---|---|
| **ADR-022** | In-process Rust AI agent runtime with manifest-declared tools and entrypoints only; no unrestricted shell access; no Claude Code / OMP / Antigravity runtime dependency | Accepted direction (refinements applied) | Extends ADR-008, ADR-013 |
| **ADR-023** | Skill bundles (`SKILL.md`, `references/`, `scripts/`, `assets/`, `skill.manifest.json`) imported, hash-pinned, and discovered read-only; instructions/outputs/artifacts all untrusted | Accepted direction (refinements applied) | Extends ADR-005, ADR-007 |
| **ADR-024** | Unified composition generation and skill-specific internal rendering strategies | Accepted direction | Revises Phase 0–1 per-scene assumption |
| **ADR-025** | Linux OCI container isolation as MVP baseline (including Windows via WSL2); isolated MediaQA (`ffmpeg`/`ffprobe`); no host secrets, Docker socket, `--privileged`, broad mounts, or network | Accepted direction (refinements applied) | Strengthens process-safety minimum |
| **ADR-026** | Three independent authorization gates (`Approval`, `CostConsent` with atomic reservation/reconciliation, and single-job/evidence-bound `ExecutionAuth`) | Accepted direction (refinements applied) | Extends ADR-017, ADR-020 |
| **ADR-027** | Provider-independent `LlmProvider` abstraction (`mock`, local, opt-in remote; no silent fallback); user-supplied Indonesian WAV first, then licensed local TTS evaluation | Accepted direction (refinements applied) | Extends ADR-008, ADR-012 |
| **ADR-028** | Exclusive worker startup/lease ownership for job recovery (moved out of ordinary `db::connect`) | Accepted direction (refinements applied) | Revises P1-04 `db::connect` recovery |
| **ADR-029** | Revised Phase 2 sequence separating auth ledgers from Indonesian speech verification, with an offline mock-provider OCI vertical slice before real TTS/remote LLMs | Accepted direction (refinements applied) | Revises Phase 2 roadmap |

---

## Decision Details

### ADR-022 — In-process Rust AI Agent Runtime & Manifest-Declared Tools
- **Decision:** Scenivox embeds a provider-independent AI agent loop in Rust (`src/agent.rs`) driving `LlmProvider` tool-calling turns against manifest-declared tools and allowlisted entrypoints (`workspace_list`, `workspace_read`, `workspace_write`, `workspace_patch`, `skill_run_step`, `probe_media`, `complete_composition`). Unrestricted shell access (`sh -c` with arbitrary commands) is prohibited.
- **Rationale:** Matches the iterative creative workflow of skill-driven animation without requiring Claude Code, OMP, or Antigravity at runtime.
- **Consequences:** All turns, tool calls, repair iterations, tokens, and wall-clock limits are bounded and logged in `render_job_events`.

### ADR-023 — Untrusted Skill Bundles & Artifact Posture
- **Decision:** Skill bundles (`skill.manifest.json`, `SKILL.md`, `references/`, `scripts/`, `assets/`) are imported locally, checked against traversal/zip bombs/symlinks, and hash-pinned (`bundle_sha256`). Skill instructions (`SKILL.md`), model outputs, generated JS/Python code, and produced media artifacts are all treated as **untrusted**.
- **Rationale:** Neither `SKILL.md` nor LLM output nor generated media may be assumed benign.
- **Consequences:** Bundle updates require a new version/hash; `ffprobe`/`ffmpeg` inspection of generated media runs inside an isolated, bounded media-validation container.

### ADR-024 — Unified Composition & Skill-Specific Rendering
- **Decision:** The agent generates a unified composition for the approved storyboard and P2-02 `Timeline` / `LocalNarration` sidecars, while each skill chooses its own internal rendering strategy inside its sandbox.
- **Rationale:** Cross-scene transitions and continuous audio mix require unified timeline awareness; `RevisionContent.storyboard.scenes` stays the editorial unit.
- **Consequences:** Partial single-scene rerender remains deferred to Phase 2.5 where manifests explicitly declare support.

### ADR-025 — Linux OCI Container Isolation (Including Windows via WSL2) & Isolated MediaQA
- **Decision:** Linux OCI container isolation (`--network=none`, `--read-only`, `--cap-drop=ALL`, `--security-opt=no-new-privileges`, non-root UID/GID, memory/CPU/PIDs/disk/time caps) is the MVP execution baseline; Windows hosts execute via WSL2 Linux OCI containers. Host secrets, Docker/Podman sockets, `--privileged` containers, unrestricted host mounts, and network access from generated-code execution are explicitly prohibited. Media inspection (`ffmpeg` / `ffprobe`) also executes inside an isolated, bounded OCI container.
- **Rationale:** Subprocess CWD/env filtering cannot safely contain arbitrary JS/Python, Chromium, or untrusted media demuxers.
- **Consequences:** Hosts lacking verified OCI/WSL2 isolation fail closed (`503 sandbox_unavailable`).

### ADR-026 — Three Independent Gates, Atomic Cost Accounting & Bound Execution Auth
- **Decision:** Separate:
  1. **Editorial `Approval`:** exact `revision_id` + `content_sha256`.
  2. **`CostConsent`:** atomic pre-call budget reservation (`spent_micros + reserved_micros <= max_cost_micros`) and post-call reconciliation inside SQLite `BEGIN IMMEDIATE`. Unknown provider pricing or missing usage reconciliation fails closed.
  3. **`ExecutionAuth`:** single-job authorization bound to exact `project_id`, `revision_id`, `content_sha256`, `job_id`, `skill_id`, `skill_version`, `bundle_sha256`, `runtime_policy_sha256`, and recorded post-run `workspace_manifest_sha256` evidence.
- **Rationale:** Prevents approval bypasses, concurrent budget races, wildcard execution replay, and unpriced spend.
- **Consequences:** Enqueue/retry verify all applicable gates atomically; removing any gate blocks execution.

### ADR-027 — Provider Abstraction & Indonesian Audio Strategy
- **Decision:** Support deterministic `mock`, local (`remote: false`), and opt-in remote (`remote: true`) `LlmProvider` adapters with zero silent fallback. Support **user-supplied Indonesian WAV (`P2-02`) first**, followed by licensed local TTS/alignment evaluation.
- **Rationale:** Keeps CI offline/deterministic and avoids premature reliance on paid or unverified Indonesian TTS services.

### ADR-028 — Exclusive Worker Startup & Lease Ownership for Job Recovery
- **Decision:** Move active-job interruption recovery (`preparing`..`validating` -> `interrupted`) **out of ordinary `db::connect()`** into exclusive worker startup guarded by a SQLite `worker_leases` lock/ownership record.
- **Rationale:** Once a background worker runs, ordinary CLI reads or API connections calling `db::connect()` must not kill or mark live in-flight jobs `interrupted`.
- **Consequences:** Only a worker acquiring exclusive lease ownership at startup recovers stale active jobs and orphaned OCI containers/workspaces.

### ADR-029 — Phased Vertical Slice & Deferred Cleanup
- **Decision:** Separate asset/cost/execution authorization implementation (`P2-02B`) from real Indonesian speech verification (`P2-02C`), and run an **offline mock-provider OCI vertical slice (`P2-03B`)** using `stick-figure` + user/fixture WAV before integrating remote LLMs or real TTS. `README.md` roadmap and legacy worktree cleanup remain deferred.
