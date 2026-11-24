# Scenivox API contract v1 — Baseline + ADR-02 Extensions

Status: At verified baseline `53db65bc72b0cc37162a5bbcac3689f126cdcf97`, P1-01 implements project CRUD and immutable revision persistence; P1-05 implements local model registry and selection resolution; P1-02 implements offline storyboard planning; P1-03 implements review and editorial approval; P1-04 adds durable RenderJob records and fail-closed job request interfaces; P2-02 adds CLI-only `voice inspect --file PATH`. Rendering, worker execution, asset registry, skill bundle registry, cost-consent ledger, execution authorization, and media artifacts remain unimplemented (planned Phase 2 under ADR-02). Approval does not authorize rendering, paid access, or code execution. Rust CLI and Axum call the same persistence service.

## 1. Implemented HTTP operations & normative v1 contracts

JSON requests/responses use `application/json`; unknown fields/versions are rejected. POST creation returns `201` with `Location`, GET/PATCH/PUT return `200`. IDs in paths must be canonical UUIDs. Lists return `{schema_version:"1", items:[...], next_cursor:null|string}` with opaque stable ID cursor, limit `1..100` (default `20`), ascending creation time then ID; invalid cursor returns `422 invalid_cursor`. Timestamps/IDs are server-owned; clients submit commands, not raw persisted records.

| Method / path | Command or response contract |
|---|---|
| GET `/health` | Legacy P0-02 diagnostic; semantics below. |
| GET `/ready` | Future readiness; not yet implemented (ADR-019). |
| POST `/v1/projects` | `{schema_version,title,idea,target,model_selection}` -> Project; both revision pointers initially null. |
| GET `/v1/projects` | Project list envelope. |
| GET `/v1/projects/{id}` | Project. |
| PATCH `/v1/projects/{id}` | `{schema_version,title}`; title-only metadata update. Idea/target/storyboard changes use revision commands, atomically updating project preferences/current pointer and clearing approval. |
| DELETE `/v1/projects/{id}` | `204`; soft-deletes the project. Subsequent project/history reads return `404`; immutable rows remain retained locally, without a restore operation. |
| GET `/v1/models` | `{schema_version:"1",items:[Model...],next_cursor:null}`; local read only, no probing/downloads/inference. |
| PUT `/v1/models/{provider}/{model}` | `{schema_version,provider_id,model_id,adapter_version,task_roles,parameters,enabled,configured,availability,reason,remote}`; explicitly registers local metadata; remote entries cannot claim `available` absent verification. |
| GET / PUT `/v1/settings/models` | GET returns `{schema_version,model_selection}`; PUT accepts `{schema_version,model_selection}` and replaces the complete two-role global map. |
| PATCH `/v1/projects/{id}/model-settings` | `{schema_version,model_selection}` replaces the project override map; empty map clears overrides. Omitted roles inherit; no null or partial provider/model pairs. Returns Project. PUT remains a compatibility alias. |
| POST `/v1/model-selections:resolve` | `{schema_version,project_id?,model_selection}`; resolves complete role pairs by `run > project > global` precedence; read-only, no provider calls. |
| POST `/v1/providers/{id}:test` | `{schema_version,model_id,cost_consent_id}`; explicit opt-in probe only (planned), with consent if remote/potentially paid. Returns Model observation; missing/unsupported provider is an explicit error. |
| POST `/v1/projects/{id}/storyboard:generate` | `{schema_version,expected_revision_id,idea,target,model_selection}`; nullable expected pointer for first draft, optional sparse run selection defaults empty. Runs the deterministic offline planner only; no provider or network call. Persists the generated draft as an immutable Revision with versioned planning and scene prompt inputs, their SHA-256 hashes, and model snapshots; stale pointers or invalid output create no revision. |
| GET `/v1/projects/{id}/revisions` | Revision list envelope. |
| POST `/v1/projects/{id}/revisions` | P1-01 manual save: `{schema_version,expected_revision_id,content}` -> Revision (`201`). Null expected pointer only for first revision. `content` is RevisionContent with scene IDs omitted/null for new scenes; supplied IDs must exist in the immediate parent. |
| GET `/v1/projects/{id}/revisions/{rid}` | Immutable Revision; wrong-project ID is `404`. |
| PATCH `/v1/projects/{id}/revisions/{rid}` | P1-01 manual replacement: same command as revision POST; `rid` and expected pointer must equal current. Creates a new Revision (`201`), never updates the old record. Explicit model snapshots and skill metadata are persisted input, not evidence of provider execution. |
| POST `/v1/projects/{id}/revisions/{rid}/review` | `{schema_version,content_sha256}`; `draft -> in_review`, returns RevisionReview; repeat via new key returns `200`. |
| POST `/v1/projects/{id}/revisions/{rid}/withdraw-review` | Same hash command; `in_review -> draft`, repeated draft returns `200`. Approved/historical revision returns `409 invalid_transition`. |
| POST `/v1/projects/{id}/revisions/{rid}/approve` | `{schema_version,content_sha256}` -> Approval; `201` new evidence, `200` existing evidence via a new key. Same-key replay preserves its original status and body. Requires `current`/`in_review` and exact server hash. |
| GET `/v1/projects/{id}/approvals/{aid}` | Immutable approval evidence; wrong-project ID is `404`. |
| POST `/v1/projects/{id}/render-jobs` | V1 implemented command: `{schema_version,revision_id,content_sha256,cost_consent_id}` -> RenderJob (`201`) only when exact current approval, validated assets, audited pinned skill and verifiable applicable authorization are present. P1-04 fails closed with `skill_unavailable`/`remote_cost_not_approved` because those registries/evidence stores are not implemented; no job is queued. *(ADR-02 Phase 2 extends this gate with `execution_auth_id`; see Section 2.)* |
| GET `/v1/render-jobs/{id}` | RenderJob. |
| POST `/v1/render-jobs/{id}/retry` | `{schema_version,cost_consent_id}` *(ADR-02 Phase 2 adds `execution_auth_id`)*; terminal jobs require full authorization revalidation. P1-04 fails closed with `skill_unavailable` when prerequisites are absent; no executor is invoked. `/retry` is used because Axum rejects a colon suffix in the same path segment as a parameter. |
| POST `/v1/render-jobs/{id}/cancel` | `{schema_version}` is validated; queued jobs become cancelled transactionally. `/cancel` is used because Axum rejects a colon suffix in the same path segment as a parameter. |
| GET `/v1/render-jobs/{id}/assets` | Not implemented in P1-04; artifacts are never produced by this task. |

Commands use strict objects, required fields as shown except explicitly optional fields. API version `/v1` and exported schema version `"1"` evolve together for breaking changes. Do not parse unknown record versions as v1. These command shapes are normative prose; the JSON Schema bundle covers records/value definitions, **not a complete OpenAPI/request schema**. New endpoints require request contract tests before dispatch.

P2-02 adds no HTTP upload or voice route. The CLI-only `voice inspect --file PATH` is read-only, works without SQLite, and accepts only a non-empty normal relative path (`./` and `..` components are rejected) whose canonical target is a regular file under the current working directory. It validates regular-file metadata and the 64 MiB limit on the opened handle, maps failures to `unsafe_audio_path`, `input_unavailable`, `audio_too_large`, `unsupported_audio_format`, `audio_limits_exceeded`, or `invalid_audio`, and emits versioned `LocalNarration` metadata (`schemas/v1/contracts.schema.json`); it does not persist the WAV, synthesize speech, align captions, access providers, or dispatch a render.

For state-changing POSTs except explicit provider probes, require `Idempotency-Key` (canonical UUID). Persist key scoped to operation/project with canonical request hash and response in the same transaction. Same key+body replays the original status, Location and record; different body -> `409 idempotency_conflict`. Repeated transition calls with a new key return the current record/status according to that operation's contract. Retain keys without expiry; replay after project deletion returns `404`, never recreates it. CLI uses the same transactional service without HTTP replay keys, emits stable error codes and nonzero exit on failure.

## Error semantics

Retain the existing application envelope (not a second Problem Details convention):
`{ "schema_version":"1", "error": {"code":"invalid_storyboard", "message":"Scene timing is invalid", "details":[{"path":"/storyboard/scenes/0/duration_ms", "code":"duration_mismatch"}]}}`.
Details use JSON Pointers and machine codes, never echo narration, tokens, raw provider responses, SQL, personal paths or stack traces. Internal structured logs retain safe operation IDs. The HTTP status is authoritative transport metadata, not a field clients must parse from message text.

| HTTP | Code / rule |
|---|---|
| 400 | `malformed_json` (including duplicate keys), `missing_idempotency_key` |
| 404 | `not_found`; unknown resource or wrong-project reference |
| 409 | `stale_revision`, `approval_required`, `hash_mismatch`, `invalid_transition`, `idempotency_conflict`, `remote_cost_not_approved` *(ADR-02 planned: `execution_not_authorized`, `cost_limit_exceeded`, `cost_reconciliation_failed`)* |
| 413 / 415 | `payload_too_large` (>1 MiB) / `unsupported_media_type` |
| 422 | `invalid_schema`, `unsupported_schema_version`, `invalid_storyboard`, `invalid_model_selection`, `unsupported_capability`, `unsupported_parameter`, `invalid_cursor`, `invalid_asset_reference` *(ADR-02 planned: `invalid_skill_manifest`)* |
| 503 | `provider_unavailable` (disabled, missing config or known unavailable), `database_unavailable`, `skill_unavailable`; no substitute/fallback *(ADR-02 planned: `provider_pricing_unavailable`, `sandbox_unavailable`)* |
| 500 | `internal_error`; generic response, safe server diagnostics |

Validation order: syntax/size/media -> version/shape -> resource ownership -> stale/hash/approval checks -> domain/model/skill/asset checks -> cost permission -> execution authorization/sandbox check -> dispatch. Resource reads do not spend credits. Successful planning never approves. Unapproved/stale render starts no executor and makes no paid call. Unknown provider availability is represented honestly; an explicit permitted execution may establish it, but never trigger a hidden probe.

## `/health` versus readiness (ADR-019)

P0-02 `/health` is retained unchanged: HTTP `200` and `{"status":"ok"}` when `SELECT 1` succeeds, otherwise HTTP `200` and `{"status":"unavailable"}`. This is a legacy diagnostic, **not readiness and not a pure liveness guarantee**; callers must inspect the body. Startup applies migrations before listening; connection failure prevents bind.

Target `/ready` is separate: `200` `{"status":"ready"}` only when startup migrations completed and a bounded local DB query succeeds; otherwise `503` `{"status":"not_ready"}`. No model probe, credit spend, rendering or external dependency query. A future pure liveness endpoint may be designed only if operational need arises. P0-03 does not add/change handlers; readiness implementation requires a later assigned task and outage tests. Never configure an orchestrator to infer readiness from `/health`'s HTTP code.

Default deployment is local loopback, no auth or public exposure implied. P1-03 implements `revision review`, `revision withdraw-review`, and `revision approve` in addition to P1-01 project/revision commands; see [SETUP.md](SETUP.md). Models/plan and fail-closed render-job gates are implemented; rendering/jobs/skills/assets remain future execution interfaces.

P1-01 does not resolve assets: unique lowercase SHA-256 `asset_hashes` are sorted inert metadata; nonempty SFX references return `422 invalid_asset_reference` until an asset registry can validate ownership. No input snapshot triggers a provider, skill or network call.

---

## 2. Planned Phase 2 Extensions (ADR-02, Not Yet Implemented)

When ADR-02 runtime modules are implemented, additive `/v1` routes and request fields preserve existing v1 contracts above (breaking changes, if ever needed, must introduce a coordinated schema/endpoint version rather than silently changing v1 behavior):

| Planned Method / Path | Purpose & Boundary |
|---|---|
| `POST /v1/skills:import` · `GET /v1/skills` · `GET /v1/skills/{id}/{version}` | Import/validate local skill bundle; read-only capability and host-owned `audit_status` / OCI sandbox readiness discovery. |
| `POST /v1/assets` · `GET /v1/assets/{id}` | Register/verify local audio/media assets by SHA-256 (`LocalNarration` metadata). |
| `POST /v1/cost-consents` · `POST /v1/cost-consents/{id}/revoke` | Create/revoke scoped remote model-cost budgets with atomic reservation and conservative reconciliation. Offline zero-cost (`remote: false`) jobs do not require a paid `CostConsent`. |
| `POST /v1/execution-authorizations` | Issue single-use authorization bound to exact `(project_id, revision_id, content_sha256, skill_id, skill_version, bundle_sha256, runtime_policy_sha256)` and claimed atomically by `job_id` on enqueue/retry. |
| `POST /v1/projects/{id}/render-jobs` & `/retry` *(ADR-02 extension)* | Will require `execution_auth_id` (and `cost_consent_id` when any selected model/TTS is `remote: true`, remaining optional/null for zero-cost offline runs); exact schema transition (`"1"` optional/additive vs. version bump) to be locked in P2-02B contract tests. |
| `GET /v1/render-jobs/{id}/events` | Read bounded, secret-scrubbed agent turns, tool calls, budget reconciliations, and repair diagnostics. |
| `GET /v1/render-jobs/{id}/artifacts` · `GET /v1/artifacts/{aid}` | List/stream content-addressed job outputs (`final.mp4`, stills, `workspace_manifest`, `qa_manifest`). |
