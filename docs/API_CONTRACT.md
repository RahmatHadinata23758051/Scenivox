# Scenivox API contract v1 — P0-03

Status: finalized specification for PM review, **not implemented Phase 1 endpoints**. P0-02 implements only `/health`. Domain invariants: [DOMAIN_CONTRACT.md](DOMAIN_CONTRACT.md). Model rules: [MODEL_CONTRACT.md](MODEL_CONTRACT.md). Machine-readable record shapes: `schemas/v1/contracts.schema.json`, JSON Schema 2020-12, schema_version `"1"`. Rust CLI and Axum call the same domain services, never duplicate approval/selection logic.

## HTTP operations

JSON requests/responses use application/json; unknown fields/versions are rejected. POST creation returns 201 with Location, GET/PATCH/PUT return 200. IDs in paths must be canonical UUIDs. Lists return `{schema_version:"1", items:[...], next_cursor:null|string}` with opaque stable ID cursor, limit 1..100 (default 20), ascending creation time then ID; invalid cursor returns 422 invalid_cursor. Timestamps/IDs are server-owned; clients submit commands, not raw persisted records.

| Method / path | Command or response contract |
|---|---|
| GET `/health` | Legacy P0-02 diagnostic; semantics below. |
| GET `/ready` | Future readiness; not yet implemented. |
| POST `/v1/projects` | `{schema_version,title,idea,target,model_selection}` -> Project; both revision pointers initially null. |
| GET `/v1/projects` | Project list envelope. |
| GET `/v1/projects/{id}` | Project. |
| PATCH `/v1/projects/{id}` | `{schema_version,title}`; title-only metadata update. Idea/target/storyboard changes use revision commands, atomically updating project preferences/current pointer and clearing approval. |
| GET `/v1/models` | Model list envelope; local read only, no probing/downloads/inference. |
| GET / PUT `/v1/settings/models` | `{schema_version,model_selection}`; PUT replaces complete two-role global map. |
| PATCH `/v1/projects/{id}/model-settings` | `{schema_version,model_selection}` replaces the project override map; empty map clears overrides. Omitted roles inherit; no null or partial provider/model pairs. Returns Project. |
| POST `/v1/providers/{id}:test` | `{schema_version,model_id,cost_consent_id}`; explicit opt-in probe only, with consent if potentially paid. Returns Model observation; missing/unsupported provider is an explicit error. |
| POST `/v1/projects/{id}/storyboard:generate` | `{schema_version,expected_revision_id,idea,target,model_selection,cost_consent_id}`; nullable expected pointer for first draft, optional run selections default empty; creates immutable Revision and updates project preferences after validation. Remote calls require separate permitted consent. |
| GET `/v1/projects/{id}/revisions` | Revision list envelope. |
| GET `/v1/projects/{id}/revisions/{rid}` | Immutable Revision; wrong-project ID is 404. |
| PATCH `/v1/projects/{id}/revisions/{rid}` | `{schema_version,expected_revision_id,idea,storyboard,target,model_selection}`; full replacement of editable content, not arbitrary JSON Patch. Asset/skill/model snapshot fields are derived and validated server-side. Creates new Revision (201); rid and expected pointer must equal current. |
| POST `/v1/projects/{id}/revisions/{rid}:review` | `{schema_version,content_sha256}`; draft -> in_review, returns `{schema_version,revision_id,review_state}`; same review repeats 200. |
| POST `/v1/projects/{id}/revisions/{rid}:withdraw-review` | Same hash command; in_review -> draft, repeated draft returns 200. Approved/historical revision returns 409 invalid_transition. |
| POST `/v1/projects/{id}/revisions/{rid}:approve` | `{schema_version,content_sha256}` -> Approval; 201 first, 200 identical repeat. Current/in_review and exact server hash required. |
| POST `/v1/projects/{id}/render-jobs` | `{schema_version,revision_id,content_sha256,cost_consent_id}` -> RenderJob; no model/target overrides. Transactionally verify current approval and queue same immutable snapshot. |
| GET `/v1/render-jobs/{id}` | RenderJob. |
| POST `/v1/render-jobs/{id}:retry` | `{schema_version,cost_consent_id}` -> NEW RenderJob with retry_of_job_id; same snapshot, renewed consent checked, current approval required. |
| POST `/v1/render-jobs/{id}:cancel` | `{schema_version}`; returns RenderJob only once cancellation is durable and child processes stopped. Terminal cancelled repeat 200; other terminal states 409. |
| GET `/v1/render-jobs/{id}/assets` | `{schema_version,items:[{id,sha256,media_type,size_bytes}],next_cursor}`; only validated allowlisted artifact IDs, never filesystem paths. |

Commands use strict objects, required fields as shown except explicitly optional fields. API version `/v1` and exported schema version `"1"` evolve together for breaking changes. Do not parse unknown record versions as v1. These command shapes are normative prose; the JSON Schema bundle covers records/value definitions, **not a complete OpenAPI/request schema**. Future endpoint implementation must add request contract tests before dispatch.

For state-changing POSTs except explicit provider probes, require `Idempotency-Key` (UUID). Persist key scoped to operation/project with canonical request hash and response in the same transaction. Same key+body replays response; different body -> 409 idempotency_conflict. Retain keys for local project lifetime; no silent expiry. A probe intentionally repeats only on a new explicit command; no automatic retry. CLI emits stable error code and nonzero exit on failure, uses the same transactional command handlers.

## Error semantics

Retain the existing application envelope (not a second Problem Details convention):
`{ "schema_version":"1", "error": {"code":"invalid_storyboard", "message":"Scene timing is invalid", "details":[{"path":"/storyboard/scenes/0/duration_ms", "code":"duration_mismatch"}]}}`.
Details use JSON Pointers and machine codes, never echo narration, tokens, raw provider responses, SQL, personal paths or stack traces. Internal structured logs retain safe operation IDs. The HTTP status is authoritative transport metadata, not a field clients must parse from message text.

| HTTP | Code / rule |
|---|---|
| 400 | `malformed_json` (including duplicate keys), `missing_idempotency_key` |
| 404 | `not_found`; unknown resource or wrong-project reference |
| 409 | `stale_revision`, `approval_required`, `hash_mismatch`, `invalid_transition`, `idempotency_conflict`, `remote_cost_not_approved` |
| 413 / 415 | `payload_too_large` (>1 MiB) / `unsupported_media_type` |
| 422 | `invalid_schema`, `unsupported_schema_version`, `invalid_storyboard`, `invalid_model_selection`, `unsupported_capability`, `unsupported_parameter`, `invalid_cursor` |
| 503 | `provider_unavailable` (disabled, missing config or known unavailable), `database_unavailable`, `skill_unavailable`; no substitute/fallback |
| 500 | `internal_error`; generic response, safe server diagnostics |

Validation order: syntax/size/media -> version/shape -> resource ownership -> stale/hash/approval checks -> domain/model/skill/asset checks -> cost permission -> dispatch. Resource reads do not spend credits. Successful planning never approves. Unapproved/stale render starts no executor and makes no paid call. Unknown provider availability is represented honestly; an explicit permitted execution may establish it, but never trigger a hidden probe.

## `/health` versus readiness (ADR-019)

P0-02 `/health` is retained unchanged: HTTP 200 and `{"status":"ok"}` when SELECT 1 succeeds, otherwise HTTP 200 and `{"status":"unavailable"}`. This is a legacy diagnostic, **not readiness and not a pure liveness guarantee**; callers must inspect the body. Startup applies migrations before listening; connection failure prevents bind.

Target `/ready` is separate: 200 `{"status":"ready"}` only when startup migrations completed and a bounded local DB query succeeds; otherwise 503 `{"status":"not_ready"}`. No model probe, credit spend, rendering or external dependency query. A future pure liveness endpoint may be designed only if operational need arises. P0-03 does not add/change handlers; readiness implementation requires a later assigned task and outage tests. Never configure an orchestrator to infer readiness from `/health`'s HTTP code.

Default deployment is local loopback, no auth or public exposure implied. CLI `scenivox serve` exists; project/models/plan/revise/review/approve/render/jobs commands remain future interfaces under these same contracts.
