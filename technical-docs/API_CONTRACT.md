# Scenivox API Contract v1 — ADR-02 Rebaseline

**Implemented at `53db65bc72b0cc37162a5bbcac3689f126cdcf97`:**
- `P1-01` Project CRUD & immutable revision persistence
- `P1-05` Local model registry & selection resolution
- `P1-02` Deterministic offline storyboard planning
- `P1-03` Revision review & editorial approval
- `P1-04` Durable `RenderJob` records & fail-closed enqueue/get/retry/cancel
- `P2-02` CLI-only `voice inspect --file PATH` (no HTTP voice route)

**Planned Phase 2 (ADR-02, not yet implemented):** `/ready`, skill bundle management, asset registry, cost-consent ledger, execution-authorization ledger, provider test probe, job agent trace, and artifact retrieval.

---

## 1. Implemented HTTP Operations

JSON requests/responses use `application/json` (max 1 MiB, duplicate keys and unknown fields rejected, `schema_version: "1"`). State-changing `POST`s require canonical UUID `Idempotency-Key`.

| Method / Path | Behavior |
|---|---|
| `GET /health` | Legacy P0-02 DB check (always 200; inspect body `{"status":"ok"\|"unavailable"}`). |
| `POST /v1/projects` · `GET /v1/projects` | Create project (`201`) / cursor-paginated list (`limit` 1..100). |
| `GET / PATCH / DELETE /v1/projects/{id}` | Read, title-only rename, or soft-delete (`204`, retains immutable rows). |
| `GET /v1/models` · `PUT /v1/models/{provider}/{model}` | Local model catalog read / explicit metadata registration. |
| `GET / PUT /v1/settings/models` | Read / replace two-role global default pairs. |
| `PATCH / PUT /v1/projects/{id}/model-settings` | Replace project role overrides (empty clears overrides). |
| `POST /v1/model-selections:resolve` | Read-only resolution (`global < project < run`), no provider calls. |
| `POST /v1/projects/{id}/storyboard:generate` | Deterministic offline planner -> immutable revision (`PlanningProvenance`). |
| `GET / POST /v1/projects/{id}/revisions` | List revisions / manual immutable snapshot save (`201`). |
| `GET / PATCH /v1/projects/{id}/revisions/{rid}` | Read revision / create child revision (`201`, never mutates old row). |
| `POST /v1/projects/{id}/revisions/{rid}/review` | `draft -> in_review` (`200`), hash-bound + `Idempotency-Key`. |
| `POST /v1/projects/{id}/revisions/{rid}/withdraw-review` | `in_review -> draft` (`200`), hash-bound + `Idempotency-Key`. |
| `POST /v1/projects/{id}/revisions/{rid}/approve` | Append immutable `Approval` (`201` new / `200` repeat), sets `approved_revision_id`. |
| `GET /v1/projects/{id}/approvals/{aid}` | Read immutable approval evidence. |
| `POST /v1/projects/{id}/render-jobs` | Fail-closed enqueue (`skill_unavailable`, `remote_cost_not_approved`, `invalid_asset_reference`). |
| `GET /v1/render-jobs/{id}` | Read `RenderJob`. |
| `POST /v1/render-jobs/{id}/retry` | Fail-closed terminal retry (`skill_unavailable`). |
| `POST /v1/render-jobs/{id}/cancel` | Cancel `queued` job transactionally (`200`). |

---

## 2. Planned Phase 2 HTTP Operations (ADR-02)

| Method / Path | Planned Behavior |
|---|---|
| `GET /ready` | `200 {"status":"ready"}` when migrations + DB query succeed; `503 {"status":"not_ready"}` otherwise. |
| `POST /v1/providers/{id}:test` | Explicit opt-in provider probe; requires `CostConsent` for `remote: true`. |
| `POST /v1/skills:import` · `GET /v1/skills` · `GET /v1/skills/{id}/{version}` | Import/validate local skill bundle; read-only capability & sandbox-readiness discovery. |
| `POST /v1/assets` · `GET /v1/assets/{id}` | Register/verify local audio/media assets by SHA-256 (`LocalNarration` metadata). |
| `POST /v1/cost-consents` · `POST /v1/cost-consents/{id}/revoke` | Create/revoke scoped model-cost budget permissions. |
| `POST /v1/execution-authorizations` | Authorize sandboxed code execution for a pinned skill + approved revision. |
| `GET /v1/render-jobs/{id}/events` | Read bounded, secret-scrubbed agent turns, tool calls, and repair diagnostics. |
| `GET /v1/render-jobs/{id}/artifacts` · `GET /v1/artifacts/{aid}` | List/stream content-addressed job outputs (`final.mp4`, stills, QA manifest). |

*(Note: `POST /v1/projects/{id}/render-jobs` and `/retry` will accept additive `execution_auth_id` alongside `cost_consent_id` when Phase 2 gates are wired, preserving strict unknown-field rules via schema/version coordination.)*

---

## 3. CLI Operations (`voice inspect` Implemented; Phase 2 Subcommands Planned)

- Implemented: `project`, `revision`, `storyboard`, `model`, `render-job`, and `voice inspect --file <relative-path>`.
- `voice inspect` accepts only a non-empty normal relative path (`./` and `..` rejected) resolving under CWD, validates regular-file metadata and the 64 MiB limit on the opened handle, and maps errors to `unsafe_audio_path`, `input_unavailable`, `audio_too_large`, `unsupported_audio_format`, `audio_limits_exceeded`, or `invalid_audio`.

---

## 4. Error Semantics

Retain `{ "schema_version": "1", "error": { "code": "...", "message": "...", "details": [...] } }`:

| HTTP | Codes |
|---|---|
| `400` | `malformed_json`, `missing_idempotency_key` |
| `404` | `not_found` |
| `409` | `stale_revision`, `approval_required`, `hash_mismatch`, `invalid_transition`, `idempotency_conflict`, `remote_cost_not_approved`, `execution_not_authorized`, `cost_limit_exceeded` *(planned)* |
| `413` / `415` | `payload_too_large` (> 1 MiB) / `unsupported_media_type` |
| `422` | `invalid_schema`, `unsupported_schema_version`, `invalid_storyboard`, `invalid_model_selection`, `unsupported_capability`, `unsupported_parameter`, `invalid_cursor`, `invalid_asset_reference`, `invalid_skill_manifest` *(planned)* |
| `503` | `provider_unavailable`, `database_unavailable`, `skill_unavailable`, `sandbox_unavailable` *(planned)* |
| `500` | `internal_error` |
