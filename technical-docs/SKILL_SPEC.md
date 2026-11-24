# Scenivox Skill & Agent Execution Contract — ADR-02

**Status:** Specification for Phase 2 skill bundles, AI agent tool runtime, unified composition, and sandbox isolation. No third-party skill execution is enabled in the current repository (`53db65bc72b0cc37162a5bbcac3689f126cdcf97`).

---

## 1. Skill Bundle Contract

A skill bundle is an explicitly imported local directory or ZIP archive pinned by cryptographic hash (`bundle_sha256`). Silent internet downloads during import or render are forbidden.

### Expected Bundle Layout
- `skill.manifest.json` — Strict machine-readable manifest (`additionalProperties: false`, required).
- `SKILL.md` — Creative and workflow instructions supplied to the AI agent as **untrusted UTF-8 text data** (never executed as code).
- `references/` *(optional)* — Read-only reference docs/examples inspectable by the agent via bounded `workspace_read`.
- `scripts/` / `engine/` / `template/` / `assets/` *(optional)* — Skill runtime templates, helper libraries, fonts, and media assets.
- `LICENSE` / `THIRD_PARTY_NOTICES` *(recommended)* — Preserved in bundle provenance.

### Pinned Manifest Schema (`skill.manifest.json`, v1 planned)
```json
{
  "schema_version": "1",
  "id": "stick-figure",
  "version": "0.1.0",
  "kind": "unified_composition",
  "composition_mode": "unified_timeline",
  "rendering_strategy": "canvas_playwright_ffmpeg",
  "supported_aspect_ratios": ["9:16"],
  "max_duration_ms": null,
  "supports_independent_scene_render": false,
  "code_generation_policy": {
    "languages": ["javascript", "python"],
    "writable_paths": ["web/shots.js", "audio.py", "script.json", "captions.json"],
    "requires_sandbox": true
  },
  "entrypoints": {
    "stills": { "runtime": "node", "script": "engine/tools/render.js", "args_template": ["stills", "{times}"], "timeout_sec": 60 },
    "render": { "runtime": "node", "script": "engine/tools/render.js", "args_template": ["video", "{out}"], "timeout_sec": 600 }
  },
  "requires": ["node", "chromium", "ffmpeg", "python"],
  "accepted_inputs": ["revision.json", "narration.wav", "timeline.json", "words.json"],
  "produced_outputs": ["final.mp4", "stills/*.jpg"],
  "audit_status": "static_audited"
}
```
`null` denotes **unverified** capability limits (for example `max_duration_ms: null` until a 60-second run is measured).

---

## 2. Import, Validation, Versioning & Discovery

1. **Import validation:** Reject ZIP bombs (max uncompressed size, max file count, max ratio), path traversal (`..`, absolute paths, drive prefixes), symlinks/hardlinks, and duplicate entries before extracting into Scenivox's local skill store.
2. **Hash pinning:** Compute `bundle_sha256` at import and store `(skill_id, skill_version, bundle_sha256, manifest_json, audit_status, enabled)` in SQLite `skill_bundles`. Modifying any file requires a new version/hash; historical revisions and jobs stay pinned to their recorded `(skill_id, skill_version, bundle_sha256)`.
3. **Capability discovery:** `GET /v1/skills` and `scenivox skill list` are **read-only**—they report declared capabilities, audit status, and verified local sandbox/runtime availability without running skill scripts or downloading packages.

---

## 3. Unified Composition & Agent Tool Boundary

Under **ADR-02**, Scenivox runs an in-process Rust AI agent (`src/agent.rs`) instead of requiring Claude Code, OMP, or Antigravity:

- **Unified composition:** The agent receives the approved `RevisionContent` (all scenes in order, narration, visual/camera/action/caption hints), `LocalNarration`, and P2-02 `Timeline` / `WordTiming` sidecar (`words.json`), generating a **single cohesive composition** (e.g., unified `web/shots.js` with cross-scene whip transitions + `audio.py` mix) rather than isolated single-scene MP4s.
- **Skill-specific internal rendering:** A skill's manifest chooses how frames and audio are built inside its sandbox (parallel shot segments + FFmpeg concat, single-pass Canvas, Remotion, etc.).
- **Bounded agent tools (all paths strictly confined to job workspace / RO skill mounts):**
  - `workspace_list(rel_dir)` — bounded directory listing.
  - `workspace_read(rel_path, offset, limit)` — bounded UTF-8 read (`<= 128 KiB` per call).
  - `workspace_write(rel_path, content)` / `workspace_patch(rel_path, edits)` — allowed **only** on manifest-declared `writable_paths` (never overwriting RO skill engine, `.env`, or input evidence).
  - `skill_run_step(entrypoint_id, args)` — runs an allowlisted manifest entrypoint inside the **fail-closed sandbox**; returns exit code, bounded stdout/stderr tail, and newly created workspace paths.
  - `probe_media(rel_path)` — runs bounded `ffprobe` / image frame inspection so the agent can inspect failures or stillness during iterative repair.
  - `complete_composition(output_rel_path, summary)` — submits candidate output to Rust host MediaQA.
- **Hard loop budgets:** Per-job limits (`max_agent_turns`, `max_tool_calls`, `max_repair_iterations`, `max_tokens`, `max_cost_micros`, `max_wall_clock_seconds`) stop runaway loops and fail the job explicitly with trace evidence.

---

## 4. Fail-Closed Sandbox & Host Controls

AI-generated JavaScript and Python are **untrusted code**. Ordinary subprocess CWD/env filtering is insufficient.

- **Mandatory sandbox invariants:**
  1. **No network:** `--network=none` / isolated netns during all sandboxed code execution, Chromium rendering, and FFmpeg muxing.
  2. **Filesystem isolation:** RO mounts for skill engine/kit/models and frozen inputs; isolated RW ephemeral job directory with size quota; zero host `$HOME`, DB, `.env`, or git access.
  3. **Zero secrets:** Host API keys never enter sandbox env, args, or files.
  4. **Resource caps & cleanup:** CPU, RAM, PIDs, wall-clock timeout, and OS-enforced descendant reaping (`cgroup`/container kill or Windows Job Object).
  5. **Fail closed:** If `SandboxBackend::is_available() == false` on the host, enqueue/execution returns `503 sandbox_unavailable` and launches **nothing**.

---

## 5. `stick-figure` Vertical Slice Baseline (P0-01B Audited)

Static audit of `C:\Users\user\Downloads\stick-figure-windows-v2.zip` (`f3b7dc6e75b476bae156a4eb5c4eba3dd395765f056715e5cb0d3d0c65ac17a5`, 29/29 files verified against `stick-figure-windows/`) established:
- **Reusable core:** Canvas 2D renderer (`lib.js`, `chars.js`, `env.js`, `main.js`, `index.html`, OFL 1.1 fonts `Poppins-Bold.ttf` & `Lora-Variable.ttf`), Playwright `1.49.1` (`render.js`), and FFmpeg (`1080×1920`, 30 fps `libx264` + `aac`).
- **Excluded from Scenivox runtime:** `INSTAL.bat`, `install-windows.sh` (auto-downloads/winget/junctions), `make.sh` (`rm -rf`, `grep -c`, bashisms), `tts_kokoro.py` (`en-us` only), `tts_elevenlabs.py` (host `.env`/unapproved HTTP), and `align.py` (English Parakeet ASR).
- **Vertical slice target:** Import a clean `stick-figure` skill bundle with `skill.manifest.json`, drive unified `web/shots.js` + audio mix inside `--network=none` sandbox using P2-02 `LocalNarration` + `words.json`, and verify `final.mp4` via `ffprobe` + multi-frame pixel-diff motion + human review.
