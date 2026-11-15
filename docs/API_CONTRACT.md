# Scenivox Backend API / Domain Contract — Draft v0.3

These are **proposed HTTP and CLI contracts**, not implemented endpoints. OMP should use one Rust domain layer for both Axum routes and CLI. Changes require migration notes and PM review; this document does not authorize building a frontend.

## 1. Versioned schema sketches
```json
{
  "project": {
    "id": "uuid", "title": "Misteri eksperimen rahasia", "idea": "...",
    "duration_ms": 60000, "aspect_ratio": "9:16", "fps": 30,
    "language": "id-ID", "style_id": "stick-figure",
    "model_selection": {
      "storyboard_planning": {"provider_id": "mock", "model_id": "planner-v1"},
      "scene_code_generation": {"provider_id": "mock", "model_id": "coder-v1"}
    },
    "current_revision_id": "uuid", "approved_revision_id": null
  },
  "storyboard_revision": {
    "id": "uuid", "project_id": "uuid", "version": 1,
    "status": "draft", "content_sha256": "hex_sha256",
    "synopsis": "...", "hook": "...",
    "scenes": [{
      "id": "uuid", "order": 1, "duration_ms": 7500,
      "visual_direction": "...", "camera": "...", "action": "...",
      "narration": "...", "caption_hint": "...", "sfx_cues": []
    }]
  }
}
```

## 2. Required invariants
- Scene IDs stable within revision; durations are positive integer milliseconds; total matches project target within an explicitly documented rounding policy. Do not silently drop long narration.
- Edits never mutate an approved historical snapshot. `PATCH` creates a **new draft revision**; only explicit approval allows generation of that revision.
- Render request includes an approved revision ID and persists content hash; stale/current disagreement must fail explicitly (e.g. 409), not silently render another revision.
- `model_selection`: separate validated `provider_id` and `model_id`, capability by role. Resolution is default → project → explicit per-run choice; a draft or job stores effective selection and safe parameters. If an unavailable provider is selected, fail explicitly; **no surprise fallback** or model substitution.
- Provider listings must not perform inference, charge credits or reveal credentials. Connection test is an explicit operation requiring opt-in.
- Artifacts referenced by safe opaque IDs and stored under a restricted workspace; do not accept arbitrary filesystem paths from API callers.
- Jobs are successful only after artifact existence + ffprobe/QA validation; metadata-only or storyboard mock cannot count as render success.

## 3. Backend REST endpoints
| Verb | Path | Purpose / initial phase |
|---|---|---|
| GET | `/health` | local health, Phase 0/1 |
| GET | `/v1/models` | list provider/model IDs, capabilities and config state (no secrets), Phase 1 |
| GET | `/v1/settings/models` | read default model IDs per task, Phase 1 |
| PUT | `/v1/settings/models` | validate/update default model settings, Phase 1 |
| PATCH | `/v1/projects/{id}/model-settings` | project overrides per task, Phase 1 |
| POST | `/v1/providers/{provider_id}:test` | explicit, user-approved connectivity test, Phase 1+ |
| POST | `/v1/projects` | create local project, Phase 1 |
| GET | `/v1/projects/{id}` | inspect project, Phase 1 |
| POST | `/v1/projects/{id}/storyboard:generate` | planner model produces a **draft**, Phase 1 |
| GET | `/v1/projects/{id}/revisions` | list revision history, Phase 1 |
| PATCH | `/v1/projects/{id}/revisions/{revision_id}` | validated edit creates a **new** draft revision, Phase 1 |
| POST | `/v1/projects/{id}/revisions/{revision_id}:approve` | explicit immutable revision approval, Phase 1 |
| POST | `/v1/projects/{id}/render-jobs` | approve check + skill/job snapshot + queue, Phase 1/2 |
| GET | `/v1/render-jobs/{id}` | progress/state/stage and safe log references, Phase 2 |
| POST | `/v1/render-jobs/{id}:retry` | bounded retry after failure, Phase 2 |
| GET | `/v1/render-jobs/{id}/assets` | allowlisted IDs, manifest and verified outputs, Phase 2 |

**Default security:** local loopback binding, no public network exposure by default; adding a network-exposed API requires a separate access-control decision. No auth UI in early phase.

## 4. Typical model setting request (illustrative)
```json
{
  "storyboard_planning": {"provider_id": "mock", "model_id": "planner-v1"},
  "scene_code_generation": {"provider_id": "mock", "model_id": "coder-v1"}
}
```
`GET /v1/models` returns IDs, task capabilities, `enabled`, `configured`, and `availability` with a reason. Status may be `unknown` when no explicit connectivity test has been performed; do not report `available=true` without verification.

## 5. Error contract
```json
{"error":{"code":"approval_required","message":"...","details":{}}}
```
Representative mapping: `409 approval_required`, `409 stale_revision`, `422 invalid_storyboard`, `422 unsupported_capability`, `503 provider_unavailable`, `409 remote_cost_not_approved`. Apply appropriate errors consistently to API and CLI.

## 6. CLI parity
Proposed command surface: `scenivox project create`, `scenivox models list`, `scenivox models select`, `scenivox plan`, `scenivox revise`, `scenivox approve`, `scenivox render`, `scenivox jobs`, `scenivox export`. Names are subject to developer feasibility review; results and state semantics must be shared with API.
