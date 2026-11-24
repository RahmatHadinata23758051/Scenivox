# Scenivox — Architecture Decisions (ADR-02 Rebaseline)

**Status:** Proposed for PM architecture review (no Phase 2 runtime code implemented yet). Baseline repository verified at `53db65bc72b0cc37162a5bbcac3689f126cdcf97`.

This document records the **ADR-02 AI-native skill-driven architecture rebaseline** authorized by the CEO, preserving the Phase 0–1 foundational decisions (Rust/Axum/Tokio/SQLx+SQLite core, immutable revisions, hash-bound editorial approval, provider-neutral selection, offline mock CI, and the Phase 3 Web UI HOLD gate).

---

## ADR Summary (ADR-02)

| ID | Decision | Status | Replaces / Extends |
|---|---|---|---|
| **ADR-022** | In-process Rust AI agent runtime with bounded tools; no Claude Code / OMP / Antigravity runtime dependency | Proposed for PM review | Extends ADR-008, ADR-013 |
| **ADR-023** | Skill bundles (`SKILL.md`, `references/`, `scripts/`, `assets/`, `skill.manifest.json`) imported, hash-pinned, and discovered read-only | Proposed for PM review | Extends ADR-005, ADR-007 |
| **ADR-024** | Unified composition generation and skill-specific internal rendering strategies over forced per-scene rendering | Proposed for PM review | Revises Phase 0–1 per-scene assumption |
| **ADR-025** | Mandatory fail-closed OS/container sandbox for AI-generated JS/Python composition code; no unsandboxed fallback | Proposed for PM review | Strengthens process-safety minimum |
| **ADR-026** | Three separate authorization gates: editorial approval, model-cost consent ledger, and code-execution authorization | Proposed for PM review | Extends ADR-017, ADR-020 |
| **ADR-027** | Unified `LlmProvider` abstraction with deterministic mock, local models, and opt-in remote APIs (no silent fallback) | Proposed for PM review | Extends ADR-008, ADR-012 |
| **ADR-028** | Revised Phase 2 implementation sequence starting with sandbox + skill registry + `stick-figure` unified vertical slice | Proposed for PM review | Revises Phase 2 roadmap |

---

## Decision Details

### ADR-022 — In-process Rust AI Agent Runtime & Bounded Tools
- **Decision:** Scenivox embeds a provider-independent AI agent loop in Rust (`src/agent.rs`) driving `LlmProvider` tool-calling turns against a bounded tool surface (`workspace_list`, `workspace_read`, `workspace_write`, `workspace_patch`, `skill_run_step`, `probe_media`, `complete_composition`).
- **Rationale:** Matches the creative workflow of Claude Code animation skills (reading `SKILL.md` + `references/`, inspecting audio timings, writing composition scripts, running stills/renders, reading console/FFmpeg errors, and repairing iteratively) without making Claude Code, OMP, or Antigravity a runtime dependency.
- **Consequences:** All tool calls, token counts, repair attempts, and wall-clock budgets are bounded and persisted in SQLite (`render_job_events`) for reproducibility and post-mortem debugging.

### ADR-023 — Skill Bundles & Pinned Manifests
- **Decision:** Skills are self-contained bundles (`skill.manifest.json` + `SKILL.md` + optional `references/`, `scripts/`, `assets/`) imported explicitly from local archives or directories, hashed (`bundle_sha256`), and stored in SQLite (`skill_bundles`).
- **Rationale:** `SKILL.md` provides creative/technical guidance to the AI agent as **untrusted text data**, never as executable code. Execution capabilities, writable workspace paths, allowed entrypoints, and pinned runtime dependencies live strictly in `skill.manifest.json`.
- **Consequences:** Updating a skill creates a new version/hash entry; existing approved revisions and jobs stay bound to the pinned `(skill_id, skill_version, bundle_sha256)`.

### ADR-024 — Unified Composition Generation & Skill-Specific Rendering
- **Decision:** Instead of forcing every skill to render independent per-scene video files and concatenate them in Rust, the agent generates a **unified composition** for the approved storyboard revision and P2-02 `Timeline` / `LocalNarration` sidecars, while allowing each skill to use its own internal rendering strategy (e.g., `stick-figure` shot-segment parallel Canvas/Playwright + FFmpeg concat/mux, or single-pass composition).
- **Rationale:** Cross-scene whip transitions, continuous background music/SFX, and natural motion continuity require unified timeline awareness; `RevisionContent.storyboard.scenes` remains the structured human-editable input.
- **Consequences:** Partial single-scene rerender/caching moves strictly to Phase 2.5 where a skill manifest explicitly declares `supports_independent_scene_render: true`.

### ADR-025 — Fail-Closed Execution Sandbox
- **Decision:** AI-generated JS/Python code and third-party skill scripts may execute **only** inside a verified isolation backend (`src/sandbox.rs`) enforcing read-only skill/runtime mounts, an isolated ephemeral job workspace, `--network=none` (no outbound network), zero host secrets, memory/CPU/PID/disk/time limits, and OS-enforced process-tree termination.
- **Rationale:** Ordinary subprocess CWD/env filtering does not contain arbitrary JS/Python or Chromium `--allow-file-access-from-files`.
- **Consequences:** If the host has no verified sandbox backend (`SandboxBackend::is_available() == false`), enqueue and worker execution **must fail closed** (`503 sandbox_unavailable`). There is no unsandboxed fallback.

### ADR-026 — Three Independent Authorization Gates
- **Decision:** Separate and independently verify:
  1. **Editorial approval (`Approval`):** Exact `revision_id` + `content_sha256` human sign-off.
  2. **Model-cost permission (`CostConsent`):** Explicit provider/model scope and monotonic micro-cost/token budget cap for remote/paid LLM or TTS inference.
  3. **Code-execution authorization (`ExecutionAuth`):** Explicit per-job/policy authorization for sandboxed execution of a pinned `(skill_id, skill_version, bundle_sha256)`.
- **Rationale:** Approving a storyboard is not permission to spend money, and approving spend is not permission to execute generated code on the host.
- **Consequences:** Removing any one of the three gates blocks render execution before provider calls or sandbox launch.

### ADR-027 — Provider Abstraction & No Silent Fallback
- **Decision:** Extend P1-05 model roles with a unified `LlmProvider` trait supporting tool calling, structured outputs, token/cost accounting, deterministic offline `mock` providers, local model endpoints (`remote: false`), and opt-in remote APIs (`remote: true`).
- **Rationale:** Creators can choose their preferred model per role/project/run without provider lock-in, while CI stays 100% offline and deterministic.
- **Consequences:** Provider outage, auth error, rate limit, or budget exhaustion fails the job explicitly; Scenivox never silently substitutes another model or `mock`.

### ADR-028 — Phase 2 Vertical Slice First
- **Decision:** Validate the ADR-02 architecture through a minimal end-to-end vertical slice using the audited `stick-figure` skill core (unified composition in sandbox, P2-02 audio/timing sidecar, `ffprobe` + multi-frame pixel-diff motion QA) before onboarding additional skills or Phase 2.5 optimizations.
- **Rationale:** Proves isolation, agent repair, unified rendering, and real Indonesian media output with minimal blast radius.
