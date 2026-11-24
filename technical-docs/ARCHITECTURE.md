# Scenivox — Backend & Media Architecture (Baseline + ADR-02)

**Status note:** Phase 0–1 foundations (`P1-01` persistence, `P1-02` deterministic offline planner, `P1-03` review/approval, `P1-04` fail-closed render jobs, `P1-05` model registry/selection) and `P2-02` WAV/timing foundations are **implemented** at `53db65bc72b0cc37162a5bbcac3689f126cdcf97`. The AI agent runtime, skill bundle registry, Linux OCI sandbox runner (including Windows via WSL2), atomic cost-consent ledger, bound execution authorization, exclusive lease-fenced worker loop, and isolated MediaQA pipeline below are **ADR-02 target architecture** (Phase 2).

---

## 1. System Boundaries & Data Flow

```text
Future website (HOLD until CEO authorizes Phase 3)
      │ REST / job polling / safe artifact links
      ▼
┌─────────────────────────────── Rust Scenivox Core ────────────────────────────────┐
│ Axum API + Clap CLI (shared Store & domain services)                              │
│                                                                                   │
│  [Implemented at 53db65b]               [Planned Phase 2 — ADR-02]                │
│  • Project / Revision / Scene CRUD      • SkillRegistry (host-owned audit_status) │
│  • Review & hash-bound Approval         • CostConsent (atomic reserve/reconcile)  │
│  • ModelRegistry & Role Resolution      • ExecutionAuth (job + policy + evidence) │
│  • RenderJob durability & P2-02 WAV     • LlmProvider (mock / local / remote API) │
│                                         • AgentRuntime + Lease-Fenced JobWorker   │
└────────┬─────────────────────┬──────────────────────────┬─────────────────────────┘
         │                     │                          │
    SQLite (SQLx)        Host Secrets / APIs       Linux OCI Sandbox (host / WSL2)
  metadata, ledgers,     LlmProvider & TTS         (--network=none, --pull=never,
  leases, traces         (host-only, CostConsent)   --read-only, --cap-drop=ALL)
                                                          │
                         ┌────────────────────────────────┴────────────────────────┐
                         │ 1. Composition Container: SKILL.md (RO) + unified code  │
                         │    written by AgentRuntime + manifest entrypoints only  │
                         │ 2. Isolated MediaQA Container: bounded ffmpeg / ffprobe │
                         │    inspecting untrusted media outputs (RO artifact)     │
                         └────────────────────────────────┬────────────────────────┘
                                                          ▼
                         Verified final.mp4 + stills + workspace_manifest_sha256
```

**Crucial invariant:** Scenivox runs its own in-process Rust agent loop against `LlmProvider`. Claude Code, OMP, and Antigravity are development tools only and are **never** required at runtime.

---

## 2. Module Impact Assessment (from `53db65bc72b0cc37162a5bbcac3689f126cdcf97`)

| Module | Current Responsibility | ADR-02 Disposition | Planned Change |
|---|---|---|---|
| `src/json.rs`, `src/config.rs` | Strict 1 MiB duplicate-key JSON parser; env config | **Unchanged** | Preserve strict parsing and loopback bind defaults |
| `migrations/0001`–`0005` | Core metadata, projects/revisions, models, review/approval, `render_jobs` | **Unchanged** | Freeze existing migrations; add `0006+` migrations only |
| `src/domain.rs` | Strict v1 commands and immutable snapshot records | **Extend (additive)** | Add skill manifest, cost consent, execution auth, asset, artifact, worker lease, and agent trace types |
| `src/store.rs` | Transactional persistence & fail-closed job enqueue/retry | **Extend** | Replace hardcoded fail-closed stubs with real checks for assets, pinned skill bundle, host-owned `audit_status`, atomic cost consent (for remote/paid), job-bound execution auth, and OCI/WSL2 sandbox availability |
| `src/planner.rs` | `Planner` trait & `DeterministicMockPlanner` | **Keep + Extend** | Retain deterministic mock for CI; add `LlmProvider`-backed planner behind `CostConsent` (when remote) |
| `src/voice.rs` | `NarrationProvider`/`AlignmentProvider` traits, WAV inspect, `Timeline` | **Extend** | User-supplied Indonesian WAV first (`P2-02`), then licensed local TTS/aligner evaluation (`P2-02C`) + `words.json`/`timeline.json` sidecars |
| `src/db.rs` | SQLite connect, migrations, and current P1-04 `startup_interrupted` | **Extend (Phase 2)** | Move active-job interruption recovery **out of ordinary `db::connect()`** into exclusive `worker.rs` startup/lease fencing (`ADR-028`) |
| `src/api.rs`, `src/cli.rs` | Axum HTTP routes and Clap CLI commands | **Extend** | Add `/v1/skills`, `/v1/assets`, `/v1/cost-consents`, `/v1/execution-authorizations`, job trace/artifacts, and `/ready` |
| `src/skill.rs` *(new)* | — | **New** | Traversal/zip-bomb-safe skill bundle import, SHA-256 pinning, `skill.manifest.json` validation, host-owned `audit_status`, read-only capability discovery |
| `src/provider.rs` *(new)* | — | **New** | Unified `LlmProvider` trait (tool calls, structured output, pricing/usage metadata) + `DeterministicMockLlm` + local/remote adapters |
| `src/agent.rs` *(new)* | — | **New** | Bounded agent loop (`max_turns`, `max_tool_calls`, `max_repairs`, atomic budget checks, safe workspace tools & manifest entrypoints only) |
| `src/sandbox.rs` *(new)* | — | **New** | Linux OCI container runner (native Linux or Windows via WSL2) with `--pull=never` for both skill execution and isolated `ffmpeg`/`ffprobe` MediaQA |
| `src/worker.rs` *(new)* | — | **New** | Exclusive-lease durable worker (`worker_leases` fencing), startup recovery + orphan container/workspace cleanup, job stage execution |

---

## 3. Implemented vs. Planned Boundaries

### Implemented at `53db65bc72b0cc37162a5bbcac3689f126cdcf97`
- **P1-01 / P1-02 / P1-03 / P1-05:** Local SQLite project CRUD, immutable copy-on-write revisions with JCS SHA-256, deterministic offline storyboard planning with frozen `PlanningProvenance`, model registry and `global < project < run` selection, and hash-bound review/approval.
- **P1-04:** Durable `render_jobs` table/triggers, startup interruption recovery, cancel/get commands, and fail-closed enqueue/retry.
- **P2-02:** `scenivox voice inspect --file <relative-path>` validates user-supplied integer PCM WAV (mono/stereo, 8–192 kHz, 8/16/24/32-bit, 1–60 s, 64 MiB max) with strict normal-relative-path confinement (`""`, `.`/`./`, `..`, absolute paths, and symlink escapes rejected; opened-handle regular-file metadata check is local defense-in-depth and does not eliminate kernel-level TOCTOU/symlink races). `Timeline` remains an internal sidecar preserving `AudioProvenance` and `CaptionProvenance`; `SyntheticTestFixture` never qualifies as verified narration or measured speech alignment.

### Planned Phase 2 Execution Pipeline (ADR-02)

```mermaid
flowchart TD
    WORKER["Exclusive Worker Startup (acquire worker_leases + lease_epoch)"] --> RECOVER["Recover orphaned active jobs + settle open reservations + clean stale OCI containers"]
    RECOVER --> POLL["Claim queued RenderJob (fenced by lease_epoch)"]
    POLL --> GATES["Verify Gates: Approval + ExecutionAuth (+ CostConsent if remote: true)"]
    GATES --> SBX{"Linux OCI / WSL2 sandbox + pinned image available?"}
    SBX -->|no| FAIL_SBX["Fail closed: sandbox_unavailable"]
    SBX -->|yes| VOICE["Voicing: user-supplied WAV / permitted TTS -> LocalNarration + Timeline + words.json"]
    VOICE --> AGENT["AgentRuntime: read SKILL.md (untrusted text) -> write unified code -> skill_run_step (OCI --network=none --pull=never)"]
    AGENT -->|"repair <= max_repairs"| AGENT
    AGENT -->|budget/pricing/turn failure| FAIL_JOB["Conservative reconciliation + fail job with trace"]
    AGENT -->|candidate mp4 + workspace_manifest_sha256| MQA["Isolated MediaQA OCI Container: ffprobe + frame pixel-diff motion"]
    MQA -->|fail| FAIL_JOB
    MQA -->|pass| SUCCEED["Persist Artifact manifests + ExecutionAuth evidence -> mark succeeded"]
```

---

## 4. Untrusted Boundaries & OCI Sandbox Prohibitions

1. **All four inputs/outputs are untrusted:** Skill bundles (`SKILL.md`, `references/`, `scripts/`, `skill.manifest.json`), model outputs, AI-generated JS/Python code, and produced media artifacts (`final.mp4`, stills, audio mixes).
2. **Host-owned audit evidence:** Skill `audit_status` is recorded by the Scenivox host, never accepted from imported manifest declarations.
3. **Mandatory Linux OCI container baseline:** MVP isolation is **Linux OCI containers** (native Linux; Windows via **WSL2**) using pinned local digests (`--pull=never`, no automatic OCI image downloads). Hosts without verified OCI isolation fail closed (`503 sandbox_unavailable`).
4. **Explicitly prohibited inside any composition or MediaQA container:** Host secrets/API keys/`.env`/`scenivox.db`; Docker or Podman socket mounts; `--privileged` or added capabilities (must use `--cap-drop=ALL`, `--security-opt=no-new-privileges`); unrestricted host mounts; and network access (`--network=none`).
5. **Zero-cost offline vs. remote cost:** `remote: false` offline runs require `Approval`, `ExecutionAuth`, and OCI sandbox isolation, without paid `CostConsent`.
6. **UI boundary:** Web frontend scaffolding, styling, auth, and dashboards remain strictly **on HOLD (Phase 3)** until explicit CEO authorization.
