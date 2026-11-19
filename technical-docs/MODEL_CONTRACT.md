# Scenivox Model Registry and Selection Contract

**Status:** P0-03 specification for the Phase 1 backend; not evidence of implemented adapters or verified remote connectivity. Applies equally to Rust domain services, API and CLI. No UI or remote provider integration is authorized by this document.

This document specializes the model rules in [ARCHITECTURE.md](ARCHITECTURE.md) and [API_CONTRACT.md](API_CONTRACT.md). Revision approval remains revision-bound; model selection does not replace approval, cost permission or the [skill execution safety contract](SKILL_SPEC.md).

## 1. Ownership and task roles

The Rust core owns the provider/model registry, selection resolution, validation and immutable snapshot persistence. Adapters translate validated requests to a supported provider interface; they must not choose a substitute model or own approval policy. OMP is a development harness, never a runtime provider dependency. A subscription to a chat application is not evidence of callable API access.

The registry recognizes these independent task roles:

| Role | Responsibility | Required capability |
|---|---|---|
| `storyboard_planning` | Produce structured draft storyboard data for schema validation and human review. | `storyboard_planning` |
| `scene_code_generation` | Produce scene code or constrained scene-generation input for the audited execution boundary. | `scene_code_generation` |

A provider/model may declare either capability or both. Selecting a model for one role does not select it for the other. Capability declarations are adapter-supported contract claims, not inferred from a display name. Unknown roles and unsupported capabilities are rejected. Generated scene code remains untrusted even when the selected model is supported.

TTS and media-rendering skills have separate contracts; these two roles do not imply voice, video generation, or execution capabilities. A deterministic, offline mock adapter is the baseline for both roles. Remote providers are supported only after their endpoints and adapter contracts are verified; no specific commercial provider is assumed available.

## 2. Registry and discovery

A registry entry identifies a `provider_id` and a `model_id` scoped to that provider, `adapter_version`, declared `task_roles` (capabilities), `enabled`, `configured`, `remote`, and `availability` with a non-secret reason. IDs are stable machine identifiers, not labels or arbitrary endpoint URLs. Internal observations retain verification time/evidence; the v1 exported Model record exposes the status and safe reason only.

These facts are distinct:

- **Capabilities:** which task contracts the adapter supports for this model.
- **Enabled:** whether local policy permits selecting the entry.
- **Configured:** whether required local configuration is present and structurally valid. This does not prove credentials are accepted, account entitlement exists or the endpoint is reachable.
- **Availability:** `unknown`, `available`, or `unavailable`. `unknown` is not `available`. A positive remote status must have explicit verification evidence and observation time; it is not a guarantee of future success. Failures or stale evidence must not be hidden as a positive status. A deterministic local mock can be reported available from local adapter readiness without a network test.

Registry listing and settings reads are local, read-only operations. They must not perform inference, implicitly probe endpoints, spend credits, download models, discover remote catalogs or expose secrets. Saving a selection likewise does not authorize a probe or inference. Missing configuration and unknown availability remain visible rather than being concealed by defaults.

A connectivity test is a separate, explicit opt-in operation. If it may incur cost, explicit cost permission and a budget are also required before it starts. A probe reports what it actually verified; connectivity alone does not prove every role or model works. No implicit paid probes are permitted.

Unknown remote availability must never be presented as verified readiness. It can be resolved through an explicit permitted test, or an explicitly authorized execution attempt can provide evidence of its actual outcome. Neither path allows a hidden preflight request. Known unavailability, disabled entries, missing required configuration and unsupported capabilities fail explicitly before dispatch where known; runtime provider failures remain explicit failures.

## 3. Selection shape and precedence

A selection is a map from task role to a complete provider/model pair:

```json
{
  "storyboard_planning": {"provider_id": "mock", "model_id": "planner-v1"},
  "scene_code_generation": {"provider_id": "mock", "model_id": "coder-v1"}
}
```

Global defaults, project overrides and explicit run overrides use this same pair shape. Override maps may omit roles to inherit them. Each included role must contain both nonempty IDs; partial pairs, unknown roles and invalid identifiers are rejected rather than merged or guessed.

Resolution is **global < project < run**, independently for each role:

1. Start with that role's global default pair.
2. If a project override contains that role, replace the entire pair.
3. If an explicitly supplied run override contains that role, replace the entire pair again.
4. Validate the resulting pair against the registry and required role capability. A required role without a resolved selection fails explicitly.

Never take a provider from one layer and a model from another. An invalid higher-precedence selection is an error, not permission to fall back to a lower layer. An absent override means inheritance; it does not mean automatic provider choice. Run overrides are scoped to that operation and do not update project settings or global defaults.

For example, a project override for planning and a run override for scene code result in the project planning pair and the run scene-code pair. Neither changes the other role. This resolution governs creating new work; it must not re-resolve already approved execution snapshots (Section 5).

No provider outage, rate limit, missing credential, unsupported parameter or unknown status permits automatic fallback, alternate model routing or substitution with the mock. The user must explicitly choose a supported alternative for new work under the revision rules below.

## 4. Effective snapshot and inference parameters

A selection records intent; an effective snapshot records the resolved execution contract. The canonical snapshot is a map with the same task-role keys, where each value contains exactly the following contract fields:

```text
role -> {
  provider_id,
  model_id,
  adapter_version,
  parameters,
  prompt_sha256
}
```

| Field | Contract |
|---|---|
| `provider_id`, `model_id` | Validated resolved pair; not a mutable reference to current settings. |
| `adapter_version` | Exact version of the adapter implementation/request contract used or pinned for the role; no floating `latest`. |
| `parameters` | Non-secret effective inference settings: required finite numeric `temperature` in the inclusive range `0..2`; optional integer `seed` in `0..2147483647`. |
| `prompt_sha256` | Lowercase 64-character hexadecimal SHA-256 of the exact versioned prompt payload for that role, including ordered messages/instructions and rendered inputs, serialized deterministically as UTF-8. |

For example, a snapshot value may contain `"parameters": {"temperature": 0, "seed": 42}`. An omitted seed requests no explicit seed; it is not equivalent to zero. Strings, booleans, fractional seeds, non-finite temperatures and temperatures outside `0..2` are invalid. Parameter defaults must be resolved before snapshot creation; missing temperature must not become a hidden provider default. These settings are not field-merged into the provider/model selection map.

Adapters must validate model-specific parameter support before dispatch. They must not silently drop or clamp a requested temperature or seed; unsupported settings fail explicitly. Secrets, tokens, credential references that reveal secrets and authorization headers never belong in selection maps, snapshots, prompt records, error messages or manifests. Prompt inputs must not include credentials.

Prompt hashes are computed, not accepted as proof from a caller. The exact prompt payload must be materialized and frozen before its snapshot is finalized, including scene-code prompts whose execution happens later. If execution would require changing that payload, selection, adapter version or parameters, it requires new work under Section 5 rather than mutating the snapshot. A hash is provenance and integrity evidence, not a substitute for retaining the permitted prompt/input artifacts needed for replay.

Prompt construction excludes snapshot/hash/approval/job envelope fields to avoid a circular hash dependency. Planning provenance hashes the actual planning request that produced the draft; scene-generation prompts are frozen from the saved storyboard/target/approved asset inputs before content hashing. A later material change produces new prompt artifacts and a new revision; do not compute prompt hashes from an envelope containing those same hashes.

## 5. Revision, approval and job immutability

- When a draft revision is saved, persist its effective role snapshots with its immutable content/provenance. Record planning choices used to produce the draft and the pinned scene-code execution choice and prompt payload for subsequent generation. Human edits create a new draft revision and new applicable snapshots; they do not rewrite history.
- Approval binds an exact revision ID, content hash and effective model snapshots. A draft is not renderable merely because a model was selected or a planning call succeeded.
- Job creation binds the approved revision and copies its effective snapshots into the durable job record, alongside the job's other approved inputs. It does not resolve fresh defaults or project settings. Check that the requested approved revision and hash match before queueing.
- Later changes to global defaults, project settings, registry metadata or adapter installations never mutate an existing revision or job snapshot. If a pinned adapter/model can no longer execute, fail explicitly; do not relabel or upgrade the snapshot in place.
- A render request cannot override the approved execution model, parameters, adapter version or prompt. To change an approved scene-code execution choice, create a new draft revision with the new effective snapshot and obtain explicit approval of that revision before creating a job. A run override is valid only when forming new work; it cannot bypass this rule at render time.
- A retry retains the original job's approved inputs and model snapshots. A requested change is new work requiring the corresponding new revision/approval, not a retry that silently uses current settings.

The historical planning snapshot describes how a draft was produced; changing a future planning default does not rewrite that provenance. Regenerating with a different planning model produces a new draft, not a mutation of an approved result.

## 6. Approval, access and cost are separate gates

Revision approval authorizes execution of that revision's creative and technical inputs. It is **not** permission to spend money. Selecting a remote provider, configuring credentials, reporting availability, or approving a storyboard does not grant paid API access.

Before any remote operation that may incur cost, require explicit user permission/budget covering that operation, provider/model and permitted usage. This includes connectivity tests, planning, scene-code generation and retries. If consent is absent, insufficient or no longer applicable, fail before the cost-incurring request. Record safe consent/budget evidence separately from the model snapshot and enforce it at dispatch; do not put secrets in that evidence. No fallback, retry or probe may expand that authorization implicitly.

## P1-04 render enqueue gate status

Editorial approval remains separate from paid-operation permission. The current backend has no verifiable cost-consent/budget evidence ledger and no audited skill capability registry. P1-04 accepts the contract-shaped job request but fails closed before insertion: unknown asset references return `invalid_asset_reference`, absent audit capability returns `skill_unavailable`, and an opaque supplied consent ID returns `remote_cost_not_approved`. A consent ID alone is never authorization. No renderer, model provider, network operation, or paid operation is invoked.

Conversely, cost consent does not approve a draft revision, authorize arbitrary generated-code execution or prove provider capability. Remote access remains user-controlled; offline mock workflows must not require remote credentials or downloads.

## 7. Failure semantics and acceptance cases

API and CLI share the same domain decisions. Use the existing API error contract: invalid input or unsupported capability is explicit validation failure (for example `422 unsupported_capability`); selected-provider execution failures are explicit (for example `503 provider_unavailable`); missing approval or stale revision fails with the applicable `409`; missing cost permission is `409 remote_cost_not_approved`. Diagnostic reasons must not leak credentials. Unknown availability is reported as unknown, not fabricated success or an automatic alternate-model choice.

Implementation acceptance must cover these cases without live paid calls:

1. List configured, unconfigured, disabled and unknown entries offline without network activity or secrets.
2. Resolve each role independently with global < project < run whole-pair replacement; reject partial pairs and invalid higher-precedence choices without fallback.
3. Select different supported mock models for planning and scene code; reject unsupported capabilities, invalid temperature and non-integer seed, and reject adapter-unsupported parameters without dropping them.
4. Persist the complete role snapshot map and verify prompt hashes against frozen payloads; prove changes to defaults cannot alter revision/job snapshots.
5. Reject a render-time model override; require a new revision and approval for a changed approved execution choice. Verify unchanged retries retain snapshots.
6. Reject an unapproved render even with cost consent, and reject a paid operation without cost consent even with revision approval. Listing never triggers a test or inference.
7. Surface unavailable or failed providers explicitly without switching providers/models or silently using the mock.

These are required future checks, not reported test results for this documentation task.

## 8. Reproducibility limits

The offline mock must produce deterministic fixture results for identical inputs. A remote model is not promised bit-for-bit reproducible: the provider may change weights, serving infrastructure, safety policies, routing or model aliases behind the same ID; parallel execution and numerical nondeterminism may change outputs. `temperature = 0` and a fixed seed do not eliminate these limitations. A seed is useful only where the adapter/provider actually supports it.

Snapshots, exact prompt/input artifacts, hashes, adapter versions and returned outputs establish traceability of what was requested and observed. Preserve a concrete provider model revision or response identifier as additional safe provenance when supplied; do not misrepresent an alias as a pinned weight version. Stored generated artifacts allow reuse of the original result without a new remote request. Replaying a remote request is new execution, requires applicable cost consent, and may yield different output; it must not overwrite the original revision, job evidence or artifacts.
