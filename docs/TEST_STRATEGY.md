# Scenivox Test & Acceptance Strategy — v0.3

## 1. Automated layers
- **Rust quality:** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` once source exists. Make CI independent of external AI subscriptions.
- **Unit:** scene duration math; revision/approval state transitions; provider capability selection, overrides and budget guard; safe path resolution; child-process timeout behavior; validation of structured generated JSON.
- **Integration:** SQLx SQLite migrations and reopen; Axum API response/status contract; Rust CLI using same services; deterministic mock planner and mock voice; idempotent job enqueue; crash/retry behavior.
- **Real engine (local/release gate):** vetted skill smoke example and full video; use ffprobe dimensions/fps/codecs/duration/audio and a sample of temporally separated frames to confirm actual motion; review captions and audio quality manually.

## 2. Required negative cases
1. Rendering a draft/unapproved or stale revision returns 409 and launches **no renderer**.
2. Editing an approved scene creates new draft while retaining original job snapshot; default-model changes do not mutate it.
3. Invalid or unavailable model/provider, unsupported capability, missing credentials and unapproved paid calls fail explicitly; never auto-fallback.
4. Malicious paths, traversal, URLs/scripts in LLM output, unknown skill IDs and excessive payload sizes are rejected.
5. Missing Node/Chromium/FFmpeg/TTS, subprocess timeout, child process cleanup and job interruption produce accurate status/diagnostics.
6. Narration exceeding scene time must be reported or resolved by explicit timing rules, not silently truncated.
7. Job success implies artifacts exist and actually pass media validation, not just that a command returned zero.
8. Any network-exposed backend mode requires explicit security review; loopback default must be tested.

## 3. Golden end-to-end (Phase 2)
Project: `Misteri eksperimen rahasia di bawah tanah yang disembunyikan selama puluhan tahun`; duration 60 seconds, language `id-ID`, portrait `9:16`. Generate multi-scene draft, edit scene 3, approve exact revision, run audited renderer, produce final MP4 and inspect camera/action changes, intelligible narration and synchronized captions. Log model selection, skill version, provenance, asset hashes and measured runtime/peak resource use.

## 4. Environment / CI
- Mock CI must work offline without large model downloads or service accounts. Use temp SQLite, disposable local files, deterministic fixtures and avoid committing large videos.
- Heavy real-render tests may run only locally initially, with commands, hashes and metadata captured for PM/CEO review. A synthetic generated sample can test mechanics but **cannot** alone pass the creative quality gate.
- Windows/WSL portability must be explicitly measured. Do not claim Windows native compatibility from a WSL pass.

## 5. Model selection matrix
- List configured vs unconfigured/unknown models without secrets or implicit connection probes.
- Select different supported mock models for `storyboard_planning` and `scene_code_generation` independently.
- Verify precedence: global default < project override < per-run selection, with no silent fallback.
- Persist selections and validate reruns; ensure existing approved revision/job records retain their snapshot after a settings change.
