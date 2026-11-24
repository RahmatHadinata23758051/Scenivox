# Scenivox Model Registry, Agent Providers & Cost Controls — ADR-02

**Status:** P1-05 (`mock` local registry, settings, `global < project < run` resolution, and frozen revision snapshots) is **implemented**. Provider adapters (`LlmProvider` / TTS), remote connectivity probes, and the `CostConsent` / `ExecutionAuth` ledgers are **ADR-02 planned Phase 2 work**; no remote provider call or spend occurs in the current codebase.

---

## 1. Ownership & Task Roles

The Rust core owns the provider/model registry, resolution precedence, capability checks, cost-consent accounting, and immutable snapshot persistence. Claude Code, OMP, and Antigravity are **never** runtime provider dependencies.

| Role | Responsibility | Status |
|---|---|---|
| `storyboard_planning` | Produce structured storyboard JSON for human review | P1-05 implemented (`mock`); Phase 2 adds `LlmProvider` |
| `scene_code_generation` | Drive unified composition code generation & repair inside the bounded agent runtime | P1-05 metadata implemented (`mock`); Phase 2 adds `LlmProvider` tool-calling |
| `voice_synthesis` / `voice_alignment` *(P2-02 sidecar)* | Local/remote Indonesian narration & measured word alignment | P2-02 traits implemented; providers pending |

Selecting a model for one role never selects it for another. No provider outage, rate limit, missing credential, or budget limit ever permits automatic fallback to another provider or to `mock`.

---

## 2. Provider Abstraction (`LlmProvider`, Planned Phase 2)

```text
AgentRuntime / Planner ──► LlmProvider trait (Rust host process only)
                             ├── DeterministicMockLlm (remote: false, CI/offline)
                             ├── LocalEndpointProvider (remote: false, e.g. Ollama / llama.cpp)
                             └── RemoteApiProvider (remote: true, explicit API + CostConsent)
```

- **Capabilities declared per model:** `task_roles`, `supports_tool_calling`, `supports_structured_json`, `max_context_tokens`, `remote` (`bool`), `availability` (`unknown` | `available` | `unavailable`).
- **Local vs. Remote:**
  - `remote: false` models (deterministic mock or verified local inference server) incur zero external API spend.
  - `remote: true` models require explicit `configured` credentials (read exclusively from host process environment/secret store at call time—never written to SQLite, snapshots, logs, or sandbox workspaces) and an active `CostConsent` covering that provider/model and operation.
- **Listing & probing:** `GET /v1/models` and settings reads are read-only and never perform network probes. `POST /v1/providers/{id}:test` is opt-in and requires `CostConsent` if `remote: true`.

---

## 3. Selection Precedence & Frozen Snapshots (P1-05 Implemented)

- Precedence remains **`global < project < run`** independently per role, with whole-pair replacement (`provider_id` + `model_id`). Partial pairs, unknown roles, and invalid higher-precedence pairs fail explicitly without fallback.
- Saving a revision freezes `models.{role}` (`provider_id`, `model_id`, `adapter_version`, `parameters`, `prompt_sha256`).
- Later edits to global defaults, project overrides, or registry entries **never** mutate existing revisions, approvals, or `render_jobs`.

---

## 4. Three Independent Authorization Gates (ADR-02)

| Gate | Record | What It Authorizes | What It Does NOT Authorize |
|---|---|---|---|
| **1. Editorial Approval** *(implemented)* | `Approval` (`revision_id`, `content_sha256`) | Human creative sign-off on exact immutable `RevisionContent` | Does not authorize API spend or code execution |
| **2. Model-Cost Permission** *(planned)* | `CostConsent` (`id`, `scope`, `provider_id`, `model_id`, `max_cost_micros`, `max_tokens`, `spent_micros`, `revoked`) | Remote/paid LLM or TTS API calls up to monotonic budget cap | Does not approve a storyboard or authorize sandboxed code execution |
| **3. Code-Execution Auth** *(planned)* | `ExecutionAuth` (`id`, `project_id`, `revision_id`, `skill_id`, `skill_version`, `bundle_sha256`, `sandbox_policy_id`) | Running the AI agent's generated JS/Python inside the verified sandbox for the pinned skill | Does not approve a storyboard or grant paid API budget |

### Enforcement Invariants
- **Monotonic budget accounting:** Before every remote `LlmProvider` turn or paid TTS request, the Rust host verifies remaining `max_cost_micros` / `max_tokens` inside a SQLite transaction and records actual/estimated spend atomically. If budget is exhausted mid-job, the agent stops and fails the job (`cost_limit_exceeded`) without silent continuation.
- **No secret leakage:** Credentials never appear in `ModelSelection`, `ModelSnapshot`, `RevisionContent`, `RenderJob`, `agent_turns`, error envelopes, or sandbox environments.
- **Current P1-04 fail-closed behavior:** Until `CostConsent`, `ExecutionAuth`, `SkillRegistry`, and `SandboxBackend` are implemented, `enqueue_render_job` and `retry_render_job` continue to reject requests (`invalid_asset_reference`, `remote_cost_not_approved`, `skill_unavailable`) without creating queued work.
