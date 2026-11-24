# Scenivox Model Registry, Agent Providers & Cost Controls — ADR-02

**Status:** P1-05 (`mock` local registry, settings, `global < project < run` resolution, and frozen revision snapshots) is **implemented**. Provider adapters (`LlmProvider` / TTS), remote connectivity probes, atomic budget reservation, and the `CostConsent` / `ExecutionAuth` ledgers are **ADR-02 planned Phase 2 work**; no remote provider call or spend occurs in the current codebase (`53db65bc72b0cc37162a5bbcac3689f126cdcf97`).

---

## 1. Ownership & Task Roles

The Rust core owns the provider/model registry, resolution precedence, pricing/usage checks, atomic budget accounting, and immutable snapshot persistence. Claude Code, OMP, and Antigravity are **never** runtime provider dependencies.

| Role | Responsibility | Status |
|---|---|---|
| `storyboard_planning` | Produce structured storyboard JSON for human review | P1-05 implemented (`mock`); Phase 2 adds `LlmProvider` |
| `scene_code_generation` | Drive unified composition code generation & repair inside the bounded agent runtime | P1-05 metadata implemented (`mock`); Phase 2 adds `LlmProvider` tool-calling |
| `voice_synthesis` / `voice_alignment` *(P2-02 sidecar)* | User-supplied Indonesian WAV first (`P2-02`), then licensed local TTS/aligner evaluation | P2-02 WAV/traits implemented; local TTS pending P2-02C |

Selecting a model for one role never selects it for another. No provider outage, rate limit, unknown pricing, missing usage, missing credential, or budget exhaustion ever permits silent fallback to another provider or to `mock`.

---

## 2. Provider Abstraction (`LlmProvider`, Planned Phase 2)

```text
AgentRuntime / Planner ──► LlmProvider trait (Rust host process only)
                             ├── DeterministicMockLlm (remote: false, zero cost, CI/offline)
                             ├── LocalEndpointProvider (remote: false, e.g. Ollama / llama.cpp)
                             └── RemoteApiProvider (remote: true, explicit pricing + CostConsent)
```

- **Capabilities & pricing metadata:** Each model declares `task_roles`, `supports_tool_calling`, `supports_structured_json`, `max_context_tokens`, `remote` (`bool`), `availability` (`unknown` | `available` | `unavailable`), and for paid/remote entries, an explicit **verified pricing schedule** (`input_cost_per_token_micros`, `output_cost_per_token_micros`, or request unit rate) plus whether the provider returns authoritative token/cost usage.
- **Fail-closed on unknown pricing or usage:** If a remote/paid provider lacks a verified pricing schedule, cannot bound worst-case `max_request_micros` from request `max_tokens` + prompt size, or omits required usage metrics needed for reconciliation, Scenivox **must fail closed** (`provider_pricing_unavailable` / `cost_limit_exceeded`) before or after the request rather than allowing unpriced/unbounded spend.
- **Secret isolation:** Credentials are read exclusively inside the Rust host process at HTTP call time and never enter SQLite, snapshots, logs, agent prompts, or OCI sandbox containers.

---

## 3. Selection Precedence & Frozen Snapshots (P1-05 Implemented)

- Precedence remains **`global < project < run`** independently per role, with whole-pair replacement (`provider_id` + `model_id`).
- Saving a revision freezes `models.{role}` (`provider_id`, `model_id`, `adapter_version`, `parameters`, `prompt_sha256`).
- Later edits to global defaults, project overrides, or registry entries **never** mutate existing revisions, approvals, or `render_jobs`.

---

## 4. Three Independent Authorization Gates & Atomic Cost Accounting

| Gate | Record | Binding & Verification |
|---|---|---|
| **1. Editorial Approval** *(implemented)* | `Approval` | Bound to exact `(project_id, revision_id, content_sha256)` |
| **2. Model-Cost Permission** *(planned)* | `CostConsent` | Bound to `provider_id`, `model_id`, `scope`, `max_cost_micros`, `max_tokens`, `spent_micros`, `reserved_micros`, `revoked` |
| **3. Code-Execution Auth** *(planned)* | `ExecutionAuth` | Bound to exact `(project_id, revision_id, content_sha256, job_id, skill_id, skill_version, bundle_sha256, runtime_policy_sha256)` + post-run `workspace_manifest_sha256` evidence |

### Atomic Budget Reservation & Usage Reconciliation (Planned)
Every remote/paid model or TTS call follows a strict two-phase SQLite `BEGIN IMMEDIATE` protocol:
1. **Pre-call atomic reservation:**
   - Verify `CostConsent` is active (not revoked, not expired, matches `provider_id`/`model_id`/`scope`).
   - Compute deterministic worst-case ceiling `reserved_micros` (and `reserved_tokens`) from the request's bounded `max_tokens` + prompt size + verified pricing schedule. Fail closed (`provider_pricing_unavailable`) if pricing is unknown.
   - Check `spent_micros + reserved_micros_total + reserved_micros <= max_cost_micros` (and token cap). If insufficient, fail closed (`cost_limit_exceeded`).
   - Atomically insert a `budget_reservations` row and increment `reserved_micros_total`.
2. **Post-call atomic reconciliation:**
   - Compute `actual_micros` from provider-reported usage + verified pricing schedule. If usage is missing/unverifiable for a priced remote call, conservatively settle at `reserved_micros` and abort further calls (`cost_reconciliation_failed`).
   - Atomically convert the reservation into `spent_micros += actual_micros` and release the reserved amount.
   - If the host crashes mid-flight, startup recovery/reconciliation settles orphaned open reservations conservatively at `reserved_micros` so restarts never refund unverified external spend.

---

## 5. Current P1-04 Fail-Closed Status

Until `CostConsent`, `ExecutionAuth`, `SkillRegistry`, and the Linux OCI/WSL2 `SandboxBackend` are implemented, `enqueue_render_job` and `retry_render_job` continue to fail closed (`invalid_asset_reference`, `remote_cost_not_approved`, `skill_unavailable`) without creating queued work.
